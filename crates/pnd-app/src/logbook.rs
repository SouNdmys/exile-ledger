//! 日志的两件事:**先脱敏,再落盘**。
//!
//! 状态行只放得下最后一条,可真出事的时候要看的往往是它前面那几条,而程序
//! 一关内存里那份就没了。所以每一行都写进 `<数据目录>\app.log`,界面上那个
//! 抽屉只是同一份东西的最近三百行。
//!
//! 脱敏放在**日志的入口**,不是每个调用点各记各的:后台线程、登录窗、
//! 交易站的报错都往这里汇,只要有一处忘了,那一行就会带着会话 cookie
//! 出现在一个挂在游戏旁边、随时被人看见的窗口上,并且永久留在盘上。

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// 抽屉里留多少行。
pub const LOG_CAPACITY: usize = 300;

/// `app.log` 超过这么大就滚一次。1 MB 大概是几万行,查得回昨天,又不会
/// 在一个数据目录里悄悄堆出几百兆。
const ROTATE_BYTES: u64 = 1024 * 1024;

/// 三段点串至少这么长才当成 JWT。短的多半是文件名(`settings.json.bak`)
/// 或者域名(`www.pathofexile.com`),把它们打码只会让日志读不懂。
const JWT_MIN_CHARS: usize = 40;

/// POESESSID 是 32 个十六进制字符。
const SESSION_HEX_CHARS: usize = 32;

/// 把一行日志里像凭据的东西换成一句描述。
///
/// 认两种东西:
/// - **JWT**(交易站的 hideout / whisper token 就是这个形状):三段 base64url
///   用点连起来,整段不短于 40 个字符 → `<jwt N chars>`。留着长度是因为
///   "token 是不是空的""是不是被截断了"要靠它判断。
/// - **32 位十六进制**(POESESSID 的形状)→ `<session>`。
///
/// 宁可漏也不敢多:认错了只是一行日志读不懂,认漏了是把会话泄在盘上。
/// 所以两条规则都要求形状**完全**对上,不做"看着像"的模糊匹配。
#[must_use]
pub fn redact_for_log(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    for ch in line.chars() {
        if is_token_char(ch) {
            run.push(ch);
        } else {
            out.push_str(&redact_run(&run));
            run.clear();
            out.push(ch);
        }
    }
    out.push_str(&redact_run(&run));
    out
}

/// 凭据里可能出现的字符。别的字符(空格、引号、斜杠、冒号)都是边界。
fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')
}

/// 一段连续的候选字符:整段是 JWT 就整段换掉,否则拆开看有没有哪一小段
/// 是 32 位十六进制。
fn redact_run(run: &str) -> String {
    if run.is_empty() {
        return String::new();
    }
    if is_jwt(run) {
        return format!("<jwt {} chars>", run.chars().count());
    }
    run.split('.')
        .map(|segment| {
            if is_session(segment) {
                "<session>"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn is_jwt(run: &str) -> bool {
    if run.chars().count() < JWT_MIN_CHARS {
        return false;
    }
    let mut segments = run.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    [header, payload, signature]
        .iter()
        .all(|segment| !segment.is_empty() && segment.chars().all(is_base64url_char))
}

fn is_base64url_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

fn is_session(segment: &str) -> bool {
    segment.chars().count() == SESSION_HEX_CHARS && segment.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// 往日志文件追一行,前面加本地时间。
///
/// 每一步失败都直接放弃:日志写不进盘(目录只读、文件被别的程序占着)
/// 不该把程序拖下水 —— 屏幕上那份还在,而为了记一行日志弹一个错误框,
/// 只会盖掉那行日志本来要说的事。
pub fn append_line(path: &Path, line: &str) {
    rotate_if_large(path);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{} {line}", stamp());
}

/// 文件太大就改名让路。只留一代:再往前的日志对一个单人工具没有意义。
fn rotate_if_large(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() <= ROTATE_BYTES {
        return;
    }
    if let Some(rotated) = rotated_path(path) {
        let _ = fs::rename(path, rotated);
    }
}

/// `app.log` → `app.log.1`。
///
/// 不用 `with_extension`:那个会把 `app.log` 的 `log` 当成后缀换掉,
/// 结果是 `app.log.1` 还是 `app.1` 取决于文件名里有几个点。
#[must_use]
pub fn rotated_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    Some(path.with_file_name(format!("{name}.1")))
}

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod logbook_tests {
    use super::*;

    /// 一个真 JWT 的形状:三段 base64url。整段换掉,长度留着。
    #[test]
    fn a_jwt_becomes_its_length() {
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let line = format!("hideout: token={token} status=503");
        let redacted = redact_for_log(&line);
        assert!(!redacted.contains(token), "{redacted}");
        assert_eq!(
            redacted,
            format!("hideout: token=<jwt {} chars> status=503", token.len())
        );
    }

    /// POESESSID 的形状:32 位十六进制。
    #[test]
    fn a_session_cookie_becomes_a_placeholder() {
        let line = "login: POESESSID=0123456789abcdef0123456789ABCDEF sent";
        assert_eq!(
            redact_for_log(line),
            "login: POESESSID=<session> sent",
            "会话 cookie 漏出去了"
        );
    }

    /// 普通日志一个字都不该被改:认错了的代价是日志读不懂。
    #[test]
    fn ordinary_lines_pass_through_untouched() {
        for line in [
            "settings: C:\\Users\\me\\AppData\\Roaming\\pnd\\settings.json",
            "watch added: Choir of the Storm (Forbidden Rites)",
            "opened https://www.pathofexile.com/trade2/search/poe2/Forbidden%20Rites/abc",
            "runtime: cloudflare hold until 1757000000",
            // 40 位十六进制(git sha 那种长度)不是 32 位,不该被当成会话。
            "build 0123456789abcdef0123456789abcdef01234567",
            // 三段点串但太短:是个文件名,不是 token。
            "settings.json.bak",
            "",
        ] {
            assert_eq!(redact_for_log(line), line, "这一行被改坏了");
        }
    }

    /// 一行里有两个凭据也得两个都换掉。
    #[test]
    fn every_credential_in_the_line_is_replaced() {
        let line = "a=0123456789abcdef0123456789abcdef b=0123456789abcdef0123456789abcdef";
        assert_eq!(redact_for_log(line), "a=<session> b=<session>");
    }

    #[test]
    fn the_rotated_file_sits_next_to_the_original() {
        let path = PathBuf::from(r"C:\tmp\pnd\app.log");
        assert_eq!(
            rotated_path(&path),
            Some(PathBuf::from(r"C:\tmp\pnd\app.log.1"))
        );
    }

    /// 落盘那一步:目录不存在也要自己建出来,写进去的那一行带时间戳,
    /// 而且是**脱敏之后**的那一行(脱敏在调用方做,这里只验证追加本身)。
    #[test]
    fn appending_creates_the_file_and_stamps_the_line() {
        let dir = std::env::temp_dir().join(format!("pnd-logbook-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("app.log");

        append_line(&path, "first line");
        append_line(&path, "second line");

        let body = fs::read_to_string(&path).expect("日志文件应该被建出来");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" first line"), "{}", lines[0]);
        assert!(lines[1].ends_with(" second line"), "{}", lines[1]);
        // 时间戳在前面:`2026-09-07 12:34:56` 是 19 个字符。
        assert_eq!(lines[0].len(), 19 + " first line".len());

        let _ = fs::remove_dir_all(&dir);
    }
}
