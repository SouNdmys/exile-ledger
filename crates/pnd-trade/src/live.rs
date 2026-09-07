//! Live Search 的 WebSocket 传输层:握手、收一条消息、算重连等待。
//!
//! 这一层和 [`crate::client`] 是一个脾气:**不睡眠、不重试、不循环**。
//! 连不上就把分类好的失败原因交回去,断线退避和重连归
//! `pnd-runtime/src/live_worker.rs` 管 —— 那里才知道全局有几条连接、
//! 该不该把这条搜索标成"WS 不健康"。
//!
//! 三处非写不可的细节:
//!
//! 1. **握手请求得手搓**。tungstenite 只有在你把 URL 字符串丢给它的时候才
//!    替你补 `Host`/`Connection`/`Upgrade`/`Sec-WebSocket-Version`/
//!    `Sec-WebSocket-Key`;一旦改用 `http::Request` 自己带头(我们必须带,
//!    因为要塞 Cookie / Origin / Referer),它会检查这五个头**在不在**,
//!    不在就直接报 `InvalidHeader`。所以下面一个都不能漏。
//! 2. **握手完了要给底下的 TcpStream 设读超时**。tungstenite 的 `read()` 是
//!    阻塞的,没有超时就没法响应"停一下"这种取消请求。设了超时之后,读不到
//!    东西每隔 `read_timeout` 会回一次 [`LiveMessage::Idle`],worker 借这个
//!    间隙看一眼取消标志。超时中断的是 TLS 记录读到一半的状态,rustls 和
//!    tungstenite 都会把半截数据留在自己的缓冲里,下次接着读,不会错帧。
//! 3. **POESESSID 只出现在请求头里**。所有公开类型的 `Debug`/`Display`
//!    都不会印出它 —— [`LiveConfig`] 的 `Debug` 是手写的,握手失败带回来的
//!    body 摘要是服务端的话,不含我们发出去的头。
//!
//! 已知的坑(计划里记着):tungstenite **不跟 Windows 的系统代理设置**,
//! ureq 跟。用户如果开的是 HTTP 代理而不是 TUN 模式,live 会连不上 →
//! 退避 → 轮询兜底照常工作。

use std::fmt;
use std::io;
use std::net::TcpStream;
use std::time::Duration;

use pnd_domain::{SearchRef, encode_league_path, live_page_url};
use serde_json::Value;
use thiserror::Error;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Live Search 的 WebSocket 根路径(poe2 在 `trade2` 下,和 poe1 是两套)。
const WS_BASE: &str = "wss://www.pathofexile.com/api/trade2/live/poe2";

/// 握手要带的三个"我是从网页来的"字段里的两个常量。
const HOST: &str = "www.pathofexile.com";
const ORIGIN: &str = "https://www.pathofexile.com";

/// 服务端规定:同一个账号最多同时开 20 条 live 连接。
///
/// 这是**硬上限**,不是我们的用量。程序自己的温柔阈值是 5 条,存在
/// `settings.json` 的 `watcher.max_live_connections` 里 —— 那个可以调,
/// 这个不行,所以放在代码里。
pub const MAX_LIVE_CONNECTIONS_PER_ACCOUNT: usize = 20;

/// 底层 socket 的默认读超时。
///
/// 30 秒是计划里定的:够长,不会把正常的空闲期当成故障;够短,取消一条 live
/// 最多等半分钟。服务端安静的时候本来就什么都不发,超时是常态不是异常。
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 握手失败时,body 最多留这么长给人看。够认出"这是 Cloudflare 的页面"
/// 或者"这是 GGG 的一句 JSON 错误",又不至于把整张 HTML 灌进日志。
const BODY_EXCERPT_CHARS: usize = 200;

/// 描述一条消息时,比这短的值可以原样写进日志,再长就只报长度。
///
/// 20 个字符放得下 `true`、`1757203200`、`"ok"` 这种一眼能懂的东西,
/// 而放不下任何一个 token —— JWT 光是头一段就比它长。
const SAFE_VALUE_CHARS: usize = 20;

// ---------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------

/// 开一条 live 连接需要知道的全部东西。
///
/// `Debug` 是手写的:这个结构会出现在日志和错误上下文里,而它带着 POESESSID。
#[derive(Clone)]
pub struct LiveConfig {
    pub league: String,
    pub search_id: String,
    /// 空串 = 匿名握手(不发 Cookie 头)。服务端几乎肯定会拒,但这条路径
    /// 必须能走通并且报得清楚,不能因为没会话就 panic。
    pub poesessid: String,
    pub user_agent: String,
    pub read_timeout: Duration,
}

impl LiveConfig {
    /// 从一条搜索引用造配置,读超时取 [`DEFAULT_READ_TIMEOUT`]。
    #[must_use]
    pub fn new(
        search: &SearchRef,
        poesessid: impl Into<String>,
        user_agent: impl Into<String>,
    ) -> Self {
        Self {
            league: search.league.clone(),
            search_id: search.search_id.clone(),
            poesessid: poesessid.into(),
            user_agent: user_agent.into(),
            read_timeout: DEFAULT_READ_TIMEOUT,
        }
    }

    /// 这次握手会不会带 Cookie。
    #[must_use]
    pub fn has_session(&self) -> bool {
        !self.poesessid.trim().is_empty()
    }

    fn search_ref(&self) -> SearchRef {
        SearchRef {
            league: self.league.clone(),
            search_id: self.search_id.clone(),
        }
    }
}

impl fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveConfig")
            .field("league", &self.league)
            .field("search_id", &self.search_id)
            // 只说有没有,绝不说是什么。
            .field(
                "poesessid",
                &if self.has_session() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .field("user_agent", &self.user_agent)
            .field("read_timeout", &self.read_timeout)
            .finish()
    }
}

/// live 接口地址。联赛名和搜索页一样要 URL 编码。
#[must_use]
pub fn live_ws_url(league: &str, search_id: &str) -> String {
    format!("{WS_BASE}/{}/{search_id}", encode_league_path(league))
}

// ---------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------

/// 从 live 连接上读到的一条东西。
///
/// `Debug` 是手写的:[`LiveMessage::Subscribed`] 里那串是服务端发的凭证,
/// 而这个类型会出现在日志和 `assert_eq!` 的失败输出里。
#[derive(Clone, PartialEq, Eq)]
pub enum LiveMessage {
    /// `{"new":["id1","id2",…]}` —— 有新挂单,按 10 个一批去 fetch。
    /// 空表也是合法的推送,照样回 `New(vec![])`。
    New(Vec<String>),
    /// 刚连上时服务端立刻回的 `{"result":"<JWT>"}` —— 这条搜索的订阅回执。
    ///
    /// 实测那个 JWT 的 payload 里 `iss` 就是搜索 id。我们不用它做任何事
    /// (推送本身不需要它),但认出来是必须的:早先它落进 `Other`,
    /// 于是**整串 token 被原样写进了状态栏**。
    Subscribed { token: String },
    /// 读超时:这段时间服务端什么都没说。连接是好的,只是没货。
    Idle,
    /// 认得是 JSON、但不是我们认识的形状(比如服务端将来加的心跳)。
    /// 不当错误:接口随时可能多一种消息,为此断线不划算。
    ///
    /// 带的是 [`describe_live_message`] 给的**描述**(键名 + 值长度),
    /// 不是原文 —— 不认识的消息里可能有凭证,日志不该替服务端保管它。
    Other(String),
}

impl fmt::Debug for LiveMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveMessage::New(ids) => f.debug_tuple("New").field(ids).finish(),
            // 只说有多长,绝不说是什么。
            LiveMessage::Subscribed { token } => f
                .debug_struct("Subscribed")
                .field("token", &format!("<{} chars>", token.chars().count()))
                .finish(),
            LiveMessage::Idle => f.write_str("Idle"),
            LiveMessage::Other(description) => f.debug_tuple("Other").field(description).finish(),
        }
    }
}

/// 消息压根不是 JSON。
///
/// 单独一个类型是因为 [`parse_live_message`] 是纯函数,要能脱离连接单测;
/// 落到连接上时它会变成 [`LiveError::Protocol`]。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("live message is not JSON: {0}")]
pub struct ProtocolError(pub String);

/// 解析一条文本消息。纯函数,不碰网络。
pub fn parse_live_message(text: &str) -> Result<LiveMessage, ProtocolError> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| ProtocolError(error.to_string()))?;
    if let Some(items) = value.get("new").and_then(Value::as_array) {
        let ids = items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        return Ok(LiveMessage::New(ids));
    }
    // 只有"`result` 是个字符串"才算订阅回执:哪天服务端拿这个键装别的东西
    // (对象、数组),它就该老老实实走下面那条"不认识"的路。
    if let Some(token) = value.get("result").and_then(Value::as_str) {
        return Ok(LiveMessage::Subscribed {
            token: token.to_string(),
        });
    }
    Ok(LiveMessage::Other(describe_live_message(text)))
}

/// 一条消息的"体检报告":有哪些键、每个值多长。**不回显长值。**
///
/// 为什么要有它:服务端连上就发 `{"result":"<JWT>"}`,而早先那条日志是把
/// 原文抄进去的 —— 一串能代表这次订阅的凭证就这么进了状态栏和日志文件。
/// 认得的形状我们照常解析,不认得的只报形状:键名是服务端定的,可以说;
/// 值是内容,超过 [`SAFE_VALUE_CHARS`] 个字符一律只说长度。
#[must_use]
pub fn describe_live_message(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return format!("not JSON ({} chars)", text.chars().count());
    };
    match &value {
        Value::Object(fields) => {
            // 显式按键名排序:serde_json 的对象顺序取决于有没有开 `preserve_order`,
            // 而那个特性是整个工作区一起决定的(gpui 会打开它),单独编译这个
            // crate 和整仓编译会得到两种顺序。日志行不该随编译方式变。
            let mut sorted: Vec<(&String, &Value)> = fields.iter().collect();
            sorted.sort_by(|left, right| left.0.cmp(right.0));
            let keys: Vec<String> = sorted.iter().map(|(key, _)| format!("{key:?}")).collect();
            let mut out = format!("keys=[{}]", keys.join(", "));
            for (key, value) in sorted {
                out.push(' ');
                out.push_str(&describe_field(key, value));
            }
            out
        }
        Value::Array(items) => format!("array len={}", items.len()),
        Value::String(text) => format!("string len={}", text.chars().count()),
        other => format!("bare {}", type_name(other)),
    }
}

/// 一个字段 → `key=值` 或者 `key_len=N`。
fn describe_field(key: &str, value: &Value) -> String {
    let (len, rendered) = match value {
        Value::String(text) => (text.chars().count(), format!("{text:?}")),
        Value::Array(items) => (items.len(), String::new()),
        Value::Object(fields) => (fields.len(), String::new()),
        other => {
            let rendered = other.to_string();
            (rendered.chars().count(), rendered)
        }
    };
    // 短到一眼能懂的标量原样写出来;长的、以及数组/对象这种"长度才是重点"
    // 的东西,只报长度。
    if !rendered.is_empty() && len < SAFE_VALUE_CHARS {
        format!("{key}={rendered}")
    } else {
        format!("{key}_len={len}")
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------

/// live 连接能出的岔子。
///
/// 分这么细是为了让 worker 能照抄网关对 HTTP 的处置:401/403 且不是 HTML
/// 说明会话不行了(停用 cookie、提示换新的);403/503 且是 HTML 说明被
/// Cloudflare 拦了(停 5 分钟,不硬刷);其余的一律退避重连。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LiveError {
    /// 握手就被拒了 —— 连 WebSocket 都没升级成功,服务端回的是一个普通 HTTP 响应。
    #[error("live handshake refused with HTTP {status}: {body_excerpt}")]
    Handshake {
        status: u16,
        looks_like_html: bool,
        retry_after: Option<u64>,
        body_excerpt: String,
    },
    #[error("live TLS failed: {0}")]
    Tls(String),
    #[error("live socket i/o failed: {0}")]
    Io(String),
    /// 请求拼不出来、或者对面说的话不合协议。
    #[error("live protocol error: {0}")]
    Protocol(String),
    /// 服务端主动关了连接。
    #[error("live connection closed by the server ({}): {reason}",
        code.map_or_else(|| "no code".to_string(), |c| c.to_string()))]
    Closed { code: Option<u16>, reason: String },
}

impl From<ProtocolError> for LiveError {
    fn from(error: ProtocolError) -> Self {
        LiveError::Protocol(error.0)
    }
}

impl LiveError {
    /// 会话不行了(401/403 而且回的是 JSON 不是拦截页)。
    ///
    /// 和网关对 HTTP 的判断一个口径:这种失败重试多少次都一样,
    /// 该做的是停用 cookie、退回匿名轮询、在设置页提示换一个新的 POESESSID。
    #[must_use]
    pub fn is_session_problem(&self) -> bool {
        matches!(
            self,
            LiveError::Handshake {
                status: 401 | 403,
                looks_like_html: false,
                ..
            }
        )
    }

    /// 被 Cloudflare 拦了(403/503 而且回的是 HTML 页)。
    #[must_use]
    pub fn is_cloudflare(&self) -> bool {
        matches!(
            self,
            LiveError::Handshake {
                status: 403 | 503,
                looks_like_html: true,
                ..
            }
        )
    }

    /// 服务端说的 `Retry-After`(秒)。有它就该听它的,别用自己算的退避。
    #[must_use]
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            LiveError::Handshake { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------
// 重连退避
// ---------------------------------------------------------------------

/// 第 `attempt` 次重连该等多久:5 / 10 / 20 / 40 / 80 / 160 秒,300 秒封顶,
/// 再上下抖 20%。
///
/// `jitter` 由调用方给(0.0..=1.0),这个函数本身不摇骰子 —— 纯函数才测得准。
/// 抖动是为了多条搜索同时断线时不要在同一秒一起扑上去。
#[must_use]
pub fn reconnect_delay(attempt: u32, jitter: f64) -> Duration {
    const BASE_SECS: u64 = 5;
    const CAP_SECS: u64 = 300;

    // `saturating_pow` 防止 attempt 很大时溢出;超过 6 次以后反正都是封顶值。
    let doubled = BASE_SECS.saturating_mul(2u64.saturating_pow(attempt.min(16)));
    let base = doubled.min(CAP_SECS);

    // 0.0 → 0.8 倍,1.0 → 1.2 倍,0.5 正好是原值。
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let factor = 0.8 + 0.4 * jitter;
    Duration::from_millis((base as f64 * factor * 1000.0).round() as u64)
}

// ---------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------

/// 一条开着的 live 连接。
///
/// 拿到手就已经握完手了。之后只有三个动作:[`LiveSession::next`] 读一条、
/// [`LiveSession::close`] 好好道别、或者直接 drop(粗暴但也没坏处)。
pub struct LiveSession {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    url: String,
}

impl fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveSession")
            .field("url", &self.url)
            .finish()
    }
}

impl LiveSession {
    /// 握手并接管连接。
    ///
    /// 成功之后立刻给底层 TcpStream 设读超时 —— 见模块头第 2 点。设不上
    /// (理论上只有非 TCP 的流才会)不算失败,只是取消会迟钝一点。
    pub fn connect(config: &LiveConfig) -> Result<LiveSession, LiveError> {
        let url = live_ws_url(&config.league, &config.search_id);
        let request = handshake_request(config, &url)?;
        let (socket, _response) = tungstenite::connect(request).map_err(classify)?;
        set_read_timeout(socket.get_ref(), config.read_timeout);
        Ok(LiveSession { socket, url })
    }

    /// 这条连接连的是哪个地址(给日志用)。
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 读下一条消息,最多阻塞一个 `read_timeout`。
    ///
    /// Ping 在这里就地回 Pong 然后继续读:那是保活,不该让调用方看见。
    /// (tungstenite 收到 Ping 会自动把 Pong 排进发送队列,但要有人调
    /// `flush()` 才真的出去,所以这里补一刀。)
    ///
    /// clippy 会提醒这个名字容易和 `Iterator::next` 搞混。这里就是要它读起来
    /// 像"取下一条",但**不能**做成迭代器:迭代器的 `None` 意思是"没了",
    /// 而这条连接读不到东西只是 [`LiveMessage::Idle`](还活着),真正的结束
    /// 是一个带原因的 `Err`。压掉这条提醒,不是把语义削成迭代器。
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<LiveMessage, LiveError> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => return Ok(parse_live_message(text.as_str())?),
                // 接口只发文本;真收到二进制就按文本试一把,好过直接丢掉。
                Ok(Message::Binary(bytes)) => {
                    return Ok(parse_live_message(&String::from_utf8_lossy(&bytes))?);
                }
                Ok(Message::Ping(_)) => {
                    self.socket.flush().map_err(classify)?;
                }
                Ok(Message::Pong(_) | Message::Frame(_)) => {}
                Ok(Message::Close(frame)) => {
                    // tungstenite 已经把回礼的 Close 排进队列了,冲一下再走,
                    // 让服务端知道我们是好好挂断的。
                    let _ = self.socket.flush();
                    return Err(LiveError::Closed {
                        code: frame.as_ref().map(|f| u16::from(f.code)),
                        reason: frame
                            .map(|f| f.reason.as_str().to_string())
                            .unwrap_or_default(),
                    });
                }
                Err(tungstenite::Error::Io(error)) if is_read_timeout(&error) => {
                    return Ok(LiveMessage::Idle);
                }
                Err(error) => return Err(classify(error)),
            }
        }
    }

    /// 尽力发一个 Close 帧再散伙。发不出去也无所谓 —— 连接反正要没了,
    /// 为了一句道别报错没有意义。
    pub fn close(mut self) {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

// ---------------------------------------------------------------------
// 内部:握手请求、错误分类、读超时
// ---------------------------------------------------------------------

/// 手搓握手请求。五个 WebSocket 必需头一个都不能少(见模块头第 1 点),
/// 另外四个头是"我是从网页那条 live 页面来的"这套身份。
///
/// 故意不公开:返回的 `http::Request` 的 `Debug` 会把 Cookie 原样印出来,
/// 让它只活在这个文件里,POESESSID 就没有漏出去的路径。
fn handshake_request(config: &LiveConfig, url: &str) -> Result<http::Request<()>, LiveError> {
    let referer = live_page_url(&config.search_ref());
    let mut builder = http::Request::builder()
        .uri(url)
        .header("Host", HOST)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .header("User-Agent", &config.user_agent)
        .header("Origin", ORIGIN)
        .header("Referer", referer);
    if config.has_session() {
        builder = builder.header("Cookie", format!("POESESSID={}", config.poesessid.trim()));
    }
    builder
        .body(())
        // 唯一会走到这里的情况是某个头的值里有换行之类的非法字符
        // (比如用户把整段 `Cookie: …` 粘进了 POESESSID 输入框)。
        // 报错而不是 panic:worker 是长命线程,不该被一次粘贴事故弄崩。
        .map_err(|error| LiveError::Protocol(format!("could not build the handshake: {error}")))
}

/// tungstenite 的错误 → 我们的分类。
///
/// 最要紧的是 `Http`:握手被拒时,状态码和 body 就是全部线索,
/// 而 tungstenite 把它们原样带在错误里(它自己 `Display` 只印状态码)。
fn classify(error: tungstenite::Error) -> LiveError {
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok());
            let content_type_is_html = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("text/html"));
            let body = response.body().as_deref().unwrap_or_default();
            let text = String::from_utf8_lossy(body);
            LiveError::Handshake {
                status,
                looks_like_html: content_type_is_html || looks_like_html(&text),
                retry_after,
                body_excerpt: excerpt(&text),
            }
        }
        tungstenite::Error::Tls(error) => LiveError::Tls(error.to_string()),
        tungstenite::Error::Io(error) => LiveError::Io(error.to_string()),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            LiveError::Closed {
                code: None,
                reason: "the socket was already closed".to_string(),
            }
        }
        other => LiveError::Protocol(other.to_string()),
    }
}

/// Cloudflare 的拦截页有时不带 `Content-Type: text/html`,再看一眼开头。
/// (`client.rs` 里对 HTTP body 有一份同样的判断;两处各自贴着自己的
/// 数据形状 —— 那边是字节,这边是握手响应里已经转成的文本。)
fn looks_like_html(text: &str) -> bool {
    let head: String = text.trim_start().chars().take(64).collect();
    let head = head.to_ascii_lowercase();
    head.starts_with("<!doctype") || head.starts_with("<html")
}

/// 一行、最多 [`BODY_EXCERPT_CHARS`] 个字符的摘要。换行压成空格,
/// 免得一张 HTML 页把日志撑开几十行。
fn excerpt(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let mut squeezed = String::with_capacity(flattened.len());
    let mut last_was_space = false;
    for c in flattened.trim().chars() {
        if c == ' ' && last_was_space {
            continue;
        }
        last_was_space = c == ' ';
        squeezed.push(c);
    }
    squeezed.chars().take(BODY_EXCERPT_CHARS).collect()
}

/// Windows 上 socket 读超时报的是 `TimedOut`,unix 上是 `WouldBlock`。
/// 两个都得认,不然超时会被当成断线。
fn is_read_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// 给底下那个 TcpStream 设读超时。`MaybeTlsStream` 是 `#[non_exhaustive]` 的,
/// 所以必须留一个兜底分支(我们只开了 rustls,不会走到)。
fn set_read_timeout(stream: &MaybeTlsStream<TcpStream>, timeout: Duration) {
    let tcp = match stream {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => tls.get_ref(),
        _ => return,
    };
    let _ = tcp.set_read_timeout(Some(timeout));
}

#[cfg(test)]
mod live_tests {
    use super::*;

    const SECRET: &str = "d34db33fd34db33fd34db33f";

    fn config() -> LiveConfig {
        LiveConfig {
            league: "Forbidden Rites".to_string(),
            search_id: "H4sIAAAA-_09".to_string(),
            poesessid: SECRET.to_string(),
            user_agent: "PoeNinjaData/0.1.0 (contact: someone@example.com)".to_string(),
            read_timeout: Duration::from_secs(30),
        }
    }

    // ---- URL ---------------------------------------------------------

    #[test]
    fn ws_url_encodes_the_league() {
        assert_eq!(
            live_ws_url("Forbidden Rites", "abcd1234"),
            "wss://www.pathofexile.com/api/trade2/live/poe2/Forbidden%20Rites/abcd1234"
        );
        assert_eq!(
            live_ws_url("Standard", "abcd1234"),
            "wss://www.pathofexile.com/api/trade2/live/poe2/Standard/abcd1234"
        );
    }

    // ---- 握手请求 ----------------------------------------------------

    #[test]
    fn handshake_carries_every_required_header() {
        let config = config();
        let url = live_ws_url(&config.league, &config.search_id);
        let request = handshake_request(&config, &url).unwrap();
        let headers = request.headers();

        // tungstenite 自己会检查的五个。少一个它就报 InvalidHeader。
        assert_eq!(headers["Host"], "www.pathofexile.com");
        assert_eq!(headers["Connection"], "Upgrade");
        assert_eq!(headers["Upgrade"], "websocket");
        assert_eq!(headers["Sec-WebSocket-Version"], "13");
        let key = headers["Sec-WebSocket-Key"].to_str().unwrap();
        assert_eq!(key.len(), 24, "16 字节 base64 = 24 个字符, got {key:?}");

        // 服务端认来源的四个。
        assert_eq!(headers["Cookie"], format!("POESESSID={SECRET}"));
        assert!(
            headers["User-Agent"]
                .to_str()
                .unwrap()
                .starts_with("PoeNinjaData/")
        );
        assert_eq!(headers["Origin"], "https://www.pathofexile.com");
        assert_eq!(
            headers["Referer"],
            "https://www.pathofexile.com/trade2/search/poe2/Forbidden%20Rites/H4sIAAAA-_09/live"
        );
        assert_eq!(request.uri().to_string(), url);
    }

    #[test]
    fn every_handshake_gets_a_fresh_key() {
        let config = config();
        let url = live_ws_url(&config.league, &config.search_id);
        let first = handshake_request(&config, &url).unwrap();
        let second = handshake_request(&config, &url).unwrap();
        assert_ne!(
            first.headers()["Sec-WebSocket-Key"],
            second.headers()["Sec-WebSocket-Key"]
        );
    }

    /// 没会话就不发 Cookie 头 —— 一个空的 `POESESSID=` 只会让服务端困惑。
    #[test]
    fn an_empty_session_sends_no_cookie() {
        let config = LiveConfig {
            poesessid: "   ".to_string(),
            ..config()
        };
        assert!(!config.has_session());
        let request = handshake_request(&config, "wss://example.com/x").unwrap();
        assert!(request.headers().get("Cookie").is_none());
    }

    /// 粘错东西(带换行的 cookie)只该报错,不该 panic。
    #[test]
    fn a_broken_session_value_is_an_error_not_a_panic() {
        let config = LiveConfig {
            poesessid: "abc\r\nX-Evil: 1".to_string(),
            ..config()
        };
        let error = handshake_request(&config, "wss://example.com/x").unwrap_err();
        assert!(matches!(error, LiveError::Protocol(_)));
        assert!(
            !error.to_string().contains("X-Evil"),
            "别把粘进来的内容回显出去"
        );
    }

    // ---- 会话不外泄 --------------------------------------------------

    #[test]
    fn the_session_never_shows_up_in_debug_output() {
        let config = config();
        let debug = format!("{config:?}");
        assert!(!debug.contains(SECRET), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("Forbidden Rites"), "别的字段照常可见");

        let anonymous = LiveConfig {
            poesessid: String::new(),
            ..config
        };
        assert!(format!("{anonymous:?}").contains("<none>"));
    }

    // ---- 消息解析 ----------------------------------------------------

    #[test]
    fn parses_a_new_listing_push() {
        assert_eq!(
            parse_live_message(r#"{"new":["aaa111","bbb222"]}"#).unwrap(),
            LiveMessage::New(vec!["aaa111".to_string(), "bbb222".to_string()])
        );
    }

    #[test]
    fn an_empty_new_list_is_still_a_push() {
        assert_eq!(
            parse_live_message(r#"{"new":[]}"#).unwrap(),
            LiveMessage::New(Vec::new())
        );
    }

    /// 服务端一连上就发的 `{"result":"<JWT>"}`。认出来是订阅回执,
    /// 而不是掉进 `Other` —— 掉进去就意味着整串 token 被抄进日志。
    #[test]
    fn the_connect_receipt_is_recognised_as_a_subscription() {
        let token = format!("eyJhbGciOiJIUzI1NiJ9.{}.sig", "A".repeat(240));
        let message = parse_live_message(&format!(r#"{{"result":"{token}"}}"#)).unwrap();
        assert_eq!(
            message,
            LiveMessage::Subscribed {
                token: token.clone()
            }
        );
        // 连 Debug 都不许把它印出来。
        let debug = format!("{message:?}");
        assert!(!debug.contains(&token), "{debug}");
        assert!(debug.contains("chars"), "{debug}");

        // `result` 不是字符串就不是回执,老实走"不认识"那条路。
        assert_eq!(
            parse_live_message(r#"{"result":{"a":1}}"#).unwrap(),
            LiveMessage::Other(r#"keys=["result"] result_len=1"#.to_string())
        );
    }

    #[test]
    fn an_unknown_shape_is_kept_as_other() {
        assert_eq!(
            parse_live_message(r#"{"auth":true}"#).unwrap(),
            LiveMessage::Other(r#"keys=["auth"] auth=true"#.to_string())
        );
        // 数组、字符串这些也是合法 JSON,同样不该炸。
        assert_eq!(
            parse_live_message("[1,2,3]").unwrap(),
            LiveMessage::Other("array len=3".to_string())
        );
    }

    #[test]
    fn a_non_json_message_is_an_error() {
        let error = parse_live_message("<!DOCTYPE html>").unwrap_err();
        assert!(error.to_string().contains("not JSON"), "{error}");
        assert!(matches!(LiveError::from(error), LiveError::Protocol(_)));
    }

    /// 描述只说形状:键名照写,长值只报长度。这是那条"token 别进日志"
    /// 纪律的落点。
    #[test]
    fn a_description_reports_keys_and_lengths_but_never_a_long_value() {
        assert_eq!(
            describe_live_message(r#"{"result":"eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9"}"#),
            r#"keys=["result"] result_len=36"#
        );
        // 短标量一眼能懂,原样留着。
        assert_eq!(
            describe_live_message(r#"{"ok":true,"n":12,"tag":"hi"}"#),
            r#"keys=["n", "ok", "tag"] n=12 ok=true tag="hi""#
        );
        // 数组和对象报的是元素个数 / 字段个数。
        assert_eq!(
            describe_live_message(r#"{"new":["a","b"],"meta":{"x":1}}"#),
            r#"keys=["meta", "new"] meta_len=1 new_len=2"#
        );
        assert_eq!(
            describe_live_message("not json at all"),
            "not JSON (15 chars)"
        );
    }

    /// 任何一个 20 字符以上的值都不许出现在描述里 —— 逐字验一遍。
    #[test]
    fn no_long_value_ever_reaches_the_description() {
        let secret = "x".repeat(SAFE_VALUE_CHARS);
        for shape in [
            format!(r#"{{"result":"{secret}"}}"#),
            format!(r#"{{"a":"{secret}","b":1}}"#),
            format!(r#"{{"nested":{{"deep":"{secret}"}}}}"#),
            format!(r#"["{secret}"]"#),
            format!(r#""{secret}""#),
        ] {
            let described = describe_live_message(&shape);
            assert!(
                !described.contains(&secret),
                "{shape} 的描述漏了值:{described}"
            );
        }
        // 刚好差一个字符的短值仍然可见 —— 边界在 20 上,不是"什么都不说"。
        let short = "x".repeat(SAFE_VALUE_CHARS - 1);
        assert!(describe_live_message(&format!(r#"{{"a":"{short}"}}"#)).contains(&short));
    }

    /// 一条巨长的怪消息不该把整份日志撑爆。
    #[test]
    fn other_messages_stay_short() {
        let long = format!(r#"{{"noise":"{}"}}"#, "x".repeat(5000));
        let LiveMessage::Other(text) = parse_live_message(&long).unwrap() else {
            panic!("expected Other");
        };
        assert_eq!(text, r#"keys=["noise"] noise_len=5000"#);
        assert!(text.chars().count() < BODY_EXCERPT_CHARS);
    }

    // ---- 错误分类 ----------------------------------------------------

    fn handshake_error(status: u16, html: bool, retry_after: Option<u64>) -> LiveError {
        LiveError::Handshake {
            status,
            looks_like_html: html,
            retry_after,
            body_excerpt: "…".to_string(),
        }
    }

    #[test]
    fn session_problems_and_cloudflare_are_told_apart() {
        // 会话过期:JSON 的 401/403。
        assert!(handshake_error(401, false, None).is_session_problem());
        assert!(handshake_error(403, false, None).is_session_problem());
        assert!(!handshake_error(401, false, None).is_cloudflare());

        // Cloudflare:HTML 的 403/503。
        assert!(handshake_error(403, true, None).is_cloudflare());
        assert!(handshake_error(503, true, None).is_cloudflare());
        assert!(
            !handshake_error(403, true, None).is_session_problem(),
            "HTML 的 403 是拦截页,不是会话问题"
        );

        // 其它都不是这两类,该走普通退避。
        assert!(!handshake_error(500, false, None).is_session_problem());
        assert!(!handshake_error(503, false, None).is_cloudflare());
        assert!(!handshake_error(429, false, Some(60)).is_session_problem());
        assert_eq!(
            handshake_error(429, false, Some(60)).retry_after(),
            Some(60)
        );
        assert_eq!(
            LiveError::Io("nope".to_string()).retry_after(),
            None,
            "只有握手响应才带 Retry-After"
        );
    }

    #[test]
    fn html_bodies_are_recognised() {
        assert!(looks_like_html("<!DOCTYPE html><html>"));
        assert!(looks_like_html("\n  <html lang=\"en\">"));
        assert!(!looks_like_html(r#"{"error":{"code":1}}"#));
        assert!(!looks_like_html(""));
    }

    #[test]
    fn close_errors_read_clearly() {
        let closed = LiveError::Closed {
            code: Some(1008),
            reason: "too many connections".to_string(),
        };
        let text = closed.to_string();
        assert!(text.contains("1008"), "{text}");
        assert!(text.contains("too many connections"), "{text}");
    }

    // ---- 退避 --------------------------------------------------------

    #[test]
    fn the_first_reconnect_waits_about_five_seconds() {
        assert_eq!(reconnect_delay(0, 0.0), Duration::from_secs(4));
        assert_eq!(reconnect_delay(0, 0.5), Duration::from_secs(5));
        assert_eq!(reconnect_delay(0, 1.0), Duration::from_secs(6));
    }

    #[test]
    fn the_delay_is_capped_at_five_minutes() {
        // 第 10 次早就撞到 300 秒的顶了,抖动之后是 240–360 秒。
        assert_eq!(reconnect_delay(10, 0.0), Duration::from_secs(240));
        assert_eq!(reconnect_delay(10, 0.5), Duration::from_secs(300));
        assert_eq!(reconnect_delay(10, 1.0), Duration::from_secs(360));
        // 荒唐的 attempt 也不能溢出。
        assert_eq!(reconnect_delay(u32::MAX, 0.5), Duration::from_secs(300));
    }

    #[test]
    fn without_jitter_the_delay_only_grows() {
        let plan: Vec<u64> = (0..8)
            .map(|attempt| reconnect_delay(attempt, 0.5).as_secs())
            .collect();
        assert_eq!(plan, vec![5, 10, 20, 40, 80, 160, 300, 300]);
        assert!(plan.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    /// 抖动越界(界面上手改设置、或者随机数给了 NaN)不该让等待变成 0 或者负数。
    #[test]
    fn jitter_outside_the_range_is_clamped() {
        assert_eq!(reconnect_delay(0, -5.0), reconnect_delay(0, 0.0));
        assert_eq!(reconnect_delay(0, 99.0), reconnect_delay(0, 1.0));
        assert_eq!(reconnect_delay(0, f64::NAN), reconnect_delay(0, 0.5));
    }

    #[test]
    fn the_server_connection_limit_is_recorded() {
        assert_eq!(MAX_LIVE_CONNECTIONS_PER_ACCOUNT, 20);
    }
}
