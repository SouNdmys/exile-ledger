//! 市场观察的纯判定:一条挂单**不见了**,它更像是卖掉了,还是被撤了。
//!
//! 交易站没有任何成交接口,所以"卖没卖掉"只能从挂单的消失反推。这里是
//! 那一步反推的全部逻辑,而且是纯函数:时间全由调用方传进来,库和网络都
//! 碰不到,于是"挂了三天忽然没了"这种情况可以在测试里一微秒跑完。
//!
//! 三条规矩,顺序要紧:
//!
//! 1. **我们只看见它活了不到 10 分钟 → [`GoneClass::Unknown`]。** 挂错价、
//!    挂错联赛、手滑上架的东西几分钟内就撤了,那不是成交数据,是噪音。
//! 2. **消失之前降过价 → [`GoneClass::SoldAfterCuts`]。** 也是卖掉了,但
//!    "降了两次才出手"和"挂上去就被秒"说的是两回事,价格中位得分开看。
//! 3. **其余 → [`GoneClass::SoldLikely`]。** 即刻购买模式下货在店铺仓库里,
//!    卖家离线单子也在;所以一条挂了足够久的单子忽然没了,最可能的解释
//!    就是有人买走了。

use serde::{Deserialize, Serialize};

/// 观察到的存活时间短于它,就不下结论。
///
/// 10 分钟正好是一个 discover 周期(默认 600 秒):没跨过一次 discover 的
/// 挂单,我们其实只见过它一次,谈不上"活了多久"。
pub const MIN_OBSERVED_LIFETIME_SECS: i64 = 600;

/// 挂了这么久还在卖,就是"没人要",不是"还没轮到"。
pub const STALE_AFTER_SECS: i64 = 7 * 24 * 3600;

/// 一条消失的挂单最可能的去向。
///
/// 前三档是 [`classify_gone`] 判出来的,故意只有三档:再细分下去(比如
/// "卖家整批撤单")需要回头查那个卖家的其它挂单,那是第二版的事
/// (见计划里的 `delisted_likely`)。
///
/// 第四档 [`GoneClass::GoneBeforeFirstLook`] 不经过判定:秒推告诉我们有这么
/// 一条挂单、几秒钟后去抓详情却已经没了 —— 那种情况我们连它标价多少都不知道,
/// 没有任何可判的东西,由运行时直接写上这一档。它偏偏是最有意思的一批货
/// (秒掉的都是好价),所以必须单独数,不能混进"看不出来"里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoneClass {
    /// 挂了一阵、一次没降价、然后没了。
    SoldLikely,
    /// 降过价之后没了。
    SoldAfterCuts,
    /// 存活太短,看不出来 —— 多半是挂错了撤掉的。
    Unknown,
    /// 第一次去抓它详情的时候就已经没了。
    GoneBeforeFirstLook,
}

impl GoneClass {
    /// 存库时的写法。存字符串而不是数字:直接打开 `watch.sqlite` 也看得懂。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GoneClass::SoldLikely => "sold_likely",
            GoneClass::SoldAfterCuts => "sold_after_cuts",
            GoneClass::Unknown => "unknown",
            GoneClass::GoneBeforeFirstLook => "gone_before_first_look",
        }
    }

    /// 认不出来的一律当 `Unknown`:老库里的值、手改过的值,都不该让整行读不出来。
    #[must_use]
    pub fn parse(raw: &str) -> GoneClass {
        match raw {
            "sold_likely" => GoneClass::SoldLikely,
            "sold_after_cuts" => GoneClass::SoldAfterCuts,
            "gone_before_first_look" => GoneClass::GoneBeforeFirstLook,
            _ => GoneClass::Unknown,
        }
    }

    /// 这一档算不算"卖掉了"。
    ///
    /// 降价之后成交也是成交:聚合表里那个"疑似成交率"问的是"这种词缀的货
    /// 能不能出手",而不是"能不能原价出手"。
    #[must_use]
    pub fn looks_sold(self) -> bool {
        matches!(self, GoneClass::SoldLikely | GoneClass::SoldAfterCuts)
    }
}

/// 我们**看见**这条挂单活了多久(秒)。
///
/// 用 `last_seen - first_seen` 而不是 `gone_at - first_seen`:`gone_at` 只是
/// "我们回头查的那一刻",而回查默认两小时一次 —— 拿它算存活,每条挂单都会
/// 平白多出最多两小时的寿命。看见的那一段才是实打实的。
#[must_use]
pub fn observed_lifetime_secs(first_seen: i64, last_seen: i64) -> i64 {
    (last_seen - first_seen).max(0)
}

/// 这条挂单为什么不见了。
///
/// `price_changes` 是它的价格轨迹 `(时刻, 金额千分整数)`,顺序不限,
/// 由调用方保证同一种货币(库里一条挂单只记一种货币)。晚于 `gone_at` 的
/// 点会被忽略 —— 那已经不是"消失之前"发生的事了。
#[must_use]
pub fn classify_gone(
    first_seen: i64,
    last_seen: i64,
    gone_at: i64,
    price_changes: &[(i64, i64)],
) -> GoneClass {
    if observed_lifetime_secs(first_seen, last_seen) < MIN_OBSERVED_LIFETIME_SECS {
        return GoneClass::Unknown;
    }
    if had_a_price_cut(price_changes, gone_at) {
        return GoneClass::SoldAfterCuts;
    }
    GoneClass::SoldLikely
}

/// 还挂着,但已经挂了七天以上 —— "还在卖,只是没人要"。
///
/// 和 [`classify_gone`] 是两个问题:那个回答"没了的是怎么没的",这个回答
/// "还在的这条是不是早该降价了"。
#[must_use]
pub fn is_stale(first_seen: i64, now: i64) -> bool {
    now - first_seen >= STALE_AFTER_SECS
}

/// 一条挂单记下来之后,第几次回头看它该等多久(秒,从 `first_seen` 算起)。
///
/// 阶梯而不是固定间隔,是因为"卖得快的"和"卖不掉的"值钱程度不一样:
/// 一件好价碑牌一分钟内就没了,而那正是我们最想量的事,所以头两档密;
/// 一件挂了三天还在的货,再精确到小时也不会改变结论("没人要"),
/// 所以往后越拉越松,直到 [`STALE_AFTER_SECS`] 干脆不看了。
pub const CHECK_RUNGS: [i64; 6] = [600, 1_800, 7_200, 21_600, 86_400, 259_200];

/// 第 `rung` 档回查该落在哪一刻(unix 秒);`None` = 这条挂单已经到了
/// [`STALE_AFTER_SECS`],不再看了。
///
/// `rung` 是**下一档的编号**:一条刚记下的挂单是第 0 档(`first_seen + 600`),
/// 在第 0 档看过一眼还活着就升到第 1 档(`first_seen + 1800`)。走完
/// [`CHECK_RUNGS`] 之后每 `recheck_interval_secs` 一档 —— 那时它只回答
/// "这件挂了三天的货今天还在不在",一天问一次绰绰有余。
///
/// 时刻一律从 `first_seen` 算,不从"上一次查的时刻"累加:回查排队晚了几分钟
/// 不该把后面每一档都往后推,那会让"它活了多久"越查越不准。
#[must_use]
pub fn next_check_after(first_seen: i64, rung: u32, recheck_interval_secs: u64) -> Option<i64> {
    let offset = match CHECK_RUNGS.get(rung as usize) {
        Some(secs) => *secs,
        None => {
            // 阶梯之外的第几次。间隔为 0 会让它原地踏步,兜一下。
            let beyond = i64::from(rung) - CHECK_RUNGS.len() as i64 + 1;
            let every = recheck_interval_secs.max(1) as i64;
            CHECK_RUNGS[CHECK_RUNGS.len() - 1].saturating_add(beyond.saturating_mul(every))
        }
    };
    let at = first_seen.saturating_add(offset);
    (!is_stale(first_seen, at)).then_some(at)
}

/// 价位阶梯的下界,单位是**整个**通货(不是千分整数)。
///
/// 为什么疏密不均:10 以下每一格都是一个真实的心理价位("2 divine 的碑牌"
/// 和"3 divine 的碑牌"是两批货,卖家也是照这个数在标价);而 100 以上再
/// 分细只会把本来就不多的样本摊成每档一两条,什么结论都撑不起来。
pub const PRICE_BUCKET_UNITS: [i64; 22] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 15, 20, 30, 50, 75, 100, 150, 200, 300, 500, 1_000,
];

/// 一个价位档。记的是这一档的**下界**(千分整数),不是区间两头:
/// 上界永远是下一档的下界,存两个数就会有对不上的那一天。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PriceBucket {
    pub lower_bound_milli: i64,
}

impl PriceBucket {
    /// 是不是最低那一档。
    ///
    /// 单独问一句是因为它的写法和别的档不一样:界面上要写「不到 1 divine」
    /// (折算那条阶梯上是「不到 0.1 divine」),写成「0 divine」会读成"白送"。
    #[must_use]
    pub fn is_under_one(self) -> bool {
        self.lower_bound_milli == 0
    }
}

/// 一个价落在哪一档:阶梯上不大于它的那个最大下界。
///
/// 为什么要分档:观察攒下来的价格是一堆散点(2.5、2.8、3、3.2 divine),
/// 一个一个数过去每个价都只有一两条,看不出"2 divine 那一批出得掉、
/// 3 divine 那一批堆着"。归了档才数得动。
#[must_use]
pub fn price_bucket(amount_milli: i64) -> PriceBucket {
    let mut lower_bound_milli = 0;
    for units in PRICE_BUCKET_UNITS {
        let bound = units * 1_000;
        if bound > amount_milli {
            break;
        }
        lower_bound_milli = bound;
    }
    PriceBucket { lower_bound_milli }
}

/// 折成 divine 之后那条阶梯在 1 以下的几根横档(千分整数)。
///
/// 只有折算过的那条要它们:按币种分的时候,"不到 1 chaos"那一格里本来就
/// 几乎没有货;而全部折成 divine 之后,原先标着几十 chaos 的碑牌一整批都
/// 落在 1 divine 以下 —— 挤在最低那一格里等于什么都没说。
pub const SUB_DIVINE_BUCKET_MILLI: [i64; 5] = [100, 200, 300, 500, 750];

/// 一个**折算成 divine** 的价落在哪一档。
///
/// 1 以上完全交给 [`price_bucket`]:两条阶梯在 1 以上是同一条,分成两份写
/// 迟早会有一天对不上号。
#[must_use]
pub fn divine_price_bucket(amount_milli: i64) -> PriceBucket {
    if amount_milli >= 1_000 {
        return price_bucket(amount_milli);
    }
    let mut lower_bound_milli = 0;
    for rung in SUB_DIVINE_BUCKET_MILLI {
        if rung > amount_milli {
            break;
        }
        lower_bound_milli = rung;
    }
    PriceBucket { lower_bound_milli }
}

/// 消失之前有没有降过价。
///
/// 只认**降**价:涨价说明卖家觉得还能卖更贵,那和"降到有人肯买"完全是
/// 两回事,不该混进同一档。
fn had_a_price_cut(price_changes: &[(i64, i64)], gone_at: i64) -> bool {
    let mut before: Vec<(i64, i64)> = price_changes
        .iter()
        .copied()
        .filter(|(at, _)| *at <= gone_at)
        .collect();
    before.sort_unstable();
    before.windows(2).any(|pair| pair[1].1 < pair[0].1)
}

#[cfg(test)]
mod observe_tests {
    use super::*;

    const HOUR: i64 = 3_600;

    /// 一口价没变过、挂了两小时才没的,就是最干净的"卖掉了"。
    #[test]
    fn a_quiet_listing_that_vanishes_reads_as_sold() {
        let first = 1_000;
        let last = first + 2 * HOUR;
        assert_eq!(
            classify_gone(first, last, last + HOUR, &[(first, 20_000)]),
            GoneClass::SoldLikely
        );
        // 一个价格点都没有(无价单)也一样。
        assert_eq!(
            classify_gone(first, last, last + HOUR, &[]),
            GoneClass::SoldLikely
        );
    }

    /// 10 分钟是"下不下结论"的分界线,差一秒就是另一档 —— 这条边界值得钉死:
    /// 挪错了方向,满库都会变成 `Unknown`(或者满库都变成"卖掉了")。
    #[test]
    fn ten_minutes_is_the_line_between_unknown_and_a_verdict() {
        let first = 1_000;
        let gone = first + 10 * HOUR;
        assert_eq!(
            classify_gone(first, first + 599, gone, &[]),
            GoneClass::Unknown,
            "只见过它 9 分 59 秒,不下结论"
        );
        assert_eq!(
            classify_gone(first, first + 600, gone, &[]),
            GoneClass::SoldLikely,
            "整 10 分钟就算数了"
        );
        // 只见过一次(first == last)当然也是 Unknown。
        assert_eq!(classify_gone(first, first, gone, &[]), GoneClass::Unknown);
        // 时钟倒着走也不能算出一个负的存活时间来。
        assert_eq!(
            classify_gone(first, first - 5_000, gone, &[]),
            GoneClass::Unknown
        );
    }

    /// 降过价再消失是另一档:"降了才卖掉"和"挂上去就被买走"不该混在一起。
    #[test]
    fn a_price_cut_before_it_vanished_is_its_own_class() {
        let first = 1_000;
        let last = first + 5 * HOUR;
        let gone = last + HOUR;
        let track = [(first, 25_000), (first + HOUR, 20_000)];
        assert_eq!(
            classify_gone(first, last, gone, &track),
            GoneClass::SoldAfterCuts
        );
        // 顺序打乱也得认出来:轨迹是按时刻排的,不是按数组下标。
        let shuffled = [(first + HOUR, 20_000), (first, 25_000)];
        assert_eq!(
            classify_gone(first, last, gone, &shuffled),
            GoneClass::SoldAfterCuts
        );
        // 但存活太短仍然优先判 Unknown —— 挂错价、改一次、马上撤,那不是成交。
        assert_eq!(
            classify_gone(first, first + 60, gone, &track),
            GoneClass::Unknown
        );
    }

    /// 涨价不是降价。卖家往上调说明他觉得还能卖更贵,和"降到有人肯买"是反的。
    #[test]
    fn a_price_rise_does_not_count_as_a_cut() {
        let first = 1_000;
        let last = first + 5 * HOUR;
        assert_eq!(
            classify_gone(
                first,
                last,
                last + HOUR,
                &[(first, 20_000), (first + HOUR, 25_000)]
            ),
            GoneClass::SoldLikely
        );
        // 先涨后降,只要降过一次就算。
        assert_eq!(
            classify_gone(
                first,
                last,
                last + HOUR,
                &[
                    (first, 20_000),
                    (first + HOUR, 25_000),
                    (first + 2 * HOUR, 21_000)
                ]
            ),
            GoneClass::SoldAfterCuts
        );
    }

    /// 消失之后才记下的价格点不算数 —— 那已经不是"消失之前"发生的事了。
    #[test]
    fn price_points_after_it_vanished_are_ignored() {
        let first = 1_000;
        let last = first + 5 * HOUR;
        let gone = last + HOUR;
        assert_eq!(
            classify_gone(first, last, gone, &[(first, 20_000), (gone + 60, 10_000)]),
            GoneClass::SoldLikely
        );
        // 正好落在 gone_at 那一秒的点还算"之前"。
        assert_eq!(
            classify_gone(first, last, gone, &[(first, 20_000), (gone, 10_000)]),
            GoneClass::SoldAfterCuts
        );
    }

    /// 七天是"还在卖但没人要"的分界线。
    #[test]
    fn stale_starts_at_seven_days() {
        let first = 1_000;
        assert!(!is_stale(first, first + STALE_AFTER_SECS - 1));
        assert!(is_stale(first, first + STALE_AFTER_SECS));
        assert_eq!(STALE_AFTER_SECS, 604_800);
    }

    #[test]
    fn a_gone_class_round_trips_through_the_database_spelling() {
        for class in [
            GoneClass::SoldLikely,
            GoneClass::SoldAfterCuts,
            GoneClass::Unknown,
            GoneClass::GoneBeforeFirstLook,
        ] {
            assert_eq!(GoneClass::parse(class.as_str()), class);
        }
        assert_eq!(GoneClass::parse("something else"), GoneClass::Unknown);
        // 两档都算"卖掉了",聚合表里的成交率数的就是它们。
        assert!(GoneClass::SoldLikely.looks_sold());
        assert!(GoneClass::SoldAfterCuts.looks_sold());
        assert!(!GoneClass::Unknown.looks_sold());
        // 一眼都没看清就没了的那种不算成交:我们连它标价多少都不知道。
        assert!(!GoneClass::GoneBeforeFirstLook.looks_sold());
    }

    /// 判定永远不会自己判出"第一眼就没了" —— 那一档是运行时直接写上去的
    /// (第一次 fetch 就回 null),不是从存活时间推出来的。
    #[test]
    fn classify_never_returns_gone_before_first_look() {
        let first = 1_000;
        for (last, gone) in [
            (first, first),
            (first + 10, first + 20),
            (first + 10 * HOUR, first + 11 * HOUR),
        ] {
            assert_ne!(
                classify_gone(first, last, gone, &[(first, 20_000)]),
                GoneClass::GoneBeforeFirstLook
            );
        }
    }

    /// 一个价落进哪一档,边界要钉死:挪错一格,"2 divine 的货卖得掉"这个
    /// 结论就会被记到 3 divine 头上。
    #[test]
    fn a_price_falls_into_the_greatest_bound_below_it() {
        let bound = |milli| price_bucket(milli).lower_bound_milli;
        // 不到 1 的全在最低那一档,负数(读坏的行)也是。
        assert_eq!(bound(0), 0);
        assert_eq!(bound(500), 0, "0.5 → 不到 1");
        assert_eq!(bound(999), 0);
        assert_eq!(bound(-1), 0, "无价单的哨兵不该冒到别的档去");
        // 整 1 就进 1 那一档。
        assert_eq!(bound(1_000), 1_000);
        assert_eq!(bound(1_999), 1_000);
        assert_eq!(bound(2_000), 2_000);
        assert_eq!(bound(2_500), 2_000, "2.5 divine 算 2 那一档");
        // 阶梯在 10 以上开始变疏:9.999 还在 9,12 落回 10。
        assert_eq!(bound(9_999), 9_000);
        assert_eq!(bound(10_000), 10_000);
        assert_eq!(bound(12_000), 10_000, "12 chaos 算 10 那一档");
        assert_eq!(bound(14_900), 10_000);
        assert_eq!(bound(15_000), 15_000);
        // 顶上那一档兜住所有更贵的。
        assert_eq!(bound(1_000_000), 1_000_000);
        assert_eq!(bound(9_999_999), 1_000_000);
        // 最低那一档要认得出自己 —— 它的写法和别人不一样。
        assert!(price_bucket(500).is_under_one());
        assert!(!price_bucket(1_000).is_under_one());
        // 阶梯本身:严格递增,而且从 0 起步。
        assert_eq!(PRICE_BUCKET_UNITS[0], 0);
        assert!(PRICE_BUCKET_UNITS.windows(2).all(|pair| pair[0] < pair[1]));
        // 每一档的下界自己落回自己那一档。
        for units in PRICE_BUCKET_UNITS {
            assert_eq!(bound(units * 1_000), units * 1_000, "{units} 那一档");
        }
    }

    /// 折成 divine 之后那条阶梯,1 以下多五根横档。
    ///
    /// 为什么只有折算过的那条要:按币种分的时候,"不到 1 chaos"里几乎没有货,
    /// 分再细也是空的;而全部折成 divine 之后,原先标着几十 chaos 的碑牌全落
    /// 在 1 divine 以下 —— 整批货挤在最低那一格里,等于什么都没说。
    #[test]
    fn the_divine_ladder_splits_everything_below_one() {
        let bound = |milli| divine_price_bucket(milli).lower_bound_milli;
        // 1 以下的五根横档:0.1、0.2、0.3、0.5、0.75。
        assert_eq!(bound(0), 0);
        assert_eq!(bound(99), 0, "不到 0.1 的还是落在最低那一格");
        assert_eq!(bound(100), 100);
        assert_eq!(bound(150), 100);
        assert_eq!(bound(200), 200);
        assert_eq!(bound(499), 300);
        assert_eq!(bound(500), 500);
        assert_eq!(bound(749), 500);
        assert_eq!(bound(750), 750);
        assert_eq!(bound(999), 750);
        // 1 以上和按币种那条阶梯一个字都不差。
        for milli in [1_000, 2_500, 9_999, 15_000, 1_000_000, 9_999_999] {
            assert_eq!(
                bound(milli),
                price_bucket(milli).lower_bound_milli,
                "{milli}"
            );
        }
        // 按币种那条阶梯**不动**:0.5 chaos 还是"不到 1 chaos"。
        assert_eq!(price_bucket(500).lower_bound_milli, 0);
        // 阶梯本身严格递增。
        assert!(
            SUB_DIVINE_BUCKET_MILLI
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert_eq!(SUB_DIVINE_BUCKET_MILLI[0], 100);
    }

    /// 阶梯的全程:10 分钟、30 分钟、2 小时、6 小时、1 天、3 天,
    /// 之后每 `recheck_interval_secs` 一次,七天到了就不再看了。
    #[test]
    fn the_check_ladder_walks_from_ten_minutes_to_seven_days() {
        let first = 1_000;
        let day: i64 = 86_400;
        let every_day = day as u64;
        let ladder: Vec<i64> = (0..6)
            .map(|rung| {
                next_check_after(first, rung, every_day).expect("still on the ladder") - first
            })
            .collect();
        assert_eq!(ladder, vec![600, 1_800, 7_200, 21_600, 86_400, 259_200]);

        // 最后一档之后就是"每 recheck_interval_secs 看一眼",从第 3 天起算。
        assert_eq!(next_check_after(first, 6, every_day), Some(first + 4 * day));
        assert_eq!(next_check_after(first, 7, every_day), Some(first + 5 * day));
        assert_eq!(next_check_after(first, 8, every_day), Some(first + 6 * day));
        // 第 7 天正好是"没人要"的分界线:到这里就不再花额度了。
        assert_eq!(next_check_after(first, 9, every_day), None);
        assert_eq!(next_check_after(first, 99, every_day), None);

        // 换一个更勤的回查间隔,只影响最后一档之后的那几次。
        assert_eq!(next_check_after(first, 5, 3_600), Some(first + 259_200));
        assert_eq!(
            next_check_after(first, 6, 3_600),
            Some(first + 259_200 + 3_600)
        );
        // 0 秒的间隔不能让它原地踏步(设置里兜着下限,这里再兜一次)。
        assert_eq!(next_check_after(first, 6, 0), Some(first + 259_201));
    }
}
