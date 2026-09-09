//! `login_probe` —— 手动验收程序内登录用的小程序。
//!
//! 它调的就是 `pnd-app` 要调的那套函数([`LoginService`]),没有另抄一份逻辑。
//!
//! ```text
//! cargo run -p pnd-platform-win --bin login_probe -- --capture-after 25
//! cargo run -p pnd-platform-win --bin login_probe -- --capture-after 20 --screenshot shot.bmp
//! ```
//!
//! 要看的三件事:窗口开出来、里面是官网(匿名访客会被弹到 `/login`)、
//! `--capture-after` 到点时读一次 cookie 并把**名字**列出来。
//!
//! **会话的值一个字符都不会打印出来。** [`LoginEvent`] 的 `Debug` 只印长度,
//! 这里也只印长度 —— 探针的输出会被贴进聊天记录和 issue,那是最容易走漏的地方。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use pnd_platform_win::{LoginConfig, LoginEvent, LoginService};

/// 事件轮询间隔。`pnd-app` 那边是 120 ms 的 tick,这里更密一点方便看反应。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

struct Options {
    /// 多少秒之后主动读一次 cookie。`None` = 不主动读,只等自动那条路。
    capture_after: Option<Duration>,
    run_for: Duration,
    user_data_dir: PathBuf,
    screenshot: Option<PathBuf>,
    screenshot_after: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            capture_after: None,
            run_for: Duration::from_secs(120),
            // 临时目录:探针不该在本机留下一份能登进去的浏览器配置。
            user_data_dir: std::env::temp_dir().join("pnd-login-probe"),
            screenshot: None,
            screenshot_after: Duration::from_secs(10),
        }
    }
}

const WINDOW_TITLE: &str = "Exile Ledger — login probe";

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            eprintln!(
                "usage: login_probe [--capture-after N] [--seconds N] \
                 [--user-data-dir PATH] [--screenshot PATH] [--screenshot-after N]"
            );
            return ExitCode::FAILURE;
        }
    };

    println!(
        "user_data_dir={} capture_after={:?} run_for={}s",
        options.user_data_dir.display(),
        options.capture_after.map(|after| after.as_secs()),
        options.run_for.as_secs(),
    );

    let service = match LoginService::start(LoginConfig {
        user_data_dir: options.user_data_dir.clone(),
        title: WINDOW_TITLE.to_owned(),
        hint_text: "Log in normally. The window closes itself once the session is read.".to_owned(),
    }) {
        Ok(service) => service,
        Err(error) => {
            eprintln!("could not start the login service: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = service.open() {
        eprintln!("could not open the login window: {error}");
        return ExitCode::FAILURE;
    }
    println!("open requested; waiting for events…");

    let started = Instant::now();
    let mut capture_at = options.capture_after.map(|after| started + after);
    let mut screenshot_at = options
        .screenshot
        .as_ref()
        .map(|_| started + options.screenshot_after);
    let mut stop_at = started + options.run_for;

    loop {
        while let Some(event) = service.try_next_event() {
            println!("event: {event:?}");
            match event {
                LoginEvent::CookieNames(names) => {
                    // 名字不是秘密,而且这一行是"匿名状态下到底有没有
                    // POESESSID"这个问题的唯一答案。
                    println!("  cookie names for the domain: {names:?}");
                }
                LoginEvent::SessionCaptured { poesessid } => {
                    println!(
                        "  captured a session of {} chars",
                        poesessid.chars().count()
                    );
                    // 留半秒把随后那条 Closed 也收进来。
                    stop_at = Instant::now() + Duration::from_millis(500);
                }
                LoginEvent::Closed => {
                    println!("window closed; done");
                    return ExitCode::SUCCESS;
                }
                LoginEvent::Failed(failure) => {
                    eprintln!("  login failed: {failure}");
                    return ExitCode::FAILURE;
                }
                _ => {}
            }
        }

        let now = Instant::now();
        if let Some(at) = screenshot_at
            && now >= at
        {
            screenshot_at = None;
            let path = options.screenshot.clone().expect("checked above");
            match capture_own_window(WINDOW_TITLE, &path) {
                Ok((width, height)) => {
                    println!("screenshot: {width}x{height} written to {}", path.display());
                }
                Err(error) => eprintln!("screenshot failed: {error}"),
            }
        }
        if let Some(at) = capture_at
            && now >= at
        {
            capture_at = None;
            println!("capture requested");
            if let Err(error) = service.capture() {
                eprintln!("could not request a capture: {error}");
            }
        }
        if now >= stop_at {
            println!("time is up; closing the login window");
            let _ = service.close();
            // service 的 Drop 会请线程收摊并等一小会儿。
            drop(service);
            return ExitCode::SUCCESS;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// 手搓参数解析:探针只有五个开关,为它引一个 CLI 库不划算。
fn parse_args() -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--capture-after" => {
                options.capture_after = Some(Duration::from_secs(parse_seconds(
                    &mut args,
                    "--capture-after",
                )?));
            }
            "--seconds" => {
                options.run_for = Duration::from_secs(parse_seconds(&mut args, "--seconds")?);
            }
            "--user-data-dir" => {
                options.user_data_dir =
                    PathBuf::from(args.next().ok_or("--user-data-dir needs a value")?);
            }
            "--screenshot" => {
                options.screenshot = Some(PathBuf::from(
                    args.next().ok_or("--screenshot needs a value")?,
                ));
            }
            "--screenshot-after" => {
                options.screenshot_after =
                    Duration::from_secs(parse_seconds(&mut args, "--screenshot-after")?);
            }
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

/// 把**本程序自己那个**登录窗抓成一张 BMP。
///
/// 只截自己的窗口,不截屏幕:探针跑的时候屏幕上还有别的东西,而这张图会被
/// 贴进 review。窗口靠标题找,所以永远只可能命中自己刚开的那一个。
///
/// 为什么 `PW_RENDERFULLCONTENT`:WebView2 的内容画在自己的子窗口/合成层上,
/// 普通的 `BitBlt` 只会得到一块空白。
#[cfg(windows)]
fn capture_own_window(title: &str, path: &Path) -> Result<(i32, i32), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HDC, HGDIOBJ, ReleaseDC,
        SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetClientRect};
    use windows::core::{BOOL, PCWSTR};

    // `PrintWindow` 在 windows-rs 里被归进了 `Win32_Storage_Xps`(它和打印
    // 共用一个头文件)。为探针里这一句截图给整个 crate 多开一个 feature 不划算,
    // 所以在这儿直接声明它 —— 它就在 user32 里。
    #[link(name = "user32")]
    unsafe extern "system" {
        fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> BOOL;
    }
    /// `PW_RENDERFULLCONTENT`
    const RENDER_FULL_CONTENT: u32 = 2;

    let mut wide: Vec<u16> = title.encode_utf16().collect();
    wide.push(0);
    // SAFETY: wide 以 NUL 结尾且活到调用返回。
    let hwnd = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())) }
        .map_err(|error| format!("the login window was not found: {error}"))?;

    let mut client = windows::Win32::Foundation::RECT::default();
    // SAFETY: hwnd 是刚找到的窗口,client 是合法出参。
    unsafe { GetClientRect(hwnd, &mut client) }.map_err(|error| error.to_string())?;
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    if width <= 0 || height <= 0 {
        return Err(format!("the window client area is {width}x{height}"));
    }

    // SAFETY: 下面这一整段是标准的"离屏位图 + PrintWindow + GetDIBits",
    // 每个句柄都在函数返回前还回去。
    let pixels = unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
        let printed = PrintWindow(hwnd, memory, RENDER_FULL_CONTENT).as_bool();

        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        // 正的高度 = 自底向上,BMP 文件本来就是这个方向,省一次翻转。
        info.bmiHeader.biHeight = height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let mut pixels = vec![0_u8; (width as usize) * (height as usize) * 4];
        let rows = GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(memory, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);

        if !printed {
            return Err("PrintWindow returned false".to_owned());
        }
        if rows == 0 {
            return Err("GetDIBits copied no rows".to_owned());
        }
        pixels
    };

    write_bmp(path, width, height, pixels).map_err(|error| error.to_string())?;
    Ok((width, height))
}

#[cfg(not(windows))]
fn capture_own_window(_title: &str, _path: &Path) -> Result<(i32, i32), String> {
    Err("screenshots are only available on Windows".to_owned())
}

/// 32 位 BGRA 像素 → 一个最普通的 BMP 文件。
///
/// 不引图像库:BMP 的头就是 14 + 40 个字节,写出来比一条依赖便宜得多,
/// 而这张图只是给人看一眼"页面渲染出来了没"。
#[cfg(windows)]
fn write_bmp(path: &Path, width: i32, height: i32, mut pixels: Vec<u8>) -> std::io::Result<()> {
    // PrintWindow 出来的 alpha 常常是 0,那样有些看图程序会画成全透明。
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel[3] = 0xFF;
    }
    const FILE_HEADER: u32 = 14;
    const INFO_HEADER: u32 = 40;
    let image_size = pixels.len() as u32;
    let mut out = Vec::with_capacity((FILE_HEADER + INFO_HEADER) as usize + pixels.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(FILE_HEADER + INFO_HEADER + image_size).to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&(FILE_HEADER + INFO_HEADER).to_le_bytes());
    out.extend_from_slice(&INFO_HEADER.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&32_u16.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&image_size.to_le_bytes());
    out.extend_from_slice(&[0_u8; 16]);
    out.extend_from_slice(&pixels);
    std::fs::write(path, out)
}
