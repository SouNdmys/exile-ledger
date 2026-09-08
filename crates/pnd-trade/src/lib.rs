//! 交易站客户端:纯限速器、ureq 版 search/fetch/whisper、限速头解析、
//! Live Search WebSocket 会话。

use thiserror::Error;

pub mod client;
pub mod jwt;
pub mod listing;
pub mod live;
pub mod rate_limit;

pub use client::{
    MAX_FETCH_IDS, SearchResponse, TradeClient, TradeResponse, TransportError, fetch_url,
    ggg_error, parse_search_response, search_url, whisper_body, whisper_url,
};
pub use jwt::{jwt_claims, jwt_expiry, jwt_header};
pub use listing::{parse_fetch_response, parse_fetch_response_by_id, parse_fetch_response_slots};
pub use live::{
    DEFAULT_READ_TIMEOUT, LiveConfig, LiveError, LiveMessage, LiveSession,
    MAX_LIVE_CONNECTIONS_PER_ACCOUNT, ProtocolError, live_ws_url, parse_live_message,
    reconnect_delay,
};
pub use rate_limit::{
    BucketUsage, Budget, FETCH_LONG_WINDOW_REQUESTS, FETCH_LONG_WINDOW_SECS, FETCH_POLICY,
    RateHeaders, RateLimiter, SEARCH_LONG_WINDOW_REQUESTS, SEARCH_LONG_WINDOW_SECS, SEARCH_POLICY,
    backoff_after_429, parse_rate_headers,
};

/// 响应体读不懂。两个解析器(search 和 fetch)共用一个错误类型:
/// 对调用方来说这两种失败的处置是一样的 —— 记一笔、这轮作废、下一轮再来。
///
/// 故意分成两种:`NotJson` 多半是被 Cloudflare 拦了(回的是 HTML),
/// `Missing` 才是接口真的改了形状,后者值得让人来看一眼。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("response is not valid JSON: {0}")]
    NotJson(String),
    #[error("response JSON has no `{0}` field")]
    Missing(&'static str),
}
