//! 用系统默认浏览器打开一个网址。
//!
//! 这是"去藏身处"第一版的全部实现:程序只负责把官方交易页开出来,买不买、
//! 传不传送都是你自己在网页上点。程序永远不替你操作游戏。

use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use super::error_from_windows;
use crate::PlatformError;

/// 打开 `url`。只接受 `https://` 开头的地址。
///
/// 为什么要挡:`ShellExecuteW` 的 "open" 动词能开的不止网页——`file:` 能开
/// 本地文件,一个可执行文件路径能直接把程序跑起来。这个函数的输入将来会来自
/// 交易站返回的数据,不该有"顺手执行点什么"的可能。
pub fn open_url(url: &str) -> Result<(), PlatformError> {
    if !url.starts_with("https://") {
        return Err(PlatformError::InvalidArgument {
            capability: "open_url",
            detail: format!("only https:// links can be opened, got {url:?}"),
        });
    }
    if url.contains('\0') {
        return Err(PlatformError::InvalidArgument {
            capability: "open_url",
            detail: "the link contains a NUL character".to_owned(),
        });
    }
    let mut wide: Vec<u16> = url.encode_utf16().collect();
    wide.push(0);
    // SAFETY: wide 以 NUL 结尾且在调用期间存活;其余参数按文档可以为 None。
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 的返回值是历史遗留:> 32 才算成功,小于等于 32 的是错误码。
    if result.0 as usize > 32 {
        Ok(())
    } else {
        Err(error_from_windows(
            "ShellExecuteW(open_url)",
            windows::core::Error::from_win32(),
        ))
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn only_https_links_are_accepted() {
        for rejected in [
            "http://www.pathofexile.com/trade2",
            "file:///C:/Windows/System32/cmd.exe",
            "C:\\Windows\\System32\\cmd.exe",
            "javascript:alert(1)",
            "",
        ] {
            let error = open_url(rejected).unwrap_err();
            assert!(matches!(
                error,
                PlatformError::InvalidArgument {
                    capability: "open_url",
                    ..
                }
            ));
        }
    }
}
