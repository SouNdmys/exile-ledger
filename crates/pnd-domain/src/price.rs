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

impl CurrencyRates {
    /// 一张空表:还没读到经济接口时用它,异币种一律判 `DifferentCurrency`。
    pub fn none() -> CurrencyRates {
        CurrencyRates::default()
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

/// 换算成 divine(千分整数)。汇率缺一个就返回 `None`,绝不用默认值糊弄。
///
/// 中间用 i128 是因为 `amount_milli * 1000` 在 mirror 这种大数上会顶到 i64 边缘;
/// 除法向零取整,误差不超过 0.001 divine,对"够不够便宜"这个判断无所谓。
pub fn to_divine_milli(price: &Price, rates: &CurrencyRates) -> Option<i64> {
    if price.currency == Currency::Divine {
        return Some(price.amount_milli);
    }
    let rate_milli = rates.per_divine_milli(&price.currency)?;
    if rate_milli <= 0 {
        return None;
    }
    let divine_milli = (price.amount_milli as i128) * 1000 / (rate_milli as i128);
    i64::try_from(divine_milli).ok()
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

    match (to_divine_milli(price, rates), to_divine_milli(cap, rates)) {
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
        assert_eq!(to_divine_milli(&divine(3.0), &rates), Some(3_000));
        assert_eq!(
            to_divine_milli(&Price::from_trade(90.0, "chaos"), &rates),
            Some(3_570)
        );
        assert_eq!(
            to_divine_milli(&Price::from_trade(1.0, "mirror"), &rates),
            None
        );
        assert_eq!(
            to_divine_milli(&Price::from_trade(1.0, "regal"), &rates),
            None
        );
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
