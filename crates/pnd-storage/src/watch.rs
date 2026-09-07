//! `watch.sqlite`:每条蹲价搜索的运行状态、已经见过的挂单、提醒历史。
//!
//! 两条纪律,和兄弟项目一致:
//!
//! - **库里没有时钟。** 所有时间都是调用方传进来的 unix 秒(`i64`),
//!   这样测试可以把时间钉死,不用等真的过一秒。
//! - **金额存千分整数。** `seen_listings` 的主键里带着价格,浮点会让
//!   "同一个价格"时不时不相等,去重就漏了。
//!
//! 去重键是 `(watch_id, listing_id, price_milli, price_currency)`:同一件东西
//! 降价重挂算一条新单,值得再叫你一次;原价还挂在那儿就不该反复响。
//!
//! 那一行上的 `verdict` 记的是**这一声叫过没有**:记成 `hit` 才算叫过。
//! 上限是你随时会改的东西,一件当初太贵、只是被记了一笔的货,在你把上限
//! 提上去之后必须还能响 —— 否则"我调高了上限却没动静"就是必然。

use std::path::Path;
use std::time::Duration;

use pnd_domain::{Currency, ListingSummary, Price, Verdict, WatchId};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not prepare the database folder: {0}")]
    Io(#[from] std::io::Error),
}

/// 无价挂单在库里的写法。
///
/// 主键里有价格,不能放 NULL(SQLite 的主键列允许 NULL,而 NULL != NULL,
/// 同一条无价单会一遍遍算新单)。用一个不可能出现的负数金额 + 空货币顶上,
/// 读回来时按空货币判成"没有价格"。
const UNPRICED_MILLI: i64 = -1;

const BASELINE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS watch_state (
    watch_id TEXT PRIMARY KEY,
    league TEXT NOT NULL,
    search_id TEXT NOT NULL,
    query_json TEXT,
    last_poll_at INTEGER,
    last_live_at INTEGER,
    live_state TEXT NOT NULL DEFAULT 'off',
    last_total INTEGER,
    failures INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS seen_listings (
    watch_id TEXT NOT NULL,
    listing_id TEXT NOT NULL,
    price_milli INTEGER NOT NULL,
    price_currency TEXT NOT NULL,
    verdict TEXT NOT NULL,
    account TEXT NOT NULL,
    indexed_at TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    PRIMARY KEY (watch_id, listing_id, price_milli, price_currency)
) STRICT;

CREATE TABLE IF NOT EXISTS alerts (
    alert_id INTEGER PRIMARY KEY AUTOINCREMENT,
    watch_id TEXT NOT NULL,
    listing_id TEXT NOT NULL,
    league TEXT NOT NULL,
    search_id TEXT NOT NULL,
    item_name TEXT NOT NULL,
    price_milli INTEGER NOT NULL,
    price_currency TEXT NOT NULL,
    account TEXT NOT NULL,
    character TEXT NOT NULL,
    whisper TEXT NOT NULL,
    hideout_token TEXT,
    token_fetched_at INTEGER,
    source TEXT NOT NULL,
    fired_at INTEGER NOT NULL,
    dismissed_at INTEGER,
    last_action TEXT
) STRICT;

CREATE INDEX IF NOT EXISTS alerts_fired ON alerts(fired_at DESC);
"#;

/// 一条搜索的 live 连接现在处在哪一档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveState {
    #[default]
    Off,
    Connecting,
    Connected,
    Backoff,
}

impl LiveState {
    pub fn as_str(&self) -> &'static str {
        match self {
            LiveState::Off => "off",
            LiveState::Connecting => "connecting",
            LiveState::Connected => "connected",
            LiveState::Backoff => "backoff",
        }
    }

    /// 认不出来的一律当 `Off`:老库里的值、手工改过的值,都不该让整行读不出来 ——
    /// 大不了这条搜索这一轮先走轮询,下一次连接会把状态写回正确的档。
    pub fn parse(raw: &str) -> LiveState {
        match raw {
            "connecting" => LiveState::Connecting,
            "connected" => LiveState::Connected,
            "backoff" => LiveState::Backoff,
            _ => LiveState::Off,
        }
    }
}

/// 这次提醒是轮询发现的还是 live 秒推来的。用来在提醒记录里回答
/// "WebSocket 到底有没有在干活"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSource {
    Poll,
    Live,
}

impl AlertSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertSource::Poll => "poll",
            AlertSource::Live => "live",
        }
    }

    pub fn parse(raw: &str) -> AlertSource {
        match raw {
            "live" => AlertSource::Live,
            _ => AlertSource::Poll,
        }
    }
}

/// 记一条挂单的结果:`New` 才值得往下走判定。
///
/// "见过"说的是**已经为它叫过一次**,不是"这条数据我读到过":一件太贵的货
/// 会进表,但它还没叫过,所以上限一提它照样能响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeenOutcome {
    New,
    AlreadySeen,
}

/// 一条搜索的运行状态。设置里那条 `WatchEntry` 是"用户填的",这一行是"跑出来的"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchState {
    pub watch_id: WatchId,
    pub league: String,
    pub search_id: String,
    /// 搜索 id 解码出来的查询 JSON。存下来是为了程序离线也能看懂这条搜索在找什么,
    /// 以及省掉每轮一次的解压。
    pub query_json: Option<String>,
    pub last_poll_at: Option<i64>,
    pub last_live_at: Option<i64>,
    pub live_state: LiveState,
    /// 上一轮 search 返回的 `total`,用来在界面上显示"市面上有多少件"。
    pub last_total: Option<u64>,
    pub failures: u32,
    pub updated_at: i64,
}

/// 要写进提醒历史的一条新提醒。全借用:它只在调用现场活一瞬间,
/// 造它的地方手上正好有这几样东西。
#[derive(Debug, Clone)]
pub struct NewAlert<'a> {
    pub watch_id: &'a WatchId,
    pub league: &'a str,
    pub search_id: &'a str,
    pub listing: &'a ListingSummary,
    pub source: AlertSource,
}

/// 提醒历史里的一行。`price` 是从两列拼回来的 —— 无价单读出来就是 `None`。
#[derive(Debug, Clone, PartialEq)]
pub struct AlertRow {
    pub alert_id: i64,
    pub watch_id: WatchId,
    pub listing_id: String,
    pub league: String,
    pub search_id: String,
    pub item_name: String,
    pub price: Option<Price>,
    pub account: String,
    pub character: String,
    pub whisper: String,
    pub hideout_token: Option<String>,
    pub token_fetched_at: Option<i64>,
    pub source: AlertSource,
    pub fired_at: i64,
    pub dismissed_at: Option<i64>,
    pub last_action: Option<String>,
}

pub struct WatchStore {
    /// 同一个连接给两套表用:蹲价那几张(本文件)和市场观察那几张
    /// (`observe.rs`)。一个文件、一个连接、一份 busy timeout —— 分成两个
    /// 连接只会让两边在同一个 `watch.sqlite` 上互相等锁。
    pub(crate) conn: Connection,
}

impl WatchStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        // 第一次启动时 `%LOCALAPPDATA%\PoeNinjaData` 还不存在,SQLite 不会替你建目录。
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        Self::initialize(conn)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(conn: Connection) -> Result<Self, StorageError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // 没有 busy timeout 的话,第一次撞车就直接 SQLITE_BUSY 失败;
        // 5 秒足够让探针和主程序共用同一个文件而互不打断。
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(BASELINE_SCHEMA)?;
        conn.execute_batch(crate::observe::OBSERVE_SCHEMA)?;
        Ok(Self { conn })
    }

    // ---- watch_state ----------------------------------------------------

    /// 整行写回。第一次是插入,之后每次都是全量覆盖 —— 这一行本来就只有
    /// 一个写者(runtime 的 actor 线程),没有"各改各的字段"这种事。
    pub fn upsert_watch_state(&self, state: &WatchState) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO watch_state (watch_id, league, search_id, query_json, last_poll_at,
                 last_live_at, live_state, last_total, failures, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(watch_id) DO UPDATE SET
                 league = excluded.league,
                 search_id = excluded.search_id,
                 query_json = excluded.query_json,
                 last_poll_at = excluded.last_poll_at,
                 last_live_at = excluded.last_live_at,
                 live_state = excluded.live_state,
                 last_total = excluded.last_total,
                 failures = excluded.failures,
                 updated_at = excluded.updated_at",
            params![
                state.watch_id.as_str(),
                state.league,
                state.search_id,
                state.query_json,
                state.last_poll_at,
                state.last_live_at,
                state.live_state.as_str(),
                state.last_total.map(to_i64),
                i64::from(state.failures),
                state.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn watch_state(&self, watch_id: &WatchId) -> Result<Option<WatchState>, StorageError> {
        let state = self
            .conn
            .query_row(
                "SELECT watch_id, league, search_id, query_json, last_poll_at, last_live_at,
                        live_state, last_total, failures, updated_at
                 FROM watch_state WHERE watch_id = ?1",
                params![watch_id.as_str()],
                watch_state_from_row,
            )
            .optional()?;
        Ok(state)
    }

    /// 一轮轮询跑完了。`total` 是 search 返回的挂单总数,失败那轮就是 `None`。
    pub fn touch_poll(
        &self,
        watch_id: &WatchId,
        now: i64,
        total: Option<u64>,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE watch_state
             SET last_poll_at = ?2, last_total = ?3, updated_at = ?2
             WHERE watch_id = ?1",
            params![watch_id.as_str(), now, total.map(to_i64)],
        )?;
        Ok(())
    }

    /// 换 live 档位。连上的那一刻顺手记 `last_live_at`:界面上"WS 还活着吗"
    /// 问的是这个时间戳离现在多远,而不是状态字符串本身。
    pub fn set_live_state(
        &self,
        watch_id: &WatchId,
        state: LiveState,
        now: i64,
    ) -> Result<(), StorageError> {
        let connected = state == LiveState::Connected;
        self.conn.execute(
            "UPDATE watch_state
             SET live_state = ?2,
                 last_live_at = CASE WHEN ?4 THEN ?3 ELSE last_live_at END,
                 updated_at = ?3
             WHERE watch_id = ?1",
            params![watch_id.as_str(), state.as_str(), now, connected],
        )?;
        Ok(())
    }

    /// 失败计数 +1,返回加完之后的值 —— 退避算的就是 `interval * 2^n` 里的那个 n。
    /// 行不存在时返回 0(还没 upsert 过状态,也就谈不上失败)。
    pub fn bump_failures(&self, watch_id: &WatchId, now: i64) -> Result<u32, StorageError> {
        let failures: Option<i64> = self
            .conn
            .query_row(
                "UPDATE watch_state SET failures = failures + 1, updated_at = ?2
                 WHERE watch_id = ?1 RETURNING failures",
                params![watch_id.as_str(), now],
                |row| row.get(0),
            )
            .optional()?;
        Ok(failures.map(to_u32).unwrap_or(0))
    }

    pub fn clear_failures(&self, watch_id: &WatchId, now: i64) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE watch_state SET failures = 0, updated_at = ?2 WHERE watch_id = ?1",
            params![watch_id.as_str(), now],
        )?;
        Ok(())
    }

    // ---- seen_listings --------------------------------------------------

    /// 记下"这条搜索见过这件东西、这个价",并回答"这一声还没叫过吗"。
    ///
    /// `New` = 这个价第一次进表,**或者**这一行头一回够便宜。所以一件当初
    /// 太贵、被记下来的货,在你把上限提上去之后会重新算一次新单 —— 早先这里
    /// 只看"这行插进去了没有",于是上限一改,先前记下的那些反而永远叫不响了。
    ///
    /// 已经叫过的只把 `last_seen_at` 往前推,好知道它还挂在市面上。
    pub fn record_listing(
        &self,
        watch_id: &WatchId,
        listing: &ListingSummary,
        verdict: Verdict,
        now: i64,
    ) -> Result<SeenOutcome, StorageError> {
        let (price_milli, price_currency) = encode_price(listing.price.as_ref());
        let inserted = self.conn.execute(
            "INSERT INTO seen_listings (watch_id, listing_id, price_milli, price_currency,
                 verdict, account, indexed_at, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(watch_id, listing_id, price_milli, price_currency) DO NOTHING",
            params![
                watch_id.as_str(),
                listing.id,
                price_milli,
                price_currency,
                verdict_code(verdict),
                listing.account,
                listing.indexed,
                now,
            ],
        )?;
        if inserted > 0 {
            // 这个价第一次进表。够不够便宜是判定那一层的事,这里只说"没见过"。
            return Ok(SeenOutcome::New);
        }
        // 冲突分支单独写一条 UPDATE,而不是 `DO UPDATE`:一条 upsert 语句
        // 无论插入还是更新都报"改了 1 行",分不出这单是不是新的。
        //
        // 存着的 `verdict` 就是"这一行叫过没有":一旦记成 `hit` 就不再改回去,
        // 否则用户把上限调低再调回来,同一件货会再响一次。
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT verdict FROM seen_listings
                 WHERE watch_id = ?1 AND listing_id = ?2
                   AND price_milli = ?3 AND price_currency = ?4",
                params![watch_id.as_str(), listing.id, price_milli, price_currency],
                |row| row.get(0),
            )
            .optional()?;
        let alerted = stored.as_deref() == Some(verdict_code(Verdict::Hit));
        let verdict = if alerted { Verdict::Hit } else { verdict };
        self.conn.execute(
            "UPDATE seen_listings SET last_seen_at = ?5, verdict = ?6
             WHERE watch_id = ?1 AND listing_id = ?2
               AND price_milli = ?3 AND price_currency = ?4",
            params![
                watch_id.as_str(),
                listing.id,
                price_milli,
                price_currency,
                now,
                verdict_code(verdict),
            ],
        )?;
        Ok(if !alerted && verdict == Verdict::Hit {
            SeenOutcome::New
        } else {
            SeenOutcome::AlreadySeen
        })
    }

    // ---- alerts ---------------------------------------------------------

    /// 写一条提醒历史,返回它的 id(卡片按钮回写状态时认这个号)。
    pub fn insert_alert(&self, alert: &NewAlert<'_>, now: i64) -> Result<i64, StorageError> {
        let listing = alert.listing;
        let (price_milli, price_currency) = encode_price(listing.price.as_ref());
        // token 是短命 JWT,记下拿到它的时刻:点"去藏身处"时超过 10 分钟就先重新 fetch。
        let token_fetched_at = listing.hideout_token.as_ref().map(|_| now);
        self.conn.execute(
            "INSERT INTO alerts (watch_id, listing_id, league, search_id, item_name,
                 price_milli, price_currency, account, character, whisper,
                 hideout_token, token_fetched_at, source, fired_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                alert.watch_id.as_str(),
                listing.id,
                alert.league,
                alert.search_id,
                listing.item_name,
                price_milli,
                price_currency,
                listing.account,
                listing.character,
                listing.whisper,
                listing.hideout_token,
                token_fetched_at,
                alert.source.as_str(),
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn alert(&self, alert_id: i64) -> Result<Option<AlertRow>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT alert_id, watch_id, listing_id, league, search_id, item_name,
                        price_milli, price_currency, account, character, whisper,
                        hideout_token, token_fetched_at, source, fired_at, dismissed_at, last_action
                 FROM alerts WHERE alert_id = ?1",
                params![alert_id],
                alert_row_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// 提醒记录页要的那一页:最新的在最前面。
    pub fn recent_alerts(&self, limit: u32) -> Result<Vec<AlertRow>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT alert_id, watch_id, listing_id, league, search_id, item_name,
                    price_milli, price_currency, account, character, whisper,
                    hideout_token, token_fetched_at, source, fired_at, dismissed_at, last_action
             FROM alerts ORDER BY fired_at DESC, alert_id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![i64::from(limit)], alert_row_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 卡片收起(用户点了忽略,或者到点自动收)。
    pub fn mark_dismissed(&self, alert_id: i64, now: i64) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE alerts SET dismissed_at = ?2 WHERE alert_id = ?1",
            params![alert_id, now],
        )?;
        Ok(())
    }

    /// 用户在这张卡片上最后做了什么(`opened_trade` / `copied_whisper` / …)。
    pub fn set_last_action(&self, alert_id: i64, action: &str) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE alerts SET last_action = ?2 WHERE alert_id = ?1",
            params![alert_id, action],
        )?;
        Ok(())
    }

    /// 重新 fetch 拿到的新 token。时间戳一起换,否则刷新完还是被当成过期的。
    pub fn set_hideout_token(
        &self,
        alert_id: i64,
        token: &str,
        now: i64,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE alerts SET hideout_token = ?2, token_fetched_at = ?3 WHERE alert_id = ?1",
            params![alert_id, token, now],
        )?;
        Ok(())
    }

    /// 这条搜索从 `since` 起响过几次。界面上"今天叫了 3 次"就是它。
    pub fn hits_since(&self, watch_id: &WatchId, since: i64) -> Result<u32, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM alerts WHERE watch_id = ?1 AND fired_at >= ?2",
            params![watch_id.as_str(), since],
            |row| row.get(0),
        )?;
        Ok(to_u32(count))
    }

    /// 删掉一条搜索:运行状态和去重记录都不留,**提醒历史留着**。
    /// 历史是"我当时看到过什么",和这条搜索还在不在没关系。
    pub fn delete_watch(&self, watch_id: &WatchId) -> Result<(), StorageError> {
        self.conn.execute(
            "DELETE FROM seen_listings WHERE watch_id = ?1",
            params![watch_id.as_str()],
        )?;
        self.conn.execute(
            "DELETE FROM watch_state WHERE watch_id = ?1",
            params![watch_id.as_str()],
        )?;
        Ok(())
    }
}

/// `Verdict` 在库里的写法。判定枚举住在 domain 层,存储层只负责给它一个稳定的字面量。
fn verdict_code(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Hit => "hit",
        Verdict::TooExpensive => "too_expensive",
        Verdict::DifferentCurrency => "different_currency",
        Verdict::Unpriced => "unpriced",
    }
}

/// 价格拆成两列。无价单走 `UNPRICED_MILLI` + 空货币,见本文件顶部的说明。
pub(crate) fn encode_price(price: Option<&Price>) -> (i64, String) {
    match price {
        Some(price) => (price.amount_milli, price.currency.code().to_string()),
        None => (UNPRICED_MILLI, String::new()),
    }
}

/// 反过来拼回一个价格。判据是货币列为空,不是金额为负 ——
/// 空货币是写入时的约定,金额只是跟着走。
pub(crate) fn decode_price(amount_milli: i64, currency: &str) -> Option<Price> {
    if currency.is_empty() {
        return None;
    }
    Some(Price::new(amount_milli, Currency::parse(currency)))
}

/// 计数类的 `u64` 进库。SQLite 只有有符号 64 位整数,溢出的那一刻钉在上限
/// 而不是绕回负数 —— 挂单总数不可能真的到这个量级,真到了也是接口出了问题。
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(crate) fn to_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn watch_state_from_row(row: &Row<'_>) -> rusqlite::Result<WatchState> {
    let live_state: String = row.get(6)?;
    let last_total: Option<i64> = row.get(7)?;
    let failures: i64 = row.get(8)?;
    Ok(WatchState {
        watch_id: WatchId(row.get(0)?),
        league: row.get(1)?,
        search_id: row.get(2)?,
        query_json: row.get(3)?,
        last_poll_at: row.get(4)?,
        last_live_at: row.get(5)?,
        live_state: LiveState::parse(&live_state),
        last_total: last_total.map(|value| u64::try_from(value).unwrap_or(0)),
        failures: to_u32(failures),
        updated_at: row.get(9)?,
    })
}

fn alert_row_from_row(row: &Row<'_>) -> rusqlite::Result<AlertRow> {
    let price_milli: i64 = row.get(6)?;
    let price_currency: String = row.get(7)?;
    let source: String = row.get(13)?;
    Ok(AlertRow {
        alert_id: row.get(0)?,
        watch_id: WatchId(row.get(1)?),
        listing_id: row.get(2)?,
        league: row.get(3)?,
        search_id: row.get(4)?,
        item_name: row.get(5)?,
        price: decode_price(price_milli, &price_currency),
        account: row.get(8)?,
        character: row.get(9)?,
        whisper: row.get(10)?,
        hideout_token: row.get(11)?,
        token_fetched_at: row.get(12)?,
        source: AlertSource::parse(&source),
        fired_at: row.get(14)?,
        dismissed_at: row.get(15)?,
        last_action: row.get(16)?,
    })
}

#[cfg(test)]
mod watch_tests {
    use super::*;

    fn store() -> WatchStore {
        WatchStore::open_in_memory().expect("open")
    }

    fn watch(id: &str) -> WatchId {
        WatchId(id.to_string())
    }

    fn listing(id: &str, price: Option<Price>) -> ListingSummary {
        ListingSummary {
            id: id.to_string(),
            item_name: "Choir of the Storm".to_string(),
            type_line: "Lapis Amulet".to_string(),
            price,
            account: "SomeSeller#1234".to_string(),
            character: "SomeChar".to_string(),
            online: true,
            afk: false,
            indexed: "2026-09-06T12:00:00Z".to_string(),
            whisper: "@SomeChar Hi, I'd like to buy...".to_string(),
            whisper_token: None,
            hideout_token: None,
            icon: "https://web.poecdn.com/image/item.png".to_string(),
            item_json: String::new(),
        }
    }

    fn divine(amount_milli: i64) -> Option<Price> {
        Some(Price::new(amount_milli, Currency::Divine))
    }

    fn state(id: &str, now: i64) -> WatchState {
        WatchState {
            watch_id: watch(id),
            league: "Forbidden Rites".to_string(),
            search_id: "H4sIAAAA-_09".to_string(),
            query_json: Some(r#"{"name":"Choir of the Storm"}"#.to_string()),
            last_poll_at: None,
            last_live_at: None,
            live_state: LiveState::Off,
            last_total: None,
            failures: 0,
            updated_at: now,
        }
    }

    fn new_alert<'a>(watch_id: &'a WatchId, listing: &'a ListingSummary) -> NewAlert<'a> {
        NewAlert {
            watch_id,
            league: "Forbidden Rites",
            search_id: "H4sIAAAA-_09",
            listing,
            source: AlertSource::Poll,
        }
    }

    /// 建表语句必须能在同一个库上跑第二遍:每次开库都会执行它。
    #[test]
    fn schema_applies_twice() {
        let store = store();
        store
            .conn
            .execute_batch(BASELINE_SCHEMA)
            .expect("second apply");
    }

    #[test]
    fn watch_state_round_trips_and_updates() {
        let store = store();
        let id = watch("w-1");
        store
            .upsert_watch_state(&state("w-1", 100))
            .expect("insert");
        assert_eq!(
            store.watch_state(&id).expect("read"),
            Some(state("w-1", 100))
        );

        // 同一个 id 再写一次是覆盖,不是第二行。
        let mut changed = state("w-1", 200);
        changed.live_state = LiveState::Backoff;
        changed.failures = 3;
        store.upsert_watch_state(&changed).expect("update");
        let read = store.watch_state(&id).expect("read").expect("row");
        assert_eq!(read.live_state, LiveState::Backoff);
        assert_eq!(read.failures, 3);
        assert_eq!(read.updated_at, 200);

        assert_eq!(store.watch_state(&watch("nope")).expect("read"), None);
    }

    #[test]
    fn poll_live_and_failure_columns_move_independently() {
        let store = store();
        let id = watch("w-1");
        store
            .upsert_watch_state(&state("w-1", 100))
            .expect("insert");

        store.touch_poll(&id, 150, Some(42)).expect("touch");
        let read = store.watch_state(&id).expect("read").expect("row");
        assert_eq!(read.last_poll_at, Some(150));
        assert_eq!(read.last_total, Some(42));
        assert_eq!(read.last_live_at, None, "轮询不该动 live 的时间戳");

        // 只有"连上了"才记 last_live_at。
        store
            .set_live_state(&id, LiveState::Connecting, 160)
            .expect("connecting");
        assert_eq!(
            store
                .watch_state(&id)
                .expect("read")
                .expect("row")
                .last_live_at,
            None
        );
        store
            .set_live_state(&id, LiveState::Connected, 170)
            .expect("connected");
        let read = store.watch_state(&id).expect("read").expect("row");
        assert_eq!(read.live_state, LiveState::Connected);
        assert_eq!(read.last_live_at, Some(170));

        assert_eq!(store.bump_failures(&id, 180).expect("bump"), 1);
        assert_eq!(store.bump_failures(&id, 190).expect("bump"), 2);
        store.clear_failures(&id, 200).expect("clear");
        assert_eq!(
            store.watch_state(&id).expect("read").expect("row").failures,
            0
        );
        // 没有这一行的时候不该炸,也不该凭空造一行。
        assert_eq!(store.bump_failures(&watch("nope"), 210).expect("bump"), 0);
    }

    /// 同一件东西、同一个价,只算一次;降价重挂算新单。
    #[test]
    fn a_listing_is_new_once_per_price() {
        let store = store();
        let id = watch("w-1");
        let twenty = listing("aaa", divine(20_000));
        assert_eq!(
            store
                .record_listing(&id, &twenty, Verdict::Hit, 100)
                .expect("record"),
            SeenOutcome::New
        );
        assert_eq!(
            store
                .record_listing(&id, &twenty, Verdict::Hit, 200)
                .expect("record"),
            SeenOutcome::AlreadySeen
        );

        let cheaper = listing("aaa", divine(15_000));
        assert_eq!(
            store
                .record_listing(&id, &cheaper, Verdict::Hit, 300)
                .expect("record"),
            SeenOutcome::New,
            "降价重挂值得再叫一次"
        );

        // 另一条搜索见到同一件东西,是它自己的第一次。
        assert_eq!(
            store
                .record_listing(&watch("w-2"), &twenty, Verdict::Hit, 400)
                .expect("record"),
            SeenOutcome::New
        );

        // 再见到时只推 last_seen_at,first_seen_at 不动。
        let (first, last): (i64, i64) = store
            .conn
            .query_row(
                "SELECT first_seen_at, last_seen_at FROM seen_listings
                 WHERE watch_id = 'w-1' AND listing_id = 'aaa' AND price_milli = 20000",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read");
        assert_eq!((first, last), (100, 200));
    }

    /// 太贵的那一行不该把以后的提醒堵死。
    ///
    /// 现场:上限 221 的时候看到一件 222 的,它进了去重表;用户随后把上限
    /// 改成 223,同一件东西再看到时如果还算"见过了",这一声就永远不会响。
    #[test]
    fn a_listing_first_seen_as_too_expensive_is_new_again_once_it_hits() {
        let store = store();
        let id = watch("w-1");
        let item = listing("aaa", divine(222_000));
        assert_eq!(
            store
                .record_listing(&id, &item, Verdict::TooExpensive, 100)
                .expect("record"),
            SeenOutcome::New
        );
        // 还是太贵:没什么新鲜的。
        assert_eq!(
            store
                .record_listing(&id, &item, Verdict::TooExpensive, 110)
                .expect("record"),
            SeenOutcome::AlreadySeen
        );
        // 上限提上去了 —— 这件货对用户来说是第一次"够便宜"。
        assert_eq!(
            store
                .record_listing(&id, &item, Verdict::Hit, 120)
                .expect("record"),
            SeenOutcome::New,
            "上限提上去之后这件货该重新算一次新单"
        );
        // 叫过一次就够了。
        assert_eq!(
            store
                .record_listing(&id, &item, Verdict::Hit, 130)
                .expect("record"),
            SeenOutcome::AlreadySeen
        );
        // 上限又调回去也不该把"已经叫过"这件事忘掉。
        assert_eq!(
            store
                .record_listing(&id, &item, Verdict::TooExpensive, 140)
                .expect("record"),
            SeenOutcome::AlreadySeen
        );
        assert_eq!(
            store
                .record_listing(&id, &item, Verdict::Hit, 150)
                .expect("record"),
            SeenOutcome::AlreadySeen
        );
    }

    /// 无价单也要能进去重表:主键里有价格,NULL 会让它每轮都算新单。
    #[test]
    fn an_unpriced_listing_still_dedupes() {
        let store = store();
        let id = watch("w-1");
        let unpriced = listing("bbb", None);
        assert_eq!(
            store
                .record_listing(&id, &unpriced, Verdict::Unpriced, 100)
                .expect("record"),
            SeenOutcome::New
        );
        assert_eq!(
            store
                .record_listing(&id, &unpriced, Verdict::Unpriced, 110)
                .expect("record"),
            SeenOutcome::AlreadySeen
        );
    }

    #[test]
    fn an_alert_round_trips_every_field() {
        let store = store();
        let id = watch("w-1");
        let mut item = listing("aaa", divine(15_500));
        item.hideout_token = Some("jwt-token".to_string());
        let mut alert = new_alert(&id, &item);
        alert.source = AlertSource::Live;

        let alert_id = store.insert_alert(&alert, 1_000).expect("insert");
        let row = store.alert(alert_id).expect("read").expect("row");
        assert_eq!(
            row,
            AlertRow {
                alert_id,
                watch_id: watch("w-1"),
                listing_id: "aaa".to_string(),
                league: "Forbidden Rites".to_string(),
                search_id: "H4sIAAAA-_09".to_string(),
                item_name: "Choir of the Storm".to_string(),
                price: divine(15_500),
                account: "SomeSeller#1234".to_string(),
                character: "SomeChar".to_string(),
                whisper: "@SomeChar Hi, I'd like to buy...".to_string(),
                hideout_token: Some("jwt-token".to_string()),
                token_fetched_at: Some(1_000),
                source: AlertSource::Live,
                fired_at: 1_000,
                dismissed_at: None,
                last_action: None,
            }
        );

        assert_eq!(store.alert(9_999).expect("read"), None);
    }

    /// 无价单的提醒读回来是 `None`,不是 `-1 ` 那个哨兵值。
    #[test]
    fn an_unpriced_alert_reads_back_as_no_price() {
        let store = store();
        let id = watch("w-1");
        let item = listing("bbb", None);
        let alert_id = store
            .insert_alert(&new_alert(&id, &item), 1_000)
            .expect("insert");
        let row = store.alert(alert_id).expect("read").expect("row");
        assert_eq!(row.price, None);
        assert_eq!(row.token_fetched_at, None, "没有 token 就没有拿到它的时刻");
    }

    #[test]
    fn dismissing_and_acting_on_an_alert_is_recorded() {
        let store = store();
        let id = watch("w-1");
        let item = listing("aaa", divine(15_000));
        let alert_id = store
            .insert_alert(&new_alert(&id, &item), 1_000)
            .expect("insert");

        store.mark_dismissed(alert_id, 1_300).expect("dismiss");
        store
            .set_last_action(alert_id, "opened_trade")
            .expect("action");
        store
            .set_hideout_token(alert_id, "fresh-jwt", 1_400)
            .expect("token");

        let row = store.alert(alert_id).expect("read").expect("row");
        assert_eq!(row.dismissed_at, Some(1_300));
        assert_eq!(row.last_action.as_deref(), Some("opened_trade"));
        assert_eq!(row.hideout_token.as_deref(), Some("fresh-jwt"));
        assert_eq!(row.token_fetched_at, Some(1_400));
    }

    #[test]
    fn recent_alerts_are_newest_first_and_limited() {
        let store = store();
        let id = watch("w-1");
        for (index, fired_at) in [1_000i64, 3_000, 2_000].into_iter().enumerate() {
            let item = listing(&format!("id-{index}"), divine(15_000));
            store
                .insert_alert(&new_alert(&id, &item), fired_at)
                .expect("insert");
        }
        let rows = store.recent_alerts(10).expect("read");
        assert_eq!(
            rows.iter().map(|row| row.fired_at).collect::<Vec<_>>(),
            vec![3_000, 2_000, 1_000]
        );
        assert_eq!(store.recent_alerts(2).expect("read").len(), 2);
    }

    #[test]
    fn hits_since_counts_only_this_watch() {
        let store = store();
        let one = watch("w-1");
        let two = watch("w-2");
        let item = listing("aaa", divine(15_000));
        for fired_at in [1_000i64, 2_000, 3_000] {
            store
                .insert_alert(&new_alert(&one, &item), fired_at)
                .expect("insert");
        }
        store
            .insert_alert(&new_alert(&two, &item), 3_000)
            .expect("insert");

        assert_eq!(store.hits_since(&one, 0).expect("count"), 3);
        assert_eq!(store.hits_since(&one, 2_000).expect("count"), 2);
        assert_eq!(store.hits_since(&one, 9_000).expect("count"), 0);
        assert_eq!(store.hits_since(&two, 0).expect("count"), 1);
    }

    /// 删搜索删掉的是"还要不要继续跑",不是"我当时看到过什么"。
    #[test]
    fn deleting_a_watch_keeps_its_alerts() {
        let store = store();
        let id = watch("w-1");
        store
            .upsert_watch_state(&state("w-1", 100))
            .expect("insert");
        let item = listing("aaa", divine(15_000));
        store
            .record_listing(&id, &item, Verdict::Hit, 100)
            .expect("record");
        let alert_id = store
            .insert_alert(&new_alert(&id, &item), 100)
            .expect("insert");

        store.delete_watch(&id).expect("delete");

        assert_eq!(store.watch_state(&id).expect("read"), None);
        assert!(store.alert(alert_id).expect("read").is_some());
        assert_eq!(store.hits_since(&id, 0).expect("count"), 1);
        // 去重记录跟着走:同一个 id 重新加回来时,该重新提醒一次。
        assert_eq!(
            store
                .record_listing(&id, &item, Verdict::Hit, 500)
                .expect("record"),
            SeenOutcome::New
        );
    }
}
