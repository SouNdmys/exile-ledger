//! 运行时 actor:一条 `pnd-runtime` 线程,手里攥着这个程序全部会动的东西 ——
//! 蹲价库、交易网关、轮询时间表、当前设置、汇率表。
//!
//! 界面(以及探针)只做两件事:往 [`RuntimeHandle::try_send`] 里投命令、
//! 从 [`RuntimeHandle::try_next_event`] 里取事件。两个方法都不阻塞,也不 join
//! 任何线程 —— GPUI 的 120ms tick 卡在这里一下,整个界面就跟着卡一下。
//!
//! 为什么是一条线程而不是一堆锁:所有状态只有一个写者,就不会出现"轮询线程
//! 刚判定完、设置线程把这条搜索删了"这种问题。真正会等的活(HTTP、SQLite)
//! 要么在网关线程上,要么快到可以忽略。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pnd_domain::{
    CurrencyRates, ListingSummary, ObservationId, PriceCap, SearchRef, WatchId, classify_gone,
    decode_search_id, judge, next_check_after, search_page_url, search_request_body, with_sort,
};
use pnd_ninja::client::NinjaClient;
use pnd_settings::{AppSettings, ObservationEntry, WatchEntry};
use pnd_storage::{
    AlertRow, AlertSource, DueListing, LiveState, NewAlert, ObservationRun, StorageError,
    WatchState, WatchStore, default_watch_db_path,
};
use pnd_trade::live::{LiveConfig, MAX_LIVE_CONNECTIONS_PER_ACCOUNT};
use pnd_trade::{
    BucketUsage, Budget, FETCH_LONG_WINDOW_REQUESTS, FETCH_LONG_WINDOW_SECS, MAX_FETCH_IDS,
    SEARCH_LONG_WINDOW_REQUESTS, SEARCH_LONG_WINDOW_SECS, TradeClient, ggg_error, jwt_expiry,
};
use thiserror::Error;

use crate::decide::{Decision, MatchedListing, coalesce, decide};
use crate::gateway::{
    GatewayError, GatewayEvent, GatewayHandle, GatewayReply, GatewayRequest, Priority, ReplyKind,
    RequestKind, RequestTag, SearchOutcome, SessionCheckOutcome, TradeGateway, TradeTransport,
};
use crate::live_worker::{
    LiveConnector, LiveEvent, LiveOffReason, LiveRunState, LiveTarget, LiveWorkerConfig,
    LiveWorkerHandle, TungsteniteConnector, spawn_live_worker,
};
use crate::observe::{ObserveScheduler, SWEEP_INTERVAL_SECS, fetch_listing_cap};
use crate::poll::{PollOutcome, PollScheduler, budget_floor_interval};
use crate::{describe_token, now_secs};

/// 汇率多久重读一次。ninja 自己的缓存是 5 分钟,15 分钟一次既不会读到
/// 陈年数据,也不至于为了一张换算表天天打扰人家。
const RATES_REFRESH_SECONDS: u64 = 15 * 60;

/// 主循环最长睡多久。轮询的时间表通常比这远得多,但一秒醒一次
/// 才能及时看到"该关门了"。
const MAX_LOOP_WAIT: Duration = Duration::from_secs(1);

/// 请求标签:回信回来时靠它认出这是轮询的哪一步。
const SEARCH_LABEL: &str = "poll-search";
const FETCH_LABEL: &str = "poll-fetch";
/// live 推来的 id 去抓详情。和轮询的 fetch 分开标,因为它不属于任何一轮轮询:
/// 回来了不该重排时间表,也不该清失败计数。
const LIVE_FETCH_LABEL: &str = "live-fetch";
/// "去藏身处"链路上的两步。
const HIDEOUT_FETCH_LABEL: &str = "hideout-fetch";
const HIDEOUT_WHISPER_LABEL: &str = "hideout-whisper";
/// 设置页那个"测试会话"发出去的一次搜索。
const SESSION_CHECK_LABEL: &str = "session-check";
/// 市场观察的四步:兜底拉最新 100 条、抓那一轮里新面孔的详情、
/// 抓秒推来的新面孔、回查到点的在册挂单。
const OBSERVE_SEARCH_LABEL: &str = "observe-discover";
const OBSERVE_DISCOVER_FETCH_LABEL: &str = "observe-discover-fetch";
const OBSERVE_LIVE_FETCH_LABEL: &str = "observe-live-fetch";
const OBSERVE_RECHECK_LABEL: &str = "observe-recheck";

/// token 里没有 `exp` 时的兜底:拿到超过这么久就当它过期了。
/// 2026-09-07 抓包证实 hideout_token 只活 300 秒,这里再减掉和
/// [`HIDEOUT_TOKEN_EXPIRY_MARGIN_SECS`] 一样的 30 秒余量。
///
/// 只是兜底:token 自己带 `exp` 的时候听它的(见 [`usable_token`])。
const HIDEOUT_TOKEN_MAX_AGE_SECS: i64 = 270;

/// `exp` 只剩这么点(秒)就别发了,先换一张。
///
/// 30 秒是给路上留的:请求要在网关队列里排一下、TLS 要握一次手,再加上
/// 本机和服务端的时钟差。卡着最后一秒发出去,到那头正好过期。
const HIDEOUT_TOKEN_EXPIRY_MARGIN_SECS: i64 = 30;

/// 一次点击最多 POST 几次 whisper。两次:原始 token 一次,换过 token 再一次。
/// 再不成就停手 —— 反复捅一个拒绝我们的接口没有意义。
const MAX_WHISPER_POSTS: u32 = 2;

/// 关机时等一条 live worker 走掉最多等这么久。
///
/// 它可能正卡在一次读超时里(默认 30 秒)。等不到就放它去:线程手里只有
/// 自己的 socket,进程退出时系统会收走 —— 为了一句道别让程序关不掉不值得。
const LIVE_JOIN_TIMEOUT: Duration = Duration::from_millis(300);

/// 内存库的路径写法。`WatchStore::open_in_memory` 走的是 SQLite 自己的
/// `:memory:`,这里借同一个字面量当哨兵,免得再造一个枚举。
pub const IN_MEMORY_DB: &str = ":memory:";

// ---------------------------------------------------------------------
// 对外的命令 / 事件
// ---------------------------------------------------------------------

/// 界面能让运行时做的事。
#[derive(Debug, Clone)]
pub enum RuntimeCommand {
    /// 设置整份换掉。运行时自己 diff:哪条搜索是新的、哪条改了、哪条没了。
    /// (装箱是因为 `AppSettings` 比别的命令大一个数量级,不装箱整个枚举
    /// 都得按它的尺寸走。)
    ApplySettings(Box<AppSettings>),
    PollNow(WatchId),
    Dismiss {
        alert_id: i64,
    },
    Action {
        alert_id: i64,
        action: String,
    },
    /// 卡片上的"去藏身处":给这条提醒的卖家发一次传送请求。
    ///
    /// **只有用户点一次才会发出这条命令** —— 程序自己永远不发,这是计划里
    /// 那条"每个游戏内动作都是你自己点一次"的实现。
    TravelToHideout {
        alert_id: i64,
    },
    /// 设置页上的"测试会话":拿现在这个 POESESSID 发一次搜索,看服务端
    /// 认不认它。花掉一次 search 额度,所以也只在用户点的时候发。
    TestSession,
    /// 市场观察页上的"立刻找一次新的":排一次 discover(1 次 search + 新面孔的
    /// fetch)。和 `PollNow` 一样,只在用户点的时候发。
    DiscoverNow {
        obs_id: ObservationId,
    },
    /// "立刻回查一次":把在册的挂单按 10 个一批查一遍,看谁没了。
    RecheckNow {
        obs_id: ObservationId,
    },
    Shutdown,
}

/// "去藏身处"点一次的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HideoutOutcome {
    /// 发出去了,游戏里应该已经收到传送邀请。
    Sent,
    /// 设置里没有 POESESSID(或者它已经失效)—— 这个接口必须带会话。
    NoSession,
    /// 这条挂单根本没有 hideout_token:多半是当初匿名 fetch 回来的,
    /// 重新抓一次也没有。只能去网页上点。
    TokenMissing,
    /// token 太老或者被拒了,已经重新抓了一次,正在再试一遍。
    /// 这是过程,不是结局:后面还会有一个 `Sent` 或 `Failed`。
    Refreshed,
    /// 失败了,不再重试。`status` 是交易站的状态码(0 = 压根没发出去)。
    Failed { status: u16, message: String },
}

impl HideoutOutcome {
    /// 写进提醒历史 `last_action` 的字符串。界面按它显示脚注,
    /// 所以这几个值是接口的一部分,别随手改。
    #[must_use]
    pub fn action(&self) -> &'static str {
        match self {
            HideoutOutcome::Sent => "hideout_sent",
            HideoutOutcome::NoSession => "hideout_no_session",
            HideoutOutcome::TokenMissing => "hideout_token_missing",
            HideoutOutcome::Refreshed => "hideout_refreshing",
            HideoutOutcome::Failed { .. } => "hideout_failed",
        }
    }

    /// 这是不是最后一句话。`Refreshed` 之后还有下文。
    #[must_use]
    pub fn is_final(&self) -> bool {
        !matches!(self, HideoutOutcome::Refreshed)
    }
}

/// 一条搜索现在处在什么档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatchRunState {
    /// 用户关掉了它。
    #[default]
    Disabled,
    /// 正常轮询中。
    Polling,
    /// 连着失败,正在退避。
    Backoff,
    /// WebSocket 在线(第二阶段才会出现)。
    Live,
    /// 网关被 Cloudflare 拦住,整条队列停着。
    Held,
}

/// 给界面看的一条搜索的运行状态。每次状态有变化就整份广播一遍 ——
/// 界面不需要自己攒增量。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WatchStatus {
    pub state: WatchRunState,
    /// WebSocket 那一头现在怎么样。`state` 只有一个 `Live` 档,这里才说得清
    /// "为什么没连上""退避到什么时候"。
    pub live: LiveRunState,
    pub next_poll_at: Option<i64>,
    /// 现在这条搜索多久轮一次(秒)。设置里那个数字和预算地板取大者,
    /// 退避期间就是退避后的间隔;0 = 没在排班(停用了,或者搜索 id 坏了)。
    ///
    /// 界面要它是因为"下一轮 14 分钟后"回答不了"到底多久一次" —— 那两个
    /// 数字在秒推连上、退避、多加一条搜索之后都会变。
    pub poll_every_secs: u64,
    pub last_poll_at: Option<i64>,
    pub last_total: Option<u64>,
    pub hits_today: u32,
    pub failures: u32,
    pub last_error: Option<String>,
}

/// 给界面看的一条市场观察的运行状态。
///
/// 只有"这条观察现在怎么样",没有任何统计结果:聚合表、挂单流那些是几百行
/// 的东西,每次广播都塞一份既大又陈旧。界面收到 [`RuntimeEvent::ObservationChanged`]
/// 之后自己去库里读,读到的一定是最新的。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservationStatus {
    /// 还挂着的挂单条数。
    pub active: u32,
    /// 已经不见了的条数(含判成 `Unknown` 的)。
    pub gone: u32,
    /// WebSocket 那一头现在怎么样。界面上"秒推:已连接 / 重连中 / 未登录"
    /// 那一行就是它 —— 观察的新挂单主要靠它,轮询只是兜底。
    pub live: LiveRunState,
    /// 秒推一共推来过多少条挂单 id(采样掉的也算)。
    pub pushed_total: u64,
    /// 其中按 `sample_every` 丢掉、没去抓详情的有多少条。
    pub sampled_out_total: u64,
    pub last_discover_at: Option<i64>,
    pub last_recheck_at: Option<i64>,
    /// 下一次兜底 discover 的 unix 秒;不在排班(停用了、搜索 id 坏了)
    /// 时是 `None`。
    pub next_discover_at: Option<i64>,
    /// 在册挂单里最早该回头看的那一刻。回查走的是每条挂单自己的阶梯,
    /// 所以这不是"下一轮全量回查",而是"下一条到点的挂单"。
    pub next_recheck_at: Option<i64>,
    pub last_error: Option<String>,
}

/// 运行时向外广播的一切。
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEvent {
    /// 线程起来了。
    Ready,
    WatchStatus {
        watch_id: WatchId,
        status: WatchStatus,
    },
    Budget {
        policy: String,
        usage: Vec<BucketUsage>,
        /// 这条策略还要等几秒才放行下一封请求;`None` = 现在就能发。
        next_allowed_in_secs: Option<u64>,
    },
    /// 命中了,该弹卡片了。(装箱同 `ApplySettings`:里面整条挂单摘要很大。)
    ListingMatched(Box<MatchedListing>),
    /// 一次"去藏身处"的进展。`Refreshed` 之后还会再来一条。
    HideoutResult {
        alert_id: i64,
        outcome: HideoutOutcome,
    },
    /// 一次"测试会话"的结果。`detail` 是给人看的技术细节(状态码 + 规则名),
    /// 界面把它原样挂在按钮旁边 —— 不认得的失败也就有话可说。
    SessionChecked {
        valid: bool,
        detail: String,
    },
    /// 一条市场观察的运行状态变了(排班动了、一轮跑完了、出错了)。
    ObservationStatus {
        obs_id: ObservationId,
        status: ObservationStatus,
    },
    /// 一条市场观察的**库里的数据**变了 —— 每轮 discover / recheck 跑完发一次。
    ///
    /// 事件里不带数据:观察页要的那几张表(聚合、挂单流)动辄几百行,
    /// 而且用户还在上面筛选。界面收到这一句就自己去库里重读一遍。
    ObservationChanged {
        obs_id: ObservationId,
    },
    /// POESESSID 失效了,程序已经停用它。
    SessionInvalid,
    CloudflareBlocked {
        until: i64,
    },
    RatesUpdated(CurrencyRates),
    Log(String),
    /// 运行时线程非正常结束。界面收到这个就该停止显示"运行中"。
    Fault(String),
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("could not open the watch database: {0}")]
    Storage(#[from] StorageError),
    #[error("could not start the runtime thread: {0}")]
    Spawn(String),
    #[error("the runtime thread is no longer running")]
    Stopped,
}

/// 运行时要用到的文件位置。现在只有一个库,包一层是为了以后加
/// `ninja.sqlite` 时不用改所有调用点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub watch_db: PathBuf,
}

impl Default for RuntimePaths {
    fn default() -> Self {
        RuntimePaths {
            watch_db: default_watch_db_path(),
        }
    }
}

impl RuntimePaths {
    #[must_use]
    pub fn new(watch_db: PathBuf) -> RuntimePaths {
        RuntimePaths { watch_db }
    }

    /// 测试和探针用:库开在内存里,跑完什么都不留。
    #[must_use]
    pub fn in_memory() -> RuntimePaths {
        RuntimePaths {
            watch_db: PathBuf::from(IN_MEMORY_DB),
        }
    }

    fn open(&self) -> Result<WatchStore, StorageError> {
        if self.watch_db == Path::new(IN_MEMORY_DB) {
            WatchStore::open_in_memory()
        } else {
            WatchStore::open(&self.watch_db)
        }
    }
}

// ---------------------------------------------------------------------
// 句柄
// ---------------------------------------------------------------------

/// actor 线程的邮箱。五个来源合并成一个通道,主循环只用在一个地方等。
enum Inbox {
    Command(RuntimeCommand),
    Reply(GatewayReply),
    Gateway(GatewayEvent),
    Live(LiveEvent),
    Rates(CurrencyRates),
}

/// 界面手里的那一头。丢掉它就是关机。
pub struct RuntimeHandle {
    inbox: Sender<Inbox>,
    events: Receiver<RuntimeEvent>,
    join: Option<JoinHandle<()>>,
}

/// 换 User-Agent 或者换预算时,拿新设置现造一个传输层。
///
/// 用函数指针而不是闭包:它只需要"照设置造一个客户端"这一件事,
/// 而函数指针不用装箱、天然 `Send`。离线注入的传输层没有这个能力,
/// 那时是 `None`(测试本来也不会去改 UA)。
type TransportFactory = fn(&AppSettings) -> Box<dyn TradeTransport>;

fn production_transport(settings: &AppSettings) -> Box<dyn TradeTransport> {
    Box::new(TradeClient::new(settings.user_agent()))
}

impl RuntimeHandle {
    /// 真跑:自己造 `TradeClient`,并起一条 `pnd-rates` 线程读汇率。
    pub fn start(
        settings: AppSettings,
        paths: RuntimePaths,
    ) -> Result<RuntimeHandle, RuntimeError> {
        let transport = production_transport(&settings);
        Self::spawn(
            settings,
            paths,
            transport,
            Arc::new(TungsteniteConnector),
            Some(production_transport),
            true,
        )
    }

    /// 测试(和以后的离线回放)用:传输层由调用方给,汇率线程不起 ——
    /// 一个测试不该因为 poe.ninja 今天抽风而红。
    ///
    /// **live 那一头还是真的**:设置里有 POESESSID、搜索又勾着 live 的话,
    /// worker 会真的去连 GGG。离线跑的测试要么把 `live` 关掉,要么改用
    /// [`RuntimeHandle::start_offline_with_connector`]。
    pub fn start_offline(
        settings: AppSettings,
        paths: RuntimePaths,
        transport: Box<dyn TradeTransport>,
    ) -> Result<RuntimeHandle, RuntimeError> {
        Self::spawn(
            settings,
            paths,
            transport,
            Arc::new(TungsteniteConnector),
            None,
            false,
        )
    }

    /// 同上,但 live 连接也由调用方给一个假的。测试用这个把整条
    /// "推送 → fetch → 判定 → 卡片"链路在没有网络的情况下跑完。
    pub fn start_offline_with_connector(
        settings: AppSettings,
        paths: RuntimePaths,
        transport: Box<dyn TradeTransport>,
        connector: Arc<dyn LiveConnector>,
    ) -> Result<RuntimeHandle, RuntimeError> {
        Self::spawn(settings, paths, transport, connector, None, false)
    }

    fn spawn(
        mut settings: AppSettings,
        paths: RuntimePaths,
        transport: Box<dyn TradeTransport>,
        connector: Arc<dyn LiveConnector>,
        rebuild: Option<TransportFactory>,
        fetch_rates: bool,
    ) -> Result<RuntimeHandle, RuntimeError> {
        settings.normalize();
        // 库在调用方这一侧打开,失败就直接把错误还给它 —— 线程起来之后再
        // 报"库打不开"就只剩一个 Fault 事件,界面没法在启动时提示用户。
        let store = paths.open()?;

        let (inbox_tx, inbox_rx) = channel::<Inbox>();
        let (events_tx, events_rx) = channel::<RuntimeEvent>();
        let thread_inbox = inbox_tx.clone();

        let join = thread::Builder::new()
            .name("pnd-runtime".to_owned())
            .spawn(move || {
                let mut sentinel = FaultOnDrop {
                    events: events_tx.clone(),
                    armed: true,
                };
                let mut actor = RuntimeActor::new(
                    settings,
                    store,
                    transport,
                    connector,
                    rebuild,
                    events_tx.clone(),
                    thread_inbox,
                );
                let _ = events_tx.send(RuntimeEvent::Ready);
                if fetch_rates {
                    actor.restart_rates_thread();
                }
                actor.apply_initial_settings();
                actor.run(&inbox_rx);
                sentinel.armed = false;
            })
            .map_err(|error| RuntimeError::Spawn(error.to_string()))?;

        Ok(RuntimeHandle {
            inbox: inbox_tx,
            events: events_rx,
            join: Some(join),
        })
    }

    /// 投一条命令。通道是无界的,所以这里永远不会因为"队列满"失败;
    /// 唯一的失败是线程已经没了。
    pub fn try_send(&self, command: RuntimeCommand) -> Result<(), RuntimeError> {
        self.inbox
            .send(Inbox::Command(command))
            .map_err(|_| RuntimeError::Stopped)
    }

    /// 取一个事件,没有就是 `None`。界面每 tick 抽干为止。
    #[must_use]
    pub fn try_next_event(&self) -> Option<RuntimeEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        let _ = self.inbox.send(Inbox::Command(RuntimeCommand::Shutdown));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 线程要是 panic 了,至少让界面知道一声。没有它,界面会一直显示"运行中",
/// 而事件通道其实早就没人写了。
struct FaultOnDrop {
    events: Sender<RuntimeEvent>,
    armed: bool,
}

impl Drop for FaultOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.events.send(RuntimeEvent::Fault(
                "the runtime thread stopped unexpectedly".to_string(),
            ));
        }
    }
}

// ---------------------------------------------------------------------
// actor
// ---------------------------------------------------------------------

/// 一条搜索在运行时里的全部状态。设置里那条 `WatchEntry` 是"用户填的",
/// 这里是"跑起来之后的"。
struct WatchRuntime {
    entry: WatchEntry,
    /// 搜索 id 解码 + 包好的 search 请求体。解不开的话是 `None`,那条搜索不排班。
    query_body: Option<String>,
    /// 上一次 search 回来的服务端搜索 id,fetch 要用。
    search_id: String,
    /// 加进来之后第一轮跑完了没有。`alert_on_first_poll = false` 的搜索靠它
    /// 把第一轮的存量货色静音。
    first_poll_done: bool,
    /// 有一次轮询正在路上。防止同一条搜索被排两次班。
    in_flight: bool,
    status: WatchStatus,
}

/// 一条市场观察在运行时里的全部状态。
struct ObservationRuntime {
    entry: ObservationEntry,
    /// 搜索 id 解出来的查询,换成**按上架时间倒序**的请求体
    /// (`with_sort(q, "indexed", "desc")`)—— 观察要的是"最新 100 条",
    /// 不是蹲价那个"最便宜的 100 条"。解不开就是 `None`,那条观察不排班。
    query_body: Option<String>,
    /// 上一次 discover 回来的服务端搜索 id,fetch 要拿它当 `?query=`。
    search_id: String,
    /// discover 的那次 search 正在路上。
    discover_in_flight: bool,
    /// 这一轮 discover 还有几批 fetch 没回来。
    ///
    /// 数它是为了知道"这一轮什么时候算跑完":跑完才落 `last_discover_at`、
    /// 才广播一次 `ObservationChanged`,而不是每回来一批就喊一声。
    discover_pending: usize,
    /// 秒推来的第几条了。采样("每 N 条抓一条")数的就是它,
    /// 而且**跨推送连续数** —— 每来一批就从头数的话,一批一条的时候
    /// 永远只会抓到第一条,那不是抽样,是偏样。
    pushed_seen: u64,
    status: ObservationStatus,
}

/// 一次"去藏身处"点击走到哪一步了。
///
/// 一个 alert 同时只会有一条:第二次点击在这条还没走完时会被挡掉,
/// 否则"每次点击最多 POST 两次"这条纪律就成了空话。
struct HideoutFlow {
    listing_id: String,
    league: String,
    search_id: String,
    /// 已经 POST 过几次 whisper。上限 [`MAX_WHISPER_POSTS`]。
    posts: u32,
    /// 这次点击已经重新 fetch 过 token 没有。最多一次。
    refreshed: bool,
}

struct RuntimeActor {
    settings: AppSettings,
    store: WatchStore,
    gateway: GatewayHandle,
    /// 网关回信的地址,每封请求里都塞一份克隆。
    replies: Sender<GatewayReply>,
    /// 网关广播的地址。重建网关时要再给一份。
    gateway_events: Sender<GatewayEvent>,
    /// live worker 说话的地址,每条 worker 拿一份克隆。
    live_events: Sender<LiveEvent>,
    events: Sender<RuntimeEvent>,
    inbox: Sender<Inbox>,
    scheduler: PollScheduler,
    watches: BTreeMap<WatchId, WatchRuntime>,
    /// 市场观察的时间表和状态。和蹲价并排,共用同一条网关、同一份预算。
    observe_scheduler: ObserveScheduler,
    observations: BTreeMap<ObservationId, ObservationRuntime>,
    /// 怎么去开一条 live 连接。生产是 tungstenite,测试是写好剧本的假货。
    connector: Arc<dyn LiveConnector>,
    /// 正在跑的 live worker,一条搜索(或一条观察)最多一条。
    /// 两种共用同一份"最多几条"的额度,所以也共用这一张表。
    live_workers: BTreeMap<LiveTarget, LiveWorkerHandle>,
    /// 已经发出去、还没回来的回查批次:批次号 → 这一批问的是谁的哪几条挂单。
    ///
    /// 一批里可以混着好几条观察,而回信只按挂单 id 对号,所以"这条 id 是替
    /// 哪条观察问的"只能记在这里。
    sweeps: BTreeMap<u64, Vec<DueListing>>,
    next_sweep_id: u64,
    /// 手上这个 POESESSID 还能用吗。服务端拒过一次就翻成 false,
    /// 直到用户粘一个新的进来 —— 不拿死会话反复试。
    session_ok: bool,
    /// "会话失效"这句话已经说过了没有。说一次就够,说三次是骚扰。
    session_invalid_reported: bool,
    /// "没有会话所以 live 开不起来"这句话已经说过了没有。
    no_session_reported: bool,
    /// 每个还没走完的"去藏身处"。
    hideout: BTreeMap<i64, HideoutFlow>,
    rates: CurrencyRates,
    rebuild: Option<TransportFactory>,
    /// 汇率线程的取消开关。换联赛时立一次、重起一条。
    rates_cancel: Option<Arc<AtomicBool>>,
    /// 现在这条网关线程是按哪个 UA / 哪份预算起的。
    user_agent: String,
    budget: Budget,
    shutdown: bool,
}

impl RuntimeActor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        settings: AppSettings,
        store: WatchStore,
        transport: Box<dyn TradeTransport>,
        connector: Arc<dyn LiveConnector>,
        rebuild: Option<TransportFactory>,
        events: Sender<RuntimeEvent>,
        inbox: Sender<Inbox>,
    ) -> RuntimeActor {
        let (reply_tx, reply_rx) = channel::<GatewayReply>();
        let (gateway_tx, gateway_rx) = channel::<GatewayEvent>();
        let (live_tx, live_rx) = channel::<LiveEvent>();
        // 三个上游通道各起一条搬运线程,把东西倒进 actor 的单一邮箱。
        // 这样主循环只用在一个地方等,而不是轮询四个通道。三条线程都在
        // 上游通道断掉时自己结束,不需要 join。
        forward(reply_rx, inbox.clone(), Inbox::Reply, "pnd-gw-replies");
        forward(gateway_rx, inbox.clone(), Inbox::Gateway, "pnd-gw-events");
        forward(live_rx, inbox.clone(), Inbox::Live, "pnd-live-events");

        let budget = budget_of(&settings);
        let user_agent = settings.user_agent();
        let gateway =
            TradeGateway::start(transport, budget, session_of(&settings), gateway_tx.clone());

        RuntimeActor {
            settings,
            store,
            gateway,
            replies: reply_tx,
            gateway_events: gateway_tx,
            live_events: live_tx,
            events,
            inbox,
            scheduler: PollScheduler::new(),
            watches: BTreeMap::new(),
            observe_scheduler: ObserveScheduler::new(),
            observations: BTreeMap::new(),
            connector,
            live_workers: BTreeMap::new(),
            sweeps: BTreeMap::new(),
            next_sweep_id: 0,
            session_ok: true,
            session_invalid_reported: false,
            no_session_reported: false,
            hideout: BTreeMap::new(),
            rates: CurrencyRates::none(),
            rebuild,
            rates_cancel: None,
            user_agent,
            budget,
            shutdown: false,
        }
    }

    /// 启动时把手上这份设置当成一次 `ApplySettings` 走一遍,免得两条路。
    fn apply_initial_settings(&mut self) {
        let settings = self.settings.clone();
        self.apply_settings(settings, now_secs());
    }

    fn run(&mut self, inbox: &Receiver<Inbox>) {
        while !self.shutdown {
            let now = now_secs();
            for watch_id in self.scheduler.due(now) {
                self.start_poll(&watch_id, now);
            }
            for obs_id in self.observe_scheduler.due(now) {
                self.start_discover(&obs_id, now);
            }
            if self.observe_scheduler.sweep_due(now) {
                self.sweep_due_listings(now);
            }

            match inbox.recv_timeout(self.wait(now)) {
                Ok(Inbox::Command(command)) => self.handle_command(command),
                Ok(Inbox::Reply(reply)) => self.handle_reply(reply),
                Ok(Inbox::Gateway(event)) => self.handle_gateway_event(event),
                Ok(Inbox::Live(event)) => self.handle_live_event(event),
                Ok(Inbox::Rates(rates)) => {
                    self.rates = rates.clone();
                    self.emit(RuntimeEvent::RatesUpdated(rates));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if let Some(cancel) = self.rates_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        // 先给所有 worker 立取消标志,再一条条等:反过来的话第一条要等满
        // 一个读超时,后面的才刚被通知到。
        let workers: Vec<LiveWorkerHandle> = std::mem::take(&mut self.live_workers)
            .into_values()
            .collect();
        for worker in &workers {
            worker.stop();
        }
        for worker in workers {
            worker.stop_and_join(LIVE_JOIN_TIMEOUT);
        }
    }

    /// 睡到下一件该做的事的时刻,封顶一秒。蹲价和观察两张时间表取早的那个。
    fn wait(&self, now: i64) -> Duration {
        let deadline = match (
            self.scheduler.next_deadline(),
            self.observe_scheduler.next_deadline(),
        ) {
            (Some(poll), Some(observe)) => Some(poll.min(observe)),
            (poll, observe) => poll.or(observe),
        };
        match deadline {
            Some(at) => {
                Duration::from_secs((at - now).clamp(0, MAX_LOOP_WAIT.as_secs() as i64) as u64)
                    .max(Duration::from_millis(50))
            }
            None => MAX_LOOP_WAIT,
        }
    }

    // ---- 命令 --------------------------------------------------------

    fn handle_command(&mut self, command: RuntimeCommand) {
        let now = now_secs();
        match command {
            RuntimeCommand::ApplySettings(settings) => self.apply_settings(*settings, now),
            RuntimeCommand::PollNow(watch_id) => {
                self.scheduler.poll_now(&watch_id, now);
            }
            RuntimeCommand::Dismiss { alert_id } => {
                self.note(self.store.mark_dismissed(alert_id, now), "mark_dismissed");
            }
            RuntimeCommand::Action { alert_id, action } => {
                self.note(
                    self.store.set_last_action(alert_id, &action),
                    "set_last_action",
                );
            }
            RuntimeCommand::TravelToHideout { alert_id } => {
                self.travel_to_hideout(alert_id, now);
            }
            RuntimeCommand::TestSession => self.test_session(),
            RuntimeCommand::DiscoverNow { obs_id } => {
                self.observe_scheduler.run_now(&obs_id, now);
            }
            // "立刻回查一次" = 把这条观察在册的挂单全部推到此刻到期,
            // 然后照常走那一条扫描的路(同一份额度、同样分批)。
            RuntimeCommand::RecheckNow { obs_id } => {
                self.note(self.store.mark_all_due(&obs_id, now), "mark_all_due");
                self.observe_scheduler.sweep_now(now);
            }
            RuntimeCommand::Shutdown => self.shutdown = true,
        }
    }

    /// 整份设置换掉:先看要不要换网关,再逐条 diff 搜索。
    fn apply_settings(&mut self, mut settings: AppSettings, now: i64) {
        settings.normalize();

        let user_agent = settings.user_agent();
        let budget = budget_of(&settings);
        let session_changed = settings.poesessid != self.settings.poesessid;
        let league_changed = settings.league != self.settings.league;
        if session_changed {
            // 用户换了一个新的 cookie:之前那句"会话失效"作废,重新试一次。
            self.session_ok = true;
            self.session_invalid_reported = false;
            self.no_session_reported = false;
        }
        let session = usable_session(&settings, self.session_ok);

        // UA 和预算都是在网关线程创建时定死的(一个在客户端里,一个在限速器里),
        // 改它们只能换一条线程。这是罕见操作(用户在设置页动了旋钮),
        // 代价是丢掉学到的限速头,下一次请求会重新学回来。
        if (user_agent != self.user_agent || budget != self.budget)
            && let Some(rebuild) = self.rebuild
        {
            self.gateway = TradeGateway::start(
                rebuild(&settings),
                budget,
                session.clone(),
                self.gateway_events.clone(),
            );
            self.user_agent = user_agent;
            self.budget = budget;
            self.emit(RuntimeEvent::Log(
                "restarted the trade gateway (user agent or budget changed)".to_string(),
            ));
        } else if session_changed {
            self.gateway.set_session(session.clone());
        }

        let mut keep: BTreeSet<WatchId> = BTreeSet::new();
        let enabled_count = settings
            .watches
            .iter()
            .filter(|entry| entry.enabled)
            .count();
        let enabled_observations = settings
            .observations
            .iter()
            .filter(|entry| entry.enabled)
            .count();
        // 搜索额度 6 小时就那么多次,几条搜索分着花。设置里填得再勤,
        // 也不能勤过这个地板 —— 超了不是报错,是被服务端 429 冷却。
        //
        // **观察也要数进来**:一次 discover 就是一次 search,和蹲价花的是
        // 同一份额度。两边各算各的地板的话,三条搜索 + 三条观察就会一起
        // 按"三个用户"的节奏跑,加起来正好超一倍。
        let search_floor = budget_floor_interval(
            enabled_count + enabled_observations,
            budget.effective_limit(SEARCH_LONG_WINDOW_REQUESTS),
            SEARCH_LONG_WINDOW_SECS,
        );
        self.scheduler.set_budget_floor(search_floor);
        self.observe_scheduler.set_discover_floor(search_floor);
        let mut stagger = 0usize;

        for entry in &settings.watches {
            keep.insert(entry.id.clone());
            if !entry.enabled {
                self.scheduler.remove(&entry.id);
                self.upsert_watch(entry.clone(), None, WatchRunState::Disabled, None, now);
                continue;
            }

            let known = self.watches.get(&entry.id);
            let query_stale = match known {
                None => true,
                Some(runtime) => {
                    runtime.query_body.is_none()
                        || runtime.entry.search_id != entry.search_id
                        || runtime.entry.league != entry.league
                }
            };
            // 上限(金额或货币)是用户刚刚亲手改过的东西 —— 他在等答案,
            // 而不是等下一个整点。别的改动(改名、开关 live)不排队,
            // 免得每保存一次就白花一次搜索额度。
            let cap_changed =
                known.is_some_and(|runtime| runtime.entry.price_cap != entry.price_cap);

            let query_body = if query_stale {
                // 搜索 id 或联赛变了 = 这是另一次搜索。已经连着的那条 live
                // 盯的是旧的,停掉它,下面的 `sync_live_workers` 会照新的重开。
                self.stop_live_worker(&LiveTarget::Watch(entry.id.clone()));
                match decode_search_id(&entry.search_id) {
                    Ok(query_json) => {
                        self.persist_watch(entry, &query_json, now);
                        Some(search_request_body(&query_json))
                    }
                    Err(error) => {
                        // 粘错了的 id 不排班,但这条搜索留在列表里,
                        // 界面上带着错误原因,用户改掉就能继续。
                        self.scheduler.remove(&entry.id);
                        let message = format!("{}: {error}", entry.label);
                        self.emit(RuntimeEvent::Log(message.clone()));
                        self.upsert_watch(
                            entry.clone(),
                            None,
                            WatchRunState::Backoff,
                            Some(message),
                            now,
                        );
                        continue;
                    }
                }
            } else {
                known.and_then(|runtime| runtime.query_body.clone())
            };

            self.scheduler.upsert(
                entry.id.clone(),
                settings.watcher.poll_interval_seconds,
                now,
                stagger,
                enabled_count,
            );
            if cap_changed {
                // 和"立刻查一次"按钮走同一条路:排在此刻,限速器一放行就发。
                self.scheduler.poll_now(&entry.id, now);
            }
            stagger += 1;
            self.upsert_watch(entry.clone(), query_body, WatchRunState::Polling, None, now);
        }

        // 设置里没有的搜索:从时间表和内存里拿掉,库里的历史留着。
        let gone: Vec<WatchId> = self
            .watches
            .keys()
            .filter(|id| !keep.contains(*id))
            .cloned()
            .collect();
        for watch_id in gone {
            self.scheduler.remove(&watch_id);
            self.watches.remove(&watch_id);
        }

        self.apply_observations(&settings, enabled_observations, now);

        if league_changed && self.rates_cancel.is_some() {
            self.settings.league = settings.league.clone();
            self.restart_rates_thread();
        }
        self.settings = settings;
        self.sync_live_workers(now);
    }

    // ---- live --------------------------------------------------------

    /// 按当前设置把 live worker 摆平:该起的起、该停的停、开不了的说清楚为什么。
    ///
    /// 每次 `ApplySettings` 都整份重来(而不是算增量):搜索列表就那么几条,
    /// 算增量省下的那点活换来的是"某条搜索的开关和实际状态对不上"这种 bug。
    fn sync_live_workers(&mut self, now: i64) {
        let session = usable_session(&self.settings, self.session_ok);
        let cap = live_connection_cap(&self.settings);
        let user_agent = self.settings.user_agent();

        // 先把"谁该跑、谁跑不了"算清楚,再动 self —— 设置列表借着 self,
        // 一边遍历一边改是借不出来的。
        let mut start: Vec<(LiveTarget, LiveConfig)> = Vec::new();
        let mut planned: BTreeMap<LiveTarget, LiveRunState> = BTreeMap::new();
        for entry in &self.settings.watches {
            let target = LiveTarget::Watch(entry.id.clone());
            if !entry.enabled || !entry.live {
                planned.insert(target, LiveRunState::Off);
                continue;
            }
            let Some(session) = session.as_deref() else {
                planned.insert(
                    target,
                    LiveRunState::Disabled(if self.session_ok {
                        LiveOffReason::NoSession
                    } else {
                        LiveOffReason::SessionInvalid
                    }),
                );
                continue;
            };
            if start.len() >= cap {
                planned.insert(target, LiveRunState::Disabled(LiveOffReason::TooMany));
                continue;
            }
            let search = SearchRef {
                league: entry.league.clone(),
                search_id: entry.search_id.clone(),
            };
            start.push((
                target.clone(),
                LiveConfig::new(&search, session, user_agent.clone()),
            ));
            planned.insert(target, LiveRunState::Connecting);
        }

        // 观察排在蹲价**后面**:两边分的是同一份连接额度,而抢不到的代价
        // 不一样 —— 蹲价没有秒推就是"好价晚三分钟才看见",观察没有秒推
        // 只是回到十分钟一次的兜底轮询(照样在跑,只是会漏掉秒卖掉的那些)。
        for entry in &self.settings.observations {
            let target = LiveTarget::Observation(entry.id.clone());
            if !entry.enabled {
                planned.insert(target, LiveRunState::Off);
                continue;
            }
            let Some(session) = session.as_deref() else {
                planned.insert(
                    target,
                    LiveRunState::Disabled(if self.session_ok {
                        LiveOffReason::NoSession
                    } else {
                        LiveOffReason::SessionInvalid
                    }),
                );
                continue;
            };
            if start.len() >= cap {
                planned.insert(target, LiveRunState::Disabled(LiveOffReason::TooMany));
                continue;
            }
            let search = SearchRef {
                league: entry.league.clone(),
                search_id: entry.search_id.clone(),
            };
            start.push((
                target.clone(),
                LiveConfig::new(&search, session, user_agent.clone()),
            ));
            planned.insert(target, LiveRunState::Connecting);
        }

        // 不该再跑的先停掉(包括设置里已经没有的那些)。
        let stop: Vec<LiveTarget> = self
            .live_workers
            .keys()
            .filter(|id| !start.iter().any(|(started, _)| started == *id))
            .cloned()
            .collect();
        for target in stop {
            self.stop_live_worker(&target);
        }

        for (target, config) in start {
            if self.live_workers.contains_key(&target) {
                // 已经在跑:它自己报上来的状态才是准的,别用一个"Connecting"
                // 把"已连上"盖掉。(搜索 id 或联赛改了的话,`apply_settings`
                // 已经在那条 `query_stale` 分支里把它停掉了,这里就轮不到。)
                planned.remove(&target);
                continue;
            }
            let handle = spawn_live_worker(
                LiveWorkerConfig::new(target.clone(), config),
                Arc::clone(&self.connector),
                self.live_events.clone(),
            );
            self.live_workers.insert(target, handle);
        }

        // 没有会话时说一句就够了,别每次 ApplySettings 都念一遍。
        let wants_live = self
            .settings
            .watches
            .iter()
            .any(|entry| entry.enabled && entry.live)
            || self.settings.observations.iter().any(|entry| entry.enabled);
        // `session_ok == false` 说的是"会话被拒了" —— 那句话
        // `on_session_invalid` 已经说过了,别再补一句"你没有会话"。
        if wants_live && session.is_none() && self.session_ok && !self.no_session_reported {
            self.no_session_reported = true;
            self.emit(RuntimeEvent::Log(
                "live search is off: no POESESSID in settings — polling anonymously".to_string(),
            ));
        }

        for (target, state) in planned {
            self.set_live_state(&target, state, now);
        }
    }

    fn stop_live_worker(&mut self, target: &LiveTarget) {
        if let Some(worker) = self.live_workers.remove(target) {
            // 只打招呼不等:它可能正卡在一次读超时里,而主循环一秒都不该停。
            worker.stop();
        }
    }

    /// 全部停掉,并把每条搜索和每条观察标上同一个原因(会话失效时用)。
    fn stop_all_live_workers(&mut self, reason: LiveOffReason, now: i64) {
        let running: Vec<LiveTarget> = self.live_workers.keys().cloned().collect();
        for target in running {
            self.stop_live_worker(&target);
        }
        let mut targets: Vec<LiveTarget> = self
            .settings
            .watches
            .iter()
            .filter(|entry| entry.live)
            .map(|entry| LiveTarget::Watch(entry.id.clone()))
            .collect();
        targets.extend(
            self.settings
                .observations
                .iter()
                .map(|entry| LiveTarget::Observation(entry.id.clone())),
        );
        for target in targets {
            self.set_live_state(&target, LiveRunState::Disabled(reason), now);
        }
    }

    fn handle_live_event(&mut self, event: LiveEvent) {
        let now = now_secs();
        match event {
            LiveEvent::State { target, state } => self.set_live_state(&target, state, now),
            LiveEvent::New { target, ids } => match target {
                LiveTarget::Watch(watch_id) => self.on_live_push(&watch_id, ids, now),
                LiveTarget::Observation(obs_id) => self.on_observation_push(&obs_id, ids),
            },
            LiveEvent::SessionInvalid { .. } => self.on_session_invalid(now),
            LiveEvent::Log(message) => self.emit(RuntimeEvent::Log(message)),
        }
    }

    fn set_live_state(&mut self, target: &LiveTarget, state: LiveRunState, now: i64) {
        match target {
            LiveTarget::Watch(watch_id) => self.set_watch_live_state(watch_id, state, now),
            LiveTarget::Observation(obs_id) => self.set_observation_live_state(obs_id, state),
        }
    }

    /// 换一条搜索的 live 档位:落库、调轮询节奏、广播。
    fn set_watch_live_state(&mut self, watch_id: &WatchId, state: LiveRunState, now: i64) {
        let Some(runtime) = self.watches.get_mut(watch_id) else {
            return;
        };
        if runtime.status.live == state {
            return;
        }
        runtime.status.live = state;
        // 界面上那个档位:WS 连着就显示 Live,断了退回 Polling。
        // 轮询自己的结论(退避、被拦)优先,那是更要紧的坏消息。
        runtime.status.state = match (runtime.status.state, state.is_connected()) {
            (WatchRunState::Polling, true) => WatchRunState::Live,
            (WatchRunState::Live, false) => WatchRunState::Polling,
            (other, _) => other,
        };

        // 有秒推兜着,轮询就放宽到 15 分钟档;断了立刻收回 5 分钟档。
        // 只影响下一次重排,已经排好的这一轮不动。
        self.scheduler
            .set_live_healthy(watch_id, state.is_connected());
        self.note(
            self.store.set_live_state(watch_id, state.stored(), now),
            "set_live_state",
        );
        if let LiveRunState::Held { until } = state {
            self.emit(RuntimeEvent::CloudflareBlocked { until });
        }
        self.emit_status(watch_id);
    }

    /// 换一条观察的 live 档位。
    ///
    /// 比蹲价那一版少两样:不落库(观察的 live 状态没有存的价值,它不像
    /// 蹲价那样要在重启之后告诉你"上次秒推是什么时候"),也不调兜底轮询的
    /// 节奏 —— 那一轮本来就是补"断线那几十秒"的漏的,连上了反而更不该放宽。
    fn set_observation_live_state(&mut self, obs_id: &ObservationId, state: LiveRunState) {
        let Some(runtime) = self.observations.get_mut(obs_id) else {
            return;
        };
        if runtime.status.live == state {
            return;
        }
        runtime.status.live = state;
        let label = runtime.entry.label.clone();
        // 被挤下来的那条要说清楚"是额度满了",不是"坏了"。只在状态**变成**
        // TooMany 的那一次说 —— 每次保存设置都念一遍是骚扰。
        if state == LiveRunState::Disabled(LiveOffReason::TooMany) {
            self.emit(RuntimeEvent::Log(format!(
                "observation {label}: no live connection left (max_live_connections) — \
                 staying on the backstop discover poll"
            )));
        }
        self.emit_observation_status(obs_id);
    }

    /// 秒推来了一批新挂单:按 10 个一批交给网关,优先级排在轮询前面。
    fn on_live_push(&mut self, watch_id: &WatchId, ids: Vec<String>, now: i64) {
        let Some(runtime) = self.watches.get(watch_id) else {
            return;
        };
        if !runtime.entry.enabled {
            return;
        }
        // fetch 要一个 `?query=` 参数。上一轮 search 回的那个最准;还没轮询过
        // 就用用户粘进来的那个(两者通常一样)。
        let search_id = if runtime.search_id.is_empty() {
            runtime.entry.search_id.clone()
        } else {
            runtime.search_id.clone()
        };

        for batch in ids.chunks(self.fetch_batch()) {
            self.gateway.submit(GatewayRequest {
                kind: RequestKind::Fetch {
                    ids: batch.to_vec(),
                    search_id: search_id.clone(),
                },
                priority: Priority::LiveFetch,
                reply: self.replies.clone(),
                tag: RequestTag::watch(watch_id.clone(), LIVE_FETCH_LABEL),
            });
        }
        // 顺手把"最后一次收到推送"的时刻记上:界面问"WS 还活着吗"看的是它。
        self.note(
            self.store
                .set_live_state(watch_id, LiveState::Connected, now),
            "set_live_state",
        );
    }

    /// 秒推告诉一条观察:这些挂单刚上架。
    ///
    /// 这是观察知道新挂单的**主要**途径,不是补充:搜索只回还活着的挂单,
    /// 所以一件挂上去一分钟就被买走的好价碑牌,十分钟一次的轮询永远看不见 ——
    /// 而那恰恰是我们最想量的那一件。秒推是在它**出生**的那一刻就知道它。
    ///
    /// `sample_every` 在这里生效:每 N 条抓一条,抽剩下的直接丢掉(不排队
    /// 等以后)。出生那一刻等距抽样是无偏的,而排队补抓会系统性地漏掉
    /// 卖得最快的那些 —— 等轮到它,它早没了。
    fn on_observation_push(&mut self, obs_id: &ObservationId, ids: Vec<String>) {
        let (wanted, search_id) = {
            let Some(runtime) = self.observations.get_mut(obs_id) else {
                return;
            };
            if !runtime.entry.enabled {
                return;
            }
            let every = u64::from(runtime.entry.sample_every.max(1));
            let mut wanted: Vec<String> = Vec::new();
            for id in ids {
                runtime.pushed_seen += 1;
                runtime.status.pushed_total += 1;
                if runtime.pushed_seen % every == 0 {
                    wanted.push(id);
                } else {
                    runtime.status.sampled_out_total += 1;
                }
            }
            let search_id = if runtime.search_id.is_empty() {
                runtime.entry.search_id.clone()
            } else {
                runtime.search_id.clone()
            };
            (wanted, search_id)
        };

        for chunk in wanted.chunks(self.fetch_batch()) {
            self.gateway.submit(GatewayRequest {
                // 按 id 对号的那一种:抓回来的那一格是 `null` 就说明这条挂单
                // 在我们看它第一眼之前就没了 —— 那是要单独记一笔的事。
                kind: RequestKind::FetchByIds {
                    ids: chunk.to_vec(),
                    search_id: search_id.clone(),
                },
                // 观察永远排在队列最后:攒数据的活晚十秒什么也不影响,
                // 而蹲价晚十秒可能就错过一件好货。
                priority: Priority::Background,
                reply: self.replies.clone(),
                tag: RequestTag::observation(obs_id.clone(), OBSERVE_LIVE_FETCH_LABEL),
            });
        }
        self.emit_observation_status(obs_id);
    }

    /// 秒推抓回来的详情。和轮询走同一套判定去重,只是来源标成 `Live`,
    /// 而且不碰轮询的时间表和失败计数 —— 那是另一条线上的事。
    fn on_live_fetch(&mut self, watch_id: &WatchId, listings: Vec<ListingSummary>, now: i64) {
        self.judge_batch(watch_id, listings, AlertSource::Live, true, now);
    }

    /// 会话被拒了:丢掉 cookie、停掉全部 live,只喊一次。
    fn on_session_invalid(&mut self, now: i64) {
        // 不清 `settings.poesessid`:界面上那串还是用户填的那串,
        // 清掉的话下一次 ApplySettings 会把它当成"用户换了新的"再试一遍。
        self.session_ok = false;
        self.gateway.set_session(None);
        self.stop_all_live_workers(LiveOffReason::SessionInvalid, now);
        if self.session_invalid_reported {
            return;
        }
        self.session_invalid_reported = true;
        self.emit(RuntimeEvent::Log(
            "the trade site did not recognise the POESESSID — it has been dropped".to_string(),
        ));
        self.emit(RuntimeEvent::SessionInvalid);
    }

    /// 把一条搜索的运行状态写进内存表并广播。已经在跑的那条只更新
    /// 用户改得动的部分(标签、上限、开关),不碰 `in_flight` 之类。
    fn upsert_watch(
        &mut self,
        entry: WatchEntry,
        query_body: Option<String>,
        state: WatchRunState,
        last_error: Option<String>,
        now: i64,
    ) {
        let watch_id = entry.id.clone();
        let hits_today = self
            .note(
                self.store.hits_since(&watch_id, start_of_today(now)),
                "hits_since",
            )
            .unwrap_or(0);
        let stored = self
            .note(self.store.watch_state(&watch_id), "watch_state")
            .flatten();
        let (next_poll_at, poll_every_secs) = self.schedule_of(&watch_id);

        match self.watches.get_mut(&watch_id) {
            Some(runtime) => {
                runtime.entry = entry;
                if query_body.is_some() {
                    runtime.query_body = query_body;
                }
                // 一条正常在跑的搜索,WS 连着的时候显示 Live 而不是 Polling。
                runtime.status.state = if state == WatchRunState::Polling {
                    healthy_state(runtime.status.live)
                } else {
                    state
                };
                runtime.status.next_poll_at = next_poll_at;
                runtime.status.poll_every_secs = poll_every_secs;
                runtime.status.hits_today = hits_today;
                if last_error.is_some() {
                    runtime.status.last_error = last_error;
                }
            }
            None => {
                let status = WatchStatus {
                    state,
                    // 刚认识这条搜索,live 还没起来。真正的档位等
                    // `sync_live_workers` 或者 worker 自己报上来。
                    live: LiveRunState::Off,
                    next_poll_at,
                    poll_every_secs,
                    last_poll_at: stored.as_ref().and_then(|state| state.last_poll_at),
                    last_total: stored.as_ref().and_then(|state| state.last_total),
                    hits_today,
                    failures: stored.as_ref().map_or(0, |state| state.failures),
                    last_error,
                };
                self.watches.insert(
                    watch_id.clone(),
                    WatchRuntime {
                        entry,
                        query_body,
                        search_id: String::new(),
                        first_poll_done: false,
                        in_flight: false,
                        status,
                    },
                );
            }
        }
        self.emit_status(&watch_id);
    }

    /// 把这条搜索的"跑出来的状态"写回库。已有的行只补 query_json 和联赛,
    /// 别把上一次轮询时刻和失败计数洗掉 —— 重启之后界面还要显示它们。
    fn persist_watch(&self, entry: &WatchEntry, query_json: &str, now: i64) {
        let previous = self
            .note(self.store.watch_state(&entry.id), "watch_state")
            .flatten();
        let state = WatchState {
            watch_id: entry.id.clone(),
            league: entry.league.clone(),
            search_id: entry.search_id.clone(),
            query_json: Some(query_json.to_string()),
            last_poll_at: previous.as_ref().and_then(|state| state.last_poll_at),
            last_live_at: previous.as_ref().and_then(|state| state.last_live_at),
            live_state: previous
                .as_ref()
                .map_or(LiveState::Off, |state| state.live_state),
            last_total: previous.as_ref().and_then(|state| state.last_total),
            failures: previous.as_ref().map_or(0, |state| state.failures),
            updated_at: now,
        };
        self.note(self.store.upsert_watch_state(&state), "upsert_watch_state");
    }

    // ---- 轮询 --------------------------------------------------------

    /// 一轮的第一步:发 search。
    fn start_poll(&mut self, watch_id: &WatchId, now: i64) {
        let Some(runtime) = self.watches.get_mut(watch_id) else {
            return;
        };
        if runtime.in_flight || !runtime.entry.enabled {
            return;
        }
        let Some(body_json) = runtime.query_body.clone() else {
            return;
        };
        let league = runtime.entry.league.clone();
        runtime.in_flight = true;
        runtime.status.state = healthy_state(runtime.status.live);

        // 请求一发出去就把下一轮排上:否则 `next_at` 停在过去,主循环会
        // 一直觉得"这条该跑了"而空转。
        self.scheduler.defer(watch_id, now);
        let (next_at, every) = self.schedule_of(watch_id);
        if let Some(runtime) = self.watches.get_mut(watch_id) {
            runtime.status.next_poll_at = next_at;
            runtime.status.poll_every_secs = every;
        }

        self.gateway.submit(GatewayRequest {
            kind: RequestKind::Search { league, body_json },
            priority: Priority::PollSearch,
            reply: self.replies.clone(),
            tag: RequestTag::watch(watch_id.clone(), SEARCH_LABEL),
        });
        self.emit_status(watch_id);
    }

    fn handle_reply(&mut self, reply: GatewayReply) {
        let now = now_secs();
        let GatewayReply { tag, kind } = reply;
        // 会话检查既不属于哪条搜索,也不属于哪条提醒:它自己认自己。
        let kind = match kind {
            ReplyKind::SessionCheck(result) => {
                self.on_session_checked(&result);
                return;
            }
            other => other,
        };
        // "去藏身处"那两步不属于任何一轮轮询,先认它。
        if let Some(alert_id) = tag.alert_id {
            self.on_hideout_reply(alert_id, kind, now);
            return;
        }
        // 一批回查可能横跨好几条观察,所以它认的是批次号,不是某一条观察。
        if let Some(sweep_id) = tag.sweep_id {
            match kind {
                ReplyKind::FetchByIds(Ok(pairs)) => self.on_sweep_fetch(sweep_id, pairs, now),
                ReplyKind::FetchByIds(Err(error)) => self.on_sweep_failed(sweep_id, &error),
                // 扫描只发 FetchByIds,别的回信不会挂着批次号。
                _ => {}
            }
            return;
        }
        // 市场观察也走自己那条路:它和蹲价共用网关,但两边的时间表、
        // 状态、库表都是分开的。
        if let Some(obs_id) = tag.obs_id.clone() {
            self.on_observe_reply(&obs_id, tag.label, kind, now);
            return;
        }
        let Some(watch_id) = tag.watch_id.clone() else {
            return;
        };
        if !self.watches.contains_key(&watch_id) {
            // 这条搜索在请求飞在路上的时候被删了,回信直接丢掉。
            return;
        }
        let from_live = tag.label == LIVE_FETCH_LABEL;
        match kind {
            ReplyKind::Search(Ok(outcome)) => self.on_search(&watch_id, outcome, now),
            ReplyKind::Fetch(Ok(listings)) if from_live => {
                self.on_live_fetch(&watch_id, listings, now);
            }
            ReplyKind::Fetch(Ok(listings)) => self.on_fetch(&watch_id, listings, now),
            ReplyKind::Fetch(Err(error)) if from_live => {
                // 秒推那一路砸了不该连累轮询的退避计数:记一笔就完了,
                // 下一次推送(或者下一轮轮询)会把这批货重新看一遍。
                self.emit(RuntimeEvent::Log(format!("live fetch failed: {error}")));
            }
            ReplyKind::Search(Err(error)) | ReplyKind::Fetch(Err(error)) => {
                self.on_poll_failed(&watch_id, &error, now);
            }
            // 走到这里的 whisper 一定带着 alert_id,会话检查更是上面就
            // 认掉了;按 id 对号的那种 fetch 只有市场观察在用,也带着自己的
            // obs_id —— 三种都轮不到这里。
            ReplyKind::Whisper(_) | ReplyKind::SessionCheck(_) | ReplyKind::FetchByIds(_) => {}
        }
    }

    /// search 回来了:记一笔,然后去抓最便宜的前几件详情。
    fn on_search(&mut self, watch_id: &WatchId, outcome: SearchOutcome, now: i64) {
        self.note(
            self.store.touch_poll(watch_id, now, Some(outcome.total)),
            "touch_poll",
        );

        let batch = self.fetch_batch();
        let ids: Vec<String> = outcome.result.iter().take(batch).cloned().collect();
        if let Some(runtime) = self.watches.get_mut(watch_id) {
            runtime.search_id = outcome.id.clone();
            runtime.status.last_poll_at = Some(now);
            runtime.status.last_total = Some(outcome.total);
        }

        if ids.is_empty() {
            // 市面上一件都没有:这一轮到此为止,不用 fetch。
            self.finish_poll(watch_id, now);
            return;
        }

        self.gateway.submit(GatewayRequest {
            kind: RequestKind::Fetch {
                ids,
                search_id: outcome.id,
            },
            priority: Priority::PollFetch,
            reply: self.replies.clone(),
            tag: RequestTag::watch(watch_id.clone(), FETCH_LABEL),
        });
    }

    /// 轮询的 fetch 回来了:判定这一批,然后收尾这一轮。
    fn on_fetch(&mut self, watch_id: &WatchId, listings: Vec<ListingSummary>, now: i64) {
        // 刚加进来的搜索第一轮看到的都是存量货色。用户如果不想被存量吵醒,
        // 这一轮只记账不叫人。
        let Some(runtime) = self.watches.get(watch_id) else {
            return;
        };
        let alerting = runtime.first_poll_done || runtime.entry.alert_on_first_poll;

        self.judge_batch(watch_id, listings, AlertSource::Poll, alerting, now);

        if let Some(runtime) = self.watches.get_mut(watch_id) {
            runtime.first_poll_done = true;
        }
        self.finish_poll(watch_id, now);
    }

    /// 一批挂单:逐条去重 + 判定,命中的合成一张卡片。
    ///
    /// 轮询和秒推共用这一段 —— 两条路发现的是同样的挂单,凭什么判得不一样;
    /// 唯一的区别是记在提醒历史里的 `source`。
    fn judge_batch(
        &mut self,
        watch_id: &WatchId,
        listings: Vec<ListingSummary>,
        source: AlertSource,
        alerting: bool,
        now: i64,
    ) {
        let Some(runtime) = self.watches.get(watch_id) else {
            return;
        };
        let cap: PriceCap = runtime.entry.price_cap.clone();
        let label = runtime.entry.label.clone();
        let league = runtime.entry.league.clone();
        let search_id = runtime.entry.search_id.clone();

        let mut hits: Vec<(i64, ListingSummary)> = Vec::new();
        for listing in listings {
            let verdict = judge(&cap, listing.price.as_ref(), &self.rates);
            let Some(seen) = self.note(
                self.store.record_listing(watch_id, &listing, verdict, now),
                "record_listing",
            ) else {
                continue;
            };
            if !matches!(
                decide(&cap, &listing, seen, &self.rates),
                Decision::Alert(_)
            ) {
                continue;
            }
            if !alerting {
                continue;
            }
            let alert = NewAlert {
                watch_id,
                league: &league,
                search_id: &search_id,
                listing: &listing,
                source,
            };
            if let Some(alert_id) = self.note(self.store.insert_alert(&alert, now), "insert_alert")
            {
                hits.push((alert_id, listing));
            }
        }

        if !hits.is_empty() {
            let listing_ids: Vec<String> =
                hits.iter().map(|(_, listing)| listing.id.clone()).collect();
            let mut alert_ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
            if let Some((headline, extra)) = coalesce(hits) {
                // 卡片上写的是标题那条,而卡片按钮只带**一个**行号(第一个)。
                // 把标题那条换到第一位:不然点"去藏身处"会传送到另一件货的
                // 卖家那儿 —— 一次点击对应哪件东西,不该靠运气。
                if let Some(index) = listing_ids.iter().position(|id| *id == headline.id) {
                    alert_ids.swap(0, index);
                }
                self.emit(RuntimeEvent::ListingMatched(Box::new(MatchedListing {
                    alert_ids,
                    watch_id: watch_id.clone(),
                    label,
                    league,
                    search_id,
                    headline,
                    extra,
                    cap,
                    source,
                })));
            }
        }
    }

    /// 一轮顺利跑完:失败计数清零,排下一轮。
    fn finish_poll(&mut self, watch_id: &WatchId, now: i64) {
        self.note(self.store.clear_failures(watch_id, now), "clear_failures");
        self.scheduler
            .reschedule(watch_id, now, PollOutcome::Ok, &self.settings.watcher);
        let (next_at, every) = self.schedule_of(watch_id);
        let hits_today = self
            .note(
                self.store.hits_since(watch_id, start_of_today(now)),
                "hits_since",
            )
            .unwrap_or(0);
        if let Some(runtime) = self.watches.get_mut(watch_id) {
            runtime.in_flight = false;
            runtime.status.state = healthy_state(runtime.status.live);
            runtime.status.failures = 0;
            runtime.status.last_error = None;
            runtime.status.next_poll_at = next_at;
            runtime.status.poll_every_secs = every;
            runtime.status.hits_today = hits_today;
        }
        self.emit_status(watch_id);
    }

    /// 这一轮砸了:失败计数 +1,退避,把原因带给界面。
    fn on_poll_failed(&mut self, watch_id: &WatchId, error: &GatewayError, now: i64) {
        let failures = self
            .note(self.store.bump_failures(watch_id, now), "bump_failures")
            .unwrap_or(0);
        self.scheduler
            .reschedule(watch_id, now, PollOutcome::Failed, &self.settings.watcher);
        let (next_at, every) = self.schedule_of(watch_id);
        let state = if matches!(error, GatewayError::CloudflareHold) {
            WatchRunState::Held
        } else {
            WatchRunState::Backoff
        };
        if let Some(runtime) = self.watches.get_mut(watch_id) {
            runtime.in_flight = false;
            runtime.status.state = state;
            runtime.status.failures = failures;
            runtime.status.last_error = Some(error.to_string());
            runtime.status.next_poll_at = next_at;
            runtime.status.poll_every_secs = every;
        }
        self.emit_status(watch_id);
    }

    /// 时间表上这条搜索的(下一次, 多久一次)。
    ///
    /// 两个数字总是一起读:界面上"下一轮 14 分钟后"回答不了"到底多久一次",
    /// 而分两处去问时间表,迟早有一处忘了更新。不在表上(停用了、搜索 id
    /// 坏了)就是 `(None, 0)`。
    fn schedule_of(&self, watch_id: &WatchId) -> (Option<i64>, u64) {
        match self.scheduler.entry(watch_id) {
            Some(entry) => (Some(entry.next_at), entry.interval),
            None => (None, 0),
        }
    }

    /// 一次 fetch 带几个 id。设置里能调,但服务端上限就是 10。
    fn fetch_batch(&self) -> usize {
        (self.settings.watcher.fetch_batch as usize).clamp(1, MAX_FETCH_IDS)
    }

    // ---- 市场观察 ----------------------------------------------------

    /// 按新设置把观察列表摆平。逐条 diff,规矩和上面那段搜索的一样:
    /// 停用的下班、粘错 id 的留在列表里带着原因、设置里没有的从时间表上拿掉。
    fn apply_observations(&mut self, settings: &AppSettings, enabled: usize, now: i64) {
        let mut keep: BTreeSet<ObservationId> = BTreeSet::new();
        let mut stagger = 0usize;

        for entry in &settings.observations {
            keep.insert(entry.id.clone());
            if !entry.enabled {
                self.observe_scheduler.remove(&entry.id);
                self.upsert_observation(entry.clone(), None, None);
                continue;
            }

            let known = self.observations.get(&entry.id);
            let query_stale = match known {
                None => true,
                Some(runtime) => {
                    runtime.query_body.is_none()
                        || runtime.entry.search_id != entry.search_id
                        || runtime.entry.league != entry.league
                }
            };
            let query_body = if query_stale {
                // 搜索 id 或联赛变了 = 这是另一条搜索。已经连着的那条 live
                // 盯的是旧的,停掉它,后面的 `sync_live_workers` 会照新的重开。
                self.stop_live_worker(&LiveTarget::Observation(entry.id.clone()));
                match decode_search_id(&entry.search_id) {
                    Ok(query_json) => {
                        self.persist_observation(entry, &query_json, now);
                        // 兜底那一轮要的是"最新挂上来的 100 条",所以把排序换成
                        // 上架时间倒序 —— 蹲价那个"最便宜的 100 条"永远看不到贵货,
                        // 而观察要的正是完整的一批。
                        Some(with_sort(&query_json, "indexed", "desc"))
                    }
                    Err(error) => {
                        self.observe_scheduler.remove(&entry.id);
                        let message = format!("{}: {error}", entry.label);
                        self.emit(RuntimeEvent::Log(message.clone()));
                        self.upsert_observation(entry.clone(), None, Some(message));
                        continue;
                    }
                }
            } else {
                known.and_then(|runtime| runtime.query_body.clone())
            };

            self.observe_scheduler.upsert(
                entry.id.clone(),
                entry.discover_interval_secs,
                now,
                stagger,
                enabled,
            );
            stagger += 1;
            self.upsert_observation(entry.clone(), query_body, None);
        }

        // 设置里没有的观察:停掉排班、从内存里拿掉,**库里的行留着**。
        // 和删搜索一个道理 —— 真要把攒下来的数据清干净,是界面上那个"删除"
        // 按钮调 `WatchStore::delete_observation` 的事,不该由一次设置保存代劳。
        let gone: Vec<ObservationId> = self
            .observations
            .keys()
            .filter(|id| !keep.contains(*id))
            .cloned()
            .collect();
        for obs_id in gone {
            self.observe_scheduler.remove(&obs_id);
            self.observations.remove(&obs_id);
        }
    }

    /// 把一条观察写进内存表并广播。已经在跑的那条只更新用户改得动的部分。
    fn upsert_observation(
        &mut self,
        entry: ObservationEntry,
        query_body: Option<String>,
        last_error: Option<String>,
    ) {
        let obs_id = entry.id.clone();
        let summary = self
            .note(
                self.store.observation_summary(&obs_id),
                "observation_summary",
            )
            .unwrap_or_default();
        let stored = self
            .note(self.store.observation_state(&obs_id), "observation_state")
            .flatten();
        let (next_discover_at, next_recheck_at) = self.observe_schedule_of(&obs_id);

        match self.observations.get_mut(&obs_id) {
            Some(runtime) => {
                runtime.entry = entry;
                if query_body.is_some() {
                    runtime.query_body = query_body;
                }
                runtime.status.active = summary.active;
                runtime.status.gone = summary.gone;
                runtime.status.next_discover_at = next_discover_at;
                runtime.status.next_recheck_at = next_recheck_at;
                if last_error.is_some() {
                    runtime.status.last_error = last_error;
                }
            }
            None => {
                let status = ObservationStatus {
                    active: summary.active,
                    gone: summary.gone,
                    // 刚认识这条观察,live 还没起来。真正的档位等
                    // `sync_live_workers` 或者 worker 自己报上来。
                    live: LiveRunState::Off,
                    pushed_total: 0,
                    sampled_out_total: 0,
                    last_discover_at: stored.as_ref().and_then(|run| run.last_discover_at),
                    last_recheck_at: stored.as_ref().and_then(|run| run.last_recheck_at),
                    next_discover_at,
                    next_recheck_at,
                    last_error,
                };
                self.observations.insert(
                    obs_id.clone(),
                    ObservationRuntime {
                        entry,
                        query_body,
                        search_id: String::new(),
                        discover_in_flight: false,
                        discover_pending: 0,
                        pushed_seen: 0,
                        status,
                    },
                );
            }
        }
        self.emit_observation_status(&obs_id);
    }

    /// 把这条观察的"跑出来的状态"写回库。已有的行只补查询和联赛,
    /// 别把两个时间戳洗掉 —— 重启之后界面还要显示它们。
    fn persist_observation(&self, entry: &ObservationEntry, query_json: &str, now: i64) {
        let previous = self
            .note(self.store.observation_state(&entry.id), "observation_state")
            .flatten();
        let run = ObservationRun {
            obs_id: entry.id.clone(),
            league: entry.league.clone(),
            search_id: entry.search_id.clone(),
            query_json: Some(query_json.to_string()),
            last_discover_at: previous.as_ref().and_then(|run| run.last_discover_at),
            last_recheck_at: previous.as_ref().and_then(|run| run.last_recheck_at),
            updated_at: now,
        };
        self.note(
            self.store.upsert_observation_state(&run),
            "upsert_observation_state",
        );
    }

    /// 兜底那一轮的第一步:一次 search,按上架时间倒序拿最新的一批 id。
    ///
    /// 秒推连着的时候这一轮基本上什么都抓不到(该知道的早知道了),那正是
    /// 它该有的样子 —— 它补的是 WebSocket 断线重连那几十秒里上架的挂单。
    fn start_discover(&mut self, obs_id: &ObservationId, now: i64) {
        let Some(runtime) = self.observations.get_mut(obs_id) else {
            return;
        };
        if !runtime.entry.enabled || runtime.discover_in_flight || runtime.discover_pending > 0 {
            return;
        }
        let Some(body_json) = runtime.query_body.clone() else {
            return;
        };
        let league = runtime.entry.league.clone();
        runtime.discover_in_flight = true;
        // 上一轮的错误到此为止:这一轮的结论由这一轮说了算。
        runtime.status.last_error = None;

        // 请求一发出去就把下一轮排上,理由同轮询:否则 `next_at` 停在过去,
        // 主循环会一直觉得"该跑了"而空转。
        self.observe_scheduler.defer(obs_id, now);
        self.refresh_observe_schedule(obs_id);

        self.gateway.submit(GatewayRequest {
            kind: RequestKind::Search { league, body_json },
            // 观察永远排在队列最后:它是攒数据的活,晚十秒钟什么也不影响,
            // 而蹲价晚十秒可能就错过一件好货。
            priority: Priority::Background,
            reply: self.replies.clone(),
            tag: RequestTag::observation(obs_id.clone(), OBSERVE_SEARCH_LABEL),
        });
        self.emit_observation_status(obs_id);
    }

    /// discover 的 search 回来了:在册的那些只推 `last_seen`,没见过的才去抓详情。
    fn on_discover_search(&mut self, obs_id: &ObservationId, outcome: SearchOutcome, now: i64) {
        let (label, interval) = {
            let Some(runtime) = self.observations.get_mut(obs_id) else {
                return;
            };
            runtime.discover_in_flight = false;
            runtime.search_id = outcome.id.clone();
            (
                runtime.entry.label.clone(),
                runtime.entry.discover_interval_secs,
            )
        };

        // 搜索只给 id、不给价格 —— 所以在册的那些**不花一次 fetch**:
        // 它出现在"最新 100 条"里就是"还挂着",推一下 last_seen 就够了,
        // 改没改价等它自己的回查档到点时再说。
        //
        // 秒推刚记下的那些也在这一批里(record_seen 已经把它们记成在册),
        // 于是"秒推抓过一次、十分钟后兜底轮询又看见它"不会再花第二次 fetch。
        let known: BTreeSet<String> = self
            .note(self.store.active_listing_ids(obs_id), "active_listing_ids")
            .unwrap_or_default()
            .into_iter()
            .collect();
        let (mut fresh, still_listed): (Vec<String>, Vec<String>) = outcome
            .result
            .into_iter()
            .partition(|id| !known.contains(id));
        self.note(
            self.store.touch_listings(obs_id, &still_listed, now),
            "touch_listings",
        );

        // 搜索是按上架时间倒序回来的,所以砍掉尾巴留下的是最新的那些。
        let cap = self.observe_fetch_cap(interval);
        if fresh.len() > cap {
            self.emit(RuntimeEvent::Log(format!(
                "observation {label}: {} new listings but this discover can only afford {cap} — \
                 taking the newest, the rest will be picked up next round",
                fresh.len()
            )));
            fresh.truncate(cap);
        }
        let search_id = outcome.id;
        self.submit_discover_fetches(obs_id, &fresh, &search_id, now);
    }

    /// 把兜底那一轮找到的新面孔按 10 个一批交给网关。
    /// 一条都没有就当这一轮当场跑完了(秒推正常工作时这是常态)。
    fn submit_discover_fetches(
        &mut self,
        obs_id: &ObservationId,
        ids: &[String],
        search_id: &str,
        now: i64,
    ) {
        if ids.is_empty() {
            self.finish_discover(obs_id, now);
            return;
        }
        let batch = self.fetch_batch();
        if let Some(runtime) = self.observations.get_mut(obs_id) {
            runtime.discover_pending = ids.chunks(batch).count();
        }
        for chunk in ids.chunks(batch) {
            self.gateway.submit(GatewayRequest {
                // 按 id 对号的那一种:普通 fetch 的回信早把 `null` 那几格丢掉了,
                // 而"我问的这一条已经没了"正是要记一笔的事。
                kind: RequestKind::FetchByIds {
                    ids: chunk.to_vec(),
                    search_id: search_id.to_string(),
                },
                priority: Priority::Background,
                reply: self.replies.clone(),
                tag: RequestTag::observation(obs_id.clone(), OBSERVE_DISCOVER_FETCH_LABEL),
            });
        }
        self.emit_observation_status(obs_id);
    }

    /// **第一次**去抓一批 id 的详情回来了(秒推来的,或者兜底轮询捡到的)。
    ///
    /// 抓到了就整条记下来;那一格是 `null` 说明它在我们看第一眼之前就没了 ——
    /// 记一行空壳,判定档写死 `gone_before_first_look`。
    fn on_first_look_fetch(
        &mut self,
        obs_id: &ObservationId,
        pairs: Vec<(String, Option<ListingSummary>)>,
        now: i64,
    ) {
        for (listing_id, listing) in pairs {
            match listing {
                Some(listing) => {
                    self.note(self.store.record_seen(obs_id, &listing, now), "record_seen");
                }
                None => {
                    self.note(
                        self.store
                            .record_gone_before_first_look(obs_id, &listing_id, now),
                        "record_gone_before_first_look",
                    );
                }
            }
        }
    }

    /// 到点该回头看的挂单:捞出来,凑成 10 个一批发出去。
    ///
    /// **跨观察一起凑批**:一次 fetch 最多带 10 个 id,两条观察各有三条到期的
    /// 挂单,拼成一批就只花一次额度。回信按 id 对号(`parse_fetch_response_by_id`),
    /// 而"这条 id 是替哪条观察问的"记在 `self.sweeps` 里。
    fn sweep_due_listings(&mut self, now: i64) {
        self.observe_scheduler.defer_sweep(now);
        if self.observations.is_empty() {
            return;
        }
        // 已经发出去还没回来的那些不能再问一遍:它们的 `next_check_at` 要等
        // 回信才会往前挪,不挡一下就会每分钟重发一次同一批。
        let in_flight: BTreeSet<(ObservationId, String)> = self
            .sweeps
            .values()
            .flatten()
            .map(|due| (due.obs_id.clone(), due.listing_id.clone()))
            .collect();
        let cap = self.observe_fetch_cap(SWEEP_INTERVAL_SECS);
        let due: Vec<DueListing> = self
            .note(self.store.due_listings(now, cap), "due_listings")
            .unwrap_or_default()
            .into_iter()
            .filter(|due| {
                !in_flight.contains(&(due.obs_id.clone(), due.listing_id.clone()))
                    && self
                        .observations
                        .get(&due.obs_id)
                        .is_some_and(|runtime| runtime.entry.enabled)
            })
            .collect();
        if due.is_empty() {
            return;
        }

        // 同一条挂单可能同时属于两条观察(两条搜索的范围重叠),那也只该问
        // 一次:一个 id 在请求里出现两遍,白占一个格子还问不出新东西。
        // 先按 id 归拢,顺序保持"最早该查的在前"。
        let mut grouped: Vec<(String, Vec<DueListing>)> = Vec::new();
        for entry in due {
            match grouped.iter_mut().find(|(id, _)| *id == entry.listing_id) {
                Some((_, asked)) => asked.push(entry),
                None => grouped.push((entry.listing_id.clone(), vec![entry])),
            }
        }

        for chunk in grouped.chunks(self.fetch_batch()) {
            // `?query=` 只是这次 fetch 的上下文参数,认的是 id 本身;混批时
            // 拿第一条观察的那个就行 —— 拆成"一条观察一批"反而会发出好几个
            // 装不满的请求,那才是真正在浪费额度。
            let search_id = self
                .observations
                .get(&chunk[0].1[0].obs_id)
                .map(|runtime| {
                    if runtime.search_id.is_empty() {
                        runtime.entry.search_id.clone()
                    } else {
                        runtime.search_id.clone()
                    }
                })
                .unwrap_or_default();
            let sweep_id = self.next_sweep_id;
            self.next_sweep_id += 1;
            self.sweeps.insert(
                sweep_id,
                chunk.iter().flat_map(|(_, asked)| asked.clone()).collect(),
            );
            self.gateway.submit(GatewayRequest {
                kind: RequestKind::FetchByIds {
                    ids: chunk.iter().map(|(id, _)| id.clone()).collect(),
                    search_id,
                },
                priority: Priority::Background,
                reply: self.replies.clone(),
                tag: RequestTag::sweep(sweep_id, OBSERVE_RECHECK_LABEL),
            });
        }
    }

    /// 一批回查回来了:还在的升一档,没了的判一下。
    fn on_sweep_fetch(
        &mut self,
        sweep_id: u64,
        pairs: Vec<(String, Option<ListingSummary>)>,
        now: i64,
    ) {
        let Some(asked) = self.sweeps.remove(&sweep_id) else {
            // 关机时的迟到回信,或者同一批被处理过两次。
            return;
        };
        let mut touched: BTreeSet<ObservationId> = BTreeSet::new();
        for (listing_id, listing) in pairs {
            // 同一条挂单可能同时属于两条观察(两条搜索的范围重叠),
            // 所以这里是"所有问过它的观察",不是"第一条"。
            for due in asked.iter().filter(|due| due.listing_id == listing_id) {
                touched.insert(due.obs_id.clone());
                match &listing {
                    Some(listing) => {
                        self.note(
                            self.store.record_seen(&due.obs_id, listing, now),
                            "record_seen",
                        );
                        self.advance_check(due);
                    }
                    None => self.on_listing_gone(&due.obs_id, &listing_id, now),
                }
            }
        }
        for obs_id in touched {
            self.note(self.store.touch_recheck(&obs_id, now), "touch_recheck");
            self.observation_data_changed(&obs_id, Some(now));
        }
    }

    /// 这条挂单还在:升一档,把下一次排上。阶梯走完(七天)就再也不查了。
    fn advance_check(&self, due: &DueListing) {
        let rung = due.check_rung.saturating_add(1);
        let recheck = self
            .observations
            .get(&due.obs_id)
            .map_or(0, |runtime| runtime.entry.recheck_interval_secs);
        let next_check_at = next_check_after(due.first_seen_at, rung, recheck);
        self.note(
            self.store
                .advance_rung(&due.obs_id, &due.listing_id, rung, next_check_at),
            "advance_rung",
        );
    }

    /// 一批回查砸了。挂单的 `next_check_at` 没往前挪,所以下一次扫描会再问
    /// 一遍它们 —— 不用在这里补什么。
    fn on_sweep_failed(&mut self, sweep_id: u64, error: &GatewayError) {
        let Some(asked) = self.sweeps.remove(&sweep_id) else {
            return;
        };
        for obs_id in asked
            .iter()
            .map(|due| due.obs_id.clone())
            .collect::<BTreeSet<_>>()
        {
            if let Some(runtime) = self.observations.get_mut(&obs_id) {
                runtime.status.last_error = Some(error.to_string());
            }
            self.emit_observation_status(&obs_id);
        }
    }

    /// 回查查不到它了 —— 判一下是怎么没的,把结论写在行上。
    ///
    /// 判定本身是 `pnd_domain::classify_gone` 那个纯函数,这里只负责把它要的
    /// 三样东西(第一次见到、最后一次见到、价格轨迹)从库里捞出来。
    fn on_listing_gone(&mut self, obs_id: &ObservationId, listing_id: &str, now: i64) {
        let Some(row) = self
            .note(
                self.store.observed_listing(obs_id, listing_id),
                "observed_listing",
            )
            .flatten()
        else {
            return;
        };
        let history = self
            .note(
                self.store.price_history(obs_id, listing_id),
                "price_history",
            )
            .unwrap_or_default();
        // 无价的那些点跳过:它没法和一个数比大小,而"降过价没有"问的就是大小。
        let points: Vec<(i64, i64)> = history
            .iter()
            .filter_map(|point| {
                point
                    .price
                    .as_ref()
                    .map(|price| (point.at, price.amount_milli))
            })
            .collect();
        let class = classify_gone(row.first_seen_at, row.last_seen_at, now, &points);
        self.note(
            self.store.mark_gone(obs_id, listing_id, now, class),
            "mark_gone",
        );
    }

    /// 少了一批在途的 discover fetch;一批都不剩就是这一轮跑完了。
    fn finish_discover_batch(&mut self, obs_id: &ObservationId, now: i64) {
        let done = {
            let Some(runtime) = self.observations.get_mut(obs_id) else {
                return;
            };
            runtime.discover_pending = runtime.discover_pending.saturating_sub(1);
            runtime.discover_pending == 0
        };
        if done {
            self.finish_discover(obs_id, now);
        }
    }

    /// 兜底那一轮跑完了:落时间戳、重算这条观察的账、告诉界面去库里重读。
    ///
    /// 中间有一批 fetch 砸了也照样算跑完:那一批的挂单下一轮还会被问到,
    /// 而这一轮确实发生过 —— 时间戳照实记,出了什么事另有 `last_error` 说。
    fn finish_discover(&mut self, obs_id: &ObservationId, now: i64) {
        self.note(self.store.touch_discover(obs_id, now), "touch_discover");
        if let Some(runtime) = self.observations.get_mut(obs_id) {
            runtime.status.last_discover_at = Some(now);
        }
        self.observation_data_changed(obs_id, None);
    }

    /// 库里这条观察的数据动了:重算它的账,喊界面重读一遍。
    ///
    /// 事件里不带数据 —— 观察页要的那几张表动辄几百行,而且用户还在上面
    /// 筛选。界面收到这一句自己去库里读,读到的一定是最新的。
    fn observation_data_changed(&mut self, obs_id: &ObservationId, rechecked_at: Option<i64>) {
        let summary = self
            .note(
                self.store.observation_summary(obs_id),
                "observation_summary",
            )
            .unwrap_or_default();
        let (next_discover_at, next_recheck_at) = self.observe_schedule_of(obs_id);
        if let Some(runtime) = self.observations.get_mut(obs_id) {
            runtime.status.active = summary.active;
            runtime.status.gone = summary.gone;
            runtime.status.next_discover_at = next_discover_at;
            runtime.status.next_recheck_at = next_recheck_at;
            if let Some(at) = rechecked_at {
                runtime.status.last_recheck_at = Some(at);
            }
        }
        self.emit(RuntimeEvent::ObservationChanged {
            obs_id: obs_id.clone(),
        });
        self.emit_observation_status(obs_id);
    }

    /// discover 的那次 search 砸了。这一轮到此为止,下一轮照常来 —— 观察不退避
    /// (它本来就是十分钟一次的慢节奏,而 429 / Cloudflare 那两种真该停手的
    /// 情况,网关在自己那一层已经拦住了)。
    fn on_discover_failed(&mut self, obs_id: &ObservationId, error: &GatewayError) {
        if let Some(runtime) = self.observations.get_mut(obs_id) {
            runtime.discover_in_flight = false;
            runtime.status.last_error = Some(error.to_string());
        }
        self.emit_observation_status(obs_id);
    }

    fn on_observe_reply(&mut self, obs_id: &ObservationId, label: &str, kind: ReplyKind, now: i64) {
        if !self.observations.contains_key(obs_id) {
            // 这条观察在请求飞在路上的时候被删了,回信直接丢掉。
            return;
        }
        // 秒推抓回来的那一批不属于任何一轮 discover:它没有"这一轮跑完了"
        // 这回事,记完账当场喊一声界面就行。
        let from_live = label == OBSERVE_LIVE_FETCH_LABEL;
        match kind {
            ReplyKind::Search(Ok(outcome)) => self.on_discover_search(obs_id, outcome, now),
            ReplyKind::Search(Err(error)) => self.on_discover_failed(obs_id, &error),
            ReplyKind::FetchByIds(Ok(pairs)) => {
                self.on_first_look_fetch(obs_id, pairs, now);
                if from_live {
                    self.observation_data_changed(obs_id, None);
                } else {
                    self.finish_discover_batch(obs_id, now);
                }
            }
            ReplyKind::FetchByIds(Err(error)) => {
                if let Some(runtime) = self.observations.get_mut(obs_id) {
                    runtime.status.last_error = Some(error.to_string());
                }
                if from_live {
                    self.emit_observation_status(obs_id);
                } else {
                    self.finish_discover_batch(obs_id, now);
                }
            }
            // 观察这条路上只发 search 和 FetchByIds,别的回信不会挂着 obs_id。
            ReplyKind::Fetch(_) | ReplyKind::Whisper(_) | ReplyKind::SessionCheck(_) => {}
        }
    }

    /// 界面要的两个时刻:(下一次兜底 discover, 下一条到点该回查的挂单)。
    fn observe_schedule_of(&self, obs_id: &ObservationId) -> (Option<i64>, Option<i64>) {
        let next_discover_at = self
            .observe_scheduler
            .entry(obs_id)
            .map(|entry| entry.next_discover_at);
        let next_recheck_at = self
            .note(self.store.next_check_due_at(obs_id), "next_check_due_at")
            .flatten();
        (next_discover_at, next_recheck_at)
    }

    fn refresh_observe_schedule(&mut self, obs_id: &ObservationId) {
        let (next_discover_at, next_recheck_at) = self.observe_schedule_of(obs_id);
        if let Some(runtime) = self.observations.get_mut(obs_id) {
            runtime.status.next_discover_at = next_discover_at;
            runtime.status.next_recheck_at = next_recheck_at;
        }
    }

    fn enabled_observation_count(&self) -> usize {
        self.settings
            .observations
            .iter()
            .filter(|entry| entry.enabled)
            .count()
    }

    /// 这一轮最多拿多少条挂单去 fetch。抓取额度是所有观察分着花的,
    /// 算法在 [`fetch_listing_cap`]。
    fn observe_fetch_cap(&self, interval_secs: u64) -> usize {
        fetch_listing_cap(
            self.enabled_observation_count().max(1),
            interval_secs,
            self.budget.effective_limit(FETCH_LONG_WINDOW_REQUESTS),
            FETCH_LONG_WINDOW_SECS,
            self.fetch_batch(),
        )
    }

    fn emit_observation_status(&self, obs_id: &ObservationId) {
        if let Some(runtime) = self.observations.get(obs_id) {
            self.emit(RuntimeEvent::ObservationStatus {
                obs_id: obs_id.clone(),
                status: runtime.status.clone(),
            });
        }
    }

    // ---- 去藏身处 ----------------------------------------------------

    /// 用户在卡片上点了"去藏身处"。
    ///
    /// 整条链路最多两次 POST:手上的 token 还新就直接发;太老或者被 503 拒了
    /// 就重新 fetch 一次拿新 token 再发一次。再不成就停手 —— 反复捅一个
    /// 拒绝我们的接口既没用,也不安全。
    fn travel_to_hideout(&mut self, alert_id: i64, now: i64) {
        if self.hideout.contains_key(&alert_id) {
            // 上一次点击还没走完。再排一次只会多发一次 whisper。
            self.emit(RuntimeEvent::Log(format!(
                "already travelling to the hideout for alert {alert_id}"
            )));
            return;
        }
        if usable_session(&self.settings, self.session_ok).is_none() {
            self.finish_hideout(alert_id, HideoutOutcome::NoSession);
            return;
        }
        let Some(row) = self.note(self.store.alert(alert_id), "alert").flatten() else {
            self.finish_hideout(
                alert_id,
                HideoutOutcome::Failed {
                    status: 0,
                    message: format!(
                        "alert {alert_id} is no longer in the alert history — \
                         the card outlived its database row, so there is nothing to travel to"
                    ),
                },
            );
            return;
        };

        let token = usable_token(row.hideout_token.as_deref(), row.token_fetched_at, now);
        self.hideout.insert(alert_id, flow_for(&row));
        match token {
            Some(token) => self.post_whisper(alert_id, &token),
            // 没有 token,或者它已经(快)作废了:先去换一张新的。
            //
            // 这一句得留在日志里:不然"点一次却打了两个请求"看着像 bug,
            // 而真跑之后要靠它回答"到底是没 token 还是 exp 过了"。
            None => {
                self.emit(RuntimeEvent::Log(format!(
                    "hideout alert {alert_id}: refetching first — hideout_token {}",
                    describe_token(row.hideout_token.as_deref(), now)
                )));
                self.refresh_hideout_token(alert_id);
            }
        }
    }

    /// 重新 fetch 这一条挂单,只为了拿一个新鲜的 hideout_token。
    /// 用户优先级 —— 他正看着卡片等结果。
    fn refresh_hideout_token(&mut self, alert_id: i64) {
        let Some(flow) = self.hideout.get_mut(&alert_id) else {
            return;
        };
        flow.refreshed = true;
        let request = GatewayRequest {
            kind: RequestKind::Fetch {
                ids: vec![flow.listing_id.clone()],
                search_id: flow.search_id.clone(),
            },
            priority: Priority::User,
            reply: self.replies.clone(),
            tag: RequestTag::alert(alert_id, HIDEOUT_FETCH_LABEL),
        };
        self.gateway.submit(request);
        self.report_hideout(alert_id, HideoutOutcome::Refreshed);
    }

    fn post_whisper(&mut self, alert_id: i64, token: &str) {
        let Some(flow) = self.hideout.get_mut(&alert_id) else {
            return;
        };
        if flow.posts >= MAX_WHISPER_POSTS {
            let listing_id = flow.listing_id.clone();
            let outcome = HideoutOutcome::Failed {
                status: 0,
                message: format!(
                    "this click already posted the travel request twice for listing \
                     {listing_id} — not trying a third time"
                ),
            };
            self.finish_hideout(alert_id, outcome);
            return;
        }
        flow.posts += 1;
        let referer = search_page_url(&SearchRef {
            league: flow.league.clone(),
            search_id: flow.search_id.clone(),
        });
        let request = GatewayRequest {
            kind: RequestKind::Whisper {
                token: token.to_string(),
                referer,
            },
            // 队列里的头一位:用户正看着卡片等这一下。
            priority: Priority::User,
            reply: self.replies.clone(),
            tag: RequestTag::alert(alert_id, HIDEOUT_WHISPER_LABEL),
        };
        self.gateway.submit(request);
    }

    fn on_hideout_reply(&mut self, alert_id: i64, kind: ReplyKind, now: i64) {
        // 已经收尾了(比如两次 503 之后停手),迟到的回信丢掉。挂单 id 顺手
        // 抄一份:下面每一句失败消息都要说清"是哪一件货"。
        let Some(listing_id) = self
            .hideout
            .get(&alert_id)
            .map(|flow| flow.listing_id.clone())
        else {
            return;
        };
        match kind {
            ReplyKind::Fetch(Ok(listings)) => self.on_hideout_token(alert_id, &listings, now),
            ReplyKind::Fetch(Err(error)) => {
                let outcome = hideout_failure(HideoutStep::Refetch, &listing_id, &error);
                self.finish_hideout(alert_id, outcome);
            }
            ReplyKind::Whisper(Ok(status)) => {
                let _ = status;
                self.finish_hideout(alert_id, HideoutOutcome::Sent);
            }
            ReplyKind::Whisper(Err(GatewayError::Status { status, excerpt }))
                if looks_like_a_stale_token(status) =>
            {
                // 401/403/503 差不多都是"这个 token 不好使了"。换一个再试
                // 一次,但只换一次。(HTML 的 403 走不到这里 —— 网关把它
                // 认成 `CloudflareHold` 了,换新 token 也照样被拦。)
                let refreshed = self.hideout.get(&alert_id).is_some_and(|f| f.refreshed);
                if refreshed {
                    let outcome =
                        hideout_retry_exhausted(status, &listing_id, &excerpt, self.session_ok);
                    self.finish_hideout(alert_id, outcome);
                } else {
                    self.refresh_hideout_token(alert_id);
                }
            }
            ReplyKind::Whisper(Err(error)) => {
                let outcome = hideout_failure(HideoutStep::Whisper, &listing_id, &error);
                self.finish_hideout(alert_id, outcome);
            }
            // 这条链路上不会有 search、不会有会话检查,也不会有按 id 对号的
            // fetch(那是市场观察专用的)。
            ReplyKind::Search(_) | ReplyKind::SessionCheck(_) | ReplyKind::FetchByIds(_) => {}
        }
    }

    /// 刷新 token 的 fetch 回来了。
    ///
    /// 只看 token 在不在,不看卖家在不在线:即刻购买(instant buyout)的货
    /// 就摆在卖家藏身处的商店里,官网对十几个小时没上线的卖家照样给
    /// "去藏身处"。所以能不能传送是交易站发不发 token 说了算。
    ///
    /// 两种"拿不到 token"分得很开:挂单没了、这次 fetch 拿回来的挂单不带
    /// token。两种都是 [`HideoutOutcome::TokenMissing`],但用户该看见的话
    /// 不一样 —— 前一种是"晚了一步",后一种是"这条货不支持传送,或者你的
    /// 会话有问题"。
    fn on_hideout_token(&mut self, alert_id: i64, listings: &[ListingSummary], now: i64) {
        let Some(flow) = self.hideout.get(&alert_id) else {
            return;
        };
        let listing_id = flow.listing_id.clone();

        let Some(listing) = listings.iter().find(|listing| listing.id == listing_id) else {
            self.token_missing(
                alert_id,
                format!(
                    "the refetch of listing {listing_id} came back without it — \
                     the item has been sold or delisted"
                ),
            );
            return;
        };
        let Some(token) = listing.hideout_token.clone() else {
            self.token_missing(
                alert_id,
                format!(
                    "listing {listing_id} came back without a hideout_token — \
                     it may not be an instant-buyout listing, or the fetch went \
                     out without a session"
                ),
            );
            return;
        };
        self.note(
            self.store.set_hideout_token(alert_id, &token, now),
            "set_hideout_token",
        );
        self.post_whisper(alert_id, &token);
    }

    /// 拿不到 token 就收尾。
    ///
    /// `TokenMissing` 上没有消息字段(界面按枚举查双语文案),所以"为什么"
    /// 走一条日志 —— 状态栏里看得见,不然三种完全不同的原因在卡片上长得一样。
    fn token_missing(&mut self, alert_id: i64, why: String) {
        self.emit(RuntimeEvent::Log(format!(
            "hideout alert {alert_id}: {why}"
        )));
        self.finish_hideout(alert_id, HideoutOutcome::TokenMissing);
    }

    /// 收尾:把这条流程从表里拿掉,回写 `last_action`,广播结果。
    fn finish_hideout(&mut self, alert_id: i64, outcome: HideoutOutcome) {
        self.hideout.remove(&alert_id);
        self.report_hideout(alert_id, outcome);
    }

    /// 报一句进展(不一定是结局)。
    fn report_hideout(&mut self, alert_id: i64, outcome: HideoutOutcome) {
        // 卡片脚注只放得下一个状态码,完整的原因走日志。没有这一条,
        // 一次失败在界面上就只剩 `失败(0)`,什么也查不出来。
        if let HideoutOutcome::Failed { status, message } = &outcome {
            let answered = if *status == 0 {
                "no HTTP answer".to_string()
            } else {
                format!("HTTP {status}")
            };
            self.emit(RuntimeEvent::Log(format!(
                "hideout alert {alert_id}: failed ({answered}) — {message}"
            )));
        }
        self.note(
            self.store.set_last_action(alert_id, outcome.action()),
            "set_last_action",
        );
        self.emit(RuntimeEvent::HideoutResult { alert_id, outcome });
    }

    // ---- 测试会话 ----------------------------------------------------

    /// 用户在设置页点了"测试会话"。
    ///
    /// 发一次真的 search(带着 cookie),然后只看响应的限速规则里有没有
    /// `Account` —— 这是判断一个 POESESSID 还活着没有的唯一可靠信号。
    /// 代价是 6 小时 299 次里的一次,所以只有点一下才发。
    fn test_session(&mut self) {
        if usable_session(&self.settings, self.session_ok).is_none() {
            // 一个请求都不发:没 cookie 可试,或者它刚刚已经被服务端拒过。
            self.emit(RuntimeEvent::SessionChecked {
                valid: false,
                detail: "no usable POESESSID in settings".to_string(),
            });
            return;
        }
        let (league, body_json) =
            session_check_request(&self.settings.watches, &self.settings.league, |watch_id| {
                self.watches
                    .get(watch_id)
                    .and_then(|runtime| runtime.query_body.clone())
            });
        self.gateway.submit(GatewayRequest {
            kind: RequestKind::SessionCheck { league, body_json },
            // 用户正看着按钮等结果,排在队列最前面。
            priority: Priority::User,
            reply: self.replies.clone(),
            tag: RequestTag::standalone(SESSION_CHECK_LABEL),
        });
    }

    fn on_session_checked(&mut self, result: &Result<SessionCheckOutcome, GatewayError>) {
        let (valid, detail) = session_check_report(result);
        self.emit(RuntimeEvent::SessionChecked { valid, detail });
    }

    // ---- 网关广播 ----------------------------------------------------

    fn handle_gateway_event(&mut self, event: GatewayEvent) {
        match event {
            GatewayEvent::Budget {
                policy,
                usage,
                next_allowed_in_secs,
            } => {
                self.emit(RuntimeEvent::Budget {
                    policy,
                    usage,
                    next_allowed_in_secs,
                });
            }
            GatewayEvent::SessionInvalid => self.on_session_invalid(now_secs()),
            GatewayEvent::CloudflareBlocked { until } => {
                self.emit(RuntimeEvent::CloudflareBlocked { until });
            }
            GatewayEvent::RateLimited { policy, retry_in } => {
                self.emit(RuntimeEvent::Log(format!(
                    "rate limited on {policy}, retrying in {retry_in}s"
                )));
            }
            GatewayEvent::Log(message) => self.emit(RuntimeEvent::Log(message)),
        }
    }

    // ---- 汇率 --------------------------------------------------------

    /// 起(或者换)一条 `pnd-rates` 线程。它每 15 分钟读一次 ninja 的换算表,
    /// 失败就保留上一份 —— 汇率过一会儿再对,总比拿一个瞎猜的数去比价好。
    fn restart_rates_thread(&mut self) {
        if let Some(cancel) = self.rates_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let league = self.settings.league.clone();
        let inbox = self.inbox.clone();
        let events = self.events.clone();
        let spawned = thread::Builder::new()
            .name("pnd-rates".to_owned())
            .spawn(move || rates_loop(&league, &thread_cancel, &inbox, &events));
        match spawned {
            Ok(_) => self.rates_cancel = Some(cancel),
            Err(error) => self.emit(RuntimeEvent::Log(format!(
                "could not start the rates thread: {error}"
            ))),
        }
    }

    // ---- 杂活 --------------------------------------------------------

    fn emit(&self, event: RuntimeEvent) {
        let _ = self.events.send(event);
    }

    fn emit_status(&self, watch_id: &WatchId) {
        if let Some(runtime) = self.watches.get(watch_id) {
            self.emit(RuntimeEvent::WatchStatus {
                watch_id: watch_id.clone(),
                status: runtime.status.clone(),
            });
        }
    }

    /// 库操作出错时记一笔日志,然后继续跑。
    ///
    /// 为什么不 panic:单用户小工具里,一次写库失败(盘满了、文件被占了)
    /// 不该把整个蹲价停掉 —— 下一轮多半就好了,而且提醒本身还能发出来。
    fn note<T>(&self, result: Result<T, StorageError>, what: &str) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.emit(RuntimeEvent::Log(format!("{what} failed: {error}")));
                None
            }
        }
    }
}

// ---------------------------------------------------------------------
// 线程小工具
// ---------------------------------------------------------------------

/// 把一个通道里的东西套上 `Inbox` 的壳倒进 actor 的邮箱。
///
/// 上游断了(网关线程结束)或者下游没了(actor 结束)就自己收摊,
/// 所以不需要取消开关,也不需要 join。
fn forward<T: Send + 'static>(
    source: Receiver<T>,
    inbox: Sender<Inbox>,
    wrap: fn(T) -> Inbox,
    name: &str,
) {
    let _ = thread::Builder::new().name(name.to_owned()).spawn(move || {
        for item in source {
            if inbox.send(wrap(item)).is_err() {
                break;
            }
        }
    });
}

/// 汇率线程本体。
fn rates_loop(
    league: &str,
    cancel: &AtomicBool,
    inbox: &Sender<Inbox>,
    events: &Sender<RuntimeEvent>,
) {
    let client = NinjaClient::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        match client.currency_rates(league) {
            Ok(overview) => match overview.rates_per_divine() {
                Some(rates) => {
                    let rates = CurrencyRates {
                        chaos_per_divine_milli: to_milli(rates.chaos),
                        exalted_per_divine_milli: to_milli(rates.exalted),
                        mirror_per_divine_milli: to_milli(rates.mirror),
                    };
                    if inbox.send(Inbox::Rates(rates)).is_err() {
                        return;
                    }
                }
                None => {
                    let _ = events.send(RuntimeEvent::Log(
                        "poe.ninja quotes this league in something other than divine — keeping the old rates"
                            .to_string(),
                    ));
                }
            },
            Err(error) => {
                let _ = events.send(RuntimeEvent::Log(format!(
                    "poe.ninja rates unavailable ({error}) — keeping the old rates"
                )));
            }
        }
        // 一秒一醒地数完 15 分钟:关机时不用等一刻钟。
        for _ in 0..RATES_REFRESH_SECONDS {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
}

fn to_milli(value: Option<f64>) -> Option<i64> {
    value
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| (value * 1000.0).round() as i64)
}

fn budget_of(settings: &AppSettings) -> Budget {
    Budget {
        percent: settings.watcher.budget_percent,
        margin: settings.watcher.limit_margin,
    }
}

/// 空的 POESESSID = 匿名。设置里存的是字符串,网关要的是 `Option`。
fn session_of(settings: &AppSettings) -> Option<String> {
    let session = settings.poesessid.trim();
    (!session.is_empty()).then(|| session.to_string())
}

/// 现在还能用的会话。服务端拒过一次(`session_ok = false`)之后,
/// 设置里那串虽然还在,但我们不再拿它去试 —— 直到用户换一个新的。
fn usable_session(settings: &AppSettings, session_ok: bool) -> Option<String> {
    if session_ok {
        session_of(settings)
    } else {
        None
    }
}

/// 同时最多开几条 live。
///
/// 两道闸:设置里的 `max_live_connections`(计划里的温柔阈值,默认 5,用户
/// 可以调)和服务端每账号 20 条的硬顶(改不了,所以写在代码里)。
fn live_connection_cap(settings: &AppSettings) -> usize {
    (settings.watcher.max_live_connections as usize).min(MAX_LIVE_CONNECTIONS_PER_ACCOUNT)
}

/// 一条正常在跑的搜索该显示哪一档:WS 连着就是 Live,否则 Polling。
fn healthy_state(live: LiveRunState) -> WatchRunState {
    if live.is_connected() {
        WatchRunState::Live
    } else {
        WatchRunState::Polling
    }
}

/// 手上这个 hideout_token 还能直接用吗。
///
/// 纯函数,两条规矩,顺序要紧:
///
/// 1. **token 自己说了什么时候作废(`exp`),就听它的。** 它是那张票的
///    发行方写的,比我们从外面猜准。剩不到
///    [`HIDEOUT_TOKEN_EXPIRY_MARGIN_SECS`] 秒就当它已经没了。
/// 2. 没有 `exp` 才退回"拿到多久了":超过 [`HIDEOUT_TOKEN_MAX_AGE_SECS`]
///    当过期,连什么时候拿的都不知道也当过期。
///
/// 只解码不验签 —— 我们要的只是一个时刻,没有任何一处拿它做安全判断。
/// 宁可多 fetch 一次,也不要发一个必然被拒的请求。
fn usable_token(token: Option<&str>, fetched_at: Option<i64>, now: i64) -> Option<String> {
    let token = token.map(str::trim).filter(|token| !token.is_empty())?;
    if let Some(exp) = jwt_expiry(token) {
        return (exp - now > HIDEOUT_TOKEN_EXPIRY_MARGIN_SECS).then(|| token.to_string());
    }
    let fetched_at = fetched_at?;
    (now - fetched_at <= HIDEOUT_TOKEN_MAX_AGE_SECS).then(|| token.to_string())
}

/// 测试会话拿什么去问:(联赛, 请求体)。
///
/// 借用户自己第一条启用着的搜索 —— 那个查询一定是合法的,而且它本来就是
/// 这个程序会发的东西。一条搜索都没有(或者粘进来的 id 还没解开)才退回
/// [`probe_body`]。纯函数:`body_of` 把"这条搜索的请求体在哪儿"这件事
/// 留给调用方,于是这一段不用碰 actor 的内部状态也测得动。
fn session_check_request(
    watches: &[WatchEntry],
    league: &str,
    body_of: impl Fn(&WatchId) -> Option<String>,
) -> (String, String) {
    for entry in watches {
        if !entry.enabled {
            continue;
        }
        if let Some(body) = body_of(&entry.id) {
            return (entry.league.clone(), body);
        }
    }
    (league.to_string(), probe_body())
}

/// 没有搜索可借时用的探针查询:在线的 Divine Orb,按价升序。
///
/// 挑它是因为任何联赛都有人在卖(所以不会因为"没有结果"而看不出会话死活),
/// 而且它和用户蹲的东西毫无关系,不会往去重表里塞奇怪的挂单。
fn probe_body() -> String {
    search_request_body(r#"{"status":{"option":"online"},"type":"Divine Orb"}"#)
}

/// 一次会话检查的回信 → (会话还能用吗, 给人看的细节)。
///
/// 判据只有一条:限速规则里有没有 `Account`。带着 cookie 发出去的请求,
/// 服务端只按 `Ip` 限速就说明它压根没认出这个会话。
fn session_check_report(result: &Result<SessionCheckOutcome, GatewayError>) -> (bool, String) {
    match result {
        Ok(outcome) => {
            let rules = if outcome.rules.is_empty() {
                "no rate-limit rules".to_string()
            } else {
                format!("rules: {}", outcome.rules.join(", "))
            };
            (
                outcome.mentions_account,
                format!("HTTP {} · {rules}", outcome.status),
            )
        }
        // 请求根本没发出去(网络断了、被 Cloudflare 拦了、关机取消了):
        // 这不是"会话坏了",但也确实没验成,照实说。
        Err(error) => (false, error.to_string()),
    }
}

/// 提醒历史里的一行 → 一次"去藏身处"的起点。
fn flow_for(row: &AlertRow) -> HideoutFlow {
    HideoutFlow {
        listing_id: row.listing_id.clone(),
        league: row.league.clone(),
        search_id: row.search_id.clone(),
        posts: 0,
        refreshed: false,
    }
}

/// "去藏身处"链路上的哪一步。
///
/// 消息里必须说清是哪一步:同样是 404,"重新抓这条挂单时 404"是货没了,
/// "POST whisper 时 404"是 token 和挂单对不上,处置完全不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HideoutStep {
    /// 为了换一个新 token 而重新 fetch 这条挂单。
    Refetch,
    /// `POST /api/trade2/whisper`。
    Whisper,
}

impl HideoutStep {
    fn label(self) -> &'static str {
        match self {
            HideoutStep::Refetch => "the refetch",
            HideoutStep::Whisper => "the travel request",
        }
    }
}

/// 这个状态码值不值得"换一张 token 再试一次"。
///
/// 503 是计划里就写着的("通常是 token 过期")。401/403 是 2026-09-07 那次
/// 真跑加进来的:一条挂了 37 分钟的挂单点下去回了个**非 HTML** 的 403 ——
/// 那不是 Cloudflare(那种带 HTML,网关另有一条路),是交易站自己在说
/// "这张票我不认"。换一张新的正好能分清"票过期了"和"人不认了":
/// 换过还是同一个码,才轮到怀疑会话。
fn looks_like_a_stale_token(status: u16) -> bool {
    matches!(status, 401 | 403 | 503)
}

/// 换过 token 还是被拒:这是最后一句话,所以必须说清下一步能做什么。
///
/// 三条路,因为它们说的根本不是一件事:
///
/// - **503** = "服务端这会儿不接这一单",多半是你自己的游戏客户端不在城里。
/// - **401/403 而会话确实已经被拒过**(`session_ok = false`)= 它真的不认你了,
///   换一个新的 cookie。
/// - **401/403 可会话还好好的** = 别再让人去换 cookie。2026-09-07 那次真跑
///   就是这一种:测试会话过了、live 连着、轮询正常,同一个 cookie 上的
///   search 和 fetch 都好好的,唯独这个 POST 回 403。换 cookie 一次也修不好,
///   因为坏的根本不是 cookie —— 当前的猜想是这封请求没被当成"从网站发出的"
///   (见 `pnd-trade` 里 `TradeClient::whisper` 的注释)。
///
/// 早先这里不分会话死活,一律说"POESESSID 多半失效了"。那句话会把人送进
/// 一个换不完的循环:换一个新 cookie、还是 403、再换一个。
fn hideout_retry_exhausted(
    status: u16,
    listing_id: &str,
    excerpt: &str,
    session_ok: bool,
) -> HideoutOutcome {
    let advice = if status == 503 {
        ", so check that your own game client is logged in and standing in a town or hideout"
    } else if session_ok {
        ", and the session itself still works — the trade site is rejecting the travel request \
         as not coming from the website (see the whisper doc comment in pnd-trade)"
    } else {
        ", so the POESESSID is probably no longer valid — paste a fresh one into settings"
    };
    let (reason, detail) = excerpt_parts(excerpt);
    HideoutOutcome::Failed {
        status,
        message: format!(
            "{reason}the trade site answered {status} to both travel requests for listing \
             {listing_id} — a fresh token did not help{advice}{detail}"
        ),
    }
}

/// 交易站的报错正文 → (提到句首的原因, 挂在句尾的原文)。
///
/// 认得出是 GGG 的错误就把那句话提到最前面,后面的模板随便裁;认不出来就
/// 原样挂在句尾,一个字都不丢。为什么要提前:卡片脚注和状态行都会被截断,
/// 而被截掉的必须是模板 —— 那句"为什么"是这一行里唯一有信息量的东西。
fn excerpt_parts(excerpt: &str) -> (String, String) {
    match ggg_error(excerpt) {
        Some(reason) => (format!("{reason} — "), String::new()),
        None => (String::new(), detail_suffix(excerpt)),
    }
}

/// 一次网关失败 → 卡片脚注上那句话。
///
/// 纯函数,所以每一条路径都能在测试里钉死。`status` 只有在服务端真的答了
/// 一个状态码时才非零:**0 的意思是"这封请求压根没上过网"**,而它下面
/// 藏着五种完全不同的原因(断网、没会话、被 Cloudflare 拦、答非所问、
/// 程序在关机)—— 早先这五种在界面上长得一模一样,都是 `失败(0)`。
fn hideout_failure(step: HideoutStep, listing_id: &str, error: &GatewayError) -> HideoutOutcome {
    let what = step.label();
    let (status, message) = match error {
        GatewayError::Transport(detail) => (
            0,
            format!("{what} for listing {listing_id} never left this machine: {detail}"),
        ),
        GatewayError::NoSession => (
            0,
            format!(
                "{what} for listing {listing_id} needs a POESESSID and the gateway has none — \
                 the trade site most likely just rejected the session; paste a fresh one \
                 into settings and try again"
            ),
        ),
        GatewayError::Parse(detail) => (
            0,
            format!(
                "the trade site's answer to {what} for listing {listing_id} \
                 could not be read: {detail}"
            ),
        ),
        GatewayError::CloudflareHold => (
            0,
            format!(
                "Cloudflare answered instead of the trade site, so {what} for listing \
                 {listing_id} was never sent — every request is held for 5 minutes"
            ),
        ),
        GatewayError::Cancelled => (
            0,
            format!("the runtime shut down before {what} for listing {listing_id} went out"),
        ),
        GatewayError::Status {
            status: 404,
            excerpt,
        } => {
            let (reason, detail) = excerpt_parts(excerpt);
            (
                404,
                format!(
                    "{reason}listing {listing_id} is gone — \
                     the trade site answered 404 to {what}{detail}"
                ),
            )
        }
        // 这一句只会从"重新 fetch 拿新 token"那一步走过来:POST 上的
        // 401/403 在 [`looks_like_a_stale_token`] 那里就被截去换 token 了。
        // 所以它写死"the refetch",不用 `what` —— 说成"the travel request"
        // 会让人以为传送请求被会话拒了,而那封 POST 还没发出去呢。
        GatewayError::Status {
            status: status @ (401 | 403),
            excerpt,
        } => {
            let (reason, detail) = excerpt_parts(excerpt);
            (
                *status,
                format!(
                    "{reason}the refetch for listing {listing_id} was refused with \
                     HTTP {status} — the POESESSID is probably no longer valid{detail}"
                ),
            )
        }
        GatewayError::Status { status, excerpt } => {
            let (reason, detail) = excerpt_parts(excerpt);
            (
                *status,
                format!(
                    "{reason}the trade site answered HTTP {status} to {what} \
                     for listing {listing_id}{detail}"
                ),
            )
        }
    };
    HideoutOutcome::Failed { status, message }
}

/// 响应 body 有话说才把它接在消息后面。
fn detail_suffix(excerpt: &str) -> String {
    if excerpt.is_empty() {
        String::new()
    } else {
        format!(": {excerpt}")
    }
}

/// 本地时区今天零点的 unix 秒。"今天叫了几次"按本地日期算才符合直觉;
/// 算不出来(时区数据坏了)就退回"过去 24 小时"。
fn start_of_today(now: i64) -> i64 {
    chrono::DateTime::from_timestamp(now, 0)
        .map(|utc| utc.with_timezone(&chrono::Local))
        .and_then(|local| {
            local
                .date_naive()
                .and_hms_opt(0, 0, 0)?
                .and_local_timezone(chrono::Local)
                .single()
        })
        .map_or(now - 86_400, |midnight| midnight.timestamp())
}

#[cfg(test)]
mod actor_tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Instant;

    use pnd_domain::{Currency, Price};
    use pnd_settings::WatchEntry;
    use pnd_trade::TradeResponse;

    use crate::live_worker::live_worker_tests::{ScriptedConnector, Step, handshake};

    use super::*;

    /// 今天从网页上抄下来的真搜索 id(Choir of the Storm)。
    /// 用真的而不是编一个:`decode_search_id` 会去解 gzip,编的解不开。
    const FIXTURE_ID: &str = "H4sIAAAAAAAAAx2LvQnAIBBGV5GvdgLbjJAyWAhRFPRO9FIEcfdo2vcz0MXJ02EGuEpiggFTTuQxNcgVv8AROTXFQUn06hRuBfof13cNyFt35eheOKQsvm1hp50f6Xdj02AAAAA";

    /// 假交易站记下来的账:测试从这里读"到底发出去了什么请求"。
    #[derive(Default)]
    struct TradeLog {
        searches: usize,
        /// 每次 fetch 带了哪些 id。"一批最多 10 个"的证据就在这里。
        fetches: Vec<Vec<String>>,
        /// 每次 whisper 用的是哪个 token。"一次点击最多两次"看的是它的长度。
        whispers: Vec<String>,
        /// 依次回给 whisper 的状态码;用完了就一律 200。
        whisper_statuses: VecDeque<u16>,
        /// whisper 响应的 body。`None` = `{}`。非 2xx 时它就是"为什么被拒"。
        whisper_body: Option<String>,
        /// whisper 响应看着像不像 HTML(Cloudflare 拦截页)。
        whisper_html: bool,
        /// 设了就让 whisper 连发都发不出去(断网、TLS 挂了)。
        whisper_error: Option<String>,
        /// fetch 回来的挂单带不带 hideout_token(真实世界里取决于带没带 cookie)。
        hideout_token: Option<String>,
        /// fetch 回来的卖家在不在线。跟 hideout_token 没关系:即刻购买的货
        /// 摆在藏身处商店里,人不在也能传送过去。
        seller_offline: bool,
        /// fetch 回一个空 `result`(那件货在这几秒里被买走了)。
        fetch_returns_nothing: bool,
        /// 让 fetch 回来的每件货都标这个价(divine)。不设就用下面那套
        /// 18、17、16… 的递减价。
        price_divine: Option<i64>,
        /// search 响应的 `X-Rate-Limit-Rules` 写什么。`None` = 不给限速头。
        /// 会话检查看的就是这一行。
        rate_rules: Option<String>,
        /// search 回哪些 id。`None` = 默认那两个("one" / "two")。
        search_ids: Option<Vec<String>>,
        /// 每次 search 的请求体。观察那条路靠它证明排序真的换成了
        /// `{"indexed":"desc"}`。
        search_bodies: Vec<String>,
        /// 这些 id 在 fetch 的答复里是 `null` —— 它们已经没了。
        /// (2026-09-07 实测的形状:数组长度不变,查不到的那一格是 null。)
        gone_ids: BTreeSet<String>,
        /// 每件货身上的显示词缀。市场观察按它们聚合。
        item_mods: Vec<String>,
    }

    /// 假交易站:不打网络,search 回两个写死的 id,fetch 按你问的 id 现编。
    struct FakeTrade {
        log: Arc<Mutex<TradeLog>>,
    }

    impl FakeTrade {
        fn new() -> (Box<FakeTrade>, Arc<Mutex<TradeLog>>) {
            let log = Arc::new(Mutex::new(TradeLog::default()));
            (
                Box::new(FakeTrade {
                    log: Arc::clone(&log),
                }),
                log,
            )
        }
    }

    fn ok(body: &str) -> Result<TradeResponse, pnd_trade::TransportError> {
        Ok(TradeResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
            // 不给限速头:限速器学不到上限就一路放行,测试不用真的等 10 秒。
            rate: None,
            looks_like_html: false,
        })
    }

    /// 按问到的 id 现编一批挂单。价格从 18 divine 起每条便宜 1 个(封底 1),
    /// 所以最后一条最便宜 —— 合成卡片的标题该是它。`price_divine` 给了
    /// 就一律用它,那是"上限刚好卡在这个数上"那几个测试要的。
    ///
    /// `online = false` 时干脆不给 `account.online` 这个键 —— 交易站离线时
    /// 就是这么回的(不是回 `null`),摘要那一层认的也是"这个键在不在"。
    ///
    /// 词缀按**交易站真正回的形状**编:一格是个对象,显示文本在 `description`
    /// 里。早先这里编的是字符串数组(照 PoE1 写的),于是这个假交易站一路绿灯,
    /// 而真跑一趟一条词缀都记不进去 —— 假的和真的不一样,测试就白测了。
    fn listings_json(
        ids: &[String],
        token: Option<&str>,
        online: bool,
        price_divine: Option<i64>,
        gone: &BTreeSet<String>,
        mods: &[String],
    ) -> String {
        let mod_lines: Vec<String> = mods
            .iter()
            .map(|line| {
                format!(
                    r#"{{"description":"{line}","domain":"explicit","hash":"stat.explicit.x"}}"#
                )
            })
            .collect();
        let items: Vec<String> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                // 查不到的 id 那一格就是个 null,数组长度不变。
                if gone.contains(id) {
                    return "null".to_string();
                }
                let hideout = token
                    .map(|token| format!(r#""hideout_token":"{token}","#))
                    .unwrap_or_default();
                let presence = if online {
                    r#","online":{"league":"x"}"#
                } else {
                    ""
                };
                format!(
                    r#"{{"id":"{id}","listing":{{"indexed":"2026-09-06T10:00:00Z",
                        "whisper":"@{id} hi",{hideout}
                        "price":{{"type":"~price","amount":{},"currency":"divine"}},
                        "account":{{"name":"Seller{id}","lastCharacterName":"Char{id}"{presence}}}}},
                     "item":{{"name":"Choir of the Storm","typeLine":"Lapis Amulet",
                       "explicitMods":[{}]}}}}"#,
                    price_divine.unwrap_or((18 - index as i64).max(1)),
                    mod_lines.join(",")
                )
            })
            .collect();
        format!(r#"{{"result":[{}]}}"#, items.join(","))
    }

    impl TradeTransport for FakeTrade {
        fn search(
            &self,
            _league: &str,
            body_json: &str,
            _session: Option<&str>,
        ) -> Result<TradeResponse, pnd_trade::TransportError> {
            let (rules, ids) = {
                let mut log = self.log.lock().unwrap();
                log.searches += 1;
                log.search_bodies.push(body_json.to_string());
                (log.rate_rules.clone(), log.search_ids.clone())
            };
            let ids = ids.unwrap_or_else(|| vec!["one".to_string(), "two".to_string()]);
            let quoted: Vec<String> = ids.iter().map(|id| format!("\"{id}\"")).collect();
            let body = format!(
                r#"{{"id":"SEARCHID","total":{},"result":[{}]}}"#,
                ids.len(),
                quoted.join(",")
            );
            let body = body.as_str();
            let Some(rules) = rules else {
                return ok(body);
            };
            Ok(TradeResponse {
                status: 200,
                body: body.as_bytes().to_vec(),
                // 窗口给得很大(6 小时 600 次):测试要的是规则名,不是让
                // 限速器真的按 10 秒 5 次去卡下一封请求。
                rate: pnd_trade::parse_rate_headers([
                    ("X-Rate-Limit-Policy", pnd_trade::SEARCH_POLICY),
                    ("X-Rate-Limit-Rules", rules.as_str()),
                    ("X-Rate-Limit-Ip", "600:21600:3600"),
                    ("X-Rate-Limit-Ip-State", "1:21600:0"),
                ]),
                looks_like_html: false,
            })
        }

        fn fetch(
            &self,
            ids: &[String],
            _search_id: &str,
            _session: Option<&str>,
        ) -> Result<TradeResponse, pnd_trade::TransportError> {
            let (token, offline, nothing, price, gone, mods) = {
                let mut log = self.log.lock().unwrap();
                log.fetches.push(ids.to_vec());
                (
                    log.hideout_token.clone(),
                    log.seller_offline,
                    log.fetch_returns_nothing,
                    log.price_divine,
                    log.gone_ids.clone(),
                    log.item_mods.clone(),
                )
            };
            if nothing {
                return ok(r#"{"result":[]}"#);
            }
            ok(&listings_json(
                ids,
                token.as_deref(),
                !offline,
                price,
                &gone,
                &mods,
            ))
        }

        fn whisper(
            &self,
            token: &str,
            _session: &str,
            _referer: &str,
        ) -> Result<TradeResponse, pnd_trade::TransportError> {
            let (status, body, html, error) = {
                let mut log = self.log.lock().unwrap();
                log.whispers.push(token.to_string());
                (
                    log.whisper_statuses.pop_front().unwrap_or(200),
                    log.whisper_body.clone().unwrap_or_else(|| "{}".to_string()),
                    log.whisper_html,
                    log.whisper_error.clone(),
                )
            };
            if let Some(error) = error {
                return Err(pnd_trade::TransportError::Unreachable(error));
            }
            Ok(TradeResponse {
                status,
                body: body.into_bytes(),
                rate: None,
                looks_like_html: html,
            })
        }
    }

    fn settings() -> AppSettings {
        AppSettings {
            league: "Forbidden Rites".to_string(),
            watches: vec![WatchEntry {
                id: WatchId("w1".to_string()),
                label: "Choir of the Storm".to_string(),
                league: "Forbidden Rites".to_string(),
                search_id: FIXTURE_ID.to_string(),
                price_cap: Price::new(20_000, Currency::Divine),
                ..WatchEntry::default()
            }],
            ..AppSettings::default()
        }
    }

    /// 等一个事件:满足 `wanted` 就返回它,超时就把收到的都打出来再 panic。
    fn wait_for(
        handle: &RuntimeHandle,
        seen: &mut Vec<RuntimeEvent>,
        wanted: impl Fn(&RuntimeEvent) -> bool,
        what: &str,
    ) -> RuntimeEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match handle.try_next_event() {
                Some(event) => {
                    let matched = wanted(&event);
                    seen.push(event.clone());
                    if matched {
                        return seen.last().unwrap().clone();
                    }
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        panic!("timed out waiting for {what}; saw {seen:#?}");
    }

    #[test]
    fn a_poll_cycle_alerts_once_and_then_stays_quiet() {
        let (transport, log) = FakeTrade::new();
        let handle =
            RuntimeHandle::start_offline(settings(), RuntimePaths::in_memory(), transport).unwrap();

        let mut seen = Vec::new();
        wait_for(&handle, &mut seen, |e| *e == RuntimeEvent::Ready, "Ready");
        let matched = wait_for(
            &handle,
            &mut seen,
            |event| matches!(event, RuntimeEvent::ListingMatched(_)),
            "ListingMatched",
        );
        let RuntimeEvent::ListingMatched(matched) = matched else {
            unreachable!()
        };
        // 两条都在 20 divine 以下,合成一张卡:标题是便宜的那条,另一条算 "+1"。
        assert_eq!(matched.headline.id, "two");
        assert_eq!(matched.extra, 1);
        assert_eq!(matched.alert_ids.len(), 2);
        assert_eq!(matched.label, "Choir of the Storm");
        assert_eq!(matched.source, AlertSource::Poll);

        // 卡片按钮只带一个行号,而卡片上写的是标题那条:标题的行号必须排在
        // 第一位,否则"去藏身处"会把你送到另一个卖家那儿。两条提醒是按 fetch
        // 的顺序插库的("one" 先、"two" 后),所以标题那条("two",更便宜)
        // 的行号是两个里更大的那个。
        assert_eq!(
            matched.alert_ids.first(),
            matched.alert_ids.iter().max(),
            "标题那条的行号没排在最前面:{:?}",
            matched.alert_ids
        );

        assert_eq!(log.lock().unwrap().searches, 1);
        assert_eq!(log.lock().unwrap().fetches.len(), 1);

        // 再跑一轮同样的两条挂单:全都见过了,不该再响。
        handle
            .try_send(RuntimeCommand::PollNow(WatchId("w1".to_string())))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while log.lock().unwrap().fetches.len() < 2 && Instant::now() < deadline {
            if let Some(event) = handle.try_next_event() {
                seen.push(event);
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(log.lock().unwrap().fetches.len(), 2, "第二轮没跑起来");
        // 把第二轮剩下的事件也抽干,再看有没有第二张卡片。
        thread::sleep(Duration::from_millis(200));
        while let Some(event) = handle.try_next_event() {
            seen.push(event);
        }
        let cards = seen
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ListingMatched(_)))
            .count();
        assert_eq!(cards, 1, "同样的挂单响了两次:{seen:#?}");
    }

    /// 太贵的挂单不该把去重表堵死。
    ///
    /// 真实现场:上限 221 的时候轮到一件 222 divine 的 Mageblood,程序把它
    /// 记成"见过了";用户随即把上限改成 223,同一件货再轮到时被去重表挡掉,
    /// 一声都没响。上限是用户随时会动的东西,"见过"必须是"已经为它叫过一次",
    /// 而不是"这条数据我读到过"。
    #[test]
    fn a_listing_that_was_too_expensive_alerts_after_the_cap_is_raised() {
        let (transport, log) = FakeTrade::new();
        log.lock().unwrap().price_divine = Some(222);

        let mut settings = settings();
        settings.watches[0].price_cap = Price::new(221_000, Currency::Divine);
        // 一次只抓一件:这样"响了几次"数的就是这一件货。
        settings.watcher.fetch_batch = 1;
        let handle =
            RuntimeHandle::start_offline(settings.clone(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(
                    event,
                    RuntimeEvent::WatchStatus { status, .. } if status.last_total == Some(2)
                )
            },
            "the first poll to finish",
        );
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ListingMatched(_))),
            "222 divine 贵过 221 的上限,这一轮不该叫人:{seen:#?}"
        );

        // 用户把上限改成 223 —— 那件 222 的货现在够便宜了。
        settings.watches[0].price_cap = Price::new(223_000, Currency::Divine);
        handle
            .try_send(RuntimeCommand::ApplySettings(Box::new(settings)))
            .unwrap();
        handle
            .try_send(RuntimeCommand::PollNow(WatchId("w1".to_string())))
            .unwrap();

        wait_for(
            &handle,
            &mut seen,
            |event| matches!(event, RuntimeEvent::ListingMatched(_)),
            "a ListingMatched after the cap was raised",
        );
        // 再跑一轮同样的货:这回它真的"已经叫过了",不该有第二张卡。
        handle
            .try_send(RuntimeCommand::PollNow(WatchId("w1".to_string())))
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        while let Some(event) = handle.try_next_event() {
            seen.push(event);
        }
        let cards = seen
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ListingMatched(_)))
            .count();
        assert_eq!(cards, 1, "同一件货响了不止一次:{seen:#?}");
    }

    /// 改完上限就立刻再查一次。
    ///
    /// live 连着的时候轮询会放宽到十几分钟一次,用户改完上限盯着屏幕等了
    /// 一刻钟才想明白"下一轮还没到"。上限是他刚刚亲手动过的东西,答案该在
    /// 几秒内出现;别的改动(改个备注名)则不该白花一次搜索额度。
    #[test]
    fn changing_the_price_cap_schedules_the_next_poll_right_away() {
        let (transport, log) = FakeTrade::new();
        let mut settings = settings();
        // 普通档拉到一小时:没有这条规矩的话,下一轮要等到一小时之后。
        settings.watcher.poll_interval_seconds = 3_600;
        let handle =
            RuntimeHandle::start_offline(settings.clone(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(
                    event,
                    RuntimeEvent::WatchStatus { status, .. } if status.last_total == Some(2)
                )
            },
            "the first poll to finish",
        );
        assert_eq!(log.lock().unwrap().searches, 1);

        // 只改备注名:时间表一动不动。
        settings.watches[0].label = "renamed".to_string();
        handle
            .try_send(RuntimeCommand::ApplySettings(Box::new(settings.clone())))
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        while let Some(event) = handle.try_next_event() {
            seen.push(event);
        }
        assert_eq!(
            log.lock().unwrap().searches,
            1,
            "改个名字不该白花一次搜索额度"
        );

        // 改上限:下一轮就排在此刻。
        settings.watches[0].price_cap = Price::new(30_000, Currency::Divine);
        handle
            .try_send(RuntimeCommand::ApplySettings(Box::new(settings)))
            .unwrap();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if status.next_poll_at.is_some_and(|at| at <= now_secs()))
            },
            "next_poll_at to move to now",
        );

        // 而且真的跑起来了 —— 排上却不发,和没排一样。
        let deadline = Instant::now() + Duration::from_secs(10);
        while log.lock().unwrap().searches < 2 && Instant::now() < deadline {
            match handle.try_next_event() {
                Some(event) => seen.push(event),
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        assert_eq!(
            log.lock().unwrap().searches,
            2,
            "改了上限之后没有立刻再查一次:{seen:#?}"
        );
    }

    /// 设置里填 60 秒也快不过预算地板,而且界面上看得见到底多久一次。
    #[test]
    fn the_budget_floor_holds_the_poll_interval_and_reaches_the_status() {
        let (transport, _log) = FakeTrade::new();
        let mut settings = settings();
        settings.watcher.poll_interval_seconds = 60;
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::in_memory(), transport).unwrap();

        let mut seen = Vec::new();
        let event = wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(
                    event,
                    RuntimeEvent::WatchStatus { status, .. } if status.last_total == Some(2)
                )
            },
            "the first poll to finish",
        );
        let RuntimeEvent::WatchStatus { status, .. } = event else {
            unreachable!()
        };
        // 6 小时 299 次的额度,一条搜索最快 73 秒一轮 —— 填 60 也没用。
        assert_eq!(status.poll_every_secs, 73);
        assert!(
            status
                .next_poll_at
                .is_some_and(|at| at - now_secs() > 60 && at - now_secs() <= 73),
            "下一轮排在 {:?},现在是 {}",
            status.next_poll_at,
            now_secs()
        );
    }

    #[test]
    fn a_watch_that_should_not_alert_on_the_first_poll_only_records() {
        let mut settings = settings();
        settings.watches[0].alert_on_first_poll = false;
        let (transport, _log) = FakeTrade::new();
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::in_memory(), transport).unwrap();

        let mut seen = Vec::new();
        // 第一轮跑完的标志是"轮询完成后的那条状态"(带 last_total)。
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(
                    event,
                    RuntimeEvent::WatchStatus { status, .. } if status.last_total == Some(2)
                )
            },
            "the first poll to finish",
        );
        thread::sleep(Duration::from_millis(200));
        while let Some(event) = handle.try_next_event() {
            seen.push(event);
        }
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ListingMatched(_))),
            "第一轮不该叫人:{seen:#?}"
        );
    }

    #[test]
    fn a_broken_search_id_is_reported_but_does_not_stop_the_runtime() {
        let mut settings = settings();
        settings.watches[0].search_id = "not base64!".to_string();
        let (transport, _log) = FakeTrade::new();
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::in_memory(), transport).unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(
                    event,
                    RuntimeEvent::WatchStatus { status, .. }
                        if status.state == WatchRunState::Backoff && status.last_error.is_some()
                )
            },
            "a Backoff status for the broken search id",
        );
    }

    #[test]
    fn start_of_today_is_midnight_local() {
        let now = now_secs();
        let midnight = start_of_today(now);
        assert!(midnight <= now);
        assert!(now - midnight < 25 * 3_600);
    }

    // ---- live --------------------------------------------------------

    /// 一份"能开 live"的设置:有会话,搜索勾了 live。
    fn live_settings() -> AppSettings {
        let mut settings = settings();
        settings.poesessid = "cookie".to_string();
        settings.watches[0].live = true;
        settings
    }

    /// 假连接器 → actor 要的那个 trait 对象。测试还要留一份自己看账,
    /// 所以不能直接把 `Arc` 交出去。
    fn shared(connector: &Arc<ScriptedConnector>) -> Arc<dyn LiveConnector> {
        Arc::clone(connector) as Arc<dyn LiveConnector>
    }

    /// 这一批 fetch 里,`New` 推来的那些 id 各分在了哪几批。
    fn live_batches(log: &Arc<Mutex<TradeLog>>) -> Vec<Vec<String>> {
        log.lock()
            .unwrap()
            .fetches
            .iter()
            .filter(|batch| batch.iter().all(|id| id.starts_with("live-")))
            .cloned()
            .collect()
    }

    /// 秒推来 25 个 id:交易站一次最多认 10 个,所以必须切成 10/10/5 三批,
    /// 而且判定出来的提醒要记成 `Live` 而不是 `Poll`。
    #[test]
    fn a_live_push_is_fetched_in_batches_of_ten_and_alerts_as_live() {
        let (transport, log) = FakeTrade::new();
        let connector = Arc::new(ScriptedConnector::new());
        let ids: Vec<String> = (0..25).map(|index| format!("live-{index}")).collect();
        connector.push(Step::New(ids.clone()));

        let handle = RuntimeHandle::start_offline_with_connector(
            live_settings(),
            RuntimePaths::in_memory(),
            transport,
            connector,
        )
        .unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::ListingMatched(matched)
                    if matched.source == AlertSource::Live)
            },
            "a live ListingMatched",
        );

        let batches = live_batches(&log);
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![10, 10, 5],
            "一次 fetch 最多 10 个 id"
        );
        let flat: Vec<String> = batches.concat();
        assert_eq!(flat, ids, "25 个 id 一个不能少、顺序也别乱");
    }

    /// 带着 cookie 被拒 = 会话没了:只喊一次,而且所有 live 都得停。
    #[test]
    fn a_refused_session_is_reported_once_and_stops_every_live_worker() {
        let (transport, _log) = FakeTrade::new();
        let connector = Arc::new(ScriptedConnector::new());
        // 两条搜索 = 两条 worker,两条都会撞上 401。
        connector.refuse(handshake(401, false, None));
        connector.refuse(handshake(401, false, None));

        let mut settings = live_settings();
        let second = WatchEntry {
            id: WatchId("w2".to_string()),
            ..settings.watches[0].clone()
        };
        settings.watches.push(second);

        let handle = RuntimeHandle::start_offline_with_connector(
            settings,
            RuntimePaths::in_memory(),
            transport,
            connector,
        )
        .unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| *event == RuntimeEvent::SessionInvalid,
            "SessionInvalid",
        );
        // 第二条 worker 的那一声也该在这段时间里到达 —— 但只能广播一次。
        thread::sleep(Duration::from_millis(400));
        while let Some(event) = handle.try_next_event() {
            seen.push(event);
        }
        let shouts = seen
            .iter()
            .filter(|event| **event == RuntimeEvent::SessionInvalid)
            .count();
        assert_eq!(shouts, 1, "会话失效只该说一次:{seen:#?}");

        // 两条搜索最后都该停在"会话失效"这一档。
        let mut live: BTreeMap<String, LiveRunState> = BTreeMap::new();
        for event in &seen {
            if let RuntimeEvent::WatchStatus { watch_id, status } = event {
                live.insert(watch_id.to_string(), status.live);
            }
        }
        for watch in ["w1", "w2"] {
            assert_eq!(
                live.get(watch),
                Some(&LiveRunState::Disabled(LiveOffReason::SessionInvalid)),
                "{watch} 还挂着 live:{live:#?}"
            );
        }
    }

    /// 没有 POESESSID 时,live 安静地停用,轮询照常跑 —— 一句日志,不刷屏。
    #[test]
    fn without_a_session_live_is_disabled_and_polling_carries_on() {
        let (transport, log) = FakeTrade::new();
        let connector = Arc::new(ScriptedConnector::new());
        let handle = RuntimeHandle::start_offline_with_connector(
            settings(),
            RuntimePaths::in_memory(),
            transport,
            shared(&connector),
        )
        .unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if status.live == LiveRunState::Disabled(LiveOffReason::NoSession))
            },
            "a Disabled(NoSession) live state",
        );
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if status.last_total == Some(2))
            },
            "the first poll to finish",
        );
        assert_eq!(connector.connects(), 0, "没会话就别去握手");
        assert!(log.lock().unwrap().searches >= 1, "轮询照常跑");
    }

    /// WS 连上了轮询就放宽到 15 分钟档,断了立刻收回 5 分钟档。
    #[test]
    fn a_live_connection_relaxes_the_poll_interval_and_a_drop_tightens_it() {
        let (transport, _log) = FakeTrade::new();
        let connector = Arc::new(ScriptedConnector::new());
        let mut settings = live_settings();
        settings.watcher.poll_interval_seconds = 60;
        settings.watcher.poll_interval_when_live_seconds = 900;

        let handle = RuntimeHandle::start_offline_with_connector(
            settings,
            RuntimePaths::in_memory(),
            transport,
            shared(&connector),
        )
        .unwrap();
        let watch = WatchId("w1".to_string());

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if status.live.is_connected())
            },
            "the live connection to come up",
        );

        // 连上之后跑一轮:排下一轮时该用宽松档。
        handle
            .try_send(RuntimeCommand::PollNow(watch.clone()))
            .unwrap();
        let relaxed = wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if status.live.is_connected()
                        && status.next_poll_at.is_some_and(|at| at - now_secs() > 600))
            },
            "the relaxed poll interval",
        );
        let RuntimeEvent::WatchStatus { status, .. } = relaxed else {
            unreachable!()
        };
        assert_eq!(status.state, WatchRunState::Live);

        // 断线:档位收回来。
        connector.push(Step::Fail(pnd_trade::live::LiveError::Io(
            "connection reset".to_string(),
        )));
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if !status.live.is_connected())
            },
            "the live connection to drop",
        );
        handle.try_send(RuntimeCommand::PollNow(watch)).unwrap();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if !status.live.is_connected()
                        && status.next_poll_at.is_some_and(|at| at - now_secs() <= 120))
            },
            "the tight poll interval to come back",
        );
    }

    // ---- 去藏身处 ----------------------------------------------------

    /// 跑一轮轮询,把第一条提醒的行号拿回来。
    fn first_alert(handle: &RuntimeHandle, seen: &mut Vec<RuntimeEvent>) -> i64 {
        let matched = wait_for(
            handle,
            seen,
            |event| matches!(event, RuntimeEvent::ListingMatched(_)),
            "ListingMatched",
        );
        let RuntimeEvent::ListingMatched(matched) = matched else {
            unreachable!()
        };
        matched.alert_ids[0]
    }

    /// 收集这次点击之后的每一句 `HideoutResult`,直到出现一个"结局"。
    fn hideout_outcomes(
        handle: &RuntimeHandle,
        seen: &mut Vec<RuntimeEvent>,
        alert_id: i64,
    ) -> Vec<HideoutOutcome> {
        handle
            .try_send(RuntimeCommand::TravelToHideout { alert_id })
            .unwrap();
        wait_for(
            handle,
            seen,
            |event| {
                matches!(event, RuntimeEvent::HideoutResult { outcome, .. }
                    if outcome.is_final())
            },
            "a final HideoutResult",
        );
        seen.iter()
            .filter_map(|event| match event {
                RuntimeEvent::HideoutResult { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .collect()
    }

    /// 一份"能去藏身处"的设置:有会话,但不开 live(这几个测试不需要 WS,
    /// 而生产的连接器会真的去连网)。
    fn hideout_settings() -> AppSettings {
        let mut settings = settings();
        settings.poesessid = "cookie".to_string();
        settings.watches[0].live = false;
        settings
    }

    /// token 是刚刚 fetch 回来的(还不到 10 分钟):直接发,不多跑一次 fetch。
    #[test]
    fn a_fresh_token_travels_without_refetching() {
        let (transport, log) = FakeTrade::new();
        log.lock().unwrap().hideout_token = Some("tok-fresh".to_string());
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let fetches_before = log.lock().unwrap().fetches.len();
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(outcomes, vec![HideoutOutcome::Sent], "{seen:#?}");
        let log = log.lock().unwrap();
        assert_eq!(log.whispers, vec!["tok-fresh".to_string()]);
        assert_eq!(
            log.fetches.len(),
            fetches_before,
            "token 还新,不该再 fetch 一次"
        );
    }

    /// 503 = token 过期:重新 fetch 一次拿新的,再发一次,成了。
    /// 一次点击最多两次 POST,这个测试也是那条纪律的证据。
    #[test]
    fn a_503_refreshes_the_token_and_retries_exactly_once() {
        let (transport, log) = FakeTrade::new();
        {
            let mut log = log.lock().unwrap();
            log.hideout_token = Some("tok".to_string());
            log.whisper_statuses.push_back(503);
            log.whisper_statuses.push_back(200);
        }
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let fetches_before = log.lock().unwrap().fetches.len();
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(
            outcomes,
            vec![HideoutOutcome::Refreshed, HideoutOutcome::Sent],
            "{seen:#?}"
        );
        let log = log.lock().unwrap();
        assert_eq!(log.whispers.len(), 2, "刷新之后正好再试一次");
        assert_eq!(
            log.fetches.len(),
            fetches_before + 1,
            "刷新 token 只该多打一次 fetch"
        );
    }

    /// 换过 token 还是 503:停手。绝不会有第三次 POST。
    #[test]
    fn two_503s_give_up_and_never_post_a_third_time() {
        let (transport, log) = FakeTrade::new();
        {
            let mut log = log.lock().unwrap();
            log.hideout_token = Some("tok".to_string());
            log.whisper_statuses.extend([503, 503, 503, 503]);
            log.whisper_body = Some(r#"{"error":{"code":8}}"#.to_string());
        }
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(outcomes[0], HideoutOutcome::Refreshed);
        let HideoutOutcome::Failed { status, message } = &outcomes[1] else {
            panic!("expected a Failed, got {outcomes:#?}");
        };
        assert_eq!(*status, 503);
        // 换过 token 还是 503:该给的是下一步能做的事(去看自己的游戏客户端),
        // 而不是干巴巴一句"被拒了两次",更不是甩锅给"卖家不在线"。
        assert!(message.contains("both travel requests"), "{message}");
        assert!(message.contains("your own game client"), "{message}");
        assert!(message.contains("town or hideout"), "{message}");
        assert!(!message.contains("not online"), "{message}");
        assert!(message.contains(r#"{"error":{"code":8}}"#), "{message}");
        // 再点一次也还是最多两次:这是"每次点击"的账,不是"每条提醒"的账。
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            log.lock().unwrap().whispers.len(),
            2,
            "第三次 POST 不该存在"
        );
    }

    /// 401/403 和 503 一样,可能只是"这张 token 过期了"—— 2026-09-07 那次
    /// 真跑里,一条离线卖家的挂单点下去就是个非 HTML 的 403。所以这两个码
    /// 也走"换一张再试一次",而且仍然只换一次、只再试一次。
    #[test]
    fn a_401_or_403_refreshes_the_token_and_retries_exactly_once() {
        for refused in [401u16, 403] {
            let (transport, log) = FakeTrade::new();
            {
                let mut log = log.lock().unwrap();
                log.hideout_token = Some("tok".to_string());
                log.whisper_statuses.push_back(refused);
                log.whisper_statuses.push_back(200);
            }
            let handle = RuntimeHandle::start_offline(
                hideout_settings(),
                RuntimePaths::in_memory(),
                transport,
            )
            .unwrap();

            let mut seen = Vec::new();
            let alert_id = first_alert(&handle, &mut seen);
            let fetches_before = log.lock().unwrap().fetches.len();
            let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

            assert_eq!(
                outcomes,
                vec![HideoutOutcome::Refreshed, HideoutOutcome::Sent],
                "{refused}: {seen:#?}"
            );
            let log = log.lock().unwrap();
            assert_eq!(log.whispers.len(), 2, "{refused}: 刷新之后正好再试一次");
            assert_eq!(
                log.fetches.len(),
                fetches_before + 1,
                "{refused}: 刷新 token 只该多打一次 fetch"
            );
        }
    }

    /// 换过 token 还是 403:停手。而且那句话不能让人去看自己站在哪儿
    /// (那是 503 的事),也不能在会话明明还好好的时候甩锅给 cookie ——
    /// 这一路上没有任何一个响应说过会话不好,所以 `session_ok` 还是 true。
    #[test]
    fn two_403s_give_up_and_never_post_a_third_time() {
        let (transport, log) = FakeTrade::new();
        {
            let mut log = log.lock().unwrap();
            log.hideout_token = Some("tok".to_string());
            log.whisper_statuses.extend([403, 403, 403, 403]);
            log.whisper_body = Some(r#"{"error":{"code":6,"message":"Forbidden"}}"#.to_string());
        }
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(outcomes[0], HideoutOutcome::Refreshed);
        let HideoutOutcome::Failed { status, message } = &outcomes[1] else {
            panic!("expected a Failed, got {outcomes:#?}");
        };
        assert_eq!(*status, 403);
        // 服务端说的那句话排在最前面:后面的模板被裁掉也没关系,它不能。
        assert!(message.starts_with("GGG error 6: Forbidden"), "{message}");
        assert!(message.contains("both travel requests"), "{message}");
        // 会话是好的,所以这句话要指向"这封请求被当成不是从网站发的",
        // 而不是让人一遍遍去换 POESESSID —— 换多少个都没用。
        assert!(
            message.contains("the session itself still works"),
            "{message}"
        );
        assert!(message.contains("not coming from the website"), "{message}");
        assert!(
            !message.contains("no longer valid"),
            "会话没坏就别说它坏了:{message}"
        );
        assert!(
            !message.contains("town or hideout"),
            "403 不是'你不在城里':{message}"
        );
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            log.lock().unwrap().whispers.len(),
            2,
            "第三次 POST 不该存在"
        );
    }

    /// 交易站的报错正文认得出来就提到句首。理由很实在:卡片脚注和状态行
    /// 都会被裁短,裁掉的该是模板,不是唯一有信息量的那半句。
    #[test]
    fn a_ggg_error_body_leads_the_sentence() {
        let refused = hideout_failure(
            HideoutStep::Whisper,
            "listing-9",
            &GatewayError::Status {
                status: 403,
                excerpt: r#"{"error":{"code":6,"message":"Forbidden"}}"#.to_string(),
            },
        );
        let HideoutOutcome::Failed { status, message } = refused else {
            unreachable!()
        };
        assert_eq!(status, 403);
        assert!(
            message.starts_with("GGG error 6: Forbidden — "),
            "{message}"
        );
        assert!(message.contains("listing-9"), "{message}");
        assert!(
            !message.contains(r#"{"error""#),
            "翻译过就别在句尾再挂一遍原文:{message}"
        );

        // 认不出来的 body 一个字都不丢,还是原样挂在句尾。
        let raw = hideout_failure(
            HideoutStep::Whisper,
            "listing-9",
            &GatewayError::Status {
                status: 500,
                excerpt: "upstream exploded".to_string(),
            },
        );
        let HideoutOutcome::Failed { message, .. } = raw else {
            unreachable!()
        };
        assert!(message.ends_with(": upstream exploded"), "{message}");
    }

    /// 401/403 这一句只可能从"重新 fetch 拿新 token"那一步走过来 ——
    /// POST 上的 401/403 在 [`looks_like_a_stale_token`] 那里就被截走去换
    /// token 了,走不到 [`hideout_failure`]。所以这句话必须指名道姓说
    /// "refetch":含含糊糊说成"the travel request"会让人以为是那封 POST
    /// 被会话拒了,而那封 POST 根本还没发出去。
    #[test]
    fn a_refused_refetch_says_refetch_not_travel_request() {
        let HideoutOutcome::Failed { status, message } = hideout_failure(
            HideoutStep::Refetch,
            "listing-9",
            &GatewayError::Status {
                status: 403,
                excerpt: String::new(),
            },
        ) else {
            unreachable!()
        };
        assert_eq!(status, 403);
        assert_eq!(
            message,
            "the refetch for listing listing-9 was refused with HTTP 403 — \
             the POESESSID is probably no longer valid"
        );
    }

    /// 最后那句建议要看会话的死活。三条路各说各的:
    /// 会话还好好的 403 说"这封请求被当成不是从网站发的",会话确实被拒过的
    /// 403 才说"换个 cookie",503 谁都不提、只让人看自己的游戏客户端。
    #[test]
    fn the_last_word_on_a_403_depends_on_whether_the_session_still_works() {
        let message = |status, session_ok| {
            let HideoutOutcome::Failed { message, .. } =
                hideout_retry_exhausted(status, "listing-9", "", session_ok)
            else {
                unreachable!()
            };
            message
        };

        // 会话是好的:别让人去换一个换了也没用的 cookie。
        let healthy = message(403, true);
        assert!(
            healthy.contains("the session itself still works"),
            "{healthy}"
        );
        assert!(healthy.contains("not coming from the website"), "{healthy}");
        assert!(!healthy.contains("no longer valid"), "{healthy}");

        // 会话真的被拒过了:那句老建议还是对的。
        let dead = message(403, false);
        assert!(
            dead.ends_with(
                "a fresh token did not help, so the POESESSID is probably \
                 no longer valid — paste a fresh one into settings"
            ),
            "{dead}"
        );
        assert_eq!(message(401, false), dead.replace("403", "401"));

        // 503 和会话没关系,两种状态下说的都是同一句。
        let offline = message(503, true);
        assert!(
            offline.contains("standing in a town or hideout"),
            "{offline}"
        );
        assert!(!offline.contains("POESESSID"), "{offline}");
        assert_eq!(message(503, false), offline);
    }

    /// 事件流里的每一句日志。失败的原因走这条路 —— 卡片脚注只放得下
    /// 一个状态码。
    fn logs(seen: &[RuntimeEvent]) -> Vec<String> {
        seen.iter()
            .filter_map(|event| match event {
                RuntimeEvent::Log(message) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// 这批日志里有没有哪一句同时含这几个词。
    fn logged(seen: &[RuntimeEvent], parts: &[&str]) -> bool {
        logs(seen)
            .iter()
            .any(|line| parts.iter().all(|part| line.contains(part)))
    }

    /// 挂单上根本没有 token(当初是匿名 fetch 回来的):重新抓一次也没有,
    /// 老实说一句,别发一个必然被拒的请求。
    #[test]
    fn a_listing_without_a_token_says_so_instead_of_guessing() {
        let (transport, log) = FakeTrade::new();
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(
            outcomes,
            vec![HideoutOutcome::Refreshed, HideoutOutcome::TokenMissing],
            "{seen:#?}"
        );
        assert!(log.lock().unwrap().whispers.is_empty(), "没 token 就别发");
        // `TokenMissing` 上没有消息字段,所以"为什么"必须出现在日志里,
        // 而且要说清是哪一件货。
        assert!(
            logged(
                &seen,
                &["came back without a hideout_token", "without a session"]
            ),
            "{:#?}",
            logs(&seen)
        );
    }

    /// 卖家显示离线,但挂单带着 token:即刻购买(instant buyout)的货就摆在
    /// 人家藏身处的商店里,官网对十几个小时没上线的卖家照样给"去藏身处"。
    /// 所以在不在线不作数,有 token 就发 —— 而且只发这一次。
    #[test]
    fn an_offline_seller_with_a_token_still_travels() {
        let (transport, log) = FakeTrade::new();
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        {
            // 重新抓回来的那一条:有 token,但人显示已经下线了。
            let mut log = log.lock().unwrap();
            log.hideout_token = Some("tok".to_string());
            log.seller_offline = true;
        }
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(
            outcomes,
            vec![HideoutOutcome::Refreshed, HideoutOutcome::Sent],
            "{seen:#?}"
        );
        assert_eq!(
            log.lock().unwrap().whispers,
            vec!["tok".to_string()],
            "有 token 就该正好发一次"
        );
    }

    /// 反过来:人在线也救不回一个不存在的 token。有没有 token 是唯一的判据,
    /// 所以这一条必须还是 `TokenMissing`,而且一个请求都不发。
    #[test]
    fn an_online_seller_without_a_token_is_still_a_missing_token() {
        let (transport, log) = FakeTrade::new();
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        {
            let mut log = log.lock().unwrap();
            log.hideout_token = None;
            log.seller_offline = false;
        }
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(
            outcomes,
            vec![HideoutOutcome::Refreshed, HideoutOutcome::TokenMissing],
            "{seen:#?}"
        );
        assert!(log.lock().unwrap().whispers.is_empty(), "没 token 就别发");
        assert!(
            logged(
                &seen,
                &["came back without a hideout_token", "instant-buyout"]
            ),
            "{:#?}",
            logs(&seen)
        );
    }

    /// 重新抓的时候那件货已经被买走了:说"没了",不是说"失败(0)"。
    #[test]
    fn a_listing_that_vanished_before_the_refetch_says_it_is_gone() {
        let (transport, log) = FakeTrade::new();
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        log.lock().unwrap().fetch_returns_nothing = true;
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(
            outcomes,
            vec![HideoutOutcome::Refreshed, HideoutOutcome::TokenMissing],
            "{seen:#?}"
        );
        assert!(
            logged(&seen, &["came back without it", "sold or delisted"]),
            "{:#?}",
            logs(&seen)
        );
    }

    /// whisper 被拒了(JSON 的 403):状态码要是真的 403,消息里要有
    /// 服务端 body 的那句话 —— 早先这两样都丢了,只剩一个 `失败(0)`。
    ///
    /// 两次都回 403:第一次会换 token 再试,所以要连着拒两次才走到结局。
    #[test]
    fn a_refused_whisper_keeps_the_real_status_and_the_body() {
        let (transport, log) = FakeTrade::new();
        {
            let mut log = log.lock().unwrap();
            // 故意不叫 "tok":那三个字母是英文单词 token 的前缀,
            // 用它去查"token 有没有泄进消息里"永远查不准。
            log.hideout_token = Some("secret-jwt-value".to_string());
            log.whisper_statuses.extend([403, 403]);
            log.whisper_body = Some(r#"{"error":{"code":6,"message":"Forbidden"}}"#.to_string());
        }
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        let HideoutOutcome::Failed { status, message } = outcomes.last().unwrap() else {
            panic!("expected a Failed, got {outcomes:#?}");
        };
        assert_eq!(*status, 403, "状态码不能被抹成 0");
        // 这一路上会话一次都没被拒过,所以最后那句建议说的是请求本身,
        // 不是 cookie(见 `the_last_word_on_a_403_...`)。
        assert!(message.contains("not coming from the website"), "{message}");
        assert!(message.contains("Forbidden"), "body 那句话要带上:{message}");
        assert!(
            !message.contains("secret-jwt-value"),
            "token 不许出现在消息里:{message}"
        );
        assert_eq!(
            log.lock().unwrap().whispers.len(),
            2,
            "换一次 token 再试一次,就这两次"
        );
        assert!(logged(&seen, &["failed (HTTP 403)"]), "{:#?}", logs(&seen));
    }

    /// whisper 撞上 Cloudflare 的拦截页(403 + HTML):这不是"会话坏了",
    /// 是整条队列被拦住了,消息必须说的是这件事。
    #[test]
    fn a_cloudflare_page_on_the_whisper_says_cloudflare() {
        let (transport, log) = FakeTrade::new();
        {
            let mut log = log.lock().unwrap();
            log.hideout_token = Some("tok".to_string());
            log.whisper_statuses.push_back(403);
            log.whisper_body = Some("<!DOCTYPE html><html>Just a moment…</html>".to_string());
            log.whisper_html = true;
        }
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        let HideoutOutcome::Failed { status, message } = outcomes.last().unwrap() else {
            panic!("expected a Failed, got {outcomes:#?}");
        };
        // 请求确实发出去了,但回来的不是交易站 —— 状态码在这里没有意义。
        assert_eq!(*status, 0);
        assert!(message.contains("Cloudflare"), "{message}");
        assert!(message.contains("5 minutes"), "{message}");
        assert!(
            logged(&seen, &["failed (no HTTP answer)", "Cloudflare"]),
            "{:#?}",
            logs(&seen)
        );
        // 403 走"换 token 再试一次",但 **HTML 的** 403 不走:那不是交易站
        // 在拒绝我们,是根本没到交易站。换十张新 token 也一样被拦。
        assert_eq!(
            log.lock().unwrap().whispers.len(),
            1,
            "Cloudflare 的 403 不该重试"
        );
    }

    /// 请求压根没上过网(断网、TLS 挂了):状态码是 0,但消息要说明白
    /// "没发出去",还要把传输层那句话带上。
    #[test]
    fn a_whisper_that_never_left_the_machine_says_exactly_that() {
        let (transport, log) = FakeTrade::new();
        {
            let mut log = log.lock().unwrap();
            log.hideout_token = Some("tok".to_string());
            log.whisper_error = Some("connection reset by peer".to_string());
        }
        let handle =
            RuntimeHandle::start_offline(hideout_settings(), RuntimePaths::in_memory(), transport)
                .unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        let HideoutOutcome::Failed { status, message } = outcomes.last().unwrap() else {
            panic!("expected a Failed, got {outcomes:#?}");
        };
        assert_eq!(*status, 0);
        assert!(message.contains("never left this machine"), "{message}");
        assert!(message.contains("connection reset by peer"), "{message}");
    }

    /// 每一种网关失败都得有一句自己的话。这是那条"状态码 0 底下藏着五种
    /// 完全不同的原因"的清单,少一条就意味着界面上又会出现一个说不清的
    /// `失败(0)`。
    #[test]
    fn every_gateway_failure_gets_its_own_sentence() {
        let cases = [
            (
                GatewayError::Transport("dns failed".to_string()),
                0,
                vec!["never left this machine", "dns failed"],
            ),
            (
                GatewayError::NoSession,
                0,
                vec!["needs a POESESSID", "paste a fresh one"],
            ),
            (
                GatewayError::Parse("missing `result`".to_string()),
                0,
                vec!["could not be read", "missing `result`"],
            ),
            (
                GatewayError::CloudflareHold,
                0,
                vec!["Cloudflare", "5 minutes"],
            ),
            (GatewayError::Cancelled, 0, vec!["shut down"]),
            (
                GatewayError::Status {
                    status: 404,
                    excerpt: "not found".to_string(),
                },
                404,
                vec!["is gone", "404", "not found"],
            ),
            (
                GatewayError::Status {
                    status: 500,
                    excerpt: "boom".to_string(),
                },
                500,
                vec!["500", "boom"],
            ),
            // 401/403 不在这张表里:那一句写死了"the refetch"(它只可能从
            // 重新 fetch 那一步走过来),所以过不了下面"哪一步都得说"的检查。
            // 它由 `a_refused_refetch_says_refetch_not_travel_request` 单独钉。
        ];

        for (error, want_status, wants) in cases {
            for step in [HideoutStep::Refetch, HideoutStep::Whisper] {
                let outcome = hideout_failure(step, "listing-9", &error);
                let HideoutOutcome::Failed { status, message } = outcome else {
                    panic!("{error:?} 该是一个 Failed");
                };
                assert_eq!(status, want_status, "{error:?}");
                assert!(message.contains("listing-9"), "哪一件货都没说:{message}");
                assert!(message.contains(step.label()), "哪一步都没说:{message}");
                for want in &wants {
                    assert!(message.contains(want), "{error:?} 少了 {want:?}:{message}");
                }
            }
        }

        // body 是空的就不该多一个孤零零的冒号。
        let quiet = hideout_failure(
            HideoutStep::Whisper,
            "x",
            &GatewayError::Status {
                status: 500,
                excerpt: String::new(),
            },
        );
        let HideoutOutcome::Failed { message, .. } = quiet else {
            unreachable!()
        };
        assert!(message.ends_with("listing x"), "{message}");
    }

    /// 没有会话时点"去藏身处":一句话说清楚,一个请求都不发。
    #[test]
    fn travelling_without_a_session_asks_for_one() {
        let (transport, log) = FakeTrade::new();
        let handle =
            RuntimeHandle::start_offline(settings(), RuntimePaths::in_memory(), transport).unwrap();

        let mut seen = Vec::new();
        let alert_id = first_alert(&handle, &mut seen);
        let outcomes = hideout_outcomes(&handle, &mut seen, alert_id);

        assert_eq!(outcomes, vec![HideoutOutcome::NoSession]);
        assert!(log.lock().unwrap().whispers.is_empty());
    }

    /// 造一张"在 `exp` 这一刻作废"的 JWT。头和载荷是真的 base64url JSON,
    /// 签名那一段是占位符 —— 我们从来不看它。
    fn jwt_expiring_at(exp: i64) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        fn encode(bytes: &[u8]) -> String {
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for i in 0..chunk.len() + 1 {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                }
            }
            out
        }
        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"HS256","typ":"JWT"}"#),
            encode(format!(r#"{{"exp":{exp}}}"#).as_bytes()),
            "c2lnbmF0dXJl"
        )
    }

    /// token 自己说了什么时候作废,就该听它的 —— "才拿到一分钟"救不了一张
    /// 五秒后就过期的票。这是那次 403 的第一个嫌疑:真实 TTL 比我们那条
    /// 10 分钟的规矩短。
    #[test]
    fn a_token_expiring_in_five_seconds_is_refetched_even_though_it_is_a_minute_old() {
        let now = 1_757_260_800;
        let a_minute_ago = Some(now - 60);

        assert_eq!(
            usable_token(Some(&jwt_expiring_at(now + 5)), a_minute_ago, now),
            None,
            "五秒后就作废,发出去也是白发"
        );
        assert!(
            usable_token(Some(&jwt_expiring_at(now + 600)), a_minute_ago, now).is_some(),
            "还有十分钟,直接发"
        );
        // 30 秒的余量是给路上留的(排队 + TLS 握手 + 两头的时钟差):
        // 卡在边上的一律先换一张。
        assert_eq!(
            usable_token(Some(&jwt_expiring_at(now + 30)), a_minute_ago, now),
            None
        );
        assert!(usable_token(Some(&jwt_expiring_at(now + 31)), a_minute_ago, now).is_some());
        assert_eq!(
            usable_token(Some(&jwt_expiring_at(now - 1)), a_minute_ago, now),
            None,
            "已经过期了"
        );
        // exp 说了算:连"什么时候拿到的"都不需要知道。
        assert!(usable_token(Some(&jwt_expiring_at(now + 600)), None, now).is_some());
        // 反过来也一样 —— 半小时前拿的,但它说自己还有十分钟,那就照发。
        assert!(usable_token(Some(&jwt_expiring_at(now + 600)), Some(now - 1_800), now).is_some());
    }

    #[test]
    fn a_token_without_exp_older_than_the_five_minute_ttl_is_not_used() {
        let now = 10_000;
        assert_eq!(
            usable_token(Some("tok"), Some(now - 269), now),
            Some("tok".to_string())
        );
        assert_eq!(
            usable_token(Some("tok"), Some(now - 271), now),
            None,
            "太老了"
        );
        assert_eq!(usable_token(None, Some(now), now), None);
        assert_eq!(
            usable_token(Some("  "), Some(now), now),
            None,
            "空 token 不算"
        );
        assert_eq!(
            usable_token(Some("tok"), None, now),
            None,
            "不知道什么时候拿的,就当它过期"
        );
    }

    // ---- 测试会话 ----------------------------------------------------

    /// 一份"有 cookie、没有搜索"的设置。没有搜索就没有轮询,于是这几个
    /// 测试里发出去的唯一一封请求就是那次会话检查。
    fn session_settings(rules: &str) -> (AppSettings, Box<FakeTrade>, Arc<Mutex<TradeLog>>) {
        let mut settings = settings();
        settings.watches.clear();
        settings.poesessid = "cookie".to_string();
        let (transport, log) = FakeTrade::new();
        log.lock().unwrap().rate_rules = Some(rules.to_string());
        (settings, transport, log)
    }

    fn session_result(handle: &RuntimeHandle, seen: &mut Vec<RuntimeEvent>) -> (bool, String) {
        handle.try_send(RuntimeCommand::TestSession).unwrap();
        let event = wait_for(
            handle,
            seen,
            |event| matches!(event, RuntimeEvent::SessionChecked { .. }),
            "a SessionChecked",
        );
        let RuntimeEvent::SessionChecked { valid, detail } = event else {
            unreachable!()
        };
        (valid, detail)
    }

    /// 服务端按 `Account` 限速 = 它认出了这个 cookie。
    #[test]
    fn a_session_check_passes_when_the_rules_mention_account() {
        let (settings, transport, log) = session_settings("Ip,Account");
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::in_memory(), transport).unwrap();

        let mut seen = Vec::new();
        wait_for(&handle, &mut seen, |e| *e == RuntimeEvent::Ready, "Ready");
        let (valid, detail) = session_result(&handle, &mut seen);

        assert!(valid, "{detail}");
        assert!(detail.contains("Account"), "{detail}");
        assert_eq!(
            log.lock().unwrap().searches,
            1,
            "一次点击只该花掉一次 search 额度"
        );
    }

    /// 带着 cookie 发出去,回来的规则里只有 `Ip`:服务端根本没认出这个会话。
    #[test]
    fn a_session_check_fails_when_the_cookie_is_not_recognised() {
        let (settings, transport, log) = session_settings("Ip");
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::in_memory(), transport).unwrap();

        let mut seen = Vec::new();
        wait_for(&handle, &mut seen, |e| *e == RuntimeEvent::Ready, "Ready");
        let (valid, detail) = session_result(&handle, &mut seen);

        assert!(!valid, "{detail}");
        assert!(detail.contains("Ip"), "{detail}");
        assert_eq!(log.lock().unwrap().searches, 1);
    }

    /// 设置里根本没有 cookie:一句话说清楚,一个请求都不发。
    #[test]
    fn a_session_check_without_a_cookie_never_leaves_the_house() {
        let (transport, log) = FakeTrade::new();
        let mut settings = settings();
        settings.watches.clear();
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::in_memory(), transport).unwrap();

        let mut seen = Vec::new();
        wait_for(&handle, &mut seen, |e| *e == RuntimeEvent::Ready, "Ready");
        let (valid, detail) = session_result(&handle, &mut seen);

        assert!(!valid);
        assert!(detail.contains("POESESSID"), "{detail}");
        assert_eq!(log.lock().unwrap().searches, 0, "没 cookie 就别发请求");
    }

    /// 有搜索就借它的查询;一条都没有才用那件"在线的 Divine Orb"。
    #[test]
    fn the_session_check_borrows_the_first_enabled_watch() {
        let watches = vec![
            WatchEntry {
                id: WatchId("off".to_string()),
                league: "Standard".to_string(),
                enabled: false,
                ..WatchEntry::default()
            },
            WatchEntry {
                id: WatchId("on".to_string()),
                league: "Forbidden Rites".to_string(),
                ..WatchEntry::default()
            },
        ];
        let bodies = |id: &WatchId| (id.as_str() == "on").then(|| "{\"query\":1}".to_string());

        let (league, body) = session_check_request(&watches, "Whatever", bodies);
        assert_eq!(league, "Forbidden Rites", "停用的那条不算");
        assert_eq!(body, "{\"query\":1}");

        // 一条搜索都没有:退回探针查询,联赛用设置里那个。
        let (league, body) = session_check_request(&[], "Forbidden Rites", |_| None);
        assert_eq!(league, "Forbidden Rites");
        assert_eq!(body, probe_body());
        // 搜索还没解开(粘错了的 id)时也一样。
        let (_, body) = session_check_request(&watches, "Forbidden Rites", |_| None);
        assert_eq!(body, probe_body());
    }

    /// 探针查询就是计划里写死的那一句,不多不少。
    #[test]
    fn the_probe_query_is_one_online_divine_orb() {
        assert_eq!(
            probe_body(),
            r#"{"query":{"status":{"option":"online"},"type":"Divine Orb"},"sort":{"price":"asc"}}"#
        );
    }

    /// 结论只看规则名;请求压根没发出去的时候,照实说是哪种失败。
    #[test]
    fn the_session_report_reads_the_rules_and_never_guesses() {
        let outcome = |rules: &[&str], mentions_account: bool| {
            Ok(SessionCheckOutcome {
                status: 200,
                rules: rules.iter().map(|rule| (*rule).to_string()).collect(),
                mentions_account,
            })
        };

        let (valid, detail) = session_check_report(&outcome(&["Ip", "Account"], true));
        assert!(valid);
        assert_eq!(detail, "HTTP 200 · rules: Ip, Account");

        let (valid, detail) = session_check_report(&outcome(&["Ip"], false));
        assert!(!valid);
        assert_eq!(detail, "HTTP 200 · rules: Ip");

        // 没有限速头 = 这封响应根本不是交易站的限速接口回的。
        let (valid, detail) = session_check_report(&outcome(&[], false));
        assert!(!valid);
        assert_eq!(detail, "HTTP 200 · no rate-limit rules");

        let (valid, detail) =
            session_check_report(&Err(GatewayError::Transport("dns failed".to_string())));
        assert!(!valid);
        assert!(detail.contains("dns failed"), "{detail}");
    }

    #[test]
    fn the_live_connection_cap_honours_both_ceilings() {
        let mut settings = AppSettings::default();
        assert_eq!(live_connection_cap(&settings), 5, "计划里的温柔阈值");
        settings.watcher.max_live_connections = 100;
        assert_eq!(
            live_connection_cap(&settings),
            MAX_LIVE_CONNECTIONS_PER_ACCOUNT,
            "服务端的硬顶改不了"
        );
        settings.watcher.max_live_connections = 0;
        assert_eq!(live_connection_cap(&settings), 0, "0 就是一条都不开");
    }

    /// 超过上限的那几条搜索要说清楚"是被挤下来的",不是"坏了"。
    #[test]
    fn watches_over_the_cap_are_disabled_with_a_reason() {
        let (transport, _log) = FakeTrade::new();
        let connector = Arc::new(ScriptedConnector::new());
        let mut settings = live_settings();
        settings.watcher.max_live_connections = 1;
        settings.watches.push(WatchEntry {
            id: WatchId("w2".to_string()),
            ..settings.watches[0].clone()
        });

        let handle = RuntimeHandle::start_offline_with_connector(
            settings,
            RuntimePaths::in_memory(),
            transport,
            shared(&connector),
        )
        .unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { watch_id, status }
                    if watch_id.as_str() == "w2"
                        && status.live == LiveRunState::Disabled(LiveOffReason::TooMany))
            },
            "w2 to be pushed out by the cap",
        );
        assert_eq!(connector.connects(), 1, "只开了一条");
    }

    // ---- 市场观察 ----------------------------------------------------

    /// 一份只有一条市场观察、一条蹲价搜索都没有的设置。
    fn observe_settings(label: &str) -> (AppSettings, ObservationId) {
        let search = SearchRef {
            league: "Forbidden Rites".to_string(),
            search_id: FIXTURE_ID.to_string(),
        };
        let entry = ObservationEntry::new(label, &search);
        let obs_id = entry.id.clone();
        let settings = AppSettings {
            league: "Forbidden Rites".to_string(),
            observations: vec![entry],
            ..AppSettings::default()
        };
        (settings, obs_id)
    }

    /// 观察的测试要在 actor 走掉之后自己读一遍库,所以库开在临时文件上 ——
    /// 内存库跟着 actor 那个连接一起消失,外面读不到。
    fn temp_db(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("pnd-observe-{name}-{}.sqlite", std::process::id()));
        remove_db(&path);
        path
    }

    fn remove_db(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut with_suffix = path.to_path_buf().into_os_string();
            with_suffix.push(suffix);
            let _ = std::fs::remove_file(with_suffix);
        }
    }

    /// 等到假交易站收够了这么多次 fetch(或者超时)。
    fn wait_for_fetches(
        handle: &RuntimeHandle,
        seen: &mut Vec<RuntimeEvent>,
        log: &Arc<Mutex<TradeLog>>,
        wanted: usize,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while log.lock().unwrap().fetches.len() < wanted && Instant::now() < deadline {
            match handle.try_next_event() {
                Some(event) => seen.push(event),
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        // 回信路上还有几个事件,一并抽干,免得下一个断言看的是半截状态。
        thread::sleep(Duration::from_millis(200));
        while let Some(event) = handle.try_next_event() {
            seen.push(event);
        }
        assert_eq!(
            log.lock().unwrap().fetches.len(),
            wanted,
            "等不到第 {wanted} 次 fetch:{seen:#?}"
        );
    }

    /// 第一轮 discover:排序换成上架时间倒序,新面孔连同物品原文和词缀一起入库。
    #[test]
    fn an_observation_stores_new_listings_with_their_item_json_and_mods() {
        let (transport, log) = FakeTrade::new();
        log.lock().unwrap().item_mods = vec!["+115 to maximum Life".to_string()];
        let (settings, obs_id) = observe_settings("Choir of the Storm");
        let db = temp_db("stores");
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::new(db.clone()), transport)
                .unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| matches!(event, RuntimeEvent::ObservationChanged { .. }),
            "the first discover to land",
        );
        drop(handle);

        // 观察要的是"最新挂上来的 100 条",不是"最便宜的 100 条"。
        let body = log.lock().unwrap().search_bodies[0].clone();
        assert!(
            body.contains(r#""sort":{"indexed":"desc"}"#),
            "discover 的排序不对:{body}"
        );

        let store = WatchStore::open(&db).expect("open");
        assert_eq!(store.active_listing_ids(&obs_id).unwrap(), ["one", "two"]);
        let row = store
            .observed_listing(&obs_id, "one")
            .unwrap()
            .expect("row");
        assert_eq!(row.first_price, Some(Price::new(18_000, Currency::Divine)));
        assert_eq!(row.last_price, row.first_price);
        assert_eq!(row.price_changes, 0);
        // 整块物品原文留着 —— 以后想统计别的(ilvl、底子)不用重抓一遍。
        let item: serde_json::Value = serde_json::from_str(&row.item_json).expect("item json");
        assert_eq!(item["name"], "Choir of the Storm");
        assert_eq!(
            item["explicitMods"][0]["description"],
            "+115 to maximum Life"
        );

        let mods = store.observed_mods(&obs_id, "one").unwrap();
        assert_eq!(mods.len(), 1);
        assert_eq!(mods[0].mod_kind, "explicit");
        assert_eq!(mods[0].template, "+# to maximum Life");
        assert_eq!(mods[0].value1, Some(115.0));

        // 聚合表当场就能出:一条词缀、两件货、一件都没卖掉。
        let aggregate = store.mod_aggregate(&obs_id, 1).unwrap();
        assert_eq!(aggregate.len(), 1);
        assert_eq!(aggregate[0].seen, 2);
        assert_eq!(aggregate[0].gone, 0);
        remove_db(&db);
    }

    /// 第二轮 discover 只抓没见过的 id。
    ///
    /// 搜索回来的 100 个 id 里绝大多数上一轮就见过了,再抓一遍就是把 fetch
    /// 额度烧在"我已经知道的事"上;它们出现在搜索结果里本身就说明还挂着,
    /// 推一下 `last_seen` 就够了。
    #[test]
    fn a_second_discover_only_fetches_ids_it_has_not_seen() {
        let (transport, log) = FakeTrade::new();
        let (settings, obs_id) = observe_settings("Choir of the Storm");
        let db = temp_db("unknown-only");
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::new(db.clone()), transport)
                .unwrap();

        let mut seen = Vec::new();
        wait_for_fetches(&handle, &mut seen, &log, 1);
        assert_eq!(log.lock().unwrap().fetches[0], ["one", "two"]);

        // 同样两条 id 再 discover 一次。
        handle
            .try_send(RuntimeCommand::DiscoverNow {
                obs_id: obs_id.clone(),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while log.lock().unwrap().searches < 2 && Instant::now() < deadline {
            match handle.try_next_event() {
                Some(event) => seen.push(event),
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        assert_eq!(log.lock().unwrap().searches, 2, "第二轮 discover 没跑起来");
        thread::sleep(Duration::from_millis(300));
        while let Some(event) = handle.try_next_event() {
            seen.push(event);
        }
        drop(handle);

        assert_eq!(
            log.lock().unwrap().fetches.len(),
            1,
            "见过的 id 不该再抓一次:{seen:#?}"
        );
        let store = WatchStore::open(&db).expect("open");
        assert_eq!(store.active_listing_ids(&obs_id).unwrap().len(), 2);
        remove_db(&db);
    }

    /// 回查时那一格是 `null` = 这条挂单没了,判定要跟着写上。
    ///
    /// 这里判出来的是 `Unknown`,而且那是**对的**:这件货我们只见过一次
    /// (第一次 discover),看得见的存活时间是 0 秒,连"它到底挂了多久"都
    /// 回答不了。三档判定各自的边界在 `pnd_domain::classify_gone` 的测试里钉着,
    /// 这一条钉的是"actor 真的把 null 认成了没了、真的调了判定"。
    #[test]
    fn a_null_on_a_recheck_marks_the_listing_gone() {
        let (transport, log) = FakeTrade::new();
        let (settings, obs_id) = observe_settings("Choir of the Storm");
        let db = temp_db("gone");
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::new(db.clone()), transport)
                .unwrap();

        let mut seen = Vec::new();
        wait_for_fetches(&handle, &mut seen, &log, 1);

        // "two" 被买走了:下一次问它,服务端那一格是 null。
        log.lock().unwrap().gone_ids.insert("two".to_string());
        handle
            .try_send(RuntimeCommand::RecheckNow {
                obs_id: obs_id.clone(),
            })
            .unwrap();
        wait_for_fetches(&handle, &mut seen, &log, 2);
        drop(handle);

        assert_eq!(
            log.lock().unwrap().fetches[1],
            ["one", "two"],
            "回查问的是在册的那两条"
        );
        let store = WatchStore::open(&db).expect("open");
        assert_eq!(store.active_listing_ids(&obs_id).unwrap(), ["one"]);
        let gone = store
            .observed_listing(&obs_id, "two")
            .unwrap()
            .expect("row");
        assert_eq!(gone.status, pnd_storage::ObservedStatus::Gone);
        assert!(gone.gone_at.is_some());
        assert_eq!(gone.gone_class, Some(pnd_domain::GoneClass::Unknown));

        let summary = store.observation_summary(&obs_id).unwrap();
        assert_eq!((summary.active, summary.gone, summary.unknown), (1, 1, 1));
        // 状态事件里也要看得见这一笔。
        let last = seen
            .iter()
            .rev()
            .find_map(|event| match event {
                RuntimeEvent::ObservationStatus { status, .. } => Some(status.clone()),
                _ => None,
            })
            .expect("an ObservationStatus");
        assert_eq!((last.active, last.gone), (1, 1));
        assert!(last.last_recheck_at.is_some());
        remove_db(&db);
    }

    /// 同一条挂单换了价:轨迹上多一个点,改价次数 +1,第一个价不动。
    #[test]
    fn a_price_change_on_a_recheck_appends_to_the_history() {
        let (transport, log) = FakeTrade::new();
        let (settings, obs_id) = observe_settings("Choir of the Storm");
        let db = temp_db("reprice");
        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::new(db.clone()), transport)
                .unwrap();

        let mut seen = Vec::new();
        wait_for_fetches(&handle, &mut seen, &log, 1);

        // 卖家降价到 5 divine。
        log.lock().unwrap().price_divine = Some(5);
        handle
            .try_send(RuntimeCommand::RecheckNow {
                obs_id: obs_id.clone(),
            })
            .unwrap();
        wait_for_fetches(&handle, &mut seen, &log, 2);
        drop(handle);

        let store = WatchStore::open(&db).expect("open");
        let row = store
            .observed_listing(&obs_id, "one")
            .unwrap()
            .expect("row");
        assert_eq!(row.first_price, Some(Price::new(18_000, Currency::Divine)));
        assert_eq!(row.last_price, Some(Price::new(5_000, Currency::Divine)));
        assert_eq!(row.price_changes, 1);
        let history = store.price_history(&obs_id, "one").unwrap();
        assert_eq!(
            history
                .iter()
                .map(|point| point.price.as_ref().map(|price| price.amount_milli))
                .collect::<Vec<_>>(),
            vec![Some(18_000), Some(5_000)]
        );
        remove_db(&db);
    }

    /// 观察也花搜索额度,所以预算地板必须把观察条数一起数进去。
    ///
    /// 一条搜索 + 两条观察 = 三个花搜索额度的东西,地板是 217 秒。要是只数
    /// 搜索,这条搜索会按"我一个人用整份额度"的 73 秒去跑,三边加起来正好
    /// 超掉 6 小时那一格。
    #[test]
    fn the_budget_floor_counts_observations_as_well_as_watches() {
        let (transport, _log) = FakeTrade::new();
        let mut settings = settings();
        settings.watcher.poll_interval_seconds = 60;
        let (first, _) = observe_settings("Tablets");
        let (second, _) = observe_settings("Rings");
        settings.observations = first
            .observations
            .into_iter()
            .chain(second.observations)
            .collect();

        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::in_memory(), transport).unwrap();
        let mut seen = Vec::new();
        let event = wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if status.poll_every_secs > 0)
            },
            "a watch status with an interval",
        );
        let RuntimeEvent::WatchStatus { status, .. } = event else {
            unreachable!()
        };
        assert_eq!(
            status.poll_every_secs,
            budget_floor_interval(3, 299, 21_600),
            "一条搜索 + 两条观察分同一份搜索额度"
        );
        assert_eq!(status.poll_every_secs, 217);
    }

    // ---- 观察的秒推 ----------------------------------------------------

    /// 一份"观察也能开 live"的设置:有会话,一条观察,没有蹲价搜索。
    fn observe_live_settings(label: &str) -> (AppSettings, ObservationId) {
        let (mut settings, obs_id) = observe_settings(label);
        settings.poesessid = "cookie".to_string();
        (settings, obs_id)
    }

    /// 往库里塞一条"已经见过"的挂单,时间由调用方定。
    ///
    /// 回查阶梯的测试要的是"这条挂单是什么时候第一次见到的",而那必须能
    /// 精确指定 —— 让 actor 自己跑出一条来,第一次见到的时刻就是"现在",
    /// 十分钟之内不会到点,测试就没得看了。
    fn seed_listing(store: &WatchStore, obs_id: &ObservationId, listing_id: &str, first_seen: i64) {
        let listing = ListingSummary {
            id: listing_id.to_string(),
            item_name: "Choir of the Storm".to_string(),
            type_line: "Lapis Amulet".to_string(),
            price: Some(Price::new(18_000, Currency::Divine)),
            account: "Seller".to_string(),
            character: "Char".to_string(),
            online: true,
            afk: false,
            indexed: "2026-09-06T10:00:00Z".to_string(),
            whisper: "@Char hi".to_string(),
            whisper_token: None,
            hideout_token: None,
            icon: String::new(),
            item_json: r#"{"name":"Choir of the Storm","explicitMods":["+115 to maximum Life"]}"#
                .to_string(),
        };
        store
            .record_seen(obs_id, &listing, first_seen)
            .expect("seed a listing");
    }

    /// 这一批 fetch 里,以某个前缀开头的 id 各分在了哪几批。
    fn batches_with_prefix(log: &Arc<Mutex<TradeLog>>, prefix: &str) -> Vec<Vec<String>> {
        log.lock()
            .unwrap()
            .fetches
            .iter()
            .filter(|batch| batch.iter().all(|id| id.starts_with(prefix)))
            .cloned()
            .collect()
    }

    /// 秒推是观察知道新挂单的**主要**途径:推来的 id 当场就去抓详情,
    /// 抓到的整条入库,抓不到(那一格是 null)的记成"第一眼就没了"。
    ///
    /// 后者才是这一整步的理由:一件挂上去一分钟就被买走的好价碑牌,
    /// 十分钟一次的搜索永远看不见 —— 搜索只回还活着的挂单。
    #[test]
    fn a_live_push_on_an_observation_is_fetched_and_stored() {
        let (transport, log) = FakeTrade::new();
        // 兜底那一轮什么都别找到:这个测试要看的是秒推那条路。
        log.lock().unwrap().search_ids = Some(Vec::new());
        // "live-2" 在我们去抓它的时候已经没了。
        log.lock().unwrap().gone_ids.insert("live-2".to_string());
        log.lock().unwrap().item_mods = vec!["+115 to maximum Life".to_string()];

        let connector = Arc::new(ScriptedConnector::new());
        connector.push(Step::New(vec!["live-1".to_string(), "live-2".to_string()]));
        let (settings, obs_id) = observe_live_settings("Choir of the Storm");
        let db = temp_db("live-push");
        let handle = RuntimeHandle::start_offline_with_connector(
            settings,
            RuntimePaths::new(db.clone()),
            transport,
            shared(&connector),
        )
        .unwrap();

        let mut seen = Vec::new();
        wait_for_fetches(&handle, &mut seen, &log, 1);
        drop(handle);

        assert_eq!(
            batches_with_prefix(&log, "live-"),
            vec![vec!["live-1".to_string(), "live-2".to_string()]],
            "推来的两个 id 要在同一次 fetch 里问掉"
        );

        let store = WatchStore::open(&db).expect("open");
        // 抓到的那条整条入库,词缀也在。
        let alive = store
            .observed_listing(&obs_id, "live-1")
            .unwrap()
            .expect("live-1");
        assert_eq!(alive.status, pnd_storage::ObservedStatus::Active);
        assert_eq!(
            alive.first_price,
            Some(Price::new(18_000, Currency::Divine))
        );
        assert_eq!(store.observed_mods(&obs_id, "live-1").unwrap().len(), 1);

        // 没抓到的那条:空壳一行,判定档写死"第一眼就没了"。
        let quick = store
            .observed_listing(&obs_id, "live-2")
            .unwrap()
            .expect("live-2");
        assert_eq!(quick.status, pnd_storage::ObservedStatus::Gone);
        assert_eq!(
            quick.gone_class,
            Some(pnd_domain::GoneClass::GoneBeforeFirstLook)
        );
        assert!(store.observed_mods(&obs_id, "live-2").unwrap().is_empty());

        let summary = store.observation_summary(&obs_id).unwrap();
        assert_eq!(summary.active, 1);
        assert_eq!(summary.gone_before_first_look, 1);
        assert_eq!(summary.unknown, 0);
        // 没有词缀的那一行进不了聚合表:成交率的分母不该被它顶高。
        let aggregate = store.mod_aggregate(&obs_id, 1).unwrap();
        assert_eq!(aggregate.len(), 1);
        assert_eq!(aggregate[0].seen, 1);
        remove_db(&db);
    }

    /// `sample_every = 3`:推来六条只抓两条,剩下四条**直接丢掉**。
    ///
    /// 丢掉而不是排队补抓:在挂单出生那一刻等距抽样是无偏的,而排队补抓
    /// 会系统性地漏掉卖得最快的那些(等轮到它,它早没了)——那正是要量的那批货。
    #[test]
    fn sampling_only_fetches_every_nth_pushed_listing() {
        let (transport, log) = FakeTrade::new();
        log.lock().unwrap().search_ids = Some(Vec::new());

        let connector = Arc::new(ScriptedConnector::new());
        let ids: Vec<String> = (1..=6).map(|index| format!("s-{index}")).collect();
        connector.push(Step::New(ids));
        let (mut settings, obs_id) = observe_live_settings("Choir of the Storm");
        settings.observations[0].sample_every = 3;
        let handle = RuntimeHandle::start_offline_with_connector(
            settings,
            RuntimePaths::in_memory(),
            transport,
            shared(&connector),
        )
        .unwrap();

        let mut seen = Vec::new();
        let status = wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::ObservationStatus { obs_id: id, status }
                    if *id == obs_id && status.pushed_total == 6)
            },
            "an observation status counting all six pushes",
        );
        let RuntimeEvent::ObservationStatus { status, .. } = status else {
            unreachable!()
        };
        assert_eq!(status.sampled_out_total, 4, "六条里丢掉四条");
        thread::sleep(Duration::from_millis(200));
        drop(handle);

        assert_eq!(
            batches_with_prefix(&log, "s-"),
            vec![vec!["s-3".to_string(), "s-6".to_string()]],
            "每第三条抓一条"
        );
    }

    /// 秒推记下的挂单,兜底那一轮再看见它时**不该**重抓一次。
    ///
    /// 它出现在搜索结果里本身就说明还挂着,推一下 last_seen 就够了 ——
    /// 再抓一遍是把 fetch 额度花在"我已经知道的事"上。
    #[test]
    fn the_backstop_poll_does_not_refetch_what_live_already_stored() {
        let (transport, log) = FakeTrade::new();
        log.lock().unwrap().search_ids = Some(Vec::new());
        let connector = Arc::new(ScriptedConnector::new());
        connector.push(Step::New(vec!["live-1".to_string()]));
        let (settings, obs_id) = observe_live_settings("Choir of the Storm");
        let db = temp_db("live-then-poll");
        let handle = RuntimeHandle::start_offline_with_connector(
            settings,
            RuntimePaths::new(db.clone()),
            transport,
            shared(&connector),
        )
        .unwrap();

        let mut seen = Vec::new();
        wait_for_fetches(&handle, &mut seen, &log, 1);
        assert_eq!(log.lock().unwrap().fetches[0], ["live-1"]);

        // 现在让兜底那一轮的搜索也报回同一条 id。
        log.lock().unwrap().search_ids = Some(vec!["live-1".to_string()]);
        let searches_before = log.lock().unwrap().searches;
        handle
            .try_send(RuntimeCommand::DiscoverNow {
                obs_id: obs_id.clone(),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while log.lock().unwrap().searches <= searches_before && Instant::now() < deadline {
            match handle.try_next_event() {
                Some(event) => seen.push(event),
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        thread::sleep(Duration::from_millis(300));
        while let Some(event) = handle.try_next_event() {
            seen.push(event);
        }
        drop(handle);

        assert_eq!(
            log.lock().unwrap().fetches.len(),
            1,
            "秒推抓过的 id 不该被兜底轮询再抓一次:{seen:#?}"
        );
        let store = WatchStore::open(&db).expect("open");
        assert_eq!(store.active_listing_ids(&obs_id).unwrap(), ["live-1"]);
        remove_db(&db);
    }

    /// 蹲价和观察分的是同一份 live 连接额度,而**蹲价优先**:
    /// 挤不上的那条观察退回兜底轮询,并且在状态里说清楚为什么。
    #[test]
    fn watches_win_the_last_live_connection_and_the_observation_says_so() {
        let (transport, _log) = FakeTrade::new();
        let connector = Arc::new(ScriptedConnector::new());
        let (mut settings, obs_id) = observe_live_settings("Choir of the Storm");
        settings.watcher.max_live_connections = 1;
        settings.watches = live_settings().watches;

        let handle = RuntimeHandle::start_offline_with_connector(
            settings,
            RuntimePaths::in_memory(),
            transport,
            shared(&connector),
        )
        .unwrap();

        let mut seen = Vec::new();
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::ObservationStatus { obs_id: id, status }
                    if *id == obs_id
                        && status.live == LiveRunState::Disabled(LiveOffReason::TooMany))
            },
            "the observation to be pushed out by the cap",
        );
        // 蹲价那条连上了,而且只开了这一条。
        wait_for(
            &handle,
            &mut seen,
            |event| {
                matches!(event, RuntimeEvent::WatchStatus { status, .. }
                    if status.live.is_connected())
            },
            "the watch to keep its live connection",
        );
        assert_eq!(connector.connects(), 1, "额度只够一条,蹲价拿走了");
        assert!(
            logged(&seen, &["observation", "no live connection left"]),
            "被挤下来要说一句为什么:{seen:#?}"
        );
    }

    // ---- 回查阶梯 ------------------------------------------------------

    /// 到点查了一次、它还在:升一档,下一档排在 `first_seen + 1800`。
    ///
    /// 阶梯本身是纯函数(在 `pnd_domain` 里钉着),这一条钉的是 actor 真的
    /// 把它接上了 —— 查完一次真的升档、真的把下一次排上,而不是原地打转。
    #[test]
    fn a_listing_that_is_still_there_climbs_one_rung() {
        let (transport, log) = FakeTrade::new();
        log.lock().unwrap().search_ids = Some(Vec::new());
        let (settings, obs_id) = observe_settings("Choir of the Storm");
        let db = temp_db("ladder");
        // 一条老挂单:它的第一档(+600 秒)早就该查了。
        let first_seen = 1_000_000i64;
        {
            let store = WatchStore::open(&db).expect("open");
            seed_listing(&store, &obs_id, "old", first_seen);
        }

        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::new(db.clone()), transport)
                .unwrap();
        let mut seen = Vec::new();
        wait_for_fetches(&handle, &mut seen, &log, 1);
        drop(handle);

        assert_eq!(log.lock().unwrap().fetches[0], ["old"], "到点的那条被查了");
        let store = WatchStore::open(&db).expect("open");
        let row = store
            .observed_listing(&obs_id, "old")
            .unwrap()
            .expect("row");
        assert_eq!(row.check_rung, 1);
        assert_eq!(row.next_check_at, first_seen + 1_800);
        assert_eq!(row.status, pnd_storage::ObservedStatus::Active);
        remove_db(&db);
    }

    /// 两条观察各有到期的挂单:拼进**同一次** fetch(一次最多 10 个 id),
    /// 而回信按 id 对号,各记各的账。
    ///
    /// 不拼批的话,两条观察各发一个装着两个 id 的请求 —— 同样的信息花掉两次
    /// 抓取额度,而那份额度是这条功能唯一真正稀缺的东西。
    #[test]
    fn one_sweep_serves_two_observations_and_attributes_each_answer() {
        let (transport, log) = FakeTrade::new();
        log.lock().unwrap().search_ids = Some(Vec::new());
        // 观察二的 "b2" 卖掉了。
        log.lock().unwrap().gone_ids.insert("b2".to_string());

        let (first, one) = observe_settings("Tablets");
        let (second, two) = observe_settings("Rings");
        let mut settings = first;
        settings.observations.extend(second.observations);
        let db = temp_db("sweep-two");
        let first_seen = 1_000_000i64;
        {
            let store = WatchStore::open(&db).expect("open");
            for listing_id in ["a1", "a2"] {
                seed_listing(&store, &one, listing_id, first_seen);
            }
            for listing_id in ["b1", "b2"] {
                seed_listing(&store, &two, listing_id, first_seen);
            }
        }

        let handle =
            RuntimeHandle::start_offline(settings, RuntimePaths::new(db.clone()), transport)
                .unwrap();
        let mut seen = Vec::new();
        wait_for_fetches(&handle, &mut seen, &log, 1);
        drop(handle);

        let batch = log.lock().unwrap().fetches[0].clone();
        assert_eq!(batch.len(), 4, "四条挂单拼成一次 fetch:{batch:?}");
        assert!(batch.len() <= 10, "一次最多 10 个 id");
        assert_eq!(
            batch.iter().collect::<BTreeSet<_>>(),
            ["a1", "a2", "b1", "b2"]
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .iter()
                .collect::<BTreeSet<_>>()
        );

        let store = WatchStore::open(&db).expect("open");
        // 观察一的两条都还在:各升一档,一条都没被判成没了。
        for listing_id in ["a1", "a2"] {
            let row = store
                .observed_listing(&one, listing_id)
                .unwrap()
                .expect("row");
            assert_eq!(
                row.status,
                pnd_storage::ObservedStatus::Active,
                "{listing_id}"
            );
            assert_eq!(row.check_rung, 1, "{listing_id}");
        }
        // 观察二:b1 还在,b2 没了 —— 而且这一笔只记在观察二头上。
        assert_eq!(
            store
                .observed_listing(&two, "b1")
                .unwrap()
                .expect("row")
                .status,
            pnd_storage::ObservedStatus::Active
        );
        let gone = store.observed_listing(&two, "b2").unwrap().expect("row");
        assert_eq!(gone.status, pnd_storage::ObservedStatus::Gone);
        assert!(gone.gone_at.is_some());
        assert_eq!(store.observation_summary(&one).unwrap().gone, 0);
        assert_eq!(store.observation_summary(&two).unwrap().gone, 1);
        remove_db(&db);
    }
}
