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

use pnd_domain::encode_league_path;
use serde_json::{Value, json};
use thiserror::Error;

use crate::ParseError;
use crate::rate_limit::{RateHeaders, parse_rate_headers};

/// 交易站 API 的根路径。poe2 的接口在 `trade2` 下,和 poe1 的 `trade` 是两套。
const API_BASE: &str = "https://www.pathofexile.com/api/trade2";

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
pub fn search_url(league: &str) -> String {
    format!("{API_BASE}/search/poe2/{}", encode_league_path(league))
}

/// fetch 接口地址:id 逗号分隔,搜索 id 走 `?query=`。
///
/// id 和搜索 id 都是 base64url 字符,不需要百分号编码 —— 逗号在路径段里也是合法字符。
#[must_use]
pub fn fetch_url(ids: &[String], search_id: &str) -> String {
    format!("{API_BASE}/fetch/{}?query={search_id}", ids.join(","))
}

/// 私聊/去藏身处接口地址。
#[must_use]
pub fn whisper_url() -> String {
    format!("{API_BASE}/whisper")
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
        league: &str,
        request_body_json: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError> {
        let mut request = self
            .agent
            .post(search_url(league))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent);
        if let Some(session) = session {
            request = request.header("Cookie", cookie(session));
        }
        collect(request.send(request_body_json))
    }

    /// GET 一批挂单详情。带上 POESESSID 时,响应里会多出
    /// `whisper_token` / `hideout_token`(短命 JWT,第二版的"去藏身处"要用)。
    pub fn fetch(
        &self,
        ids: &[String],
        search_id: &str,
        session: Option<&str>,
    ) -> Result<TradeResponse, TransportError> {
        if ids.len() > MAX_FETCH_IDS {
            return Err(TransportError::TooManyIds(ids.len()));
        }
        let mut request = self
            .agent
            .get(fetch_url(ids, search_id))
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
    /// # 这几个头是猜的吗
    ///
    /// 不是猜的,但也**没有和官网逐字比对过**。计划里核实过的只有地址和
    /// 请求体(`{"token":…,"continue":true}`)以及"要带 Cookie";
    /// `Content-Type`/`Accept`/`User-Agent`/`Referer` 是照着 search 和 fetch
    /// 那两个已经跑通的接口来的,同一套服务端、同一套 Cloudflare 规则。
    ///
    /// 2026-09-07 试着匿名去拿官网那份 JS 来对一遍,拿不到:
    /// `https://www.pathofexile.com/trade2/search/poe2/Standard` 回 403
    /// (Cloudflare),而 CDN 上的 bundle 名字要先读到那张 HTML 才知道。
    /// 所以**没有**照猜测加过任何头(比如 `X-Requested-With`);哪天真要动
    /// 这里的头,先想办法拿到官网那份 JS,别照感觉加。
    pub fn whisper(
        &self,
        token: &str,
        session: &str,
        referer: &str,
    ) -> Result<TradeResponse, TransportError> {
        let body = json!({ "token": token, "continue": true }).to_string();
        let request = self
            .agent
            .post(whisper_url())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent)
            .header("Referer", referer)
            .header("Cookie", cookie(session));
        collect(request.send(body.as_str()))
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
            search_url("Forbidden Rites"),
            "https://www.pathofexile.com/api/trade2/search/poe2/Forbidden%20Rites"
        );
        assert_eq!(
            search_url("Standard"),
            "https://www.pathofexile.com/api/trade2/search/poe2/Standard"
        );
    }

    #[test]
    fn fetch_url_joins_ids_with_commas() {
        let ids = vec!["aaa111".to_string(), "bbb222".to_string()];
        assert_eq!(
            fetch_url(&ids, "H4sIAAAA"),
            "https://www.pathofexile.com/api/trade2/fetch/aaa111,bbb222?query=H4sIAAAA"
        );
        assert_eq!(
            fetch_url(&["only".to_string()], "q"),
            "https://www.pathofexile.com/api/trade2/fetch/only?query=q"
        );
    }

    #[test]
    fn fetch_refuses_more_than_ten_ids() {
        let client = TradeClient::new("test".to_string());
        let ids: Vec<String> = (0..11).map(|i| format!("id{i}")).collect();
        let error = client.fetch(&ids, "q", None).unwrap_err();
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

    #[test]
    fn html_bodies_are_recognised() {
        assert!(body_starts_like_html(b"<!DOCTYPE html><html>"));
        assert!(body_starts_like_html(b"\n  <html lang=\"en\">"));
        assert!(!body_starts_like_html(br#"{"id":"x"}"#));
        assert!(!body_starts_like_html(b""));
    }
}
