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
/// 故意只有三档:再细分下去(比如"卖家整批撤单")需要回头查那个卖家的
/// 其它挂单,那是第二版的事(见计划里的 `delisted_likely`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoneClass {
    /// 挂了一阵、一次没降价、然后没了。
    SoldLikely,
    /// 降过价之后没了。
    SoldAfterCuts,
    /// 存活太短,看不出来 —— 多半是挂错了撤掉的。
    Unknown,
}

impl GoneClass {
    /// 存库时的写法。存字符串而不是数字:直接打开 `watch.sqlite` 也看得懂。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GoneClass::SoldLikely => "sold_likely",
            GoneClass::SoldAfterCuts => "sold_after_cuts",
            GoneClass::Unknown => "unknown",
        }
    }

    /// 认不出来的一律当 `Unknown`:老库里的值、手改过的值,都不该让整行读不出来。
    #[must_use]
    pub fn parse(raw: &str) -> GoneClass {
        match raw {
            "sold_likely" => GoneClass::SoldLikely,
            "sold_after_cuts" => GoneClass::SoldAfterCuts,
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
        ] {
            assert_eq!(GoneClass::parse(class.as_str()), class);
        }
        assert_eq!(GoneClass::parse("something else"), GoneClass::Unknown);
        // 两档都算"卖掉了",聚合表里的成交率数的就是它们。
        assert!(GoneClass::SoldLikely.looks_sold());
        assert!(GoneClass::SoldAfterCuts.looks_sold());
        assert!(!GoneClass::Unknown.looks_sold());
    }
}
