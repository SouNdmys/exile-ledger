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
pub mod observations;
pub mod settings;
pub mod watches;

use gpui::{
    App, Context, Div, InteractiveElement as _, IntoElement, ParentElement, Pixels, SharedString,
    Stateful, Styled, Window, div, px,
};
use gpui_component::StyledExt as _;
use gpui_component::table::{Column, TableDelegate, TableState};

use crate::i18n::{self, Text};
use crate::theme::*;
use pnd_domain::Game;

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

impl Tone {
    /// 这个语气用哪个颜色槽位。
    ///
    /// 单独一个纯函数(返回槽位号,不返回画笔)是为了测得动:格子的字压在
    /// 面板、斑马行、以及这两种被选中之后的底色上都得读得清,而那是一道
    /// 算术题,不是一件要开窗才看得见的事。
    #[must_use]
    pub const fn color(self) -> u32 {
        match self {
            Self::Plain => TEXT_PRIMARY,
            Self::Muted => TEXT_META,
            Self::Data => TEXT_DATA,
            Self::Accent => ACCENT_TEXT,
            Self::Good => FRESH,
            Self::Warn => WARN,
        }
    }

    /// 这个语气用多大的字。
    fn font_size(self) -> gpui::Pixels {
        match self {
            Self::Muted => fs(FS_11_5),
            Self::Data => fs(FS_11),
            _ => fs(FS_12),
        }
    }

    /// 数字要等宽,才对得齐。
    const fn monospaced(self) -> bool {
        matches!(self, Self::Data)
    }

    /// 每一种语气,给测试遍历用。少一种,那一种就没人查它读不读得清。
    #[cfg(test)]
    const ALL: [Self; 6] = [
        Self::Plain,
        Self::Muted,
        Self::Data,
        Self::Accent,
        Self::Good,
        Self::Warn,
    ];
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

/// 表里"联赛"那一格的字。
///
/// PoE1 和 PoE2 都有 Standard、Hardcore,只写联赛名分不出这条搜索盯的是哪个
/// 游戏。PoE2 是常态,写出来只会让每一行都变长,所以只有 PoE1 才加前缀。
pub fn league_cell_text(game: Game, league: &str, text: &'static Text) -> String {
    match game {
        Game::Poe2 => league.to_owned(),
        Game::Poe1 => i18n::fill(text.common_game_league, &[text.common_game_poe1, league]),
    }
}

/// 四页共用的表格委托。
pub struct SimpleTable {
    content: TableContent,
}

impl SimpleTable {
    pub fn new(content: TableContent) -> Self {
        Self { content }
    }

    /// 换语言(或者数据变了)时整份换掉。返回**列头要不要重建一遍**。
    ///
    /// 为什么要这么个返回值:用户拖出来的列宽只活在上游表格自己那份运行时
    /// 列组里(`col_groups`),委托手上这份是不动的。而上游一重建列头
    /// (`TableState::refresh`)就回到委托这儿把宽度重读一遍 —— 观察页的
    /// 数据每秒变一次,于是每秒把用户拖出来的宽度弹回默认值一次。行变了、
    /// 列没变的那些刷新干脆不让它重建,拖出来的宽度就留住了。
    ///
    /// key 一样就是同一组列(换语言、换选中的那条观察都算),这时候把上一
    /// 份宽度接过来:列头还是得重建(文字变了),但重读到的是用户的宽度。
    #[must_use]
    pub fn set_content(&mut self, mut content: TableContent) -> bool {
        if same_keys(&self.content.columns, &content.columns) {
            for (column, previous) in content.columns.iter_mut().zip(&self.content.columns) {
                column.width = previous.width;
            }
        }
        let rebuild = !same_columns(&self.content.columns, &content.columns);
        self.content = content;
        rebuild
    }

    /// 用户拖完列宽的那一下,把上游那份新宽度抄回来。
    ///
    /// 上游只改它自己那份运行时宽度,不回头动委托;不抄回来的话,下一次
    /// 真的需要重建列头(换语言)时重读到的还是默认值,拖过的宽度当场作废。
    pub fn remember_widths(&mut self, widths: &[Pixels]) {
        for (column, width) in self.content.columns.iter_mut().zip(widths) {
            column.width = *width;
        }
    }
}

/// 两组列是不是同一组列:同一批 key、同样的顺序。
fn same_keys(left: &[Column], right: &[Column]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.key == right.key)
}

/// 两组列画出来一模一样吗。`Column` 没有 `PartialEq`,所以逐项比 ——
/// 列头上看得见的就是 key、名字、宽度和对齐这四样。
fn same_columns(left: &[Column], right: &[Column]) -> bool {
    same_keys(left, right)
        && left.iter().zip(right).all(|(left, right)| {
            left.name == right.name && left.width == right.width && left.align == right.align
        })
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
        let tone = cell.tone;
        let body = div()
            .h_flex()
            .items_center()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(tone.font_size())
            .text_color(c(tone.color()))
            .child(cell.text.clone());
        if tone.monospaced() {
            body.font_family(FONT_MONO)
        } else {
            body
        }
    }

    /// 斑马行的底色。
    ///
    /// 为什么自己画而不是开上游的 `stripe`:上游那个开关顺带会在数据行下面
    /// 补一批空的假行,把整张表铺满 —— 屏幕上就是一串没有内容的条纹,看起来
    /// 像"还有几条正在加载"。关掉 `stripe` 就没有假行了,条纹自己在这儿画。
    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let row = div().id(("row", row_ix));
        if row_ix % 2 == 1 {
            row.bg(c(ZEBRA))
        } else {
            row
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

    /// PoE2 是常态,不加前缀;PoE1 要看得出来,两个游戏都有 Standard。
    #[test]
    fn only_a_poe1_league_says_which_game_it_is() {
        assert_eq!(
            league_cell_text(Game::Poe2, "Forbidden Rites", &i18n::ENGLISH),
            "Forbidden Rites"
        );
        assert_eq!(
            league_cell_text(Game::Poe1, "Standard", &i18n::ENGLISH),
            "PoE1 · Standard"
        );
        assert_eq!(
            league_cell_text(Game::Poe1, "Standard", &i18n::SIMPLIFIED_CHINESE),
            "PoE1 · Standard",
            "游戏名不翻译"
        );
    }

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
            price_currency: Some(pnd_domain::Currency::Divine),
            listings: Some(131),
            change_percent: Some(-4.5),
        }];
        ninja_uniques::table_content_for(&rows, &pnd_domain::CurrencyRates::none(), false, text)
    }

    /// 一行词缀统计,拿来量词缀表。
    fn mod_content(text: &'static i18n::Text) -> TableContent {
        let stats = vec![pnd_ninja::aggregate::SlotModStat {
            class: String::new(),
            slot: "BodyArmour".to_string(),
            rarity: "Rare".to_string(),
            mod_kind: "explicit".to_string(),
            stat_id: "base_maximum_life".to_string(),
            display: "+# to maximum Life".to_string(),
            value_index: 1,
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

    /// 一条观察 + 它的运行状态,拿来量观察表。
    fn observation_content(text: &'static i18n::Text) -> TableContent {
        let settings = AppSettings {
            observations: vec![pnd_settings::ObservationEntry {
                id: pnd_domain::ObservationId("o-1".to_string()),
                label: "Precursor Tablets".to_string(),
                league: "Forbidden Rites".to_string(),
                ..pnd_settings::ObservationEntry::default()
            }],
            ..AppSettings::default()
        };
        let status = BTreeMap::from([(
            pnd_domain::ObservationId("o-1".to_string()),
            pnd_runtime::ObservationStatus {
                active: 42,
                gone: 17,
                next_recheck_at: Some(1_000_090),
                ..pnd_runtime::ObservationStatus::default()
            },
        )]);
        observations::table_content_for(&settings, &status, text, 1_000_000)
    }

    /// 一条词缀战绩,拿来量观察页那张聚合表。
    fn observation_mod_content(text: &'static i18n::Text) -> TableContent {
        let outcomes = vec![pnd_storage::ModOutcome {
            template: "+# to maximum Life".to_string(),
            mod_kind: "explicit".to_string(),
            seen: 30,
            gone: 22,
            sold_likely: 20,
            currency: "exalted".to_string(),
            median_gone_price_milli: Some(12_500),
            median_active_price_milli: Some(20_000),
            median_hours_alive: Some(3.5),
        }];
        observations::mods_table_content_for(
            &outcomes,
            "",
            1,
            &observations::FavouriteMods::default(),
            false,
            text,
        )
    }

    /// 一档价位战绩,拿来量观察页那张价位表。
    fn observation_price_content(text: &'static i18n::Text) -> TableContent {
        let outcomes = vec![pnd_storage::PriceOutcome {
            currency: "divine".to_string(),
            bucket_milli: 2_000,
            seen: 8,
            gone: 6,
            looks_sold: 6,
            median_lifetime_secs: Some(12_600),
            active: 2,
        }];
        observations::price_table_content_for(&outcomes, 1, text)
    }

    /// 一条提醒历史,拿来量提醒表。
    fn alert_content(text: &'static i18n::Text) -> TableContent {
        let rows = vec![AlertRow {
            alert_id: 1,
            watch_id: WatchId("w-1".to_string()),
            listing_id: "abc".to_string(),
            game: pnd_domain::Game::Poe2,
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
                ("observations", observation_content(text)),
                ("observation mods", observation_mod_content(text)),
                ("observation prices", observation_price_content(text)),
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

    /// 每一种语气的字,压在表格真会出现的四种底色上都得读得清。
    ///
    /// 四种底是:面板(普通行)、斑马行,以及这两种被选中时那块半透明覆盖层
    /// 压完之后的颜色。选中那两档是这条测试的由来 —— 屏幕上出现过"选中了、
    /// 一个字都看不见"的行,而那正是"字色和它身下的底色撞了"的样子。
    #[test]
    fn every_tone_reads_on_every_row_background() {
        use crate::theme::{
            PANEL, SELECTED_ALPHA, SELECTED_WASH, ZEBRA, blend, contrast_ratio, selected_row_bg,
        };

        for tone in Tone::ALL {
            // 安静的灰本来就是"次要信息",3:1 是它的线;别的按正文算。
            let floor = if tone == Tone::Muted { 3.0 } else { 4.5 };
            for (name, background) in [
                ("面板", PANEL),
                ("斑马行", ZEBRA),
                ("选中行", selected_row_bg()),
                ("选中的斑马行", blend(SELECTED_WASH, SELECTED_ALPHA, ZEBRA)),
            ] {
                let ratio = contrast_ratio(tone.color(), background);
                assert!(
                    ratio >= floor,
                    "{tone:?} 压在{name}上只有 {ratio:.2}:1(要 {floor}:1)"
                );
            }
        }
    }

    /// 一张小表,拿来量列宽:列由调用方给,行只是"数据变了"的占位。
    fn widths_content(columns: Vec<Column>, rows: usize) -> TableContent {
        TableContent {
            columns,
            rows: (0..rows)
                .map(|row| vec![Cell::plain(row.to_string())])
                .collect(),
            empty: "nothing yet".into(),
        }
    }

    /// 用户拖宽的那一列,不该被一次刷新弹回默认宽度。
    ///
    /// 上游把"现在这一列多宽"存在表格自己那边,只有重建列头
    /// (`TableState::refresh`)时才回到委托这儿重读一遍 —— 而观察页的数据
    /// 每秒变一次。所以委托这一侧要做两件事:**行变了列没变就说"不用重建"**,
    /// 以及列真的变了(换语言把表头换成另一种文字)时,把上一份宽度接过来。
    #[test]
    fn a_resized_column_keeps_its_width_across_a_refresh() {
        let english = || {
            vec![
                column("label", "Label", 180.),
                column("league", "League", 120.),
            ]
        };
        let mut table = SimpleTable::new(widths_content(english(), 0));

        // 用户把第一列从 180 拖到 300。
        table.remember_widths(&[px(300.), px(120.)]);

        // 数据变了、列没变:不用重建列头,拖出来的宽度就没人动得了。
        assert!(
            !table.set_content(widths_content(english(), 3)),
            "只有行变了,不该重建列头"
        );
        assert_eq!(table.content.columns[0].width, px(300.));

        // 换语言:表头文字变了,列头得重建;但 key 没变,还是同一组列,
        // 宽度接着用。
        let chinese = vec![
            column("label", "备注", 180.),
            column("league", "联赛", 120.),
        ];
        assert!(
            table.set_content(widths_content(chinese, 3)),
            "表头文字变了,得重建列头"
        );
        assert_eq!(table.content.columns[0].width, px(300.));

        // 换成另一组列(整张表换了):宽度回到新那组列自己的默认值,
        // 而不是把上一张表的宽度硬套过来。
        let other = vec![column("template", "Modifier", 90.)];
        assert!(table.set_content(widths_content(other, 1)));
        assert_eq!(table.content.columns[0].width, px(90.));
    }

    /// 上面那条只在"有行"时才检查得到东西 —— 行构造器要是哪天返回了空表,
    /// 它会一声不响地通过。这条守住"确实造出了行"。
    #[test]
    fn the_real_row_builders_produce_rows() {
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            assert_eq!(watch_content(text).rows.len(), 1);
            assert_eq!(alert_content(text).rows.len(), 1);
            assert_eq!(observation_content(text).rows.len(), 1);
            assert_eq!(observation_mod_content(text).rows.len(), 1);
            assert_eq!(observation_price_content(text).rows.len(), 1);
            assert_eq!(unique_content(text).rows.len(), 1);
            assert_eq!(mod_content(text).rows.len(), 1);
        }
    }
}
