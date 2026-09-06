//! 隔离的 Win32 平台服务:报警音循环播放、置顶提醒小卡片、打开交易页的
//! shell 调用。全部 `#[cfg(windows)]` 门控。
//!
//! 分层和两个兄弟项目一样:文字校验、几何计算、命令/事件这些不碰系统的部分
//! 放在平台无关的模块里(可以在任何机器上测),所有 `unsafe` 和 Win32 调用
//! 关在私有的 `win32` 里。
//!
//! 三块内容:
//!
//! - [`ValidatedWave`] / [`LoopingWavePlayer`] / [`built_in_alert_wave`] —— 报警音,
//!   整段搬自 POE-Alarm;
//! - [`AlertCardService`] —— 屏幕角落那张不抢焦点的提醒卡片;
//! - [`open_url`] —— 用默认浏览器打开官方交易页。

#![forbid(unsafe_op_in_unsafe_fn)]

mod alert_card;
mod alert_cue;
mod error;
#[cfg(not(windows))]
mod non_windows;
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
pub use wave::{
    LoopingWavePlayer, PcmWaveFormat, ValidatedWave, WaveValidationError, WaveValidationErrorKind,
    validate_pcm_wave,
};

#[cfg(not(windows))]
pub use non_windows::open_url;
#[cfg(windows)]
pub use win32::open_url;
