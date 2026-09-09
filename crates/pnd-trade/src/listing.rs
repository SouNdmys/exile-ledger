//! 把 fetch 接口那一大坨 JSON 摘成 [`ListingSummary`]。
//!
//! 摘要的原则是**宁缺毋滥**:认得的字段取出来,不认得的整片忽略,
//! 单条挂单解析不出来就跳过它而不是整批报错 —— 一次 fetch 回来 10 件,
//! 为了其中一件形状怪就丢掉另外 9 件,是最不划算的做法。
//!
//! 已知会踩的坑,每一条都在下面的解析里对上号:
//! - `result` 里会有 `null`(那个 id 的挂单在这几秒里被买走/下架了)
//! - 稀有物品的 `item.name` 是空串,真正的名字在 `typeLine` 里
//! - `listing.price` 可能整个不存在(挂了个"仅供展示"的无价单)
//! - `account.online` 只有在线时才是个对象,离线时这个键干脆没有

use pnd_domain::{ListingSummary, Price};
use serde_json::Value;

use crate::ParseError;

/// 解析一次 fetch 的响应体。
///
/// 只有"根本不是 JSON"和"没有 `result` 数组"才算失败:那说明我们拿到的
/// 压根不是这个接口的东西(比如 Cloudflare 的 HTML 页)。
pub fn parse_fetch_response(body: &[u8]) -> Result<Vec<ListingSummary>, ParseError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| ParseError::NotJson(e.to_string()))?;
    let entries = root
        .get("result")
        .and_then(Value::as_array)
        .ok_or(ParseError::Missing("result"))?;
    Ok(entries.iter().filter_map(summarize).collect())
}

/// 同一次 fetch,但按**请求时的 id 顺序**回话:每个 id 要么有摘要,要么是 `None`。
///
/// 市场观察问的问题和蹲价不一样:蹲价只想要"现在有哪些便宜货",少一条无所谓;
/// 观察要的是"我问的这 10 条里,哪几条没了" —— 没了才是那条挂单卖掉/撤掉的
/// 时刻。[`parse_fetch_response`] 把 `null` 直接丢掉,丢完就分不清是哪个 id 没了。
///
/// 按 id 对上号而不是按位置对:服务端到底是补一个 `null` 还是干脆少给一条,
/// 我们不替它保证 —— 两种形状这里都能对上。
pub fn parse_fetch_response_by_id(
    requested_ids: &[String],
    body: &[u8],
) -> Result<Vec<(String, Option<ListingSummary>)>, ParseError> {
    let found = parse_fetch_response(body)?;
    Ok(requested_ids
        .iter()
        .map(|id| {
            let listing = found.iter().find(|listing| listing.id == *id).cloned();
            (id.clone(), listing)
        })
        .collect())
}

/// 同一次 fetch,但**按位置**回话:数组里第几格就是回信里第几格,
/// `null` 那一格是 `None`。
///
/// 给 live 推来的"把手"用。把手是一整张 JWT,不是挂单 id —— 回信里那几条
/// 挂单带的是它们**自己**的 id,和请求里那串对不上号,所以
/// [`parse_fetch_response_by_id`] 那种按 id 对号的做法在这里一条也认不出来。
/// 而一张把手可能换回 1 到 3 条挂单(token 长度会跟着跳),所以也不能假定
/// "一格对一条"。位置是这里唯一还站得住的东西。
pub fn parse_fetch_response_slots(body: &[u8]) -> Result<Vec<Option<ListingSummary>>, ParseError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| ParseError::NotJson(e.to_string()))?;
    let entries = root
        .get("result")
        .and_then(Value::as_array)
        .ok_or(ParseError::Missing("result"))?;
    Ok(entries.iter().map(summarize).collect())
}

/// 一条挂单 → 一份摘要。返回 `None` 就是"这条不要了"(null、或者连 id 都没有)。
fn summarize(entry: &Value) -> Option<ListingSummary> {
    // 过期的 id 在 result 里是 null;没有 id 的条目我们也没法去重,一并跳过。
    let id = entry.get("id")?.as_str()?.to_string();

    let listing = entry.get("listing");
    let item = entry.get("item");
    let account = child(listing, "account");
    let online = child(account, "online");

    // 在线 = `online` 是个对象。离线时服务端不是给 null,而是根本不给这个键。
    let is_online = online.is_some_and(Value::is_object);
    let afk = text(online, "status").eq_ignore_ascii_case("afk");

    // 稀有物品的 name 是空串,typeLine 才是"Sapphire Ring"这种能看的东西。
    let type_line = text(item, "typeLine");
    let name = text(item, "name");
    let item_name = if name.is_empty() {
        type_line.clone()
    } else {
        name
    };

    Some(ListingSummary {
        id,
        item_name,
        type_line,
        price: child(listing, "price").and_then(parse_price),
        account: text(account, "name"),
        character: text(account, "lastCharacterName"),
        online: is_online,
        afk,
        indexed: text(listing, "indexed"),
        // 缺这个键的响应当"还在":老形状里没有它,而凭空判一条挂单没了
        // 比漏判一条贵得多。
        verified: child(item, "verified")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        whisper: text(listing, "whisper"),
        whisper_token: optional_text(listing, "whisper_token"),
        hideout_token: optional_text(listing, "hideout_token"),
        icon: text(item, "icon"),
        // 原样一份 `item`,给市场观察按词缀聚合用。`to_string` 而不是原始
        // 字节切片:我们手上只有解析好的 `Value`,再序列化一次得到的是同样
        // 的内容(键序按 serde_json 的保留顺序),而这里要的是内容不是字节。
        item_json: item.map(Value::to_string).unwrap_or_default(),
    })
}

/// `price{type, amount, currency}`。三个字段少一个就当没标价:
/// 半个价格没法比较,更不能拿来当去重键。
fn parse_price(price: &Value) -> Option<Price> {
    let amount = price.get("amount")?.as_f64()?;
    let currency = price.get("currency")?.as_str()?;
    Some(Price::from_trade(amount, currency))
}

fn child<'a>(parent: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    parent.and_then(|value| value.get(key))
}

/// 取一个字符串字段,缺了就是空串 —— 摘要里这些字段全是"给人看的",
/// 空着不影响判定,没必要为它们做 `Option`。
fn text(parent: Option<&Value>, key: &str) -> String {
    child(parent, key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 两个 token 例外:它们要么有要么没有(取决于请求带没带 POESESSID),
/// 空串和没有是一回事,都得是 `None`,否则按钮会拿空 token 去请求。
fn optional_text(parent: Option<&Value>, key: &str) -> Option<String> {
    child(parent, key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod listing_tests {
    use super::*;
    use pnd_domain::Currency;

    /// 照今天实测的形状剪的:第一条是计划里那单(Seller#1234、90 chaos、
    /// zh_TW 的私聊文本、afk),第二条带两个 token 且没有 price,
    /// 第三条是 null(id 过期),第四条是稀有物品(name 为空)。
    const FETCH_JSON: &str = r#"{
      "result": [
        {
          "id": "aaa111",
          "listing": {
            "method": "psapi",
            "indexed": "2026-09-06T09:12:31Z",
            "stash": {"name": "~price 90 chaos", "x": 3, "y": 1},
            "price": {"type": "~price", "amount": 90, "currency": "chaos"},
            "account": {
              "name": "Seller#1234",
              "online": {"league": "Forbidden Rites", "status": "afk"},
              "lastCharacterName": "測試角色",
              "language": "zh_TW",
              "realm": "poe2"
            },
            "whisper": "@測試角色 你好，我想購買 天雷之詠 標價 90 混沌石 在 Forbidden Rites (倉庫頁 \"~price 90 chaos\"; 位置: 左 3, 上 1)"
          },
          "item": {
            "name": "Choir of the Storm",
            "typeLine": "Lapis Amulet",
            "baseType": "Lapis Amulet",
            "rarity": "Unique",
            "ilvl": 68,
            "icon": "https://web.poecdn.com/gen/image/choir.png",
            "explicitMods": ["+35 to Intelligence"]
          }
        },
        {
          "id": "bbb222",
          "listing": {
            "indexed": "2026-09-06T09:30:00Z",
            "account": {
              "name": "Seller#1234",
              "online": {"league": "Forbidden Rites"},
              "lastCharacterName": "SomeChar"
            },
            "whisper": "@SomeChar Hi, I would like to buy your Choir of the Storm",
            "whisper_token": "eyJ3aGlzcGVy",
            "hideout_token": "eyJoaWRlb3V0"
          },
          "item": {
            "name": "Choir of the Storm",
            "typeLine": "Lapis Amulet",
            "icon": "https://web.poecdn.com/gen/image/choir.png"
          }
        },
        null,
        {
          "id": "ccc333",
          "listing": {
            "indexed": "2026-09-06T09:40:00Z",
            "price": {"type": "~b/o", "amount": 0.5, "currency": "divine"},
            "account": {"name": "Offline#9", "lastCharacterName": "Gone"},
            "whisper": "@Gone Hi"
          },
          "item": {"name": "", "typeLine": "Sapphire Ring", "icon": "https://web.poecdn.com/ring.png",
                   "verified": false}
        }
      ]
    }"#;

    fn parsed() -> Vec<ListingSummary> {
        parse_fetch_response(FETCH_JSON.as_bytes()).unwrap()
    }

    #[test]
    fn null_entries_are_dropped_not_fatal() {
        // 四个条目,中间那个 null 被跳过。
        let listings = parsed();
        assert_eq!(listings.len(), 3);
        assert_eq!(listings[0].id, "aaa111");
        assert_eq!(listings[1].id, "bbb222");
        assert_eq!(listings[2].id, "ccc333");
    }

    #[test]
    fn reads_price_account_and_online_state() {
        let first = &parsed()[0];
        assert_eq!(
            first.price,
            Some(Price::new(90_000, Currency::Chaos)),
            "90 chaos 存成千分整数"
        );
        assert_eq!(first.account, "Seller#1234");
        assert_eq!(first.character, "測試角色");
        assert!(first.online, "online 是个对象就算在线");
        assert!(first.afk, "对象里 status=afk");
        assert_eq!(first.indexed, "2026-09-06T09:12:31Z");
        assert_eq!(first.item_name, "Choir of the Storm");
        assert_eq!(first.type_line, "Lapis Amulet");
        assert!(first.whisper.starts_with("@測試角色"));
        assert_eq!(first.icon, "https://web.poecdn.com/gen/image/choir.png");
        assert_eq!(first.whisper_token, None, "没带 POESESSID 就没有 token");
        assert_eq!(first.hideout_token, None);
    }

    #[test]
    fn online_without_status_is_not_afk() {
        let second = &parsed()[1];
        assert!(second.online);
        assert!(!second.afk);
    }

    #[test]
    fn tokens_show_up_when_the_session_was_sent() {
        let second = &parsed()[1];
        assert_eq!(second.whisper_token.as_deref(), Some("eyJ3aGlzcGVy"));
        assert_eq!(second.hideout_token.as_deref(), Some("eyJoaWRlb3V0"));
    }

    #[test]
    fn missing_price_becomes_none() {
        assert_eq!(parsed()[1].price, None);
    }

    /// **卖掉的挂单不是 `null`,是 `item.verified == false`。**
    ///
    /// 2026-09-08 拿主人的程序观察过的 20 个 id 匿名 fetch 了两次:5.8 小时前
    /// 第一次见到的那 10 条一条 `null` 都没有,却有 3 条带着 `"verified": false`,
    /// 而且 `listing.indexed` 被顶到"这一趟索引发现它不见了"的时刻;9 分钟前
    /// 的那 10 条里也已经有 1 条是 false。`null` 只留给被彻底清掉的 id。
    ///
    /// 没有这一格,市场观察就在等一个几乎不会来的 `null` —— 那正是主人那六个
    /// 小时里 gone 一直是 0 的原因。
    #[test]
    fn a_sold_listing_says_so_with_verified_false() {
        let listings = parsed();
        assert!(listings[0].verified, "没有 verified 这个键就当它还在");
        assert!(listings[1].verified);
        assert!(
            !listings[2].verified,
            "item.verified = false 就是这条挂单已经没了"
        );
    }

    #[test]
    fn offline_account_and_rare_item_name_fallback() {
        let third = &parsed()[2];
        assert!(!third.online, "没有 online 键 = 离线");
        assert!(!third.afk);
        // 稀有物品的 name 是空串,拿 typeLine 顶上,否则卡片标题会是空的。
        assert_eq!(third.item_name, "Sapphire Ring");
        assert_eq!(third.type_line, "Sapphire Ring");
        assert_eq!(third.price, Some(Price::new(500, Currency::Divine)));
    }

    /// 问了 4 个 id,其中 `zzz999` 已经没了 —— 服务端在它的位置上放了个 `null`。
    /// 按 id 回话就能指着它说"就是这条没了",而不是只知道"少了一条"。
    #[test]
    fn a_gone_id_comes_back_as_none_in_request_order() {
        let requested: Vec<String> = ["aaa111", "bbb222", "zzz999", "ccc333"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pairs = parse_fetch_response_by_id(&requested, FETCH_JSON.as_bytes()).unwrap();

        let shape: Vec<(&str, bool)> = pairs
            .iter()
            .map(|(id, listing)| (id.as_str(), listing.is_some()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("aaa111", true),
                ("bbb222", true),
                ("zzz999", false),
                ("ccc333", true)
            ]
        );
        assert_eq!(pairs[0].1.as_ref().unwrap().item_name, "Choir of the Storm");
    }

    /// **2026-09-07 实测的形状**(`trade_probe --observe`,匿名):拿一条真 id
    /// 加一条把末 4 位改过的假 id 去 fetch,服务端回的是 **HTTP 200**、`result`
    /// 数组**长度仍然等于问的个数**、查不到的那一格是 `null`,而且顺序就是
    /// 请求里的顺序。不是 404,也不是少给一条。
    ///
    /// 这一条钉的就是那次真答复的形状 —— 市场观察全靠"哪个 id 变成了 null"
    /// 来断定一条挂单没了,形状变了必须有人来看一眼。
    #[test]
    fn the_shape_the_server_really_answered_on_2026_09_07() {
        // 真 id 在前、假 id 在后,`null` 落在末尾 —— 上面那个测试里 `null`
        // 在中间,两处位置都验过了。
        let real = "75da72f5ff03ee9fba70c3e1b9bb8f41c0abfca87d877b24aa01fbfc9150eb0e";
        let fake = "75da72f5ff03ee9fba70c3e1b9bb8f41c0abfca87d877b24aa01fbfc9150fc1f";
        let body = format!(
            r#"{{"result":[{{"id":"{real}","listing":{{"indexed":"2026-09-07T12:47:39Z",
               "price":{{"type":"~price","amount":1,"currency":"divine"}},
               "account":{{"name":"Seller#1234","online":{{"league":"Forbidden Rites"}}}},
               "whisper":"@x hi"}},
               "item":{{"name":"Choir of the Storm","typeLine":"Lapis Amulet"}}}},null]}}"#
        );
        let requested = vec![real.to_string(), fake.to_string()];
        let pairs = parse_fetch_response_by_id(&requested, body.as_bytes()).unwrap();

        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, real);
        assert_eq!(pairs[0].1.as_ref().unwrap().account, "Seller#1234");
        assert_eq!(pairs[1].0, fake);
        assert!(pairs[1].1.is_none(), "查不到的 id 就是 None");
    }

    /// 万一服务端不补 `null` 而是干脆少给一条,答案也必须一样 ——
    /// 我们按 id 对号,不按位置。
    #[test]
    fn a_short_result_array_still_lines_up_with_the_request() {
        let body = br#"{"result":[{"id":"ccc333","listing":{"indexed":"2026-09-06T09:40:00Z"},
            "item":{"typeLine":"Sapphire Ring"}}]}"#;
        let requested: Vec<String> = ["aaa111", "ccc333"].iter().map(|s| s.to_string()).collect();
        let pairs = parse_fetch_response_by_id(&requested, body).unwrap();
        assert_eq!(pairs[0].0, "aaa111");
        assert!(pairs[0].1.is_none(), "第一个 id 没回来 = 它没了");
        assert_eq!(pairs[1].1.as_ref().unwrap().type_line, "Sapphire Ring");
    }

    /// `item` 那一整块要原样留一份。
    ///
    /// 市场观察按词缀聚合,而词缀数组(explicit/implicit/crafted/…)不在摘要
    /// 里 —— 摘要只管卡片要显示什么。原文留着,统计想换个算法就不用回头改
    /// 解析。摘要里那几个字段还得照常有,原文只是**多**一份,不是替代。
    #[test]
    fn the_raw_item_block_is_kept_alongside_the_summary() {
        let first = &parsed()[0];
        let item: Value = serde_json::from_str(&first.item_json).expect("item_json is JSON");
        assert_eq!(item["name"], "Choir of the Storm");
        assert_eq!(item["rarity"], "Unique");
        assert_eq!(item["ilvl"], 68);
        assert_eq!(item["explicitMods"][0], "+35 to Intelligence");
        // 摘出来的字段没有被原文顶掉。
        assert_eq!(first.item_name, "Choir of the Storm");

        // 没有 `item` 那一块的条目留空串,而不是塞一个 "null" 进去。
        let body = br#"{"result":[{"id":"aaa","listing":{"indexed":"x"}}]}"#;
        assert_eq!(parse_fetch_response(body).unwrap()[0].item_json, "");
    }

    /// 把手换回来的那几条挂单带的是它们自己的 id,和请求里那张 token 对不上号,
    /// 所以只能按位置读:第三格是 `null`,那一格就是"这一件在我们看它第一眼
    /// 之前就没了"。
    #[test]
    fn the_slot_view_keeps_the_nulls_in_place() {
        let slots = parse_fetch_response_slots(FETCH_JSON.as_bytes()).unwrap();
        assert_eq!(slots.len(), 4, "四格一格不少");
        assert_eq!(slots[0].as_ref().unwrap().id, "aaa111");
        assert_eq!(slots[1].as_ref().unwrap().id, "bbb222");
        assert!(slots[2].is_none(), "第三格是 null");
        assert_eq!(slots[3].as_ref().unwrap().id, "ccc333");
    }

    #[test]
    fn a_body_without_result_is_an_error() {
        assert_eq!(
            parse_fetch_response(br#"{"error":{"code":1}}"#),
            Err(ParseError::Missing("result"))
        );
        assert!(matches!(
            parse_fetch_response(b"<!DOCTYPE html>"),
            Err(ParseError::NotJson(_))
        ));
        assert!(
            parse_fetch_response(br#"{"result":[]}"#)
                .unwrap()
                .is_empty()
        );
    }
}
