//! 暗金热度页:热门 BD 在穿什么,拼上经济接口的参考价和挂单数。
//!
//! 人数来自 poe.ninja builds 搜索的 `items` 分面(全联赛一次请求就有,
//! 不靠角色采样),价格来自它文档化的经济接口。两边拼在一张表上,才回答得了
//! 真正的问题:"大家都在用的这件东西,现在多少钱、有几个人在卖"。
//!
//! 分区筛选器就是采样跑过的那一串查询(全联赛 / 某个职业 / 某个技能 /
//! 穿着某件暗金 / 职业+技能)。换一个分区,占比的**分母也跟着换** ——
//! "Deadeye 里有 41% 的人穿它"和"全联赛 11%"是两句不同的话。

use gpui::{Context, ParentElement, SharedString, Styled, div, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::switch::Switch;
use gpui_component::{Disableable as _, Sizable as _, Size, StyledExt as _};

use pnd_domain::CurrencyRates;
use pnd_runtime::now_secs;

use super::watches::milli_text;
use super::{Cell, TableContent, Tone, column, number_column};
use crate::i18n::{self, Text};
use crate::shell::link::ago_text;
use crate::shell::ninja::{NinjaData, UniqueRow, partition_label, percent_text};
use crate::shell::{AppShell, Choice, hint, page_heading, panel, picker, table};
use crate::theme::*;

/// 占比低于这个数的行默认收起来。
///
/// 0.5% 不是洁癖:全联赛的 `items` 分面有四百多条,其中三百多条是"某个人
/// 捡到一件就穿上了"。全铺出来,真正的热门装备反而要往下翻半天。
pub const MIN_SHARE_PERCENT: f64 = 0.5;

/// 分区下拉的选项。库里还什么都没有时留一条"全联赛",免得下拉是空的。
pub fn partition_choices(keys: &[String], text: &'static Text) -> Vec<Choice> {
    if keys.is_empty() {
        return vec![Choice::new("", partition_label("", text))];
    }
    keys.iter()
        .map(|key| Choice::new(key.clone(), partition_label(key, text)))
        .collect()
}

/// 暗金榜的列。行由 [`table_content_for`] 填。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            number_column("rank", text.uniques_col_rank, 44.),
            column("name", text.uniques_col_name, 230.),
            number_column("users", text.uniques_col_characters, 85.),
            number_column("share", text.uniques_col_share, 70.),
            number_column("price", text.uniques_col_reference_price, 170.),
            number_column("listings", text.uniques_col_listings, 80.),
            number_column("seven_day", text.uniques_col_seven_day, 70.),
        ],
        rows: Vec::new(),
        empty: text.uniques_empty.into(),
    }
}

/// 列 + 真行。
pub fn table_content_for(
    rows: &[UniqueRow],
    rates: &CurrencyRates,
    show_all: bool,
    text: &'static Text,
) -> TableContent {
    TableContent {
        rows: unique_rows(rows, rates, show_all, text),
        ..table_content(text)
    }
}

/// 每件暗金一行。人数已经由库排好序(多的在前),这里只管挑和画。
///
/// **名次按没筛之前算**:收起冷门行之后第 12 名还是第 12 名,不会因为
/// 前面藏了几条就变成第 9 名。
pub fn unique_rows(
    rows: &[UniqueRow],
    rates: &CurrencyRates,
    show_all: bool,
    text: &'static Text,
) -> Vec<Vec<Cell>> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| show_all || row.share_percent >= MIN_SHARE_PERCENT)
        .map(|(index, row)| {
            vec![
                Cell::muted((index + 1).to_string()),
                if index == 0 {
                    Cell::accent(row.name.clone())
                } else {
                    Cell::plain(row.name.clone())
                },
                Cell::data(row.users.to_string()),
                Cell::data(percent_text(row.share_percent, text)),
                match price_text(row, rates, text) {
                    Some(price) => Cell::data(price),
                    None => Cell::muted(text.common_none),
                },
                match row.listings {
                    Some(listings) => Cell::data(listings.to_string()),
                    None => Cell::muted(text.common_none),
                },
                change_cell(row.change_percent, text),
            ]
        })
        .collect()
}

/// 收起来了几行。写在筛选器那一条上 —— 藏东西必须说出来,否则"表里没有它"
/// 和"它被藏起来了"看着一模一样。
#[must_use]
pub fn hidden_count(rows: &[UniqueRow], show_all: bool) -> usize {
    if show_all {
        return 0;
    }
    rows.iter()
        .filter(|row| row.share_percent < MIN_SHARE_PERCENT)
        .count()
}

/// 参考价那一格:"29.9 ex ≈ 0.36 div"。汇率还没读到就只写 exalted,
/// 不猜一个换算。
fn price_text(row: &UniqueRow, rates: &CurrencyRates, text: &'static Text) -> Option<String> {
    let milli = row.price_milli?;
    let exalted = milli_text(milli);
    match divine_text(milli, rates) {
        Some(divine) => Some(i18n::fill(
            text.uniques_price_with_divine,
            &[&exalted, &divine],
        )),
        None => Some(i18n::fill(text.uniques_price_exalted, &[&exalted])),
    }
}

/// exalted 千分整数 → divine 的小数写法。
///
/// 两个数的单位相同(都是"千分之一个"),所以直接相除就是"几个 divine"。
fn divine_text(price_milli: i64, rates: &CurrencyRates) -> Option<String> {
    let rate_milli = rates.exalted_per_divine_milli?;
    if rate_milli <= 0 {
        return None;
    }
    let divine = price_milli as f64 / rate_milli as f64;
    if !divine.is_finite() {
        return None;
    }
    Some(format!("{divine:.2}"))
}

/// 7 天涨跌。涨了标绿、跌了标琥珀 —— 这一列是给"现在该不该买"用的。
fn change_cell(change: Option<f64>, text: &'static Text) -> Cell {
    let Some(change) = change.filter(|value| value.is_finite()) else {
        return Cell::muted(text.common_none);
    };
    let body = format!("{change:+.0}{}", text.common_percent);
    if change > 0.0 {
        Cell::new(body, Tone::Good)
    } else if change < 0.0 {
        Cell::new(body, Tone::Warn)
    } else {
        Cell::muted(body)
    }
}

/// 抬头那一行:哪个联赛、哪一版快照、采了多少人、这份缓存多旧。
///
/// 四件事挤在一行,是因为看榜的人第一个问题永远是"这数是什么时候的"——
/// 一张不知道多旧的热度榜没法拿来做决定。
#[must_use]
pub fn header_line(league: &str, data: &NinjaData, text: &'static Text, now: i64) -> String {
    let mut parts = vec![league.to_owned()];
    match &data.snapshot {
        Some(row) => {
            parts.push(i18n::fill(text.uniques_snapshot, &[&row.version]));
            parts.push(i18n::fill(
                text.uniques_sampled,
                &[&data.characters_done.to_string()],
            ));
            // 整轮跑完的按跑完时刻算,跑到一半的只有开跑时刻可用。
            let at = row.finished_at.unwrap_or(row.started_at);
            parts.push(ago_text(now - at, text));
        }
        None => parts.push(text.uniques_no_snapshot.to_owned()),
    }
    parts.join(" · ")
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
            .child(self.uniques_controls(cx))
            .child(
                panel()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(table(&self.uniques_table)),
            )
            .child(self.uniques_footer())
    }

    /// 抬头 + 筛选器。刷新按钮和"这份数据多旧"必须挨着:看见旧了,
    /// 下一个动作就在手边。
    fn uniques_controls(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let busy = self.sampler_busy;
        let show_all = self.uniques_show_all;
        let hidden = hidden_count(&self.ninja.uniques, show_all);
        let header = header_line(&self.settings.league, &self.ninja, text, now_secs());
        panel()
            .flex_none()
            .gap(px(8.))
            .px(px(10.))
            .py(px(8.))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(FONT_MONO)
                            .text_size(fs(FS_11))
                            .text_color(c(TEXT_DATA))
                            .child(SharedString::from(header)),
                    )
                    .child(
                        Button::new("uniques-refresh")
                            .primary()
                            .label(text.uniques_refresh)
                            .with_size(Size::Small)
                            // 一个库上同时跑两条采样线程只会互相等锁。
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_ninja_sampling(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(picker(
                        text.uniques_partition_label,
                        &self.uniques_partition_select,
                        320.,
                    ))
                    .child(
                        Switch::new("uniques-show-all")
                            .checked(show_all)
                            .label(SharedString::from(text.common_show_all))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.uniques_show_all = *checked;
                                this.uniques_dirty = true;
                                cx.notify();
                            })),
                    )
                    .children((hidden > 0).then(|| {
                        hint(i18n::fill(
                            text.common_hidden_rows,
                            &[&hidden.to_string(), &percent_text(MIN_SHARE_PERCENT, text)],
                        ))
                    })),
            )
    }

    /// 脚注:采样跑到哪了,以及参考价是什么时候抓的。
    fn uniques_footer(&self) -> gpui::Div {
        let text = self.text();
        let price_age = self
            .ninja
            .prices_fetched_at
            .map(|at| i18n::fill(text.uniques_price_age, &[&ago_text(now_secs() - at, text)]));
        div()
            .flex_none()
            .h_flex()
            .items_center()
            .gap(px(16.))
            .children((!self.sampler_line.is_empty()).then(|| hint(self.sampler_line.clone())))
            .children(price_age.map(hint))
    }
}

#[cfg(test)]
mod ninja_uniques_tests {
    use gpui_component::select::SelectItem as _;
    use pnd_storage::{SnapshotRow, SnapshotStage};

    use super::*;
    use crate::i18n;

    fn rates() -> CurrencyRates {
        CurrencyRates {
            chaos_per_divine_milli: Some(25_210),
            exalted_per_divine_milli: Some(83_420),
            mirror_per_divine_milli: None,
        }
    }

    fn rows() -> Vec<UniqueRow> {
        vec![
            UniqueRow {
                name: "Wake of Destruction".to_owned(),
                users: 7_464,
                share_percent: 11.42,
                price_milli: Some(29_900),
                listings: Some(131),
                change_percent: Some(6.0),
            },
            UniqueRow {
                name: "Beira's Anguish".to_owned(),
                users: 7_220,
                share_percent: 11.04,
                price_milli: Some(2_000),
                listings: Some(44),
                change_percent: Some(-9.0),
            },
            UniqueRow {
                name: "Breath of the Mountains".to_owned(),
                users: 4_213,
                share_percent: 6.44,
                price_milli: None,
                listings: None,
                change_percent: None,
            },
            UniqueRow {
                name: "Somebody's Trinket".to_owned(),
                users: 120,
                share_percent: 0.18,
                price_milli: Some(1_000),
                listings: Some(3),
                change_percent: Some(0.0),
            },
        ]
    }

    /// 榜上一行要同时回答"多少人在用"和"现在多少钱"。
    #[test]
    fn a_row_carries_both_the_usage_and_the_price() {
        let built = unique_rows(&rows(), &rates(), false, &i18n::ENGLISH);
        assert_eq!(built.len(), 3, "占比 0.18% 的那条默认收起来");
        assert_eq!(built[0][0].text(), "1");
        assert_eq!(built[0][1].text(), "Wake of Destruction");
        assert_eq!(built[0][1].tone(), Tone::Accent, "榜首标出来");
        assert_eq!(built[0][2].text(), "7464");
        assert_eq!(built[0][3].text(), "11.4%");
        // 29.9 exalted,按 1 divine = 83.42 exalted 换算。
        assert_eq!(built[0][4].text(), "29.9 ex ≈ 0.36 div");
        assert_eq!(built[0][5].text(), "131");
        assert_eq!(built[0][6].text(), "+6%");
        assert_eq!(built[0][6].tone(), Tone::Good);
        assert_eq!(built[1][6].text(), "-9%");
        assert_eq!(built[1][6].tone(), Tone::Warn);
    }

    /// 经济接口里没有的那件东西,三格都写"—",不写 0 —— 0 会被读成"不值钱"。
    #[test]
    fn a_unique_without_a_listing_says_so_instead_of_showing_zero() {
        let built = unique_rows(&rows(), &rates(), false, &i18n::ENGLISH);
        assert_eq!(built[2][1].text(), "Breath of the Mountains");
        assert_eq!(built[2][4].text(), "—");
        assert_eq!(built[2][5].text(), "—");
        assert_eq!(built[2][6].text(), "—");
    }

    /// 汇率还没读到就只写 exalted,不猜一个 divine 数。
    #[test]
    fn without_rates_the_price_stays_in_exalted() {
        let built = unique_rows(&rows(), &CurrencyRates::none(), false, &i18n::ENGLISH);
        assert_eq!(built[0][4].text(), "29.9 ex");
    }

    /// 打开"显示全部"就把冷门行放出来,而且名次不变 —— 名次是按没筛之前算的。
    #[test]
    fn showing_everything_keeps_the_ranks_stable() {
        let rows = rows();
        assert_eq!(hidden_count(&rows, false), 1);
        assert_eq!(hidden_count(&rows, true), 0);

        let all = unique_rows(&rows, &rates(), true, &i18n::ENGLISH);
        assert_eq!(all.len(), 4);
        assert_eq!(all[3][0].text(), "4");
        assert_eq!(all[3][1].text(), "Somebody's Trinket");
        assert_eq!(all[3][6].text(), "+0%", "不涨不跌也是一个数");
        assert_eq!(all[3][6].tone(), Tone::Muted);
    }

    /// 抬头必须说清这份数据是什么时候的 —— 不知道多旧的热度榜没法用。
    #[test]
    fn the_header_says_which_snapshot_and_how_old_it_is() {
        let mut data = NinjaData::empty("forbiddenrites".to_owned());
        data.characters_done = 74;
        data.snapshot = Some(SnapshotRow {
            league_url: "forbiddenrites".to_owned(),
            version: "1733-20260906-24495".to_owned(),
            snapshot_name: "forbidden-rites".to_owned(),
            total_characters: 65_371,
            stage: SnapshotStage::Aggregated,
            started_at: 1_000,
            finished_at: Some(2_000),
        });
        let line = header_line("Forbidden Rites", &data, &i18n::ENGLISH, 2_000 + 3 * 3_600);
        assert_eq!(
            line,
            "Forbidden Rites · snapshot 1733-20260906-24495 · 74 characters sampled · 3 h ago"
        );

        // 一次都没采过的时候也得说话,而不是留一串空的分隔点。
        let empty = NinjaData::empty("forbiddenrites".to_owned());
        let line = header_line("Forbidden Rites", &empty, &i18n::ENGLISH, 5_000);
        assert_eq!(line, "Forbidden Rites · no snapshot yet");
    }

    /// 下拉里的值必须是库里那个分区键(界面拿它回去查),显示的却是人话。
    #[test]
    fn the_partition_picker_shows_names_but_carries_keys() {
        let keys = vec![
            String::new(),
            "class=Deadeye".to_owned(),
            "items=Wake of Destruction".to_owned(),
        ];
        let choices = partition_choices(&keys, &i18n::ENGLISH);
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[1].value().to_string(), "class=Deadeye");
        assert_eq!(choices[1].title().to_string(), "Class Deadeye");

        // 一条分区都没有时也留一项,免得下拉是空的。
        let none = partition_choices(&[], &i18n::ENGLISH);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].value().to_string(), "");
    }
}
