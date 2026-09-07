//! `alert_probe` —— 手动验收提醒卡片用的小程序。
//!
//! 它调的就是 `pnd-app` 将来要调的那套函数(`AlertCardService` / `open_url`),
//! 没有另抄一份逻辑:探针和生产代码走两条路的话,探针跑通也证明不了什么。
//!
//! ```text
//! cargo run -p pnd-platform-win --bin alert_probe -- \
//!     --corner bottom_right --opacity 235 --auto-hide-seconds 60 --seconds 20
//! cargo run -p pnd-platform-win --bin alert_probe -- --seconds 8 --no-sound
//! ```
//!
//! 要看的五件事:游戏窗口化全屏时卡片出现而游戏**不失焦**(键盘还在游戏里)、
//! 声音循环、四个按钮各打印一条事件、到点自动收起,标题条能拖;带
//! `--hotkey ctrl+alt+d` 时,游戏在前台按那一下会打印 `HotkeyDismiss`
//! 并把卡片收起来 —— 而游戏里什么都没发生。

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use pnd_platform_win::{
    AlertCardService, CardButton, CardConfig, CardEvent, CardText, Corner, DEFAULT_CARD_OPACITY,
    Hotkey, built_in_alert_wave, open_url, parse_hotkey,
};

/// 点"打开交易页"时开的地址,和计划里的第一版"去藏身处"流程一致。
const SAMPLE_TRADE_URL: &str = "https://www.pathofexile.com/trade2/search/poe2/Forbidden%20Rites";

/// 事件轮询间隔。`pnd-app` 那边是 120 ms 的 tick,这里更密一点方便看反应。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

struct Options {
    corner: Corner,
    opacity: u8,
    auto_hide: Duration,
    run_for: Duration,
    sound: bool,
    hotkey: Option<Hotkey>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            corner: Corner::BottomRight,
            opacity: DEFAULT_CARD_OPACITY,
            auto_hide: Duration::from_secs(60),
            run_for: Duration::from_secs(20),
            sound: true,
            hotkey: None,
        }
    }
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            eprintln!(
                "usage: alert_probe [--corner top_left|top_right|bottom_left|bottom_right] \
                 [--opacity 0-255] [--auto-hide-seconds N] [--seconds N] [--no-sound]                  [--hotkey ctrl+alt+d]"
            );
            return ExitCode::FAILURE;
        }
    };

    let mut config = CardConfig::new();
    config.corner = options.corner;
    config.opacity = options.opacity;
    config.auto_hide = options.auto_hide;
    config.dismiss_hotkey = options.hotkey;
    if options.sound {
        match built_in_alert_wave() {
            Ok(wave) => config.sound = Some(wave),
            Err(error) => {
                eprintln!("could not build the built-in chime: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    println!(
        "corner={} opacity={} auto_hide={}s run_for={}s sound={} hotkey={}",
        options.corner.as_str(),
        options.opacity,
        options.auto_hide.as_secs(),
        options.run_for.as_secs(),
        options.sound,
        options
            .hotkey
            .map_or_else(|| "off".to_owned(), |hotkey| hotkey.to_string()),
    );

    let service = match AlertCardService::start(config) {
        Ok(service) => service,
        Err(error) => {
            eprintln!("could not start the alert card service: {error}");
            return ExitCode::FAILURE;
        }
    };

    let alert_id = 1_i64;
    let text = match sample_text() {
        Ok(text) => text,
        Err(error) => {
            eprintln!("sample card text is invalid: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = service.show(alert_id, text) {
        eprintln!("could not show the card: {error}");
        return ExitCode::FAILURE;
    }
    println!("card shown (alert_id={alert_id}); waiting for events…");

    let deadline = Instant::now() + options.run_for;
    while Instant::now() < deadline {
        while let Some(event) = service.try_next_event() {
            println!("event: {event:?}");
            match event {
                CardEvent::Clicked {
                    button: CardButton::Primary,
                    ..
                } => match open_url(SAMPLE_TRADE_URL) {
                    Ok(()) => println!("  opened {SAMPLE_TRADE_URL}"),
                    Err(error) => eprintln!("  could not open the trade page: {error}"),
                },
                // 热键和"忽略"按钮做同一件事:收起卡片,别的什么都不做。
                CardEvent::Clicked {
                    button: CardButton::Dismiss,
                    ..
                }
                | CardEvent::HotkeyDismiss { .. } => {
                    if let Err(error) = service.hide() {
                        eprintln!("  could not hide the card: {error}");
                    } else {
                        println!("  card hidden");
                    }
                }
                _ => {}
            }
        }
        thread::sleep(POLL_INTERVAL);
    }

    println!("time is up; shutting the card thread down");
    // service 的 Drop 会发 Shutdown 并 join 卡片线程。
    drop(service);
    ExitCode::SUCCESS
}

fn sample_text() -> Result<CardText, pnd_platform_win::CardTextError> {
    CardText::new(
        "Choir of the Storm · 15 divine",
        "Tongzii#6639 · online · listed 1 hour ago",
        "cap 20 divine · +2 more listings",
        "search 6h 41/299 · fetch 6h 88/499",
        ["Open trade", "Copy whisper", "Hideout", "Dismiss"],
    )
}

/// 手搓参数解析:探针只有五个开关,为它引一个 CLI 库不划算。
fn parse_args() -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--corner" => {
                let value = args.next().ok_or("--corner needs a value")?;
                options.corner =
                    Corner::parse(&value).ok_or(format!("unknown corner {value:?}"))?;
            }
            "--opacity" => {
                let value = args.next().ok_or("--opacity needs a value")?;
                options.opacity = value
                    .parse::<u8>()
                    .map_err(|error| format!("--opacity must be 0-255: {error}"))?;
            }
            "--auto-hide-seconds" => {
                options.auto_hide = Duration::from_secs(parse_seconds(&mut args, flag.as_str())?);
            }
            "--seconds" => {
                options.run_for = Duration::from_secs(parse_seconds(&mut args, flag.as_str())?);
            }
            "--hotkey" => {
                let value = args.next().ok_or("--hotkey needs a value")?;
                options.hotkey = Some(parse_hotkey(&value).ok_or(format!(
                    "{value:?} is not a hotkey this program understands"
                ))?);
            }
            "--no-sound" => options.sound = false,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(options)
}

fn parse_seconds(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    let value = args.next().ok_or(format!("{flag} needs a value"))?;
    value
        .parse::<u64>()
        .map_err(|error| format!("{flag} must be a whole number of seconds: {error}"))
}
