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
pub mod tray;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::select::{SearchableVec, SelectEvent, SelectItem, SelectState};
use gpui_component::table::{TableEvent, TableState};
use gpui_component::{IndexPath, Selectable as _, Sizable as _, Size, StyledExt as _};

use pnd_domain::{CurrencyRates, Game, ObservationId, RateSources, WatchId};
use pnd_platform_win::{AlertCardService, LoginService, TrayHandle};
use pnd_runtime::{
    MatchedListing, ObservationStatus, RuntimeHandle, SamplerHandle, WatchStatus, now_secs,
};
use pnd_storage::{AlertRow, NinjaStore, WatchStore};
use pnd_trade::BucketUsage;

use crate::i18n;
use crate::logbook::{self, LOG_CAPACITY};
use crate::theme::*;
use ninja::NinjaData;
use pages::SimpleTable;

/// 日志抽屉的高度。够看十来行,又不至于把它下面的页面挤没。
const H_LOG_PANE: f32 = 200.0;
/// 左导航宽度。中文四个字 + 内边距,英文最长的 "Modifier heat" 也放得下。
const W_NAV: f32 = 136.0;
/// 一句话通知("已保存")挂多久。够看清,又不会一直杵在那儿。
const NOTICE_LIFETIME: Duration = Duration::from_secs(4);

/// 六个页面。顺序就是导航顺序:每天先看蹲价,再看它响过什么,接着是
/// 市场观察(它和前两页同属"交易站那一半"),然后才是 ninja 那两张榜,
/// 设置沉底。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Watches,
    Alerts,
    Observations,
    NinjaUniques,
    NinjaMods,
    Settings,
}

impl Page {
    pub const ALL: [Self; 6] = [
        Self::Watches,
        Self::Alerts,
        Self::Observations,
        Self::NinjaUniques,
        Self::NinjaMods,
        Self::Settings,
    ];

    fn label(self, text: &'static i18n::Text) -> &'static str {
        match self {
            Self::Watches => text.nav_watches,
            Self::Alerts => text.nav_alerts,
            Self::Observations => text.nav_observations,
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
            Self::Observations => "nav-observations",
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

    /// 存储值。
    ///
    /// 和 `SelectItem::value` 拿到的是同一个东西,但那是个 trait 方法 ——
    /// 把那个 trait 引进页面文件里,`String` 上的 `matches` 会跟着被它的
    /// 同名方法接管(上游给 `String` 也实现了 `SelectItem`)。
    pub fn stored_value(&self) -> &str {
        &self.value
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

/// 登录窗现在处于哪一档。设置页那两个按钮和那条状态字都看它。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoginPhase {
    /// 没开过,或者已经关了。
    #[default]
    Idle,
    /// 窗口开着(或正在开)。这期间两个按钮不该再被按第二下。
    Open,
    /// 这台机器上没有 Edge 内核,登录窗根本开不出来 —— 只能手动粘 cookie,
    /// 旁边给一个"去装 WebView2"的按钮。
    RuntimeMissing,
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
    let state = cx.new(|cx| {
        TableState::new(SimpleTable::new(content), window, cx)
            .row_selectable(true)
            .col_selectable(false)
    });
    // 用户拖完一列,上游广播一次新宽度。抄回委托手上那份列 —— 上游只改
    // 它自己那份运行时宽度,而下一次重建列头(换语言)时它是回到委托这儿
    // 重读的:不抄,拖过的宽度那一下就作废了。
    cx.subscribe(&state, |_: &mut AppShell, table, event: &TableEvent, cx| {
        if let TableEvent::ColumnWidthsChanged(widths) = event {
            let widths = widths.clone();
            table.update(cx, |state, _| {
                state.delegate_mut().remember_widths(&widths);
            });
        }
    })
    .detach();
    state
}

/// 把新内容装进一张表。
///
/// 只有列真的变了才让上游重建列头:重建会把用户拖出来的列宽重读成委托
/// 手上那份(见 [`SimpleTable::set_content`]),而这几张表的数据每秒都在变。
fn apply_content(
    table: &Entity<TableState<SimpleTable>>,
    content: pages::TableContent,
    cx: &mut Context<AppShell>,
) {
    table.update(cx, |state, cx| {
        if state.delegate_mut().set_content(content) {
            state.refresh(cx);
        } else {
            cx.notify();
        }
    });
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
    /// 日志抽屉开着没有。状态行左边那个"日志"按钮拨它。
    pub(crate) log_open: bool,
    /// 每一行还要往这个文件里追一份 —— 程序关掉之后内存里那份就没了。
    pub(crate) log_path: PathBuf,
    /// 抽屉里那块滚动区。新的一行进来就把它滚到底,否则最要紧的那条永远
    /// 在屏幕外。
    log_scroll: ScrollHandle,
    /// 状态行上那句话("已保存"、"保存失败:…")。空串 = 显示最后一条日志。
    pub(crate) notice: String,
    notice_at: Option<Instant>,

    /// 后台 actor。`None` = 它没起来(库打不开之类),程序照常能看历史、改设置。
    pub(crate) runtime: Option<RuntimeHandle>,
    /// 提醒卡片线程。`None` = 没有卡片,提醒只落在提醒记录页。
    pub(crate) alert_card: Option<AlertCardService>,
    /// 通知区图标那条线程。`None` = 图标没起来,点叉就照旧退出。
    pub(crate) tray: Option<TrayHandle>,
    /// 登录窗那条线程。第一次点"登录官网"时才建 —— 大多数启动根本用不上它,
    /// 没必要每次开程序都拉起一条 WebView2 线程。
    pub(crate) login: Option<LoginService>,
    /// 建那条线程时用的是哪门语言。换了语言就重建:窗口标题和顶上那条提示
    /// 是启动时传进去的,之后改不了。
    pub(crate) login_language: String,
    pub(crate) login_phase: LoginPhase,
    /// 登录那一行上的状态字。
    pub(crate) login_line: String,
    /// 界面这一侧的 `watch.sqlite` 连接(actor 那条在别的线程上,不能共用)。
    /// 提醒记录页和市场观察页共用它 —— 两页读的是同一个库文件。
    pub(crate) alerts_store: Option<WatchStore>,
    /// ninja 两页自己的库连接,**每代一条**:两代各写各的文件,页面读的是
    /// 开关选中那一代的那条。采样线程另开一条,WAL 让两边互不打断。
    ///
    /// 一代打不开只是那一代的两页空着,另一代照常。
    pub(crate) ninja_stores: BTreeMap<Game, NinjaStore>,
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
    /// 每条市场观察现在跑到哪一步。同上,只管存最新的。
    ///
    /// 统计结果**不在**这里面:那几张表动辄几百行,广播一份既大又立刻过时。
    /// actor 只发一句"这条观察的数据变了",界面自己回库里读(见 `observe`)。
    pub(crate) observation_status: BTreeMap<ObservationId, ObservationStatus>,
    /// 每条限速策略的用量。键是策略名(`trade-search-request-limit` 这些)。
    pub(crate) budget: BTreeMap<String, Vec<BucketUsage>>,
    /// 每条策略还要等几秒才放行下一封请求。没有这一项 = 现在就能发。
    pub(crate) budget_next_allowed: BTreeMap<String, u64>,
    /// 一次"测试会话"正在路上。按钮据此变灰 —— 那一下要花掉一次搜索额度,
    /// 手快点两下就是两次。
    pub(crate) session_check_busy: bool,
    /// 上一次测试会话的结论,画在设置页那个按钮旁边。
    pub(crate) session_check_line: String,
    /// 现在真正在用的换算表(手填的已经盖在 poe.ninja 那份上)。
    pub(crate) rates: CurrencyRates,
    /// 上面那张表每一档是从哪儿来的。观察页那句汇率靠它说出"(手动)"。
    pub(crate) rate_sources: RateSources,
    /// 弹过的卡片:卡片按钮事件只带一个 alert_id,靠它找回是哪一批命中。
    pub(crate) shown_cards: BTreeMap<i64, MatchedListing>,
    /// 提醒记录页当前显示的那些行。
    pub(crate) alert_rows: Vec<AlertRow>,
    /// ninja 缓存在界面这边的那份副本。
    pub(crate) ninja: NinjaData,
    /// 选中那条观察在库里攒到的东西,同样是一份内存副本。
    pub(crate) observe: pages::observations::ObservationData,
    /// 新增搜索表单下面那行红字。空串 = 没有错。
    pub(crate) watch_error: String,
    /// 新增观察表单下面那行红字。
    pub(crate) obs_error: String,
    /// 词缀战绩表现在要求至少见过几件。
    pub(crate) obs_min_samples: u32,
    /// 词缀战绩表只留收藏过的那几行。
    pub(crate) obs_favourites_only: bool,
    /// 左下那块聚合表现在看的是哪一栏(词缀战绩 / 价位战绩)。
    pub(crate) obs_agg_tab: pages::observations::AggregateTab,
    /// 价位战绩按哪种口径分档:按币种,还是全部折成 divine。
    ///
    /// 默认折算 —— 按币种那一栏答不出"值 2 divine 的货卖不卖得掉":
    /// 标 chaos 的那批和标 divine 的那批在两套档位里各算各的。
    pub(crate) obs_price_mode: pnd_storage::PriceMode,
    /// 挂单流现在看的是哪一栏。
    pub(crate) obs_stream_tab: pages::observations::StreamTab,
    /// 攒着"下一条蹲价要哪几条词缀"的篮子,跟着选中那条观察走
    /// (换一条就倒空,见 [`pages::observations::retarget_basket`])。
    ///
    /// 不存进 `settings.json`:它是一次"挑词缀 → 做成蹲价"当中的草稿,
    /// 做完就没用了,而真正要留下来的是做出来的那条蹲价。
    pub(crate) obs_basket: pages::observations::ModBasket,
    /// 删除观察的按钮已经按过第一下了。删掉的东西找不回来,所以要按两下。
    pub(crate) obs_remove_armed: bool,
    /// 挂单流那块滚动区。
    pub(crate) obs_stream_scroll: ScrollHandle,
    /// 表格里刚被选中的那一行,等着填进蹲价表单。
    ///
    /// 单独一个标志的理由和 `ninja_filters_dirty` 一样:写输入框要
    /// `&mut Window`,而发出"选中了第几行"那条事件的订阅回调手上没有窗口。
    pub(crate) watches_form_load: Option<usize>,
    /// 同上,观察表那一份。选中一行同时是"改这一条"和"下面两块画这一条"。
    pub(crate) observations_form_load: Option<usize>,

    /// 表格内容要重建了。行是每次整份换掉的,没有这几个标志就得每拍重建。
    pub(crate) watches_dirty: bool,
    pub(crate) alerts_dirty: bool,
    pub(crate) uniques_dirty: bool,
    pub(crate) mods_dirty: bool,
    pub(crate) observations_dirty: bool,
    pub(crate) obs_mods_dirty: bool,
    /// 价位战绩表要重排了。和上面那个分开,是因为词缀类型下拉只筛得动
    /// 词缀那张表 —— 换一次类型不该顺带把价位表也重排一遍。
    pub(crate) obs_price_dirty: bool,
    /// 观察页那个词缀类型下拉的选项要重造了(读了新数据,或者换了语言)。
    pub(crate) obs_filters_dirty: bool,
    /// ninja 那几个下拉的选项要重造了(数据换了或者语言换了)。
    ///
    /// 单独一个标志是因为重造下拉要 `&mut Window`,而 tick 手上没有窗口;
    /// 真正的重造放在 render 里做。
    pub(crate) ninja_filters_dirty: bool,
    /// 登录窗读到的会话要写回设置页那个掩码输入框。
    ///
    /// 单独一个标志的理由和 `ninja_filters_dirty` 一样:改输入框要
    /// `&mut Window`,而心跳手上没有窗口。不写回的话,框里还是空的,
    /// 下一次按保存就会把刚登出来的会话清掉。
    pub(crate) poesessid_dirty: bool,
    /// 两页各自的"显示全部"开关。
    pub(crate) uniques_show_all: bool,
    pub(crate) mods_show_all: bool,
    /// 暗金榜只留下"紧俏"那一档。
    pub(crate) uniques_scarce_only: bool,
    /// 暗金榜按供需比排,而不是按人数。
    ///
    /// 做成一个开关而不是点列头:上游那张表的列头不支持排序,而这一页
    /// 真正想按的只有这一列 —— 别的列(人数)本来就是库给的默认顺序。
    pub(crate) uniques_sort_by_demand: bool,
    /// 到这个时刻重读一次提醒历史(等 actor 把命令写进库)。
    pub(crate) alerts_refresh_at: Option<Instant>,
    /// 同上,观察数据那一份:actor 每跑完一轮发一句"变了",这里延迟一点再读。
    pub(crate) observe_refresh_at: Option<Instant>,
    /// 上一次重建蹲价表时的秒数。倒计时每秒动一次,不必每拍动。
    last_second: i64,

    pub(crate) settings_form: pages::settings::SettingsForm,
    pub(crate) watches_form: pages::watches::WatchesForm,
    pub(crate) observations_form: pages::observations::ObservationsForm,

    pub(crate) watches_table: Entity<TableState<SimpleTable>>,
    pub(crate) alerts_table: Entity<TableState<SimpleTable>>,
    pub(crate) uniques_table: Entity<TableState<SimpleTable>>,
    pub(crate) mods_table: Entity<TableState<SimpleTable>>,
    pub(crate) observations_table: Entity<TableState<SimpleTable>>,
    pub(crate) obs_mods_table: Entity<TableState<SimpleTable>>,
    pub(crate) obs_price_table: Entity<TableState<SimpleTable>>,

    pub(crate) obs_kind_select: ChoiceSelect,
    pub(crate) uniques_partition_select: ChoiceSelect,
    pub(crate) mods_class_select: ChoiceSelect,
    pub(crate) mods_slot_select: ChoiceSelect,
    pub(crate) mods_rarity_select: ChoiceSelect,
    pub(crate) mods_kind_select: ChoiceSelect,
}

impl AppShell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 开窗之前的这几行先攒着:`push_log` 要脱敏、要落盘,而那两件事都得
        // 等外壳自己造出来(日志文件的位置存在它身上)。造完立刻补记。
        let mut boot_log: Vec<String> = Vec::new();

        // 数据目录搬家(PoeNinjaData → ExileLedger)排在最前面,比读设置还早:
        // 只要有谁先开了新目录里的文件,新目录就存在了,搬家条件再也不成立,
        // 老数据就静默留在原地 —— 用户看到的是"我的搜索列表全没了"。
        //
        // 它同时定下这次启动用哪个目录(搬不动就留在老目录里跑),所以下面
        // 每一句要路径的话都得排在它后面。
        if let Some(line) = crate::migrate_data_dir() {
            boot_log.push(line);
        }

        let settings_store = crate::settings_store();
        let loaded = settings_store.load();
        let mut settings = loaded.settings;
        // 手改坏的值在这里就拉回可用范围:后面每一页都直接读 `settings`,
        // 让一个 5 秒的轮询间隔流到界面上,读到的人会以为程序就该那么跑。
        settings.normalize();
        let language = settings.ui_language.clone();
        let text = i18n::text(&language);

        let mut notice = String::new();
        let read_only = match &loaded.status {
            pnd_settings::LoadStatus::FutureSchemaReadOnly { detected } => {
                boot_log.push(format!("settings: schema {detected} is newer than ours"));
                notice = text.settings_read_only.to_owned();
                true
            }
            pnd_settings::LoadStatus::Corrupt {
                backup_path,
                reason,
            } => {
                // 设置读不动是必须当场看见的事:不说的话页面看起来就像
                // 一次全新安装,用户的搜索列表"没了"。
                boot_log.push(format!("settings: {reason}"));
                notice = format!("settings moved aside: {}", backup_path.display());
                false
            }
            pnd_settings::LoadStatus::Loaded | pnd_settings::LoadStatus::Defaults => false,
        };
        // 会话解不开也得说一声(整份设置是好的,只有那段密文这台机器打不开)。
        // 不说的话,live 和"去藏身处"就是莫名其妙地不工作。
        if let Some(warning) = &loaded.session_warning {
            boot_log.push(format!("settings: {warning}"));
        }
        boot_log.push(format!("settings: {}", settings_store.path().display()));

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
        // 托盘图标要等窗口存在才起得来 —— 它得知道藏的是哪一个窗口。
        let tray = tray::start_tray(text, window, &mut startup);
        // 界面这一侧的库连接是只读用途(提醒历史),但 SQLite 的连接本来就
        // 是读写的;WAL + busy_timeout 让它和 actor 那条互不打断。
        let alerts_store = match WatchStore::open(crate::watch_db_path()) {
            Ok(store) => Some(store),
            Err(error) => {
                startup.push(format!("could not open the alert history: {error}"));
                None
            }
        };
        // ninja 那两个库是纯缓存:打不开只是那一代的两页空着,剩下的功能一个都不少。
        // 两代都开:开关一按就要能立刻读到另一份,而 SQLite 的连接本来就便宜。
        let mut ninja_stores = BTreeMap::new();
        for game in [Game::Poe1, Game::Poe2] {
            match NinjaStore::open(crate::ninja_db_path_for(game)) {
                Ok(store) => {
                    ninja_stores.insert(game, store);
                }
                Err(error) => {
                    startup.push(format!("could not open the {game} ninja cache: {error}"));
                    if notice.is_empty() {
                        notice = text.ninja_store_missing.to_owned();
                    }
                }
            }
        }
        boot_log.extend(startup);

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
        let observations_form = pages::observations::ObservationsForm::new(text, window, cx);

        let watches_table = new_table(pages::watches::table_content(text), window, cx);
        // 选中一行 = "我要改这一条"。真正把值填进表单要 `&mut Window`,
        // 这里只记下是第几行,下一帧 `sync_watch_form` 去填。
        cx.subscribe(&watches_table, |this: &mut AppShell, _, event, cx| {
            if let TableEvent::SelectRow(row) = event {
                this.watches_form_load = Some(*row);
                cx.notify();
            }
        })
        .detach();
        let alerts_table = new_table(pages::alerts::table_content(text), window, cx);
        let uniques_table = new_table(pages::ninja_uniques::table_content(text), window, cx);
        let mods_table = new_table(pages::ninja_mods::table_content(text), window, cx);
        let observations_table = new_table(pages::observations::table_content(text), window, cx);
        // 选中一行 = "我要看这一条"(顺带把它装进表单)。理由同蹲价那张表:
        // 真正去填框要 `&mut Window`,这里只记下是第几行。
        cx.subscribe(&observations_table, |this: &mut AppShell, _, event, cx| {
            if let TableEvent::SelectRow(row) = event {
                this.observations_form_load = Some(*row);
                cx.notify();
            }
        })
        .detach();
        let obs_mods_table = new_table(pages::observations::mods_table_content(text), window, cx);
        let obs_price_table = new_table(pages::observations::price_table_content(text), window, cx);

        // 五个下拉先按空数据造出来:库还没读呢。第一次 render 时
        // `sync_ninja_filters` 会拿真数据把它们重造一遍。
        let uniques_partition_select = choice_select(
            pages::ninja_uniques::partition_choices(&[], text),
            "",
            window,
            cx,
        );
        let mods_class_select =
            choice_select(pages::ninja_mods::class_choices(&[], text), "", window, cx);
        let mods_slot_select =
            choice_select(pages::ninja_mods::slot_choices(&[], text), "", window, cx);
        // 稀有度是唯一一个不从"全部"起步的下拉:这一页要回答的是"我该给自己
        // 做一件什么样的装备",而只有稀有装答得上。`rarity_choices` 保证这一档
        // 永远在选项里,所以这里选得中。
        let mods_rarity_select = choice_select(
            pages::ninja_mods::rarity_choices(&[], text),
            pages::ninja_mods::DEFAULT_RARITY,
            window,
            cx,
        );
        let mods_kind_select =
            choice_select(pages::ninja_mods::kind_choices(&[], text), "", window, cx);
        // 观察页那个词缀类型下拉同样从库里长出来,第一次读完数据才有真选项。
        let obs_kind_select =
            choice_select(pages::observations::kind_choices(&[], text), "", window, cx);
        cx.subscribe(&obs_kind_select, |this: &mut AppShell, _, event, cx| {
            if matches!(event, SelectEvent::Confirm(_)) {
                this.obs_mods_dirty = true;
                cx.notify();
            }
        })
        .detach();

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
        // 职业不在那三个之列:每个职业各有一套完整的词缀行,换一个就得回库里
        // 重查那一套,而不是在内存里筛一遍现有的行。
        cx.subscribe(&mods_class_select, |this: &mut AppShell, _, event, cx| {
            let SelectEvent::Confirm(Some(value)) = event else {
                return;
            };
            if this.ninja.selected_class == value.as_ref() {
                return;
            }
            this.ninja.selected_class = value.to_string();
            this.reload_ninja_mods();
            cx.notify();
        })
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

        // 开局先给一份空的:短名和数据都由下面那句 `resync_ninja_league` 一起
        // 定。这里不提前把短名算出来 —— 算了也是白算(马上被覆盖),而且看起来
        // 像"视图已经建好了"。
        let ninja = NinjaData::default();

        let mut shell = Self {
            focus_handle: cx.focus_handle(),
            page: Page::Watches,
            language,
            settings_store,
            settings,
            read_only,
            log: VecDeque::new(),
            log_open: false,
            log_path: crate::app_log_path(),
            log_scroll: ScrollHandle::new(),
            notice,
            notice_at: None,
            runtime,
            alert_card,
            tray,
            login: None,
            login_language: String::new(),
            login_phase: LoginPhase::default(),
            login_line: String::new(),
            alerts_store,
            ninja_stores,
            sampler: None,
            sampler_busy: false,
            sampler_line: String::new(),
            watch_status: BTreeMap::new(),
            observation_status: BTreeMap::new(),
            budget: BTreeMap::new(),
            budget_next_allowed: BTreeMap::new(),
            session_check_busy: false,
            session_check_line: String::new(),
            rates: CurrencyRates::none(),
            rate_sources: RateSources::default(),
            shown_cards: BTreeMap::new(),
            alert_rows: Vec::new(),
            ninja,
            observe: pages::observations::ObservationData::default(),
            watch_error: String::new(),
            obs_error: String::new(),
            obs_min_samples: pages::observations::DEFAULT_MIN_SAMPLES,
            obs_favourites_only: false,
            obs_agg_tab: pages::observations::AggregateTab::default(),
            obs_price_mode: pnd_storage::PriceMode::default(),
            obs_stream_tab: pages::observations::StreamTab::default(),
            obs_basket: pages::observations::ModBasket::default(),
            obs_remove_armed: false,
            obs_stream_scroll: ScrollHandle::new(),
            watches_form_load: None,
            observations_form_load: None,
            // 六张表现在还是空的(列已经有了):第一拍就会填上真数据。
            watches_dirty: true,
            alerts_dirty: true,
            uniques_dirty: true,
            mods_dirty: true,
            observations_dirty: true,
            obs_mods_dirty: true,
            obs_price_dirty: true,
            obs_filters_dirty: true,
            ninja_filters_dirty: true,
            poesessid_dirty: false,
            uniques_show_all: false,
            mods_show_all: false,
            uniques_scarce_only: false,
            uniques_sort_by_demand: false,
            alerts_refresh_at: None,
            observe_refresh_at: None,
            last_second: 0,
            settings_form,
            watches_form,
            observations_form,
            watches_table,
            alerts_table,
            uniques_table,
            mods_table,
            observations_table,
            obs_mods_table,
            obs_price_table,
            obs_kind_select,
            uniques_partition_select,
            mods_class_select,
            mods_slot_select,
            mods_rarity_select,
            mods_kind_select,
        };
        for line in boot_log {
            shell.push_log(line);
        }
        shell.refresh_alerts();
        shell.resync_ninja_league();
        shell.reload_observation();
        shell
    }

    /// 当前语言的文案目录。每个页面第一行都是它。
    pub(crate) fn text(&self) -> &'static i18n::Text {
        i18n::text(&self.language)
    }

    /// 往日志里追一行:**先脱敏**,再进抽屉,再落盘。
    ///
    /// 脱敏只在这一处做。日志有四五个来源(后台 actor、登录窗、卡片线程、
    /// 界面自己),每处各记各的迟早会漏一处,而漏出去的那一行会同时出现在
    /// 状态行上和盘上的 `app.log` 里。
    pub(crate) fn push_log(&mut self, line: String) {
        let line = logbook::redact_for_log(&line);
        logbook::append_line(&self.log_path, &line);
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(line);
        // 抽屉开着的时候新行要自己滚进视野:最要紧的永远是最后一条。
        if self.log_open {
            self.log_scroll.scroll_to_bottom();
        }
    }

    /// 抽屉里那几行整份复制走。出问题要贴给别人看的时候,一行行选是不现实的。
    fn copy_log(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        let body = self
            .log
            .iter()
            .cloned()
            .collect::<Vec<String>>()
            .join("\r\n");
        cx.write_to_clipboard(ClipboardItem::new_string(body));
        self.set_notice(text.log_copied.to_owned());
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
        changed |= self.refresh_observation_if_due();

        // 倒计时("42 秒后再轮询")每秒动一次,不必每拍动。
        let now = now_secs();
        if now != self.last_second {
            self.last_second = now;
            if self.page == Page::Watches {
                self.watches_dirty = true;
            }
            // 观察页那两条倒计时精确到秒,更需要每秒重画一次。
            if self.page == Page::Observations {
                self.observations_dirty = true;
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
        if self.observations_dirty {
            self.rebuild_observations_table(cx);
            changed = true;
        }
        if self.obs_mods_dirty {
            self.rebuild_obs_mods_table(cx);
            changed = true;
        }
        if self.obs_price_dirty {
            self.rebuild_obs_price_table(cx);
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
        // 观察页同理:离开这一页期间 actor 照样在往库里写。
        if page == Page::Observations {
            self.reload_observation();
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
        apply_content(&self.watches_table, content, cx);
    }

    /// 提醒表 = `watch.sqlite` 里最近的那 200 行。
    fn rebuild_alerts_table(&mut self, cx: &mut Context<Self>) {
        self.alerts_dirty = false;
        let content = pages::alerts::table_content_for(&self.alert_rows, self.text());
        apply_content(&self.alerts_table, content, cx);
    }

    /// 暗金表 = 选中分区的 `items` 分面 × 经济接口的参考价。
    fn rebuild_uniques_table(&mut self, cx: &mut Context<Self>) {
        self.uniques_dirty = false;
        let content = pages::ninja_uniques::table_content_for(
            &self.ninja.uniques,
            &self.rates,
            self.uniques_view(),
            self.text(),
        );
        apply_content(&self.uniques_table, content, cx);
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
        apply_content(&self.mods_table, content, cx);
    }

    /// 观察表 = 设置里的观察列表 × actor 广播的运行状态。
    fn rebuild_observations_table(&mut self, cx: &mut Context<Self>) {
        self.observations_dirty = false;
        let content = pages::observations::table_content_for(
            &self.settings,
            &self.observation_status,
            self.text(),
            now_secs(),
        );
        apply_content(&self.observations_table, content, cx);
    }

    /// 词缀战绩表 = 选中那条观察的聚合结果,按类型和样本数筛一遍。
    fn rebuild_obs_mods_table(&mut self, cx: &mut Context<Self>) {
        self.obs_mods_dirty = false;
        let kind = selected_value(&self.obs_kind_select, cx);
        let content = pages::observations::mods_table_content_for(
            &self.observe.mods,
            &kind,
            self.obs_min_samples,
            &self.observation_favourites(),
            self.obs_favourites_only,
            self.text(),
        );
        apply_content(&self.obs_mods_table, content, cx);
    }

    /// 价位战绩表 = 选中那条观察按价位分的档,按样本数筛一遍。
    ///
    /// 不看词缀类型下拉:一件货身上有七条词缀,但只有一个价 —— 按"这一档
    /// 里带 explicit 词缀的货"筛出来的数,分母是什么谁也说不清。
    fn rebuild_obs_price_table(&mut self, cx: &mut Context<Self>) {
        self.obs_price_dirty = false;
        let content = pages::observations::price_table_content_for(
            self.observe.prices(self.obs_price_mode),
            self.obs_min_samples,
            self.obs_price_mode,
            self.text(),
        );
        apply_content(&self.obs_price_table, content, cx);
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
        let classes = pages::ninja_mods::class_choices(&self.ninja.classes, text);
        let slots = pages::ninja_mods::slot_choices(&self.ninja.mods, text);
        let rarities = pages::ninja_mods::rarity_choices(&self.ninja.mods, text);
        let kinds = pages::ninja_mods::kind_choices(&self.ninja.mods, text);
        let partition = self.ninja.selected_partition.clone();
        let class = self.ninja.selected_class.clone();

        relabel_select(
            &self.uniques_partition_select.clone(),
            partitions,
            Some(&partition),
            window,
            cx,
        );
        // 分区和职业跟着数据走:上一轮选的那个这一版没有了,`load` 已经把它退回
        // "全部",下拉得跟上,不然它指着一个查不到东西的值。
        relabel_select(
            &self.mods_class_select.clone(),
            classes,
            Some(&class),
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

    /// 登录窗读到的会话写回那个掩码输入框。理由见 `poesessid_dirty`。
    fn sync_poesessid_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.poesessid_dirty {
            return;
        }
        self.poesessid_dirty = false;
        let value = self.settings.poesessid.clone();
        self.settings_form.write_poesessid(value, window, cx);
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

        // 六张表连行带列一起重建(表头和格子里的状态词都跟着语言走),
        // 走各自的 rebuild;那几个下拉交给 `sync_ninja_filters` 和
        // `sync_observation_form`。
        self.watches_dirty = true;
        self.alerts_dirty = true;
        self.uniques_dirty = true;
        self.mods_dirty = true;
        self.observations_dirty = true;
        self.obs_mods_dirty = true;
        self.obs_price_dirty = true;
        self.obs_filters_dirty = true;
        self.ninja_filters_dirty = true;

        self.watches_form.relabel(&self.settings, text, window, cx);
        self.observations_form.relabel(text, window, cx);
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

    /// 底部状态行:有通知就说通知,否则说最后一条日志,左边挂一个日志开关。
    fn status_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let (line, tone) = if self.notice.is_empty() {
            (self.log.back().cloned().unwrap_or_default(), muted())
        } else if self.read_only {
            (self.notice.clone(), warn_amber())
        } else {
            (self.notice.clone(), c(ACCENT_TEXT))
        };
        div()
            .h(px(26.))
            .flex_none()
            .h_flex()
            .items_center()
            .gap(px(8.))
            .px(px(6.))
            .bg(c(RAIL))
            .border_t_1()
            .border_color(c(HAIRLINE))
            .child(
                Button::new("log-toggle")
                    .ghost()
                    .xsmall()
                    .selected(self.log_open)
                    .label(text.log_toggle)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.log_open = !this.log_open;
                        if this.log_open {
                            this.log_scroll.scroll_to_bottom();
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(fs(FS_11))
                    .text_color(tone)
                    .child(SharedString::from(line)),
            )
    }

    /// 日志抽屉:最近三百行,新的在下面。
    ///
    /// 状态行只放得下一条,而"刚才那一串到底发生了什么"要连着看才成句。
    /// 复制按钮把整份丢进剪贴板 —— 出问题时要贴出去的就是这一整段。
    fn log_pane(&self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let lines: Vec<SharedString> = self
            .log
            .iter()
            .map(|line| SharedString::from(line.clone()))
            .collect();
        panel()
            .flex_none()
            .h(px(H_LOG_PANE))
            .child(
                div()
                    .flex_none()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(4.))
                    .border_b_1()
                    .border_color(c(HAIRLINE))
                    .child(
                        div()
                            .text_size(fs(FS_11_5))
                            .text_color(c(TEXT_SECONDARY))
                            .child(text.log_toggle),
                    )
                    .child(
                        // 盘上那份在哪儿,得说出来:关掉程序之后要翻的是它。
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_family(FONT_MONO)
                            .text_size(fs(FS_10_5))
                            .text_color(muted())
                            .child(SharedString::from(i18n::fill(
                                text.log_file_path,
                                &[&self.log_path.display().to_string()],
                            ))),
                    )
                    .child(
                        Button::new("log-copy")
                            .ghost()
                            .xsmall()
                            .label(text.log_copy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.copy_log(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("log-lines")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.log_scroll)
                    .flex()
                    .flex_col()
                    .px(px(8.))
                    .py(px(4.))
                    .font_family(FONT_MONO)
                    .text_size(fs(FS_10_5))
                    .text_color(c(TEXT_DATA))
                    .children(
                        lines
                            .is_empty()
                            .then(|| div().text_color(muted()).child(text.log_empty)),
                    )
                    .children(lines.into_iter().map(|line| div().child(line))),
            )
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_language(window, cx);
        self.sync_ninja_filters(window, cx);
        self.sync_poesessid_field(window, cx);
        self.sync_watch_form(window, cx);
        self.sync_observation_form(window, cx);
        let text = self.text();

        let body = match self.page {
            Page::Watches => self.render_watches(cx),
            Page::Alerts => self.render_alerts(cx),
            Page::Observations => self.render_observations(cx),
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
            .children(self.log_open.then(|| self.log_pane(cx)))
            .child(self.status_bar(cx))
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
///
/// `stripe(false)`:上游的斑马开关顺手会在数据行下面补一批空的假行把表铺满,
/// 那串没有内容的条纹看起来像"还有几条正在加载"。条纹改由 `SimpleTable`
/// 自己的 `render_tr` 画,只画真行。
pub(crate) fn table(
    state: &Entity<TableState<SimpleTable>>,
) -> gpui_component::table::Table<SimpleTable> {
    gpui_component::table::Table::new(state)
        .stripe(false)
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
