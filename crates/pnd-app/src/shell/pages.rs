//! 五个页面,外加它们共用的那张表。
//!
//! 四页表格长得一样:一排列头、若干行文字、空的时候一句话。所以只有
//! **一个** `TableDelegate`,页面各自给它列和行 —— 四份几乎一样的委托
//! 抄下来,以后改一次表格外观就要改四处,总有一处会忘。
//!
//! 四页的行现在都是真数据:蹲价和提醒记录来自设置 + actor 状态 +
//! `watch.sqlite`,暗金热度和词缀热度来自 `ninja.sqlite` 里的采样缓存。
//! 每一页的"行构造器"都是纯函数(输入是数据,输出是格子),所以列宽、
//! 对齐、每一格写什么都测得动。

pub mod alerts;
pub mod ninja_mods;
pub mod ninja_uniques;
pub mod settings;
pub mod watches;

use gpui::{App, Context, IntoElement, ParentElement, SharedString, Styled, Window, div, px};
use gpui_component::StyledExt as _;
use gpui_component::table::{Column, TableDelegate, TableState};

use crate::theme::*;

/// 一个格子的语气。颜色不写在页面里,写在这儿:改配色只动 `theme.rs`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// 正文。
    Plain,
    /// 安静的灰:单位、脚注、"—"。
    Muted,
    /// 数字:等宽,才对得齐。
    Data,
    /// 金色:强调的那一格(命中价、榜首)。
    Accent,
    /// 绿:命中、live 健康。
    Good,
    /// 琥珀:退避、过期、预算吃紧。
    Warn,
}

/// 一个格子。
#[derive(Clone, Debug)]
pub struct Cell {
    text: SharedString,
    tone: Tone,
}

impl Cell {
    pub fn new(text: impl Into<SharedString>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }

    pub fn plain(text: impl Into<SharedString>) -> Self {
        Self::new(text, Tone::Plain)
    }

    pub fn muted(text: impl Into<SharedString>) -> Self {
        Self::new(text, Tone::Muted)
    }

    pub fn data(text: impl Into<SharedString>) -> Self {
        Self::new(text, Tone::Data)
    }

    pub fn accent(text: impl Into<SharedString>) -> Self {
        Self::new(text, Tone::Accent)
    }

    pub fn good(text: impl Into<SharedString>) -> Self {
        Self::new(text, Tone::Good)
    }

    pub fn warn(text: impl Into<SharedString>) -> Self {
        Self::new(text, Tone::Warn)
    }

    /// 格子里的字。测试查它 —— "这一格写的是什么"是唯一值得断言的东西。
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn tone(&self) -> Tone {
        self.tone
    }
}

/// 一张表的全部内容:列、行、空的时候说什么。
pub struct TableContent {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub empty: SharedString,
}

/// 一列。宽度是像素:上游的表格不会自己回流,列宽加起来必须放得进面板。
pub fn column(key: &'static str, name: &'static str, width: f32) -> Column {
    Column::new(key, name).width(px(width))
}

/// 一列右对齐的数字。
pub fn number_column(key: &'static str, name: &'static str, width: f32) -> Column {
    column(key, name, width).text_right()
}

/// 四页共用的表格委托。
pub struct SimpleTable {
    content: TableContent,
}

impl SimpleTable {
    pub fn new(content: TableContent) -> Self {
        Self { content }
    }

    /// 换语言(或者第 7b 步换成真数据)时整份换掉。
    pub fn set_content(&mut self, content: TableContent) {
        self.content = content;
    }
}

impl TableDelegate for SimpleTable {
    fn columns_count(&self, _: &App) -> usize {
        self.content.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.content.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.content.columns[col_ix]
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(cell) = self
            .content
            .rows
            .get(row_ix)
            .and_then(|row| row.get(col_ix))
        else {
            return div();
        };
        let body = div()
            .h_flex()
            .items_center()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(cell.text.clone());
        match cell.tone {
            Tone::Plain => body.text_size(fs(FS_12)).text_color(c(TEXT_PRIMARY)),
            Tone::Muted => body.text_size(fs(FS_11_5)).text_color(muted()),
            Tone::Data => body
                .font_family(FONT_MONO)
                .text_size(fs(FS_11))
                .text_color(c(TEXT_DATA)),
            Tone::Accent => body.text_size(fs(FS_12)).text_color(c(ACCENT_TEXT)),
            Tone::Good => body.text_size(fs(FS_12)).text_color(hit_green()),
            Tone::Warn => body.text_size(fs(FS_12)).text_color(warn_amber()),
        }
    }

    /// 空表说人话。上游默认画一个收件箱图标,那张图不告诉任何人下一步该做什么。
    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .h_flex()
            .justify_center()
            .p(px(24.))
            .text_size(fs(FS_11_5))
            .text_color(muted())
            .child(self.content.empty.clone())
    }
}

#[cfg(test)]
mod pages_tests {
    use std::collections::BTreeMap;

    use pnd_domain::{Currency, Price, WatchId};
    use pnd_settings::{AppSettings, WatchEntry};
    use pnd_storage::{AlertRow, AlertSource};

    use super::*;
    use crate::i18n;

    /// 一条搜索 + 一条它的运行状态,拿来量蹲价表。
    fn watch_content(text: &'static i18n::Text) -> TableContent {
        let settings = AppSettings {
            watches: vec![WatchEntry {
                id: WatchId("w-1".to_string()),
                label: "Choir of the Storm".to_string(),
                league: "Forbidden Rites".to_string(),
                price_cap: Price::new(20_000, Currency::Divine),
                ..WatchEntry::default()
            }],
            ..AppSettings::default()
        };
        let status = BTreeMap::from([(
            WatchId("w-1".to_string()),
            pnd_runtime::WatchStatus {
                state: pnd_runtime::WatchRunState::Polling,
                next_poll_at: Some(1_000_100),
                last_poll_at: Some(1_000_000),
                hits_today: 2,
                ..pnd_runtime::WatchStatus::default()
            },
        )]);
        watches::table_content_for(&settings, &status, text, 1_000_000)
    }

    /// 一件暗金 + 它的参考价,拿来量暗金榜。
    fn unique_content(text: &'static i18n::Text) -> TableContent {
        let rows = vec![crate::shell::ninja::UniqueRow {
            name: "Wake of Destruction".to_string(),
            users: 7_464,
            share_percent: 11.42,
            price_milli: Some(29_900),
            listings: Some(131),
            change_percent: Some(-4.5),
        }];
        ninja_uniques::table_content_for(&rows, &pnd_domain::CurrencyRates::none(), false, text)
    }

    /// 一行词缀统计,拿来量词缀表。
    fn mod_content(text: &'static i18n::Text) -> TableContent {
        let stats = vec![pnd_ninja::aggregate::SlotModStat {
            slot: "BodyArmour".to_string(),
            rarity: "Rare".to_string(),
            mod_kind: "explicit".to_string(),
            stat_id: "base_maximum_life".to_string(),
            mod_family: "IncreasedLife".to_string(),
            characters: 13,
            occurrences: 13,
            sample_size: 47,
            p25: Some(108.0),
            p50: Some(176.0),
            p75: Some(211.0),
        }];
        ninja_mods::table_content_for(&stats, "BodyArmour", "", "", false, text)
    }

    /// 一条提醒历史,拿来量提醒表。
    fn alert_content(text: &'static i18n::Text) -> TableContent {
        let rows = vec![AlertRow {
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
        }];
        alerts::table_content_for(&rows, text)
    }

    /// 每一页的表在两种语言下,列数和行的格子数必须对得上 —— 少一个格子,
    /// 那一列在那一行就是空白,而空白看起来和"这里没有数据"一模一样。
    #[test]
    fn every_table_row_has_one_cell_per_column() {
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            for (name, content) in [
                ("watches", watch_content(text)),
                ("alerts", alert_content(text)),
                ("uniques", unique_content(text)),
                ("mods", mod_content(text)),
            ] {
                assert!(!content.columns.is_empty(), "{name} 一列都没有");
                assert!(
                    !content.empty.trim().is_empty(),
                    "{name} 空表时没有话说 ({language})"
                );
                for (index, row) in content.rows.iter().enumerate() {
                    assert_eq!(
                        row.len(),
                        content.columns.len(),
                        "{name} 第 {index} 行的格子数对不上列数 ({language})"
                    );
                }
            }
        }
    }

    /// 上面那条只在"有行"时才检查得到东西 —— 行构造器要是哪天返回了空表,
    /// 它会一声不响地通过。这条守住"确实造出了行"。
    #[test]
    fn the_real_row_builders_produce_rows() {
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            assert_eq!(watch_content(text).rows.len(), 1);
            assert_eq!(alert_content(text).rows.len(), 1);
            assert_eq!(unique_content(text).rows.len(), 1);
            assert_eq!(mod_content(text).rows.len(), 1);
        }
    }
}
