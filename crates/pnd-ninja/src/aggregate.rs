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

use crate::character::{CharacterDetail, ItemData, ModEntry, mod_family};
use crate::plan::is_rarity_bucket;

/// 一行统计。字段顺序和 `ninja_item_mods` 那张表一一对应。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotModStat {
    /// `inventoryId`;为空时退化成 `slot<itemSlot>`。
    pub slot: String,
    /// `"Rare"` / `"Unique"` / `"Magic"` / `"Normal"` / `""`(接口没给)。
    pub rarity: String,
    /// `explicit` / `implicit` / `crafted` / `desecrated` / `rune`。
    pub mod_kind: String,
    pub stat_id: String,
    pub mod_family: String,
    pub characters: u32,
    pub occurrences: u32,
    /// 这个部位 + 这个稀有度下,有装备的角色数。占比的分母。
    pub sample_size: u32,
    pub p25: Option<f64>,
    pub p50: Option<f64>,
    pub p75: Option<f64>,
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
/// 输出按 部位 → 稀有度 → 词缀类型 → 人数从多到少 排好,界面直接照着画即可。
#[must_use]
pub fn aggregate_mods(details: &[CharacterDetail]) -> Vec<SlotModStat> {
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

            for (kind, mods) in mod_groups(item) {
                for entry_mod in mods {
                    let family = mod_family(&entry_mod.id);
                    let numeric = entry_mod.numeric_stats();
                    if numeric.is_empty() {
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
                        for (stat_id, value) in numeric {
                            let key = (slot.clone(), rarity.clone(), kind.to_owned(), stat_id);
                            record(&mut buckets, key, family, Some(value), index);
                        }
                    }
                }
            }
        }
    }

    let mut out: Vec<SlotModStat> = buckets
        .into_iter()
        .map(|((slot, rarity, mod_kind, stat_id), mut bucket)| {
            bucket.values.sort_by(f64::total_cmp);
            let sample_size = samples
                .get(&(slot.clone(), rarity.clone()))
                .map_or(0, |sample| sample.characters);
            SlotModStat {
                slot,
                rarity,
                mod_kind,
                stat_id,
                mod_family: dominant_family(&bucket.families),
                characters: bucket.characters,
                occurrences: bucket.occurrences,
                sample_size,
                p25: nearest_rank(&bucket.values, 0.25),
                p50: nearest_rank(&bucket.values, 0.50),
                p75: nearest_rank(&bucket.values, 0.75),
            }
        })
        .collect();

    out.sort_by(|left, right| {
        left.slot
            .cmp(&right.slot)
            .then_with(|| left.rarity.cmp(&right.rarity))
            .then_with(|| left.mod_kind.cmp(&right.mod_kind))
            .then_with(|| right.characters.cmp(&left.characters))
            .then_with(|| left.stat_id.cmp(&right.stat_id))
    });
    out
}

/// 五组词缀,顺序无所谓——输出最后会重排。
fn mod_groups(item: &ItemData) -> [(&'static str, &[ModEntry]); 5] {
    [
        ("implicit", &item.mods.implicit),
        ("explicit", &item.mods.explicit),
        ("crafted", &item.mods.crafted),
        ("desecrated", &item.mods.desecrated),
        ("rune", &item.mods.rune),
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

fn record(
    buckets: &mut BTreeMap<StatKey, Bucket>,
    key: StatKey,
    family: &str,
    value: Option<f64>,
    character: usize,
) {
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
    out.sort_by(|left, right| right.count.cmp(&left.count));
    out
}

#[cfg(test)]
mod aggregate_tests {
    use super::*;

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
              "account": "heygyus-0416",
              "name": "ResurrectForbidden",
              "items": [
                {"itemSlot": 8, "itemData": {"inventoryId": "Ring", "rarity": "Rare",
                  "mods": {"explicit": [
                    {"id": "IncreasedLife8", "stats": {"base_maximum_life": 115}},
                    {"id": "LightningResist5", "stats": {"base_lightning_damage_resistance_%": 28}}
                  ]}}},
                {"itemSlot": 9, "itemData": {"inventoryId": "Ring", "rarity": "Rare",
                  "mods": {"explicit": [{"id": "IncreasedLife6", "stats": {"base_maximum_life": 95}}],
                           "crafted": [{"id": "FireResist6", "stats": {"base_fire_damage_resistance_%": 34}}]}}},
                {"itemSlot": 3, "itemData": {"inventoryId": "BodyArmour", "rarity": "Unique",
                  "mods": {"explicit": [{"id": "UniqueCannotBeFrozen1", "stats": {"cannot_be_frozen": true}}]}}}
              ],
              "jewels": [
                {"itemSlot": 0, "itemData": {"inventoryId": "Jewel", "rarity": "Rare",
                  "mods": {"explicit": [{"id": "JewelIncreasedLife3", "stats": {"base_maximum_life": 22}}]}}}
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
                  "mods": {"explicit": [{"id": "IncreasedLife8", "stats": {"base_maximum_life": 135}}]}}},
                {"itemSlot": 3, "itemData": {"inventoryId": "BodyArmour", "rarity": "Unique",
                  "mods": {"explicit": [{"id": "UniqueCannotBeFrozen1", "stats": {"cannot_be_frozen": true}}]}}}
              ],
              "flasks": [
                {"itemSlot": 4, "itemData": {"rarity": "Magic",
                  "mods": {"implicit": [{"id": "FlaskChargesUsed2", "stats": {"local_charges_used_+%": [-30, -20]}}]}}}
              ]
            }"#,
        )
        .unwrap()
    }

    fn row<'a>(rows: &'a [SlotModStat], slot: &str, kind: &str, stat: &str) -> &'a SlotModStat {
        rows.iter()
            .find(|row| row.slot == slot && row.mod_kind == kind && row.stat_id == stat)
            .unwrap_or_else(|| panic!("no row for {slot}/{kind}/{stat}"))
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
