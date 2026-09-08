//! 观察调度:每条市场观察下一次该 discover,以及下一次该扫一遍到期的挂单。
//!
//! 和 [`crate::poll`] 并排的一层,纪律一样 —— 纯逻辑,不碰时钟也不碰网络,
//! `now` 一律由调用方传进来,于是"三天后再看一眼"这种事可以在测试里
//! 一微秒跑完。
//!
//! 两件事,节奏完全不同:
//!
//! - **discover**(默认 10 分钟)= 一次 search,拉"最新 100 条"找新面孔。
//!   它现在只是**兜底**:新挂单主要靠 live 秒推知道(那条路一条搜索额度都不花),
//!   这一轮是给"WebSocket 断线的那几十秒"补漏的。它花的是搜索额度,所以和
//!   蹲价共用同一条地板([`crate::poll::budget_floor_interval`],把观察条数
//!   一起数进去)。
//! - **sweep**(最快一分钟一次)= 把**到点该回头看**的挂单捞出来 fetch。
//!   哪条挂单什么时候到点,是 [`pnd_domain::next_check_after`] 那条阶梯说了算,
//!   不在这里 —— 这里只管"多久去库里问一次谁到点了"。
//!
//! 为什么不再是"每条观察每两小时把在册的全查一遍":那种查法里,一条挂上去
//! 三分钟就被买走的碑牌和一条挂了六天的护符花掉的额度一模一样,而前者才是
//! 我们要量的东西。阶梯把额度花在挂单**刚出生**的那几个小时里,老货一天问
//! 一次,七天以后干脆不问 —— 同一份 fetch 额度能盯住的挂单多得多。
//!
//! 观察失败不退避:一轮 discover 砸了(网断了、被 429 了)就等下一轮,
//! 因为它本来就是十分钟一次的慢节奏,而真正需要退避的那几种情况
//! (429、Cloudflare)网关已经在自己那一层拦住了。

use std::collections::BTreeMap;

use pnd_domain::ObservationId;

/// 最快多久扫一次"谁到点该回查了"。
///
/// 一分钟不是回查的精度,是**问库的**精度:阶梯上最密的一档是 10 分钟,
/// 晚一分钟去问它无所谓;而扫得再勤也只是空跑一条 SQL。
pub const SWEEP_INTERVAL_SECS: u64 = 60;

/// 一条观察的兜底 discover 时间线。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveEntry {
    pub next_discover_at: i64,
    pub discover_interval: u64,
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
    /// 下一次扫描到期挂单的时刻。0 = 现在就该扫一次。
    next_sweep_at: i64,
}

impl ObserveScheduler {
    #[must_use]
    pub fn new() -> ObserveScheduler {
        ObserveScheduler::default()
    }

    /// 加一条观察,或者更新一条已有观察的 discover 间隔。
    ///
    /// **新加的**那条按 `stagger_index / count` 把第一次 discover 往后推
    /// (理由同轮询:三条观察同一秒醒来,网关会连着发三次 search)。
    ///
    /// 已有的那条只换间隔,不动 `next_discover_at`:用户在设置页改个备注名,
    /// 不该让所有观察重新排队。
    pub fn upsert(
        &mut self,
        obs_id: ObservationId,
        discover_interval: u64,
        now: i64,
        stagger_index: usize,
        count: usize,
    ) {
        let discover_interval = discover_interval.max(self.discover_floor);
        if let Some(entry) = self.entries.get_mut(&obs_id) {
            entry.discover_interval = discover_interval;
            return;
        }
        let count = count.max(1);
        let offset =
            discover_interval.saturating_mul(stagger_index.min(count) as u64) / count as u64;
        self.entries.insert(
            obs_id,
            ObserveEntry {
                next_discover_at: now + offset as i64,
                discover_interval,
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
    ///
    /// 一条观察都没有的时候连扫描都不用排:库里不可能有到期的挂单。
    #[must_use]
    pub fn next_deadline(&self) -> Option<i64> {
        let discover = self
            .entries
            .values()
            .map(|entry| entry.next_discover_at)
            .min()?;
        Some(discover.min(self.next_sweep_at))
    }

    /// 现在该 discover 的观察,早该做的排在前面。
    #[must_use]
    pub fn due(&self, now: i64) -> Vec<ObservationId> {
        let mut due: Vec<(i64, ObservationId)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.next_discover_at <= now)
            .map(|(obs_id, entry)| (entry.next_discover_at, obs_id.clone()))
            .collect();
        due.sort();
        due.into_iter().map(|(_, obs_id)| obs_id).collect()
    }

    /// 一轮 discover **刚发出去**就把下一轮排上,理由同
    /// [`crate::poll::PollScheduler::defer`]:不然 `next_at` 停在过去,
    /// 主循环会一直觉得"该跑了"而空转。
    pub fn defer(&mut self, obs_id: &ObservationId, now: i64) {
        if let Some(entry) = self.entries.get_mut(obs_id) {
            entry.next_discover_at = now + entry.discover_interval as i64;
        }
    }

    /// 用户点了"立刻找一次新的"。
    pub fn run_now(&mut self, obs_id: &ObservationId, now: i64) {
        if let Some(entry) = self.entries.get_mut(obs_id) {
            entry.next_discover_at = now;
        }
    }

    /// 该去问一遍"谁到点了"没有。
    #[must_use]
    pub fn sweep_due(&self, now: i64) -> bool {
        now >= self.next_sweep_at
    }

    #[must_use]
    pub fn next_sweep_at(&self) -> i64 {
        self.next_sweep_at
    }

    /// 扫完了(哪怕一条到期的都没有),把下一次排上。
    pub fn defer_sweep(&mut self, now: i64) {
        self.next_sweep_at = now + SWEEP_INTERVAL_SECS as i64;
    }

    /// 用户点了"立刻回查一次":下一圈主循环就扫。
    pub fn sweep_now(&mut self, now: i64) {
        self.next_sweep_at = now;
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
/// **÷2 是给"抓新面孔"和"回查在册的"各留一半**:两边花的是同一份抓取额度,
/// 谁也不能把对方饿死 —— 一边记下新货、一边看它们最后怎么样了,少了哪一边
/// 这条观察都不成立。
///
/// 默认那份配置下这条上限基本碰不到,它是给"加了十条观察 + 每条几百件在册"
/// 那天兜底的。
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

/// 出生登记的令牌桶最多攒几张票。
///
/// 五张而不是一张:秒推是一阵一阵来的(同一秒里推三条很常见),桶空着的时候
/// 一条也接不住就太笨了。也不能更大 —— 攒得越多,一阵爆发就能把半个窗口的
/// 抓取额度一次性烧光,而那份额度还要养着回查扫描。
pub const BIRTH_BUCKET_CAPACITY: f64 = 5.0;

/// 秒推来的新挂单能花掉多少抓取额度。
///
/// 为什么非要有这道闸:一张 live 把手换一次 fetch,而 6 小时 1000 次的一半
/// 只有 499 次 —— 平摊下来**每分钟 1.4 次**,还要和所有蹲价、所有 discover、
/// 所有回查扫描分。主人那条碑牌搜索一分钟推 6 到 9 条。照单全收的话,不出
/// 十分钟额度就见底,然后所有请求一起排队,排到把手 14 秒的有效期过完 ——
/// 花光了额度,一条挂单也没记下来。
///
/// 所以接不住的那些**当场丢掉**,不排队:在挂单出生那一刻丢是无偏的
/// (`sample_every` 同理),而排队补抓会系统性地漏掉卖得最快的那批货 ——
/// 等轮到它,它早没了,而那正是我们要量的东西。
///
/// 一半给出生登记,另一半留给回查扫描:少了哪一边这条观察都不成立
/// (一边记下新货,一边看它们最后怎么样了)。
///
/// 纯逻辑,不碰时钟:`now` 一律由调用方传进来。
#[derive(Debug, Clone)]
pub struct BirthBudget {
    tokens: f64,
    capacity: f64,
    /// 每秒回多少张票 = 窗口额度的一半 ÷ 窗口秒数。
    refill_per_sec: f64,
    /// 上一次算到哪一刻。`None` = 还没花过,第一次 `take` 时对表。
    last_at: Option<i64>,
}

impl BirthBudget {
    /// `fetch_budget_per_window` 是我们自己那份额度(已经打过 50% 折的那个数),
    /// `window_secs` 是它对应的窗口。桶是满的:开机头几条推送该接得住。
    #[must_use]
    pub fn new(fetch_budget_per_window: u32, window_secs: u32) -> BirthBudget {
        let window = f64::from(window_secs.max(1));
        BirthBudget {
            tokens: BIRTH_BUCKET_CAPACITY,
            capacity: BIRTH_BUCKET_CAPACITY,
            refill_per_sec: f64::from(fetch_budget_per_window) / 2.0 / window,
            last_at: None,
        }
    }

    /// 花掉一张票。`false` = 这一条推送买不起,丢掉它。
    pub fn take(&mut self, now: i64) -> bool {
        let last = self.last_at.unwrap_or(now);
        // 时钟往回跳(改过系统时间)时不倒扣:`max(0)`。
        let elapsed = (now - last).max(0) as f64;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_at = Some(now);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }

    /// 桶里还剩几张(给日志和测试看)。
    #[must_use]
    pub fn tokens(&self) -> f64 {
        self.tokens
    }
}

#[cfg(test)]
mod observe_tests {
    use super::*;

    fn obs(id: &str) -> ObservationId {
        ObservationId(id.to_string())
    }

    fn scheduler() -> ObserveScheduler {
        let mut scheduler = ObserveScheduler::new();
        scheduler.upsert(obs("a"), 600, 1_000, 0, 1);
        scheduler
    }

    /// 刚加的观察立刻 discover 一次:秒推还没连上之前,那是它唯一的入口。
    #[test]
    fn a_new_observation_discovers_now() {
        let scheduler = scheduler();
        let entry = scheduler.entry(&obs("a")).unwrap();
        assert_eq!(entry.next_discover_at, 1_000);
        assert_eq!(scheduler.due(1_000), vec![obs("a")]);
        assert!(scheduler.due(999).is_empty());
    }

    /// 多条观察错开起跑,理由同轮询:三条同一秒醒来,网关会连着发三次 search。
    #[test]
    fn several_observations_stagger_their_first_discover() {
        let mut scheduler = ObserveScheduler::new();
        for (index, id) in ["a", "b", "c"].iter().enumerate() {
            scheduler.upsert(obs(id), 600, 1_000, index, 3);
        }
        assert_eq!(scheduler.entry(&obs("a")).unwrap().next_discover_at, 1_000);
        assert_eq!(scheduler.entry(&obs("b")).unwrap().next_discover_at, 1_200);
        assert_eq!(scheduler.entry(&obs("c")).unwrap().next_discover_at, 1_400);
        assert_eq!(scheduler.len(), 3);
        // 早该做的排前面。
        assert_eq!(scheduler.due(1_400), vec![obs("a"), obs("b"), obs("c")]);
    }

    /// 兜底轮询和回查扫描是两条独立的线:发了 discover 不该把扫描往后推,
    /// 反过来也一样。
    #[test]
    fn the_backstop_poll_and_the_sweep_move_independently() {
        let mut scheduler = scheduler();
        assert!(scheduler.sweep_due(1_000), "开机第一圈就该扫一次");

        scheduler.defer_sweep(1_000);
        assert!(!scheduler.sweep_due(1_059));
        assert!(scheduler.sweep_due(1_060), "一分钟一次");
        assert_eq!(
            scheduler.entry(&obs("a")).unwrap().next_discover_at,
            1_000,
            "扫描没碰 discover"
        );

        scheduler.defer(&obs("a"), 1_000);
        assert_eq!(scheduler.entry(&obs("a")).unwrap().next_discover_at, 1_600);
        assert_eq!(scheduler.next_sweep_at(), 1_060, "discover 没碰扫描");

        // 用户点"立刻回查一次":下一圈就扫。
        scheduler.sweep_now(1_010);
        assert!(scheduler.sweep_due(1_010));
        // 而"立刻找一次新的"只动 discover 那条线。
        scheduler.defer_sweep(1_010);
        scheduler.run_now(&obs("a"), 1_020);
        assert_eq!(scheduler.due(1_020), vec![obs("a")]);
        assert!(!scheduler.sweep_due(1_020));
    }

    /// 主循环该睡到哪一刻:两条线里早的那个。
    #[test]
    fn the_deadline_is_the_earlier_of_the_two_lines() {
        let mut scheduler = ObserveScheduler::new();
        assert_eq!(scheduler.next_deadline(), None, "没有观察就没有事要做");

        scheduler.upsert(obs("a"), 600, 1_000, 0, 1);
        scheduler.defer_sweep(1_000);
        assert_eq!(scheduler.next_deadline(), Some(1_000), "discover 更早");
        scheduler.defer(&obs("a"), 1_000);
        assert_eq!(scheduler.next_deadline(), Some(1_060), "现在是扫描更早");
    }

    /// 已经在表上的观察只换间隔,不重排 —— 改个备注名不该让所有观察重新排队。
    #[test]
    fn upserting_again_only_changes_the_interval() {
        let mut scheduler = scheduler();
        scheduler.defer(&obs("a"), 1_000);
        scheduler.upsert(obs("a"), 900, 5_000, 0, 1);
        let entry = scheduler.entry(&obs("a")).unwrap();
        assert_eq!(entry.discover_interval, 900);
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
        scheduler.upsert(obs("a"), 300, 0, 0, 1);
        assert_eq!(scheduler.entry(&obs("a")).unwrap().discover_interval, 362);

        // 后来又加了一条观察,地板抬高:已经在表上的那条也跟着抬。
        scheduler.set_discover_floor(600);
        assert_eq!(scheduler.entry(&obs("a")).unwrap().discover_interval, 600);
        scheduler.defer(&obs("a"), 1_000);
        assert_eq!(scheduler.entry(&obs("a")).unwrap().next_discover_at, 1_600);

        // 填得比地板还慢就听用户的。
        scheduler.upsert(obs("b"), 3_600, 0, 0, 1);
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
        scheduler.defer(&obs("a"), 0);
        scheduler.run_now(&obs("a"), 0);
    }

    /// 一阵爆发最多接住五条,第六条当场丢掉。
    ///
    /// 主人那条搜索一分钟推 6 到 9 条,而一张把手就是一次 fetch —— 照单全收
    /// 十分钟就能把 6 小时的额度烧光,然后所有请求一起排队,排到把手 14 秒的
    /// 有效期过完:额度花光了,一条挂单也没记下来。
    #[test]
    fn a_burst_of_pushes_is_capped_at_the_bucket_size() {
        let mut budget = BirthBudget::new(499, 21_600);
        for index in 0..5 {
            assert!(budget.take(1_000), "第 {index} 条该接住");
        }
        assert!(!budget.take(1_000), "同一秒里第六条买不起了");
    }

    /// 票是按"抓取额度的一半"慢慢回的。
    ///
    /// 6 小时 499 次的一半 = 249.5 次,摊到 21600 秒 = 每 86.6 秒一张。
    /// 也就是说秒推那条路平均一分半才抓得起一条 —— 剩下那一半额度是留给
    /// 回查扫描的,两边谁也不能把对方饿死。
    #[test]
    fn the_bucket_refills_at_half_the_fetch_budget() {
        let mut budget = BirthBudget::new(499, 21_600);
        for _ in 0..5 {
            assert!(budget.take(1_000));
        }
        assert!(!budget.take(1_086), "86 秒还差一点点");
        assert!(budget.take(1_087), "87 秒攒够一张");
        assert!(!budget.take(1_087), "花掉了就又空了");
    }

    /// 攒不过头:离开三个小时再回来,桶也只有五张。
    ///
    /// 没有这个上限的话,程序在后台挂一夜,早上第一阵推送就能把整个窗口的
    /// 抓取额度一次性烧光。
    #[test]
    fn a_long_quiet_spell_does_not_bank_more_than_the_capacity() {
        let mut budget = BirthBudget::new(499, 21_600);
        for _ in 0..5 {
            assert!(budget.take(1_000));
        }
        for index in 0..5 {
            assert!(budget.take(1_000_000), "睡醒之后第 {index} 条");
        }
        assert!(!budget.take(1_000_000), "最多还是五张");
        assert!(budget.tokens() < 1.0);
    }

    /// 额度小到回票比一次推送还慢也不该 panic 或者除零。
    #[test]
    fn a_tiny_budget_just_means_almost_everything_is_dropped() {
        let mut budget = BirthBudget::new(1, 0);
        assert!(budget.take(0), "开机那一桶还是满的");
        for _ in 0..4 {
            assert!(budget.take(0));
        }
        assert!(!budget.take(0));
    }

    /// 抓取额度的分账。默认配置下这条上限碰不到,它是给"十条观察"那天兜底的。
    #[test]
    fn the_fetch_cap_splits_the_budget_between_discover_and_recheck() {
        // 一条观察、6 小时 499 次的一半给回查、一分钟扫一次(360 轮):
        // 499/1/2 = 249,249/360 除不出一次,兜成一次 fetch = 10 条。
        assert_eq!(
            fetch_listing_cap(1, SWEEP_INTERVAL_SECS, 499, 21_600, 10),
            10
        );
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
