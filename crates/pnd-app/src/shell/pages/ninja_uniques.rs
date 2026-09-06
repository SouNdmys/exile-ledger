//! 暗金热度页:热门 BD 在穿什么,拼上经济接口的参考价和挂单数。
//!
//! 人数来自 poe.ninja builds 搜索的 `items` 分面(全联赛一次请求就有,
//! 不靠采样),价格来自它文档化的经济接口。两边拼在一张表上,才回答得了
//! 真正的问题:"大家都在用的这件东西,现在多少钱、有几个人在卖"。

use gpui::{Context, ParentElement, Styled, div, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Sizable as _, Size, StyledExt as _};

use super::{Cell, TableContent, column, number_column};
use crate::i18n::{self, Text};
use crate::shell::{AppShell, Choice, hint, page_heading, panel, picker, table};

/// 联赛下拉的选项。现在只有设置里那一个 —— 联赛清单是第 9 步从
/// `build-index-state` 读回来的。
pub fn league_choices(settings: &pnd_settings::AppSettings) -> Vec<Choice> {
    vec![Choice::plain(settings.league.clone())]
}

/// 职业下拉。名字是专有名词不翻译,只有"全部"跟着语言走。
pub fn class_choices(text: &'static Text) -> Vec<Choice> {
    let mut items = vec![Choice::new("", text.common_all)];
    items.extend(
        [
            "Gemling Legionnaire",
            "Infernalist",
            "Stormweaver",
            "Deadeye",
            "Titan",
        ]
        .map(Choice::plain),
    );
    items
}

/// 主技能下拉。同上,只有"全部"要翻译。
pub fn skill_choices(text: &'static Text) -> Vec<Choice> {
    let mut items = vec![Choice::new("", text.common_all)];
    items.extend(["Spark", "Lightning Arrow", "Bone Storm", "Cast on Freeze"].map(Choice::plain));
    items
}

/// 暗金榜的列和示例行。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("name", text.uniques_col_name, 240.),
            number_column("characters", text.uniques_col_characters, 90.),
            number_column("share", text.uniques_col_share, 70.),
            number_column("price", text.uniques_col_reference_price, 110.),
            number_column("listings", text.uniques_col_listings, 80.),
            number_column("seven_day", text.uniques_col_seven_day, 70.),
        ],
        // 三行示例,数字取自计划里今天实测的那一批,列宽按最长的名字量。
        rows: vec![
            vec![
                Cell::accent("Wake of Destruction"),
                Cell::data("7158"),
                Cell::data("11.7%"),
                Cell::data("42 exalted"),
                Cell::data("31"),
                Cell::good("+6%"),
            ],
            vec![
                Cell::plain("Beira's Anguish"),
                Cell::data("6413"),
                Cell::data("10.4%"),
                Cell::data("128 exalted"),
                Cell::data("12"),
                Cell::warn("-9%"),
            ],
            vec![
                Cell::plain("Arakaali's Gift"),
                Cell::data("5044"),
                Cell::data("8.2%"),
                Cell::data("240 exalted"),
                Cell::data("24"),
                Cell::muted("0%"),
            ],
        ],
        empty: text.uniques_empty.into(),
    }
}

impl AppShell {
    pub(crate) fn render_ninja_uniques(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .child(page_heading(text.uniques_heading, text.uniques_subtitle))
            .child(
                panel()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(8.))
                    .child(picker(
                        text.uniques_league_label,
                        &self.uniques_league_select,
                        180.,
                    ))
                    .child(picker(
                        text.uniques_class_label,
                        &self.uniques_class_select,
                        190.,
                    ))
                    .child(picker(
                        text.uniques_skill_label,
                        &self.uniques_skill_select,
                        180.,
                    ))
                    .child(div().flex_grow())
                    .child(
                        Button::new("uniques-refresh")
                            .primary()
                            .label(text.uniques_refresh)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.not_wired_yet("ninja sampling", cx);
                            })),
                    ),
            )
            .child(
                panel()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(table(&self.uniques_table)),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(16.))
                    // 采样进度和"这份快照有多旧"是同一件事的两面:一个说
                    // 现在跑到哪,一个说上一轮是什么时候的。
                    .child(hint(i18n::fill(text.uniques_progress, &["0", "65"])))
                    .child(hint(i18n::fill(
                        text.uniques_stale_hint,
                        &[&format!("2 {}", text.common_hours_short)],
                    ))),
            )
    }
}
