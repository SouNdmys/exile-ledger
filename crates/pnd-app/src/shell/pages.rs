//! 五个页面,外加它们共用的那张表。
//!
//! 四页表格长得一样:一排列头、若干行文字、空的时候一句话。所以只有
//! **一个** `TableDelegate`,页面各自给它列和行 —— 四份几乎一样的委托
//! 抄下来,以后改一次表格外观就要改四处,总有一处会忘。
//!
//! 这一步的行是写死的示例数据。它不是占位横杠:列宽、对齐、颜色都要拿
//! 真实长度的字去量,空表量不出来。

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
    use super::*;
    use crate::i18n;

    /// 每一页的表在两种语言下,列数和行的格子数必须对得上 —— 少一个格子,
    /// 那一列在那一行就是空白,而空白看起来和"这里没有数据"一模一样。
    #[test]
    fn every_table_row_has_one_cell_per_column() {
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            for (name, content) in [
                ("watches", watches::table_content(text)),
                ("alerts", alerts::table_content(text)),
                ("uniques", ninja_uniques::table_content(text)),
                ("mods", ninja_mods::table_content(text)),
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
}
