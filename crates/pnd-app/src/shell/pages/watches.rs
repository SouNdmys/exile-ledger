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

use pnd_domain::{Price, WatchId, parse_search_reference};
use pnd_runtime::{RuntimeCommand, WatchRunState, WatchStatus};
use pnd_settings::{AppSettings, WatchEntry};
use pnd_trade::{FETCH_POLICY, SEARCH_POLICY};

use super::{Cell, TableContent, Tone, column, number_column};
use crate::i18n::{self, Text};
use crate::shell::link::{countdown_text, local_clock};
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
    pub cap: Entity<InputState>,
    pub currency: ChoiceSelect,
    /// 新加的这条要不要立刻启用 / 要不要开 live 秒推。
    pub enabled: bool,
    pub live: bool,
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
        let cap = cx
            .new(|cx| InputState::new(window, cx).placeholder(text.watches_price_cap_placeholder));
        // 货币是专有名词,不翻译,所以这个下拉换语言时不用重造。
        let currency = choice_select(currency_choices(), "divine", window, cx);
        Self {
            search,
            label,
            cap,
            currency,
            // 默认和 `WatchEntry::default()` 一致:加进来就是要它跑。
            enabled: true,
            live: !settings.poesessid.is_empty(),
        }
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
            (&self.cap, text.watches_price_cap_placeholder),
        ] {
            input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx);
            });
        }
    }

    /// 加完一条之后清空三个输入框,货币和两个开关留着 —— 连着加几条时
    /// 它们多半是同一个值。
    fn clear(&self, window: &mut Window, cx: &mut Context<AppShell>) {
        for input in [&self.search, &self.label, &self.cap] {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
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
            column("status", text.watches_col_status, 150.),
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
                Cell::muted(entry.league.clone()),
                Cell::data(entry.price_cap.display()),
                Cell::new(status_text(state, live, text, now), status_tone(state)),
                match live.and_then(|status| status.last_poll_at) {
                    Some(at) => Cell::data(local_clock(at)),
                    None => Cell::muted(text.watches_never),
                },
                match live.map_or(0, |status| status.hits_today) {
                    0 => Cell::muted(text.common_none),
                    hits => Cell::accent(hits.to_string()),
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

/// 状态词 + 下一轮倒计时。停用的那条不写倒计时:它没有下一轮。
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
    match status.and_then(|status| status.next_poll_at) {
        Some(next) => format!(
            "{word} · {}",
            i18n::fill(text.watches_next_in, &[&countdown_text(next - now, text)])
        ),
        None => word.to_owned(),
    }
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
            .child(self.watches_row_actions(cx))
            .child(self.budget_strip())
    }

    /// 新增表单:粘一条搜索进来,给它起个名字和一个价格上限。
    fn watches_add_form(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let enabled = self.watches_form.enabled;
        let live = self.watches_form.live;
        let error = self.watch_error.clone();
        panel()
            .flex_none()
            .p(px(10.))
            .gap(px(8.))
            .child(
                field_row()
                    .child(field_label(text.watches_add_search_label))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Input::new(&self.watches_form.search).with_size(Size::Small)),
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
                    )),
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
                    .child(
                        Button::new("watch-add")
                            .primary()
                            .label(text.watches_add_button)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_watch(window, cx);
                            })),
                    ),
            )
            // 粘错了的东西就说在表单底下,而不是状态行:错在这儿,话也该在这儿。
            .children((!error.is_empty()).then(|| {
                div()
                    .text_size(fs(FS_11))
                    .text_color(c(DANGER_TEXT))
                    .child(SharedString::from(error))
            }))
    }

    /// 选中一行之后能对它做的四件事。
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
            .child(div().flex_grow())
            .child(hint(self.rates_text()))
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
        let Some(search_ref) = parse_search_reference(&raw, &self.settings.league) else {
            self.watch_error = text.watches_invalid_search.to_owned();
            cx.notify();
            return;
        };
        let Some(cap) = text_of(&self.watches_form.cap, cx)
            .parse::<f64>()
            .ok()
            .filter(|amount| amount.is_finite() && *amount > 0.0)
        else {
            self.watch_error = text.watches_invalid_cap.to_owned();
            cx.notify();
            return;
        };
        let currency = self
            .watches_form
            .currency
            .read(cx)
            .selected_value()
            .map_or_else(|| "divine".to_owned(), ToString::to_string);

        let mut label = text_of(&self.watches_form.label, cx);
        if label.is_empty() {
            label = search_ref
                .search_id
                .chars()
                .take(LABEL_FROM_ID_CHARS)
                .collect();
        }

        let mut entry = WatchEntry::new(label, &search_ref, Price::from_trade(cap, &currency));
        entry.enabled = self.watches_form.enabled;
        entry.live = self.watches_form.live;
        self.push_log(format!("watch added: {} ({})", entry.label, entry.league));
        self.settings.watches.push(entry);

        self.watch_error.clear();
        if self.save_and_apply() {
            self.set_notice(text.watches_added.to_owned());
            self.watches_form.clear(window, cx);
        }
        self.watches_dirty = true;
        cx.notify();
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

    /// 还没接线的按钮:说一句"下一步才有",而不是假装什么都没发生。
    pub(crate) fn not_wired_yet(&mut self, what: &str, cx: &mut Context<Self>) {
        self.set_notice(format!("not wired yet: {what}"));
        cx.notify();
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

    fn status() -> BTreeMap<WatchId, WatchStatus> {
        BTreeMap::from([(
            WatchId("w-1".to_string()),
            WatchStatus {
                state: WatchRunState::Polling,
                next_poll_at: Some(1_000_090),
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

    /// 状态那一格要同时回答"现在在干嘛"和"下一轮什么时候"。
    #[test]
    fn the_status_cell_carries_the_countdown() {
        let rows = watch_rows(&settings(), &status(), &i18n::ENGLISH, 1_000_000);
        assert_eq!(rows[0][3].text(), "polling · next in 1 min");
        assert_eq!(
            rows[0][4].text(),
            crate::shell::link::local_clock(1_000_000)
        );
        assert_eq!(rows[0][5].text(), "4");
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
}
