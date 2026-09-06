//! 角色详情 JSON 的模型。
//!
//! `GET /poe2/api/builds/<version>/character?account=…&name=…&overview=…` 返回
//! 一个很大的对象(PoB 导出串、天赋树、技能树……)。词缀统计只要装备那部分,
//! 所以这里只声明用得上的字段,其余交给 serde 忽略。
//!
//! 关键在 `itemData.mods`:那里的词缀是**结构化**的
//! (`{"id":"IncreasedLife8","stats":{"base_maximum_life":115}}`),
//! 而 `explicitMods` 只是给人看的显示文本。统计一定要走 `mods`,
//! 因为 `IncreasedLife8` 和 `IncreasedLife6` 得靠 stat id 归成一家。

use std::collections::BTreeMap;

use serde::Deserialize;

/// 一条结构化词缀。`stats` 的值形状不固定:多数是数字,范围词缀是 `[min,max]`,
/// 少数是布尔,所以只能先收成 `serde_json::Value`,由 [`ModEntry::numeric_stats`] 归一。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub stats: BTreeMap<String, serde_json::Value>,
}

impl ModEntry {
    /// 把 `stats` 压成"名字 → 一个数"。
    ///
    /// 范围取中点:词缀统计要的是 p25/p50/p75 这种分布感,
    /// 一条 13–18 的冷伤当成 15.5 比丢掉它有用得多。布尔和其它形状直接跳过——
    /// 它们没有可比较的量纲,硬塞成 0/1 会污染分位数。
    #[must_use]
    pub fn numeric_stats(&self) -> Vec<(String, f64)> {
        self.stats
            .iter()
            .filter_map(|(name, value)| {
                let number = match value {
                    serde_json::Value::Number(number) => number.as_f64()?,
                    serde_json::Value::Array(items) if items.len() == 2 => {
                        let low = items[0].as_f64()?;
                        let high = items[1].as_f64()?;
                        (low + high) / 2.0
                    }
                    _ => return None,
                };
                Some((name.clone(), number))
            })
            .collect()
    }
}

/// 把 `IncreasedLife8` 砍成 `IncreasedLife`。
///
/// 尾部数字是词缀的"档位"(tier),同一族不同档要合并统计。stat id 也能归一,
/// 但一条词缀可能带多个 stat;按族名归一才对得上"这件装备有没有生命词缀"。
#[must_use]
pub fn mod_family(id: &str) -> &str {
    id.trim_end_matches(|character: char| character.is_ascii_digit())
}

/// `mods` 下按类型分好的五组。分组本身就是统计维度:同一条
/// `base_maximum_life` 是天生的还是工艺出来的,意义完全不同。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModGroups {
    #[serde(default)]
    pub implicit: Vec<ModEntry>,
    #[serde(default)]
    pub explicit: Vec<ModEntry>,
    #[serde(default)]
    pub crafted: Vec<ModEntry>,
    #[serde(default)]
    pub desecrated: Vec<ModEntry>,
    #[serde(default)]
    pub rune: Vec<ModEntry>,
}

/// 一件装备。`inventory_id`("Ring"/"BodyArmour"/……)是词缀统计的分组键;
/// `frame_type` 3 = 暗金、2 = 稀有。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemData {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub type_line: String,
    #[serde(default)]
    pub base_type: String,
    #[serde(default)]
    pub rarity: String,
    #[serde(default)]
    pub frame_type: i32,
    #[serde(default)]
    pub inventory_id: String,
    #[serde(default)]
    pub ilvl: i32,
    #[serde(default)]
    pub corrupted: bool,
    #[serde(default)]
    pub desecrated: bool,
    #[serde(default)]
    pub mods: ModGroups,
    #[serde(default)]
    pub explicit_mods: Vec<String>,
    #[serde(default)]
    pub implicit_mods: Vec<String>,
    #[serde(default)]
    pub crafted_mods: Vec<String>,
    #[serde(default)]
    pub desecrated_mods: Vec<String>,
    #[serde(default)]
    pub rune_mods: Vec<String>,
    #[serde(default)]
    pub enchant_mods: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemEntry {
    #[serde(default)]
    pub item_slot: i32,
    #[serde(default)]
    pub item_data: ItemData,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Keystone {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub stats: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterDetail {
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub league: String,
    #[serde(default)]
    pub level: u32,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub items: Vec<ItemEntry>,
    #[serde(default)]
    pub jewels: Vec<ItemEntry>,
    #[serde(default)]
    pub flasks: Vec<ItemEntry>,
    #[serde(default)]
    pub keystones: Vec<Keystone>,
    #[serde(default)]
    pub updated_utc: String,
    #[serde(default)]
    pub last_seen_utc: String,
}

#[cfg(test)]
mod character_tests {
    use super::*;

    /// 线上真实响应剪出来的一件戒指(隐式 1 条 + 显式 4 条 + 工艺 1 条 + 亵渎 1 条),
    /// 外面套了角色对象最外层的字段。故意留着 `pathOfBuildingExport`、`sockets`
    /// 这些我们不声明的字段:它们就是"未知字段必须被忽略"的守门测试。
    const CHARACTER_JSON: &str = r#"{
      "account": "heygyus-0416",
      "name": "ResurrectForbidden",
      "league": "Forbidden Rites",
      "level": 98,
      "class": "Gemling Legionnaire",
      "baseClass": "Mercenary",
      "pathOfBuildingExport": "eNrtvQ==",
      "defensiveStats": {"life": 3120},
      "items": [
        {
          "itemSlot": 8,
          "itemData": {
            "desecrated": true,
            "desecratedMods": ["Adds 13 to 18 [Cold] damage to [Attack|Attacks]"],
            "runeMods": [],
            "sockets": [],
            "id": "e15fa227f7f7ee01",
            "inventoryId": "Ring",
            "name": "Entropy Coil",
            "frameTypeId": "Rare",
            "frameType": 2,
            "ilvl": 56,
            "typeLine": "Unset Ring",
            "baseType": "Unset Ring",
            "corrupted": false,
            "mods": {
              "implicit": [
                {"id": "RingImplicitAdditionalSkillSlots1", "stats": {"local_item_additional_skill_slots": 1}}
              ],
              "explicit": [
                {"id": "ItemFoundRarityIncreasePrefix3", "stats": {"base_item_found_rarity_+%": 19}},
                {"id": "ItemFoundRarityIncrease3", "stats": {"base_item_found_rarity_+%": 16}},
                {"id": "IncreasedLife8", "stats": {"base_maximum_life": 115}},
                {"id": "LightningResist5", "stats": {"base_lightning_damage_resistance_%": 28}}
              ],
              "crafted": [
                {"id": "FireResist6", "stats": {"base_fire_damage_resistance_%": 34}}
              ],
              "desecrated": [
                {"id": "AddedColdDamage6", "stats": {"attack_minimum_added_cold_damage": 13,
                                                     "attack_maximum_added_cold_damage": 18}}
              ]
            },
            "implicitMods": ["Grants 1 additional Skill Slot"],
            "explicitMods": ["+115 to maximum Life", "35% increased [ItemRarity|Rarity of Items] found",
                             "+28% to [Resistances|Lightning Resistance]"],
            "craftedMods": ["+34% to [Resistances|Fire Resistance]"],
            "enchantMods": [],
            "rarity": "Rare"
          }
        }
      ],
      "jewels": [{"itemSlot": 0, "itemData": {"name": "Grand Spectrum", "inventoryId": "Jewel", "frameType": 3}}],
      "flasks": [{"itemSlot": 1, "itemData": {"typeLine": "Ultimate Life Flask", "inventoryId": "Flask"}}],
      "keystones": [{"name": "Dance with Death", "icon": "passives/dwd.webp",
                     "stats": ["25% more Skill Speed while Off Hand is empty"]}],
      "lastSeenUtc": "2026-09-06T15:08:24.593716Z",
      "updatedUtc": "2026-09-06T15:04:48.910172Z",
      "lastCheckedUtc": "2026-09-06T15:08:24.593716Z"
    }"#;

    #[test]
    fn parses_the_character_envelope() {
        let character: CharacterDetail = serde_json::from_str(CHARACTER_JSON).unwrap();
        assert_eq!(character.account, "heygyus-0416");
        assert_eq!(character.name, "ResurrectForbidden");
        assert_eq!(character.league, "Forbidden Rites");
        assert_eq!(character.level, 98);
        assert_eq!(character.class, "Gemling Legionnaire");
        assert_eq!(character.updated_utc, "2026-09-06T15:04:48.910172Z");
        assert_eq!(character.last_seen_utc, "2026-09-06T15:08:24.593716Z");
        assert_eq!(character.jewels.len(), 1);
        assert_eq!(character.flasks[0].item_data.inventory_id, "Flask");
        assert_eq!(character.keystones[0].name, "Dance with Death");
        assert_eq!(character.keystones[0].stats.len(), 1);
    }

    #[test]
    fn parses_the_ring_and_all_five_mod_groups() {
        let character: CharacterDetail = serde_json::from_str(CHARACTER_JSON).unwrap();
        let ring = &character.items[0];
        assert_eq!(ring.item_slot, 8);

        let data = &ring.item_data;
        assert_eq!(data.name, "Entropy Coil");
        assert_eq!(data.base_type, "Unset Ring");
        assert_eq!(data.type_line, "Unset Ring");
        assert_eq!(data.inventory_id, "Ring");
        assert_eq!(data.rarity, "Rare");
        assert_eq!(data.frame_type, 2);
        assert_eq!(data.ilvl, 56);
        assert!(!data.corrupted);
        assert!(data.desecrated);

        assert_eq!(data.mods.implicit.len(), 1);
        assert_eq!(data.mods.explicit.len(), 4);
        assert_eq!(data.mods.crafted.len(), 1);
        assert_eq!(data.mods.desecrated.len(), 1);
        assert!(data.mods.rune.is_empty());

        assert_eq!(data.explicit_mods.len(), 3);
        assert_eq!(data.crafted_mods, ["+34% to [Resistances|Fire Resistance]"]);
        assert_eq!(data.desecrated_mods.len(), 1);
        assert!(data.rune_mods.is_empty());
        assert!(data.enchant_mods.is_empty());
    }

    #[test]
    fn reads_numeric_stats_off_a_mod() {
        let character: CharacterDetail = serde_json::from_str(CHARACTER_JSON).unwrap();
        let life = &character.items[0].item_data.mods.explicit[2];
        assert_eq!(life.id, "IncreasedLife8");
        assert_eq!(
            life.numeric_stats(),
            vec![("base_maximum_life".to_owned(), 115.0)]
        );

        let cold = &character.items[0].item_data.mods.desecrated[0];
        // BTreeMap 按键排序,所以 maximum 在 minimum 前面。
        assert_eq!(
            cold.numeric_stats(),
            vec![
                ("attack_maximum_added_cold_damage".to_owned(), 18.0),
                ("attack_minimum_added_cold_damage".to_owned(), 13.0),
            ]
        );
    }

    #[test]
    fn range_stats_average_and_non_numeric_stats_are_skipped() {
        let entry: ModEntry = serde_json::from_str(
            r#"{"id":"AddedColdDamage6","stats":{"a":[13,18],"b":true,"c":"text","d":[1,2,3],"e":7.5}}"#,
        )
        .unwrap();
        assert_eq!(
            entry.numeric_stats(),
            vec![("a".to_owned(), 15.5), ("e".to_owned(), 7.5)]
        );
    }

    #[test]
    fn mod_ids_collapse_to_families() {
        assert_eq!(mod_family("IncreasedLife8"), "IncreasedLife");
        assert_eq!(mod_family("IncreasedLife6"), "IncreasedLife");
        assert_eq!(mod_family("LightningResist5"), "LightningResist");
        assert_eq!(
            mod_family("RingImplicitAdditionalSkillSlots1"),
            "RingImplicitAdditionalSkillSlots"
        );
        assert_eq!(mod_family("NoTierHere"), "NoTierHere");
        assert_eq!(mod_family(""), "");
        assert_eq!(mod_family("123"), "");
    }

    #[test]
    fn an_empty_object_still_parses() {
        let character: CharacterDetail = serde_json::from_str("{}").unwrap();
        assert_eq!(character.level, 0);
        assert!(character.items.is_empty());
    }
}
