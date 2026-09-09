//! 通知区(俗称"托盘")图标:平台无关的对外接口。
//!
//! 为什么要它:这个程序一开就是几个小时(蹲价在轮询、市场观察在攒数据、
//! ninja 在采样),而窗口右上角那个叉今天等于"退出",一不留神就把后台整个
//! 关掉了。有了托盘图标,叉只是把窗口藏起来,后台照跑;想看的时候点一下
//! 图标就回来,右键菜单里才是真正的退出。
//!
//! 真正的 Win32 实现在 `win32::tray`,这里只放:
//!
//! - 调用方需要的类型(配置、事件、错误);
//! - 一个把托盘线程的事件收回来的 [`TrayHandle`];
//! - 两个纯函数([`decode_tray_callback`]、[`menu_command`]),它们带单元测试。
//!   托盘上那点交互靠肉眼很难试全:解错一位就是"点左键弹出退出菜单",
//!   而那种错只会在某一次手滑之后才被发现。

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::PlatformError;

/// 托盘线程启动握手的上限。和卡片同一个数:图标加不出来必须当场知道,
/// 不能等到用户点了叉、窗口藏起来、却找不到图标时才发现。
const STARTUP_TIMEOUT: Duration = Duration::from_millis(750);

/// 托盘图标在本进程里的编号。只有一个图标,所以固定 1。
pub const TRAY_ICON_ID: u32 = 1;

/// 右键菜单第一条("打开主窗口")的命令 id。
pub const MENU_OPEN_ID: usize = 1;
/// 右键菜单第二条("退出程序")的命令 id。
pub const MENU_QUIT_ID: usize = 2;

// 托盘回调里会出现的那几条系统消息号。
//
// 写成裸数字而不是从 `windows` crate 引:这个文件要在非 Windows 上也编得过
// (那边整个 `windows` 依赖都不存在),而它们是三十年没动过的常量。
const NIN_SELECT: u32 = 0x0400;
const NIN_KEYSELECT: u32 = 0x0401;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_CONTEXTMENU: u32 = 0x007B;

/// 托盘线程报回来的事情。调用方每帧 `try_next_event` 抽干即可。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayEvent {
    /// 把主窗口拿回来(左键点了图标,或者选了菜单第一条)。
    Restore,
    /// 用户选了菜单里的"退出程序"。这是**唯一**一条"真的关掉这个程序"的路。
    Quit,
}

/// 一次托盘回调解出来的东西。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayClick {
    /// 左键点了图标(或者用键盘选中了它)。
    Select,
    /// 右键:在这个屏幕坐标上弹菜单。
    Menu { x: i32, y: i32 },
}

/// 把托盘图标的回调消息解开。
///
/// 纯函数带测试,因为这一步全是位运算,而且错了很难看出来:
/// `NOTIFYICON_VERSION_4` 下**发生了什么**在 `lparam` 的低 16 位、
/// **哪个图标**在它的高 16 位,鼠标位置反而在 `wparam` 里(版本 4 之前
/// 是反过来的)。少移 16 位的结果是点左键弹出退出菜单。
#[must_use]
pub fn decode_tray_callback(wparam: usize, lparam: isize) -> Option<TrayClick> {
    // isize → u32 只留低 32 位,而消息号和图标编号本来就都住在那儿。
    let packed = lparam as u32;
    if packed >> 16 != TRAY_ICON_ID {
        return None;
    }
    match packed & 0xFFFF {
        NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP => Some(TrayClick::Select),
        WM_CONTEXTMENU | WM_RBUTTONUP => Some(TrayClick::Menu {
            x: loword_i32(wparam),
            y: hiword_i32(wparam),
        }),
        _ => None,
    }
}

/// 右键菜单选中的那一条 → 一个事件。
///
/// `TrackPopupMenu` 在用户点空白处关掉菜单时返回 0,所以"认不出来的 id"
/// 必须是"什么都不做",不能兜底成某一条。
#[must_use]
pub fn menu_command(id: usize) -> Option<TrayEvent> {
    match id {
        MENU_OPEN_ID => Some(TrayEvent::Restore),
        MENU_QUIT_ID => Some(TrayEvent::Quit),
        _ => None,
    }
}

const fn loword_i32(value: usize) -> i32 {
    (value & 0xFFFF) as u16 as i16 as i32
}

const fn hiword_i32(value: usize) -> i32 {
    ((value >> 16) & 0xFFFF) as u16 as i16 as i32
}

/// 启动托盘线程需要的全部配置。
#[derive(Clone, Debug)]
pub struct TrayConfig {
    /// 鼠标停在图标上时的浮动提示。
    pub tooltip: String,
    /// 主窗口的 `HWND`。
    ///
    /// **0 = 这个进程没有主窗口**(探针就是这么跑的):图标照样加得出来,
    /// 只是 [`TrayHandle::hide_main`] / [`TrayHandle::show_main`] 无事可做。
    pub main_hwnd: isize,
    /// 右键菜单第一条的文字。平台层不拼任何文案:双语目录在 `pnd-app`。
    pub menu_open: String,
    /// 右键菜单第二条的文字。
    pub menu_quit: String,
}

/// 托盘服务的失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrayError {
    /// 线程没起来 / 没在 750 ms 内就绪。
    Thread(String),
    /// 托盘线程已经退出。
    Disconnected,
    /// 底层 Win32 失败(或者非 Windows 平台上的"不支持")。
    Platform(PlatformError),
}

impl fmt::Display for TrayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Thread(detail) => write!(formatter, "tray thread failed: {detail}"),
            Self::Disconnected => formatter.write_str("the tray thread is gone"),
            Self::Platform(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for TrayError {}

impl From<PlatformError> for TrayError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

/// 调用线程和托盘线程之间的全部共享状态。
///
/// 比卡片那份简单得多:托盘线程只需要收一条"收摊"命令,而那一条本身就能
/// 用消息号表达,不需要一条命令队列。
#[derive(Debug, Default)]
pub(crate) struct TrayShared {
    native_thread_id: AtomicU32,
}

impl TrayShared {
    pub(crate) fn native_thread_id(&self) -> u32 {
        self.native_thread_id.load(Ordering::Acquire)
    }

    pub(crate) fn set_native_thread_id(&self, id: u32) {
        self.native_thread_id.store(id, Ordering::Release);
    }
}

/// 起托盘线程的入口。
pub struct TrayService;

impl TrayService {
    /// 起一条名为 `pnd-tray` 的线程,等它把图标加上去再返回。
    ///
    /// 只有这一步会等(最多 750 ms):图标加不出来必须当场知道 —— 那意味着
    /// "缩到托盘"会把窗口藏进一个找不回来的地方。
    pub fn start(config: TrayConfig) -> Result<TrayHandle, TrayError> {
        let main_hwnd = config.main_hwnd;
        let shared = Arc::new(TrayShared::default());
        let (event_sender, events) = mpsc::channel();
        let (ready_sender, ready) = mpsc::sync_channel(1);
        let thread =
            platform_spawn_worker(config, Arc::clone(&shared), event_sender, ready_sender)?;
        match ready.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(icon_source)) => Ok(TrayHandle {
                shared,
                events,
                thread: Some(thread),
                main_hwnd,
                icon_source,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                // 线程还活着但没就绪:先请它退出,再把超时报上去。
                let _ = platform_stop(shared.native_thread_id());
                Err(TrayError::Thread(format!(
                    "the tray icon was not ready within {} ms: {error}",
                    STARTUP_TIMEOUT.as_millis()
                )))
            }
        }
    }
}

/// 拥有一条托盘线程。所有方法都不阻塞。
pub struct TrayHandle {
    shared: Arc<TrayShared>,
    events: mpsc::Receiver<TrayEvent>,
    thread: Option<JoinHandle<()>>,
    main_hwnd: isize,
    icon_source: String,
}

impl fmt::Debug for TrayHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrayHandle")
            .field("native_thread_id", &self.shared.native_thread_id())
            .field("main_hwnd", &self.main_hwnd)
            .field("icon_source", &self.icon_source)
            .finish()
    }
}

impl TrayHandle {
    /// 取一条已经发生的事件,没有就返回 `None`,永不阻塞。
    #[must_use]
    pub fn try_next_event(&self) -> Option<TrayEvent> {
        self.events.try_recv().ok()
    }

    /// 图标最后是从哪儿取来的(资源 id,还是系统默认那个)。
    ///
    /// 值得报出来:图标退回系统默认的时候托盘上什么都没坏,只是那个图标
    /// 不再是本程序的 —— 不说一声的话,只会以为"托盘图标长这样"。
    #[must_use]
    pub fn icon_source(&self) -> &str {
        &self.icon_source
    }

    /// 把主窗口藏起来(点叉时走这条)。
    pub fn hide_main(&self) -> Result<(), TrayError> {
        platform_show_main(self.main_hwnd, false)
    }

    /// 把主窗口拿回来并放到前台。
    pub fn show_main(&self) -> Result<(), TrayError> {
        platform_show_main(self.main_hwnd, true)
    }

    /// 摘掉托盘图标并等线程收摊。
    ///
    /// 退出之前必须显式调一次:图标归托盘线程所有,进程直接结束的话,
    /// 通知区里会留下一个点了没反应的僵尸图标,要鼠标划过去才消失。
    pub fn stop(&mut self) {
        let _ = platform_stop(self.shared.native_thread_id());
        if let Some(thread) = self.thread.take() {
            // 图标和窗口都归那条线程,等它自己收干净比在这里遥控删除安全。
            let _ = thread.join();
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn platform_spawn_worker(
    config: TrayConfig,
    shared: Arc<TrayShared>,
    events: mpsc::Sender<TrayEvent>,
    ready: mpsc::SyncSender<Result<String, TrayError>>,
) -> Result<JoinHandle<()>, TrayError> {
    #[cfg(windows)]
    {
        crate::win32::spawn_tray_worker(config, shared, events, ready)
    }
    #[cfg(not(windows))]
    {
        crate::non_windows::spawn_tray_worker(config, shared, events, ready)
    }
}

fn platform_stop(thread_id: u32) -> Result<(), TrayError> {
    #[cfg(windows)]
    {
        crate::win32::stop_tray(thread_id)
    }
    #[cfg(not(windows))]
    {
        crate::non_windows::stop_tray(thread_id)
    }
}

/// 主窗口没有(`hwnd == 0`)时什么都不做,而不是报错。
///
/// 探针就是这么跑的:它只想看图标加得上、摘得掉,没有窗口可藏。
fn platform_show_main(hwnd: isize, visible: bool) -> Result<(), TrayError> {
    if hwnd == 0 {
        return Ok(());
    }
    #[cfg(windows)]
    {
        crate::win32::show_main_window(hwnd, visible)
    }
    #[cfg(not(windows))]
    {
        crate::non_windows::show_main_window(hwnd, visible)
    }
}

#[cfg(test)]
mod tray_tests {
    use super::*;

    /// 把一条版本 4 的回调打包成 `lparam`:低 16 位是消息,高 16 位是图标编号。
    const fn callback_lparam(event: u32) -> isize {
        ((TRAY_ICON_ID << 16) | event) as isize
    }

    /// 左键(以及键盘选中)= 打开主窗口,右键 = 弹菜单,而且菜单要弹在鼠标那儿。
    ///
    /// 这一条守的是位运算:版本 4 把"发生了什么"和"鼠标在哪"换了个位置,
    /// 照旧版写就是点左键弹出退出菜单。
    #[test]
    fn a_left_click_restores_and_a_right_click_asks_for_the_menu() {
        assert_eq!(
            decode_tray_callback(0, callback_lparam(NIN_SELECT)),
            Some(TrayClick::Select)
        );
        assert_eq!(
            decode_tray_callback(0, callback_lparam(NIN_KEYSELECT)),
            Some(TrayClick::Select)
        );
        assert_eq!(
            decode_tray_callback(0, callback_lparam(WM_LBUTTONUP)),
            Some(TrayClick::Select)
        );

        // 鼠标位置在 wparam:低 16 位 x、高 16 位 y。
        let wparam = (900_usize << 16) | 1600;
        assert_eq!(
            decode_tray_callback(wparam, callback_lparam(WM_CONTEXTMENU)),
            Some(TrayClick::Menu { x: 1600, y: 900 })
        );
        assert_eq!(
            decode_tray_callback(wparam, callback_lparam(WM_RBUTTONUP)),
            Some(TrayClick::Menu { x: 1600, y: 900 })
        );
    }

    /// 不是我们那个图标、或者只是鼠标划过去,一律不产生动作。
    ///
    /// 鼠标一移进图标就是一串 `WM_MOUSEMOVE`;把它们当成点击的话,
    /// 光标扫过通知区就会把窗口拽出来。
    #[test]
    fn hovering_and_other_icons_do_nothing() {
        assert_eq!(decode_tray_callback(0, callback_lparam(0x0200)), None);
        let other_icon = ((7_u32 << 16) | NIN_SELECT) as isize;
        assert_eq!(decode_tray_callback(0, other_icon), None);
    }

    /// 副屏摆在主屏左边时鼠标坐标是负数。按无符号读会变成六万多,
    /// 菜单于是弹在屏幕外面 —— 点了图标什么都没出现。
    #[test]
    fn a_negative_cursor_position_stays_negative() {
        let wparam = ((-40_i16 as u16 as usize) << 16) | (-1600_i16 as u16 as usize);
        assert_eq!(
            decode_tray_callback(wparam, callback_lparam(WM_CONTEXTMENU)),
            Some(TrayClick::Menu { x: -1600, y: -40 })
        );
    }

    /// 菜单两条各对一个事件,而"没选"必须什么都不发生。
    #[test]
    fn the_menu_ids_map_to_one_event_each() {
        assert_ne!(MENU_OPEN_ID, MENU_QUIT_ID);
        assert_eq!(menu_command(MENU_OPEN_ID), Some(TrayEvent::Restore));
        assert_eq!(menu_command(MENU_QUIT_ID), Some(TrayEvent::Quit));
        // TrackPopupMenu 在用户点空白处关掉菜单时返回 0。
        assert_eq!(menu_command(0), None);
        assert_eq!(menu_command(99), None);
    }
}
