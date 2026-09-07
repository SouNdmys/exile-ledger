//! 采样分区计划(纯函数:不碰网络,也不碰数据库)。
//!
//! builds 的 search 接口每次只返回**前 100 个角色**,而且没有分页——想多看几个
//! 角色,唯一的办法是换一组筛选条件再问一次。于是采样就成了"分区并集":
//! 用一批互相重叠的条件各要一次前 100,再把结果按 `(账号, 角色名)` 去重合起来。
//!
//! 分区分五档([`PartitionTier`]),前四档在第一轮就能算出来,最后一档要等
//! `class=X` 那一轮的响应回来才知道该问哪些技能:
//!
//! | 档 | 条件 | 从哪来 |
//! |---|---|---|
//! | `Whole` | 无(整个联赛) | 固定一条,顺便把三张分面表捞回来 |
//! | `Class` | `class=X` | build-index-state 里占比 ≥ 阈值的职业 |
//! | `Skill` | `skills=Y` | 全联赛 `skills` 分面的前 N |
//! | `Unique` | `items=U` | 全联赛 `items` 分面的前 N(排掉稀有度桶) |
//! | `ClassSkill` | `class=X&skills=Y` | 每个职业**自己**分面里的前几个技能 |
//!
//! 分区键([`Partition::key`])就是查询串本身(`class=Gemling+Legionnaire` 里的
//! `+` 由客户端负责,这里存的是没编码的原文)。这样数据库里只存一个字符串,
//! 断点续跑时用 [`query_from_key`] 就能原样拆回该发的请求,不用另存一份 JSON。

use std::collections::HashSet;

use crate::index_state::LeagueBuild;
use crate::search::SearchResponse;

/// 采样规模的旋钮。默认值就是计划里定的那一组:约 65 次搜索、2,000 个角色。
///
/// 2,000 这个数是有理由的:真实占比 10% 的词缀在这个样本量下误差约 ±1.3%
/// (95% 置信),够回答"这个部位大家都上什么词缀";再往上翻倍只换来 ±0.9%,
/// 但抓取时间也翻倍(角色详情按小时预算走,默认 36 秒一个)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleOptions {
    pub sample_target: u32,
    pub min_class_share_percent: f64,
    pub skills_per_class: u32,
    pub top_global_skills: u32,
    pub top_uniques: u32,
}

impl Default for SampleOptions {
    fn default() -> Self {
        Self {
            sample_target: 2_000,
            min_class_share_percent: 1.0,
            skills_per_class: 2,
            top_global_skills: 10,
            top_uniques: 20,
        }
    }
}

/// 一条分区来自哪一档。除了给界面看,它还是**去重后排座次**的依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PartitionTier {
    #[default]
    Whole,
    Class,
    Skill,
    Unique,
    ClassSkill,
}

impl PartitionTier {
    /// 截样本时谁先留下:数越小越先留。
    ///
    /// 职业榜最能代表"这个联赛长什么样",所以排在技能榜和暗金榜前面;
    /// 后两档天然偏向某几套 build,截到 2,000 时该先砍的就是它们。
    /// `ClassSkill` 排在 `Skill` 前面是因为它已经被职业分过一次,
    /// 覆盖面比全联赛技能榜更均匀。
    #[must_use]
    pub fn priority(self) -> u8 {
        match self {
            PartitionTier::Whole => 0,
            PartitionTier::Class => 1,
            PartitionTier::ClassSkill => 2,
            PartitionTier::Skill => 3,
            PartitionTier::Unique => 4,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PartitionTier::Whole => "whole",
            PartitionTier::Class => "class",
            PartitionTier::Skill => "skill",
            PartitionTier::Unique => "unique",
            PartitionTier::ClassSkill => "class_skill",
        }
    }

    /// 认不出来的一律当 `Whole`(和 `LiveState::parse` 一个道理):老库里的值
    /// 不该让整行读不出来,大不了这一条的座次排前面一点。
    #[must_use]
    pub fn parse(raw: &str) -> PartitionTier {
        match raw {
            "class" => PartitionTier::Class,
            "skill" => PartitionTier::Skill,
            "unique" => PartitionTier::Unique,
            "class_skill" => PartitionTier::ClassSkill,
            _ => PartitionTier::Whole,
        }
    }
}

/// 这一档分区是按哪张 NDIC 字典切出来的。
///
/// 采样管线得先把对应的字典抓下来,才有名字可以填进查询串;`Whole` 不需要
/// 任何字典,因为它压根没有条件。
#[must_use]
pub fn dictionary_key_for_tier(tier: PartitionTier) -> Option<&'static str> {
    match tier {
        PartitionTier::Whole => None,
        PartitionTier::Class => Some("class"),
        PartitionTier::Skill | PartitionTier::ClassSkill => Some("gem"),
        PartitionTier::Unique => Some("item"),
    }
}

/// 一次 search 请求的工作单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// `""` / `class=X` / `skills=Y` / `items=U` / `class=X&skills=Y`。
    pub key: String,
    /// 查询参数,值**没有** URL 编码——编码是 `client.rs` 的活。
    pub query: Vec<(String, String)>,
    pub tier: PartitionTier,
}

impl Partition {
    #[must_use]
    pub fn new(tier: PartitionTier, query: Vec<(String, String)>) -> Self {
        Self {
            key: key_for_query(&query),
            query,
            tier,
        }
    }
}

/// 分区键 = 查询串。职业名和暗金名里不会出现 `&` 或 `=`,所以这个拼法可逆。
#[must_use]
pub fn key_for_query(query: &[(String, String)]) -> String {
    query
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// [`key_for_query`] 的反向:库里只存了键,断点续跑时靠它还原该发的请求。
#[must_use]
pub fn query_from_key(key: &str) -> Vec<(String, String)> {
    if key.is_empty() {
        return Vec::new();
    }
    key.split('&')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((name.to_owned(), value.to_owned()))
        })
        .collect()
}

/// item 字典里混着"稀有度桶":`Rare Ring`、`Magic Flask`、`Normal Body Armour`、
/// `Unknown`。它们不是某一件暗金,而是"这个部位穿了一件稀有装"的统称。
///
/// 人数上它们几乎永远排在最前面(60,943 人穿着某件 `Magic Flask`),不排掉的话
/// 热门暗金榜的前几名全是它们,`items=` 分区也会浪费在一条问不出东西的查询上。
#[must_use]
pub fn is_rarity_bucket(name: &str) -> bool {
    name.starts_with("Normal ")
        || name.starts_with("Magic ")
        || name.starts_with("Rare ")
        || name.starts_with("Unknown")
}

/// 第一轮的分区清单:`""` → 职业 → 技能 → 暗金,顺序就是它们该被请求的顺序。
///
/// 顺序不是审美问题:角色去重时"先来的留下",而分区是按这个顺序跑的,
/// 所以职业档的角色天然会盖过技能档和暗金档的同一个人。
#[must_use]
pub fn first_pass_partitions(
    league: &LeagueBuild,
    whole: &SearchResponse,
    gem_dict: &[String],
    item_dict: &[String],
    opts: &SampleOptions,
) -> Vec<Partition> {
    let mut out = vec![Partition::new(PartitionTier::Whole, Vec::new())];

    // build-index-state 里的 statistics 不保证有序,自己按占比从大到小排一遍。
    let mut classes: Vec<&crate::index_state::ClassShare> = league
        .statistics
        .iter()
        .filter(|share| !share.class.is_empty() && share.percentage >= opts.min_class_share_percent)
        .collect();
    classes.sort_by(|left, right| right.percentage.total_cmp(&left.percentage));
    out.extend(classes.into_iter().map(|share| {
        Partition::new(
            PartitionTier::Class,
            vec![("class".to_owned(), share.class.clone())],
        )
    }));

    out.extend(
        top_facet_names(whole, "skills", gem_dict, opts.top_global_skills, false)
            .into_iter()
            .map(|skill| Partition::new(PartitionTier::Skill, vec![("skills".to_owned(), skill)])),
    );

    out.extend(
        top_facet_names(whole, "items", item_dict, opts.top_uniques, true)
            .into_iter()
            .map(|item| Partition::new(PartitionTier::Unique, vec![("items".to_owned(), item)])),
    );

    out
}

/// 第二轮:某个职业**自己**分面里最热的几个技能。
///
/// 用全联赛技能榜配职业会问出一堆零结果的组合(法师不会去玩重击),
/// 所以必须等 `class=X` 那一轮的响应回来,拿它自己的分面再切。
#[must_use]
pub fn class_skill_partitions(
    class: &str,
    class_response: &SearchResponse,
    gem_dict: &[String],
    opts: &SampleOptions,
) -> Vec<Partition> {
    top_facet_names(
        class_response,
        "skills",
        gem_dict,
        opts.skills_per_class,
        false,
    )
    .into_iter()
    .map(|skill| {
        Partition::new(
            PartitionTier::ClassSkill,
            vec![
                ("class".to_owned(), class.to_owned()),
                ("skills".to_owned(), skill),
            ],
        )
    })
    .collect()
}

/// 分面前 N 名的名字。`resolve_facet` 已经按人数从多到少排好了。
///
/// 空名字要滤掉:字典缺一条就会给出空串,拼进查询串会变成 `skills=`,
/// 那是一次注定白跑的请求。
fn top_facet_names(
    response: &SearchResponse,
    facet: &str,
    dictionary: &[String],
    limit: u32,
    drop_buckets: bool,
) -> Vec<String> {
    response
        .resolve_facet(facet, dictionary)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty())
        .filter(|name| !drop_buckets || !is_rarity_bucket(name))
        .take(limit as usize)
        .collect()
}

/// 从某个分区的前 100 名里捞出来的一个角色。`from_partition` 记的是哪条分区
/// 把他带进来的——界面上"这人是从暗金榜进来的"就是看它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledCharacter {
    pub account: String,
    pub name: String,
    pub class: String,
    pub level: u32,
    pub from_partition: String,
    pub tier: PartitionTier,
}

/// 把所有分区的结果并成最终名单。
///
/// 三步:按 `(账号, 角色名)` 去重(留先来的那条,也就是排在前面的分区)、
/// 按档次排座次(同档内保持原有先后)、截到 `target`。
///
/// 去重"留先来的"而不是"留档次最高的",是因为分区本来就是按档次顺序跑的:
/// 先来的那条天生就是档次最高的那条,不用再比一次。
#[must_use]
pub fn select_sample(rows: Vec<SampledCharacter>, target: u32) -> Vec<SampledCharacter> {
    let mut seen: HashSet<(String, String)> = HashSet::with_capacity(rows.len());
    let mut unique: Vec<SampledCharacter> = Vec::with_capacity(rows.len());
    for row in rows {
        if seen.insert((row.account.clone(), row.name.clone())) {
            unique.push(row);
        }
    }
    // sort_by_key 是稳定排序,所以同一档里谁先被采到谁就还在前面。
    unique.sort_by_key(|row| row.tier.priority());
    unique.truncate(target as usize);
    unique
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::index_state::ClassShare;
    use crate::search::{Facet, FacetEntry};

    fn class_share(class: &str, percentage: f64) -> ClassShare {
        ClassShare {
            class: class.to_owned(),
            percentage,
            trend: 0,
        }
    }

    fn facet(name: &str, entries: &[(u32, u64)]) -> Facet {
        Facet {
            name: name.to_owned(),
            kind: name.to_owned(),
            entries: entries
                .iter()
                .map(|(index, count)| FacetEntry {
                    index: *index,
                    count: *count,
                })
                .collect(),
        }
    }

    fn gem_dictionary() -> Vec<String> {
        ["Lightning Arrow", "Spark", "Contagion", "Ice Nova"]
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect()
    }

    /// 前两条是稀有度桶,后三条才是真暗金。桶排在最前面,和线上一样。
    fn item_dictionary() -> Vec<String> {
        [
            "Magic Flask",
            "Rare Ring",
            "Wake of Destruction",
            "Beira's Anguish",
            "Arakaali's Gift",
        ]
        .iter()
        .map(|entry| (*entry).to_owned())
        .collect()
    }

    fn whole_response() -> SearchResponse {
        SearchResponse {
            total: 61_390,
            facets: vec![
                // 故意乱序:resolve_facet 负责按人数排。
                facet("skills", &[(2, 900), (0, 5_000), (1, 3_000), (3, 40)]),
                facet(
                    "items",
                    &[(0, 60_943), (1, 44_000), (2, 7_158), (3, 6_413), (4, 5_044)],
                ),
            ],
            ..SearchResponse::default()
        }
    }

    fn league() -> LeagueBuild {
        LeagueBuild {
            league_name: "Forbidden Rites".to_owned(),
            league_url: "forbiddenrites".to_owned(),
            total: 61_390,
            hardcore: false,
            statistics: vec![
                class_share("Disciple of Varashta", 10.0),
                class_share("Gemling Legionnaire", 35.8),
                // 低于 1% 的职业不值得单独占一次请求。
                class_share("Blood Mage", 0.4),
                class_share("Smith of Kitava", 1.0),
            ],
        }
    }

    fn options() -> SampleOptions {
        SampleOptions {
            top_global_skills: 2,
            top_uniques: 2,
            ..SampleOptions::default()
        }
    }

    #[test]
    fn the_defaults_are_the_ones_from_the_plan() {
        let opts = SampleOptions::default();
        assert_eq!(opts.sample_target, 2_000);
        assert_eq!(opts.min_class_share_percent, 1.0);
        assert_eq!(opts.skills_per_class, 2);
        assert_eq!(opts.top_global_skills, 10);
        assert_eq!(opts.top_uniques, 20);
    }

    /// 顺序和键一起钉死:去重时"先来的留下",顺序错了样本的构成就变了。
    #[test]
    fn first_pass_runs_whole_then_classes_then_skills_then_uniques() {
        let partitions = first_pass_partitions(
            &league(),
            &whole_response(),
            &gem_dictionary(),
            &item_dictionary(),
            &options(),
        );
        assert_eq!(
            partitions
                .iter()
                .map(|p| p.key.as_str())
                .collect::<Vec<_>>(),
            vec![
                "",
                "class=Gemling Legionnaire",
                "class=Disciple of Varashta",
                "class=Smith of Kitava",
                "skills=Lightning Arrow",
                "skills=Spark",
                "items=Wake of Destruction",
                "items=Beira's Anguish",
            ]
        );
        assert_eq!(
            partitions.iter().map(|p| p.tier).collect::<Vec<_>>(),
            vec![
                PartitionTier::Whole,
                PartitionTier::Class,
                PartitionTier::Class,
                PartitionTier::Class,
                PartitionTier::Skill,
                PartitionTier::Skill,
                PartitionTier::Unique,
                PartitionTier::Unique,
            ]
        );

        // 键是查询串本身,值不编码。
        assert!(partitions[0].query.is_empty());
        assert_eq!(
            partitions[1].query,
            vec![("class".to_owned(), "Gemling Legionnaire".to_owned())]
        );
        assert_eq!(
            partitions[6].query,
            vec![("items".to_owned(), "Wake of Destruction".to_owned())]
        );
    }

    /// 桶的人数比任何一件暗金都高,不排掉的话前两名全被它们占了。
    #[test]
    fn rarity_buckets_never_become_item_partitions() {
        let partitions = first_pass_partitions(
            &league(),
            &whole_response(),
            &gem_dictionary(),
            &item_dictionary(),
            &options(),
        );
        let items: Vec<&str> = partitions
            .iter()
            .filter(|p| p.tier == PartitionTier::Unique)
            .map(|p| p.key.as_str())
            .collect();
        assert_eq!(
            items,
            vec!["items=Wake of Destruction", "items=Beira's Anguish"]
        );
    }

    #[test]
    fn bucket_names_are_recognised_by_their_prefix() {
        assert!(is_rarity_bucket("Rare Ring"));
        assert!(is_rarity_bucket("Magic Flask"));
        assert!(is_rarity_bucket("Normal Body Armour"));
        assert!(is_rarity_bucket("Unknown"));
        assert!(is_rarity_bucket("Unknown Charm"));
        // 真暗金不会被误伤——包括名字里带 Rare/Magic 但不是前缀的。
        assert!(!is_rarity_bucket("Wake of Destruction"));
        assert!(!is_rarity_bucket("Rarity of Wealth"));
        assert!(!is_rarity_bucket("Magicka"));
        assert!(!is_rarity_bucket(""));
    }

    #[test]
    fn class_skill_partitions_carry_both_conditions() {
        let class_response = SearchResponse {
            total: 22_032,
            facets: vec![facet("skills", &[(3, 10), (1, 900), (2, 4_000)])],
            ..SearchResponse::default()
        };
        let partitions = class_skill_partitions(
            "Gemling Legionnaire",
            &class_response,
            &gem_dictionary(),
            &SampleOptions::default(),
        );
        assert_eq!(
            partitions
                .iter()
                .map(|p| p.key.as_str())
                .collect::<Vec<_>>(),
            vec![
                "class=Gemling Legionnaire&skills=Contagion",
                "class=Gemling Legionnaire&skills=Spark",
            ]
        );
        assert_eq!(partitions[0].tier, PartitionTier::ClassSkill);
        assert_eq!(
            partitions[0].query,
            vec![
                ("class".to_owned(), "Gemling Legionnaire".to_owned()),
                ("skills".to_owned(), "Contagion".to_owned()),
            ]
        );
    }

    #[test]
    fn a_key_round_trips_back_into_a_query() {
        for query in [
            vec![],
            vec![("class".to_owned(), "Gemling Legionnaire".to_owned())],
            vec![
                ("class".to_owned(), "Gemling Legionnaire".to_owned()),
                ("skills".to_owned(), "Lightning Arrow".to_owned()),
            ],
        ] {
            assert_eq!(query_from_key(&key_for_query(&query)), query);
        }
    }

    #[test]
    fn tier_strings_round_trip_and_unknown_falls_back() {
        for tier in [
            PartitionTier::Whole,
            PartitionTier::Class,
            PartitionTier::Skill,
            PartitionTier::Unique,
            PartitionTier::ClassSkill,
        ] {
            assert_eq!(PartitionTier::parse(tier.as_str()), tier);
        }
        assert_eq!(PartitionTier::parse("nonsense"), PartitionTier::Whole);
        assert_eq!(dictionary_key_for_tier(PartitionTier::Whole), None);
        assert_eq!(
            dictionary_key_for_tier(PartitionTier::ClassSkill),
            Some("gem")
        );
        assert_eq!(dictionary_key_for_tier(PartitionTier::Unique), Some("item"));
    }

    fn sampled(name: &str, from: &str, tier: PartitionTier) -> SampledCharacter {
        SampledCharacter {
            account: format!("{name}-acct"),
            name: name.to_owned(),
            class: "Gemling Legionnaire".to_owned(),
            level: 98,
            from_partition: from.to_owned(),
            tier,
        }
    }

    /// 分区是按档次顺序跑的,所以同一个人的职业档那条排在暗金档那条前面。
    /// 去重留先来的 = 留职业档那条。
    #[test]
    fn dedupe_keeps_the_class_tier_copy() {
        let rows = vec![
            sampled(
                "ResurrectForbidden",
                "class=Gemling Legionnaire",
                PartitionTier::Class,
            ),
            sampled("KingPinUwU", "skills=Spark", PartitionTier::Skill),
            sampled(
                "ResurrectForbidden",
                "items=Wake of Destruction",
                PartitionTier::Unique,
            ),
        ];
        let picked = select_sample(rows, 100);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].name, "ResurrectForbidden");
        assert_eq!(picked[0].tier, PartitionTier::Class);
        assert_eq!(picked[0].from_partition, "class=Gemling Legionnaire");
        assert_eq!(picked[1].name, "KingPinUwU");
    }

    /// 同名不同账号是两个人,不能因为角色名撞了就丢掉一个。
    #[test]
    fn dedupe_keys_on_account_and_name_together() {
        let mut other = sampled("Twin", "class=A", PartitionTier::Class);
        other.account = "someone-else".to_owned();
        let rows = vec![sampled("Twin", "class=A", PartitionTier::Class), other];
        assert_eq!(select_sample(rows, 100).len(), 2);
    }

    /// 档次决定座次,同档内保持采样顺序;截断从最后一档开始砍。
    #[test]
    fn tier_priority_orders_the_sample_and_truncation_cuts_the_tail() {
        let rows = vec![
            sampled("u1", "items=A", PartitionTier::Unique),
            sampled("s1", "skills=A", PartitionTier::Skill),
            sampled("w1", "", PartitionTier::Whole),
            sampled("cs1", "class=A&skills=B", PartitionTier::ClassSkill),
            sampled("c1", "class=A", PartitionTier::Class),
            sampled("c2", "class=B", PartitionTier::Class),
        ];
        let ordered = select_sample(rows.clone(), 100);
        assert_eq!(
            ordered
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            vec!["w1", "c1", "c2", "cs1", "s1", "u1"]
        );

        let truncated = select_sample(rows, 3);
        assert_eq!(
            truncated
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            vec!["w1", "c1", "c2"]
        );
        assert!(select_sample(Vec::new(), 10).is_empty());
    }
}
