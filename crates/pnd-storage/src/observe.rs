//! `watch.sqlite` 上的市场观察那几张表:见过哪些挂单、价格怎么走的、
//! 哪些不见了、身上都有什么词缀。
//!
//! 为什么和蹲价共用一个库文件:计划里就是这么定的,而且两边问的其实是同一批
//! 数据的两个侧面(一个"现在有没有便宜货",一个"这批货最后都怎么样了")。
//! 一个文件、一个连接([`WatchStore`] 那一个),就不会有两个连接在同一个
//! SQLite 文件上互相等锁。
//!
//! 纪律和 `watch.rs` 一模一样:
//!
//! - **库里没有时钟。** 时间全是调用方传进来的 unix 秒。
//! - **金额存千分整数**,货币单独一列;无价单走 `UNPRICED_MILLI` + 空货币。
//! - **判定不在这里。** "它是卖掉了还是撤了"是 `pnd_domain::classify_gone`
//!   那个纯函数的事,这里只负责把结论存下来 —— 存储层不该有观点。
//!
//! 一条挂单在这里的一生:`record_seen` 第一次见到它(连同物品原文和词缀)→
//! 之后每次 discover/recheck 见到就推 `last_seen_at`、改价就往
//! `observed_price_history` 里追加一行 → 某次 recheck 查不到了,调用方拿
//! `classify_gone` 判一下,`mark_gone` 把结论写在行上。

use std::collections::{BTreeMap, BTreeSet};

use pnd_domain::{GoneClass, ListingSummary, ObservationId, Price, next_check_after};
use pnd_ninja::character::{line_numbers, mod_template};
use rusqlite::{OptionalExtension, Row, params};

use crate::watch::{StorageError, WatchStore, decode_price, encode_price, to_u32};

/// 建表语句。和 `BASELINE_SCHEMA` 一起在每次开库时跑一遍,所以必须能跑第二遍。
pub(crate) const OBSERVE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS observation_state (
    obs_id TEXT PRIMARY KEY,
    league TEXT NOT NULL,
    search_id TEXT NOT NULL,
    query_json TEXT,
    last_discover_at INTEGER,
    last_recheck_at INTEGER,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS observed_listings (
    obs_id TEXT NOT NULL,
    listing_id TEXT NOT NULL,
    item_name TEXT NOT NULL,
    item_json TEXT NOT NULL,
    seller TEXT NOT NULL,
    indexed_at TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    currency TEXT NOT NULL,
    first_price_milli INTEGER NOT NULL,
    last_price_milli INTEGER NOT NULL,
    price_changes INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    gone_at INTEGER,
    gone_class TEXT,
    check_rung INTEGER NOT NULL DEFAULT 0,
    next_check_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (obs_id, listing_id)
) STRICT;

CREATE INDEX IF NOT EXISTS observed_listings_status
    ON observed_listings(obs_id, status, last_seen_at);

CREATE INDEX IF NOT EXISTS observed_listings_due
    ON observed_listings(status, next_check_at);

CREATE TABLE IF NOT EXISTS observed_price_history (
    obs_id TEXT NOT NULL,
    listing_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    price_milli INTEGER NOT NULL,
    currency TEXT NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS observed_price_history_listing
    ON observed_price_history(obs_id, listing_id, at);

CREATE TABLE IF NOT EXISTS observed_mods (
    obs_id TEXT NOT NULL,
    listing_id TEXT NOT NULL,
    mod_kind TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    template TEXT NOT NULL,
    value1 REAL,
    value2 REAL,
    PRIMARY KEY (obs_id, listing_id, mod_kind, ordinal)
) STRICT;

CREATE INDEX IF NOT EXISTS observed_mods_template
    ON observed_mods(obs_id, template);
"#;

/// 交易站 `item` 里的词缀数组,和它在库里的那个名字。
///
/// 顺序就是行里 `ordinal` 的分组顺序;认不出的键整片忽略(接口以后加了
/// 新的一类词缀,不该让整件货的词缀都记不进去)。
const MOD_ARRAYS: [(&str, &str); 7] = [
    ("implicitMods", "implicit"),
    ("explicitMods", "explicit"),
    ("craftedMods", "crafted"),
    ("enchantMods", "enchant"),
    ("runeMods", "rune"),
    ("desecratedMods", "desecrated"),
    ("fracturedMods", "fractured"),
];

/// `next_check_at` 上的"别再查了"。
///
/// 用一个哨兵而不是把这一列改成可空:回查的取数条件是
/// `next_check_at <= now`,哨兵天然过不了那一关,而可空列还要在每条查询里
/// 多写一个 `IS NOT NULL`,漏写一处就会有一批七天前的老货被反复问。
pub const NEVER_CHECK_AGAIN: i64 = i64::MAX;

/// 一条到点该回查的挂单。
///
/// 带着 `first_seen_at` 和 `check_rung` 一起出来,是因为回信回来时要算
/// "下一档排在什么时候"([`pnd_domain::next_check_after`]),而那两个数在
/// 这一趟里不会变 —— 再回库读一次只是多一次查询。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueListing {
    pub obs_id: ObservationId,
    pub listing_id: String,
    pub first_seen_at: i64,
    pub check_rung: u32,
}

/// 一条挂单现在是还挂着还是没了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ObservedStatus {
    #[default]
    Active,
    Gone,
}

impl ObservedStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ObservedStatus::Active => "active",
            ObservedStatus::Gone => "gone",
        }
    }

    /// 认不出来的当"还挂着":最坏的结果是下一轮回查再问一次,
    /// 而反过来会凭空多出一条"卖掉了"。
    #[must_use]
    pub fn parse(raw: &str) -> ObservedStatus {
        match raw {
            "gone" => ObservedStatus::Gone,
            _ => ObservedStatus::Active,
        }
    }
}

/// 一次 `record_seen` 的结果。
///
/// 调用方只在 `PriceChanged` 上做别的事(界面上标一笔降价);`New` 和
/// `Unchanged` 都只是"记下了"。
#[derive(Debug, Clone, PartialEq)]
pub enum SeenKind {
    /// 头一回见到这条挂单:物品原文和词缀也是这一刻存下来的。
    New,
    /// 同一条挂单换了价(或者换了货币)。
    PriceChanged {
        from: Option<Price>,
        to: Option<Price>,
    },
    /// 见过,价格没动 —— 只把 `last_seen_at` 往前推了。
    Unchanged,
}

/// 一条观察在库里的运行状态。设置里那条 `ObservationEntry` 是"用户填的",
/// 这一行是"跑出来的"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRun {
    pub obs_id: ObservationId,
    pub league: String,
    pub search_id: String,
    /// 搜索 id 解出来的查询原文。存着是为了离线也能看懂这条观察在盯什么。
    pub query_json: Option<String>,
    pub last_discover_at: Option<i64>,
    pub last_recheck_at: Option<i64>,
    pub updated_at: i64,
}

/// 观察到的一条挂单。
#[derive(Debug, Clone, PartialEq)]
pub struct ObservedListingRow {
    pub listing_id: String,
    pub item_name: String,
    /// 第一次见到它时 `item` 那一整块的原文。**不随后续 fetch 更新** ——
    /// 我们要的是"这件货长什么样",而那是不会变的。
    pub item_json: String,
    pub seller: String,
    pub indexed_at: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub first_price: Option<Price>,
    pub last_price: Option<Price>,
    pub price_changes: u32,
    pub status: ObservedStatus,
    pub gone_at: Option<i64>,
    pub gone_class: Option<GoneClass>,
    /// 回查阶梯走到第几档了。见 [`pnd_domain::next_check_after`]。
    pub check_rung: u32,
    /// 下一次该回头看它的时刻;[`NEVER_CHECK_AGAIN`] = 不再看了。
    pub next_check_at: i64,
}

impl ObservedListingRow {
    /// 我们**看见**它活了多久(秒)。判定用的也是这个口径,见
    /// [`pnd_domain::observed_lifetime_secs`]。
    #[must_use]
    pub fn observed_lifetime_secs(&self) -> i64 {
        pnd_domain::observed_lifetime_secs(self.first_seen_at, self.last_seen_at)
    }
}

/// 价格轨迹上的一个点。
#[derive(Debug, Clone, PartialEq)]
pub struct PricePoint {
    pub at: i64,
    pub price: Option<Price>,
}

/// 一条观察现在的账:还挂着几条、没了几条、没了的那些分别判成了什么。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObservationSummary {
    pub active: u32,
    pub gone: u32,
    pub sold_likely: u32,
    pub sold_after_cuts: u32,
    pub unknown: u32,
    /// 第一次去抓详情时就已经没了的那些。也算在 `gone` 里,但必须能单独看见:
    /// 它们是秒掉的那一批,占比本身就是"这条搜索里有多少好价"的答案。
    pub gone_before_first_look: u32,
}

/// 聚合表里的一行:某条词缀模板的战绩。
#[derive(Debug, Clone, PartialEq)]
pub struct ModOutcome {
    /// 游戏里那句话,数字换成 `#`(`+# to maximum Life`)。
    pub template: String,
    /// `explicit` / `implicit` / `crafted` / …
    pub mod_kind: String,
    /// 带这条词缀的挂单见过几条。
    pub seen: u32,
    /// 其中已经不见了的有几条(含判成 `Unknown` 的)。
    pub gone: u32,
    /// 其中**看着像卖掉了**的有几条 —— [`GoneClass::looks_sold`],
    /// 也就是 `SoldLikely` + `SoldAfterCuts`。界面上那个"疑似成交率"
    /// 的分子就是它,分母是 `seen`。
    pub sold_likely: u32,
    /// 下面两个中位价按哪种货币算。
    ///
    /// 同一条搜索里偶尔混着别的货币(有人用 exalted 标价),中位数没法跨
    /// 货币算,所以取这一组里最常见的那种,只统计用它标价的挂单;一条有价
    /// 的都没有时是空串。
    pub currency: String,
    /// 卖掉的那些的最后一个价(中位)。
    pub median_gone_price_milli: Option<i64>,
    /// 还挂着的那些的现价(中位)—— 和上面那个一比就是"在售价 vs 成交价"。
    pub median_active_price_milli: Option<i64>,
    /// 卖掉的那些从第一次见到到最后一次见到隔了多久(中位,小时)。
    pub median_hours_alive: Option<f64>,
}

/// 一条挂单身上的一条词缀。
#[derive(Debug, Clone, PartialEq)]
pub struct ObservedMod {
    pub mod_kind: String,
    /// 它在同一组里的第几条(0 起)。主键要它,显示顺序也要它。
    pub ordinal: u32,
    pub template: String,
    /// 这一行里的第一个数(`+115 to maximum Life` 的 115)。
    pub value1: Option<f64>,
    /// 第二个数(`Adds 29 to 38 Cold Damage` 的 38);只有一个数时是 `None`。
    pub value2: Option<f64>,
}

impl WatchStore {
    // ---- observation_state ----------------------------------------------

    /// 整行写回。和 `upsert_watch_state` 一样,这一行只有一个写者(actor)。
    pub fn upsert_observation_state(&self, run: &ObservationRun) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO observation_state (obs_id, league, search_id, query_json,
                 last_discover_at, last_recheck_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(obs_id) DO UPDATE SET
                 league = excluded.league,
                 search_id = excluded.search_id,
                 query_json = excluded.query_json,
                 last_discover_at = excluded.last_discover_at,
                 last_recheck_at = excluded.last_recheck_at,
                 updated_at = excluded.updated_at",
            params![
                run.obs_id.as_str(),
                run.league,
                run.search_id,
                run.query_json,
                run.last_discover_at,
                run.last_recheck_at,
                run.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn observation_state(
        &self,
        obs_id: &ObservationId,
    ) -> Result<Option<ObservationRun>, StorageError> {
        let run = self
            .conn
            .query_row(
                "SELECT obs_id, league, search_id, query_json, last_discover_at,
                        last_recheck_at, updated_at
                 FROM observation_state WHERE obs_id = ?1",
                params![obs_id.as_str()],
                observation_run_from_row,
            )
            .optional()?;
        Ok(run)
    }

    /// 一轮 discover 跑完了。
    pub fn touch_discover(&self, obs_id: &ObservationId, now: i64) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE observation_state SET last_discover_at = ?2, updated_at = ?2 WHERE obs_id = ?1",
            params![obs_id.as_str(), now],
        )?;
        Ok(())
    }

    /// 一轮 recheck 跑完了。
    pub fn touch_recheck(&self, obs_id: &ObservationId, now: i64) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE observation_state SET last_recheck_at = ?2, updated_at = ?2 WHERE obs_id = ?1",
            params![obs_id.as_str(), now],
        )?;
        Ok(())
    }

    // ---- observed_listings ----------------------------------------------

    /// 记一次"我看见它了"。
    ///
    /// 第一次见到就连物品原文和词缀一起存下来;之后每次只推 `last_seen_at`,
    /// 除非价格变了 —— 那要往价格轨迹里追加一行,并把改价次数 +1。
    ///
    /// 见到一条**先前标成没了**的挂单会把它重新算成还挂着:那说明上一轮的
    /// "没了"判错了(搜索那一刻它恰好不在前 100 条里,或者服务端抽了一下),
    /// 而"它还在"是眼见为实的事。
    pub fn record_seen(
        &self,
        obs_id: &ObservationId,
        listing: &ListingSummary,
        now: i64,
    ) -> Result<SeenKind, StorageError> {
        let (price_milli, currency) = encode_price(listing.price.as_ref());
        let previous: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT last_price_milli, currency FROM observed_listings
                 WHERE obs_id = ?1 AND listing_id = ?2",
                params![obs_id.as_str(), listing.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let Some((old_milli, old_currency)) = previous else {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO observed_listings (obs_id, listing_id, item_name, item_json, seller,
                     indexed_at, first_seen_at, last_seen_at, currency, first_price_milli,
                     last_price_milli, price_changes, status, check_rung, next_check_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?9, 0, 'active', 0, ?10)",
                params![
                    obs_id.as_str(),
                    listing.id,
                    listing.item_name,
                    listing.item_json,
                    listing.account,
                    listing.indexed,
                    now,
                    currency,
                    price_milli,
                    first_check_at(now),
                ],
            )?;
            tx.execute(
                "INSERT INTO observed_price_history (obs_id, listing_id, at, price_milli, currency)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![obs_id.as_str(), listing.id, now, price_milli, currency],
            )?;
            for (ordinal_kind, entry) in observed_mods_from_item_json(&listing.item_json)
                .into_iter()
                .map(|entry| (entry.mod_kind.clone(), entry))
            {
                tx.execute(
                    "INSERT INTO observed_mods (obs_id, listing_id, mod_kind, ordinal,
                         template, value1, value2)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(obs_id, listing_id, mod_kind, ordinal) DO NOTHING",
                    params![
                        obs_id.as_str(),
                        listing.id,
                        ordinal_kind,
                        entry.ordinal,
                        entry.template,
                        entry.value1,
                        entry.value2,
                    ],
                )?;
            }
            tx.commit()?;
            return Ok(SeenKind::New);
        };

        if old_milli == price_milli && old_currency == currency {
            self.conn.execute(
                "UPDATE observed_listings
                 SET last_seen_at = ?3, status = 'active', gone_at = NULL, gone_class = NULL
                 WHERE obs_id = ?1 AND listing_id = ?2",
                params![obs_id.as_str(), listing.id, now],
            )?;
            return Ok(SeenKind::Unchanged);
        }

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE observed_listings
             SET last_seen_at = ?3, last_price_milli = ?4, currency = ?5,
                 price_changes = price_changes + 1,
                 status = 'active', gone_at = NULL, gone_class = NULL
             WHERE obs_id = ?1 AND listing_id = ?2",
            params![
                obs_id.as_str(),
                listing.id,
                now,
                price_milli,
                currency.clone()
            ],
        )?;
        tx.execute(
            "INSERT INTO observed_price_history (obs_id, listing_id, at, price_milli, currency)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![obs_id.as_str(), listing.id, now, price_milli, currency],
        )?;
        tx.commit()?;
        Ok(SeenKind::PriceChanged {
            from: decode_price(old_milli, &old_currency),
            to: listing.price.clone(),
        })
    }

    /// 搜索又把这些 id 报回来了 —— 它们还挂着,但**不花一次 fetch**。
    ///
    /// discover 拿回来的只有 id,没有价格,所以这里只推 `last_seen_at`:
    /// 存活时间靠它算,而"改没改价"等下一次真正 fetch 到它时再说。
    /// 返回真的动了几行(不认识的 id 一行都不动)。
    pub fn touch_listings(
        &self,
        obs_id: &ObservationId,
        listing_ids: &[String],
        now: i64,
    ) -> Result<u32, StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut touched = 0u32;
        {
            let mut statement = tx.prepare(
                "UPDATE observed_listings
                 SET last_seen_at = ?3, status = 'active', gone_at = NULL, gone_class = NULL
                 WHERE obs_id = ?1 AND listing_id = ?2",
            )?;
            for listing_id in listing_ids {
                touched +=
                    to_u32(statement.execute(params![obs_id.as_str(), listing_id, now])? as i64);
            }
        }
        tx.commit()?;
        Ok(touched)
    }

    /// 还挂着的那些 id,**最久没见到的排在前面**。
    ///
    /// 顺序不是好看:回查的条数有上限(fetch 额度有限),砍掉尾巴的时候
    /// 该留下的是最可能已经没了的那几条。
    pub fn active_listing_ids(&self, obs_id: &ObservationId) -> Result<Vec<String>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT listing_id FROM observed_listings
             WHERE obs_id = ?1 AND status = 'active'
             ORDER BY last_seen_at ASC, listing_id ASC",
        )?;
        let rows = statement.query_map(params![obs_id.as_str()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 到点该回头看的挂单,**跨所有观察**一起捞,最早该查的排前面。
    ///
    /// 跨观察是有意的:一次 fetch 最多带 10 个 id,凑不满就白花一次额度。
    /// 两条观察各有三条到期的挂单,拼成一批发出去就只花一次。
    /// `in_flight` 是**已经问出去、还没回来**的那些 `(观察, 挂单)`,它们要跳过:
    /// 它们的 `next_check_at` 要等回信才会往前挪,不跳的话每一轮扫描都会把
    /// 同一批重发一遍。
    ///
    /// 跳过是在 SQL **里面**做的(多取 `in_flight.len()` 行再筛),不是取完
    /// 再筛:先 `LIMIT` 后筛的话,万一有一批回信丢了(网关关掉、回信路上
    /// actor 走了),那几条就永远占着最前面 `limit` 个名额,后面的挂单一条
    /// 也轮不上 —— 整条回查线就此停摆,而界面上什么都看不出来。
    pub fn due_listings(
        &self,
        now: i64,
        limit: usize,
        in_flight: &BTreeSet<(ObservationId, String)>,
    ) -> Result<Vec<DueListing>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT obs_id, listing_id, first_seen_at, check_rung FROM observed_listings
             WHERE status = 'active' AND next_check_at <= ?1
             ORDER BY next_check_at ASC, obs_id ASC, listing_id ASC
             LIMIT ?2",
        )?;
        let fetch = limit.saturating_add(in_flight.len());
        let rows = statement.query_map(params![now, fetch as i64], |row| {
            let rung: i64 = row.get(3)?;
            Ok(DueListing {
                obs_id: ObservationId(row.get(0)?),
                listing_id: row.get(1)?,
                first_seen_at: row.get(2)?,
                check_rung: to_u32(rung),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            if in_flight.contains(&(row.obs_id.clone(), row.listing_id.clone())) {
                continue;
            }
            out.push(row);
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }

    /// 看过一眼、它还在:升一档,把下一次排上。`None` = 阶梯走完了
    /// (七天),从此不再回查。
    pub fn advance_rung(
        &self,
        obs_id: &ObservationId,
        listing_id: &str,
        rung: u32,
        next_check_at: Option<i64>,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE observed_listings SET check_rung = ?3, next_check_at = ?4
             WHERE obs_id = ?1 AND listing_id = ?2",
            params![
                obs_id.as_str(),
                listing_id,
                i64::from(rung),
                next_check_at.unwrap_or(NEVER_CHECK_AGAIN),
            ],
        )?;
        Ok(())
    }

    /// 界面上那个"立刻回查一次":把这条观察在册的挂单全部推到此刻到期。
    ///
    /// 不自己发请求 —— 推到期之后,下一次扫描会照常按额度分批把它们查完,
    /// 于是"手动查一次"和"自动查一次"走的是同一条路,不会有第二套限速账。
    /// 返回推了几条。
    pub fn mark_all_due(&self, obs_id: &ObservationId, now: i64) -> Result<u32, StorageError> {
        let changed = self.conn.execute(
            "UPDATE observed_listings SET next_check_at = ?2
             WHERE obs_id = ?1 AND status = 'active'",
            params![obs_id.as_str(), now],
        )?;
        Ok(to_u32(changed as i64))
    }

    /// 这条观察下一次该回查的时刻(在册挂单里最早的那个)。
    /// 界面上"下一次回查"那一行就是它;一条都没有(或者全都不再查了)是 `None`。
    pub fn next_check_due_at(&self, obs_id: &ObservationId) -> Result<Option<i64>, StorageError> {
        let at: Option<i64> = self.conn.query_row(
            "SELECT MIN(next_check_at) FROM observed_listings
             WHERE obs_id = ?1 AND status = 'active' AND next_check_at < ?2",
            params![obs_id.as_str(), NEVER_CHECK_AGAIN],
            |row| row.get(0),
        )?;
        Ok(at)
    }

    /// 第一次去抓它详情就已经没了。
    ///
    /// 记一行**空壳**:没有物品原文、没有词缀、没有价格 —— 那些我们从来
    /// 没见过,编一个假的会毒化聚合表。留下的只有"这么一条挂单存在过,
    /// 而且没等到我们看它第一眼" —— 那恰恰是最值钱的一条信息:秒掉的都是好价。
    ///
    /// 已经在册的 id 一个字都不动(返回 `false`):它可能正活得好好的,
    /// 一次超时的 fetch 不该把它写成"没了"。
    pub fn record_gone_before_first_look(
        &self,
        obs_id: &ObservationId,
        listing_id: &str,
        now: i64,
    ) -> Result<bool, StorageError> {
        let (unpriced, no_currency) = encode_price(None);
        let inserted = self.conn.execute(
            "INSERT INTO observed_listings (obs_id, listing_id, item_name, item_json, seller,
                 indexed_at, first_seen_at, last_seen_at, currency, first_price_milli,
                 last_price_milli, price_changes, status, gone_at, gone_class,
                 check_rung, next_check_at)
             VALUES (?1, ?2, '', '', '', '', ?3, ?3, ?4, ?5, ?5, 0, 'gone', ?3, ?6, 0, ?7)
             ON CONFLICT(obs_id, listing_id) DO NOTHING",
            params![
                obs_id.as_str(),
                listing_id,
                now,
                no_currency,
                unpriced,
                GoneClass::GoneBeforeFirstLook.as_str(),
                NEVER_CHECK_AGAIN,
            ],
        )?;
        Ok(inserted > 0)
    }

    /// 这条挂单不见了,判定结果一起写上。
    ///
    /// 只动还挂着的行:一条已经标成没了的挂单不该因为第二次回查又被改一次
    /// `gone_at`(那会让存活时间越查越长)。
    pub fn mark_gone(
        &self,
        obs_id: &ObservationId,
        listing_id: &str,
        now: i64,
        class: GoneClass,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE observed_listings
             SET status = 'gone', gone_at = ?3, gone_class = ?4
             WHERE obs_id = ?1 AND listing_id = ?2 AND status = 'active'",
            params![obs_id.as_str(), listing_id, now, class.as_str()],
        )?;
        Ok(())
    }

    /// 这条挂单在**某个已知的时刻**就没了 —— 而那个时刻比我们发现它早。
    ///
    /// 和 [`WatchStore::mark_gone`] 的差别只有一处:`last_seen_at` 也被拉回
    /// `gone_at`,而不是留在"上一次回查看见它"那一刻。
    ///
    /// 为什么要这一版:交易站告诉我们一条挂单没了的方式是 `item.verified`
    /// 变成 `false`,同一条响应里 `listing.indexed` 已经被顶到"发现它不见了"
    /// 的那一趟索引 —— 也就是说服务端顺手告诉了我们它大概什么时候走的。
    /// 拿那个时刻当 `gone_at`,存活时间就是 `gone_at − first_seen_at`;
    /// 而 `last_seen_at` 要是还停在六小时前的那次回查,存活时间会被算成
    /// "从第一次见到到最后一次看见它还在",两头都不对。
    pub fn mark_gone_at(
        &self,
        obs_id: &ObservationId,
        listing_id: &str,
        gone_at: i64,
        class: GoneClass,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE observed_listings
             SET status = 'gone', gone_at = ?3, last_seen_at = ?3, gone_class = ?4
             WHERE obs_id = ?1 AND listing_id = ?2 AND status = 'active'",
            params![obs_id.as_str(), listing_id, gone_at, class.as_str()],
        )?;
        Ok(())
    }

    pub fn observed_listing(
        &self,
        obs_id: &ObservationId,
        listing_id: &str,
    ) -> Result<Option<ObservedListingRow>, StorageError> {
        let row = self
            .conn
            .query_row(
                &format!("{LISTING_COLUMNS} WHERE obs_id = ?1 AND listing_id = ?2"),
                params![obs_id.as_str(), listing_id],
                observed_listing_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// 最近不见的那些,新的在前。挂单流的左半边。
    pub fn recent_gone(
        &self,
        obs_id: &ObservationId,
        limit: u32,
    ) -> Result<Vec<ObservedListingRow>, StorageError> {
        self.listing_page(
            &format!(
                "{LISTING_COLUMNS} WHERE obs_id = ?1 AND status = 'gone'
                 ORDER BY gone_at DESC, listing_id ASC LIMIT ?2"
            ),
            obs_id,
            limit,
        )
    }

    /// 挂得最久还没卖掉的那些。挂单流的右半边,也是"没人要"那一栏。
    pub fn oldest_active(
        &self,
        obs_id: &ObservationId,
        limit: u32,
    ) -> Result<Vec<ObservedListingRow>, StorageError> {
        self.listing_page(
            &format!(
                "{LISTING_COLUMNS} WHERE obs_id = ?1 AND status = 'active'
                 ORDER BY first_seen_at ASC, listing_id ASC LIMIT ?2"
            ),
            obs_id,
            limit,
        )
    }

    fn listing_page(
        &self,
        sql: &str,
        obs_id: &ObservationId,
        limit: u32,
    ) -> Result<Vec<ObservedListingRow>, StorageError> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map(
            params![obs_id.as_str(), i64::from(limit)],
            observed_listing_from_row,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 一条观察的账:还挂着几条、没了几条、没了的分别判成了什么。
    pub fn observation_summary(
        &self,
        obs_id: &ObservationId,
    ) -> Result<ObservationSummary, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT status, COALESCE(gone_class, ''), COUNT(*)
             FROM observed_listings WHERE obs_id = ?1
             GROUP BY status, COALESCE(gone_class, '')",
        )?;
        let rows = statement.query_map(params![obs_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut summary = ObservationSummary::default();
        for row in rows {
            let (status, class, count) = row?;
            let count = to_u32(count);
            if ObservedStatus::parse(&status) == ObservedStatus::Active {
                summary.active += count;
                continue;
            }
            summary.gone += count;
            match GoneClass::parse(&class) {
                GoneClass::SoldLikely => summary.sold_likely += count,
                GoneClass::SoldAfterCuts => summary.sold_after_cuts += count,
                GoneClass::Unknown => summary.unknown += count,
                GoneClass::GoneBeforeFirstLook => summary.gone_before_first_look += count,
            }
        }
        Ok(summary)
    }

    // ---- observed_price_history / observed_mods -------------------------

    /// 一条挂单的价格轨迹,老的在前。判定("消失前降过价没有")读的就是它。
    pub fn price_history(
        &self,
        obs_id: &ObservationId,
        listing_id: &str,
    ) -> Result<Vec<PricePoint>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT at, price_milli, currency FROM observed_price_history
             WHERE obs_id = ?1 AND listing_id = ?2 ORDER BY at ASC",
        )?;
        let rows = statement.query_map(params![obs_id.as_str(), listing_id], |row| {
            let at: i64 = row.get(0)?;
            let milli: i64 = row.get(1)?;
            let currency: String = row.get(2)?;
            Ok(PricePoint {
                at,
                price: decode_price(milli, &currency),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 一条挂单身上的词缀,按类型和顺序排好。
    pub fn observed_mods(
        &self,
        obs_id: &ObservationId,
        listing_id: &str,
    ) -> Result<Vec<ObservedMod>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT mod_kind, ordinal, template, value1, value2 FROM observed_mods
             WHERE obs_id = ?1 AND listing_id = ?2 ORDER BY mod_kind ASC, ordinal ASC",
        )?;
        let rows = statement.query_map(params![obs_id.as_str(), listing_id], |row| {
            let ordinal: i64 = row.get(1)?;
            Ok(ObservedMod {
                mod_kind: row.get(0)?,
                ordinal: to_u32(ordinal),
                template: row.get(2)?,
                value1: row.get(3)?,
                value2: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 按词缀模板聚合:这条观察攒下来的全部答案就在这张表里。
    ///
    /// `min_samples` 是"至少见过几条带这条词缀的货才值得摆出来" —— 三条货
    /// 里两条卖掉了不能叫 67% 成交率。
    ///
    /// 中位数在 Rust 这一侧算,不在 SQL 里:SQLite 没有中位数函数,而且
    /// "只统计最常见那种货币"这一步用 SQL 写出来会是一段没人看得懂的东西。
    pub fn mod_aggregate(
        &self,
        obs_id: &ObservationId,
        min_samples: u32,
    ) -> Result<Vec<ModOutcome>, StorageError> {
        // DISTINCT 是必需的:同一件货身上可能有两行一模一样的词缀
        // (两条 `+#% to Fire Resistance` 合不到一起),不去重会把它算成两件。
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT m.mod_kind, m.template, m.listing_id, l.status, l.gone_class,
                    l.last_price_milli, l.currency, l.first_seen_at, l.last_seen_at
             FROM observed_mods m
             JOIN observed_listings l
               ON l.obs_id = m.obs_id AND l.listing_id = m.listing_id
             WHERE m.obs_id = ?1 AND m.template <> ''",
        )?;
        let rows = statement.query_map(params![obs_id.as_str()], |row| {
            Ok(AggregateRow {
                mod_kind: row.get(0)?,
                template: row.get(1)?,
                status: ObservedStatus::parse(&row.get::<_, String>(3)?),
                gone_class: row
                    .get::<_, Option<String>>(4)?
                    .map(|raw| GoneClass::parse(&raw)),
                price_milli: row.get(5)?,
                currency: row.get(6)?,
                first_seen_at: row.get(7)?,
                last_seen_at: row.get(8)?,
            })
        })?;

        let mut buckets: BTreeMap<(String, String), Accumulator> = BTreeMap::new();
        for row in rows {
            let row = row?;
            buckets
                .entry((row.mod_kind.clone(), row.template.clone()))
                .or_default()
                .add(&row);
        }

        let mut out: Vec<ModOutcome> = buckets
            .into_iter()
            .filter(|(_, bucket)| bucket.seen >= min_samples)
            .map(|((mod_kind, template), bucket)| bucket.finish(mod_kind, template))
            .collect();
        // 样本多的排前面,同样多的按模板字典序 —— 同样的库永远给同样的顺序。
        out.sort_by(|left, right| {
            right
                .seen
                .cmp(&left.seen)
                .then_with(|| left.template.cmp(&right.template))
                .then_with(|| left.mod_kind.cmp(&right.mod_kind))
        });
        Ok(out)
    }

    /// 删掉一条观察:四张表全清。
    ///
    /// 和 `delete_watch` 不一样,这里**什么都不留**:观察攒的全部价值就在
    /// 这些行里,留一半没有意义(提醒历史是"我当时看到过什么",观察数据
    /// 是"这条观察的结论",观察没了结论也就没了)。
    pub fn delete_observation(&self, obs_id: &ObservationId) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        for table in [
            "observed_mods",
            "observed_price_history",
            "observed_listings",
            "observation_state",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE obs_id = ?1"),
                params![obs_id.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// 挂单行的列清单。三处查询共用,免得加一列漏改一处。
const LISTING_COLUMNS: &str = "SELECT listing_id, item_name, item_json, seller, indexed_at,
        first_seen_at, last_seen_at, currency, first_price_milli, last_price_milli,
        price_changes, status, gone_at, gone_class, check_rung, next_check_at
 FROM observed_listings";

/// 刚记下的挂单十分钟后第一次回头看(阶梯第 0 档)。
///
/// 第 0 档不看回查间隔,所以这里传什么都一样 —— 阶梯只有一份,在
/// [`pnd_domain::next_check_after`] 里,存储层不自己抄一个 600。
fn first_check_at(now: i64) -> i64 {
    next_check_after(now, 0, 0).unwrap_or(now)
}

/// 聚合时从库里读出来的一行(一件货 × 一条词缀)。
struct AggregateRow {
    mod_kind: String,
    template: String,
    status: ObservedStatus,
    gone_class: Option<GoneClass>,
    price_milli: i64,
    currency: String,
    first_seen_at: i64,
    last_seen_at: i64,
}

/// 一条词缀模板的账本。
#[derive(Default)]
struct Accumulator {
    seen: u32,
    gone: u32,
    sold: u32,
    /// 每种货币出现了几次 —— 中位价只按最常见的那种算。
    currencies: BTreeMap<String, u32>,
    /// (货币, 金额),分别来自卖掉的和还挂着的。
    sold_prices: Vec<(String, i64)>,
    active_prices: Vec<(String, i64)>,
    /// 卖掉的那些看得见的存活时间(秒)。
    sold_lifetimes: Vec<i64>,
}

impl Accumulator {
    fn add(&mut self, row: &AggregateRow) {
        self.seen += 1;
        if !row.currency.is_empty() {
            *self.currencies.entry(row.currency.clone()).or_default() += 1;
        }
        if row.status == ObservedStatus::Active {
            if !row.currency.is_empty() {
                self.active_prices
                    .push((row.currency.clone(), row.price_milli));
            }
            return;
        }
        self.gone += 1;
        if !row.gone_class.is_some_and(GoneClass::looks_sold) {
            return;
        }
        self.sold += 1;
        self.sold_lifetimes.push(pnd_domain::observed_lifetime_secs(
            row.first_seen_at,
            row.last_seen_at,
        ));
        if !row.currency.is_empty() {
            self.sold_prices
                .push((row.currency.clone(), row.price_milli));
        }
    }

    fn finish(self, mod_kind: String, template: String) -> ModOutcome {
        let currency = dominant_currency(&self.currencies);
        ModOutcome {
            template,
            mod_kind,
            seen: self.seen,
            gone: self.gone,
            sold_likely: self.sold,
            median_gone_price_milli: median_i64(&mut in_currency(&self.sold_prices, &currency)),
            median_active_price_milli: median_i64(&mut in_currency(&self.active_prices, &currency)),
            median_hours_alive: median_i64(&mut self.sold_lifetimes.clone())
                .map(|secs| secs as f64 / 3_600.0),
            currency,
        }
    }
}

/// 这一组里最常见的货币;打平了按字典序小的,同样的库永远给同样的答案。
fn dominant_currency(currencies: &BTreeMap<String, u32>) -> String {
    currencies
        .iter()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
        .map(|(name, _)| name.clone())
        .unwrap_or_default()
}

fn in_currency(prices: &[(String, i64)], currency: &str) -> Vec<i64> {
    prices
        .iter()
        .filter(|(name, _)| name == currency)
        .map(|(_, milli)| *milli)
        .collect()
}

/// 中位数用最近秩(排序后取第 `ceil(n/2)` 个),不插值。
///
/// 和 `pnd-ninja` 的分位数一个理由:插出来的 `17.5 divine` 是个市面上不存在
/// 的价,而"卖掉的那些货的中位价"要能拿去和网页上的挂单对着看。
fn median_i64(values: &mut [i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = values.len().div_ceil(2);
    values.get(rank - 1).copied()
}

/// 物品原文 → 一串词缀。纯函数,所以"这件货身上有什么"可以离线复现。
///
/// 模板和取数都借 `pnd-ninja` 里那一对(`mod_template` / `line_numbers`):
/// 交易站和 ninja 的显示文本是同一套写法(带 `[Resistances|…]` 那种标记),
/// 这里再抄一份规则,迟早两边对不上号。
#[must_use]
pub fn observed_mods_from_item_json(item_json: &str) -> Vec<ObservedMod> {
    let Ok(item) = serde_json::from_str::<serde_json::Value>(item_json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, mod_kind) in MOD_ARRAYS {
        let Some(lines) = item.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for (ordinal, entry) in lines.iter().enumerate() {
            let Some(line) = mod_display_line(entry) else {
                continue;
            };
            let numbers = line_numbers(line);
            out.push(ObservedMod {
                mod_kind: mod_kind.to_string(),
                ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                template: mod_template(line),
                value1: numbers.first().copied(),
                value2: numbers.get(1).copied(),
            });
        }
    }
    out
}

/// 词缀数组里的一格 → 那行显示文本。
///
/// **poe2 的交易站给的是对象**,显示文本在 `description` 里,旁边还跟着
/// `hash`(stat id)和 `mods`(词缀等级和这一档的取值范围)。2026-09-07
/// 匿名跑一趟 `--observe-run` 才看见:此前照 PoE1 的写法只认字符串,
/// 于是每一条词缀都被跳过,聚合表一直是空的。
///
/// 字符串那种形状还照收:PoE1 的接口就是那样,哪天 poe2 也改回去,
/// 或者别处塞进来一份手写的原文,都不用再改这里。
fn mod_display_line(entry: &serde_json::Value) -> Option<&str> {
    entry
        .as_str()
        .or_else(|| entry.get("description").and_then(serde_json::Value::as_str))
}

fn observation_run_from_row(row: &Row<'_>) -> rusqlite::Result<ObservationRun> {
    Ok(ObservationRun {
        obs_id: ObservationId(row.get(0)?),
        league: row.get(1)?,
        search_id: row.get(2)?,
        query_json: row.get(3)?,
        last_discover_at: row.get(4)?,
        last_recheck_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn observed_listing_from_row(row: &Row<'_>) -> rusqlite::Result<ObservedListingRow> {
    let currency: String = row.get(7)?;
    let first_milli: i64 = row.get(8)?;
    let last_milli: i64 = row.get(9)?;
    let price_changes: i64 = row.get(10)?;
    let status: String = row.get(11)?;
    let gone_class: Option<String> = row.get(13)?;
    let check_rung: i64 = row.get(14)?;
    Ok(ObservedListingRow {
        listing_id: row.get(0)?,
        item_name: row.get(1)?,
        item_json: row.get(2)?,
        seller: row.get(3)?,
        indexed_at: row.get(4)?,
        first_seen_at: row.get(5)?,
        last_seen_at: row.get(6)?,
        first_price: decode_price(first_milli, &currency),
        last_price: decode_price(last_milli, &currency),
        price_changes: to_u32(price_changes),
        status: ObservedStatus::parse(&status),
        gone_at: row.get(12)?,
        gone_class: gone_class.map(|raw| GoneClass::parse(&raw)),
        check_rung: to_u32(check_rung),
        next_check_at: row.get(15)?,
    })
}

#[cfg(test)]
mod observe_tests {
    use pnd_domain::Currency;

    use super::*;

    fn store() -> WatchStore {
        WatchStore::open_in_memory().expect("open")
    }

    fn obs(id: &str) -> ObservationId {
        ObservationId(id.to_string())
    }

    /// 一条挂单摘要,带着真实形状的物品原文(词缀数组就在里面)。
    fn listing(id: &str, price: Option<Price>, mods: &[&str]) -> ListingSummary {
        let lines: Vec<String> = mods.iter().map(|line| format!("\"{line}\"")).collect();
        let item_json = format!(
            r#"{{"name":"","typeLine":"Precursor Tablet","rarity":"Magic","ilvl":80,
                 "explicitMods":[{}]}}"#,
            lines.join(",")
        );
        ListingSummary {
            id: id.to_string(),
            item_name: "Precursor Tablet".to_string(),
            type_line: "Precursor Tablet".to_string(),
            price,
            account: "Seller#1234".to_string(),
            character: "SomeChar".to_string(),
            online: true,
            afk: false,
            indexed: "2026-09-07T12:00:00Z".to_string(),
            verified: true,
            whisper: "@SomeChar hi".to_string(),
            whisper_token: None,
            hideout_token: None,
            icon: String::new(),
            item_json,
        }
    }

    fn divine(amount_milli: i64) -> Option<Price> {
        Some(Price::new(amount_milli, Currency::Divine))
    }

    /// 建表语句必须能在同一个库上跑第二遍:每次开库都会执行它。
    #[test]
    fn the_observe_schema_applies_twice() {
        let store = store();
        store
            .conn
            .execute_batch(OBSERVE_SCHEMA)
            .expect("second apply");
    }

    #[test]
    fn observation_state_round_trips_and_the_two_stamps_move_apart() {
        let store = store();
        let id = obs("o-1");
        let run = ObservationRun {
            obs_id: id.clone(),
            league: "Forbidden Rites".to_string(),
            search_id: "H4sIAAAA-_09".to_string(),
            query_json: Some(r#"{"type":"Precursor Tablet"}"#.to_string()),
            last_discover_at: None,
            last_recheck_at: None,
            updated_at: 100,
        };
        store.upsert_observation_state(&run).expect("insert");
        assert_eq!(store.observation_state(&id).expect("read"), Some(run));

        store.touch_discover(&id, 200).expect("discover");
        let read = store.observation_state(&id).expect("read").expect("row");
        assert_eq!(read.last_discover_at, Some(200));
        assert_eq!(read.last_recheck_at, None, "discover 不该动 recheck 的时刻");

        store.touch_recheck(&id, 300).expect("recheck");
        let read = store.observation_state(&id).expect("read").expect("row");
        assert_eq!(read.last_discover_at, Some(200));
        assert_eq!(read.last_recheck_at, Some(300));
        assert_eq!(read.updated_at, 300);

        assert_eq!(store.observation_state(&obs("nope")).expect("read"), None);
    }

    /// 第一次见到一条挂单:整件货、词缀、第一个价格点都得进库。
    #[test]
    fn a_new_listing_is_stored_with_its_item_json_and_mods() {
        let store = store();
        let id = obs("o-1");
        let item = listing(
            "aaa",
            divine(20_000),
            &["+115 to maximum Life", "Adds 29 to 38 [Cold|Cold] Damage"],
        );
        assert_eq!(
            store.record_seen(&id, &item, 1_000).expect("record"),
            SeenKind::New
        );

        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!(row.item_name, "Precursor Tablet");
        assert_eq!(row.seller, "Seller#1234");
        assert_eq!(row.indexed_at, "2026-09-07T12:00:00Z");
        assert_eq!(row.first_seen_at, 1_000);
        assert_eq!(row.last_seen_at, 1_000);
        assert_eq!(row.first_price, divine(20_000));
        assert_eq!(row.last_price, divine(20_000));
        assert_eq!(row.price_changes, 0);
        assert_eq!(row.status, ObservedStatus::Active);
        assert_eq!(row.gone_at, None);
        assert_eq!(row.gone_class, None);
        // 物品原文原样留着 —— 以后想统计别的(ilvl、底子)不用重抓。
        let item_back: serde_json::Value = serde_json::from_str(&row.item_json).expect("json");
        assert_eq!(item_back["ilvl"], 80);

        let mods = store.observed_mods(&id, "aaa").expect("mods");
        assert_eq!(
            mods.iter()
                .map(|entry| (entry.template.as_str(), entry.value1, entry.value2))
                .collect::<Vec<_>>(),
            vec![
                ("+# to maximum Life", Some(115.0), None),
                ("Adds # to # Cold Damage", Some(29.0), Some(38.0)),
            ]
        );
        assert!(mods.iter().all(|entry| entry.mod_kind == "explicit"));

        assert_eq!(
            store.price_history(&id, "aaa").expect("history"),
            vec![PricePoint {
                at: 1_000,
                price: divine(20_000)
            }]
        );

        // 再见到同一条:只推 last_seen_at,first_seen_at 和词缀都不动。
        assert_eq!(
            store.record_seen(&id, &item, 2_000).expect("record"),
            SeenKind::Unchanged
        );
        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!((row.first_seen_at, row.last_seen_at), (1_000, 2_000));
        assert_eq!(store.observed_mods(&id, "aaa").expect("mods").len(), 2);
        assert_eq!(store.price_history(&id, "aaa").expect("history").len(), 1);
    }

    /// 改价要往轨迹里追加一行,并把改价次数 +1 —— 判定就是照这条轨迹算的。
    #[test]
    fn a_price_change_appends_to_the_history() {
        let store = store();
        let id = obs("o-1");
        store
            .record_seen(&id, &listing("aaa", divine(20_000), &[]), 1_000)
            .expect("first");

        let cheaper = listing("aaa", divine(15_000), &[]);
        assert_eq!(
            store.record_seen(&id, &cheaper, 2_000).expect("record"),
            SeenKind::PriceChanged {
                from: divine(20_000),
                to: divine(15_000),
            }
        );

        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!(row.first_price, divine(20_000), "第一个价永远不动");
        assert_eq!(row.last_price, divine(15_000));
        assert_eq!(row.price_changes, 1);
        assert_eq!(
            store.price_history(&id, "aaa").expect("history"),
            vec![
                PricePoint {
                    at: 1_000,
                    price: divine(20_000)
                },
                PricePoint {
                    at: 2_000,
                    price: divine(15_000)
                },
            ]
        );

        // 同一个价再见到就只是 Unchanged,不会重复记账。
        assert_eq!(
            store.record_seen(&id, &cheaper, 3_000).expect("record"),
            SeenKind::Unchanged
        );
        assert_eq!(
            store
                .observed_listing(&id, "aaa")
                .expect("read")
                .expect("row")
                .price_changes,
            1
        );
    }

    /// 无价单也要能记:主键在 (obs_id, listing_id) 上,价格那两列走哨兵值。
    #[test]
    fn an_unpriced_listing_is_recorded_and_reads_back_as_no_price() {
        let store = store();
        let id = obs("o-1");
        let item = listing("bbb", None, &[]);
        assert_eq!(
            store.record_seen(&id, &item, 1_000).expect("record"),
            SeenKind::New
        );
        let row = store
            .observed_listing(&id, "bbb")
            .expect("read")
            .expect("row");
        assert_eq!(row.first_price, None);
        assert_eq!(row.last_price, None);
        assert_eq!(
            store.price_history(&id, "bbb").expect("history")[0].price,
            None
        );
        // 后来标价了 = 一次改价。
        assert_eq!(
            store
                .record_seen(&id, &listing("bbb", divine(9_000), &[]), 2_000)
                .expect("record"),
            SeenKind::PriceChanged {
                from: None,
                to: divine(9_000),
            }
        );
    }

    /// 在册的 id、最久没见到的排前面 —— 回查砍尾巴时留下的才是该留的。
    #[test]
    fn active_ids_come_back_oldest_seen_first() {
        let store = store();
        let id = obs("o-1");
        for (listing_id, at) in [("aaa", 3_000i64), ("bbb", 1_000), ("ccc", 2_000)] {
            store
                .record_seen(&id, &listing(listing_id, divine(1_000), &[]), at)
                .expect("record");
        }
        assert_eq!(
            store.active_listing_ids(&id).expect("ids"),
            vec!["bbb", "ccc", "aaa"]
        );

        // 另一条观察见到同样的货,两边各记各的。
        store
            .record_seen(&obs("o-2"), &listing("zzz", divine(1_000), &[]), 500)
            .expect("record");
        assert_eq!(store.active_listing_ids(&obs("o-2")).expect("ids"), ["zzz"]);
    }

    /// 搜索又报回来的 id 只推 last_seen_at,不花 fetch、不动价格。
    #[test]
    fn touching_a_listing_only_moves_the_last_seen_stamp() {
        let store = store();
        let id = obs("o-1");
        store
            .record_seen(&id, &listing("aaa", divine(20_000), &[]), 1_000)
            .expect("record");

        let touched = store
            .touch_listings(&id, &["aaa".to_string(), "unknown".to_string()], 2_500)
            .expect("touch");
        assert_eq!(touched, 1, "不认识的 id 一行都不该动");

        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!(row.last_seen_at, 2_500);
        assert_eq!(row.last_price, divine(20_000));
        assert_eq!(row.price_changes, 0);
        assert_eq!(store.price_history(&id, "aaa").expect("history").len(), 1);
    }

    /// 服务端顺手告诉了我们它是什么时候走的:那一刻既是 `gone_at`,
    /// 也是我们该认的 `last_seen_at`。
    ///
    /// 交易站说一条挂单没了的方式是 `item.verified` 变成 `false`,而同一条
    /// 响应里 `listing.indexed` 已经被顶到"这一趟索引发现它不见了"的时刻。
    /// 存活时间因此是 `gone_at − first_seen_at`;`last_seen_at` 要是还停在
    /// 上一次回查那一刻,同一条挂单的存活时间会短掉整整一个回查间隔。
    #[test]
    fn marking_gone_at_a_known_moment_pulls_last_seen_back_to_it() {
        let store = store();
        let id = obs("o-1");
        let item = listing("aaa", divine(20_000), &[]);
        store.record_seen(&id, &item, 1_000).expect("record");
        // 中间回查过一次,它还在。
        store
            .touch_listings(&id, &["aaa".to_string()], 5_000)
            .expect("touch");

        // 下一次回查:verified = false,而服务端说它 6_200 那一刻就不见了。
        store
            .mark_gone_at(&id, "aaa", 6_200, GoneClass::SoldLikely)
            .expect("gone");
        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!(row.status, ObservedStatus::Gone);
        assert_eq!(row.gone_at, Some(6_200));
        assert_eq!(row.last_seen_at, 6_200, "最后一次见到 = 它走的那一刻");
        assert_eq!(row.observed_lifetime_secs(), 5_200);

        // 已经标成没了的行不该被第二次改写(和 `mark_gone` 同一条纪律)。
        store
            .mark_gone_at(&id, "aaa", 9_000, GoneClass::Unknown)
            .expect("gone");
        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!(row.gone_at, Some(6_200));
        assert_eq!(row.gone_class, Some(GoneClass::SoldLikely));
    }

    /// 标成没了之后,再看见它就得复活 —— "它还在"是眼见为实的事。
    #[test]
    fn seeing_a_gone_listing_again_brings_it_back() {
        let store = store();
        let id = obs("o-1");
        let item = listing("aaa", divine(20_000), &[]);
        store.record_seen(&id, &item, 1_000).expect("record");
        store
            .mark_gone(&id, "aaa", 5_000, GoneClass::SoldLikely)
            .expect("gone");
        assert!(store.active_listing_ids(&id).expect("ids").is_empty());

        store.record_seen(&id, &item, 6_000).expect("record");
        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!(row.status, ObservedStatus::Active);
        assert_eq!(row.gone_at, None);
        assert_eq!(row.gone_class, None);

        // touch 也一样能复活。
        store
            .mark_gone(&id, "aaa", 7_000, GoneClass::SoldLikely)
            .expect("gone");
        store
            .touch_listings(&id, &["aaa".to_string()], 8_000)
            .expect("touch");
        assert_eq!(store.active_listing_ids(&id).expect("ids"), ["aaa"]);
    }

    /// 已经标成没了的行不该被第二次回查改一次 `gone_at`:
    /// 那会让"它是什么时候没的"越查越晚。
    #[test]
    fn marking_a_gone_listing_gone_again_does_nothing() {
        let store = store();
        let id = obs("o-1");
        store
            .record_seen(&id, &listing("aaa", divine(20_000), &[]), 1_000)
            .expect("record");
        store
            .mark_gone(&id, "aaa", 5_000, GoneClass::SoldLikely)
            .expect("gone");
        store
            .mark_gone(&id, "aaa", 9_000, GoneClass::Unknown)
            .expect("gone again");

        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!(row.gone_at, Some(5_000));
        assert_eq!(row.gone_class, Some(GoneClass::SoldLikely));
    }

    #[test]
    fn the_summary_counts_active_and_each_gone_class() {
        let store = store();
        let id = obs("o-1");
        for listing_id in ["a", "b", "c", "d", "e"] {
            store
                .record_seen(&id, &listing(listing_id, divine(1_000), &[]), 1_000)
                .expect("record");
        }
        store
            .mark_gone(&id, "a", 2_000, GoneClass::SoldLikely)
            .expect("gone");
        store
            .mark_gone(&id, "b", 2_000, GoneClass::SoldLikely)
            .expect("gone");
        store
            .mark_gone(&id, "c", 2_000, GoneClass::SoldAfterCuts)
            .expect("gone");
        store
            .mark_gone(&id, "d", 2_000, GoneClass::Unknown)
            .expect("gone");

        assert_eq!(
            store.observation_summary(&id).expect("summary"),
            ObservationSummary {
                active: 1,
                gone: 4,
                sold_likely: 2,
                sold_after_cuts: 1,
                unknown: 1,
                gone_before_first_look: 0,
            }
        );
        assert_eq!(
            store.observation_summary(&obs("empty")).expect("summary"),
            ObservationSummary::default()
        );
    }

    #[test]
    fn the_listing_flow_is_newest_gone_and_oldest_active() {
        let store = store();
        let id = obs("o-1");
        for (listing_id, first_seen) in [("a", 1_000i64), ("b", 2_000), ("c", 3_000), ("d", 4_000)]
        {
            store
                .record_seen(&id, &listing(listing_id, divine(1_000), &[]), first_seen)
                .expect("record");
        }
        store
            .mark_gone(&id, "a", 9_000, GoneClass::SoldLikely)
            .expect("gone");
        store
            .mark_gone(&id, "b", 8_000, GoneClass::SoldAfterCuts)
            .expect("gone");

        let gone: Vec<String> = store
            .recent_gone(&id, 10)
            .expect("gone")
            .into_iter()
            .map(|row| row.listing_id)
            .collect();
        assert_eq!(gone, ["a", "b"], "最近没的排最前");
        assert_eq!(store.recent_gone(&id, 1).expect("gone").len(), 1);

        let active: Vec<String> = store
            .oldest_active(&id, 10)
            .expect("active")
            .into_iter()
            .map(|row| row.listing_id)
            .collect();
        assert_eq!(active, ["c", "d"], "挂得最久的排最前");
    }

    /// 聚合表的算术:见过几条、没了几条、疑似卖掉几条、三个中位数。
    ///
    /// 摆的这一组是故意算得出整数的:`+# to maximum Life` 五条货 ——
    /// 三条卖掉(20/10/30 divine,活了 1/2/3 小时)、一条 Unknown、
    /// 一条还挂着 40。中位价取最近秩(第 ceil(3/2)=2 个),所以是 20;
    /// 中位存活是 2 小时。
    #[test]
    fn the_mod_aggregate_counts_and_takes_medians() {
        let store = store();
        let id = obs("o-1");
        let hour = 3_600i64;
        let rows = [
            ("sold-a", 20_000i64, hour, Some(GoneClass::SoldLikely)),
            ("sold-b", 10_000, 2 * hour, Some(GoneClass::SoldAfterCuts)),
            ("sold-c", 30_000, 3 * hour, Some(GoneClass::SoldLikely)),
            ("dunno", 99_000, 4 * hour, Some(GoneClass::Unknown)),
            ("alive", 40_000, 5 * hour, None),
        ];
        for (listing_id, price, alive, class) in rows {
            let item = listing(listing_id, divine(price), &["+115 to maximum Life"]);
            store.record_seen(&id, &item, 1_000).expect("record");
            // 存活时间是"看见的那一段":推一次 last_seen 就是它。
            store
                .touch_listings(&id, &[listing_id.to_string()], 1_000 + alive)
                .expect("touch");
            if let Some(class) = class {
                store
                    .mark_gone(&id, listing_id, 1_000 + alive + 60, class)
                    .expect("gone");
            }
        }

        let aggregate = store.mod_aggregate(&id, 1).expect("aggregate");
        assert_eq!(aggregate.len(), 1, "只有一条模板:{aggregate:#?}");
        let life = &aggregate[0];
        assert_eq!(life.template, "+# to maximum Life");
        assert_eq!(life.mod_kind, "explicit");
        assert_eq!(life.seen, 5);
        assert_eq!(life.gone, 4, "Unknown 也算不见了");
        assert_eq!(life.sold_likely, 3, "SoldLikely + SoldAfterCuts");
        assert_eq!(life.currency, "divine");
        assert_eq!(life.median_gone_price_milli, Some(20_000));
        assert_eq!(life.median_active_price_milli, Some(40_000));
        assert_eq!(life.median_hours_alive, Some(2.0));

        // 样本不够的整行不摆出来。
        assert!(store.mod_aggregate(&id, 6).expect("aggregate").is_empty());
    }

    /// 同一件货身上两行一模一样的词缀,只能算一件 —— 不然成交率的分母会虚高。
    #[test]
    fn the_same_template_twice_on_one_item_counts_once() {
        let store = store();
        let id = obs("o-1");
        let item = listing(
            "aaa",
            divine(1_000),
            &[
                "+20% to [Resistances|Fire Resistance]",
                "+35% to [Resistances|Fire Resistance]",
            ],
        );
        store.record_seen(&id, &item, 1_000).expect("record");
        // 两条词缀行都进了库(ordinal 不同),但聚合时是一件货。
        assert_eq!(store.observed_mods(&id, "aaa").expect("mods").len(), 2);

        let aggregate = store.mod_aggregate(&id, 1).expect("aggregate");
        assert_eq!(aggregate.len(), 1);
        assert_eq!(aggregate[0].template, "+#% to Fire Resistance");
        assert_eq!(aggregate[0].seen, 1);
    }

    /// 混着货币的时候,中位价只按最常见那种算 —— 跨货币取中位是个没有意义的数。
    #[test]
    fn medians_stick_to_the_most_common_currency() {
        let store = store();
        let id = obs("o-1");
        let mods = ["+115 to maximum Life"];
        for (listing_id, price) in [
            ("a", Price::new(20_000, Currency::Divine)),
            ("b", Price::new(30_000, Currency::Divine)),
            ("c", Price::new(900_000, Currency::Chaos)),
        ] {
            let item = listing(listing_id, Some(price), &mods);
            store.record_seen(&id, &item, 1_000).expect("record");
        }
        let aggregate = store.mod_aggregate(&id, 1).expect("aggregate");
        assert_eq!(aggregate[0].currency, "divine");
        assert_eq!(aggregate[0].seen, 3, "三条都算见过");
        assert_eq!(
            aggregate[0].median_active_price_milli,
            Some(20_000),
            "只有两条 divine 的参与中位(900000 chaos 那条不掺和),\
             两个值的最近秩中位取靠下那个"
        );
    }

    /// 删观察就是删干净:四张表一行不留,而且不碰别的观察。
    #[test]
    fn deleting_an_observation_clears_all_four_tables() {
        let store = store();
        let id = obs("o-1");
        let other = obs("o-2");
        for target in [&id, &other] {
            store
                .upsert_observation_state(&ObservationRun {
                    obs_id: target.clone(),
                    league: "Forbidden Rites".to_string(),
                    search_id: "H4sIAAAA-_09".to_string(),
                    query_json: None,
                    last_discover_at: None,
                    last_recheck_at: None,
                    updated_at: 100,
                })
                .expect("state");
            let item = listing("aaa", divine(20_000), &["+115 to maximum Life"]);
            store.record_seen(target, &item, 1_000).expect("record");
            store
                .record_seen(target, &listing("aaa", divine(10_000), &[]), 2_000)
                .expect("record");
        }

        store.delete_observation(&id).expect("delete");
        assert_eq!(store.observation_state(&id).expect("read"), None);
        assert!(store.active_listing_ids(&id).expect("ids").is_empty());
        assert!(store.price_history(&id, "aaa").expect("history").is_empty());
        assert!(store.observed_mods(&id, "aaa").expect("mods").is_empty());

        // 另一条观察一根毫毛都没动。
        assert!(store.observation_state(&other).expect("read").is_some());
        assert_eq!(store.active_listing_ids(&other).expect("ids"), ["aaa"]);
        assert_eq!(store.price_history(&other, "aaa").expect("h").len(), 2);
        assert_eq!(store.observed_mods(&other, "aaa").expect("m").len(), 1);
    }

    /// 词缀原文 → 模板 + 数值。方括号标记要摊平,认不出的键整片忽略。
    #[test]
    fn mods_are_read_out_of_the_raw_item_json() {
        let mods = observed_mods_from_item_json(
            r#"{"implicitMods":["Grants 1 additional Skill Slot"],
                 "explicitMods":["+115 to maximum Life","Adds 29 to 38 [Cold|Cold] Damage"],
                 "somethingNewMods":["ignored"],
                 "ilvl":80}"#,
        );
        assert_eq!(
            mods.iter()
                .map(|entry| (
                    entry.mod_kind.as_str(),
                    entry.ordinal,
                    entry.template.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("implicit", 0, "Grants # additional Skill Slot"),
                ("explicit", 0, "+# to maximum Life"),
                ("explicit", 1, "Adds # to # Cold Damage"),
            ]
        );
        assert_eq!(mods[2].value1, Some(29.0));
        assert_eq!(mods[2].value2, Some(38.0));

        // 读不动的原文不该 panic,只是没有词缀。
        assert!(observed_mods_from_item_json("").is_empty());
        assert!(observed_mods_from_item_json("not json").is_empty());
        assert!(observed_mods_from_item_json("{}").is_empty());
    }

    // ---- 回查阶梯 ------------------------------------------------------

    /// 刚记下的挂单站在阶梯第 0 档:十分钟后第一次回头看它。
    #[test]
    fn a_new_listing_starts_on_the_first_rung() {
        let store = store();
        let id = obs("o-1");
        store
            .record_seen(&id, &listing("aaa", divine(20_000), &[]), 1_000)
            .expect("record");
        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!(row.check_rung, 0);
        assert_eq!(row.next_check_at, 1_000 + 600);
    }

    /// 到点该查的挂单是**跨观察一起**捞出来的:一次 fetch 带 10 个 id,
    /// 凑不满就白花一次额度,所以两条观察的到期挂单要能拼进同一批。
    #[test]
    fn due_listings_come_from_every_observation_oldest_deadline_first() {
        let store = store();
        let one = obs("o-1");
        let two = obs("o-2");
        store
            .record_seen(&one, &listing("a", divine(1_000), &[]), 1_000)
            .expect("record");
        store
            .record_seen(&two, &listing("b", divine(1_000), &[]), 900)
            .expect("record");
        // 还没到点的那条不该被捞出来。
        store
            .record_seen(&one, &listing("c", divine(1_000), &[]), 5_000)
            .expect("record");

        let due = store
            .due_listings(1_600, 10, &BTreeSet::new())
            .expect("due");
        assert_eq!(
            due.iter()
                .map(|entry| (entry.obs_id.to_string(), entry.listing_id.as_str()))
                .collect::<Vec<_>>(),
            vec![("o-2".to_string(), "b"), ("o-1".to_string(), "a")],
            "最早该查的排前面"
        );
        assert_eq!(due[0].first_seen_at, 900);
        assert_eq!(due[0].check_rung, 0);

        // 上限砍掉尾巴,留下的是最该查的那条。
        assert_eq!(
            store
                .due_listings(1_600, 1, &BTreeSet::new())
                .expect("due")
                .len(),
            1
        );
        // 已经没了的挂单不再花额度。
        store
            .mark_gone(&two, "b", 1_500, GoneClass::SoldLikely)
            .expect("gone");
        assert_eq!(
            store
                .due_listings(1_600, 10, &BTreeSet::new())
                .expect("due")
                .iter()
                .map(|entry| entry.listing_id.clone())
                .collect::<Vec<_>>(),
            vec!["a".to_string()]
        );
    }

    /// **在途的那几条不能把名额占死。**
    ///
    /// 一批回查发出去之后,那几条挂单的 `next_check_at` 要等回信才往前挪。
    /// 万一那封回信丢了(关机、actor 先走一步),它们会永远排在"最早该查"
    /// 的最前面 —— 先 `LIMIT` 后筛的话,每一轮扫描捞上来的正好是这几条,
    /// 筛完一条不剩,后面的挂单一辈子轮不上,整条回查线就此停摆。
    ///
    /// 所以跳过要在取数**里面**做:多取几行,再把在途的挑出去。
    #[test]
    fn in_flight_listings_do_not_starve_the_ones_behind_them() {
        let store = store();
        let id = obs("o-1");
        for (listing_id, at) in [("a", 1_000), ("b", 1_100), ("c", 1_200)] {
            store
                .record_seen(&id, &listing(listing_id, divine(1_000), &[]), at)
                .expect("record");
        }
        // 三条都到点了(第一档是 +600 秒)。
        let none = BTreeSet::new();
        assert_eq!(store.due_listings(2_000, 3, &none).expect("due").len(), 3);

        // "a" 那一批还在路上,而这一轮只问得起两条:该问的是 b 和 c,
        // 不是"捞出 a、b 再把 a 筛掉"剩下的那一条。
        let in_flight: BTreeSet<(ObservationId, String)> =
            [(id.clone(), "a".to_string())].into_iter().collect();
        assert_eq!(
            store
                .due_listings(2_000, 2, &in_flight)
                .expect("due")
                .iter()
                .map(|entry| entry.listing_id.clone())
                .collect::<Vec<_>>(),
            vec!["b".to_string(), "c".to_string()]
        );
    }

    /// 查过一次就升一档,并把下一次排上;阶梯走完(`None`)就再也不查了。
    #[test]
    fn advancing_a_rung_moves_the_deadline_and_none_stops_it_for_good() {
        let store = store();
        let id = obs("o-1");
        store
            .record_seen(&id, &listing("aaa", divine(1_000), &[]), 1_000)
            .expect("record");

        store
            .advance_rung(&id, "aaa", 1, Some(1_000 + 1_800))
            .expect("advance");
        let row = store
            .observed_listing(&id, "aaa")
            .expect("read")
            .expect("row");
        assert_eq!((row.check_rung, row.next_check_at), (1, 2_800));
        assert!(
            store
                .due_listings(2_000, 10, &BTreeSet::new())
                .expect("due")
                .is_empty()
        );
        assert_eq!(
            store
                .due_listings(2_800, 10, &BTreeSet::new())
                .expect("due")
                .len(),
            1
        );

        // 七天到了:`None` = 不再回查,哪怕过一年再问也不该冒出来。
        store.advance_rung(&id, "aaa", 9, None).expect("advance");
        assert_eq!(
            store
                .observed_listing(&id, "aaa")
                .expect("read")
                .expect("row")
                .next_check_at,
            NEVER_CHECK_AGAIN
        );
        assert!(
            store
                .due_listings(i64::MAX - 1, 10, &BTreeSet::new())
                .expect("due")
                .is_empty()
        );
    }

    /// **2026-09-07 匿名实测的形状**(`trade_probe --observe-run`,
    /// Choir of the Storm 八件货,原文照抄、只删了卖家和挂单 id)。
    ///
    /// 词缀数组里装的**不是字符串,是对象**:显示文本在 `description` 那一格,
    /// 旁边还跟着 `hash`(stat id)和 `mods`(词缀等级、这一档的取值范围)。
    /// 计划里记的"字符串数组"是照 PoE1 的接口写的,poe2 不长那样 —— 于是
    /// 那一趟真跑八件货、每件四五条词缀全被丢掉,聚合表是空的。
    ///
    /// `magnitudes` 里的 min/max 是**这一档能卷到的范围**,不是这件货卷了多少;
    /// 真正卷出来的数在 `description` 里(`+64%`),所以数值照旧从显示文本读。
    #[test]
    fn mods_come_back_as_objects_with_a_description() {
        let mods = observed_mods_from_item_json(
            r#"{"name":"Choir of the Storm","typeLine":"Lapis Amulet","rarity":"Unique","ilvl":81,
                 "implicitMods":[{"description":"+15 to [Dexterity|Dexterity]",
                    "domain":"implicit","hash":"stat.implicit.stat_3261801346",
                    "mods":[{"level":10,"magnitudes":[{"max":"15","min":"10"}]}]}],
                 "explicitMods":[
                   {"description":"+64% to [Resistances|Lightning Resistance]",
                    "domain":"explicit","hash":"stat.explicit.stat_1671376347",
                    "mods":[{"level":69,"magnitudes":[{"max":"100","min":"50"}]}]},
                   {"description":"[Trigger] Lightning Bolt Skill on [Critical|Critical Hit]",
                    "domain":"explicit","hash":"stat.explicit.stat_704919631",
                    "mods":[{"level":69,"magnitudes":[{"max":"1","min":"1"}]}]}],
                 "enchantMods":[{"description":"Allocates [criticals39|Preemptive Strike]",
                    "domain":"enchant","hash":"stat.enchant.stat_2954116742|21380",
                    "mods":[{"magnitudes":[{"max":1,"min":1}]}]}]}"#,
        );
        assert_eq!(
            mods.iter()
                .map(|entry| (
                    entry.mod_kind.as_str(),
                    entry.ordinal,
                    entry.template.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("implicit", 0, "+# to Dexterity"),
                ("explicit", 0, "+#% to Lightning Resistance"),
                (
                    "explicit",
                    1,
                    "Trigger Lightning Bolt Skill on Critical Hit"
                ),
                ("enchant", 0, "Allocates Preemptive Strike"),
            ]
        );
        assert_eq!(mods[0].value1, Some(15.0), "卷出来的数在 description 里");
        assert_eq!(mods[1].value1, Some(64.0));
        // 没有 `description` 的对象跳过,不该顶一条空模板进去 ——
        // 聚合那边 `template <> ''` 会把它滤掉,留着只是白占一行。
        assert!(
            observed_mods_from_item_json(r#"{"explicitMods":[{"hash":"stat.explicit.x"}]}"#)
                .is_empty()
        );
    }

    /// 界面上的"立刻回查一次":把这条观察在册的挂单全部推到此刻到期,
    /// 别的观察一条都不动。
    #[test]
    fn marking_a_whole_observation_due_only_touches_that_observation() {
        let store = store();
        let one = obs("o-1");
        let two = obs("o-2");
        for (target, listing_id) in [(&one, "a"), (&one, "b"), (&two, "c")] {
            store
                .record_seen(target, &listing(listing_id, divine(1_000), &[]), 1_000)
                .expect("record");
        }
        store.mark_gone(&one, "b", 1_100, GoneClass::Unknown).ok();

        assert_eq!(store.mark_all_due(&one, 1_200).expect("due"), 1);
        let due = store
            .due_listings(1_200, 10, &BTreeSet::new())
            .expect("due");
        assert_eq!(
            due.iter()
                .map(|entry| entry.listing_id.clone())
                .collect::<Vec<_>>(),
            vec!["a".to_string()],
            "另一条观察和已经没了的那条都不该被推到期"
        );
    }

    /// 状态栏上那句"下一次回查":这条观察在册的挂单里最早的那个到期时刻。
    #[test]
    fn the_next_check_of_an_observation_is_the_earliest_deadline_on_file() {
        let store = store();
        let id = obs("o-1");
        assert_eq!(store.next_check_due_at(&id).expect("next"), None);
        store
            .record_seen(&id, &listing("a", divine(1_000), &[]), 1_000)
            .expect("record");
        store
            .record_seen(&id, &listing("b", divine(1_000), &[]), 2_000)
            .expect("record");
        assert_eq!(store.next_check_due_at(&id).expect("next"), Some(1_600));
        // 不再回查的那条不算数。
        store.advance_rung(&id, "a", 9, None).expect("advance");
        assert_eq!(store.next_check_due_at(&id).expect("next"), Some(2_600));
        store.advance_rung(&id, "b", 9, None).expect("advance");
        assert_eq!(store.next_check_due_at(&id).expect("next"), None);
    }

    // ---- 第一眼就没了 --------------------------------------------------

    /// 秒推说有这么一条挂单,几秒钟后去抓详情却已经是 null:记一行,
    /// 判定档写死 `gone_before_first_look`,而且它没有词缀 ——
    /// 所以聚合表一根毫毛都不会动。
    #[test]
    fn a_listing_gone_before_the_first_look_is_counted_but_never_aggregated() {
        let store = store();
        let id = obs("o-1");
        store
            .record_seen(
                &id,
                &listing("alive", divine(20_000), &["+115 to maximum Life"]),
                1_000,
            )
            .expect("record");
        let before = store.mod_aggregate(&id, 1).expect("aggregate");

        assert!(
            store
                .record_gone_before_first_look(&id, "quick", 2_000)
                .expect("record")
        );
        let row = store
            .observed_listing(&id, "quick")
            .expect("read")
            .expect("row");
        assert_eq!(row.status, ObservedStatus::Gone);
        assert_eq!(row.gone_class, Some(GoneClass::GoneBeforeFirstLook));
        assert_eq!(row.first_seen_at, 2_000);
        assert_eq!(row.last_seen_at, 2_000);
        assert_eq!(row.gone_at, Some(2_000));
        assert_eq!(row.first_price, None, "我们从没见过它的价");
        assert!(row.item_json.is_empty(), "连它长什么样都没见过");
        assert!(store.observed_mods(&id, "quick").expect("mods").is_empty());
        assert_eq!(
            row.next_check_at, NEVER_CHECK_AGAIN,
            "已经没了的东西不该再排回查"
        );

        // 账上单独一栏,而"没了"的总数里也算它一条。
        let summary = store.observation_summary(&id).expect("summary");
        assert_eq!(summary.active, 1);
        assert_eq!(summary.gone, 1);
        assert_eq!(summary.gone_before_first_look, 1);
        assert_eq!(summary.unknown, 0, "别混进'看不出来'那一档");

        // 聚合表按词缀分组,而这一行没有词缀:一个数都不该变。
        assert_eq!(store.mod_aggregate(&id, 1).expect("aggregate"), before);

        // 已经记过的 id 不该被第二次覆盖(比如两条推送里都有它)。
        assert!(
            !store
                .record_gone_before_first_look(&id, "alive", 3_000)
                .expect("record"),
            "还活着的那条不能被写成'第一眼就没了'"
        );
        assert_eq!(
            store
                .observed_listing(&id, "alive")
                .expect("read")
                .expect("row")
                .status,
            ObservedStatus::Active
        );
    }

    #[test]
    fn the_median_takes_the_nearest_rank() {
        assert_eq!(median_i64(&mut []), None);
        assert_eq!(median_i64(&mut [7]), Some(7));
        // 偶数个不插值:取第 ceil(n/2) 个,也就是靠下的那个真实值。
        assert_eq!(median_i64(&mut [10, 20]), Some(10));
        assert_eq!(median_i64(&mut [30, 10, 20]), Some(20));
        assert_eq!(median_i64(&mut [40, 10, 30, 20]), Some(20));
    }
}
