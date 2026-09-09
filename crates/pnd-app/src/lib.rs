//! Exile Ledger 的 GPUI 前端。
//!
//! 库 + 一层薄薄的二进制:窗口是 `run()` 开的,`main.rs` 只调它一句。这样
//! 以后要再加一个入口(比如只画组件的预览窗)时,共用的这套模块只编译一次。
//!
//! 第 7b 步把运行时接了上来:`shell::AppShell` 启动时开一条
//! `pnd-runtime` actor 线程和一条提醒卡片线程,120ms 的 tick 把两边的事件
//! 抽干,蹲价页 / 提醒记录页画的都是真数据。

pub mod assets;
pub mod crashlog;
pub mod i18n;
pub mod logbook;
pub mod shell;
pub mod theme;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};
use gpui_component::Root;

use shell::AppShell;

/// 把设置文件换个地方的环境变量。
///
/// 存在的理由只有一个:端到端手测要跑真程序,而真程序默认读的是本机那份
/// 有真会话、真搜索列表的 `settings.json`。测试用例设一下这个变量,就不会
/// 碰到它。**只有 `pnd-app` 认这两个变量**,`pnd-settings` / `pnd-storage`
/// 里的默认路径不受影响。
pub const SETTINGS_PATH_ENV: &str = "EXILE_LEDGER_SETTINGS_PATH";
/// 同上,换的是两个 sqlite、`app.log`、`panic.log` 所在的目录。
pub const DATA_DIR_ENV: &str = "EXILE_LEDGER_DATA_DIR";

/// 这次启动要用的设置文件。
///
/// 目录跟着 [`data_dir`] 走,不再自己问 `pnd-settings` 要默认路径:老目录
/// 搬不动的那次启动,设置文件得和两个 sqlite 待在同一个(老)目录里。
#[must_use]
pub fn settings_store() -> pnd_settings::SettingsStore {
    match env_override(SETTINGS_PATH_ENV) {
        Some(path) => pnd_settings::SettingsStore::at_path(PathBuf::from(path)),
        None => pnd_settings::SettingsStore::in_dir(&data_dir()),
    }
}

/// 蹲价库的位置。
#[must_use]
pub fn watch_db_path() -> PathBuf {
    redirect(&pnd_storage::default_watch_db_path())
}

/// ninja 采样缓存的位置(PoE2)。界面这一侧只读它,采样线程另开一条连接写它。
#[must_use]
pub fn ninja_db_path() -> PathBuf {
    ninja_db_path_for(pnd_domain::Game::Poe2)
}

/// 这一代的 ninja 采样缓存放在哪。两代各一个文件,同一个数据目录。
#[must_use]
pub fn ninja_db_path_for(game: pnd_domain::Game) -> PathBuf {
    redirect(&pnd_storage::default_ninja_db_path_for(game))
}

/// 日志文件的位置。
///
/// 和两个 sqlite 放在同一个数据目录下,所以 `EXILE_LEDGER_DATA_DIR` 一并搬走它 ——
/// 手测时不该往本机那份真日志里掺测试的行。
#[must_use]
pub fn app_log_path() -> PathBuf {
    data_dir().join("app.log")
}

/// 登录窗那个 Edge 内核的用户数据目录。
///
/// 和两个 sqlite 放在同一个数据目录下,单独一个子目录:里面是本程序自己
/// 那份浏览器状态(cookie、缓存),和你平时用的浏览器毫无关系,
/// 登录出问题时把这个目录整个删掉就是"重来一次"。
#[must_use]
pub fn webview2_data_dir() -> PathBuf {
    data_dir().join("webview2")
}

/// 这次启动最后定下来的数据目录。搬家跑完写一次,之后 [`data_dir`] 只读它。
///
/// 用 `OnceLock` 是为了让下面每一条路径**说的是同一个目录**:设置文件、
/// 两个 sqlite、`app.log`、`panic.log`、webview2 —— 它们各自去问一遍
/// "默认目录在哪"的话,搬家没搬动的那次启动就会一半在新目录、一半在老目录。
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 程序改名之前那个数据目录搬家。有话要记进日志就返回那一行。
///
/// 必须在打开这个目录里任何一个文件**之前**调:先搬再开,老文件夹里的
/// 会话、搜索列表、提醒历史才跟着一起过来;反过来的话新文件夹会被
/// SQLite 先建出来,搬家条件就永远不成立了。
///
/// 设了 [`DATA_DIR_ENV`] 的那次启动一律不搬 —— 那是手测,不该动真数据。
#[must_use]
pub fn migrate_data_dir() -> Option<String> {
    if env_override(DATA_DIR_ENV).is_some() {
        return None;
    }
    let outcome = pnd_storage::migrate_data_dir();
    // 已经定过就算了(一次启动只该走到这儿一次),不覆盖。
    let _ = DATA_DIR.set(pnd_storage::data_dir_after(&outcome));
    data_dir_move_line(&outcome)
}

/// 搬家结果里该说给用户听的那一句。没搬动那句要把原因说全:
/// 用户看见"老版本还开着吗"才知道去托盘里把它退掉,再开一次就搬过来了。
fn data_dir_move_line(outcome: &pnd_storage::DataDirMove) -> Option<String> {
    match outcome {
        pnd_storage::DataDirMove::NotNeeded => None,
        pnd_storage::DataDirMove::Moved { old, new } => Some(format!(
            "data dir: moved {} to {}",
            old.display(),
            new.display()
        )),
        pnd_storage::DataDirMove::Failed { old, new, error } => Some(format!(
            "data dir: could not move {} to {} ({error}) (still in use by the old version?) \
             — using {} this run",
            old.display(),
            new.display(),
            old.display()
        )),
    }
}

/// 这次启动往哪个目录写东西。
///
/// 搬家还没跑过(单元测试、探针)就退回默认目录 —— 和从前一样。
fn data_dir() -> PathBuf {
    if let Some(dir) = env_override(DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    DATA_DIR
        .get()
        .cloned()
        .unwrap_or_else(pnd_storage::default_data_dir)
}

/// 环境变量的值,空串当成没设 —— `set EXILE_LEDGER_DATA_DIR=` 的意思是"别改",
/// 不是"把数据写到当前目录"。
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// 把 `pnd-storage` 给的那条默认路径挪进这次启动真正在用的目录。
fn redirect(default: &Path) -> PathBuf {
    resolve_in_dir(default, Some(&data_dir()))
}

/// 换目录不换文件名:`watch.sqlite` 这些名字只写在 `pnd-storage` 一处,
/// 这里只把它们搬到另一个目录下。
fn resolve_in_dir(default: &Path, dir: Option<&Path>) -> PathBuf {
    match (dir, default.file_name()) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => default.to_path_buf(),
    }
}

/// 开窗尺寸。表格的列宽预算按这个宽度定,所以最小宽度不能比它小太多。
pub const WORKBENCH_SIZE: (f32, f32) = (1180.0, 640.0);
/// 最小窗口。上游表格是纯像素的、不会回流:再窄下去最后一列就静默走出面板。
pub const WORKBENCH_MIN_SIZE: (f32, f32) = (900.0, 560.0);

/// Opens the product window and runs until it closes.
pub fn run() {
    // release 是 panic = "abort" + 无控制台:先装 hook,否则任何 panic 都是
    // 窗口无声消失,连"哪个版本、哪一行"都留不下。
    crashlog::install();
    // 不注册资源源,gpui-component 的 SVG 图标(下拉箭头、菜单勾)会静默
    // 画成空白。
    Application::new()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);
            // 顺序有意:先把我们的深色盘装进 gpui-component 的主题结构体,
            // 再开窗。反过来的话第一帧画的是上游的默认色。
            theme::apply_app_theme(cx);

            let (width, height) = WORKBENCH_SIZE;
            let (min_width, min_height) = WORKBENCH_MIN_SIZE;
            let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(min_width), px(min_height))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Exile Ledger".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| AppShell::new(window, cx));
                    let focus = view.read(cx).focus_handle.clone();
                    window.focus(&focus);
                    // 点右上角那个叉:默认只是把窗口缩进通知区,后台照跑。
                    // 回调返回 false = "别关",gpui 就把这条 WM_CLOSE 吃掉。
                    let shell = view.clone();
                    window.on_window_should_close(cx, move |_, cx| {
                        shell.update(cx, |shell, _| shell.on_close_requested())
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .expect("failed to open window");
            cx.activate(true);
        });
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// 环境变量只换目录,文件名跟着 `pnd-storage` 走 —— 两处各写一遍
    /// `watch.sqlite`,迟早会有一处改漏。
    #[test]
    fn a_data_dir_override_keeps_the_file_names() {
        let default = pnd_storage::default_watch_db_path();
        let moved = resolve_in_dir(&default, Some(Path::new(r"C:\tmp\pnd")));
        assert_eq!(moved.file_name(), default.file_name());
        assert_eq!(moved.parent(), Some(Path::new(r"C:\tmp\pnd")));
    }

    #[test]
    fn without_an_override_the_default_path_is_untouched() {
        let default = pnd_storage::default_ninja_db_path();
        assert_eq!(resolve_in_dir(&default, None), default);
    }

    /// 老目录搬不动的那次启动:设置文件也得跟着回老目录。
    ///
    /// 这条盯的是最容易漏的一处 —— `settings.json` 的默认路径是
    /// `pnd-settings` 自己拼的(永远是新目录),两个 sqlite 却跟着
    /// `data_dir()` 走。漏了的话会话和搜索列表读的是空的新文件。
    #[test]
    fn a_failed_move_puts_every_file_back_in_the_old_folder() {
        let old = PathBuf::from(r"C:\fake\Local\PoeNinjaData");
        let outcome = pnd_storage::DataDirMove::Failed {
            old: old.clone(),
            new: PathBuf::from(r"C:\fake\Local\ExileLedger"),
            error: "被另一个进程占用".to_owned(),
        };
        let dir = pnd_storage::data_dir_after(&outcome);
        assert_eq!(dir, old);
        assert_eq!(
            pnd_settings::SettingsStore::in_dir(&dir).path(),
            old.join("settings.json")
        );
        assert_eq!(
            resolve_in_dir(&pnd_storage::default_watch_db_path(), Some(&dir)),
            old.join("watch.sqlite")
        );
    }

    /// 没搬动那一行得把三件事说全:搬不动、可能是老版本还开着、这次用老目录。
    #[test]
    fn the_failed_move_line_says_why_and_what_it_did_instead() {
        let line = data_dir_move_line(&pnd_storage::DataDirMove::Failed {
            old: PathBuf::from(r"C:\fake\Local\PoeNinjaData"),
            new: PathBuf::from(r"C:\fake\Local\ExileLedger"),
            error: "被另一个进程占用".to_owned(),
        })
        .expect("搬不动是要说一声的");
        assert!(line.contains("could not move"), "{line}");
        assert!(line.contains("old version"), "{line}");
        assert!(
            line.contains(r"using C:\fake\Local\PoeNinjaData this run"),
            "{line}"
        );
    }

    /// 没什么可搬的就一个字都不用记。
    #[test]
    fn nothing_to_move_logs_nothing() {
        assert_eq!(
            data_dir_move_line(&pnd_storage::DataDirMove::NotNeeded),
            None
        );
    }
}
