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
use gpui_component::{Disableable as _, Selectable as _, Sizable as _, Size, StyledExt as _};

use pnd_domain::{Currency, CurrencyRates, Game};
use pnd_runtime::now_secs;

use super::watches::milli_text;
use super::{Cell, TableContent, Tone, column, number_column};
use crate::i18n::{self, Text};
use crate::shell::link::ago_text;
use crate::shell::ninja::{NinjaData, UniqueRow, game_league_text, partition_label, percent_text};
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

/// 参考价那一格:"0.085 div ≈ 8.2 ex"。
///
/// 单位是**这一行自己带回来的**(经济接口那份 overview 报的 `core.primary`),
/// 不是写死的 "ex"。写死的那一版在 2026-09-07 基准币换成 divine 那天变成了
/// 近百倍的错:`0.085 div`(≈ 8 个 exalted)被印成 `0.085 ex ≈ 0.00 div`,
/// 看着像不值钱;`1200 div` 被印成 `1200 ex ≈ 12.10 div`。
///
/// 汇率还没读到、或者基准币是我们不认得的那种,就只写数字和它的单位 ——
/// 编一个换算率只会把"我不知道"说成一个具体的数。
fn price_text(row: &UniqueRow, rates: &CurrencyRates, text: &'static Text) -> Option<String> {
    let milli = row.price_milli?;
    let amount = milli_text(milli);
    // 有价格却没单位,只可能是这一列还不存在时留下的老行:宁可少说一个单位,
    // 也不替它猜一个。
    let Some(currency) = row.price_currency.as_ref() else {
        return Some(amount);
    };
    let unit = currency_label(currency, text);
    match converted(milli, currency, rates) {
        Some((value, into)) => Some(i18n::fill(
            text.uniques_price_approx,
            &[
                &amount,
                unit,
                &approx_text(value),
                currency_label(&into, text),
            ],
        )),
        None => Some(i18n::fill(text.uniques_price_plain, &[&amount, unit])),
    }
}

/// 换到哪个币、换出来多少。
///
/// divine 计价的往 exalted 换,别的往 divine 换 —— 一格里两个数的用处是
/// "另一个我熟的单位大概是多少",所以对手币永远是那个不熟的那边。
/// mirror 和认不出来的币返回 `None`:物品榜从来没拿它们计过价,真轮到了
/// 也该照写原文,而不是拿一个没验证过的方向去算。
fn converted(
    price_milli: i64,
    currency: &Currency,
    rates: &CurrencyRates,
) -> Option<(f64, Currency)> {
    let amount = price_milli as f64 / 1000.0;
    let (value, into) = match currency {
        Currency::Divine => (
            amount * rate(rates.exalted_per_divine_milli)?,
            Currency::Exalted,
        ),
        Currency::Exalted => (
            amount / rate(rates.exalted_per_divine_milli)?,
            Currency::Divine,
        ),
        Currency::Chaos => (
            amount / rate(rates.chaos_per_divine_milli)?,
            Currency::Divine,
        ),
        Currency::Mirror | Currency::Other(_) => return None,
    };
    value.is_finite().then_some((value, into))
}

/// 千分整数的汇率 → 浮点。缺席或者不是正数都当"这一轮没读到汇率"。
fn rate(rate_milli: Option<i64>) -> Option<f64> {
    rate_milli
        .filter(|value| *value > 0)
        .map(|value| value as f64 / 1000.0)
}

/// 换算出来那个数写几位小数。
///
/// 位数是"这一位还有意义吗"的问题:0.36 divine 里第二位是三成的差别,
/// 2887 exalted 里第一位小数连一个 chaos 都不到。
fn approx_text(value: f64) -> String {
    let size = value.abs();
    if size < 1.0 {
        format!("{value:.2}")
    } else if size < 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

/// 参考价那一列的单位写法。认不出来的币照写接口给的代号 —— 一个看得懂的
/// 怪词好过一个编出来的单位。
fn currency_label<'a>(currency: &'a Currency, text: &'static Text) -> &'a str {
    match currency {
        Currency::Divine => text.common_currency_divine,
        Currency::Exalted => text.common_currency_exalted,
        Currency::Chaos => text.common_currency_chaos,
        other => other.code(),
    }
}

/// 7 天涨跌。涨了标绿、跌了标琥珀 —— 这一列是给"现在该不该买"用的。
///
/// 留一位小数,不是四舍五入到整数。整数版把 `-99.53%` 印成 `-100%`
/// ("这东西归零了"),也把 `+0.4%` 印成 `+0%` —— 两句都不是原话。
fn change_cell(change: Option<f64>, text: &'static Text) -> Cell {
    let Some(change) = change.filter(|value| value.is_finite()) else {
        return Cell::muted(text.common_none);
    };
    let body = format!("{change:+.1}{}", text.common_percent);
    if change > 0.0 {
        Cell::new(body, Tone::Good)
    } else if change < 0.0 {
        Cell::new(body, Tone::Warn)
    } else {
        Cell::muted(body)
    }
}

/// 抬头那一行:哪一代、哪个联赛、哪一版快照、采了多少人、这份缓存多旧。
///
/// 五件事挤在一行,是因为看榜的人第一个问题永远是"这数是什么时候的"——
/// 一张不知道多旧的热度榜没法拿来做决定。代号排在最前面:这一页顶上有个
/// 游戏开关,按一下整张表就换了内容,抬头不跟着换就是一句假话。
#[must_use]
pub fn header_line(
    game: Game,
    league: &str,
    data: &NinjaData,
    text: &'static Text,
    now: i64,
) -> String {
    let mut parts = vec![game_league_text(game, league, text)];
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

/// 两页共用的游戏开关:PoE2 / PoE1 两个按钮,选中的那个高亮。
///
/// 做成两个按钮而不是下拉,是因为只有两个选项:下拉要点两下才换得了,
/// 而且不点开就看不见另一个选项存在。
///
/// `prefix` 是元素 id 的前缀 —— 两页各有一套按钮,id 撞了的话框架会把它们
/// 当成同一个控件,点哪一页都只有一页有反应。
pub fn game_switch(
    prefix: &'static str,
    selected: Game,
    text: &'static Text,
    cx: &mut Context<AppShell>,
) -> gpui::Div {
    div()
        .h_flex()
        .items_center()
        .gap(px(2.))
        .children([Game::Poe2, Game::Poe1].into_iter().map(|game| {
            let id = match game {
                Game::Poe1 => "-game-poe1",
                Game::Poe2 => "-game-poe2",
            };
            Button::new(SharedString::from(format!("{prefix}{id}")))
                .ghost()
                .with_size(Size::Small)
                .selected(game == selected)
                .label(crate::shell::ninja::game_word(game, text))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.switch_ninja_game(game, cx);
                }))
        }))
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
        let game = self.ninja_game();
        let header = header_line(
            game,
            crate::shell::ninja::ninja_league_name(&self.settings, game),
            &self.ninja,
            text,
            now_secs(),
        );
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
                    .child(game_switch("uniques", game, text, cx))
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
                price_currency: Some(Currency::Divine),
                listings: Some(131),
                change_percent: Some(6.0),
            },
            UniqueRow {
                name: "Beira's Anguish".to_owned(),
                users: 7_220,
                share_percent: 11.04,
                price_milli: Some(2_000),
                price_currency: Some(Currency::Divine),
                listings: Some(44),
                change_percent: Some(-9.0),
            },
            UniqueRow {
                name: "Breath of the Mountains".to_owned(),
                users: 4_213,
                share_percent: 6.44,
                price_milli: None,
                price_currency: None,
                listings: None,
                change_percent: None,
            },
            UniqueRow {
                name: "Somebody's Trinket".to_owned(),
                users: 120,
                share_percent: 0.18,
                price_milli: Some(1_000),
                price_currency: Some(Currency::Divine),
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
        // 29.9 divine(接口今天的基准币),按 1 divine = 83.42 exalted 换算。
        assert_eq!(built[0][4].text(), "29.9 div ≈ 2494 ex");
        assert_eq!(built[0][5].text(), "131");
        assert_eq!(built[0][6].text(), "+6.0%");
        assert_eq!(built[0][6].tone(), Tone::Good);
        assert_eq!(built[1][6].text(), "-9.0%");
        assert_eq!(built[1][6].tone(), Tone::Warn);
    }

    /// 跌了 99.53% 就写 `-99.5%`,不是 `-100%`。
    ///
    /// 那个 `-100%` 是四舍五入印出来的,而 `-100%` 在人眼里是"归零了"——
    /// 完全不同的一句话。Trenchtimbre 那条榜上的怪数字就是这么来的。
    #[test]
    fn a_big_drop_keeps_its_decimal_instead_of_reading_as_zero() {
        let text = &i18n::ENGLISH;
        let cell = |change: Option<f64>| {
            let rows = vec![UniqueRow {
                name: "Trenchtimbre".to_owned(),
                users: 1_000,
                share_percent: 5.0,
                price_milli: Some(85),
                price_currency: Some(Currency::Divine),
                listings: Some(1_509),
                change_percent: change,
            }];
            unique_rows(&rows, &rates(), false, text)[0][6].clone()
        };
        assert_eq!(cell(Some(-99.53)).text(), "-99.5%");
        // 小到看不见的涨幅也是涨,别被抹成 +0%。
        assert_eq!(cell(Some(0.4)).text(), "+0.4%");
        assert_eq!(cell(Some(0.4)).tone(), Tone::Good);
        // 真的没有一周历史时是"—",不是一个编出来的 0。
        assert_eq!(cell(None).text(), "—");
        assert_eq!(cell(None).tone(), Tone::Muted);
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

    /// 汇率还没读到就只写它自己的单位,不猜一个换算值。
    #[test]
    fn without_rates_the_price_keeps_its_own_currency() {
        let built = unique_rows(&rows(), &CurrencyRates::none(), false, &i18n::ENGLISH);
        assert_eq!(built[0][4].text(), "29.9 div");
    }

    /// 和 fixture 同一天(2026-09-07)的汇率:1 divine = 96.56 exalted / 23.4 chaos。
    fn rates_0907() -> CurrencyRates {
        CurrencyRates {
            chaos_per_divine_milli: Some(23_400),
            exalted_per_divine_milli: Some(96_560),
            mirror_per_divine_milli: None,
        }
    }

    /// 一行只有价格有意义的暗金,专门喂给 `price_text`。
    fn priced(milli: i64, currency: Option<Currency>) -> UniqueRow {
        UniqueRow {
            name: "Skysliver".to_owned(),
            users: 1_000,
            share_percent: 5.0,
            price_milli: Some(milli),
            price_currency: currency,
            listings: Some(12),
            change_percent: None,
        }
    }

    /// 经济接口 2026-09-07 报的基准币是 divine:那就写 div,换算方向也反过来。
    ///
    /// 写死"ex"的那一版把 Skysliver 印成 `0.085 ex ≈ 0.00 div`("不值钱"),
    /// 把 Lavianga's Spirits 印成 `1200 ex ≈ 12.10 div` —— 真相是 1200 divine,
    /// 差了近百倍。
    #[test]
    fn a_divine_quoted_price_reads_in_divine() {
        let text = &i18n::ENGLISH;
        let say =
            |milli: i64| price_text(&priced(milli, Some(Currency::Divine)), &rates_0907(), text);
        assert_eq!(say(85).unwrap(), "0.085 div ≈ 8.2 ex");
        assert_eq!(say(29_900).unwrap(), "29.9 div ≈ 2887 ex");
        assert_eq!(say(1_200_000).unwrap(), "1200 div ≈ 115872 ex");
        // 汇率还没读到时只写 divine,不猜一个换算。
        assert_eq!(
            price_text(
                &priced(85, Some(Currency::Divine)),
                &CurrencyRates::none(),
                text
            )
            .unwrap(),
            "0.085 div"
        );
    }

    /// 基准币真是 exalted 的那一天(2026-09-06 的原文就是),写法不变。
    #[test]
    fn an_exalted_quoted_price_still_reads_in_exalted() {
        let text = &i18n::ENGLISH;
        assert_eq!(
            price_text(&priced(29_900, Some(Currency::Exalted)), &rates(), text).unwrap(),
            "29.9 ex ≈ 0.36 div"
        );
        assert_eq!(
            price_text(
                &priced(29_900, Some(Currency::Exalted)),
                &CurrencyRates::none(),
                text
            )
            .unwrap(),
            "29.9 ex"
        );
    }

    /// chaos 计价的那天(接口换过一次基准币,就还会再换)照样换算到 divine。
    #[test]
    fn a_chaos_quoted_price_converts_to_divine() {
        let text = &i18n::ENGLISH;
        assert_eq!(
            price_text(
                &priced(1_200_000, Some(Currency::Chaos)),
                &rates_0907(),
                text
            )
            .unwrap(),
            "1200 chaos ≈ 51.3 div"
        );
    }

    /// 认不出来的基准币:照写数字和它的代号,不换算。
    ///
    /// 编一个换算率只会把"我不知道"说成一个具体的数。
    #[test]
    fn an_unknown_base_currency_shows_the_number_and_its_code() {
        let text = &i18n::ENGLISH;
        assert_eq!(
            price_text(
                &priced(2_500, Some(Currency::Other("annul".to_owned()))),
                &rates_0907(),
                text
            )
            .unwrap(),
            "2.5 annul"
        );
        assert_eq!(
            price_text(&priced(1_000, Some(Currency::Mirror)), &rates_0907(), text).unwrap(),
            "1 mirror"
        );
        // 经济接口里压根没有这件东西:价格那一格什么都不写。
        let mut none = priced(0, Some(Currency::Divine));
        none.price_milli = None;
        assert_eq!(price_text(&none, &rates_0907(), text), None);
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
        assert_eq!(all[3][6].text(), "+0.0%", "有历史、真的没涨没跌,也是一个数");
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
        let line = header_line(
            Game::Poe2,
            "Forbidden Rites",
            &data,
            &i18n::ENGLISH,
            2_000 + 3 * 3_600,
        );
        assert_eq!(
            line,
            "PoE2 · Forbidden Rites · snapshot 1733-20260906-24495 · 74 characters sampled · 3 h ago"
        );

        // 一次都没采过的时候也得说话,而不是留一串空的分隔点。
        let empty = NinjaData::empty("forbiddenrites".to_owned());
        let line = header_line(Game::Poe2, "Forbidden Rites", &empty, &i18n::ENGLISH, 5_000);
        assert_eq!(line, "PoE2 · Forbidden Rites · no snapshot yet");
    }

    /// 切到 PoE1 时抬头要跟着换代、跟着换联赛。
    ///
    /// 这一行是两页上唯一说得清"表里这些数字属于谁"的地方:开关按下去之后
    /// 表格整份换了内容,抬头还写着 PoE2 的联赛,那就是一句假话。
    #[test]
    fn the_header_follows_the_game_switch() {
        let empty = NinjaData::empty("allflame".to_owned());
        assert_eq!(
            header_line(Game::Poe1, "Allflame", &empty, &i18n::ENGLISH, 5_000),
            "PoE1 · Allflame · no snapshot yet"
        );
        // PoE1 的联赛设置留空("用当季挑战联赛")、又一次都没采过:
        // 只写代号,不留一个空荡荡的分隔点。
        assert_eq!(
            header_line(Game::Poe1, "", &NinjaData::default(), &i18n::ENGLISH, 5_000),
            "PoE1 · no snapshot yet"
        );
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
