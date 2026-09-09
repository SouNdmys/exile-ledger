//! Live Search worker:一条搜索一条线程,负责"连上、听推送、断了再连"。
//!
//! 分工和 `pnd-trade/src/live.rs` 咬得很死:那一层只会"握一次手、读一条消息",
//! 不睡眠、不重试、不循环;**所有的节奏都在这里**——第几次重连、等多久、
//! 什么时候认输。反过来,这里一行协议也不写。
//!
//! worker **不碰交易网关**。它把推来的挂单 id 原样交给 actor
//! ([`LiveEvent::New`]),由 actor 排进那条唯一的串行队列。为什么不让 worker
//! 自己 fetch:限速的账全局只有一本,五条 live 各自发 fetch,五份账都是错的。
//!
//! 三件事值得单独说:
//!
//! 1. **取消要快。** 底下的 socket 有读超时(默认 30 秒),所以 worker 最迟
//!    隔一个读超时就能看见取消标志;退避期间则是每 50 毫秒看一眼。
//! 2. **重连次数什么时候清零。** 连上超过 [`LIVE_STABLE_SECS`] 秒才算"这条
//!    连接是好的",断了从头数;秒断秒连说明对面根本不欢迎我们,得继续拉长间隔。
//! 3. **抖动不摇骰子。** [`jitter_for`] 是"搜索 id + 第几次"的哈希:同一条
//!    搜索每次算出来一样(测试能钉死),不同搜索算出来不一样(五条一起断线
//!    也不会在同一秒一起扑上去)。
//!
//! 会话失效(带着 cookie 却被 401/403 拒)是**唯一**会让 worker 主动认输的
//! 情况:那种失败重试多少次都一样,该做的是告诉 actor 停掉全部 live、丢掉
//! cookie、请用户换一个新的 POESESSID。轮询照常匿名跑着。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pnd_domain::{ObservationId, WatchId};
use pnd_storage::LiveState;
use pnd_trade::live::{LiveConfig, LiveError, LiveMessage, LiveSession, reconnect_delay};

use crate::now_secs;

/// 一条 live 连接是替谁连的。
///
/// 蹲价和市场观察用的是**同一条** WebSocket 链路(同一个协议、同样的重连
/// 阶梯、同一份"最多几条"的额度),区别只在推来的 id 交给谁:蹲价拿去判价
/// 报警,观察拿去记账。所以 worker 只认这一个标识,不认它背后是哪一种东西。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LiveTarget {
    Watch(WatchId),
    Observation(ObservationId),
}

impl LiveTarget {
    /// 退避抖动和线程名要的那个字符串。同一条搜索每次算出来一样,
    /// 所以这里必须是 id 本身,不能带上"watch/obs"之类的前缀。
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            LiveTarget::Watch(watch_id) => watch_id.as_str(),
            LiveTarget::Observation(obs_id) => obs_id.as_str(),
        }
    }
}

impl std::fmt::Display for LiveTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 连上超过这么久,才算"这条连接是好的",下次断线从第一档重新开始退避。
pub const LIVE_STABLE_SECS: i64 = 60;

/// 被 Cloudflare 拦住之后这条 live 停多久。和网关那边一个数(5 分钟):
/// 硬刷只会让它拦得更久。
pub const CLOUDFLARE_HOLD_SECS: i64 = 300;

/// 退避期间每隔这么久看一眼取消标志。
const CANCEL_SLICE: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------
// 传输层抽象
// ---------------------------------------------------------------------

/// 一条已经握完手的 live 连接。
///
/// 生产实现就是 [`LiveSession`];抽成 trait 只有一个目的:测试能塞一条
/// **写好剧本**的连接进来(第三条消息推两个 id、第四条断线),于是"断了会
/// 不会重连、重连的账对不对"这些问题不用真的去连 GGG 就能验。
pub trait LiveStream {
    /// 读下一条消息,最多阻塞一个读超时。超时回 [`LiveMessage::Idle`]。
    fn next(&mut self) -> Result<LiveMessage, LiveError>;
    /// 好好道别。按值拿走 `Box`,因为道别之后这条连接就不该再被用了。
    fn close(self: Box<Self>);
}

/// "去开一条 live 连接"这件事。
pub trait LiveConnector: Send + Sync {
    fn connect(&self, config: &LiveConfig) -> Result<Box<dyn LiveStream>, LiveError>;
}

impl LiveStream for LiveSession {
    fn next(&mut self) -> Result<LiveMessage, LiveError> {
        LiveSession::next(self)
    }

    fn close(self: Box<Self>) {
        LiveSession::close(*self);
    }
}

/// 生产用的连接器:真的去握一次 WebSocket 手。
pub struct TungsteniteConnector;

impl LiveConnector for TungsteniteConnector {
    fn connect(&self, config: &LiveConfig) -> Result<Box<dyn LiveStream>, LiveError> {
        Ok(Box::new(LiveSession::connect(config)?))
    }
}

// ---------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------

/// live 开不起来的原因。做成枚举而不是一句话,是因为界面要按语言显示它
/// (`pnd-app` 的文案都在 `i18n.rs` 里,业务代码里不写中文)。
///
/// "用户压根没勾 live"不在这里 —— 那是 [`LiveRunState::Off`],不是"想跑但跑不了"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveOffReason {
    /// 设置里没有 POESESSID —— live 接口不接待匿名连接。
    NoSession,
    /// 已经开满了(设置里的 `max_live_connections`,或者服务端的 20 条硬顶)。
    TooMany,
    /// 会话被服务端拒了,程序已经停用它。
    SessionInvalid,
}

/// 一条搜索的 live 连接在运行时眼里的状态。
///
/// 比库里那个 [`LiveState`] 多带"为什么"和"等到什么时候" —— 那两样是给
/// 界面看的,不值得为它们改数据库 schema。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveRunState {
    /// 没在跑,也没人要它跑。
    #[default]
    Off,
    /// 想跑但跑不了。
    Disabled(LiveOffReason),
    Connecting,
    Connected {
        /// 这条连接是什么时候接通的(unix 秒)。
        since: i64,
    },
    Backoff {
        /// 等到这个时刻再重连。
        until: i64,
        /// 这是第几次重连(0 = 第一次)。
        attempt: u32,
    },
    /// 被 Cloudflare 拦住,停到这个时刻。
    Held {
        until: i64,
    },
}

impl LiveRunState {
    /// WebSocket 现在是不是活的。轮询档位就看这一个答案。
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, LiveRunState::Connected { .. })
    }

    /// 折成库里那四档。`Disabled`/`Off` 都是"没在跑",`Held` 和退避一样
    /// 都是"等着重连"。
    #[must_use]
    pub fn stored(&self) -> LiveState {
        match self {
            LiveRunState::Off | LiveRunState::Disabled(_) => LiveState::Off,
            LiveRunState::Connecting => LiveState::Connecting,
            LiveRunState::Connected { .. } => LiveState::Connected,
            LiveRunState::Backoff { .. } | LiveRunState::Held { .. } => LiveState::Backoff,
        }
    }
}

/// worker 说给 actor 听的话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    /// 档位变了。
    State {
        target: LiveTarget,
        state: LiveRunState,
    },
    /// 服务端推来了新挂单 id。actor 负责按 10 个一批交给网关去 fetch。
    New {
        target: LiveTarget,
        ids: Vec<String>,
    },
    /// 带着 cookie 却被拒了 —— 这个 POESESSID 已经不能用了。
    SessionInvalid { target: LiveTarget },
    /// 一行给用户看的日志(握手失败原因之类)。
    Log(String),
}

// ---------------------------------------------------------------------
// 退避
// ---------------------------------------------------------------------

/// 算一次重连要等多久。
///
/// 生产用 [`backoff_delay`];测试塞一个"等 5 毫秒"的版本进来,这样"第几次
/// 重连、退到什么时候"照样验得了,而测试不用真的坐等 5 秒。
pub type BackoffFn = fn(target: &str, attempt: u32, retry_after: Option<u64>) -> Duration;

/// 第 `attempt` 次重连等多久:5/10/20/40/80/160/300 秒的阶梯(见
/// [`reconnect_delay`])加上这条搜索自己的抖动;服务端要是明说了
/// `Retry-After`,取两者的大者 —— 它说的话优先,但也不能比阶梯还急。
#[must_use]
pub fn backoff_delay(target: &str, attempt: u32, retry_after: Option<u64>) -> Duration {
    let ladder = reconnect_delay(attempt, jitter_for(target, attempt));
    match retry_after {
        Some(secs) => ladder.max(Duration::from_secs(secs)),
        None => ladder,
    }
}

/// "搜索 id + 第几次重连" → 0.0..1.0 的抖动。
///
/// 用 FNV-1a 哈希而不是随机数:同一条搜索每次算出来一样(测试能钉死一个数),
/// 不同搜索算出来不一样(五条一起断线不会在同一秒一起扑上去)。
#[must_use]
pub fn jitter_for(target: &str, attempt: u32) -> f64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in target
        .as_bytes()
        .iter()
        .copied()
        .chain(attempt.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    // 取高 53 位:f64 的尾数就这么宽,除出来正好落在 [0, 1)。
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

/// 断线之后,重连次数接着数还是从头数。
///
/// 连上超过 [`LIVE_STABLE_SECS`] 就说明这条线路本身是通的(可能只是服务端
/// 重启了一下),下次从第一档重来;秒断秒连则继续拉长间隔。
#[must_use]
pub fn next_attempt(attempt: u32, connected_secs: i64) -> u32 {
    if connected_secs > LIVE_STABLE_SECS {
        0
    } else {
        attempt.saturating_add(1)
    }
}

// ---------------------------------------------------------------------
// 配置与句柄
// ---------------------------------------------------------------------

/// 一条 worker 线程需要知道的全部东西。
///
/// `backoff` 是给测试留的后门(生产值就是 [`backoff_delay`]):测试想验
/// "第几次重连、退到什么时候",但不想真的坐等 5 秒。
pub struct LiveWorkerConfig {
    pub target: LiveTarget,
    pub live: LiveConfig,
    pub backoff: BackoffFn,
}

impl LiveWorkerConfig {
    #[must_use]
    pub fn new(target: LiveTarget, live: LiveConfig) -> LiveWorkerConfig {
        LiveWorkerConfig {
            target,
            live,
            backoff: backoff_delay,
        }
    }
}

/// 一条 worker 线程的遥控器。
pub struct LiveWorkerHandle {
    target: LiveTarget,
    cancel: Arc<AtomicBool>,
    /// 线程自己走完之后翻成 true。用来实现"最多等这么久"的 join。
    finished: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl LiveWorkerHandle {
    #[must_use]
    pub fn target(&self) -> &LiveTarget {
        &self.target
    }

    /// 打个招呼让它收摊。**不等** —— 线程可能正卡在一次读超时里(最多 30 秒),
    /// 而调用方(actor 主循环)一秒都不该被卡住。
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 收摊并等它走掉,最多等 `timeout`。
    ///
    /// 等不到就放它去:线程手里只有自己的 socket 和一个发送端,进程退出时
    /// 系统会一起收走。为了一次读超时让整个程序关不掉,不值得。
    pub fn stop_and_join(mut self, timeout: Duration) {
        self.stop();
        let deadline = std::time::Instant::now() + timeout;
        while !self.finished.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            thread::sleep(CANCEL_SLICE);
        }
        if self.finished.load(Ordering::Relaxed)
            && let Some(join) = self.join.take()
        {
            let _ = join.join();
        }
    }
}

impl Drop for LiveWorkerHandle {
    /// 被丢掉(比如这条搜索删了)时至少把取消标志立上,线程读超时之后会自己走。
    fn drop(&mut self) {
        self.stop();
    }
}

/// 起一条 worker 线程。
pub fn spawn_live_worker(
    config: LiveWorkerConfig,
    connector: Arc<dyn LiveConnector>,
    events: Sender<LiveEvent>,
) -> LiveWorkerHandle {
    let target = config.target.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let thread_finished = Arc::clone(&finished);
    let join = thread::Builder::new()
        .name(format!("pnd-live-{}", short_id(target.as_str())))
        .spawn(move || {
            // 哨兵先建:worker panic 了也要把"我走了"翻上去,
            // 否则 `stop_and_join` 会白等一个超时。
            let _done = FinishedOnDrop(thread_finished);
            run_live_worker(&config, connector.as_ref(), &events, &thread_cancel);
        })
        .ok();
    LiveWorkerHandle {
        target,
        cancel,
        finished,
        join,
    }
}

struct FinishedOnDrop(Arc<AtomicBool>);

impl Drop for FinishedOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// 线程名里只放 id 的前 8 个字符:uuid 全长会把线程名撑得没法看。
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

// ---------------------------------------------------------------------
// 线程本体
// ---------------------------------------------------------------------

/// 一次连接是怎么结束的。
enum PumpEnd {
    /// 我们自己要停(取消,或者 actor 已经没了)。
    Cancelled,
    Failed(LiveError),
}

/// worker 主循环:连 → 听 → 断 → 退避 → 再连。
///
/// 直接暴露出来是为了测试可以在当前线程上跑它(不用 spawn 就能钉死时序)。
pub fn run_live_worker(
    config: &LiveWorkerConfig,
    connector: &dyn LiveConnector,
    events: &Sender<LiveEvent>,
    cancel: &AtomicBool,
) {
    let mut attempt: u32 = 0;
    while !cancel.load(Ordering::Relaxed) {
        if !emit(events, config, LiveRunState::Connecting) {
            return;
        }

        let opened_at = now_secs();
        let error = match connector.connect(&config.live) {
            Ok(stream) => {
                if !emit(events, config, LiveRunState::Connected { since: opened_at }) {
                    return;
                }
                match pump(stream, config, events, cancel) {
                    PumpEnd::Cancelled => return,
                    PumpEnd::Failed(error) => {
                        // 连上过就按"这条连接活了多久"决定要不要清零重连次数。
                        attempt = next_attempt(attempt, now_secs() - opened_at);
                        error
                    }
                }
            }
            Err(error) => {
                if error.is_session_problem() {
                    report_session_problem(config, events, &error);
                    return;
                }
                // 握手都没成:这次不算"连上过",次数只加不减。
                attempt = attempt.saturating_add(1);
                error
            }
        };

        if !hold(config, events, cancel, &error, attempt) {
            return;
        }
    }
}

/// 读消息,直到断线或者被叫停。
fn pump(
    mut stream: Box<dyn LiveStream>,
    config: &LiveWorkerConfig,
    events: &Sender<LiveEvent>,
    cancel: &AtomicBool,
) -> PumpEnd {
    loop {
        if cancel.load(Ordering::Relaxed) {
            stream.close();
            return PumpEnd::Cancelled;
        }
        match stream.next() {
            Ok(LiveMessage::New(ids)) => {
                // 空表也是合法推送,只是没什么好转告的。
                if ids.is_empty() {
                    continue;
                }
                let sent = events.send(LiveEvent::New {
                    target: config.target.clone(),
                    ids,
                });
                if sent.is_err() {
                    stream.close();
                    return PumpEnd::Cancelled;
                }
            }
            // 读超时:这段时间服务端什么都没说,连接还好着。回到循环顶上
            // 就是为了看一眼取消标志。
            Ok(LiveMessage::Idle) => {}
            // 一张没装挂单的 `{"result":"<JWT>"}`(装了的走上面那条
            // [`LiveMessage::New`])。**只报形状,不报内容** —— 那串是服务端
            // 发的 JWT,早先它被原样抄进日志,整条 token 就进了状态栏。
            Ok(LiveMessage::Subscribed { token }) => {
                let line = format!(
                    "live {}: server message keys=[\"result\"] result_len={}",
                    config.target,
                    token.chars().count()
                );
                if events.send(LiveEvent::Log(line)).is_err() {
                    stream.close();
                    return PumpEnd::Cancelled;
                }
            }
            // `description` 已经是"键名 + 值长度",不是原文(见
            // `pnd_trade::live::describe_live_message`)。
            Ok(LiveMessage::Other(description)) => {
                let line = format!("live {}: server message {description}", config.target);
                if events.send(LiveEvent::Log(line)).is_err() {
                    stream.close();
                    return PumpEnd::Cancelled;
                }
            }
            Err(error) => return PumpEnd::Failed(error),
        }
    }
}

/// 断线之后停一会儿。返回 `false` 表示"别再连了"(被取消,或者 actor 没了)。
fn hold(
    config: &LiveWorkerConfig,
    events: &Sender<LiveEvent>,
    cancel: &AtomicBool,
    error: &LiveError,
    attempt: u32,
) -> bool {
    let now = now_secs();
    let (state, wait) = if error.is_cloudflare() {
        (
            LiveRunState::Held {
                until: now + CLOUDFLARE_HOLD_SECS,
            },
            Duration::from_secs(CLOUDFLARE_HOLD_SECS as u64),
        )
    } else {
        let wait = (config.backoff)(config.target.as_str(), attempt, error.retry_after());
        (
            LiveRunState::Backoff {
                until: now + wait.as_secs() as i64,
                attempt,
            },
            wait,
        )
    };

    if events
        .send(LiveEvent::Log(format!("live {}: {error}", config.target)))
        .is_err()
    {
        return false;
    }
    if !emit(events, config, state) {
        return false;
    }
    sleep_cancellable(wait, cancel)
}

/// 会话出了问题:带着 cookie 就喊一声让 actor 停掉全部 live;没带 cookie
/// 就只是"live 本来就需要会话",说一句、把自己标成停用,然后安静地走。
fn report_session_problem(
    config: &LiveWorkerConfig,
    events: &Sender<LiveEvent>,
    error: &LiveError,
) {
    if config.live.has_session() {
        let _ = events.send(LiveEvent::Log(format!(
            "live {}: the trade site refused the session ({error})",
            config.target
        )));
        let _ = events.send(LiveEvent::SessionInvalid {
            target: config.target.clone(),
        });
        return;
    }
    // 匿名重试多少次都是同一个 401,退避阶梯在这里只是空转。
    let _ = events.send(LiveEvent::Log(format!(
        "live {}: live search needs a POESESSID — polling continues anonymously",
        config.target
    )));
    let _ = emit(
        events,
        config,
        LiveRunState::Disabled(LiveOffReason::NoSession),
    );
}

fn emit(events: &Sender<LiveEvent>, config: &LiveWorkerConfig, state: LiveRunState) -> bool {
    events
        .send(LiveEvent::State {
            target: config.target.clone(),
            state,
        })
        .is_ok()
}

/// 睡 `total`,每 [`CANCEL_SLICE`] 看一眼取消标志。返回 `false` = 被叫停了。
fn sleep_cancellable(total: Duration, cancel: &AtomicBool) -> bool {
    let deadline = std::time::Instant::now() + total;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(CANCEL_SLICE));
    }
}

// 测试用的假连接:actor 的测试也要用同一套剧本,所以这个模块对整个 crate
// 可见(仍然只在 `cargo test` 里编译)。两处各造一份假的,迟早会造出两种
// 不一样的行为。
#[cfg(test)]
pub(crate) mod live_worker_tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};

    use pnd_domain::SearchRef;

    use super::*;

    /// 剧本里的一步。
    #[derive(Debug, Clone)]
    pub(crate) enum Step {
        /// 推一批挂单 id。
        New(Vec<String>),
        /// 服务端发来的一条**原文**,走生产的 `parse_live_message`。
        ///
        /// 和 `New` 分开是因为要验的正是解析那一段:`{"result":"<JWT>"}`
        /// 认不认得出来、认不出来的话日志里会出现什么。
        Raw(String),
        /// 断线。
        Fail(LiveError),
    }

    /// 一条写好剧本的 live 连接 + 它的连接器。
    ///
    /// 剧本空了就一直回 [`LiveMessage::Idle`](像真的连接一样安静),
    /// 所以测试可以随时往里追加下一步。
    pub(crate) struct ScriptedConnector {
        steps: Arc<Mutex<VecDeque<Step>>>,
        connects: Arc<std::sync::atomic::AtomicUsize>,
        /// 每次 `connect()` 的结果。空了就当成功。
        outcomes: Arc<Mutex<VecDeque<LiveError>>>,
        /// 每次握手要去的那个地址。PoE1 的搜索证明"live 也走对了那一代"
        /// 靠的就是它 —— 配置里那个 `game` 只有拼成 URL 才看得见效果。
        urls: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedConnector {
        pub(crate) fn new() -> ScriptedConnector {
            ScriptedConnector {
                steps: Arc::new(Mutex::new(VecDeque::new())),
                connects: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                outcomes: Arc::new(Mutex::new(VecDeque::new())),
                urls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub(crate) fn push(&self, step: Step) {
            self.steps.lock().unwrap().push_back(step);
        }

        /// 下一次(或者接下来第几次)握手直接被拒。
        pub(crate) fn refuse(&self, error: LiveError) {
            self.outcomes.lock().unwrap().push_back(error);
        }

        pub(crate) fn connects(&self) -> usize {
            self.connects.load(Ordering::Relaxed)
        }

        pub(crate) fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
    }

    impl LiveConnector for ScriptedConnector {
        fn connect(&self, config: &LiveConfig) -> Result<Box<dyn LiveStream>, LiveError> {
            self.connects.fetch_add(1, Ordering::Relaxed);
            self.urls.lock().unwrap().push(pnd_trade::live::live_ws_url(
                config.game,
                &config.league,
                &config.search_id,
            ));
            if let Some(error) = self.outcomes.lock().unwrap().pop_front() {
                return Err(error);
            }
            Ok(Box::new(ScriptedStream {
                steps: Arc::clone(&self.steps),
            }))
        }
    }

    struct ScriptedStream {
        steps: Arc<Mutex<VecDeque<Step>>>,
    }

    impl LiveStream for ScriptedStream {
        fn next(&mut self) -> Result<LiveMessage, LiveError> {
            let step = self.steps.lock().unwrap().pop_front();
            match step {
                Some(Step::New(ids)) => Ok(LiveMessage::New(ids)),
                // 生产的解析函数,不是抄一份:验的就是它。
                Some(Step::Raw(text)) => Ok(pnd_trade::live::parse_live_message(&text)?),
                Some(Step::Fail(error)) => Err(error),
                None => {
                    // 空剧本 = 服务端在发呆。别把 CPU 烧了。
                    thread::sleep(Duration::from_millis(5));
                    Ok(LiveMessage::Idle)
                }
            }
        }

        fn close(self: Box<Self>) {}
    }

    /// 测试里的退避:阶梯的账照记(状态里那个 `attempt` 还是真的),但只等 5 毫秒。
    fn fast_backoff(_target: &str, _attempt: u32, _retry_after: Option<u64>) -> Duration {
        Duration::from_millis(5)
    }

    pub(crate) fn live_config(session: &str) -> LiveConfig {
        let search = SearchRef {
            game: pnd_domain::Game::Poe2,
            league: "Forbidden Rites".to_string(),
            search_id: "H4sIAAAA-_09".to_string(),
        };
        LiveConfig::new(&search, session, "ExileLedger/test")
    }

    fn worker_config(watch: &str, session: &str) -> LiveWorkerConfig {
        LiveWorkerConfig {
            target: LiveTarget::Watch(WatchId(watch.to_string())),
            live: live_config(session),
            backoff: fast_backoff,
        }
    }

    fn io(reason: &str) -> LiveError {
        LiveError::Io(reason.to_string())
    }

    pub(crate) fn handshake(status: u16, html: bool, retry_after: Option<u64>) -> LiveError {
        LiveError::Handshake {
            status,
            looks_like_html: html,
            retry_after,
            body_excerpt: "…".to_string(),
        }
    }

    /// 起一条 worker,收事件直到 `wanted` 满意为止,然后叫停。
    fn drive(
        config: LiveWorkerConfig,
        connector: Arc<ScriptedConnector>,
        wanted: impl Fn(&[LiveEvent]) -> bool,
    ) -> Vec<LiveEvent> {
        let (tx, rx): (Sender<LiveEvent>, Receiver<LiveEvent>) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let worker = thread::spawn(move || {
            run_live_worker(&config, connector.as_ref(), &tx, &thread_cancel);
        });

        let mut seen: Vec<LiveEvent> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !wanted(&seen) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => seen.push(event),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        cancel.store(true, Ordering::Relaxed);
        let _ = worker.join();
        seen
    }

    fn states(seen: &[LiveEvent]) -> Vec<LiveRunState> {
        seen.iter()
            .filter_map(|event| match event {
                LiveEvent::State { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    // ---- 纯函数 ------------------------------------------------------

    #[test]
    fn the_backoff_ladder_grows_and_respects_retry_after() {
        let plan: Vec<u64> = (0..8)
            .map(|attempt| backoff_delay("w-1", attempt, None).as_secs())
            .collect();
        // 每一档都在标称值的 ±20% 里,而且只增不减。
        for (attempt, secs) in plan.iter().enumerate() {
            let nominal = reconnect_delay(attempt as u32, 0.5).as_secs();
            assert!(
                *secs >= nominal * 8 / 10 && *secs <= nominal * 12 / 10,
                "attempt {attempt}: {secs}s 不在 {nominal}s 的 ±20% 里"
            );
        }
        assert!(plan.windows(2).all(|pair| pair[0] <= pair[1]), "{plan:?}");

        // 服务端说等 120 秒,而阶梯第一档只有 5 秒 —— 听服务端的。
        assert_eq!(backoff_delay("w-1", 0, Some(120)), Duration::from_secs(120));
        // 反过来,阶梯已经比它长了就不用缩回去。
        assert!(backoff_delay("w-1", 6, Some(10)) >= Duration::from_secs(240));
    }

    #[test]
    fn jitter_is_stable_per_watch_and_differs_between_watches() {
        assert_eq!(
            jitter_for("w-1", 0),
            jitter_for("w-1", 0),
            "同一条要算得出一样的"
        );
        assert_ne!(jitter_for("w-1", 0), jitter_for("w-1", 1));
        assert_ne!(
            jitter_for("w-1", 0),
            jitter_for("w-2", 0),
            "别让两条一起扑上去"
        );
        for attempt in 0..64u32 {
            let jitter = jitter_for("some-watch-id", attempt);
            assert!((0.0..1.0).contains(&jitter), "{jitter}");
        }
    }

    #[test]
    fn a_connection_that_held_for_a_minute_resets_the_ladder() {
        assert_eq!(next_attempt(3, 61), 0, "活过一分钟就从头数");
        assert_eq!(next_attempt(3, 60), 4, "刚好一分钟还不算稳");
        assert_eq!(next_attempt(0, 0), 1);
        assert_eq!(next_attempt(u32::MAX, 0), u32::MAX, "不能绕回 0");
    }

    #[test]
    fn run_states_fold_onto_the_four_stored_ones() {
        assert_eq!(LiveRunState::Off.stored(), LiveState::Off);
        assert_eq!(
            LiveRunState::Disabled(LiveOffReason::NoSession).stored(),
            LiveState::Off
        );
        assert_eq!(LiveRunState::Connecting.stored(), LiveState::Connecting);
        assert_eq!(
            LiveRunState::Connected { since: 1 }.stored(),
            LiveState::Connected
        );
        assert_eq!(
            LiveRunState::Backoff {
                until: 1,
                attempt: 0
            }
            .stored(),
            LiveState::Backoff
        );
        assert_eq!(LiveRunState::Held { until: 1 }.stored(), LiveState::Backoff);
        assert!(LiveRunState::Connected { since: 1 }.is_connected());
        assert!(!LiveRunState::Connecting.is_connected());
    }

    // ---- worker 循环 --------------------------------------------------

    #[test]
    fn a_push_is_forwarded_and_the_connection_keeps_going() {
        let connector = Arc::new(ScriptedConnector::new());
        connector.push(Step::New(vec!["a".to_string(), "b".to_string()]));
        // 空推送不该转告任何人。
        connector.push(Step::New(Vec::new()));
        connector.push(Step::New(vec!["c".to_string()]));

        let seen = drive(
            worker_config("w-1", "cookie"),
            Arc::clone(&connector),
            |seen| {
                seen.iter()
                    .filter(|event| matches!(event, LiveEvent::New { .. }))
                    .count()
                    >= 2
            },
        );

        let pushes: Vec<Vec<String>> = seen
            .iter()
            .filter_map(|event| match event {
                LiveEvent::New { ids, .. } => Some(ids.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            pushes,
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string()]
            ]
        );
        assert_eq!(connector.connects(), 1, "推送不该让它重连");
        assert_eq!(states(&seen)[0], LiveRunState::Connecting);
        assert!(states(&seen)[1].is_connected());
    }

    /// poe2 真正的推送长这样:`{"result":"<JWT>"}`,一条 `new` 都没有。
    /// worker 得把它当推送转告 actor,而不是当一条日志咽下去 ——
    /// 咽下去的后果就是 2026-09-07 那一上午:3008 帧,秒推一件也没记下来。
    ///
    /// token 是 2026-09-08 取证跑里那一帧,值换成了占位符
    /// (和 `pnd-trade/src/live.rs` 的 `RESULT_TOKEN` 是同一张)。
    #[test]
    fn a_result_frame_reaches_the_actor_as_a_push() {
        const TOKEN: &str = "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9.\
eyJkIjoiM3ZVQ0FQTEFDRUhPTERFUnBsYWNlaG9sZGVyMDEyMzQ1Njc4OWFiY2RlZisvIiwiZXhwIjoxNzg4ODUxMTQ2\
LCJpc3MiOiJINHNJQUFBQUFBQUFBMVdPcGxhY2Vob2xkZXJzZWFyY2hpZCJ9.ZHVtbXktc2lnbmF0dXJl";

        let connector = Arc::new(ScriptedConnector::new());
        connector.push(Step::Raw(format!(r#"{{"result":"{TOKEN}"}}"#)));

        let seen = drive(
            worker_config("w-1", "cookie"),
            Arc::clone(&connector),
            |seen| {
                seen.iter()
                    .any(|event| matches!(event, LiveEvent::New { .. }))
            },
        );

        let pushes: Vec<Vec<String>> = seen
            .iter()
            .filter_map(|event| match event {
                LiveEvent::New { ids, .. } => Some(ids.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            pushes,
            vec![vec![TOKEN.to_string()]],
            "整张 token 就是去 fetch 的把手"
        );
        // 而且它不该同时又被写成一行日志。
        assert!(
            !seen.iter().any(|event| matches!(event, LiveEvent::Log(_))),
            "{seen:#?}"
        );
    }

    /// 一张没装挂单的 result token(以及别的不认识的消息)只上日志,
    /// 而日志里只许出现形状,绝不许出现那串 token ——
    /// 这是 2026-09-07 那次真跑里状态栏泄露的东西。
    #[test]
    fn the_connect_receipt_is_logged_by_shape_never_by_value() {
        let token = format!(
            "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.{}.c2ln",
            "Z".repeat(260)
        );
        let connector = Arc::new(ScriptedConnector::new());
        connector.push(Step::Raw(format!(r#"{{"result":"{token}"}}"#)));
        // 一条不认识的消息也走同一条路。
        connector.push(Step::Raw(
            r#"{"heartbeat":123,"payload":"aaaaaaaaaaaaaaaaaaaaaaaa"}"#.to_string(),
        ));

        let seen = drive(
            worker_config("w-1", "cookie"),
            Arc::clone(&connector),
            |seen| {
                seen.iter()
                    .filter(|event| matches!(event, LiveEvent::Log(_)))
                    .count()
                    >= 2
            },
        );

        let logs: Vec<String> = seen
            .iter()
            .filter_map(|event| match event {
                LiveEvent::Log(message) => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            logs,
            vec![
                format!("live w-1: server message keys=[\"result\"] result_len={}", token.chars().count()),
                r#"live w-1: server message keys=["heartbeat", "payload"] heartbeat=123 payload_len=24"#.to_string(),
            ],
            "{seen:#?}"
        );
        for log in &logs {
            assert!(!log.contains(&token), "token 漏进日志了:{log}");
            assert!(
                !log.contains("aaaaaaaaaaaaaaaaaaaaaaaa"),
                "长值漏进日志了:{log}"
            );
        }
        assert_eq!(connector.connects(), 1, "一条回执不该让它重连");
    }

    #[test]
    fn a_dropped_connection_climbs_the_ladder_and_reconnects() {
        let connector = Arc::new(ScriptedConnector::new());
        for _ in 0..3 {
            connector.push(Step::Fail(io("connection reset")));
        }

        let seen = drive(
            worker_config("w-1", "cookie"),
            Arc::clone(&connector),
            |seen| {
                seen.iter()
                    .filter(|event| {
                        matches!(
                            event,
                            LiveEvent::State {
                                state: LiveRunState::Backoff { .. },
                                ..
                            }
                        )
                    })
                    .count()
                    >= 3
            },
        );

        let attempts: Vec<u32> = states(&seen)
            .into_iter()
            .filter_map(|state| match state {
                LiveRunState::Backoff { attempt, .. } => Some(attempt),
                _ => None,
            })
            .collect();
        assert_eq!(attempts, vec![1, 2, 3], "每断一次就往上走一档");
        // 三次退避 = 第一次连上之后又自己爬起来连了两次。
        assert!(
            connector.connects() >= 3,
            "断了要自己爬起来重连, got {}",
            connector.connects()
        );
    }

    #[test]
    fn a_refused_session_stops_the_worker_and_tells_the_actor() {
        let connector = Arc::new(ScriptedConnector::new());
        connector.refuse(handshake(401, false, None));

        let seen = drive(
            worker_config("w-1", "cookie"),
            Arc::clone(&connector),
            |seen| {
                seen.iter()
                    .any(|event| matches!(event, LiveEvent::SessionInvalid { .. }))
            },
        );

        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, LiveEvent::SessionInvalid { .. }))
                .count(),
            1
        );
        assert_eq!(connector.connects(), 1, "会话不行了就别再试了");
    }

    /// 没带 cookie 的 401 只是"live 需要会话",不是"你的会话坏了" ——
    /// 不能让匿名用户看到一个"请换新的 POESESSID"的提示。
    #[test]
    fn an_anonymous_refusal_never_claims_the_session_went_bad() {
        let connector = Arc::new(ScriptedConnector::new());
        connector.refuse(handshake(401, false, None));

        let seen = drive(worker_config("w-1", ""), Arc::clone(&connector), |seen| {
            seen.iter().any(|event| {
                matches!(
                    event,
                    LiveEvent::State {
                        state: LiveRunState::Disabled(LiveOffReason::NoSession),
                        ..
                    }
                )
            })
        });

        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, LiveEvent::SessionInvalid { .. })),
            "{seen:#?}"
        );
        assert_eq!(connector.connects(), 1);
    }

    #[test]
    fn a_cloudflare_page_holds_this_worker_for_five_minutes() {
        let connector = Arc::new(ScriptedConnector::new());
        connector.refuse(handshake(403, true, None));

        let seen = drive(
            worker_config("w-1", "cookie"),
            Arc::clone(&connector),
            |seen| {
                seen.iter().any(|event| {
                    matches!(
                        event,
                        LiveEvent::State {
                            state: LiveRunState::Held { .. },
                            ..
                        }
                    )
                })
            },
        );

        let held = states(&seen)
            .into_iter()
            .find_map(|state| match state {
                LiveRunState::Held { until } => Some(until),
                _ => None,
            })
            .expect("a Held state");
        let left = held - now_secs();
        assert!(
            (CLOUDFLARE_HOLD_SECS - 2..=CLOUDFLARE_HOLD_SECS).contains(&left),
            "拦截页该停 5 分钟, got {left}s"
        );
        // 5 分钟的等待必须能被取消打断(`drive` 收到状态就叫停了),
        // 否则关程序要等到天荒地老 —— 这个测试本身跑完就是证据。
        assert_eq!(connector.connects(), 1);
    }
}
