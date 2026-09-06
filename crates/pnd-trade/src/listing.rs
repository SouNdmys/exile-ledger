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
        whisper: text(listing, "whisper"),
        whisper_token: optional_text(listing, "whisper_token"),
        hideout_token: optional_text(listing, "hideout_token"),
        icon: text(item, "icon"),
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

    /// 照今天实测的形状剪的:第一条是计划里那单(山箏#5319、90 chaos、
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
              "name": "山箏#5319",
              "online": {"league": "Forbidden Rites", "status": "afk"},
              "lastCharacterName": "箏箏放電",
              "language": "zh_TW",
              "realm": "poe2"
            },
            "whisper": "@箏箏放電 你好，我想購買 天雷之詠 標價 90 混沌石 在 Forbidden Rites (倉庫頁 \"~price 90 chaos\"; 位置: 左 3, 上 1)"
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
          "item": {"name": "", "typeLine": "Sapphire Ring", "icon": "https://web.poecdn.com/ring.png"}
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
        assert_eq!(first.account, "山箏#5319");
        assert_eq!(first.character, "箏箏放電");
        assert!(first.online, "online 是个对象就算在线");
        assert!(first.afk, "对象里 status=afk");
        assert_eq!(first.indexed, "2026-09-06T09:12:31Z");
        assert_eq!(first.item_name, "Choir of the Storm");
        assert_eq!(first.type_line, "Lapis Amulet");
        assert!(first.whisper.starts_with("@箏箏放電"));
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
