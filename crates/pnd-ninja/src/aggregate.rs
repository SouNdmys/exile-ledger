//! 词缀统计(纯函数:输入是一堆已经抓好的角色详情,输出是可以直接落库的行)。
//!
//! 统计口径来自计划:**按部位 × 稀有度 × 词缀类型 × stat id** 分组。四个维度
//! 缺一不可——
//!
//! - **部位**(`inventoryId`):戒指上的生命和胸甲上的生命不是一回事
//! - **稀有度**:暗金的词缀是固定的,和稀有装的随机词缀混在一起看没有意义
//! - **词缀类型**(explicit/implicit/crafted/desecrated/rune):同一条
//!   `base_maximum_life` 是天生的还是工艺出来的,决定你该怎么做这件装备
//! - **stat id**:`IncreasedLife8` 和 `IncreasedLife6` 只是档位不同,靠
//!   `base_maximum_life` 才能归成一家
//!
//! 每行报两个计数,别混:`characters` 是**多少个角色**带着它(两只戒指都带生命
//! 也只算一个人),`occurrences` 是**出现了多少次**。前者算占比,后者看堆叠。
//! `sample_size` 是"这个部位 + 这个稀有度一共有多少个角色有装备",占比的分母
//! 是它而不是总样本数——不是每个人都戴护身符。

use std::collections::BTreeMap;

use crate::character::{CharacterDetail, ItemData, ModEntry, mod_family, pair_mod_displays};
use crate::plan::is_rarity_bucket;

/// 一行统计。字段顺序和 `ninja_item_mods` 那张表一一对应。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotModStat {
    /// 这一行算的是哪个职业。**空串 = 全样本**(所有职业合起来)。
    ///
    /// 同一份样本因此出两套行:一套 `""` 的全局行,外加每个职业各一套。
    /// 分开算而不是让界面拿全局行去按职业筛,是因为分母也得跟着换 ——
    /// "多少人戴戒指"在 Deadeye 里和在全联赛里根本不是一个数。
    pub class: String,
    /// `inventoryId`;为空时退化成 `slot<itemSlot>`。
    pub slot: String,
    /// `"Rare"` / `"Unique"` / `"Magic"` / `"Normal"` / `""`(接口没给)。
    pub rarity: String,
    /// `explicit` / `implicit` / `crafted` / `desecrated` / `rune`。
    pub mod_kind: String,
    pub stat_id: String,
    /// 这条 stat 在游戏里印出来是哪句话,数字换成了 `#`
    /// (`+# to maximum Mana`)。**认不出来时是空串**,界面退回显示 stat id。
    ///
    /// 一条 stat 偶尔会配到不同的写法,这里存的是这一桶里出现最多的那句
    /// (理由同 [`dominant_family`])。
    pub display: String,
    /// 这一行数的是那句话里的第几个数(1 起)。`Adds # to # Fire Damage`
    /// 有两个数,minimum 是 1、maximum 是 2 —— 没有它,那两行在界面上
    /// 是一模一样的字。认不出来时是 0。
    pub value_index: u8,
    pub mod_family: String,
    pub characters: u32,
    pub occurrences: u32,
    /// 这个部位 + 这个稀有度下,有装备的角色数。占比的分母。
    pub sample_size: u32,
    pub p25: Option<f64>,
    pub p50: Option<f64>,
    pub p75: Option<f64>,
}

impl SlotModStat {
    /// 这一行要不要在游戏文本后面补一句"数的是第几个数"。
    ///
    /// 判据是那句话里有几个 `#`:两个以上就说明最小/最大共用同一句,不补的话
    /// 屏幕上是两行一模一样的字。只有一个数的行补了只是噪音。
    ///
    /// 规则放在这儿而不是各画各的:界面用括号、探针用方括号,但"什么时候该补"
    /// 只能有一个答案。
    #[must_use]
    pub fn needs_value_marker(&self) -> bool {
        self.value_index > 0 && self.display.matches('#').count() > 1
    }
}

/// 热门暗金榜的一行。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UniqueUsage {
    pub name: String,
    pub count: u64,
    pub share_percent: f64,
}

/// 聚合键。**族名不在键里**:`ninja_item_mods` 的主键就是这四列,把族名塞进键
/// 会让"两族共用一个 stat"的行(生命既能来自 `IncreasedLife`,也能来自
/// `LifeAndMana`)在插库时撞主键。
type StatKey = (String, String, String, String);

#[derive(Default)]
struct Bucket {
    characters: u32,
    occurrences: u32,
    values: Vec<f64>,
    families: BTreeMap<String, u32>,
    /// 显示文本和"第几个数"一起投票。捆成一个键是有意的:分开投的话,
    /// 可能挑出 A 行的话配 B 行的序号,拼出一句谁也没说过的描述。
    displays: BTreeMap<(String, u8), u32>,
    /// 只记"上一个碰过这个桶的角色是谁"就够去重了:角色是一个接一个处理的,
    /// 比存一整个 HashSet 省得多。
    last_character: Option<usize>,
}

#[derive(Default)]
struct SlotSample {
    characters: u32,
    last_character: Option<usize>,
}

/// 把一批角色详情压成词缀统计表。
///
/// 输出**两套行**:一套 `class` 为空的全样本行,外加每个职业各一套。分开算是
/// 因为占比的分母也得跟着换 —— "多少人戴戒指"在 Deadeye 里和在全联赛里是两个
/// 数,拿全局行去按职业筛只会得到一堆分母错了的百分比。
///
/// 详情里没写职业的人只进全样本:空串是"全部"那一档的键,让他也写空串,
/// 就等于把全联赛的统计悄悄换成这几个人的。
///
/// 输出按 职业 → 部位 → 稀有度 → 词缀类型 → 人数从多到少 排好,界面直接
/// 照着画即可。
#[must_use]
pub fn aggregate_mods(details: &[CharacterDetail]) -> Vec<SlotModStat> {
    let everyone: Vec<&CharacterDetail> = details.iter().collect();
    let mut out = aggregate_one_class(&everyone, "");

    let mut classes: Vec<&str> = details
        .iter()
        .map(|detail| detail.class.as_str())
        .filter(|class| !class.is_empty())
        .collect();
    classes.sort_unstable();
    classes.dedup();
    for class in classes {
        let members: Vec<&CharacterDetail> = details
            .iter()
            .filter(|detail| detail.class == class)
            .collect();
        out.extend(aggregate_one_class(&members, class));
    }

    out.sort_by(|left, right| {
        left.class
            .cmp(&right.class)
            .then_with(|| left.slot.cmp(&right.slot))
            .then_with(|| left.rarity.cmp(&right.rarity))
            .then_with(|| left.mod_kind.cmp(&right.mod_kind))
            .then_with(|| right.characters.cmp(&left.characters))
            .then_with(|| left.stat_id.cmp(&right.stat_id))
    });
    out
}

/// 一批角色 → 一套行,每行都盖上 `class`。全样本那一套传空串。
fn aggregate_one_class(details: &[&CharacterDetail], class: &str) -> Vec<SlotModStat> {
    let mut buckets: BTreeMap<StatKey, Bucket> = BTreeMap::new();
    let mut samples: BTreeMap<(String, String), SlotSample> = BTreeMap::new();

    for (index, detail) in details.iter().enumerate() {
        // 珠宝和药剂/护符是分开的三个数组,但统计口径完全一样。
        let worn = detail
            .items
            .iter()
            .chain(detail.jewels.iter())
            .chain(detail.flasks.iter());
        for entry in worn {
            let item = &entry.item_data;
            let slot = if item.inventory_id.is_empty() {
                format!("slot{}", entry.item_slot)
            } else {
                item.inventory_id.clone()
            };
            let rarity = item.rarity.clone();

            let sample = samples.entry((slot.clone(), rarity.clone())).or_default();
            if sample.last_character != Some(index) {
                sample.last_character = Some(index);
                sample.characters += 1;
            }

            for (kind, mods, lines) in mod_groups(item) {
                // 显示文本在这一步就配好:出了这个循环,行和它来自哪件装备
                // 的联系就断了,而配对只有在同一件装备里才能做。
                let paired = pair_mod_displays(mods, lines);
                for (entry_mod, stats) in mods.iter().zip(paired) {
                    let family = mod_family(&entry_mod.id);
                    if stats.is_empty() {
                        // 布尔词缀、纯文本词缀:数值统计不了,但"有多少人带着它"
                        // 照样有用,所以还是给它一行,只是没有分位数。
                        let key = (
                            slot.clone(),
                            rarity.clone(),
                            kind.to_owned(),
                            fallback_stat_id(entry_mod),
                        );
                        record(&mut buckets, key, family, None, index);
                    } else {
                        for stat in stats {
                            let key = (slot.clone(), rarity.clone(), kind.to_owned(), stat.stat_id);
                            let bucket = record(&mut buckets, key, family, Some(stat.value), index);
                            if !stat.display.is_empty() {
                                *bucket
                                    .displays
                                    .entry((stat.display, stat.value_index))
                                    .or_default() += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    buckets
        .into_iter()
        .map(|((slot, rarity, mod_kind, stat_id), mut bucket)| {
            bucket.values.sort_by(f64::total_cmp);
            let sample_size = samples
                .get(&(slot.clone(), rarity.clone()))
                .map_or(0, |sample| sample.characters);
            let (display, value_index) = dominant_display(&bucket.displays);
            SlotModStat {
                class: class.to_owned(),
                slot,
                rarity,
                mod_kind,
                stat_id,
                display,
                value_index,
                mod_family: dominant_family(&bucket.families),
                characters: bucket.characters,
                occurrences: bucket.occurrences,
                sample_size,
                p25: nearest_rank(&bucket.values, 0.25),
                p50: nearest_rank(&bucket.values, 0.50),
                p75: nearest_rank(&bucket.values, 0.75),
            }
        })
        .collect()
}

/// 七组词缀,顺序无所谓——输出最后会重排。
///
/// 每组带着**自己那一份显示文本**:`mods.explicit` 配 `explicitMods`。
/// 配错组就等于把工艺词缀的话挂到天生词缀上。
///
/// 两代各有自己独占的组(PoE2 的 `desecrated`/`rune`,PoE1 的
/// `fractured`/`enchant`),这里一并列全:对面没有那一组时它就是个空数组,
/// 白列一行的代价是零,而漏列一行的代价是那一组**静悄悄地不进统计**。
fn mod_groups(item: &ItemData) -> [(&'static str, &[ModEntry], &[String]); 7] {
    [
        ("implicit", &item.mods.implicit, &item.implicit_mods),
        ("explicit", &item.mods.explicit, &item.explicit_mods),
        ("crafted", &item.mods.crafted, &item.crafted_mods),
        ("desecrated", &item.mods.desecrated, &item.desecrated_mods),
        ("rune", &item.mods.rune, &item.rune_mods),
        ("fractured", &item.mods.fractured, &item.fractured_mods),
        ("enchant", &item.mods.enchant, &item.enchant_mods),
    ]
}

/// 一条词缀一个数值都没有时拿什么当 stat id:先用它自己的第一个 stat 名
/// (`stats` 是 `BTreeMap`,所以"第一个"是字典序,同一条词缀每次都一样),
/// 一个 stat 都没有就退回词缀 id 本身。
fn fallback_stat_id(entry: &ModEntry) -> String {
    entry
        .stats
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| entry.id.clone())
}

/// 记一笔,并把桶还回去 —— 调用方还要往里投一票显示文本。
fn record<'a>(
    buckets: &'a mut BTreeMap<StatKey, Bucket>,
    key: StatKey,
    family: &str,
    value: Option<f64>,
    character: usize,
) -> &'a mut Bucket {
    let bucket = buckets.entry(key).or_default();
    if bucket.last_character != Some(character) {
        bucket.last_character = Some(character);
        bucket.characters += 1;
    }
    bucket.occurrences += 1;
    if let Some(value) = value {
        bucket.values.push(value);
    }
    *bucket.families.entry(family.to_owned()).or_default() += 1;
    bucket
}

/// 键里没有族名,可行里要显示一个,只能选个代表:出现次数最多的那族,
/// 打平了按字典序小的——同样的输入永远给同样的结果,库里的行才不会来回抖。
fn dominant_family(families: &BTreeMap<String, u32>) -> String {
    families
        .iter()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
        .map(|(name, _)| name.clone())
        .unwrap_or_default()
}

/// 同一条 stat 在不同装备上偶尔印出不一样的话(`+16%` 和 `16%`,或者某一件
/// 上配错了一行),但一行只能挂一句:挑出现次数最多的那对,打平了按字典序小的。
/// 规则和 [`dominant_family`] 一样,理由也一样 —— 同样的输入永远给同样的结果,
/// 库里的行才不会来回抖。
///
/// 一票都没有(全都配不上)就交白卷,让界面退回 stat id。
fn dominant_display(displays: &BTreeMap<(String, u8), u32>) -> (String, u8) {
    displays
        .iter()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
        .map(|((display, value_index), _)| (display.clone(), *value_index))
        .unwrap_or_default()
}

/// 最近秩(nearest-rank)分位数:排序后取第 `ceil(p × n)` 个。
///
/// 不做插值是故意的:词缀数值大多是整数,插出来的 `112.5 生命` 是个游戏里
/// 不存在的数,反而让人以为自己看错了。
fn nearest_rank(sorted: &[f64], percentile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let count = sorted.len();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = (percentile * count as f64).ceil().max(1.0) as usize;
    sorted.get(rank.min(count) - 1).copied()
}

/// 分面 → 热门暗金榜。`total` 是这个分区的角色总数(`SearchResponse::total`),
/// 占比的分母。
///
/// 稀有度桶(`Rare Ring` 这种)要排掉,理由见 [`is_rarity_bucket`]:
/// 不排的话榜首永远是它们,真正的暗金一个都挤不进前十。
#[must_use]
pub fn unique_usage_from_facet(entries: &[(String, u64)], total: u64) -> Vec<UniqueUsage> {
    let mut out: Vec<UniqueUsage> = entries
        .iter()
        .filter(|(name, _)| !is_rarity_bucket(name))
        .map(|(name, count)| UniqueUsage {
            name: name.clone(),
            count: *count,
            share_percent: if total == 0 {
                0.0
            } else {
                *count as f64 * 100.0 / total as f64
            },
        })
        .collect();
    out.sort_by_key(|usage| std::cmp::Reverse(usage.count));
    out
}

#[cfg(test)]
mod aggregate_tests {
    use super::*;

    /// PoE1 的裂隙词缀和迷宫附魔也要进统计。
    ///
    /// 它们是 `mods` 下 PoE1 才有的两组。分组表漏掉一组不会报错 —— 那一组只是
    /// **静悄悄地不出现在词缀页上**,而裂隙词缀恰恰是 PoE1 好装备上最贵的那一条。
    #[test]
    fn a_poe1_items_fractured_and_enchant_mods_reach_the_stats() {
        let poe1: CharacterDetail = serde_json::from_str(
            r#"{
              "account": "someone-0000", "name": "Someone", "class": "Champion",
              "items": [
                {"itemSlot": 8, "itemData": {"inventoryId": "Ring", "rarity": "Rare",
                  "mods": {
                    "fractured": [{"id": "Strength9", "stats": {"additional_strength": 52}}],
                    "enchant": [{"id": "EnchantmentLife1", "stats": {"base_maximum_life": 40}}]},
                  "fracturedMods": ["+52 to Strength"],
                  "enchantMods": ["+40 to maximum Life"]}}
              ]
            }"#,
        )
        .unwrap();

        let stats = aggregate_mods(&[poe1]);
        let find = |kind: &str, stat_id: &str| {
            stats
                .iter()
                .find(|row| row.mod_kind == kind && row.stat_id == stat_id && row.class.is_empty())
        };
        let fractured = find("fractured", "additional_strength").expect("裂隙词缀得进统计");
        assert_eq!(fractured.slot, "Ring");
        assert_eq!(fractured.characters, 1);
        assert_eq!(fractured.p50, Some(52.0));
        // 显示文本也得配上它自己那一份数组,不能挂到别组的话上。
        assert_eq!(fractured.display, "+# to Strength");

        let enchant = find("enchant", "base_maximum_life").expect("附魔也得进统计");
        assert_eq!(enchant.display, "+# to maximum Life");
    }

    /// 两个手写角色。形状照着线上响应剪的,只是把装备砍到刚好够验每条规则:
    ///
    /// - alpha 两只戒指都带生命(115 / 95)→ 一个人、两次出现
    /// - beta 一只戒指带生命(135)→ 三个值,分位数有得算
    /// - alpha 的胸甲是暗金、词缀没有数值 → 有行但没有分位数
    /// - beta 的药剂没有 `inventoryId` → 部位退化成 `slot4`
    /// - alpha 的珠宝证明 `jewels` 数组也被算进来
    fn alpha() -> CharacterDetail {
        serde_json::from_str(
            r#"{
              "account": "player-0416",
              "name": "ExileCharacter",
              "items": [
                {"itemSlot": 8, "itemData": {"inventoryId": "Ring", "rarity": "Rare",
                  "mods": {"explicit": [
                    {"id": "IncreasedLife8", "stats": {"base_maximum_life": 115}},
                    {"id": "LightningResist5", "stats": {"base_lightning_damage_resistance_%": 28}}
                  ]},
                  "explicitMods": ["+28% to [Resistances|Lightning Resistance]",
                                   "+115 to maximum Life"]}},
                {"itemSlot": 9, "itemData": {"inventoryId": "Ring", "rarity": "Rare",
                  "mods": {"explicit": [{"id": "IncreasedLife6", "stats": {"base_maximum_life": 95}}],
                           "crafted": [{"id": "FireResist6", "stats": {"base_fire_damage_resistance_%": 34}}]},
                  "explicitMods": ["+95 to maximum Life"],
                  "craftedMods": ["+34% to [Resistances|Fire Resistance]"]}},
                {"itemSlot": 3, "itemData": {"inventoryId": "BodyArmour", "rarity": "Unique",
                  "mods": {"explicit": [{"id": "UniqueCannotBeFrozen1", "stats": {"cannot_be_frozen": true}}]},
                  "explicitMods": ["Cannot be Frozen"]}}
              ],
              "jewels": [
                {"itemSlot": 0, "itemData": {"inventoryId": "Jewel", "rarity": "Rare",
                  "mods": {"explicit": [{"id": "JewelIncreasedLife3", "stats": {"base_maximum_life": 22}}]},
                  "explicitMods": ["+22 to maximum Life"]}}
              ]
            }"#,
        )
        .unwrap()
    }

    fn beta() -> CharacterDetail {
        serde_json::from_str(
            r#"{
              "account": "dota2enjoyer-1809",
              "name": "KingPinUwU",
              "items": [
                {"itemSlot": 8, "itemData": {"inventoryId": "Ring", "rarity": "Rare",
                  "mods": {"explicit": [{"id": "IncreasedLife8", "stats": {"base_maximum_life": 135}}]},
                  "explicitMods": ["+135 to maximum Life"]}},
                {"itemSlot": 3, "itemData": {"inventoryId": "BodyArmour", "rarity": "Unique",
                  "mods": {"explicit": [{"id": "UniqueCannotBeFrozen1", "stats": {"cannot_be_frozen": true}}]},
                  "explicitMods": ["Cannot be Frozen"]}}
              ],
              "flasks": [
                {"itemSlot": 4, "itemData": {"rarity": "Magic",
                  "mods": {"implicit": [{"id": "FlaskChargesUsed2", "stats": {"local_charges_used_+%": [-30, -20]}}]},
                  "implicitMods": ["30% reduced Charges used"]}}
              ]
            }"#,
        )
        .unwrap()
    }

    /// 全样本(职业 = 空串)的那一行。
    fn row<'a>(rows: &'a [SlotModStat], slot: &str, kind: &str, stat: &str) -> &'a SlotModStat {
        of_class(rows, "", slot, kind, stat)
    }

    fn of_class<'a>(
        rows: &'a [SlotModStat],
        class: &str,
        slot: &str,
        kind: &str,
        stat: &str,
    ) -> &'a SlotModStat {
        rows.iter()
            .find(|row| {
                row.class == class
                    && row.slot == slot
                    && row.mod_kind == kind
                    && row.stat_id == stat
            })
            .unwrap_or_else(|| panic!("no row for {class}/{slot}/{kind}/{stat}"))
    }

    fn as_class(mut detail: CharacterDetail, class: &str) -> CharacterDetail {
        detail.class = class.to_owned();
        detail
    }

    /// 一个人两只戒指都带生命,只算一个人、两次出现。
    #[test]
    fn characters_and_occurrences_count_different_things() {
        let rows = aggregate_mods(&[alpha(), beta()]);
        let life = row(&rows, "Ring", "explicit", "base_maximum_life");
        assert_eq!(life.characters, 2);
        assert_eq!(life.occurrences, 3);
        assert_eq!(life.rarity, "Rare");
        // 两个角色都有稀有戒指,所以分母是 2。
        assert_eq!(life.sample_size, 2);
        // IncreasedLife8 和 IncreasedLife6 归到同一族。
        assert_eq!(life.mod_family, "IncreasedLife");

        let resist = row(
            &rows,
            "Ring",
            "explicit",
            "base_lightning_damage_resistance_%",
        );
        assert_eq!((resist.characters, resist.occurrences), (1, 1));
        assert_eq!(
            resist.sample_size, 2,
            "分母是部位+稀有度,不是带这条词缀的人"
        );

        // 工艺词缀单独一档,不和 explicit 混。
        let crafted = row(&rows, "Ring", "crafted", "base_fire_damage_resistance_%");
        assert_eq!(crafted.characters, 1);
        assert_eq!(crafted.mod_family, "FireResist");
    }

    /// 三个生命值 95 / 115 / 135:最近秩下 p25 = 95、p50 = 115、p75 = 135。
    #[test]
    fn percentiles_use_nearest_rank() {
        let rows = aggregate_mods(&[alpha(), beta()]);
        let life = row(&rows, "Ring", "explicit", "base_maximum_life");
        assert_eq!(life.p25, Some(95.0));
        assert_eq!(life.p50, Some(115.0));
        assert_eq!(life.p75, Some(135.0));

        // 只有一个值时三个分位数都是它。
        let jewel = row(&rows, "Jewel", "explicit", "base_maximum_life");
        assert_eq!(
            (jewel.p25, jewel.p50, jewel.p75),
            (Some(22.0), Some(22.0), Some(22.0))
        );
    }

    /// 布尔词缀没有可比的量纲,但"多少人带着它"照样要统计。
    #[test]
    fn a_mod_without_numbers_still_gets_a_row() {
        let rows = aggregate_mods(&[alpha(), beta()]);
        let frozen = row(&rows, "BodyArmour", "explicit", "cannot_be_frozen");
        assert_eq!(frozen.characters, 2);
        assert_eq!(frozen.occurrences, 2);
        assert_eq!(frozen.rarity, "Unique");
        assert_eq!(frozen.mod_family, "UniqueCannotBeFrozen");
        assert_eq!((frozen.p25, frozen.p50, frozen.p75), (None, None, None));
    }

    /// `inventoryId` 缺失时用 `itemSlot` 顶上,而不是把这件装备算进一个空部位。
    #[test]
    fn a_missing_inventory_id_falls_back_to_the_item_slot() {
        let rows = aggregate_mods(&[alpha(), beta()]);
        let flask = row(&rows, "slot4", "implicit", "local_charges_used_+%");
        assert_eq!(flask.rarity, "Magic");
        assert_eq!(flask.characters, 1);
        // [-30,-20] 取中点。
        assert_eq!(flask.p50, Some(-25.0));
        assert!(rows.iter().all(|row| !row.slot.is_empty()));
    }

    #[test]
    fn rows_come_out_sorted_by_slot_rarity_kind_then_characters() {
        let rows = aggregate_mods(&[alpha(), beta()]);
        let shape: Vec<(&str, &str, &str, u32)> = rows
            .iter()
            .map(|row| {
                (
                    row.slot.as_str(),
                    row.rarity.as_str(),
                    row.mod_kind.as_str(),
                    row.characters,
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("BodyArmour", "Unique", "explicit", 2),
                ("Jewel", "Rare", "explicit", 1),
                ("Ring", "Rare", "crafted", 1),
                // 同一组里人多的排前面。
                ("Ring", "Rare", "explicit", 2),
                ("Ring", "Rare", "explicit", 1),
                ("slot4", "Magic", "implicit", 1),
            ]
        );
    }

    /// 每个职业各来一套行,分母跟着换。
    ///
    /// "戒指上带生命的人占多少"这个问题,在全联赛和在某一个职业里是两个不同
    /// 的数,而后者才是"我这个号该做什么装备"的答案。全局那一套一个字都不能
    /// 变:它是两千个人的底,换职业只是多一个视角,不是换一份数据。
    #[test]
    fn every_class_present_gets_its_own_copy_of_the_stats() {
        let rows = aggregate_mods(&[
            as_class(alpha(), "Deadeye"),
            as_class(beta(), "Gemling Legionnaire"),
        ]);

        let all = row(&rows, "Ring", "explicit", "base_maximum_life");
        assert_eq!(
            (all.characters, all.occurrences, all.sample_size),
            (2, 3, 2)
        );

        // Deadeye 只有 alpha 一个人,两只戒指:115 和 95。
        let deadeye = of_class(&rows, "Deadeye", "Ring", "explicit", "base_maximum_life");
        assert_eq!(
            (deadeye.characters, deadeye.occurrences, deadeye.sample_size),
            (1, 2, 1),
            "分母是这个职业里戴稀有戒指的人数,不是全样本的 2"
        );
        assert_eq!(
            (deadeye.p25, deadeye.p50, deadeye.p75),
            (Some(95.0), Some(95.0), Some(115.0))
        );

        let gemling = of_class(
            &rows,
            "Gemling Legionnaire",
            "Ring",
            "explicit",
            "base_maximum_life",
        );
        assert_eq!(
            (gemling.characters, gemling.sample_size, gemling.p50),
            (1, 1, Some(135.0))
        );

        // alpha 的珠宝只属于 Deadeye:另一个职业里没有这一行。
        assert!(
            !rows
                .iter()
                .any(|row| row.class == "Gemling Legionnaire" && row.slot == "Jewel"),
            "没有这件装备的职业不该凭空多出一行"
        );
    }

    /// 详情里没写职业的角色只进全样本,不自己开一档。
    ///
    /// 空串是"全部"那一档的键:让一个无名职业也写空串,它就会和全样本行
    /// 撞在一起,把全联赛的统计悄悄换成这几个人的。
    #[test]
    fn a_character_without_a_class_only_lands_in_the_whole_sample() {
        let rows = aggregate_mods(&[as_class(alpha(), "Deadeye"), beta()]);

        let mut classes: Vec<&str> = rows.iter().map(|row| row.class.as_str()).collect();
        classes.sort_unstable();
        classes.dedup();
        assert_eq!(classes, vec!["", "Deadeye"]);

        // 没有职业的那个人照样算进全样本。
        let all = row(&rows, "Ring", "explicit", "base_maximum_life");
        assert_eq!((all.characters, all.occurrences), (2, 3));
        assert_eq!(all.sample_size, 2);
    }

    /// 全样本那一套排在最前面,后面每个职业各自成块 —— 界面照着往下画即可。
    #[test]
    fn the_whole_sample_comes_first_then_one_block_per_class() {
        let rows = aggregate_mods(&[
            as_class(alpha(), "Deadeye"),
            as_class(beta(), "Gemling Legionnaire"),
        ]);
        let classes: Vec<&str> = rows.iter().map(|row| row.class.as_str()).collect();
        let mut sorted = classes.clone();
        sorted.sort_unstable();
        assert_eq!(classes, sorted, "职业块之间不该交叉");
        assert_eq!(classes[0], "", "全样本排最前");

        // 一块之内还是老规矩:部位 → 稀有度 → 类型 → 人多的在前。
        let block: Vec<(&str, &str, &str, u32)> = rows
            .iter()
            .filter(|row| row.class == "Deadeye")
            .map(|row| {
                (
                    row.slot.as_str(),
                    row.rarity.as_str(),
                    row.mod_kind.as_str(),
                    row.characters,
                )
            })
            .collect();
        assert_eq!(
            block,
            vec![
                ("BodyArmour", "Unique", "explicit", 1),
                ("Jewel", "Rare", "explicit", 1),
                ("Ring", "Rare", "crafted", 1),
                ("Ring", "Rare", "explicit", 1),
                ("Ring", "Rare", "explicit", 1),
            ]
        );
    }

    /// 一把武器,`Adds {min} to {max} Fire Damage`。两个 stat 共用一行文本,
    /// 所以每一行还得说清自己数的是那行里的第几个数。
    fn weapon_bearer(min: u32, max: u32) -> CharacterDetail {
        serde_json::from_str(&format!(
            r#"{{
              "items": [
                {{"itemSlot": 5, "itemData": {{"inventoryId": "Weapon", "rarity": "Rare",
                  "mods": {{"explicit": [{{"id": "LocalAddedFireDamage6",
                    "stats": {{"local_minimum_added_fire_damage": {min},
                              "local_maximum_added_fire_damage": {max}}}}}]}},
                  "explicitMods": ["Adds {min} to {max} [Fire|Fire] Damage"]}}}}
              ]
            }}"#
        ))
        .unwrap()
    }

    /// 一只戒指,全抗那一行的写法由调用方给 —— 用来验"少数服从多数"。
    fn ring_bearer(value: u32, line: &str) -> CharacterDetail {
        serde_json::from_str(&format!(
            r#"{{
              "items": [
                {{"itemSlot": 8, "itemData": {{"inventoryId": "Ring", "rarity": "Rare",
                  "mods": {{"explicit": [{{"id": "AllResistances5",
                    "stats": {{"base_resist_all_elements_%": {value}}}}}]}},
                  "explicitMods": ["{line}"]}}}}
              ]
            }}"#
        ))
        .unwrap()
    }

    /// 每一行都要带上游戏里那句话。`base_maximum_life` 谁也认不出来,
    /// `+# to maximum Life` 一眼就懂。
    ///
    /// alpha 那只戒指的 `explicitMods` 是**反着写**的(抗性在前、生命在后),
    /// 照下标配对的话生命这一行会挂上抗性的文本。
    #[test]
    fn every_row_carries_the_in_game_text_of_its_stat() {
        let rows = aggregate_mods(&[alpha(), beta()]);

        let life = row(&rows, "Ring", "explicit", "base_maximum_life");
        assert_eq!(life.display, "+# to maximum Life");
        assert_eq!(life.value_index, 1);

        let resist = row(
            &rows,
            "Ring",
            "explicit",
            "base_lightning_damage_resistance_%",
        );
        assert_eq!(resist.display, "+#% to Lightning Resistance");

        let crafted = row(&rows, "Ring", "crafted", "base_fire_damage_resistance_%");
        assert_eq!(crafted.display, "+#% to Fire Resistance");
    }

    /// 配不上的行留空,界面照旧退回 stat id —— 空着好过挂一句别的词缀的话。
    #[test]
    fn a_stat_without_a_matching_line_leaves_the_text_empty() {
        let rows = aggregate_mods(&[alpha(), beta()]);

        // 布尔词缀根本没有数,无从配起。
        let frozen = row(&rows, "BodyArmour", "explicit", "cannot_be_frozen");
        assert_eq!(frozen.display, "");
        assert_eq!(frozen.value_index, 0);

        // 范围词缀取的是中点(-25),而那一行里写的是 30。
        let flask = row(&rows, "slot4", "implicit", "local_charges_used_+%");
        assert_eq!(flask.display, "");
    }

    /// 一行里有两个数时,两个 stat 各自记住"我数的是第几个" ——
    /// 没有它,最小和最大冷伤在界面上是两行一模一样的字。
    #[test]
    fn a_two_number_line_says_which_number_each_row_counts() {
        let rows = aggregate_mods(&[weapon_bearer(27, 68), weapon_bearer(31, 74)]);

        let low = row(
            &rows,
            "Weapon",
            "explicit",
            "local_minimum_added_fire_damage",
        );
        assert_eq!(low.display, "Adds # to # Fire Damage");
        assert_eq!(low.value_index, 1);
        assert_eq!(low.p50, Some(27.0), "两把武器的最小火伤是 27 和 31");

        let high = row(
            &rows,
            "Weapon",
            "explicit",
            "local_maximum_added_fire_damage",
        );
        assert_eq!(high.display, "Adds # to # Fire Damage");
        assert_eq!(high.value_index, 2);
    }

    /// "第几个数"那个标记只在一句话里真有两个数时才该出现。
    #[test]
    fn only_a_line_with_two_numbers_needs_a_value_marker() {
        let rows = aggregate_mods(&[weapon_bearer(27, 68), alpha()]);
        assert!(
            row(
                &rows,
                "Weapon",
                "explicit",
                "local_maximum_added_fire_damage"
            )
            .needs_value_marker()
        );
        assert!(
            !row(&rows, "Ring", "explicit", "base_maximum_life").needs_value_marker(),
            "+# to maximum Life 只有一个数,补个序号只是噪音"
        );
        // 连文本都没配上的行更没有什么可标的。
        assert!(!row(&rows, "BodyArmour", "explicit", "cannot_be_frozen").needs_value_marker());
    }

    /// 同一条 stat 在不同装备上偶尔会印出不一样的话(`+16%` / `16%`,或者
    /// 一次配错)。一行只能挂一句,所以挑**出现次数最多**的那句;打平了按
    /// 字典序,同样的输入永远给同样的结果。
    #[test]
    fn the_row_keeps_the_wording_most_of_its_items_agreed_on() {
        let rows = aggregate_mods(&[
            ring_bearer(16, "+16% to all Elemental Resistances"),
            ring_bearer(14, "+14% to all Elemental Resistances"),
            ring_bearer(12, "12% to all Elemental Resistances"),
        ]);
        let resist = row(&rows, "Ring", "explicit", "base_resist_all_elements_%");
        assert_eq!(resist.characters, 3);
        assert_eq!(resist.display, "+#% to all Elemental Resistances");
    }

    #[test]
    fn no_characters_means_no_rows() {
        assert!(aggregate_mods(&[]).is_empty());
        assert!(aggregate_mods(&[CharacterDetail::default()]).is_empty());
    }

    #[test]
    fn unique_usage_drops_buckets_and_computes_shares() {
        let entries = vec![
            ("Magic Flask".to_owned(), 60_943u64),
            ("Wake of Destruction".to_owned(), 7_158),
            ("Rare Ring".to_owned(), 44_000),
            ("Beira's Anguish".to_owned(), 6_413),
        ];
        let usage = unique_usage_from_facet(&entries, 61_390);
        assert_eq!(
            usage
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Wake of Destruction", "Beira's Anguish"]
        );
        assert_eq!(usage[0].count, 7_158);
        assert!((usage[0].share_percent - 7_158.0 * 100.0 / 61_390.0).abs() < 1e-12);
        assert!((usage[0].share_percent - 11.66).abs() < 0.01);
    }

    /// 分区总数是 0 时不能除零(理论上不会发生,但一次 500 就够炸整轮聚合了)。
    #[test]
    fn a_zero_total_gives_a_zero_share() {
        let entries = vec![("Wake of Destruction".to_owned(), 7u64)];
        let usage = unique_usage_from_facet(&entries, 0);
        assert_eq!(usage[0].share_percent, 0.0);
        assert!(unique_usage_from_facet(&[], 100).is_empty());
    }
}
