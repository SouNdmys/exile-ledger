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

/// 数据目录的名字。程序叫 Exile Ledger,盘上那个文件夹就跟着叫这个。
const APP_DIR_NAME: &str = "ExileLedger";

/// 改名之前它叫什么。留着这一行只为一件事:启动时把老文件夹整个搬过来,
/// 见 [`migrate_data_dir`]。
const LEGACY_APP_DIR_NAME: &str = "PoeNinjaData";

/// 和 `settings.json` 同一个目录:`%LOCALAPPDATA%\ExileLedger`。
/// 取不到环境变量时退化成相对路径,程序照样能在当前目录跑起来。
pub fn default_data_dir() -> PathBuf {
    local_app_data().join(APP_DIR_NAME)
}

/// 程序改名之前用的那个数据目录:`%LOCALAPPDATA%\PoeNinjaData`。
pub fn legacy_data_dir() -> PathBuf {
    local_app_data().join(LEGACY_APP_DIR_NAME)
}

fn local_app_data() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
}

/// 启动时对老数据目录该做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDirMigration {
    /// 把老文件夹整个改名成新的。
    Rename,
    /// 什么都不做。
    Leave,
}

/// 纯判定:只看那两个文件夹在不在,不碰盘。
///
/// 只有"新的还没有、老的还在"才搬。新文件夹已经存在,就说明这台机器要么
/// 已经搬过一次、要么本来就是新装的 —— 这时候再把老的盖上去,等于拿一份
/// 陈年数据顶掉正在用的那份会话和提醒历史。
#[must_use]
pub fn data_dir_migration(new_exists: bool, old_exists: bool) -> DataDirMigration {
    if !new_exists && old_exists {
        DataDirMigration::Rename
    } else {
        DataDirMigration::Leave
    }
}

/// 启动时那一次搬家到底怎么了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataDirMove {
    /// 搬过去了,这次启动用新目录。
    Moved { old: PathBuf, new: PathBuf },
    /// 没什么可搬的(全新安装,或者早就搬过了)。
    NotNeeded,
    /// 该搬,但改名失败了 —— 最常见的原因是老版本还开着,
    /// 它把老目录里的两个 sqlite 和 `app.log` 占住了,而 Windows 不让
    /// 改名一个还有人开着文件的文件夹。
    Failed {
        old: PathBuf,
        new: PathBuf,
        error: String,
    },
}

/// 真去搬一次。
///
/// **一次文件夹改名就够了**:`settings.json`、两个 sqlite、`app.log`、
/// `panic.log`、登录窗那份 webview2 状态本来就都躺在这一个文件夹里,
/// 所以它们是一起走的,没有"搬到一半"这种中间状态。
pub fn migrate_data_dir() -> DataDirMove {
    let new = default_data_dir();
    let old = legacy_data_dir();
    if data_dir_migration(new.exists(), old.exists()) != DataDirMigration::Rename {
        return DataDirMove::NotNeeded;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => DataDirMove::Moved { old, new },
        Err(error) => DataDirMove::Failed {
            old,
            new,
            error: error.to_string(),
        },
    }
}

/// 搬完之后,这次启动该往哪个目录写。纯判定,不碰盘,所以测得起来。
///
/// 只有"该搬却没搬动"这一种情况回老目录:数据还在那儿,这次就直接在那儿跑,
/// 顺带不把新目录建出来 —— 新目录一旦存在,下次启动就不会再试着搬了。
#[must_use]
pub fn data_dir_after(outcome: &DataDirMove) -> PathBuf {
    match outcome {
        DataDirMove::Failed { old, .. } => old.clone(),
        DataDirMove::Moved { .. } | DataDirMove::NotNeeded => default_data_dir(),
    }
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

#[cfg(test)]
mod data_dir_tests {
    use super::*;

    /// 改名当天第一次开程序:新文件夹还不存在,老的一整份数据还在 —— 搬。
    #[test]
    fn the_old_folder_moves_when_the_new_one_is_not_there_yet() {
        assert_eq!(data_dir_migration(false, true), DataDirMigration::Rename);
    }

    /// 已经搬过一次(或者用户自己建了新文件夹)就再也不动它。
    /// 这一条挡的是"拿老数据盖掉正在用的那份"。
    #[test]
    fn an_existing_new_folder_is_never_overwritten() {
        assert_eq!(data_dir_migration(true, true), DataDirMigration::Leave);
        assert_eq!(data_dir_migration(true, false), DataDirMigration::Leave);
    }

    /// 全新安装:两个都没有,没什么可搬的。
    #[test]
    fn a_fresh_install_has_nothing_to_move() {
        assert_eq!(data_dir_migration(false, false), DataDirMigration::Leave);
    }

    /// 搬失败的那次启动**留在老目录里跑**。
    ///
    /// 老版本还开着的时候改名一定失败;这时候要是照旧用新目录,新目录就被
    /// 设置和 sqlite 现建出来了 —— 搬家条件(新目录不存在)从此永远不成立,
    /// 用户的会话、搜索列表、提醒历史就永远留在老目录里没人读,界面看着
    /// 像一次全新安装。所以这一次先将就用老目录,下次启动再搬。
    #[test]
    fn a_failed_move_keeps_this_run_in_the_old_folder() {
        let outcome = DataDirMove::Failed {
            old: legacy_data_dir(),
            new: default_data_dir(),
            error: "文件正由另一进程使用".to_owned(),
        };
        assert_eq!(data_dir_after(&outcome), legacy_data_dir());
    }

    /// 搬成了、或者本来就没什么可搬的:都用新目录。
    #[test]
    fn every_other_outcome_uses_the_new_folder() {
        assert_eq!(data_dir_after(&DataDirMove::NotNeeded), default_data_dir());
        let moved = DataDirMove::Moved {
            old: legacy_data_dir(),
            new: default_data_dir(),
        };
        assert_eq!(data_dir_after(&moved), default_data_dir());
    }

    /// 两个目录只差最后那一段名字 —— 搬家搬的就是这一段。
    #[test]
    fn the_two_folders_are_siblings() {
        assert_eq!(default_data_dir().parent(), legacy_data_dir().parent());
        assert_eq!(
            default_data_dir().file_name().and_then(|n| n.to_str()),
            Some("ExileLedger")
        );
        assert_eq!(
            legacy_data_dir().file_name().and_then(|n| n.to_str()),
            Some("PoeNinjaData")
        );
    }
}
