//! 非 Windows 平台上的占位实现。
//!
//! 这个工具只在 Windows 上跑,但 `cargo check --target` 到别的平台时整个
//! workspace 也该编得过——编译错误和"这个功能这里没有"是两回事,后者应该是
//! 一个能拿在手里的 `PlatformError`。

use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use crate::PlatformError;
use crate::alert_card::{CardConfig, CardError, CardEvent, CardOwnership, CardShared};
use crate::login::{LoginConfig, LoginEvent, LoginShared};
use crate::tray::{TrayConfig, TrayError, TrayEvent, TrayShared};

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

pub(crate) fn spawn_login_worker(
    _config: LoginConfig,
    _shared: Arc<LoginShared>,
    _events: mpsc::Sender<LoginEvent>,
    ready: mpsc::SyncSender<Result<(), PlatformError>>,
) -> Result<JoinHandle<()>, PlatformError> {
    // 和卡片一样:立刻通过握手通道报错,`LoginService::start` 就不会白等。
    thread::Builder::new()
        .name("pnd-login".to_owned())
        .spawn(move || {
            let _ = ready.send(Err(PlatformError::unsupported("WebView2 login window")));
        })
        .map_err(|error| PlatformError::Thread {
            operation: "spawn(pnd-login)",
            detail: error.to_string(),
        })
}

pub(crate) fn wake_login(_thread_id: u32) -> Result<(), PlatformError> {
    Err(PlatformError::unsupported("WebView2 login window"))
}

pub(crate) fn spawn_tray_worker(
    _config: TrayConfig,
    _shared: Arc<TrayShared>,
    _events: mpsc::Sender<TrayEvent>,
    ready: mpsc::SyncSender<Result<String, TrayError>>,
) -> Result<JoinHandle<()>, TrayError> {
    // 和卡片一样:立刻通过握手通道报错,`TrayService::start` 就不会白等 750 ms。
    thread::Builder::new()
        .name("pnd-tray".to_owned())
        .spawn(move || {
            let _ = ready.send(Err(TrayError::Platform(PlatformError::unsupported(
                "notification area icon",
            ))));
        })
        .map_err(|error| TrayError::Thread(format!("could not start pnd-tray: {error}")))
}

pub(crate) fn stop_tray(_thread_id: u32) -> Result<(), TrayError> {
    Err(TrayError::Platform(PlatformError::unsupported(
        "notification area icon",
    )))
}

pub(crate) fn show_main_window(_hwnd: isize, _visible: bool) -> Result<(), TrayError> {
    Err(TrayError::Platform(PlatformError::unsupported(
        "ShowWindow",
    )))
}
