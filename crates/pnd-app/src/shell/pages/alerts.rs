//! 提醒记录页:弹过的每一张卡片,以及当时你拿它做了什么。
//!
//! 卡片是会自己收起来的,所以它必须在别处留下痕迹 —— 否则"刚才那件东西
//! 多少钱来着"就永远查不回来了。四个按钮和卡片上的是同一套动作,理由是
//! 卡片收起来之后你还想去买。
//!
//! 行来自 `watch.sqlite` 的 `alerts` 表(最近 200 条),命中一次就重读一次;
//! 按钮作用在选中的那一行。

use gpui::{Context, ParentElement, SharedString, Styled, div, px};
use gpui_component::button::Button;
use gpui_component::{Disableable as _, Sizable as _, Size};

use pnd_runtime::RuntimeCommand;
use pnd_storage::{AlertRow, AlertSource};

use super::{Cell, TableContent, column, number_column};
use crate::i18n::Text;
use crate::shell::link::{hideout_action_text, local_stamp};
use crate::shell::{AppShell, hint, page_heading, panel, table};

/// 提醒表的列。行由 [`table_content_for`] 填。
pub fn table_content(text: &'static Text) -> TableContent {
    TableContent {
        columns: vec![
            column("time", text.alerts_col_time, 120.),
            column("item", text.alerts_col_item, 200.),
            number_column("price", text.alerts_col_price, 90.),
            column("seller", text.alerts_col_seller, 150.),
            column("source", text.alerts_col_source, 70.),
            // "去藏身处:正在换 token" 是这一列里最长的一句,窄了会被裁掉。
            column("action", text.alerts_col_action, 200.),
        ],
        rows: Vec::new(),
        empty: text.alerts_empty.into(),
    }
}

/// 列 + 真行。顺序就是库里的顺序(最新的在最前),按钮靠行号找回是哪一条。
pub fn table_content_for(rows: &[AlertRow], text: &'static Text) -> TableContent {
    TableContent {
        rows: rows.iter().map(|row| alert_cells(row, text)).collect(),
        ..table_content(text)
    }
}

fn alert_cells(row: &AlertRow, text: &'static Text) -> Vec<Cell> {
    vec![
        Cell::data(local_stamp(row.fired_at)),
        Cell::plain(row.item_name.clone()),
        match &row.price {
            // 记录里的每一条都是当时判定"在上限内"的,所以价格是绿的。
            Some(price) => Cell::good(price.display()),
            None => Cell::muted(text.common_none),
        },
        Cell::muted(row.account.clone()),
        Cell::muted(match row.source {
            AlertSource::Poll => text.alerts_source_poll,
            AlertSource::Live => text.alerts_source_live,
        }),
        Cell::muted(action_text(row, text)),
    ]
}

/// "动作"那一格。
///
/// 动作码有两个来源:界面自己写的("open"/"copy")和 runtime 写的
/// (`HideoutOutcome::action()` 那几个 `hideout_*`)。两边都认得;认不出来的
/// 原样显示 —— 以后加了新动作忘了翻译,至少看得出发生过什么。
fn action_text(row: &AlertRow, text: &'static Text) -> String {
    match row.last_action.as_deref() {
        Some("open") => text.alerts_action_opened.to_owned(),
        Some("copy") => text.alerts_action_copied.to_owned(),
        Some(other) => {
            hideout_action_text(other, text).map_or_else(|| other.to_owned(), ToOwned::to_owned)
        }
        None if row.dismissed_at.is_some() => text.alerts_action_dismissed.to_owned(),
        None => text.alerts_action_none.to_owned(),
    }
}

impl AppShell {
    pub(crate) fn render_alerts(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let selected = self.selected_alert(cx).map_or_else(
            || text.common_select_row.to_owned(),
            |row| format!("{} · {}", local_stamp(row.fired_at), row.item_name),
        );
        // 去藏身处必须带会话:没有 cookie 就把按钮变灰,并在下面说明为什么。
        // 让它能按、按了再回一句"没有会话",是白让人点一下。
        let no_session = self.settings.poesessid.trim().is_empty();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .child(page_heading(text.alerts_heading, text.alerts_subtitle))
            .child(
                panel()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(table(&self.alerts_table)),
            )
            .child(
                // 选中一行之后对它做的四件事,和卡片上那四个按钮一一对应。
                panel()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(crate::theme::fs(crate::theme::FS_11_5))
                            .text_color(crate::theme::muted())
                            .child(SharedString::from(selected)),
                    )
                    .child(
                        Button::new("alert-open")
                            .label(text.alerts_open)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.open_selected_alert(cx))),
                    )
                    .child(
                        Button::new("alert-copy-whisper")
                            .label(text.alerts_copy_whisper)
                            .with_size(Size::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_selected_alert(cx))),
                    )
                    .child(
                        Button::new("alert-hideout")
                            .label(text.alerts_hideout)
                            .with_size(Size::Small)
                            .disabled(no_session)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.hideout_selected_alert(cx)),
                            ),
                    )
                    .child(
                        Button::new("alert-dismiss")
                            .label(text.alerts_dismiss)
                            .with_size(Size::Small)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.dismiss_selected_alert(cx)),
                            ),
                    ),
            )
            .children(no_session.then(|| hint(text.alerts_hideout_needs_session)))
    }

    /// 表格里选中的那条提醒。
    fn selected_alert(&self, cx: &mut Context<Self>) -> Option<&AlertRow> {
        let row = self.alerts_table.read(cx).selected_row()?;
        self.alert_rows.get(row)
    }

    /// 打开这条提醒当时那次搜索的交易页。按价升序,那件多半还在最上面。
    fn open_selected_alert(&mut self, cx: &mut Context<Self>) {
        let Some((alert_id, league, search_id)) = self
            .selected_alert(cx)
            .map(|row| (row.alert_id, row.league.clone(), row.search_id.clone()))
        else {
            self.select_an_alert_first(cx);
            return;
        };
        self.open_trade_page(&league, &search_id);
        self.record_action(alert_id, "open");
        cx.notify();
    }

    fn copy_selected_alert(&mut self, cx: &mut Context<Self>) {
        let Some((alert_id, whisper)) = self
            .selected_alert(cx)
            .map(|row| (row.alert_id, row.whisper.clone()))
        else {
            self.select_an_alert_first(cx);
            return;
        };
        self.copy_whisper(&whisper, cx);
        self.record_action(alert_id, "copy");
        cx.notify();
    }

    /// 给这条提醒的卖家发一次传送请求。**按一次发一次**,程序自己不会补第二次。
    fn hideout_selected_alert(&mut self, cx: &mut Context<Self>) {
        let Some(alert_id) = self.selected_alert(cx).map(|row| row.alert_id) else {
            self.select_an_alert_first(cx);
            return;
        };
        self.travel_to_hideout(alert_id);
        cx.notify();
    }

    fn dismiss_selected_alert(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(alert_id) = self.selected_alert(cx).map(|row| row.alert_id) else {
            self.select_an_alert_first(cx);
            return;
        };
        if self.send_runtime(RuntimeCommand::Dismiss { alert_id }) {
            self.set_notice(text.notice_dismissed.to_owned());
        }
        self.refresh_alerts_soon();
        cx.notify();
    }

    fn select_an_alert_first(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        self.set_notice(text.common_select_row.to_owned());
        cx.notify();
    }
}

#[cfg(test)]
mod alerts_page_tests {
    use pnd_domain::{Currency, Price, WatchId};

    use super::*;
    use crate::i18n;

    fn row() -> AlertRow {
        AlertRow {
            alert_id: 1,
            watch_id: WatchId("w-1".to_string()),
            listing_id: "abc".to_string(),
            league: "Forbidden Rites".to_string(),
            search_id: "H4sIAAAA-_09".to_string(),
            item_name: "Choir of the Storm".to_string(),
            price: Some(Price::new(18_000, Currency::Divine)),
            account: "Exile#1234".to_string(),
            character: "ExileChar".to_string(),
            whisper: "@ExileChar hi".to_string(),
            hideout_token: None,
            token_fetched_at: None,
            source: AlertSource::Live,
            fired_at: 1_000_000,
            dismissed_at: None,
            last_action: None,
        }
    }

    #[test]
    fn a_row_shows_what_fired_and_for_how_much() {
        let cells = alert_cells(&row(), &i18n::ENGLISH);
        assert_eq!(cells[0].text(), local_stamp(1_000_000));
        assert_eq!(cells[1].text(), "Choir of the Storm");
        assert_eq!(cells[2].text(), "18 divine");
        assert_eq!(cells[3].text(), "Exile#1234");
        assert_eq!(cells[4].text(), "live");
        assert_eq!(cells[5].text(), "—");
    }

    /// 无价挂单也会进历史(判定是 Unpriced,不会提醒),读回来不能是那个
    /// 表示"没有价格"的哨兵数字。
    #[test]
    fn an_unpriced_alert_shows_a_dash() {
        let mut row = row();
        row.price = None;
        assert_eq!(alert_cells(&row, &i18n::ENGLISH)[2].text(), "—");
    }

    /// 动作码 → 人话。认不出来的原样显示,不吞。
    #[test]
    fn the_action_column_translates_the_codes_we_write() {
        let text = &i18n::ENGLISH;
        let mut row = row();
        assert_eq!(action_text(&row, text), "—");

        row.dismissed_at = Some(1_000_500);
        assert_eq!(action_text(&row, text), "dismissed");

        for (code, shown) in [
            ("open", "opened"),
            ("copy", "whisper copied"),
            ("hideout_sent", "hideout: sent"),
            ("hideout_no_session", "hideout: no session"),
            ("something-new", "something-new"),
        ] {
            row.last_action = Some(code.to_string());
            assert_eq!(action_text(&row, text), shown);
        }
    }

    /// runtime 写进 `last_action` 的每一个码,这一页都得认得。
    ///
    /// 码是 `HideoutOutcome::action()` 定的:直接遍历它,而不是照着抄一份
    /// 清单 —— 抄的那份迟早会落后,而落后的表现是动作列上突然冒出
    /// `hideout_token_missing` 这种给机器看的字。
    #[test]
    fn every_hideout_outcome_code_has_a_translation() {
        let outcomes = [
            pnd_runtime::HideoutOutcome::Sent,
            pnd_runtime::HideoutOutcome::NoSession,
            pnd_runtime::HideoutOutcome::TokenMissing,
            pnd_runtime::HideoutOutcome::Refreshed,
            pnd_runtime::HideoutOutcome::Failed {
                status: 503,
                message: "busy".to_string(),
            },
        ];
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            let mut row = row();
            for outcome in &outcomes {
                let code = outcome.action();
                row.last_action = Some(code.to_string());
                let shown = action_text(&row, text);
                assert_ne!(shown, code, "{language} 的 {code} 没翻译");
                assert!(!shown.trim().is_empty());
            }
        }
    }
}
