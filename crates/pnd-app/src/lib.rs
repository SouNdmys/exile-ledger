//! POE Ninja Data 的 GPUI 前端。
//!
//! 库 + 一层薄薄的二进制:窗口是 `run()` 开的,`main.rs` 只调它一句。这样
//! 以后要再加一个入口(比如只画组件的预览窗)时,共用的这套模块只编译一次。
//!
//! 这一步(第 7 步的前半)只搭外壳:导航、五个页面的版式、真的设置页。
//! 运行时接线(轮询事件、提醒卡片)是第 7b 步的事,`shell::AppShell::tick`
//! 里留着那个位置。

pub mod assets;
pub mod crashlog;
pub mod i18n;
pub mod shell;
pub mod theme;

use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};
use gpui_component::Root;

use shell::AppShell;

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
                        title: Some("POE Ninja Data".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| AppShell::new(window, cx));
                    let focus = view.read(cx).focus_handle.clone();
                    window.focus(&focus);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .expect("failed to open window");
            cx.activate(true);
        });
}
