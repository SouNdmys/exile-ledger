//! POE Ninja Data 的 GPUI 前端骨架。
//!
//! 目前只开一扇空窗口,用来证明工作区能编译、gpui-component 的运行时能
//! 起来。真正的页面(蹲价、提醒记录、暗金热度、设置)在后续阶段替换掉
//! 这里的占位视图。

use gpui::{
    App, Application, Bounds, Context, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use gpui_component::Root;

pub const WORKBENCH_SIZE: (f32, f32) = (1180.0, 640.0);

/// 骨架阶段的根视图:只画一行居中文字。
struct Shell;

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child("POE Ninja Data")
    }
}

/// Opens the product window and runs until it closes.
pub fn run() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);

        let (width, height) = WORKBENCH_SIZE;
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(width), px(height))),
                titlebar: Some(TitlebarOptions {
                    title: Some("POE Ninja Data".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|_| Shell);
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
