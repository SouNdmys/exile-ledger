//! 隔离的 Win32 平台服务:报警音循环播放、置顶提醒小卡片、打开交易页的
//! shell 调用。全部 `#[cfg(windows)]` 门控。
//!
//! 分层和两个兄弟项目一样:文字校验、几何计算、命令/事件这些不碰系统的部分
//! 放在平台无关的模块里(可以在任何机器上测),所有 `unsafe` 和 Win32 调用
//! 关在私有的 `win32` 里。
//!
//! 四块内容:
//!
//! - [`ValidatedWave`] / [`LoopingWavePlayer`] / [`built_in_alert_wave`] —— 报警音,
//!   整段搬自 POE-Alarm;
//! - [`AlertCardService`] —— 屏幕角落那张不抢焦点的提醒卡片,连同那条只用来
//!   收起它自己的全局热键([`parse_hotkey`]);
//! - [`LoginService`] —— 装着 Edge 内核(WebView2)的登录窗,用来取 `POESESSID`;
//! - [`TrayService`] —— 通知区("托盘")图标,让主窗口能藏起来而不是退出;
//! - [`open_url`] —— 用默认浏览器打开官方交易页。

#![forbid(unsafe_op_in_unsafe_fn)]

mod alert_card;
mod alert_cue;
mod error;
mod hotkey;
mod login;
#[cfg(not(windows))]
mod non_windows;
mod tray;
mod wave;
#[cfg(windows)]
mod win32;

pub use alert_card::{
    AlertCardService, CardButton, CardConfig, CardError, CardEvent, CardStyle, CardText,
    CardTextError, Corner, DEFAULT_AUTO_HIDE, DEFAULT_CARD_OPACITY, RectI, button_rects,
    card_geometry_for_corner, hit_button,
};
pub use alert_cue::built_in_alert_wave;
pub use error::PlatformError;
pub use hotkey::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, parse_hotkey};
pub use login::{
    ACCOUNT_URL, AfterNavigation, COOKIE_ORIGIN, LOGIN_LOGICAL_HEIGHT, LOGIN_LOGICAL_WIDTH,
    LOGIN_URL, LoginConfig, LoginEvent, LoginFailure, LoginService, MAX_AUTO_NAVIGATIONS,
    SESSION_COOKIE, SITE_PREFIX, after_navigation, is_account_url, login_geometry,
    pick_session_cookie,
};
pub use tray::{
    MENU_OPEN_ID, MENU_QUIT_ID, TRAY_ICON_ID, TrayClick, TrayConfig, TrayError, TrayEvent,
    TrayHandle, TrayService, decode_tray_callback, menu_command,
};
pub use wave::{
    LoopingWavePlayer, PcmWaveFormat, ValidatedWave, WaveValidationError, WaveValidationErrorKind,
    validate_pcm_wave,
};

#[cfg(not(windows))]
pub use non_windows::open_url;
#[cfg(windows)]
pub use win32::open_url;
