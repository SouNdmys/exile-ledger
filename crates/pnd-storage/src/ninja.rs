//! `ninja.sqlite`:poe.ninja 采样的工作台。
//!
//! 这个库和 `watch.sqlite` 的性质完全不同:**它随手删掉也不心疼**。里面全是能
//! 重新抓回来的东西,存下来只为两件事——
//!
//! 1. **断点续跑。** 一轮完整采样是 ~65 次分区搜索 + 2,000 次角色详情(1 秒一个,
//!    大半个小时)。程序关掉、断网、换快照,下次开起来必须接着上次跑,
//!    而不是从头再来一遍。所以每个分区、每个角色都是一行带 `status` 的工作单。
//! 2. **一天只打扰 poe.ninja 一次。** 快照 `version` 一天变好几次,但我们 24 小时
//!    内不重跑;界面上翻来翻去看的都是库里这份缓存,零网络请求。
//!
//! 纪律和 `watch.rs` 一样:**库里没有时钟**,所有时间都是调用方传进来的 unix 秒;
//! 金额存千分整数(暗金参考价 `primary_value_milli` 是 exalted × 1000)。
//!
//! 角色表的主键是 `(league_url, account, name)` 而**不带 version**:同一个人在
//! 新快照里还是同一个人,重跑时只补新面孔,已经抓过的不再花那一秒。

use std::path::Path;
use std::time::Duration;

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

CREATE TABLE IF NOT EXISTS ninja_unique_prices (
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
"#;

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
/// 价格那三格是 `Option`:没有价格的暗金照样要上榜(新出的、或者压根没人挂单),
/// 只是价格那几列写"—"。
#[derive(Debug, Clone, PartialEq)]
pub struct UniqueUsagePriced {
    pub name: String,
    /// 这个分区里有多少个角色穿着它。
    pub users: u64,
    /// 参考价,exalted × 1000。
    pub price_milli: Option<i64>,
    pub listings: Option<i64>,
    /// 7 天涨跌,百分比。价格表里有这件东西、但 `sparkLine` 是空的时候也是 `None`。
    pub change_percent: Option<f64>,
}

/// 一件暗金的参考价快照。价格单位是 exalted × 1000。
#[derive(Debug, Clone, PartialEq)]
pub struct UniquePriceRow {
    pub type_name: String,
    pub name: String,
    pub base_type: String,
    pub category: String,
    pub primary_value_milli: i64,
    pub listing_count: i64,
    pub total_change: Option<f64>,
    pub fetched_at: i64,
}

pub struct NinjaStore {
    conn: Connection,
}

impl NinjaStore {
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
        conn.execute_batch(NINJA_SCHEMA)?;
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
                     (league_url, version, slot, rarity, mod_kind, stat_id, mod_family,
                      characters, occurrences, sample_size, p25, p50, p75)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for stat in stats {
                insert.execute(params![
                    league_url,
                    version,
                    stat.slot,
                    stat.rarity,
                    stat.mod_kind,
                    stat.stat_id,
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
        self.slot_mods_of_kind(league_url, version, slot, rarity, None, min_share_percent)
    }

    /// 同一张表,再多一个词缀类型的筛子(`explicit` / `implicit` / `rune` / …)。
    ///
    /// 类型是主键的一维,所以在库里筛比读回来再筛便宜。分成一个兄弟函数而不是
    /// 给 [`slot_mods`](Self::slot_mods) 加参数:那个签名有三个调用方,
    /// 其中大部分本来就不关心类型。
    pub fn slot_mods_of_kind(
        &self,
        league_url: &str,
        version: &str,
        slot: Option<&str>,
        rarity: Option<&str>,
        mod_kind: Option<&str>,
        min_share_percent: f64,
    ) -> Result<Vec<SlotModStat>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT slot, rarity, mod_kind, stat_id, mod_family,
                    characters, occurrences, sample_size, p25, p50, p75
             FROM ninja_item_mods
             WHERE league_url = ?1 AND version = ?2
               AND (?3 IS NULL OR slot = ?3)
               AND (?4 IS NULL OR rarity = ?4)
               AND (?5 IS NULL OR mod_kind = ?5)
               AND CAST(characters AS REAL) * 100.0 / MAX(sample_size, 1) >= ?6
             ORDER BY slot, rarity, mod_kind, characters DESC, stat_id",
        )?;
        let rows = statement.query_map(
            params![
                league_url,
                version,
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
            "SELECT facets.entry, facets.count,
                    prices.primary_value_milli, prices.listing_count, prices.total_change
             FROM ninja_facets AS facets
             LEFT JOIN (
                 SELECT name, primary_value_milli, listing_count, total_change,
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
                listings: row.get(3)?,
                change_percent: row.get(4)?,
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
    pub fn replace_unique_prices(
        &self,
        league_url: &str,
        type_name: &str,
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
                      primary_value_milli, listing_count, total_change)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
                    line.listing_count,
                    line.spark_line.total_change,
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
                        listing_count, total_change, fetched_at
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

/// exalted 的浮点参考价 → 千分整数。非有限值(接口给了 NaN/Infinity)记 0,
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
    let characters: i64 = row.get(5)?;
    let occurrences: i64 = row.get(6)?;
    let sample_size: i64 = row.get(7)?;
    Ok(SlotModStat {
        slot: row.get(0)?,
        rarity: row.get(1)?,
        mod_kind: row.get(2)?,
        stat_id: row.get(3)?,
        mod_family: row.get(4)?,
        characters: to_u32(characters),
        occurrences: to_u32(occurrences),
        sample_size: to_u32(sample_size),
        p25: row.get(8)?,
        p50: row.get(9)?,
        p75: row.get(10)?,
    })
}

fn unique_price_from_row(row: &Row<'_>) -> rusqlite::Result<UniquePriceRow> {
    Ok(UniquePriceRow {
        type_name: row.get(0)?,
        name: row.get(1)?,
        base_type: row.get(2)?,
        category: row.get(3)?,
        primary_value_milli: row.get(4)?,
        listing_count: row.get(5)?,
        total_change: row.get(6)?,
        fetched_at: row.get(7)?,
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
            slot: slot.to_owned(),
            rarity: rarity.to_owned(),
            mod_kind: "explicit".to_owned(),
            stat_id: stat_id.to_owned(),
            mod_family: "IncreasedLife".to_owned(),
            characters,
            occurrences: characters + 1,
            sample_size: sample,
            p25: Some(95.0),
            p50: Some(115.0),
            p75: Some(135.0),
        }
    }

    fn price_line(name: &str, base_type: &str, value: f64, listings: i64) -> UniquePriceLine {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","baseType":"{base_type}","category":"Ring",
                 "primaryValue":{value},"listingCount":{listings},
                 "sparkLine":{{"totalChange":-4.5,"data":[]}}}}"#
        ))
        .expect("line")
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
                    listings: Some(900),
                    change_percent: Some(-4.5),
                },
                UniqueUsagePriced {
                    name: "Berek's Grip".to_owned(),
                    users: 6_413,
                    price_milli: Some(240_000),
                    listings: Some(24),
                    change_percent: Some(-4.5),
                },
                UniqueUsagePriced {
                    name: "Beira's Anguish".to_owned(),
                    users: 2_000,
                    price_milli: None,
                    listings: None,
                    change_percent: None,
                },
            ],
            "人多的在前;没挂单的照样上榜,只是价格三格空着"
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
            .slot_mods_of_kind(LEAGUE, VERSION, None, None, Some("explicit"), 0.0)
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
            .slot_mods_of_kind(LEAGUE, VERSION, None, None, Some("rune"), 0.0)
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
                2.0,
            )
            .expect("read");
        assert_eq!(common.len(), 2, "占比 1% 的那条被阈值挡掉");

        // 不认识的类型给空,而不是悄悄退回全部。
        assert!(
            store
                .slot_mods_of_kind(LEAGUE, VERSION, None, None, Some("enchant"), 0.0)
                .expect("read")
                .is_empty()
        );

        // `None` 就是不筛:和老的 `slot_mods` 一字不差。
        assert_eq!(
            store
                .slot_mods_of_kind(LEAGUE, VERSION, None, None, None, 0.0)
                .expect("read"),
            store
                .slot_mods(LEAGUE, VERSION, None, None, 0.0)
                .expect("read")
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
                // exalted × 1000,不存浮点。
                primary_value_milli: 240_500,
                listing_count: 24,
                total_change: Some(-4.5),
                fetched_at: 9_000,
            }
        );
        assert_eq!(
            store
                .unique_price(LEAGUE, "Wake of Destruction")
                .expect("read")
                .expect("row")
                .primary_value_milli,
            3_250
        );

        // 整张表有多旧,看最旧的那一类。
        assert_eq!(store.unique_prices_age(LEAGUE).expect("age"), Some(9_000));

        // 换一类价格,不该动到别的类。
        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueAccessories",
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

    /// 同名不同底子时挑挂单多的那条——挂单少的那条不该当市价。
    #[test]
    fn the_busiest_listing_wins_when_names_collide() {
        let store = store();
        store
            .replace_unique_prices(
                LEAGUE,
                "UniqueAccessories",
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
