//! poe.ninja 的 HTTP 层。
//!
//! 纪律照搬兄弟项目的 `ptt-exchange-history/src/fetch.rs`:一个带连接池的
//! `ureq::Agent`、全局超时、显式 User-Agent、响应体长度上限、错误分成
//! "对面拒绝"和"根本没连上"两类。这里刻意**不重试、不睡眠**——
//! "1 秒一个请求""24 小时不重跑"属于采样管线的节奏,不是网络层的事。
//!
//! URL 拼装全是独立的纯函数,好让测试把字符串一个字一个字钉住:
//! 这些路径没有文档,拼错一个字符就是 404,而其它代码全对也救不了。

use std::fmt::Write as _;
use std::time::Duration;

use thiserror::Error;

use crate::USER_AGENT;
use crate::character::CharacterDetail;
use crate::economy::{ExchangeOverview, ItemOverview};
use crate::index_state::{BuildIndexState, IndexState};
use crate::ndic::parse_ndic;
use crate::search::{SearchResponse, decode};

/// 单请求限时。后台采样挂在一个假死的连接上没有任何意义。
const TIMEOUT: Duration = Duration::from_secs(20);

/// 实测最大的一份是 `allskills` 字典和整包搜索响应,都在 100KB 量级。
/// 32MB 留了三个数量级的余量:超过它说明端点行为变了,该报错让人来看。
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

const BUILDS_BASE: &str = "https://poe.ninja/poe2/api/builds";
const DATA_BASE: &str = "https://poe.ninja/poe2/api/data";
const ECONOMY_BASE: &str = "https://poe.ninja/poe2/api/economy";

#[derive(Debug, Error)]
pub enum NinjaError {
    #[error("poe.ninja rejected the request with status {0}")]
    Rejected(u16),
    #[error("response exceeded the {MAX_BODY_BYTES} byte limit")]
    TooLarge,
    /// 断网、DNS、TLS、代理——对调用方来说都是"稍后再试"。
    #[error("could not reach poe.ninja: {0}")]
    Unreachable(String),
    /// 连上了、也拿到字节了,但内容不是我们认识的东西。格式漂移就长这样。
    #[error("could not decode the response: {0}")]
    Decode(String),
}

#[must_use]
pub fn index_state_url() -> String {
    format!("{DATA_BASE}/index-state")
}

#[must_use]
pub fn build_index_state_url() -> String {
    format!("{DATA_BASE}/build-index-state")
}

/// `filters` 是 `class`/`skills`/`items`/`keypassives`/`spiritgems`/`allskills`/
/// `timeMachine`/`sort` 这些查询参数,多个条件是 AND。
#[must_use]
pub fn search_url(version: &str, snapshot_name: &str, filters: &[(&str, &str)]) -> String {
    let mut url = format!(
        "{BUILDS_BASE}/{}/search?overview={}",
        encode(version),
        encode(snapshot_name)
    );
    for (key, value) in filters {
        let _ = write!(url, "&{}={}", encode(key), encode(value));
    }
    url
}

#[must_use]
pub fn dictionary_url(sha1: &str) -> String {
    format!("{BUILDS_BASE}/dictionary/{}", encode(sha1))
}

/// 账号名里的 `#` 要换成 `-`。搜索响应给回来的账号名已经是换过的,
/// 所以这个替换是幂等的:用户手输的 `heygyus#0416` 和列里的 `heygyus-0416`
/// 都能走通同一条路。
#[must_use]
pub fn character_url(version: &str, account: &str, name: &str, snapshot_name: &str) -> String {
    format!(
        "{BUILDS_BASE}/{}/character?account={}&name={}&overview={}&timeMachine=",
        encode(version),
        encode(&account.replace('#', "-")),
        encode(name),
        encode(snapshot_name)
    )
}

/// 经济接口要的是联赛**显示名**("Forbidden Rites"),不是 builds 那边的短名。
#[must_use]
pub fn currency_rates_url(league: &str) -> String {
    format!(
        "{ECONOMY_BASE}/exchange/current/overview?league={}&type=Currency",
        encode(league)
    )
}

#[must_use]
pub fn unique_prices_url(league: &str, type_name: &str) -> String {
    format!(
        "{ECONOMY_BASE}/stash/current/item/overview?league={}&type={}",
        encode(league),
        encode(type_name)
    )
}

/// application/x-www-form-urlencoded 风格:空格写成 `+`。
/// 实测 poe.ninja 两种写法都收,选 `+` 是因为 URL 贴到浏览器里更好读。
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// 持有连接池的 poe.ninja 客户端。一轮采样要打几十上百个请求,
/// 每次现建 agent 会把 TLS 握手也做几百遍。
pub struct NinjaClient {
    agent: ureq::Agent,
}

impl Default for NinjaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl NinjaClient {
    #[must_use]
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build();
        Self {
            agent: config.into(),
        }
    }

    /// 所有请求的唯一出口:一个地方管 User-Agent、gzip 和长度上限。
    /// (gzip 由 ureq 的 `gzip` feature 自动协商,不用手写 Accept-Encoding。)
    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, NinjaError> {
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(classify_transport)?;
        response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_vec()
            .map_err(classify_transport)
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, NinjaError> {
        let bytes = self.get_bytes(url)?;
        serde_json::from_slice(&bytes).map_err(|error| NinjaError::Decode(error.to_string()))
    }

    /// 每轮采样的第一步:快照 `version` 一天变好几次,别的接口全要它。
    pub fn index_state(&self) -> Result<IndexState, NinjaError> {
        self.get_json(&index_state_url())
    }

    /// 每个联赛的角色总数和职业占比。分区计划按这里的占比挑职业。
    pub fn build_index_state(&self) -> Result<BuildIndexState, NinjaError> {
        self.get_json(&build_index_state_url())
    }

    /// 原始 protobuf 字节。单独暴露是为了让采样管线能先落盘、再解析——
    /// 解析失败时人还能看到原文。
    pub fn search_raw(
        &self,
        version: &str,
        snapshot_name: &str,
        filters: &[(&str, &str)],
    ) -> Result<Vec<u8>, NinjaError> {
        self.get_bytes(&search_url(version, snapshot_name, filters))
    }

    pub fn search(
        &self,
        version: &str,
        snapshot_name: &str,
        filters: &[(&str, &str)],
    ) -> Result<SearchResponse, NinjaError> {
        let bytes = self.search_raw(version, snapshot_name, filters)?;
        decode(&bytes).map_err(|error| NinjaError::Decode(error.to_string()))
    }

    /// NDIC 字符串表。按 sha1 寻址、内容不可变,所以调用方可以放心永久缓存。
    pub fn dictionary(&self, sha1: &str) -> Result<Vec<String>, NinjaError> {
        let bytes = self.get_bytes(&dictionary_url(sha1))?;
        parse_ndic(&bytes).map_err(|error| NinjaError::Decode(error.to_string()))
    }

    /// 角色详情的原文。采样管线把它整段存进 `ninja.sqlite`,
    /// 词缀统计随时可以从原文重建,不用重新联网。
    pub fn character_raw(
        &self,
        version: &str,
        account: &str,
        name: &str,
        snapshot_name: &str,
    ) -> Result<String, NinjaError> {
        let bytes = self.get_bytes(&character_url(version, account, name, snapshot_name))?;
        String::from_utf8(bytes).map_err(|error| NinjaError::Decode(error.to_string()))
    }

    pub fn character(
        &self,
        version: &str,
        account: &str,
        name: &str,
        snapshot_name: &str,
    ) -> Result<CharacterDetail, NinjaError> {
        self.get_json(&character_url(version, account, name, snapshot_name))
    }

    pub fn currency_rates(&self, league: &str) -> Result<ExchangeOverview, NinjaError> {
        self.get_json(&currency_rates_url(league))
    }

    pub fn unique_prices(&self, league: &str, type_name: &str) -> Result<ItemOverview, NinjaError> {
        self.get_json(&unique_prices_url(league, type_name))
    }
}

/// 把 ureq 的错分成"对面拒绝""包太大""根本没连上"三类,别的都当连不上。
fn classify_transport(error: ureq::Error) -> NinjaError {
    match error {
        ureq::Error::StatusCode(status) => NinjaError::Rejected(status),
        ureq::Error::BodyExceedsLimit(_) => NinjaError::TooLarge,
        other => NinjaError::Unreachable(other.to_string()),
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;

    const VERSION: &str = "1508-20260906-55820";

    #[test]
    fn data_urls_are_locked() {
        assert_eq!(
            index_state_url(),
            "https://poe.ninja/poe2/api/data/index-state"
        );
        assert_eq!(
            build_index_state_url(),
            "https://poe.ninja/poe2/api/data/build-index-state"
        );
    }

    #[test]
    fn search_url_without_filters_is_locked() {
        assert_eq!(
            search_url(VERSION, "forbidden-rites", &[]),
            "https://poe.ninja/poe2/api/builds/1508-20260906-55820/search?overview=forbidden-rites"
        );
    }

    /// 空格写成 `+`(实测 poe.ninja 接受),多个筛选按给定顺序拼在后面。
    #[test]
    fn search_url_encodes_filters_with_plus_for_space() {
        assert_eq!(
            search_url(
                VERSION,
                "forbidden-rites",
                &[
                    ("class", "Gemling Legionnaire"),
                    ("items", "Wake of Destruction"),
                ]
            ),
            "https://poe.ninja/poe2/api/builds/1508-20260906-55820/search\
             ?overview=forbidden-rites&class=Gemling+Legionnaire&items=Wake+of+Destruction"
        );
    }

    #[test]
    fn search_url_percent_encodes_the_awkward_characters() {
        assert_eq!(
            search_url(
                VERSION,
                "forbidden-rites",
                &[("items", "Berek's Grip & Co")]
            ),
            "https://poe.ninja/poe2/api/builds/1508-20260906-55820/search\
             ?overview=forbidden-rites&items=Berek%27s+Grip+%26+Co"
        );
    }

    #[test]
    fn dictionary_url_is_locked() {
        assert_eq!(
            dictionary_url("4b3dfeccb2f4aa52ccc458925cd87f4575a4c25b"),
            "https://poe.ninja/poe2/api/builds/dictionary/4b3dfeccb2f4aa52ccc458925cd87f4575a4c25b"
        );
    }

    /// 账号名里的 `#` 是 URL 的片段分隔符,不换掉的话 query 会被整段截掉。
    #[test]
    fn character_url_turns_the_account_hash_into_a_dash() {
        assert_eq!(
            character_url(
                VERSION,
                "heygyus#0416",
                "ResurrectForbidden",
                "forbidden-rites"
            ),
            "https://poe.ninja/poe2/api/builds/1508-20260906-55820/character\
             ?account=heygyus-0416&name=ResurrectForbidden&overview=forbidden-rites&timeMachine="
        );
    }

    /// 搜索响应里的账号名已经是 `-` 形式,再换一次不能改变结果。
    #[test]
    fn character_url_is_idempotent_for_already_dashed_accounts() {
        assert_eq!(
            character_url(
                VERSION,
                "heygyus-0416",
                "ResurrectForbidden",
                "forbidden-rites"
            ),
            character_url(
                VERSION,
                "heygyus#0416",
                "ResurrectForbidden",
                "forbidden-rites"
            )
        );
    }

    #[test]
    fn economy_urls_are_locked() {
        assert_eq!(
            currency_rates_url("Forbidden Rites"),
            "https://poe.ninja/poe2/api/economy/exchange/current/overview\
             ?league=Forbidden+Rites&type=Currency"
        );
        assert_eq!(
            unique_prices_url("Forbidden Rites", "UniqueAccessories"),
            "https://poe.ninja/poe2/api/economy/stash/current/item/overview\
             ?league=Forbidden+Rites&type=UniqueAccessories"
        );
    }

    #[test]
    fn encoding_leaves_the_unreserved_set_alone() {
        assert_eq!(encode("aZ0-_.~"), "aZ0-_.~");
        assert_eq!(encode("护甲"), "%E6%8A%A4%E7%94%B2");
        assert_eq!(encode("a/b?c=d"), "a%2Fb%3Fc%3Dd");
    }
}
