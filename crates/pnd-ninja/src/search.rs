//! builds 搜索响应的解码。
//!
//! `GET /poe2/api/builds/<version>/search?overview=<snapshotName>&…` 返回一个
//! 没有 schema 的 protobuf。整包被 field 1 包了一层信封,里面三样东西对我们有用:
//!
//! - **field 1 varint** — 符合筛选条件的角色总数(不只是返回的那 100 个)
//! - **repeated field 2** — 分面(facet):"用这件暗金的有多少人""这个职业多少人",
//!   名字全是字典下标。热门暗金榜就是这里出来的,一次请求覆盖整个联赛
//! - **repeated field 6** — 字典引用:分面键 → NDIC 的 sha1
//! - **repeated field 12** — 前 100 名角色的列(列式存储:一列 100 个值)
//!
//! 未知字段一律跳过,只把字段号记下来放进 `Column::other_fields`,方便探针
//! 报告"对面又加了什么"。

use thiserror::Error;

use crate::wire::{WireError, WireReader, WireValue, packed_varints, utf8};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SearchError {
    #[error("protobuf wire error: {0}")]
    Wire(#[from] WireError),
    /// 整包应当被 field 1 包一层。没有就说明拿到的不是搜索响应(比如一段 HTML 错误页)。
    #[error("response has no field 1 envelope")]
    MissingEnvelope,
}

/// 分面里的一条:某个字典下标有多少个角色命中。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FacetEntry {
    pub index: u32,
    pub count: u64,
}

/// 一个分面。`kind` 就是它该查哪张字典(见 [`dictionary_key_for_facet`])。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facet {
    pub name: String,
    pub kind: String,
    pub entries: Vec<FacetEntry>,
}

/// 字典引用。`overlay_sha1` 是可选的叠加表(poe.ninja 用来给条目补元数据),
/// 我们目前只用主表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DictionaryRef {
    pub key: String,
    pub sha1: String,
    pub overlay_sha1: Option<String>,
}

/// 一列。三种承载方式互斥地出现:数值列用 `varints`(packed),文本列用
/// `strings`(每个角色一条),多值列用 `lists`(每个角色一个 packed 列表)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Column {
    pub id: String,
    pub group: String,
    pub varints: Vec<u64>,
    pub strings: Vec<String>,
    pub lists: Vec<Vec<u64>>,
    pub dictionary_key: Option<String>,
    /// 我们没消费的字段号(去重、按出现顺序)。格式漂移时这是第一手线索。
    pub other_fields: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResponse {
    pub total: u64,
    pub facets: Vec<Facet>,
    pub dictionaries: Vec<DictionaryRef>,
    pub columns: Vec<Column>,
}

/// 一个角色的最小身份:够我们去拉详情、够去重。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CharacterRef {
    pub account: String,
    pub name: String,
    pub class: String,
    pub level: u32,
}

/// 分面名 → NDIC 字典键。
///
/// 多个分面共用一张表(`skills`/`allskills`/`spiritgems` 都是宝石),所以这层
/// 映射不能省。认不出的分面就拿它自己的名字当键——线上观察到 `kind` 字段
/// 恰好就是这个键,所以猜错的代价只是查不到、不会错查。
#[must_use]
pub fn dictionary_key_for_facet(facet: &str) -> &str {
    match facet {
        "class" => "class",
        "items" => "item",
        "skills" | "allskills" | "spiritgems" => "gem",
        "keypassives" => "keypassive",
        "weaponmode" => "weaponmode",
        "anointed" => "anointed",
        "traits" => "skilltrait",
        other => other,
    }
}

/// 解码整包。
pub fn decode(bytes: &[u8]) -> Result<SearchResponse, SearchError> {
    let mut envelope: Option<&[u8]> = None;
    for field in WireReader::new(bytes) {
        if let (1, WireValue::Bytes(inner)) = field? {
            envelope = Some(inner);
        }
    }
    decode_inner(envelope.ok_or(SearchError::MissingEnvelope)?)
}

fn decode_inner(bytes: &[u8]) -> Result<SearchResponse, SearchError> {
    let mut response = SearchResponse::default();
    for field in WireReader::new(bytes) {
        match field? {
            (1, WireValue::Varint(total)) => response.total = total,
            (2, WireValue::Bytes(payload)) => response.facets.push(decode_facet(payload)?),
            (6, WireValue::Bytes(payload)) => {
                response.dictionaries.push(decode_dictionary_ref(payload)?);
            }
            (12, WireValue::Bytes(payload)) => response.columns.push(decode_column(payload)?),
            _ => {}
        }
    }
    Ok(response)
}

fn decode_facet(bytes: &[u8]) -> Result<Facet, SearchError> {
    let mut facet = Facet::default();
    for field in WireReader::new(bytes) {
        match field? {
            (1, WireValue::Bytes(payload)) => facet.name = utf8(payload)?.to_owned(),
            (2, WireValue::Bytes(payload)) => facet.kind = utf8(payload)?.to_owned(),
            (3, WireValue::Bytes(payload)) => facet.entries.push(decode_facet_entry(payload)?),
            _ => {}
        }
    }
    Ok(facet)
}

/// 下标 0 的条目省略 field 1——protobuf 不发默认值。漏掉这一点会让每个分面的
/// 第一名(通常是最热门那个)错位。
fn decode_facet_entry(bytes: &[u8]) -> Result<FacetEntry, SearchError> {
    let mut entry = FacetEntry::default();
    for field in WireReader::new(bytes) {
        match field? {
            (1, WireValue::Varint(index)) => entry.index = u32::try_from(index).unwrap_or(u32::MAX),
            (2, WireValue::Varint(count)) => entry.count = count,
            _ => {}
        }
    }
    Ok(entry)
}

fn decode_dictionary_ref(bytes: &[u8]) -> Result<DictionaryRef, SearchError> {
    let mut reference = DictionaryRef::default();
    for field in WireReader::new(bytes) {
        match field? {
            (1, WireValue::Bytes(payload)) => reference.key = utf8(payload)?.to_owned(),
            (2, WireValue::Bytes(payload)) => reference.sha1 = utf8(payload)?.to_owned(),
            (3, WireValue::Bytes(payload)) => {
                reference.overlay_sha1 = Some(utf8(payload)?.to_owned());
            }
            _ => {}
        }
    }
    Ok(reference)
}

fn decode_column(bytes: &[u8]) -> Result<Column, SearchError> {
    let mut column = Column::default();
    for field in WireReader::new(bytes) {
        match field? {
            (1, WireValue::Bytes(payload)) => column.id = utf8(payload)?.to_owned(),
            (2, WireValue::Bytes(payload)) => column.group = utf8(payload)?.to_owned(),
            // 观察到每列只发一段 packed varint,但 protobuf 允许拆成多段,
            // 所以这里是 extend 而不是赋值。
            (6, WireValue::Bytes(payload)) => column.varints.extend(packed_varints(payload)?),
            (7, WireValue::Bytes(payload)) => column.strings.push(utf8(payload)?.to_owned()),
            (9, WireValue::Bytes(payload)) => column.lists.push(packed_varints(payload)?),
            (11, WireValue::Bytes(payload)) => {
                column.dictionary_key = Some(utf8(payload)?.to_owned());
            }
            (number, _) => {
                if !column.other_fields.contains(&number) {
                    column.other_fields.push(number);
                }
            }
        }
    }
    Ok(column)
}

impl SearchResponse {
    #[must_use]
    pub fn facet(&self, name: &str) -> Option<&Facet> {
        self.facets.iter().find(|facet| facet.name == name)
    }

    #[must_use]
    pub fn dictionary_sha1(&self, key: &str) -> Option<&str> {
        self.dictionaries
            .iter()
            .find(|reference| reference.key == key)
            .map(|reference| reference.sha1.as_str())
    }

    #[must_use]
    pub fn column(&self, id: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.id == id)
    }

    /// 把一个分面的下标换成名字,按人数从多到少排。
    ///
    /// 字典和响应是分别缓存的,可能对不上(字典旧、响应新)。下标越界时给
    /// `"#123"` 而不是丢掉这一条:少一个名字总比少一行数据好,而且这个 `#`
    /// 一眼就能看出是字典该刷新了。
    #[must_use]
    pub fn resolve_facet(&self, name: &str, dictionary: &[String]) -> Vec<(String, u64)> {
        let Some(facet) = self.facet(name) else {
            return Vec::new();
        };
        let mut resolved: Vec<(String, u64)> = facet
            .entries
            .iter()
            .map(|entry| {
                let label = dictionary
                    .get(entry.index as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("#{}", entry.index));
                (label, entry.count)
            })
            .collect();
        resolved.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        resolved
    }

    /// 把 `name`/`account`/`class`/`level` 四列拉链成角色名单。
    ///
    /// 以 `name` 列为准循环:少一列时那一项留空,而不是整个名单为空——
    /// 我们要的是"去拉详情的清单",拿到名字和账号就已经够用了。
    #[must_use]
    pub fn character_refs(&self, class_dictionary: &[String]) -> Vec<CharacterRef> {
        let Some(names) = self.column("name") else {
            return Vec::new();
        };
        let accounts = self.column("account");
        let classes = self.column("class");
        let levels = self.column("level");

        names
            .strings
            .iter()
            .enumerate()
            .map(|(index, name)| CharacterRef {
                account: accounts
                    .and_then(|column| column.strings.get(index))
                    .cloned()
                    .unwrap_or_default(),
                name: name.clone(),
                class: classes
                    .and_then(|column| column.varints.get(index))
                    .and_then(|slot| class_dictionary.get(*slot as usize))
                    .cloned()
                    .unwrap_or_default(),
                level: levels
                    .and_then(|column| column.varints.get(index))
                    .map(|value| u32::try_from(*value).unwrap_or(u32::MAX))
                    .unwrap_or_default(),
            })
            .collect()
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    /// 手写一个最小 protobuf 编码器。解码器的每条规则都得有人从另一头验它,
    /// 用真实响应当夹具不行:快照一天变几次,测试会莫名其妙变红。
    mod encode {
        pub fn varint(mut value: u64) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let byte = (value & 0x7f) as u8;
                value >>= 7;
                if value == 0 {
                    out.push(byte);
                    return out;
                }
                out.push(byte | 0x80);
            }
        }

        pub fn tag(field: u32, wire_type: u8) -> Vec<u8> {
            varint(u64::from(field) << 3 | u64::from(wire_type))
        }

        pub fn varint_field(field: u32, value: u64) -> Vec<u8> {
            let mut out = tag(field, 0);
            out.extend(varint(value));
            out
        }

        pub fn bytes_field(field: u32, payload: &[u8]) -> Vec<u8> {
            let mut out = tag(field, 2);
            out.extend(varint(payload.len() as u64));
            out.extend_from_slice(payload);
            out
        }

        pub fn string_field(field: u32, value: &str) -> Vec<u8> {
            bytes_field(field, value.as_bytes())
        }

        pub fn packed_field(field: u32, values: &[u64]) -> Vec<u8> {
            let mut payload = Vec::new();
            for value in values {
                payload.extend(varint(*value));
            }
            bytes_field(field, &payload)
        }

        pub fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
            parts.iter().flatten().copied().collect()
        }
    }

    use encode::*;

    fn facet_entry(index: Option<u64>, count: u64) -> Vec<u8> {
        let mut payload = Vec::new();
        if let Some(index) = index {
            payload.extend(varint_field(1, index));
        }
        payload.extend(varint_field(2, count));
        payload
    }

    /// 造一份形状和线上一致、但只有 3 个角色的响应。
    fn sample_response() -> Vec<u8> {
        let class_facet = concat(&[
            string_field(1, "class"),
            string_field(2, "class"),
            // 下标 0 的条目不带 field 1,和线上一样。
            bytes_field(3, &facet_entry(None, 46)),
            bytes_field(3, &facet_entry(Some(1), 780)),
            bytes_field(3, &facet_entry(Some(2), 22_316)),
        ]);
        let items_facet = concat(&[
            string_field(1, "items"),
            string_field(2, "item"),
            bytes_field(3, &facet_entry(Some(0), 60_943)),
            bytes_field(3, &facet_entry(Some(2), 7_158)),
            // 字典只有 3 条,下标 9 越界。
            bytes_field(3, &facet_entry(Some(9), 12)),
        ]);

        let name_column = concat(&[
            string_field(1, "name"),
            string_field(2, "name"),
            varint_field(4, 1),
            string_field(7, "ExileCharacter"),
            string_field(7, "KingPinUwU"),
            string_field(7, "sqvoznyak"),
            varint_field(13, 3),
        ]);
        let account_column = concat(&[
            string_field(1, "account"),
            string_field(2, "account"),
            string_field(7, "player-0416"),
            string_field(7, "dota2enjoyer-1809"),
            string_field(7, "elinskiy2002-4257"),
        ]);
        let class_column = concat(&[
            string_field(1, "class"),
            string_field(2, "class"),
            packed_field(6, &[2, 2, 1]),
            string_field(11, "class"),
            varint_field(13, 3),
        ]);
        let level_column = concat(&[
            string_field(1, "level"),
            string_field(2, "level"),
            packed_field(6, &[98, 97, 97]),
        ]);
        let skills_column = concat(&[
            string_field(1, "skills"),
            string_field(2, "skills"),
            packed_field(9, &[4, 7]),
            packed_field(9, &[4]),
            packed_field(9, &[]),
            string_field(11, "gem"),
        ]);

        let inner = concat(&[
            varint_field(1, 62_108),
            bytes_field(2, &class_facet),
            bytes_field(2, &items_facet),
            bytes_field(
                6,
                &concat(&[string_field(1, "class"), string_field(2, "aaa1")]),
            ),
            bytes_field(
                6,
                &concat(&[
                    string_field(1, "item"),
                    string_field(2, "bbb2"),
                    string_field(3, "ccc3"),
                ]),
            ),
            bytes_field(12, &name_column),
            bytes_field(12, &account_column),
            bytes_field(12, &class_column),
            bytes_field(12, &level_column),
            bytes_field(12, &skills_column),
            // 我们不认识的顶层字段,必须被静静跳过。
            bytes_field(11, b"whatever"),
        ]);
        bytes_field(1, &inner)
    }

    fn class_dictionary() -> Vec<String> {
        ["Abyssal Lich", "Spirit Walker", "Gemling Legionnaire"]
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect()
    }

    fn item_dictionary() -> Vec<String> {
        ["Magic Flask", "Rare Ring", "Wake of Destruction"]
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect()
    }

    #[test]
    fn decodes_total_and_shape() {
        let response = decode(&sample_response()).unwrap();
        assert_eq!(response.total, 62_108);
        assert_eq!(response.facets.len(), 2);
        assert_eq!(response.dictionaries.len(), 2);
        assert_eq!(response.columns.len(), 5);
    }

    #[test]
    fn missing_envelope_is_an_error() {
        assert_eq!(decode(b""), Err(SearchError::MissingEnvelope));
        assert_eq!(
            decode(&varint_field(1, 7)),
            Err(SearchError::MissingEnvelope)
        );
    }

    #[test]
    fn decodes_facets_including_the_implicit_zero_index() {
        let response = decode(&sample_response()).unwrap();
        let facet = response.facet("class").unwrap();
        assert_eq!(facet.kind, "class");
        assert_eq!(
            facet.entries,
            vec![
                FacetEntry {
                    index: 0,
                    count: 46
                },
                FacetEntry {
                    index: 1,
                    count: 780
                },
                FacetEntry {
                    index: 2,
                    count: 22_316
                },
            ]
        );
        assert!(response.facet("nosuchfacet").is_none());
    }

    #[test]
    fn resolves_facets_through_the_dictionary_sorted_by_count() {
        let response = decode(&sample_response()).unwrap();
        assert_eq!(
            response.resolve_facet("class", &class_dictionary()),
            vec![
                ("Gemling Legionnaire".to_owned(), 22_316),
                ("Spirit Walker".to_owned(), 780),
                ("Abyssal Lich".to_owned(), 46),
            ]
        );
    }

    /// 字典比响应旧时下标会越界。宁可显示 `#9`,也不要悄悄丢一行。
    #[test]
    fn out_of_range_indices_become_hash_labels() {
        let response = decode(&sample_response()).unwrap();
        assert_eq!(
            response.resolve_facet("items", &item_dictionary()),
            vec![
                ("Magic Flask".to_owned(), 60_943),
                ("Wake of Destruction".to_owned(), 7_158),
                ("#9".to_owned(), 12),
            ]
        );
        assert!(
            response
                .resolve_facet("nosuchfacet", &item_dictionary())
                .is_empty()
        );
    }

    #[test]
    fn reads_dictionary_references() {
        let response = decode(&sample_response()).unwrap();
        assert_eq!(response.dictionary_sha1("class"), Some("aaa1"));
        assert_eq!(response.dictionary_sha1("item"), Some("bbb2"));
        assert_eq!(response.dictionaries[0].overlay_sha1, None);
        assert_eq!(
            response.dictionaries[1].overlay_sha1.as_deref(),
            Some("ccc3")
        );
        assert_eq!(response.dictionary_sha1("gem"), None);
    }

    #[test]
    fn columns_keep_their_three_payload_shapes() {
        let response = decode(&sample_response()).unwrap();

        let names = response.column("name").unwrap();
        assert_eq!(names.group, "name");
        assert_eq!(names.strings.len(), 3);
        assert!(names.varints.is_empty());
        // field 4 和 13 我们不消费,但记下号码给探针看。
        assert_eq!(names.other_fields, vec![4, 13]);

        let levels = response.column("level").unwrap();
        assert_eq!(levels.varints, vec![98, 97, 97]);

        let skills = response.column("skills").unwrap();
        assert_eq!(skills.lists, vec![vec![4, 7], vec![4], vec![]]);
        assert_eq!(skills.dictionary_key.as_deref(), Some("gem"));
        assert_eq!(response.column("nosuchcolumn"), None);
    }

    #[test]
    fn zips_the_four_columns_into_character_refs() {
        let response = decode(&sample_response()).unwrap();
        let refs = response.character_refs(&class_dictionary());
        assert_eq!(refs.len(), 3);
        assert_eq!(
            refs[0],
            CharacterRef {
                account: "player-0416".to_owned(),
                name: "ExileCharacter".to_owned(),
                class: "Gemling Legionnaire".to_owned(),
                level: 98,
            }
        );
        assert_eq!(refs[2].class, "Spirit Walker");
        assert_eq!(refs[2].level, 97);
    }

    /// 没有 name 列就没有可抓的角色,这时给空名单而不是 panic。
    #[test]
    fn character_refs_without_a_name_column_is_empty() {
        let inner = concat(&[varint_field(1, 5)]);
        let response = decode(&bytes_field(1, &inner)).unwrap();
        assert!(response.character_refs(&class_dictionary()).is_empty());
    }

    /// PoE1 多出来八张字典(2026-09-09 实测,Allflame 的搜索响应带 15 条字典引用,
    /// PoE2 只有 7 条):兜底那条"认不出就用它自己的名字"正好把它们全接住了,
    /// 一个特例都不用加。
    ///
    /// 反过来 PoE1 **没有** `spiritgems`(那是 PoE2 才有的部位)。少一个分面不是
    /// 错误:[`SearchResponse::facet`] 给 `None`,[`SearchResponse::resolve_facet`]
    /// 给空表,整轮采样照跑。
    #[test]
    fn the_poe1_only_facets_fall_through_to_their_own_names() {
        for facet in [
            "secondascendancy",
            "bandit",
            "atlasskill",
            "mastery",
            "runegraft",
            "tattoo",
            "vestigialmod",
            "pantheon",
        ] {
            assert_eq!(dictionary_key_for_facet(facet), facet);
        }

        let response = decode(&sample_response()).unwrap();
        assert!(response.facet("spiritgems").is_none());
        assert!(
            response
                .resolve_facet("spiritgems", &item_dictionary())
                .is_empty()
        );
    }

    #[test]
    fn facet_names_map_to_dictionary_keys() {
        assert_eq!(dictionary_key_for_facet("class"), "class");
        assert_eq!(dictionary_key_for_facet("items"), "item");
        assert_eq!(dictionary_key_for_facet("skills"), "gem");
        assert_eq!(dictionary_key_for_facet("allskills"), "gem");
        assert_eq!(dictionary_key_for_facet("spiritgems"), "gem");
        assert_eq!(dictionary_key_for_facet("keypassives"), "keypassive");
        assert_eq!(dictionary_key_for_facet("weaponmode"), "weaponmode");
        assert_eq!(dictionary_key_for_facet("anointed"), "anointed");
        assert_eq!(dictionary_key_for_facet("traits"), "skilltrait");
        assert_eq!(dictionary_key_for_facet("brandnew"), "brandnew");
    }

    #[test]
    fn broken_payload_reports_a_wire_error() {
        let mut bytes = sample_response();
        bytes.truncate(bytes.len() - 5);
        assert!(matches!(decode(&bytes), Err(SearchError::Wire(_))));
    }
}
