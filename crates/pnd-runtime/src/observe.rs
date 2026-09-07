//! 观察调度:每条市场观察下一次该 discover、下一次该 recheck。
//!
//! 和 [`crate::poll`] 并排的一层,纪律一样 —— 纯逻辑,不碰时钟也不碰网络,
//! `now` 一律由调用方传进来,于是"两小时回查一次"这种事可以在测试里
//! 一微秒跑完。
//!
//! 两条时间线,一条观察各走各的:
//!
//! - **discover**(默认 10 分钟)= 一次 search,拉"最新 100 条"找新面孔。
//!   它花的是**搜索**额度,所以和蹲价共用同一条地板
//!   ([`crate::poll::budget_floor_interval`],把观察条数一起数进去)。
//! - **recheck**(默认 2 小时)= 把在册的挂单按 10 个一批 fetch,查谁没了。
//!   它花的是**抓取**额度,那一份额度宽得多(6 小时 1000 次的一半),
//!   但也不是无限的 —— [`fetch_listing_cap`] 算的就是"一轮最多查多少条"。
//!
//! 观察失败不退避:一轮 discover 砸了(网断了、被 429 了)就等下一轮,
//! 因为它本来就是十分钟一次的慢节奏,而真正需要退避的那几种情况
//! (429、Cloudflare)网关已经在自己那一层拦住了。

use std::collections::BTreeMap;

use pnd_domain::ObservationId;

/// 观察的两件事。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObserveKind {
    /// 拉最新 100 条,找没见过的。
    Discover,
    /// 回头查在册的还在不在。
    Recheck,
}

impl ObserveKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ObserveKind::Discover => "discover",
            ObserveKind::Recheck => "recheck",
        }
    }
}

/// 一条观察的两条时间线。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveEntry {
    pub next_discover_at: i64,
    pub next_recheck_at: i64,
    pub discover_interval: u64,
    pub recheck_interval: u64,
}

impl ObserveEntry {
    fn next_at(&self, kind: ObserveKind) -> i64 {
        match kind {
            ObserveKind::Discover => self.next_discover_at,
            ObserveKind::Recheck => self.next_recheck_at,
        }
    }

    fn interval(&self, kind: ObserveKind) -> u64 {
        match kind {
            ObserveKind::Discover => self.discover_interval,
            ObserveKind::Recheck => self.recheck_interval,
        }
    }

    fn set_next_at(&mut self, kind: ObserveKind, at: i64) {
        match kind {
            ObserveKind::Discover => self.next_discover_at = at,
            ObserveKind::Recheck => self.next_recheck_at = at,
        }
    }
}

/// 所有观察的时间表。
///
/// 和 [`crate::poll::PollScheduler`] 一样用 `BTreeMap`:`due()` 的顺序因此是
/// 稳定的,两次运行看到的顺序一样。
#[derive(Debug, Clone, Default)]
pub struct ObserveScheduler {
    entries: BTreeMap<ObservationId, ObserveEntry>,
    /// discover 最快能多久一次(秒)。0 = 还没算过,不设限。
    discover_floor: u64,
}

impl ObserveScheduler {
    #[must_use]
    pub fn new() -> ObserveScheduler {
        ObserveScheduler::default()
    }

    /// 加一条观察,或者更新一条已有观察的两个间隔。
    ///
    /// **新加的**那条按 `stagger_index / count` 把第一次 discover 往后推
    /// (理由同轮询:三条观察同一秒醒来,网关会连着发三次 search)。
    /// **第一次 recheck 排在一个完整的 recheck 间隔之后** —— 一条刚加上的
    /// 观察手里一条在册挂单都没有,立刻回查是查一个空表。
    ///
    /// 已有的那条只换间隔,不动两个 `next_at`:用户在设置页改个备注名,
    /// 不该让所有观察重新排队。
    pub fn upsert(
        &mut self,
        obs_id: ObservationId,
        discover_interval: u64,
        recheck_interval: u64,
        now: i64,
        stagger_index: usize,
        count: usize,
    ) {
        let discover_interval = discover_interval.max(self.discover_floor);
        if let Some(entry) = self.entries.get_mut(&obs_id) {
            entry.discover_interval = discover_interval;
            entry.recheck_interval = recheck_interval;
            return;
        }
        let count = count.max(1);
        let offset =
            discover_interval.saturating_mul(stagger_index.min(count) as u64) / count as u64;
        self.entries.insert(
            obs_id,
            ObserveEntry {
                next_discover_at: now + offset as i64,
                next_recheck_at: now + recheck_interval as i64,
                discover_interval,
                recheck_interval,
            },
        );
    }

    pub fn remove(&mut self, obs_id: &ObservationId) {
        self.entries.remove(obs_id);
    }

    #[must_use]
    pub fn entry(&self, obs_id: &ObservationId) -> Option<&ObserveEntry> {
        self.entries.get(obs_id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 最早的那件事在什么时候。主循环拿它算"该睡多久"。
    #[must_use]
    pub fn next_deadline(&self) -> Option<i64> {
        self.entries
            .values()
            .map(|entry| entry.next_discover_at.min(entry.next_recheck_at))
            .min()
    }

    /// 现在该做的事,早该做的排在前面。
    #[must_use]
    pub fn due(&self, now: i64) -> Vec<(ObservationId, ObserveKind)> {
        let mut due: Vec<(i64, ObserveKind, ObservationId)> = Vec::new();
        for (obs_id, entry) in &self.entries {
            for kind in [ObserveKind::Discover, ObserveKind::Recheck] {
                let at = entry.next_at(kind);
                if at <= now {
                    due.push((at, kind, obs_id.clone()));
                }
            }
        }
        due.sort();
        due.into_iter()
            .map(|(_, kind, obs_id)| (obs_id, kind))
            .collect()
    }

    /// 一件事**刚发出去**就把下一次排上,理由同 [`crate::poll::PollScheduler::defer`]:
    /// 不然 `next_at` 停在过去,主循环会一直觉得"该跑了"而空转。
    pub fn defer(&mut self, obs_id: &ObservationId, kind: ObserveKind, now: i64) {
        if let Some(entry) = self.entries.get_mut(obs_id) {
            let interval = entry.interval(kind);
            entry.set_next_at(kind, now + interval as i64);
        }
    }

    /// 一轮跑完了,排下一轮。观察不退避(理由见本文件顶部),所以成功失败
    /// 都是同一个间隔 —— 有它只是为了让"这一轮到此为止"有个明确的落点。
    pub fn reschedule(&mut self, obs_id: &ObservationId, kind: ObserveKind, now: i64) {
        self.defer(obs_id, kind, now);
    }

    /// 用户点了"立刻找一次新的" / "立刻回查一次"。
    pub fn run_now(&mut self, obs_id: &ObservationId, kind: ObserveKind, now: i64) {
        if let Some(entry) = self.entries.get_mut(obs_id) {
            entry.set_next_at(kind, now);
        }
    }

    /// 换一条 discover 地板。观察或搜索的条数一变就重算一次。
    ///
    /// 只影响**以后**算出来的间隔:已经排好的那一轮不动。
    pub fn set_discover_floor(&mut self, floor: u64) {
        self.discover_floor = floor;
        for entry in self.entries.values_mut() {
            entry.discover_interval = entry.discover_interval.max(floor);
        }
    }

    #[must_use]
    pub fn discover_floor(&self) -> u64 {
        self.discover_floor
    }
}

/// 一轮里最多拿多少条挂单去 fetch。
///
/// 抓取额度是按窗口算的(6 小时 1000 次的一半 = 499),而一条观察在一个窗口里
/// 要跑 `窗口 / 间隔` 轮。所以一轮能花的次数是
/// `预算 ÷ 观察条数 ÷ 2 ÷ 轮数`,再乘上"一次 fetch 带几个 id"就是挂单条数。
///
/// **÷2 是给 discover 和 recheck 各留一半**:两边花的是同一份抓取额度,谁也
/// 不能把对方饿死 —— discover 抓新面孔、recheck 查谁没了,少了哪一边这条
/// 观察都不成立。
///
/// 默认那份配置下这条上限基本碰不到(recheck:一轮 830 条;discover:一轮
/// 60 条),它是给"加了十条观察 + 每条几百件在册"那天兜底的。
#[must_use]
pub fn fetch_listing_cap(
    observations: usize,
    interval_secs: u64,
    fetch_budget_per_window: u32,
    window_secs: u32,
    fetch_batch: usize,
) -> usize {
    if observations == 0 || fetch_batch == 0 {
        return 0;
    }
    let share = u64::from(fetch_budget_per_window) / observations as u64 / 2;
    // 间隔比窗口还长的话,一个窗口里连一轮都跑不满,那就按一轮算。
    let cycles = (u64::from(window_secs) / interval_secs.max(1)).max(1);
    let per_cycle = (share / cycles).max(1);
    (per_cycle * fetch_batch as u64) as usize
}

#[cfg(test)]
mod observe_tests {
    use super::*;

    fn obs(id: &str) -> ObservationId {
        ObservationId(id.to_string())
    }

    fn scheduler() -> ObserveScheduler {
        let mut scheduler = ObserveScheduler::new();
        scheduler.upsert(obs("a"), 600, 7_200, 1_000, 0, 1);
        scheduler
    }

    /// 刚加的观察:discover 立刻,recheck 等一个完整间隔 —— 手里一条在册的
    /// 挂单都没有,立刻回查是查一个空表。
    #[test]
    fn a_new_observation_discovers_now_and_rechecks_one_interval_later() {
        let scheduler = scheduler();
        let entry = scheduler.entry(&obs("a")).unwrap();
        assert_eq!(entry.next_discover_at, 1_000);
        assert_eq!(entry.next_recheck_at, 1_000 + 7_200);
        assert_eq!(scheduler.next_deadline(), Some(1_000));
        assert_eq!(
            scheduler.due(1_000),
            vec![(obs("a"), ObserveKind::Discover)]
        );
        assert_eq!(
            scheduler.due(8_200),
            vec![
                (obs("a"), ObserveKind::Discover),
                (obs("a"), ObserveKind::Recheck)
            ],
            "早该做的排前面"
        );
    }

    /// 多条观察错开起跑,理由同轮询:三条同一秒醒来,网关会连着发三次 search。
    #[test]
    fn several_observations_stagger_their_first_discover() {
        let mut scheduler = ObserveScheduler::new();
        for (index, id) in ["a", "b", "c"].iter().enumerate() {
            scheduler.upsert(obs(id), 600, 7_200, 1_000, index, 3);
        }
        assert_eq!(scheduler.entry(&obs("a")).unwrap().next_discover_at, 1_000);
        assert_eq!(scheduler.entry(&obs("b")).unwrap().next_discover_at, 1_200);
        assert_eq!(scheduler.entry(&obs("c")).unwrap().next_discover_at, 1_400);
        assert_eq!(scheduler.len(), 3);
    }

    /// 两条时间线各走各的:发了 discover 不该把 recheck 往后推。
    #[test]
    fn the_two_timelines_move_independently() {
        let mut scheduler = scheduler();
        scheduler.defer(&obs("a"), ObserveKind::Discover, 1_000);
        let entry = scheduler.entry(&obs("a")).unwrap();
        assert_eq!(entry.next_discover_at, 1_600);
        assert_eq!(entry.next_recheck_at, 8_200, "recheck 没被碰");

        scheduler.reschedule(&obs("a"), ObserveKind::Recheck, 9_000);
        let entry = scheduler.entry(&obs("a")).unwrap();
        assert_eq!(entry.next_discover_at, 1_600);
        assert_eq!(entry.next_recheck_at, 9_000 + 7_200);

        // 用户点"立刻查一次"只动那一条线。
        scheduler.run_now(&obs("a"), ObserveKind::Recheck, 9_500);
        assert_eq!(
            scheduler.due(9_500),
            vec![
                (obs("a"), ObserveKind::Discover),
                (obs("a"), ObserveKind::Recheck)
            ]
        );
    }

    /// 已经在表上的观察只换间隔,不重排 —— 改个备注名不该让所有观察重新排队。
    #[test]
    fn upserting_again_only_changes_the_intervals() {
        let mut scheduler = scheduler();
        scheduler.defer(&obs("a"), ObserveKind::Discover, 1_000);
        scheduler.upsert(obs("a"), 900, 10_800, 5_000, 0, 1);
        let entry = scheduler.entry(&obs("a")).unwrap();
        assert_eq!(entry.discover_interval, 900);
        assert_eq!(entry.recheck_interval, 10_800);
        assert_eq!(entry.next_discover_at, 1_600, "已经排好的这一轮不该被挪动");
    }

    /// discover 花的是搜索额度,所以它同样受预算地板管 —— 而且地板是
    /// 蹲价和观察一起算出来的那一个。
    #[test]
    fn the_discover_floor_holds_the_interval() {
        let mut scheduler = ObserveScheduler::new();
        // 三条搜索 + 两条观察一起分 6 小时 299 次:地板 362 秒。
        scheduler.set_discover_floor(crate::poll::budget_floor_interval(5, 299, 21_600));
        assert_eq!(scheduler.discover_floor(), 362);
        scheduler.upsert(obs("a"), 300, 7_200, 0, 0, 1);
        assert_eq!(scheduler.entry(&obs("a")).unwrap().discover_interval, 362);

        // 后来又加了一条观察,地板抬高:已经在表上的那条也跟着抬。
        scheduler.set_discover_floor(600);
        assert_eq!(scheduler.entry(&obs("a")).unwrap().discover_interval, 600);
        scheduler.defer(&obs("a"), ObserveKind::Discover, 1_000);
        assert_eq!(scheduler.entry(&obs("a")).unwrap().next_discover_at, 1_600);

        // 填得比地板还慢就听用户的。
        scheduler.upsert(obs("b"), 3_600, 7_200, 0, 0, 1);
        assert_eq!(scheduler.entry(&obs("b")).unwrap().discover_interval, 3_600);
    }

    #[test]
    fn removing_an_observation_takes_it_off_the_timetable() {
        let mut scheduler = scheduler();
        scheduler.remove(&obs("a"));
        assert!(scheduler.is_empty());
        assert_eq!(scheduler.next_deadline(), None);
        assert!(scheduler.due(9_999).is_empty());
        // 不在表上的观察做什么都不该 panic。
        scheduler.defer(&obs("a"), ObserveKind::Discover, 0);
        scheduler.reschedule(&obs("a"), ObserveKind::Recheck, 0);
        scheduler.run_now(&obs("a"), ObserveKind::Discover, 0);
    }

    /// 抓取额度的分账。默认配置下这条上限碰不到,它是给"十条观察"那天兜底的。
    #[test]
    fn the_fetch_cap_splits_the_budget_between_discover_and_recheck() {
        // 一条观察、6 小时 499 次的一半给 recheck、两小时一轮(3 轮):
        // 499/1/2 = 249,249/3 = 83 次 fetch,一次 10 条 = 830 条。
        assert_eq!(fetch_listing_cap(1, 7_200, 499, 21_600, 10), 830);
        // 同一份额度给 discover:10 分钟一轮(36 轮)→ 249/36 = 6 次 = 60 条。
        assert_eq!(fetch_listing_cap(1, 600, 499, 21_600, 10), 60);
        // 十条观察分同一份额度,一轮就只剩 10 条。
        assert_eq!(fetch_listing_cap(10, 7_200, 499, 21_600, 10), 80);
        assert_eq!(fetch_listing_cap(10, 600, 499, 21_600, 10), 10);
        // 一条观察都没有 = 没人花额度。
        assert_eq!(fetch_listing_cap(0, 600, 499, 21_600, 10), 0);
        // 再挤也得给一次 fetch,否则这条观察永远查不动。
        assert_eq!(fetch_listing_cap(100, 60, 499, 21_600, 10), 10);
        // 间隔比窗口还长:按一个窗口跑一轮算,不该除出个 0。
        assert_eq!(fetch_listing_cap(1, 86_400, 499, 21_600, 10), 2_490);
    }
}
