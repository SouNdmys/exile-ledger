//! poe.ninja 的经济接口(这一半是**有官方文档**的:<https://poe.ninja/docs/api>)。
//!
//! 两个用途:
//!
//! 1. **汇率** — 交易站的挂单币种五花八门,要换算到 divine 才能和你的价格上限比。
//! 2. **暗金参考价** — 热门暗金榜的"值多少钱""有几个人在卖"。
//!
//! 两个端点的 `core` 都会告诉你计价基准是什么币:交易所是 divine,
//! 物品榜是 exalted。**不要硬编码基准币**——它换过,以后还会换。

use std::collections::BTreeMap;

use serde::Deserialize;

/// 物品榜支持的暗金分类。poe.ninja 还有 `UniqueSanctumRelics`/`UniqueTablets`/
/// `PrecursorTablets`,但那些和 build 的装备栏对不上,热门榜用不到。
pub const UNIQUE_TYPES: [&str; 6] = [
    "UniqueWeapons",
    "UniqueArmours",
    "UniqueAccessories",
    "UniqueFlasks",
    "UniqueCharms",
    "UniqueJewels",
];

/// 交易所总览的表头。`rates` 是"1 个 primary 换多少个它"。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeCore {
    #[serde(default)]
    pub primary: String,
    #[serde(default)]
    pub secondary: String,
    #[serde(default)]
    pub rates: BTreeMap<String, f64>,
}

/// 一种通货的行情。`primary_value` 是"1 个它值多少个 primary"。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeLine {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub primary_value: f64,
    #[serde(default)]
    pub volume_primary_value: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeOverview {
    #[serde(default)]
    pub core: ExchangeCore,
    #[serde(default)]
    pub lines: Vec<ExchangeLine>,
}

/// 一个 divine 值多少 chaos / exalted / mirror。
///
/// 统一以 divine 为基准,是因为设置里的价格上限就是按 divine 填的。
/// 每一项都是 `Option`:某个币这一轮没行情很正常(mirror 有时整天没成交),
/// 这时应该标 `DifferentCurrency` 而不是猜一个数。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RatesPerDivine {
    pub chaos: Option<f64>,
    pub exalted: Option<f64>,
    pub mirror: Option<f64>,
}

impl ExchangeOverview {
    /// 换算成"每 divine"。基准币不是 divine 时返回 `None`——与其按 `secondary`
    /// 猜一遍换算链,不如让调用方知道这份数据它读不懂。
    #[must_use]
    pub fn rates_per_divine(&self) -> Option<RatesPerDivine> {
        if self.core.primary != "divine" {
            return None;
        }
        // mirror 比 divine 值钱,所以它在 lines 里而不是 rates 里,而且方向是反的:
        // primaryValue 400 的意思是"1 个 mirror = 400 divine",要取倒数。
        let mirror = self
            .lines
            .iter()
            .find(|line| line.id == "mirror")
            .map(|line| line.primary_value)
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| 1.0 / value);
        Some(RatesPerDivine {
            chaos: self.core.rates.get("chaos").copied(),
            exalted: self.core.rates.get("exalted").copied(),
            mirror,
        })
    }
}

/// 7 天走势。`data` 里会有 `null`(那天没成交),所以是 `Option<f64>`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SparkLine {
    #[serde(default)]
    pub total_change: f64,
    #[serde(default)]
    pub data: Vec<Option<f64>>,
}

/// 物品榜的表头。这里 `primary` 是 exalted,所以 `primary_value` 的单位是 exalted。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemCore {
    #[serde(default)]
    pub primary: String,
}

/// 一件暗金的参考价。`listing_count` 是挂单数——只有一两个挂单的"高价"不作数。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UniquePriceLine {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_type: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub level_required: i32,
    #[serde(default)]
    pub primary_value: f64,
    #[serde(default)]
    pub listing_count: i64,
    #[serde(default)]
    pub corrupted: bool,
    #[serde(default)]
    pub spark_line: SparkLine,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemOverview {
    #[serde(default)]
    pub core: ItemCore,
    #[serde(default)]
    pub lines: Vec<UniquePriceLine>,
}

#[cfg(test)]
mod economy_tests {
    use super::*;

    /// 线上原文剪出来的:留着 `items`、`maxVolumeCurrency`、`sparkline` 这些
    /// 我们不声明的字段,确认它们被忽略而不是让整包解析失败。
    const EXCHANGE_JSON: &str = r#"{
      "core": {
        "items": [{"id":"divine","name":"Divine Orb","category":"Currency"}],
        "rates": {"exalted": 83.42, "chaos": 25.21},
        "primary": "divine",
        "secondary": "exalted"
      },
      "lines": [
        {"id":"exalted","primaryValue":0.01157,"volumePrimaryValue":28832,
         "maxVolumeCurrency":"divine","maxVolumeRate":83.42,
         "sparkline":{"totalChange":-51.83,"data":[null,null,0,-51.83]}},
        {"id":"chaos","primaryValue":0.03987,"volumePrimaryValue":11879},
        {"id":"mirror","primaryValue":400,"volumePrimaryValue":133.3}
      ],
      "items": []
    }"#;

    #[test]
    fn parses_the_exchange_overview() {
        let overview: ExchangeOverview = serde_json::from_str(EXCHANGE_JSON).unwrap();
        assert_eq!(overview.core.primary, "divine");
        assert_eq!(overview.core.secondary, "exalted");
        assert_eq!(overview.core.rates["exalted"], 83.42);
        assert_eq!(overview.core.rates["chaos"], 25.21);
        assert_eq!(overview.lines.len(), 3);
        assert_eq!(overview.lines[0].id, "exalted");
        assert_eq!(overview.lines[0].volume_primary_value, 28_832.0);
    }

    #[test]
    fn converts_everything_to_per_divine() {
        let overview: ExchangeOverview = serde_json::from_str(EXCHANGE_JSON).unwrap();
        let rates = overview.rates_per_divine().unwrap();
        assert_eq!(rates.exalted, Some(83.42));
        assert_eq!(rates.chaos, Some(25.21));
        // 1 mirror = 400 divine,所以 1 divine = 0.0025 mirror。
        assert_eq!(rates.mirror, Some(0.0025));
    }

    /// mirror 那天没成交,或者基准币换了——两种情况都不能瞎猜。
    #[test]
    fn missing_currencies_stay_none() {
        let overview: ExchangeOverview = serde_json::from_str(
            r#"{"core":{"primary":"divine","rates":{"chaos":25.21}},"lines":[]}"#,
        )
        .unwrap();
        let rates = overview.rates_per_divine().unwrap();
        assert_eq!(rates.chaos, Some(25.21));
        assert_eq!(rates.exalted, None);
        assert_eq!(rates.mirror, None);
    }

    #[test]
    fn a_non_divine_base_gives_no_rates() {
        let overview: ExchangeOverview =
            serde_json::from_str(r#"{"core":{"primary":"exalted","rates":{"chaos":0.3}}}"#)
                .unwrap();
        assert_eq!(overview.rates_per_divine(), None);
    }

    #[test]
    fn a_zero_mirror_price_does_not_divide_by_zero() {
        let overview: ExchangeOverview = serde_json::from_str(
            r#"{"core":{"primary":"divine","rates":{}},"lines":[{"id":"mirror","primaryValue":0}]}"#,
        )
        .unwrap();
        assert_eq!(overview.rates_per_divine().unwrap().mirror, None);
    }

    /// 线上 `UniqueAccessories` 的第一行,剪掉了 icon / flavourText / 词缀文本。
    const ITEM_JSON: &str = r#"{
      "core": {"items": [], "rates": {}, "primary": "exalted", "secondary": "divine"},
      "lines": [
        {"id":523,"itemId":"Berek's Grip Two-Stone Ring","detailsId":"bereks-grip-two-stone-ring",
         "name":"Berek's Grip","baseType":"Two-Stone Ring","icon":"https://web.poecdn.com/x.png",
         "flavourText":"...","levelRequired":42,"category":"Ring",
         "primaryValue":240.0,"listingCount":24,"corrupted":false,
         "sparkLine":{"totalChange":0,"data":[null,null,null,null,null,null,0]},
         "implicitModifiers":[{"text":"+(12-16)% to Cold and Lightning Resistances","optional":false}],
         "explicitModifiers":[]},
        {"id":297,"name":"Yoke of Suffering","baseType":"Bloodstone Amulet","levelRequired":18,
         "category":"Amulet","primaryValue":120.0,"listingCount":36,"corrupted":false,
         "sparkLine":{"totalChange":-4.5,"data":[null,1.0,2.0]}}
      ]
    }"#;

    #[test]
    fn parses_the_unique_item_overview() {
        let overview: ItemOverview = serde_json::from_str(ITEM_JSON).unwrap();
        assert_eq!(overview.core.primary, "exalted");

        let berek = &overview.lines[0];
        assert_eq!(berek.id, 523);
        assert_eq!(berek.name, "Berek's Grip");
        assert_eq!(berek.base_type, "Two-Stone Ring");
        assert_eq!(berek.category, "Ring");
        assert_eq!(berek.level_required, 42);
        assert_eq!(berek.primary_value, 240.0);
        assert_eq!(berek.listing_count, 24);
        assert!(!berek.corrupted);
        assert_eq!(berek.spark_line.total_change, 0.0);
        assert_eq!(berek.spark_line.data.len(), 7);
        assert_eq!(berek.spark_line.data[6], Some(0.0));
        assert_eq!(berek.spark_line.data[0], None);

        assert_eq!(overview.lines[1].spark_line.total_change, -4.5);
        assert_eq!(overview.lines[1].spark_line.data[1], Some(1.0));
    }

    #[test]
    fn the_six_unique_types_are_locked() {
        assert_eq!(
            UNIQUE_TYPES,
            [
                "UniqueWeapons",
                "UniqueArmours",
                "UniqueAccessories",
                "UniqueFlasks",
                "UniqueCharms",
                "UniqueJewels",
            ]
        );
    }
}
