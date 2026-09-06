//! SQLite 持久化:蹲价状态与提醒历史(`watch.sqlite`)、
//! ninja 采样缓存与词缀统计(`ninja.sqlite`)。

use std::path::PathBuf;

pub mod ninja;
pub mod watch;

pub use ninja::*;
pub use watch::*;

/// 和 `settings.json` 同一个目录:`%LOCALAPPDATA%\PoeNinjaData`。
/// 取不到环境变量时退化成相对路径,程序照样能在当前目录跑起来。
pub fn default_data_dir() -> PathBuf {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    PathBuf::from(local).join("PoeNinjaData")
}

/// 蹲价库。ninja 那个库是可以随手删的缓存,这个不是 —— 提醒历史只有这一份。
pub fn default_watch_db_path() -> PathBuf {
    default_data_dir().join("watch.sqlite")
}

/// ninja 采样缓存。里面全是能重新抓回来的东西,删掉最多就是下次开程序时
/// 重跑一轮采样,所以出问题时可以放心让用户直接删掉这个文件。
pub fn default_ninja_db_path() -> PathBuf {
    default_data_dir().join("ninja.sqlite")
}
