//! 外壳:左边五个导航按钮,右边当前页,底下一条状态行。
//!
//! 五页画的都是真数据:搜索列表来自 `settings.json`,运行状态和预算来自
//! `pnd-runtime` 的 actor,提醒历史来自 `watch.sqlite`,暗金热度和词缀热度
//! 来自 `ninja.sqlite` 里那份采样缓存。
//!
//! `tick` 是外壳唯一的心跳(120ms)。运行时事件、卡片按钮的回声、ninja
//! 采样的进度都从这里抽干,理由和兄弟项目一样:GPUI 的视图只能在它自己的
//! 线程上改,后台线程只能把消息塞进通道,总得有人定期来取。交易那一侧的
//! 接线在 [`link`] 里,ninja 那一侧在 [`ninja`] 里。

pub mod link;
pub mod ninja;
pub mod pages;

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement, Render, SharedString, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::select::{SearchableVec, SelectEvent, SelectItem, SelectState};
use gpui_component::table::TableState;
use gpui_component::{IndexPath, Selectable as _, Sizable as _, Size, StyledExt as _};

use pnd_domain::{CurrencyRates, WatchId};
use pnd_platform_win::AlertCardService;
use pnd_runtime::{MatchedListing, RuntimeHandle, SamplerHandle, WatchStatus, now_secs};
use pnd_storage::{AlertRow, NinjaStore, WatchStore};
use pnd_trade::BucketUsage;

use crate::i18n;
use crate::theme::*;
use ninja::NinjaData;
use pages::SimpleTable;

/// 状态行只显示最后一条,但留一小段历史,方便将来做"日志"抽屉。
const LOG_CAPACITY: usize = 120;
/// 左导航宽度。中文四个字 + 内边距,英文最长的 "Modifier heat" 也放得下。
const W_NAV: f32 = 136.0;
/// 一句话通知("已保存")挂多久。够看清,又不会一直杵在那儿。
const NOTICE_LIFETIME: Duration = Duration::from_secs(4);

/// 五个页面。顺序就是导航顺序:每天先看蹲价,再看它响过什么,
/// 然后才是 ninja 那两张榜,设置沉底。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Watches,
    Alerts,
    NinjaUniques,
    NinjaMods,
    Settings,
}

impl Page {
    pub const ALL: [Self; 5] = [
        Self::Watches,
        Self::Alerts,
        Self::NinjaUniques,
        Self::NinjaMods,
        Self::Settings,
    ];

    fn label(self, text: &'static i18n::Text) -> &'static str {
        match self {
            Self::Watches => text.nav_watches,
            Self::Alerts => text.nav_alerts,
            Self::NinjaUniques => text.nav_ninja_uniques,
            Self::NinjaMods => text.nav_ninja_mods,
            Self::Settings => text.nav_settings,
        }
    }

    /// 和界面语言无关的元素 id。id 要是跟着语言走,换一次语言在框架看来
    /// 就是换了一整套控件,焦点和点击状态会跟着丢。
    const fn element_id(self) -> &'static str {
        match self {
            Self::Watches => "nav-watches",
            Self::Alerts => "nav-alerts",
            Self::NinjaUniques => "nav-ninja-uniques",
            Self::NinjaMods => "nav-ninja-mods",
            Self::Settings => "nav-settings",
        }
    }
}

/// 下拉里的一项:屏幕上写 `label`,程序里存 `value`。
///
/// 分开是因为这两件事经常不是一回事 —— 语言选择器上写的是"简体中文",
/// 存进 `settings.json` 的是 `zh`;卡片角落写的是"右下",存的是
/// `bottom_right`。用上游的 `SharedString` 当选项就只有一个字符串,
/// 于是得靠显示文字反查设置值,换个语言就反查不到了。
#[derive(Clone, Debug)]
pub struct Choice {
    value: SharedString,
    label: SharedString,
}

impl Choice {
    pub fn new(value: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }

    /// 选项的显示文字和存储值一样(联赛名、职业名这种专有名词)。
    pub fn plain(value: impl Into<SharedString>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
        }
    }
}

impl SelectItem for Choice {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// 一个下拉的状态。
pub type ChoiceSelect = Entity<SelectState<SearchableVec<Choice>>>;

/// 造一个下拉,并把当前值选上。
///
/// 选不中(设置里存着一个选项里没有的值)就不选 —— 下拉显示占位符,
/// 好过悄悄替用户改成第一项。
pub fn choice_select(
    items: Vec<Choice>,
    selected: &str,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> ChoiceSelect {
    let index = items
        .iter()
        .position(|item| item.value == selected)
        .map(|row| IndexPath::default().row(row));
    cx.new(|cx| SelectState::new(SearchableVec::new(items), index, window, cx))
}

/// 造一张表。四页共用同一份委托,差别只在列和行的内容。
fn new_table(
    content: pages::TableContent,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> Entity<TableState<SimpleTable>> {
    cx.new(|cx| {
        TableState::new(SimpleTable::new(content), window, cx)
            .row_selectable(true)
            .col_selectable(false)
    })
}

/// 换语言之后把一个下拉的选项文字重造一遍。
///
/// `selected` 给 `None` 就保留现在选着的那一项 —— 换个界面语言不该顺手把
/// 用户筛好的职业清回"全部"。
pub(crate) fn relabel_select(
    select: &ChoiceSelect,
    items: Vec<Choice>,
    selected: Option<&str>,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) {
    let selected = selected.map(|value| SharedString::from(value.to_owned()));
    select.update(cx, |state, cx| {
        let keep = selected.or_else(|| state.selected_value().cloned());
        state.set_items(SearchableVec::new(items), window, cx);
        if let Some(value) = keep {
            state.set_selected_value(&value, window, cx);
        }
    });
}

pub struct AppShell {
    /// 窗口开出来先把焦点给外壳:没有它,第一次按键落在谁身上要看运气。
    pub focus_handle: FocusHandle,
    pub(crate) page: Page,
    /// 当前界面语言。跟着 `settings.ui_language`,单独留一份是为了知道它
    /// **什么时候变了** —— 变了才需要把下拉选项和表头重造一遍。
    pub(crate) language: String,
    pub(crate) settings_store: pnd_settings::SettingsStore,
    pub(crate) settings: pnd_settings::AppSettings,
    /// 盘上那份设置是更新版本写的:读得了,存不得。
    pub(crate) read_only: bool,
    pub(crate) log: VecDeque<String>,
    /// 状态行上那句话("已保存"、"保存失败:…")。空串 = 显示最后一条日志。
    pub(crate) notice: String,
    notice_at: Option<Instant>,

    /// 后台 actor。`None` = 它没起来(库打不开之类),程序照常能看历史、改设置。
    pub(crate) runtime: Option<RuntimeHandle>,
    /// 提醒卡片线程。`None` = 没有卡片,提醒只落在提醒记录页。
    pub(crate) alert_card: Option<AlertCardService>,
    /// 提醒记录页自己的一条库连接(actor 那条在别的线程上,不能共用)。
    pub(crate) alerts_store: Option<WatchStore>,
    /// ninja 两页自己的一条库连接。采样线程另开一条,WAL 让两边互不打断。
    pub(crate) ninja_store: Option<NinjaStore>,
    /// 正在跑的那一轮采样。`None` = 这个进程还没按过刷新。
    ///
    /// 句柄的 `Drop` 会取消并等线程收摊,所以它挂在这儿就等于"关窗即停手"。
    pub(crate) sampler: Option<SamplerHandle>,
    /// 采样线程还在跑。刷新按钮据此变灰。
    pub(crate) sampler_busy: bool,
    /// 采样最近说的那句话,画在暗金热度页的脚注上。
    pub(crate) sampler_line: String,

    /// 每条搜索现在跑到哪一步。actor 每次状态有变就整份广播,这里只管存最新的。
    pub(crate) watch_status: BTreeMap<WatchId, WatchStatus>,
    /// 每条限速策略的用量。键是策略名(`trade-search-request-limit` 这些)。
    pub(crate) budget: BTreeMap<String, Vec<BucketUsage>>,
    pub(crate) rates: CurrencyRates,
    /// 弹过的卡片:卡片按钮事件只带一个 alert_id,靠它找回是哪一批命中。
    pub(crate) shown_cards: BTreeMap<i64, MatchedListing>,
    /// 提醒记录页当前显示的那些行。
    pub(crate) alert_rows: Vec<AlertRow>,
    /// ninja 缓存在界面这边的那份副本。
    pub(crate) ninja: NinjaData,
    /// 新增搜索表单下面那行红字。空串 = 没有错。
    pub(crate) watch_error: String,

    /// 表格内容要重建了。行是每次整份换掉的,没有这四个标志就得每拍重建。
    pub(crate) watches_dirty: bool,
    pub(crate) alerts_dirty: bool,
    pub(crate) uniques_dirty: bool,
    pub(crate) mods_dirty: bool,
    /// ninja 那几个下拉的选项要重造了(数据换了或者语言换了)。
    ///
    /// 单独一个标志是因为重造下拉要 `&mut Window`,而 tick 手上没有窗口;
    /// 真正的重造放在 render 里做。
    pub(crate) ninja_filters_dirty: bool,
    /// 两页各自的"显示全部"开关。
    pub(crate) uniques_show_all: bool,
    pub(crate) mods_show_all: bool,
    /// 到这个时刻重读一次提醒历史(等 actor 把命令写进库)。
    pub(crate) alerts_refresh_at: Option<Instant>,
    /// 上一次重建蹲价表时的秒数。倒计时每秒动一次,不必每拍动。
    last_second: i64,

    pub(crate) settings_form: pages::settings::SettingsForm,
    pub(crate) watches_form: pages::watches::WatchesForm,

    pub(crate) watches_table: Entity<TableState<SimpleTable>>,
    pub(crate) alerts_table: Entity<TableState<SimpleTable>>,
    pub(crate) uniques_table: Entity<TableState<SimpleTable>>,
    pub(crate) mods_table: Entity<TableState<SimpleTable>>,

    pub(crate) uniques_partition_select: ChoiceSelect,
    pub(crate) mods_slot_select: ChoiceSelect,
    pub(crate) mods_rarity_select: ChoiceSelect,
    pub(crate) mods_kind_select: ChoiceSelect,
}

impl AppShell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings_store = crate::settings_store();
        let loaded = settings_store.load();
        let mut settings = loaded.settings;
        // 手改坏的值在这里就拉回可用范围:后面每一页都直接读 `settings`,
        // 让一个 5 秒的轮询间隔流到界面上,读到的人会以为程序就该那么跑。
        settings.normalize();
        let language = settings.ui_language.clone();
        let text = i18n::text(&language);

        let mut log = VecDeque::new();
        let mut notice = String::new();
        let read_only = match &loaded.status {
            pnd_settings::LoadStatus::FutureSchemaReadOnly { detected } => {
                log.push_back(format!("settings: schema {detected} is newer than ours"));
                notice = text.settings_read_only.to_owned();
                true
            }
            pnd_settings::LoadStatus::Corrupt {
                backup_path,
                reason,
            } => {
                // 设置读不动是必须当场看见的事:不说的话页面看起来就像
                // 一次全新安装,用户的搜索列表"没了"。
                log.push_back(format!("settings: {reason}"));
                notice = format!("settings moved aside: {}", backup_path.display());
                false
            }
            pnd_settings::LoadStatus::Loaded | pnd_settings::LoadStatus::Defaults => false,
        };
        log.push_back(format!("settings: {}", settings_store.path().display()));

        // 后台三件套。每一件失败都只是少一半功能,不该拦着窗口开出来:
        // actor 没起来还能看历史、改设置;卡片没起来提醒仍然进记录页。
        let mut startup: Vec<String> = Vec::new();
        let runtime = link::start_runtime(&settings, &mut startup);
        if runtime.is_none() && notice.is_empty() {
            notice = i18n::fill(
                text.notice_runtime_failed,
                &[startup.last().map_or("", String::as_str)],
            );
        }
        let alert_card = match link::start_alert_card(&settings, &mut startup) {
            Ok(card) => Some(card),
            Err(error) => {
                startup.push(format!("alert card failed to start: {error}"));
                if notice.is_empty() {
                    notice = i18n::fill(text.notice_card_failed, &[&error.to_string()]);
                }
                None
            }
        };
        // 界面这一侧的库连接是只读用途(提醒历史),但 SQLite 的连接本来就
        // 是读写的;WAL + busy_timeout 让它和 actor 那条互不打断。
        let alerts_store = match WatchStore::open(crate::watch_db_path()) {
            Ok(store) => Some(store),
            Err(error) => {
                startup.push(format!("could not open the alert history: {error}"));
                None
            }
        };
        // ninja 那个库是纯缓存:打不开只是两页空着,剩下的功能一个都不少。
        let ninja_store = match NinjaStore::open(crate::ninja_db_path()) {
            Ok(store) => Some(store),
            Err(error) => {
                startup.push(format!("could not open the ninja cache: {error}"));
                if notice.is_empty() {
                    notice = text.ninja_store_missing.to_owned();
                }
                None
            }
        };
        for line in startup {
            log.push_back(line);
        }

        // 120ms 心跳。频率照兄弟项目:比一帧慢得多,又快到让"点了按钮"和
        // "屏幕上有反应"之间看不出间隔。视图没了(窗口关掉)`update` 会报错,
        // 循环随之退出。
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                if this
                    .update(cx, |this: &mut AppShell, cx| this.tick(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let settings_form = pages::settings::SettingsForm::new(&settings, text, window, cx);
        let watches_form = pages::watches::WatchesForm::new(&settings, text, window, cx);

        let watches_table = new_table(pages::watches::table_content(text), window, cx);
        let alerts_table = new_table(pages::alerts::table_content(text), window, cx);
        let uniques_table = new_table(pages::ninja_uniques::table_content(text), window, cx);
        let mods_table = new_table(pages::ninja_mods::table_content(text), window, cx);

        // 四个下拉先按空数据造出来:库还没读呢。第一次 render 时
        // `sync_ninja_filters` 会拿真数据把它们重造一遍。
        let uniques_partition_select = choice_select(
            pages::ninja_uniques::partition_choices(&[], text),
            "",
            window,
            cx,
        );
        let mods_slot_select =
            choice_select(pages::ninja_mods::slot_choices(&[], text), "", window, cx);
        let mods_rarity_select =
            choice_select(pages::ninja_mods::rarity_choices(&[], text), "", window, cx);
        let mods_kind_select =
            choice_select(pages::ninja_mods::kind_choices(&[], text), "", window, cx);

        // 换一次筛选器 = 换一份要画的行。分区那个还要回库里重查一次:
        // 每个分区的暗金榜和分母都不一样。
        cx.subscribe(
            &uniques_partition_select,
            |this: &mut AppShell, _, event, cx| {
                let SelectEvent::Confirm(Some(value)) = event else {
                    return;
                };
                if this.ninja.selected_partition == value.as_ref() {
                    return;
                }
                this.ninja.selected_partition = value.to_string();
                this.reload_ninja_uniques();
                cx.notify();
            },
        )
        .detach();
        for select in [&mods_slot_select, &mods_rarity_select, &mods_kind_select] {
            cx.subscribe(select, |this: &mut AppShell, _, event, cx| {
                if matches!(event, SelectEvent::Confirm(_)) {
                    this.mods_dirty = true;
                    cx.notify();
                }
            })
            .detach();
        }

        let ninja = NinjaData::empty(pnd_ninja::index_state::league_url_guess(&settings.league));

        let mut shell = Self {
            focus_handle: cx.focus_handle(),
            page: Page::Watches,
            language,
            settings_store,
            settings,
            read_only,
            log,
            notice,
            notice_at: None,
            runtime,
            alert_card,
            alerts_store,
            ninja_store,
            sampler: None,
            sampler_busy: false,
            sampler_line: String::new(),
            watch_status: BTreeMap::new(),
            budget: BTreeMap::new(),
            rates: CurrencyRates::none(),
            shown_cards: BTreeMap::new(),
            alert_rows: Vec::new(),
            ninja,
            watch_error: String::new(),
            // 四张表现在还是空的(列已经有了):第一拍就会填上真数据。
            watches_dirty: true,
            alerts_dirty: true,
            uniques_dirty: true,
            mods_dirty: true,
            ninja_filters_dirty: true,
            uniques_show_all: false,
            mods_show_all: false,
            alerts_refresh_at: None,
            last_second: 0,
            settings_form,
            watches_form,
            watches_table,
            alerts_table,
            uniques_table,
            mods_table,
            uniques_partition_select,
            mods_slot_select,
            mods_rarity_select,
            mods_kind_select,
        };
        shell.refresh_alerts();
        shell.reload_ninja();
        shell
    }

    /// 当前语言的文案目录。每个页面第一行都是它。
    pub(crate) fn text(&self) -> &'static i18n::Text {
        i18n::text(&self.language)
    }

    /// 往日志里追一行,满了就丢最老的。
    pub(crate) fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    /// 状态行上那句会自己消失的话。
    pub(crate) fn set_notice(&mut self, notice: String) {
        self.notice = notice;
        self.notice_at = Some(Instant::now());
    }

    /// 常驻通知(只读设置那条)。它描述的是一直成立的事实,不该几秒后消失。
    pub(crate) fn set_sticky_notice(&mut self, notice: String) {
        self.notice = notice;
        self.notice_at = None;
    }

    /// 外壳的心跳,120ms 一次。
    ///
    /// 一拍里做四件事:抽干后台事件、到点重读提醒历史、把变了的表重建、
    /// 让那句一次性通知到点消失。**只在真有东西变了的时候** `notify` 一次:
    /// 每拍无条件重画会让一个挂在游戏旁边的窗口白烧 GPU。
    fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = self.drain_events(cx);
        changed |= self.refresh_alerts_if_due();

        // 倒计时("42 秒后再轮询")每秒动一次,不必每拍动。
        let now = now_secs();
        if now != self.last_second {
            self.last_second = now;
            if self.page == Page::Watches {
                self.watches_dirty = true;
            }
        }

        if self.watches_dirty {
            self.rebuild_watches_table(cx);
            changed = true;
        }
        if self.alerts_dirty {
            self.rebuild_alerts_table(cx);
            changed = true;
        }
        if self.uniques_dirty {
            self.rebuild_uniques_table(cx);
            changed = true;
        }
        if self.mods_dirty {
            self.rebuild_mods_table(cx);
            changed = true;
        }

        if let Some(at) = self.notice_at
            && at.elapsed() >= NOTICE_LIFETIME
        {
            self.notice.clear();
            self.notice_at = None;
            changed = true;
        }

        if changed {
            cx.notify();
        }
    }

    pub(crate) fn show_page(&mut self, page: Page) {
        self.page = page;
        // 翻到提醒记录页就重读一次:上一次读可能是几分钟前的事了。
        if page == Page::Alerts {
            self.refresh_alerts();
        }
    }

    /// 蹲价表 = 设置里的搜索列表 × actor 广播的运行状态。
    fn rebuild_watches_table(&mut self, cx: &mut Context<Self>) {
        self.watches_dirty = false;
        let content = pages::watches::table_content_for(
            &self.settings,
            &self.watch_status,
            self.text(),
            now_secs(),
        );
        self.watches_table.update(cx, |state, cx| {
            state.delegate_mut().set_content(content);
            state.refresh(cx);
        });
    }

    /// 提醒表 = `watch.sqlite` 里最近的那 200 行。
    fn rebuild_alerts_table(&mut self, cx: &mut Context<Self>) {
        self.alerts_dirty = false;
        let content = pages::alerts::table_content_for(&self.alert_rows, self.text());
        self.alerts_table.update(cx, |state, cx| {
            state.delegate_mut().set_content(content);
            state.refresh(cx);
        });
    }

    /// 暗金表 = 选中分区的 `items` 分面 × 经济接口的参考价。
    fn rebuild_uniques_table(&mut self, cx: &mut Context<Self>) {
        self.uniques_dirty = false;
        let content = pages::ninja_uniques::table_content_for(
            &self.ninja.uniques,
            &self.rates,
            self.uniques_show_all,
            self.text(),
        );
        self.uniques_table.update(cx, |state, cx| {
            state.delegate_mut().set_content(content);
            state.refresh(cx);
        });
    }

    /// 词缀表 = 这一轮的统计,按三个下拉筛一遍。
    fn rebuild_mods_table(&mut self, cx: &mut Context<Self>) {
        self.mods_dirty = false;
        let (slot, rarity, kind) = self.mods_filters(cx);
        let content = pages::ninja_mods::table_content_for(
            &self.ninja.mods,
            &slot,
            &rarity,
            &kind,
            self.mods_show_all,
            self.text(),
        );
        self.mods_table.update(cx, |state, cx| {
            state.delegate_mut().set_content(content);
            state.refresh(cx);
        });
    }

    /// 词缀页三个下拉现在选的是什么。空串 = "全部"。
    pub(crate) fn mods_filters(&self, cx: &App) -> (String, String, String) {
        (
            selected_value(&self.mods_slot_select, cx),
            selected_value(&self.mods_rarity_select, cx),
            selected_value(&self.mods_kind_select, cx),
        )
    }

    /// ninja 那几个下拉:数据换了(采样跑完)或者语言换了就重造一遍。
    ///
    /// 只能在 render 里做 —— 上游的下拉换选项要 `&mut Window`,而心跳
    /// 手上只有一个 `Context`。
    fn sync_ninja_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ninja_filters_dirty {
            return;
        }
        self.ninja_filters_dirty = false;
        let text = self.text();
        let partitions = pages::ninja_uniques::partition_choices(&self.ninja.partition_keys, text);
        let slots = pages::ninja_mods::slot_choices(&self.ninja.mods, text);
        let rarities = pages::ninja_mods::rarity_choices(&self.ninja.mods, text);
        let kinds = pages::ninja_mods::kind_choices(&self.ninja.mods, text);
        let partition = self.ninja.selected_partition.clone();

        relabel_select(
            &self.uniques_partition_select.clone(),
            partitions,
            Some(&partition),
            window,
            cx,
        );
        for (select, items) in [
            (self.mods_slot_select.clone(), slots),
            (self.mods_rarity_select.clone(), rarities),
            (self.mods_kind_select.clone(), kinds),
        ] {
            // `None` = 留着用户现在筛的那一档:换个界面语言不该把它清回"全部"。
            relabel_select(&select, items, None, window, cx);
        }
    }

    /// 换语言之后,把语言烘进去的那些东西重造一遍。
    ///
    /// 表头和下拉选项在造出来的那一刻就把文字复制走了,之后不会自己跟着
    /// 目录变。render 里查一次比在切语言那条路上散落一堆重建调用可靠:
    /// 设置页以后多一个改语言的入口,也不用记得来这儿加一行。
    fn sync_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.language == self.settings.ui_language {
            return;
        }
        self.language = self.settings.ui_language.clone();
        let text = self.text();

        // 四张表连行带列一起重建(表头和格子里的状态词都跟着语言走),
        // 走各自的 rebuild;ninja 那几个下拉交给 `sync_ninja_filters`。
        self.watches_dirty = true;
        self.alerts_dirty = true;
        self.uniques_dirty = true;
        self.mods_dirty = true;
        self.ninja_filters_dirty = true;

        self.watches_form.relabel(&self.settings, text, window, cx);
        self.settings_form.relabel(&self.settings, text, window, cx);
    }

    fn nav_rail(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .w(px(W_NAV))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(6.))
            .bg(c(RAIL))
            .border_r_1()
            .border_color(c(HAIRLINE))
            .children(Page::ALL.into_iter().map(|page| {
                Button::new(page.element_id())
                    .ghost()
                    .selected(page == self.page)
                    .label(SharedString::from(page.label(self.text()).to_owned()))
                    .w_full()
                    .justify_start()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show_page(page);
                        cx.notify();
                    }))
            }))
    }

    /// 底部状态行:有通知就说通知,否则说最后一条日志。
    fn status_bar(&self) -> gpui::Div {
        let (line, tone) = if self.notice.is_empty() {
            (self.log.back().cloned().unwrap_or_default(), muted())
        } else if self.read_only {
            (self.notice.clone(), warn_amber())
        } else {
            (self.notice.clone(), c(ACCENT_TEXT))
        };
        div()
            .h(px(24.))
            .flex_none()
            .flex()
            .items_center()
            .px(px(10.))
            .bg(c(RAIL))
            .border_t_1()
            .border_color(c(HAIRLINE))
            .text_size(fs(FS_11))
            .text_color(tone)
            .child(SharedString::from(line))
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_language(window, cx);
        self.sync_ninja_filters(window, cx);
        let text = self.text();

        let body = match self.page {
            Page::Watches => self.render_watches(cx),
            Page::Alerts => self.render_alerts(cx),
            Page::NinjaUniques => self.render_ninja_uniques(cx),
            Page::NinjaMods => self.render_ninja_mods(cx),
            Page::Settings => self.render_settings(cx),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(c(CANVAS))
            .text_color(c(TEXT_PRIMARY))
            .font_family(FONT_UI)
            .track_focus(&self.focus_handle)
            .child(
                // 顶条:程序名 + 当前联赛。联赛常驻是因为每张榜、每条搜索
                // 都是"某个联赛里的",看错联赛时每个数都"看起来对"。
                div()
                    .h(px(34.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .bg(c(RAIL))
                    .border_b_1()
                    .border_color(c(HAIRLINE_STRONG))
                    .child(
                        div()
                            .text_size(fs(FS_13))
                            .text_color(c(ACCENT_TEXT))
                            .child(text.app_title),
                    )
                    .child(div().flex_grow())
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(fs(FS_11))
                            .text_color(c(TEXT_META))
                            .child(SharedString::from(self.settings.league.clone())),
                    ),
            )
            .child(
                // `min_h(0)` 一路往下传:flex 子项的默认最小高度是它的内容,
                // 不写这一句,长列表会把面板顶得比窗口还高,里面的
                // `overflow_y_scroll` 就永远没有东西可裁。
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .overflow_hidden()
                    .child(self.nav_rail(cx))
                    .child(body.flex_1().min_w(px(0.)).min_h(px(0.))),
            )
            .child(self.status_bar())
    }
}

impl Drop for AppShell {
    /// 关窗时先请采样线程停手。
    ///
    /// 它可能正睡在一个 60 秒的退避里,而句柄自己的 `Drop` 紧接着要 join 它;
    /// 先立标志,那一觉最多再睡 100 毫秒就醒。
    fn drop(&mut self) {
        if let Some(sampler) = &self.sampler {
            sampler.cancel();
        }
    }
}

/// 一个下拉现在选的值。没选中就是空串 —— 每个下拉的第一项都是"全部",
/// 空串本来就是它的值。
pub(crate) fn selected_value(select: &ChoiceSelect, cx: &App) -> String {
    select
        .read(cx)
        .selected_value()
        .map_or_else(String::new, ToString::to_string)
}

/// 页面通用的一块面板。
pub(crate) fn panel() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .bg(c(PANEL))
        .border_1()
        .border_color(c(HAIRLINE))
}

/// 页面标题 + 一行副标题。
pub(crate) fn page_heading(title: &'static str, subtitle: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .text_size(fs(FS_16))
                .text_color(c(TEXT_PRIMARY))
                .child(title),
        )
        .child(
            div()
                .text_size(fs(FS_11_5))
                .text_color(c(TEXT_META))
                .child(subtitle.into()),
        )
}

/// 表单里的一个字段名。定宽,一列标签才对得齐。
pub(crate) fn field_label(label: &'static str) -> gpui::Div {
    div()
        .w(px(150.))
        .flex_none()
        .text_size(fs(FS_11_5))
        .text_color(c(TEXT_META))
        .child(label)
}

/// 表单里的一行。
pub(crate) fn field_row() -> gpui::Div {
    div().h_flex().items_center().gap(px(8.))
}

/// 一个带标签的下拉。筛选器行上到处是它。
pub(crate) fn picker(label: &'static str, select: &ChoiceSelect, width: f32) -> gpui::Div {
    div()
        .h_flex()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .text_size(fs(FS_11_5))
                .text_color(muted())
                .child(label),
        )
        .child(
            div()
                .w(px(width))
                .flex_none()
                .child(gpui_component::select::Select::new(select).with_size(Size::Small)),
        )
}

/// 小一号的说明字。
pub(crate) fn hint(body: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(fs(FS_11))
        .text_color(c(TEXT_META))
        .child(body.into())
}

/// 一张表格。四页共用同一套外观参数,免得每页各调一份。
pub(crate) fn table(
    state: &Entity<TableState<SimpleTable>>,
) -> gpui_component::table::Table<SimpleTable> {
    gpui_component::table::Table::new(state)
        .stripe(true)
        .bordered(false)
        .with_size(Size::XSmall)
}

#[cfg(test)]
mod shell_tests {
    use super::Page;
    use crate::i18n;

    /// 每一页在两种语言下都得有名字,而且名字互不相同 —— 导航上出现两个
    /// 一样的词,用户就分不清自己点的是哪一页。
    #[test]
    fn every_page_has_a_distinct_name_in_both_languages() {
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            let mut labels: Vec<&str> = Page::ALL.into_iter().map(|p| p.label(text)).collect();
            labels.sort_unstable();
            let count = labels.len();
            labels.dedup();
            assert_eq!(labels.len(), count, "{language} 的导航里有重名");
            assert!(labels.iter().all(|label| !label.trim().is_empty()));
        }
    }

    /// 元素 id 不跟语言走,而且五个各不相同。
    #[test]
    fn the_element_ids_are_stable_and_unique() {
        let mut ids: Vec<&str> = Page::ALL.into_iter().map(Page::element_id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }
}
