//! 所有 `unsafe` 和所有 Win32 调用都关在这个模块里。上层(`alert_card`、
//! `wave`)只看得见普通的 Rust 类型。

mod alert_card;
mod shell;
mod wave;

pub(crate) use alert_card::{spawn_card_worker, wake_card};
pub use shell::open_url;
pub(crate) use wave::{play_wave, stop_wave};

use crate::PlatformError;

/// 把 `windows::core::Error` 折成本 crate 的错误。
///
/// HRESULT 的高 16 位是 `0x8007` 时,低 16 位就是原始的 Win32 错误码
/// (`ERROR_*`),还原出来比 HRESULT 好查。
pub(super) fn error_from_windows(
    operation: &'static str,
    error: windows::core::Error,
) -> PlatformError {
    let hresult = error.code().0 as u32;
    let code = if hresult & 0xFFFF_0000 == 0x8007_0000 {
        hresult & 0x0000_FFFF
    } else {
        hresult
    };
    PlatformError::Win32 {
        operation,
        code,
        message: error.message(),
    }
}
