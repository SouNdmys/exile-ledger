//! 交易站 HTTP 客户端:只负责"把请求发出去、把响应原样带回来"。
//!
//! 这一层刻意什么都不做:**不睡眠、不重试、不循环**。节奏归网关
//! (`pnd-runtime/src/gateway.rs`)管,它拿着限速器,知道全局什么时候该放行;
//! 客户端要是自己偷偷重试一次,网关的账就对不上了。
//!
//! 最要紧的一处设计:agent 关掉了 `http_status_as_error`。ureq 3 默认把
//! 4xx/5xx 变成 `Error::StatusCode`,而错误里**没有响应头** —— 可 429 响应上
//! 的那几个 `X-Rate-Limit-*` 头恰恰是我们最想学到的东西(服务端在告诉我们
//! 还要冷却多久)。403 + HTML body 也一样,得看见 body 才能认出 Cloudflare。
//! 所以这里让 4xx/5xx 走正常返回路径,状态码交给调用方判断。

use std::time::Duration;

use pnd_domain::{Game, encode_league_path};
use serde_json::{Value, json};
use thiserror::Error;

use crate::ParseError;
use crate::rate_limit::{RateHeaders, parse_rate_headers};

/// 交易站 API 的根路径。两代是两套:poe2 在 `trade2` 下,poe1 在 `trade` 下。
const API_BASE_POE2: &str = "https://www.pathofexile.com/api/trade2";
const API_BASE_POE1: &str = "https://www.pathofexile.com/api/trade";

/// 这一代的接口根路径。
#[must_use]
fn api_base(game: Game) -> &'static str {
    match game {
        Game::Poe1 => API_BASE_POE1,
        Game::Poe2 => API_BASE_POE2,
    }
}

/// 搜索路径里"联赛之前"的那一段。
///
/// poe2 在联赛前面还夹了一层 `poe2/`(`/search/poe2/Standard`),poe1 没有
/// (`/search/Standard`)—— 就这一处不对称,别的形状两代一模一样。
#[must_use]
fn search_path(game: Game) -> &'static str {
    match game {
        Game::Poe1 => "/search/",
        Game::Poe2 => "/search/poe2/",
    }
}

/// 浏览器在同源请求上会带的 `Origin`。只有 whisper 用得着(见 [`TradeClient::whisper`])。
const ORIGIN: &str = "https://www.pathofexile.com";

/// 一次请求的总时限(连接 + 传输)。挂在一个假死的连接上没有意义,
/// 而且网关是单线程串行的,一条卡住就是整条队列卡住。
const TIMEOUT: Duration = Duration::from_secs(20);

/// 响应体上限。search 回的是一串 id(几十 KB),fetch 一次最多 10 件物品
/// (百来 KB),8MB 是留了两个数量级的余量;超了说明接口行为变了,该报错。
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// fetch 接口一次最多 10 个挂单 id,这是服务端定的。
pub const MAX_FETCH_IDS: usize = 10;

/// 一次失败响应的 body 最多留这么长给人看。
///
/// 够看清 GGG 的一句 `{"error":{"code":6,"message":"…"}}`,也够认出
/// Cloudflare 的拦截页开头,又不至于把一整张 HTML 灌进卡片脚注和日志。
pub const BODY_EXCERPT_CHARS: usize = 160;

/// 网络层能出的岔子。注意 **HTTP 错误码不在这里**:429/403/503 都是正常返回的
/// `TradeResponse`,状态码由调用方处理,因为那些响应上的头和 body 都有用。
#[derive(Debug, Error)]
pub enum TransportError {
    /// 根本没连上:断网、DNS、TLS、超时全归这类,对调用方来说都是"稍后再试"。
    #[error("could not reach the trade site: {0}")]
    Unreachable(String),
    #[error("response exceeded the {MAX_BODY_BYTES} byte limit")]
    TooLarge,
    /// 调用方自己传错了参数,还没发出去就拦下来。返回错误而不是 panic:
    /// 网关是一条长命线程,不该被一次拼错的批次弄崩。
    #[error("fetch takes at most {MAX_FETCH_IDS} listing ids, got {0}")]
    TooManyIds(usize),
    /// 这一代根本没有这个接口。
    ///
    /// 现在只有一处会用到:**"去藏身处"是 PoE2 的即刻购买才有的东西** ——
    /// PoE1 的挂单没有 `hideout_token`,那一代的成交方式就是自己私聊卖家。
    /// 单独一类而不是硬发一次请求让服务端回 404:那样白花一次额度,
    /// 报出来的还是一句看不懂的状态码。
    #[error("the trade site has no {0} for {1}")]
    Unsupported(&'static str, Game),
}

/// 一次交易站响应的全部有用信息。
///
/// `rate` 是解析好的限速头(没有 `X-Rate-Limit-Policy` 就是 `None`);
/// `looks_like_html` 是给网关认 Cloudflare 拦截页用的 —— JSON 接口回 HTML,
/// 说明请求根本没到 GGG 的应用层。
#[derive(Debug, Clone)]
pub struct TradeResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub rate: Option<RateHeaders>,
    pub looks_like_html: bool,
}

impl TradeResponse {
    /// 2xx 才算成功。429/403/503 都会走到这里,由调用方分别处理。
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// body 当文本看,给探针和日志打印用(接口是 UTF-8 JSON)。
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// 一行、最多 [`BODY_EXCERPT_CHARS`] 个字符的 body 摘要,给错误消息用。
    ///
    /// 非 2xx 的时候,状态码只说"被拒了",body 才说"为什么被拒" —— 早先
    /// 这一段被整个丢掉了,于是卡片上只剩一个数字。换行压成空格是为了让它
    /// 能塞进一行日志。
    ///
    /// 这里出去的**只有服务端说的话**:我们发出去的 token 和 cookie 都在
    /// 请求头/请求体里,不在响应 body 里。
    #[must_use]
    pub fn body_excerpt(&self) -> String {
        excerpt(&String::from_utf8_lossy(&self.body))
    }
}

/// 交易站的报错正文 → 一句人话:`GGG error 6: Forbidden`。
///
/// GGG 的接口错误统一长这样:`{"error":{"code":N,"message":"..."}}`。原样
/// 摆出来的话,卡片脚注上那一行就成了一串大括号和引号,而真正有信息量的
/// 只有里面那半句 —— 尤其是它还会被裁短。
///
/// **`code` 和 `message` 两样齐了才认。** 只有 code 的时候(服务端确实这么
/// 回过一次)翻译成 "GGG error 8" 反而更糟:那个数字我们自己也不认识,
/// 不如把 body 原样摆出来,至少能拿去搜。
#[must_use]
pub fn ggg_error(body: &str) -> Option<String> {
    let root: Value = serde_json::from_str(body).ok()?;
    let error = root.get("error")?;
    let code = error.get("code")?.as_i64()?;
    let message = error.get("message")?.as_str()?.trim();
    (!message.is_empty()).then(|| format!("GGG error {code}: {message}"))
}

/// 压成一行、掐到 [`BODY_EXCERPT_CHARS`] 个字符。
fn excerpt(text: &str) -> String {
    let mut squeezed = String::new();
    let mut last_was_space = false;
    for ch in text.trim().chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' && last_was_space {
            continue;
        }
        last_was_space = ch == ' ';
        squeezed.push(ch);
        if squeezed.chars().count() >= BODY_EXCERPT_CHARS {
            break;
        }
    }
    squeezed
}

/// search 接口的响应。
///
/// `id` 就是网页地址栏里那串搜索 id;`result` 是按价升序的挂单 id,
/// 一次最多 100 个,我们只取前 10 个去 fetch。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResponse {
    pub id: String,
    pub total: u64,
    pub result: Vec<String>,
    pub complexity: Option<u32>,
}

/// search 接口地址。联赛名要 URL 编码(`Forbidden Rites` → `Forbidden%20Rites`)。
#[must_use]
pub fn search_url(game: Game, league: &str) -> String {
    format!(
        "{}{}{}",
        api_base(game),
        search_path(game),
        encode_league_path(league)
    )
}

/// "已保存的查询"接口地址:`GET .../search/<联赛>/<搜索id>`。
///
/// **只有 PoE1 有这条路,而且只对老式的短把手 id 有用。** 2026-09-09 实测:
/// PoE1 的接口自己发回来的那种自描述长 id(`H4sIAAAA…`)拿到这里也是 404,
/// 它本来就该在本地解开(见 [`pnd_domain::decode_search_id`]);trade2 则整条
/// 路都没有。所以调用它之前先本地解一次,解不开才问。
#[must_use]
pub fn saved_search_url(game: Game, league: &str, search_id: &str) -> String {
    format!("{}/{search_id}", search_url(game, league))
}

/// fetch 接口地址:id 逗号分隔,搜索 id 走 `?query=`。
///
/// id 和搜索 id 都是 base64url 字符,不需要百分号编码 —— 逗号在路径段里也是合法字符。
#[must_use]
pub fn fetch_url(game: Game, ids: &[String], search_id: &str) -> String {
    format!(
        "{}/fetch/{}?query={search_id}",
        api_base(game),
        ids.join(",")
    )
}

/// 私聊/去藏身处接口地址。**只有 PoE2 有**(见 [`TransportError::Unsupported`])。
#[must_use]
pub fn whisper_url() -> String {
    format!("{API_BASE_POE2}/whisper")
}

/// "已保存的查询"响应 → 查询 JSON 原文。
///
/// PoE1 的这封回信长这样:`{"id":…,"complexity":…,"league":…,"query":{…},
/// "sort":{…}}`。我们只要 `query` 那一半:`sort` 由我们自己按用途换
/// (蹲价按价升序、观察按上架时间倒序),原样带着只会被覆盖掉。
pub fn parse_saved_search_query(body: &[u8]) -> Result<String, ParseError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| ParseError::NotJson(e.to_string()))?;
    let query = root
        .get("query")
        .filter(|query| query.is_object())
        .ok_or(ParseError::Missing("query"))?;
    Ok(query.to_string())
}

/// whisper 的请求体:**只有 token,没有第二个字段**。
///
/// 2026-09-07 主人在 Chrome DevTools 里抄下官网点"Travel to Hideout"发出的
/// 那一封:`Content-Length: 636`,而里面那张 token 是 624 个字符。
/// `{"token":"…"}` 的包装正好 12 个字节,624 + 12 = 636 —— 一个字段也塞不下,
/// 这就把"还有没有别的字段"这个问题彻底关掉了。以前那个 `"continue":true`
/// 是从早期笔记里抄来的,官网不发它。
#[must_use]
pub fn whisper_body(token: &str) -> String {
    json!({ "token": token }).to_string()
}

/// 持有连接池的交易站客户端。
///
/// User-Agent 由调用方传进来(设置页可以在"带联系方式"和"浏览器样式"之间切):
/// 今天实测自定义 UA 能过 Cloudflare,哪天不行了改设置就好,不用改代码。
pub struct TradeClient {
    agent: ureq::Agent,
    user_agent: String,
}

impl TradeClient {
    #[must_use]
    pub fn new(user_agent: String) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            // 见模块头注释:429 的价值全在它的响应头上,不能让 ureq 把它变成错误。
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
            user_agent,
        }
    }

    /// POST 一次搜索。`request_body_json` 用 [`pnd_domain::search_request_body`]
    /// 拼好再传进来,这里不碰查询内容。
    pub fn search(
        &self,
        game: Game,
        league: &str,
        request_body_json: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError> {
        let mut request = self
            .agent
            .post(search_url(game, league))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent);
        if let Some(session) = session {
            request = request.header("Cookie", cookie(session));
        }
        collect(request.send(request_body_json))
    }

    /// GET 一条已经存在服务端的查询(PoE1 那条路,见 [`saved_search_url`])。
    ///
    /// 花的是 search 的限速额度 —— 地址就在 `/search/` 下面,服务端按同一条
    /// 策略记账。一条搜索一辈子只问一次:回来的查询存进 `saved_queries`。
    pub fn load_saved_query(
        &self,
        game: Game,
        league: &str,
        search_id: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError> {
        let mut request = self
            .agent
            .get(saved_search_url(game, league, search_id))
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent);
        if let Some(session) = session {
            request = request.header("Cookie", cookie(session));
        }
        collect(request.call())
    }

    /// GET 一批挂单详情。带上 POESESSID 时,响应里会多出
    /// `whisper_token` / `hideout_token`(短命 JWT,第二版的"去藏身处"要用)。
    pub fn fetch(
        &self,
        game: Game,
        ids: &[String],
        search_id: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError> {
        if ids.len() > MAX_FETCH_IDS {
            return Err(TransportError::TooManyIds(ids.len()));
        }
        let mut request = self
            .agent
            .get(fetch_url(game, ids, search_id))
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent);
        if let Some(session) = session {
            request = request.header("Cookie", cookie(session));
        }
        collect(request.call())
    }

    /// 发一次私聊(`whisper_token`)或去藏身处(`hideout_token`)。
    ///
    /// 必须带会话,而且 `Referer` 要是这条挂单所在的搜索页 —— 服务端拿它当
    /// 来源校验。
    ///
    /// # token 过期长什么样
    ///
    /// **401 / 403 / 503 都可能只是"这张 token 不好使了"**,处置一样:重新
    /// fetch 一次那条挂单拿新 token,再发一次;换过还是同一个码,才轮到
    /// 怀疑会话(401/403)或者你自己的游戏客户端不在城里(503)。
    /// 2026-09-07 一次真跑里,一条挂了 37 分钟的挂单点下去回的就是个
    /// **非 HTML** 的 403 —— 那不是 Cloudflare(那种带 HTML,调用方另有
    /// 一条路),是交易站自己在拒绝这张票。
    ///
    /// 别只靠"拿到多久了"去判断过不过期:token 是 JWT,自己带 `exp`
    /// (见 [`crate::jwt::jwt_expiry`]),听它的比从外面猜准。
    ///
    /// 这个方法是给第二版的卡片按钮用的:**永远由用户点一次才调用**,
    /// 程序自己不会调。
    ///
    /// # 这几个头不再是猜的了
    ///
    /// 2026-09-07 主人在 Chrome DevTools 里抄下了官网真实的那一封:已登录、
    /// 在一条即刻购买的挂单上点 "Travel to Hideout"。**这一段以下都是抄件里
    /// 的事实,不是推测。**
    ///
    /// - 地址:`POST https://www.pathofexile.com/api/trade2/whisper`
    /// - 请求体:逐字就是 `{"token":"<hideout_token>"}`(见 [`whisper_body`])
    /// - 头:`Content-Type: application/json`、`Accept: */*`、
    ///   `Origin: https://www.pathofexile.com`、`X-Requested-With: XMLHttpRequest`、
    ///   `Referer` = 这条挂单所在的那张搜索页、浏览器的 `User-Agent`、
    ///   以及带 cookie 的 `Cookie`
    /// - `hideout_token` 的载荷:`{"jti":…,"iss":"<搜索id>","aud":"<uuid>",
    ///   "tok":"hideout","sub":"<挂单id>","dat":…,"iat":…,"exp":iat+300}`
    ///   —— **有效期 300 秒**
    ///
    /// `Origin` 和 `X-Requested-With` 这两个原来是猜的(2026-09-07 那次 403 的
    /// 当时猜想),抄件把它们坐实了:官网确实发这两个。`Accept` 原来照着
    /// search/fetch 写成 `application/json`,抄件里是 `*/*`,现在照抄件改了。
    ///
    /// # 抄件里有、我们发不出去的东西
    ///
    /// 浏览器还发了 `Accept-Language`、`Sec-Fetch-Site/Mode/Dest`
    /// (`same-origin`/`cors`/`empty`),以及 cookie 里的 `cf_clearance`、
    /// `cf_chl_rc_ni` 和一张 **`POETOKEN` JWT**(载荷开头是
    /// `{"aud":"oauth/internal",…}`)。前两类是浏览器自己加的,补上也没意义;
    /// `cf_clearance` 那两个是 Cloudflare 的,我们的 cookie 罐子里根本没有。
    ///
    /// **`POETOKEN` 是唯一还没弄清的那一格。** 它是网站登录时另发的一张票,
    /// 我们的 WebView2 登录只抄了 `POESESSID`。要是改成官网这封请求之后还是
    /// 403,下一个该查的就是它 —— 不要再往这里加别的头了。
    /// # PoE1 没有这个接口
    ///
    /// 藏身处传送是 trade2 那套"即刻购买"的一部分:挂单上带一张
    /// `hideout_token`,POST 回去服务端就替你发传送邀请。PoE1 的挂单里根本
    /// 没有那张票,那一代的成交方式是自己私聊卖家。所以这里在**发出去之前**
    /// 就拦下来,而不是拿一张不存在的 token 去换一个 404。
    pub fn whisper(
        &self,
        game: Game,
        token: &str,
        session: &str,
        referer: &str,
    ) -> Result<TradeResponse, TransportError> {
        if game != Game::Poe2 {
            return Err(TransportError::Unsupported("hideout travel", game));
        }
        let body = whisper_body(token);
        collect(self.whisper_request(session, referer).send(body.as_str()))
    }

    /// 把 whisper 的请求拼好但不发。
    ///
    /// 分出来只为一件事:让"到底带了哪些头"在测试里看得见。不然那几个头
    /// 只有真发一次请求才验证得了,而这个接口恰恰是全程序唯一一个我们不能
    /// 随便试的 —— 它会在游戏里给别人发传送邀请。
    ///
    /// 故意不公开:builder 的 `Debug` 会把 Cookie 原样印出来,让它只活在
    /// 这个文件里,POESESSID 就没有漏出去的路径。
    fn whisper_request(
        &self,
        session: &str,
        referer: &str,
    ) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        self.agent
            .post(whisper_url())
            .header("Content-Type", "application/json")
            // 抄件里就是 `*/*`,不是 search/fetch 那两个上的 `application/json`。
            .header("Accept", "*/*")
            .header("User-Agent", &self.user_agent)
            .header("Origin", ORIGIN)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Referer", referer)
            .header("Cookie", cookie(session))
    }
}

/// 解析 search 的响应体。
///
/// `total` 有时是浮点写法(服务端偶尔回 `1.0`),所以两种数字都认;
/// `result` 缺失当空表 —— 一个都没搜到时服务端就是这么回的。
pub fn parse_search_response(body: &[u8]) -> Result<SearchResponse, ParseError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| ParseError::NotJson(e.to_string()))?;
    let id = root
        .get("id")
        .and_then(Value::as_str)
        .ok_or(ParseError::Missing("id"))?
        .to_string();
    let total = root
        .get("total")
        .and_then(as_u64)
        .ok_or(ParseError::Missing("total"))?;
    let result = root
        .get("result")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let complexity = root
        .get("complexity")
        .and_then(as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Ok(SearchResponse {
        id,
        total,
        result,
        complexity,
    })
}

fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64))
}

fn cookie(session: &str) -> String {
    format!("POESESSID={session}")
}

/// 把 ureq 的响应榨干成 [`TradeResponse`]:先抄头(body 一读就把响应吃掉了),
/// 再限长读 body。
fn collect(
    response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<TradeResponse, TransportError> {
    let mut response = response.map_err(classify)?;
    let status = response.status().as_u16();

    // 先存成 String 再借出去:`parse_rate_headers` 要的是 `&str`,
    // 而 header 的值是字节,可能不是合法 UTF-8(那种就当没有)。
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    let content_type_is_html = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type") && value.contains("text/html")
    });
    let rate = parse_rate_headers(
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    );

    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_vec()
        .map_err(classify)?;

    Ok(TradeResponse {
        looks_like_html: content_type_is_html || body_starts_like_html(&body),
        status,
        body,
        rate,
    })
}

/// Cloudflare 的拦截页有时不带 `Content-Type: text/html`,再看一眼开头。
fn body_starts_like_html(body: &[u8]) -> bool {
    let head = &body[..body.len().min(64)];
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype") || head.starts_with("<html")
}

/// agent 关了 `http_status_as_error`,所以 `StatusCode` 理论上不会再出现;
/// 真出现了也只能当"连不上"处理(状态码丢在错误里,body 已经没了)。
fn classify(error: ureq::Error) -> TransportError {
    match error {
        ureq::Error::BodyExceedsLimit(_) => TransportError::TooLarge,
        other => TransportError::Unreachable(other.to_string()),
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;

    #[test]
    fn search_url_encodes_the_league() {
        assert_eq!(
            search_url(Game::Poe2, "Forbidden Rites"),
            "https://www.pathofexile.com/api/trade2/search/poe2/Forbidden%20Rites"
        );
        assert_eq!(
            search_url(Game::Poe2, "Standard"),
            "https://www.pathofexile.com/api/trade2/search/poe2/Standard"
        );
    }

    #[test]
    fn fetch_url_joins_ids_with_commas() {
        let ids = vec!["aaa111".to_string(), "bbb222".to_string()];
        assert_eq!(
            fetch_url(Game::Poe2, &ids, "H4sIAAAA"),
            "https://www.pathofexile.com/api/trade2/fetch/aaa111,bbb222?query=H4sIAAAA"
        );
        assert_eq!(
            fetch_url(Game::Poe2, &["only".to_string()], "q"),
            "https://www.pathofexile.com/api/trade2/fetch/only?query=q"
        );
    }

    /// PoE1 走的是另一套根路径,而且联赛前面**没有** `poe2/` 那一层。
    #[test]
    fn poe1_urls_drop_the_trade2_base_and_the_poe2_segment() {
        assert_eq!(
            search_url(Game::Poe1, "Standard"),
            "https://www.pathofexile.com/api/trade/search/Standard"
        );
        assert_eq!(
            search_url(Game::Poe1, "Hardcore Settlers"),
            "https://www.pathofexile.com/api/trade/search/Hardcore%20Settlers"
        );
        assert_eq!(
            fetch_url(
                Game::Poe1,
                &["aaa".to_string(), "bbb".to_string()],
                "Rj3mL5Sw"
            ),
            "https://www.pathofexile.com/api/trade/fetch/aaa,bbb?query=Rj3mL5Sw"
        );
        // 服务端存着的那条查询:GET 同一条 search 路径再跟一个 id。
        assert_eq!(
            saved_search_url(Game::Poe1, "Standard", "Rj3mL5Sw"),
            "https://www.pathofexile.com/api/trade/search/Standard/Rj3mL5Sw"
        );
    }

    /// 一封真实形状的"已保存查询"回信 → 查询那一半。`sort` 不要:
    /// 排序由调用方按用途换,原样带着只会被覆盖。
    #[test]
    fn a_saved_search_response_yields_just_the_query() {
        let body = br#"{"id":"Rj3mL5Sw","complexity":4,"league":"Standard",
            "query":{"status":{"option":"online"},"name":"Headhunter","type":"Leather Belt"},
            "sort":{"price":"asc"}}"#;
        let query = parse_saved_search_query(body).unwrap();
        let parsed: Value = serde_json::from_str(&query).unwrap();
        assert_eq!(parsed["name"], "Headhunter");
        assert_eq!(parsed["type"], "Leather Belt");
        assert_eq!(parsed["status"]["option"], "online");
        assert!(parsed.get("sort").is_none(), "{query}");
        // 拼成请求体之后就是我们平常发的那一封。
        let body: Value = serde_json::from_str(&pnd_domain::search_request_body(&query)).unwrap();
        assert_eq!(body["query"]["name"], "Headhunter");
        assert_eq!(body["sort"]["price"], "asc");
    }

    /// 形状不对的回信要说清缺了什么,别让上层拿一句"不是 JSON"去猜。
    #[test]
    fn a_saved_search_response_reports_what_is_missing() {
        assert_eq!(
            parse_saved_search_query(br#"{"id":"Rj3mL5Sw","league":"Standard"}"#),
            Err(ParseError::Missing("query"))
        );
        // 404 的 body 是 GGG 的错误 JSON:里面也没有 `query`。
        assert_eq!(
            parse_saved_search_query(br#"{"error":{"code":2,"message":"Invalid query"}}"#),
            Err(ParseError::Missing("query"))
        );
        assert!(matches!(
            parse_saved_search_query(b"<!DOCTYPE html>"),
            Err(ParseError::NotJson(_))
        ));
    }

    /// 藏身处传送是 PoE2 独有的。PoE1 点下去连一个字节都不该出门。
    #[test]
    fn a_poe1_hideout_travel_is_refused_before_any_request() {
        let client = TradeClient::new("test".to_string());
        let error = client
            .whisper(Game::Poe1, "tok", "session", "https://example.com")
            .unwrap_err();
        assert!(
            matches!(
                error,
                TransportError::Unsupported("hideout travel", Game::Poe1)
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "the trade site has no hideout travel for poe1"
        );
    }

    #[test]
    fn fetch_refuses_more_than_ten_ids() {
        let client = TradeClient::new("test".to_string());
        let ids: Vec<String> = (0..11).map(|i| format!("id{i}")).collect();
        let error = client.fetch(Game::Poe2, &ids, "q", None).unwrap_err();
        assert!(matches!(error, TransportError::TooManyIds(11)));
    }

    /// 今天实测的 search 响应形状(id/complexity/result/total)。
    #[test]
    fn parses_a_search_response() {
        let body = br#"{"id":"H4sIAAAAAAAAA","complexity":6,
            "result":["a1","b2","c3"],"total":137}"#;
        let parsed = parse_search_response(body).unwrap();
        assert_eq!(parsed.id, "H4sIAAAAAAAAA");
        assert_eq!(parsed.total, 137);
        assert_eq!(parsed.complexity, Some(6));
        assert_eq!(parsed.result, vec!["a1", "b2", "c3"]);
    }

    #[test]
    fn search_response_tolerates_missing_optional_fields() {
        let parsed = parse_search_response(br#"{"id":"x","total":0}"#).unwrap();
        assert!(parsed.result.is_empty());
        assert_eq!(parsed.complexity, None);
    }

    #[test]
    fn search_response_reports_what_is_missing() {
        assert_eq!(
            parse_search_response(br#"{"total":3}"#),
            Err(ParseError::Missing("id"))
        );
        assert_eq!(
            parse_search_response(br#"{"id":"x"}"#),
            Err(ParseError::Missing("total"))
        );
        assert!(matches!(
            parse_search_response(b"<!DOCTYPE html>"),
            Err(ParseError::NotJson(_))
        ));
    }

    /// 非 2xx 的时候,body 才是"为什么被拒"。摘要要压成一行、掐短,
    /// 而且不能因为服务端回了一堆空白就变成一片空格。
    #[test]
    fn body_excerpts_are_one_short_line() {
        let response = |body: &str| TradeResponse {
            status: 403,
            body: body.as_bytes().to_vec(),
            rate: None,
            looks_like_html: false,
        };

        assert_eq!(
            response("{\n  \"error\": {\n    \"code\": 6,\n    \"message\": \"Forbidden\"\n  }\n}")
                .body_excerpt(),
            r#"{ "error": { "code": 6, "message": "Forbidden" } }"#
        );
        assert_eq!(response("   ").body_excerpt(), "");
        assert_eq!(response("").body_excerpt(), "");

        let long = response(&"x".repeat(5_000)).body_excerpt();
        assert_eq!(long.chars().count(), BODY_EXCERPT_CHARS);
        // 多字节也按字符数掐,不能从半个汉字中间切开。
        let chinese = response(&"错".repeat(500)).body_excerpt();
        assert_eq!(chinese.chars().count(), BODY_EXCERPT_CHARS);
    }

    /// GGG 的报错要翻成"错误码 + 那句话",而且只在两样都齐的时候翻 ——
    /// 翻不出来的一律回 `None`,让调用方原样把 body 摆出来。
    #[test]
    fn a_ggg_error_body_reads_as_one_sentence() {
        assert_eq!(
            ggg_error(r#"{"error":{"code":6,"message":"Forbidden"}}"#).as_deref(),
            Some("GGG error 6: Forbidden")
        );
        // `body_excerpt` 会把换行压成空格,压过的那一版也得认得出来。
        assert_eq!(
            ggg_error(r#"{ "error": { "code": 2, "message": "Invalid query" } }"#).as_deref(),
            Some("GGG error 2: Invalid query")
        );

        // 只有 code 没有 message:翻译出来的 "GGG error 8" 谁也不认识,
        // 不如把原文留给调用方。
        assert_eq!(ggg_error(r#"{"error":{"code":8}}"#), None);
        assert_eq!(ggg_error(r#"{"error":{"message":"Forbidden"}}"#), None);
        assert_eq!(ggg_error(r#"{"error":{"code":6,"message":"  "}}"#), None);
        assert_eq!(ggg_error(r#"{"id":"x","total":3}"#), None, "根本不是报错");
        assert_eq!(ggg_error("<!DOCTYPE html><html>"), None);
        assert_eq!(ggg_error(""), None);
        // 裁短过的 JSON 解不开 —— 那也该回 None,而不是拼一句半截话。
        assert_eq!(ggg_error(r#"{"error":{"code":6,"messa"#), None);
    }

    /// 这一串就是 2026-09-07 那份 DevTools 抄件里我们发得出去的全部头
    /// (见 [`TradeClient::whisper`])。钉死它是为了下一次再吃 403 时能立刻
    /// 排除"是不是哪个头掉了" —— 那时该查的是 `POETOKEN`,不是这里。
    ///
    /// `Accept` 特意是 `*/*`:抄件里就是这个,和 search/fetch 上的
    /// `application/json` 不一样。
    #[test]
    fn a_whisper_request_carries_the_browser_headers() {
        let client = TradeClient::new("ExileLedger/0.1.0".to_string());
        let referer = "https://www.pathofexile.com/trade2/search/poe2/Standard/abcd1234";
        let request = client.whisper_request("not-a-real-session", referer);
        let headers = request.headers_ref().expect("builder has no error");

        assert_eq!(headers["Origin"], "https://www.pathofexile.com");
        assert_eq!(headers["X-Requested-With"], "XMLHttpRequest");
        assert_eq!(headers["Content-Type"], "application/json");
        assert_eq!(headers["Accept"], "*/*");
        assert_eq!(headers["User-Agent"], "ExileLedger/0.1.0");
        assert_eq!(headers["Referer"], referer);
        assert_eq!(headers["Cookie"], "POESESSID=not-a-real-session");
    }

    /// 请求体逐字就是抄件里那一行。
    ///
    /// 抄件里的 Content-Length 是 636,而 token 有 624 个字符 ——
    /// `{"token":"…"}` 正好是 12 个字节的包装,一个字段也塞不下了。
    /// 所以这里是"等于",不是"包含":多一个 `continue` 就是多 16 个字节,
    /// 那封请求就不是官网发的那一封了。
    #[test]
    fn a_whisper_body_is_only_the_token() {
        let body = whisper_body("eyJhbGciOiJIUzI1NiJ9.payload.sig");
        assert_eq!(body, r#"{"token":"eyJhbGciOiJIUzI1NiJ9.payload.sig"}"#);
        assert!(!body.contains("continue"), "{body}");
    }

    #[test]
    fn html_bodies_are_recognised() {
        assert!(body_starts_like_html(b"<!DOCTYPE html><html>"));
        assert!(body_starts_like_html(b"\n  <html lang=\"en\">"));
        assert!(!body_starts_like_html(br#"{"id":"x"}"#));
        assert!(!body_starts_like_html(b""));
    }
}
