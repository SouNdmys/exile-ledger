//! 词缀热度页(第二阶段)。
//!
//! 数据要等角色采样和词缀聚合(第 12 步)才有。这一页现在只把版式定下来:
//! 三个筛选器、七列、一句说明。先画出来是有用的 —— 列宽和筛选器的组合
//! 是设计决定,不该等到数据到位那天才第一次被人看见。

use gpui::{Context, ParentElement, Styled, div, px};

use super::{TableContent, column, number_column};
use crate::i18n::Text;
use crate::shell::{AppShell, Choice, page_heading, panel, picker, table};

/// 部位下拉。部位名是 ninja 的 `inventoryId`,第 12 步从采样结果里长出来;
/// 这里先摆几个最常看的。
pub fn slot_choices(text: &'static Text) -> Vec<Choice> {
    let mut items = vec![Choice::new("", text.common_all)];
    items.extend(["BodyArmour", "Helm", "Gloves", "Boots", "Ring", "Amulet"].map(Choice::plain));
    items
}

/// 稀有度下拉。
pub fn rarity_choices(text: &'static Text) -> Vec<Choice> {
    vec![
        Choice::new("", text.common_all),
        Choice::new("unique", text.mods_rarity_unique),
        Choice::new("rare", text.mods_rarity_rare),
        Choice::new("magic", text.mods_rarity_magic),
    ]
}

/// 词缀类型下拉。五种类型在 ninja 的角色详情里是五个数组,统计口径必须分开。
pub fn kind_choices(text: &'static Text) -> Vec<Choice> {
    vec![
        Choice::new("", text.common_all),
        Choice::new("explicit", text.mods_kind_explicit),
        Choice::new("implicit", text.mods_kind_implicit),
        Choice::new("crafted", text.mods_kind_crafted),
        Choice::new("desecrated", text.mods_kind_desecrated),
        Choice::new("rune", text.mods_kind_rune),
    ]
}

/// 词缀表的列。行是空的:这一页的数据是第 12 步的事,空表的那句话就是
/// 它的说明,与其画一行编出来的示例,不如让它诚实地空着。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("stat", text.mods_col_stat, 200.),
            column("family", text.mods_col_family, 140.),
            number_column("characters", text.mods_col_characters_percent, 90.),
            number_column("occurrences", text.mods_col_occurrences, 100.),
            number_column("p25", text.mods_col_p25, 60.),
            number_column("p50", text.mods_col_p50, 60.),
            number_column("p75", text.mods_col_p75, 60.),
        ],
        rows: Vec::new(),
        empty: text.mods_phase_two_placeholder.into(),
    }
}

impl AppShell {
    pub(crate) fn render_ninja_mods(&mut self, _cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .child(page_heading(text.mods_heading, text.mods_subtitle))
            .child(
                panel()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(8.))
                    .child(picker(text.mods_slot_label, &self.mods_slot_select, 170.))
                    .child(picker(
                        text.mods_rarity_label,
                        &self.mods_rarity_select,
                        140.,
                    ))
                    .child(picker(text.mods_kind_label, &self.mods_kind_select, 150.)),
            )
            .child(
                panel()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(table(&self.mods_table)),
            )
    }
}
