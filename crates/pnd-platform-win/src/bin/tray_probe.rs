//! `tray_probe` —— 手动验收通知区图标用的小程序。
//!
//! 它调的就是 `pnd-app` 要调的那套东西([`TrayService::start`] / [`TrayHandle`]),
//! 没有另抄一份逻辑:探针和生产代码走两条路的话,探针跑通也证明不了什么。
//!
//! ```text
//! cargo run -p pnd-platform-win --bin tray_probe
//! cargo run -p pnd-platform-win --bin tray_probe -- --seconds 20
//! ```
//!
//! 它**不需要你点任何东西**:默认加上图标、挂 5 秒、摘掉、退出。想手动试的话
//! 加 `--seconds 20`,这期间左键点图标会打印 `Restore`,右键弹出两条菜单,
//! 选哪条就打印哪条。
//!
//! 探针没有主窗口(`main_hwnd` 传 0),所以那两条事件只是打印出来 ——
//! 藏 / 拿回窗口那一半要在真程序里看。

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use pnd_platform_win::{TrayConfig, TrayEvent, TrayService};

/// 事件轮询间隔。`pnd-app` 那边是 120 ms 的 tick,这里更密一点方便看反应。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

struct Options {
    run_for: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            // 默认只挂几秒:这条路要能在没人看着的时候跑完并且自己退出。
            run_for: Duration::from_secs(5),
        }
    }
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("usage: tray_probe [--seconds N]");
            return ExitCode::FAILURE;
        }
    };

    let config = TrayConfig {
        tooltip: "POE Ninja Data".to_owned(),
        // 探针没有主窗口:服务必须照样起得来。
        main_hwnd: 0,
        menu_open: "Open the main window".to_owned(),
        menu_quit: "Quit".to_owned(),
    };
    println!(
        "tooltip={:?} main_hwnd={} run_for={}s",
        config.tooltip,
        config.main_hwnd,
        options.run_for.as_secs()
    );

    let mut tray = match TrayService::start(config) {
        Ok(tray) => tray,
        Err(error) => {
            eprintln!("could not start the tray service: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("tray icon added; icon source = {}", tray.icon_source());

    // 没有主窗口时这两句该是安安静静的空操作,不是报错 —— 真程序里它们
    // 就是"点叉藏起来"和"点图标拿回来"。
    if let Err(error) = tray.hide_main() {
        eprintln!("hide_main failed: {error}");
        return ExitCode::FAILURE;
    }
    if let Err(error) = tray.show_main() {
        eprintln!("show_main failed: {error}");
        return ExitCode::FAILURE;
    }
    println!("hide_main / show_main tolerated a missing main window");

    let deadline = Instant::now() + options.run_for;
    while Instant::now() < deadline {
        while let Some(event) = tray.try_next_event() {
            println!("event: {event:?}");
            if event == TrayEvent::Quit {
                println!("  (the real app would quit here)");
            }
        }
        thread::sleep(POLL_INTERVAL);
    }

    println!("time is up; removing the icon");
    tray.stop();
    println!("tray icon removed");
    ExitCode::SUCCESS
}

/// 手搓参数解析:探针只有一个开关,为它引一个 CLI 库不划算。
fn parse_args() -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--seconds" => {
                let value = args.next().ok_or("--seconds needs a value")?;
                options.run_for = Duration::from_secs(
                    value
                        .parse::<u64>()
                        .map_err(|error| format!("--seconds must be a whole number: {error}"))?,
                );
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(options)
}
