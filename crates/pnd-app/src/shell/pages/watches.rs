//! 蹲价页:新增一条搜索,看它们各自跑到哪一步,以及限速预算还剩多少。
//!
//! 这一页是整个程序的主页。它回答两个问题:"我在盯着什么"和"程序还敢
//! 不敢再发请求" —— 第二个问题必须常驻,因为把交易站惹毛的代价不是报错,
//! 是账号。
//!
//! 表格里的行是 `settings.json` 里的搜索列表和 actor 广播的运行状态拼出来的:
//! 名字、联赛、上限是用户填的,状态、上次轮询、今日命中是跑出来的。
//! 每行的操作(立即轮询 / 启停 / 删)作用在选中的那一行 —— 上游的表格
//! 不支持在格子里放按钮,所以按钮统一放在表下面那一条。

use std::collections::BTreeMap;

use gpui::{
    App, AppContext as _, Context, Entity, ParentElement, SharedString, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{Sizable as _, Size, StyledExt as _};

use pnd_domain::{
    Price, SearchRef, WatchId, decode_search_id, default_label_for, parse_search_reference,
    search_page_url,
};
use pnd_runtime::{LiveOffReason, LiveRunState, RuntimeCommand, WatchRunState, WatchStatus};
use pnd_settings::{AppSettings, WatchEntry};
use pnd_trade::{FETCH_POLICY, SEARCH_POLICY};

use super::{Cell, TableContent, Tone, column, league_cell_text, number_column};
use crate::i18n::{self, Text};
use crate::shell::link::{countdown_text, local_clock, local_hm};
use crate::shell::pages;
use crate::shell::{
    AppShell, Choice, ChoiceSelect, choice_select, field_label, field_row, hint, page_heading,
    panel, picker, table,
};
use crate::theme::*;

/// 备注名留空时,拿搜索 id 的前几个字符顶上。
///
/// 不留空的理由:表格第一列全空的话,三条搜索长得一模一样;而 id 的前缀
/// 至少是互不相同的。
const LABEL_FROM_ID_CHARS: usize = 10;

/// 新增表单的控件。
///
/// 单独一个结构体而不是散在 `AppShell` 上:这几个东西是同生同死的,
/// "清空表单"将来只需要动这里一处。
pub struct WatchesForm {
    pub search: Entity<InputState>,
    pub label: Entity<InputState>,
    /// 留空 = 用设置页那个联赛。
    pub league: Entity<InputState>,
    pub cap: Entity<InputState>,
    pub currency: ChoiceSelect,
    /// 新加的这条要不要立刻启用 / 要不要开 live 秒推。
    pub enabled: bool,
    pub live: bool,
    /// 表单现在是"新增"还是"改这一条"。
    ///
    /// 存 id 不存行号:行号在删掉一条之后就指向别人了,而按保存的时候
    /// 屏幕上写的还是原来那个名字 —— 那一下会把改动写到另一条搜索上。
    pub editing: Option<WatchId>,
}

impl WatchesForm {
    pub fn new(
        settings: &pnd_settings::AppSettings,
        text: &'static Text,
        window: &mut Window,
        cx: &mut Context<AppShell>,
    ) -> Self {
        let search = cx
            .new(|cx| InputState::new(window, cx).placeholder(text.watches_add_search_placeholder));
        let label =
            cx.new(|cx| InputState::new(window, cx).placeholder(text.watches_label_placeholder));
        let league =
            cx.new(|cx| InputState::new(window, cx).placeholder(text.watches_league_placeholder));
        let cap = cx
            .new(|cx| InputState::new(window, cx).placeholder(text.watches_price_cap_placeholder));
        // 货币是专有名词,不翻译,所以这个下拉换语言时不用重造。
        let currency = choice_select(currency_choices(), "divine", window, cx);
        Self {
            search,
            label,
            league,
            cap,
            currency,
            // 默认和 `WatchEntry::default()` 一致:加进来就是要它跑。
            enabled: true,
            live: !settings.poesessid.is_empty(),
            editing: None,
        }
    }

    /// 把一条现有的搜索装进表单,表单随之进入"改这一条"的模式。
    ///
    /// 搜索那一格也填上(填的是 id),但它在这个模式下是灰的:换搜索等于
    /// 换一条别的东西,该走"删了重加"。
    pub fn load(&mut self, entry: &WatchEntry, window: &mut Window, cx: &mut Context<AppShell>) {
        for (input, value) in [
            (&self.search, entry.search_id.clone()),
            (&self.label, entry.label.clone()),
            (&self.league, entry.league.clone()),
            (&self.cap, milli_text(entry.price_cap.amount_milli)),
        ] {
            input.update(cx, |state, cx| {
                state.set_value(value, window, cx);
            });
        }
        let currency = SharedString::from(entry.price_cap.currency.code().to_owned());
        self.currency.update(cx, |state, cx| {
            state.set_selected_value(&currency, window, cx);
        });
        self.enabled = entry.enabled;
        self.live = entry.live;
        self.editing = Some(entry.id.clone());
    }

    /// 换语言之后把占位符换掉。占位符是造控件那一刻复制走的,不会自己跟着变。
    pub fn relabel(
        &mut self,
        _settings: &pnd_settings::AppSettings,
        text: &'static Text,
        window: &mut Window,
        cx: &mut Context<AppShell>,
    ) {
        for (input, placeholder) in [
            (&self.search, text.watches_add_search_placeholder),
            (&self.label, text.watches_label_placeholder),
            (&self.league, text.watches_league_placeholder),
            (&self.cap, text.watches_price_cap_placeholder),
        ] {
            input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx);
            });
        }
    }

    /// 清空输入框并退回"新增"模式。加完一条、改完一条、按取消都走这里。
    ///
    /// 货币和两个开关留着 —— 连着加几条时它们多半是同一个值。名字必须清:
    /// 上一条的备注名留在框里,下一条会顶着"风暴合唱"这个名字加进来。
    fn clear(&mut self, window: &mut Window, cx: &mut Context<AppShell>) {
        for input in [&self.search, &self.label, &self.league, &self.cap] {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        self.editing = None;
    }
}

/// 价格上限能用的通货。交易站上还有别的,但拿它们当上限没有意义。
fn currency_choices() -> Vec<Choice> {
    vec![
        Choice::plain("divine"),
        Choice::plain("exalted"),
        Choice::plain("chaos"),
    ]
}

/// 蹲价表的列。行由 [`table_content_for`] 填。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("label", text.watches_col_label, 200.),
            column("league", text.watches_col_league, 130.),
            number_column("cap", text.watches_col_cap, 90.),
            // 这一格现在写三件事:轮询、节奏、live。量过最长的一句
            // ("polling · next in 15 min · every 300 s · live disabled: session
            // invalid"),窄一点就会把"为什么没连上"那半句裁掉。
            column("status", text.watches_col_status, 430.),
            column("last_poll", text.watches_col_last_poll, 120.),
            number_column("hits", text.watches_col_hits_today, 80.),
        ],
        rows: Vec::new(),
        empty: text.watches_empty.into(),
    }
}

/// 列 + 真行。
pub fn table_content_for(
    settings: &AppSettings,
    status: &BTreeMap<WatchId, WatchStatus>,
    text: &'static Text,
    now: i64,
) -> TableContent {
    TableContent {
        rows: watch_rows(settings, status, text, now),
        ..table_content(text)
    }
}

/// 每条搜索一行。顺序就是 `settings.watches` 的顺序 —— 下面那排按钮
/// 靠"第几行"找回是哪一条,两边必须是同一个顺序。
pub fn watch_rows(
    settings: &AppSettings,
    status: &BTreeMap<WatchId, WatchStatus>,
    text: &'static Text,
    now: i64,
) -> Vec<Vec<Cell>> {
    settings
        .watches
        .iter()
        .map(|entry| {
            let live = status.get(&entry.id);
            let state = run_state(entry, live);
            vec![
                Cell::plain(entry.label.clone()),
                Cell::muted(league_cell_text(entry.game, &entry.league, text)),
                Cell::data(entry.price_cap.display()),
                Cell::new(status_text(state, live, text, now), status_tone(state)),
                match live.and_then(|status| status.last_poll_at) {
                    Some(at) => Cell::data(local_clock(at)),
                    None => Cell::muted(text.watches_never),
                },
                // "—" 留给"还没有状态可说"。已经在跑却一次没中,那是个
                // 实实在在的 0 —— 用破折号写它,看起来像程序没在数。
                match live.map(|status| status.hits_today) {
                    None => Cell::muted(text.common_none),
                    Some(0) => Cell::muted("0"),
                    Some(hits) => Cell::accent(hits.to_string()),
                },
            ]
        })
        .collect()
}

/// 这条搜索该显示成哪一档。
///
/// 设置里关掉的一律显示"已停用",不管 actor 上一次广播的是什么 ——
/// 用户刚把开关拨掉,屏幕上就该立刻变,而不是等下一条事件。
fn run_state(entry: &WatchEntry, status: Option<&WatchStatus>) -> WatchRunState {
    if !entry.enabled {
        return WatchRunState::Disabled;
    }
    status.map_or(WatchRunState::Polling, |status| status.state)
}

fn status_word(state: WatchRunState, text: &'static Text) -> &'static str {
    match state {
        WatchRunState::Disabled => text.status_disabled,
        WatchRunState::Polling => text.status_polling,
        WatchRunState::Backoff => text.status_backoff,
        WatchRunState::Live => text.status_live,
        WatchRunState::Held => text.status_held,
    }
}

fn status_tone(state: WatchRunState) -> Tone {
    match state {
        WatchRunState::Live => Tone::Good,
        WatchRunState::Polling => Tone::Plain,
        WatchRunState::Backoff | WatchRunState::Held => Tone::Warn,
        WatchRunState::Disabled => Tone::Muted,
    }
}

/// live 那一头现在怎么样,一句话。
///
/// 轮询状态只有五个词,说不清"秒推为什么没连上" —— 而那恰恰是用户最常问的:
/// 勾了 Live 却没反应,到底是没粘 cookie、被上限挤下来了,还是正在退避重连。
pub(crate) fn live_text(live: LiveRunState, text: &'static Text, now: i64) -> String {
    match live {
        LiveRunState::Off => text.live_off.to_owned(),
        LiveRunState::Disabled(reason) => {
            i18n::fill(text.live_disabled, &[live_reason(reason, text)])
        }
        LiveRunState::Connecting => text.live_connecting.to_owned(),
        LiveRunState::Connected { since } => {
            i18n::fill(text.live_connected_since, &[&local_hm(since)])
        }
        LiveRunState::Backoff { until, attempt } => i18n::fill(
            text.live_backoff,
            &[&countdown_text(until - now, text), &attempt.to_string()],
        ),
        LiveRunState::Held { until } => i18n::fill(text.live_held_until, &[&local_clock(until)]),
    }
}

fn live_reason(reason: LiveOffReason, text: &'static Text) -> &'static str {
    match reason {
        LiveOffReason::NoSession => text.live_reason_no_session,
        LiveOffReason::TooMany => text.live_reason_too_many,
        LiveOffReason::SessionInvalid => text.live_reason_session_invalid,
    }
}

/// 状态词 + 下一轮倒计时 + live 档位。
///
/// 停用的那条只写一个词:它既没有下一轮,也没有 live 连接,多写的每一段
/// 都是在描述一件没在发生的事。
fn status_text(
    state: WatchRunState,
    status: Option<&WatchStatus>,
    text: &'static Text,
    now: i64,
) -> String {
    let word = status_word(state, text);
    if state == WatchRunState::Disabled {
        return word.to_owned();
    }
    let mut line = word.to_owned();
    if let Some(next) = status.and_then(|status| status.next_poll_at) {
        line.push_str(" · ");
        line.push_str(&i18n::fill(
            text.watches_next_in,
            &[&countdown_text(next - now, text)],
        ));
    }
    // 倒计时只说"下一轮什么时候",说不出节奏 —— 而节奏会因为秒推连上、
    // 退避、多加一条搜索(预算要分)而变。没在排班的那条是 0,不写。
    if let Some(every) = status
        .map(|status| status.poll_every_secs)
        .filter(|every| *every > 0)
    {
        line.push_str(" · ");
        line.push_str(&i18n::fill(text.watches_poll_every, &[&every.to_string()]));
    }
    if let Some(status) = status {
        line.push_str(" · ");
        line.push_str(&live_text(status.live, text, now));
    }
    line
}

/// 备注名:用户填了就听他的,留空就问搜索自己叫什么,再不行拿 id 前缀顶上。
///
/// 新增和"改这一条"走的是同一句 —— 改的时候把名字清空,拿到的也是从搜索里
/// 取出来的名字,而不是又一次 `H4sIAAAAA`。
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

/// 选中那条搜索在交易站上的那一页。
///
/// 拆成一个纯函数是为了能测:一条搜索记着自己是哪一代游戏,而两代的搜索页
/// 在两条不同的路径上(`/trade2/search/poe2/…` 和 `/trade/search/…`),
/// 联赛名里的空格还得编码 —— 哪一段拼错了,开出来都是一张 404,而按下按钮
/// 的那一刻没人会回头核对地址栏。
fn watch_trade_page_url(entry: &WatchEntry) -> String {
    search_page_url(&SearchRef {
        game: entry.game,
        league: entry.league.clone(),
        search_id: entry.search_id.clone(),
    })
}

/// 表下面那一句"倒计时怎么突然变长了"。
///
/// 为什么不写进状态那一格:那一列只有 430 像素,而这句话对每一条连上秒推的
/// 搜索都是同一句 —— 抄 N 遍还把前面"还有多久轮询"挤没了。真连上了才说:
/// 连接中、退避中都还在按普通档轮询,说了就是假话。
fn live_relaxed_hint(
    settings: &AppSettings,
    status: &BTreeMap<WatchId, WatchStatus>,
    text: &'static Text,
) -> Option<&'static str> {
    if settings.watcher.poll_interval_when_live_seconds <= settings.watcher.poll_interval_seconds {
        return None;
    }
    settings
        .watches
        .iter()
        .filter_map(|entry| status.get(&entry.id))
        .any(|status| matches!(status.live, LiveRunState::Connected { .. }))
        .then_some(text.watches_live_relaxed)
}

/// 框里的字。
fn text_of(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_string()
}

/// 千分整数 → 人看的小数(和 `Price::display` 同一套规矩,但不带货币名)。
///
/// 汇率是"1 divine 换多少 chaos",货币名写在模板里,数字这边只出数字。
/// 暗金热度页的参考价(exalted × 1000)也是这个规矩,所以它借这一份。
pub(crate) fn milli_text(milli: i64) -> String {
    let whole = milli / 1000;
    let frac = (milli % 1000).unsigned_abs();
    if frac == 0 {
        return whole.to_string();
    }
    let frac = format!("{frac:03}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

impl AppShell {
    pub(crate) fn render_watches(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .child(page_heading(text.watches_heading, text.watches_subtitle))
            .child(self.watches_add_form(cx))
            .child(
                panel()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(table(&self.watches_table)),
            )
            // 整张表共用的一句注解:有搜索连上了秒推,所以那几条的轮询间隔
            // 是放宽过的。没有这回事的时候它不出现。
            .children(live_relaxed_hint(&self.settings, &self.watch_status, text).map(hint))
            .child(self.watches_row_actions(cx))
            .child(self.budget_strip())
    }

    /// 表单:新增一条搜索,或者改选中的那一条。
    ///
    /// 两种模式共用同一组框,差别只有三处:标题那一行、搜索框灰不灰、
    /// 右下角那个按钮写什么。做成两套表单的话,"备注名填在哪儿"这种事
    /// 要在两处各写一遍,迟早分家。
    fn watches_add_form(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let enabled = self.watches_form.enabled;
        let live = self.watches_form.live;
        let error = self.watch_error.clone();
        let editing = self.watches_form.editing.clone();
        let editing_label = editing.as_ref().and_then(|id| {
            self.settings
                .watches
                .iter()
                .find(|entry| &entry.id == id)
                .map(|entry| entry.label.clone())
        });
        let is_editing = editing.is_some();
        panel()
            .flex_none()
            .p(px(10.))
            .gap(px(8.))
            // 改的是哪一条,写在最上面:表单里那些框长得和新增时一模一样,
            // 没有这一行就分不出自己按下去会发生什么。
            .children(editing_label.map(|label| {
                div()
                    .text_size(fs(FS_11_5))
                    .text_color(c(ACCENT_TEXT))
                    .child(SharedString::from(i18n::fill(
                        text.watches_editing,
                        &[&label],
                    )))
            }))
            .child(
                field_row()
                    .child(field_label(text.watches_add_search_label))
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            Input::new(&self.watches_form.search)
                                .with_size(Size::Small)
                                .disabled(is_editing),
                        ),
                    ),
            )
            .child(
                field_row()
                    .child(field_label(text.watches_label_label))
                    .child(
                        div()
                            .w(px(200.))
                            .flex_none()
                            .child(Input::new(&self.watches_form.label).with_size(Size::Small)),
                    )
                    .child(
                        div()
                            .text_size(fs(FS_11_5))
                            .text_color(muted())
                            .child(text.watches_league_label),
                    )
                    .child(
                        div()
                            .w(px(160.))
                            .flex_none()
                            .child(Input::new(&self.watches_form.league).with_size(Size::Small)),
                    )
                    .child(
                        div()
                            .text_size(fs(FS_11_5))
                            .text_color(muted())
                            .child(text.watches_price_cap_label),
                    )
                    .child(
                        div()
                            .w(px(90.))
                            .flex_none()
                            .child(Input::new(&self.watches_form.cap).with_size(Size::Small)),
                    )
                    .child(picker(
                        text.watches_currency_label,
                        &self.watches_form.currency,
                        110.,
                    ))
                    // 上限本身也算命中。不写出来的话,"我填 223,它是 223,
                    // 到底响不响"只能靠猜。
                    .child(hint(text.watches_cap_hint)),
            )
            .child(
                field_row()
                    .child(field_label(""))
                    .child(
                        Switch::new("watch-enabled")
                            .checked(enabled)
                            .label(SharedString::from(text.watches_enable_toggle))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.watches_form.enabled = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Switch::new("watch-live")
                            .checked(live)
                            .label(SharedString::from(text.watches_live_toggle))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.watches_form.live = *checked;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_grow())
                    .children(is_editing.then(|| {
                        Button::new("watch-cancel-edit")
                            .label(text.watches_cancel_edit)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_watch_edit(window, cx);
                            }))
                    }))
                    .child(if is_editing {
                        Button::new("watch-save")
                            .primary()
                            .label(text.watches_save_changes)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_watch_edit(window, cx);
                            }))
                    } else {
                        Button::new("watch-add")
                            .primary()
                            .label(text.watches_add_button)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_watch(window, cx);
                            }))
                    }),
            )
            // 粘错了的东西就说在表单底下,而不是状态行:错在这儿,话也该在这儿。
            .children((!error.is_empty()).then(|| {
                div()
                    .text_size(fs(FS_11))
                    .text_color(c(DANGER_TEXT))
                    .child(SharedString::from(error))
            }))
            .children(is_editing.then(|| hint(text.watches_edit_search_locked)))
    }

    /// 选中一行之后能对它做的五件事。
    fn watches_row_actions(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let selected = self.selected_watch(cx).cloned();
        let label = selected.as_ref().map_or_else(
            || text.common_select_row.to_owned(),
            |entry| entry.label.clone(),
        );
        let enabled = selected.as_ref().is_some_and(|entry| entry.enabled);
        let live = selected.as_ref().is_some_and(|entry| entry.live);
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
                    .child(text.watches_row_actions),
            )
            .child(
                div()
                    .w(px(180.))
                    .flex_none()
                    .text_size(fs(FS_11_5))
                    .text_color(c(TEXT_SECONDARY))
                    .child(SharedString::from(label)),
            )
            .child(
                Switch::new("watch-row-enabled")
                    .checked(enabled)
                    .label(SharedString::from(text.watches_enable_toggle))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.update_selected_watch(cx, |entry| entry.enabled = checked);
                    })),
            )
            .child(
                Switch::new("watch-row-live")
                    .checked(live)
                    .label(SharedString::from(text.watches_live_toggle))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.update_selected_watch(cx, |entry| entry.live = checked);
                    })),
            )
            .child(div().flex_grow())
            .child(
                Button::new("watch-open-trade")
                    .label(text.common_open_trade_site)
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_selected_watch_on_trade(cx);
                    })),
            )
            .child(
                Button::new("watch-poll-now")
                    .label(text.watches_poll_now)
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.poll_selected_watch(cx);
                    })),
            )
            .child(
                Button::new("watch-remove")
                    .danger()
                    .label(text.watches_remove)
                    .with_size(Size::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.remove_selected_watch(cx);
                    })),
            )
    }

    /// 预算条:两条策略各用掉多少,右边挂着汇率 —— 判定异币种挂单靠它,
    /// 它没读到的时候得看得见。
    fn budget_strip(&self) -> gpui::Div {
        let text = self.text();
        panel()
            .flex_none()
            .h_flex()
            .items_center()
            .gap(px(16.))
            .px(px(10.))
            .py(px(6.))
            .child(
                div()
                    .text_size(fs(FS_11_5))
                    .text_color(c(TEXT_SECONDARY))
                    .child(text.budget_strip_title),
            )
            .child(budget_item(
                text.budget_search,
                self.budget_text(SEARCH_POLICY),
            ))
            .child(budget_item(
                text.budget_fetch,
                self.budget_text(FETCH_POLICY),
            ))
            // 限速器把下一封请求压住的时候,这一句是"程序看起来没在动"
            // 的唯一解释。不用等的时候它不出现。
            .children(self.next_allowed_text().map(hint))
            .child(div().flex_grow())
            .child(hint(self.rates_text()))
    }

    /// 两条策略里等得最久的那一句"{} 秒后可再请求"。
    fn next_allowed_text(&self) -> Option<String> {
        let text = self.text();
        let waiting = [SEARCH_POLICY, FETCH_POLICY]
            .into_iter()
            .filter_map(|policy| self.budget_next_allowed.get(policy).copied())
            .max()?;
        Some(i18n::fill(
            text.budget_next_allowed,
            &[&waiting.to_string()],
        ))
    }

    /// 一条策略的"用掉 / 允许"。还没发过请求就没有限速头,也就没有数字。
    fn budget_text(&self, policy: &str) -> String {
        let text = self.text();
        match self.budget_usage(policy) {
            Some(usage) => i18n::fill(
                text.budget_used_of,
                &[&usage.used.to_string(), &usage.allowed.to_string()],
            ),
            None => text.common_none.to_owned(),
        }
    }

    /// "1 divine = N chaos / M exalted"。
    fn rates_text(&self) -> String {
        let text = self.text();
        match (
            self.rates.chaos_per_divine_milli,
            self.rates.exalted_per_divine_milli,
        ) {
            (Some(chaos), Some(exalted)) => i18n::fill(
                text.watches_rates,
                &[&milli_text(chaos), &milli_text(exalted)],
            ),
            _ => text.watches_rates_unknown.to_owned(),
        }
    }

    // ---- 动作 --------------------------------------------------------

    /// 粘进来的东西 → 一条新搜索。
    fn add_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text();
        let raw = text_of(&self.watches_form.search, cx);
        let league = self.form_league(cx);
        let Some(mut search_ref) = parse_search_reference(&raw, &league) else {
            self.watch_error = text.watches_invalid_search.to_owned();
            cx.notify();
            return;
        };
        // URL 里自带联赛,但用户在框里明写的那个说了算 —— 明说的意图
        // 压过从地址里猜出来的。
        search_ref.league = league;
        let Some(cap) = self.form_cap(cx) else {
            self.watch_error = text.watches_invalid_cap.to_owned();
            cx.notify();
            return;
        };

        let label = self.form_label(&search_ref.search_id, cx);
        let mut entry = WatchEntry::new(label, &search_ref, cap);
        entry.enabled = self.watches_form.enabled;
        entry.live = self.watches_form.live;
        self.push_log(format!("watch added: {} ({})", entry.label, entry.league));
        self.settings.watches.push(entry);

        self.watch_error.clear();
        if self.save_and_apply() {
            self.set_notice(text.watches_added.to_owned());
            // 加完必须清干净:上一条的备注名留在框里,下一条会顶着
            // 别人的名字加进来。
            self.watches_form.clear(window, cx);
        }
        self.watches_dirty = true;
        cx.notify();
    }

    /// 把表单里的改动写回**同一条**搜索。
    ///
    /// `WatchId` 一个字都不动:运行状态、提醒历史、去重记录全挂在它上面,
    /// 换个 id 等于把这条搜索的过去全丢了,而屏幕上看起来只是改了个名字。
    fn save_watch_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(id) = self.watches_form.editing.clone() else {
            return;
        };
        let Some(index) = self
            .settings
            .watches
            .iter()
            .position(|entry| entry.id == id)
        else {
            // 改到一半这条被删了。退回新增模式,而不是把它又写回去。
            self.watches_form.clear(window, cx);
            cx.notify();
            return;
        };
        let Some(cap) = self.form_cap(cx) else {
            self.watch_error = text.watches_invalid_cap.to_owned();
            cx.notify();
            return;
        };
        let league = self.form_league(cx);
        let search_id = self.settings.watches[index].search_id.clone();
        let label = self.form_label(&search_id, cx);
        let (enabled, live) = (self.watches_form.enabled, self.watches_form.live);

        let entry = &mut self.settings.watches[index];
        entry.label = label;
        entry.league = league;
        entry.price_cap = cap;
        entry.enabled = enabled;
        entry.live = live;
        let (label, league) = (entry.label.clone(), entry.league.clone());
        self.push_log(format!("watch updated: {label} ({league})"));

        self.watch_error.clear();
        if self.save_and_apply() {
            self.set_notice(text.watches_changes_saved.to_owned());
            self.watches_form.clear(window, cx);
        }
        self.watches_dirty = true;
        cx.notify();
    }

    /// 不改了。表单退回新增模式,那一条搜索一个字都没动过。
    fn cancel_watch_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.watch_error.clear();
        self.watches_form.clear(window, cx);
        cx.notify();
    }

    /// 联赛那一格。留空 = 用设置页那个。
    fn form_league(&self, cx: &Context<Self>) -> String {
        let league = text_of(&self.watches_form.league, cx);
        if league.is_empty() {
            self.settings.league.clone()
        } else {
            league
        }
    }

    /// 上限 + 货币。填的不是个大于 0 的数就返回 `None`。
    fn form_cap(&self, cx: &Context<Self>) -> Option<Price> {
        let amount = text_of(&self.watches_form.cap, cx)
            .parse::<f64>()
            .ok()
            .filter(|amount| amount.is_finite() && *amount > 0.0)?;
        let currency = self
            .watches_form
            .currency
            .read(cx)
            .selected_value()
            .map_or_else(|| "divine".to_owned(), ToString::to_string);
        Some(Price::from_trade(amount, &currency))
    }

    /// 备注名那一格。留空的处理在 [`label_for`] 里。
    fn form_label(&self, search_id: &str, cx: &Context<Self>) -> String {
        label_for(&text_of(&self.watches_form.label, cx), search_id)
    }

    /// 市场观察那边递过来一份草稿 → 填进新增表单。
    ///
    /// 填完**什么都没发生**:表单退回"新增"模式,按不按那颗"新增"是用户
    /// 自己的事。一条自动加进去的搜索会立刻开始花限速预算,而它是不是用户
    /// 要的那条,只有他看一眼才知道。
    pub(crate) fn prefill_add_form(
        &mut self,
        draft: pages::observations::WatchDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (input, value) in [
            (&self.watches_form.search, draft.search_id),
            (&self.watches_form.label, draft.label),
            (&self.watches_form.league, draft.league),
            // 中位价算不出来时留空:凭空填一个数,用户按下新增就是照着它蹲。
            (
                &self.watches_form.cap,
                draft.cap_milli.map(milli_text).unwrap_or_default(),
            ),
        ] {
            input.update(cx, |state, cx| {
                state.set_value(value, window, cx);
            });
        }
        // 下拉里没有的货币(交易站上还有几十种)就别去动它:选不中会把
        // 用户上一次挑的那个也一起清掉。
        if currency_choices()
            .iter()
            .any(|choice| choice.stored_value() == draft.currency)
        {
            let currency = SharedString::from(draft.currency);
            self.watches_form.currency.update(cx, |state, cx| {
                state.set_selected_value(&currency, window, cx);
            });
        }
        self.watches_form.editing = None;
        self.watch_error.clear();
    }

    /// 表格里选中了一行 → 把那条搜索装进表单。
    ///
    /// 只能在 render 里做:写输入框要 `&mut Window`,而发出"选中了第几行"
    /// 那条事件的订阅回调手上没有窗口。
    pub(crate) fn sync_watch_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.watches_form_load.take() else {
            return;
        };
        let Some(entry) = self.settings.watches.get(row).cloned() else {
            return;
        };
        self.watch_error.clear();
        self.watches_form.load(&entry, window, cx);
    }

    /// 表格里选中的那条搜索。
    fn selected_watch(&self, cx: &mut Context<Self>) -> Option<&WatchEntry> {
        let row = self.watches_table.read(cx).selected_row()?;
        self.settings.watches.get(row)
    }

    fn selected_index(&self, cx: &mut Context<Self>) -> Option<usize> {
        let row = self.watches_table.read(cx).selected_row()?;
        (row < self.settings.watches.len()).then_some(row)
    }

    /// 改选中那条的一个开关,存盘,推给 actor。
    fn update_selected_watch(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut WatchEntry),
    ) {
        let Some(index) = self.selected_index(cx) else {
            self.select_a_row_first(cx);
            return;
        };
        change(&mut self.settings.watches[index]);
        self.save_and_apply();
        self.watches_dirty = true;
        cx.notify();
    }

    fn remove_selected_watch(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(index) = self.selected_index(cx) else {
            self.select_a_row_first(cx);
            return;
        };
        let removed = self.settings.watches.remove(index);
        self.push_log(format!("watch removed: {}", removed.label));
        self.watch_status.remove(&removed.id);
        // 正在改的就是它:表单不能再停在"改这一条"上,那条已经不存在了。
        if self.watches_form.editing.as_ref() == Some(&removed.id) {
            self.watches_form.editing = None;
        }
        if self.save_and_apply() {
            self.set_notice(text.watches_removed.to_owned());
        }
        self.watches_dirty = true;
        cx.notify();
    }

    fn poll_selected_watch(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(entry) = self.selected_watch(cx).cloned() else {
            self.select_a_row_first(cx);
            return;
        };
        if self.send_runtime(RuntimeCommand::PollNow(entry.id)) {
            self.set_notice(text.watches_poll_requested.to_owned());
        }
        cx.notify();
    }

    /// 在浏览器里开选中那条搜索的交易站页面。
    ///
    /// 省掉的是"把 id 抄下来、拼一条地址、粘进浏览器"这一串手活。开出来之后
    /// 是浏览器在跟交易站说话,不占这个程序的任何限速预算。
    fn open_selected_watch_on_trade(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_watch(cx).cloned() else {
            self.select_a_row_first(cx);
            return;
        };
        self.open_trade_url(&watch_trade_page_url(&entry));
        cx.notify();
    }

    fn select_a_row_first(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        self.set_notice(text.common_select_row.to_owned());
        cx.notify();
    }

    /// 存盘 + 推给 actor。搜索列表改了这两件事永远一起做:只存不推,
    /// 后台还在跑旧的那份;只推不存,重启就没了。
    ///
    /// 返回"存进去了没有" —— 存不进去的时候调用方不该再报"已新增"。
    pub(crate) fn save_and_apply(&mut self) -> bool {
        let text = self.text();
        if self.read_only {
            self.set_sticky_notice(text.settings_read_only.to_owned());
            return false;
        }
        self.settings.normalize();
        if let Err(error) = self.settings_store.save(&self.settings) {
            self.push_log(format!("settings save failed: {error}"));
            self.set_notice(i18n::fill(text.settings_save_failed, &[&error.to_string()]));
            return false;
        }
        self.apply_settings_to_runtime();
        true
    }
}

/// 预算条上的一格:名字 + 数字。
fn budget_item(name: &'static str, value: String) -> gpui::Div {
    div()
        .h_flex()
        .items_center()
        .gap(px(6.))
        .child(div().text_size(fs(FS_11)).text_color(muted()).child(name))
        .child(
            div()
                .font_family(FONT_MONO)
                .text_size(fs(FS_11))
                .text_color(c(TEXT_DATA))
                .child(SharedString::from(value)),
        )
}

#[cfg(test)]
mod watches_page_tests {
    use pnd_domain::Currency;

    use super::*;
    use crate::i18n;

    /// 一条真的搜索 id(Choir of the Storm),查询里写着物品名。
    const FIXTURE_ID: &str = "H4sIAAAAAAAAAx2LvQnAIBBGV5GvdgLbjJAyWAhRFPRO9FIEcfdo2vcz0MXJ02EGuEpiggFTTuQxNcgVv8AROTXFQUn06hRuBfof13cNyFt35eheOKQsvm1hp50f6Xdj02AAAAA";

    fn settings() -> AppSettings {
        AppSettings {
            watches: vec![
                WatchEntry {
                    id: WatchId("w-1".to_string()),
                    label: "Choir of the Storm".to_string(),
                    league: "Forbidden Rites".to_string(),
                    price_cap: Price::new(20_000, Currency::Divine),
                    ..WatchEntry::default()
                },
                WatchEntry {
                    id: WatchId("w-2".to_string()),
                    label: "Beira's Anguish".to_string(),
                    league: "Forbidden Rites".to_string(),
                    price_cap: Price::new(1_500, Currency::Divine),
                    enabled: false,
                    ..WatchEntry::default()
                },
            ],
            ..AppSettings::default()
        }
    }

    /// 联赛那一格要说清是哪个游戏:PoE1 和 PoE2 都有 Standard。
    #[test]
    fn a_poe1_watch_says_so_in_the_league_cell() {
        let mut settings = settings();
        settings.watches[0].game = pnd_domain::Game::Poe1;
        settings.watches[0].league = "Standard".to_string();
        let rows = watch_rows(&settings, &status(), &i18n::ENGLISH, 1_000_000);
        assert_eq!(rows[0][1].text(), "PoE1 · Standard");
        assert_eq!(rows[1][1].text(), "Forbidden Rites", "PoE2 那条不加前缀");
    }

    fn status() -> BTreeMap<WatchId, WatchStatus> {
        BTreeMap::from([(
            WatchId("w-1".to_string()),
            WatchStatus {
                state: WatchRunState::Polling,
                live: LiveRunState::Disabled(LiveOffReason::NoSession),
                next_poll_at: Some(1_000_090),
                poll_every_secs: 180,
                last_poll_at: Some(1_000_000),
                hits_today: 4,
                ..WatchStatus::default()
            },
        )])
    }

    /// 一行 = 一条搜索,顺序和设置里一致 —— 下面那排按钮靠行号找回是哪一条。
    #[test]
    fn one_row_per_watch_in_settings_order() {
        let rows = watch_rows(&settings(), &status(), &i18n::ENGLISH, 1_000_000);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].text(), "Choir of the Storm");
        assert_eq!(rows[1][0].text(), "Beira's Anguish");
        assert_eq!(rows[0][2].text(), "20 divine");
    }

    /// 状态那一格要同时回答四件事:现在在干嘛、下一轮什么时候、多久一次、
    /// 秒推怎么样。
    #[test]
    fn the_status_cell_carries_the_countdown_the_cadence_and_the_live_state() {
        let rows = watch_rows(&settings(), &status(), &i18n::ENGLISH, 1_000_000);
        assert_eq!(
            rows[0][3].text(),
            "polling · next in 1 min · every 180 s · live disabled: no session"
        );
        assert_eq!(
            rows[0][4].text(),
            crate::shell::link::local_clock(1_000_000)
        );
        assert_eq!(rows[0][5].text(), "4");
    }

    /// 还没排上班的那条(停用、搜索 id 坏了)节奏是 0,那半句就不该出现 ——
    /// "每 0 秒"是句假话。
    #[test]
    fn a_watch_that_is_not_scheduled_says_nothing_about_its_cadence() {
        let status = BTreeMap::from([(
            WatchId("w-1".to_string()),
            WatchStatus {
                state: WatchRunState::Backoff,
                poll_every_secs: 0,
                ..WatchStatus::default()
            },
        )]);
        let rows = watch_rows(&settings(), &status, &i18n::ENGLISH, 1_000_000);
        assert!(
            !rows[0][3].text().contains("every"),
            "{}",
            rows[0][3].text()
        );
    }

    /// 已经在跑却一次没中,那是个实实在在的 0。"—" 只留给"还没有状态可说"。
    #[test]
    fn a_running_watch_with_no_hits_shows_a_zero() {
        let status = BTreeMap::from([(
            WatchId("w-1".to_string()),
            WatchStatus {
                state: WatchRunState::Polling,
                ..WatchStatus::default()
            },
        )]);
        let rows = watch_rows(&settings(), &status, &i18n::ENGLISH, 1_000_000);
        assert_eq!(rows[0][5].text(), "0");
        assert_eq!(rows[1][5].text(), "—", "没有状态的那条还是破折号");
    }

    /// 上限本身也算命中,两种语言都得把"≤"说出来 —— 填 223 遇上 223 到底
    /// 响不响,不该靠猜。
    #[test]
    fn the_cap_hint_says_the_cap_itself_counts() {
        for language in i18n::LANGUAGES {
            assert!(
                i18n::text(language).watches_cap_hint.contains('≤'),
                "{language} 的上限说明没写清等于也算"
            );
        }
    }

    /// 用户把开关拨掉,屏幕上立刻就是"已停用",不等 actor 的下一条事件。
    #[test]
    fn a_disabled_watch_says_so_even_without_a_status_event() {
        let rows = watch_rows(&settings(), &status(), &i18n::ENGLISH, 1_000_000);
        assert_eq!(rows[1][3].text(), "disabled");
        // 没跑过就没有上次轮询,也没有今日命中,这两格不能空着。
        assert_eq!(rows[1][4].text(), "never");
        assert_eq!(rows[1][5].text(), "—");
    }

    /// "去市集看"按钮开的是哪一条地址。
    ///
    /// 两代的搜索页在两条不同的路径上(`/trade2/search/poe2/…` 和
    /// `/trade/search/…`),而联赛名里的空格非编码不可 —— 拼错任何一段,
    /// 开出来就是一张 404,而按下按钮的那一刻没人会去核对地址栏。
    #[test]
    fn a_watch_turns_into_the_trade_page_for_its_own_game() {
        let mut settings = settings();
        settings.watches[0].search_id = "abcd1234".to_string();
        assert_eq!(
            watch_trade_page_url(&settings.watches[0]),
            "https://www.pathofexile.com/trade2/search/poe2/Forbidden%20Rites/abcd1234"
        );

        settings.watches[0].game = pnd_domain::Game::Poe1;
        settings.watches[0].league = "Standard".to_string();
        settings.watches[0].search_id = "Rj3mL5Sw".to_string();
        assert_eq!(
            watch_trade_page_url(&settings.watches[0]),
            "https://www.pathofexile.com/trade/search/Standard/Rj3mL5Sw"
        );
    }

    /// 备注名留空时,名字从搜索自己身上取 —— `H4sIAAAAA` 认不出是什么东西。
    #[test]
    fn an_empty_label_comes_from_the_search_itself() {
        assert_eq!(label_for("My amulet", FIXTURE_ID), "My amulet");
        assert_eq!(label_for("   ", FIXTURE_ID), "Choir of the Storm");
        // 解不开的 id(以及三样都没写的纯词缀搜索)才轮到 id 前缀顶上。
        assert_eq!(label_for("", "notasearchid"), "notasearch");
    }

    /// 秒推连上之后轮询自己放慢一档,得有人说一声 —— 倒计时从 3 分钟跳到
    /// 5 分钟,不解释的话看起来就像"轮询停了"。
    ///
    /// 但这句解释不进状态那一格:那一列只有 430 像素,挤进来就把前面
    /// "还有多久轮询"一起裁掉了。它是整张表共用的一句话,所以放表下面。
    #[test]
    fn the_relaxed_note_sits_under_the_table_not_in_the_status_cell() {
        let mut settings = settings();
        let status = BTreeMap::from([(
            WatchId("w-1".to_string()),
            WatchStatus {
                state: WatchRunState::Live,
                live: LiveRunState::Connected { since: 999_000 },
                next_poll_at: Some(1_000_900),
                last_poll_at: Some(1_000_000),
                ..WatchStatus::default()
            },
        )]);

        // 默认设置里普通档 180 秒、秒推档 300 秒 —— 确实放宽了。
        assert!(
            settings.watcher.poll_interval_when_live_seconds
                > settings.watcher.poll_interval_seconds
        );
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            let rows = watch_rows(&settings, &status, text, 1_000_000);
            assert!(
                !rows[0][3].text().contains('('),
                "{language} 的状态格里还留着括号解释:{}",
                rows[0][3].text()
            );
            let footer = live_relaxed_hint(&settings, &status, text);
            assert_eq!(footer, Some(text.watches_live_relaxed));
            assert!(!footer.unwrap().trim().is_empty());
        }

        // 两档一样就没有"放宽"这回事,那句话不该出现。
        settings.watcher.poll_interval_when_live_seconds = settings.watcher.poll_interval_seconds;
        assert_eq!(live_relaxed_hint(&settings, &status, &i18n::ENGLISH), None);
    }

    /// 只有真连上了才说。连接中、退避中都还在按普通档轮询,说了就是假话。
    #[test]
    fn a_live_connection_that_is_not_up_yet_claims_nothing() {
        let status = BTreeMap::from([(
            WatchId("w-1".to_string()),
            WatchStatus {
                state: WatchRunState::Polling,
                live: LiveRunState::Connecting,
                last_poll_at: Some(1_000_000),
                ..WatchStatus::default()
            },
        )]);
        let rows = watch_rows(&settings(), &status, &i18n::ENGLISH, 1_000_000);
        assert!(
            !rows[0][3].text().contains("relaxed"),
            "{}",
            rows[0][3].text()
        );
        assert_eq!(
            live_relaxed_hint(&settings(), &status, &i18n::ENGLISH),
            None
        );
    }

    /// "秒推自 15:18:49" 里的秒数在这一列上只是占地方:它回答的是"连上多久
    /// 了",精确到分钟就够,而省下的三个字符正是状态格差的那几个。
    #[test]
    fn the_live_since_time_drops_the_seconds() {
        for language in i18n::LANGUAGES {
            let line = live_text(
                LiveRunState::Connected { since: 1_000_000 },
                i18n::text(language),
                1_000_000,
            );
            assert_eq!(line.matches(':').count(), 1, "{language}: {line}");
        }
    }

    #[test]
    fn every_run_state_has_a_word_in_both_languages() {
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            for state in [
                WatchRunState::Disabled,
                WatchRunState::Polling,
                WatchRunState::Backoff,
                WatchRunState::Live,
                WatchRunState::Held,
            ] {
                assert!(!status_word(state, text).trim().is_empty());
            }
        }
    }

    /// live 的每一档都得说得出话来,而且两档不能长得一样 —— "为什么没连上"
    /// 分不清的话,这一列就白加了。
    #[test]
    fn every_live_state_reads_differently_in_both_languages() {
        let states = [
            LiveRunState::Off,
            LiveRunState::Disabled(LiveOffReason::NoSession),
            LiveRunState::Disabled(LiveOffReason::TooMany),
            LiveRunState::Disabled(LiveOffReason::SessionInvalid),
            LiveRunState::Connecting,
            LiveRunState::Connected { since: 1_000_000 },
            LiveRunState::Backoff {
                until: 1_000_030,
                attempt: 2,
            },
            LiveRunState::Held { until: 1_000_300 },
        ];
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            let mut lines: Vec<String> = states
                .iter()
                .map(|state| live_text(*state, text, 1_000_000))
                .collect();
            assert!(lines.iter().all(|line| !line.trim().is_empty()));
            lines.sort();
            let count = lines.len();
            lines.dedup();
            assert_eq!(lines.len(), count, "{language} 里有两档 live 撞词了");
        }
    }

    /// 退避那一档要同时说清"还要等多久"和"这是第几次" —— 没有次数的话,
    /// 一条连不上的搜索看起来和一条刚断一次的一模一样。
    #[test]
    fn the_backoff_line_carries_the_countdown_and_the_attempt() {
        let line = live_text(
            LiveRunState::Backoff {
                until: 1_000_030,
                attempt: 3,
            },
            &i18n::ENGLISH,
            1_000_000,
        );
        assert_eq!(line, "live retry in 30 s (attempt 3)");
        // 停用的搜索不写 live:它没有下一轮,也没有连接。
        let rows = watch_rows(&settings(), &status(), &i18n::ENGLISH, 1_000_000);
        assert_eq!(rows[1][3].text(), "disabled");
    }
}
