//! 蹲价页:新增一条搜索,看它们各自跑到哪一步,以及限速预算还剩多少。
//!
//! 这一页是整个程序的主页。它回答两个问题:"我在盯着什么"和"程序还敢
//! 不敢再发请求" —— 第二个问题必须常驻,因为把交易站惹毛的代价不是报错,
//! 是账号。

use gpui::{
    AppContext as _, Context, Entity, ParentElement, SharedString, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{Sizable as _, Size, StyledExt as _};

use super::{Cell, TableContent, column, number_column};
use crate::i18n::{self, Text};
use crate::shell::{
    AppShell, Choice, ChoiceSelect, choice_select, field_label, field_row, hint, page_heading,
    panel, picker, table,
};
use crate::theme::*;

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
}

/// 价格上限能用的通货。交易站上还有别的,但拿它们当上限没有意义。
fn currency_choices() -> Vec<Choice> {
    vec![
        Choice::plain("divine"),
        Choice::plain("exalted"),
        Choice::plain("chaos"),
    ]
}

/// 蹲价表的列和示例行。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("label", text.watches_col_label, 200.),
            column("league", text.watches_col_league, 130.),
            number_column("cap", text.watches_col_cap, 90.),
            column("status", text.watches_col_status, 80.),
            column("last_poll", text.watches_col_last_poll, 120.),
            number_column("hits", text.watches_col_hits_today, 80.),
        ],
        // 一行真实长度的示例:列宽是拿它量出来的,空表量不出来。
        rows: vec![vec![
            Cell::plain("Choir of the Storm"),
            Cell::muted("Forbidden Rites"),
            Cell::data("20 divine"),
            Cell::good(text.status_live),
            Cell::data("12:04:31"),
            Cell::accent("2"),
        ]],
        empty: text.watches_empty.into(),
    }
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
            .child(self.budget_strip())
    }

    /// 新增表单:粘一条搜索进来,给它起个名字和一个价格上限。
    fn watches_add_form(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let enabled = self.watches_form.enabled;
        let live = self.watches_form.live;
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
                        Button::new("watch-poll-now")
                            .label(text.watches_poll_now)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.not_wired_yet(cx);
                            })),
                    )
                    .child(
                        Button::new("watch-add")
                            .primary()
                            .label(text.watches_add_button)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.not_wired_yet(cx);
                            })),
                    ),
            )
    }

    /// 预算条:两条策略各用掉多少,以及下一次什么时候能发。
    ///
    /// 数字是第 7b 步从网关的限速器来的;现在写的是 50% 预算下的额定值
    /// (6 小时 search 299 次 / fetch 499 次),先把位置和宽度占住。
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
                i18n::fill(text.budget_used_of, &["1", "299"]),
            ))
            .child(budget_item(
                text.budget_fetch,
                i18n::fill(text.budget_used_of, &["4", "499"]),
            ))
            .child(div().flex_grow())
            .child(hint(i18n::fill(text.budget_next_allowed, &["41"])))
    }

    /// 还没接线的按钮:说一句"下一步才有",而不是假装什么都没发生。
    pub(crate) fn not_wired_yet(&mut self, cx: &mut Context<Self>) {
        self.set_notice("runtime wiring lands in step 7b".to_owned());
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
