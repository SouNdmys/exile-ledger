//! 词缀热度页:采样到的角色身上,各个部位实际带着哪些词缀。
//!
//! 统计口径是计划里定的那四维:**部位 × 稀有度 × 词缀类型 × stat id**。
//! 三个下拉就是前三维,表里一行是一个 stat id。三个下拉的选项不是写死的,
//! 而是从库里已有的统计里长出来的 —— 写死的话,ninja 哪天多一个部位,
//! 那个部位的行就永远筛不出来。
//!
//! 默认藏掉占比低于 2% 的行:2,000 个样本下,占比比这还低的"刁钻词缀"
//! 统计误差比它本身还大,铺出来只会误导。想看就把开关打开。

use gpui::{Context, ParentElement, SharedString, Styled, div, px};
use gpui_component::StyledExt as _;
use gpui_component::switch::Switch;

use pnd_ninja::aggregate::SlotModStat;

use super::{Cell, TableContent, column, number_column};
use crate::i18n::{self, Text};
use crate::shell::ninja::{mod_share_percent, percent_text};
use crate::shell::{AppShell, Choice, hint, page_heading, panel, picker, table};

/// 占比低于这个数的词缀默认不显示(计划里定的阈值)。
pub const MIN_SHARE_PERCENT: f64 = 2.0;

/// 稀有度下拉一开始停在哪一档。
///
/// 停在"稀有"而不是"全部",是因为这一页要回答的问题是**"我该给自己做一件
/// 什么样的装备"**,而那个问题只有稀有装答得上:能自己搓的就是它。
pub const DEFAULT_RARITY: &str = "Rare";

/// 暗金那一档在 ninja 角色详情里的原文。
pub const UNIQUE_RARITY: &str = "Unique";

/// 三个下拉共用的一件事:把库里出现过的值去重排好,前面加一条"全部"。
fn choices_from(
    values: impl Iterator<Item = String>,
    label: impl Fn(&str) -> String,
    text: &'static Text,
) -> Vec<Choice> {
    let mut seen: Vec<String> = values.collect();
    seen.sort_unstable();
    seen.dedup();
    let mut items = vec![Choice::new("", text.common_all)];
    items.extend(
        seen.into_iter()
            .map(|value| Choice::new(value.clone(), label(&value))),
    );
    items
}

/// 部位下拉。部位名是 ninja 的 `inventoryId`,不翻译 —— 它就是游戏里的栏位名。
pub fn slot_choices(mods: &[SlotModStat], text: &'static Text) -> Vec<Choice> {
    choices_from(
        mods.iter().map(|stat| stat.slot.clone()),
        |value: &str| value.to_owned(),
        text,
    )
}

/// 稀有度下拉。
///
/// [`DEFAULT_RARITY`] 那一档**永远在选项里**,哪怕这一轮的统计还是空的:
/// 它是下拉默认选中的那一项,选项不在的话下拉一开始就是个空占位符。
pub fn rarity_choices(mods: &[SlotModStat], text: &'static Text) -> Vec<Choice> {
    let values = mods
        .iter()
        .map(|stat| stat.rarity.clone())
        .chain(std::iter::once(DEFAULT_RARITY.to_owned()));
    choices_from(values, |value| rarity_label(value, text).to_owned(), text)
}

/// 词缀类型下拉。五种类型在 ninja 的角色详情里是五个数组,统计口径必须分开。
pub fn kind_choices(mods: &[SlotModStat], text: &'static Text) -> Vec<Choice> {
    choices_from(
        mods.iter().map(|stat| stat.mod_kind.clone()),
        |value| kind_label(value, text).to_owned(),
        text,
    )
}

/// 库里存的是 ninja 的原文(`Rare`/`Unique`/…)。认不出来的原样显示:
/// 接口哪天多一档,下拉里出现一个英文词,好过那一档整个消失。
fn rarity_label<'a>(rarity: &'a str, text: &'static Text) -> &'a str {
    match rarity {
        "Unique" => text.mods_rarity_unique,
        "Rare" => text.mods_rarity_rare,
        "Magic" => text.mods_rarity_magic,
        "Normal" => text.mods_rarity_normal,
        other => other,
    }
}

fn kind_label<'a>(kind: &'a str, text: &'static Text) -> &'a str {
    match kind {
        "explicit" => text.mods_kind_explicit,
        "implicit" => text.mods_kind_implicit,
        "crafted" => text.mods_kind_crafted,
        "desecrated" => text.mods_kind_desecrated,
        "rune" => text.mods_kind_rune,
        other => other,
    }
}

/// 词缀表的列。行由 [`table_content_for`] 填。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("stat", text.mods_col_stat, 220.),
            column("family", text.mods_col_family, 150.),
            number_column("share", text.mods_col_characters_percent, 85.),
            number_column("characters", text.mods_col_characters, 90.),
            number_column("occurrences", text.mods_col_occurrences, 95.),
            number_column("p25", text.mods_col_p25, 65.),
            number_column("p50", text.mods_col_p50, 65.),
            number_column("p75", text.mods_col_p75, 65.),
        ],
        rows: Vec::new(),
        empty: text.mods_empty.into(),
    }
}

/// 三个下拉筛过、按占比从高到低排好的那些行。
///
/// 筛选在内存里做:整轮统计一共一千多行,每换一次下拉回去打一次库不值当,
/// 而且"这一页显示的和那三个下拉说的是同一件事"在同一个函数里看得见。
///
/// 稀有度那一档的"全部"**不含暗金**:暗金的词缀是固定的,一件
/// Wake of Destruction 上写着什么,全联赛每一件都写着一模一样的东西,
/// 所以它的"携带比例"量的是"多少人穿这件暗金",和稀有装的随机词缀不是
/// 一个量纲,混在一张表里排序只会把真正有用的行挤下去。想看就明选"暗金"。
#[must_use]
pub fn filtered<'a>(
    mods: &'a [SlotModStat],
    slot: &str,
    rarity: &str,
    kind: &str,
    show_all: bool,
) -> Vec<&'a SlotModStat> {
    let floor = if show_all { 0.0 } else { MIN_SHARE_PERCENT };
    let mut rows: Vec<&SlotModStat> = mods
        .iter()
        .filter(|stat| slot.is_empty() || stat.slot == slot)
        .filter(|stat| {
            if rarity.is_empty() {
                stat.rarity != UNIQUE_RARITY
            } else {
                stat.rarity == rarity
            }
        })
        .filter(|stat| kind.is_empty() || stat.mod_kind == kind)
        .filter(|stat| mod_share_percent(stat) >= floor)
        .collect();
    // 携带比例从高到低。比例一样时人数多的在前,再一样就按 stat id 定序 ——
    // 每次打开顺序都一样,不然"上次那一行"就找不回来了。
    rows.sort_by(|left, right| {
        mod_share_percent(right)
            .total_cmp(&mod_share_percent(left))
            .then(right.characters.cmp(&left.characters))
            .then(left.stat_id.cmp(&right.stat_id))
    });
    rows
}

/// 筛掉了几行。
#[must_use]
pub fn hidden_count(mods: &[SlotModStat], slot: &str, rarity: &str, kind: &str) -> usize {
    filtered(mods, slot, rarity, kind, true).len() - filtered(mods, slot, rarity, kind, false).len()
}

/// 这一批行是从多少个角色身上算出来的。
///
/// 取最大值而不是求和:同一个 (部位, 稀有度) 下每一行的分母都是同一个数
/// (穿了这个部位的角色数),加起来就成了它的几十倍。
#[must_use]
pub fn sample_size(rows: &[&SlotModStat]) -> u32 {
    rows.iter().map(|stat| stat.sample_size).max().unwrap_or(0)
}

/// 列 + 真行。
pub fn table_content_for(
    mods: &[SlotModStat],
    slot: &str,
    rarity: &str,
    kind: &str,
    show_all: bool,
    text: &'static Text,
) -> TableContent {
    TableContent {
        rows: mod_rows(&filtered(mods, slot, rarity, kind, show_all), text),
        ..table_content(text)
    }
}

/// 每个 stat id 一行。
pub fn mod_rows(rows: &[&SlotModStat], text: &'static Text) -> Vec<Vec<Cell>> {
    rows.iter()
        .map(|stat| {
            vec![
                Cell::plain(stat.stat_id.clone()),
                if stat.mod_family.is_empty() {
                    Cell::muted(text.common_none)
                } else {
                    Cell::muted(stat.mod_family.clone())
                },
                Cell::data(percent_text(mod_share_percent(stat), text)),
                Cell::data(stat.characters.to_string()),
                Cell::data(stat.occurrences.to_string()),
                percentile_cell(stat.p25, text),
                percentile_cell(stat.p50, text),
                percentile_cell(stat.p75, text),
            ]
        })
        .collect()
}

/// 分位数那一格。布尔词缀("不会被冰冻")没有数值,那三格就是"—"。
fn percentile_cell(value: Option<f64>, text: &'static Text) -> Cell {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return Cell::muted(text.common_none);
    };
    // 整数就写整数:胸甲生命写 176,不写 176.0;抗性 34.5 那种才留一位。
    if (value - value.round()).abs() < 0.05 {
        Cell::data(format!("{value:.0}"))
    } else {
        Cell::data(format!("{value:.1}"))
    }
}

impl AppShell {
    pub(crate) fn render_ninja_mods(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .child(page_heading(text.mods_heading, text.mods_subtitle))
            .child(self.mods_controls(cx))
            .child(
                panel()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(table(&self.mods_table)),
            )
            .child(self.mods_footer(cx))
    }

    fn mods_controls(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let show_all = self.mods_show_all;
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
            .child(picker(text.mods_kind_label, &self.mods_kind_select, 150.))
            .child(div().flex_grow())
            .child(
                Switch::new("mods-show-all")
                    .checked(show_all)
                    .label(SharedString::from(text.common_show_all))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.mods_show_all = *checked;
                        this.mods_dirty = true;
                        cx.notify();
                    })),
            )
    }

    /// 脚注:这张表是从多少个角色身上算出来的,以及藏了几行。
    ///
    /// 样本量必须常驻:47 个人身上量出来的 p50 和 2,000 个人量出来的 p50
    /// 在屏幕上长得一模一样,但只有后者能拿来做决定。
    fn mods_footer(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let (slot, rarity, kind) = self.mods_filters(cx);
        let rows = filtered(&self.ninja.mods, &slot, &rarity, &kind, true);
        let sample = sample_size(&rows);
        let hidden = if self.mods_show_all {
            0
        } else {
            hidden_count(&self.ninja.mods, &slot, &rarity, &kind)
        };
        let sampled = self.ninja.characters_done.to_string();
        // "穿了这个部位的有 47 个人"只有在真的选了一个部位时才成立;
        // 筛选器停在"全部"时那个数字是各部位里最大的一个,写成"这个部位"
        // 就成了一句假话。
        let line = if slot.is_empty() {
            i18n::fill(text.mods_sample_any, &[&sampled])
        } else {
            i18n::fill(
                text.mods_sample_line,
                &[&sample.to_string(), &slot, &sampled],
            )
        };
        div()
            .flex_none()
            .h_flex()
            .items_center()
            .gap(px(16.))
            .child(hint(line))
            .children((hidden > 0).then(|| {
                hint(i18n::fill(
                    text.common_hidden_rows,
                    &[&hidden.to_string(), &percent_text(MIN_SHARE_PERCENT, text)],
                ))
            }))
    }
}

#[cfg(test)]
mod ninja_mods_tests {
    use gpui_component::select::SelectItem as _;

    use super::*;
    use crate::i18n;

    fn stat(
        slot: &str,
        rarity: &str,
        kind: &str,
        stat_id: &str,
        characters: u32,
        sample: u32,
    ) -> SlotModStat {
        SlotModStat {
            slot: slot.to_owned(),
            rarity: rarity.to_owned(),
            mod_kind: kind.to_owned(),
            stat_id: stat_id.to_owned(),
            mod_family: "IncreasedLife".to_owned(),
            characters,
            occurrences: characters + 2,
            sample_size: sample,
            p25: Some(108.0),
            p50: Some(176.0),
            p75: Some(211.5),
        }
    }

    fn mods() -> Vec<SlotModStat> {
        vec![
            stat(
                "BodyArmour",
                "Rare",
                "explicit",
                "base_maximum_life",
                13,
                47,
            ),
            stat(
                "BodyArmour",
                "Rare",
                "explicit",
                "base_cold_damage_resistance_%",
                20,
                47,
            ),
            stat(
                "BodyArmour",
                "Rare",
                "desecrated",
                "local_energy_shield",
                14,
                47,
            ),
            stat(
                "BodyArmour",
                "Unique",
                "explicit",
                "cannot_be_frozen",
                9,
                26,
            ),
            stat("Ring", "Rare", "explicit", "base_maximum_life", 40, 60),
            // 一个人都没带:占比 0%,默认要被 2% 的阈值挡掉。
            stat(
                "BodyArmour",
                "Rare",
                "explicit",
                "base_movement_velocity_+%",
                0,
                47,
            ),
        ]
    }

    /// 三个下拉是"和"的关系,而且筛完按携带比例从高到低排。
    #[test]
    fn the_three_filters_narrow_down_and_the_rows_come_out_sorted() {
        let mods = mods();
        // 六行里那一行暗金不进"全部"(见 `the_all_bucket_leaves_unique_mods_out`)。
        let all = filtered(&mods, "", "", "", true);
        assert_eq!(all.len(), 5);
        // Ring 的 40/60 = 66.7% 排最前。
        assert_eq!(all[0].slot, "Ring");

        // 明选暗金那一档,排序照样按比例来:9/26 = 34.6% 要排在
        // 14/47 = 29.8% 前面,哪怕后者人还多五个。
        let body = filtered(&mods, "BodyArmour", "", "", true);
        assert_eq!(body.len(), 4);
        assert!(body.iter().all(|stat| stat.slot == "BodyArmour"));
        assert_eq!(body[0].stat_id, "base_cold_damage_resistance_%");
        assert_eq!(body[1].stat_id, "local_energy_shield");

        let rare_explicit = filtered(&mods, "BodyArmour", "Rare", "explicit", true);
        assert_eq!(rare_explicit.len(), 3);
        assert!(
            rare_explicit
                .iter()
                .all(|stat| stat.rarity == "Rare" && stat.mod_kind == "explicit")
        );
    }

    /// 默认藏掉占比不足 2% 的行,开关一开就全放出来。
    #[test]
    fn the_rare_modifiers_stay_hidden_until_you_ask_for_them() {
        let mods = mods();
        assert_eq!(hidden_count(&mods, "", "", ""), 1);
        let shown = filtered(&mods, "BodyArmour", "Rare", "explicit", false);
        assert_eq!(shown.len(), 2, "0/47 的那条被 2% 挡掉");
        assert!(
            shown
                .iter()
                .all(|stat| stat.stat_id != "base_movement_velocity_+%")
        );
    }

    /// 一行要同时说清"多少人带着它"和"带的人身上是什么数值"。
    #[test]
    fn a_row_carries_the_share_the_counts_and_the_percentiles() {
        let mods = mods();
        let rows = filtered(&mods, "BodyArmour", "Rare", "explicit", false);
        let built = mod_rows(&rows, &i18n::ENGLISH);
        assert_eq!(built.len(), 2);
        let life = built
            .iter()
            .find(|row| row[0].text() == "base_maximum_life")
            .expect("life row");
        assert_eq!(life[1].text(), "IncreasedLife");
        // 13 / 47 = 27.7%
        assert_eq!(life[2].text(), "27.7%");
        assert_eq!(life[3].text(), "13");
        assert_eq!(life[4].text(), "15");
        assert_eq!(life[5].text(), "108");
        assert_eq!(life[6].text(), "176", "整数就写整数");
        assert_eq!(life[7].text(), "211.5");
    }

    /// 没有数值的词缀(布尔的那种)三格写"—",不写 0。
    #[test]
    fn a_modifier_without_numbers_shows_dashes() {
        let mut boolean = stat("Boots", "Unique", "explicit", "cannot_be_frozen", 9, 26);
        boolean.p25 = None;
        boolean.p50 = None;
        boolean.p75 = None;
        boolean.mod_family = String::new();
        let built = mod_rows(&[&boolean], &i18n::ENGLISH);
        assert_eq!(built[0][1].text(), "—", "族名为空时也不留白");
        assert_eq!(built[0][5].text(), "—");
        assert_eq!(built[0][6].text(), "—");
        assert_eq!(built[0][7].text(), "—");
    }

    /// 分母取最大值:同一个部位下每一行的分母都是同一个数,求和会放大几十倍。
    #[test]
    fn the_sample_size_is_the_slot_denominator_not_a_sum() {
        let mods = mods();
        let rows = filtered(&mods, "BodyArmour", "Rare", "", true);
        assert_eq!(sample_size(&rows), 47);
        assert_eq!(sample_size(&[]), 0);
    }

    /// 下拉的选项从库里长出来:写死一份的话,ninja 多一个部位就永远筛不到它。
    #[test]
    fn the_pickers_grow_out_of_the_stored_stats() {
        let mods = mods();
        let slots = slot_choices(&mods, &i18n::ENGLISH);
        assert_eq!(
            slots
                .iter()
                .map(|choice| choice.value().to_string())
                .collect::<Vec<_>>(),
            vec!["", "BodyArmour", "Ring"]
        );
        assert_eq!(slots[0].title().to_string(), "All");

        let rarities = rarity_choices(&mods, &i18n::ENGLISH);
        assert_eq!(rarities[1].value().to_string(), "Rare");
        assert_eq!(rarities[1].title().to_string(), "Rare");
        assert_eq!(rarities[2].value().to_string(), "Unique");

        let kinds = kind_choices(&mods, &i18n::ENGLISH);
        assert_eq!(kinds[1].value().to_string(), "desecrated");
        assert_eq!(kinds[1].title().to_string(), "Desecrated");

        // 库还是空的时候只剩"全部",而不是一个空下拉。
        assert_eq!(slot_choices(&[], &i18n::ENGLISH).len(), 1);
    }

    /// "全部"那一档要排掉暗金。
    ///
    /// 暗金的词缀是**固定**的:一件 Wake of Destruction 上写着什么,
    /// 全联赛每一件都写着一模一样的东西。把它和稀有装的随机词缀混在一张表里,
    /// "多少人带着这条词缀"就变成了"多少人穿着这件暗金"——那是暗金热度页的
    /// 问题,不是词缀热度页的。想看暗金的词缀,下拉里明选"暗金"。
    #[test]
    fn the_all_bucket_leaves_unique_mods_out() {
        let mods = mods();
        let all = filtered(&mods, "", "", "", true);
        assert!(
            all.iter().all(|stat| stat.rarity != UNIQUE_RARITY),
            "全部那一档里不该有暗金的固定词缀"
        );
        assert_eq!(all.len(), 5, "六行里那一行暗金被排掉了");

        // 明选"暗金"就照样看得到,一行都不少。
        let unique = filtered(&mods, "", UNIQUE_RARITY, "", true);
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].stat_id, "cannot_be_frozen");

        // 部位那一档也跟着走:BodyArmour 的五行里有一行是暗金。
        assert_eq!(filtered(&mods, "BodyArmour", "", "", true).len(), 4);
    }

    /// 稀有度下拉默认停在"稀有",所以**这一档必须永远在选项里** ——
    /// 哪怕这一轮的统计还是空的(第一次开程序就是这样),
    /// 选项不在的话下拉一打开是个空的占位符。
    #[test]
    fn the_rarity_picker_always_offers_the_default() {
        let values: Vec<String> = rarity_choices(&[], &i18n::ENGLISH)
            .iter()
            .map(|choice| choice.value().to_string())
            .collect();
        assert_eq!(values, vec!["", DEFAULT_RARITY]);

        // 库里有别的档次时它们照样都在,不重复。
        let values: Vec<String> = rarity_choices(&mods(), &i18n::ENGLISH)
            .iter()
            .map(|choice| choice.value().to_string())
            .collect();
        assert_eq!(values, vec!["", "Rare", "Unique"]);
    }

    /// 认不出来的稀有度/类型原样显示,而不是整档消失。
    #[test]
    fn unknown_buckets_keep_their_raw_name() {
        let text = &i18n::ENGLISH;
        assert_eq!(rarity_label("Relic", text), "Relic");
        assert_eq!(kind_label("enchant", text), "enchant");
        assert_eq!(rarity_label("Normal", text), "Normal");
        assert_eq!(rarity_label("Unique", &i18n::SIMPLIFIED_CHINESE), "暗金");
    }
}
