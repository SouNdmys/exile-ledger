//! 暗金热度页:热门 BD 在穿什么,拼上经济接口的参考价和挂单数。
//!
//! 人数来自 poe.ninja builds 搜索的 `items` 分面(全联赛一次请求就有,
//! 不靠角色采样),价格来自它文档化的经济接口。两边拼在一张表上,才回答得了
//! 真正的问题:"大家都在用的这件东西,现在多少钱、有几个人在卖"。
//!
//! 分区筛选器就是采样跑过的那一串查询(全联赛 / 某个职业 / 某个技能 /
//! 穿着某件暗金 / 职业+技能)。换一个分区,占比的**分母也跟着换** ——
//! "Deadeye 里有 41% 的人穿它"和"全联赛 11%"是两句不同的话。

use gpui::{ClipboardItem, Context, ParentElement, SharedString, Styled, Window, div, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::switch::Switch;
use gpui_component::{Disableable as _, Selectable as _, Sizable as _, Size, StyledExt as _};
use serde_json::Value;

use pnd_domain::{Currency, CurrencyRates, Game, encode_search_id, unique_search_page_url};
use pnd_platform_win::open_url;
use pnd_runtime::now_secs;

use super::observations::WatchDraft;
use super::watches::milli_text;
use super::{Cell, TableContent, Tone, column, number_column};
use crate::i18n::{self, Text};
use crate::shell::link::ago_text;
use crate::shell::ninja::{
    DemandTier, NinjaData, UniqueRow, demand_ratio_milli, demand_tier, game_league_text,
    median_milli, ninja_league_name, partition_label, percent_text,
};
use crate::shell::{AppShell, Choice, hint, page_heading, panel, picker, table};
use crate::theme::*;

/// 占比低于这个数的行默认收起来。
///
/// 0.5% 不是洁癖:全联赛的 `items` 分面有四百多条,其中三百多条是"某个人
/// 捡到一件就穿上了"。全铺出来,真正的热门装备反而要往下翻半天。
pub const MIN_SHARE_PERCENT: f64 = 0.5;

/// 7 天涨跌那一列现在画不画。
///
/// **现在是 `false`。** poe.ninja 在 2026-09-07 把经济接口的计价基准币换掉了
/// (从 exalted 换成 divine)。7 天窗口只要还跨着那一天,这个百分比就是拿
/// 两把不同的尺子相减的结果:榜上整片 `-99%` 说的不是"这东西跌没了",
/// 是"分母换了一个近百倍的单位"。一个看着像价格、其实是量纲事故的数字,
/// 比没有这个数字更坏 —— 所以整列先闭嘴,画成一个安静的破折号。
///
/// **什么时候翻回 `true`**:等 7 天窗口整个滚过 2026-09-07 之后(也就是
/// 2026-09-14 起),再对着榜上几行手动核一次涨跌方向,确认不再出现整片
/// `-99%`,就把这里改成 `true`,同时改掉
/// `the_seven_day_column_is_a_dash_while_the_switch_is_off` 那个测试。
const SHOW_SEVEN_DAY_CHANGE: bool = false;

/// 一键蹲价的上限是参考价的**八成**(分子/分母写成两个整数,免得碰浮点)。
///
/// 参考价是"现在市面上大概多少钱",而蹲价要等的是比市价便宜的那一件:
/// 填成十成的话,市价一波动就响,每一条都是白跑一趟。
const CAP_NUMERATOR: i64 = 4;
const CAP_DENOMINATOR: i64 = 5;

/// 表格现在按什么筛、按什么排。
///
/// 三个 `bool` 装进一个结构体,不是三个并排的参数:调用处写成
/// `unique_rows(rows, rates, false, true, false, text)` 的话,谁也说不出
/// 中间那个 `true` 是哪一个开关。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UniquesView {
    /// 占比不足 [`MIN_SHARE_PERCENT`] 的也铺出来。
    pub show_all: bool,
    /// 只留下 [`DemandTier::Scarce`] 那一档。
    pub scarce_only: bool,
    /// 按供需比从高到低排,而不是按人数(库给的顺序)。
    pub sort_by_demand: bool,
}

/// 表上的一行:那件暗金,加上算给它的供需比和档位。
///
/// 借着榜里那一行而不是复制一份:这份清单每次重画都重算,活不过一帧。
#[derive(Clone, Copy, Debug)]
pub struct DemandRow<'a> {
    /// **没筛之前**的名次,1 起。收起冷门行、换个排序都不会改它。
    pub rank: usize,
    pub row: &'a UniqueRow,
    /// 供需比 ×1000。`None` = 挂单数缺席,算不出来。
    pub ratio_milli: Option<i64>,
    pub tier: DemandTier,
}

/// 0.5% 那一刀筛完、档位也评完的那一批行 —— 「只看紧俏」和排序还没生效。
///
/// 为什么这一步要单独拿出来:档位是相对**这一批**评的,而「只看紧俏」是按
/// 档位筛的。拿筛完的结果再去算中位数就成了一个咬自己尾巴的循环 —— 留下的
/// 全是紧俏,中位数被抬高,下一轮又有一半掉出紧俏。
#[must_use]
pub fn tiered_rows(rows: &[UniqueRow], show_all: bool) -> Vec<DemandRow<'_>> {
    let shown: Vec<(usize, &UniqueRow)> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| show_all || row.share_percent >= MIN_SHARE_PERCENT)
        .collect();
    // 中位数只认算得出比值的那些行:挂单数缺席的一行既不是高也不是低,
    // 让它参与只会把这条线往下拽。
    let ratios: Vec<i64> = shown
        .iter()
        .filter_map(|(_, row)| demand_ratio_milli(row.users, row.listings))
        .collect();
    let median = median_milli(&ratios);
    shown
        .into_iter()
        .map(|(index, row)| {
            let ratio_milli = demand_ratio_milli(row.users, row.listings);
            DemandRow {
                rank: index + 1,
                row,
                ratio_milli,
                tier: demand_tier(ratio_milli, median),
            }
        })
        .collect()
}

/// 表上真正画出来的那些行:筛过、排过。
#[must_use]
pub fn visible_rows(rows: &[UniqueRow], view: UniquesView) -> Vec<DemandRow<'_>> {
    let mut shown = tiered_rows(rows, view.show_all);
    if view.scarce_only {
        shown.retain(|shown| shown.tier == DemandTier::Scarce);
    }
    if view.sort_by_demand {
        // 算不出比值的沉到最后:它不是"供需比 0",而是"不知道" ——
        // 混在过剩那一头会让人以为它烂大街。`sort_by_key` 是稳定的,
        // 所以比值打平的几行还按人数排(库给的顺序)。
        shown.sort_by_key(|shown| std::cmp::Reverse(shown.ratio_milli.unwrap_or(i64::MIN)));
    }
    shown
}

/// 紧俏几件、过剩几件。
///
/// 数的是**「只看紧俏」筛之前**的那一批:开着那个开关的时候,过剩那个数
/// 才不会永远是 0 —— 而"按下去会剩几行"正是这句话要回答的问题。
#[must_use]
pub fn demand_counts(rows: &[UniqueRow], show_all: bool) -> (usize, usize) {
    let tiered = tiered_rows(rows, show_all);
    let count = |want: DemandTier| tiered.iter().filter(|shown| shown.tier == want).count();
    (count(DemandTier::Scarce), count(DemandTier::Glut))
}

/// 一件暗金 → 一份蹲价草稿。
///
/// 查询里只写一个名字:底子名是 poe.ninja 那边的写法,和交易站不一定逐字
/// 一致,而多写一个对不上的条件,搜出来的是零件 —— 一条永远不响的蹲价
/// 比没有这条还坏,因为它看起来在工作。
///
/// **只做草稿,不加**:按下"新增"的永远是用户自己(同观察页那条一键蹲价)。
#[must_use]
pub fn unique_watch_draft(row: &UniqueRow, league: &str) -> WatchDraft {
    WatchDraft {
        search_id: encode_search_id(&unique_query_json(&row.name)),
        league: league.to_owned(),
        label: row.name.clone(),
        cap_milli: row.price_milli.map(cap_milli),
        currency: row
            .price_currency
            .as_ref()
            .map_or_else(String::new, |currency| currency.code().to_owned()),
    }
}

/// 蹲这件暗金的那条查询:在线的,按价格从低到高。
///
/// 手拼字符串而不是 `json!`:`serde_json` 的对象是有序表,`json!` 拼出来的
/// 键会按字母重排,而这条查询的形状是照着交易站网页发出去的那份抄的。
/// 名字仍旧走 `Value::String` 转义 —— 暗金名里有撇号(`Beira's Anguish`),
/// 哪天再冒出个引号,手写的引号就会把整段 JSON 断成两半。
fn unique_query_json(name: &str) -> String {
    let name = Value::String(name.to_owned());
    format!(
        r#"{{"query":{{"status":{{"option":"online"}},"name":{name},"stats":[{{"type":"and","filters":[]}}]}},"sort":{{"price":"asc"}}}}"#
    )
}

/// 参考价 → 蹲价上限,整数运算。
///
/// 折出来还有一个整单位以上就抹掉零头(23.92 → 23):蹲价上限是要念给
/// 自己听的一个数,`23` 比 `23.92` 好记。不到一个整单位的原样留着千分位
/// (0.085 的八成是 0.068)—— 抹成 0 的话那条蹲价永远不会响。
fn cap_milli(price_milli: i64) -> i64 {
    let cap = price_milli.saturating_mul(CAP_NUMERATOR) / CAP_DENOMINATOR;
    if cap >= 1_000 {
        return cap / 1_000 * 1_000;
    }
    cap
}

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
            // 紧挨着挂单数:这一列就是"人数 ÷ 挂单数",两个数分开摆的话,
            // 中间隔着的那些列会让人以为它是另一件事。
            number_column("demand", text.uniques_col_demand, 70.),
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
    view: UniquesView,
    text: &'static Text,
) -> TableContent {
    TableContent {
        rows: unique_rows(rows, rates, view, text),
        ..table_content(text)
    }
}

/// 每件暗金一行。筛和排都在 [`visible_rows`] 里做完了,这里只管画。
///
/// **名次按没筛之前算**:收起冷门行之后第 12 名还是第 12 名,不会因为
/// 前面藏了几条就变成第 9 名;按供需重排之后也一样跟着行走。
pub fn unique_rows(
    rows: &[UniqueRow],
    rates: &CurrencyRates,
    view: UniquesView,
    text: &'static Text,
) -> Vec<Vec<Cell>> {
    visible_rows(rows, view)
        .into_iter()
        .map(|shown| {
            let row = shown.row;
            vec![
                Cell::muted(shown.rank.to_string()),
                Cell::new(row.name.clone(), name_tone(shown.tier, shown.rank)),
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
                Cell::new(
                    demand_text(shown.ratio_milli, text),
                    demand_tone(shown.tier, shown.ratio_milli),
                ),
                seven_day_cell(row.change_percent, text),
            ]
        })
        .collect()
}

/// 供需那一格:`56.98`。算不出来就写"—"。
///
/// 两位小数直接从千分整数上取,不经过浮点:这一列排序用的是同一个整数,
/// 显示和排序读同一个数,才不会出现"印得更小的那行却排在前面"。
fn demand_text(ratio_milli: Option<i64>, text: &'static Text) -> String {
    let Some(milli) = ratio_milli else {
        return text.common_none.to_owned();
    };
    // 千分位 → 百分位,四舍五入(+5 再除 10)。
    let hundredths = (milli + 5) / 10;
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// 暗金名那一格的语气。
///
/// 紧俏压过榜首那一抹金色:榜首本来就在第一行、名次那一列还写着 1,
/// 而"这件东西市面上不够卖"正是这一列存在的理由,不该被排名的装饰盖掉。
fn name_tone(tier: DemandTier, rank: usize) -> Tone {
    match tier {
        DemandTier::Scarce => Tone::Good,
        DemandTier::Glut => Tone::Muted,
        DemandTier::Balanced if rank == 1 => Tone::Accent,
        DemandTier::Balanced => Tone::Plain,
    }
}

/// 供需那一格的语气。算不出来的那一格是安静的灰,和表上别的"—"一样。
fn demand_tone(tier: DemandTier, ratio_milli: Option<i64>) -> Tone {
    if ratio_milli.is_none() {
        return Tone::Muted;
    }
    match tier {
        DemandTier::Scarce => Tone::Good,
        DemandTier::Glut => Tone::Muted,
        DemandTier::Balanced => Tone::Data,
    }
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

/// 表上那一格,先过一遍 [`SHOW_SEVEN_DAY_CHANGE`] 这个闸。
///
/// 闸和格式分成两个函数,是为了让 [`change_cell`] 那套涨跌写法在闸关着的
/// 这段时间里仍然有测试盯着 —— 否则等到哪天把闸推回去,推开的会是一段
/// 谁也没验过多久的代码。
fn seven_day_cell(change: Option<f64>, text: &'static Text) -> Cell {
    if !SHOW_SEVEN_DAY_CHANGE {
        return Cell::muted(text.common_none);
    }
    change_cell(change, text)
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
            .child(self.uniques_row_actions(cx))
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
                    .child(
                        Switch::new("uniques-scarce-only")
                            .checked(self.uniques_scarce_only)
                            .label(SharedString::from(text.uniques_scarce_only))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.uniques_scarce_only = *checked;
                                this.uniques_dirty = true;
                                cx.notify();
                            })),
                    )
                    // 上游那张表的列头不支持点一下排序,所以排序做成一个开关。
                    .child(
                        Switch::new("uniques-sort-demand")
                            .checked(self.uniques_sort_by_demand)
                            .label(SharedString::from(text.uniques_sort_by_demand))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.uniques_sort_by_demand = *checked;
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

    /// 选中那件暗金能做的三件事:复制名字、去交易站看行情、一键做成蹲价。
    ///
    /// 前两件两代都有 —— 榜上只有一个名字,认不出那是什么东西的时候,
    /// 要么把名字抄走自己去查,要么直接开一张填好的搜索页看看长什么样、
    /// 现在卖多少。
    ///
    /// **一键蹲价那个按钮 PoE1 上不出现**:本地拼搜索 id 只对 PoE2 成立
    /// (PoE1 的 id 只有服务端发得出来),做出来的草稿粘到蹲价页也用不了 ——
    /// 同观察页那个
    /// [`make_watch_offered`](super::observations::make_watch_offered)。
    fn uniques_row_actions(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let offered = self.ninja_game() == Game::Poe2;
        let selected = self.selected_unique(cx);
        let label = selected
            .as_ref()
            .map_or_else(|| text.common_select_row.to_owned(), |row| row.name.clone());
        panel()
            .flex_none()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(6.))
            .child(
                div()
                    .text_size(fs(FS_11_5))
                    .text_color(muted())
                    .child(text.uniques_row_actions),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(fs(FS_11_5))
                    .text_color(c(TEXT_SECONDARY))
                    .child(SharedString::from(label)),
            )
            .child(
                Button::new("uniques-copy-name")
                    .label(text.uniques_copy_name)
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.copy_unique_name(cx);
                    })),
            )
            .child(
                Button::new("uniques-open-trade")
                    .label(text.common_open_trade_site)
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_unique_on_trade(cx);
                    })),
            )
            .children(offered.then(|| {
                Button::new("uniques-make-watch")
                    .label(text.uniques_make_watch)
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.make_watch_from_unique(window, cx);
                    }))
            }))
    }

    /// 脚注:紧俏 / 过剩各几件,采样跑到哪了,参考价是什么时候抓的。
    fn uniques_footer(&self) -> gpui::Div {
        let text = self.text();
        let (scarce, glut) = demand_counts(&self.ninja.uniques, self.uniques_show_all);
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
            .child(hint(i18n::fill(
                text.uniques_tier_line,
                &[&scarce.to_string(), &glut.to_string()],
            )))
    }

    /// 三个开关合起来就是"表上现在画的是哪一份"。
    ///
    /// 重建表格和"选中的是第几行"都要读它,而两处算出不一样的一份,
    /// 按钮作用的就是另一件暗金了。
    pub(crate) fn uniques_view(&self) -> UniquesView {
        UniquesView {
            show_all: self.uniques_show_all,
            scarce_only: self.uniques_scarce_only,
            sort_by_demand: self.uniques_sort_by_demand,
        }
    }

    /// 表里选中的那件暗金。
    ///
    /// 表上画的是筛过、排过的那一份,所以这里用同一套参数再算一遍 ——
    /// 直接拿行号去 `self.ninja.uniques`(库给的原始顺序)里取的话,
    /// 选中的和按钮作用的就是两件不同的东西(同 `selected_mod`)。
    fn selected_unique(&self, cx: &mut Context<Self>) -> Option<UniqueRow> {
        let row = self.uniques_table.read(cx).selected_row()?;
        visible_rows(&self.ninja.uniques, self.uniques_view())
            .get(row)
            .map(|shown| shown.row.clone())
    }

    /// 把选中那件暗金的名字抄到剪贴板。
    ///
    /// 榜上给的只有一个名字,而"这到底是件什么东西"得拿这几个字去别处问 ——
    /// 手抄一个 `Lavianga's Spirits` 是很容易抄错一个字母的活。走的是复制
    /// 私聊那条同样的路([`AppShell::copy_whisper`]):写剪贴板 + 状态行说一声。
    fn copy_unique_name(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(row) = self.selected_unique(cx) else {
            self.set_notice(text.common_select_row.to_owned());
            cx.notify();
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(row.name));
        self.set_notice(text.uniques_name_copied.to_owned());
        cx.notify();
    }

    /// 在浏览器里开一张已经填好这件暗金的交易站搜索页。
    ///
    /// 参考价那一列只是一个中位数,而"现在挂着的都长什么样、便宜的那几件
    /// 差在哪"只有交易站答得出。查询是本地拼的(`?q=` 那一段),不占任何
    /// 限速预算 —— 开出来之后是浏览器在跟交易站说话,不是这个程序。
    ///
    /// 联赛留空时不开:PoE1 的联赛名默认是空的("用当季挑战联赛"),而
    /// 交易站的搜索页地址里非有一个联赛不可,拼出来的会是一条打不开的链接。
    fn open_unique_on_trade(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(row) = self.selected_unique(cx) else {
            self.set_notice(text.common_select_row.to_owned());
            cx.notify();
            return;
        };
        let game = self.ninja_game();
        let league = ninja_league_name(&self.settings, game).to_owned();
        if league.is_empty() {
            self.set_notice(text.uniques_no_league.to_owned());
            cx.notify();
            return;
        }
        let url = unique_search_page_url(game, &league, &row.name);
        match open_url(&url) {
            Ok(()) => {
                self.push_log(format!("opened {url}"));
                self.set_notice(text.notice_opened_trade.to_owned());
            }
            Err(error) => {
                self.push_log(format!("could not open {url}: {error}"));
                self.set_notice(i18n::fill(text.notice_open_failed, &[&error.to_string()]));
            }
        }
        cx.notify();
    }

    /// 一键蹲价:把选中那件暗金做成一份草稿,翻到蹲价页填进表单。
    ///
    /// **只填,不加**:按下"新增"的永远是用户自己 —— 一条自动加进去的搜索
    /// 会立刻开始花限速预算,而它是不是用户要的还没人确认过。
    fn make_watch_from_unique(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(row) = self.selected_unique(cx) else {
            self.set_notice(text.common_select_row.to_owned());
            cx.notify();
            return;
        };
        // 表上画的是哪个联赛,草稿就写哪个 —— 这条按钮只在 PoE2 上出现,
        // 所以取到的是 `settings.league`。
        let league = ninja_league_name(&self.settings, self.ninja_game()).to_owned();
        let draft = unique_watch_draft(&row, &league);
        self.push_log(format!("watch draft from unique: {}", draft.label));
        self.prefill_add_form(draft, window, cx);
        self.show_page(crate::shell::Page::Watches);
        self.set_notice(text.uniques_prefilled.to_owned());
        cx.notify();
    }
}

#[cfg(test)]
mod ninja_uniques_tests {
    use gpui_component::select::SelectItem as _;
    use pnd_domain::decode_search_id;
    use pnd_storage::{SnapshotRow, SnapshotStage};

    use super::*;
    use crate::i18n;

    /// 默认视图:收起冷门行,不筛紧俏,按人数排(库给的顺序)。
    fn view() -> UniquesView {
        UniquesView::default()
    }

    /// 全铺出来的那一份。
    fn view_all() -> UniquesView {
        UniquesView {
            show_all: true,
            ..UniquesView::default()
        }
    }

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
        let built = unique_rows(&rows(), &rates(), view(), &i18n::ENGLISH);
        assert_eq!(built.len(), 3, "占比 0.18% 的那条默认收起来");
        assert_eq!(built[0][0].text(), "1");
        assert_eq!(built[0][1].text(), "Wake of Destruction");
        assert_eq!(built[0][1].tone(), Tone::Accent, "榜首标出来");
        assert_eq!(built[0][2].text(), "7464");
        assert_eq!(built[0][3].text(), "11.4%");
        // 29.9 divine(接口今天的基准币),按 1 divine = 83.42 exalted 换算。
        assert_eq!(built[0][4].text(), "29.9 div ≈ 2494 ex");
        assert_eq!(built[0][5].text(), "131");
        // 7 天那一列现在是关着的,见 `SHOW_SEVEN_DAY_CHANGE`;涨跌怎么写
        // 由 `a_big_drop_keeps_its_decimal_instead_of_reading_as_zero` 盯着。
        let text = &i18n::ENGLISH;
        assert_eq!(change_cell(Some(6.0), text).text(), "+6.0%");
        assert_eq!(change_cell(Some(6.0), text).tone(), Tone::Good);
        assert_eq!(change_cell(Some(-9.0), text).text(), "-9.0%");
        assert_eq!(change_cell(Some(-9.0), text).tone(), Tone::Warn);
    }

    /// 跌了 99.53% 就写 `-99.5%`,不是 `-100%`。
    ///
    /// 那个 `-100%` 是四舍五入印出来的,而 `-100%` 在人眼里是"归零了"——
    /// 完全不同的一句话。Trenchtimbre 那条榜上的怪数字就是这么来的。
    ///
    /// 直接问 [`change_cell`],不走表格:表上那一列眼下被
    /// [`SHOW_SEVEN_DAY_CHANGE`] 关着,而这段写法要在闸推回去的那天还是对的。
    #[test]
    fn a_big_drop_keeps_its_decimal_instead_of_reading_as_zero() {
        let text = &i18n::ENGLISH;
        let cell = |change: Option<f64>| change_cell(change, text);
        assert_eq!(cell(Some(-99.53)).text(), "-99.5%");
        // 小到看不见的涨幅也是涨,别被抹成 +0%。
        assert_eq!(cell(Some(0.4)).text(), "+0.4%");
        assert_eq!(cell(Some(0.4)).tone(), Tone::Good);
        // 真的没有一周历史时是"—",不是一个编出来的 0。
        assert_eq!(cell(None).text(), "—");
        assert_eq!(cell(None).tone(), Tone::Muted);
    }

    /// 开关关着的时候,7 天那一格一律是安静的"—",连算都不算。
    ///
    /// 有涨有跌的两行都要验:关掉这一列不是"把负数藏起来",是整列都不说话。
    ///
    /// **这个测试描述的是 `SHOW_SEVEN_DAY_CHANGE == false` 时的样子**,所以
    /// 哪天把那个开关翻回 `true`,第一个变红的就是它 —— 那正是提醒你回来改
    /// 这一条的机制。
    #[test]
    fn the_seven_day_column_is_a_dash_while_the_switch_is_off() {
        let built = unique_rows(&rows(), &rates(), view(), &i18n::ENGLISH);
        for (row, built) in built.iter().enumerate() {
            assert_eq!(built[7].text(), "—", "第 {row} 行");
            assert_eq!(built[7].tone(), Tone::Muted, "第 {row} 行");
        }
    }

    /// 经济接口里没有的那件东西,四格都写"—",不写 0 —— 0 会被读成"不值钱"。
    #[test]
    fn a_unique_without_a_listing_says_so_instead_of_showing_zero() {
        let built = unique_rows(&rows(), &rates(), view(), &i18n::ENGLISH);
        assert_eq!(built[2][1].text(), "Breath of the Mountains");
        assert_eq!(built[2][4].text(), "—");
        assert_eq!(built[2][5].text(), "—");
        assert_eq!(built[2][6].text(), "—", "挂单数都没有,供需比更算不出来");
        assert_eq!(built[2][7].text(), "—");
    }

    /// 汇率还没读到就只写它自己的单位,不猜一个换算值。
    #[test]
    fn without_rates_the_price_keeps_its_own_currency() {
        let built = unique_rows(&rows(), &CurrencyRates::none(), view(), &i18n::ENGLISH);
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

        let all = unique_rows(&rows, &rates(), view_all(), &i18n::ENGLISH);
        assert_eq!(all.len(), 4);
        assert_eq!(all[3][0].text(), "4");
        assert_eq!(all[3][1].text(), "Somebody's Trinket");
        // "有历史、真的没涨没跌"也是一个数,不是"—"。这一条问的是
        // `change_cell` 本身:表上那一列被 `SHOW_SEVEN_DAY_CHANGE` 关着。
        let flat = change_cell(Some(0.0), &i18n::ENGLISH);
        assert_eq!(flat.text(), "+0.0%");
        assert_eq!(flat.tone(), Tone::Muted);
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

    /// 五件供需比故意排得很开的暗金,专门喂给档位那几条测试。
    ///
    /// 后两条的占比在 0.5% 以下 —— 默认收起来,放出来之后中位数会往下掉,
    /// 于是每一行的档位都得重算一遍。
    fn demand_rows() -> Vec<UniqueRow> {
        let row = |name: &str, users: u64, share: f64, listings: i64| UniqueRow {
            name: name.to_owned(),
            users,
            share_percent: share,
            price_milli: Some(1_000),
            price_currency: Some(Currency::Divine),
            listings: Some(listings),
            change_percent: None,
        };
        vec![
            row("Tight", 9_000, 20.0, 100),  // 90.00
            row("Middle", 3_000, 10.0, 100), // 30.00
            row("Loose", 1_000, 5.0, 100),   // 10.00
            row("Cold", 100, 0.2, 100),      // 1.00,默认收起来
            row("Colder", 60, 0.1, 100),     // 0.60,默认收起来
        ]
    }

    /// 供需那一格:两位小数,算不出来就是"—"。
    ///
    /// "—" 而不是 0:挂单数缺席的意思是"经济接口里没有这件东西",而 0
    /// 会被读成"一个人都没在用"。
    #[test]
    fn the_demand_column_shows_two_decimals_or_a_dash() {
        let built = unique_rows(&rows(), &rates(), view(), &i18n::ENGLISH);
        // 7464 人 / 131 件挂单 = 56.977…,四舍五入到两位。
        assert_eq!(built[0][6].text(), "56.98");
        assert_eq!(built[1][6].text(), "164.09");
        assert_eq!(built[2][6].text(), "—");
        assert_eq!(built[2][6].tone(), Tone::Muted);
    }

    /// 紧俏的那一行标绿(名字和比值都是),过剩的那一行整体压暗。
    #[test]
    fn a_scarce_row_goes_green_and_a_glut_row_goes_quiet() {
        let built = unique_rows(&demand_rows(), &rates(), view(), &i18n::ENGLISH);
        assert_eq!(built.len(), 3, "占比不足 0.5% 的两条默认收起来");

        assert_eq!(built[0][1].text(), "Tight");
        assert_eq!(built[0][6].text(), "90.00");
        assert_eq!(built[0][1].tone(), Tone::Good, "紧俏压过榜首那抹金色");
        assert_eq!(built[0][6].tone(), Tone::Good);

        assert_eq!(built[1][1].tone(), Tone::Plain, "常态那一行不着色");
        assert_eq!(built[1][6].tone(), Tone::Data);

        assert_eq!(built[2][1].text(), "Loose");
        assert_eq!(built[2][1].tone(), Tone::Muted, "过剩的压暗");
        assert_eq!(built[2][6].tone(), Tone::Muted);
    }

    /// 档位是**相对这张表**算的:放出冷门行,中位数跟着掉,每一行都得重评。
    ///
    /// 这一条是整个功能的支点。写死一条"5 个人抢一件就算紧俏"的线,换到
    /// 某个职业分区(分母小一个数量级)之后要么整张表全绿,要么一条都没有。
    #[test]
    fn hiding_the_cold_rows_moves_the_median_and_re_tiers_everything() {
        let rows = demand_rows();
        let tier_of = |view: UniquesView, name: &str| {
            visible_rows(&rows, view)
                .into_iter()
                .find(|shown| shown.row.name == name)
                .expect("行还在表上")
                .tier
        };

        // 只看三条热门的:中位数 30.00。
        assert_eq!(tier_of(view(), "Tight"), DemandTier::Scarce);
        assert_eq!(tier_of(view(), "Middle"), DemandTier::Balanced);
        assert_eq!(tier_of(view(), "Loose"), DemandTier::Glut);

        // 放出两条冷门的:中位数掉到 10.00,于是常态的变紧俏、过剩的变常态。
        assert_eq!(tier_of(view_all(), "Tight"), DemandTier::Scarce);
        assert_eq!(tier_of(view_all(), "Middle"), DemandTier::Scarce);
        assert_eq!(tier_of(view_all(), "Loose"), DemandTier::Balanced);
        assert_eq!(tier_of(view_all(), "Cold"), DemandTier::Glut);

        // 表下面那一句数的是筛之前的那一批 —— 不然开了"只看紧俏"之后
        // 过剩那个数永远是 0,那句话就白写了。
        assert_eq!(demand_counts(&rows, false), (1, 1));
        assert_eq!(demand_counts(&rows, true), (2, 2));
    }

    /// 「只看紧俏」只留紧俏那一档,而中位数**不跟着变** —— 跟着变的话
    /// 就成了一个咬自己尾巴的循环:留下的全是紧俏,中位数被抬高,
    /// 下一轮又有一半掉出紧俏。
    #[test]
    fn scarce_only_keeps_the_tight_rows_without_moving_the_line() {
        let rows = demand_rows();
        let scarce = UniquesView {
            scarce_only: true,
            ..view()
        };
        let names: Vec<&str> = visible_rows(&rows, scarce)
            .iter()
            .map(|shown| shown.row.name.as_str())
            .collect();
        assert_eq!(names, vec!["Tight"]);

        let scarce_all = UniquesView {
            scarce_only: true,
            ..view_all()
        };
        let names: Vec<&str> = visible_rows(&rows, scarce_all)
            .iter()
            .map(|shown| shown.row.name.as_str())
            .collect();
        assert_eq!(names, vec!["Tight", "Middle"]);
    }

    /// 「按供需排序」把最紧俏的提到最前,而名次还是原来那个名次。
    ///
    /// 算不出比值的排到最后:它不是"供需比 0",而是"不知道" —— 混在
    /// 过剩那一头会让人以为它烂大街。
    #[test]
    fn sorting_by_demand_puts_the_tightest_first_and_the_unknown_last() {
        let sorted = UniquesView {
            sort_by_demand: true,
            ..view_all()
        };
        let built = unique_rows(&rows(), &rates(), sorted, &i18n::ENGLISH);
        let names: Vec<&str> = built.iter().map(|row| row[1].text()).collect();
        assert_eq!(
            names,
            vec![
                "Beira's Anguish",
                "Wake of Destruction",
                "Somebody's Trinket",
                "Breath of the Mountains",
            ]
        );
        // 名次跟着行走,不跟着位置走:排到第一位的还是榜上第 2 名。
        assert_eq!(built[0][0].text(), "2");
        assert_eq!(built[1][0].text(), "1");
        assert_eq!(built[3][6].text(), "—");
    }

    /// 一键蹲价:查询只写名字,上限压到参考价的八成。
    ///
    /// 八成是"比市价便宜才值得响一声";只写名字是因为底子名是 poe.ninja
    /// 那边的写法,和交易站不一定逐字一致 —— 多写一个对不上的条件,
    /// 搜出来的是零件。
    #[test]
    fn a_unique_turns_into_a_watch_draft_at_four_fifths_of_the_reference_price() {
        let rows = rows();
        let draft = unique_watch_draft(&rows[0], "Forbidden Rites");
        assert_eq!(draft.label, "Wake of Destruction");
        assert_eq!(draft.league, "Forbidden Rites");
        assert_eq!(draft.currency, "divine");
        // 29.9 div 的八成是 23.92 —— 一个整单位以上就抹掉零头。
        assert_eq!(draft.cap_milli, Some(23_000));
        assert_eq!(
            decode_search_id(&draft.search_id).expect("解得开"),
            r#"{"query":{"status":{"option":"online"},"name":"Wake of Destruction","stats":[{"type":"and","filters":[]}]},"sort":{"price":"asc"}}"#
        );
    }

    /// 不到一个整单位的价格保留千分位 —— 抹成 0 的话那条蹲价永远不会响。
    #[test]
    fn a_sub_unit_price_keeps_its_thousandths() {
        let mut cheap = rows()[0].clone();
        cheap.price_milli = Some(85);
        // 0.085 div 的八成是 0.068。
        assert_eq!(
            unique_watch_draft(&cheap, "Forbidden Rites").cap_milli,
            Some(68)
        );

        // 经济接口里压根没有这件东西:上限和货币都留空,让用户自己填。
        let draft = unique_watch_draft(&rows()[2], "Forbidden Rites");
        assert_eq!(draft.label, "Breath of the Mountains");
        assert_eq!(draft.cap_milli, None);
        assert_eq!(draft.currency, "");
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
