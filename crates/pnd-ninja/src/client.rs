//! poe.ninja 的 HTTP 层。
//!
//! 纪律照搬兄弟项目的 `ptt-exchange-history/src/fetch.rs`:一个带连接池的
//! `ureq::Agent`、全局超时、显式 User-Agent、响应体长度上限、错误分成
//! "对面拒绝"和"根本没连上"两类。这里刻意**不重试、不睡眠**——
//! "一小时 100 个请求""24 小时不重跑"属于采样管线的节奏,不是网络层的事。
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

/// 哪一代流放之路。**和交易站那半边是同一个类型**([`pnd_domain::Game`]):
/// 设置页选的那一个要能直接喂给采样管线,中间不该有一次翻译。
///
/// poe.ninja 把两代摆在**同一个站点的两个前缀**下:`/poe2/api/…` 和
/// `/poe1/api/…`。2026-09-09 实测:`/api/data/index-state`(不带前缀的老路径)
/// 是 404,`/poe1/api/data/index-state` 是 200,而且顶层键和 PoE2 那份一模一样
/// —— 也就是说 builds 这半边两代是**同一条管线,只差一个前缀**。
pub use pnd_domain::Game;

fn prefix(game: Game) -> &'static str {
    match game {
        Game::Poe1 => "/poe1",
        Game::Poe2 => "/poe2",
    }
}

fn builds_base(game: Game) -> String {
    format!("https://poe.ninja{}/api/builds", prefix(game))
}

fn data_base(game: Game) -> String {
    format!("https://poe.ninja{}/api/data", prefix(game))
}

fn economy_base(game: Game) -> String {
    format!("https://poe.ninja{}/api/economy", prefix(game))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NinjaError {
    #[error("poe.ninja rejected the request with status {0}")]
    Rejected(u16),
    /// 429。单独一档而不是 `Rejected(429)`,是因为**只有它带着一句服务端自己的
    /// 建议**(`Retry-After`),而调用方对它的处置也和别的拒绝完全不同:
    /// 别的拒绝是"这条抓不到了",429 是"你太快了,等等再来"。
    #[error("poe.ninja asked us to slow down (429)")]
    RateLimited { retry_after_secs: Option<u64> },
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
pub fn index_state_url(game: Game) -> String {
    format!("{}/index-state", data_base(game))
}

#[must_use]
pub fn build_index_state_url(game: Game) -> String {
    format!("{}/build-index-state", data_base(game))
}

/// `filters` 是 `class`/`skills`/`items`/`keypassives`/`spiritgems`/`allskills`/
/// `timeMachine`/`sort` 这些查询参数,多个条件是 AND。
///
/// PoE1 没有 spirit gem,那边少一个 `spiritgems`;多给一个不认识的筛选**不会**
/// 被拒,服务端直接忽略(2026-09-09 实测)。
#[must_use]
pub fn search_url(
    game: Game,
    version: &str,
    snapshot_name: &str,
    filters: &[(&str, &str)],
) -> String {
    let mut url = format!(
        "{}/{}/search?overview={}",
        builds_base(game),
        encode(version),
        encode(snapshot_name)
    );
    for (key, value) in filters {
        let _ = write!(url, "&{}={}", encode(key), encode(value));
    }
    url
}

#[must_use]
pub fn dictionary_url(game: Game, sha1: &str) -> String {
    format!("{}/dictionary/{}", builds_base(game), encode(sha1))
}

/// 账号名里的 `#` 要换成 `-`。搜索响应给回来的账号名已经是换过的,
/// 所以这个替换是幂等的:用户手输的 `heygyus#0416` 和列里的 `heygyus-0416`
/// 都能走通同一条路。
#[must_use]
pub fn character_url(
    game: Game,
    version: &str,
    account: &str,
    name: &str,
    snapshot_name: &str,
) -> String {
    format!(
        "{}/{}/character?account={}&name={}&overview={}&timeMachine=",
        builds_base(game),
        encode(version),
        encode(&account.replace('#', "-")),
        encode(name),
        encode(snapshot_name)
    )
}

/// 经济接口要的是联赛**显示名**("Forbidden Rites"、"Allflame"),
/// 不是 builds 那边的短名。
///
/// 路径两代一样,只差前缀 —— 但返回的 JSON **不一样**:PoE1 的物品榜是老那套
/// `chaosValue`/`divineValue`,见 [`crate::economy`]。
#[must_use]
pub fn currency_rates_url(game: Game, league: &str) -> String {
    format!(
        "{}/exchange/current/overview?league={}&type=Currency",
        economy_base(game),
        encode(league)
    )
}

/// PoE1 的分类名是**单数**(`UniqueWeapon`),PoE2 是复数(`UniqueWeapons`)。
/// 写错不是空榜,是 404 —— 所以两张表都写死在 [`crate::economy::unique_types_for`]。
#[must_use]
pub fn unique_prices_url(game: Game, league: &str, type_name: &str) -> String {
    format!(
        "{}/stash/current/item/overview?league={}&type={}",
        economy_base(game),
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
    game: Game,
}

impl Default for NinjaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl NinjaClient {
    /// PoE2 的客户端。**签名刻意不带参数**:今天所有调用方问的都是 PoE2,
    /// 让它们一个字都不用改,PoE1 的调用方走 [`NinjaClient::for_game`]。
    #[must_use]
    pub fn new() -> Self {
        Self::for_game(Game::Poe2)
    }

    #[must_use]
    pub fn for_game(game: Game) -> Self {
        // 关掉 `http_status_as_error`(和 `pnd-trade` 那个 agent 同一个理由):
        // ureq 3 默认把 4xx/5xx 变成一个只剩状态码的错误,响应头连同 429 的
        // `Retry-After` 一起被扔掉。我们要那一行,所以自己判状态。
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
            game,
        }
    }

    /// 这个客户端问的是哪一代游戏。存储按它分表、日志按它标行。
    #[must_use]
    pub fn game(&self) -> Game {
        self.game
    }

    /// 任意一个 poe.ninja URL 的原始字节,走的还是同一个 UA / 状态码 / 长度上限。
    ///
    /// 存在的唯一理由是**探路**:问一个我们还没有拼装函数的路径存不存在
    /// (比如"PoE1 有没有 PoE2 那种 `/api/economy/…`"),404 本身就是答案。
    /// 日常抓取一律走下面那些具名方法,别用这个绕过 URL 拼装。
    pub fn raw_get(&self, url: &str) -> Result<Vec<u8>, NinjaError> {
        self.get_bytes(url)
    }

    /// 所有请求的唯一出口:一个地方管 User-Agent、gzip、状态码和长度上限。
    /// (gzip 由 ureq 的 `gzip` feature 自动协商,不用手写 Accept-Encoding。)
    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, NinjaError> {
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(classify_transport)?;

        let status = response.status().as_u16();
        if status == 429 {
            return Err(NinjaError::RateLimited {
                retry_after_secs: response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_retry_after),
            });
        }
        if !(200..300).contains(&status) {
            return Err(NinjaError::Rejected(status));
        }

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
        self.get_json(&index_state_url(self.game))
    }

    /// 每个联赛的角色总数和职业占比。分区计划按这里的占比挑职业。
    pub fn build_index_state(&self) -> Result<BuildIndexState, NinjaError> {
        self.get_json(&build_index_state_url(self.game))
    }

    /// 原始 protobuf 字节。单独暴露是为了让采样管线能先落盘、再解析——
    /// 解析失败时人还能看到原文。
    pub fn search_raw(
        &self,
        version: &str,
        snapshot_name: &str,
        filters: &[(&str, &str)],
    ) -> Result<Vec<u8>, NinjaError> {
        self.get_bytes(&search_url(self.game, version, snapshot_name, filters))
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
        let bytes = self.get_bytes(&dictionary_url(self.game, sha1))?;
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
        let bytes = self.get_bytes(&character_url(
            self.game,
            version,
            account,
            name,
            snapshot_name,
        ))?;
        String::from_utf8(bytes).map_err(|error| NinjaError::Decode(error.to_string()))
    }

    pub fn character(
        &self,
        version: &str,
        account: &str,
        name: &str,
        snapshot_name: &str,
    ) -> Result<CharacterDetail, NinjaError> {
        self.get_json(&character_url(
            self.game,
            version,
            account,
            name,
            snapshot_name,
        ))
    }

    pub fn currency_rates(&self, league: &str) -> Result<ExchangeOverview, NinjaError> {
        self.get_json(&currency_rates_url(self.game, league))
    }

    /// 暗金参考价。PoE1 的原文形状和 PoE2 不一样,所以出口处统一摊平成一种
    /// (见 [`ItemOverview::normalized_for`])——调用方不该关心是哪一代。
    pub fn unique_prices(&self, league: &str, type_name: &str) -> Result<ItemOverview, NinjaError> {
        let overview: ItemOverview =
            self.get_json(&unique_prices_url(self.game, league, type_name))?;
        Ok(overview.normalized_for(self.game))
    }
}

/// `Retry-After` 的值 → 秒。
///
/// RFC 允许两种写法:一个秒数,或者一个 HTTP 日期。poe.ninja 用的是秒数,
/// 日期那种**故意不解析** —— 为了一个没见过的写法引一个日期库不划算,
/// 认不出来时调用方自己有一套退避阶梯兜着,比解析出一个错的秒数安全。
#[must_use]
pub fn parse_retry_after(raw: &str) -> Option<u64> {
    let secs: u64 = raw.trim().parse().ok()?;
    // 上限一小时:一个离谱的头不该把后台线程钉在那儿睡一整天。
    Some(secs.min(3_600))
}

/// 把 ureq 的错分成"对面拒绝""包太大""根本没连上"三类,别的都当连不上。
///
/// agent 关了 `http_status_as_error`,所以 `StatusCode` 理论上不会再出现;
/// 留着这一支是因为 ureq 内部(比如跟重定向)还可能自己抛一个。
fn classify_transport(error: ureq::Error) -> NinjaError {
    match error {
        ureq::Error::StatusCode(429) => NinjaError::RateLimited {
            retry_after_secs: None,
        },
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
            index_state_url(Game::Poe2),
            "https://poe.ninja/poe2/api/data/index-state"
        );
        assert_eq!(
            build_index_state_url(Game::Poe2),
            "https://poe.ninja/poe2/api/data/build-index-state"
        );
    }

    #[test]
    fn search_url_without_filters_is_locked() {
        assert_eq!(
            search_url(Game::Poe2, VERSION, "forbidden-rites", &[]),
            "https://poe.ninja/poe2/api/builds/1508-20260906-55820/search?overview=forbidden-rites"
        );
    }

    /// 空格写成 `+`(实测 poe.ninja 接受),多个筛选按给定顺序拼在后面。
    #[test]
    fn search_url_encodes_filters_with_plus_for_space() {
        assert_eq!(
            search_url(
                Game::Poe2,
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
                Game::Poe2,
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
            dictionary_url(Game::Poe2, "4b3dfeccb2f4aa52ccc458925cd87f4575a4c25b"),
            "https://poe.ninja/poe2/api/builds/dictionary/4b3dfeccb2f4aa52ccc458925cd87f4575a4c25b"
        );
    }

    /// 账号名里的 `#` 是 URL 的片段分隔符,不换掉的话 query 会被整段截掉。
    #[test]
    fn character_url_turns_the_account_hash_into_a_dash() {
        assert_eq!(
            character_url(
                Game::Poe2,
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
                Game::Poe2,
                VERSION,
                "heygyus-0416",
                "ResurrectForbidden",
                "forbidden-rites"
            ),
            character_url(
                Game::Poe2,
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
            currency_rates_url(Game::Poe2, "Forbidden Rites"),
            "https://poe.ninja/poe2/api/economy/exchange/current/overview\
             ?league=Forbidden+Rites&type=Currency"
        );
        assert_eq!(
            unique_prices_url(Game::Poe2, "Forbidden Rites", "UniqueAccessories"),
            "https://poe.ninja/poe2/api/economy/stash/current/item/overview\
             ?league=Forbidden+Rites&type=UniqueAccessories"
        );
    }

    /// builds 这半边两代**只差一个前缀**:`/poe1` 换 `/poe2`,路径、查询参数、
    /// 账号名的 `#` → `-` 全都一样。
    ///
    /// 不带前缀的老路径(`https://poe.ninja/api/data/index-state`)是 404,
    /// 2026-09-09 实测过 —— 所以这里钉的是 `/poe1`,不是空前缀。
    #[test]
    fn poe1_builds_urls_only_swap_the_prefix() {
        assert_eq!(
            index_state_url(Game::Poe1),
            "https://poe.ninja/poe1/api/data/index-state"
        );
        assert_eq!(
            build_index_state_url(Game::Poe1),
            "https://poe.ninja/poe1/api/data/build-index-state"
        );
        assert_eq!(
            search_url(
                Game::Poe1,
                "3260-20260908-1",
                "keepers-of-the-flame",
                &[("class", "Witch")]
            ),
            "https://poe.ninja/poe1/api/builds/3260-20260908-1/search\
             ?overview=keepers-of-the-flame&class=Witch"
        );
        assert_eq!(
            dictionary_url(Game::Poe1, "4b3dfeccb2f4aa52ccc458925cd87f4575a4c25b"),
            "https://poe.ninja/poe1/api/builds/dictionary/\
             4b3dfeccb2f4aa52ccc458925cd87f4575a4c25b"
        );
        assert_eq!(
            character_url(
                Game::Poe1,
                "3260-20260908-1",
                "sound#1234",
                "Bob",
                "keepers-of-the-flame"
            ),
            "https://poe.ninja/poe1/api/builds/3260-20260908-1/character\
             ?account=sound-1234&name=Bob&overview=keepers-of-the-flame&timeMachine="
        );
    }

    /// 经济这半边**路径**也只差前缀,差的是**分类名**:PoE1 是单数
    /// `UniqueWeapon`,PoE2 是复数 `UniqueWeapons`。
    ///
    /// 2026-09-09 实测:`…/poe1/api/economy/stash/current/item/overview
    /// ?league=Allflame&type=UniqueWeapons` 是 **404**,换成 `UniqueWeapon` 才 200。
    /// 分类名写错不是"空榜",是直接 404。
    #[test]
    fn poe1_economy_urls_swap_the_prefix_and_use_singular_types() {
        assert_eq!(
            currency_rates_url(Game::Poe1, "Allflame"),
            "https://poe.ninja/poe1/api/economy/exchange/current/overview\
             ?league=Allflame&type=Currency"
        );
        assert_eq!(
            unique_prices_url(Game::Poe1, "Allflame", "UniqueWeapon"),
            "https://poe.ninja/poe1/api/economy/stash/current/item/overview\
             ?league=Allflame&type=UniqueWeapon"
        );
    }

    /// `Game` 本身归 `pnd-domain` 管(解析、serde、Display 都在那儿测)。
    /// 这一条只钉住这个 crate 用得着的那一半:两个写法各自对应哪个前缀。
    #[test]
    fn each_game_keeps_its_own_url_prefix() {
        assert_eq!(prefix(Game::Poe1), "/poe1");
        assert_eq!(prefix(Game::Poe2), "/poe2");
        assert_eq!(Game::Poe1.as_str(), "poe1");
        // 默认是 PoE2:今天在跑的采样就是它,换默认值等于悄悄改行为。
        assert_eq!(Game::default(), Game::Poe2);
    }

    #[test]
    fn encoding_leaves_the_unreserved_set_alone() {
        assert_eq!(encode("aZ0-_.~"), "aZ0-_.~");
        assert_eq!(encode("护甲"), "%E6%8A%A4%E7%94%B2");
        assert_eq!(encode("a/b?c=d"), "a%2Fb%3Fc%3Dd");
    }

    /// `Retry-After` 只认秒数那种写法,认不出来就交回给调用方的退避阶梯。
    #[test]
    fn retry_after_reads_seconds_and_ignores_everything_else() {
        assert_eq!(parse_retry_after("60"), Some(60));
        assert_eq!(parse_retry_after(" 120 "), Some(120));
        assert_eq!(parse_retry_after("0"), Some(0));
        // HTTP 日期那种写法:不解析,不猜。
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("-5"), None);
        // 离谱的值钉在一小时,而不是让后台线程睡一整天。
        assert_eq!(parse_retry_after("86400"), Some(3_600));
    }

    /// 429 有自己的一档:调用方靠它区分"这条抓不到了"和"你太快了"。
    #[test]
    fn a_rate_limit_is_its_own_kind_of_error() {
        assert_eq!(
            classify_transport(ureq::Error::StatusCode(429)),
            NinjaError::RateLimited {
                retry_after_secs: None
            }
        );
        assert_eq!(
            classify_transport(ureq::Error::StatusCode(404)),
            NinjaError::Rejected(404)
        );
    }
}
