//! 所有 `unsafe` 和所有 Win32 调用都关在这个模块里。上层(`alert_card`、
//! `login`、`wave`)只看得见普通的 Rust 类型。

mod alert_card;
mod login_window;
mod paint;
mod shell;
mod wave;

pub(crate) use alert_card::{spawn_card_worker, wake_card};
pub(crate) use login_window::{spawn_login_worker, wake_login};
pub use shell::open_url;
pub(crate) use wave::{play_wave, stop_wave};

use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

use crate::PlatformError;
use crate::alert_card::RectI;

/// 桌面工作区(整块屏幕减掉任务栏)。卡片贴角和登录窗居中都从它起算。
pub(super) fn work_area() -> Result<RectI, PlatformError> {
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
