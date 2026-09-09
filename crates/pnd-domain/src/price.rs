//! 价格、货币、汇率换算和"这单值不值得叫你"的判定。
//!
//! 金额一律用千分整数 `amount_milli` 存(0.5 divine = 500),不碰浮点:
//! 去重键要精确相等,浮点会让"同一个价格"时不时不相等。

use serde::{Deserialize, Serialize};

/// 交易站用得上的几种通货。不认识的原样留在 `Other` 里,不猜、不丢。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Currency {
    Chaos,
    Divine,
    Exalted,
    Mirror,
    Other(String),
}

impl Currency {
    /// 认交易站接口里的货币串,大小写不敏感;认不出的保留原文。
    pub fn parse(s: &str) -> Currency {
        match s.to_ascii_lowercase().as_str() {
            "chaos" => Currency::Chaos,
            "divine" => Currency::Divine,
            "exalted" => Currency::Exalted,
            "mirror" => Currency::Mirror,
            _ => Currency::Other(s.to_string()),
        }
    }

    /// 规范化的小写 id —— 存库、拼 JSON、和 ninja 汇率表对账都用它。
    pub fn code(&self) -> &str {
        match self {
            Currency::Chaos => "chaos",
            Currency::Divine => "divine",
            Currency::Exalted => "exalted",
            Currency::Mirror => "mirror",
            Currency::Other(s) => s,
        }
    }
}

// serde 走普通字符串:settings.json 里写 "divine" 比写 {"Divine":null} 好读得多。
impl From<String> for Currency {
    fn from(s: String) -> Currency {
        Currency::parse(&s)
    }
}

impl From<Currency> for String {
    fn from(c: Currency) -> String {
        match c {
            Currency::Other(s) => s,
            other => other.code().to_string(),
        }
    }
}

/// 一个价格 = 数量(千分整数)+ 货币。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    pub amount_milli: i64,
    pub currency: Currency,
}

impl Price {
    pub fn new(amount_milli: i64, currency: Currency) -> Price {
        Price {
            amount_milli,
            currency,
        }
    }

    /// 从交易站返回的 `price{amount, currency}` 转过来:浮点只在这一处出现,
    /// 四舍五入到千分位之后就再也不用浮点了。
    pub fn from_trade(amount: f64, currency: &str) -> Price {
        Price {
            amount_milli: (amount * 1000.0).round() as i64,
            currency: Currency::parse(currency),
        }
    }

    /// 给人看的写法:`15 divine`、`0.5 divine`、`90 chaos`(最多三位小数,末尾零去掉)。
    pub fn display(&self) -> String {
        let whole = self.amount_milli / 1000;
        let frac = (self.amount_milli % 1000).unsigned_abs();
        let amount = if frac == 0 {
            whole.to_string()
        } else {
            let frac_str = format!("{frac:03}");
            let frac_str = frac_str.trim_end_matches('0');
            // -0.5 的整数部分是 0,负号会在格式化时丢掉,得自己补。
            if self.amount_milli < 0 && whole == 0 {
                format!("-0.{frac_str}")
            } else {
                format!("{whole}.{frac_str}")
            }
        };
        format!("{amount} {}", self.currency.code())
    }
}

/// 价格上限就是一个价格:挂单低于等于它才叫你。
pub type PriceCap = Price;

/// poe.ninja 的换算表:每 1 divine 值多少 chaos / exalted / mirror(同样 ×1000 存)。
///
/// 用 `Option` 而不是默认值:汇率拿不到的时候要能说"不知道",而不是拿一个瞎编的数去比价。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrencyRates {
    pub chaos_per_divine_milli: Option<i64>,
    pub exalted_per_divine_milli: Option<i64>,
    pub mirror_per_divine_milli: Option<i64>,
}

/// 手填的汇率。`None` = 这一档听 poe.ninja 的。
///
/// 为什么要有它:交易站自己那个"折合 exalted"的价格筛选按一个和市面差得
/// 很远的内部汇率换算(2026-09-09 它把 700 exalted 当成 8 divine,而市面上
/// 1 divine ≈ 186 exalted),而且一旦按某种货币筛,别的货币标价的挂单会
/// **整批消失**。所以价格区间搬进这个程序里判,而程序手上那份汇率得能被
/// 人工顶掉 —— poe.ninja 的经济接口也会有跟不上的那一天。
///
/// 只给 chaos 和 exalted:mirror 那一档是给"一件镜子货"用的,填它没有意义。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateOverride {
    pub chaos_per_divine_milli: Option<i64>,
    pub exalted_per_divine_milli: Option<i64>,
}

/// 一档汇率是从哪儿来的。界面上那句「(poe.ninja)」/「(手动)」写的就是它。
///
/// 要单独记一笔,是因为"这个数是我自己填的"和"这个数是接口给的"在出错时
/// 是两条完全不同的路:前者去设置页改,后者去看 ninja 是不是又抽风了。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateSource {
    /// 这一档没有汇率:接口没给,人也没填。
    #[default]
    Unknown,
    /// poe.ninja 的经济接口。
    Ninja,
    /// 设置页里手填的那个数。
    Manual,
}

/// 三档货币各自的来路。和 [`CurrencyRates`] 分开放,是因为汇率表本身会
/// 被存进事件、传给纯判定,而"来路"只有界面关心。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateSources {
    pub chaos: RateSource,
    pub exalted: RateSource,
    pub mirror: RateSource,
}

impl CurrencyRates {
    /// 一张空表:还没读到经济接口时用它,异币种一律判 `DifferentCurrency`。
    pub fn none() -> CurrencyRates {
        CurrencyRates::default()
    }

    /// 手填的那几档盖上去,没填的原样留着。
    ///
    /// `or` 而不是 `unwrap_or`:填了就用填的(哪怕接口那一档也有值),
    /// 没填就退回接口那一档 —— 清空输入框于是真的等于"重新听 poe.ninja 的"。
    #[must_use]
    pub fn with_override(&self, manual: &RateOverride) -> CurrencyRates {
        CurrencyRates {
            chaos_per_divine_milli: manual
                .chaos_per_divine_milli
                .or(self.chaos_per_divine_milli),
            exalted_per_divine_milli: manual
                .exalted_per_divine_milli
                .or(self.exalted_per_divine_milli),
            // mirror 填不了,所以它永远是接口那一档。
            mirror_per_divine_milli: self.mirror_per_divine_milli,
        }
    }

    /// 盖完之后每一档各是从哪儿来的。顺序和 [`CurrencyRates::with_override`]
    /// 一致 —— 两个方法必须对同一份输入给出对得上的答案。
    #[must_use]
    pub fn rate_sources(&self, manual: &RateOverride) -> RateSources {
        RateSources {
            chaos: rate_source(self.chaos_per_divine_milli, manual.chaos_per_divine_milli),
            exalted: rate_source(
                self.exalted_per_divine_milli,
                manual.exalted_per_divine_milli,
            ),
            mirror: rate_source(self.mirror_per_divine_milli, None),
        }
    }

    /// 换算成 divine(千分整数)。汇率缺一个就返回 `None`,绝不用默认值糊弄。
    ///
    /// 中间用 i128 是因为 `amount_milli * 1000` 在 mirror 这种大数上会顶到
    /// i64 边缘;除法向零取整,误差不超过 0.001 divine,对"够不够便宜"这个
    /// 判断无所谓。
    #[must_use]
    pub fn to_divine_milli(&self, price: &Price) -> Option<i64> {
        if price.currency == Currency::Divine {
            return Some(price.amount_milli);
        }
        let rate_milli = self.per_divine_milli(&price.currency)?;
        if rate_milli <= 0 {
            return None;
        }
        let divine_milli = (price.amount_milli as i128) * 1000 / (rate_milli as i128);
        i64::try_from(divine_milli).ok()
    }

    fn per_divine_milli(&self, currency: &Currency) -> Option<i64> {
        match currency {
            Currency::Chaos => self.chaos_per_divine_milli,
            Currency::Exalted => self.exalted_per_divine_milli,
            Currency::Mirror => self.mirror_per_divine_milli,
            Currency::Divine | Currency::Other(_) => None,
        }
    }
}

/// 手填的优先,其次接口,都没有就是"不知道"。
fn rate_source(from_ninja: Option<i64>, manual: Option<i64>) -> RateSource {
    if manual.is_some() {
        RateSource::Manual
    } else if from_ninja.is_some() {
        RateSource::Ninja
    } else {
        RateSource::Unknown
    }
}

/// 一条挂单相对你设的上限是什么结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Hit,
    TooExpensive,
    DifferentCurrency,
    Unpriced,
}

/// 判定一条挂单。
///
/// 同币种直接比整数,连汇率都不用查 —— 大多数情况走的就是这条路,也最不会出错。
/// 异币种才去换算;换不出来就老实说 `DifferentCurrency`,不猜价。
pub fn judge(cap: &PriceCap, price: Option<&Price>, rates: &CurrencyRates) -> Verdict {
    let Some(price) = price else {
        return Verdict::Unpriced;
    };

    if price.currency == cap.currency {
        return if price.amount_milli <= cap.amount_milli {
            Verdict::Hit
        } else {
            Verdict::TooExpensive
        };
    }

    match (rates.to_divine_milli(price), rates.to_divine_milli(cap)) {
        (Some(listed), Some(limit)) if listed <= limit => Verdict::Hit,
        (Some(_), Some(_)) => Verdict::TooExpensive,
        _ => Verdict::DifferentCurrency,
    }
}

#[cfg(test)]
mod price_tests {
    use super::*;

    fn divine(amount: f64) -> Price {
        Price::from_trade(amount, "divine")
    }

    /// 今天从 ninja 经济接口量到的:1 divine = 25.21 chaos / 83.42 exalted。
    fn rates_today() -> CurrencyRates {
        CurrencyRates {
            chaos_per_divine_milli: Some(25_210),
            exalted_per_divine_milli: Some(83_420),
            mirror_per_divine_milli: None,
        }
    }

    #[test]
    fn from_trade_rounds_to_milli() {
        assert_eq!(Price::from_trade(0.5, "divine").amount_milli, 500);
        assert_eq!(Price::from_trade(15.0, "divine").amount_milli, 15_000);
        assert_eq!(Price::from_trade(0.75, "chaos").amount_milli, 750);
        assert_eq!(Price::from_trade(1.0, "Divine").currency, Currency::Divine);
        assert_eq!(
            Price::from_trade(1.0, "regal").currency,
            Currency::Other("regal".to_string())
        );
    }

    #[test]
    fn displays_without_trailing_zeros() {
        assert_eq!(divine(15.0).display(), "15 divine");
        assert_eq!(divine(0.5).display(), "0.5 divine");
        assert_eq!(Price::from_trade(90.0, "chaos").display(), "90 chaos");
        assert_eq!(Price::from_trade(1.25, "exalted").display(), "1.25 exalted");
        assert_eq!(Price::from_trade(0.001, "chaos").display(), "0.001 chaos");
    }

    #[test]
    fn same_currency_compares_directly() {
        let cap = divine(20.0);
        assert_eq!(
            judge(&cap, Some(&divine(20.0)), &CurrencyRates::none()),
            Verdict::Hit
        );
        assert_eq!(
            judge(&cap, Some(&divine(15.0)), &CurrencyRates::none()),
            Verdict::Hit
        );
        assert_eq!(
            judge(&cap, Some(&divine(25.0)), &CurrencyRates::none()),
            Verdict::TooExpensive
        );
    }

    /// 上限本身算命中("≤ 上限",不是"< 上限")。
    ///
    /// 界面上那句说明照着这条写。差一个等号就是"我填 223,市面上摆着一件
    /// 223,程序一声不响" —— 这个边界值得单独钉一次。
    #[test]
    fn a_listing_priced_exactly_at_the_cap_is_a_hit() {
        let cap = divine(223.0);
        assert_eq!(
            judge(&cap, Some(&divine(223.0)), &CurrencyRates::none()),
            Verdict::Hit
        );
        assert_eq!(
            judge(&cap, Some(&divine(222.999)), &CurrencyRates::none()),
            Verdict::Hit
        );
        assert_eq!(
            judge(&cap, Some(&divine(223.001)), &CurrencyRates::none()),
            Verdict::TooExpensive
        );
        // 换算过去正好等于上限的异币种也一样算命中。
        let rates = CurrencyRates {
            chaos_per_divine_milli: Some(10_000),
            ..CurrencyRates::none()
        };
        assert_eq!(
            judge(
                &divine(2.0),
                Some(&Price::from_trade(20.0, "chaos")),
                &rates
            ),
            Verdict::Hit
        );
    }

    #[test]
    fn cross_currency_needs_rates() {
        let cap = divine(20.0);
        let ninety_chaos = Price::from_trade(90.0, "chaos");
        // 90 chaos ≈ 3.57 divine,远低于 20 divine 的上限。
        assert_eq!(
            judge(&cap, Some(&ninety_chaos), &rates_today()),
            Verdict::Hit
        );
        assert_eq!(
            judge(&cap, Some(&ninety_chaos), &CurrencyRates::none()),
            Verdict::DifferentCurrency
        );
        // 有 chaos 汇率也换不出 mirror,不猜。
        assert_eq!(
            judge(
                &cap,
                Some(&Price::from_trade(1.0, "mirror")),
                &rates_today()
            ),
            Verdict::DifferentCurrency
        );
        // 换得出但确实太贵。
        assert_eq!(
            judge(
                &cap,
                Some(&Price::from_trade(2000.0, "exalted")),
                &rates_today()
            ),
            Verdict::TooExpensive
        );
    }

    #[test]
    fn unpriced_listing_is_its_own_verdict() {
        assert_eq!(
            judge(&divine(20.0), None, &rates_today()),
            Verdict::Unpriced
        );
    }

    #[test]
    fn converts_to_divine() {
        let rates = rates_today();
        assert_eq!(rates.to_divine_milli(&divine(3.0)), Some(3_000));
        assert_eq!(
            rates.to_divine_milli(&Price::from_trade(90.0, "chaos")),
            Some(3_570)
        );
        assert_eq!(
            rates.to_divine_milli(&Price::from_trade(1.0, "mirror")),
            None
        );
        assert_eq!(
            rates.to_divine_milli(&Price::from_trade(1.0, "regal")),
            None
        );
    }

    /// 手填的汇率盖过 poe.ninja 那个,清空就退回去。
    ///
    /// 这一条是整件事的起点:交易站自己那个"等值"筛选把 700 exalted 当成
    /// 8 divine(市面上是 1 divine ≈ 186 exalted),所以价格区间搬进程序里,
    /// 而程序手上那份汇率必须能被人工顶掉。
    #[test]
    fn a_manual_rate_beats_the_one_from_ninja_and_clearing_it_falls_back() {
        let ninja = rates_today();
        let manual = RateOverride {
            chaos_per_divine_milli: Some(13_000),
            exalted_per_divine_milli: None,
        };
        let merged = ninja.with_override(&manual);
        assert_eq!(merged.chaos_per_divine_milli, Some(13_000), "手填的说了算");
        assert_eq!(
            merged.exalted_per_divine_milli, ninja.exalted_per_divine_milli,
            "没填的那一档还是 poe.ninja 的"
        );
        assert_eq!(
            merged.mirror_per_divine_milli,
            ninja.mirror_per_divine_milli
        );

        // 清空之后一切照旧 —— 手填过一次不该从此和 ninja 断了。
        assert_eq!(ninja.with_override(&RateOverride::default()), ninja);

        // 一张空表 + 手填的数,照样能换算:这正是 ninja 读不到时的救急路。
        let only_manual = CurrencyRates::none().with_override(&manual);
        assert_eq!(
            only_manual.to_divine_milli(&Price::from_trade(65.0, "chaos")),
            Some(5_000),
            "65 chaos ÷ 13 = 5 divine"
        );
    }

    /// 每一档汇率都要说得出自己是从哪儿来的 —— 界面上那句话靠它。
    #[test]
    fn each_currency_says_where_its_rate_came_from() {
        let ninja = rates_today();
        let manual = RateOverride {
            chaos_per_divine_milli: Some(13_000),
            exalted_per_divine_milli: None,
        };
        assert_eq!(
            ninja.rate_sources(&manual),
            RateSources {
                chaos: RateSource::Manual,
                exalted: RateSource::Ninja,
                // 今天这张表里没有 mirror,而 mirror 是填不了的。
                mirror: RateSource::Unknown,
            }
        );
        // 什么都没有的时候三档都是"不知道",而不是默认成 poe.ninja。
        assert_eq!(
            CurrencyRates::none().rate_sources(&RateOverride::default()),
            RateSources::default()
        );
        assert_eq!(RateSources::default().chaos, RateSource::Unknown);
    }

    #[test]
    fn currency_serializes_as_plain_string() {
        let price = Price::new(20_000, Currency::Divine);
        let text = serde_json::to_string(&price).unwrap();
        assert_eq!(text, r#"{"amount_milli":20000,"currency":"divine"}"#);
        assert_eq!(serde_json::from_str::<Price>(&text).unwrap(), price);

        let other: Currency = serde_json::from_str(r#""regal""#).unwrap();
        assert_eq!(other, Currency::Other("regal".to_string()));
        assert_eq!(serde_json::to_string(&other).unwrap(), r#""regal""#);
    }
}
