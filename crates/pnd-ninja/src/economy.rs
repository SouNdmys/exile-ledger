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

impl SparkLine {
    /// 7 天涨跌。`None` 的意思是**这一周没有可比的历史**,不是"没涨没跌"。
    ///
    /// 这个区别是 7 天那一列唯一的难点。新联赛开了没几天时,poe.ninja 给的
    /// `data` 长这样:`[null, null, null, null, null, null, 0]` —— 一整周只有
    /// 今天一个点。一个点算不出变化,所以它照实写 `totalChange: 0`。
    /// 把这个 0 原样存下来、原样画出去,界面上四百多件暗金就全是 `+0%`,
    /// 读起来像"这一周整个市场纹丝不动",而真相是"还没有一周的数据"。
    ///
    /// 所以规矩是:`data` 里至少要有**两个**真数,这条走势才算数。
    #[must_use]
    pub fn change_percent(&self) -> Option<f64> {
        let points = self.data.iter().flatten().count();
        (points >= 2 && self.total_change.is_finite()).then_some(self.total_change)
    }
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

    /// 2026-09-07 从线上 `UniqueWeapons` 剪下来的四行(icon、风味文本、词缀文本
    /// 删掉了,`core.items` 清空,别的一个字没动)。
    ///
    /// 留一份真原文是因为 7 天那一列的 bug 完全是**数据形状**的问题:
    /// 手编一份"看起来对"的 JSON 永远编不出 `[null × 6, 0]` 这个形状,
    /// 而线上 140 行里有 121 行长这样。
    const UNIQUE_WEAPONS: &str = include_str!("../fixtures/unique_weapons_overview.json");

    /// 只有一个数据点时,7 天涨跌是"不知道",不是"0%"。
    ///
    /// 这就是"几乎每件暗金都显示 +0%"的根:线上 140 行里 121 行的 `data` 是
    /// `[null × 6, 0]`,`totalChange` 于是照实写 0。把这个 0 当成"这周没变"
    /// 存下来,整张榜就变成一片绿油油的 `+0%`。
    #[test]
    fn a_sparkline_with_a_single_point_has_no_seven_day_change() {
        let overview: ItemOverview = serde_json::from_str(UNIQUE_WEAPONS).unwrap();
        assert_eq!(overview.lines.len(), 4);

        let ordained = &overview.lines[0];
        assert_eq!(ordained.name, "The Ordained");
        assert_eq!(ordained.spark_line.total_change, 0.0);
        assert_eq!(ordained.spark_line.data.len(), 7);
        assert_eq!(
            ordained.spark_line.data.iter().flatten().count(),
            1,
            "一整周只有今天一个点"
        );
        assert_eq!(
            ordained.spark_line.change_percent(),
            None,
            "一个点算不出一周的涨跌,该说不知道而不是说 0%"
        );

        // 两个点的那条才算数,而且值原样保留(-99.53,不是四舍五入的 -100)。
        let trenchtimbre = &overview.lines[2];
        assert_eq!(trenchtimbre.base_type, "Spiked Club");
        assert_eq!(trenchtimbre.listing_count, 1_509);
        assert_eq!(trenchtimbre.spark_line.change_percent(), Some(-99.53));

        // 同一个名字的另一个底子只有一个点:它是 `None`,不是 0%。
        let runeforged = &overview.lines[1];
        assert_eq!(runeforged.base_type, "Runeforged Spiked Club");
        assert_eq!(runeforged.spark_line.change_percent(), None);

        assert_eq!(overview.lines[3].spark_line.change_percent(), Some(-95.73));
    }

    /// 手编的几种边角形状。
    #[test]
    fn only_two_real_points_make_a_seven_day_change() {
        let spark = |json: &str| -> SparkLine { serde_json::from_str(json).unwrap() };
        // 压根没有 `sparkLine` 字段时的默认值:空数组,当然不知道。
        assert_eq!(SparkLine::default().change_percent(), None);
        assert_eq!(
            spark(r#"{"totalChange":-4.5,"data":[]}"#).change_percent(),
            None
        );
        assert_eq!(
            spark(r#"{"totalChange":-4.5,"data":[null,null,3.0]}"#).change_percent(),
            None,
            "只有一个真数还是不知道"
        );
        assert_eq!(
            spark(r#"{"totalChange":-4.5,"data":[null,1.0,2.0]}"#).change_percent(),
            Some(-4.5)
        );
        // 两个点、真的没变:这时候的 0% 是"这周确实平"，该显示出来。
        assert_eq!(
            spark(r#"{"totalChange":0,"data":[0,null,0]}"#).change_percent(),
            Some(0.0)
        );
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
