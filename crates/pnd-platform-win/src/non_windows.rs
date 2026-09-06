//! 非 Windows 平台上的占位实现。
//!
//! 这个工具只在 Windows 上跑,但 `cargo check --target` 到别的平台时整个
//! workspace 也该编得过——编译错误和"这个功能这里没有"是两回事,后者应该是
//! 一个能拿在手里的 `PlatformError`。

use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use crate::PlatformError;
use crate::alert_card::{CardConfig, CardError, CardEvent, CardOwnership, CardShared};

pub(crate) fn play_wave(_bytes: &[u8]) -> Result<(), PlatformError> {
    Err(PlatformError::unsupported("WinMM WAV playback"))
}

pub(crate) fn stop_wave() -> Result<(), PlatformError> {
    Err(PlatformError::unsupported("WinMM WAV playback"))
}

pub fn open_url(_url: &str) -> Result<(), PlatformError> {
    Err(PlatformError::unsupported("ShellExecuteW"))
}

pub(crate) fn spawn_card_worker(
    _config: CardConfig,
    _shared: Arc<CardShared>,
    _events: mpsc::Sender<CardEvent>,
    ready: mpsc::SyncSender<Result<(), CardError>>,
    ownership: CardOwnership,
) -> Result<JoinHandle<()>, CardError> {
    // 立刻通过握手通道报错,`AlertCardService::start` 就不会白等 750 ms。
    thread::Builder::new()
        .name("pnd-alert-card".to_owned())
        .spawn(move || {
            let _ = ready.send(Err(CardError::Platform(PlatformError::unsupported(
                "native alert card",
            ))));
            drop(ownership);
        })
        .map_err(|error| CardError::Thread(format!("could not start pnd-alert-card: {error}")))
}

pub(crate) fn wake_card(_thread_id: u32) -> Result<(), CardError> {
    Err(CardError::Platform(PlatformError::unsupported(
        "native alert card",
    )))
}
