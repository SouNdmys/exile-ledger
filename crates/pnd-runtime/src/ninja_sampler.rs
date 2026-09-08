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
//! 5. 暗金参考价(6 个有文档的经济接口)。**每一轮都跑,而且排在角色详情前面**:
//!    6 个请求、几分钟,而下一步要跑一整天——把便宜的排在后面,就等于
//!    "一被限流打断,暗金页的价格列永远是空的"。
//! 6. 逐个抓角色详情,原文整段落库(`stop_after >= Characters` 才跑)。**每 50 个
//!    重建一次词缀统计**:小时预算下 2,000 个人要采一两天,词缀页不该空一整天。
//! 7. 从原文重建词缀统计(`stop_after == Aggregated` 才跑)。第 6 步已经建过很多
//!    次了,这一步只是收尾那一次。
//!
//! 第 4–7 步的顺序写在 [`stage_plan`] 这个纯函数里,`run_sampler` 只照单执行。
//!
//! 阶段只许前进:第 4 步在续跑时会空跑一遍(队列是空的,一个请求都不发),
//! 库里的阶段不能因此从 `characters` 退回 `facets`(见 [`highest_stage`])。
//!
//! 对 poe.ninja 的礼貌全压在一个 [`Pacer`] 上:**所有**出站请求都从它过一遍,
//! 每次请求前看一眼取消标志。闸门本身是一个 [`HourlyBudget`]:builds 接口的
//! 真正上限是"一个 IP 一小时多少个请求"(实测 ~120,和发多快无关),
//! 所以主控是那个小时预算,`min_request_gap_ms` 只当下限。撞上 429 就只等
//! 不放弃(听 `Retry-After`,没有就爬 [`rate_limit_delay`] 那把阶梯),并且
//! 把这一轮剩下的小时预算砍一半;5xx 是对面自己的毛病,睡 60 秒再试一次就不纠缠。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pnd_domain::Game;
use pnd_ninja::aggregate::aggregate_mods;
use pnd_ninja::character::CharacterDetail;
use pnd_ninja::client::{NinjaClient, NinjaError};
use pnd_ninja::economy::unique_types_for;
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

/// 撞上 5xx 之后先躺多久。一分钟是"明显在道歉"的量级:
/// 对面缓存 30 分钟,我们急这一分钟没有任何意义。
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// 连着吃 429 时的等待阶梯,单位秒。
///
/// 阶梯而不是一个定值,是因为 429 只有两种成因,而它们要的答案不一样:
/// 一种是"这一分钟发多了",睡一分钟就过去;另一种是对面把我们整个拉黑了
/// 一段时间,那再怎么一分钟一试也只是继续敲门。撞得越多越可能是后者,
/// 所以等得越久;顶到 10 分钟就不再涨——再久就不如让用户自己决定还跑不跑。
const RATE_LIMIT_LADDER: [u64; 4] = [60, 120, 300, 600];

/// 撞上 429 之后该等多久。`consecutive` 是**连续**第几个 429(从 1 数起),
/// 成功一次就归零。
///
/// `retry_after` 是服务端自己在 `Retry-After` 里写的秒数:**它说了就听它的**。
/// 我们那把阶梯只是在对面什么都没说时的猜测,而猜测没有理由压过原话。
#[must_use]
pub fn rate_limit_delay(consecutive: u32, retry_after: Option<u64>) -> Duration {
    if let Some(secs) = retry_after {
        return Duration::from_secs(secs);
    }
    let step = (consecutive.max(1) as usize - 1).min(RATE_LIMIT_LADDER.len() - 1);
    Duration::from_secs(RATE_LIMIT_LADDER[step])
}

/// 这个错是不是"你太快了"。是的话顺手把服务端说的秒数带出来。
fn rate_limited(error: &NinjaError) -> Option<Option<u64>> {
    match error {
        NinjaError::RateLimited { retry_after_secs } => Some(*retry_after_secs),
        _ => None,
    }
}

/// 睡觉时每隔这么久醒一次看取消标志。关程序不该等满一个 60 秒的退避。
const CANCEL_SLICE: Duration = Duration::from_millis(100);

/// 角色详情每抓这么多个发一次进度。2,000 个角色发 200 条事件,界面够用又不刷屏。
const CHARACTER_PROGRESS_EVERY: usize = 10;

/// 每抓这么多个角色就把词缀统计重建一次。
///
/// 一小时 100 个请求意味着 2,000 个人要采一整天。等到最后一步才建统计的话,
/// 词缀页会空一整天,而中途关一次程序就前功尽弃。50 个 ≈ 半小时一次,
/// 重建本身不出网(从库里的原文算),几百毫秒的事。
const CHARACTER_AGGREGATE_EVERY: usize = 50;

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

/// 一轮采样实际要跑的那几步,按顺序。
///
/// 和 [`SamplerStage`] 不是一回事:阶段是"存进库里的进度条",步骤是
/// "这一轮依次干哪几件事"。参考价就是差别所在 —— 它每一轮都跑,却不占
/// 阶段阶梯上的一格(库里没有 `prices` 这一档)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerStep {
    /// 分区搜索:热门暗金榜的人气数。
    Facets,
    /// 暗金参考价,6 个经济接口。
    Prices,
    /// 角色详情:两千个人,按小时预算 36 秒一个,一整天。
    Characters,
    /// 词缀统计重建。不出网,从库里的原文算。
    Aggregated,
}

/// 这一轮按什么顺序跑哪几步。
///
/// 顺序本身就是一个决定,而它决定的是**被限流打断时哪些数据已经落地**:
/// 参考价只有 6 个请求、几分钟,角色详情要跑一整天,所以便宜的那一步
/// 排在前面。反过来排的那一版里,角色详情一撞上 429,暗金页的
/// 参考价 / 挂单数 / 7 天三列就永远是"—"。
///
/// 参考价**每一轮都跑**,和 `stop_after` 无关:它是"这件暗金现在值多少",
/// 半小时前的价格和刚采完的人气榜摆在一起才有意义。
#[must_use]
pub fn stage_plan(stop_after: SamplerStage) -> Vec<SamplerStep> {
    let mut steps = Vec::new();
    if stage_order(stop_after) >= stage_order(SamplerStage::Facets) {
        steps.push(SamplerStep::Facets);
    }
    steps.push(SamplerStep::Prices);
    if stage_order(stop_after) >= stage_order(SamplerStage::Characters) {
        steps.push(SamplerStep::Characters);
    }
    if stage_order(stop_after) >= stage_order(SamplerStage::Aggregated) {
        steps.push(SamplerStep::Aggregated);
    }
    steps
}

/// 跑一轮采样要知道的全部东西。
#[derive(Debug, Clone)]
pub struct SamplerConfig {
    /// 这一轮问哪一代游戏。它一个字段管三件事:客户端指哪个前缀、
    /// 库写哪个文件、参考价问哪张分类表。
    pub game: Game,
    /// builds 接口用的联赛短名,例如 `forbiddenrites`。
    ///
    /// **留空 = "用当季挑战联赛"**:那时候由 index-state 自己说了算
    /// (见 [`resolve_league_url`])。
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
    /// 常用的一组:PoE2、跑到分面、不强制、默认库。
    ///
    /// **签名刻意不带 `game`**:老调用方问的一直是 PoE2,让它们一个字都不用改。
    #[must_use]
    pub fn new(league_url: impl Into<String>, league_name: impl Into<String>) -> SamplerConfig {
        SamplerConfig::for_game(Game::Poe2, league_url, league_name)
    }

    /// 某一代的那一组。库的默认路径跟着游戏走 —— 两代混进一个文件,
    /// 分区键长得一模一样,混进去就再也分不开了。
    #[must_use]
    pub fn for_game(
        game: Game,
        league_url: impl Into<String>,
        league_name: impl Into<String>,
    ) -> SamplerConfig {
        SamplerConfig {
            game,
            league_url: league_url.into(),
            league_name: league_name.into(),
            tuning: NinjaTuning::default(),
            user_agent: String::new(),
            db_path: pnd_storage::default_ninja_db_path_for(game),
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
    /// 词缀统计又重建了一次(每 [`CHARACTER_AGGREGATE_EVERY`] 个角色一次,
    /// 外加进出角色那一步各一次)。
    ///
    /// 一轮采样要跑一整天,所以这条事件同时干两件事:告诉界面"该重读一遍库了"
    /// (词缀页于是一点一点长出来,而不是最后一刻才有),以及报一句
    /// 人看得懂的进度。数字是散的、不是拼好的一句话 —— 界面文案归
    /// `pnd-app` 的 `i18n` 管,运行时不该内联中文。
    Sampled {
        /// 这个联赛累计抓到手的角色数(**跨快照**,不是这一轮抓了几个)。
        characters: u32,
        /// `sample_target`。
        target: u32,
        /// 滑动的这一小时里已经发了几个请求。
        used_this_hour: u32,
        /// 这一小时准发几个(撞过 429 的话已经砍过一半)。
        hourly_budget: u32,
        /// 按这个预算,补满还要多少秒。
        eta_secs: u64,
    },
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
    /// 设置里那个联赛留空(意思是"用当季挑战联赛"),而 index-state 里
    /// 一条挑战联赛都挑不出来。宁可停在这里,也不要退回 Standard 去采一天。
    #[error("poe.ninja did not name a current {0} challenge league — set the league by hand")]
    NoChallengeLeague(Game),
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

/// 按小时预算算,还差 `remaining` 个角色要花多少秒。
///
/// 只看预算不看别的:一小时 100 个请求就是一小时 100 个角色,网速、
/// 对面响应快慢都被那个间隔盖住了。
#[must_use]
pub fn budget_eta_secs(remaining: u32, hourly_budget: u32) -> u64 {
    u64::from(remaining) * 3_600 / u64::from(hourly_budget.max(1))
}

/// 这一轮该按哪个联赛短名去问 poe.ninja。
///
/// 界面手上只有 `settings.json` 里那个显示名(`Forbidden Rites`),短名是
/// [`league_url_guess`](pnd_ninja::index_state::league_url_guess) 猜的。但
/// index-state 每条快照都同时带着显示名和短名 —— **手上有它就别再猜**:
/// 猜法只是规律,没有任何接口承诺过,猜错一次的代价是整轮采样以
/// `UnknownLeague` 收场,而正确答案就在刚读回来的那份 JSON 里。
///
/// 两个名字**都空**是另一件事:那是设置里那个联赛留着没填,意思是
/// "用当季挑战联赛"。联赛三个月换一次名字,让人每赛季回设置页改一次字符串
/// 是纯手工活,而 index-state 自己就说得出当季那个叫什么。挑不出来时给
/// `None` —— 退回 Standard 等于把一整天的请求配额喂给一份没人看的数据。
fn resolve_league_url(index: &IndexState, league_name: &str, guessed: &str) -> Option<String> {
    if !league_name.is_empty()
        && let Some(url) = index.league_url_for_name(league_name)
    {
        return Some(url.to_owned());
    }
    if !guessed.is_empty() {
        return Some(guessed.to_owned());
    }
    index
        .current_challenge_league()
        .map(|league| league.url.clone())
}

/// 滑动窗口有多宽。配额是"每滚动一小时多少个",不是"每个整点清零"。
const HOUR: Duration = Duration::from_secs(3_600);

/// 距离下一次请求还该等多久。
///
/// 三个输入都是"相对现在"的时长,所以这个函数是纯的:测试里给几个
/// [`Duration`] 就能把一整个小时的节奏跑完,不用真的睡。
///
/// - `used` / `limit`:这一小时已经发了几个、一共准发几个。
/// - `since_last`:离上一个请求的**起点**过去多久(第一个请求是 `None`)。
///   看起点而不是"上一次回来之后再等":后者会让每个慢请求都额外赔上
///   它自己的耗时,一轮 2,000 个角色能白等十几分钟。
/// - `oldest_age`:窗口里最老那个请求的年龄。
/// - `floor`:设置里的最小间隔。
///
/// 两条约束取更晚的那个:
///
/// - **摊平**:一小时的预算均匀铺开(`3600/limit` 秒一个)。不摊平的话,
///   开头三分钟就把 100 个打光,然后干等 57 分钟——平均值一样漂亮,
///   可界面上是"要么全有要么全无",而且一被打断就什么都没落地。
/// - **硬顶**:窗口里已经攒够 `limit` 个,就等最老的那个滚出这一小时。
///   这一条才是"绝不超额"的保证,摊平只是让它好看。
#[must_use]
pub fn budget_delay(
    used: u32,
    limit: u32,
    since_last: Option<Duration>,
    oldest_age: Option<Duration>,
    floor: Duration,
) -> Duration {
    // 0 会让摊平变成除以零。1 是"一小时一个",慢得离谱但不会炸。
    let limit = limit.max(1);
    let gap = (HOUR / limit).max(floor);
    let mut wait = since_last.map_or(Duration::ZERO, |elapsed| gap.saturating_sub(elapsed));
    if used >= limit {
        // 窗口没有最老的一条却已经满了,只可能是 `limit` 是 0 被抬成了 1 而
        // 一条都没发过 —— 那就别等。
        let age = oldest_age.unwrap_or(HOUR);
        wait = wait.max(HOUR.saturating_sub(age));
    }
    wait
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

/// 一小时的请求预算,滑动窗口。分面、参考价、角色详情共用**同一个**——
/// poe.ninja 数的是这个 IP 一共发了多少个,不管它们分别是干什么的。
///
/// 自己不睡也不看表以外的东西:什么时候算"现在"由调用方给,
/// 于是它是纯的,测试可以把一整个小时钉死时间跑完。
pub struct HourlyBudget {
    limit: u32,
    floor: Duration,
    /// 这一小时里每个请求的起点,最老的排在前面。
    starts: VecDeque<Instant>,
}

impl HourlyBudget {
    #[must_use]
    pub fn new(limit: u32, floor: Duration) -> HourlyBudget {
        HourlyBudget {
            // 0 会让"摊平"变成除以零。1 是"一小时一个",慢得离谱但不会炸。
            limit: limit.max(1),
            floor,
            starts: VecDeque::new(),
        }
    }

    #[must_use]
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// 把滚出这一小时的请求丢掉,返回窗口里还剩几个。进度行上的"本小时已用"。
    pub fn used(&mut self, now: Instant) -> u32 {
        while let Some(oldest) = self.starts.front() {
            if now.duration_since(*oldest) >= HOUR {
                self.starts.pop_front();
            } else {
                break;
            }
        }
        count(self.starts.len())
    }

    /// 还该等多久。
    pub fn delay(&mut self, now: Instant) -> Duration {
        let used = self.used(now);
        budget_delay(
            used,
            self.limit,
            self.starts.back().map(|last| now.duration_since(*last)),
            self.starts.front().map(|old| now.duration_since(*old)),
            self.floor,
        )
    }

    /// 记一笔"这个时刻发了一个"。
    pub fn record(&mut self, at: Instant) {
        self.starts.push_back(at);
    }

    /// 撞上 429 之后:这一轮剩下的时间预算砍一半。
    ///
    /// 观察到的配额只是估的(~120),吃到 429 说明估高了或者今天这个 IP
    /// 还有别的东西在用。砍一半是**这一轮**的事,下次开程序重新按设置来。
    pub fn halve(&mut self) {
        self.limit = (self.limit / 2).max(1);
    }
}

/// 全流程唯一的出网闸门。
struct Pacer {
    budget: HourlyBudget,
}

impl Pacer {
    fn new(limit: u32, floor: Duration) -> Pacer {
        Pacer {
            budget: HourlyBudget::new(limit, floor),
        }
    }

    /// 睡到可以发下一个请求,顺便把取消标志看一遍。
    fn before_request(&mut self, cancel: &AtomicBool) -> Result<(), SamplerError> {
        nap(self.budget.delay(Instant::now()), cancel)?;
        check_cancel(cancel)?;
        self.budget.record(Instant::now());
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
        NinjaError::RateLimited { .. } => true,
        NinjaError::Rejected(status) => *status >= 500,
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
    // 联赛名留空时还不知道这一轮会落在哪个短名上(要等 index-state),所以
    // 拿库里最近的那一轮当"上一轮"。这个库只装一代,一代通常只盯一个联赛,
    // 所以"最近那一轮"就是它 —— 少了这一步,留空的那条路每次都从零开始,
    // "24 小时内不重跑"和断点续跑一起失效。
    let latest = if config.league_url.is_empty() {
        store.latest_snapshot_any()?
    } else {
        store.latest_snapshot(&config.league_url)?
    };
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

    // 跑哪几步、什么顺序全在 [`stage_plan`] 里,这儿只负责照单执行 ——
    // 顺序是个会被改的决定(参考价就从最后挪到了角色详情前面),
    // 让它待在一个测得动的纯函数里,比散在几个 `if` 中间可靠。
    for step in stage_plan(config.stop_after) {
        match step {
            SamplerStep::Facets => {
                sampler.partition_stage(&league)?;
                sampler.finish_stage(SamplerStage::Facets)?;
            }
            SamplerStep::Prices => sampler.prices_stage()?,
            SamplerStep::Characters => {
                sampler.character_stage()?;
                sampler.finish_stage(SamplerStage::Characters)?;
            }
            SamplerStep::Aggregated => {
                sampler.aggregate_stage()?;
                sampler.finish_stage(SamplerStage::Aggregated)?;
            }
        }
    }

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
    /// 当前阶段跑到哪了。**出网那一层手上没有循环变量**:限流时要报一句
    /// "等一下",数字只能从这儿拿 —— 老版本写死 `0/0`,于是界面上是
    /// "角色 0/0 · 429",而队列里明明还排着两千多个人。
    progress_done: u32,
    progress_total: u32,
    /// 连着吃了几个 429。通一次就归零,决定退避阶梯爬到第几格。
    rate_limit_hits: u32,
    /// 这一轮有没有因为 429 把节奏放慢过。只放慢一次,不叠加。
    slowed_down: bool,
    /// 这个快照**最远**走到过哪一步。库里的阶段只跟着它走,所以只会前进。
    reached: SamplerStage,
    version: String,
    snapshot_name: String,
    /// 经济接口用的联赛显示名。设置里留空时用 index-state 里的那个,
    /// 免得 `--league someotherleague` 拿着 "Forbidden Rites" 去问价格。
    league_name: String,
    /// 库里所有行按它分。一开始就是 `config.league_url`;只有配置里那个
    /// **留空**(= "用当季挑战联赛")时,才在读到 index-state 之后填进来。
    league_url: String,
}

impl<'a> Sampler<'a> {
    fn new(
        store: NinjaStore,
        config: &'a SamplerConfig,
        cancel: &'a AtomicBool,
        emit: &'a dyn Fn(SamplerEvent),
    ) -> Sampler<'a> {
        Sampler {
            client: NinjaClient::for_game(config.game),
            pacer: Pacer::new(
                config.tuning.max_requests_per_hour,
                Duration::from_millis(config.tuning.min_request_gap_ms),
            ),
            store,
            dictionaries: HashMap::new(),
            config,
            cancel,
            emit,
            stage: SamplerStage::Planned,
            progress_done: 0,
            progress_total: 0,
            rate_limit_hits: 0,
            slowed_down: false,
            reached: SamplerStage::Planned,
            version: String::new(),
            snapshot_name: String::new(),
            league_name: config.league_name.clone(),
            league_url: config.league_url.clone(),
        }
    }

    // ---- 出网 --------------------------------------------------------

    /// 所有请求的唯一出口:节流 + 取消 + 出错重试。
    ///
    /// 两种错分开处置,因为它们说的根本不是同一件事:
    ///
    /// - **429("你太快了")只等,不放弃。** 等多久见 [`rate_limit_delay`]。
    ///   上一版是"睡 60 秒、再试一次、还不行就把这个请求判死",于是限流一来,
    ///   2,000 个角色变成一人两个请求 + 60 秒的空转,一整夜也采不完一个人,
    ///   还在持续敲一个明说了"别敲"的门。等待是**唯一**正确的反应。
    /// - **5xx 照旧只重试一次。** 那是对面自己出问题,和我们的节奏无关,
    ///   守在这儿一直等没有意义。重试完是回到循环顶上而不是直接返回:
    ///   万一那一次撞的是 429,它该走上面那条路,而不是被当成"这条抓不到了"。
    fn fetch<T>(
        &mut self,
        call: impl Fn(&NinjaClient) -> Result<T, NinjaError>,
    ) -> Result<T, SamplerError> {
        let mut server_error_retried = false;
        loop {
            match self.attempt(&call) {
                Ok(value) => {
                    self.rate_limit_hits = 0;
                    return Ok(value);
                }
                Err(SamplerError::Ninja(error)) => {
                    if let Some(retry_after) = rate_limited(&error) {
                        self.back_off(retry_after)?;
                        continue;
                    }
                    if is_transient(&error) && !server_error_retried {
                        server_error_retried = true;
                        self.note(format!("{error} — waiting 60s and retrying once"));
                        nap(RETRY_AFTER, self.cancel)?;
                        continue;
                    }
                    return Err(SamplerError::Ninja(error));
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// 撞上一个 429:爬一格退避阶梯,把这一轮剩下的小时预算砍一半,然后睡。
    ///
    /// 砍只做一次:再撞第二个 429 时该长的是等待时间,不是预算 ——
    /// 砍两次就只剩四分之一,一轮采样从一天拖成四天,而多出来的三天
    /// 并不会让对面更高兴。
    fn back_off(&mut self, retry_after: Option<u64>) -> Result<(), SamplerError> {
        self.rate_limit_hits += 1;
        if !self.slowed_down {
            self.slowed_down = true;
            self.pacer.budget.halve();
        }
        let wait = rate_limit_delay(self.rate_limit_hits, retry_after);
        self.note(format!(
            "poe.ninja asked us to slow down — waiting {}s (attempt {})",
            wait.as_secs(),
            self.rate_limit_hits
        ));
        nap(wait, self.cancel)
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
            resolve_league_url(&index, &self.config.league_name, &self.config.league_url)
                .ok_or(SamplerError::NoChallengeLeague(self.config.game))?;
        // 设置里那个联赛留空时,这一轮所有的库行都按刚认出来的这个短名存 ——
        // 否则它们会全挤在 `""` 这个键上,界面再也找不回来。设置里填了名字的
        // 那条路一个字都不变:老库里的行就是按那个键存的。
        if self.league_url.is_empty() {
            self.league_url = league_url.clone();
            self.note(format!(
                "{} league resolved to {league_url} from index-state",
                self.config.game
            ));
        }
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
            league_url: self.league_url.clone(),
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
        let league_url = self.league_url.clone();
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
                // 先记下"正在跑第几个":这一条要是撞上 429,状态行得说得出数字。
                self.mark_progress(done, total.max(done));
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
        // 这一步就一个请求,但它也可能吃 429,所以进度数字先立起来。
        self.mark_progress(0, 1);
        let response = self.search(&[])?;
        self.ensure_dictionaries(&response)?;

        let options = sample_options(&self.config.tuning);
        let partitions = {
            let gems = dictionary_slice(&response, "gem", &self.dictionaries);
            let items = dictionary_slice(&response, "item", &self.dictionaries);
            first_pass_partitions(league, &response, gems, items, &options)
        };
        self.store
            .enqueue_partitions(&self.league_url, &self.version, &partitions)?;
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
                .enqueue_partitions(&self.league_url, &self.version, &partitions)?;
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
            &self.league_url,
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
        let league_url = self.league_url.clone();
        let league_name = self.league_name.clone();
        // 分类表按代取:PoE1 的名字是**单数**,拿 PoE2 那张复数表去问是 404,
        // 而 404 在下面被当成"这一类跳过" —— 于是整页价格空着,一句错也不报。
        let types = unique_types_for(self.config.game);
        let total = count(types.len());
        let mut done = 0u32;
        for type_name in types {
            check_cancel(self.cancel)?;
            self.mark_progress(done, total);
            let name = league_name.clone();
            match self.fetch(|client| client.unique_prices(&name, type_name)) {
                Ok(overview) => {
                    // 计价基准币跟着这一份原文一起落库:接口自己说它是 divine
                    // 还是 exalted,程序不替它记(它换过一次,还会再换)。
                    self.store.replace_unique_prices(
                        &league_url,
                        type_name,
                        &overview.core.primary,
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
        let league_url = self.league_url.clone();
        // 一个请求都还没发就先建一份统计。词缀表是按 version 存的,而换一天
        // 就是换一个 version:不先建,词缀页会从今天开工那一刻起空到收工。
        self.checkpoint()?;
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
            // 每 10 个才报一次进度,但数字每一个都要更新:中间那 9 个撞上 429 时,
            // 状态行说的得是"角色 71/2000",不是"角色 0/0"。
            self.mark_progress(count(index), total);
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
            // 每 50 个重算一次词缀统计:一轮要跑一整天,词缀页不该等到最后
            // 一刻才有东西,中途关一次程序也不该前功尽弃。
            if position % CHARACTER_AGGREGATE_EVERY == 0 || position == queue.len() {
                self.checkpoint()?;
            }
        }
        Ok(())
    }

    /// 重建一次词缀统计,顺便报一句"采到哪了、这一小时还剩多少预算"。
    fn checkpoint(&mut self) -> Result<(), SamplerError> {
        self.rebuild_mods()?;
        let league_url = self.league_url.clone();
        let (_, characters, _) = self.store.character_counts(&league_url)?;
        let target = self.config.tuning.sample_target;
        let now = Instant::now();
        let used_this_hour = self.pacer.budget.used(now);
        let hourly_budget = self.pacer.budget.limit();
        (self.emit)(SamplerEvent::Sampled {
            characters,
            target,
            used_this_hour,
            hourly_budget,
            eta_secs: budget_eta_secs(target.saturating_sub(characters), hourly_budget),
        });
        Ok(())
    }

    // ---- 第 7 步:词缀统计 --------------------------------------------

    fn aggregate_stage(&mut self) -> Result<(), SamplerError> {
        self.stage = SamplerStage::Aggregated;
        let (characters, rows, broken) = self.rebuild_mods()?;
        self.progress(
            count(characters),
            count(characters + broken as usize),
            format!("{rows} mod rows from {characters} characters ({broken} unparseable)"),
        );
        Ok(())
    }

    /// 从库里**整个联赛**已抓到手的角色原文重建词缀统计。不出网。
    ///
    /// "整个联赛"是重点:详情按 `(联赛, 账号, 角色名)` 缓存,不带 version,
    /// 所以换一版快照只补新面孔,而统计要把以前采过的人全算进去。
    /// 只统计这一轮排队的那批人,等于每天把昨天的样本扔掉重来。
    ///
    /// 返回 (算进去几个人, 出了几行统计, 几份原文读不动)。
    fn rebuild_mods(&mut self) -> Result<(usize, usize, u32), SamplerError> {
        let league_url = self.league_url.clone();
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
        Ok((details.len(), stats.len(), broken))
    }

    // ---- 杂活 --------------------------------------------------------

    /// 推进阶段并广播。`Aggregated` 那一下顺便盖上 `finished_at`(存储层的规矩)。
    ///
    /// 写进库的是"走到过的最远那一步":续跑时前面几步会被空跑一遍,
    /// 照着空跑的结果写库会让阶段倒退(见 [`highest_stage`])。
    fn finish_stage(&mut self, stage: SamplerStage) -> Result<(), SamplerError> {
        self.reached = highest_stage(self.reached, stage);
        self.store.set_stage(
            &self.league_url,
            &self.version,
            self.reached.to_snapshot(),
            now_secs(),
        )?;
        self.stage = stage;
        (self.emit)(SamplerEvent::StageDone(stage));
        Ok(())
    }

    /// 只记下"跑到哪了",不发事件。
    ///
    /// 角色那一步每 10 个才报一次进度,中间那 9 个要是撞上 429,
    /// 状态行也得说得出真数字,而不是把它抹成 `0/0`。
    fn mark_progress(&mut self, done: u32, total: u32) {
        self.progress_done = done;
        self.progress_total = total;
    }

    /// 一条只带说明、沿用当前阶段进度数字的事件。出网那一层用它。
    fn note(&mut self, note: String) {
        let (done, total) = (self.progress_done, self.progress_total);
        self.progress(done, total, note);
    }

    fn progress(&mut self, done: u32, total: u32, note: String) {
        self.mark_progress(done, total);
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
            resolve_league_url(&index, "Forbidden Rites", "forbiddenrites").as_deref(),
            Some("fr2")
        );
        // 大小写不该影响(用户在设置里怎么打的都算)。
        assert_eq!(
            resolve_league_url(&index, "forbidden rites", "forbiddenrites").as_deref(),
            Some("fr2")
        );
        // 这一轮没索引这个联赛:名字查不到,只能用猜的那个。
        assert_eq!(
            resolve_league_url(&index, "Runes of Aldur", "runesofaldur").as_deref(),
            Some("runesofaldur")
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

    /// 一小时 100 个 = 36 秒一个,而且这个间隔从第一个请求起就生效。
    ///
    /// 这是这次改动的核心:老版本只管"两次之间隔 2 秒",于是一轮采样在头两
    /// 分钟就把 100 多个请求打光,第 130 个左右吃 429 —— 而 `Retry-After: 3600`
    /// 说得很清楚,对面数的是**这一小时一共发了多少个**,不是发得多密。
    #[test]
    fn an_hourly_budget_spreads_the_requests_across_the_hour() {
        let floor = Duration::from_secs(2);
        // 第一个请求不等。
        assert_eq!(budget_delay(0, 100, None, None, floor), Duration::ZERO);
        // 刚发过一个:补满 3600/100 = 36 秒。
        assert_eq!(
            budget_delay(1, 100, Some(Duration::ZERO), Some(Duration::ZERO), floor),
            Duration::from_secs(36)
        );
        // 上一个是 10 秒前发的,还差 26 秒。
        assert_eq!(
            budget_delay(
                1,
                100,
                Some(Duration::from_secs(10)),
                Some(Duration::from_secs(10)),
                floor
            ),
            Duration::from_secs(26)
        );
        // 已经隔够了就立刻走。
        assert_eq!(
            budget_delay(
                9,
                100,
                Some(Duration::from_secs(50)),
                Some(Duration::from_secs(400)),
                floor
            ),
            Duration::ZERO
        );
        // 预算减半 = 间隔加倍。
        assert_eq!(
            budget_delay(1, 50, Some(Duration::ZERO), Some(Duration::ZERO), floor),
            Duration::from_secs(72)
        );
    }

    /// 窗口满了就硬等最老的那个滚出这一小时 —— 这一条才是"绝不超额"的保证。
    #[test]
    fn a_full_window_waits_for_the_oldest_request_to_age_out() {
        let floor = Duration::from_secs(2);
        // 100 个都在窗口里,最老的那个是 50 分钟前发的:还要等 10 分钟。
        assert_eq!(
            budget_delay(
                100,
                100,
                Some(Duration::from_secs(30)),
                Some(Duration::from_secs(3_000)),
                floor
            ),
            Duration::from_secs(600)
        );
        // 最老的刚好满一小时:硬顶那一条不再有话说,只剩摊平那一条。
        assert_eq!(
            budget_delay(100, 100, Some(HOUR), Some(HOUR), floor),
            Duration::ZERO
        );
        // 超额了(设置被人改小)也只等到最老的那个滚出去,不会算出一个负数
        // 绕回天文数字。上一个请求是两分钟前发的,摊平那一条已经没话说了。
        assert_eq!(
            budget_delay(
                140,
                100,
                Some(Duration::from_secs(120)),
                Some(Duration::from_secs(3_599)),
                floor
            ),
            Duration::from_secs(1)
        );
    }

    /// `min_request_gap_ms` 是**下限**:预算大到摊平出来比它还密时由它兜住。
    #[test]
    fn the_minimum_gap_is_a_floor_under_the_budget() {
        let floor = Duration::from_secs(2);
        // 一小时 36,000 个 = 0.1 秒一个,但设置说最少隔 2 秒。
        assert_eq!(
            budget_delay(1, 36_000, Some(Duration::ZERO), Some(Duration::ZERO), floor),
            floor
        );
        // 预算是 0 的时候当成 1(一小时一个),而不是除以零。
        assert_eq!(
            budget_delay(1, 0, Some(Duration::ZERO), Some(Duration::ZERO), floor),
            HOUR
        );
    }

    /// 剩下的角色数 ÷ 小时预算 = 还要多少小时。状态行上那句"预计还需"。
    #[test]
    fn the_eta_comes_straight_out_of_the_hourly_budget() {
        // 一个都没采,2,000 个人、一小时 100 个 = 20 小时。
        assert_eq!(budget_eta_secs(2_000, 100), 20 * 3_600);
        // 采了 1,850 个,还剩 150 个 = 5.4 小时。
        assert_eq!(budget_eta_secs(150, 100), 5_400);
        // 采满了就是 0,不是一个负数绕回来的天文数字。
        assert_eq!(budget_eta_secs(0, 100), 0);
        // 撞过 429、预算砍半:同样的活要两倍的时间。
        assert_eq!(budget_eta_secs(2_000, 50), 40 * 3_600);
        // 预算是 0 时当成 1,而不是除以零。
        assert_eq!(budget_eta_secs(1, 0), 3_600);
    }

    /// 一份最小的角色详情原文:一只带生命的稀有戒指。
    fn ring_detail(account: &str, name: &str) -> String {
        format!(
            r#"{{"account":"{account}","name":"{name}","items":[
                 {{"itemSlot": 8, "itemData": {{"inventoryId": "Ring", "rarity": "Rare",
                   "mods": {{"explicit": [
                     {{"id": "IncreasedLife8", "stats": {{"base_maximum_life": 115}}}}]}}}}}}]}}"#
        )
    }

    /// 换一天(= 换一版快照)之后,统计里必须还有昨天采到的那些人。
    ///
    /// 这是那个"一整天空白页"的坑:角色详情按 `(联赛, 账号, 角色名)` 缓存、
    /// **不带 version**,词缀表却是按 version 存的。一小时 100 个请求下,
    /// 2,000 个人要采一整天 —— 今天开工换了 version,不马上用昨天的原文重建
    /// 一份挂在新 version 上,词缀页就从今天早上空到今晚收工。
    #[test]
    fn a_new_snapshot_rebuilds_the_stats_from_every_character_ever_cached() {
        let store = NinjaStore::open_in_memory().expect("store");
        let league = "forbiddenrites";
        // 昨天那一轮:两个人,原文已经在库里。
        store
            .enqueue_partitions(
                league,
                "yesterday",
                &[Partition::new(PartitionTier::Whole, Vec::new())],
            )
            .expect("enqueue");
        let queued: Vec<SampledCharacter> = [
            ("heygyus-0416", "ResurrectForbidden"),
            ("dota2enjoyer-1809", "KingPinUwU"),
        ]
        .iter()
        .map(|(account, name)| SampledCharacter {
            account: (*account).to_owned(),
            name: (*name).to_owned(),
            class: "Gemling Legionnaire".to_owned(),
            level: 98,
            from_partition: String::new(),
            tier: PartitionTier::Whole,
        })
        .collect();
        store
            .complete_partition(league, "yesterday", "", 2, &[], &queued, 1_000)
            .expect("complete");
        for character in &queued {
            store
                .complete_character(
                    league,
                    &character.account,
                    &character.name,
                    "yesterday",
                    &ring_detail(&character.account, &character.name),
                    1_000,
                )
                .expect("done");
        }

        // 今天:新的 version,而且**一个请求都不发**(`max_characters = 0`)。
        let mut config = brisk_config();
        config.max_characters = Some(0);
        let cancel = AtomicBool::new(false);
        let seen = std::cell::RefCell::new(Vec::new());
        let emit = |event: SamplerEvent| seen.borrow_mut().push(event);
        let mut sampler = Sampler::new(store, &config, &cancel, &emit);
        sampler.version = "today".to_owned();
        sampler.character_stage().expect("character stage");

        let stats = sampler
            .store
            .slot_mods(league, "today", None, None, 0.0)
            .expect("mods");
        let life = stats
            .iter()
            .find(|row| row.stat_id == "base_maximum_life")
            .expect("新 version 下就该有统计,而不是等到今晚聚合那一步");
        assert_eq!(
            life.characters, 2,
            "昨天采到的人不能因为换了一版快照就从统计里消失"
        );

        // 报出来的也是**跨快照**的累计数,不是"这一轮抓了几个"。
        let sampled = seen
            .borrow()
            .iter()
            .find_map(|event| match event {
                SamplerEvent::Sampled {
                    characters,
                    target,
                    hourly_budget,
                    eta_secs,
                    ..
                } => Some((*characters, *target, *hourly_budget, *eta_secs)),
                _ => None,
            })
            .expect("重建一次就该报一句进度");
        assert_eq!(sampled.0, 2);
        assert_eq!(sampled.1, 2_000);
        assert_eq!(sampled.3, budget_eta_secs(1_998, sampled.2));
    }

    /// 滑动窗口自己会把过期的请求丢掉,`used` 报的就是"本小时已用"。
    #[test]
    fn the_window_forgets_requests_older_than_an_hour() {
        let mut budget = HourlyBudget::new(100, Duration::from_secs(2));
        let now = Instant::now();
        assert_eq!(budget.used(now), 0);

        // 两个是 61 分钟前发的(已经滚出去了),一个是 10 分钟前。
        budget.record(now - Duration::from_secs(3_700));
        budget.record(now - Duration::from_secs(3_650));
        budget.record(now - Duration::from_secs(600));
        assert_eq!(budget.used(now), 1);
        // 最近一个是 10 分钟前,早就隔够 36 秒了。
        assert_eq!(budget.delay(now), Duration::ZERO);

        // 429 之后砍一半:100 → 50 → 还是 50(只砍一次是调用方的规矩)。
        assert_eq!(budget.limit(), 100);
        budget.halve();
        assert_eq!(budget.limit(), 50);
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

    /// 一个脚本化的"客户端":按顺序把预先排好的结果一个个吐出来。
    ///
    /// 真的 [`NinjaClient`] 要联网,而 429 这一段的全部风险都在"第几次、
    /// 等多久、要不要放弃"上,跟网络没关系 —— 所以拿一份剧本演给它看就够了。
    fn scripted(
        script: Vec<Result<u8, NinjaError>>,
    ) -> impl Fn(&NinjaClient) -> Result<u8, NinjaError> {
        let queue = std::cell::RefCell::new(script.into_iter());
        move |_client| queue.borrow_mut().next().expect("剧本演完了还在要下一条")
    }

    fn limited(retry_after_secs: Option<u64>) -> NinjaError {
        NinjaError::RateLimited { retry_after_secs }
    }

    /// 一轮"节奏尽量快"的采样,好让测试不用真的睡:预算大到摊平也只有
    /// 几毫秒,下限再压到 2 毫秒。
    fn brisk_config() -> SamplerConfig {
        let mut config = SamplerConfig::new("forbiddenrites", "Forbidden Rites");
        config.tuning.max_requests_per_hour = 1_800_000;
        config.tuning.min_request_gap_ms = 2;
        config
    }

    /// 连着两个 429 也不能放弃 —— 这就是那次"角色 0/0"卡住的根:
    /// 老版本只重试一次,第二个 429 就被当成"这个角色抓不到了"抛出去,
    /// 于是 2,000 个角色一人烧两个请求 + 60 秒,一整夜也采不完一个人。
    #[test]
    fn a_rate_limit_is_waited_out_instead_of_dropped_after_one_retry() {
        let store = NinjaStore::open_in_memory().expect("store");
        let config = brisk_config();
        let cancel = AtomicBool::new(false);
        let emit = |_: SamplerEvent| {};
        let mut sampler = Sampler::new(store, &config, &cancel, &emit);

        // 服务端自己说"0 秒后再来",所以这条测试不用真的睡满退避阶梯。
        let call = scripted(vec![Err(limited(Some(0))), Err(limited(Some(0))), Ok(7)]);
        assert_eq!(sampler.fetch(call).expect("第三次该通了"), 7);
    }

    /// 429 那条状态行必须说得出"跑到哪了"。
    ///
    /// 老版本在出网那一层写死 `progress(0, 0, …)`,于是界面上永远是
    /// "角色 0/0 · 429" —— 队列里明明还有 2,207 个人在排队。
    #[test]
    fn the_rate_limit_note_keeps_the_stage_counters() {
        let store = NinjaStore::open_in_memory().expect("store");
        let config = brisk_config();
        let cancel = AtomicBool::new(false);
        let seen = std::cell::RefCell::new(Vec::new());
        let emit = |event: SamplerEvent| seen.borrow_mut().push(event);
        let mut sampler = Sampler::new(store, &config, &cancel, &emit);
        sampler.stage = SamplerStage::Characters;

        // 阶段刚报过"第 70 个,共 2,000 个"。
        sampler.progress(70, 2_000, "KingPinUwU".to_owned());
        let call = scripted(vec![Err(limited(Some(0))), Ok(1)]);
        sampler.fetch(call).expect("第二次该通了");

        let notes: Vec<(u32, u32, String)> = seen
            .borrow()
            .iter()
            .filter_map(|event| match event {
                SamplerEvent::Progress {
                    done, total, note, ..
                } => Some((*done, *total, note.clone())),
                _ => None,
            })
            .collect();
        let slow_down = notes
            .iter()
            .find(|(_, _, note)| note.contains("slow down"))
            .expect("撞上 429 该说一句人话");
        assert_eq!(
            (slow_down.0, slow_down.1),
            (70, 2_000),
            "429 那条不该把进度数字抹成 0/0"
        );
        assert!(slow_down.2.contains("attempt 1"), "{}", slow_down.2);
    }

    /// 撞过一次 429 之后,这一轮剩下的**小时预算砍一半**:配额只是估出来的,
    /// 吃到 429 就说明估高了,按原来的数字跑完只会再撞一次。
    #[test]
    fn one_rate_limit_halves_the_hourly_budget_for_the_rest_of_the_run() {
        let store = NinjaStore::open_in_memory().expect("store");
        // 用 brisk 的那个天文数字当预算,测试才不用真的等 36 秒一个请求;
        // "砍一半"这件事和数字大小无关。
        let config = brisk_config();
        let cancel = AtomicBool::new(false);
        let emit = |_: SamplerEvent| {};
        let mut sampler = Sampler::new(store, &config, &cancel, &emit);
        assert_eq!(sampler.pacer.budget.limit(), 1_800_000);

        sampler
            .fetch(scripted(vec![Err(limited(Some(0))), Ok(1)]))
            .expect("重试该通");
        assert_eq!(sampler.pacer.budget.limit(), 900_000);

        // 只砍一次:再撞一个 429 不该变成四分之一、八分之一……那是把一天拖成四天。
        sampler
            .fetch(scripted(vec![Err(limited(Some(0))), Ok(1)]))
            .expect("重试该通");
        assert_eq!(sampler.pacer.budget.limit(), 900_000);
    }

    /// 参考价必须排在角色详情**前面**。
    ///
    /// 这是那次卡住最贵的一笔账:参考价只有 6 个请求、几秒钟,角色详情却要
    /// 跑一整天。排在后面的那一版里,角色详情一被限流,暗金页的
    /// 参考价 / 挂单数 / 7 天三列就全是"—",而它们本来早就该到手了。
    #[test]
    fn the_cheap_prices_step_runs_before_the_long_character_step() {
        use SamplerStep::{Aggregated, Characters, Facets, Prices};

        assert_eq!(
            stage_plan(SamplerStage::Aggregated),
            vec![Facets, Prices, Characters, Aggregated]
        );
        assert_eq!(
            stage_plan(SamplerStage::Characters),
            vec![Facets, Prices, Characters]
        );
        assert_eq!(stage_plan(SamplerStage::Facets), vec![Facets, Prices]);
        // `Planned` 是个"只刷新快照行"的停车位,但参考价照样跑:
        // 它每一轮都跑,和跑到哪一步无关。
        assert_eq!(stage_plan(SamplerStage::Planned), vec![Prices]);
    }

    /// 退避阶梯:服务端说了听服务端的,没说就 60 → 120 → 300 → 600 顶住。
    #[test]
    fn the_backoff_ladder_climbs_and_then_holds() {
        assert_eq!(rate_limit_delay(1, None), Duration::from_secs(60));
        assert_eq!(rate_limit_delay(2, None), Duration::from_secs(120));
        assert_eq!(rate_limit_delay(3, None), Duration::from_secs(300));
        assert_eq!(rate_limit_delay(4, None), Duration::from_secs(600));
        // 封顶之后一直是 600,不会越滚越大。
        assert_eq!(rate_limit_delay(9, None), Duration::from_secs(600));
        // 第 0 次不该越界(理论上不会发生,但一次 panic 就是整轮采样没了)。
        assert_eq!(rate_limit_delay(0, None), Duration::from_secs(60));

        // 服务端说了话就听它的,哪怕它说的比阶梯短。
        assert_eq!(rate_limit_delay(4, Some(5)), Duration::from_secs(5));
        assert_eq!(rate_limit_delay(1, Some(0)), Duration::ZERO);
    }

    /// 429 和 5xx 值得再等一分钟,404 和解析失败不值得。
    #[test]
    fn only_rate_limits_and_server_errors_are_retried() {
        assert!(is_transient(&NinjaError::RateLimited {
            retry_after_secs: None
        }));
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
        let mut pacer = Pacer::new(100, Duration::from_secs(1));
        assert!(matches!(
            pacer.before_request(&cancel),
            Err(SamplerError::Cancelled)
        ));
    }

    /// 一轮 PoE1 采样问的是 PoE1 的接口,写的是 PoE1 那个库文件。
    ///
    /// 两件事都必须钉死,因为**搞错任何一件都不会报错**:客户端指错代,拿回来的
    /// 是另一代的榜单(联赛名对不上,一整轮全是空);库写错文件,PoE1 的行会混进
    /// PoE2 那张表里,而两代的分区键长得一模一样,混进去就再也分不开了。
    ///
    /// 客户端那一半靠一个"假请求"来问:`fetch` 收的是一个拿得到客户端的闭包,
    /// 所以让它什么都别抓、只报一句"你问的是哪一代"就行 —— 一个网络请求都不发。
    #[test]
    fn a_poe1_run_talks_to_poe1_and_writes_only_the_poe1_store() {
        let dir = std::env::temp_dir().join(format!("pnd-sampler-poe1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = SamplerConfig::for_game(Game::Poe1, "allflame", "Allflame");
        config.db_path = dir.join(pnd_storage::ninja_db_file_name(Game::Poe1));
        assert_eq!(config.game, Game::Poe1);

        let store = NinjaStore::open_for(Game::Poe1, &dir).expect("store");
        let cancel = AtomicBool::new(false);
        let emit = |_: SamplerEvent| {};
        let mut sampler = Sampler::new(store, &config, &cancel, &emit);

        let asked = sampler
            .fetch(|client: &NinjaClient| Ok(client.game()))
            .expect("假请求");
        assert_eq!(asked, Game::Poe1, "客户端得指向 PoE1");

        sampler
            .store
            .upsert_snapshot(&SnapshotRow {
                league_url: "allflame".to_owned(),
                version: "1707-20260908-44259".to_owned(),
                snapshot_name: "allflame".to_owned(),
                total_characters: 124_459,
                stage: SnapshotStage::Facets,
                started_at: 1_000,
                finished_at: None,
            })
            .expect("write");
        assert!(dir.join("ninja-poe1.sqlite").is_file());
        assert!(
            !dir.join("ninja.sqlite").exists(),
            "PoE1 的一轮不该碰 PoE2 那个库"
        );

        drop(sampler);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PoE2 那条路一个字都没变:不带参数的构造还是 PoE2 + 老文件名。
    #[test]
    fn the_default_config_is_still_a_poe2_run() {
        let config = SamplerConfig::new("forbiddenrites", "Forbidden Rites");
        assert_eq!(config.game, Game::Poe2);
        assert_eq!(config.db_path, pnd_storage::default_ninja_db_path());
    }

    /// 两代的暗金分类表不是同一张:PoE1 是单数、少一个 charm。
    ///
    /// 拿 PoE2 那张表去问 PoE1 不是"空榜",是 **404**(2026-09-09 实测),
    /// 而参考价那一步对 404 的处置是"跳过这一类" —— 于是 PoE1 的暗金页会
    /// 一列价格都没有,却一句错也不报。
    #[test]
    fn each_game_asks_for_its_own_unique_categories() {
        assert_eq!(unique_types_for(Game::Poe2).len(), 6);
        assert!(unique_types_for(Game::Poe2).contains(&"UniqueWeapons"));
        assert_eq!(unique_types_for(Game::Poe1).len(), 5);
        assert!(unique_types_for(Game::Poe1).contains(&"UniqueWeapon"));
    }

    /// 设置里 PoE1 联赛留空 = "用当季挑战联赛",而不是"没有联赛"。
    #[test]
    fn an_empty_league_setting_resolves_to_the_current_challenge_league() {
        let index = IndexState {
            build_leagues: vec![
                pnd_ninja::index_state::LeagueRef {
                    name: "Allflame".to_owned(),
                    url: "allflame".to_owned(),
                    ..pnd_ninja::index_state::LeagueRef::default()
                },
                pnd_ninja::index_state::LeagueRef {
                    name: "Standard".to_owned(),
                    url: "standard".to_owned(),
                    ..pnd_ninja::index_state::LeagueRef::default()
                },
            ],
            snapshot_versions: vec![SnapshotVersion {
                url: "allflame".to_owned(),
                name: "Allflame".to_owned(),
                ..SnapshotVersion::default()
            }],
            ..IndexState::default()
        };

        // 两个名字都空:去 index-state 问当季挑战联赛。
        assert_eq!(
            resolve_league_url(&index, "", ""),
            Some("allflame".to_owned())
        );
        // 填了名字就以填的为准,当季那条不许压过用户的话。
        assert_eq!(
            resolve_league_url(&index, "Standard", "standard"),
            Some("standard".to_owned())
        );
        // 一条挑战联赛都挑不出来时给 `None` —— 采到 Standard 去是一整天的
        // 请求配额喂给一份没人看的数据。
        assert_eq!(resolve_league_url(&IndexState::default(), "", ""), None);
    }

    /// 分区计划从**这一份响应真的带回来的分面**里长出来,不假设两代一样。
    ///
    /// PoE1 没有 `spiritgems`,却多八张 PoE2 没有的字典(`bandit`、`tattoo`……)。
    /// 计划里要是写死"去拿 spiritgems 那一栏",PoE1 这一轮就会在一个不存在的
    /// 分面上算出一张空清单。
    #[test]
    fn the_partition_plan_tolerates_facets_only_one_game_has() {
        use pnd_ninja::plan::{SampleOptions, first_pass_partitions};

        // 一份 PoE1 形状的响应:有 items、没有 skills / spiritgems,
        // 外加一个 PoE2 根本没有的 `bandit` 分面。
        let poe1 = SearchResponse {
            total: 124_459,
            facets: vec![
                Facet {
                    name: "items".to_owned(),
                    kind: "item".to_owned(),
                    entries: vec![FacetEntry {
                        index: 0,
                        count: 9_000,
                    }],
                },
                Facet {
                    name: "bandit".to_owned(),
                    kind: "bandit".to_owned(),
                    entries: vec![FacetEntry {
                        index: 0,
                        count: 5_000,
                    }],
                },
            ],
            dictionaries: Vec::new(),
            columns: Vec::new(),
        };
        let league = LeagueBuild {
            league_url: "allflame".to_owned(),
            total: 124_459,
            statistics: vec![pnd_ninja::index_state::ClassShare {
                class: "Champion".to_owned(),
                percentage: 12.0,
                trend: 0,
            }],
            ..LeagueBuild::default()
        };
        let items = vec!["Headhunter".to_owned()];
        let plan = first_pass_partitions(&league, &poe1, &[], &items, &SampleOptions::default());

        let keys: Vec<&str> = plan.iter().map(|entry| entry.key.as_str()).collect();
        assert!(keys.contains(&""), "全联赛那条永远在");
        assert!(keys.contains(&"class=Champion"));
        assert!(keys.contains(&"items=Headhunter"));
        // 没有 skills 分面就一条技能分区都不排,而不是排一堆问不出东西的。
        assert!(!keys.iter().any(|key| key.starts_with("skills=")));
        // 我们不认识的分面也不会把这一步弄炸。
        assert_eq!(facet_rows(&poe1, &HashMap::new()).len(), 2);
    }

    /// 两个 crate 只许有**一个** `Game`。
    ///
    /// 一度有两个:`pnd_domain::Game`(交易站那半边用)和 `pnd_ninja::client::Game`
    /// (poe.ninja 那半边用)。写法一模一样,所以谁也不会在编译期报错,直到有一
    /// 天要把设置里那一个传给采样管线 —— 那时候才发现它们是两个互不认识的类型。
    /// 这条测试把"它们是同一个类型"钉死:赋值过得去就是同一个。
    #[test]
    fn the_two_crates_share_one_game_enum() {
        let from_domain: pnd_ninja::client::Game = pnd_domain::Game::Poe1;
        assert_eq!(from_domain.as_str(), "poe1");
        assert_eq!(pnd_ninja::client::Game::default(), pnd_domain::Game::Poe2);
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
