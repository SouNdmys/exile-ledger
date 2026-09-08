//! SQLite 持久化:蹲价状态与提醒历史、市场观察(`watch.sqlite`)、
//! ninja 采样缓存与词缀统计(`ninja.sqlite`)。

use std::path::PathBuf;

use pnd_domain::Game;

pub mod ninja;
pub mod observe;
pub mod watch;

pub use ninja::*;
pub use observe::*;
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
///
/// **签名刻意不带参数**:它问的一直是 PoE2,让老调用方一个字都不用改。
pub fn default_ninja_db_path() -> PathBuf {
    default_ninja_db_path_for(Game::Poe2)
}

/// 这一代的 ninja 采样缓存放在哪。
pub fn default_ninja_db_path_for(game: Game) -> PathBuf {
    default_data_dir().join(ninja_db_file_name(game))
}

/// 两代各一个文件名。
///
/// PoE2 那个**保持原样**:本机那个库里已经躺着一整轮采样(两千个角色、
/// 一整天的请求配额),换个名字等于让它明天从头再采一遍。
#[must_use]
pub fn ninja_db_file_name(game: Game) -> &'static str {
    match game {
        Game::Poe1 => "ninja-poe1.sqlite",
        Game::Poe2 => "ninja.sqlite",
    }
}
