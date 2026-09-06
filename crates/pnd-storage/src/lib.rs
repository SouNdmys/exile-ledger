//! SQLite 持久化:蹲价状态与提醒历史(`watch.sqlite`)、
//! ninja 采样缓存与词缀统计(`ninja.sqlite`)。

use std::path::PathBuf;

pub mod watch;

pub use watch::*;

// ninja store lands in step 9

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
