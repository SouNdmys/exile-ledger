//! 判定 + 去重 + 合成一张卡片。纯函数,不碰库也不碰时钟。
//!
//! 一条挂单要走完两道关才值得把你叫过来:
//! 1. **还没为它叫过**。`seen_listings` 的主键带着价格,所以"同一件东西降价
//!    重挂"算新单(值得再叫一次),原价还挂在那儿则不算;而当初太贵、只被
//!    记了一笔的那些,在你把上限提上去之后照样算新单 —— 那一声从没响过。
//! 2. **够便宜**。判定本身住在 `pnd-domain::judge` 里,这里只负责把
//!    "见没见过"和"够不够便宜"这两件事拼起来。
//!
//! 为什么要合成:一轮轮询可能一口气命中三件。三张卡片、三段报警音,
//! 是在惩罚你运气好。所以一轮只出一张卡,标题写最便宜那件,右下角写 "+2"。

use pnd_domain::{CurrencyRates, ListingSummary, Price, PriceCap, Verdict, WatchId, judge};
use pnd_storage::{AlertSource, SeenOutcome};

/// 一轮轮询(或者一次 live 推送)合出来的那一张卡片。
///
/// `alert_ids` 是这一批**每一条**命中在提醒历史里的行号,不只是标题那条:
/// 卡片上点"忽略"要把这一批一起标记掉,提醒记录页也要能一条条列出来。
#[derive(Debug, Clone, PartialEq)]
pub struct MatchedListing {
    pub alert_ids: Vec<i64>,
    pub watch_id: WatchId,
    pub label: String,
    pub league: String,
    pub search_id: String,
    /// 卡片标题要显示的那一条。
    pub headline: ListingSummary,
    /// 除了标题之外还有几条(卡片上的 "+N")。
    pub extra: usize,
    pub cap: Price,
    pub source: AlertSource,
}

/// 一条挂单的处置。
///
/// `Alert` 里的判定永远是 [`Verdict::Hit`],带着它只是为了让调用方不用再判一次;
/// `Record` 是"记下来,但不值得叫你"(太贵、异币种换不出、没标价)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Alert(Verdict),
    Record(Verdict),
    Ignore,
}

/// 这条挂单该怎么办。
///
/// **已经叫过的一律 `Ignore`,连判定都不看** —— 这是"响一次就够"的全部实现。
/// 一件 15 divine 的东西挂在那儿三天,前两天我们已经叫过了,第三天它还是
/// 便宜,但你早就知道了。(判断"叫过没有"是 `record_listing` 的活。)
pub fn decide(
    cap: &PriceCap,
    listing: &ListingSummary,
    seen: SeenOutcome,
    rates: &CurrencyRates,
) -> Decision {
    if seen == SeenOutcome::AlreadySeen {
        return Decision::Ignore;
    }
    let verdict = judge(cap, listing.price.as_ref(), rates);
    if verdict == Verdict::Hit {
        Decision::Alert(verdict)
    } else {
        Decision::Record(verdict)
    }
}

/// 一批命中 → (标题那条, 还剩几条)。空批返回 `None`。
///
/// 标题取"以 divine 计最便宜"的那条;一条都不是 divine 计价时取第一条 ——
/// search 是按价升序回来的,第一条本来就是服务端眼里最便宜的那件。
/// 这里不换算异币种:换算要汇率,而汇率是会变的运行时状态,不该混进纯函数;
/// 何况标题只是"给你看一眼",真正的判定在 [`decide`] 里已经做完了。
pub fn coalesce(hits: Vec<(i64, ListingSummary)>) -> Option<(ListingSummary, usize)> {
    let extra = hits.len().checked_sub(1)?;
    let cheapest = hits
        .iter()
        .filter_map(|(_, listing)| divine_milli(listing).map(|milli| (milli, listing)))
        .min_by_key(|(milli, _)| *milli)
        .map(|(_, listing)| listing.clone());
    let headline = match cheapest {
        Some(listing) => listing,
        None => hits.into_iter().next()?.1,
    };
    Some((headline, extra))
}

/// 以 divine 标价的金额;别的货币(或者没标价)返回 `None`。
fn divine_milli(listing: &ListingSummary) -> Option<i64> {
    let price = listing.price.as_ref()?;
    (price.currency == pnd_domain::Currency::Divine).then_some(price.amount_milli)
}

#[cfg(test)]
mod decide_tests {
    use super::*;
    use pnd_domain::Currency;

    fn listing(id: &str, price: Option<Price>) -> ListingSummary {
        ListingSummary {
            id: id.to_string(),
            item_name: "Choir of the Storm".to_string(),
            type_line: "Lapis Amulet".to_string(),
            price,
            account: "Seller".to_string(),
            character: "Char".to_string(),
            online: true,
            afk: false,
            indexed: "2026-09-06T12:00:00Z".to_string(),
            whisper: "@Char hi".to_string(),
            whisper_token: None,
            hideout_token: None,
            icon: String::new(),
            item_json: String::new(),
        }
    }

    fn divine(amount: f64) -> Price {
        Price::from_trade(amount, "divine")
    }

    fn rates() -> CurrencyRates {
        CurrencyRates {
            chaos_per_divine_milli: Some(25_210),
            exalted_per_divine_milli: Some(83_420),
            mirror_per_divine_milli: None,
        }
    }

    #[test]
    fn a_new_cheap_listing_is_worth_a_card() {
        let decision = decide(
            &divine(20.0),
            &listing("a", Some(divine(15.0))),
            SeenOutcome::New,
            &rates(),
        );
        assert_eq!(decision, Decision::Alert(Verdict::Hit));
    }

    #[test]
    fn a_listing_we_already_saw_is_never_worth_a_second_card() {
        // 同一件东西、同一个价:上次已经叫过了。
        let decision = decide(
            &divine(20.0),
            &listing("a", Some(divine(15.0))),
            SeenOutcome::AlreadySeen,
            &rates(),
        );
        assert_eq!(decision, Decision::Ignore);
    }

    #[test]
    fn new_but_not_cheap_enough_is_only_recorded() {
        let cap = divine(20.0);
        assert_eq!(
            decide(
                &cap,
                &listing("a", Some(divine(25.0))),
                SeenOutcome::New,
                &rates()
            ),
            Decision::Record(Verdict::TooExpensive)
        );
        assert_eq!(
            decide(&cap, &listing("a", None), SeenOutcome::New, &rates()),
            Decision::Record(Verdict::Unpriced)
        );
        // 有 chaos/exalted 汇率也换不出 mirror,不猜价。
        assert_eq!(
            decide(
                &cap,
                &listing("a", Some(Price::from_trade(1.0, "mirror"))),
                SeenOutcome::New,
                &rates()
            ),
            Decision::Record(Verdict::DifferentCurrency)
        );
    }

    #[test]
    fn cross_currency_hits_still_alert_when_rates_are_known() {
        // 90 chaos ≈ 3.6 divine。
        let decision = decide(
            &divine(20.0),
            &listing("a", Some(Price::from_trade(90.0, "chaos"))),
            SeenOutcome::New,
            &rates(),
        );
        assert_eq!(decision, Decision::Alert(Verdict::Hit));
    }

    #[test]
    fn coalesce_picks_the_cheapest_and_counts_the_rest() {
        let hits = vec![
            (1, listing("a", Some(divine(18.0)))),
            (2, listing("b", Some(divine(9.0)))),
            (3, listing("c", Some(divine(12.0)))),
        ];
        let (headline, extra) = coalesce(hits).unwrap();
        assert_eq!(headline.id, "b");
        assert_eq!(extra, 2);
    }

    #[test]
    fn coalesce_falls_back_to_the_first_when_nothing_is_priced_in_divine() {
        let hits = vec![
            (1, listing("a", Some(Price::from_trade(80.0, "chaos")))),
            (2, listing("b", None)),
        ];
        let (headline, extra) = coalesce(hits).unwrap();
        assert_eq!(
            headline.id, "a",
            "search 是按价升序回来的,第一条就是最便宜那条"
        );
        assert_eq!(extra, 1);
    }

    #[test]
    fn coalesce_of_a_single_hit_has_no_extras() {
        let (headline, extra) = coalesce(vec![(7, listing("only", Some(divine(1.0))))]).unwrap();
        assert_eq!(headline.id, "only");
        assert_eq!(extra, 0);
        assert_eq!(headline.price, Some(Price::new(1_000, Currency::Divine)));
    }

    #[test]
    fn coalesce_of_nothing_is_nothing() {
        assert_eq!(coalesce(Vec::new()), None);
    }
}
