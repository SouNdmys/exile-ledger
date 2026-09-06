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
    CurrencyRates, ListingSummary, PriceCap, WatchId, decode_search_id, judge, search_request_body,
};
use pnd_ninja::client::NinjaClient;
use pnd_settings::{AppSettings, WatchEntry};
use pnd_storage::{
    AlertSource, LiveState, NewAlert, StorageError, WatchState, WatchStore, default_watch_db_path,
};
use pnd_trade::{BucketUsage, Budget, MAX_FETCH_IDS, TradeClient};
use thiserror::Error;

use crate::decide::{Decision, MatchedListing, coalesce, decide};
use crate::gateway::{
    GatewayError, GatewayEvent, GatewayHandle, GatewayReply, GatewayRequest, Priority, ReplyKind,
    RequestKind, RequestTag, SearchOutcome, TradeGateway, TradeTransport,
};
use crate::now_secs;
use crate::poll::{PollOutcome, PollScheduler};

/// 汇率多久重读一次。ninja 自己的缓存是 5 分钟,15 分钟一次既不会读到
/// 陈年数据,也不至于为了一张换算表天天打扰人家。
const RATES_REFRESH_SECONDS: u64 = 15 * 60;

/// 主循环最长睡多久。轮询的时间表通常比这远得多,但一秒醒一次
/// 才能及时看到"该关门了"。
const MAX_LOOP_WAIT: Duration = Duration::from_secs(1);

/// 请求标签:回信回来时靠它认出这是轮询的哪一步。
const SEARCH_LABEL: &str = "poll-search";
const FETCH_LABEL: &str = "poll-fetch";

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
    Shutdown,
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
    pub next_poll_at: Option<i64>,
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
    },
    /// 命中了,该弹卡片了。(装箱同 `ApplySettings`:里面整条挂单摘要很大。)
    ListingMatched(Box<MatchedListing>),
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

/// actor 线程的邮箱。四个来源合并成一个通道,主循环只用在一个地方等。
enum Inbox {
    Command(RuntimeCommand),
    Reply(GatewayReply),
    Gateway(GatewayEvent),
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
        Self::spawn(settings, paths, transport, Some(production_transport), true)
    }

    /// 测试(和以后的离线回放)用:传输层由调用方给,汇率线程不起 ——
    /// 一个测试不该因为 poe.ninja 今天抽风而红。
    pub fn start_offline(
        settings: AppSettings,
        paths: RuntimePaths,
        transport: Box<dyn TradeTransport>,
    ) -> Result<RuntimeHandle, RuntimeError> {
        Self::spawn(settings, paths, transport, None, false)
    }

    fn spawn(
        mut settings: AppSettings,
        paths: RuntimePaths,
        transport: Box<dyn TradeTransport>,
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

struct RuntimeActor {
    settings: AppSettings,
    store: WatchStore,
    gateway: GatewayHandle,
    /// 网关回信的地址,每封请求里都塞一份克隆。
    replies: Sender<GatewayReply>,
    /// 网关广播的地址。重建网关时要再给一份。
    gateway_events: Sender<GatewayEvent>,
    events: Sender<RuntimeEvent>,
    inbox: Sender<Inbox>,
    scheduler: PollScheduler,
    watches: BTreeMap<WatchId, WatchRuntime>,
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
    fn new(
        settings: AppSettings,
        store: WatchStore,
        transport: Box<dyn TradeTransport>,
        rebuild: Option<TransportFactory>,
        events: Sender<RuntimeEvent>,
        inbox: Sender<Inbox>,
    ) -> RuntimeActor {
        let (reply_tx, reply_rx) = channel::<GatewayReply>();
        let (gateway_tx, gateway_rx) = channel::<GatewayEvent>();
        // 网关的两个通道各起一条搬运线程,把东西倒进 actor 的单一邮箱。
        // 这样主循环只用在一个地方等,而不是轮询三个通道。两条线程都在
        // 上游通道断掉时自己结束,不需要 join。
        forward(reply_rx, inbox.clone(), Inbox::Reply, "pnd-gw-replies");
        forward(gateway_rx, inbox.clone(), Inbox::Gateway, "pnd-gw-events");

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
            events,
            inbox,
            scheduler: PollScheduler::new(),
            watches: BTreeMap::new(),
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

        // UA 和预算都是在网关线程创建时定死的(一个在客户端里,一个在限速器里),
        // 改它们只能换一条线程。这是罕见操作(用户在设置页动了旋钮),
        // 代价是丢掉学到的限速头,下一次请求会重新学回来。
        if (user_agent != self.user_agent || budget != self.budget)
            && let Some(rebuild) = self.rebuild
        {
            self.gateway = TradeGateway::start(
                rebuild(&settings),
                budget,
                session_of(&settings),
                self.gateway_events.clone(),
            );
            self.user_agent = user_agent;
            self.budget = budget;
            self.emit(RuntimeEvent::Log(
                "restarted the trade gateway (user agent or budget changed)".to_string(),
            ));
        } else if session_changed {
            self.gateway.set_session(session_of(&settings));
        }

        let mut keep: BTreeSet<WatchId> = BTreeSet::new();
        let enabled_count = settings
            .watches
            .iter()
            .filter(|entry| entry.enabled)
            .count();
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

            let query_body = if query_stale {
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
        let next_poll_at = self.scheduler.entry(&watch_id).map(|entry| entry.next_at);

        match self.watches.get_mut(&watch_id) {
            Some(runtime) => {
                runtime.entry = entry;
                if query_body.is_some() {
                    runtime.query_body = query_body;
                }
                runtime.status.state = state;
                runtime.status.next_poll_at = next_poll_at;
                runtime.status.hits_today = hits_today;
                if last_error.is_some() {
                    runtime.status.last_error = last_error;
                }
            }
            None => {
                let status = WatchStatus {
                    state,
                    next_poll_at,
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
        runtime.status.state = WatchRunState::Polling;

        // 请求一发出去就把下一轮排上:否则 `next_at` 停在过去,主循环会
        // 一直觉得"这条该跑了"而空转。
        self.scheduler.defer(watch_id, now);
        let next_at = self.scheduler.entry(watch_id).map(|entry| entry.next_at);
        if let Some(runtime) = self.watches.get_mut(watch_id) {
            runtime.status.next_poll_at = next_at;
        }

        self.gateway.submit(GatewayRequest {
            kind: RequestKind::Search { league, body_json },
            priority: Priority::PollSearch,
            reply: self.replies.clone(),
            tag: RequestTag {
                watch_id: Some(watch_id.clone()),
                label: SEARCH_LABEL,
            },
        });
        self.emit_status(watch_id);
    }

    fn handle_reply(&mut self, reply: GatewayReply) {
        let now = now_secs();
        let Some(watch_id) = reply.tag.watch_id.clone() else {
            return;
        };
        if !self.watches.contains_key(&watch_id) {
            // 这条搜索在请求飞在路上的时候被删了,回信直接丢掉。
            return;
        }
        match reply.kind {
            ReplyKind::Search(Ok(outcome)) => self.on_search(&watch_id, outcome, now),
            ReplyKind::Fetch(Ok(listings)) => self.on_fetch(&watch_id, listings, now),
            ReplyKind::Search(Err(error)) | ReplyKind::Fetch(Err(error)) => {
                self.on_poll_failed(&watch_id, &error, now);
            }
            // whisper 是第二阶段的事;现在没人发,回来了也没人等。
            ReplyKind::Whisper(_) => {}
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
                label: FETCH_LABEL,
            },
        });
    }

    /// fetch 回来了:逐条去重 + 判定,命中的合成一张卡片。
    fn on_fetch(&mut self, watch_id: &WatchId, listings: Vec<ListingSummary>, now: i64) {
        let Some(runtime) = self.watches.get(watch_id) else {
            return;
        };
        let cap: PriceCap = runtime.entry.price_cap.clone();
        let label = runtime.entry.label.clone();
        let league = runtime.entry.league.clone();
        let search_id = runtime.entry.search_id.clone();
        // 刚加进来的搜索第一轮看到的都是存量货色。用户如果不想被存量吵醒,
        // 这一轮只记账不叫人。
        let alerting = runtime.first_poll_done || runtime.entry.alert_on_first_poll;

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
                source: AlertSource::Poll,
            };
            if let Some(alert_id) = self.note(self.store.insert_alert(&alert, now), "insert_alert")
            {
                hits.push((alert_id, listing));
            }
        }

        if !hits.is_empty() {
            let alert_ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
            if let Some((headline, extra)) = coalesce(hits) {
                self.emit(RuntimeEvent::ListingMatched(Box::new(MatchedListing {
                    alert_ids,
                    watch_id: watch_id.clone(),
                    label,
                    league,
                    search_id,
                    headline,
                    extra,
                    cap,
                    source: AlertSource::Poll,
                })));
            }
        }

        if let Some(runtime) = self.watches.get_mut(watch_id) {
            runtime.first_poll_done = true;
        }
        self.finish_poll(watch_id, now);
    }

    /// 一轮顺利跑完:失败计数清零,排下一轮。
    fn finish_poll(&mut self, watch_id: &WatchId, now: i64) {
        self.note(self.store.clear_failures(watch_id, now), "clear_failures");
        self.scheduler
            .reschedule(watch_id, now, PollOutcome::Ok, &self.settings.watcher);
        let next_at = self.scheduler.entry(watch_id).map(|entry| entry.next_at);
        let hits_today = self
            .note(
                self.store.hits_since(watch_id, start_of_today(now)),
                "hits_since",
            )
            .unwrap_or(0);
        if let Some(runtime) = self.watches.get_mut(watch_id) {
            runtime.in_flight = false;
            runtime.status.state = WatchRunState::Polling;
            runtime.status.failures = 0;
            runtime.status.last_error = None;
            runtime.status.next_poll_at = next_at;
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
        let next_at = self.scheduler.entry(watch_id).map(|entry| entry.next_at);
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
        }
        self.emit_status(watch_id);
    }

    /// 一次 fetch 带几个 id。设置里能调,但服务端上限就是 10。
    fn fetch_batch(&self) -> usize {
        (self.settings.watcher.fetch_batch as usize).clamp(1, MAX_FETCH_IDS)
    }

    // ---- 网关广播 ----------------------------------------------------

    fn handle_gateway_event(&mut self, event: GatewayEvent) {
        match event {
            GatewayEvent::Budget { policy, usage } => {
                self.emit(RuntimeEvent::Budget { policy, usage });
            }
            GatewayEvent::SessionInvalid => {
                self.emit(RuntimeEvent::Log(
                    "the trade site did not recognise the POESESSID — it has been dropped"
                        .to_string(),
                ));
                self.emit(RuntimeEvent::SessionInvalid);
            }
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
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use pnd_domain::{Currency, Price};
    use pnd_settings::WatchEntry;
    use pnd_trade::TradeResponse;

    use super::*;

    /// 今天从网页上抄下来的真搜索 id(Choir of the Storm)。
    /// 用真的而不是编一个:`decode_search_id` 会去解 gzip,编的解不开。
    const FIXTURE_ID: &str = "H4sIAAAAAAAAAx2LvQnAIBBGV5GvdgLbjJAyWAhRFPRO9FIEcfdo2vcz0MXJ02EGuEpiggFTTuQxNcgVv8AROTXFQUn06hRuBfof13cNyFt35eheOKQsvm1hp50f6Xdj02AAAAA";

    /// 假交易站:不打网络,回一份写死的 search + fetch。
    struct FakeTrade {
        searches: Arc<AtomicUsize>,
        fetches: Arc<AtomicUsize>,
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

    impl TradeTransport for FakeTrade {
        fn search(
            &self,
            _league: &str,
            _body_json: &str,
            _session: Option<&str>,
        ) -> Result<TradeResponse, pnd_trade::TransportError> {
            self.searches.fetch_add(1, Ordering::Relaxed);
            ok(r#"{"id":"SEARCHID","total":2,"result":["one","two"]}"#)
        }

        fn fetch(
            &self,
            _ids: &[String],
            _search_id: &str,
            _session: Option<&str>,
        ) -> Result<TradeResponse, pnd_trade::TransportError> {
            self.fetches.fetch_add(1, Ordering::Relaxed);
            ok(r#"{"result":[
                {"id":"one","listing":{"indexed":"2026-09-06T10:00:00Z","whisper":"@A hi",
                    "price":{"type":"~price","amount":18,"currency":"divine"},
                    "account":{"name":"Aaa","lastCharacterName":"AaaChar","online":{"league":"x"}}},
                 "item":{"name":"Choir of the Storm","typeLine":"Lapis Amulet"}},
                {"id":"two","listing":{"indexed":"2026-09-06T10:05:00Z","whisper":"@B hi",
                    "price":{"type":"~price","amount":9,"currency":"divine"},
                    "account":{"name":"Bbb","lastCharacterName":"BbbChar","online":{"league":"x"}}},
                 "item":{"name":"Choir of the Storm","typeLine":"Lapis Amulet"}}
            ]}"#)
        }

        fn whisper(
            &self,
            _token: &str,
            _session: &str,
            _referer: &str,
        ) -> Result<TradeResponse, pnd_trade::TransportError> {
            ok("{}")
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
        let searches = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::new(AtomicUsize::new(0));
        let transport = Box::new(FakeTrade {
            searches: Arc::clone(&searches),
            fetches: Arc::clone(&fetches),
        });
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

        assert_eq!(searches.load(Ordering::Relaxed), 1);
        assert_eq!(fetches.load(Ordering::Relaxed), 1);

        // 再跑一轮同样的两条挂单:全都见过了,不该再响。
        handle
            .try_send(RuntimeCommand::PollNow(WatchId("w1".to_string())))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while fetches.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
            if let Some(event) = handle.try_next_event() {
                seen.push(event);
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(fetches.load(Ordering::Relaxed), 2, "第二轮没跑起来");
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

    #[test]
    fn a_watch_that_should_not_alert_on_the_first_poll_only_records() {
        let mut settings = settings();
        settings.watches[0].alert_on_first_poll = false;
        let transport = Box::new(FakeTrade {
            searches: Arc::new(AtomicUsize::new(0)),
            fetches: Arc::new(AtomicUsize::new(0)),
        });
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
        let transport = Box::new(FakeTrade {
            searches: Arc::new(AtomicUsize::new(0)),
            fetches: Arc::new(AtomicUsize::new(0)),
        });
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
}
