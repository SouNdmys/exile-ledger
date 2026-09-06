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

use pnd_domain::{ListingSummary, WatchId};
use pnd_trade::{
    BucketUsage, Budget, FETCH_POLICY, RateLimiter, SEARCH_POLICY, TradeClient, TradeResponse,
    TransportError, backoff_after_429, parse_fetch_response, parse_search_response,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTag {
    pub watch_id: Option<WatchId>,
    pub alert_id: Option<i64>,
    pub label: &'static str,
}

/// 三种请求。请求体已经拼好了:网关不认识查询 JSON 长什么样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    Search { league: String, body_json: String },
    Fetch { ids: Vec<String>, search_id: String },
    Whisper { token: String, referer: String },
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

/// 一次请求失败的原因。分这几类是因为上层的处置不同:`Transport` 和 `Status`
/// 下一轮再来,`CloudflareHold` 要告诉用户"被拦了,程序在等",
/// `Cancelled` 则连日志都不用记 —— 那是我们自己要关的。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GatewayError {
    #[error("could not reach the trade site: {0}")]
    Transport(String),
    #[error("trade site answered {0}")]
    Status(u16),
    #[error("could not read the response: {0}")]
    Parse(String),
    #[error("held back: the trade site answered with a Cloudflare page")]
    CloudflareHold,
    #[error("cancelled")]
    Cancelled,
}

/// 回信的内容,形状跟着请求走。
#[derive(Debug)]
pub enum ReplyKind {
    Search(Result<SearchOutcome, GatewayError>),
    Fetch(Result<Vec<ListingSummary>, GatewayError>),
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
        RequestKind::Search { .. } => SEARCH_POLICY,
        RequestKind::Fetch { .. } => FETCH_POLICY,
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

        let ticket = self.limiter.insert_request(&policy, now);
        let response = match &pending.request.kind {
            RequestKind::Search { league, body_json } => {
                self.transport.search(league, body_json, session.as_deref())
            }
            RequestKind::Fetch { ids, search_id } => {
                self.transport.fetch(ids, search_id, session.as_deref())
            }
            RequestKind::Whisper { token, referer } => match session.as_deref() {
                Some(session) => self.transport.whisper(token, session, referer),
                // whisper 没有会话根本发不出去,当传输错误回掉。
                None => Err(TransportError::Unreachable(
                    "whisper needs a POESESSID".to_string(),
                )),
            },
        };

        let done_at = now_secs();
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                // 连头都没拿到,只销掉在途计数,已知的限速信息原样留着。
                self.limiter.finish_request(ticket, None, done_at);
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

        if !response.is_success() {
            self.reply(pending, GatewayError::Status(response.status));
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

/// 2xx 响应 → 解析好的回信。解析失败也是一封回信,不是 panic。
fn success_reply(kind: &RequestKind, response: &TradeResponse) -> ReplyKind {
    match kind {
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
        RequestKind::Whisper { .. } => ReplyKind::Whisper(Ok(response.status)),
    }
}

fn error_reply(kind: &RequestKind, error: GatewayError) -> ReplyKind {
    match kind {
        RequestKind::Search { .. } => ReplyKind::Search(Err(error)),
        RequestKind::Fetch { .. } => ReplyKind::Fetch(Err(error)),
        RequestKind::Whisper { .. } => ReplyKind::Whisper(Err(error)),
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::*;

    fn pending(priority: Priority, sequence: u64, not_before: i64, kind: RequestKind) -> Pending {
        // 这几个测试只排队、不执行,所以没人会往这个回信地址发东西,
        // 接收端就地丢掉也没关系。
        let (tx, _rx) = channel();
        Pending {
            request: GatewayRequest {
                kind,
                priority,
                reply: tx,
                tag: RequestTag {
                    watch_id: None,
                    alert_id: None,
                    label: "test",
                },
            },
            sequence,
            not_before,
            attempts: 0,
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
            tag: RequestTag {
                watch_id: None,
                alert_id: None,
                label: "test",
            },
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
