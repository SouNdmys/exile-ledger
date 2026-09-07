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
    CurrencyRates, ListingSummary, PriceCap, SearchRef, WatchId, decode_search_id, judge,
    search_page_url, search_request_body,
};
use pnd_ninja::client::NinjaClient;
use pnd_settings::{AppSettings, WatchEntry};
use pnd_storage::{
    AlertRow, AlertSource, LiveState, NewAlert, StorageError, WatchState, WatchStore,
    default_watch_db_path,
};
use pnd_trade::live::{LiveConfig, MAX_LIVE_CONNECTIONS_PER_ACCOUNT};
use pnd_trade::{
    BucketUsage, Budget, MAX_FETCH_IDS, SEARCH_LONG_WINDOW_REQUESTS, SEARCH_LONG_WINDOW_SECS,
    TradeClient, ggg_error, jwt_expiry,
};
use thiserror::Error;

use crate::decide::{Decision, MatchedListing, coalesce, decide};
use crate::gateway::{
    GatewayError, GatewayEvent, GatewayHandle, GatewayReply, GatewayRequest, Priority, ReplyKind,
    RequestKind, RequestTag, SearchOutcome, SessionCheckOutcome, TradeGateway, TradeTransport,
};
use crate::live_worker::{
    LiveConnector, LiveEvent, LiveOffReason, LiveRunState, LiveWorkerConfig, LiveWorkerHandle,
    TungsteniteConnector, spawn_live_worker,
};
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

/// token 里没有 `exp` 时的兜底:拿到超过这么久就当它过期了。
/// 计划里定的 10 分钟 —— 那是个短命 JWT。
///
/// 只是兜底:token 自己带 `exp` 的时候听它的(见 [`usable_token`])。
/// 2026-09-07 那次 403 的头号嫌疑就是"真实 TTL 比这条规矩短"。
const HIDEOUT_TOKEN_MAX_AGE_SECS: i64 = 600;

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
    /// 怎么去开一条 live 连接。生产是 tungstenite,测试是写好剧本的假货。
    connector: Arc<dyn LiveConnector>,
    /// 正在跑的 live worker,一条搜索最多一条。
    live_workers: BTreeMap<WatchId, LiveWorkerHandle>,
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
            connector,
            live_workers: BTreeMap::new(),
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

    /// 睡到下一次该轮询的时刻,封顶一秒。
    fn wait(&self, now: i64) -> Duration {
        match self.scheduler.next_deadline() {
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
        // 搜索额度 6 小时就那么多次,几条搜索分着花。设置里填得再勤,
        // 也不能勤过这个地板 —— 超了不是报错,是被服务端 429 冷却。
        self.scheduler.set_budget_floor(budget_floor_interval(
            enabled_count,
            budget.effective_limit(SEARCH_LONG_WINDOW_REQUESTS),
            SEARCH_LONG_WINDOW_SECS,
        ));
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
                self.stop_live_worker(&entry.id);
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
        let mut start: Vec<(WatchId, LiveConfig)> = Vec::new();
        let mut planned: BTreeMap<WatchId, LiveRunState> = BTreeMap::new();
        for entry in &self.settings.watches {
            if !entry.enabled || !entry.live {
                planned.insert(entry.id.clone(), LiveRunState::Off);
                continue;
            }
            let Some(session) = session.as_deref() else {
                planned.insert(
                    entry.id.clone(),
                    LiveRunState::Disabled(if self.session_ok {
                        LiveOffReason::NoSession
                    } else {
                        LiveOffReason::SessionInvalid
                    }),
                );
                continue;
            };
            if start.len() >= cap {
                planned.insert(
                    entry.id.clone(),
                    LiveRunState::Disabled(LiveOffReason::TooMany),
                );
                continue;
            }
            let search = SearchRef {
                league: entry.league.clone(),
                search_id: entry.search_id.clone(),
            };
            start.push((
                entry.id.clone(),
                LiveConfig::new(&search, session, user_agent.clone()),
            ));
            planned.insert(entry.id.clone(), LiveRunState::Connecting);
        }

        // 不该再跑的先停掉(包括设置里已经没有的那些)。
        let stop: Vec<WatchId> = self
            .live_workers
            .keys()
            .filter(|id| !start.iter().any(|(started, _)| started == *id))
            .cloned()
            .collect();
        for watch_id in stop {
            self.stop_live_worker(&watch_id);
        }

        for (watch_id, config) in start {
            if self.live_workers.contains_key(&watch_id) {
                // 已经在跑:它自己报上来的状态才是准的,别用一个"Connecting"
                // 把"已连上"盖掉。(搜索 id 或联赛改了的话,`apply_settings`
                // 已经在那条 `query_stale` 分支里把它停掉了,这里就轮不到。)
                planned.remove(&watch_id);
                continue;
            }
            let handle = spawn_live_worker(
                LiveWorkerConfig::new(watch_id.clone(), config),
                Arc::clone(&self.connector),
                self.live_events.clone(),
            );
            self.live_workers.insert(watch_id, handle);
        }

        // 没有会话时说一句就够了,别每次 ApplySettings 都念一遍。
        let wants_live = self
            .settings
            .watches
            .iter()
            .any(|entry| entry.enabled && entry.live);
        // `session_ok == false` 说的是"会话被拒了" —— 那句话
        // `on_session_invalid` 已经说过了,别再补一句"你没有会话"。
        if wants_live && session.is_none() && self.session_ok && !self.no_session_reported {
            self.no_session_reported = true;
            self.emit(RuntimeEvent::Log(
                "live search is off: no POESESSID in settings — polling anonymously".to_string(),
            ));
        }

        for (watch_id, state) in planned {
            self.set_live_state(&watch_id, state, now);
        }
    }

    fn stop_live_worker(&mut self, watch_id: &WatchId) {
        if let Some(worker) = self.live_workers.remove(watch_id) {
            // 只打招呼不等:它可能正卡在一次读超时里,而主循环一秒都不该停。
            worker.stop();
        }
    }

    /// 全部停掉,并把每条搜索标上同一个原因(会话失效时用)。
    fn stop_all_live_workers(&mut self, reason: LiveOffReason, now: i64) {
        let running: Vec<WatchId> = self.live_workers.keys().cloned().collect();
        for watch_id in running {
            self.stop_live_worker(&watch_id);
        }
        let live_watches: Vec<WatchId> = self
            .settings
            .watches
            .iter()
            .filter(|entry| entry.live)
            .map(|entry| entry.id.clone())
            .collect();
        for watch_id in live_watches {
            self.set_live_state(&watch_id, LiveRunState::Disabled(reason), now);
        }
    }

    fn handle_live_event(&mut self, event: LiveEvent) {
        let now = now_secs();
        match event {
            LiveEvent::State { watch_id, state } => self.set_live_state(&watch_id, state, now),
            LiveEvent::New { watch_id, ids } => self.on_live_push(&watch_id, ids, now),
            LiveEvent::SessionInvalid { .. } => self.on_session_invalid(now),
            LiveEvent::Log(message) => self.emit(RuntimeEvent::Log(message)),
        }
    }

    /// 换一条搜索的 live 档位:落库、调轮询节奏、广播。
    fn set_live_state(&mut self, watch_id: &WatchId, state: LiveRunState, now: i64) {
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
                tag: RequestTag {
                    watch_id: Some(watch_id.clone()),
                    alert_id: None,
                    label: LIVE_FETCH_LABEL,
                },
            });
        }
        // 顺手把"最后一次收到推送"的时刻记上:界面问"WS 还活着吗"看的是它。
        self.note(
            self.store
                .set_live_state(watch_id, LiveState::Connected, now),
            "set_live_state",
        );
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
            tag: RequestTag {
                watch_id: Some(watch_id.clone()),
                alert_id: None,
                label: SEARCH_LABEL,
            },
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
            // 认掉了,两种都轮不到这里。
            ReplyKind::Whisper(_) | ReplyKind::SessionCheck(_) => {}
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
            tag: RequestTag {
                watch_id: Some(watch_id.clone()),
                alert_id: None,
                label: FETCH_LABEL,
            },
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
            tag: RequestTag {
                watch_id: None,
                alert_id: Some(alert_id),
                label: HIDEOUT_FETCH_LABEL,
            },
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
            tag: RequestTag {
                watch_id: None,
                alert_id: Some(alert_id),
                label: HIDEOUT_WHISPER_LABEL,
            },
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
            // 这条链路上不会有 search,也不会有会话检查。
            ReplyKind::Search(_) | ReplyKind::SessionCheck(_) => {}
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
            tag: RequestTag {
                watch_id: None,
                alert_id: None,
                label: SESSION_CHECK_LABEL,
            },
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
        GatewayError::Status {
            status: status @ (401 | 403),
            excerpt,
        } => {
            let (reason, detail) = excerpt_parts(excerpt);
            (
                *status,
                format!(
                    "{reason}the trade site refused {what} for listing {listing_id} with \
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
    fn listings_json(
        ids: &[String],
        token: Option<&str>,
        online: bool,
        price_divine: Option<i64>,
    ) -> String {
        let items: Vec<String> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
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
                     "item":{{"name":"Choir of the Storm","typeLine":"Lapis Amulet"}}}}"#,
                    price_divine.unwrap_or((18 - index as i64).max(1))
                )
            })
            .collect();
        format!(r#"{{"result":[{}]}}"#, items.join(","))
    }

    impl TradeTransport for FakeTrade {
        fn search(
            &self,
            _league: &str,
            _body_json: &str,
            _session: Option<&str>,
        ) -> Result<TradeResponse, pnd_trade::TransportError> {
            let rules = {
                let mut log = self.log.lock().unwrap();
                log.searches += 1;
                log.rate_rules.clone()
            };
            let body = r#"{"id":"SEARCHID","total":2,"result":["one","two"]}"#;
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
            let (token, offline, nothing, price) = {
                let mut log = self.log.lock().unwrap();
                log.fetches.push(ids.to_vec());
                (
                    log.hideout_token.clone(),
                    log.seller_offline,
                    log.fetch_returns_nothing,
                    log.price_divine,
                )
            };
            if nothing {
                return ok(r#"{"result":[]}"#);
            }
            ok(&listings_json(ids, token.as_deref(), !offline, price))
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
                    status: 401,
                    excerpt: String::new(),
                },
                401,
                vec!["401", "POESESSID"],
            ),
            (
                GatewayError::Status {
                    status: 500,
                    excerpt: "boom".to_string(),
                },
                500,
                vec!["500", "boom"],
            ),
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
    fn a_token_older_than_ten_minutes_is_not_used() {
        let now = 10_000;
        assert_eq!(
            usable_token(Some("tok"), Some(now - 599), now),
            Some("tok".to_string())
        );
        assert_eq!(
            usable_token(Some("tok"), Some(now - 601), now),
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
}
