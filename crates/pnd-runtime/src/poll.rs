//! 轮询调度:每条搜索下一次该在什么时候跑。
//!
//! 纯逻辑,不碰时钟也不碰网络 —— `now` 一律由调用方传进来。这样"退避到底
//! 退了多久"这种问题可以在测试里一秒钟跑完两个小时,而不用真的等。
//!
//! 四条规矩:
//! - 多条搜索**错开起跑**。同时加三条搜索,如果三条都在同一秒醒,网关会连着
//!   发三次 search,预算的账在瞬间被打满,而且这种整齐的节奏一看就是机器。
//! - 失败了就**指数退避**,封顶 30 分钟。接口挂了或者网断了,原样的节奏反复
//!   捅它没有意义。
//! - WebSocket 健康的时候轮询**放宽**到另一档:有秒推兜着,轮询只是保险。
//! - 再勤也不能勤过**预算地板**([`budget_floor_interval`])。设置里那两个
//!   数字是"想多久一次",地板是"6 小时 299 次的额度分给这几条搜索之后,
//!   最快能多久一次" —— 两者取大。

use std::collections::BTreeMap;

use pnd_domain::WatchId;
use pnd_settings::WatcherTuning;

/// 退避的天花板。半小时还没好的东西,再等下去也不差这一会儿,
/// 但也不能真的停掉 —— 用户不会想手动点重试。
const MAX_BACKOFF_SECONDS: u64 = 1800;

/// 一轮轮询的结局。只有两种:成功要恢复正常节奏,失败要退避。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollOutcome {
    Ok,
    Failed,
}

/// 一条搜索的调度状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollEntry {
    /// 下一次该跑的 unix 秒。
    pub next_at: i64,
    /// 这一档的间隔(秒)。退避期间这里就是退避后的间隔,界面照着显示即可。
    pub interval: u64,
    pub failures: u32,
    /// 这条搜索的 WebSocket 是不是活的。第一版永远是 false(live 在第二阶段)。
    pub live_healthy: bool,
}

/// 所有搜索的下一次轮询时刻。
///
/// 用 `BTreeMap` 而不是 `HashMap`:`due()` 和状态广播的顺序因此是稳定的,
/// 调试的时候两次运行看到的顺序一样。
#[derive(Debug, Clone, Default)]
pub struct PollScheduler {
    entries: BTreeMap<WatchId, PollEntry>,
    /// 每条搜索最快能多久轮一次(秒)。0 = 还没算过,不设限。
    budget_floor: u64,
}

impl PollScheduler {
    #[must_use]
    pub fn new() -> PollScheduler {
        PollScheduler::default()
    }

    /// 加一条搜索,或者更新一条已有搜索的间隔。
    ///
    /// **新加的**那条按 `stagger_index / count` 的比例把第一次轮询往后推:
    /// 三条搜索、间隔 300 秒,就分别在 0、100、200 秒后起跑。
    /// **已有的**那条只换间隔,不动 `next_at` —— 用户在设置页改个标签不该让
    /// 所有搜索重新排队。
    pub fn upsert(
        &mut self,
        watch_id: WatchId,
        interval: u64,
        now: i64,
        stagger_index: usize,
        count: usize,
    ) {
        let interval = interval.max(self.budget_floor);
        if let Some(entry) = self.entries.get_mut(&watch_id) {
            entry.interval = interval;
            return;
        }
        let count = count.max(1);
        let offset = interval.saturating_mul(stagger_index.min(count) as u64) / count as u64;
        self.entries.insert(
            watch_id,
            PollEntry {
                next_at: now + offset as i64,
                interval,
                failures: 0,
                live_healthy: false,
            },
        );
    }

    pub fn remove(&mut self, watch_id: &WatchId) {
        self.entries.remove(watch_id);
    }

    #[must_use]
    pub fn entry(&self, watch_id: &WatchId) -> Option<&PollEntry> {
        self.entries.get(watch_id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 最早的那次轮询在什么时候。主循环拿它算"该睡多久"。
    #[must_use]
    pub fn next_deadline(&self) -> Option<i64> {
        self.entries.values().map(|entry| entry.next_at).min()
    }

    /// 现在该跑的搜索,早该跑的排在前面。
    #[must_use]
    pub fn due(&self, now: i64) -> Vec<WatchId> {
        let mut due: Vec<(&WatchId, &PollEntry)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.next_at <= now)
            .collect();
        due.sort_by_key(|(id, entry)| (entry.next_at, (*id).clone()));
        due.into_iter().map(|(id, _)| id.clone()).collect()
    }

    /// 一轮跑完了,排下一轮。
    ///
    /// 成功:失败计数清零,回到正常档(或者 live 健康时的宽松档)。
    /// 失败:失败计数 +1,间隔翻 `2^失败次数` 倍,封顶半小时。
    pub fn reschedule(
        &mut self,
        watch_id: &WatchId,
        now: i64,
        outcome: PollOutcome,
        tuning: &WatcherTuning,
    ) {
        let floor = self.budget_floor;
        let Some(entry) = self.entries.get_mut(watch_id) else {
            return;
        };
        let base = base_interval(entry.live_healthy, tuning, floor);
        match outcome {
            PollOutcome::Ok => {
                entry.failures = 0;
                entry.interval = base;
            }
            PollOutcome::Failed => {
                entry.failures = entry.failures.saturating_add(1);
                entry.interval = backoff(base, entry.failures);
            }
        }
        entry.next_at = now + entry.interval as i64;
    }

    /// 换一条预算地板。搜索条数一变(加了一条、停了一条)就重算一次。
    ///
    /// 只影响**以后**算出来的间隔:已经排好的那一轮不动 —— 用户刚加了第二条
    /// 搜索,第一条没理由为此推迟。
    pub fn set_budget_floor(&mut self, floor: u64) {
        self.budget_floor = floor;
    }

    #[must_use]
    pub fn budget_floor(&self) -> u64 {
        self.budget_floor
    }

    /// 换一条搜索的 live 档位。只影响**下一次** `reschedule` 算出来的间隔:
    /// WebSocket 刚连上就把已经排好的这一轮取消掉,没有意义。
    pub fn set_live_healthy(&mut self, watch_id: &WatchId, healthy: bool) {
        if let Some(entry) = self.entries.get_mut(watch_id) {
            entry.live_healthy = healthy;
        }
    }

    /// 用户点了"立刻查一次"。
    pub fn poll_now(&mut self, watch_id: &WatchId, now: i64) {
        if let Some(entry) = self.entries.get_mut(watch_id) {
            entry.next_at = now;
        }
    }

    /// 一轮**刚发出去**时把 `next_at` 先推到一个间隔之后。
    ///
    /// 没有这一步,`next_at` 会一直停在过去:主循环每次都看到这条搜索"该跑了",
    /// 于是 `recv_timeout(0)` 空转,直到回信到达才安静下来。请求飞在路上的
    /// 那几秒钟里,时间表上就该已经写着下一轮。
    pub fn defer(&mut self, watch_id: &WatchId, now: i64) {
        if let Some(entry) = self.entries.get_mut(watch_id) {
            entry.next_at = now + entry.interval as i64;
        }
    }
}

/// 正常情况下的间隔:WS 活着走宽松档,否则走普通档;两档都不能快过地板。
fn base_interval(live_healthy: bool, tuning: &WatcherTuning, floor: u64) -> u64 {
    let configured = if live_healthy {
        tuning.poll_interval_when_live_seconds
    } else {
        tuning.poll_interval_seconds
    };
    configured.max(floor)
}

/// 这么多条搜索一起跑,每条最快能多久轮一次而不超预算。
///
/// 一轮轮询发一次 search,所以一个窗口里的搜索总数是
/// `搜索条数 × (窗口 / 间隔)`;要它不超过 `budget_per_window`,反解出来就是
/// `间隔 ≥ 搜索条数 × 窗口 / 预算`。向上取整 —— 差一次也是超。
///
/// 默认那份预算(6 小时 299 次)下:一条搜索 73 秒,两条 145 秒,三条 217 秒。
/// 一条搜索都没启用时是 0(不设限):没人花额度,也就没有地板可言。
#[must_use]
pub fn budget_floor_interval(
    enabled_watches: usize,
    budget_per_window: u32,
    window_secs: u32,
) -> u64 {
    if enabled_watches == 0 {
        return 0;
    }
    // 预算是 0 就除不下去。当成 1 次:算出来是"一个窗口跑一轮",
    // 已经是能给的最慢节奏了,再慢也没有意义。
    let budget = u64::from(budget_per_window.max(1));
    let total = u64::from(window_secs).saturating_mul(enabled_watches as u64);
    total.div_ceil(budget)
}

/// `interval * 2^failures`,封顶半小时。`checked_pow` 兜住"连挂 40 轮"
/// 这种情况 —— 溢出会绕回一个很小的数,那就成了失败越多捅得越勤。
fn backoff(interval: u64, failures: u32) -> u64 {
    let factor = 2u64.checked_pow(failures).unwrap_or(u64::MAX);
    interval
        .checked_mul(factor)
        .unwrap_or(MAX_BACKOFF_SECONDS)
        .min(MAX_BACKOFF_SECONDS)
}

#[cfg(test)]
mod poll_tests {
    use super::*;

    fn watch(id: &str) -> WatchId {
        WatchId(id.to_string())
    }

    fn tuning() -> WatcherTuning {
        WatcherTuning {
            poll_interval_seconds: 300,
            poll_interval_when_live_seconds: 900,
            ..WatcherTuning::default()
        }
    }

    #[test]
    fn staggers_the_first_run_of_each_watch() {
        let mut scheduler = PollScheduler::new();
        for (index, id) in ["a", "b", "c"].iter().enumerate() {
            scheduler.upsert(watch(id), 300, 1_000, index, 3);
        }
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 1_000);
        assert_eq!(scheduler.entry(&watch("b")).unwrap().next_at, 1_100);
        assert_eq!(scheduler.entry(&watch("c")).unwrap().next_at, 1_200);
        assert_eq!(scheduler.next_deadline(), Some(1_000));
    }

    #[test]
    fn a_single_watch_starts_right_away() {
        let mut scheduler = PollScheduler::new();
        scheduler.upsert(watch("a"), 300, 1_000, 0, 1);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 1_000);
        assert_eq!(scheduler.due(1_000), vec![watch("a")]);
    }

    #[test]
    fn upserting_again_only_changes_the_interval() {
        let mut scheduler = PollScheduler::new();
        scheduler.upsert(watch("a"), 300, 1_000, 0, 1);
        scheduler.reschedule(&watch("a"), 1_000, PollOutcome::Ok, &tuning());
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 1_300);

        scheduler.upsert(watch("a"), 600, 5_000, 0, 1);
        let entry = scheduler.entry(&watch("a")).unwrap();
        assert_eq!(entry.interval, 600);
        assert_eq!(entry.next_at, 1_300, "已经排好的这一轮不该被挪动");
    }

    #[test]
    fn due_returns_the_oldest_first() {
        let mut scheduler = PollScheduler::new();
        scheduler.upsert(watch("a"), 300, 1_000, 0, 3);
        scheduler.upsert(watch("b"), 300, 1_000, 1, 3);
        scheduler.upsert(watch("c"), 300, 1_000, 2, 3);

        assert_eq!(scheduler.due(1_000), vec![watch("a")]);
        assert_eq!(scheduler.due(1_100), vec![watch("a"), watch("b")]);
        assert_eq!(
            scheduler.due(1_500),
            vec![watch("a"), watch("b"), watch("c")]
        );
        assert!(scheduler.due(999).is_empty());
    }

    #[test]
    fn failures_back_off_exponentially_and_stop_at_half_an_hour() {
        let mut scheduler = PollScheduler::new();
        let tuning = tuning();
        scheduler.upsert(watch("a"), 300, 0, 0, 1);

        scheduler.reschedule(&watch("a"), 0, PollOutcome::Failed, &tuning);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 600);
        scheduler.reschedule(&watch("a"), 600, PollOutcome::Failed, &tuning);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 600 + 1_200);
        // 第三次是 300*8 = 2400,已经超过封顶。
        scheduler.reschedule(&watch("a"), 0, PollOutcome::Failed, &tuning);
        let entry = scheduler.entry(&watch("a")).unwrap();
        assert_eq!(entry.failures, 3);
        assert_eq!(entry.interval, MAX_BACKOFF_SECONDS);

        // 连挂几十轮也不会因为溢出绕回一个小间隔。
        for _ in 0..40 {
            scheduler.reschedule(&watch("a"), 0, PollOutcome::Failed, &tuning);
        }
        assert_eq!(
            scheduler.entry(&watch("a")).unwrap().interval,
            MAX_BACKOFF_SECONDS
        );

        // 成功一次就回到正常档。
        scheduler.reschedule(&watch("a"), 10_000, PollOutcome::Ok, &tuning);
        let entry = scheduler.entry(&watch("a")).unwrap();
        assert_eq!(entry.failures, 0);
        assert_eq!(entry.interval, 300);
        assert_eq!(entry.next_at, 10_300);
    }

    #[test]
    fn a_healthy_websocket_relaxes_the_polling() {
        let mut scheduler = PollScheduler::new();
        let tuning = tuning();
        scheduler.upsert(watch("a"), 300, 0, 0, 1);

        scheduler.set_live_healthy(&watch("a"), true);
        scheduler.reschedule(&watch("a"), 1_000, PollOutcome::Ok, &tuning);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 1_900);

        scheduler.set_live_healthy(&watch("a"), false);
        scheduler.reschedule(&watch("a"), 2_000, PollOutcome::Ok, &tuning);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 2_300);
    }

    #[test]
    fn poll_now_and_defer_move_the_next_run() {
        let mut scheduler = PollScheduler::new();
        scheduler.upsert(watch("a"), 300, 0, 0, 1);
        scheduler.reschedule(&watch("a"), 1_000, PollOutcome::Ok, &tuning());
        assert!(scheduler.due(1_000).is_empty());

        scheduler.poll_now(&watch("a"), 1_000);
        assert_eq!(scheduler.due(1_000), vec![watch("a")]);

        // 请求一发出去,时间表上就写着下一轮,主循环不会再把它算成"该跑了"。
        scheduler.defer(&watch("a"), 1_000);
        assert!(scheduler.due(1_000).is_empty());
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 1_300);
    }

    /// 地板就是"额度分完之后每条搜索最快多久一次"。
    #[test]
    fn the_budget_floor_is_the_window_divided_by_the_budget() {
        // 6 小时 299 次:一条搜索 72.2 秒,向上取整 73 —— 差一次也是超。
        assert_eq!(budget_floor_interval(1, 299, 21_600), 73);
        assert_eq!(budget_floor_interval(2, 299, 21_600), 145);
        assert_eq!(budget_floor_interval(3, 299, 21_600), 217);
        // 整除的时候不多加一秒。
        assert_eq!(budget_floor_interval(1, 100, 1_000), 10);
        assert_eq!(budget_floor_interval(4, 100, 1_000), 40);
        // 一条都没启用 = 没人花额度,不设限。
        assert_eq!(budget_floor_interval(0, 299, 21_600), 0);
        // 预算是 0 也不能除出个 panic:退成"一个窗口跑一轮"。
        assert_eq!(budget_floor_interval(1, 0, 21_600), 21_600);
    }

    /// 设置里填得再勤也快不过地板,而且宽松档同样受它管。
    #[test]
    fn the_floor_wins_over_a_too_eager_setting() {
        let mut scheduler = PollScheduler::new();
        let tuning = WatcherTuning {
            poll_interval_seconds: 60,
            poll_interval_when_live_seconds: 90,
            ..WatcherTuning::default()
        };
        scheduler.set_budget_floor(budget_floor_interval(3, 299, 21_600));
        scheduler.upsert(watch("a"), 60, 0, 0, 1);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().interval, 217);

        scheduler.reschedule(&watch("a"), 1_000, PollOutcome::Ok, &tuning);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 1_217);

        // 宽松档也一样:90 秒快过地板,照地板走。
        scheduler.set_live_healthy(&watch("a"), true);
        scheduler.reschedule(&watch("a"), 2_000, PollOutcome::Ok, &tuning);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 2_217);

        // 填得比地板还慢就听用户的。
        scheduler.set_budget_floor(73);
        scheduler.reschedule(&watch("a"), 3_000, PollOutcome::Ok, &tuning);
        assert_eq!(scheduler.entry(&watch("a")).unwrap().next_at, 3_090);
    }

    #[test]
    fn removing_a_watch_takes_it_off_the_timetable() {
        let mut scheduler = PollScheduler::new();
        scheduler.upsert(watch("a"), 300, 0, 0, 1);
        assert_eq!(scheduler.len(), 1);
        scheduler.remove(&watch("a"));
        assert!(scheduler.is_empty());
        assert_eq!(scheduler.next_deadline(), None);
        // 不存在的搜索上做任何事都不该 panic。
        scheduler.reschedule(&watch("a"), 0, PollOutcome::Ok, &tuning());
        scheduler.poll_now(&watch("a"), 0);
        scheduler.defer(&watch("a"), 0);
        scheduler.set_live_healthy(&watch("a"), true);
    }
}
