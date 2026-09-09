//! 市场观察页:一整条搜索里的货,最后都怎么样了。
//!
//! 蹲价问的是"这一件到我的价了没有",观察问的是另一个问题:**这一类货能不能
//! 卖掉、多久卖掉、什么价卖掉**。交易站没有成交接口,所以答案只能从"挂单
//! 消失了"反推 —— 那一步反推在 `pnd-domain::observe` 里,这一页只负责把
//! 库里攒下来的结论摆出来。
//!
//! 三块,从上到下:
//!
//! 1. **观察列表**。粘一条搜索进来就多一条观察;每一行写着它在册多少条、
//!    没了多少条、秒推连上没有、下一次什么时候动。
//! 2. **聚合表**(左下),两栏。**词缀战绩**一行是一条词缀模板:带着它的货
//!    见过几件、卖掉几件、成交价和在售价的中位差多少 —— "+# 生命 的石板
//!    到底好不好卖"只有它答得上。**价位战绩**一行是一个价位档:标这个价的
//!    货见过几件、卖掉几件、在市面上待多久。头一天的数据就说明后者一样要紧:
//!    消失的碑牌全是标 2 divine 的,还挂着的那些中位价在 3 divine ——
//!    而那批货的词缀一模一样,词缀表看不出这件事。
//! 3. **挂单流**(右下)。最近消失的和在售最久的两栏,带着每条货身上的词缀。
//!    聚合表是结论,这一栏是原始证据:结论看着不对的时候,得能翻到具体是
//!    哪几件货撑起了那个数。
//!
//! 数据不在每一帧去库里读:actor 每跑完一轮发一条 `ObservationChanged`,
//! 界面收到之后延迟一点重读一次(理由同提醒历史 —— 写库的是别的线程)。

use std::collections::BTreeMap;

use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{Selectable as _, Sizable as _, Size, StyledExt as _};

use pnd_domain::{
    CurrencyRates, Game, GoneClass, ObservationId, Price, PriceBucket, RateSource, RateSources,
    SUB_DIVINE_BUCKET_MILLI, decode_search_id, default_label_for, encode_search_id,
    parse_search_reference, with_stat_filter,
};
use pnd_runtime::{LiveRunState, ObservationStatus, RuntimeCommand, now_secs};
use pnd_settings::{AppSettings, ObservationEntry};
use pnd_storage::{
    ModOutcome, ObservationSummary, ObservedListingRow, ObservedMod, PriceMode, PriceOutcome,
    StorageError,
};

use super::{Cell, TableContent, Tone, column, league_cell_text, number_column};
use crate::i18n::{self, Text};
use crate::shell::link::local_stamp;
use crate::shell::ninja::percent_text;
use crate::shell::pages::watches::{live_text, milli_text};
use crate::shell::{
    AppShell, Choice, field_label, field_row, hint, page_heading, panel, picker, table,
};
use crate::theme::*;

/// 备注名留空时,拿搜索 id 的前几个字符顶上(同蹲价页)。
const LABEL_FROM_ID_CHARS: usize = 10;

/// 挂单流一栏一次读多少条。
///
/// 20 条足够回答"最近这一阵都卖掉了些什么",而每条还要连它身上的词缀一起读,
/// 再多就是在为一块滚不到底的区域打库。
pub const STREAM_ROWS: u32 = 20;

/// 挂单流里一条货最多列几行词缀。石板上词缀不多,稀有装能有十几条 ——
/// 全铺出来一屏就只剩得下两件货。
const STREAM_MOD_LINES: usize = 4;

/// 收藏过的词缀前面挂的那颗星。不走 i18n:它不是一句话,两种语言下都是
/// 同一颗星。
const FAVOURITE_MARK: &str = "★ ";

/// 一键蹲价拼出来的备注名里,词缀最多留几个字。
///
/// 40:蹲价表那一列 200 像素,"观察名 · 一整句词缀"常常比它长一倍,
/// 而截断之后剩下的那半句已经够认出是哪条词缀了。
pub const DRAFT_TEMPLATE_CHARS: usize = 40;

/// 词缀表默认要求的样本数。
///
/// 3 是"能不能开口说话"的下限:两件货里卖掉一件不能叫 50% 成交率。
pub const DEFAULT_MIN_SAMPLES: u32 = 3;

/// 样本数那几个预设档。做成按钮不做输入框:这个数只会在这几档之间跳,
/// 而每按一下就要重排一次表。
pub const MIN_SAMPLE_PRESETS: [u32; 4] = [1, 3, 5, 10];

/// 左下那块聚合表现在看的是哪一栏。
///
/// 两栏问的是同一批货的两个问题,而头一天的数据说明第二个问题一样有话说:
/// 消失的碑牌全是标 2 divine 的,还挂着的那些中位价在 3 divine —— 那是
/// 词缀那张表看不出来的事(它们的词缀都一样)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AggregateTab {
    /// 词缀战绩:什么样的货出得掉。
    #[default]
    Mods,
    /// 价位战绩:什么价出得掉。
    Price,
}

/// 挂单流现在看的是哪一栏。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StreamTab {
    /// 最近消失的:成交(或者撤单)刚刚发生的那些。
    #[default]
    Gone,
    /// 在售最久的:挂在那儿没人要的那些。
    Active,
}

/// 挂单流里的一条:库里那一行,加上这件货身上的词缀。
///
/// 词缀在读列表的时候就一起读出来,而不是画的时候现查:画表是每帧都在做的事,
/// 而这两样东西一起变、一起过时。
#[derive(Clone, Debug, PartialEq)]
pub struct StreamEntry {
    pub row: ObservedListingRow,
    pub mods: Vec<ObservedMod>,
}

/// 界面手上那份"选中的这条观察攒到了什么"。
///
/// 和 [`crate::shell::ninja::NinjaData`] 一个路数:库里读一次进内存,之后
/// 筛选、排序、换语言都在内存里做,不再打库。
#[derive(Clone, Debug, Default)]
pub struct ObservationData {
    /// 下面两块画的是哪一条观察。`None` = 一条观察都没有。
    pub selected: Option<ObservationId>,
    pub summary: ObservationSummary,
    /// 全部词缀模板(样本数没过滤,过滤在 [`mod_filtered`] 里做)。
    pub mods: Vec<ModOutcome>,
    /// 按币种分的价位档,同样没过滤([`price_filtered`] 里做)。
    pub prices_by_currency: Vec<PriceOutcome>,
    /// 全部折成 divine 的那一份。两份一起读进来,是因为那个两档开关按一下
    /// 就该换表 —— 为了换一栏再打一次库不值当,而这两份一起变、一起过时。
    pub prices_converted: Vec<PriceOutcome>,
    /// 折算那一份里有价、却换不出 divine 的挂单条数。
    pub unconverted: u32,
    pub gone: Vec<StreamEntry>,
    pub active: Vec<StreamEntry>,
}

impl ObservationData {
    /// 这一栏要画的那些价位档。
    #[must_use]
    pub fn prices(&self, mode: PriceMode) -> &[PriceOutcome] {
        match mode {
            PriceMode::ByCurrency => &self.prices_by_currency,
            PriceMode::Converted => &self.prices_converted,
        }
    }

    /// 当前这一栏要画的那些条目。
    #[must_use]
    pub fn stream(&self, tab: StreamTab) -> &[StreamEntry] {
        match tab {
            StreamTab::Gone => &self.gone,
            StreamTab::Active => &self.active,
        }
    }

    /// 这条观察一条挂单都还没记下来。空表要说的话不一样:没有观察是
    /// "去粘一条搜索",有观察没数据是"再等等"。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.summary == ObservationSummary::default()
    }
}

/// 新增 / 编辑表单的控件。形状照蹲价页那份:同生同死的东西放一起。
pub struct ObservationsForm {
    pub search: Entity<InputState>,
    pub label: Entity<InputState>,
    pub sample_every: Entity<InputState>,
    pub enabled: bool,
    /// 表单现在是"新增"还是"改这一条"。存 id 不存行号,理由同蹲价页:
    /// 行号在删掉一条之后就指向别人了。
    pub editing: Option<ObservationId>,
}

impl ObservationsForm {
    pub fn new(text: &'static Text, window: &mut Window, cx: &mut Context<AppShell>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(text.obs_add_search_placeholder));
        let label =
            cx.new(|cx| InputState::new(window, cx).placeholder(text.obs_label_placeholder));
        let sample_every = cx.new(|cx| InputState::new(window, cx).placeholder("1"));
        Self {
            search,
            label,
            sample_every,
            enabled: true,
            editing: None,
        }
    }

    /// 把一条现有的观察装进表单。搜索那一格也填(填的是 id),但它在这个模式
    /// 下是灰的 —— 换搜索等于换一条别的观察。
    pub fn load(
        &mut self,
        entry: &ObservationEntry,
        window: &mut Window,
        cx: &mut Context<AppShell>,
    ) {
        for (input, value) in [
            (&self.search, entry.search_id.clone()),
            (&self.label, entry.label.clone()),
            (&self.sample_every, entry.sample_every.to_string()),
        ] {
            input.update(cx, |state, cx| {
                state.set_value(value, window, cx);
            });
        }
        self.enabled = entry.enabled;
        self.editing = Some(entry.id.clone());
    }

    /// 换语言之后把占位符换掉。
    pub fn relabel(
        &mut self,
        text: &'static Text,
        window: &mut Window,
        cx: &mut Context<AppShell>,
    ) {
        for (input, placeholder) in [
            (&self.search, text.obs_add_search_placeholder),
            (&self.label, text.obs_label_placeholder),
        ] {
            input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx);
            });
        }
    }

    /// 清空并退回"新增"模式。
    fn clear(&mut self, window: &mut Window, cx: &mut Context<AppShell>) {
        for input in [&self.search, &self.label, &self.sample_every] {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        self.editing = None;
    }
}

// ---------------------------------------------------------------------
// 纯函数:时间、观察列表
// ---------------------------------------------------------------------

/// 秒数 → `mm:ss`(超过一小时就是 `h:mm:ss`)。
///
/// 观察页的倒计时和蹲价页的不一样:那边问的是"下一轮大概什么时候"(所以
/// "1 分"就够),这边问的是"我按了立即回查,它到底动没动" —— 秒针得看得见。
#[must_use]
pub fn countdown_mmss(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (hours, minutes, secs) = (seconds / 3_600, (seconds % 3_600) / 60, seconds % 60);
    if hours > 0 {
        return format!("{hours}:{minutes:02}:{secs:02}");
    }
    format!("{minutes:02}:{secs:02}")
}

/// 秒数 → "12 分钟 / 3.5 小时 / 2 天"。
///
/// 三档各留一位小数(整数就不留):一条挂单活了 12,600 秒,写成"3.5 小时"
/// 一眼就知道是"半天不到",写成"12600 秒"得自己心算。
#[must_use]
pub fn lifetime_text(seconds: i64, text: &'static Text) -> String {
    let seconds = seconds.max(0);
    if seconds < 3_600 {
        return i18n::fill(text.obs_lifetime_minutes, &[&(seconds / 60).to_string()]);
    }
    if seconds < 86_400 {
        return i18n::fill(
            text.obs_lifetime_hours,
            &[&one_decimal(seconds as f64 / 3_600.0)],
        );
    }
    i18n::fill(
        text.obs_lifetime_days,
        &[&one_decimal(seconds as f64 / 86_400.0)],
    )
}

/// 整数就写整数,否则留一位小数(同词缀热度页那几格分位数的规矩)。
fn one_decimal(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        return format!("{value:.0}");
    }
    format!("{value:.1}")
}

/// 观察列表的列。行由 [`table_content_for`] 填。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("label", text.obs_col_label, 180.),
            column("league", text.obs_col_league, 120.),
            number_column("sample", text.obs_col_sample_every, 60.),
            number_column("active", text.obs_col_active, 70.),
            number_column("gone", text.obs_col_gone, 70.),
            // 这一格写三件事:秒推档位、两条倒计时、出错原因。量过最长的那句
            // ("live disabled: no session · new listings in 09:12 · next check
            // in 01:30"),窄一点就把"为什么没连上"裁掉了。
            column("status", text.obs_col_status, 430.),
        ],
        rows: Vec::new(),
        empty: text.obs_empty.into(),
    }
}

/// 列 + 真行。
pub fn table_content_for(
    settings: &AppSettings,
    status: &BTreeMap<ObservationId, ObservationStatus>,
    text: &'static Text,
    now: i64,
) -> TableContent {
    TableContent {
        rows: observation_rows(settings, status, text, now),
        ..table_content(text)
    }
}

/// 每条观察一行,顺序就是 `settings.observations` 的顺序 —— 下面那排按钮
/// 靠"第几行"找回是哪一条,两边必须同序。
#[must_use]
pub fn observation_rows(
    settings: &AppSettings,
    status: &BTreeMap<ObservationId, ObservationStatus>,
    text: &'static Text,
    now: i64,
) -> Vec<Vec<Cell>> {
    settings
        .observations
        .iter()
        .map(|entry| {
            let live = status.get(&entry.id);
            vec![
                Cell::plain(entry.label.clone()),
                Cell::muted(league_cell_text(entry.game, &entry.league, text)),
                Cell::data(entry.sample_every.to_string()),
                count_cell(live.map(|status| status.active), text),
                count_cell(live.map(|status| status.gone), text),
                Cell::new(
                    status_text(entry, live, text, now),
                    status_tone(entry, live),
                ),
            ]
        })
        .collect()
}

/// 词缀那排按钮里的「做成蹲价」这次给不给。
///
/// 一键蹲价要把观察的搜索 id 解回查询原文(`decode_search_id`),而 PoE1 的 id
/// 是服务器发的一串号,里面没有查询 —— 那条路在 PoE1 上走不通,所以按钮干脆
/// 不出现,而不是点下去再报错。还没选中任何观察时照常显示。
#[must_use]
pub fn make_watch_offered(entry: Option<&ObservationEntry>) -> bool {
    entry.is_none_or(|entry| entry.game == Game::Poe2)
}

/// 计数那两格。"—" 只留给"还没有状态可说";已经在跑却一条都没记下来是个
/// 实实在在的 0。
fn count_cell(count: Option<u32>, text: &'static Text) -> Cell {
    match count {
        None => Cell::muted(text.common_none),
        Some(0) => Cell::muted("0"),
        Some(count) => Cell::data(count.to_string()),
    }
}

/// 状态那一格。
///
/// 停用的那条只写一个词:它既不会去找新货,也不会回查,更没有秒推连接 ——
/// 多写的每一段都是在描述一件没在发生的事。
fn status_text(
    entry: &ObservationEntry,
    status: Option<&ObservationStatus>,
    text: &'static Text,
    now: i64,
) -> String {
    if !entry.enabled {
        return text.status_disabled.to_owned();
    }
    let Some(status) = status else {
        // actor 还没广播过这一条(刚加进来,或者后台没起来)。
        return text.status_polling.to_owned();
    };
    let mut parts = vec![live_text(status.live, text, now)];
    // 秒推送来了多少条、其中多少条没去看。紧跟在 live 那一段后面:先说
    // 连没连上,再说它到底在不在送货 —— 只看"已连接"看不出这一点,而
    // 一条连着却一条都不送的 live 和一条没连上的,对用户是同一件坏事。
    // 一条都没推来时整段不写:那是"刚起来"的常态。
    if status.pushed_total > 0 {
        parts.push(i18n::fill(
            text.obs_pushed,
            &[&status.pushed_total.to_string()],
        ));
        parts.push(i18n::fill(
            text.obs_not_looked_at,
            &[&status.sampled_out_total.to_string()],
        ));
    }
    if let Some(at) = status.next_discover_at {
        parts.push(i18n::fill(
            text.obs_next_discover_in,
            &[&countdown_mmss(at - now)],
        ));
    }
    // 在册一条挂单都没有的时候没有"下一条到点的",这一段就不写 ——
    // 写个 00:00 会让人以为它卡住了。
    if let Some(at) = status.next_recheck_at {
        // 在册的挂单积压着的时候,最早到点的那一条永远在过去 —— 倒计时就
        // 永远写着 00:00,看起来像卡死了。到点了就直说"排队中":它确实在
        // 排队,只是队比一分钟长。
        parts.push(if at <= now {
            text.obs_recheck_queued.to_owned()
        } else {
            i18n::fill(text.obs_next_check_in, &[&countdown_mmss(at - now)])
        });
    }
    // 秒推的把手只活 14 秒:排队排过头就换不回挂单了。一条都没过期时不写
    // 这一段 —— 常态是 0,而写着"过期 0"只会让人以为这里有毛病。
    if status.expired_total > 0 {
        parts.push(i18n::fill(
            text.obs_expired,
            &[&status.expired_total.to_string()],
        ));
    }
    if let Some(error) = &status.last_error {
        parts.push(i18n::fill(text.obs_status_error, &[error]));
    }
    parts.join(" · ")
}

fn status_tone(entry: &ObservationEntry, status: Option<&ObservationStatus>) -> Tone {
    if !entry.enabled {
        return Tone::Muted;
    }
    match status {
        Some(status) if status.last_error.is_some() => Tone::Warn,
        Some(status) if matches!(status.live, LiveRunState::Connected { .. }) => Tone::Good,
        _ => Tone::Plain,
    }
}

/// 备注名:填了就听他的,留空就问搜索自己叫什么,再不行拿 id 前缀顶上。
fn label_for(typed: &str, search_id: &str) -> String {
    let typed = typed.trim();
    if !typed.is_empty() {
        return typed.to_owned();
    }
    decode_search_id(search_id)
        .ok()
        .and_then(|query| default_label_for(&query))
        .unwrap_or_else(|| search_id.chars().take(LABEL_FROM_ID_CHARS).collect())
}

// ---------------------------------------------------------------------
// 纯函数:词缀战绩
// ---------------------------------------------------------------------

/// 一个联赛里被收藏的那些词缀,`(mod_kind, template)` 一对。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FavouriteMods {
    keys: Vec<(String, String)>,
}

impl FavouriteMods {
    /// 设置里那份收藏清单,只留下这个联赛的那几条。
    #[must_use]
    pub fn for_league(settings: &AppSettings, league: &str) -> Self {
        let keys = settings
            .favourite_mods
            .iter()
            .filter(|entry| entry.league == league)
            .map(|entry| (entry.mod_kind.clone(), entry.template.clone()))
            .collect();
        Self { keys }
    }

    #[must_use]
    pub fn contains(&self, mod_kind: &str, template: &str) -> bool {
        self.keys
            .iter()
            .any(|(kind, saved)| kind == mod_kind && saved == template)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// 蹲价表单该被填成什么样。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchDraft {
    pub search_id: String,
    pub league: String,
    pub label: String,
    pub cap_milli: Option<i64>,
    pub currency: String,
}

/// 词缀表上选中的那一行 → 一份蹲价草稿。
///
/// 搜索是**观察自己那条查询再加一格词缀筛选**,不是从零拼一条:观察那条
/// 查询里还写着底子、物品等级、在线与否这些条件,丢掉它们就变成"全服所有
/// 带这条词缀的东西",一天能响几百次。
///
/// 上限取**成交价**中位,没有才退到在售价中位:在售价是"卖不掉的人开的价",
/// 拿它当蹲价上限等于永远在等一个没人接的价。两个都没有就留空 —— 凭空猜一个
/// 数出来,用户按下"新增"就是照着那个数在蹲。
#[must_use]
pub fn watch_draft(
    observation: &ObservationEntry,
    query_json: &str,
    outcome: &ModOutcome,
    stat_id: &str,
) -> WatchDraft {
    WatchDraft {
        search_id: encode_search_id(&with_stat_filter(query_json, stat_id)),
        league: observation.league.clone(),
        label: format!(
            "{} · {}",
            observation.label,
            short_template(&outcome.template)
        ),
        cap_milli: outcome
            .median_gone_price_milli
            .or(outcome.median_active_price_milli),
        currency: outcome.currency.clone(),
    }
}

/// 备注名里那半句词缀。超了就截断,末尾留一个省略号说明"还有下文"。
fn short_template(template: &str) -> String {
    if template.chars().count() <= DRAFT_TEMPLATE_CHARS {
        return template.to_owned();
    }
    let head: String = template.chars().take(DRAFT_TEMPLATE_CHARS - 1).collect();
    format!("{head}…")
}

/// 词缀类型下拉。选项从库里已有的行长出来 —— 写死一份的话,交易站哪天多一种
/// 词缀数组,那一档就永远筛不出来。
pub fn kind_choices(outcomes: &[ModOutcome], text: &'static Text) -> Vec<Choice> {
    let mut kinds: Vec<String> = outcomes.iter().map(|row| row.mod_kind.clone()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let mut items = vec![Choice::new("", text.common_all)];
    items.extend(
        kinds
            .into_iter()
            .map(|kind| Choice::new(kind.clone(), kind_label(&kind, text).to_owned())),
    );
    items
}

/// 库里存的是交易站的原文(`explicit`/`enchant`/…)。认不出来的原样显示:
/// 接口哪天多一种,下拉里出现一个英文词,好过那一档整个消失。
fn kind_label<'a>(kind: &'a str, text: &'static Text) -> &'a str {
    match kind {
        "explicit" => text.mods_kind_explicit,
        "implicit" => text.mods_kind_implicit,
        "crafted" => text.mods_kind_crafted,
        "desecrated" => text.mods_kind_desecrated,
        "rune" => text.mods_kind_rune,
        "enchant" => text.mods_kind_enchant,
        "fractured" => text.mods_kind_fractured,
        other => other,
    }
}

/// 筛一遍、排好序。
///
/// 排序按**卖掉几件**从多到少,而不是按见过几件:这一页要回答的是"什么样的
/// 货出得掉",一条见过 80 件、一件没卖掉的词缀不该压在榜首。见过的件数只是
/// 打平时的次序。
///
/// 收藏过的那几条整体提到最前:用户亲口认下"这条值钱"的词缀,常常恰恰是
/// 样本还薄、排在第二屏的那几条 —— 收藏了却还要每次翻下去找,等于没收藏。
/// 提上来之后它们内部仍按同一套次序排,和下面那半张表读起来是一样的。
#[must_use]
pub fn mod_filtered<'a>(
    outcomes: &'a [ModOutcome],
    kind: &str,
    min_samples: u32,
    favourites: &FavouriteMods,
    favourites_only: bool,
) -> Vec<&'a ModOutcome> {
    let mut rows: Vec<&ModOutcome> = outcomes
        .iter()
        .filter(|row| kind.is_empty() || row.mod_kind == kind)
        .filter(|row| row.seen >= min_samples)
        .filter(|row| !favourites_only || favourites.contains(&row.mod_kind, &row.template))
        .collect();
    rows.sort_by(|left, right| {
        // `false < true`,所以"是收藏"要写成 `!contains`:0 排在 1 前面。
        let pinned = |row: &ModOutcome| !favourites.contains(&row.mod_kind, &row.template);
        pinned(left)
            .cmp(&pinned(right))
            .then(right.sold_likely.cmp(&left.sold_likely))
            .then(right.seen.cmp(&left.seen))
            .then(left.template.cmp(&right.template))
            .then(left.mod_kind.cmp(&right.mod_kind))
    });
    rows
}

/// 词缀表的列。
pub fn mods_table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("template", text.obs_col_template, 300.),
            column("kind", text.obs_col_mod_kind, 90.),
            number_column("seen", text.obs_col_seen, 60.),
            number_column("gone", text.obs_col_gone, 60.),
            number_column("sold", text.obs_col_sold, 80.),
            number_column("rate", text.obs_col_sold_rate, 70.),
            number_column("sold_price", text.obs_col_sold_price, 110.),
            number_column("ask_price", text.obs_col_ask_price, 110.),
            number_column("life", text.obs_col_median_life, 90.),
        ],
        rows: Vec::new(),
        empty: text.obs_mods_empty.into(),
    }
}

/// 列 + 真行。
pub fn mods_table_content_for(
    outcomes: &[ModOutcome],
    kind: &str,
    min_samples: u32,
    favourites: &FavouriteMods,
    favourites_only: bool,
    text: &'static Text,
) -> TableContent {
    TableContent {
        rows: mod_rows(
            &mod_filtered(outcomes, kind, min_samples, favourites, favourites_only),
            favourites,
            text,
        ),
        ..mods_table_content(text)
    }
}

/// 一条词缀模板一行。
#[must_use]
pub fn mod_rows(
    rows: &[&ModOutcome],
    favourites: &FavouriteMods,
    text: &'static Text,
) -> Vec<Vec<Cell>> {
    rows.iter()
        .map(|row| {
            vec![
                Cell::plain(starred(
                    &row.template,
                    favourites.contains(&row.mod_kind, &row.template),
                )),
                Cell::muted(kind_label(&row.mod_kind, text).to_owned()),
                Cell::data(row.seen.to_string()),
                Cell::data(row.gone.to_string()),
                // 卖掉几件是这张表的主角,给它金色。
                if row.sold_likely == 0 {
                    Cell::muted("0")
                } else {
                    Cell::accent(row.sold_likely.to_string())
                },
                Cell::data(sold_rate_text(row.sold_likely, row.seen, text)),
                price_cell(row.median_gone_price_milli, &row.currency, text),
                price_cell(row.median_active_price_milli, &row.currency, text),
                match row.median_hours_alive {
                    Some(hours) => Cell::data(lifetime_text((hours * 3_600.0) as i64, text)),
                    None => Cell::muted(text.common_none),
                },
            ]
        })
        .collect()
}

/// 疑似成交率:分子是"看着像卖掉了"的件数,分母是见过的件数。
///
/// 分母用 `seen` 而不是 `gone`:还挂在那儿的货也是这条词缀的战绩的一部分 ——
/// "十件里卖掉两件"和"两件里卖掉两件"说的完全是两回事。
#[must_use]
pub fn sold_rate_text(sold: u32, seen: u32, text: &'static Text) -> String {
    if seen == 0 {
        return text.common_none.to_owned();
    }
    percent_text(f64::from(sold) * 100.0 / f64::from(seen), text)
}

/// 中位价那两格。价格一律是千分整数 + 货币码,界面这一层只管拼字。
fn price_cell(milli: Option<i64>, currency: &str, text: &'static Text) -> Cell {
    match milli {
        Some(milli) if !currency.is_empty() => {
            Cell::data(format!("{} {currency}", milli_text(milli)))
        }
        _ => Cell::muted(text.common_none),
    }
}

// ---------------------------------------------------------------------
// 纯函数:价位战绩
// ---------------------------------------------------------------------

/// 样本太薄的档藏起来。**顺序原样保留** —— 库那边已经排好了
/// (divine 在前,档位从低到高),而这张表是竖着读的:从便宜往贵看下去,
/// 成交率在哪一档掉下来,那里就是这条搜索的天花板。按成交率重排会把
/// 那条线打散。
#[must_use]
pub fn price_filtered(outcomes: &[PriceOutcome], min_samples: u32) -> Vec<&PriceOutcome> {
    outcomes
        .iter()
        .filter(|row| row.seen >= min_samples)
        .collect()
}

/// 这一栏的阶梯上最低的那根**正**横档(千分整数)。
///
/// 两条阶梯不一样低:按币种那条从 1 起步,折成 divine 那条从 0.1 起步
/// (原先标几十 chaos 的货折过来全在 1 以下,不分开就挤成一格)。最低那一档
/// 的写法要照着它写,不然会写出一句"不到 1 divine"而 0.5 divine 明明另有一档。
#[must_use]
pub fn bucket_floor_milli(mode: PriceMode) -> i64 {
    match mode {
        PriceMode::ByCurrency => 1_000,
        PriceMode::Converted => SUB_DIVINE_BUCKET_MILLI[0],
    }
}

/// 一档价位的写法:`2 divine`、`0.5 divine`。
///
/// 最低那一档单独一句「不到 0.1 divine」:它的下界是 0,而写成「0 divine」
/// 会读成"白送"。`floor_milli` 是这条阶梯上最低的那根正横档,见
/// [`bucket_floor_milli`]。
#[must_use]
pub fn price_bucket_label(
    bucket_milli: i64,
    floor_milli: i64,
    currency: &str,
    text: &'static Text,
) -> String {
    let bucket = PriceBucket {
        lower_bound_milli: bucket_milli,
    };
    if bucket.is_under_one() {
        return i18n::fill(text.obs_price_under, &[&milli_text(floor_milli), currency]);
    }
    format!("{} {currency}", milli_text(bucket_milli))
}

/// 价位表上头那句汇率。
///
/// 必须常驻:折算那一栏的每一个数都建在这份汇率上,而汇率是会过时的
/// (poe.ninja 那条线断了、手填的忘了改)。不写出来的话,一张按错汇率
/// 折出来的表和一张对的长得一模一样。
///
/// `unconverted` 是有价、但记下它那一刻换不出 divine 的挂单条数。它们在表上
/// 一行都看不见 —— 不说一声会让人以为总共就这么几件货。
#[must_use]
pub fn rates_status_line(
    rates: &CurrencyRates,
    sources: &RateSources,
    unconverted: u32,
    text: &'static Text,
) -> String {
    let mut line = match (rates.chaos_per_divine_milli, rates.exalted_per_divine_milli) {
        (None, None) => text.obs_rates_unknown.to_owned(),
        (chaos, exalted) => i18n::fill(
            text.obs_rates_line,
            &[
                &rate_number(chaos, text),
                &rate_number(exalted, text),
                rate_source_word(sources, text),
            ],
        ),
    };
    if unconverted > 0 {
        line.push_str(" · ");
        line.push_str(&i18n::fill(
            text.obs_unconverted,
            &[&unconverted.to_string()],
        ));
    }
    line
}

/// 汇率里的一个数。缺的那一档写破折号,不写 0 —— 0 会被读成"一个都不值"。
fn rate_number(milli: Option<i64>, text: &'static Text) -> String {
    milli.map_or_else(|| text.common_none.to_owned(), milli_text)
}

/// 这份汇率是从哪儿来的。两档来路不同就都说 —— "一半是我自己填的"
/// 恰恰是最该看见的那种情况。
fn rate_source_word(sources: &RateSources, text: &'static Text) -> &'static str {
    let manual = [sources.chaos, sources.exalted].contains(&RateSource::Manual);
    let ninja = [sources.chaos, sources.exalted].contains(&RateSource::Ninja);
    match (manual, ninja) {
        (true, true) => text.obs_rate_source_mixed,
        (true, false) => text.obs_rate_source_manual,
        (false, true) => text.obs_rate_source_ninja,
        (false, false) => text.obs_rate_source_unknown,
    }
}

/// 价位表的列。
///
/// 比词缀表少三列:词缀那边的两个中位价在这里没有意义(价位本身就是价),
/// 而"词缀类型"根本不适用。
pub fn price_table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("bucket", text.obs_col_price_bucket, 160.),
            number_column("seen", text.obs_col_seen, 60.),
            number_column("gone", text.obs_col_gone, 60.),
            number_column("sold", text.obs_col_sold, 80.),
            number_column("rate", text.obs_col_sold_rate, 70.),
            number_column("life", text.obs_col_median_life, 90.),
        ],
        rows: Vec::new(),
        empty: text.obs_price_empty.into(),
    }
}

/// 列 + 真行。
pub fn price_table_content_for(
    outcomes: &[PriceOutcome],
    min_samples: u32,
    mode: PriceMode,
    text: &'static Text,
) -> TableContent {
    TableContent {
        rows: price_rows(&price_filtered(outcomes, min_samples), mode, text),
        ..price_table_content(text)
    }
}

/// 一档价位一行。语气跟词缀表走,两张表并排看时才不像两个程序画的。
#[must_use]
pub fn price_rows(rows: &[&PriceOutcome], mode: PriceMode, text: &'static Text) -> Vec<Vec<Cell>> {
    let floor_milli = bucket_floor_milli(mode);
    rows.iter()
        .map(|row| {
            vec![
                Cell::plain(price_bucket_label(
                    row.bucket_milli,
                    floor_milli,
                    &row.currency,
                    text,
                )),
                Cell::data(row.seen.to_string()),
                Cell::data(row.gone.to_string()),
                // 卖掉几件是这张表的主角,给它金色(同词缀表)。
                if row.looks_sold == 0 {
                    Cell::muted("0")
                } else {
                    Cell::accent(row.looks_sold.to_string())
                },
                Cell::data(sold_rate_text(row.looks_sold, row.seen, text)),
                match row.median_lifetime_secs {
                    Some(secs) => Cell::data(lifetime_text(secs, text)),
                    None => Cell::muted(text.common_none),
                },
            ]
        })
        .collect()
}

// ---------------------------------------------------------------------
// 纯函数:挂单流
// ---------------------------------------------------------------------

/// 一条挂单的标题:物品名 + 现价。
///
/// 稀有物品的 `name` 是空的,`pnd-trade` 已经用 `typeLine` 顶上了;真的两样
/// 都没有(手写的老行)才退回挂单 id —— 一行空白看起来和"这一条坏了"一样。
#[must_use]
pub fn listing_title(row: &ObservedListingRow, text: &'static Text) -> String {
    let name = if row.item_name.trim().is_empty() {
        row.listing_id
            .chars()
            .take(LABEL_FROM_ID_CHARS)
            .collect::<String>()
    } else {
        row.item_name.clone()
    };
    let price = row
        .last_price
        .as_ref()
        .map_or_else(|| text.common_none.to_owned(), Price::display);
    format!("{name} · {price}")
}

/// 一条挂单的第二行。
///
/// 消失的那一栏先说**判成了什么**:整页的结论都建在那一档上,而它是可能判错的
/// (存活太短一律 `Unknown`),所以要能一眼看见每条货各自落在哪一档。
#[must_use]
pub fn listing_detail(
    entry: &StreamEntry,
    tab: StreamTab,
    text: &'static Text,
    now: i64,
) -> String {
    let row = &entry.row;
    let mut parts = Vec::new();
    match tab {
        StreamTab::Gone => {
            parts.push(gone_class_label(row.gone_class, text).to_owned());
            parts.push(i18n::fill(
                text.obs_stream_lifetime,
                &[&lifetime_text(row.observed_lifetime_secs(), text)],
            ));
        }
        StreamTab::Active => {
            parts.push(i18n::fill(
                text.obs_stream_alive_for,
                &[&lifetime_text(now - row.first_seen_at, text)],
            ));
            parts.push(i18n::fill(
                text.obs_stream_first_seen,
                &[&local_stamp(row.first_seen_at)],
            ));
        }
    }
    if row.price_changes > 0 {
        parts.push(i18n::fill(
            text.obs_stream_price_changes,
            &[&row.price_changes.to_string()],
        ));
    }
    parts.join(" · ")
}

/// 四档判定各自的写法。`None`(库里那一列是空的)当"看不出来"。
#[must_use]
pub fn gone_class_label(class: Option<GoneClass>, text: &'static Text) -> &'static str {
    match class {
        Some(GoneClass::SoldLikely) => text.obs_gone_sold_likely,
        Some(GoneClass::SoldAfterCuts) => text.obs_gone_sold_after_cuts,
        Some(GoneClass::GoneBeforeFirstLook) => text.obs_gone_first_look,
        Some(GoneClass::Unknown) | None => text.obs_gone_unknown,
    }
}

/// 挂单身上那几行词缀,超了就截断。
///
/// 收藏过的那几条带星:聚合表是结论,这一栏是撑起结论的证据 —— 翻证据的
/// 时候第一眼要找的就是"这件货身上有没有我认下的那条词缀"。
#[must_use]
pub fn listing_mod_lines(mods: &[ObservedMod], favourites: &FavouriteMods) -> Vec<String> {
    mods.iter()
        .take(STREAM_MOD_LINES)
        .map(|entry| {
            starred(
                &entry.template,
                favourites.contains(&entry.mod_kind, &entry.template),
            )
        })
        .collect()
}

/// 收藏过的词缀在屏幕上前面挂一颗星。
///
/// 为什么要标:置顶之后那几行凭什么在上面,只有这颗星说得清 —— 不然看起来
/// 像"这条卖得最好",而它其实只是被收藏了。
fn starred(template: &str, favourite: bool) -> String {
    if favourite {
        return format!("{FAVOURITE_MARK}{template}");
    }
    template.to_owned()
}

// ---------------------------------------------------------------------
// 画面
// ---------------------------------------------------------------------

impl AppShell {
    pub(crate) fn render_observations(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let has_observations = !self.settings.observations.is_empty();
        // 没有会话 = 秒推连不上,而观察最想量的恰恰是"一分钟内卖掉的那批货"。
        // 这一句必须常驻:不解释的话,秒掉的货一条都记不下来,而表上看不出
        // 少了什么。
        let no_session = self.settings.poesessid.trim().is_empty();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .child(page_heading(text.obs_heading, text.obs_subtitle))
            .child(self.observations_add_form(cx))
            .child(
                panel()
                    .flex_none()
                    .h(px(150.))
                    .overflow_hidden()
                    .child(table(&self.observations_table)),
            )
            .children((!has_observations).then(|| hint(text.obs_empty_hint)))
            .children(no_session.then(|| hint(text.obs_no_live_session)))
            .child(self.observations_row_actions(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    // 聚合表和它那排按钮竖着摞在一起:按钮作用在表里选中的
                    // 那一行,分开摆就看不出它们是一伙的。
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(self.observations_aggregate_panel(cx))
                            .child(self.observations_mod_actions(cx)),
                    )
                    .child(self.observations_stream_panel(cx)),
            )
    }

    /// 词缀表里选中一行之后能对它做的两件事:收藏,或者把它做成一条蹲价。
    ///
    /// 为什么不做成表格里的按钮:上游的表格不支持在格子里放控件(同蹲价页
    /// 那排按钮的理由)。
    fn observations_mod_actions(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let selected = self.selected_mod(cx);
        let favourites = self.observation_favourites();
        let is_favourite = selected
            .as_ref()
            .is_some_and(|row| favourites.contains(&row.mod_kind, &row.template));
        let label = selected.as_ref().map_or_else(
            || text.common_select_row.to_owned(),
            |row| row.template.clone(),
        );
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
                    .child(text.obs_mod_actions),
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
                Button::new("obs-favourite")
                    .label(if is_favourite {
                        text.obs_unfavourite
                    } else {
                        text.obs_favourite
                    })
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_selected_mod_favourite(cx);
                    })),
            )
            .children(
                make_watch_offered(
                    self.observe
                        .selected
                        .as_ref()
                        .and_then(|id| self.settings.observation(id)),
                )
                .then(|| {
                    Button::new("obs-make-watch")
                        .label(text.obs_make_watch)
                        .with_size(Size::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.make_watch_from_selected_mod(window, cx);
                        }))
                }),
            )
    }

    /// 表单:新增一条观察,或者改选中的那一条。两种模式共用同一组框,
    /// 差别只有标题那一行、搜索框灰不灰、右下角那个按钮写什么(同蹲价页)。
    fn observations_add_form(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let enabled = self.observations_form.enabled;
        let error = self.obs_error.clone();
        let editing = self.observations_form.editing.clone();
        let editing_label = editing
            .as_ref()
            .and_then(|id| self.settings.observation(id))
            .map(|entry| entry.label.clone());
        let is_editing = editing.is_some();
        panel()
            .flex_none()
            .p(px(10.))
            .gap(px(8.))
            .children(editing_label.map(|label| {
                div()
                    .text_size(fs(FS_11_5))
                    .text_color(c(ACCENT_TEXT))
                    .child(SharedString::from(i18n::fill(text.obs_editing, &[&label])))
            }))
            .child(
                field_row()
                    .child(field_label(text.obs_add_search_label))
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            Input::new(&self.observations_form.search)
                                .with_size(Size::Small)
                                .disabled(is_editing),
                        ),
                    ),
            )
            .child(
                field_row()
                    .child(field_label(text.obs_label_label))
                    .child(
                        div().w(px(200.)).flex_none().child(
                            Input::new(&self.observations_form.label).with_size(Size::Small),
                        ),
                    )
                    .child(
                        div()
                            .text_size(fs(FS_11_5))
                            .text_color(muted())
                            .child(text.obs_sample_every_label),
                    )
                    .child(div().w(px(70.)).flex_none().child(
                        Input::new(&self.observations_form.sample_every).with_size(Size::Small),
                    ))
                    .child(hint(text.obs_sample_every_hint)),
            )
            .child(
                field_row()
                    .child(field_label(""))
                    .child(
                        Switch::new("obs-enabled")
                            .checked(enabled)
                            .label(SharedString::from(text.obs_enable_toggle))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.observations_form.enabled = *checked;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_grow())
                    .children(is_editing.then(|| {
                        Button::new("obs-cancel-edit")
                            .label(text.obs_cancel_edit)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_observation_edit(window, cx);
                            }))
                    }))
                    .child(if is_editing {
                        Button::new("obs-save")
                            .primary()
                            .label(text.obs_save_changes)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_observation_edit(window, cx);
                            }))
                    } else {
                        Button::new("obs-add")
                            .primary()
                            .label(text.obs_add_button)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_observation(window, cx);
                            }))
                    }),
            )
            .children((!error.is_empty()).then(|| {
                div()
                    .text_size(fs(FS_11))
                    .text_color(c(DANGER_TEXT))
                    .child(SharedString::from(error))
            }))
            .children(is_editing.then(|| hint(text.obs_edit_search_locked)))
    }

    /// 选中一行之后能对它做的三件事。
    ///
    /// 删除要按两下:一条观察攒的全部结论就在那几张表里,删掉不留任何东西
    /// ([`pnd_storage::WatchStore::delete_observation`]),手滑一下没法撤销。
    fn observations_row_actions(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let selected = self.selected_observation(cx);
        let label = selected.map_or_else(
            || text.common_select_row.to_owned(),
            |entry| entry.label.clone(),
        );
        let enabled = selected.is_some_and(|entry| entry.enabled);
        let armed = self.obs_remove_armed;
        panel()
            .flex_none()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(8.))
            .child(
                div()
                    .text_size(fs(FS_11_5))
                    .text_color(muted())
                    .child(text.obs_row_actions),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(fs(FS_11_5))
                    .text_color(c(TEXT_SECONDARY))
                    .child(SharedString::from(label)),
            )
            // 启停一下就生效,不用走"保存修改" —— 同蹲价页那个开关。
            .child(
                Switch::new("obs-row-enabled")
                    .checked(enabled)
                    .label(SharedString::from(text.obs_enable_toggle))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.update_selected_observation(cx, |entry| entry.enabled = checked);
                    })),
            )
            .child(
                Button::new("obs-discover-now")
                    .label(text.obs_discover_now)
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.discover_selected_observation(cx);
                    })),
            )
            .child(
                Button::new("obs-recheck-now")
                    .label(text.obs_recheck_now)
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.recheck_selected_observation(cx);
                    })),
            )
            .child(
                Button::new("obs-remove")
                    .danger()
                    .label(if armed {
                        text.obs_remove_confirm
                    } else {
                        text.obs_remove
                    })
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.remove_selected_observation(window, cx);
                    })),
            )
    }

    /// 左下:聚合表。两栏 —— 词缀战绩和价位战绩。
    fn observations_aggregate_panel(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let min_samples = self.obs_min_samples;
        let favourites_only = self.obs_favourites_only;
        let tab = self.obs_agg_tab;
        let price_mode = self.obs_price_mode;
        let selected = self.observe.selected.is_some();
        let no_data = self.observe.is_empty();
        // 折算那一栏的每个数都建在这份汇率上,所以它必须写在表的正上方:
        // 按错汇率折出来的表和对的长得一模一样。
        // "未折算"那一段只在折算那一栏说:按币种分的时候一条都没被漏下,
        // 说它反而像是这张表也藏了东西。
        let unconverted = match price_mode {
            PriceMode::Converted => self.observe.unconverted,
            PriceMode::ByCurrency => 0,
        };
        let rates_line = rates_status_line(&self.rates, &self.rate_sources, unconverted, text);
        panel()
            .flex_1()
            .min_w(px(0.))
            // 它现在和下面那排按钮竖着摞在一起:不写这一句,flex 子项的默认
            // 最小高度是它的内容,长表格会把按钮那一条顶出屏幕。
            .min_h(px(0.))
            .child(
                div()
                    .flex_none()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(c(HAIRLINE))
                    .child(
                        Button::new("obs-agg-mods")
                            .ghost()
                            .xsmall()
                            .selected(tab == AggregateTab::Mods)
                            .label(text.obs_mods_tab)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.obs_agg_tab = AggregateTab::Mods;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("obs-agg-price")
                            .ghost()
                            .xsmall()
                            .selected(tab == AggregateTab::Price)
                            .label(text.obs_price_tab)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.obs_agg_tab = AggregateTab::Price;
                                cx.notify();
                            })),
                    )
                    // 词缀类型只筛得动词缀那张表。价位表上留着它就是个按了
                    // 不动的控件 —— 那比它不在更让人以为程序坏了。
                    .children(
                        (tab == AggregateTab::Mods)
                            .then(|| picker(text.mods_kind_label, &self.obs_kind_select, 140.)),
                    )
                    // 价位表专属的两档开关:按币种,还是全部折成 divine。
                    // 默认折算 —— 按币种那一栏答不了"这批货值多少 divine 卖得掉"。
                    .children((tab == AggregateTab::Price).then(|| {
                        Button::new("obs-price-by-currency")
                            .ghost()
                            .xsmall()
                            .selected(price_mode == PriceMode::ByCurrency)
                            .label(text.obs_price_by_currency)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.obs_price_mode = PriceMode::ByCurrency;
                                this.obs_price_dirty = true;
                                cx.notify();
                            }))
                    }))
                    .children((tab == AggregateTab::Price).then(|| {
                        Button::new("obs-price-in-divine")
                            .ghost()
                            .xsmall()
                            .selected(price_mode == PriceMode::Converted)
                            .label(text.obs_price_in_divine)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.obs_price_mode = PriceMode::Converted;
                                this.obs_price_dirty = true;
                                cx.notify();
                            }))
                    }))
                    .child(div().flex_grow())
                    .child(
                        div()
                            .text_size(fs(FS_11))
                            .text_color(muted())
                            .child(text.obs_min_samples_label),
                    )
                    // "只看收藏"只筛得动词缀那张表(价位档没有"收藏"这回事),
                    // 所以它和类型下拉一样跟着栏走。
                    .children((tab == AggregateTab::Mods).then(|| {
                        Button::new("obs-favourites-only")
                            .ghost()
                            .xsmall()
                            .selected(favourites_only)
                            .label(text.obs_favourites_only)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.obs_favourites_only = !this.obs_favourites_only;
                                this.obs_mods_dirty = true;
                                cx.notify();
                            }))
                    }))
                    .children(MIN_SAMPLE_PRESETS.into_iter().map(|preset| {
                        Button::new(("obs-min-samples", preset as usize))
                            .ghost()
                            .xsmall()
                            .selected(preset == min_samples)
                            .label(SharedString::from(preset.to_string()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.obs_min_samples = preset;
                                // 两张表都按这个门槛筛,所以两张都得重排。
                                this.obs_mods_dirty = true;
                                this.obs_price_dirty = true;
                                cx.notify();
                            }))
                    })),
            )
            // 汇率那一行只在价位表上出现:词缀表的中位价也折算过,但那张表
            // 一行只有一个数,而这一行是整栏的前提。
            .children((tab == AggregateTab::Price).then(|| {
                div()
                    .flex_none()
                    .px(px(10.))
                    .py(px(4.))
                    .border_b_1()
                    .border_color(c(HAIRLINE_SOFT))
                    .text_size(fs(FS_11))
                    .text_color(muted())
                    .child(SharedString::from(rates_line))
            }))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(match tab {
                        AggregateTab::Mods => table(&self.obs_mods_table),
                        AggregateTab::Price => table(&self.obs_price_table),
                    }),
            )
            // 头几个小时表上一行都没有是正常的:判定要么等挂单消失,要么等它
            // 挂满 72 小时。不说的话看起来就像程序没在跑。
            .children((selected && no_data).then(|| {
                div()
                    .flex_none()
                    .px(px(10.))
                    .py(px(6.))
                    .child(hint(text.obs_no_data))
            }))
            .children((!selected).then(|| {
                div()
                    .flex_none()
                    .px(px(10.))
                    .py(px(6.))
                    .child(hint(text.obs_select_observation))
            }))
    }

    /// 右下:挂单流。聚合表是结论,这一栏是撑起结论的那几件货。
    fn observations_stream_panel(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let tab = self.obs_stream_tab;
        let now = now_secs();
        let favourites = self.observation_favourites();
        let entries: Vec<gpui::Div> = self
            .observe
            .stream(tab)
            .iter()
            .map(|entry| stream_row(entry, tab, &favourites, text, now))
            .collect();
        let empty = entries.is_empty();
        let first_look = self.observe.summary.gone_before_first_look;
        panel()
            .w(px(360.))
            .flex_none()
            .child(
                div()
                    .flex_none()
                    .h_flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(8.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(c(HAIRLINE))
                    .child(
                        Button::new("obs-stream-gone")
                            .ghost()
                            .xsmall()
                            .selected(tab == StreamTab::Gone)
                            .label(text.obs_stream_gone_tab)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.obs_stream_tab = StreamTab::Gone;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("obs-stream-active")
                            .ghost()
                            .xsmall()
                            .selected(tab == StreamTab::Active)
                            .label(text.obs_stream_active_tab)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.obs_stream_tab = StreamTab::Active;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("obs-stream")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.obs_stream_scroll)
                    .flex()
                    .flex_col()
                    .children(empty.then(|| {
                        div()
                            .px(px(8.))
                            .py(px(10.))
                            .text_size(fs(FS_11_5))
                            .text_color(muted())
                            .child(text.obs_stream_empty)
                    }))
                    .children(entries),
            )
            // "推来时已经卖掉了"的那一批必须单独说:它们是秒掉的好价,
            // 一条词缀都没拿到,却恰恰是这条搜索里最值钱的信息。
            .children((first_look > 0).then(|| {
                div()
                    .flex_none()
                    .px(px(8.))
                    .py(px(6.))
                    .border_t_1()
                    .border_color(c(HAIRLINE))
                    .child(hint(i18n::fill(
                        text.obs_gone_before_first_look,
                        &[&first_look.to_string()],
                    )))
            }))
    }

    // ---- 同步(要 `&mut Window`,只能在 render 里做)--------------------

    /// 表格里选中了一行 → 装进表单,并把下面两块切到那一条观察上。
    pub(crate) fn sync_observation_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.obs_filters_dirty {
            self.obs_filters_dirty = false;
            let items = kind_choices(&self.observe.mods, self.text());
            // `None` = 留着用户现在筛的那一档:换语言不该把它清回"全部"。
            crate::shell::relabel_select(&self.obs_kind_select.clone(), items, None, window, cx);
        }
        let Some(row) = self.observations_form_load.take() else {
            return;
        };
        let Some(entry) = self.settings.observations.get(row).cloned() else {
            return;
        };
        self.obs_error.clear();
        // 换了一条观察,上一条那半下删除就不算数了 —— 否则第二下会删到别人。
        self.obs_remove_armed = false;
        self.observations_form.load(&entry, window, cx);
        if self.observe.selected.as_ref() != Some(&entry.id) {
            self.observe.selected = Some(entry.id);
            self.reload_observation();
        }
    }

    // ---- 动作 --------------------------------------------------------

    /// 粘进来的东西 → 一条新观察。
    fn add_observation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text();
        let raw = input_text(&self.observations_form.search, cx);
        let league = self.settings.league.clone();
        let Some(mut search_ref) = parse_search_reference(&raw, &league) else {
            self.obs_error = text.obs_invalid_search.to_owned();
            cx.notify();
            return;
        };
        search_ref.league = league;
        let Some(sample_every) = self.form_sample_every(cx) else {
            self.obs_error = text.obs_invalid_sample_every.to_owned();
            cx.notify();
            return;
        };

        let label = label_for(
            &input_text(&self.observations_form.label, cx),
            &search_ref.search_id,
        );
        let mut entry = ObservationEntry::new(label, &search_ref);
        entry.enabled = self.observations_form.enabled;
        entry.sample_every = sample_every;
        self.push_log(format!(
            "observation added: {} ({})",
            entry.label, entry.league
        ));
        // 加完就把下面两块切到它:新加的那条是用户此刻正在看的东西。
        self.observe.selected = Some(entry.id.clone());
        self.settings.observations.push(entry);

        self.obs_error.clear();
        if self.save_and_apply() {
            self.set_notice(text.obs_added.to_owned());
            self.observations_form.clear(window, cx);
        }
        self.reload_observation();
        self.observations_dirty = true;
        cx.notify();
    }

    /// 把表单里的改动写回**同一条**观察。`ObservationId` 一个字都不动:
    /// 库里那几百条挂单全挂在它上面。
    fn save_observation_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(id) = self.observations_form.editing.clone() else {
            return;
        };
        let Some(index) = self
            .settings
            .observations
            .iter()
            .position(|entry| entry.id == id)
        else {
            // 改到一半这条被删了。退回新增模式,而不是把它又写回去。
            self.observations_form.clear(window, cx);
            cx.notify();
            return;
        };
        let Some(sample_every) = self.form_sample_every(cx) else {
            self.obs_error = text.obs_invalid_sample_every.to_owned();
            cx.notify();
            return;
        };
        let search_id = self.settings.observations[index].search_id.clone();
        let label = label_for(&input_text(&self.observations_form.label, cx), &search_id);
        let enabled = self.observations_form.enabled;

        let entry = &mut self.settings.observations[index];
        entry.label = label;
        entry.enabled = enabled;
        entry.sample_every = sample_every;
        let (label, league) = (entry.label.clone(), entry.league.clone());
        self.push_log(format!("observation updated: {label} ({league})"));

        self.obs_error.clear();
        if self.save_and_apply() {
            self.set_notice(text.obs_changes_saved.to_owned());
            self.observations_form.clear(window, cx);
        }
        self.observations_dirty = true;
        cx.notify();
    }

    fn cancel_observation_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.obs_error.clear();
        self.observations_form.clear(window, cx);
        cx.notify();
    }

    /// 抽样比例那一格。填的不是个不小于 1 的整数就返回 `None`;留空当 1。
    fn form_sample_every(&self, cx: &Context<Self>) -> Option<u32> {
        let raw = input_text(&self.observations_form.sample_every, cx);
        if raw.is_empty() {
            return Some(1);
        }
        raw.parse::<u32>().ok().filter(|every| *every >= 1)
    }

    /// 表格里选中的那条观察。
    fn selected_observation(&self, cx: &mut Context<Self>) -> Option<&ObservationEntry> {
        let row = self.selected_observation_index(cx)?;
        self.settings.observations.get(row)
    }

    fn selected_observation_index(&self, cx: &mut Context<Self>) -> Option<usize> {
        let row = self.observations_table.read(cx).selected_row()?;
        (row < self.settings.observations.len()).then_some(row)
    }

    /// 改选中那条的一个开关,存盘,推给 actor。
    fn update_selected_observation(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut ObservationEntry),
    ) {
        let Some(index) = self.selected_observation_index(cx) else {
            self.select_an_observation_first(cx);
            return;
        };
        change(&mut self.settings.observations[index]);
        self.save_and_apply();
        // 表单里那个开关跟着走 —— 两处显示同一件事,不同步就成了两件事。
        if self.observations_form.editing.as_ref() == Some(&self.settings.observations[index].id) {
            self.observations_form.enabled = self.settings.observations[index].enabled;
        }
        self.observations_dirty = true;
        cx.notify();
    }

    fn discover_selected_observation(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(obs_id) = self.selected_observation(cx).map(|entry| entry.id.clone()) else {
            self.select_an_observation_first(cx);
            return;
        };
        if self.send_runtime(RuntimeCommand::DiscoverNow { obs_id }) {
            self.set_notice(text.obs_discover_requested.to_owned());
        }
        cx.notify();
    }

    fn recheck_selected_observation(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(obs_id) = self.selected_observation(cx).map(|entry| entry.id.clone()) else {
            self.select_an_observation_first(cx);
            return;
        };
        if self.send_runtime(RuntimeCommand::RecheckNow { obs_id }) {
            self.set_notice(text.obs_recheck_requested.to_owned());
        }
        cx.notify();
    }

    /// 删掉选中那条观察 —— 第二下才真删。
    ///
    /// 设置里那一条和库里那几张表一起删:留一半没有意义,观察攒的全部价值就是
    /// 那些行,而设置里少了这条之后没有任何入口能再看到它们。
    fn remove_selected_observation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(index) = self.selected_observation_index(cx) else {
            self.select_an_observation_first(cx);
            return;
        };
        if !self.obs_remove_armed {
            self.obs_remove_armed = true;
            self.set_notice(text.obs_remove_confirm.to_owned());
            cx.notify();
            return;
        }
        self.obs_remove_armed = false;
        let removed = self.settings.observations.remove(index);
        self.push_log(format!("observation removed: {}", removed.label));
        self.observation_status.remove(&removed.id);
        if self.observations_form.editing.as_ref() == Some(&removed.id) {
            self.observations_form.clear(window, cx);
        }
        if let Some(store) = &self.alerts_store
            && let Err(error) = store.delete_observation(&removed.id)
        {
            self.push_log(format!("could not delete the observation data: {error}"));
        }
        if self.observe.selected.as_ref() == Some(&removed.id) {
            self.observe.selected = None;
        }
        if self.save_and_apply() {
            self.set_notice(text.obs_removed.to_owned());
        }
        self.reload_observation();
        self.observations_dirty = true;
        cx.notify();
    }

    /// 现在这张词缀表该按哪个联赛的收藏来标星。
    ///
    /// 跟着**选中那条观察**的联赛,不是设置页那个当前联赛:翻看上赛季的
    /// 观察时,该亮的是上赛季认下的那几条。
    pub(crate) fn observation_favourites(&self) -> FavouriteMods {
        FavouriteMods::for_league(&self.settings, &self.observation_league())
    }

    /// 选中那条观察是哪个联赛的。一条都没选中(还没加过观察)时退回设置页
    /// 那个当前联赛 —— 收藏总得记在某个联赛名下。
    fn observation_league(&self) -> String {
        self.observe
            .selected
            .as_ref()
            .and_then(|id| self.settings.observation(id))
            .map_or_else(
                || self.settings.league.clone(),
                |entry| entry.league.clone(),
            )
    }

    /// 词缀表里选中的那一行。
    ///
    /// 表上画的是筛过、排过的那一份,所以这里要用同一套参数再算一遍 ——
    /// 拿行号去 `self.observe.mods`(库里那份原始顺序)里取,选中的和
    /// 按钮作用的就是两条不同的词缀。
    fn selected_mod(&self, cx: &mut Context<Self>) -> Option<ModOutcome> {
        let row = self.obs_mods_table.read(cx).selected_row()?;
        let kind = crate::shell::selected_value(&self.obs_kind_select, cx);
        let favourites = self.observation_favourites();
        mod_filtered(
            &self.observe.mods,
            &kind,
            self.obs_min_samples,
            &favourites,
            self.obs_favourites_only,
        )
        .get(row)
        .map(|row| (*row).clone())
    }

    /// 收藏 / 取消收藏选中那条词缀。
    fn toggle_selected_mod_favourite(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_mod(cx) else {
            self.select_an_observation_first(cx);
            return;
        };
        let league = self.observation_league();
        let now_favourite = self
            .settings
            .toggle_favourite(&league, &row.mod_kind, &row.template);
        self.push_log(format!(
            "modifier {} favourite: {} ({league})",
            if now_favourite {
                "added to"
            } else {
                "removed from"
            },
            row.template
        ));
        self.save_and_apply();
        // 置顶顺序和星号都变了,挂单流那一栏也跟着重画。
        self.obs_mods_dirty = true;
        // 刚收藏的那一行会跳到表头去,而"选中的是第几行"是个行号 ——
        // 不让它跟着走的话,下一下按钮作用的就是另一条词缀了。
        self.reselect_mod_row(&row, cx);
        cx.notify();
    }

    /// 重排之后,把选中标记挪回原来那条词缀身上。
    fn reselect_mod_row(&mut self, row: &ModOutcome, cx: &mut Context<Self>) {
        let kind = crate::shell::selected_value(&self.obs_kind_select, cx);
        let favourites = self.observation_favourites();
        let moved_to = mod_filtered(
            &self.observe.mods,
            &kind,
            self.obs_min_samples,
            &favourites,
            self.obs_favourites_only,
        )
        .iter()
        .position(|candidate| {
            candidate.mod_kind == row.mod_kind && candidate.template == row.template
        });
        // 找不着了("只看收藏"开着,而这一下正是取消收藏):选中标记留在原处,
        // 反正那一行已经不在表上了。
        let Some(index) = moved_to else {
            return;
        };
        self.obs_mods_table.clone().update(cx, |state, cx| {
            state.set_selected_row(index, cx);
        });
    }

    /// 一键蹲价:把选中那条词缀做成一份蹲价草稿,翻到蹲价页填进表单。
    ///
    /// **只填,不加**:按下"新增"的永远是用户自己 —— 一条自动加进去的搜索
    /// 会立刻开始花限速预算,而它是不是用户要的还没人确认过。
    fn make_watch_from_selected_mod(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text();
        let (Some(row), Some(obs_id)) = (self.selected_mod(cx), self.observe.selected.clone())
        else {
            self.select_an_observation_first(cx);
            return;
        };
        let Some(entry) = self.settings.observation(&obs_id).cloned() else {
            return;
        };
        // 词缀筛选认的是 stat id,而库里只有模板 —— id 得回物品原文里翻。
        let stat_id = self.alerts_store.as_ref().and_then(|store| {
            store
                .stat_id_for_template(&obs_id, &row.mod_kind, &row.template)
                .unwrap_or_default()
        });
        let Some(stat_id) = stat_id else {
            self.set_notice(text.obs_no_stat_id.to_owned());
            cx.notify();
            return;
        };
        // 查询原文优先用库里存的那份(actor 每轮都写);它还没写过(刚加的
        // 观察)就当场从搜索 id 解一份出来 —— 两条路解出来的是同一段。
        let query_json = self
            .alerts_store
            .as_ref()
            .and_then(|store| store.observation_state(&obs_id).ok().flatten())
            .and_then(|run| run.query_json)
            .or_else(|| decode_search_id(&entry.search_id).ok());
        let Some(query_json) = query_json else {
            self.set_notice(text.obs_invalid_search.to_owned());
            cx.notify();
            return;
        };

        let draft = watch_draft(&entry, &query_json, &row, &stat_id);
        self.push_log(format!("watch draft from modifier: {}", draft.label));
        self.prefill_add_form(draft, window, cx);
        self.show_page(crate::shell::Page::Watches);
        self.set_notice(text.watches_prefilled.to_owned());
        cx.notify();
    }

    fn select_an_observation_first(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        self.set_notice(text.common_select_row.to_owned());
        cx.notify();
    }

    // ---- 读库 --------------------------------------------------------

    /// 把选中那条观察的账、聚合表和两栏挂单流整份读进内存。
    ///
    /// 一次读齐,理由同 ninja 那两页:换筛选器、换栏、换语言都不该再打一次库。
    /// 库读不动只记一行日志 —— 少几行数据,比整页炸掉好。
    pub(crate) fn reload_observation(&mut self) {
        // 选中的那条没了(删掉了、设置被手改过)就退回第一条,而不是画一份
        // 属于别人的数据。
        let still_there = self
            .observe
            .selected
            .as_ref()
            .is_some_and(|id| self.settings.observation(id).is_some());
        if !still_there {
            self.observe.selected = self
                .settings
                .observations
                .first()
                .map(|entry| entry.id.clone());
        }
        // 先清空:读不动的时候屏幕上该是"什么都没有",而不是上一条观察的数据。
        let selected = self.observe.selected.clone();
        self.observe = ObservationData {
            selected: selected.clone(),
            ..ObservationData::default()
        };
        self.obs_mods_dirty = true;
        self.obs_price_dirty = true;
        self.obs_filters_dirty = true;
        let (Some(store), Some(obs_id)) = (&self.alerts_store, selected) else {
            return;
        };
        // 样本数的门槛在界面这一侧筛(见 `mod_filtered` / `price_filtered`):
        // 按一下预设按钮就回库里重查一遍不值当,而这一份行数是几百条量级。
        let read: Result<_, StorageError> = (|| {
            Ok((
                store.observation_summary(&obs_id)?,
                store.mod_aggregate(&obs_id, 1)?,
                // 两种口径一次读齐:那个两档开关按一下就该换表,而为了换一栏
                // 再打一次库不值当 —— 这两份一起变、一起过时。
                store.price_aggregate(&obs_id, PriceMode::ByCurrency)?,
                store.price_aggregate(&obs_id, PriceMode::Converted)?,
                store.recent_gone(&obs_id, STREAM_ROWS)?,
                store.oldest_active(&obs_id, STREAM_ROWS)?,
            ))
        })();
        match read {
            Ok((summary, mods, by_currency, converted, gone, active)) => {
                self.observe.summary = summary;
                self.observe.mods = mods;
                self.observe.prices_by_currency = by_currency.rows;
                self.observe.prices_converted = converted.rows;
                self.observe.unconverted = converted.unconverted;
                self.observe.gone = self.stream_entries(&obs_id, gone);
                self.observe.active = self.stream_entries(&obs_id, active);
            }
            Err(error) => self.push_log(format!("could not read the observation data: {error}")),
        }
    }

    /// 每条挂单再把它身上的词缀读出来。行数是 [`STREAM_ROWS`] 封顶的,
    /// 所以这一串小查询加起来还是一次翻页的量。
    fn stream_entries(
        &self,
        obs_id: &ObservationId,
        rows: Vec<ObservedListingRow>,
    ) -> Vec<StreamEntry> {
        let Some(store) = &self.alerts_store else {
            return Vec::new();
        };
        rows.into_iter()
            .map(|row| {
                let mods = store
                    .observed_mods(obs_id, &row.listing_id)
                    .unwrap_or_default();
                StreamEntry { row, mods }
            })
            .collect()
    }
}

/// 挂单流里的一条。
fn stream_row(
    entry: &StreamEntry,
    tab: StreamTab,
    favourites: &FavouriteMods,
    text: &'static Text,
    now: i64,
) -> gpui::Div {
    let lines = listing_mod_lines(&entry.mods, favourites);
    div()
        .flex()
        .flex_col()
        .gap(px(1.))
        .px(px(8.))
        .py(px(5.))
        .border_b_1()
        .border_color(c(HAIRLINE_SOFT))
        .child(
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(fs(FS_11_5))
                .text_color(c(TEXT_PRIMARY))
                .child(SharedString::from(listing_title(&entry.row, text))),
        )
        .child(
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(fs(FS_11))
                .text_color(if entry.row.gone_class.is_some_and(GoneClass::looks_sold) {
                    hit_green()
                } else {
                    muted()
                })
                .child(SharedString::from(listing_detail(entry, tab, text, now))),
        )
        .children(lines.into_iter().map(|line| {
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(fs(FS_10_5))
                .text_color(c(TEXT_META))
                .child(SharedString::from(line))
        }))
}

/// 框里的字。
fn input_text(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_string()
}

#[cfg(test)]
mod observations_page_tests {
    use gpui_component::select::SelectItem as _;
    use pnd_domain::Currency;
    use pnd_storage::{ObservedListingRow, ObservedStatus, PriceOutcome};

    use super::*;
    use crate::i18n;

    const HOUR: i64 = 3_600;
    const NOW: i64 = 1_000_000;

    /// 网页上真抄下来的那条查询(Choir of the Storm),一键蹲价拿它当底子。
    const FIXTURE_QUERY: &str = r#"{"status":{"option":"online"},"name":"Choir of the Storm","stats":[{"type":"and","filters":[]}]}"#;

    /// 一份收藏清单,联赛统一是观察那一条的("Forbidden Rites")。
    fn favourites(pairs: &[(&str, &str)]) -> FavouriteMods {
        let mut settings = AppSettings::default();
        for (kind, template) in pairs {
            settings.toggle_favourite("Forbidden Rites", kind, template);
        }
        FavouriteMods::for_league(&settings, "Forbidden Rites")
    }

    fn templates(rows: &[&ModOutcome]) -> Vec<String> {
        rows.iter().map(|row| row.template.clone()).collect()
    }

    fn settings() -> AppSettings {
        AppSettings {
            observations: vec![
                ObservationEntry {
                    id: ObservationId("o-1".to_string()),
                    label: "Precursor Tablets".to_string(),
                    league: "Forbidden Rites".to_string(),
                    search_id: "H4sIAAAA-_09".to_string(),
                    sample_every: 3,
                    ..ObservationEntry::default()
                },
                ObservationEntry {
                    id: ObservationId("o-2".to_string()),
                    label: "Rare amulets".to_string(),
                    league: "Forbidden Rites".to_string(),
                    enabled: false,
                    ..ObservationEntry::default()
                },
            ],
            ..AppSettings::default()
        }
    }

    /// 联赛那一格要说清是哪个游戏,理由同蹲价页。
    #[test]
    fn a_poe1_observation_says_so_in_the_league_cell() {
        let mut settings = settings();
        settings.observations[0].game = pnd_domain::Game::Poe1;
        settings.observations[0].league = "Standard".to_string();
        let rows = observation_rows(&settings, &status(), &i18n::ENGLISH, NOW);
        assert_eq!(rows[0][1].text(), "PoE1 · Standard");
        assert_eq!(rows[1][1].text(), "Forbidden Rites", "PoE2 那条不加前缀");
    }

    /// PoE1 的观察上没有「做成蹲价」:那条路要把搜索 id 解回查询原文,
    /// PoE1 的 id 里没有查询。
    #[test]
    fn make_watch_is_hidden_for_a_poe1_observation() {
        let settings = settings();
        assert!(make_watch_offered(None), "还没选中时照常显示");
        assert!(make_watch_offered(Some(&settings.observations[0])));
        let mut poe1 = settings.observations[0].clone();
        poe1.game = pnd_domain::Game::Poe1;
        assert!(!make_watch_offered(Some(&poe1)));
    }

    fn status() -> BTreeMap<ObservationId, ObservationStatus> {
        BTreeMap::from([(
            ObservationId("o-1".to_string()),
            ObservationStatus {
                active: 42,
                gone: 17,
                live: LiveRunState::Connected { since: NOW - 600 },
                next_discover_at: Some(NOW + 552),
                next_recheck_at: Some(NOW + 90),
                ..ObservationStatus::default()
            },
        )])
    }

    fn outcome(template: &str, kind: &str, seen: u32, gone: u32, sold: u32) -> ModOutcome {
        ModOutcome {
            template: template.to_string(),
            mod_kind: kind.to_string(),
            seen,
            gone,
            sold_likely: sold,
            currency: "exalted".to_string(),
            median_gone_price_milli: Some(12_500),
            median_active_price_milli: Some(20_000),
            median_hours_alive: Some(3.5),
        }
    }

    fn listing(id: &str, class: Option<GoneClass>) -> ObservedListingRow {
        ObservedListingRow {
            listing_id: id.to_string(),
            item_name: "Precursor Tablet".to_string(),
            item_json: String::new(),
            seller: "Exile#1234".to_string(),
            indexed_at: "2026-09-06T12:00:00Z".to_string(),
            first_seen_at: NOW - 4 * HOUR,
            last_seen_at: NOW - HOUR,
            first_price: Some(Price::new(20_000, Currency::Exalted)),
            last_price: Some(Price::new(12_500, Currency::Exalted)),
            first_price_div_milli: Some(240),
            last_price_div_milli: Some(150),
            price_changes: 1,
            status: if class.is_some() {
                ObservedStatus::Gone
            } else {
                ObservedStatus::Active
            },
            gone_at: class.is_some().then_some(NOW - 600),
            gone_class: class,
            check_rung: 3,
            next_check_at: NOW + 90,
        }
    }

    fn entry(id: &str, class: Option<GoneClass>) -> StreamEntry {
        StreamEntry {
            row: listing(id, class),
            mods: vec![
                ObservedMod {
                    mod_kind: "explicit".to_string(),
                    ordinal: 0,
                    template: "+# to maximum Life".to_string(),
                    value1: Some(115.0),
                    value2: None,
                },
                ObservedMod {
                    mod_kind: "explicit".to_string(),
                    ordinal: 1,
                    template: "#% increased Quantity of Waystones found".to_string(),
                    value1: Some(24.0),
                    value2: None,
                },
            ],
        }
    }

    /// 存活时间要读得出口。"12600 秒"是给机器看的,"3.5 小时"才回答得了
    /// "这货卖得快不快"。
    #[test]
    fn a_lifetime_reads_like_a_person_wrote_it() {
        let text = &i18n::SIMPLIFIED_CHINESE;
        assert_eq!(lifetime_text(0, text), "0 分钟");
        assert_eq!(lifetime_text(59, text), "0 分钟");
        assert_eq!(lifetime_text(720, text), "12 分钟");
        assert_eq!(lifetime_text(3_599, text), "59 分钟");
        assert_eq!(lifetime_text(3_600, text), "1 小时");
        assert_eq!(lifetime_text(12_600, text), "3.5 小时");
        assert_eq!(lifetime_text(86_400, text), "1 天");
        assert_eq!(lifetime_text(172_800, text), "2 天");
        assert_eq!(lifetime_text(216_000, text), "2.5 天");
        // 时钟往回跳不该算出一个负的存活时间。
        assert_eq!(lifetime_text(-5, text), "0 分钟");

        assert_eq!(lifetime_text(12_600, &i18n::ENGLISH), "3.5 h");
    }

    /// 倒计时精确到秒:按了"立即回查"之后,秒针是唯一能证明它真的动了的东西。
    #[test]
    fn a_countdown_is_minutes_and_seconds() {
        assert_eq!(countdown_mmss(0), "00:00");
        assert_eq!(countdown_mmss(9), "00:09");
        assert_eq!(countdown_mmss(90), "01:30");
        assert_eq!(countdown_mmss(3_599), "59:59");
        assert_eq!(countdown_mmss(3_600), "1:00:00");
        assert_eq!(countdown_mmss(7_322), "2:02:02");
        // 到点了(或者时钟往回跳)写 00:00,不写负数。
        assert_eq!(countdown_mmss(-30), "00:00");
    }

    /// 一行 = 一条观察,顺序和设置里一致 —— 下面那排按钮靠行号找回是哪一条。
    #[test]
    fn one_row_per_observation_in_settings_order() {
        let rows = observation_rows(&settings(), &status(), &i18n::ENGLISH, NOW);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].text(), "Precursor Tablets");
        assert_eq!(rows[1][0].text(), "Rare amulets");
        assert_eq!(rows[0][1].text(), "Forbidden Rites");
        assert_eq!(rows[0][2].text(), "3", "抽样比例要看得见");
        assert_eq!(rows[0][3].text(), "42");
        assert_eq!(rows[0][4].text(), "17");
    }

    /// 状态那一格要同时回答三件事:秒推怎么样、下一次什么时候动、出没出错。
    #[test]
    fn the_status_cell_carries_the_live_state_and_both_countdowns() {
        let rows = observation_rows(&settings(), &status(), &i18n::ENGLISH, NOW);
        assert_eq!(
            rows[0][5].text(),
            "live since 15:26 · new listings in 09:12 · next check in 01:30"
                .replace("15:26", &crate::shell::link::local_hm(NOW - 600))
        );
        assert_eq!(rows[0][5].tone(), Tone::Good, "连上了就是绿的");
    }

    /// 出错要写在脸上,而且那一格得变色 —— 一条连着几小时报错的观察,
    /// 表面上和一条正常跑的长得一模一样。
    #[test]
    fn an_error_shows_up_in_the_status_cell() {
        let mut status = status();
        status
            .get_mut(&ObservationId("o-1".to_string()))
            .unwrap()
            .last_error = Some("HTTP 429".to_string());
        let rows = observation_rows(&settings(), &status, &i18n::ENGLISH, NOW);
        assert!(rows[0][5].text().ends_with("error: HTTP 429"));
        assert_eq!(rows[0][5].tone(), Tone::Warn);
    }

    /// 用户把开关拨掉,屏幕上立刻就是"已停用",不等 actor 的下一条事件;
    /// 而且不再写倒计时 —— 那描述的是一件没在发生的事。
    #[test]
    fn a_disabled_observation_says_so_even_without_a_status_event() {
        let rows = observation_rows(&settings(), &status(), &i18n::ENGLISH, NOW);
        assert_eq!(rows[1][5].text(), "disabled");
        assert_eq!(rows[1][5].tone(), Tone::Muted);
        // 没跑过就没有计数,那两格是破折号,不是 0。
        assert_eq!(rows[1][3].text(), "—");
        assert_eq!(rows[1][4].text(), "—");
    }

    /// 在册一条挂单都没有时没有"下一条到点的",那半句不该出现 ——
    /// 写个 00:00 会让人以为它卡住了。
    #[test]
    fn an_observation_with_nothing_on_file_says_nothing_about_the_next_check() {
        let status = BTreeMap::from([(
            ObservationId("o-1".to_string()),
            ObservationStatus {
                live: LiveRunState::Off,
                next_discover_at: Some(NOW + 60),
                next_recheck_at: None,
                ..ObservationStatus::default()
            },
        )]);
        let rows = observation_rows(&settings(), &status, &i18n::ENGLISH, NOW);
        assert_eq!(rows[0][5].text(), "live off · new listings in 01:00");
        assert!(!rows[0][5].text().contains("next check"));
    }

    /// 票在路上过期的条数要摆在状态格里 —— 它一涨就说明请求排队排得太久
    /// (额度紧、网关在退避),而那是要动手调的事,不是"等等就好"。
    /// 一条都没过期时不写这一段:常态是 0,写着"0 expired"只会让人误会。
    #[test]
    fn expired_handles_show_up_in_the_status_cell() {
        let mut status = status();
        let rows = observation_rows(&settings(), &status, &i18n::ENGLISH, NOW);
        assert!(!rows[0][5].text().contains("expired"), "常态不写这一段");

        status
            .get_mut(&ObservationId("o-1".to_string()))
            .expect("the first observation")
            .expired_total = 3;
        let rows = observation_rows(&settings(), &status, &i18n::ENGLISH, NOW);
        assert!(
            rows[0][5].text().contains("3 expired"),
            "{}",
            rows[0][5].text()
        );
        let rows = observation_rows(&settings(), &status, &i18n::SIMPLIFIED_CHINESE, NOW);
        assert!(
            rows[0][5].text().contains("过期 3"),
            "{}",
            rows[0][5].text()
        );
    }

    /// 秒推推来了多少条、其中多少条没去看,得写在状态格里。
    ///
    /// 只写"过期"的话,一条秒推正常在送货的观察,和一条一条都没推来的,
    /// 屏幕上长得一模一样 —— 而"live 到底在不在干活"正是这一格该回答的事。
    /// 一条都没推来时不写这一段:那是"刚起来"的常态,写着"已推 0"只会
    /// 让人以为坏了。
    #[test]
    fn the_status_cell_says_how_much_live_pushed() {
        let mut status = status();
        let rows = observation_rows(&settings(), &status, &i18n::ENGLISH, NOW);
        assert!(
            !rows[0][5].text().contains("pushed"),
            "一条都没推来时不写这一段:{}",
            rows[0][5].text()
        );

        let live = status
            .get_mut(&ObservationId("o-1".to_string()))
            .expect("the first observation");
        live.pushed_total = 218;
        live.sampled_out_total = 145;
        let english = observation_rows(&settings(), &status, &i18n::ENGLISH, NOW);
        let cell = english[0][5].text();
        assert!(cell.contains("218 pushed · 145 not looked at"), "{cell}");
        // 紧跟在 live 那一段后面:先说连没连上,再说它送来了多少。
        assert!(
            cell.find("218 pushed") < cell.find("new listings in"),
            "{cell}"
        );

        let chinese = observation_rows(&settings(), &status, &i18n::SIMPLIFIED_CHINESE, NOW);
        assert!(
            chinese[0][5].text().contains("已推 218 · 未看 145"),
            "{}",
            chinese[0][5].text()
        );
    }

    /// 在册的挂单积压着的时候,最早该回头看的那一条永远在过去,倒计时就
    /// 永远写着 00:00 —— 看起来像卡死了,其实是排着队在扫。到点了就直说
    /// "排队中",别再写一个不会动的倒计时。
    #[test]
    fn a_recheck_that_is_already_due_says_it_is_queued() {
        let mut overdue = status();
        overdue
            .get_mut(&ObservationId("o-1".to_string()))
            .expect("the first observation")
            .next_recheck_at = Some(NOW - 5);
        let rows = observation_rows(&settings(), &overdue, &i18n::ENGLISH, NOW);
        assert!(
            rows[0][5].text().contains("recheck queued"),
            "{}",
            rows[0][5].text()
        );
        assert!(
            !rows[0][5].text().contains("00:00"),
            "{}",
            rows[0][5].text()
        );
        let rows = observation_rows(&settings(), &overdue, &i18n::SIMPLIFIED_CHINESE, NOW);
        assert!(
            rows[0][5].text().contains("回查排队中"),
            "{}",
            rows[0][5].text()
        );

        // 还在未来的那一刻照旧是倒计时 —— 按了"立即回查"之后,秒针是唯一
        // 能证明它真的动了的东西。
        let rows = observation_rows(&settings(), &status(), &i18n::ENGLISH, NOW);
        assert!(
            rows[0][5].text().contains("next check in 01:30"),
            "{}",
            rows[0][5].text()
        );
    }

    /// 词缀表按**卖掉几件**排,不按见过几件:这一页要回答的是"什么样的货
    /// 出得掉",一条见过 80 件一件没卖掉的词缀不该压在榜首。
    #[test]
    fn the_modifier_table_leads_with_what_actually_sells() {
        let outcomes = vec![
            outcome("#% increased Rarity", "explicit", 80, 4, 1),
            outcome("+# to maximum Life", "explicit", 30, 22, 20),
            outcome("+#% to Fire Resistance", "explicit", 40, 10, 9),
        ];
        let rows = mod_filtered(&outcomes, "", 1, &FavouriteMods::default(), false);
        assert_eq!(rows[0].template, "+# to maximum Life");
        assert_eq!(rows[1].template, "+#% to Fire Resistance");
        assert_eq!(rows[2].template, "#% increased Rarity");
    }

    /// 一行要同时说清"见过几件、卖掉几件、成交率多少、两个中位价差多少"。
    #[test]
    fn a_modifier_row_carries_the_counts_the_rate_and_both_medians() {
        let outcomes = vec![outcome("+# to maximum Life", "explicit", 30, 22, 20)];
        let favourites = FavouriteMods::default();
        let rows = mod_rows(
            &mod_filtered(&outcomes, "", 1, &favourites, false),
            &favourites,
            &i18n::ENGLISH,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].text(), "+# to maximum Life");
        assert_eq!(rows[0][1].text(), "Explicit");
        assert_eq!(rows[0][2].text(), "30");
        assert_eq!(rows[0][3].text(), "22");
        assert_eq!(rows[0][4].text(), "20");
        // 20 / 30 = 66.7%,分母是见过的件数,不是消失的件数。
        assert_eq!(rows[0][5].text(), "66.7%");
        assert_eq!(rows[0][6].text(), "12.5 exalted");
        assert_eq!(rows[0][7].text(), "20 exalted");
        assert_eq!(rows[0][8].text(), "3.5 h");
    }

    /// 中位价算不出来(那一组一条有价的都没有)写破折号,不写 0 ——
    /// 0 exalted 是个会让人当真的价。
    #[test]
    fn a_modifier_without_prices_shows_dashes() {
        let mut thin = outcome("+# to Spirit", "implicit", 5, 0, 0);
        thin.currency = String::new();
        thin.median_gone_price_milli = None;
        thin.median_active_price_milli = None;
        thin.median_hours_alive = None;
        let rows = mod_rows(&[&thin], &FavouriteMods::default(), &i18n::ENGLISH);
        assert_eq!(rows[0][4].text(), "0");
        assert_eq!(rows[0][5].text(), "0.0%");
        assert_eq!(rows[0][6].text(), "—");
        assert_eq!(rows[0][7].text(), "—");
        assert_eq!(rows[0][8].text(), "—");
    }

    /// 样本太薄的行默认藏起来:三件货里卖掉两件不能叫 67% 成交率。
    #[test]
    fn the_minimum_sample_filter_hides_the_thin_rows() {
        let outcomes = vec![
            outcome("+# to maximum Life", "explicit", 30, 22, 20),
            outcome("+# to Spirit", "implicit", 2, 2, 2),
        ];
        let none = FavouriteMods::default();
        assert_eq!(mod_filtered(&outcomes, "", 1, &none, false).len(), 2);
        assert_eq!(
            mod_filtered(&outcomes, "", DEFAULT_MIN_SAMPLES, &none, false).len(),
            1
        );
        assert_eq!(mod_filtered(&outcomes, "", 10, &none, false).len(), 1);
        assert!(mod_filtered(&outcomes, "", 99, &none, false).is_empty());
        // 类型筛选和样本数是"和"的关系。
        assert_eq!(
            mod_filtered(&outcomes, "implicit", 1, &none, false).len(),
            1
        );
        assert!(mod_filtered(&outcomes, "implicit", 3, &none, false).is_empty());
    }

    /// 收藏过的词缀置顶,而且带一颗星。
    ///
    /// 置顶的理由:这张表默认按"卖掉几件"排,而用户亲口认下"这条值钱"的
    /// 那几条,常常恰恰是样本还薄、排在第二屏的 —— 收藏了却还要每次翻下去找,
    /// 等于没收藏。星是给"它在上面是因为被收藏了,不是因为它卖得最好"这件事
    /// 一个交代。
    #[test]
    fn favourite_modifiers_are_pinned_to_the_top_and_wear_a_star() {
        let outcomes = vec![
            outcome("+# to maximum Life", "explicit", 30, 22, 20),
            outcome("+#% to Fire Resistance", "explicit", 40, 10, 9),
            outcome("#% increased Rarity", "explicit", 80, 4, 1),
        ];
        let none = FavouriteMods::default();
        let pinned = favourites(&[("explicit", "#% increased Rarity")]);

        // 没有收藏时就是原来的顺序:卖掉最多的在前。
        assert_eq!(
            templates(&mod_filtered(&outcomes, "", 1, &none, false)),
            vec![
                "+# to maximum Life",
                "+#% to Fire Resistance",
                "#% increased Rarity"
            ]
        );
        // 收藏那条跳到最前,剩下的相对次序一个不动。
        assert_eq!(
            templates(&mod_filtered(&outcomes, "", 1, &pinned, false)),
            vec![
                "#% increased Rarity",
                "+# to maximum Life",
                "+#% to Fire Resistance"
            ]
        );

        let rows = mod_rows(
            &mod_filtered(&outcomes, "", 1, &pinned, false),
            &pinned,
            &i18n::ENGLISH,
        );
        assert_eq!(rows[0][0].text(), "★ #% increased Rarity");
        assert_eq!(rows[1][0].text(), "+# to maximum Life", "没收藏的不带星");
    }

    /// 收藏是按联赛记的:上赛季认下的那条,在这个赛季的观察上不该置顶。
    #[test]
    fn a_favourite_from_another_league_does_not_count() {
        let outcomes = vec![
            outcome("+# to maximum Life", "explicit", 30, 22, 20),
            outcome("#% increased Rarity", "explicit", 80, 4, 1),
        ];
        let mut settings = AppSettings::default();
        settings.toggle_favourite("Standard", "explicit", "#% increased Rarity");
        let here = FavouriteMods::for_league(&settings, "Forbidden Rites");
        assert!(here.is_empty());
        assert_eq!(
            templates(&mod_filtered(&outcomes, "", 1, &here, false)),
            vec!["+# to maximum Life", "#% increased Rarity"]
        );
        // 类型也是键的一部分。
        let other_kind = favourites(&[("implicit", "#% increased Rarity")]);
        assert!(!other_kind.contains("explicit", "#% increased Rarity"));
    }

    /// "只看收藏"打开之后,没收藏的一行都不留 —— 这一档是"我认下的这几条
    /// 今天卖得怎么样",别的词缀在这个问题里只是噪音。
    #[test]
    fn the_favourites_only_toggle_hides_everything_else() {
        let outcomes = vec![
            outcome("+# to maximum Life", "explicit", 30, 22, 20),
            outcome("#% increased Rarity", "explicit", 80, 4, 1),
        ];
        let picked = favourites(&[("explicit", "#% increased Rarity")]);
        assert_eq!(
            templates(&mod_filtered(&outcomes, "", 1, &picked, true)),
            vec!["#% increased Rarity"]
        );
        // 和样本门槛、类型筛选是"和"的关系。
        assert!(mod_filtered(&outcomes, "", 99, &picked, true).is_empty());
        assert!(mod_filtered(&outcomes, "implicit", 1, &picked, true).is_empty());
        // 一条都没收藏时开着它就是一张空表(而不是悄悄退回全部)。
        assert!(mod_filtered(&outcomes, "", 1, &FavouriteMods::default(), true).is_empty());
    }

    /// 挂单流里的词缀也要带星:聚合表是结论,这一栏是证据 —— 一件货身上
    /// 有没有我认下的那条词缀,是翻证据时第一眼要看的东西。
    #[test]
    fn a_favourite_modifier_is_starred_in_the_listing_stream() {
        let entry = entry("one", Some(GoneClass::SoldLikely));
        let picked = favourites(&[("explicit", "+# to maximum Life")]);
        assert_eq!(
            listing_mod_lines(&entry.mods, &picked),
            vec![
                "★ +# to maximum Life".to_string(),
                "#% increased Quantity of Waystones found".to_string(),
            ]
        );
        assert_eq!(
            listing_mod_lines(&entry.mods, &FavouriteMods::default())[0],
            "+# to maximum Life"
        );
    }

    /// 一键蹲价:选中的词缀 + 这条观察自己的查询 = 一条能直接粘回交易站的搜索。
    ///
    /// 上限取**成交价**中位,不取在售价:在售价是"卖不掉的人开的价",拿它当
    /// 蹲价上限等于永远在等一个没人接的价。
    #[test]
    fn a_watch_draft_carries_the_filtered_search_the_label_and_the_sold_median() {
        let settings = settings();
        let observation = &settings.observations[0];
        let mut row = outcome("+#% to Lightning Resistance", "explicit", 30, 22, 20);
        row.median_gone_price_milli = Some(12_500);
        row.median_active_price_milli = Some(20_000);

        let draft = watch_draft(observation, FIXTURE_QUERY, &row, "explicit.stat_1671376347");
        assert_eq!(draft.league, "Forbidden Rites");
        assert_eq!(
            draft.label,
            "Precursor Tablets · +#% to Lightning Resistance"
        );
        assert_eq!(draft.cap_milli, Some(12_500));
        assert_eq!(draft.currency, "exalted");

        // 搜索 id 解开之后,是原来的查询加上那一格词缀筛选。
        let query: serde_json::Value =
            serde_json::from_str(&decode_search_id(&draft.search_id).expect("decode")).unwrap();
        assert_eq!(query["name"], "Choir of the Storm");
        assert_eq!(
            query["stats"][0]["filters"],
            serde_json::json!([{ "id": "explicit.stat_1671376347" }])
        );
    }

    /// 一条都还没卖掉的词缀没有成交价中位,退到在售价中位;两个都没有就
    /// 把上限那一格留空 —— 猜一个数出来,用户按下新增就是照着它蹲。
    #[test]
    fn a_watch_draft_falls_back_to_the_asking_median_then_to_nothing() {
        let settings = settings();
        let observation = &settings.observations[0];
        let mut unsold = outcome("+# to maximum Life", "explicit", 30, 0, 0);
        unsold.median_gone_price_milli = None;
        assert_eq!(
            watch_draft(observation, FIXTURE_QUERY, &unsold, "explicit.a").cap_milli,
            Some(20_000)
        );

        let mut priceless = unsold.clone();
        priceless.median_active_price_milli = None;
        priceless.currency = String::new();
        let draft = watch_draft(observation, FIXTURE_QUERY, &priceless, "explicit.a");
        assert_eq!(draft.cap_milli, None);
        assert!(draft.currency.is_empty());
    }

    /// 长词缀要截断:备注名那一列只有 200 像素,而"观察名 · 一整句词缀"
    /// 常常比它长一倍。
    #[test]
    fn a_watch_draft_label_keeps_the_template_short() {
        let settings = settings();
        let long = outcome(
            "#% increased Quantity of Items found in this Area and #% increased Rarity",
            "explicit",
            30,
            22,
            20,
        );
        let draft = watch_draft(
            &settings.observations[0],
            FIXTURE_QUERY,
            &long,
            "explicit.a",
        );
        let tail = draft
            .label
            .strip_prefix("Precursor Tablets · ")
            .expect("the observation name leads");
        assert_eq!(tail.chars().count(), DRAFT_TEMPLATE_CHARS);
        assert!(tail.ends_with('…'), "{tail}");
        assert!(long.template.starts_with(tail.trim_end_matches('…')));
    }

    /// 类型下拉从库里长出来,而且交易站那七种都得有中文写法 ——
    /// 屏幕上冒出一个 `enchant` 就是没人翻译过它。
    #[test]
    fn the_kind_picker_grows_out_of_the_stored_modifiers() {
        let outcomes = vec![
            outcome("+# to maximum Life", "explicit", 30, 22, 20),
            outcome("+# to Spirit", "implicit", 5, 1, 1),
            outcome("Allocates #", "enchant", 4, 1, 1),
        ];
        let choices = kind_choices(&outcomes, &i18n::ENGLISH);
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.value().to_string())
                .collect::<Vec<_>>(),
            vec!["", "enchant", "explicit", "implicit"]
        );
        assert_eq!(choices[0].title().to_string(), "All");
        assert_eq!(choices[1].title().to_string(), "Enchant");
        // 库还是空的时候只剩"全部",而不是一个空下拉。
        assert_eq!(kind_choices(&[], &i18n::ENGLISH).len(), 1);

        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            for kind in [
                "explicit",
                "implicit",
                "crafted",
                "desecrated",
                "rune",
                "enchant",
                "fractured",
            ] {
                assert_ne!(kind_label(kind, text), kind, "{language} 的 {kind} 没翻译");
            }
        }
        // 认不出来的原样显示,而不是整档消失。
        assert_eq!(kind_label("something-new", &i18n::ENGLISH), "something-new");
    }

    fn price_outcome(bucket_milli: i64, seen: u32, gone: u32, sold: u32) -> PriceOutcome {
        PriceOutcome {
            currency: "divine".to_string(),
            bucket_milli,
            seen,
            gone,
            looks_sold: sold,
            median_lifetime_secs: Some(12_600),
            active: seen - gone,
        }
    }

    /// 一档价位一行,而且顺序原样照抄库里那份(从便宜往贵)——
    /// 这张表是竖着读的:成交率在哪一档掉下来,那里就是天花板。
    #[test]
    fn a_price_row_carries_the_counts_the_rate_and_the_median_life() {
        let outcomes = vec![
            price_outcome(2_000, 8, 6, 6),
            price_outcome(3_000, 10, 2, 1),
            price_outcome(5_000, 4, 0, 0),
        ];
        let rows = price_rows(
            &price_filtered(&outcomes, 1),
            PriceMode::ByCurrency,
            &i18n::ENGLISH,
        );
        assert_eq!(rows.len(), 3, "一档一行");
        assert_eq!(
            rows.iter().map(|row| row[0].text()).collect::<Vec<_>>(),
            vec!["2 divine", "3 divine", "5 divine"],
            "顺序照抄库里那份"
        );
        assert_eq!(rows[0][1].text(), "8");
        assert_eq!(rows[0][2].text(), "6");
        assert_eq!(rows[0][3].text(), "6");
        // 6 / 8 = 75%,分母是见过的件数,不是消失的件数(同词缀表)。
        assert_eq!(rows[0][4].text(), "75.0%");
        assert_eq!(rows[0][5].text(), "3.5 h");
        // 一件没卖掉的那一档是灰的 0,不是金色 —— 金色留给这张表的主角。
        assert_eq!(rows[2][3].text(), "0");
        assert_eq!(rows[2][3].tone(), Tone::Muted);
        assert_eq!(rows[0][3].tone(), Tone::Accent);
    }

    /// 一条都没消失的那一档没有存活中位,写破折号 —— 写 0 会读成"秒没"。
    #[test]
    fn a_bucket_with_nothing_gone_yet_shows_a_dash_for_the_median_life() {
        let mut fresh = price_outcome(10_000, 5, 0, 0);
        fresh.median_lifetime_secs = None;
        let rows = price_rows(&[&fresh], PriceMode::ByCurrency, &i18n::ENGLISH);
        assert_eq!(rows[0][0].text(), "10 divine");
        assert_eq!(rows[0][5].text(), "—");
    }

    /// 最低那一档要写「不到 1 divine」,两种语言都得有话说。
    /// 写成「0 divine」会读成"白送",而那一档恰恰是最好卖的一批。
    #[test]
    fn the_cheapest_bucket_says_under_one_in_both_languages() {
        let floor = bucket_floor_milli(PriceMode::ByCurrency);
        assert_eq!(
            price_bucket_label(0, floor, "divine", &i18n::ENGLISH),
            "under 1 divine"
        );
        assert_eq!(
            price_bucket_label(0, floor, "divine", &i18n::SIMPLIFIED_CHINESE),
            "不到 1 divine"
        );
        // 别的档就是"数字 + 货币",两种语言一样(货币码本来就是英文)。
        for text in [&i18n::ENGLISH, &i18n::SIMPLIFIED_CHINESE] {
            assert_eq!(price_bucket_label(1_000, floor, "divine", text), "1 divine");
            assert_eq!(price_bucket_label(15_000, floor, "chaos", text), "15 chaos");
            assert_eq!(
                price_bucket_label(1_000_000, floor, "exalted", text),
                "1000 exalted"
            );
        }
    }

    /// 折算那一栏的 1 以下几档要写成小数,而它的最低档是「不到 0.1 divine」——
    /// 照按币种那条阶梯写成「不到 1 divine」的话,0.5 divine 那一档明明就在
    /// 隔壁,整栏立刻变成一句自相矛盾的话。
    #[test]
    fn the_converted_ladder_writes_fractions_and_its_own_floor() {
        let floor = bucket_floor_milli(PriceMode::Converted);
        assert_eq!(floor, 100, "折算那条阶梯从 0.1 起步");
        assert_eq!(
            price_bucket_label(0, floor, "divine", &i18n::ENGLISH),
            "under 0.1 divine"
        );
        assert_eq!(
            price_bucket_label(0, floor, "divine", &i18n::SIMPLIFIED_CHINESE),
            "不到 0.1 divine"
        );
        for text in [&i18n::ENGLISH, &i18n::SIMPLIFIED_CHINESE] {
            assert_eq!(price_bucket_label(100, floor, "divine", text), "0.1 divine");
            assert_eq!(price_bucket_label(500, floor, "divine", text), "0.5 divine");
            assert_eq!(
                price_bucket_label(750, floor, "divine", text),
                "0.75 divine"
            );
            assert_eq!(price_bucket_label(2_000, floor, "divine", text), "2 divine");
        }
        // 整张表走的是同一条阶梯:行也得照 0.5 写。
        let mut cheap = price_outcome(500, 6, 4, 4);
        cheap.median_lifetime_secs = Some(3_600);
        let rows = price_rows(&[&cheap], PriceMode::Converted, &i18n::ENGLISH);
        assert_eq!(rows[0][0].text(), "0.5 divine");
    }

    /// 表上头那句汇率:两个数、从哪儿来的、还有多少条折不出来。
    ///
    /// 折算那一栏的每个数都建在这份汇率上,而按错汇率折出来的表和对的
    /// 长得一模一样 —— 所以这一行必须写全,而且必须说清是谁给的数。
    #[test]
    fn the_rates_line_says_the_numbers_the_source_and_what_could_not_be_converted() {
        let rates = CurrencyRates {
            chaos_per_divine_milli: Some(13_000),
            exalted_per_divine_milli: Some(186_000),
            mirror_per_divine_milli: None,
        };
        let ninja = RateSources {
            chaos: RateSource::Ninja,
            exalted: RateSource::Ninja,
            mirror: RateSource::Unknown,
        };
        assert_eq!(
            rates_status_line(&rates, &ninja, 0, &i18n::SIMPLIFIED_CHINESE),
            "汇率:1 divine = 13 chaos · 186 exalted(poe.ninja)"
        );
        assert_eq!(
            rates_status_line(&rates, &ninja, 0, &i18n::ENGLISH),
            "Rates: 1 divine = 13 chaos · 186 exalted (poe.ninja)"
        );

        // 手填的那几档要说出来:汇率不对时"去设置页改"和"等 ninja 恢复"
        // 是两条不同的路。
        let manual = RateSources {
            chaos: RateSource::Manual,
            exalted: RateSource::Manual,
            mirror: RateSource::Unknown,
        };
        assert_eq!(
            rates_status_line(&rates, &manual, 4, &i18n::SIMPLIFIED_CHINESE),
            "汇率:1 divine = 13 chaos · 186 exalted(手动) · 4 条未折算"
        );
        // 一档手填、一档接口给的,两边都说。
        let mixed = RateSources {
            chaos: RateSource::Manual,
            ..ninja
        };
        assert!(
            rates_status_line(&rates, &mixed, 0, &i18n::ENGLISH).contains("poe.ninja + manual"),
            "一半是自己填的,那正是最该看见的情况"
        );

        // 一档都没有:写"还没读到",而不是一行 0。
        let nothing = rates_status_line(
            &CurrencyRates::none(),
            &RateSources::default(),
            2,
            &i18n::SIMPLIFIED_CHINESE,
        );
        assert!(nothing.starts_with("汇率:还没读到"), "{nothing}");
        assert!(nothing.ends_with("2 条未折算"), "{nothing}");
        // 只有一档时缺的那格写破折号,不写 0("值 0 chaos"是另一个意思)。
        let half = CurrencyRates {
            exalted_per_divine_milli: None,
            ..rates
        };
        assert_eq!(
            rates_status_line(&half, &ninja, 0, &i18n::ENGLISH),
            "Rates: 1 divine = 13 chaos · — exalted (poe.ninja)"
        );
    }

    /// 两种口径各画各的那一份,而且换一栏不用回库里重读。
    #[test]
    fn the_price_tab_keeps_both_ladders_in_memory() {
        let data = ObservationData {
            prices_by_currency: vec![price_outcome(2_000, 8, 6, 6)],
            prices_converted: vec![price_outcome(500, 3, 2, 2), price_outcome(2_000, 5, 4, 4)],
            unconverted: 1,
            ..ObservationData::default()
        };
        assert_eq!(data.prices(PriceMode::ByCurrency).len(), 1);
        assert_eq!(data.prices(PriceMode::Converted).len(), 2);
        // 默认就是折算那一栏:按币种那一栏答不出"值 2 divine 的货卖不卖得掉"。
        assert_eq!(PriceMode::default(), PriceMode::Converted);
    }

    /// 样本门槛对两张表是同一个:三件货里卖掉两件不能叫 67% 成交率,
    /// 换成价位也一样。
    #[test]
    fn the_minimum_sample_filter_also_hides_thin_price_buckets() {
        let outcomes = vec![
            price_outcome(2_000, 8, 6, 6),
            price_outcome(3_000, 2, 2, 2),
            price_outcome(5_000, 4, 1, 1),
        ];
        assert_eq!(price_filtered(&outcomes, 1).len(), 3);
        assert_eq!(
            price_filtered(&outcomes, DEFAULT_MIN_SAMPLES)
                .iter()
                .map(|row| row.bucket_milli)
                .collect::<Vec<_>>(),
            vec![2_000, 5_000],
            "只见过两条的那一档藏起来"
        );
        assert_eq!(price_filtered(&outcomes, 5).len(), 1);
        assert!(price_filtered(&outcomes, 99).is_empty());
    }

    /// 消失那一栏先说判成了什么:整页的结论都建在那一档上,而它可能判错。
    #[test]
    fn a_gone_listing_line_says_what_happened_and_how_long_it_lived() {
        let text = &i18n::SIMPLIFIED_CHINESE;
        let sold = entry("one", Some(GoneClass::SoldLikely));
        assert_eq!(
            listing_title(&sold.row, text),
            "Precursor Tablet · 12.5 exalted"
        );
        assert_eq!(
            listing_detail(&sold, StreamTab::Gone, text, NOW),
            "疑似成交 · 存活 3 小时 · 改价 1 次"
        );
        // 判定那一档在库里可能是空的(老行、手改过的值),当"看不出来"。
        let unknown = entry("two", None);
        assert!(listing_detail(&unknown, StreamTab::Gone, text, NOW).starts_with("太短,看不出"));
    }

    /// 在售那一栏问的是另一件事:它已经在那儿坐了多久。
    #[test]
    fn an_active_listing_line_says_how_long_it_has_been_sitting() {
        let text = &i18n::SIMPLIFIED_CHINESE;
        let mut sitting = entry("three", None);
        sitting.row.first_seen_at = NOW - 3 * 86_400;
        sitting.row.price_changes = 0;
        let line = listing_detail(&sitting, StreamTab::Active, text, NOW);
        assert!(line.starts_with("已挂 3 天"), "{line}");
        // 一次都没改过价就不写那一段 —— "改价 0 次"是句废话。
        assert!(!line.contains("改价"), "{line}");
    }

    /// 名字空着的老行退回挂单 id,而不是留一行空白 —— 空白看起来像坏了。
    #[test]
    fn a_listing_without_a_name_falls_back_to_its_id() {
        let mut nameless = listing("abcdefghijklmnop", Some(GoneClass::SoldLikely));
        nameless.item_name = String::new();
        nameless.last_price = None;
        assert_eq!(listing_title(&nameless, &i18n::ENGLISH), "abcdefghij · —");
    }

    /// 挂单流里一条货只列头几行词缀:稀有装十几条全铺出来,一屏就只剩得下两件。
    #[test]
    fn the_stream_only_lists_the_first_few_modifiers() {
        let mut many = entry("four", Some(GoneClass::SoldLikely));
        many.mods = (0..9)
            .map(|ordinal| ObservedMod {
                mod_kind: "explicit".to_string(),
                ordinal,
                template: format!("modifier {ordinal}"),
                value1: None,
                value2: None,
            })
            .collect();
        let lines = listing_mod_lines(&many.mods, &FavouriteMods::default());
        assert_eq!(lines.len(), STREAM_MOD_LINES);
        assert_eq!(lines[0], "modifier 0");
        assert!(listing_mod_lines(&[], &FavouriteMods::default()).is_empty());
    }

    /// 四档判定在两种语言下都得有话说,而且互不重复 —— 分不清"卖掉了"和
    /// "太短看不出",这一栏就白列了。
    #[test]
    fn every_gone_class_reads_differently_in_both_languages() {
        let classes = [
            Some(GoneClass::SoldLikely),
            Some(GoneClass::SoldAfterCuts),
            Some(GoneClass::Unknown),
            Some(GoneClass::GoneBeforeFirstLook),
        ];
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            let mut words: Vec<&str> = classes
                .iter()
                .map(|class| gone_class_label(*class, text))
                .collect();
            assert!(words.iter().all(|word| !word.trim().is_empty()));
            words.sort_unstable();
            let count = words.len();
            words.dedup();
            assert_eq!(words.len(), count, "{language} 里有两档判定撞词了");
        }
    }

    /// 备注名留空时,名字从搜索自己身上取 —— `H4sIAAAAA` 认不出是什么东西。
    #[test]
    fn an_empty_label_comes_from_the_search_itself() {
        assert_eq!(label_for("Tablets", "whatever"), "Tablets");
        assert_eq!(label_for("   ", "notasearchid"), "notasearch");
    }

    /// 一条观察攒到东西之前和之后要分得开:空的时候说"再等等",
    /// 有数据了就不该再说。
    #[test]
    fn an_observation_with_no_rows_yet_knows_it_is_empty() {
        let mut data = ObservationData::default();
        assert!(data.is_empty());
        data.summary.active = 1;
        assert!(!data.is_empty());
        assert!(data.stream(StreamTab::Gone).is_empty());
        assert!(data.stream(StreamTab::Active).is_empty());
    }
}
