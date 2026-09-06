//! poe.ninja 采样管线:一条自己管自己的工作线程,把"分区搜索 → 角色详情 →
//! 词缀统计"整轮跑完,中途随时可以被杀掉、下次接着跑。
//!
//! 和 [`crate::actor`] 的关系是**平行**的:采样不碰交易站,也不需要 actor 手里那些
//! 会动的状态,所以它不挤进那条串行队列,而是自己一条线程、自己一个 `NinjaStore`
//! 连接。app(或者探针)直接拿 [`SamplerHandle`] 用,像用一个后台任务一样。
//!
//! 全流程七步(和计划里的"ninja 管线"一一对应):
//!
//! 1. `refresh_hours` 内已经跑到目标阶段就跳过(`--force` 除外);跑了一半的那轮
//!    **不看年龄,直接接着跑**——半份数据对谁都没用。
//! 2. 读 index-state / build-index-state,落一行快照。
//! 3. NDIC 字典按 sha1 在内存里缓存:9 个分面共用 7 张表,一轮只抓 7 次。
//! 4. 跑完所有 pending 分区(每个分区一次搜索 + 一个事务)。热门暗金榜到这里就有了。
//! 5. 逐个抓角色详情,原文整段落库(`stop_after >= Characters` 才跑)。
//! 6. 从原文重建词缀统计(`stop_after == Aggregated` 才跑)。
//! 7. 暗金参考价(6 个有文档的经济接口)。**每一轮都跑,而且排在最后**:
//!    6 个请求、6 秒钟,却能让"这件暗金现在值多少"跟着刚采完的人气榜一起新。
//!
//! 阶段只许前进:第 4 步在续跑时会空跑一遍(队列是空的,一个请求都不发),
//! 库里的阶段不能因此从 `characters` 退回 `facets`(见 [`highest_stage`])。
//!
//! 对 poe.ninja 的礼貌全压在一个 [`Pacer`] 上:**所有**出站请求都从它过一遍,
//! 保证两次请求的**起点**至少隔 `min_request_gap_ms`(默认 1 秒),
//! 每次请求前看一眼取消标志,429/5xx 睡 60 秒再试一次就不再纠缠。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pnd_ninja::aggregate::aggregate_mods;
use pnd_ninja::character::CharacterDetail;
use pnd_ninja::client::{NinjaClient, NinjaError};
use pnd_ninja::economy::UNIQUE_TYPES;
use pnd_ninja::index_state::{IndexState, LeagueBuild};
use pnd_ninja::plan::{
    Partition, PartitionTier, SampleOptions, SampledCharacter, class_skill_partitions,
    first_pass_partitions, query_from_key,
};
use pnd_ninja::search::{SearchResponse, dictionary_key_for_facet};
use pnd_settings::NinjaTuning;
use pnd_storage::{NinjaStore, SnapshotRow, SnapshotStage, StorageError};
use thiserror::Error;

use crate::now_secs;

/// 撞上 429 或者 5xx 之后先躺多久。一分钟是"明显在道歉"的量级:
/// 对面缓存 30 分钟,我们急这一分钟没有任何意义。
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// 睡觉时每隔这么久醒一次看取消标志。关程序不该等满一个 60 秒的退避。
const CANCEL_SLICE: Duration = Duration::from_millis(100);

/// 角色详情每抓这么多个发一次进度。2,000 个角色发 200 条事件,界面够用又不刷屏。
const CHARACTER_PROGRESS_EVERY: usize = 10;

// ---------------------------------------------------------------------
// 对外的类型
// ---------------------------------------------------------------------

/// 这一轮打算跑到哪一步。
///
/// 和 `pnd-storage` 的 [`SnapshotStage`] 是同一套阶梯,单独定义一份是因为
/// 调用方(app / 探针)不该为了说一句"只跑到分面"就去 use 存储层的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SamplerStage {
    /// 分区清单排好了,还没开跑。
    #[default]
    Planned,
    /// 分区搜索跑完了:热门暗金榜可以看了(第一版界面要的就是这一步)。
    Facets,
    /// 角色详情抓完了。
    Characters,
    /// 词缀统计重建完了:整轮到此为止。
    Aggregated,
}

/// 阶段的先后。**只用它比大小**,不给 `SamplerStage` 派生 `Ord`——
/// 派生出来的顺序跟着枚举的书写顺序走,哪天有人插一个变体进去就悄悄错了。
#[must_use]
pub fn stage_order(stage: SamplerStage) -> u8 {
    match stage {
        SamplerStage::Planned => 0,
        SamplerStage::Facets => 1,
        SamplerStage::Characters => 2,
        SamplerStage::Aggregated => 3,
    }
}

/// 两个阶段里靠后的那个。
///
/// 存在的理由是**阶段只许前进**:续跑一轮已经跑到 `characters` 的快照时,
/// 分区那一步照样会跑一遍(队列是空的,一个请求都不发),跑完顺手报一句
/// "分面完成"。如果照着这句话写库,库里的阶段就从 `characters` 倒退回
/// `facets` —— 这时候被杀掉,下一轮就会以为角色详情还没开始。
#[must_use]
pub fn highest_stage(left: SamplerStage, right: SamplerStage) -> SamplerStage {
    if stage_order(right) > stage_order(left) {
        right
    } else {
        left
    }
}

impl SamplerStage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.to_snapshot().as_str()
    }

    #[must_use]
    pub fn to_snapshot(self) -> SnapshotStage {
        match self {
            SamplerStage::Planned => SnapshotStage::Planned,
            SamplerStage::Facets => SnapshotStage::Facets,
            SamplerStage::Characters => SnapshotStage::Characters,
            SamplerStage::Aggregated => SnapshotStage::Aggregated,
        }
    }

    #[must_use]
    pub fn from_snapshot(stage: SnapshotStage) -> SamplerStage {
        match stage {
            SnapshotStage::Planned => SamplerStage::Planned,
            SnapshotStage::Facets => SamplerStage::Facets,
            SnapshotStage::Characters => SamplerStage::Characters,
            SnapshotStage::Aggregated => SamplerStage::Aggregated,
        }
    }

    /// 认不出来的当 `Facets`:那是界面真正需要的那一步,也是最省事的默认。
    #[must_use]
    pub fn parse(raw: &str) -> SamplerStage {
        match raw {
            "planned" => SamplerStage::Planned,
            "characters" => SamplerStage::Characters,
            "aggregated" => SamplerStage::Aggregated,
            _ => SamplerStage::Facets,
        }
    }
}

/// 跑一轮采样要知道的全部东西。
#[derive(Debug, Clone)]
pub struct SamplerConfig {
    /// builds 接口用的联赛短名,例如 `forbiddenrites`。
    pub league_url: String,
    /// 经济接口用的联赛显示名,例如 `Forbidden Rites`。两个接口要的就是不同的东西。
    pub league_name: String,
    pub tuning: NinjaTuning,
    /// 设置里算出来的 User-Agent。
    ///
    /// 今天 [`NinjaClient`] 自己带一份写死的、带联系方式的 UA
    /// (`pnd_ninja::USER_AGENT`),所以这个字段暂时只是把 app 的设置原样记下来;
    /// 等客户端支持自定义 UA 时,这里一行就能接上,调用方不用改。
    pub user_agent: String,
    pub db_path: PathBuf,
    /// 跑到哪一步停。第一版界面用 [`SamplerStage::Facets`],词缀热度页要
    /// [`SamplerStage::Aggregated`]。
    pub stop_after: SamplerStage,
    /// 无视"24 小时内不重跑"。
    pub force: bool,
    /// 这一轮最多抓几个角色详情。`None` = 按 `tuning.sample_target` 补齐。
    /// 探针的 `--sample --limit 30` 就是靠它只花 30 秒。
    pub max_characters: Option<u32>,
}

impl SamplerConfig {
    /// 常用的一组:盯 Forbidden Rites、跑到分面、不强制、默认库。
    #[must_use]
    pub fn new(league_url: impl Into<String>, league_name: impl Into<String>) -> SamplerConfig {
        SamplerConfig {
            league_url: league_url.into(),
            league_name: league_name.into(),
            tuning: NinjaTuning::default(),
            user_agent: String::new(),
            db_path: pnd_storage::default_ninja_db_path(),
            stop_after: SamplerStage::Facets,
            force: false,
            max_characters: None,
        }
    }
}

/// 采样线程往外播的一切。界面照着画进度条,探针照着打印。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SamplerEvent {
    Started {
        version: String,
        snapshot_name: String,
        /// build-index-state 报的联赛总人数,用来算"我们采了百分之几"。
        total_characters: u64,
    },
    /// 这一轮什么都没做。`reason` 是给人看的("6h12m 前刚跑过")。
    Skipped {
        reason: String,
    },
    Progress {
        stage: SamplerStage,
        done: u32,
        total: u32,
        note: String,
    },
    StageDone(SamplerStage),
    /// 暗金参考价刷新了几类(一共 6 类)。
    Prices {
        types_done: u32,
    },
    Finished {
        version: String,
    },
    Failed(String),
}

#[derive(Debug, Error)]
pub enum SamplerError {
    #[error("the sampler was cancelled")]
    Cancelled,
    #[error("poe.ninja: {0}")]
    Ninja(#[from] NinjaError),
    #[error("ninja.sqlite: {0}")]
    Storage(#[from] StorageError),
    /// index-state 或者 build-index-state 里没有这个联赛短名——多半是名字打错了。
    #[error("poe.ninja has no build league called {0}")]
    UnknownLeague(String),
    #[error("could not start the sampler thread: {0}")]
    Spawn(String),
}

// ---------------------------------------------------------------------
// 纯函数(可以在测试里把几十个小时钉死时间跑完)
// ---------------------------------------------------------------------

/// 这一轮该不该直接跳过?返回 `Some(理由)` 就是跳过。
///
/// 两条规则:
/// - **没跑到目标阶段的那轮不跳过**,而且不看年龄。跑了一半的快照留在库里
///   就是为了被接着跑完,拿"太新了"当理由把它晾着,它永远也跑不完。
/// - 跑到了目标阶段,就看这轮数据有多老:`finished_at`(只有整轮跑完才有)
///   优先,没有就用 `started_at`。
#[must_use]
pub fn should_skip(
    latest: Option<&SnapshotRow>,
    stop_after: SamplerStage,
    refresh_hours: u32,
    now: i64,
) -> Option<String> {
    let row = latest?;
    let stage = SamplerStage::from_snapshot(row.stage);
    if stage_order(stage) < stage_order(stop_after) {
        return None;
    }
    let at = row.finished_at.unwrap_or(row.started_at);
    let age = now.saturating_sub(at);
    let window = i64::from(refresh_hours) * 3_600;
    (age < window).then(|| {
        format!(
            "snapshot {} is already at stage {} from {}h{:02}m ago (refresh window {}h)",
            row.version,
            stage.as_str(),
            age / 3_600,
            (age % 3_600) / 60,
            refresh_hours
        )
    })
}

/// 这一轮最多再抓几个角色详情。
///
/// 两个上限取小的那个:`sample_target` 是"这个联赛一共要采多少人"(已经抓到手的
/// 要扣掉),`max_characters` 是"这一次最多花多少时间"(探针的 `--limit 30`)。
/// 只看后者的话,`--sample --limit 5000` 会一路抓到天亮,把 2,000 这个目标架空。
#[must_use]
pub fn character_limit(sample_target: u32, already_done: u32, max_characters: Option<u32>) -> u32 {
    let remaining = sample_target.saturating_sub(already_done);
    max_characters.map_or(remaining, |cap| cap.min(remaining))
}

/// 这一轮该按哪个联赛短名去问 poe.ninja。
///
/// 界面手上只有 `settings.json` 里那个显示名(`Forbidden Rites`),短名是
/// [`league_url_guess`](pnd_ninja::index_state::league_url_guess) 猜的。但
/// index-state 每条快照都同时带着显示名和短名 —— **手上有它就别再猜**:
/// 猜法只是规律,没有任何接口承诺过,猜错一次的代价是整轮采样以
/// `UnknownLeague` 收场,而正确答案就在刚读回来的那份 JSON 里。
fn resolve_league_url(index: &IndexState, league_name: &str, guessed: &str) -> String {
    index
        .league_url_for_name(league_name)
        .unwrap_or(guessed)
        .to_owned()
}

/// 距离下一次请求还该等多久。
///
/// 节流看的是两次请求的**起点**而不是"上一次回来之后再等一秒":后者会让
/// 每个慢请求都额外赔上它自己的耗时,一轮 2,000 个角色能白等十几分钟。
#[must_use]
pub fn pace_delay(last_start: Option<Instant>, gap: Duration) -> Duration {
    match last_start {
        None => Duration::ZERO,
        Some(last) => gap.saturating_sub(last.elapsed()),
    }
}

/// 一个分区的响应 → 可以直接进 `ninja_facets` 的行。
///
/// **所有**分面都存,不只是 `items`:职业/技能/关键天赋那几张表是同一次请求
/// 白送的,存下来界面就能离线换着看,重抓一次反而是对 poe.ninja 不客气。
fn facet_rows(
    response: &SearchResponse,
    dictionaries: &HashMap<String, Vec<String>>,
) -> Vec<(String, String, u64)> {
    let mut rows = Vec::new();
    for facet in &response.facets {
        let dictionary = dictionary_slice(
            response,
            dictionary_key_for_facet(&facet.name),
            dictionaries,
        );
        for (label, count) in response.resolve_facet(&facet.name, dictionary) {
            if !label.is_empty() {
                rows.push((facet.name.clone(), label, count));
            }
        }
    }
    rows
}

/// 前 100 名角色 → 采样名单。`from_partition` 记的是哪条分区把他带进来的。
fn sampled_characters(
    response: &SearchResponse,
    key: &str,
    tier: PartitionTier,
    class_dictionary: &[String],
) -> Vec<SampledCharacter> {
    response
        .character_refs(class_dictionary)
        .into_iter()
        .filter(|entry| !entry.name.is_empty())
        .map(|entry| SampledCharacter {
            account: entry.account,
            name: entry.name,
            class: entry.class,
            level: entry.level,
            from_partition: key.to_owned(),
            tier,
        })
        .collect()
}

/// 按分面键取字典。取不到就给空表:`resolve_facet` 那时会给出 `#123`,
/// 一眼能看出是字典没跟上,总比整条分面消失强。
fn dictionary_slice<'a>(
    response: &SearchResponse,
    key: &str,
    dictionaries: &'a HashMap<String, Vec<String>>,
) -> &'a [String] {
    response
        .dictionary_sha1(key)
        .and_then(|sha1| dictionaries.get(sha1))
        .map_or(&[], Vec::as_slice)
}

fn sample_options(tuning: &NinjaTuning) -> SampleOptions {
    SampleOptions {
        sample_target: tuning.sample_target,
        min_class_share_percent: tuning.min_class_share_percent,
        skills_per_class: tuning.skills_per_class,
        top_global_skills: tuning.top_global_skills,
        top_uniques: tuning.top_uniques,
    }
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------
// 节流
// ---------------------------------------------------------------------

/// 全流程唯一的出网闸门。
struct Pacer {
    gap: Duration,
    last_start: Option<Instant>,
}

impl Pacer {
    fn new(gap: Duration) -> Pacer {
        Pacer {
            gap,
            last_start: None,
        }
    }

    /// 睡到可以发下一个请求,顺便把取消标志看一遍。
    fn before_request(&mut self, cancel: &AtomicBool) -> Result<(), SamplerError> {
        nap(pace_delay(self.last_start, self.gap), cancel)?;
        check_cancel(cancel)?;
        self.last_start = Some(Instant::now());
        Ok(())
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), SamplerError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(SamplerError::Cancelled);
    }
    Ok(())
}

/// 可以被打断的睡眠。
fn nap(total: Duration, cancel: &AtomicBool) -> Result<(), SamplerError> {
    let deadline = Instant::now() + total;
    loop {
        check_cancel(cancel)?;
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(CANCEL_SLICE));
    }
}

/// 值得再试一次的错:限流和对面自己出问题。404 不在其列——那是"这人没了",
/// 再问一百遍也还是没了。
fn is_transient(error: &NinjaError) -> bool {
    match error {
        NinjaError::Rejected(status) => *status == 429 || *status >= 500,
        _ => false,
    }
}

// ---------------------------------------------------------------------
// 句柄
// ---------------------------------------------------------------------

/// 采样线程的那一头。丢掉它 = 取消 + 等它收摊。
pub struct SamplerHandle {
    events: Receiver<SamplerEvent>,
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SamplerHandle {
    /// 起一条 `pnd-ninja-sampler` 线程跑一轮。
    ///
    /// 不返回 `Result`:线程都起不来的时候,与其让每个调用点写一遍错误处理,
    /// 不如把它当成"这一轮失败了"从事件通道里发出去——反正调用方本来就在读事件。
    #[must_use]
    pub fn start(config: SamplerConfig) -> SamplerHandle {
        let (events_tx, events_rx) = channel::<SamplerEvent>();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let thread_events = events_tx.clone();

        let spawned = thread::Builder::new()
            .name("pnd-ninja-sampler".to_owned())
            .spawn(move || {
                let mut sentinel = FaultOnDrop {
                    events: thread_events.clone(),
                    armed: true,
                };
                let emit = |event: SamplerEvent| {
                    let _ = thread_events.send(event);
                };
                if let Err(error) = run_sampler(&config, &thread_cancel, &emit) {
                    emit(SamplerEvent::Failed(error.to_string()));
                }
                sentinel.armed = false;
            });

        match spawned {
            Ok(join) => SamplerHandle {
                events: events_rx,
                cancel,
                join: Some(join),
            },
            Err(error) => {
                let _ = events_tx.send(SamplerEvent::Failed(
                    SamplerError::Spawn(error.to_string()).to_string(),
                ));
                SamplerHandle {
                    events: events_rx,
                    cancel,
                    join: None,
                }
            }
        }
    }

    /// 取一个事件,没有就是 `None`。界面每 tick 抽干为止。
    #[must_use]
    pub fn try_next_event(&self) -> Option<SamplerEvent> {
        self.events.try_recv().ok()
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.join.as_ref().is_some_and(|join| !join.is_finished())
    }

    /// 请它停。线程最多在一个 100 毫秒的睡眠片里才发现,不会立刻结束。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for SamplerHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 线程要是 panic 了,至少让界面知道一声——否则进度条会永远停在半路。
struct FaultOnDrop {
    events: Sender<SamplerEvent>,
    armed: bool,
}

impl Drop for FaultOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.events.send(SamplerEvent::Failed(
                "the ninja sampler thread stopped unexpectedly".to_owned(),
            ));
        }
    }
}

// ---------------------------------------------------------------------
// 管线本体
// ---------------------------------------------------------------------

/// 同步跑完一轮。探针直接调它,线程版 [`SamplerHandle::start`] 也只是把它包起来。
pub fn run_sampler(
    config: &SamplerConfig,
    cancel: &AtomicBool,
    emit: &dyn Fn(SamplerEvent),
) -> Result<(), SamplerError> {
    let store = NinjaStore::open(&config.db_path)?;
    let latest = store.latest_snapshot(&config.league_url)?;
    if !config.force
        && let Some(reason) = should_skip(
            latest.as_ref(),
            config.stop_after,
            config.tuning.refresh_hours,
            now_secs(),
        )
    {
        emit(SamplerEvent::Skipped { reason });
        return Ok(());
    }

    let mut sampler = Sampler::new(store, config, cancel, emit);
    let league = sampler.open_snapshot(latest.as_ref())?;

    // `Planned` 是个"只把快照行刷新一下"的停车位(存储层的阶段阶梯里有它,
    // 所以配置里也得能表达)。真正的调用方都从 `Facets` 起步。
    if stage_order(config.stop_after) >= stage_order(SamplerStage::Facets) {
        sampler.partition_stage(&league)?;
        sampler.finish_stage(SamplerStage::Facets)?;
    }

    if stage_order(config.stop_after) >= stage_order(SamplerStage::Characters) {
        sampler.character_stage()?;
        sampler.finish_stage(SamplerStage::Characters)?;
    }
    if stage_order(config.stop_after) >= stage_order(SamplerStage::Aggregated) {
        sampler.aggregate_stage()?;
        sampler.finish_stage(SamplerStage::Aggregated)?;
    }

    // 参考价放在最后、而且每一轮都跑:6 个有文档的接口、6 秒钟,
    // 却让"这件暗金现在值多少"跟着最新一次采样一起新。
    sampler.prices_stage()?;

    emit(SamplerEvent::Finished {
        version: sampler.version.clone(),
    });
    Ok(())
}

/// 只跑第 5 步(暗金参考价)。探针的 `--prices` 和界面上的"只刷新价格"用它。
pub fn refresh_prices(
    config: &SamplerConfig,
    cancel: &AtomicBool,
    emit: &dyn Fn(SamplerEvent),
) -> Result<(), SamplerError> {
    let store = NinjaStore::open(&config.db_path)?;
    let mut sampler = Sampler::new(store, config, cancel, emit);
    sampler.prices_stage()
}

/// `--plan` 要的那张单子:这一轮打算问 poe.ninja 哪些问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplerPlan {
    pub version: String,
    pub snapshot_name: String,
    /// build-index-state 报的联赛总人数。
    pub total_characters: u64,
    pub partitions: Vec<Partition>,
}

/// 只把第一轮的分区清单算出来,**不碰磁盘**。
///
/// 实现上借一个内存库跑真正的第一步(读快照 → 全联赛搜索 → `first_pass_partitions`),
/// 而不是在探针里另写一遍近似逻辑:这样 `--plan` 打印的顺序和条数,
/// 就是采样线程真的会跑的那一份。
///
/// `class=X&skills=Y` 那一档不在里面 —— 它要等每个职业自己的分面回来才知道
/// 该问哪些技能,一次全联赛搜索是算不出来的。
pub fn plan_partitions(
    config: &SamplerConfig,
    cancel: &AtomicBool,
    emit: &dyn Fn(SamplerEvent),
) -> Result<SamplerPlan, SamplerError> {
    let store = NinjaStore::open_in_memory()?;
    let mut sampler = Sampler::new(store, config, cancel, emit);
    let league = sampler.open_snapshot(None)?;
    sampler.stage = SamplerStage::Facets;
    let partitions = sampler.first_pass(&league)?;
    Ok(SamplerPlan {
        version: sampler.version.clone(),
        snapshot_name: sampler.snapshot_name.clone(),
        total_characters: league.total,
        partitions,
    })
}

/// 一轮采样跑起来之后的全部家当。捆成一个结构是为了别让每个步骤都拖着
/// 八个参数走(clippy 会念,人读着也累)。
struct Sampler<'a> {
    client: NinjaClient,
    pacer: Pacer,
    store: NinjaStore,
    /// NDIC 按 sha1 缓存:内容不可变,一个会话里抓一次就够。
    dictionaries: HashMap<String, Vec<String>>,
    config: &'a SamplerConfig,
    cancel: &'a AtomicBool,
    emit: &'a dyn Fn(SamplerEvent),
    /// 正在跑哪一步。只用来给进度事件贴标签。
    stage: SamplerStage,
    /// 这个快照**最远**走到过哪一步。库里的阶段只跟着它走,所以只会前进。
    reached: SamplerStage,
    version: String,
    snapshot_name: String,
    /// 经济接口用的联赛显示名。设置里留空时用 index-state 里的那个,
    /// 免得 `--league someotherleague` 拿着 "Forbidden Rites" 去问价格。
    league_name: String,
}

impl<'a> Sampler<'a> {
    fn new(
        store: NinjaStore,
        config: &'a SamplerConfig,
        cancel: &'a AtomicBool,
        emit: &'a dyn Fn(SamplerEvent),
    ) -> Sampler<'a> {
        Sampler {
            client: NinjaClient::new(),
            pacer: Pacer::new(Duration::from_millis(config.tuning.min_request_gap_ms)),
            store,
            dictionaries: HashMap::new(),
            config,
            cancel,
            emit,
            stage: SamplerStage::Planned,
            reached: SamplerStage::Planned,
            version: String::new(),
            snapshot_name: String::new(),
            league_name: config.league_name.clone(),
        }
    }

    // ---- 出网 --------------------------------------------------------

    /// 所有请求的唯一出口:节流 + 取消 + 429/5xx 重试一次。
    fn fetch<T>(
        &mut self,
        call: impl Fn(&NinjaClient) -> Result<T, NinjaError>,
    ) -> Result<T, SamplerError> {
        match self.attempt(&call) {
            Err(SamplerError::Ninja(error)) if is_transient(&error) => {
                self.progress(0, 0, format!("{error} — waiting 60s and retrying once"));
                nap(RETRY_AFTER, self.cancel)?;
                self.attempt(&call)
            }
            other => other,
        }
    }

    fn attempt<T>(
        &mut self,
        call: &impl Fn(&NinjaClient) -> Result<T, NinjaError>,
    ) -> Result<T, SamplerError> {
        self.pacer.before_request(self.cancel)?;
        Ok(call(&self.client)?)
    }

    /// 一次分区搜索。
    fn search(&mut self, query: &[(String, String)]) -> Result<SearchResponse, SamplerError> {
        let version = self.version.clone();
        let snapshot = self.snapshot_name.clone();
        let filters: Vec<(&str, &str)> = query
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        self.fetch(|client| client.search(&version, &snapshot, &filters))
    }

    /// 把这份响应用到的字典全抓齐。按 sha1 去重,所以三个宝石分面只花一次请求。
    fn ensure_dictionaries(&mut self, response: &SearchResponse) -> Result<(), SamplerError> {
        let mut wanted: Vec<String> = Vec::new();
        for facet in &response.facets {
            if let Some(sha1) = response.dictionary_sha1(dictionary_key_for_facet(&facet.name))
                && !self.dictionaries.contains_key(sha1)
                && !wanted.iter().any(|seen| seen == sha1)
            {
                wanted.push(sha1.to_owned());
            }
        }
        // 角色列要 class 表,而某些分区的响应里不一定带 class 分面。
        if let Some(sha1) = response.dictionary_sha1("class")
            && !self.dictionaries.contains_key(sha1)
            && !wanted.iter().any(|seen| seen == sha1)
        {
            wanted.push(sha1.to_owned());
        }

        for sha1 in wanted {
            let key = sha1.clone();
            let entries = self.fetch(|client| client.dictionary(&key))?;
            self.dictionaries.insert(sha1, entries);
        }
        Ok(())
    }

    // ---- 第 2 步:快照 ------------------------------------------------

    /// 读 index-state / build-index-state,决定这一轮挂在哪个 version 上,落一行快照。
    fn open_snapshot(&mut self, latest: Option<&SnapshotRow>) -> Result<LeagueBuild, SamplerError> {
        let index = self.fetch(NinjaClient::index_state)?;
        // 手上有 index-state 了,就别再用猜出来的短名(见 `resolve_league_url`)。
        let league_url =
            resolve_league_url(&index, &self.config.league_name, &self.config.league_url);
        let fresh = index
            .snapshot_for_url(&league_url)
            .ok_or_else(|| SamplerError::UnknownLeague(league_url.clone()))?
            .clone();

        let builds = self.fetch(NinjaClient::build_index_state)?;
        let league = builds
            .league_builds
            .iter()
            .find(|entry| entry.league_url == league_url)
            .cloned()
            .ok_or_else(|| SamplerError::UnknownLeague(league_url.clone()))?;

        // 续跑时**沿用库里那个 version**:分区和统计都是按 version 存的,
        // 换成刚读回来的新号等于把跑了一半的工作全作废重来一遍。
        // 只在"确实有活干"时才沿用——一行光杆快照(上一轮刚落库就被杀)
        // 没有任何可续的东西,那就老老实实用新快照。
        let resumed = match latest {
            Some(row)
                if stage_order(SamplerStage::from_snapshot(row.stage))
                    < stage_order(self.config.stop_after) =>
            {
                let pending = self
                    .store
                    .pending_partitions(&row.league_url, &row.version)?;
                let done = self.store.partition_keys(&row.league_url, &row.version)?;
                (!pending.is_empty() || !done.is_empty()).then(|| row.clone())
            }
            _ => None,
        };

        let (version, snapshot_name, stage, finished_at) = match &resumed {
            Some(row) => (
                row.version.clone(),
                row.snapshot_name.clone(),
                row.stage,
                row.finished_at,
            ),
            None => (
                fresh.version.clone(),
                fresh.snapshot_name.clone(),
                SnapshotStage::Planned,
                None,
            ),
        };

        // `started_at` 每次开工都刷新:"这份数据多新"问的是最后一次真的去问了
        // poe.ninja 是什么时候,不是这个 version 第一次被看见是什么时候。
        self.store.upsert_snapshot(&SnapshotRow {
            league_url: self.config.league_url.clone(),
            version: version.clone(),
            snapshot_name: snapshot_name.clone(),
            total_characters: league.total,
            stage,
            started_at: now_secs(),
            finished_at,
        })?;

        self.version = version.clone();
        self.snapshot_name = snapshot_name.clone();
        self.stage = SamplerStage::from_snapshot(stage);
        self.reached = self.stage;
        if self.league_name.is_empty() {
            self.league_name = fresh.name.clone();
        }
        (self.emit)(SamplerEvent::Started {
            version,
            snapshot_name,
            total_characters: league.total,
        });
        Ok(league)
    }

    // ---- 第 4 步:分区 ------------------------------------------------

    fn partition_stage(&mut self, league: &LeagueBuild) -> Result<(), SamplerError> {
        self.stage = SamplerStage::Facets;
        let league_url = self.config.league_url.clone();
        let version = self.version.clone();

        let mut done = count(self.store.partition_keys(&league_url, &version)?.len());
        if done == 0
            && self
                .store
                .pending_partitions(&league_url, &version)?
                .is_empty()
        {
            let _planned = self.first_pass(league)?;
            done += 1;
        }

        // 外层循环跑两遍:第一遍把第一轮的分区跑掉,顺手把 `class=X&skills=Y`
        // 排进队列;第二遍把它们跑掉。之后队列空了就出来。
        loop {
            let pending = self.store.pending_partitions(&league_url, &version)?;
            if pending.is_empty() {
                return Ok(());
            }
            let total = done + count(pending.len());
            for row in pending {
                check_cancel(self.cancel)?;
                let note = match self.run_partition(&row.partition_key, row.tier) {
                    Ok(matched) => format!("{} → {matched} characters", label(&row.partition_key)),
                    Err(SamplerError::Cancelled) => return Err(SamplerError::Cancelled),
                    Err(error) => {
                        self.store.fail_partition(
                            &league_url,
                            &version,
                            &row.partition_key,
                            now_secs(),
                        )?;
                        format!("{} failed: {error}", label(&row.partition_key))
                    }
                };
                done += 1;
                self.progress(done, total.max(done), note);
            }
        }
    }

    /// 第一轮:一次 `""` 全联赛搜索,就够算出整张分区清单。
    ///
    /// `""` 自己也在清单里,而它的响应此刻就在手上——直接标完成,
    /// 不要为了走流程再问一次同样的问题。
    fn first_pass(&mut self, league: &LeagueBuild) -> Result<Vec<Partition>, SamplerError> {
        let response = self.search(&[])?;
        self.ensure_dictionaries(&response)?;

        let options = sample_options(&self.config.tuning);
        let partitions = {
            let gems = dictionary_slice(&response, "gem", &self.dictionaries);
            let items = dictionary_slice(&response, "item", &self.dictionaries);
            first_pass_partitions(league, &response, gems, items, &options)
        };
        self.store
            .enqueue_partitions(&self.config.league_url, &self.version, &partitions)?;
        let matched = self.store_partition("", PartitionTier::Whole, &response)?;
        self.progress(
            1,
            count(partitions.len()),
            format!(
                "planned {} partitions from the whole-league search ({matched} characters)",
                partitions.len()
            ),
        );
        Ok(partitions)
    }

    /// 跑一条分区:搜索 → 分面 + 角色名单落库 →(职业档)排它自己的技能分区。
    fn run_partition(&mut self, key: &str, tier: PartitionTier) -> Result<u32, SamplerError> {
        let query = query_from_key(key);
        let response = self.search(&query)?;
        let matched = self.store_partition(key, tier, &response)?;

        if tier == PartitionTier::Class
            && let Some(class) = query
                .iter()
                .find(|(name, _)| name == "class")
                .map(|(_, value)| value.clone())
        {
            let options = sample_options(&self.config.tuning);
            let partitions = {
                let gems = dictionary_slice(&response, "gem", &self.dictionaries);
                class_skill_partitions(&class, &response, gems, &options)
            };
            self.store
                .enqueue_partitions(&self.config.league_url, &self.version, &partitions)?;
        }
        Ok(matched)
    }

    /// 分面 + 角色名单一个事务写进去。返回这次带回来几个角色(去重前)。
    fn store_partition(
        &mut self,
        key: &str,
        tier: PartitionTier,
        response: &SearchResponse,
    ) -> Result<u32, SamplerError> {
        self.ensure_dictionaries(response)?;
        let facets = facet_rows(response, &self.dictionaries);
        let characters = {
            let classes = dictionary_slice(response, "class", &self.dictionaries);
            sampled_characters(response, key, tier, classes)
        };
        let matched = count(characters.len());
        self.store.complete_partition(
            &self.config.league_url,
            &self.version,
            key,
            response.total,
            &facets,
            &characters,
            now_secs(),
        )?;
        Ok(matched)
    }

    // ---- 第 5 步:参考价 ----------------------------------------------

    fn prices_stage(&mut self) -> Result<(), SamplerError> {
        let league_url = self.config.league_url.clone();
        let league_name = self.league_name.clone();
        let total = count(UNIQUE_TYPES.len());
        let mut done = 0u32;
        for type_name in UNIQUE_TYPES {
            check_cancel(self.cancel)?;
            let name = league_name.clone();
            match self.fetch(|client| client.unique_prices(&name, type_name)) {
                Ok(overview) => {
                    self.store.replace_unique_prices(
                        &league_url,
                        type_name,
                        &overview.lines,
                        now_secs(),
                    )?;
                    done += 1;
                    (self.emit)(SamplerEvent::Prices { types_done: done });
                }
                Err(SamplerError::Cancelled) => return Err(SamplerError::Cancelled),
                // 一类抓砸了不该连累另外五类:`replace_unique_prices` 本来就是
                // 按分类替换的,少一类只是那一类还是上次的价。
                Err(error) => self.progress(done, total, format!("prices {type_name}: {error}")),
            }
        }
        Ok(())
    }

    // ---- 第 6 步:角色详情 --------------------------------------------

    fn character_stage(&mut self) -> Result<(), SamplerError> {
        self.stage = SamplerStage::Characters;
        let league_url = self.config.league_url.clone();
        let (pending_now, already_done, _) = self.store.character_counts(&league_url)?;
        // 名单是按分区档次的顺序插进来的,所以"队列的下 N 个"天然就是
        // `select_sample` 会挑的那 N 个,不用把 6,500 行读进内存再排一次。
        let limit = character_limit(
            self.config.tuning.sample_target,
            already_done,
            self.config.max_characters,
        );
        if limit == 0 {
            self.progress(
                already_done,
                already_done,
                format!("nothing to fetch: {already_done} done, {pending_now} still queued"),
            );
            return Ok(());
        }

        let queue = self.store.pending_characters(&league_url, limit)?;
        let total = count(queue.len());
        for (index, row) in queue.iter().enumerate() {
            check_cancel(self.cancel)?;
            let version = self.version.clone();
            let snapshot = self.snapshot_name.clone();
            let account = row.account.clone();
            let name = row.name.clone();
            let result =
                self.fetch(|client| client.character_raw(&version, &account, &name, &snapshot));

            let note = match result {
                Ok(detail) => {
                    self.store.complete_character(
                        &league_url,
                        &account,
                        &name,
                        &self.version,
                        &detail,
                        now_secs(),
                    )?;
                    name.clone()
                }
                Err(SamplerError::Cancelled) => return Err(SamplerError::Cancelled),
                // 404 = 删号或改名了,标 failed 免得下一轮又卡在队首。
                Err(SamplerError::Ninja(NinjaError::Rejected(404))) => {
                    self.store
                        .fail_character(&league_url, &account, &name, now_secs())?;
                    format!("{name}: gone (404)")
                }
                // 别的错(断网、超时)留 pending:下一轮接着补,不浪费一个名额。
                Err(error) => format!("{name}: {error} (will retry next run)"),
            };

            let position = index + 1;
            if position % CHARACTER_PROGRESS_EVERY == 0 || position == queue.len() {
                self.progress(count(position), total, note);
            }
        }
        Ok(())
    }

    // ---- 第 7 步:词缀统计 --------------------------------------------

    fn aggregate_stage(&mut self) -> Result<(), SamplerError> {
        self.stage = SamplerStage::Aggregated;
        let league_url = self.config.league_url.clone();
        let raw = self.store.done_character_details(&league_url)?;

        let mut details: Vec<CharacterDetail> = Vec::with_capacity(raw.len());
        let mut broken = 0u32;
        for json in &raw {
            match serde_json::from_str::<CharacterDetail>(json) {
                Ok(detail) => details.push(detail),
                // 一份读不动的详情不该让整轮聚合失败:数一笔、跳过,
                // 数字大起来就是格式漂移的第一个信号。
                Err(_) => broken += 1,
            }
        }
        check_cancel(self.cancel)?;

        let stats = aggregate_mods(&details);
        self.store
            .replace_item_mods(&league_url, &self.version, &stats)?;
        self.progress(
            count(details.len()),
            count(raw.len()),
            format!(
                "{} mod rows from {} characters ({broken} unparseable)",
                stats.len(),
                details.len()
            ),
        );
        Ok(())
    }

    // ---- 杂活 --------------------------------------------------------

    /// 推进阶段并广播。`Aggregated` 那一下顺便盖上 `finished_at`(存储层的规矩)。
    ///
    /// 写进库的是"走到过的最远那一步":续跑时前面几步会被空跑一遍,
    /// 照着空跑的结果写库会让阶段倒退(见 [`highest_stage`])。
    fn finish_stage(&mut self, stage: SamplerStage) -> Result<(), SamplerError> {
        self.reached = highest_stage(self.reached, stage);
        self.store.set_stage(
            &self.config.league_url,
            &self.version,
            self.reached.to_snapshot(),
            now_secs(),
        )?;
        self.stage = stage;
        (self.emit)(SamplerEvent::StageDone(stage));
        Ok(())
    }

    fn progress(&self, done: u32, total: u32, note: String) {
        (self.emit)(SamplerEvent::Progress {
            stage: self.stage,
            done,
            total,
            note,
        });
    }
}

/// 空分区键在日志里长得像个 bug,给它一个名字。
fn label(key: &str) -> &str {
    if key.is_empty() {
        "(whole league)"
    } else {
        key
    }
}

#[cfg(test)]
mod ninja_sampler_tests {
    use super::*;
    use pnd_ninja::index_state::SnapshotVersion;
    use pnd_ninja::search::{Column, DictionaryRef, Facet, FacetEntry};

    fn snapshot(stage: SnapshotStage, started_at: i64, finished_at: Option<i64>) -> SnapshotRow {
        SnapshotRow {
            league_url: "forbiddenrites".to_owned(),
            version: "1508-20260906-55820".to_owned(),
            snapshot_name: "forbidden-rites".to_owned(),
            total_characters: 61_390,
            stage,
            started_at,
            finished_at,
        }
    }

    /// 一个库都没有的时候当然要跑。
    #[test]
    fn no_snapshot_means_no_skip() {
        assert_eq!(should_skip(None, SamplerStage::Facets, 24, 10_000), None);
    }

    /// 跑了一半的那轮不看年龄:它留在库里就是为了被接着跑完。
    #[test]
    fn an_unfinished_snapshot_is_always_resumed() {
        let row = snapshot(SnapshotStage::Planned, 0, None);
        assert_eq!(should_skip(Some(&row), SamplerStage::Facets, 24, 1), None);
        // 分面跑完了,但这一轮的目标是聚合:还差两步,照跑。
        let row = snapshot(SnapshotStage::Facets, 0, None);
        assert_eq!(
            should_skip(Some(&row), SamplerStage::Aggregated, 24, 1),
            None
        );
    }

    /// 到了目标阶段 + 还在刷新窗口里 = 跳过,理由里得写清是哪个快照、多久以前。
    #[test]
    fn a_fresh_snapshot_at_the_target_stage_is_skipped() {
        let row = snapshot(SnapshotStage::Facets, 1_000, None);
        let reason = should_skip(
            Some(&row),
            SamplerStage::Facets,
            24,
            1_000 + 6 * 3_600 + 720,
        )
        .expect("should skip");
        assert!(reason.contains("1508-20260906-55820"), "{reason}");
        assert!(reason.contains("facets"), "{reason}");
        assert!(reason.contains("6h12m"), "{reason}");
    }

    /// 整轮跑完的那种,年龄按 `finished_at` 算 —— 一轮采样跑 40 分钟,
    /// 拿开跑时刻当年龄会让下一轮早 40 分钟开工。
    #[test]
    fn a_finished_snapshot_ages_from_finished_at() {
        // 开跑在 1,000,跑完在 90,000;现在 100,000 = 跑完才 2h47m。
        let row = snapshot(SnapshotStage::Aggregated, 1_000, Some(90_000));
        assert!(should_skip(Some(&row), SamplerStage::Aggregated, 24, 100_000).is_some());
        // 同一行,24 小时之后就该重跑了。
        assert_eq!(
            should_skip(
                Some(&row),
                SamplerStage::Aggregated,
                24,
                90_000 + 24 * 3_600
            ),
            None
        );
    }

    /// `refresh_hours = 0` 是"每次都重跑"的开关,不能被解释成"永远跳过"。
    #[test]
    fn a_zero_refresh_window_never_skips() {
        let row = snapshot(SnapshotStage::Aggregated, 1_000, Some(1_000));
        assert_eq!(
            should_skip(Some(&row), SamplerStage::Aggregated, 0, 1_000),
            None
        );
    }

    /// 续跑时"分面"那一步会被空跑一遍(队列是空的,一个请求都不发),
    /// 但库里的阶段不能因此从 `characters` 倒退回 `facets` ——
    /// 倒退之后再被杀掉,下一轮就会以为角色详情还没开始。
    #[test]
    fn finishing_an_earlier_stage_never_moves_the_snapshot_backwards() {
        let store = NinjaStore::open_in_memory().expect("store");
        store
            .upsert_snapshot(&snapshot(SnapshotStage::Characters, 1_000, None))
            .expect("insert");

        let config = SamplerConfig::new("forbiddenrites", "Forbidden Rites");
        let cancel = AtomicBool::new(false);
        let emit = |_: SamplerEvent| {};
        let mut sampler = Sampler::new(store, &config, &cancel, &emit);
        sampler.version = "1508-20260906-55820".to_owned();
        sampler.reached = SamplerStage::Characters;

        sampler
            .finish_stage(SamplerStage::Facets)
            .expect("facets again");
        assert_eq!(
            sampler
                .store
                .latest_snapshot("forbiddenrites")
                .expect("read")
                .expect("row")
                .stage,
            SnapshotStage::Characters,
            "空跑一遍分面阶段不该把库里的进度打回去"
        );

        // 真的往前走一步时照样写得进去,`finished_at` 也照样盖上。
        sampler
            .finish_stage(SamplerStage::Aggregated)
            .expect("aggregated");
        let row = sampler
            .store
            .latest_snapshot("forbiddenrites")
            .expect("read")
            .expect("row");
        assert_eq!(row.stage, SnapshotStage::Aggregated);
        assert!(row.finished_at.is_some());
    }

    /// 联赛短名以 index-state 说的为准。
    ///
    /// 界面手上只有显示名,短名是猜出来的(去掉非字母数字再小写)。这个猜法
    /// 对 `Forbidden Rites` 是对的,但它只是规律不是承诺 —— 猜错了的话
    /// `snapshot_for_url` 找不到东西,整轮采样以 `UnknownLeague` 收场,
    /// 而 poe.ninja 明明在同一份 index-state 里把正确答案写着。
    #[test]
    fn the_league_short_name_comes_from_the_index_when_the_name_matches() {
        let index = IndexState {
            snapshot_versions: vec![SnapshotVersion {
                url: "fr2".to_owned(),
                name: "Forbidden Rites".to_owned(),
                ..SnapshotVersion::default()
            }],
            ..IndexState::default()
        };

        // 猜出来的 `forbiddenrites` 是错的,但显示名对得上。
        assert_eq!(
            resolve_league_url(&index, "Forbidden Rites", "forbiddenrites"),
            "fr2"
        );
        // 大小写不该影响(用户在设置里怎么打的都算)。
        assert_eq!(
            resolve_league_url(&index, "forbidden rites", "forbiddenrites"),
            "fr2"
        );
        // 这一轮没索引这个联赛:名字查不到,只能用猜的那个。
        assert_eq!(
            resolve_league_url(&index, "Runes of Aldur", "runesofaldur"),
            "runesofaldur"
        );
    }

    #[test]
    fn highest_stage_picks_the_later_one_either_way_round() {
        assert_eq!(
            highest_stage(SamplerStage::Characters, SamplerStage::Facets),
            SamplerStage::Characters
        );
        assert_eq!(
            highest_stage(SamplerStage::Facets, SamplerStage::Characters),
            SamplerStage::Characters
        );
        assert_eq!(
            highest_stage(SamplerStage::Planned, SamplerStage::Planned),
            SamplerStage::Planned
        );
    }

    /// 探针的 `--limit` 和设置里的 `sample_target` 谁小听谁的。
    #[test]
    fn the_character_budget_is_the_smaller_of_the_two_caps() {
        // 一次跑不完的那种:目标 2,000,这次只准抓 30 个。
        assert_eq!(character_limit(2_000, 0, Some(30)), 30);
        // 已经抓到手的要扣掉。
        assert_eq!(character_limit(2_000, 1_990, Some(30)), 10);
        assert_eq!(character_limit(2_000, 1_500, None), 500);
        // `--limit 5000` 不能把 2,000 这个目标架空。
        assert_eq!(character_limit(2_000, 0, Some(5_000)), 2_000);
        // 采满了就一个都不抓,而不是绕回一个巨大的数。
        assert_eq!(character_limit(2_000, 2_400, None), 0);
        assert_eq!(character_limit(2_000, 2_400, Some(30)), 0);
    }

    #[test]
    fn stage_order_climbs_and_round_trips_through_storage() {
        let stages = [
            SamplerStage::Planned,
            SamplerStage::Facets,
            SamplerStage::Characters,
            SamplerStage::Aggregated,
        ];
        for pair in stages.windows(2) {
            assert!(stage_order(pair[0]) < stage_order(pair[1]));
        }
        for stage in stages {
            assert_eq!(SamplerStage::from_snapshot(stage.to_snapshot()), stage);
            assert_eq!(SamplerStage::parse(stage.as_str()), stage);
        }
        assert_eq!(SamplerStage::parse("nonsense"), SamplerStage::Facets);
        assert_eq!(SamplerStage::Facets.as_str(), "facets");
    }

    /// 第一次请求不等;隔得不够就补到一秒;已经超了就立刻走。
    #[test]
    fn the_pacer_spaces_request_starts_not_request_ends() {
        let gap = Duration::from_secs(1);
        assert_eq!(pace_delay(None, gap), Duration::ZERO);

        let left = pace_delay(Some(Instant::now() - Duration::from_millis(300)), gap);
        assert!(
            left > Duration::from_millis(500) && left <= Duration::from_millis(700),
            "{left:?}"
        );

        assert_eq!(
            pace_delay(Some(Instant::now() - Duration::from_secs(5)), gap),
            Duration::ZERO
        );
    }

    fn response() -> SearchResponse {
        SearchResponse {
            total: 61_390,
            facets: vec![
                Facet {
                    name: "items".to_owned(),
                    kind: "item".to_owned(),
                    entries: vec![
                        FacetEntry {
                            index: 0,
                            count: 60_943,
                        },
                        FacetEntry {
                            index: 2,
                            count: 7_158,
                        },
                        // 字典只有 3 条:下标 9 越界,得留下 `#9` 而不是丢掉。
                        FacetEntry {
                            index: 9,
                            count: 12,
                        },
                    ],
                },
                Facet {
                    name: "class".to_owned(),
                    kind: "class".to_owned(),
                    entries: vec![FacetEntry {
                        index: 1,
                        count: 22_032,
                    }],
                },
            ],
            dictionaries: vec![
                DictionaryRef {
                    key: "item".to_owned(),
                    sha1: "item-sha".to_owned(),
                    overlay_sha1: None,
                },
                DictionaryRef {
                    key: "class".to_owned(),
                    sha1: "class-sha".to_owned(),
                    overlay_sha1: None,
                },
            ],
            columns: vec![
                Column {
                    id: "name".to_owned(),
                    strings: vec![
                        "ResurrectForbidden".to_owned(),
                        "KingPinUwU".to_owned(),
                        // 没名字的行没法去抓详情,该被扔掉。
                        String::new(),
                    ],
                    ..Column::default()
                },
                Column {
                    id: "account".to_owned(),
                    strings: vec![
                        "heygyus-0416".to_owned(),
                        "dota2enjoyer-1809".to_owned(),
                        "nobody-0000".to_owned(),
                    ],
                    ..Column::default()
                },
                Column {
                    id: "class".to_owned(),
                    varints: vec![1, 1, 0],
                    ..Column::default()
                },
                Column {
                    id: "level".to_owned(),
                    varints: vec![98, 97, 12],
                    ..Column::default()
                },
            ],
        }
    }

    fn dictionaries() -> HashMap<String, Vec<String>> {
        let mut dictionaries = HashMap::new();
        dictionaries.insert(
            "item-sha".to_owned(),
            vec![
                "Magic Flask".to_owned(),
                "Rare Ring".to_owned(),
                "Wake of Destruction".to_owned(),
            ],
        );
        dictionaries.insert(
            "class-sha".to_owned(),
            vec!["Abyssal Lich".to_owned(), "Gemling Legionnaire".to_owned()],
        );
        dictionaries
    }

    /// 分面整张存,连稀有度桶也存:排掉桶是**读**的时候的事
    /// (`NinjaStore::unique_usage`),存的时候丢了就再也补不回来。
    #[test]
    fn every_facet_is_stored_with_its_resolved_name() {
        let rows = facet_rows(&response(), &dictionaries());
        assert_eq!(
            rows,
            vec![
                ("items".to_owned(), "Magic Flask".to_owned(), 60_943),
                ("items".to_owned(), "Wake of Destruction".to_owned(), 7_158),
                ("items".to_owned(), "#9".to_owned(), 12),
                ("class".to_owned(), "Gemling Legionnaire".to_owned(), 22_032),
            ]
        );
    }

    /// 字典还没抓回来时不能整包炸掉:名字退化成 `#下标`,行还在。
    #[test]
    fn a_missing_dictionary_leaves_hash_labels_instead_of_dropping_rows() {
        let rows = facet_rows(&response(), &HashMap::new());
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|(_, name, _)| name.starts_with('#')));
    }

    #[test]
    fn character_rows_carry_the_partition_that_found_them() {
        let dictionaries = dictionaries();
        let response = response();
        let classes = dictionary_slice(&response, "class", &dictionaries);
        let rows = sampled_characters(
            &response,
            "class=Gemling Legionnaire",
            PartitionTier::Class,
            classes,
        );
        assert_eq!(rows.len(), 2, "没名字的那行不该进队列");
        assert_eq!(
            rows[0],
            SampledCharacter {
                account: "heygyus-0416".to_owned(),
                name: "ResurrectForbidden".to_owned(),
                class: "Gemling Legionnaire".to_owned(),
                level: 98,
                from_partition: "class=Gemling Legionnaire".to_owned(),
                tier: PartitionTier::Class,
            }
        );
        assert_eq!(rows[1].level, 97);
    }

    /// 429 和 5xx 值得再等一分钟,404 和解析失败不值得。
    #[test]
    fn only_rate_limits_and_server_errors_are_retried() {
        assert!(is_transient(&NinjaError::Rejected(429)));
        assert!(is_transient(&NinjaError::Rejected(503)));
        assert!(!is_transient(&NinjaError::Rejected(404)));
        assert!(!is_transient(&NinjaError::Rejected(403)));
        assert!(!is_transient(&NinjaError::Decode("nope".to_owned())));
    }

    /// 取消标志一立,睡眠和节流都要马上让路。
    #[test]
    fn a_cancelled_run_stops_sleeping_immediately() {
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            nap(Duration::from_secs(60), &cancel),
            Err(SamplerError::Cancelled)
        ));
        let mut pacer = Pacer::new(Duration::from_secs(1));
        assert!(matches!(
            pacer.before_request(&cancel),
            Err(SamplerError::Cancelled)
        ));
    }

    #[test]
    fn tuning_maps_straight_onto_the_plan_options() {
        let tuning = NinjaTuning::default();
        let options = sample_options(&tuning);
        assert_eq!(options, SampleOptions::default());
        assert_eq!(options.sample_target, 2_000);
        assert_eq!(label(""), "(whole league)");
        assert_eq!(label("class=X"), "class=X");
    }
}
