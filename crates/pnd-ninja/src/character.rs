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

/// 一行显示文本 → 模板:每个数换成 `#`,方括号标记摊平成人话。
///
/// `base_maximum_mana` 认不出来是什么,`+# to maximum Mana` 一眼就懂 ——
/// 这个函数就是统计表能用游戏里的话说话的全部原因。
///
/// 数换成 `#` 而不是留着,是因为统计的是**这条词缀有多少人带**:
/// `+81 to maximum Mana` 和 `+64 to maximum Mana` 是同一条词缀的两个卷法,
/// 数值那一栏另有 p25/p50/p75 三格在说。
#[must_use]
pub fn mod_template(display: &str) -> String {
    let flat = flatten_markup(display);
    let mut out = String::with_capacity(flat.len());
    let bytes: Vec<char> = flat.chars().collect();
    let mut index = 0;
    let mut pending_space = false;
    while index < bytes.len() {
        let character = bytes[index];
        if character.is_whitespace() {
            // 长词缀在原文里是换行的,压成一行,不然表格那一格会被撑高。
            pending_space = !out.is_empty();
            index += 1;
            continue;
        }
        if let Some(end) = number_at(&bytes, index) {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push('#');
            index = end;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(character);
        index += 1;
    }
    out
}

/// `[Resistances|Lightning Resistance]` → `Lightning Resistance`,
/// `[Attack]` → `Attack`。竖线前面那半是游戏客户端查词条用的键,不是给人看的。
fn flatten_markup(display: &str) -> String {
    let mut out = String::with_capacity(display.len());
    let mut rest = display;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            // 没闭合的方括号原样留着:吞掉反而让人以为文本就是这样。
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = &after[..close];
        out.push_str(inner.rsplit('|').next().unwrap_or(inner));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// `chars[start]` 起是不是一个数;是就返回它后面第一个字符的下标。
///
/// 负号只在**前一个字符不是字母数字**时才算数的一部分:`-30% reduced` 里的
/// 是符号,`Level 1-2` 里的是连字符。
fn number_at(chars: &[char], start: usize) -> Option<usize> {
    let mut index = start;
    if chars[index] == '-' {
        let leads = start == 0 || !chars[start - 1].is_alphanumeric();
        if !leads || !chars.get(index + 1).is_some_and(char::is_ascii_digit) {
            return None;
        }
        index += 1;
    }
    if !chars.get(index).is_some_and(char::is_ascii_digit) {
        return None;
    }
    while chars.get(index).is_some_and(char::is_ascii_digit) {
        index += 1;
    }
    // 小数点后面得真的跟着数字,不然句号会被当成小数点吞掉。
    if chars.get(index) == Some(&'.') && chars.get(index + 1).is_some_and(char::is_ascii_digit) {
        index += 1;
        while chars.get(index).is_some_and(char::is_ascii_digit) {
            index += 1;
        }
    }
    Some(index)
}

/// 一行显示文本里的那些数,顺序和 [`mod_template`] 里 `#` 的顺序一致。
///
/// 对外开着,是因为交易站那边的词缀只有显示文本(没有 ninja 那份结构化的
/// `mods`):市场观察把 `Adds 29 to 38 Cold Damage` 记成模板 + 两个数,靠的
/// 就是这一对函数,不能自己再抄一份取数的规则。
#[must_use]
pub fn line_numbers(display: &str) -> Vec<f64> {
    let chars: Vec<char> = flatten_markup(display).chars().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if let Some(end) = number_at(&chars, index) {
            let text: String = chars[index..end].iter().collect();
            if let Ok(number) = text.parse::<f64>() {
                out.push(number);
            }
            index = end;
        } else {
            index += 1;
        }
    }
    out
}

/// 一个 `(stat id, 数值)` 配上它在显示文本里的出处。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatDisplay {
    pub stat_id: String,
    pub value: f64,
    /// 模板文本。**配不上时是空串**,界面退回显示 stat id。
    pub display: String,
    /// 这个数是那一行里的第几个(1 起)。`Adds 29 to 38 Cold Damage` 的
    /// minimum 是 1、maximum 是 2 —— 没有它,那两行在界面上一模一样。
    /// 配不上时是 0。
    pub value_index: u8,
}

/// 结构化词缀 ↔ 显示文本,**按数值配对,不按下标**。
///
/// 两个数组看着一样长就想按下标走是个陷阱:线上真实数据里
/// `mods.explicit[0]` 是"杀敌回血"而 `explicitMods[0]` 是"增加冰伤",
/// 两千个角色里对得上的只有两成。ninja 给的这两份东西一份是生成顺序、
/// 一份是游戏里的显示顺序,而且不是一一对应 ——
///
/// - 一条词缀能摊成两行(命中 + 攻速的合成工艺)
/// - 两条同类词缀会合并成一行(19% + 16% 稀有度写成 `35% increased`)
/// - 有的结构化词缀根本不显示(长矛那条"显示用"的隐式)
///
/// 唯一能对上的是数:一行里写着 29,带 29 的那个 stat 就是它。同一个数在
/// 两行里都出现(`+70 to maximum Energy Shield` 和 `70% increased Energy
/// Shield`)时**交白卷**,宁可让界面退回 stat id,也不能把文本配错。
///
/// 返回的外层和 `mods` 一一对应,内层是这条词缀的每个 `(stat id, 数值)`。
#[must_use]
pub fn pair_mod_displays(mods: &[ModEntry], lines: &[String]) -> Vec<Vec<StatDisplay>> {
    let templates: Vec<String> = lines.iter().map(|line| mod_template(line)).collect();
    let numbers: Vec<Vec<f64>> = lines.iter().map(|line| line_numbers(line)).collect();
    mods.iter()
        .map(|entry| {
            entry
                .numeric_stats()
                .into_iter()
                .map(|(stat_id, value)| {
                    let mut hits = 0usize;
                    let mut found = (0usize, 0usize);
                    for (line, line_numbers) in numbers.iter().enumerate() {
                        let position = line_numbers
                            .iter()
                            .position(|number| (number - value).abs() < 1e-9);
                        if let Some(position) = position {
                            if hits == 0 {
                                found = (line, position);
                            }
                            hits += 1;
                        }
                    }
                    let (display, value_index) = if hits == 1 {
                        (
                            templates[found.0].clone(),
                            u8::try_from(found.1 + 1).unwrap_or(0),
                        )
                    } else {
                        (String::new(), 0)
                    };
                    StatDisplay {
                        stat_id,
                        value,
                        display,
                        value_index,
                    }
                })
                .collect()
        })
        .collect()
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

    /// 线上真实剪下来的一根长矛。它一件就把这套配对要处理的四种情况都摆全了,
    /// 所以后面几个测试都拿它当靶子。
    const SPEAR_JSON: &str = include_str!("../fixtures/character_item_spear.json");

    fn spear() -> ItemData {
        serde_json::from_str::<ItemEntry>(SPEAR_JSON)
            .unwrap()
            .item_data
    }

    #[test]
    fn a_display_line_becomes_a_template() {
        assert_eq!(mod_template("+81 to maximum Mana"), "+# to maximum Mana");
        assert_eq!(
            mod_template("Adds 27 to 68 Fire Damage"),
            "Adds # to # Fire Damage"
        );
        assert_eq!(
            mod_template("25% increased Attack Speed"),
            "#% increased Attack Speed"
        );
        // 小数和负数各算一个数,整段一起换掉。
        assert_eq!(
            mod_template("+7.51% to Critical Hit Chance"),
            "+#% to Critical Hit Chance"
        );
        assert_eq!(
            mod_template("-1 Prefix Modifier allowed"),
            "# Prefix Modifier allowed"
        );
    }

    /// ninja 的显示文本里带着游戏客户端用的方括号标记,摊平了才是人话。
    #[test]
    fn the_bracket_markup_gets_flattened() {
        assert_eq!(
            mod_template("+28% to [Resistances|Lightning Resistance]"),
            "+#% to Lightning Resistance"
        );
        assert_eq!(
            mod_template("25% increased [Attack] Speed"),
            "#% increased Attack Speed"
        );
        // 换行的长词缀压成一行,不然表格里那一格会撑高。
        assert_eq!(
            mod_template("Enemies take 20% increased Damage\nfor each type"),
            "Enemies take #% increased Damage for each type"
        );
    }

    /// **两个数组不是按下标对齐的。** 这根矛的 `mods.explicit[0]` 是
    /// "杀敌回血",而 `explicitMods[0]` 是"增加冰伤" —— 照下标走会把每一行
    /// 都配错。配对只能认数值。
    #[test]
    fn the_two_arrays_are_not_index_parallel() {
        let item = spear();
        assert_eq!(item.mods.explicit[0].id, "LifeGainedFromEnemyDeath5");
        assert_eq!(item.explicit_mods[0], "Adds 29 to 38 [Cold|Cold] Damage");

        let paired = pair_mod_displays(&item.mods.explicit, &item.explicit_mods);
        assert_eq!(paired[0][0].stat_id, "base_life_gained_on_enemy_death");
        assert_eq!(paired[0][0].display, "Gain # Life per enemy killed");
    }

    /// 一行里有两个数时,每个 stat 还要说清自己是第几个 —— 不然
    /// "Adds # to # Cold Damage" 那两行在界面上一模一样。
    #[test]
    fn each_value_knows_which_number_of_the_line_it_is() {
        let item = spear();
        let paired = pair_mod_displays(&item.mods.explicit, &item.explicit_mods);
        // BTreeMap 按键排序,maximum 在 minimum 前面。
        let cold = &paired[4];
        assert_eq!(
            (
                cold[0].stat_id.as_str(),
                cold[0].display.as_str(),
                cold[0].value_index
            ),
            (
                "local_maximum_added_cold_damage",
                "Adds # to # Cold Damage",
                2
            )
        );
        assert_eq!(
            (
                cold[1].stat_id.as_str(),
                cold[1].display.as_str(),
                cold[1].value_index
            ),
            (
                "local_minimum_added_cold_damage",
                "Adds # to # Cold Damage",
                1
            )
        );
        assert_eq!(cold[1].value, 29.0);

        // 只有一个数的行就是第 1 个。
        let speed = &paired[1];
        assert_eq!(speed[0].display, "#% increased Attack Speed");
        assert_eq!(speed[0].value_index, 1);
    }

    /// 一条词缀可以摊成两行显示文本(命中 + 攻速的合成工艺),两个 stat
    /// 各配各的那一行。
    #[test]
    fn one_mod_can_span_two_display_lines() {
        let item = spear();
        let paired = pair_mod_displays(&item.mods.crafted, &item.crafted_mods);
        assert_eq!(paired.len(), 1);
        let mut got: Vec<(&str, &str)> = paired[0]
            .iter()
            .map(|stat| (stat.stat_id.as_str(), stat.display.as_str()))
            .collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![
                ("accuracy_rating", "+# to Accuracy Rating"),
                ("local_attack_speed_+%", "#% increased Attack Speed"),
            ]
        );
    }

    /// 配不上就交白卷,而不是硬塞一行别的词缀的文本。隐式那一条在
    /// `implicitMods` 里根本没有对应行(它是"显示用"的占位词缀)。
    #[test]
    fn an_unmatched_stat_falls_back_to_no_template() {
        let item = spear();
        let paired = pair_mod_displays(&item.mods.implicit, &item.implicit_mods);
        assert_eq!(
            paired[0][0].stat_id,
            "local_display_grants_spear_throw_skill"
        );
        assert_eq!(paired[0][0].display, "");
        assert_eq!(paired[0][0].value_index, 0);

        // 同一个数在两行里都出现时也一样:宁可空着,也不能猜错。
        let mods: Vec<ModEntry> = serde_json::from_str(
            r#"[{"id":"A","stats":{"local_energy_shield":70}},
                {"id":"B","stats":{"local_energy_shield_+%":70}}]"#,
        )
        .unwrap();
        let lines = vec![
            "+70 to maximum [EnergyShield|Energy Shield]".to_owned(),
            "70% increased [EnergyShield|Energy Shield]".to_owned(),
        ];
        let paired = pair_mod_displays(&mods, &lines);
        assert!(
            paired
                .iter()
                .all(|mod_stats| mod_stats[0].display.is_empty())
        );

        // 显示文本整个缺席时也不能 panic。
        assert!(pair_mod_displays(&mods, &[])[0][0].display.is_empty());
        assert!(pair_mod_displays(&[], &lines).is_empty());
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
