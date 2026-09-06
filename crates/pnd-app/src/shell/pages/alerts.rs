//! 提醒记录页:弹过的每一张卡片,以及当时你拿它做了什么。
//!
//! 卡片是会自己收起来的,所以它必须在别处留下痕迹 —— 否则"刚才那件东西
//! 多少钱来着"就永远查不回来了。四个按钮和卡片上的是同一套动作,理由是
//! 卡片收起来之后你还想去买。

use gpui::{Context, ParentElement, Styled, div, px};
use gpui_component::button::Button;
use gpui_component::{Sizable as _, Size};

use super::{Cell, TableContent, column, number_column};
use crate::i18n::Text;
use crate::shell::{AppShell, page_heading, panel, table};

/// 提醒表的列和示例行。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("time", text.alerts_col_time, 120.),
            column("item", text.alerts_col_item, 200.),
            number_column("price", text.alerts_col_price, 90.),
            column("seller", text.alerts_col_seller, 130.),
            column("source", text.alerts_col_source, 70.),
            column("action", text.alerts_col_action, 90.),
        ],
        rows: vec![vec![
            Cell::data("09-06 23:41"),
            Cell::plain("Choir of the Storm"),
            Cell::good("18 divine"),
            Cell::muted("Exile#1234"),
            Cell::muted(text.alerts_source_live),
            Cell::muted(text.alerts_action_none),
        ]],
        empty: text.alerts_empty.into(),
    }
}

impl AppShell {
    pub(crate) fn render_alerts(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .child(page_heading(text.alerts_heading, text.alerts_subtitle))
            .child(
                panel()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(table(&self.alerts_table)),
            )
            .child(
                // 选中一行之后对它做的四件事。第 7b 步接上去:打开交易页走
                // `ShellExecuteW`,复制私聊走剪贴板,忽略写回 `alerts` 表,
                // 去藏身处是第二阶段的事。
                panel()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(8.))
                    .child(
                        Button::new("alert-open")
                            .label(text.alerts_open)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.not_wired_yet(cx))),
                    )
                    .child(
                        Button::new("alert-copy-whisper")
                            .label(text.alerts_copy_whisper)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.not_wired_yet(cx))),
                    )
                    .child(
                        Button::new("alert-hideout")
                            .label(text.alerts_hideout)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.not_wired_yet(cx))),
                    )
                    .child(
                        Button::new("alert-dismiss")
                            .label(text.alerts_dismiss)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.not_wired_yet(cx))),
                    ),
            )
    }
}
