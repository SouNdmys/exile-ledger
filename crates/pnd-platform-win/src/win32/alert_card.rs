//! 提醒小卡片的 Win32 实现:一条自己的线程 + 一个绝不抢焦点的置顶弹窗。
//!
//! 为什么要一条独立线程:窗口是"线程绑定"的,谁建的窗口就得由谁泵消息。把它
//! 放在 GPUI 的主线程上,界面一卡卡片就不动了;放在自己线程上,主界面最小化、
//! 忙着画表格,卡片照样弹、照样响。
//!
//! 为什么它不像 POE-Alarm 那个红警报:那个要拦住你手滑的点击,所以铺满整个桌面
//! 并吃掉鼠标;这张卡片只是提醒,**只有卡片自己那 420×180 吃点击**,别的地方
//! 该点游戏点游戏。所以这里没有全屏层、没有鼠标钩子、没有确认组合键。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush,
    DEFAULT_CHARSET, DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE,
    DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FW_NORMAL, FW_SEMIBOLD, FillRect, FrameRect,
    HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT, ScreenToClient,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWL_EXSTYLE, GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW,
    GetWindowRect, HTCAPTION, HTCLIENT, HWND_TOPMOST, IDC_ARROW, IsWindow, KillTimer, LWA_ALPHA,
    LoadCursorW, MA_NOACTIVATE, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    RegisterClassExW, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetLayeredWindowAttributes,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage,
    WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND,
    WM_EXITSIZEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCREATE,
    WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use super::error_from_windows;
use crate::PlatformError;
use crate::alert_card::{
    CardButton, CardCommand, CardConfig, CardError, CardEvent, CardOwnership, CardShared,
    CardStyle, CardText, RectI, button_rects, card_geometry_for_corner, hit_button, scale_for_dpi,
};
use crate::wave::LoopingWavePlayer;

const CARD_CLASS: PCWSTR = w!("PndAlertCard");
const CARD_TITLE: PCWSTR = w!("POE Ninja Data alert");
/// 唤醒消息:命令本身在 `CardShared` 的队列里,这条消息只负责把线程从
/// `GetMessageW` 里叫醒。
const WM_CARD_COMMANDS: u32 = WM_APP + 0x2A1;
/// 自动收起计时器 id("PNDC")。
const AUTO_HIDE_TIMER_ID: usize = 0x504E_4443;
/// `WM_MOUSELEAVE`。它在 commctrl.h 里,windows-rs 因此把它放进了
/// `Win32_UI_Controls` feature;为一个常量拉进一整套控件绑定不值得。
const WM_MOUSELEAVE: u32 = 0x02A3;

/// 标题条高度(96 dpi 逻辑像素)。这一条返回 `HTCAPTION`,所以卡片可以拖。
const STRIP_HEIGHT: i32 = 30;
/// 正文左右内边距。
const PAD_X: i32 = 12;
const LINE1_TOP: i32 = 38;
const LINE2_TOP: i32 = 62;
const LINE_HEIGHT: i32 = 24;
const FOOTER_TOP: i32 = 92;
const FOOTER_HEIGHT: i32 = 20;
const SEPARATOR_TOP: i32 = 126;

static CLASS_READY: OnceLock<Result<(), CardError>> = OnceLock::new();

pub(crate) fn spawn_card_worker(
    config: CardConfig,
    shared: Arc<CardShared>,
    events: mpsc::Sender<CardEvent>,
    ready: mpsc::SyncSender<Result<(), CardError>>,
    ownership: CardOwnership,
) -> Result<JoinHandle<()>, CardError> {
    thread::Builder::new()
        .name("pnd-alert-card".to_owned())
        .spawn(move || {
            worker_thread(config, Arc::clone(&shared), events, ready);
            shared.set_native_thread_id(0);
            // 名额最后才归还:归还早了,下一个 start() 可能撞上还没销毁的窗口。
            drop(ownership);
        })
        .map_err(|error| CardError::Thread(format!("could not start pnd-alert-card: {error}")))
}

/// 把卡片线程从 `GetMessageW` 里叫醒,让它去队列里取命令。
pub(crate) fn wake_card(thread_id: u32) -> Result<(), CardError> {
    if thread_id == 0 {
        return Err(CardError::Disconnected);
    }
    // SAFETY: 线程 id 只在消息队列建好之后才被公布。
    unsafe { PostThreadMessageW(thread_id, WM_CARD_COMMANDS, WPARAM(0), LPARAM(0)) }.map_err(
        |error| CardError::Platform(error_from_windows("PostThreadMessageW(alert card)", error)),
    )
}

fn worker_thread(
    config: CardConfig,
    shared: Arc<CardShared>,
    events: mpsc::Sender<CardEvent>,
    ready: mpsc::SyncSender<Result<(), CardError>>,
) {
    // 只把**这条线程**设成 per-monitor DPI 感知,不动整个进程:
    // 不这么做的话,在 150% 缩放的屏幕上 GetDpiForWindow 永远返回 96,卡片会被
    // 系统按位图拉大——尺寸对,但字是糊的。线程跑完就结束,不需要恢复。
    // SAFETY: SetThreadDpiAwarenessContext 只影响当前线程,无指针参数。
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let mut queue_message = MSG::default();
    // SAFETY: 一次 PeekMessageW 就能逼 Windows 给本线程建出消息队列,
    // 必须发生在公布线程 id 之前,否则 PostThreadMessageW 会丢消息。
    let _ = unsafe { PeekMessageW(&mut queue_message, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: GetCurrentThreadId 没有前置条件。
    shared.set_native_thread_id(unsafe { GetCurrentThreadId() });

    let style = config.style();
    let window = match CardWindow::create(style) {
        Ok(window) => window,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut worker = CardWorker {
        window,
        player: config.sound.map(LoopingWavePlayer::new),
        events,
        shared: Arc::clone(&shared),
        style,
        active: None,
    };
    let _ = ready.send(Ok(()));

    let mut message = MSG::default();
    loop {
        // SAFETY: 本线程自己的标准消息循环。
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        if message.message == WM_CARD_COMMANDS {
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
        worker.poll_window_signals();
    }
    worker.shutdown();
}

struct CardWorker {
    window: CardWindow,
    player: Option<LoopingWavePlayer>,
    events: mpsc::Sender<CardEvent>,
    shared: Arc<CardShared>,
    style: CardStyle,
    /// 当前卡片对应的提醒 id;卡片收起后为 `None`。
    active: Option<i64>,
}

impl CardWorker {
    /// 返回 true 表示收到了 Shutdown。
    fn drain_commands(&mut self) -> bool {
        while let Some(command) = self.shared.pop() {
            match command {
                CardCommand::Show { alert_id, text } => self.present(alert_id, text, true),
                CardCommand::Update { alert_id, text } => {
                    // 卡片已经收起时,"原地更新"没有原地可言:当成一次新提醒。
                    let fresh = self.active.is_none();
                    self.present(alert_id, text, fresh);
                }
                CardCommand::Hide => self.dismiss(),
                CardCommand::SetStyle(style) => self.apply_style(style),
                CardCommand::Shutdown => return true,
            }
        }
        false
    }

    fn present(&mut self, alert_id: i64, text: CardText, fresh: bool) {
        let was_visible = self.window.visible;
        if let Some(context) = self.window.context_mut() {
            context.text = text;
            if fresh {
                // 新提醒:上一张卡片残留的按下/悬停状态不该跟着过来。
                context.pressed = None;
                context.hovered = None;
            }
        }
        // 只有从"收起"变"显示"才回到角落:用户把卡片拖到别处是有意为之,
        // 来一条新挂单就把它拽回右下角,等于把他的调整撤销掉。
        if !was_visible && let Err(error) = self.window.place(self.style) {
            self.warn(error.to_string());
        }
        if let Err(error) = self.window.show() {
            self.warn(error.to_string());
        }
        if let Some(warning) = self.window.passive_style_warning() {
            self.warn(warning);
        }
        self.window.arm_auto_hide(self.style.auto_hide);
        self.active = Some(alert_id);
        if fresh {
            self.start_sound();
        }
        self.window.repaint();
    }

    fn dismiss(&mut self) {
        self.stop_sound();
        self.window.cancel_auto_hide();
        self.window.hide();
        self.active = None;
    }

    fn apply_style(&mut self, style: CardStyle) {
        self.style = style;
        if let Err(error) = self.window.set_opacity(style.opacity) {
            self.warn(error.to_string());
        }
        if self.window.visible {
            if let Err(error) = self.window.place(style) {
                self.warn(error.to_string());
            }
            self.window.arm_auto_hide(style.auto_hide);
            self.window.repaint();
        }
    }

    /// 把 wndproc 留下的"留言"取走。wndproc 够不到 worker 的字段(它只有一个
    /// 裸指针),所以点击、拖动、超时都是先记在 `WindowContext` 上,再由这里读走。
    fn poll_window_signals(&mut self) {
        let (clicked, moved, auto_hidden, replace) = match self.window.context_mut() {
            Some(context) => (
                context.clicked.take(),
                context.moved.take(),
                std::mem::take(&mut context.auto_hide_fired),
                std::mem::take(&mut context.replace_requested),
            ),
            None => return,
        };
        if let Some(button) = clicked {
            // 任意按钮都停声——包括"打开交易页":你已经看见它了。
            self.stop_sound();
            if let Some(alert_id) = self.active {
                self.send(CardEvent::Clicked { alert_id, button });
            }
        }
        if let Some((x, y)) = moved {
            self.send(CardEvent::Moved { x, y });
        }
        if auto_hidden {
            let alert_id = self.active;
            self.dismiss();
            if let Some(alert_id) = alert_id {
                self.send(CardEvent::AutoHidden { alert_id });
            }
        }
        if replace && self.window.visible {
            // 分辨率或 dpi 变了:原来的角落坐标可能已经在屏幕外。
            if let Err(error) = self.window.place(self.style) {
                self.warn(error.to_string());
            }
            self.window.repaint();
        }
    }

    fn start_sound(&mut self) {
        if let Some(player) = self.player.as_mut()
            && let Err(error) = player.start()
        {
            let _ = self.events.send(CardEvent::Warning(error.to_string()));
        }
    }

    fn stop_sound(&mut self) {
        if let Some(player) = self.player.as_mut()
            && let Err(error) = player.stop()
        {
            let _ = self.events.send(CardEvent::Warning(error.to_string()));
        }
    }

    fn warn(&self, detail: String) {
        self.send(CardEvent::Warning(detail));
    }

    fn send(&self, event: CardEvent) {
        let _ = self.events.send(event);
    }

    fn shutdown(&mut self) {
        self.stop_sound();
        self.window.cancel_auto_hide();
        self.window.hide();
        self.active = None;
    }
}

/// wndproc 和 worker 共用的状态。所有权在 `WM_NCCREATE` 交给窗口,
/// `WM_NCDESTROY` 时释放——和 POE-Alarm 的警报窗同一套握手。
struct WindowContext {
    native_ownership_claimed: Arc<AtomicBool>,
    text: CardText,
    /// 客户区坐标下的四个按钮矩形;画和命中共用。
    buttons: [RectI; 4],
    strip_height: i32,
    dpi: u32,
    hovered: Option<usize>,
    pressed: Option<usize>,
    tracking_leave: bool,
    clicked: Option<CardButton>,
    moved: Option<(i32, i32)>,
    auto_hide_fired: bool,
    replace_requested: bool,
}

impl WindowContext {
    fn placeholder(native_ownership_claimed: Arc<AtomicBool>) -> Self {
        Self {
            native_ownership_claimed,
            text: CardText::default(),
            buttons: [RectI::default(); 4],
            strip_height: STRIP_HEIGHT,
            dpi: 96,
            hovered: None,
            pressed: None,
            tracking_leave: false,
            clicked: None,
            moved: None,
            auto_hide_fired: false,
            replace_requested: false,
        }
    }
}

struct CardWindow {
    hwnd: HWND,
    visible: bool,
    auto_hide_armed: bool,
}

impl CardWindow {
    fn create(style: CardStyle) -> Result<Self, CardError> {
        ensure_class()?;
        // SAFETY: 取本模块的 HINSTANCE,无前置条件。
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|error| platform(error_from_windows("GetModuleHandleW", error)))?;
        let claimed = Arc::new(AtomicBool::new(false));
        let context = Box::new(WindowContext::placeholder(Arc::clone(&claimed)));
        let context_pointer = Box::into_raw(context);
        // TOPMOST 置顶、TOOLWINDOW 不进 Alt+Tab、NOACTIVATE 点了也不抢焦点
        // (游戏不会因此失焦)、LAYERED 让整卡有统一透明度。
        let extended = WINDOW_EX_STYLE(
            WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_LAYERED.0,
        );
        // SAFETY: 类已注册;WindowContext 指针由 WM_NCDESTROY 回收,
        // 建窗失败且 WM_NCCREATE 没跑过时由下面这段回收。
        let created = unsafe {
            CreateWindowExW(
                extended,
                CARD_CLASS,
                CARD_TITLE,
                WS_POPUP,
                0,
                0,
                1,
                1,
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
                return Err(platform(error_from_windows(
                    "CreateWindowExW(alert card)",
                    error,
                )));
            }
        };
        let window = Self {
            hwnd,
            visible: false,
            auto_hide_armed: false,
        };
        window.set_opacity(style.opacity).map_err(platform)?;
        Ok(window)
    }

    fn context_mut(&mut self) -> Option<&mut WindowContext> {
        // SAFETY: 指针由 WM_NCCREATE 写入、WM_NCDESTROY 清零,且只被本线程使用。
        let pointer = unsafe { GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) } as *mut WindowContext;
        if pointer.is_null() {
            None
        } else {
            // SAFETY: 非空即为 Box::into_raw 得到的、仍然存活的分配。
            Some(unsafe { &mut *pointer })
        }
    }

    /// 按当前 dpi 和角落重新摆放窗口,并把按钮矩形算好存进 context。
    fn place(&mut self, style: CardStyle) -> Result<(), PlatformError> {
        // SAFETY: hwnd 存活;窗口还没上屏时返回主显示器 dpi。
        let dpi = match unsafe { GetDpiForWindow(self.hwnd) } {
            0 => 96,
            value => value,
        };
        let work = work_area()?;
        let card = card_geometry_for_corner(work, dpi, style.corner);
        if let Some(context) = self.context_mut() {
            context.dpi = dpi;
            context.strip_height = scale_for_dpi(STRIP_HEIGHT, dpi);
            // WS_POPUP 没有边框,客户区就是整个窗口,所以按钮直接按 0,0 起算。
            context.buttons = button_rects(RectI::new(0, 0, card.w, card.h), dpi);
        }
        // SAFETY: hwnd 存活;NOACTIVATE 保证摆放不会把焦点抢过来。
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                card.x,
                card.y,
                card.w,
                card.h,
                SWP_NOACTIVATE,
            )
        }
        .map_err(|error| error_from_windows("SetWindowPos(place alert card)", error))
    }

    fn show(&mut self) -> Result<(), PlatformError> {
        // SAFETY: hwnd 存活;SW_SHOWNOACTIVATE 显示但不激活。
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        // 每次显示都重申一次置顶:别的程序可能在中间抢过 topmost。
        // SAFETY: 只改 Z 序,不动位置和大小。
        let result = unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE,
            )
        }
        .map_err(|error| error_from_windows("SetWindowPos(show alert card)", error));
        self.visible = true;
        result
    }

    fn hide(&mut self) {
        if !self.visible {
            return;
        }
        // SAFETY: hwnd 存活;SW_HIDE 不会激活任何窗口。
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.visible = false;
        if let Some(context) = self.context_mut() {
            context.hovered = None;
            context.pressed = None;
        }
    }

    fn set_opacity(&self, alpha: u8) -> Result<(), PlatformError> {
        // 整卡统一 alpha:逐像素 alpha 要走 UpdateLayeredWindow,就用不了
        // 普通的 WM_PAINT 了,为一张纯色卡片不值得。
        // SAFETY: hwnd 是本线程建的 WS_EX_LAYERED 顶层窗口。
        unsafe { SetLayeredWindowAttributes(self.hwnd, COLORREF(0), alpha, LWA_ALPHA) }
            .map_err(|error| error_from_windows("SetLayeredWindowAttributes(alert card)", error))
    }

    fn arm_auto_hide(&mut self, after: Duration) {
        self.cancel_auto_hide();
        if after.is_zero() {
            // 0 = 不自动收起(设置里可以关掉)。
            return;
        }
        let millis = after.as_millis().min(u128::from(u32::MAX)) as u32;
        // SAFETY: hwnd 存活;计时器只是给 wndproc 发 WM_TIMER。
        if unsafe { SetTimer(Some(self.hwnd), AUTO_HIDE_TIMER_ID, millis, None) } != 0 {
            self.auto_hide_armed = true;
        }
    }

    fn cancel_auto_hide(&mut self) {
        if self.auto_hide_armed {
            // SAFETY: 计时器由本窗口创建。
            let _ = unsafe { KillTimer(Some(self.hwnd), AUTO_HIDE_TIMER_ID) };
            self.auto_hide_armed = false;
        }
    }

    fn repaint(&self) {
        // SAFETY: hwnd 存活;失效区域会在本线程的下一轮消息里变成 WM_PAINT。
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// 显示之后确认三个"不打扰"的扩展样式还在。丢了也不致命(卡片照样能看),
    /// 但用户该知道:那意味着它可能不置顶,或者点一下会把游戏顶掉。
    fn passive_style_warning(&self) -> Option<String> {
        let required = WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0;
        // SAFETY: 读自己窗口的扩展样式。
        let extended = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32;
        if extended & required == required {
            return None;
        }
        Some(format!(
            "alert card lost its passive extended styles (0x{extended:08X}); \
             it may steal focus or stop staying on top"
        ))
    }
}

impl Drop for CardWindow {
    fn drop(&mut self) {
        // SAFETY: 本类型不跨线程,窗口至多销毁一次。
        if unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            self.cancel_auto_hide();
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

fn work_area() -> Result<RectI, PlatformError> {
    let mut rect = RECT::default();
    // SAFETY: SPI_GETWORKAREA 要求 pvparam 指向一个 RECT,这里正是。
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(std::ptr::from_mut(&mut rect).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .map_err(|error| error_from_windows("SystemParametersInfoW(SPI_GETWORKAREA)", error))?;
    Ok(RectI::new(
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

fn ensure_class() -> Result<(), CardError> {
    CLASS_READY
        .get_or_init(|| {
            // SAFETY: 无前置条件。
            let module = unsafe { GetModuleHandleW(None) }
                .map_err(|error| platform(error_from_windows("GetModuleHandleW", error)))?;
            // 不给类光标的话,鼠标划过卡片时会保留上一个窗口设的形状
            // (比如游戏里的忙碌指针),看着像卡死。
            // SAFETY: 实例为 null 时取系统预定义光标。
            let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                hInstance: module.into(),
                lpszClassName: CARD_CLASS,
                hCursor: cursor,
                ..Default::default()
            };
            // SAFETY: WNDCLASSEXW 只指向静态类数据。
            if unsafe { RegisterClassExW(&class) } == 0 {
                return Err(platform(error_from_windows(
                    "RegisterClassExW(alert card)",
                    windows::core::Error::from_win32(),
                )));
            }
            Ok(())
        })
        .clone()
}

fn platform(error: PlatformError) -> CardError {
    CardError::Platform(error)
}

const fn loword_i32(value: isize) -> i32 {
    (value & 0xFFFF) as i16 as i32
}

const fn hiword_i32(value: isize) -> i32 {
    ((value >> 16) & 0xFFFF) as i16 as i32
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
        let context = create.lpCreateParams.cast::<WindowContext>();
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
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowContext;
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
    // SAFETY: 非空指针指向本线程独占的、仍然存活的 WindowContext。
    let context = unsafe { &mut *pointer };
    match message {
        WM_PAINT => {
            paint_card(hwnd, context);
            LRESULT(0)
        }
        // 整个客户区每次都自己重画,交给系统擦一遍只会闪。
        WM_ERASEBKGND => LRESULT(1),
        WM_NCHITTEST => {
            // WM_NCHITTEST 的坐标是屏幕坐标,要先换算成客户区坐标。
            let mut point = POINT {
                x: loword_i32(lparam.0),
                y: hiword_i32(lparam.0),
            };
            // SAFETY: hwnd 就是正在处理消息的窗口。
            let in_strip = unsafe { ScreenToClient(hwnd, &mut point) }.as_bool()
                && point.y < context.strip_height;
            if in_strip {
                // 标题条当成"标题栏":系统的拖动逻辑直接可用,而且
                // WS_EX_NOACTIVATE 下拖动也不会激活窗口。
                LRESULT(HTCAPTION as isize)
            } else {
                LRESULT(HTCLIENT as isize)
            }
        }
        // 点卡片不抢焦点——游戏保持前台,键盘还在游戏里。
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let x = loword_i32(lparam.0);
            let y = hiword_i32(lparam.0);
            let hovered = hit_button(&context.buttons, x, y).map(CardButton::index);
            if hovered != context.hovered {
                context.hovered = hovered;
                // SAFETY: hwnd 存活。
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            if !context.tracking_leave {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // 不订阅 WM_MOUSELEAVE 的话,鼠标滑出卡片后高亮会一直亮着。
                // SAFETY: track 完整填写且生命周期覆盖调用。
                context.tracking_leave = unsafe { TrackMouseEvent(&mut track) }.is_ok();
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            context.tracking_leave = false;
            context.hovered = None;
            // 按下之后把鼠标拖出卡片再松手 = 反悔,不算点击。
            context.pressed = None;
            // SAFETY: hwnd 存活。
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // 这里**不能**用 SetCapture:卡片是 WS_EX_NOACTIVATE 的后台窗口,
            // 而 SetCapture 只对前台窗口完整生效,后台窗口调用它会被系统立刻
            // 收回,并回敬一条 WM_CAPTURECHANGED——按下状态当场被清掉,于是
            // 每一次点击都在松手时"对不上按下的那个按钮",按钮全体失灵。
            // 拖出去反悔改由 WM_MOUSELEAVE 负责。
            context.pressed =
                hit_button(&context.buttons, loword_i32(lparam.0), hiword_i32(lparam.0))
                    .map(CardButton::index);
            // SAFETY: hwnd 存活。
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let released = hit_button(&context.buttons, loword_i32(lparam.0), hiword_i32(lparam.0));
            // 只有"按下和松手在同一个按钮上"才算点击:按错了可以拖出去松手反悔。
            if let Some(button) = released
                && context.pressed == Some(button.index())
            {
                context.clicked = Some(button);
            }
            context.pressed = None;
            // SAFETY: hwnd 存活。
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == AUTO_HIDE_TIMER_ID => {
            context.auto_hide_fired = true;
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            let mut rect = RECT::default();
            // SAFETY: hwnd 是刚被用户拖完的那个窗口。
            if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
                context.moved = Some((rect.left, rect.top));
            }
            // SAFETY: 记完位置再交回系统,拖动的收尾照常走默认流程。
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            context.replace_requested = true;
            LRESULT(0)
        }
        // 卡片没有关闭按钮,系统层面的关闭一律忽略,由服务决定生死。
        WM_CLOSE => LRESULT(0),
        // SAFETY: 其余消息交回系统默认处理。
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF((red as u32) | ((green as u32) << 8) | ((blue as u32) << 16))
}

/// 深色卡片配色,和 POE-Trade-Tracker 的 HUD 用同一套 token,免得两个工具
/// 摆在一起像两家做的。
const PANEL: COLORREF = rgb(0x17, 0x1B, 0x23);
const BORDER: COLORREF = rgb(0x39, 0x42, 0x4F);
const RAIL: COLORREF = rgb(0x1C, 0x21, 0x2B);
const HAIRLINE: COLORREF = rgb(0x22, 0x28, 0x34);
const GOLD: COLORREF = rgb(0xD9, 0xB9, 0x78);
const TEXT_PRIMARY: COLORREF = rgb(0xE6, 0xE9, 0xEF);
const TEXT_SECONDARY: COLORREF = rgb(0xA9, 0xB1, 0xBE);
const TEXT_META: COLORREF = rgb(0x78, 0x82, 0x8F);
const BUTTON_FILL: COLORREF = rgb(0x22, 0x28, 0x34);
const BUTTON_HOVER: COLORREF = rgb(0x2E, 0x36, 0x44);
const BUTTON_PRESSED: COLORREF = rgb(0x3A, 0x44, 0x54);
const BUTTON_TEXT: COLORREF = rgb(0xD9, 0xE0, 0xEA);

fn paint_card(hwnd: HWND, context: &WindowContext) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: BeginPaint/EndPaint 在本函数内配对。
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    let mut client = RECT::default();
    // SAFETY: hwnd 存活,client 是合法出参。
    let _ = unsafe { GetClientRect(hwnd, &mut client) };
    let dpi = context.dpi.max(1);
    let px = |logical: i32| scale_for_dpi(logical, dpi);

    let panel = Brush::new(PANEL);
    let border = Brush::new(BORDER);
    let rail = Brush::new(RAIL);
    let hairline = Brush::new(HAIRLINE);
    // SAFETY: 所有 GDI 对象都由 Brush/Font 的 Drop 释放,dc 在本次绘制内有效。
    unsafe {
        FillRect(dc, &client, panel.0);
        FrameRect(dc, &client, border.0);
        let strip = RECT {
            left: client.left + 1,
            top: client.top + 1,
            right: client.right - 1,
            bottom: client.top + context.strip_height,
        };
        FillRect(dc, &strip, rail.0);
        let seam = RECT {
            left: client.left + 1,
            top: strip.bottom,
            right: client.right - 1,
            bottom: strip.bottom + 1,
        };
        FillRect(dc, &seam, hairline.0);
        SetBkMode(dc, TRANSPARENT);
    }

    let title_font = Font::new(px(15), FW_SEMIBOLD.0 as i32);
    let body_font = Font::new(px(13), FW_NORMAL.0 as i32);
    let small_font = Font::new(px(11), FW_NORMAL.0 as i32);
    let button_font = Font::new(px(12), FW_NORMAL.0 as i32);
    let left = client.left + px(PAD_X);
    let right = client.right - px(PAD_X);
    let single = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_LEFT;

    draw_text(
        dc,
        title_font.0,
        GOLD,
        &context.text.title,
        RECT {
            left,
            top: client.top,
            right,
            bottom: client.top + context.strip_height,
        },
        single,
    );
    draw_text(
        dc,
        body_font.0,
        TEXT_PRIMARY,
        &context.text.line1,
        RECT {
            left,
            top: px(LINE1_TOP),
            right,
            bottom: px(LINE1_TOP + LINE_HEIGHT),
        },
        single,
    );
    draw_text(
        dc,
        body_font.0,
        TEXT_SECONDARY,
        &context.text.line2,
        RECT {
            left,
            top: px(LINE2_TOP),
            right,
            bottom: px(LINE2_TOP + LINE_HEIGHT),
        },
        single,
    );
    draw_text(
        dc,
        small_font.0,
        TEXT_META,
        &context.text.footer,
        RECT {
            left,
            top: px(FOOTER_TOP),
            right,
            bottom: px(FOOTER_TOP + FOOTER_HEIGHT),
        },
        single,
    );
    let separator = RECT {
        left: client.left + 1,
        top: px(SEPARATOR_TOP),
        right: client.right - 1,
        bottom: px(SEPARATOR_TOP) + 1,
    };
    // SAFETY: dc 在本次绘制内有效。
    unsafe {
        FillRect(dc, &separator, hairline.0);
    }

    for (index, rect) in context.buttons.iter().enumerate() {
        let fill = if context.pressed == Some(index) {
            BUTTON_PRESSED
        } else if context.hovered == Some(index) {
            BUTTON_HOVER
        } else {
            BUTTON_FILL
        };
        let brush = Brush::new(fill);
        let native = RECT {
            left: rect.x,
            top: rect.y,
            right: rect.right(),
            bottom: rect.bottom(),
        };
        // SAFETY: dc 有效,brush 在本次循环内存活。
        unsafe {
            FillRect(dc, &native, brush.0);
            FrameRect(dc, &native, border.0);
        }
        draw_text(
            dc,
            button_font.0,
            BUTTON_TEXT,
            &context.text.buttons[index],
            native,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_END_ELLIPSIS,
        );
    }

    // SAFETY: 与上面的 BeginPaint 配对。
    unsafe {
        let _ = EndPaint(hwnd, &paint);
    }
}

fn draw_text(
    dc: HDC,
    font: HFONT,
    color: COLORREF,
    text: &str,
    mut bounds: RECT,
    format: DRAW_TEXT_FORMAT,
) {
    // 空串的 Vec<u16> 是悬垂指针,DrawTextW 会访问违例;直接跳过。
    if text.is_empty() {
        return;
    }
    let mut utf16 = text.encode_utf16().collect::<Vec<_>>();
    // SAFETY: dc/font 在本次绘制内有效;DrawTextW 只在 bounds 内绘制。
    unsafe {
        let previous = SelectObject(dc, HGDIOBJ(font.0));
        SetTextColor(dc, color);
        DrawTextW(dc, &mut utf16, &mut bounds, format);
        SelectObject(dc, previous);
    }
}

struct Brush(HBRUSH);

impl Brush {
    fn new(color: COLORREF) -> Self {
        // SAFETY: CreateSolidBrush 无前置条件;Drop 里 DeleteObject。
        Self(unsafe { CreateSolidBrush(color) })
    }
}

impl Drop for Brush {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: 本类型独占这个画刷,至多删一次。
            let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
        }
    }
}

struct Font(HFONT);

impl Font {
    fn new(pixels: i32, weight: i32) -> Self {
        // 用雅黑:卡片上会出现中文物品名和界面文案,Segoe UI 画中文要靠字体回退。
        // SAFETY: 固定字体名的 CreateFontW;Drop 里 DeleteObject。
        Self(unsafe {
            CreateFontW(
                -pixels.max(1),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Microsoft YaHei UI"),
            )
        })
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: 本类型独占这个字体,至多删一次。
            let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
        }
    }
}
