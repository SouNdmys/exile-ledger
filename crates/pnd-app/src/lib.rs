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
#[must_use]
pub fn settings_store() -> pnd_settings::SettingsStore {
    match env_override(SETTINGS_PATH_ENV) {
        Some(path) => pnd_settings::SettingsStore::at_path(PathBuf::from(path)),
        None => pnd_settings::SettingsStore::release_default(),
    }
}

/// 蹲价库的位置。
#[must_use]
pub fn watch_db_path() -> PathBuf {
    redirect(&pnd_storage::default_watch_db_path(), DATA_DIR_ENV)
}

/// ninja 采样缓存的位置(PoE2)。界面这一侧只读它,采样线程另开一条连接写它。
#[must_use]
pub fn ninja_db_path() -> PathBuf {
    ninja_db_path_for(pnd_domain::Game::Poe2)
}

/// 这一代的 ninja 采样缓存放在哪。两代各一个文件,同一个数据目录。
#[must_use]
pub fn ninja_db_path_for(game: pnd_domain::Game) -> PathBuf {
    redirect(&pnd_storage::default_ninja_db_path_for(game), DATA_DIR_ENV)
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

/// 程序改名之前那个数据目录搬家。搬了就返回该记进日志的那一行。
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
    let (old, new) = pnd_storage::migrate_data_dir()?;
    Some(format!(
        "data dir: moved {} to {}",
        old.display(),
        new.display()
    ))
}

/// 这次启动往哪个目录写东西。
fn data_dir() -> PathBuf {
    env_override(DATA_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(pnd_storage::default_data_dir)
}

/// 环境变量的值,空串当成没设 —— `set EXILE_LEDGER_DATA_DIR=` 的意思是"别改",
/// 不是"把数据写到当前目录"。
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn redirect(default: &Path, env_name: &str) -> PathBuf {
    resolve_in_dir(default, env_override(env_name).as_deref())
}

/// 换目录不换文件名:`watch.sqlite` 这些名字只写在 `pnd-storage` 一处,
/// 这里只把它们搬到另一个目录下。
fn resolve_in_dir(default: &Path, dir: Option<&str>) -> PathBuf {
    match (dir, default.file_name()) {
        (Some(dir), Some(name)) => PathBuf::from(dir).join(name),
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
        let moved = resolve_in_dir(&default, Some(r"C:\tmp\pnd"));
        assert_eq!(moved.file_name(), default.file_name());
        assert_eq!(moved.parent(), Some(Path::new(r"C:\tmp\pnd")));
    }

    #[test]
    fn without_an_override_the_default_path_is_untouched() {
        let default = pnd_storage::default_ninja_db_path();
        assert_eq!(resolve_in_dir(&default, None), default);
    }
}
