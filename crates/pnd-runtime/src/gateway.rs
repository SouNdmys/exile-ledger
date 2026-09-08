//! 串行交易网关:一条线程 + 一个按优先级排序的队列,程序里**所有**交易站请求
//! 都从这里出去。
//!
//! 为什么非得串成一条:限速头是按 IP 记的,同一时刻只该有一个人在数"我还剩
//! 多少次"。轮询、live 推送、用户点按钮要是各拿一个限速器各发各的,三方的账
//! 都是错的,迟早一起撞上 429 —— 而 429 是会连累整个 IP 的。所以别人只能往
//! 这条线程的邮箱里投一封请求,然后等一封回信。
//!
//! 这一层同样**不做业务判断**:它不认识"蹲价"这回事,只认识优先级、限速和
//! 回信地址。谁该轮询、命中了要不要叫人,是 `actor.rs` 的事。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use pnd_domain::{ListingSummary, ObservationId, WatchId};
use pnd_trade::{
    BucketUsage, Budget, FETCH_POLICY, RateLimiter, SEARCH_POLICY, TradeClient, TradeResponse,
    TransportError, backoff_after_429, parse_fetch_response, parse_fetch_response_by_id,
    parse_search_response,
};
use thiserror::Error;

use crate::now_secs;

/// whisper 接口的限速策略名在文档里没有,只能等第一次响应把它带回来。
/// 在那之前用这个占位串当限速器的键 —— 反正键叫什么不重要,重要的是
/// whisper 和 search/fetch 各记各的账。
pub const WHISPER_POLICY_PLACEHOLDER: &str = "trade-whisper";

/// 被 Cloudflare 拦住之后停多久。五分钟是计划里定的:硬刷只会让它拦得更久。
const CLOUDFLARE_HOLD_SECS: i64 = 300;

/// 没事干的时候最多睡这么久再醒一次。醒来只是为了看一眼取消标志,
/// 不醒的话 `Drop` 里的 join 就得等到下一封请求才回得来。
const IDLE_WAIT: Duration = Duration::from_secs(1);

/// 队列里明明有活却算出"零秒后放行"时的兜底睡眠。没有它,某个环节算错
/// 就会变成一条 100% 占 CPU 的空转线程。
const MIN_WAIT: Duration = Duration::from_millis(50);

/// "压根没连上"之后隔多久再试一次。
///
/// 一秒只为一件事:别在网络正抽风的那一瞬间连着捅两下。真正解决问题的是
/// **换一条连接** —— 握手砸了的那条连接不会进 ureq 的连接池,所以下一次
/// 一定是新开的。
const TRANSPORT_RETRY_DELAY_SECS: i64 = 1;

// ---------------------------------------------------------------------
// 传输层抽象
// ---------------------------------------------------------------------

/// 网关眼里的"交易站"。
///
/// 生产用的是 [`TradeClient`](pnd_trade::TradeClient);抽成 trait 只有一个
/// 目的:测试(和以后的离线回放)能塞一个假的进来,把整条 actor 链路在没有
/// 网络的情况下跑完。签名和 `TradeClient` 上的三个方法逐字一致。
pub trait TradeTransport: Send {
    fn search(
        &self,
        league: &str,
        body_json: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError>;

    fn fetch(
        &self,
        ids: &[String],
        search_id: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError>;

    fn whisper(
        &self,
        token: &str,
        session: &str,
        referer: &str,
    ) -> Result<TradeResponse, TransportError>;
}

impl TradeTransport for TradeClient {
    fn search(
        &self,
        league: &str,
        body_json: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError> {
        TradeClient::search(self, league, body_json, session)
    }

    fn fetch(
        &self,
        ids: &[String],
        search_id: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError> {
        TradeClient::fetch(self, ids, search_id, session)
    }

    fn whisper(
        &self,
        token: &str,
        session: &str,
        referer: &str,
    ) -> Result<TradeResponse, TransportError> {
        TradeClient::whisper(self, token, session, referer)
    }
}

// ---------------------------------------------------------------------
// 请求 / 回信
// ---------------------------------------------------------------------

/// 谁先谁后。用户点的按钮永远排在最前面 —— 他正看着屏幕等结果,
/// 而轮询晚三十秒没人看得出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priority {
    User = 0,
    LiveFetch = 1,
    PollFetch = 2,
    PollSearch = 3,
    Background = 4,
}

/// 请求的来源标签,原样跟着回信走 —— actor 靠它认出"这封回信是哪条搜索的
/// 哪一步"。网关自己不看这里面的内容。
///
/// `alert_id` 是"去藏身处"那条链路用的:那次刷新 token 的 fetch 和随后的
/// whisper 都不属于任何一轮轮询,回信要认的是提醒记录里的行号。
/// `obs_id` 同理,认的是市场观察那一条。
///
/// `sweep_id` 是回查扫描用的:**一批回查里可以混着好几条观察的挂单**
/// (凑满 10 个 id 才不浪费一次 fetch),所以它认的不是"哪一条观察",
/// 而是"哪一批" —— actor 手里存着那一批的 (观察, 挂单) 清单,回信按 id 对号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTag {
    pub watch_id: Option<WatchId>,
    pub obs_id: Option<ObservationId>,
    pub alert_id: Option<i64>,
    pub sweep_id: Option<u64>,
    pub label: &'static str,
}

impl RequestTag {
    /// 一条蹲价搜索的某一步。
    #[must_use]
    pub fn watch(watch_id: WatchId, label: &'static str) -> RequestTag {
        RequestTag {
            watch_id: Some(watch_id),
            obs_id: None,
            alert_id: None,
            sweep_id: None,
            label,
        }
    }

    /// 一条市场观察的某一步。
    #[must_use]
    pub fn observation(obs_id: ObservationId, label: &'static str) -> RequestTag {
        RequestTag {
            watch_id: None,
            obs_id: Some(obs_id),
            alert_id: None,
            sweep_id: None,
            label,
        }
    }

    /// 一批回查扫描。可能横跨好几条观察,所以只带批次号。
    #[must_use]
    pub fn sweep(sweep_id: u64, label: &'static str) -> RequestTag {
        RequestTag {
            watch_id: None,
            obs_id: None,
            alert_id: None,
            sweep_id: Some(sweep_id),
            label,
        }
    }

    /// 一条提醒上的某一步("去藏身处"那两下)。
    #[must_use]
    pub fn alert(alert_id: i64, label: &'static str) -> RequestTag {
        RequestTag {
            watch_id: None,
            obs_id: None,
            alert_id: Some(alert_id),
            sweep_id: None,
            label,
        }
    }

    /// 谁也不属于的一次请求(设置页那个"测试会话")。
    #[must_use]
    pub fn standalone(label: &'static str) -> RequestTag {
        RequestTag {
            watch_id: None,
            obs_id: None,
            alert_id: None,
            sweep_id: None,
            label,
        }
    }
}

/// 四种请求。请求体已经拼好了:网关不认识查询 JSON 长什么样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    Search {
        league: String,
        body_json: String,
    },
    /// 和 `Search` 发的是一模一样的请求,只是**要的东西不同**:回信里带的
    /// 不是挂单 id,而是这次响应的限速规则名 —— 设置页上那个"测试会话"
    /// 就靠规则里有没有 `Account` 判断 cookie 还活着没有。
    ///
    /// 单独一种而不是复用 `Search`:限速头是每封响应都有的东西,让所有
    /// 搜索回信都拖着一份规则名,只为了一个按钮,不划算。
    SessionCheck {
        league: String,
        body_json: String,
    },
    Fetch {
        ids: Vec<String>,
        search_id: String,
    },
    /// 和 `Fetch` 发的是同一个请求,**要的答案不同**:回信按请求时的 id 逐个
    /// 对号,查不到的那一格是 `None`。
    ///
    /// 市场观察问的正是"我问的这几条里哪几条没了" —— 而 `Fetch` 的回信是一个
    /// 摘要数组,`null` 那几格早被丢掉了,丢完就分不清是哪个 id 没的。
    /// 蹲价不需要这份信息(少一条便宜货无所谓),所以没有让所有 fetch 回信
    /// 都拖着一份 id 清单。
    FetchByIds {
        ids: Vec<String>,
        search_id: String,
    },
    Whisper {
        token: String,
        referer: String,
    },
}

/// 一封投进网关邮箱的请求。`reply` 是回信地址:谁投的谁给一个 `Sender`,
/// 网关不需要知道对面是 actor 还是一个测试。
pub struct GatewayRequest {
    pub kind: RequestKind,
    pub priority: Priority,
    pub reply: Sender<GatewayReply>,
    pub tag: RequestTag,
}

/// search 回来的三样东西。`id` 是这次搜索在服务端的编号,fetch 要拿它当
/// `?query=` 参数 —— 它和用户粘进来的那个搜索 id 通常一样,但以服务端为准。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    pub id: String,
    pub total: u64,
    pub result: Vec<String>,
}

/// 一次会话检查看到的东西。
///
/// 只有限速头这一样:响应体是什么不重要(哪怕交易站回一句"没找到"也行),
/// 重要的是服务端按哪些规则给这次请求限速。带了 cookie 却只回 `Ip`,
/// 就说明它根本没认出这个会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCheckOutcome {
    pub status: u16,
    /// 服务端说的规则名(`Ip`、`Account`…)。没有限速头时是空的。
    pub rules: Vec<String>,
    /// 规则里有 `Account` —— 也就是"这个 POESESSID 还认得出来"。
    pub mentions_account: bool,
}

/// 一次请求失败的原因。分这几类是因为上层的处置不同:`Transport` 和 `Status`
/// 下一轮再来,`CloudflareHold` 要告诉用户"被拦了,程序在等",
/// `Cancelled` 则连日志都不用记 —— 那是我们自己要关的。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GatewayError {
    #[error("could not reach the trade site: {0}")]
    Transport(String),
    /// 服务端答了,但不是 2xx。
    ///
    /// `excerpt` 是响应 body 的一行摘要(见
    /// [`TradeResponse::body_excerpt`](pnd_trade::TradeResponse::body_excerpt))。
    /// 早先这里只有一个状态码,于是"去藏身处"失败时卡片上只剩一个数字,
    /// 而服务端明明在 body 里写了原因。
    #[error("trade site answered {status}{}", excerpt_suffix(excerpt))]
    Status { status: u16, excerpt: String },
    #[error("could not read the response: {0}")]
    Parse(String),
    #[error("held back: the trade site answered with a Cloudflare page")]
    CloudflareHold,
    /// 这封请求非带 POESESSID 不可,可网关手上没有(设置里就没填,
    /// 或者服务端刚刚拒了它、网关已经把它丢掉了)。
    ///
    /// 单独一类而不是塞进 `Transport`:它压根没上过网,说"连不上交易站"
    /// 会把人往查网络的方向带。
    #[error("this request needs a POESESSID and the gateway has none")]
    NoSession,
    #[error("cancelled")]
    Cancelled,
}

/// `Status` 的 `Display` 后缀:body 有话说才带上,空的就只报状态码。
fn excerpt_suffix(excerpt: &str) -> String {
    if excerpt.is_empty() {
        String::new()
    } else {
        format!(": {excerpt}")
    }
}

/// 回信的内容,形状跟着请求走。
#[derive(Debug)]
pub enum ReplyKind {
    Search(Result<SearchOutcome, GatewayError>),
    SessionCheck(Result<SessionCheckOutcome, GatewayError>),
    Fetch(Result<Vec<ListingSummary>, GatewayError>),
    /// 按请求时的 id 顺序逐个对号,`None` = 那条挂单没了。见
    /// [`RequestKind::FetchByIds`]。
    FetchByIds(Result<Vec<(String, Option<ListingSummary>)>, GatewayError>),
    /// whisper 只回一个状态码:200 = 发出去了,503 多半是 token 过期。
    Whisper(Result<u16, GatewayError>),
}

#[derive(Debug)]
pub struct GatewayReply {
    pub tag: RequestTag,
    pub kind: ReplyKind,
}

/// 不针对某一封请求、而是"整条链路发生了什么"的广播。
///
/// 走单独一个通道:这些事件和某次请求的成败无关,谁都可能想看
/// (界面要画预算条,actor 要据此停用会话)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    Budget {
        policy: String,
        usage: Vec<BucketUsage>,
        /// 这条策略还要等几秒才放行下一封请求(界面上那句"N 秒后可用")。
        /// `None` = 现在就能发,或者还没学到任何限速头。
        next_allowed_in_secs: Option<u64>,
    },
    /// 带了 cookie,可服务端的限速规则里没有 `Account` —— 会话已经失效。
    SessionInvalid,
    CloudflareBlocked {
        until: i64,
    },
    RateLimited {
        policy: String,
        retry_in: u32,
    },
    Log(String),
}

/// 投进网关邮箱的三种东西。`Wake` 只是把 `recv_timeout` 捅醒
/// (取消的时候要用),自己什么也不做。
pub enum GatewayMessage {
    Request(GatewayRequest),
    SetSession(Option<String>),
    Wake,
}

// ---------------------------------------------------------------------
// 句柄
// ---------------------------------------------------------------------

/// 网关线程的遥控器。丢掉它 = 关掉网关。
pub struct GatewayHandle {
    tx: Sender<GatewayMessage>,
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl GatewayHandle {
    /// 投一封请求。发送失败(线程已经没了)不报错:回信本来就不保证会来,
    /// 上层的失败计数和退避会接住这种情况。
    pub fn submit(&self, request: GatewayRequest) {
        let _ = self.tx.send(GatewayMessage::Request(request));
    }

    /// 换一个 POESESSID(或者 `None` = 从此匿名)。
    pub fn set_session(&self, session: Option<String>) {
        let _ = self.tx.send(GatewayMessage::SetSession(session));
    }

    /// 把线程从等待里捅醒。队列外部条件变了(比如取消)时用。
    pub fn wake(&self) {
        let _ = self.tx.send(GatewayMessage::Wake);
    }
}

impl Drop for GatewayHandle {
    /// 先立取消标志再捅醒,最后 join:反过来的话线程可能正睡在
    /// `recv_timeout` 上,要等一秒才发现自己该走了。
    ///
    /// 有一条请求正在网络上飞的时候,这里最长会等到那条请求超时(20 秒)。
    /// 强行掐断一个 TLS 连接的代价更大,所以宁可等。
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.tx.send(GatewayMessage::Wake);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// ---------------------------------------------------------------------
// 队列
// ---------------------------------------------------------------------

/// 队列里的一封请求 + 网关自己记的三件事。
struct Pending {
    request: GatewayRequest,
    /// 投递顺序。同优先级里先来先走,靠它而不是 `Vec` 的位置 ——
    /// 429 重排队会把位置打乱。
    sequence: u64,
    /// 在这个时刻之前不许发(429 退避用)。
    not_before: i64,
    /// 已经因为 429 重试过几次,退避的指数就是它。
    attempts: u32,
    /// 已经因为"连都没连上"重发过一次。和 `attempts` 分开记:429 是服务端
    /// 说"你太快了",连不上是这封请求压根没上过网,两件事的次数不该混着算。
    retried_transport: bool,
}

/// 队列里挑一封现在就能发的:先看能不能发(时间到了、限速放行了),
/// 再在能发的里面挑优先级最高、投得最早的那封。
///
/// `next_allowed` 的键是 [`policy_for`] 给的静态策略名,不是限速器里的真名
/// (whisper 的真名要等第一次响应才知道)—— 这个函数只做排序,不认识策略。
fn pick_runnable(
    queue: &[Pending],
    next_allowed: &BTreeMap<&'static str, i64>,
    now: i64,
) -> Option<usize> {
    queue
        .iter()
        .enumerate()
        .filter(|(_, pending)| pending.not_before <= now)
        .filter(|(_, pending)| {
            next_allowed
                .get(policy_for(&pending.request.kind))
                .copied()
                .unwrap_or(now)
                <= now
        })
        .min_by_key(|(_, pending)| (pending.request.priority, pending.sequence))
        .map(|(index, _)| index)
}

/// 这种请求记在哪条限速策略名下。search 和 fetch 的策略名是实测到的常量,
/// whisper 用占位串(见 [`WHISPER_POLICY_PLACEHOLDER`])。
fn policy_for(kind: &RequestKind) -> &'static str {
    match kind {
        // 会话检查发的就是一次 search,它当然要从 search 的预算里出。
        RequestKind::Search { .. } | RequestKind::SessionCheck { .. } => SEARCH_POLICY,
        RequestKind::Fetch { .. } | RequestKind::FetchByIds { .. } => FETCH_POLICY,
        RequestKind::Whisper { .. } => WHISPER_POLICY_PLACEHOLDER,
    }
}

/// 这条策略还要等几秒。
///
/// 两种情况都回 `None`:现在就能发(等 0 秒不值得在界面上显示),以及限速器
/// 给出一个荒唐的远未来 —— 那是"还有请求在途、上限还没学到"的哨兵值
/// (见 `RateLimiter::next_request_time`),不是真的要等一万年。
fn next_allowed_in(limiter: &mut RateLimiter, policy: &str, now: i64) -> Option<u64> {
    let wait = limiter.next_request_time(policy, now).saturating_sub(now);
    (wait > 0 && wait <= MAX_REPORTED_WAIT_SECS).then_some(wait as u64)
}

/// 超过一天的等待一律当"不知道"。真实的限速窗口最长 6 小时。
const MAX_REPORTED_WAIT_SECS: i64 = 86_400;

/// 这封请求重发一次是不是安全的。
///
/// search / fetch / 会话检查都只是"问一句",问两遍和问一遍的后果一模一样,
/// 所以连不上的时候可以再试。**whisper 不行**:它是全程序唯一一个会在游戏里
/// 留下痕迹的请求(给别人发传送邀请),而"连不上"并不能证明服务端没收到。
/// 宁可少发一次让用户自己再点一下,也不能替他发两次。
fn is_read_only(kind: &RequestKind) -> bool {
    !matches!(kind, RequestKind::Whisper { .. })
}

/// 这个响应是不是 Cloudflare 的拦截页;是的话整条队列停到什么时候。
///
/// 判据是"状态码 + body 是 HTML"两条都满足:光看 503 不行(服务端自己也会
/// 503),光看 HTML 也不行(200 回 HTML 那是别的毛病)。
fn hold_until(status: u16, looks_like_html: bool, now: i64) -> Option<i64> {
    if looks_like_html && (status == 403 || status == 503) {
        Some(now + CLOUDFLARE_HOLD_SECS)
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// 线程本体
// ---------------------------------------------------------------------

pub struct TradeGateway {
    transport: Box<dyn TradeTransport>,
    limiter: RateLimiter,
    session: Option<String>,
    events: Sender<GatewayEvent>,
    /// 第一次 whisper 响应回来之前是占位串。
    whisper_policy: String,
    /// 被 Cloudflare 拦住的话,停到这个时刻。期间照收请求,只是不发。
    hold_until: Option<i64>,
    queue: Vec<Pending>,
    next_sequence: u64,
}

impl TradeGateway {
    /// 起一条 `pnd-trade-gateway` 线程,返回遥控器。
    ///
    /// `events` 是广播通道的发送端,由调用方(actor)持有接收端。
    pub fn start(
        transport: Box<dyn TradeTransport>,
        budget: Budget,
        session: Option<String>,
        events: Sender<GatewayEvent>,
    ) -> GatewayHandle {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let join = std::thread::Builder::new()
            .name("pnd-trade-gateway".to_owned())
            .spawn(move || {
                let mut gateway = TradeGateway {
                    transport,
                    limiter: RateLimiter::new(budget),
                    session,
                    events,
                    whisper_policy: WHISPER_POLICY_PLACEHOLDER.to_string(),
                    hold_until: None,
                    queue: Vec::new(),
                    next_sequence: 0,
                };
                gateway.run(&rx, &thread_cancel);
            })
            .expect("spawn trade gateway thread");
        GatewayHandle {
            tx,
            cancel,
            join: Some(join),
        }
    }

    fn run(&mut self, rx: &Receiver<GatewayMessage>, cancel: &AtomicBool) {
        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }

            // 先把邮箱清空再挑活:不然一封刚投进来的"用户点击"会被压在
            // 通道里,等前面几封轮询请求全跑完才被看见。
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(message) => self.accept(message),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected || cancel.load(Ordering::Relaxed) {
                break;
            }

            let now = now_secs();
            let next_allowed = self.next_allowed(now);
            let runnable = if self.holding(now) {
                None
            } else {
                pick_runnable(&self.queue, &next_allowed, now)
            };
            if let Some(index) = runnable {
                let pending = self.queue.remove(index);
                self.execute(pending, now_secs());
                continue;
            }

            match rx.recv_timeout(self.wait_for(&next_allowed, now)) {
                Ok(message) => self.accept(message),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.drain_cancelled(rx);
    }

    fn accept(&mut self, message: GatewayMessage) {
        match message {
            GatewayMessage::Request(request) => {
                let sequence = self.next_sequence;
                self.next_sequence += 1;
                self.queue.push(Pending {
                    request,
                    sequence,
                    not_before: 0,
                    attempts: 0,
                    retried_transport: false,
                });
            }
            GatewayMessage::SetSession(session) => self.session = session,
            GatewayMessage::Wake => {}
        }
    }

    fn holding(&self, now: i64) -> bool {
        self.hold_until.is_some_and(|until| until > now)
    }

    /// 队列里出现过的每条策略,现在还要等到什么时候。
    fn next_allowed(&mut self, now: i64) -> BTreeMap<&'static str, i64> {
        let mut out: BTreeMap<&'static str, i64> = BTreeMap::new();
        for pending in &self.queue {
            let label = policy_for(&pending.request.kind);
            if out.contains_key(label) {
                continue;
            }
            let real = if label == WHISPER_POLICY_PLACEHOLDER {
                self.whisper_policy.clone()
            } else {
                label.to_string()
            };
            out.insert(label, self.limiter.next_request_time(&real, now));
        }
        out
    }

    /// 睡多久:睡到队列里最早那封能发的时刻,封顶一秒。
    fn wait_for(&self, next_allowed: &BTreeMap<&'static str, i64>, now: i64) -> Duration {
        let earliest = self
            .queue
            .iter()
            .map(|pending| {
                let allowed = next_allowed
                    .get(policy_for(&pending.request.kind))
                    .copied()
                    .unwrap_or(now);
                pending
                    .not_before
                    .max(allowed)
                    .max(self.hold_until.unwrap_or(0))
            })
            .min();
        match earliest {
            None => IDLE_WAIT,
            Some(at) => {
                let seconds = (at - now).clamp(0, IDLE_WAIT.as_secs() as i64) as u64;
                Duration::from_secs(seconds).max(MIN_WAIT)
            }
        }
    }

    /// 真正发一次请求:登记 → 发 → 结算限速头 → 回信。
    fn execute(&mut self, mut pending: Pending, now: i64) {
        let label = policy_for(&pending.request.kind);
        let policy = if label == WHISPER_POLICY_PLACEHOLDER {
            self.whisper_policy.clone()
        } else {
            label.to_string()
        };
        let session = self.session.clone();

        // whisper 没有会话根本发不出去。在花掉限速额度**之前**就回掉,
        // 而且回一个说得清的原因 —— 早先这里伪装成"连不上交易站",
        // 于是卡片上显示的是"失败(0)",看不出是会话没了。
        if matches!(pending.request.kind, RequestKind::Whisper { .. }) && session.is_none() {
            self.reply(pending, GatewayError::NoSession);
            return;
        }

        let ticket = self.limiter.insert_request(&policy, now);
        let response = match &pending.request.kind {
            RequestKind::Search { league, body_json }
            | RequestKind::SessionCheck { league, body_json } => {
                self.transport.search(league, body_json, session.as_deref())
            }
            RequestKind::Fetch { ids, search_id } | RequestKind::FetchByIds { ids, search_id } => {
                self.transport.fetch(ids, search_id, session.as_deref())
            }
            // 上面那道闸已经保证了这里一定有会话。
            RequestKind::Whisper { token, referer } => {
                self.transport
                    .whisper(token, session.as_deref().unwrap_or_default(), referer)
            }
        };

        let done_at = now_secs();
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                // 连头都没拿到,只销掉在途计数,已知的限速信息原样留着。
                self.limiter.finish_request(ticket, None, done_at);
                if is_read_only(&pending.request.kind) && !pending.retried_transport {
                    pending.retried_transport = true;
                    pending.not_before = done_at + TRANSPORT_RETRY_DELAY_SECS;
                    self.emit(GatewayEvent::Log(format!(
                        "{policy}: the request never reached the trade site ({error}) — \
                         trying once more on a new connection"
                    )));
                    // 保留原来的 sequence,和 429 重排队一样:它仍然排在
                    // 同优先级的最前面,不会被后来的请求插队。
                    self.queue.push(pending);
                    return;
                }
                self.reply(pending, GatewayError::Transport(error.to_string()));
                return;
            }
        };
        self.limiter
            .finish_request(ticket, response.rate.as_ref(), done_at);

        // whisper 的策略名是从第一次响应里学来的,学到了下次就用真名记账。
        if matches!(pending.request.kind, RequestKind::Whisper { .. })
            && let Some(rate) = &response.rate
        {
            self.whisper_policy = rate.policy.clone();
        }

        // 带了 cookie 却没有 Account 规则 = 服务端没认出这个会话。
        if session.is_some()
            && response
                .rate
                .as_ref()
                .is_some_and(|rate| !rate.mentions_account())
        {
            self.session = None;
            self.emit(GatewayEvent::SessionInvalid);
        }

        let usage = self.limiter.budget_view(&policy, done_at);
        let next_allowed_in_secs = next_allowed_in(&mut self.limiter, &policy, done_at);
        self.emit(GatewayEvent::Budget {
            policy: policy.clone(),
            usage,
            next_allowed_in_secs,
        });

        if response.status == 429 {
            pending.attempts += 1;
            let retry_in = backoff_after_429(
                pending.attempts,
                response
                    .rate
                    .as_ref()
                    .and_then(|rate| rate.retry_after_secs),
            );
            pending.not_before = done_at + i64::from(retry_in);
            // 保留原来的 sequence:重排队之后它仍然排在同优先级的最前面。
            self.queue.push(pending);
            self.emit(GatewayEvent::RateLimited { policy, retry_in });
            return;
        }

        if let Some(until) = hold_until(response.status, response.looks_like_html, done_at) {
            self.hold_until = Some(until);
            self.emit(GatewayEvent::CloudflareBlocked { until });
            self.reply(pending, GatewayError::CloudflareHold);
            return;
        }

        // 会话检查问的是限速头,不是响应体:服务端就算回一句 400,头里
        // 有没有 `Account` 一样说明了 cookie 的死活,所以它不走下面那条
        // "非 2xx 就当失败"的路。
        if matches!(pending.request.kind, RequestKind::SessionCheck { .. }) {
            let kind = ReplyKind::SessionCheck(Ok(session_check_outcome(&response)));
            let _ = pending.request.reply.send(GatewayReply {
                tag: pending.request.tag,
                kind,
            });
            return;
        }

        if !response.is_success() {
            // body 一起带上:状态码只说"被拒了",body 才说"为什么"。
            self.reply(
                pending,
                GatewayError::Status {
                    status: response.status,
                    excerpt: response.body_excerpt(),
                },
            );
            return;
        }

        let kind = success_reply(&pending.request.kind, &response);
        let _ = pending.request.reply.send(GatewayReply {
            tag: pending.request.tag,
            kind,
        });
    }

    fn reply(&self, pending: Pending, error: GatewayError) {
        let kind = error_reply(&pending.request.kind, error);
        let _ = pending.request.reply.send(GatewayReply {
            tag: pending.request.tag,
            kind,
        });
    }

    fn emit(&self, event: GatewayEvent) {
        let _ = self.events.send(event);
    }

    /// 关门前给每一封还在排队的请求回一句 `Cancelled`:上层可能正等着回信,
    /// 没有这一步它就会一直挂着 `in_flight`。
    fn drain_cancelled(&mut self, rx: &Receiver<GatewayMessage>) {
        let queued: Vec<Pending> = self.queue.drain(..).collect();
        for pending in queued {
            self.reply(pending, GatewayError::Cancelled);
        }
        while let Ok(message) = rx.try_recv() {
            if let GatewayMessage::Request(request) = message {
                let kind = error_reply(&request.kind, GatewayError::Cancelled);
                let _ = request.reply.send(GatewayReply {
                    tag: request.tag,
                    kind,
                });
            }
        }
    }
}

/// 一封响应 → 会话检查要的那三样。没有限速头(响应根本不是交易站回的)
/// 就是"没有规则",调用方据此说"看不出来"。
fn session_check_outcome(response: &TradeResponse) -> SessionCheckOutcome {
    let rate = response.rate.as_ref();
    SessionCheckOutcome {
        status: response.status,
        rules: rate.map(|rate| rate.rules.clone()).unwrap_or_default(),
        mentions_account: rate.is_some_and(pnd_trade::RateHeaders::mentions_account),
    }
}

/// 2xx 响应 → 解析好的回信。解析失败也是一封回信,不是 panic。
fn success_reply(kind: &RequestKind, response: &TradeResponse) -> ReplyKind {
    match kind {
        // `execute` 在到这儿之前就把会话检查回掉了。
        RequestKind::SessionCheck { .. } => {
            ReplyKind::SessionCheck(Ok(session_check_outcome(response)))
        }
        RequestKind::Search { .. } => ReplyKind::Search(
            parse_search_response(&response.body)
                .map(|parsed| SearchOutcome {
                    id: parsed.id,
                    total: parsed.total,
                    result: parsed.result,
                })
                .map_err(|error| GatewayError::Parse(error.to_string())),
        ),
        RequestKind::Fetch { .. } => ReplyKind::Fetch(
            parse_fetch_response(&response.body)
                .map_err(|error| GatewayError::Parse(error.to_string())),
        ),
        RequestKind::FetchByIds { ids, .. } => ReplyKind::FetchByIds(
            parse_fetch_response_by_id(ids, &response.body)
                .map_err(|error| GatewayError::Parse(error.to_string())),
        ),
        RequestKind::Whisper { .. } => ReplyKind::Whisper(Ok(response.status)),
    }
}

fn error_reply(kind: &RequestKind, error: GatewayError) -> ReplyKind {
    match kind {
        RequestKind::Search { .. } => ReplyKind::Search(Err(error)),
        RequestKind::SessionCheck { .. } => ReplyKind::SessionCheck(Err(error)),
        RequestKind::Fetch { .. } => ReplyKind::Fetch(Err(error)),
        RequestKind::FetchByIds { .. } => ReplyKind::FetchByIds(Err(error)),
        RequestKind::Whisper { .. } => ReplyKind::Whisper(Err(error)),
    }
}

#[cfg(test)]
mod gateway_tests {
    use std::sync::Mutex;

    use super::*;

    /// 一个"头几封请求连不上、之后正常"的假交易站。
    ///
    /// 复现的是 2026-09-08 早上那条错误:`io: unexpected end of file` ——
    /// rustls 在**握手途中**读到 EOF(ureq 是连上就立刻握手的),也就是说
    /// 那封请求一个 HTTP 字节都没发出去。
    struct FlakyTrade {
        fails_left: Mutex<usize>,
        calls: Arc<Mutex<usize>>,
    }

    impl FlakyTrade {
        fn new(fails: usize) -> (Box<FlakyTrade>, Arc<Mutex<usize>>) {
            let calls = Arc::new(Mutex::new(0));
            (
                Box::new(FlakyTrade {
                    fails_left: Mutex::new(fails),
                    calls: Arc::clone(&calls),
                }),
                calls,
            )
        }

        fn answer(&self) -> Result<TradeResponse, TransportError> {
            *self.calls.lock().unwrap() += 1;
            let mut left = self.fails_left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(TransportError::Unreachable(
                    "io: unexpected end of file".to_string(),
                ));
            }
            Ok(TradeResponse {
                status: 200,
                body: br#"{"result":[]}"#.to_vec(),
                rate: None,
                looks_like_html: false,
            })
        }
    }

    impl TradeTransport for FlakyTrade {
        fn search(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> Result<TradeResponse, TransportError> {
            self.answer()
        }
        fn fetch(
            &self,
            _: &[String],
            _: &str,
            _: Option<&str>,
        ) -> Result<TradeResponse, TransportError> {
            self.answer()
        }
        fn whisper(&self, _: &str, _: &str, _: &str) -> Result<TradeResponse, TransportError> {
            self.answer()
        }
    }

    /// 一个不起线程的网关:测试直接调 `execute`,一封一封地看它怎么处置。
    fn offline_gateway(
        transport: Box<dyn TradeTransport>,
    ) -> (TradeGateway, Receiver<GatewayEvent>) {
        let (events, rx) = channel();
        (
            TradeGateway {
                transport,
                limiter: RateLimiter::default(),
                session: Some("cookie".to_string()),
                events,
                whisper_policy: WHISPER_POLICY_PLACEHOLDER.to_string(),
                hold_until: None,
                queue: Vec::new(),
                next_sequence: 0,
            },
            rx,
        )
    }

    fn queued(kind: RequestKind) -> (Pending, Receiver<GatewayReply>) {
        let (tx, rx) = channel();
        (
            Pending {
                request: GatewayRequest {
                    kind,
                    priority: Priority::Background,
                    reply: tx,
                    tag: RequestTag::sweep(7, "observe-recheck"),
                },
                sequence: 0,
                not_before: 0,
                attempts: 0,
                retried_transport: false,
            },
            rx,
        )
    }

    /// 一封读请求死在连接上(还没拿到任何响应字节),要再试一次。
    ///
    /// 为什么这条最要紧:回查是一分钟一次的独立时间线,ureq 的连接池
    /// 只留 15 秒,所以**每一封回查都是一次全新的 TLS 握手** —— 握手一失手,
    /// 整批挂单这一分钟就白等了,而且没有任何东西会把它补回来。
    /// 兜底轮询那一轮是 1 次 search + 3 次 fetch 挤在一秒里,复用同一条连接,
    /// 所以同样的网络抖动几乎砸不到它 —— 主人那六个小时里 discover 一直好好的,
    /// 回查一条都没成,差别就在这儿。
    #[test]
    fn a_read_request_that_dies_on_the_connection_gets_one_more_try() {
        let (transport, calls) = FlakyTrade::new(1);
        let (mut gateway, _events) = offline_gateway(transport);
        let (pending, replies) = queued(RequestKind::FetchByIds {
            ids: vec!["a".to_string()],
            search_id: "q".to_string(),
        });

        gateway.execute(pending, 1_000);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(
            replies.try_recv().is_err(),
            "第一次连不上还不算结论,不该现在就回信说失败"
        );
        assert_eq!(gateway.queue.len(), 1, "这一批该回到队列里再试一次");
        assert!(gateway.queue[0].retried_transport, "重试只给一次,得记下来");

        let retry = gateway.queue.remove(0);
        gateway.execute(retry, 1_001);
        assert_eq!(*calls.lock().unwrap(), 2);
        let reply = replies.try_recv().expect("重试之后该有回信了");
        assert!(
            matches!(reply.kind, ReplyKind::FetchByIds(Ok(_))),
            "{:?}",
            reply.kind
        );
        assert!(gateway.queue.is_empty());
    }

    /// 再试一次还是连不上就认了:上层收到失败,下一轮扫描会重新问这批挂单。
    #[test]
    fn a_second_connection_failure_is_reported_instead_of_retried_forever() {
        let (transport, calls) = FlakyTrade::new(9);
        let (mut gateway, _events) = offline_gateway(transport);
        let (pending, replies) = queued(RequestKind::FetchByIds {
            ids: vec!["a".to_string()],
            search_id: "q".to_string(),
        });

        gateway.execute(pending, 1_000);
        let retry = gateway.queue.remove(0);
        gateway.execute(retry, 1_001);

        assert_eq!(*calls.lock().unwrap(), 2, "最多两次,不能没完没了");
        assert!(gateway.queue.is_empty());
        let reply = replies.try_recv().expect("该回一封失败的信");
        match reply.kind {
            ReplyKind::FetchByIds(Err(error)) => {
                assert!(
                    error.to_string().contains("unexpected end of file"),
                    "{error}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// whisper 不重试。它是唯一一个会在**游戏里**留下痕迹的请求(传送邀请),
    /// 而"连不上"并不能证明服务端没收到 —— 宁可少发一次,也不能发两次。
    #[test]
    fn a_whisper_is_never_retried_behind_the_users_back() {
        let (transport, calls) = FlakyTrade::new(1);
        let (mut gateway, _events) = offline_gateway(transport);
        let (pending, replies) = queued(RequestKind::Whisper {
            token: "t".to_string(),
            referer: "r".to_string(),
        });

        gateway.execute(pending, 1_000);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(gateway.queue.is_empty(), "whisper 不回队列");
        assert!(matches!(
            replies.try_recv().expect("回信").kind,
            ReplyKind::Whisper(Err(GatewayError::Transport(_)))
        ));
    }

    fn pending(priority: Priority, sequence: u64, not_before: i64, kind: RequestKind) -> Pending {
        // 这几个测试只排队、不执行,所以没人会往这个回信地址发东西,
        // 接收端就地丢掉也没关系。
        let (tx, _rx) = channel();
        Pending {
            request: GatewayRequest {
                kind,
                priority,
                reply: tx,
                tag: RequestTag::standalone("test"),
            },
            sequence,
            not_before,
            attempts: 0,
            retried_transport: false,
        }
    }

    fn search() -> RequestKind {
        RequestKind::Search {
            league: "Forbidden Rites".to_string(),
            body_json: "{}".to_string(),
        }
    }

    fn fetch() -> RequestKind {
        RequestKind::Fetch {
            ids: vec!["a".to_string()],
            search_id: "q".to_string(),
        }
    }

    #[test]
    fn policy_names_follow_the_request_kind() {
        assert_eq!(policy_for(&search()), SEARCH_POLICY);
        assert_eq!(policy_for(&fetch()), FETCH_POLICY);
        // 会话检查花的是 search 的额度:它就是一次 search。
        assert_eq!(
            policy_for(&RequestKind::SessionCheck {
                league: "Forbidden Rites".to_string(),
                body_json: "{}".to_string()
            }),
            SEARCH_POLICY
        );
        assert_eq!(
            policy_for(&RequestKind::Whisper {
                token: "t".to_string(),
                referer: "r".to_string()
            }),
            WHISPER_POLICY_PLACEHOLDER
        );
    }

    #[test]
    fn picks_the_highest_priority_request() {
        let queue = vec![
            pending(Priority::PollSearch, 0, 0, search()),
            pending(Priority::User, 1, 0, fetch()),
            pending(Priority::PollFetch, 2, 0, fetch()),
        ];
        let allowed = BTreeMap::new();
        assert_eq!(pick_runnable(&queue, &allowed, 100), Some(1));
    }

    #[test]
    fn same_priority_goes_first_come_first_served() {
        let queue = vec![
            pending(Priority::PollSearch, 7, 0, search()),
            pending(Priority::PollSearch, 3, 0, search()),
        ];
        assert_eq!(pick_runnable(&queue, &BTreeMap::new(), 100), Some(1));
    }

    #[test]
    fn skips_requests_whose_policy_is_still_cooling_down() {
        let queue = vec![
            pending(Priority::PollSearch, 0, 0, search()),
            pending(Priority::Background, 1, 0, fetch()),
        ];
        let mut allowed = BTreeMap::new();
        allowed.insert(SEARCH_POLICY, 200);
        allowed.insert(FETCH_POLICY, 100);
        // search 还要等到 200,所以优先级更低的 fetch 先走。
        assert_eq!(pick_runnable(&queue, &allowed, 100), Some(1));

        allowed.insert(FETCH_POLICY, 300);
        assert_eq!(pick_runnable(&queue, &allowed, 100), None);
    }

    #[test]
    fn a_429_backoff_holds_the_request_back() {
        let queue = vec![pending(Priority::User, 0, 150, search())];
        assert_eq!(pick_runnable(&queue, &BTreeMap::new(), 100), None);
        assert_eq!(pick_runnable(&queue, &BTreeMap::new(), 150), Some(0));
    }

    /// 会话检查只看限速头:规则里有 `Account` 就是认得,没有就是不认得,
    /// 连限速头都没有(响应压根不是交易站回的)就两样都没有。
    #[test]
    fn a_session_check_reads_the_rule_names_off_the_headers() {
        let response = |rules: &str| TradeResponse {
            status: 200,
            body: b"{}".to_vec(),
            rate: pnd_trade::parse_rate_headers([
                ("X-Rate-Limit-Policy", SEARCH_POLICY),
                ("X-Rate-Limit-Rules", rules),
            ]),
            looks_like_html: false,
        };

        let good = session_check_outcome(&response("Ip,Account"));
        assert!(good.mentions_account);
        assert_eq!(good.rules, ["Ip", "Account"]);
        assert_eq!(good.status, 200);

        let anonymous = session_check_outcome(&response("Ip"));
        assert!(!anonymous.mentions_account);
        assert_eq!(anonymous.rules, ["Ip"]);

        let headerless = session_check_outcome(&TradeResponse {
            status: 403,
            body: b"{}".to_vec(),
            rate: None,
            looks_like_html: false,
        });
        assert!(!headerless.mentions_account);
        assert!(headerless.rules.is_empty());
        assert_eq!(headerless.status, 403);
    }

    /// 没有会话的 whisper 连发都不该发:回一个说得清的 `NoSession`,
    /// 而且不占限速额度。早先它伪装成"连不上交易站",于是卡片上只剩
    /// 一个 `失败(0)`,看不出是会话没了。
    #[test]
    fn a_whisper_without_a_session_is_refused_before_it_costs_anything() {
        /// 一被调用就说话的假交易站:这个测试要证明的正是"它没被调用"。
        struct NeverCalled(Arc<AtomicBool>);

        impl TradeTransport for NeverCalled {
            fn search(
                &self,
                _league: &str,
                _body_json: &str,
                _session: Option<&str>,
            ) -> Result<TradeResponse, TransportError> {
                self.0.store(true, Ordering::Relaxed);
                Err(TransportError::Unreachable("nope".to_string()))
            }

            fn fetch(
                &self,
                _ids: &[String],
                _search_id: &str,
                _session: Option<&str>,
            ) -> Result<TradeResponse, TransportError> {
                self.0.store(true, Ordering::Relaxed);
                Err(TransportError::Unreachable("nope".to_string()))
            }

            fn whisper(
                &self,
                _token: &str,
                _session: &str,
                _referer: &str,
            ) -> Result<TradeResponse, TransportError> {
                self.0.store(true, Ordering::Relaxed);
                Err(TransportError::Unreachable("nope".to_string()))
            }
        }

        let called = Arc::new(AtomicBool::new(false));
        let (events_tx, _events_rx) = channel();
        let (reply_tx, reply_rx) = channel();
        let gateway = TradeGateway::start(
            Box::new(NeverCalled(Arc::clone(&called))),
            Budget::default(),
            // 关键:网关手上没有会话。
            None,
            events_tx,
        );
        gateway.submit(GatewayRequest {
            kind: RequestKind::Whisper {
                token: "tok".to_string(),
                referer: "https://example.com".to_string(),
            },
            priority: Priority::User,
            reply: reply_tx,
            tag: RequestTag::alert(7, "test"),
        });

        let reply = reply_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a reply");
        drop(gateway);

        assert_eq!(reply.tag.alert_id, Some(7));
        assert!(
            matches!(reply.kind, ReplyKind::Whisper(Err(GatewayError::NoSession))),
            "{:?}",
            reply.kind
        );
        assert!(!called.load(Ordering::Relaxed), "一个请求都不该发出去");
    }

    /// 非 2xx 的时候,body 里那句话要跟着错误一起走 —— 它才是"为什么"。
    #[test]
    fn a_status_error_carries_the_body_and_reads_well_without_one() {
        let with_body = GatewayError::Status {
            status: 403,
            excerpt: r#"{"error":{"code":6}}"#.to_string(),
        };
        assert_eq!(
            with_body.to_string(),
            r#"trade site answered 403: {"error":{"code":6}}"#
        );
        // body 是空的就别留一个孤零零的冒号。
        assert_eq!(
            GatewayError::Status {
                status: 500,
                excerpt: String::new(),
            }
            .to_string(),
            "trade site answered 500"
        );
    }

    #[test]
    fn only_an_html_403_or_503_holds_the_queue() {
        assert_eq!(hold_until(403, true, 1_000), Some(1_300));
        assert_eq!(hold_until(503, true, 1_000), Some(1_300));
        assert_eq!(hold_until(403, false, 1_000), None);
        assert_eq!(hold_until(200, true, 1_000), None);
        assert_eq!(hold_until(429, true, 1_000), None);
    }

    /// 一个 429 之后,广播里必须带上"还要等几秒" —— 界面上那句
    /// "next allowed in N s" 就是从这里来的。
    #[test]
    fn a_429_tells_the_ui_how_long_it_has_to_wait() {
        /// 只会回 429 的假交易站,响应头是今天从服务端量到的那一组。
        struct AlwaysRateLimited;

        fn rate_limited() -> Result<TradeResponse, TransportError> {
            let headers = [
                ("X-Rate-Limit-Policy", "trade-search-request-limit"),
                ("X-Rate-Limit-Rules", "Ip"),
                ("X-Rate-Limit-Ip", "5:10:60,15:60:300"),
                ("X-Rate-Limit-Ip-State", "5:10:60,6:60:0"),
                ("Retry-After", "30"),
            ];
            Ok(TradeResponse {
                status: 429,
                body: b"{}".to_vec(),
                rate: pnd_trade::parse_rate_headers(headers),
                looks_like_html: false,
            })
        }

        impl TradeTransport for AlwaysRateLimited {
            fn search(
                &self,
                _league: &str,
                _body_json: &str,
                _session: Option<&str>,
            ) -> Result<TradeResponse, TransportError> {
                rate_limited()
            }

            fn fetch(
                &self,
                _ids: &[String],
                _search_id: &str,
                _session: Option<&str>,
            ) -> Result<TradeResponse, TransportError> {
                rate_limited()
            }

            fn whisper(
                &self,
                _token: &str,
                _session: &str,
                _referer: &str,
            ) -> Result<TradeResponse, TransportError> {
                rate_limited()
            }
        }

        let (events_tx, events_rx) = channel();
        let (reply_tx, _reply_rx) = channel();
        let gateway = TradeGateway::start(
            Box::new(AlwaysRateLimited),
            Budget::default(),
            None,
            events_tx,
        );
        gateway.submit(GatewayRequest {
            kind: search(),
            priority: Priority::PollSearch,
            reply: reply_tx,
            tag: RequestTag::standalone("test"),
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut seen: Vec<GatewayEvent> = Vec::new();
        while std::time::Instant::now() < deadline {
            match events_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    let done = matches!(event, GatewayEvent::Budget { .. });
                    seen.push(event);
                    if done {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        drop(gateway);

        let waited = seen
            .iter()
            .find_map(|event| match event {
                GatewayEvent::Budget {
                    next_allowed_in_secs,
                    ..
                } => Some(*next_allowed_in_secs),
                _ => None,
            })
            .expect("a Budget broadcast");
        // 服务端说 Retry-After: 30,限速器再加一秒的安全垫。
        assert_eq!(waited, Some(30), "saw {seen:#?}");
    }
}
