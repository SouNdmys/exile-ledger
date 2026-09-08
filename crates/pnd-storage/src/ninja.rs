//! `ninja.sqlite`:poe.ninja 采样的工作台。
//!
//! 这个库和 `watch.sqlite` 的性质完全不同:**它随手删掉也不心疼**。里面全是能
//! 重新抓回来的东西,存下来只为两件事——
//!
//! 1. **断点续跑。** 一轮完整采样是 ~65 次分区搜索 + 2,000 次角色详情。builds 接口
//!    一个 IP 一小时只给 ~120 个请求,所以整轮要跑一两天。程序关掉、断网、换快照,
//!    下次开起来必须接着上次跑,
//!    而不是从头再来一遍。所以每个分区、每个角色都是一行带 `status` 的工作单。
//! 2. **一天只打扰 poe.ninja 一次。** 快照 `version` 一天变好几次,但我们 24 小时
//!    内不重跑;界面上翻来翻去看的都是库里这份缓存,零网络请求。
//!
//! 纪律和 `watch.rs` 一样:**库里没有时钟**,所有时间都是调用方传进来的 unix 秒;
//! 金额存千分整数(暗金参考价 `primary_value_milli` × 1000),而**单位跟着行走**
//! —— 经济接口自报的 `core.primary` 换过一次(2026-09-06 是 exalted,
//! 2026-09-07 是 divine),所以它存在 `primary_currency` 那一列里,不是常数。
//!
//! 角色表的主键是 `(league_url, account, name)` 而**不带 version**:同一个人在
//! 新快照里还是同一个人,重跑时只补新面孔,已经抓过的不再花那一秒。

use std::path::Path;
use std::time::Duration;

use pnd_domain::Game;
use pnd_ninja::aggregate::SlotModStat;
use pnd_ninja::economy::UniquePriceLine;
use pnd_ninja::plan::{Partition, PartitionTier, SampledCharacter, is_rarity_bucket};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};

use crate::watch::StorageError;

const NINJA_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ninja_snapshots (
    league_url TEXT NOT NULL,
    version TEXT NOT NULL,
    snapshot_name TEXT NOT NULL,
    total_characters INTEGER NOT NULL,
    stage TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    PRIMARY KEY (league_url, version)
) STRICT;

CREATE TABLE IF NOT EXISTS ninja_partitions (
    league_url TEXT NOT NULL,
    version TEXT NOT NULL,
    partition_key TEXT NOT NULL,
    tier TEXT NOT NULL,
    status TEXT NOT NULL,
    total INTEGER,
    fetched_at INTEGER,
    PRIMARY KEY (league_url, version, partition_key)
) STRICT;

CREATE TABLE IF NOT EXISTS ninja_facets (
    league_url TEXT NOT NULL,
    version TEXT NOT NULL,
    partition_key TEXT NOT NULL,
    facet TEXT NOT NULL,
    entry TEXT NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (league_url, version, partition_key, facet, entry)
) STRICT;

CREATE TABLE IF NOT EXISTS ninja_characters (
    league_url TEXT NOT NULL,
    account TEXT NOT NULL,
    name TEXT NOT NULL,
    class TEXT NOT NULL,
    level INTEGER NOT NULL,
    from_partition TEXT NOT NULL,
    tier TEXT NOT NULL,
    status TEXT NOT NULL,
    version TEXT,
    fetched_at INTEGER,
    detail_json TEXT,
    PRIMARY KEY (league_url, account, name)
) STRICT;

CREATE INDEX IF NOT EXISTS ninja_characters_queue ON ninja_characters(league_url, status);

CREATE TABLE IF NOT EXISTS ninja_item_mods (
    league_url TEXT NOT NULL,
    version TEXT NOT NULL,
    class TEXT NOT NULL DEFAULT '',
    slot TEXT NOT NULL,
    rarity TEXT NOT NULL,
    mod_kind TEXT NOT NULL,
    stat_id TEXT NOT NULL,
    display TEXT NOT NULL DEFAULT '',
    value_index INTEGER NOT NULL DEFAULT 0,
    mod_family TEXT NOT NULL,
    characters INTEGER NOT NULL,
    occurrences INTEGER NOT NULL,
    sample_size INTEGER NOT NULL,
    p25 REAL,
    p50 REAL,
    p75 REAL,
    PRIMARY KEY (league_url, version, class, slot, rarity, mod_kind, stat_id)
) STRICT;

CREATE TABLE IF NOT EXISTS ninja_unique_prices (
    league_url TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    type_name TEXT NOT NULL,
    name TEXT NOT NULL,
    base_type TEXT NOT NULL,
    category TEXT NOT NULL,
    primary_value_milli INTEGER NOT NULL,
    primary_currency TEXT NOT NULL DEFAULT 'divine',
    listing_count INTEGER NOT NULL,
    total_change REAL,
    PRIMARY KEY (league_url, type_name, name, base_type)
) STRICT;
"#;

/// 2026-09-07 加的一列:参考价的计价基准币。
///
/// 建表语句是 `CREATE TABLE IF NOT EXISTS`,所以它对**已经存在**的库一个字
/// 都改不动 —— 老库里那张表还是没有这一列,读它会直接报错。这里补一次
/// `ALTER TABLE`,用 `PRAGMA table_info` 判断加没加过(SQLite 没有
/// "ADD COLUMN IF NOT EXISTS")。
///
/// 默认值填 `divine` 而不是 `exalted`:老行是从同一个端点抓来的,而那个端点
/// 今天报的就是 divine;这张表本来也是一天重抓一次的缓存,下一轮价格一到
/// 这些老行就全被换掉了。
fn add_primary_currency_column(conn: &Connection) -> Result<(), StorageError> {
    let mut columns = conn.prepare("PRAGMA table_info(ninja_unique_prices)")?;
    let names = columns.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == "primary_currency" {
            return Ok(());
        }
    }
    drop(columns);
    conn.execute_batch(
        "ALTER TABLE ninja_unique_prices
             ADD COLUMN primary_currency TEXT NOT NULL DEFAULT 'divine'",
    )?;
    Ok(())
}

/// 词缀统计表少了哪一列都整张删掉重建。
///
/// 两次加列都走这条路,而不是 `ALTER TABLE ADD COLUMN`:
///
/// - 2026-09-07 加的 `class` 进了主键,而 SQLite 改不动一张已有表的主键。
///   新列不进主键的话,十几个职业的行会在插库时全撞进同一个键 ——
///   表面上有数据,实际只剩最后一个职业那份。
/// - 同日加的 `display` / `value_index`(游戏里那句话 + 这一行数的是第几个数)
///   本可以 `ADD COLUMN`,但补出来的老行会是一片空白,词缀页就得挂着一整屏
///   没有文本的行等到下一次聚合。整张删掉,那一屏根本不会出现。
///
/// 删得起:这张表是从 `ninja_characters` 里的详情原文算出来的派生数据,采样线程
/// 进角色那一步、每 50 个角色、收尾时各重建一次,一个网络请求都不用发。
///
/// 必须**在建表语句之前**跑:删完还得有人把它建回来。
fn drop_stale_item_mods(conn: &Connection) -> Result<(), StorageError> {
    /// 少一个就重建。以后再加列往这儿添一个名字即可。
    const REQUIRED: [&str; 3] = ["class", "display", "value_index"];

    let mut columns = conn.prepare("PRAGMA table_info(ninja_item_mods)")?;
    let names = columns.query_map([], |row| row.get::<_, String>(1))?;
    // 表还不存在时 `table_info` 一行都不给 —— 那就没有什么可删的。
    let mut present: Vec<String> = Vec::new();
    for name in names {
        present.push(name?);
    }
    drop(columns);
    let complete = REQUIRED
        .iter()
        .all(|wanted| present.iter().any(|name| name == wanted));
    if !present.is_empty() && !complete {
        conn.execute_batch("DROP TABLE ninja_item_mods")?;
    }
    Ok(())
}

/// 一轮采样跑到哪一步了。四步是顺序推进的,中途杀掉重开就从这里接着走。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapshotStage {
    /// 分区清单已经排好,一个都还没跑。
    #[default]
    Planned,
    /// 分区搜索在跑或跑完了:热门暗金榜这时候就已经能看了。
    Facets,
    /// 在逐个抓角色详情。
    Characters,
    /// 词缀统计已经重建完:这一轮到此为止。
    Aggregated,
}

impl SnapshotStage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotStage::Planned => "planned",
            SnapshotStage::Facets => "facets",
            SnapshotStage::Characters => "characters",
            SnapshotStage::Aggregated => "aggregated",
        }
    }

    /// 认不出来的一律当 `Planned`:最坏的结果是这一轮从头再跑一遍,
    /// 而不是读不出这一行。
    #[must_use]
    pub fn parse(raw: &str) -> SnapshotStage {
        match raw {
            "facets" => SnapshotStage::Facets,
            "characters" => SnapshotStage::Characters,
            "aggregated" => SnapshotStage::Aggregated,
            _ => SnapshotStage::Planned,
        }
    }
}

/// 分区和角色共用的工作单状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkStatus {
    #[default]
    Pending,
    Done,
    /// 抓失败了。角色那边多半是 404(人删号了),不该一直卡在队首反复重试。
    Failed,
}

impl WorkStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WorkStatus::Pending => "pending",
            WorkStatus::Done => "done",
            WorkStatus::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> WorkStatus {
        match raw {
            "done" => WorkStatus::Done,
            "failed" => WorkStatus::Failed,
            _ => WorkStatus::Pending,
        }
    }
}

/// 一轮采样的抬头。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    pub league_url: String,
    pub version: String,
    pub snapshot_name: String,
    /// build-index-state 报的联赛总人数,用来算"我们采了百分之几"。
    pub total_characters: u64,
    pub stage: SnapshotStage,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// 一条分区工作单。查询参数没有单独存:分区键就是查询串,
/// 用 `pnd_ninja::plan::query_from_key` 拆回来即可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionRow {
    pub partition_key: String,
    pub tier: PartitionTier,
    pub status: WorkStatus,
    /// 这个分区一共匹配多少角色(不只是返回的那 100 个)。热门榜的分母。
    pub total: Option<u64>,
    pub fetched_at: Option<i64>,
}

/// 一条角色工作单。不带 `detail_json`——待抓的角色本来就没有,
/// 抓完的那一大坨 JSON 也不该跟着队列一起读进内存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterRow {
    pub account: String,
    pub name: String,
    pub class: String,
    pub level: u32,
    pub from_partition: String,
    pub tier: PartitionTier,
    pub status: WorkStatus,
    /// 抓的时候是哪个快照。快照一天变几次,统计时得知道这条详情多新。
    pub version: Option<String>,
    pub fetched_at: Option<i64>,
}

/// 暗金榜上的一行:人气(builds 的 `items` 分面)+ 参考价(经济接口)。
///
/// 两张表在库里就拼好了。一行一次 [`NinjaStore::unique_price`] 也能拼出来,
/// 但全联赛有四百多件暗金,那就是四百多次查询;`LEFT JOIN` 一次就够。
///
/// 价格那几格是 `Option`:没有价格的暗金照样要上榜(新出的、或者压根没人挂单),
/// 只是价格那几列写"—"。
#[derive(Debug, Clone, PartialEq)]
pub struct UniqueUsagePriced {
    pub name: String,
    /// 这个分区里有多少个角色穿着它。
    pub users: u64,
    /// 参考价 × 1000。**单位看下面那一格**,不是常数。
    pub price_milli: Option<i64>,
    /// 上面那个数是什么币(`divine` / `exalted` / `chaos` / …)。
    /// 和 `price_milli` 同生共死:价格表里没这件东西时两格都是 `None`。
    pub price_currency: Option<String>,
    pub listings: Option<i64>,
    /// 7 天涨跌,百分比。价格表里有这件东西、但 `sparkLine` 是空的时候也是 `None`。
    pub change_percent: Option<f64>,
}

/// 一件暗金的参考价快照。价格是 `primary_currency` × 1000。
#[derive(Debug, Clone, PartialEq)]
pub struct UniquePriceRow {
    pub type_name: String,
    pub name: String,
    pub base_type: String,
    pub category: String,
    pub primary_value_milli: i64,
    /// 抓这一行时经济接口自报的计价基准币。**不是常数**:2026-09-06 是
    /// `exalted`,2026-09-07 就成了 `divine`。
    pub primary_currency: String,
    pub listing_count: i64,
    pub total_change: Option<f64>,
    pub fetched_at: i64,
}

pub struct NinjaStore {
    conn: Connection,
}

impl NinjaStore {
    /// 这一代游戏在 `data_dir` 下的那个库。
    ///
    /// 分库不分表:表结构一个字都没动,两代各写各的文件。这个库整个是可以
    /// 随手删的缓存,所以"PoE1 的数据出问题就删 PoE1 那个文件"是它该有的
    /// 粒度;反过来给十几张表都加一列 `game` 要改一遍主键、还要迁移老库,
    /// 为一份删得起的缓存付这个价不值。
    pub fn open_for(game: Game, data_dir: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open(data_dir.as_ref().join(crate::ninja_db_file_name(game)))
    }

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
        // 采样线程和界面线程各开一个连接读同一个文件,5 秒足够错开一次写事务。
        conn.busy_timeout(Duration::from_secs(5))?;
        // 顺序有讲究:老词缀表得在建表语句**之前**删掉,不然没人把它建回来。
        drop_stale_item_mods(&conn)?;
        conn.execute_batch(NINJA_SCHEMA)?;
        add_primary_currency_column(&conn)?;
        Ok(Self { conn })
    }

    // ---- ninja_snapshots ------------------------------------------------

    /// 这个联赛最近开的一轮。"24 小时内不重跑"问的就是它的 `started_at`。
    pub fn latest_snapshot(&self, league_url: &str) -> Result<Option<SnapshotRow>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT league_url, version, snapshot_name, total_characters, stage,
                        started_at, finished_at
                 FROM ninja_snapshots WHERE league_url = ?1
                 ORDER BY started_at DESC, version DESC LIMIT 1",
                params![league_url],
                snapshot_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// 这个库里最近开的那一轮,**不管是哪个联赛**。
    ///
    /// 存在的理由是"联赛名留空 = 用当季挑战联赛":那时候调用方手上还没有短名
    /// (要等 index-state 才知道),却已经需要回答"上一轮跑到哪了"和"该按哪个
    /// 短名读缓存"。一个库只装一代、一代通常只盯一个联赛,所以"最近那一轮"
    /// 就是答案。
    pub fn latest_snapshot_any(&self) -> Result<Option<SnapshotRow>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT league_url, version, snapshot_name, total_characters, stage,
                        started_at, finished_at
                 FROM ninja_snapshots
                 ORDER BY started_at DESC, version DESC LIMIT 1",
                [],
                snapshot_from_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn upsert_snapshot(&self, snapshot: &SnapshotRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO ninja_snapshots (league_url, version, snapshot_name, total_characters,
                 stage, started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(league_url, version) DO UPDATE SET
                 snapshot_name = excluded.snapshot_name,
                 total_characters = excluded.total_characters,
                 stage = excluded.stage,
                 started_at = excluded.started_at,
                 finished_at = excluded.finished_at",
            params![
                snapshot.league_url,
                snapshot.version,
                snapshot.snapshot_name,
                to_i64(snapshot.total_characters),
                snapshot.stage.as_str(),
                snapshot.started_at,
                snapshot.finished_at,
            ],
        )?;
        Ok(())
    }

    /// 换阶段。只有推进到 `Aggregated` 才盖 `finished_at`——那才是"这一轮跑完了",
    /// 中间几步没有结束时刻可言。
    pub fn set_stage(
        &self,
        league_url: &str,
        version: &str,
        stage: SnapshotStage,
        now: i64,
    ) -> Result<(), StorageError> {
        let finished = (stage == SnapshotStage::Aggregated).then_some(now);
        self.conn.execute(
            "UPDATE ninja_snapshots
             SET stage = ?3, finished_at = COALESCE(?4, finished_at)
             WHERE league_url = ?1 AND version = ?2",
            params![league_url, version, stage.as_str(), finished],
        )?;
        Ok(())
    }

    // ---- ninja_partitions -----------------------------------------------

    /// 排工作单。**已经在库里的那条一个字都不动**(`INSERT OR IGNORE`):
    /// 重启后重新算一遍分区清单是常事,跑完的分区不该被打回 pending。
    pub fn enqueue_partitions(
        &self,
        league_url: &str,
        version: &str,
        partitions: &[Partition],
    ) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO ninja_partitions
                     (league_url, version, partition_key, tier, status)
                 VALUES (?1, ?2, ?3, ?4, 'pending')",
            )?;
            for partition in partitions {
                insert.execute(params![
                    league_url,
                    version,
                    partition.key,
                    partition.tier.as_str()
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 还没跑的分区,按当初排进来的顺序(职业档在技能档前面,见 `plan.rs`)。
    pub fn pending_partitions(
        &self,
        league_url: &str,
        version: &str,
    ) -> Result<Vec<PartitionRow>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT partition_key, tier, status, total, fetched_at
             FROM ninja_partitions
             WHERE league_url = ?1 AND version = ?2 AND status = 'pending'
             ORDER BY rowid",
        )?;
        let rows = statement.query_map(params![league_url, version], partition_from_row)?;
        collect(rows)
    }

    /// 一个分区跑完了:标 done、换掉它的分面、把角色名单加进队列——**一个事务**。
    ///
    /// 拆成三条独立语句的话,中途被杀会留下"标了 done 但分面只写了一半"的库,
    /// 而续跑逻辑正是靠 `status` 判断要不要重跑的,那样就永远补不回来了。
    ///
    /// 分面是**整组替换**(先删后插):同一个分区重跑一次,新数据该完全盖掉旧的。
    /// 角色是 `INSERT OR IGNORE`:已经抓过详情的人不能被打回 pending。
    ///
    /// 参数确实多(8 个),但拆不开:前三个是这一行的身份,后四个是这次事务里
    /// 要一起写下去的东西,少任何一个这个事务就不完整了。
    #[allow(clippy::too_many_arguments)]
    pub fn complete_partition(
        &self,
        league_url: &str,
        version: &str,
        key: &str,
        total: u64,
        facets: &[(String, String, u64)],
        characters: &[SampledCharacter],
        now: i64,
    ) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE ninja_partitions SET status = 'done', total = ?4, fetched_at = ?5
             WHERE league_url = ?1 AND version = ?2 AND partition_key = ?3",
            params![league_url, version, key, to_i64(total), now],
        )?;
        tx.execute(
            "DELETE FROM ninja_facets
             WHERE league_url = ?1 AND version = ?2 AND partition_key = ?3",
            params![league_url, version, key],
        )?;
        {
            let mut insert_facet = tx.prepare(
                "INSERT OR REPLACE INTO ninja_facets
                     (league_url, version, partition_key, facet, entry, count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (facet, entry, count) in facets {
                insert_facet.execute(params![
                    league_url,
                    version,
                    key,
                    facet,
                    entry,
                    to_i64(*count)
                ])?;
            }

            let mut insert_character = tx.prepare(
                "INSERT OR IGNORE INTO ninja_characters
                     (league_url, account, name, class, level, from_partition, tier, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')",
            )?;
            for character in characters {
                insert_character.execute(params![
                    league_url,
                    character.account,
                    character.name,
                    character.class,
                    i64::from(character.level),
                    character.from_partition,
                    character.tier.as_str(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 分区抓砸了。标 failed 而不是留 pending,免得同一条一直卡在队首;
    /// 下一轮换了 version 会重新排一遍。
    pub fn fail_partition(
        &self,
        league_url: &str,
        version: &str,
        key: &str,
        now: i64,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE ninja_partitions SET status = 'failed', fetched_at = ?4
             WHERE league_url = ?1 AND version = ?2 AND partition_key = ?3",
            params![league_url, version, key, now],
        )?;
        Ok(())
    }

    /// 跑完的分区键,给界面当筛选器用(全联赛 / 按职业 / 按技能)。
    pub fn partition_keys(
        &self,
        league_url: &str,
        version: &str,
    ) -> Result<Vec<String>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT partition_key FROM ninja_partitions
             WHERE league_url = ?1 AND version = ?2 AND status = 'done'
             ORDER BY rowid",
        )?;
        let rows = statement.query_map(params![league_url, version], |row| row.get(0))?;
        collect(rows)
    }

    /// 这个分区匹配了多少角色。热门暗金榜的"占比"分母就是它。
    pub fn partition_total(
        &self,
        league_url: &str,
        version: &str,
        key: &str,
    ) -> Result<Option<u64>, StorageError> {
        let total: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT total FROM ninja_partitions
                 WHERE league_url = ?1 AND version = ?2 AND partition_key = ?3",
                params![league_url, version, key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(total.flatten().map(to_u64))
    }

    // ---- ninja_characters -----------------------------------------------

    /// 队列的下 `limit` 个。顺序就是当初插进来的顺序,也就是分区的档次顺序:
    /// 万一这一轮跑不完,先抓到的至少是最有代表性的那批人。
    pub fn pending_characters(
        &self,
        league_url: &str,
        limit: u32,
    ) -> Result<Vec<CharacterRow>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT account, name, class, level, from_partition, tier, status, version, fetched_at
             FROM ninja_characters
             WHERE league_url = ?1 AND status = 'pending'
             ORDER BY rowid LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![league_url, i64::from(limit)],
            character_row_from_row,
        )?;
        collect(rows)
    }

    /// 详情原文整段存下来。存原文而不是存解析结果,是因为聚合口径以后一定会改
    /// (加个"珠宝按类型分开看"之类),那时候不该为了改一个统计维度重抓 2,000 个人。
    pub fn complete_character(
        &self,
        league_url: &str,
        account: &str,
        name: &str,
        version: &str,
        detail_json: &str,
        now: i64,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE ninja_characters
             SET status = 'done', version = ?4, detail_json = ?5, fetched_at = ?6
             WHERE league_url = ?1 AND account = ?2 AND name = ?3",
            params![league_url, account, name, version, detail_json, now],
        )?;
        Ok(())
    }

    /// 抓不到(多半是 404:这人删号或者改名了)。标 failed,这一轮不再理他。
    pub fn fail_character(
        &self,
        league_url: &str,
        account: &str,
        name: &str,
        now: i64,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE ninja_characters SET status = 'failed', fetched_at = ?4
             WHERE league_url = ?1 AND account = ?2 AND name = ?3",
            params![league_url, account, name, now],
        )?;
        Ok(())
    }

    /// 所有抓到手的详情原文,喂给 `aggregate_mods`。
    pub fn done_character_details(&self, league_url: &str) -> Result<Vec<String>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT detail_json FROM ninja_characters
             WHERE league_url = ?1 AND status = 'done' AND detail_json IS NOT NULL
             ORDER BY rowid",
        )?;
        let rows = statement.query_map(params![league_url], |row| row.get(0))?;
        collect(rows)
    }

    /// (待抓, 已抓, 失败)。进度条要的就这三个数。
    pub fn character_counts(&self, league_url: &str) -> Result<(u32, u32, u32), StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT status, COUNT(*) FROM ninja_characters WHERE league_url = ?1 GROUP BY status",
        )?;
        let rows = statement.query_map(params![league_url], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let (mut pending, mut done, mut failed) = (0, 0, 0);
        for row in rows {
            let (status, count) = row?;
            match WorkStatus::parse(&status) {
                WorkStatus::Pending => pending = to_u32(count),
                WorkStatus::Done => done = to_u32(count),
                WorkStatus::Failed => failed = to_u32(count),
            }
        }
        Ok((pending, done, failed))
    }

    // ---- ninja_item_mods ------------------------------------------------

    /// 整表重建(先删后插,一个事务)。词缀统计是从 `detail_json` 算出来的派生数据,
    /// 增量更新只会带来"半新半旧"的行;重算一次也就几百毫秒。
    pub fn replace_item_mods(
        &self,
        league_url: &str,
        version: &str,
        stats: &[SlotModStat],
    ) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM ninja_item_mods WHERE league_url = ?1 AND version = ?2",
            params![league_url, version],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO ninja_item_mods
                     (league_url, version, class, slot, rarity, mod_kind, stat_id,
                      display, value_index, mod_family,
                      characters, occurrences, sample_size, p25, p50, p75)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            )?;
            for stat in stats {
                insert.execute(params![
                    league_url,
                    version,
                    stat.class,
                    stat.slot,
                    stat.rarity,
                    stat.mod_kind,
                    stat.stat_id,
                    stat.display,
                    i64::from(stat.value_index),
                    stat.mod_family,
                    i64::from(stat.characters),
                    i64::from(stat.occurrences),
                    i64::from(stat.sample_size),
                    stat.p25,
                    stat.p50,
                    stat.p75,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 词缀热度页要的那一页。
    ///
    /// `min_share_percent` 过滤的是"带这条词缀的角色占这个部位样本的百分之几"。
    /// 计划里定的默认阈值是 2%:2,000 样本下,占比低于这个数的"刁钻词缀"
    /// 统计误差比它本身还大,显示出来只会误导。
    pub fn slot_mods(
        &self,
        league_url: &str,
        version: &str,
        slot: Option<&str>,
        rarity: Option<&str>,
        min_share_percent: f64,
    ) -> Result<Vec<SlotModStat>, StorageError> {
        self.slot_mods_of_kind(
            league_url,
            version,
            slot,
            rarity,
            None,
            None,
            min_share_percent,
        )
    }

    /// 这一轮的统计里出现过哪些职业,各自采到了多少个角色。词缀页那个职业下拉。
    ///
    /// 名单从 `ninja_item_mods` 来(那才是这一版快照真的算得出统计的职业),
    /// 人数从 `ninja_characters` 数(**不带 version**:详情是按联赛缓存的,
    /// 昨天采的人今天照样算数,统计也是这么算的)。
    pub fn mod_classes(
        &self,
        league_url: &str,
        version: &str,
    ) -> Result<Vec<(String, u32)>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT stats.class,
                    (SELECT COUNT(*) FROM ninja_characters AS people
                     WHERE people.league_url = ?1 AND people.status = 'done'
                       AND people.class = stats.class)
             FROM (SELECT DISTINCT class FROM ninja_item_mods
                   WHERE league_url = ?1 AND version = ?2 AND class <> '') AS stats
             ORDER BY 2 DESC, 1",
        )?;
        let rows = statement.query_map(params![league_url, version], |row| {
            Ok((row.get::<_, String>(0)?, to_u32(row.get::<_, i64>(1)?)))
        })?;
        collect(rows)
    }

    /// 同一张表,再多两个筛子:词缀类型(`explicit` / `implicit` / `rune` / …)
    /// 和职业。
    ///
    /// 两个都是主键的一维,所以在库里筛比读回来再筛便宜。分成一个兄弟函数而不是
    /// 给 [`slot_mods`](Self::slot_mods) 加参数:那个签名有三个调用方,
    /// 其中大部分本来就不关心这两维。
    ///
    /// `class` 给 `None` 就是**全样本那一套行**(库里写的是空串),不是"所有职业
    /// 的行都要":后者会把同一条词缀按职业数了十几遍,占比全乱。
    ///
    /// 参数确实多,但它们就是这张表的五个维度加一个阈值,捆成一个结构体只是把
    /// 同样几个名字换个地方写。
    #[allow(clippy::too_many_arguments)]
    pub fn slot_mods_of_kind(
        &self,
        league_url: &str,
        version: &str,
        slot: Option<&str>,
        rarity: Option<&str>,
        mod_kind: Option<&str>,
        class: Option<&str>,
        min_share_percent: f64,
    ) -> Result<Vec<SlotModStat>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT class, slot, rarity, mod_kind, stat_id, display, value_index, mod_family,
                    characters, occurrences, sample_size, p25, p50, p75
             FROM ninja_item_mods
             WHERE league_url = ?1 AND version = ?2
               AND class = COALESCE(?3, '')
               AND (?4 IS NULL OR slot = ?4)
               AND (?5 IS NULL OR rarity = ?5)
               AND (?6 IS NULL OR mod_kind = ?6)
               AND CAST(characters AS REAL) * 100.0 / MAX(sample_size, 1) >= ?7
             ORDER BY slot, rarity, mod_kind, characters DESC, stat_id",
        )?;
        let rows = statement.query_map(
            params![
                league_url,
                version,
                class,
                slot,
                rarity,
                mod_kind,
                min_share_percent
            ],
            slot_mod_from_row,
        )?;
        collect(rows)
    }

    // ---- ninja_facets ---------------------------------------------------

    /// 热门暗金榜:某个分区的 `items` 分面,人多的在前,稀有度桶排掉。
    pub fn unique_usage(
        &self,
        league_url: &str,
        version: &str,
        partition_key: &str,
    ) -> Result<Vec<(String, u64)>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT entry, count FROM ninja_facets
             WHERE league_url = ?1 AND version = ?2 AND partition_key = ?3 AND facet = 'items'
             ORDER BY count DESC, entry",
        )?;
        let rows = statement.query_map(params![league_url, version, partition_key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (entry, count) = row?;
            if !is_rarity_bucket(&entry) {
                out.push((entry, to_u64(count)));
            }
        }
        Ok(out)
    }

    /// 同一份榜,价格在库里就拼好了。
    ///
    /// 挑价格的规矩和 [`unique_price`](Self::unique_price) 一模一样:同名不同底子时
    /// 取挂单最多的那条,打平了按底子名定序。这里用窗口函数在子查询里先排好名次,
    /// 只让第一名参与 `JOIN` —— 不这么做的话,`Berek's Grip` 这种一名多底的
    /// 会在榜上出现好几行。
    pub fn unique_usage_with_prices(
        &self,
        league_url: &str,
        version: &str,
        partition_key: &str,
    ) -> Result<Vec<UniqueUsagePriced>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT facets.entry, facets.count, prices.primary_value_milli,
                    prices.primary_currency, prices.listing_count, prices.total_change
             FROM ninja_facets AS facets
             LEFT JOIN (
                 SELECT name, primary_value_milli, primary_currency, listing_count, total_change,
                        ROW_NUMBER() OVER (
                            PARTITION BY name ORDER BY listing_count DESC, base_type
                        ) AS seat
                 FROM ninja_unique_prices
                 WHERE league_url = ?1
             ) AS prices ON prices.name = facets.entry AND prices.seat = 1
             WHERE facets.league_url = ?1 AND facets.version = ?2
               AND facets.partition_key = ?3 AND facets.facet = 'items'
             ORDER BY facets.count DESC, facets.entry",
        )?;
        let rows = statement.query_map(params![league_url, version, partition_key], |row| {
            Ok(UniqueUsagePriced {
                name: row.get(0)?,
                users: to_u64(row.get::<_, i64>(1)?),
                price_milli: row.get(2)?,
                price_currency: row.get(3)?,
                listings: row.get(4)?,
                change_percent: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            if !is_rarity_bucket(&row.name) {
                out.push(row);
            }
        }
        Ok(out)
    }

    // ---- ninja_unique_prices --------------------------------------------

    /// 一整类暗金的参考价快照(先删后插,一个事务)。
    ///
    /// 按 `type_name` 分别替换而不是整表清空:6 个分类是 6 次请求,
    /// 其中一次失败不该把另外五类的价格也抹掉。
    ///
    /// `primary_currency` 是这一份 overview 的 `core.primary`,一行一存。
    /// 存在行上而不是当成常数写死,是因为它换过:2026-09-06 是 `exalted`,
    /// 2026-09-07 是 `divine`。六个分类各抓各的,理论上也可能一时不一致。
    pub fn replace_unique_prices(
        &self,
        league_url: &str,
        type_name: &str,
        primary_currency: &str,
        lines: &[UniquePriceLine],
        now: i64,
    ) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM ninja_unique_prices WHERE league_url = ?1 AND type_name = ?2",
            params![league_url, type_name],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO ninja_unique_prices
                     (league_url, fetched_at, type_name, name, base_type, category,
                      primary_value_milli, primary_currency, listing_count, total_change)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for line in lines {
                insert.execute(params![
                    league_url,
                    now,
                    type_name,
                    line.name,
                    line.base_type,
                    line.category,
                    to_milli(line.primary_value),
                    primary_currency,
                    line.listing_count,
                    // 存 `change_percent()` 而不是原始的 `total_change`:
                    // 只有一个数据点时那个 0 的意思是"没有一周的历史",
                    // 存成 0 会在界面上变成一句"这周没涨没跌"的谎话。
                    line.spark_line.change_percent(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 按暗金名找参考价。同名不同底子时挑挂单最多的那条——
    /// 挂单少的那条本来就不该当作市价。
    pub fn unique_price(
        &self,
        league_url: &str,
        name: &str,
    ) -> Result<Option<UniquePriceRow>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT type_name, name, base_type, category, primary_value_milli,
                        primary_currency, listing_count, total_change, fetched_at
                 FROM ninja_unique_prices
                 WHERE league_url = ?1 AND name = ?2
                 ORDER BY listing_count DESC, base_type LIMIT 1",
                params![league_url, name],
                unique_price_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// 这份价格表里**最旧**的那次抓取时刻(库里没有时钟,差值由调用方拿 `now` 减)。
    ///
    /// 取最旧而不是最新:6 个分类分别抓,只要有一类没刷新,整张表就是那么旧。
    pub fn unique_prices_age(&self, league_url: &str) -> Result<Option<i64>, StorageError> {
        let oldest: Option<i64> = self.conn.query_row(
            "SELECT MIN(fetched_at) FROM ninja_unique_prices WHERE league_url = ?1",
            params![league_url],
            |row| row.get(0),
        )?;
        Ok(oldest)
    }
}

/// `query_map` 的迭代器铺平成 `Vec`,顺手把第一个错抛出去。
fn collect<T>(rows: impl Iterator<Item = rusqlite::Result<T>>) -> Result<Vec<T>, StorageError> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 计数类的 `u64` 进库。SQLite 只有有符号 64 位整数,溢出的那一刻钉在上限
/// 而不是绕回负数——角色数不可能真到这个量级,真到了也是接口出了问题。
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn to_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// 浮点参考价 → 千分整数(单位由同一行的 `primary_currency` 说了算)。
/// 非有限值(接口给了 NaN/Infinity)记 0,
/// 而不是让一个 `as` 转换悄悄给出随便一个数。
fn to_milli(value: f64) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation)]
    let milli = (value * 1000.0).round() as i64;
    milli
}

fn snapshot_from_row(row: &Row<'_>) -> rusqlite::Result<SnapshotRow> {
    let total: i64 = row.get(3)?;
    let stage: String = row.get(4)?;
    Ok(SnapshotRow {
        league_url: row.get(0)?,
        version: row.get(1)?,
        snapshot_name: row.get(2)?,
        total_characters: to_u64(total),
        stage: SnapshotStage::parse(&stage),
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
    })
}

fn partition_from_row(row: &Row<'_>) -> rusqlite::Result<PartitionRow> {
    let tier: String = row.get(1)?;
    let status: String = row.get(2)?;
    let total: Option<i64> = row.get(3)?;
    Ok(PartitionRow {
        partition_key: row.get(0)?,
        tier: PartitionTier::parse(&tier),
        status: WorkStatus::parse(&status),
        total: total.map(to_u64),
        fetched_at: row.get(4)?,
    })
}

fn character_row_from_row(row: &Row<'_>) -> rusqlite::Result<CharacterRow> {
    let level: i64 = row.get(3)?;
    let tier: String = row.get(5)?;
    let status: String = row.get(6)?;
    Ok(CharacterRow {
        account: row.get(0)?,
        name: row.get(1)?,
        class: row.get(2)?,
        level: to_u32(level),
        from_partition: row.get(4)?,
        tier: PartitionTier::parse(&tier),
        status: WorkStatus::parse(&status),
        version: row.get(7)?,
        fetched_at: row.get(8)?,
    })
}

fn slot_mod_from_row(row: &Row<'_>) -> rusqlite::Result<SlotModStat> {
    let value_index: i64 = row.get(6)?;
    let characters: i64 = row.get(8)?;
    let occurrences: i64 = row.get(9)?;
    let sample_size: i64 = row.get(10)?;
    Ok(SlotModStat {
        class: row.get(0)?,
        slot: row.get(1)?,
        rarity: row.get(2)?,
        mod_kind: row.get(3)?,
        stat_id: row.get(4)?,
        display: row.get(5)?,
        value_index: u8::try_from(value_index).unwrap_or(0),
        mod_family: row.get(7)?,
        characters: to_u32(characters),
        occurrences: to_u32(occurrences),
        sample_size: to_u32(sample_size),
        p25: row.get(11)?,
        p50: row.get(12)?,
        p75: row.get(13)?,
    })
}

fn unique_price_from_row(row: &Row<'_>) -> rusqlite::Result<UniquePriceRow> {
    Ok(UniquePriceRow {
        type_name: row.get(0)?,
        name: row.get(1)?,
        base_type: row.get(2)?,
        category: row.get(3)?,
        primary_value_milli: row.get(4)?,
        primary_currency: row.get(5)?,
        listing_count: row.get(6)?,
        total_change: row.get(7)?,
        fetched_at: row.get(8)?,
    })
}

#[cfg(test)]
mod ninja_tests {
    use super::*;

    const LEAGUE: &str = "forbiddenrites";
    const VERSION: &str = "1508-20260906-55820";

    fn store() -> NinjaStore {
        NinjaStore::open_in_memory().expect("open")
    }

    /// 两代各一个库文件,**PoE2 那个的路径一个字都不许变**。
    ///
    /// 分文件而不是给每张表加一列 `game`,理由是这个库整个都是可以随手删的
    /// 缓存:分文件等于"PoE1 的数据出问题就删 PoE1 那个文件",而加一列意味着
    /// 十几张表的主键全要改一遍、老库还得迁移 —— 为一份删得起的缓存付这个价
    /// 不值。PoE2 的文件名保持原样,是因为本机那个库里已经躺着一整轮采样,
    /// 换个名字等于让它明天从头再采一天。
    #[test]
    fn each_game_gets_its_own_database_file() {
        let dir = std::env::temp_dir().join(format!("pnd-ninja-games-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let poe2 = NinjaStore::open_for(Game::Poe2, &dir).expect("poe2");
        assert!(dir.join("ninja.sqlite").is_file(), "PoE2 还是老文件名");
        assert!(!dir.join("ninja-poe1.sqlite").exists());

        let poe1 = NinjaStore::open_for(Game::Poe1, &dir).expect("poe1");
        assert!(dir.join("ninja-poe1.sqlite").is_file());

        // 两边真的是两份数据:写进 PoE1 的快照不会从 PoE2 那边读出来。
        poe1.upsert_snapshot(&SnapshotRow {
            league_url: "allflame".to_owned(),
            version: "1707-20260908-44259".to_owned(),
            snapshot_name: "allflame".to_owned(),
            total_characters: 124_459,
            stage: SnapshotStage::Facets,
            started_at: 1_000,
            finished_at: None,
        })
        .expect("write");
        assert!(poe1.latest_snapshot("allflame").expect("read").is_some());
        assert!(poe2.latest_snapshot("allflame").expect("read").is_none());

        drop(poe1);
        drop(poe2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 联赛名留空时,库自己说得出"最近跑的是哪个联赛"。
    ///
    /// 界面读缓存要一个联赛短名,而"用当季挑战联赛"那条路上,短名只有采样
    /// 线程读过 index-state 之后才知道。库里那一行就是它留下的答案。
    #[test]
    fn the_store_can_name_the_league_it_most_recently_sampled() {
        let store = store();
        assert!(store.latest_snapshot_any().expect("read").is_none());

        store
            .upsert_snapshot(&SnapshotRow {
                league_url: "allflame".to_owned(),
                version: "old".to_owned(),
                snapshot_name: "allflame".to_owned(),
                total_characters: 124_459,
                stage: SnapshotStage::Facets,
                started_at: 1_000,
                finished_at: None,
            })
            .expect("old");
        store
            .upsert_snapshot(&SnapshotRow {
                league_url: "allflamehc".to_owned(),
                version: "new".to_owned(),
                snapshot_name: "hardcore-allflame".to_owned(),
                total_characters: 9_000,
                stage: SnapshotStage::Facets,
                started_at: 5_000,
                finished_at: None,
            })
            .expect("new");

        let latest = store.latest_snapshot_any().expect("read").expect("row");
        assert_eq!(latest.league_url, "allflamehc", "最近开的那一轮");
        assert_eq!(latest.version, "new");
    }

    /// 默认路径也分代,而且两个都落在同一个数据目录下。
    #[test]
    fn the_default_paths_sit_side_by_side_in_one_folder() {
        let poe2 = crate::default_ninja_db_path_for(Game::Poe2);
        let poe1 = crate::default_ninja_db_path_for(Game::Poe1);
        assert_eq!(poe2, crate::default_ninja_db_path(), "老调用方一个字不用改");
        assert_eq!(poe2.file_name().unwrap(), "ninja.sqlite");
        assert_eq!(poe1.file_name().unwrap(), "ninja-poe1.sqlite");
        assert_eq!(poe1.parent(), poe2.parent());
    }

    fn snapshot(stage: SnapshotStage, started_at: i64) -> SnapshotRow {
        SnapshotRow {
            league_url: LEAGUE.to_owned(),
            version: VERSION.to_owned(),
            snapshot_name: "forbidden-rites".to_owned(),
            total_characters: 61_390,
            stage,
            started_at,
            finished_at: None,
        }
    }

    fn partitions() -> Vec<Partition> {
        vec![
            Partition::new(PartitionTier::Whole, Vec::new()),
            Partition::new(
                PartitionTier::Class,
                vec![("class".to_owned(), "Gemling Legionnaire".to_owned())],
            ),
            Partition::new(
                PartitionTier::Unique,
                vec![("items".to_owned(), "Wake of Destruction".to_owned())],
            ),
        ]
    }

    fn sampled(account: &str, name: &str, tier: PartitionTier) -> SampledCharacter {
        SampledCharacter {
            account: account.to_owned(),
            name: name.to_owned(),
            class: "Gemling Legionnaire".to_owned(),
            level: 98,
            from_partition: "class=Gemling Legionnaire".to_owned(),
            tier,
        }
    }

    fn stat(slot: &str, rarity: &str, stat_id: &str, characters: u32, sample: u32) -> SlotModStat {
        SlotModStat {
            class: String::new(),
            slot: slot.to_owned(),
            rarity: rarity.to_owned(),
            mod_kind: "explicit".to_owned(),
            stat_id: stat_id.to_owned(),
            display: "+# to maximum Life".to_owned(),
            value_index: 1,
            mod_family: "IncreasedLife".to_owned(),
            characters,
            occurrences: characters + 1,
            sample_size: sample,
            p25: Some(95.0),
            p50: Some(115.0),
            p75: Some(135.0),
        }
    }

    /// 一行有一周历史的参考价:`data` 里两个真数,所以 `-4.5%` 算数。
    fn price_line(name: &str, base_type: &str, value: f64, listings: i64) -> UniquePriceLine {
        priced(
            name,
            base_type,
            value,
            listings,
            "[null,100.0,null,95.5]",
            -4.5,
        )
    }

    /// 同上,但走势由调用方给 —— 新联赛那种"一整周只有今天一个点"的形状
    /// 就是靠它进测试的。
    fn priced(
        name: &str,
        base_type: &str,
        value: f64,
        listings: i64,
        data: &str,
        total_change: f64,
    ) -> UniquePriceLine {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","baseType":"{base_type}","category":"Ring",
                 "primaryValue":{value},"listingCount":{listings},
                 "sparkLine":{{"totalChange":{total_change},"data":{data}}}}}"#
        ))
        .expect("line")
    }

    /// 2026-09-06 建的老库必须能原地升级。
    ///
    /// 建表语句是 `CREATE TABLE IF NOT EXISTS`:表已经在了,它一个字都改不动。
    /// 不补那一次 `ALTER TABLE`,老库一开就是 "no such column: primary_currency",
    /// 整个暗金页读不出来。
    #[test]
    fn an_old_cache_gets_the_currency_column_added() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE ninja_unique_prices (
                 league_url TEXT NOT NULL,
                 fetched_at INTEGER NOT NULL,
                 type_name TEXT NOT NULL,
                 name TEXT NOT NULL,
                 base_type TEXT NOT NULL,
                 category TEXT NOT NULL,
                 primary_value_milli INTEGER NOT NULL,
                 listing_count INTEGER NOT NULL,
                 total_change REAL,
                 PRIMARY KEY (league_url, type_name, name, base_type)
             ) STRICT;
             INSERT INTO ninja_unique_prices VALUES
                 ('forbiddenrites', 9000, 'UniqueWeapons', 'Skysliver',
                  'Sacrificial Blade', 'Sceptre', 85, 12, NULL);",
        )
        .expect("old schema");

        let store = NinjaStore::initialize(conn).expect("upgrade");
        let row = store
            .unique_price(LEAGUE, "Skysliver")
            .expect("read")
            .expect("row");
        assert_eq!(row.primary_value_milli, 85);
        assert_eq!(
            row.primary_currency, "divine",
            "老行是从同一个端点抓的,而它今天报的就是 divine"
        );

        // 升级那一步每次开库都会走一遍,第二遍必须是空操作。
        add_primary_currency_column(&store.conn).expect("second run");
        assert_eq!(
            store
                .unique_price(LEAGUE, "Skysliver")
                .expect("read")
                .expect("row"),
            row
        );
    }

    /// 建表语句必须能在同一个库上跑第二遍:每次开库都会执行它。
    #[test]
    fn schema_applies_twice() {
        let store = store();
        store
            .conn
            .execute_batch(NINJA_SCHEMA)
            .expect("second apply");
    }

    #[test]
    fn a_snapshot_round_trips_and_stages_advance() {
        let store = store();
        assert_eq!(store.latest_snapshot(LEAGUE).expect("read"), None);

        store
            .upsert_snapshot(&snapshot(SnapshotStage::Planned, 1_000))
            .expect("insert");
        assert_eq!(
            store.latest_snapshot(LEAGUE).expect("read"),
            Some(snapshot(SnapshotStage::Planned, 1_000))
        );

        // 同一个 (league, version) 再写一次是覆盖,不是第二行。
        store
            .upsert_snapshot(&snapshot(SnapshotStage::Facets, 1_100))
            .expect("update");
        let row = store.latest_snapshot(LEAGUE).expect("read").expect("row");
        assert_eq!(row.stage, SnapshotStage::Facets);
        assert_eq!(row.started_at, 1_100);

        // 中间几步没有结束时刻。
        store
            .set_stage(LEAGUE, VERSION, SnapshotStage::Characters, 1_200)
            .expect("stage");
        let row = store.latest_snapshot(LEAGUE).expect("read").expect("row");
        assert_eq!(row.stage, SnapshotStage::Characters);
        assert_eq!(row.finished_at, None);

        store
            .set_stage(LEAGUE, VERSION, SnapshotStage::Aggregated, 1_300)
            .expect("stage");
        let row = store.latest_snapshot(LEAGUE).expect("read").expect("row");
        assert_eq!(row.stage, SnapshotStage::Aggregated);
        assert_eq!(row.finished_at, Some(1_300));

        assert_eq!(store.latest_snapshot("nosuchleague").expect("read"), None);
    }

    /// 一天跑好几轮时,"最近一轮"看的是 started_at。
    #[test]
    fn the_latest_snapshot_is_the_one_started_last() {
        let store = store();
        let mut older = snapshot(SnapshotStage::Aggregated, 1_000);
        older.version = "1400-20260905-11111".to_owned();
        store.upsert_snapshot(&older).expect("insert");
        store
            .upsert_snapshot(&snapshot(SnapshotStage::Planned, 2_000))
            .expect("insert");
        assert_eq!(
            store
                .latest_snapshot(LEAGUE)
                .expect("read")
                .expect("row")
                .version,
            VERSION
        );
    }

    #[test]
    fn enqueue_then_complete_moves_a_partition_out_of_the_queue() {
        let store = store();
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");

        let pending = store.pending_partitions(LEAGUE, VERSION).expect("pending");
        assert_eq!(
            pending
                .iter()
                .map(|row| row.partition_key.as_str())
                .collect::<Vec<_>>(),
            vec!["", "class=Gemling Legionnaire", "items=Wake of Destruction"]
        );
        assert_eq!(pending[0].tier, PartitionTier::Whole);
        assert_eq!(pending[1].tier, PartitionTier::Class);
        assert_eq!(pending[2].tier, PartitionTier::Unique);
        assert_eq!(pending[0].status, WorkStatus::Pending);
        assert_eq!(pending[0].total, None);
        assert!(
            store
                .partition_keys(LEAGUE, VERSION)
                .expect("keys")
                .is_empty()
        );

        let facets = vec![
            (
                "items".to_owned(),
                "Wake of Destruction".to_owned(),
                7_158u64,
            ),
            ("items".to_owned(), "Rare Ring".to_owned(), 44_000),
            ("skills".to_owned(), "Spark".to_owned(), 3_000),
        ];
        let characters = vec![
            sampled("heygyus-0416", "ResurrectForbidden", PartitionTier::Whole),
            sampled("dota2enjoyer-1809", "KingPinUwU", PartitionTier::Whole),
        ];
        store
            .complete_partition(LEAGUE, VERSION, "", 61_390, &facets, &characters, 5_000)
            .expect("complete");

        let pending = store.pending_partitions(LEAGUE, VERSION).expect("pending");
        assert_eq!(pending.len(), 2, "跑完的那条不该还在队列里");
        assert_eq!(
            store.partition_keys(LEAGUE, VERSION).expect("keys"),
            vec![""]
        );
        assert_eq!(
            store.partition_total(LEAGUE, VERSION, "").expect("total"),
            Some(61_390)
        );
        // 还没跑的分区没有 total,不存在的分区也没有。
        assert_eq!(
            store
                .partition_total(LEAGUE, VERSION, "class=Gemling Legionnaire")
                .expect("total"),
            None
        );
        assert_eq!(
            store
                .partition_total(LEAGUE, VERSION, "nope")
                .expect("total"),
            None
        );

        // 角色名单跟着进了队列,状态是 pending。
        assert_eq!(store.character_counts(LEAGUE).expect("counts"), (2, 0, 0));
    }

    /// 重启后重新算一遍分区清单是常事:已经跑完的那条一个字都不能被动。
    #[test]
    fn enqueueing_the_same_partitions_again_changes_nothing() {
        let store = store();
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        store
            .complete_partition(LEAGUE, VERSION, "", 61_390, &[], &[], 5_000)
            .expect("complete");

        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue again");

        assert_eq!(
            store
                .pending_partitions(LEAGUE, VERSION)
                .expect("pending")
                .len(),
            2
        );
        assert_eq!(
            store.partition_keys(LEAGUE, VERSION).expect("keys"),
            vec![""],
            "跑完的分区不该被打回 pending"
        );
    }

    /// 同一个分区重跑一次,新分面完全盖掉旧的,而不是叠加。
    #[test]
    fn completing_a_partition_replaces_its_facets() {
        let store = store();
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        let first = vec![
            (
                "items".to_owned(),
                "Wake of Destruction".to_owned(),
                7_158u64,
            ),
            ("items".to_owned(), "Beira's Anguish".to_owned(), 6_413),
        ];
        store
            .complete_partition(LEAGUE, VERSION, "", 61_390, &first, &[], 5_000)
            .expect("complete");

        let second = vec![(
            "items".to_owned(),
            "Wake of Destruction".to_owned(),
            9_000u64,
        )];
        store
            .complete_partition(LEAGUE, VERSION, "", 62_000, &second, &[], 6_000)
            .expect("recomplete");

        assert_eq!(
            store.unique_usage(LEAGUE, VERSION, "").expect("usage"),
            vec![("Wake of Destruction".to_owned(), 9_000)]
        );
        assert_eq!(
            store.partition_total(LEAGUE, VERSION, "").expect("total"),
            Some(62_000)
        );
    }

    #[test]
    fn a_failed_partition_leaves_the_queue_too() {
        let store = store();
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        store
            .fail_partition(LEAGUE, VERSION, "items=Wake of Destruction", 5_000)
            .expect("fail");

        let pending = store.pending_partitions(LEAGUE, VERSION).expect("pending");
        assert_eq!(pending.len(), 2);
        assert!(
            store
                .partition_keys(LEAGUE, VERSION)
                .expect("keys")
                .is_empty(),
            "失败的分区不算 done"
        );
    }

    /// 热门暗金榜绝不能被 `Rare Ring` 这类稀有度桶占掉榜首。
    #[test]
    fn unique_usage_drops_rarity_buckets() {
        let store = store();
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        let facets = vec![
            ("items".to_owned(), "Magic Flask".to_owned(), 60_943u64),
            ("items".to_owned(), "Rare Ring".to_owned(), 44_000),
            ("items".to_owned(), "Wake of Destruction".to_owned(), 7_158),
            ("items".to_owned(), "Beira's Anguish".to_owned(), 6_413),
            // 别的分面不该混进暗金榜。
            ("skills".to_owned(), "Spark".to_owned(), 3_000),
        ];
        store
            .complete_partition(LEAGUE, VERSION, "", 61_390, &facets, &[], 5_000)
            .expect("complete");

        assert_eq!(
            store.unique_usage(LEAGUE, VERSION, "").expect("usage"),
            vec![
                ("Wake of Destruction".to_owned(), 7_158),
                ("Beira's Anguish".to_owned(), 6_413),
            ]
        );
        assert!(
            store
                .unique_usage(LEAGUE, VERSION, "nosuchpartition")
                .expect("usage")
                .is_empty()
        );
    }

    /// 榜和价格在库里就拼好:同一份行,同一个顺序,少四百多次查询。
    ///
    /// 三件事必须成立:没有价格的暗金照样上榜(三格是 `None`)、顺序还是人多的在前、
    /// 一名多底子时挑的还是挂单最多的那条(和 `unique_price` 同一条规矩)。
    #[test]
    fn unique_usage_with_prices_joins_the_two_tables_in_sql() {
        let store = store();
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        let facets = vec![
            ("items".to_owned(), "Rare Ring".to_owned(), 44_000u64),
            ("items".to_owned(), "Wake of Destruction".to_owned(), 7_158),
            ("items".to_owned(), "Berek's Grip".to_owned(), 6_413),
            // 经济接口里查不到的那件:新出的,或者压根没人挂单。
            ("items".to_owned(), "Beira's Anguish".to_owned(), 2_000),
            ("skills".to_owned(), "Spark".to_owned(), 3_000),
        ];
        store
            .complete_partition(LEAGUE, VERSION, "", 61_390, &facets, &[], 5_000)
            .expect("complete");
        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueAccessories",
                "divine",
                &[
                    // 同一个名字两个底子:挂单少的那条不该被当成市价。
                    price_line("Berek's Grip", "Coral Ring", 10.0, 2),
                    price_line("Berek's Grip", "Two-Stone Ring", 240.0, 24),
                ],
                9_000,
            )
            .expect("prices");
        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueWeapons",
                "divine",
                &[price_line(
                    "Wake of Destruction",
                    "Wrapped Greathelm",
                    3.25,
                    900,
                )],
                9_000,
            )
            .expect("prices");

        let rows = store
            .unique_usage_with_prices(LEAGUE, VERSION, "")
            .expect("usage");
        assert_eq!(
            rows,
            vec![
                UniqueUsagePriced {
                    name: "Wake of Destruction".to_owned(),
                    users: 7_158,
                    price_milli: Some(3_250),
                    price_currency: Some("divine".to_owned()),
                    listings: Some(900),
                    change_percent: Some(-4.5),
                },
                UniqueUsagePriced {
                    name: "Berek's Grip".to_owned(),
                    users: 6_413,
                    price_milli: Some(240_000),
                    price_currency: Some("divine".to_owned()),
                    listings: Some(24),
                    change_percent: Some(-4.5),
                },
                UniqueUsagePriced {
                    name: "Beira's Anguish".to_owned(),
                    users: 2_000,
                    price_milli: None,
                    price_currency: None,
                    listings: None,
                    change_percent: None,
                },
            ],
            "人多的在前;没挂单的照样上榜,只是价格那几格空着"
        );

        // 和老函数说的是同一件事,只是多带了价格。
        assert_eq!(
            store.unique_usage(LEAGUE, VERSION, "").expect("usage"),
            rows.iter()
                .map(|row| (row.name.clone(), row.users))
                .collect::<Vec<_>>()
        );

        assert!(
            store
                .unique_usage_with_prices(LEAGUE, VERSION, "nosuchpartition")
                .expect("usage")
                .is_empty()
        );
    }

    #[test]
    fn characters_move_from_pending_to_done_or_failed() {
        let store = store();
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        let characters = vec![
            sampled("heygyus-0416", "ResurrectForbidden", PartitionTier::Class),
            sampled("dota2enjoyer-1809", "KingPinUwU", PartitionTier::Skill),
            sampled("elinskiy2002-4257", "sqvoznyak", PartitionTier::Unique),
        ];
        store
            .complete_partition(LEAGUE, VERSION, "", 61_390, &[], &characters, 5_000)
            .expect("complete");

        let queue = store.pending_characters(LEAGUE, 2).expect("pending");
        assert_eq!(queue.len(), 2, "limit 说了几个就给几个");
        assert_eq!(queue[0].name, "ResurrectForbidden");
        assert_eq!(queue[0].tier, PartitionTier::Class);
        assert_eq!(queue[0].level, 98);
        assert_eq!(queue[0].from_partition, "class=Gemling Legionnaire");
        assert_eq!(queue[0].status, WorkStatus::Pending);
        assert_eq!(queue[0].version, None);
        assert_eq!(queue[0].fetched_at, None);

        store
            .complete_character(
                LEAGUE,
                "heygyus-0416",
                "ResurrectForbidden",
                VERSION,
                r#"{"name":"ResurrectForbidden"}"#,
                6_000,
            )
            .expect("done");
        store
            .fail_character(LEAGUE, "dota2enjoyer-1809", "KingPinUwU", 6_001)
            .expect("failed");

        assert_eq!(store.character_counts(LEAGUE).expect("counts"), (1, 1, 1));
        let queue = store.pending_characters(LEAGUE, 100).expect("pending");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].name, "sqvoznyak");

        assert_eq!(
            store.done_character_details(LEAGUE).expect("details"),
            vec![r#"{"name":"ResurrectForbidden"}"#.to_owned()],
            "只有抓到手的才进聚合"
        );
    }

    /// 第二轮采样重排同一批人时,已经抓过详情的不能被打回 pending —— 那等于白抓。
    #[test]
    fn re_enqueueing_a_character_does_not_reset_it() {
        let store = store();
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        let characters = vec![sampled(
            "heygyus-0416",
            "ResurrectForbidden",
            PartitionTier::Class,
        )];
        store
            .complete_partition(LEAGUE, VERSION, "", 1, &[], &characters, 5_000)
            .expect("complete");
        store
            .complete_character(
                LEAGUE,
                "heygyus-0416",
                "ResurrectForbidden",
                VERSION,
                "{}",
                6_000,
            )
            .expect("done");

        store
            .complete_partition(
                LEAGUE,
                VERSION,
                "class=Gemling Legionnaire",
                1,
                &[],
                &characters,
                7_000,
            )
            .expect("complete again");

        assert_eq!(store.character_counts(LEAGUE).expect("counts"), (0, 1, 0));
        assert!(
            store
                .pending_characters(LEAGUE, 100)
                .expect("pending")
                .is_empty()
        );
    }

    /// 角色详情是按 `(联赛, 账号, 角色名)` 缓存的,**和快照 version 无关**。
    ///
    /// 一小时 100 个请求意味着 2,000 个人要采一两天,中间快照 version 会换好几
    /// 次。换一次就把昨天采到的人当成没采过,那这个目标永远也够不着 ——
    /// 所以三件事必须成立:昨天抓过的今天不排队、计数把昨天的算进去、
    /// 聚合读得到昨天的原文。
    #[test]
    fn character_details_survive_a_snapshot_change_and_stay_in_the_league_pot() {
        let store = store();
        // 昨天那一轮:排两个人,抓到一个。
        store
            .enqueue_partitions(LEAGUE, "yesterday", &partitions())
            .expect("enqueue");
        let yesterday = vec![
            sampled("heygyus-0416", "ResurrectForbidden", PartitionTier::Whole),
            sampled("dota2enjoyer-1809", "KingPinUwU", PartitionTier::Whole),
        ];
        store
            .complete_partition(LEAGUE, "yesterday", "", 2, &[], &yesterday, 1_000)
            .expect("complete");
        store
            .complete_character(
                LEAGUE,
                "heygyus-0416",
                "ResurrectForbidden",
                "yesterday",
                r#"{"name":"ResurrectForbidden"}"#,
                1_000,
            )
            .expect("done");

        // 今天那一轮:新 version,同一批人加一张新面孔。
        store
            .enqueue_partitions(LEAGUE, "today", &partitions())
            .expect("enqueue");
        let mut today = yesterday.clone();
        today.push(sampled(
            "elinskiy2002-4257",
            "sqvoznyak",
            PartitionTier::Whole,
        ));
        store
            .complete_partition(LEAGUE, "today", "", 3, &[], &today, 2_000)
            .expect("complete");

        // 昨天抓过的那个不再排队,队列里只剩两张没抓过的脸。
        let queue = store.pending_characters(LEAGUE, 100).expect("pending");
        assert_eq!(
            queue
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            vec!["KingPinUwU", "sqvoznyak"],
            "换一版快照不该让已经抓过的人重新排队"
        );

        // 计数和聚合读的都是整个联赛的锅,不分 version。
        assert_eq!(store.character_counts(LEAGUE).expect("counts"), (2, 1, 0));
        assert_eq!(
            store.done_character_details(LEAGUE).expect("details"),
            vec![r#"{"name":"ResurrectForbidden"}"#.to_owned()]
        );

        // 今天再抓一个,昨天那个还在。
        store
            .complete_character(
                LEAGUE,
                "dota2enjoyer-1809",
                "KingPinUwU",
                "today",
                r#"{"name":"KingPinUwU"}"#,
                2_100,
            )
            .expect("done");
        assert_eq!(store.character_counts(LEAGUE).expect("counts"), (1, 2, 0));
        assert_eq!(
            store.done_character_details(LEAGUE).expect("details").len(),
            2,
            "统计要把两天采到的人都算进去"
        );
    }

    #[test]
    fn item_mods_are_replaced_wholesale_and_filtered_on_read() {
        let store = store();
        let stats = vec![
            stat("Ring", "Rare", "base_maximum_life", 900, 1_000),
            stat("Ring", "Rare", "base_fire_damage_resistance_%", 700, 1_000),
            // 占比 1%,低于默认阈值 2%:统计误差比它本身还大。
            stat("Ring", "Rare", "base_movement_velocity_+%", 10, 1_000),
            stat("BodyArmour", "Unique", "cannot_be_frozen", 500, 1_000),
        ];
        store
            .replace_item_mods(LEAGUE, VERSION, &stats)
            .expect("replace");

        let all = store
            .slot_mods(LEAGUE, VERSION, None, None, 0.0)
            .expect("read");
        assert_eq!(all.len(), 4);
        // 排序:部位 → 稀有度 → 类型 → 人多的在前。
        assert_eq!(all[0].slot, "BodyArmour");
        assert_eq!(all[1].stat_id, "base_maximum_life");
        assert_eq!(all[1].mod_family, "IncreasedLife");
        assert_eq!(all[1].occurrences, 901);
        assert_eq!(all[1].sample_size, 1_000);
        assert_eq!(
            (all[1].p25, all[1].p50, all[1].p75),
            (Some(95.0), Some(115.0), Some(135.0))
        );

        let by_slot = store
            .slot_mods(LEAGUE, VERSION, Some("Ring"), None, 0.0)
            .expect("read");
        assert_eq!(by_slot.len(), 3);
        assert!(by_slot.iter().all(|row| row.slot == "Ring"));

        let by_rarity = store
            .slot_mods(LEAGUE, VERSION, None, Some("Unique"), 0.0)
            .expect("read");
        assert_eq!(by_rarity.len(), 1);
        assert_eq!(by_rarity[0].slot, "BodyArmour");

        let common = store
            .slot_mods(LEAGUE, VERSION, Some("Ring"), Some("Rare"), 2.0)
            .expect("read");
        assert_eq!(
            common
                .iter()
                .map(|row| row.stat_id.as_str())
                .collect::<Vec<_>>(),
            vec!["base_maximum_life", "base_fire_damage_resistance_%"],
            "占比 1% 的刁钻词缀被 2% 的阈值挡掉"
        );

        // 重建是整表替换,不是叠加。
        store
            .replace_item_mods(
                LEAGUE,
                VERSION,
                &[stat("Ring", "Rare", "base_maximum_life", 950, 1_000)],
            )
            .expect("replace again");
        let all = store
            .slot_mods(LEAGUE, VERSION, None, None, 0.0)
            .expect("read");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].characters, 950);

        // 另一个 version 的行互不干扰。
        assert!(
            store
                .slot_mods(LEAGUE, "other-version", None, None, 0.0)
                .expect("read")
                .is_empty()
        );
    }

    /// 词缀类型是主键的一维,所以在库里筛得动:要"只看后缀词缀"就不必把
    /// 符文和铭刻也读回来。
    #[test]
    fn item_mods_can_be_narrowed_to_one_kind() {
        let store = store();
        let of_kind = |kind: &str, stat_id: &str, characters: u32| SlotModStat {
            mod_kind: kind.to_owned(),
            ..stat("Ring", "Rare", stat_id, characters, 1_000)
        };
        let stats = vec![
            of_kind("explicit", "base_maximum_life", 900),
            of_kind("explicit", "base_fire_damage_resistance_%", 700),
            of_kind("rune", "local_physical_damage_+%", 600),
            of_kind("implicit", "base_chaos_damage_resistance_%", 500),
            // 占比 1%:阈值和类型筛子要能叠在一起用。
            of_kind("explicit", "base_movement_velocity_+%", 10),
        ];
        store
            .replace_item_mods(LEAGUE, VERSION, &stats)
            .expect("replace");

        let explicit = store
            .slot_mods_of_kind(LEAGUE, VERSION, None, None, Some("explicit"), None, 0.0)
            .expect("read");
        assert_eq!(
            explicit
                .iter()
                .map(|row| row.stat_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "base_maximum_life",
                "base_fire_damage_resistance_%",
                "base_movement_velocity_+%",
            ]
        );

        let runes = store
            .slot_mods_of_kind(LEAGUE, VERSION, None, None, Some("rune"), None, 0.0)
            .expect("read");
        assert_eq!(runes.len(), 1);
        assert_eq!(runes[0].mod_kind, "rune");

        // 类型和部位、稀有度、阈值是"和"的关系。
        let common = store
            .slot_mods_of_kind(
                LEAGUE,
                VERSION,
                Some("Ring"),
                Some("Rare"),
                Some("explicit"),
                None,
                2.0,
            )
            .expect("read");
        assert_eq!(common.len(), 2, "占比 1% 的那条被阈值挡掉");

        // 不认识的类型给空,而不是悄悄退回全部。
        assert!(
            store
                .slot_mods_of_kind(LEAGUE, VERSION, None, None, Some("enchant"), None, 0.0)
                .expect("read")
                .is_empty()
        );

        // `None` 就是不筛:和老的 `slot_mods` 一字不差。
        assert_eq!(
            store
                .slot_mods_of_kind(LEAGUE, VERSION, None, None, None, None, 0.0)
                .expect("read"),
            store
                .slot_mods(LEAGUE, VERSION, None, None, 0.0)
                .expect("read")
        );
    }

    /// 职业是主键的一维,所以在库里筛得动;`None` 问的是全样本那一套。
    #[test]
    fn item_mods_can_be_narrowed_to_one_class() {
        let store = store();
        let of_class = |class: &str, stat_id: &str, characters: u32, sample: u32| SlotModStat {
            class: class.to_owned(),
            ..stat("Ring", "Rare", stat_id, characters, sample)
        };
        let stats = vec![
            // 全样本:2,000 个人里 1,800 个戴稀有戒指。
            of_class("", "base_maximum_life", 1_500, 1_800),
            of_class("", "base_fire_damage_resistance_%", 900, 1_800),
            // Deadeye 自己那一套,分母跟着换。
            of_class("Deadeye", "base_maximum_life", 100, 120),
            of_class("Deadeye", "base_movement_velocity_+%", 2, 120),
            of_class("Gemling Legionnaire", "base_maximum_life", 60, 90),
        ];
        store
            .replace_item_mods(LEAGUE, VERSION, &stats)
            .expect("replace");

        // `None` = 全样本,而不是"所有职业的行加起来"。
        let whole = store
            .slot_mods_of_kind(LEAGUE, VERSION, None, None, None, None, 0.0)
            .expect("read");
        assert_eq!(whole.len(), 2);
        assert!(whole.iter().all(|row| row.class.is_empty()));
        assert_eq!(whole[0].characters, 1_500);

        let deadeye = store
            .slot_mods_of_kind(LEAGUE, VERSION, None, None, None, Some("Deadeye"), 0.0)
            .expect("read");
        assert_eq!(
            deadeye
                .iter()
                .map(|row| (row.class.as_str(), row.stat_id.as_str(), row.sample_size))
                .collect::<Vec<_>>(),
            vec![
                ("Deadeye", "base_maximum_life", 120),
                ("Deadeye", "base_movement_velocity_+%", 120),
            ]
        );

        // 阈值按这个职业自己的分母算:2/120 = 1.7%,被 2% 挡掉。
        let common = store
            .slot_mods_of_kind(LEAGUE, VERSION, None, None, None, Some("Deadeye"), 2.0)
            .expect("read");
        assert_eq!(common.len(), 1);
        assert_eq!(common[0].stat_id, "base_maximum_life");

        // 没采到的职业给空,而不是悄悄退回全样本。
        assert!(
            store
                .slot_mods_of_kind(LEAGUE, VERSION, None, None, None, Some("Nobody"), 0.0)
                .expect("read")
                .is_empty()
        );

        // 老签名一个字没变:它问的还是全样本。
        assert_eq!(
            store
                .slot_mods(LEAGUE, VERSION, None, None, 0.0)
                .expect("read"),
            whole
        );
    }

    /// 职业下拉的选项:这一版统计里有哪些职业,各自采到了几个人。
    #[test]
    fn mod_classes_lists_the_sampled_classes_with_their_counts() {
        let store = store();
        assert!(store.mod_classes(LEAGUE, VERSION).expect("read").is_empty());

        let of_class = |class: &str| SlotModStat {
            class: class.to_owned(),
            ..stat("Ring", "Rare", "base_maximum_life", 10, 20)
        };
        store
            .replace_item_mods(
                LEAGUE,
                VERSION,
                &[
                    of_class(""),
                    of_class("Deadeye"),
                    of_class("Gemling Legionnaire"),
                ],
            )
            .expect("replace");

        // 人数从角色表来:两个 Deadeye 抓到手,一个还在队列里不算数。
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        let mut people = vec![
            sampled("a-1", "One", PartitionTier::Whole),
            sampled("b-2", "Two", PartitionTier::Whole),
            sampled("c-3", "Three", PartitionTier::Whole),
            sampled("d-4", "Four", PartitionTier::Whole),
        ];
        people[0].class = "Deadeye".to_owned();
        people[1].class = "Deadeye".to_owned();
        people[2].class = "Deadeye".to_owned();
        people[3].class = "Gemling Legionnaire".to_owned();
        store
            .complete_partition(LEAGUE, VERSION, "", 4, &[], &people, 1_000)
            .expect("complete");
        for (account, name) in [("a-1", "One"), ("b-2", "Two"), ("d-4", "Four")] {
            store
                .complete_character(LEAGUE, account, name, VERSION, "{}", 1_000)
                .expect("done");
        }

        assert_eq!(
            store.mod_classes(LEAGUE, VERSION).expect("read"),
            vec![
                ("Deadeye".to_owned(), 2),
                ("Gemling Legionnaire".to_owned(), 1),
            ],
            "全样本那一档不是一个职业,人多的排前面"
        );

        // 另一版快照的统计里还没有职业行,下拉就该是空的。
        assert!(
            store
                .mod_classes(LEAGUE, "other-version")
                .expect("read")
                .is_empty()
        );
    }

    /// 游戏里那句话要跟着行一起存,不然界面每次都得回头翻角色原文重算一遍。
    #[test]
    fn a_mod_row_round_trips_its_in_game_text() {
        let store = store();
        let low = SlotModStat {
            stat_id: "local_minimum_added_fire_damage".to_owned(),
            display: "Adds # to # Fire Damage".to_owned(),
            value_index: 1,
            ..stat("Weapon", "Rare", "x", 10, 20)
        };
        let high = SlotModStat {
            stat_id: "local_maximum_added_fire_damage".to_owned(),
            value_index: 2,
            ..low.clone()
        };
        // 配不上的行照样要存得下:空串 + 0。
        let blank = SlotModStat {
            stat_id: "cannot_be_frozen".to_owned(),
            display: String::new(),
            value_index: 0,
            ..low.clone()
        };
        store
            .replace_item_mods(LEAGUE, VERSION, &[low, high, blank])
            .expect("replace");

        let rows = store
            .slot_mods(LEAGUE, VERSION, None, None, 0.0)
            .expect("read");
        let of = |stat_id: &str| {
            rows.iter()
                .find(|row| row.stat_id == stat_id)
                .unwrap_or_else(|| panic!("no row for {stat_id}"))
                .clone()
        };
        assert_eq!(
            (
                of("local_minimum_added_fire_damage").display,
                of("local_minimum_added_fire_damage").value_index
            ),
            ("Adds # to # Fire Damage".to_owned(), 1)
        );
        assert_eq!(of("local_maximum_added_fire_damage").value_index, 2);
        assert_eq!(
            (
                of("cannot_be_frozen").display,
                of("cannot_be_frozen").value_index
            ),
            (String::new(), 0)
        );
    }

    /// 已经升过一次(有 `class`)但还没有显示文本的库也得能再升一次。
    ///
    /// 老行没有那两列,读它会直接报错;而这张表是从角色原文算出来的派生数据,
    /// 整张删掉、下一个检查点重建即可,一个网络请求都不用发。
    #[test]
    fn a_cache_without_the_display_columns_gets_rebuilt() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE ninja_item_mods (
                 league_url TEXT NOT NULL,
                 version TEXT NOT NULL,
                 class TEXT NOT NULL DEFAULT '',
                 slot TEXT NOT NULL,
                 rarity TEXT NOT NULL,
                 mod_kind TEXT NOT NULL,
                 stat_id TEXT NOT NULL,
                 mod_family TEXT NOT NULL,
                 characters INTEGER NOT NULL,
                 occurrences INTEGER NOT NULL,
                 sample_size INTEGER NOT NULL,
                 p25 REAL,
                 p50 REAL,
                 p75 REAL,
                 PRIMARY KEY (league_url, version, class, slot, rarity, mod_kind, stat_id)
             ) STRICT;
             INSERT INTO ninja_item_mods VALUES
                 ('forbiddenrites', '1508-20260906-55820', '', 'Ring', 'Rare', 'explicit',
                  'base_maximum_life', 'IncreasedLife', 900, 901, 1000, 95.0, 115.0, 135.0);",
        )
        .expect("class-era schema");

        let store = NinjaStore::initialize(conn).expect("upgrade");
        assert!(
            store
                .slot_mods(LEAGUE, VERSION, None, None, 0.0)
                .expect("read")
                .is_empty(),
            "老行没有显示文本这两列,留着只会是一半新一半旧"
        );

        // 重建出来的表装得下新字段。
        store
            .replace_item_mods(
                LEAGUE,
                VERSION,
                &[stat("Ring", "Rare", "base_maximum_life", 9, 20)],
            )
            .expect("replace");
        assert_eq!(
            store
                .slot_mods(LEAGUE, VERSION, None, None, 0.0)
                .expect("read")[0]
                .display,
            "+# to maximum Life"
        );

        // 升级那一步每次开库都走一遍,第二遍必须是空操作。
        drop_stale_item_mods(&store.conn).expect("second run");
        assert_eq!(
            store
                .slot_mods(LEAGUE, VERSION, None, None, 0.0)
                .expect("read")
                .len(),
            1
        );
    }

    /// 2026-09-07 之前建的库必须能原地升级。
    ///
    /// 这一列进了主键,而 SQLite 改不动已有表的主键:不整张删掉重建,
    /// 十几个职业的行会全撞进同一个键,词缀页看着有数据,其实只剩最后一个
    /// 职业那份。删得起——统计是从详情原文算出来的,下一个检查点就重建。
    #[test]
    fn an_old_cache_drops_its_class_less_mod_stats() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE ninja_item_mods (
                 league_url TEXT NOT NULL,
                 version TEXT NOT NULL,
                 slot TEXT NOT NULL,
                 rarity TEXT NOT NULL,
                 mod_kind TEXT NOT NULL,
                 stat_id TEXT NOT NULL,
                 mod_family TEXT NOT NULL,
                 characters INTEGER NOT NULL,
                 occurrences INTEGER NOT NULL,
                 sample_size INTEGER NOT NULL,
                 p25 REAL,
                 p50 REAL,
                 p75 REAL,
                 PRIMARY KEY (league_url, version, slot, rarity, mod_kind, stat_id)
             ) STRICT;
             INSERT INTO ninja_item_mods VALUES
                 ('forbiddenrites', '1508-20260906-55820', 'Ring', 'Rare', 'explicit',
                  'base_maximum_life', 'IncreasedLife', 900, 901, 1000, 95.0, 115.0, 135.0);",
        )
        .expect("old schema");

        let store = NinjaStore::initialize(conn).expect("upgrade");
        assert!(
            store
                .slot_mods(LEAGUE, VERSION, None, None, 0.0)
                .expect("read")
                .is_empty(),
            "老行没有职业这一维,留着只会是一半新一半旧"
        );

        // 新形状是能装下每个职业的:同一条词缀,三个职业三行。
        let of_class = |class: &str| SlotModStat {
            class: class.to_owned(),
            ..stat("Ring", "Rare", "base_maximum_life", 10, 20)
        };
        store
            .replace_item_mods(
                LEAGUE,
                VERSION,
                &[of_class(""), of_class("Deadeye"), of_class("Warbringer")],
            )
            .expect("replace");
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM ninja_item_mods", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 3, "职业没进主键的话这三行会撞成一行");

        // 升级那一步每次开库都走一遍,第二遍必须是空操作 —— 不然每次开程序
        // 都把统计清一次。
        drop_stale_item_mods(&store.conn).expect("second run");
        assert_eq!(
            store
                .slot_mods_of_kind(LEAGUE, VERSION, None, None, None, Some("Deadeye"), 0.0)
                .expect("read")
                .len(),
            1
        );
    }

    #[test]
    fn unique_prices_round_trip_per_type() {
        let store = store();
        assert_eq!(store.unique_prices_age(LEAGUE).expect("age"), None);
        assert_eq!(
            store.unique_price(LEAGUE, "Berek's Grip").expect("read"),
            None
        );

        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueAccessories",
                // 两类故意用不同的基准币:这一列是**每行**的事实,不是全表一个常数。
                "exalted",
                &[
                    price_line("Berek's Grip", "Two-Stone Ring", 240.5, 24),
                    price_line("Yoke of Suffering", "Bloodstone Amulet", 120.0, 36),
                ],
                9_000,
            )
            .expect("replace");
        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueWeapons",
                "divine",
                &[price_line(
                    "Wake of Destruction",
                    "Wrapped Greathelm",
                    3.25,
                    900,
                )],
                9_500,
            )
            .expect("replace");

        let berek = store
            .unique_price(LEAGUE, "Berek's Grip")
            .expect("read")
            .expect("row");
        assert_eq!(
            berek,
            UniquePriceRow {
                type_name: "UniqueAccessories".to_owned(),
                name: "Berek's Grip".to_owned(),
                base_type: "Two-Stone Ring".to_owned(),
                category: "Ring".to_owned(),
                // 千分整数,不存浮点;单位就是下面那一格。
                primary_value_milli: 240_500,
                primary_currency: "exalted".to_owned(),
                listing_count: 24,
                total_change: Some(-4.5),
                fetched_at: 9_000,
            }
        );
        let wake = store
            .unique_price(LEAGUE, "Wake of Destruction")
            .expect("read")
            .expect("row");
        assert_eq!(wake.primary_value_milli, 3_250);
        assert_eq!(
            wake.primary_currency, "divine",
            "另一类是另一个基准币,不该被隔壁那一类盖掉"
        );

        // 整张表有多旧,看最旧的那一类。
        assert_eq!(store.unique_prices_age(LEAGUE).expect("age"), Some(9_000));

        // 换一类价格,不该动到别的类。
        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueAccessories",
                "exalted",
                &[price_line("Berek's Grip", "Two-Stone Ring", 300.0, 30)],
                10_000,
            )
            .expect("replace again");
        let berek = store
            .unique_price(LEAGUE, "Berek's Grip")
            .expect("read")
            .expect("row");
        assert_eq!(berek.primary_value_milli, 300_000);
        assert_eq!(berek.fetched_at, 10_000);
        assert_eq!(
            store
                .unique_price(LEAGUE, "Yoke of Suffering")
                .expect("read"),
            None,
            "同一类里没再出现的行跟着被清掉"
        );
        assert!(
            store
                .unique_price(LEAGUE, "Wake of Destruction")
                .expect("read")
                .is_some(),
            "别的分类不受影响"
        );
        assert_eq!(store.unique_prices_age(LEAGUE).expect("age"), Some(9_500));
    }

    /// 一整周只有一个数据点时,7 天那一列存进去的是 NULL,不是 0。
    ///
    /// 新联赛开头几天,poe.ninja 给的 `data` 是 `[null × 6, 0]` —— 它自己也
    /// 只能写 `totalChange: 0`。照单存下来的话,榜上四百多件暗金全是 `+0%`,
    /// 看着像"这周整个市场纹丝不动",而真相是"还没有一周的数据"。
    #[test]
    fn a_unique_without_a_weeks_history_stores_no_seven_day_change() {
        let store = store();
        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueWeapons",
                "divine",
                &[
                    // 线上那 121 行的形状:六个 null + 今天的 0。
                    priced(
                        "The Ordained",
                        "Grand Spear",
                        53.0,
                        97,
                        "[null,null,null,null,null,null,0]",
                        0.0,
                    ),
                    // 线上那 19 行的形状:四天前一个点,今天一个点。
                    priced(
                        "Trenchtimbre",
                        "Spiked Club",
                        0.085_48,
                        1_509,
                        "[null,null,null,0,null,null,-99.53]",
                        -99.53,
                    ),
                ],
                9_000,
            )
            .expect("replace");

        assert_eq!(
            store
                .unique_price(LEAGUE, "The Ordained")
                .expect("read")
                .expect("row")
                .total_change,
            None,
            "没有一周的历史就该是空的,不是 0%"
        );
        assert_eq!(
            store
                .unique_price(LEAGUE, "Trenchtimbre")
                .expect("read")
                .expect("row")
                .total_change,
            Some(-99.53),
            "真的跌了 99.53% 的那条要原样保留,不许四舍五入成 -100"
        );
    }

    /// 2026-09-07 从线上 `UniqueWeapons` 剪下来的原文,和 `pnd-ninja` 用的是
    /// 同一份文件 —— 单位这件事只有拿真原文才试得出来。
    const UNIQUE_WEAPONS: &str =
        include_str!("../../pnd-ninja/fixtures/unique_weapons_overview.json");

    /// 价格的单位是**这一份 overview 自己说的**,所以它得跟着价格一起进库。
    ///
    /// 不存的话,`0.08548` 到了界面上就只是一个裸数字,只能靠一句写死的
    /// "它是 exalted" 去猜 —— 那句话 2026-09-06 是对的,2026-09-07 就成了
    /// 近百倍的错。
    #[test]
    fn a_stored_price_carries_the_base_currency_the_feed_declared() {
        let store = store();
        let overview: pnd_ninja::economy::ItemOverview =
            serde_json::from_str(UNIQUE_WEAPONS).expect("fixture");
        assert_eq!(overview.core.primary, "divine");

        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueWeapons",
                &overview.core.primary,
                &overview.lines,
                9_000,
            )
            .expect("replace");

        let trenchtimbre = store
            .unique_price(LEAGUE, "Trenchtimbre")
            .expect("read")
            .expect("row");
        assert_eq!(trenchtimbre.primary_value_milli, 85);
        assert_eq!(trenchtimbre.primary_currency, "divine");

        // 榜上那一行也要带着单位:界面读的是这个 JOIN,不是上面那条单行查询。
        store
            .enqueue_partitions(LEAGUE, VERSION, &partitions())
            .expect("enqueue");
        store
            .complete_partition(
                LEAGUE,
                VERSION,
                "",
                61_390,
                &[("items".to_owned(), "Trenchtimbre".to_owned(), 1_509u64)],
                &[],
                5_000,
            )
            .expect("complete");
        let rows = store
            .unique_usage_with_prices(LEAGUE, VERSION, "")
            .expect("usage");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].price_milli, Some(85));
        assert_eq!(rows[0].price_currency.as_deref(), Some("divine"));
    }

    /// 同名不同底子时挑挂单多的那条——挂单少的那条不该当市价。
    #[test]
    fn the_busiest_listing_wins_when_names_collide() {
        let store = store();
        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueAccessories",
                "divine",
                &[
                    price_line("Berek's Grip", "Coral Ring", 10.0, 2),
                    price_line("Berek's Grip", "Two-Stone Ring", 240.0, 24),
                ],
                9_000,
            )
            .expect("replace");
        assert_eq!(
            store
                .unique_price(LEAGUE, "Berek's Grip")
                .expect("read")
                .expect("row")
                .base_type,
            "Two-Stone Ring"
        );
    }
}
