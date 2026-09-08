//! poe.ninja 的经济接口(这一半是**有官方文档**的:<https://poe.ninja/docs/api>)。
//!
//! 两个用途:
//!
//! 1. **汇率** — 交易站的挂单币种五花八门,要换算到 divine 才能和你的价格上限比。
//! 2. **暗金参考价** — 热门暗金榜的"值多少钱""有几个人在卖"。
//!
//! 两个端点的 `core` 都会告诉你计价基准是什么币,**读它,别硬编码**——
//! 物品榜的基准币 2026-09-06 还是 `exalted`,2026-09-07 就成了 `divine`。
//! 硬编码那一版把 `0.08548`(≈ 8 个 exalted)读成了八分钱。

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::client::Game;

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

/// PoE1 的同一张表:分类名是**单数**,而且没有 charm(PoE2 才有的部位)。
///
/// 名字写错的下场是 **404**,不是空榜:2026-09-09 实测,同一条路径
/// `type=UniqueWeapons`(复数)404、`type=UniqueWeapon` 200。所以这两张表
/// 宁可写死,也不按规律猜。
///
/// 只有 `UniqueWeapon` 是当场验过的,另外四个是照 poe.ninja 文档里的写法填的,
/// 第一次跑 PoE1 采样时要盯一眼有没有 404。
const UNIQUE_TYPES_POE1: [&str; 5] = [
    "UniqueWeapon",
    "UniqueArmour",
    "UniqueAccessory",
    "UniqueFlask",
    "UniqueJewel",
];

/// 这一代该问哪些暗金分类。
#[must_use]
pub fn unique_types_for(game: Game) -> &'static [&'static str] {
    match game {
        Game::Poe1 => &UNIQUE_TYPES_POE1,
        Game::Poe2 => &UNIQUE_TYPES,
    }
}

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
    /// 换算成"每 divine"。
    ///
    /// 两代的基准币不一样:PoE2 以 divine 计价,PoE1 以 chaos 计价。
    /// divine 基准那条路原样保留(它读的是 `core.rates`,那是 poe.ninja 自己
    /// 算好的成交价,比拿 lines 相除准);别的基准走下面那条通用路。
    #[must_use]
    pub fn rates_per_divine(&self) -> Option<RatesPerDivine> {
        if self.core.primary != "divine" {
            return self.rates_per_divine_from_lines();
        }
        // mirror 比 divine 值钱,所以它在 lines 里而不是 rates 里,而且方向是反的:
        // primaryValue 400 的意思是"1 个 mirror = 400 divine",要取倒数。
        let mirror = self.line_value("mirror").map(|value| 1.0 / value);
        Some(RatesPerDivine {
            chaos: self.core.rates.get("chaos").copied(),
            exalted: self.core.rates.get("exalted").copied(),
            mirror,
        })
    }

    /// 基准币不是 divine 时的换算:拿 divine 那一行当锚点。
    ///
    /// `lines[x].primaryValue` 的意思恒为"1 个 x 值多少个基准币"。所以
    /// "1 个 divine 换多少个 x" = divine 那行的值 ÷ x 那行的值。PoE1 实测:
    /// divine 358.9、exalted 1.83 → 196.1 exalted,和 poe.ninja 自己写在
    /// exalted 那行 `maxVolumeRate` 里的 196.7 对得上。
    ///
    /// 没有 divine 那一行就给 `None`:没有锚点,与其按 `secondary` 猜一条换算链,
    /// 不如让调用方知道这份数据它读不懂。
    fn rates_per_divine_from_lines(&self) -> Option<RatesPerDivine> {
        let divine = self.line_value("divine")?;
        let per_divine = |id: &str| self.line_value(id).map(|value| divine / value);
        Some(RatesPerDivine {
            chaos: per_divine("chaos"),
            exalted: per_divine("exalted"),
            mirror: per_divine("mirror"),
        })
    }

    /// 一行的 `primaryValue`,除非它是 0 / NaN —— 那种值除下去只会得到
    /// inf 或者 NaN,不如当成"这一轮没有行情"。
    fn line_value(&self, id: &str) -> Option<f64> {
        self.lines
            .iter()
            .find(|line| line.id == id)
            .map(|line| line.primary_value)
            .filter(|value| value.is_finite() && *value > 0.0)
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

/// 物品榜的表头。`primary` 就是 `primary_value` 的单位,**每一份原文自己说**:
/// 2026-09-06 是 `exalted`,2026-09-07 是 `divine`,以后还会再换。
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
    /// PoE1 专属:那边没有 `core.primary`,每行直接写死三种币的价钱。
    /// PoE2 的原文里没有这三个字段,所以永远是 0 —— 别拿它们当"免费的换算表"。
    #[serde(default)]
    pub chaos_value: f64,
    #[serde(default)]
    pub divine_value: f64,
    #[serde(default)]
    pub exalted_value: f64,
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

impl ItemOverview {
    /// 把 PoE1 的物品榜摊平成 PoE2 那套 `core.primary` + `primaryValue`。
    ///
    /// 两代同一个路径、同一个客户端,返回的却是两种形状:PoE1 没有 `core`,
    /// 价钱直接写在每行的 `chaosValue`/`divineValue`/`exaltedValue` 里。
    /// 不摊平的话 serde 会老老实实把 `primaryValue` 填成默认值 0,整张榜
    /// **静悄悄地全是 0**,一条报错都不给 —— 那是最难查的一种错。
    ///
    /// 基准币选 chaos:PoE1 的交易所本来就以 chaos 计价
    /// (`core.primary` = "chaos"),两边同一个单位才对得上。
    #[must_use]
    pub fn normalized_for(mut self, game: Game) -> Self {
        if game == Game::Poe2 {
            return self;
        }
        self.core.primary = "chaos".to_owned();
        for line in &mut self.lines {
            line.primary_value = line.chaos_value;
        }
        self
    }
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

    /// 2026-09-09 从 `/poe1/api/economy/exchange/current/overview?league=Allflame`
    /// 原样剪下来的:`core` 一个字没动,102 行里只留了 chaos / divine / exalted /
    /// mirror 四行,`items` 清空。
    const POE1_EXCHANGE: &str = include_str!("../fixtures/poe1_exchange_overview.json");

    /// PoE1 的交易所以 **chaos** 计价,PoE2 以 divine 计价 —— 同一个接口、同一种
    /// 形状,基准币不同。
    ///
    /// 老代码只认 divine 基准,别的一律给 `None`,那样 PoE1 的价格一条都换算不出来。
    /// 但这份原文其实说得清清楚楚:divine 那一行写着 358.9,意思就是"一个 divine
    /// 值 358.9 个 chaos",拿它当锚点,别的币除一下就有了。
    #[test]
    fn a_chaos_based_overview_still_converts_to_per_divine() {
        let overview: ExchangeOverview = serde_json::from_str(POE1_EXCHANGE).unwrap();
        assert_eq!(overview.core.primary, "chaos");
        assert_eq!(overview.core.secondary, "divine");
        assert_eq!(overview.core.rates["divine"], 0.002_786);

        let rates = overview.rates_per_divine().unwrap();
        assert_eq!(rates.chaos, Some(358.9));
        // 1 exalted = 1.83 chaos,所以 1 divine = 358.9 / 1.83 ≈ 196.1 exalted。
        // poe.ninja 自己在 exalted 那行的 maxVolumeRate 里写的是 196.7,对得上。
        assert!((rates.exalted.unwrap() - 196.12).abs() < 0.1);
        // mirror 同理:358.9 / 415183 ≈ 0.00086,它那行的 maxVolumeRate 是 0.0008645。
        assert!((rates.mirror.unwrap() - 0.000_864_4).abs() < 1e-7);
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

    /// 物品榜的计价基准是**这一份原文自己说的**,不是常数。
    ///
    /// 2026-09-06 抓到的那份 `core.primary` 是 `exalted`,代码于是把
    /// "exalted" 写死在了存储和界面里。2026-09-07 这份原文里它已经是
    /// `divine` —— 同一个 `primaryValue: 0.08548`,读成 exalted 是
    /// "8 分钱",读成 divine 是"8 个 exalted",差了近百倍。
    #[test]
    fn the_item_overview_declares_its_own_base_currency() {
        let overview: ItemOverview = serde_json::from_str(UNIQUE_WEAPONS).unwrap();
        assert_eq!(overview.core.primary, "divine");

        let trenchtimbre = &overview.lines[2];
        assert_eq!(trenchtimbre.name, "Trenchtimbre");
        assert_eq!(trenchtimbre.primary_value, 0.085_48);

        // 老原文(2026-09-06)是另一个基准币,同样得照它说的读。
        let older: ItemOverview = serde_json::from_str(ITEM_JSON).unwrap();
        assert_eq!(older.core.primary, "exalted");
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
        assert_eq!(unique_types_for(Game::Poe2), UNIQUE_TYPES);
    }

    /// PoE1 物品榜的一行,字段名照抄 2026-09-09 探针从
    /// `/poe1/api/economy/stash/current/item/overview?league=Allflame&type=UniqueWeapon`
    /// 打出来的那张键表(第一行的值也是真的,icon / flavourText / 词缀文本剪掉了;
    /// 第二行是手编的,专门放一个便宜货看小数)。
    ///
    /// 和 PoE2 的差别一眼可见:**没有 `core`**,价格直接写在每行的
    /// `chaosValue` / `divineValue` / `exaltedValue` 里,没有 `primaryValue`,
    /// 也没有 `levelRequired` / `category` / `corrupted`。
    const POE1_ITEM_JSON: &str = r#"{
      "lines": [
        {"id":1,"name":"Foulborn Reefbane","baseType":"Fishing Rod","itemClass":3,"itemType":"Fishing Rod",
         "detailsId":"foulborn-reefbane-otherworldly-lure-fishing-rod","variant":null,
         "chaosValue":415183,"divineValue":1157,"exaltedValue":226876,
         "count":4,"listingCount":6,
         "sparkLine":{"totalChange":11.36,"data":[0.93,6.79,24.49]},
         "mutatedModifiers":[]},
        {"id":2,"name":"Goldrim","baseType":"Leather Cap","itemClass":10,"itemType":"Helmet",
         "detailsId":"goldrim-leather-cap","chaosValue":2.5,"divineValue":0.007,"exaltedValue":1.37,
         "count":100,"listingCount":842,"sparkLine":{"totalChange":0,"data":[null,null,0]}}
      ]
    }"#;

    /// PoE1 的价格得摊平成 PoE2 那套(`core.primary` + `primaryValue`),
    /// 存储和界面才不用为两代各写一遍。
    ///
    /// 不摊平的话 serde 会把 `primaryValue` 填成默认值 0 —— 整张 PoE1 暗金榜
    /// 会**静悄悄地全是 0 分钱**,一条报错都不给。
    #[test]
    fn a_poe1_item_overview_is_flattened_onto_the_poe2_shape() {
        let raw: ItemOverview = serde_json::from_str(POE1_ITEM_JSON).unwrap();
        // 摊平之前:没有 core,价格读不出来。
        assert_eq!(raw.core.primary, "");
        assert_eq!(raw.lines[0].primary_value, 0.0);

        let overview = raw.normalized_for(Game::Poe1);
        assert_eq!(overview.core.primary, "chaos");
        assert_eq!(overview.lines[0].name, "Foulborn Reefbane");
        assert_eq!(overview.lines[0].base_type, "Fishing Rod");
        assert_eq!(overview.lines[0].primary_value, 415_183.0);
        assert_eq!(overview.lines[0].chaos_value, 415_183.0);
        assert_eq!(overview.lines[0].divine_value, 1_157.0);
        assert_eq!(overview.lines[0].listing_count, 6);
        assert_eq!(overview.lines[1].primary_value, 2.5);
        assert_eq!(overview.lines[1].spark_line.change_percent(), None);
        // PoE1 没有这几个字段,摊平不会替它们编数出来。
        assert_eq!(overview.lines[0].level_required, 0);
        assert_eq!(overview.lines[0].category, "");
        assert!(!overview.lines[0].corrupted);
    }

    /// PoE2 那边一个字都不许动:同一份原文摊平前后必须完全一样。
    #[test]
    fn flattening_leaves_a_poe2_overview_alone() {
        let before: ItemOverview = serde_json::from_str(UNIQUE_WEAPONS).unwrap();
        let after = before.clone().normalized_for(Game::Poe2);
        assert_eq!(after.core.primary, before.core.primary);
        assert_eq!(after.lines.len(), before.lines.len());
        for (left, right) in after.lines.iter().zip(&before.lines) {
            assert_eq!(left.name, right.name);
            assert_eq!(left.primary_value, right.primary_value);
            assert_eq!(left.chaos_value, 0.0, "PoE2 的原文里根本没有 chaosValue");
        }
    }

    /// PoE1 的分类名是单数,而且没有 charm(那是 PoE2 才有的东西)。
    /// 把 PoE2 那个复数名字拿去问 PoE1 会直接 404(2026-09-09 实测),
    /// 所以这张表必须按代分开。
    #[test]
    fn poe1_unique_types_are_singular_and_have_no_charms() {
        assert_eq!(
            unique_types_for(Game::Poe1),
            [
                "UniqueWeapon",
                "UniqueArmour",
                "UniqueAccessory",
                "UniqueFlask",
                "UniqueJewel",
            ]
        );
    }
}
