//! 托盘图标的 Win32 实现:一条自己的线程 + 一个从来不显示的顶层窗口。
//!
//! 为什么要一条独立线程:和卡片同一个理由 —— 窗口是"线程绑定"的,谁建的窗口
//! 就得由谁泵消息。托盘图标的点击是通过窗口消息回来的,挂在 GPUI 主线程上的话,
//! 界面一忙(重建一张几百行的表)托盘就点不动;而托盘是窗口藏起来之后唯一能
//! 把它拿回来的地方,不能跟着界面一起卡。
//!
//! 为什么**不用** message-only 窗口(`HWND_MESSAGE`):explorer 崩了重启之后
//! 通知区是空的,系统靠广播一条 `TaskbarCreated` 让每个程序重新加一次图标 ——
//! 而广播消息只发给**顶层**窗口,message-only 窗口收不到,图标就再也回不来了。
//! 所以这里建的是一个正常的顶层窗口,只是从头到尾没有 `ShowWindow` 过,
//! 而且带 `WS_EX_TOOLWINDOW`(不进 Alt+Tab、不进任务栏)。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICON_VERSION_4,
    NOTIFYICONDATAW, NOTIFYICONDATAW_0, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, HICON, IDC_ARROW, IDI_APPLICATION, IMAGE_ICON, IsIconic, IsWindow,
    LR_SHARED, LoadCursorW, LoadIconW, LoadImageW, MF_STRING, MSG, PM_NOREMOVE, PeekMessageW,
    PostMessageW, PostThreadMessageW, RegisterClassExW, RegisterWindowMessageW, SM_CXSMICON,
    SM_CYSMICON, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP,
    WM_NCCREATE, WM_NCDESTROY, WM_NULL, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use super::error_from_windows;
use crate::PlatformError;
use crate::tray::{
    MENU_OPEN_ID, MENU_QUIT_ID, TRAY_ICON_ID, TrayClick, TrayConfig, TrayError, TrayEvent,
    TrayShared, decode_tray_callback, menu_command,
};

const TRAY_CLASS: PCWSTR = w!("PndTrayIcon");
const TRAY_TITLE: PCWSTR = w!("POE Ninja Data tray");
/// 托盘图标的回调消息号。挑在 `WM_APP` 之上,和卡片那条错开。
const WM_TRAY_CALLBACK: u32 = WM_APP + 0x2A2;
/// 请托盘线程收摊的线程消息。
const WM_TRAY_SHUTDOWN: u32 = WM_APP + 0x2A3;
/// exe 资源段里图标的 id。
///
/// `pnd-app` 的 build.rs 把 `assets/icon.ico` 放在 1 上(gpui 找窗口图标时
/// 写死的也是这个 1),所以托盘照同一个 id 取:窗口左上角、任务栏、通知区
/// 三处于是永远是同一张图。
const ICON_RESOURCE_ID: usize = 1;

static CLASS_READY: OnceLock<Result<(), TrayError>> = OnceLock::new();

pub(crate) fn spawn_tray_worker(
    config: TrayConfig,
    shared: Arc<TrayShared>,
    events: mpsc::Sender<TrayEvent>,
    ready: mpsc::SyncSender<Result<String, TrayError>>,
) -> Result<JoinHandle<()>, TrayError> {
    thread::Builder::new()
        .name("pnd-tray".to_owned())
        .spawn(move || {
            worker_thread(config, Arc::clone(&shared), events, ready);
            shared.set_native_thread_id(0);
        })
        .map_err(|error| TrayError::Thread(format!("could not start pnd-tray: {error}")))
}

/// 请托盘线程收摊。线程 id 是 0 就说明它已经不在了。
pub(crate) fn stop_tray(thread_id: u32) -> Result<(), TrayError> {
    if thread_id == 0 {
        return Err(TrayError::Disconnected);
    }
    // SAFETY: 线程 id 只在消息队列建好之后才被公布。
    unsafe { PostThreadMessageW(thread_id, WM_TRAY_SHUTDOWN, WPARAM(0), LPARAM(0)) }
        .map_err(|error| TrayError::Platform(error_from_windows("PostThreadMessageW(tray)", error)))
}

/// 藏起 / 拿回主窗口。
///
/// 由调用线程直接做,不绕托盘线程一圈:两处调用点都在 GPUI 主线程上,
/// 而主窗口本来就是它建的 —— 自己的窗口自己 `ShowWindow`,最短也最稳。
pub(crate) fn show_main_window(hwnd: isize, visible: bool) -> Result<(), TrayError> {
    let window = HWND(hwnd as *mut core::ffi::c_void);
    if !visible {
        // SAFETY: hwnd 由调用方从 gpui 的窗口句柄取来,SW_HIDE 不激活任何窗口。
        unsafe {
            let _ = ShowWindow(window, SW_HIDE);
        }
        return Ok(());
    }
    // SAFETY: 同上。三步都只作用在这一个窗口上。
    unsafe {
        let _ = ShowWindow(window, SW_SHOW);
        // 藏起来之前如果是最小化的,SW_SHOW 会把它原样恢复成"最小化且可见",
        // 也就是屏幕上还是什么都没有。SW_RESTORE 才是"变回一个看得见的窗口"。
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        let _ = SetForegroundWindow(window);
    }
    Ok(())
}

fn worker_thread(
    config: TrayConfig,
    shared: Arc<TrayShared>,
    events: mpsc::Sender<TrayEvent>,
    ready: mpsc::SyncSender<Result<String, TrayError>>,
) {
    let mut queue_message = MSG::default();
    // SAFETY: 一次 PeekMessageW 就能逼 Windows 给本线程建出消息队列,必须发生在
    // 公布线程 id 之前,否则 PostThreadMessageW 会丢消息。
    let _ = unsafe { PeekMessageW(&mut queue_message, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: GetCurrentThreadId 没有前置条件。
    shared.set_native_thread_id(unsafe { GetCurrentThreadId() });

    let window = match TrayWindow::create(&config, events) {
        Ok(window) => window,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _ = ready.send(Ok(window.icon_source.clone()));

    let mut message = MSG::default();
    loop {
        // SAFETY: 本线程自己的标准消息循环。
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        if message.message == WM_TRAY_SHUTDOWN {
            break;
        }
        // SAFETY: message 刚由 GetMessageW 填好。
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    // 图标和窗口都归本线程:显式 drop,不指望作用域末尾。
    drop(window);
}

/// wndproc 和托盘线程共用的状态。所有权在 `WM_NCCREATE` 交给窗口,
/// `WM_NCDESTROY` 时释放 —— 和提醒卡片同一套握手。
struct TrayContext {
    native_ownership_claimed: Arc<AtomicBool>,
    /// 事件直接由 wndproc 发出去。卡片那边要先记在 context 上再由 worker 取走,
    /// 是因为 worker 还管着声音和计时器;托盘没有那些东西,少一道转手。
    events: mpsc::Sender<TrayEvent>,
    icon: HICON,
    tooltip: [u16; 128],
    menu_open: Vec<u16>,
    menu_quit: Vec<u16>,
    /// `RegisterWindowMessageW("TaskbarCreated")` 的消息号,0 = 注册不上。
    taskbar_created: u32,
}

struct TrayWindow {
    hwnd: HWND,
    icon_source: String,
}

impl TrayWindow {
    fn create(config: &TrayConfig, events: mpsc::Sender<TrayEvent>) -> Result<Self, TrayError> {
        ensure_class()?;
        // SAFETY: 取本模块的 HINSTANCE,无前置条件。
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|error| platform(error_from_windows("GetModuleHandleW", error)))?;
        let (icon, icon_source) = load_tray_icon(module.into());
        let claimed = Arc::new(AtomicBool::new(false));
        let context = Box::new(TrayContext {
            native_ownership_claimed: Arc::clone(&claimed),
            events,
            icon,
            tooltip: tip_buffer(&config.tooltip),
            menu_open: wide(&config.menu_open),
            menu_quit: wide(&config.menu_quit),
            // SAFETY: 无指针参数;注册不上时返回 0,下面按 0 处理。
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        });
        let context_pointer = Box::into_raw(context);
        // TOOLWINDOW:这个窗口永远不显示,但万一哪天被显示出来,也不该出现在
        // 任务栏和 Alt+Tab 里。
        // SAFETY: 类已注册;TrayContext 指针由 WM_NCDESTROY 回收,建窗失败且
        // WM_NCCREATE 没跑过时由下面这段回收。
        let created = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                TRAY_CLASS,
                TRAY_TITLE,
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(module.into()),
                Some(context_pointer.cast()),
            )
        };
        let hwnd = match created {
            Ok(hwnd) => hwnd,
            Err(error) => {
                if !claimed.load(Ordering::Acquire) {
                    // SAFETY: WM_NCCREATE 没跑过就没人接手,这里是唯一的所有者。
                    unsafe { drop(Box::from_raw(context_pointer)) };
                }
                return Err(platform(error_from_windows("CreateWindowExW(tray)", error)));
            }
        };
        // 先造出来,这样下面任何一步失败,Drop 都会把窗口收干净。
        let window = Self { hwnd, icon_source };
        // 窗口有了而图标没加上,等于"缩到托盘"会把主窗口藏进一个拿不回来的
        // 地方 —— 所以这一步失败要整个失败,不是留一条警告。
        // SAFETY: CreateWindowExW 成功即 WM_NCCREATE 跑过,context 仍然存活,
        // 而且只被本线程使用。
        unsafe { add_tray_icon(hwnd, &*context_pointer) }.map_err(platform)?;
        Ok(window)
    }
}

impl Drop for TrayWindow {
    fn drop(&mut self) {
        // SAFETY: 本类型不跨线程,窗口至多销毁一次。
        if unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            remove_tray_icon(self.hwnd);
            // SAFETY: 同上。
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

/// 把图标挂进通知区。explorer 重启后的重新挂载走的也是这一句。
fn add_tray_icon(hwnd: HWND, context: &TrayContext) -> Result<(), PlatformError> {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: context.icon,
        szTip: context.tooltip,
        ..Default::default()
    };
    // SAFETY: data 完整填写,且在调用期间存活。
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        return Err(error_from_windows(
            "Shell_NotifyIconW(NIM_ADD)",
            windows::core::Error::from_win32(),
        ));
    }
    // 必须紧接着声明版本 4:只有那一版才把鼠标坐标放进 wparam、把消息号放进
    // lparam 的低 16 位,而 `decode_tray_callback` 就是照这套约定解的。
    data.Anonymous = NOTIFYICONDATAW_0 {
        uVersion: NOTIFYICON_VERSION_4,
    };
    // SAFETY: 同上。声明失败只意味着退回旧约定,不值得让整个服务起不来。
    let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) };
    Ok(())
}

/// 摘掉图标。不摘的话进程结束后通知区里会留一个点了没反应的僵尸图标,
/// 要鼠标划过去才消失。
fn remove_tray_icon(hwnd: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    // SAFETY: NIM_DELETE 只看 hWnd + uID,别的字段是零也无所谓。
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

/// 托盘图标从哪儿来。
///
/// 先按 exe 资源段里的 [`ICON_RESOURCE_ID`] 取小尺寸的那一张(通知区要的就是
/// 16×16 那一档),取不到再用 `LoadIconW` 试一次,还不行才退回系统的默认程序
/// 图标 —— 通知区上有个图标总比没有强,那是窗口藏起来之后唯一能点回来的地方。
///
/// 返回的那句话会一路报到调用方:图标悄悄退成系统默认,只会让人以为"本来就
/// 长这样"。
fn load_tray_icon(module: HINSTANCE) -> (HICON, String) {
    // SAFETY: GetSystemMetrics 无前置条件;取不到时返回 0,下面兜到 16。
    let width = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    // SAFETY: 同上。
    let height = unsafe { GetSystemMetrics(SM_CYSMICON) }.max(16);
    let name = PCWSTR(ICON_RESOURCE_ID as *const u16);
    // LR_SHARED 取到的图标由系统缓存,不必也不该 DestroyIcon。
    // SAFETY: name 是 MAKEINTRESOURCE 形式的资源 id,不是真指针 —— 这是
    // LoadImageW 认的两种写法之一。
    if let Ok(handle) =
        unsafe { LoadImageW(Some(module), name, IMAGE_ICON, width, height, LR_SHARED) }
    {
        return (
            HICON(handle.0),
            format!("module resource id {ICON_RESOURCE_ID} ({width}x{height})"),
        );
    }
    // SAFETY: 同上。
    if let Ok(icon) = unsafe { LoadIconW(Some(module), name) } {
        return (
            icon,
            format!("module resource id {ICON_RESOURCE_ID} (LoadIconW)"),
        );
    }
    // SAFETY: 实例为 None 时取系统预定义图标。
    let fallback = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    (fallback, "IDI_APPLICATION (system default)".to_owned())
}

/// 右键菜单:两条,文字由上层给(双语目录在 `pnd-app`)。
fn show_tray_menu(hwnd: HWND, context: &TrayContext, x: i32, y: i32) {
    // SAFETY: 无前置条件。
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    // SAFETY: 两个标题都是本 context 里 NUL 结尾的宽串,活到本函数结束。
    unsafe {
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_OPEN_ID,
            PCWSTR(context.menu_open.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_QUIT_ID,
            PCWSTR(context.menu_quit.as_ptr()),
        );
    }
    // 不先把自己拉到前台,菜单会在用户点别处之后赖着不走(这是 Shell 那条
    // 三十年的老毛病,官方文档自己写着这个变通)。
    // SAFETY: hwnd 是本进程自己的窗口。
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
    // TPM_RETURNCMD:选中哪条直接当返回值拿回来,不用再接一条 WM_COMMAND。
    // SAFETY: menu 有效,hwnd 存活。
    let chosen = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            x,
            y,
            None,
            hwnd,
            None,
        )
    };
    // 同一条老毛病的另一半:菜单收起来之后给自己补一条消息,否则下一次
    // 右键可能弹不出来。
    // SAFETY: 往自己的队列里投一条空消息。
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
    if let Some(event) = menu_command(chosen.0 as usize) {
        let _ = context.events.send(event);
    }
}

fn ensure_class() -> Result<(), TrayError> {
    CLASS_READY
        .get_or_init(|| {
            // SAFETY: 无前置条件。
            let module = unsafe { GetModuleHandleW(None) }
                .map_err(|error| platform(error_from_windows("GetModuleHandleW", error)))?;
            // SAFETY: 实例为 None 时取系统预定义光标。
            let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(window_proc),
                hInstance: module.into(),
                lpszClassName: TRAY_CLASS,
                hCursor: cursor,
                ..Default::default()
            };
            // SAFETY: WNDCLASSEXW 只指向静态类数据。
            if unsafe { RegisterClassExW(&class) } == 0 {
                return Err(platform(error_from_windows(
                    "RegisterClassExW(tray)",
                    windows::core::Error::from_win32(),
                )));
            }
            Ok(())
        })
        .clone()
}

fn platform(error: PlatformError) -> TrayError {
    TrayError::Platform(error)
}

/// 提示文字装进 `szTip` 那 128 个 u16,超长的截断 —— 系统本来也只显示这么多,
/// 而末尾那一格必须留给 NUL。
fn tip_buffer(value: &str) -> [u16; 128] {
    let mut buffer = [0_u16; 128];
    for (slot, unit) in buffer.iter_mut().take(127).zip(value.encode_utf16()) {
        *slot = unit;
    }
    buffer
}

/// NUL 结尾的宽串。菜单标题要的就是这个。
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
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
        let context = create.lpCreateParams.cast::<TrayContext>();
        if context.is_null() {
            return LRESULT(0);
        }
        // SAFETY: 指针来自 Box::into_raw,尚未被任何人释放。
        unsafe { &*context }
            .native_ownership_claimed
            .store(true, Ordering::Release);
        // SAFETY: 把所有权交给窗口;从此由 WM_NCDESTROY 负责释放。
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, context as isize) };
        return LRESULT(1);
    }
    // SAFETY: 读本窗口的 userdata 槽。
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TrayContext;
    if message == WM_NCDESTROY {
        // SAFETY: 先让系统跑完默认销毁,再回收 Box。
        let result = unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        if !pointer.is_null() {
            // SAFETY: 窗口正在消失,这是最后一次触碰 context。
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(pointer));
            }
        }
        return result;
    }
    if pointer.is_null() {
        // SAFETY: WM_NCCREATE 之前的少数消息没有 context 可用。
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: 非空指针指向本线程独占的、仍然存活的 TrayContext。
    let context = unsafe { &*pointer };

    if message == WM_TRAY_CALLBACK {
        match decode_tray_callback(wparam.0, lparam.0) {
            Some(TrayClick::Select) => {
                let _ = context.events.send(TrayEvent::Restore);
            }
            Some(TrayClick::Menu { x, y }) => show_tray_menu(hwnd, context, x, y),
            None => {}
        }
        return LRESULT(0);
    }
    // explorer 崩了重启之后通知区是空的,只有这一条广播能告诉我们该重新挂一次。
    if context.taskbar_created != 0 && message == context.taskbar_created {
        let _ = add_tray_icon(hwnd, context);
        return LRESULT(0);
    }
    // SAFETY: 其余消息交回系统默认处理。
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
