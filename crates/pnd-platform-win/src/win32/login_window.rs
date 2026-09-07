//! 登录窗的 Win32 + WebView2 实现:一条自己的 STA 线程 + 一个普通的
//! `WS_OVERLAPPEDWINDOW`,客户区几乎全部让给 Edge 内核。
//!
//! 为什么又是一条独立线程:和卡片同一个理由 —— 窗口是线程绑定的,而 WebView2
//! 还额外要求那条线程是**单线程套间**(STA)并且一直在泵消息。GPUI 的主线程
//! 两条都不满足。
//!
//! 为什么不用 `wry` 之类的封装:那些库自带窗口管理、事件循环和一堆平台后端,
//! 而这里要的只是"一个窗口 + 一个 cookie"。`webview2-com` 是微软官方的裸绑定,
//! 而且 0.38 那条分支的 `windows` 依赖正好和 workspace 对得上,整棵树里只有
//! 一份 COM 类型。
//!
//! 和卡片不一样的地方:这个窗口**该**抢焦点(你自己点出来的,马上要在里面
//! 打字),所以没有 `WS_EX_NOACTIVATE`、没有置顶、进 Alt+Tab。

use std::cell::{Cell, RefCell};
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, JoinHandle};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, GetAvailableCoreWebView2BrowserVersionString,
    ICoreWebView2, ICoreWebView2_2, ICoreWebView2Controller, ICoreWebView2Environment,
    ICoreWebView2NavigationCompletedEventArgs, ICoreWebView2NavigationCompletedEventArgs2,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    GetCookiesCompletedHandler, NavigationCompletedEventHandler, take_pwstr,
};
use windows::Win32::Foundation::{E_POINTER, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, EndPaint, FW_NORMAL, FillRect,
    PAINTSTRUCT, SetBkMode, TRANSPARENT,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, IDC_ARROW,
    IsWindow, LoadCursorW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, RegisterClassExW,
    SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, WM_APP, WM_CLOSE, WM_DPICHANGED, WM_ERASEBKGND, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WM_SIZE, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{Interface as _, PCWSTR, PWSTR, w};

use super::paint::{Brush, Font, HAIRLINE, PANEL, RAIL, TEXT_SECONDARY, draw_text};
use super::{error_from_windows, work_area};
use crate::PlatformError;
use crate::alert_card::scale_for_dpi;
use crate::login::{
    ACCOUNT_URL, AfterNavigation, COOKIE_ORIGIN, LOGIN_URL, LoginCommand, LoginConfig, LoginEvent,
    LoginFailure, LoginShared, MAX_AUTO_NAVIGATIONS, after_navigation, login_geometry,
    pick_session_cookie,
};

const LOGIN_CLASS: PCWSTR = w!("PndLoginWindow");
/// 唤醒消息。命令本身在 `LoginShared` 的队列里,这条只负责把线程从
/// `GetMessageW` 里叫醒。号码和卡片那条错开。
const WM_LOGIN_COMMANDS: u32 = WM_APP + 0x2A2;

/// 顶上那条提示带的高度(96 dpi 逻辑像素)、左右内边距和字号。
const STRIP_HEIGHT: i32 = 30;
const PAD_X: i32 = 12;
const HINT_FONT_SIZE: i32 = 12;

/// 强制让"有没有 WebView2 运行时"这一步失败的环境变量。
///
/// 存在的唯一理由是验证:`RuntimeMissing` 那条路要求一台没装 Edge 内核的
/// Windows,而这是唯一能在开发机上把它真跑一遍的办法。没设就是一句
/// `env::var` 的开销。
const FORCE_MISSING_ENV: &str = "PND_WEBVIEW2_FORCE_MISSING";

static CLASS_READY: OnceLock<Result<(), PlatformError>> = OnceLock::new();

pub(crate) fn spawn_login_worker(
    config: LoginConfig,
    shared: Arc<LoginShared>,
    events: mpsc::Sender<LoginEvent>,
    ready: mpsc::SyncSender<Result<(), PlatformError>>,
) -> Result<JoinHandle<()>, PlatformError> {
    thread::Builder::new()
        .name("pnd-login".to_owned())
        .spawn(move || {
            worker_thread(config, Arc::clone(&shared), events, ready);
            shared.set_native_thread_id(0);
        })
        .map_err(|error| PlatformError::Thread {
            operation: "spawn(pnd-login)",
            detail: error.to_string(),
        })
}

/// 把登录线程从 `GetMessageW` 里叫醒,让它去队列里取命令。
pub(crate) fn wake_login(thread_id: u32) -> Result<(), PlatformError> {
    if thread_id == 0 {
        return Err(PlatformError::Thread {
            operation: "wake_login",
            detail: "the login thread is gone".to_owned(),
        });
    }
    // SAFETY: 线程 id 只在消息队列建好之后才被公布。
    unsafe { PostThreadMessageW(thread_id, WM_LOGIN_COMMANDS, WPARAM(0), LPARAM(0)) }
        .map_err(|error| error_from_windows("PostThreadMessageW(login)", error))
}

fn worker_thread(
    config: LoginConfig,
    shared: Arc<LoginShared>,
    events: mpsc::Sender<LoginEvent>,
    ready: mpsc::SyncSender<Result<(), PlatformError>>,
) {
    // WebView2 只在单线程套间里工作。S_FALSE("这条线程已经初始化过")不是错。
    // SAFETY: CoInitializeEx 只影响当前线程,第一个参数按文档可以为 None。
    let hresult = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hresult.is_err() {
        let _ = ready.send(Err(error_from_windows(
            "CoInitializeEx(login)",
            windows::core::Error::from(hresult),
        )));
        return;
    }
    let _com = ComGuard;

    // 和卡片同一个理由:不设的话高 dpi 下 GetDpiForWindow 永远返回 96,
    // 窗口会被系统按位图拉大,网页上的字是糊的。
    // SAFETY: SetThreadDpiAwarenessContext 只影响当前线程,无指针参数。
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let mut queue_message = MSG::default();
    // SAFETY: 一次 PeekMessageW 就能逼 Windows 给本线程建出消息队列,必须发生在
    // 公布线程 id 之前,否则 PostThreadMessageW 会丢消息。
    let _ = unsafe { PeekMessageW(&mut queue_message, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: GetCurrentThreadId 没有前置条件。
    shared.set_native_thread_id(unsafe { GetCurrentThreadId() });
    let _ = ready.send(Ok(()));

    let mut worker = LoginWorker {
        config,
        events,
        shared: Arc::clone(&shared),
        window: None,
        auto_navigations: 0,
        report_next_check: false,
    };

    let mut message = MSG::default();
    loop {
        // SAFETY: 本线程自己的标准消息循环。
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        if message.message == WM_LOGIN_COMMANDS {
            if worker.drain_commands() {
                break;
            }
        } else {
            // SAFETY: message 刚由 GetMessageW 填好。
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        worker.poll_signals();
        worker.rewake_if_pending();
    }
    worker.close_window();
}

/// 线程退出时把 COM 还回去。放一个 guard 而不是在每条返回路径上手写一句:
/// 漏掉一条,这条线程的套间就永远不散。
struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: 和本线程开头那次 CoInitializeEx 配对。
        unsafe { CoUninitialize() };
    }
}

struct LoginWorker {
    config: LoginConfig,
    events: mpsc::Sender<LoginEvent>,
    shared: Arc<LoginShared>,
    window: Option<LoginWindow>,
    /// 窗口自己主动跳了几次。见 `MAX_AUTO_NAVIGATIONS`。
    auto_navigations: u32,
    /// 用户按了"现在核对一次":下一次"我的账号"的结论要报回去,
    /// 哪怕结论是"还没登录"。自动那条路不报,否则从开窗那一刻起
    /// 状态行就一直在喊"还没登录"。
    report_next_check: bool,
}

impl LoginWorker {
    /// 返回 true 表示收到了 Shutdown。
    fn drain_commands(&mut self) -> bool {
        while let Some(command) = self.shared.pop() {
            match command {
                LoginCommand::Open => self.open(),
                LoginCommand::Capture => self.request_check(),
                LoginCommand::Close => self.close_window(),
                LoginCommand::Shutdown => return true,
            }
        }
        false
    }

    /// 建环境和读 cookie 都要跑一段嵌套消息泵,那段泵会把队列里的唤醒消息
    /// 顺手取走 —— 取走了就没人再叫醒本线程,后来的命令会一直躺在队列里。
    /// 所以每一轮末尾看一眼,还有命令就再敲一次。
    fn rewake_if_pending(&self) {
        if self.shared.has_commands() {
            let _ = wake_login(self.shared.native_thread_id());
        }
    }

    fn open(&mut self) {
        if self.window.is_some() {
            return;
        }
        self.auto_navigations = 0;
        self.report_next_check = false;
        // 先问一句这台机器上有没有 Edge 内核。不问的话,建环境会以一个看不懂的
        // HRESULT 失败,而用户需要的是"去装 WebView2"这句话。
        if available_runtime_version().is_err() {
            self.send(LoginEvent::Failed(LoginFailure::RuntimeMissing));
            return;
        }
        let window = match LoginWindow::create(&self.config) {
            Ok(window) => window,
            Err(error) => {
                self.send(LoginEvent::Failed(LoginFailure::ControllerFailed(
                    error.to_string(),
                )));
                return;
            }
        };
        self.window = Some(window);
        if let Err(failure) = self.attach_webview() {
            self.send(LoginEvent::Failed(failure));
            self.close_window();
            return;
        }
        self.send(LoginEvent::Opened);
    }

    /// 把 WebView2 挂到已经建好的窗口上,并走到"我的账号"页。
    fn attach_webview(&mut self) -> Result<(), LoginFailure> {
        let environment = create_environment(&self.config.user_data_dir)
            .map_err(LoginFailure::EnvironmentFailed)?;
        let window = self.window.as_mut().expect("the window was just created");
        let controller =
            create_controller(&environment, window.hwnd).map_err(LoginFailure::ControllerFailed)?;
        // SAFETY: controller 刚建好,归本线程所有。
        let webview = unsafe { controller.CoreWebView2() }
            .map_err(|error| LoginFailure::ControllerFailed(error.message()))?;

        let signals = Rc::clone(&window.signals);
        // 每一跳都记下来:地址 + HTTP 状态码。Cloudflare 的"稍等一下"、
        // Steam / Epic 的授权页在这里都只是普通的一跳,为它们写特判只会在
        // 对面改版的那天坏掉 —— 判断交给纯函数 `after_navigation`。
        let on_navigated =
            NavigationCompletedEventHandler::create(Box::new(move |source, args| {
                let Some(source) = source else {
                    return Ok(());
                };
                let mut url = PWSTR::null();
                // SAFETY: url 是合法出参;take_pwstr 负责 CoTaskMemFree。
                if unsafe { source.Source(&mut url) }.is_err() {
                    return Ok(());
                }
                signals
                    .navigated
                    .borrow_mut()
                    .push((take_pwstr(url), http_status(args.as_ref())));
                Ok(())
            }));
        let mut token = 0_i64;
        // SAFETY: 回调只在本线程被调用,捕获的 Rc 因此不跨线程;token 是合法出参。
        unsafe { webview.add_NavigationCompleted(&on_navigated, &mut token) }
            .map_err(|error| LoginFailure::ControllerFailed(error.message()))?;

        window.controller = Some(controller);
        window.webview = Some(webview);
        window.layout();
        window.navigate(ACCOUNT_URL);
        Ok(())
    }

    /// 设置页那个"我已经登录了,现在核对一次"的按钮。
    ///
    /// 它**不直接读 cookie**,而是重新走一趟"我的账号"页,让同一条判据
    /// (URL + 200)来定夺。理由是那条判据本身:Cloudflare 给每个匿名访客都发
    /// 一个 `POESESSID`,所以"读到 cookie"从来就不等于"登录了"。直接读的话,
    /// 这个按钮就成了一个能把你上一次真会话盖掉的按钮。
    ///
    /// 用户点的这一下不吃自动跳转的额度 —— 额度防的是程序自己乱弹。
    fn request_check(&mut self) {
        let Some(window) = self.window.as_ref() else {
            self.send(LoginEvent::NotLoggedIn);
            return;
        };
        self.report_next_check = true;
        window.navigate(ACCOUNT_URL);
    }

    /// "我的账号"页答了 200:现在读 cookie 才有意义。
    fn capture_now(&mut self) {
        self.report_next_check = false;
        let Some(cookies) = self.read_cookies_now() else {
            return;
        };
        match pick_session_cookie(&cookies) {
            Some(poesessid) => {
                self.send(LoginEvent::SessionCaptured { poesessid });
                self.close_window();
            }
            // 页面说登录了,cookie 里却没有会话:只可能是 WebView2 那头出了
            // 岔子,报出去总比装作抓到了好。
            None => self.send(LoginEvent::NotLoggedIn),
        }
    }

    /// 读一次 cookie 并把**名字**报出去。
    ///
    /// 名字不是秘密,而它是这套东西唯一可查的现场:出问题时"浏览器手上到底
    /// 有哪些 cookie"是第一个要问的。值永远留在这个函数里。
    fn read_cookies_now(&mut self) -> Option<Vec<(String, String)>> {
        let webview = self.window.as_ref().and_then(|w| w.webview.clone())?;
        match read_cookies(&webview) {
            Ok(cookies) => {
                self.send(LoginEvent::CookieNames(
                    cookies.iter().map(|(name, _)| name.clone()).collect(),
                ));
                Some(cookies)
            }
            Err(detail) => {
                self.send(LoginEvent::Failed(LoginFailure::CookieReadFailed(detail)));
                None
            }
        }
    }

    /// 程序自己发起的一跳。有额度上限,防的是"401 就送去登录页"和"回到官网
    /// 别的页面就回去核对"这两条规则跟着用户的浏览来回弹。
    fn auto_navigate(&mut self, url: &str) {
        if self.auto_navigations >= MAX_AUTO_NAVIGATIONS {
            return;
        }
        self.auto_navigations += 1;
        if let Some(window) = self.window.as_ref() {
            window.navigate(url);
        }
    }

    /// 把 wndproc 和 WebView2 回调留下的"留言"取走。它们够不到 worker 的字段,
    /// 所以都是先记在 `Signals` 上,再由这里读走 —— 和卡片同一套。
    fn poll_signals(&mut self) {
        let Some((hops, resized, closing)) = self.window.as_ref().map(|window| {
            (
                std::mem::take(&mut *window.signals.navigated.borrow_mut()),
                window.signals.resized.replace(false),
                window.signals.close_requested.replace(false),
            )
        }) else {
            return;
        };
        if resized && let Some(window) = self.window.as_ref() {
            window.layout();
        }
        for (url, status) in hops {
            let decision = after_navigation(&url, status);
            self.send(LoginEvent::Navigated {
                url,
                http_status: status,
            });
            match decision {
                AfterNavigation::Capture => self.capture_now(),
                AfterNavigation::GoToLogin => {
                    // 现场先留一份(只有名字),再把人送到登录页。
                    let _ = self.read_cookies_now();
                    if std::mem::take(&mut self.report_next_check) {
                        self.send(LoginEvent::NotLoggedIn);
                    }
                    self.auto_navigate(LOGIN_URL);
                }
                AfterNavigation::RecheckAccount => self.auto_navigate(ACCOUNT_URL),
                AfterNavigation::Wait => {}
            }
            // 抓到会话时窗口已经关了,后面那些迟到的跳转不该再动它。
            if self.window.is_none() {
                break;
            }
        }
        if closing {
            self.close_window();
        }
    }

    fn close_window(&mut self) {
        if self.window.take().is_some() {
            self.send(LoginEvent::Closed);
        }
    }

    fn send(&self, event: LoginEvent) {
        let _ = self.events.send(event);
    }
}

/// 这一跳的 HTTP 状态码。
///
/// `HttpStatusCode` 是 `ICoreWebView2NavigationCompletedEventArgs2` 才有的
/// (WebView2 1.0.1108,2022 年),太老的运行时取不到 —— 那就返回 `None`,
/// 由 [`after_navigation`] 当"没登录"处理。
fn http_status(args: Option<&ICoreWebView2NavigationCompletedEventArgs>) -> Option<i32> {
    let args = args?
        .cast::<ICoreWebView2NavigationCompletedEventArgs2>()
        .ok()?;
    let mut code = 0_i32;
    // SAFETY: code 是合法出参。
    unsafe { args.HttpStatusCode(&mut code) }.ok()?;
    Some(code)
}

/// 这台机器上装着的 Edge 内核版本。装了就是 `Ok(版本号)`。
fn available_runtime_version() -> Result<String, String> {
    if std::env::var(FORCE_MISSING_ENV).is_ok_and(|value| !value.trim().is_empty()) {
        return Err(format!("{FORCE_MISSING_ENV} is set"));
    }
    let mut version = PWSTR::null();
    // SAFETY: 第一个参数为 null 表示"用系统装着的那个";version 是合法出参,
    // take_pwstr 负责 CoTaskMemFree。
    unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) }
        .map_err(|error| error.message())?;
    let version = take_pwstr(version);
    if version.is_empty() {
        return Err("the runtime reported an empty version".to_owned());
    }
    Ok(version)
}

/// 建 WebView2 环境。cookie 和缓存都落在 `user_data_dir` 里,和你浏览器里的
/// 登录状态互不相干 —— 也因此,出问题时把那个目录整个删掉就是"重来一次"。
fn create_environment(user_data_dir: &Path) -> Result<ICoreWebView2Environment, String> {
    let mut folder: Vec<u16> = user_data_dir.as_os_str().encode_wide().collect();
    folder.push(0);
    let (sender, receiver) = mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            // SAFETY: folder 以 NUL 结尾且活到调用返回;options 可以为 None。
            unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    PCWSTR(folder.as_ptr()),
                    None,
                    &handler,
                )
            }
            .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, environment| {
            code?;
            let _ = sender.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)));
            Ok(())
        }),
    )
    .map_err(|error| error.to_string())?;
    receiver
        .recv()
        .map_err(|error| error.to_string())?
        .map_err(|error| error.message())
}

/// 把 WebView2 控件挂到窗口上。
fn create_controller(
    environment: &ICoreWebView2Environment,
    hwnd: HWND,
) -> Result<ICoreWebView2Controller, String> {
    let environment = environment.clone();
    let (sender, receiver) = mpsc::channel();
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            // SAFETY: hwnd 是本线程刚建的窗口,回调也在本线程被调用。
            unsafe { environment.CreateCoreWebView2Controller(hwnd, &handler) }
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, controller| {
            code?;
            let _ = sender.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)));
            Ok(())
        }),
    )
    .map_err(|error| error.to_string())?;
    receiver
        .recv()
        .map_err(|error| error.to_string())?
        .map_err(|error| error.message())
}

/// 问 WebView2 自己的 cookie 管理器要 `pathofexile.com` 域下的全部 cookie。
///
/// 走这条路而不是去读浏览器的 cookie 文件:这是**本程序自己那份**登录状态,
/// 就是你刚在这个窗口里登出来的,不碰你 Chrome / Edge 里的任何东西。
fn read_cookies(webview: &ICoreWebView2) -> Result<Vec<(String, String)>, String> {
    let webview2 = webview
        .cast::<ICoreWebView2_2>()
        .map_err(|error| error.message())?;
    // SAFETY: webview2 归本线程所有。
    let manager = unsafe { webview2.CookieManager() }.map_err(|error| error.message())?;
    let collected: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
    let sink = Rc::clone(&collected);
    let mut origin: Vec<u16> = COOKIE_ORIGIN.encode_utf16().collect();
    origin.push(0);
    GetCookiesCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            // SAFETY: origin 以 NUL 结尾且活到调用返回。
            unsafe { manager.GetCookies(PCWSTR(origin.as_ptr()), &handler) }
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, list| {
            code?;
            let Some(list) = list else {
                return Ok(());
            };
            let mut count = 0_u32;
            // SAFETY: list 由 WebView2 交过来,count 是合法出参。
            unsafe { list.Count(&mut count) }?;
            for index in 0..count {
                // SAFETY: index 在 0..count 之内;两个 PWSTR 都是合法出参,
                // take_pwstr 负责 CoTaskMemFree。
                let (name, value) = unsafe {
                    let cookie = list.GetValueAtIndex(index)?;
                    let mut name = PWSTR::null();
                    let mut value = PWSTR::null();
                    cookie.Name(&mut name)?;
                    cookie.Value(&mut value)?;
                    (name, value)
                };
                sink.borrow_mut()
                    .push((take_pwstr(name), take_pwstr(value)));
            }
            Ok(())
        }),
    )
    .map_err(|error| error.to_string())?;
    let cookies = collected.borrow().clone();
    Ok(cookies)
}

/// wndproc 和 worker 共用的状态。所有权在 `WM_NCCREATE` 交给窗口,
/// `WM_NCDESTROY` 时释放 —— 和卡片同一套握手。
#[derive(Default)]
struct Signals {
    /// 顶上那条提示。建窗之后不再变,wndproc 只读它。
    hint: String,
    /// 已经走完、还没报给调用方的那些跳转:地址 + HTTP 状态码。
    navigated: RefCell<Vec<(String, Option<i32>)>>,
    resized: Cell<bool>,
    close_requested: Cell<bool>,
}

struct LoginWindow {
    hwnd: HWND,
    signals: Rc<Signals>,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
}

impl LoginWindow {
    fn create(config: &LoginConfig) -> Result<Self, PlatformError> {
        ensure_class()?;
        // SAFETY: 取本模块的 HINSTANCE,无前置条件。
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|error| error_from_windows("GetModuleHandleW", error))?;
        let signals = Rc::new(Signals {
            hint: config.hint_text.clone(),
            ..Signals::default()
        });
        // 窗口拿走一个 Rc,worker 手上留一个。裸指针只是把它送进 wndproc 的路。
        let carried = Box::into_raw(Box::new(Rc::clone(&signals)));
        let before = Rc::strong_count(&signals);
        let mut title: Vec<u16> = config.title.encode_utf16().collect();
        title.push(0);
        // SAFETY: 类已注册;那个 Rc 的所有权由 WM_NCDESTROY 回收,建窗失败且
        // WM_NCCREATE 没跑过时由下面这段回收。
        let created = unsafe {
            CreateWindowExW(
                Default::default(),
                LOGIN_CLASS,
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(module.into()),
                Some(carried.cast()),
            )
        };
        let hwnd = match created {
            Ok(hwnd) => hwnd,
            Err(error) => {
                if Rc::strong_count(&signals) == before {
                    // 计数没变 = WM_NCCREATE 没跑过 = 没人接手,这里是唯一的所有者。
                    // SAFETY: carried 来自 Box::into_raw,还没被任何人释放。
                    unsafe { drop(Box::from_raw(carried)) };
                }
                return Err(error_from_windows("CreateWindowExW(login)", error));
            }
        };
        let mut window = Self {
            hwnd,
            signals,
            controller: None,
            webview: None,
        };
        window.place()?;
        window.show();
        Ok(window)
    }

    /// 按当前 dpi 把窗口摆到工作区正中。
    fn place(&mut self) -> Result<(), PlatformError> {
        let rect = login_geometry(work_area()?, self.dpi());
        // SAFETY: hwnd 存活。
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .map_err(|error| error_from_windows("SetWindowPos(login)", error))
    }

    fn dpi(&self) -> u32 {
        // SAFETY: hwnd 存活;窗口还没上屏时返回主显示器 dpi。
        match unsafe { GetDpiForWindow(self.hwnd) } {
            0 => 96,
            value => value,
        }
    }

    fn show(&self) {
        // SAFETY: hwnd 存活。这个窗口该抢焦点 —— 用户刚点了"登录官网",
        // 下一秒就要在里面打字。抢的是本程序自己的窗口,不动游戏。
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    /// 让 WebView2 填满提示带以下的客户区。
    fn layout(&self) {
        let Some(controller) = &self.controller else {
            return;
        };
        let mut client = RECT::default();
        // SAFETY: hwnd 存活,client 是合法出参。
        let _ = unsafe { GetClientRect(self.hwnd, &mut client) };
        let strip = scale_for_dpi(STRIP_HEIGHT, self.dpi());
        let bounds = RECT {
            left: client.left,
            top: (client.top + strip).min(client.bottom),
            right: client.right,
            bottom: client.bottom,
        };
        // SAFETY: controller 归本线程所有。
        unsafe {
            let _ = controller.SetBounds(bounds);
            let _ = controller.SetIsVisible(true);
        }
    }

    fn navigate(&self, url: &str) {
        let Some(webview) = &self.webview else {
            return;
        };
        let mut wide: Vec<u16> = url.encode_utf16().collect();
        wide.push(0);
        // SAFETY: wide 以 NUL 结尾且活到调用返回。
        unsafe {
            let _ = webview.Navigate(PCWSTR(wide.as_ptr()));
        }
    }
}

impl Drop for LoginWindow {
    fn drop(&mut self) {
        // 顺序要紧:先请 WebView2 收摊,再销毁窗口。反过来的话控件会对着一个
        // 已经没了的父窗口继续跑,退出时挂在那儿。
        if let Some(controller) = self.controller.take() {
            // SAFETY: controller 归本线程所有,至多关一次。
            let _ = unsafe { controller.Close() };
        }
        self.webview = None;
        // SAFETY: 本类型不跨线程,窗口至多销毁一次。
        if unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

fn ensure_class() -> Result<(), PlatformError> {
    CLASS_READY
        .get_or_init(|| {
            // SAFETY: 无前置条件。
            let module = unsafe { GetModuleHandleW(None) }
                .map_err(|error| error_from_windows("GetModuleHandleW", error))?;
            // SAFETY: 实例为 null 时取系统预定义光标。
            let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                hInstance: module.into(),
                lpszClassName: LOGIN_CLASS,
                hCursor: cursor,
                ..Default::default()
            };
            // SAFETY: WNDCLASSEXW 只指向静态类数据。
            if unsafe { RegisterClassExW(&class) } == 0 {
                return Err(error_from_windows(
                    "RegisterClassExW(login)",
                    windows::core::Error::from_win32(),
                ));
            }
            Ok(())
        })
        .clone()
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: WM_NCCREATE 的 lparam 就是 CREATESTRUCTW。
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        let carried = create.lpCreateParams.cast::<Rc<Signals>>();
        if !carried.is_null() {
            // SAFETY: 把所有权交给窗口;从此由 WM_NCDESTROY 负责释放。
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, carried as isize) };
        }
        // SAFETY: 普通窗口的非客户区还要系统自己初始化一遍,不能像卡片那样
        // 直接返回 1 —— 那样标题栏和边框都画不出来。
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: 读本窗口的 userdata 槽。
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Rc<Signals>;
    if message == WM_NCDESTROY {
        // SAFETY: 先让系统跑完默认销毁,再回收 Box。
        let result = unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        if !pointer.is_null() {
            // SAFETY: 窗口正在消失,这是最后一次触碰 signals。
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(pointer));
            }
        }
        return result;
    }
    if pointer.is_null() {
        // SAFETY: WM_NCCREATE 之前的少数消息没有 signals 可用。
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: 非空指针指向本线程独占的、仍然存活的 Rc<Signals>。
    let signals: &Signals = unsafe { &*pointer };
    match message {
        WM_PAINT => {
            paint_strip(hwnd, signals);
            LRESULT(0)
        }
        // 客户区自己重画,交给系统擦一遍只会闪。
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            signals.resized.set(true);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // 拖到另一块缩放不同的屏上:lparam 是系统建议的新矩形,照办。
            // SAFETY: WM_DPICHANGED 的 lparam 就是一个 RECT。
            let suggested = unsafe { &*(lparam.0 as *const RECT) };
            // SAFETY: hwnd 存活。
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            signals.resized.set(true);
            LRESULT(0)
        }
        // 不在这里 DestroyWindow:WebView2 得先收摊,那是 worker 的事。
        WM_CLOSE => {
            signals.close_requested.set(true);
            LRESULT(0)
        }
        // SAFETY: 其余消息交回系统默认处理。
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// 顶上那条提示带。窗口剩下的部分被 WebView2 的子窗口盖着,不用画。
fn paint_strip(hwnd: HWND, signals: &Signals) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: BeginPaint/EndPaint 在本函数内配对。
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    let mut client = RECT::default();
    // SAFETY: hwnd 存活,client 是合法出参。
    let _ = unsafe { GetClientRect(hwnd, &mut client) };
    // SAFETY: hwnd 存活;窗口还没上屏时返回主显示器 dpi。
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        value => value,
    };
    let strip_height = scale_for_dpi(STRIP_HEIGHT, dpi);

    let panel = Brush::new(PANEL);
    let rail = Brush::new(RAIL);
    let hairline = Brush::new(HAIRLINE);
    let strip = RECT {
        left: client.left,
        top: client.top,
        right: client.right,
        bottom: (client.top + strip_height).min(client.bottom),
    };
    let seam = RECT {
        left: client.left,
        top: (strip.bottom - 1).max(client.top),
        right: client.right,
        bottom: strip.bottom,
    };
    // SAFETY: 画刷在本次绘制内存活,dc 有效。
    unsafe {
        // 控件建出来之前整块客户区都是空的:先铺底,免得第一帧是白的。
        FillRect(dc, &client, panel.0);
        FillRect(dc, &strip, rail.0);
        FillRect(dc, &seam, hairline.0);
        SetBkMode(dc, TRANSPARENT);
    }
    let font = Font::new(scale_for_dpi(HINT_FONT_SIZE, dpi), FW_NORMAL.0 as i32);
    let pad = scale_for_dpi(PAD_X, dpi);
    draw_text(
        dc,
        font.0,
        TEXT_SECONDARY,
        &signals.hint,
        RECT {
            left: client.left + pad,
            top: strip.top,
            right: client.right - pad,
            bottom: strip.bottom,
        },
        DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS,
    );
    // SAFETY: 与上面的 BeginPaint 配对。
    unsafe {
        let _ = EndPaint(hwnd, &paint);
    }
}
