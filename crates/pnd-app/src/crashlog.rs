//! panic 落盘。
//!
//! release 是 `panic = "abort"` 加 GUI 子系统:没有控制台,没有栈回溯,
//! 任何 panic 都表现为窗口无声消失。中止之前唯一的出口是 panic hook,
//! 把发生了什么追加写到设置文件旁边的 `panic.log` —— 这样"双击没反应"
//! 至少留下一行能拿来报告的东西。

use std::path::PathBuf;

/// `%LOCALAPPDATA%\PoeNinjaData\panic.log`:和两个数据库同一个目录,
/// 所以 `PND_DATA_DIR` 也把它一起搬走。
pub fn panic_log_path() -> PathBuf {
    crate::redirect(
        &pnd_storage::default_data_dir().join("panic.log"),
        crate::DATA_DIR_ENV,
    )
}

/// 一次 panic 一行:时间、版本、位置、消息。版本在最前面 —— 报告回来的
/// 第一个问题永远是"你跑的是哪个版本"。
pub fn panic_log_line(
    now: chrono::DateTime<chrono::Utc>,
    version: &str,
    message: &str,
    location: Option<&str>,
) -> String {
    format!(
        "{} v{version} panic at {}: {message}\n",
        now.format("%Y-%m-%d %H:%M:%S UTC"),
        location.unwrap_or("unknown location")
    )
}

/// 装 hook。写盘失败就只剩 stderr(GUI 子系统下等于没有),但绝不能再 panic。
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_owned());
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()));
        let line = panic_log_line(
            chrono::Utc::now(),
            env!("CARGO_PKG_VERSION"),
            &message,
            location.as_deref(),
        );
        let path = panic_log_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write as _;
            let _ = file.write_all(line.as_bytes());
        }
        eprint!("{line}");
    }));
}

#[cfg(test)]
mod crashlog_tests {
    use super::{panic_log_line, panic_log_path};

    #[test]
    fn the_log_line_names_version_location_and_message() {
        let now = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).expect("ts");
        let line = panic_log_line(now, "0.1.0", "index out of bounds", Some("src/x.rs:42"));
        assert!(line.contains("0.1.0"), "{line}");
        assert!(line.contains("src/x.rs:42"), "{line}");
        assert!(line.contains("index out of bounds"), "{line}");
        assert!(line.ends_with('\n'), "one panic, one line: {line:?}");
        let line = panic_log_line(now, "0.1.0", "boom", None);
        assert!(line.contains("boom"), "{line}");
    }

    #[test]
    fn the_log_sits_next_to_the_databases() {
        let path = panic_log_path();
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("panic.log"));
        let data_dir = pnd_storage::default_data_dir();
        assert_eq!(path.parent(), Some(data_dir.as_path()));
    }
}
