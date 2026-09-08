//! 挂单摘要:从 fetch 接口那一大坨 JSON 里,只留下卡片和列表要用的字段。
//!
//! 为什么要摘一遍而不是原样传:上层(runtime、UI、SQLite)都不该认识交易站的
//! JSON 形状,接口哪天加字段改字段,只动 `pnd-trade` 里的解析,这里不受影响。

use serde::{Deserialize, Serialize};

use crate::price::Price;

/// 一条蹲价搜索的 id。包一层是为了别把它和挂单 id、搜索 id 这些同样是字符串的东西弄混。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WatchId(pub String);

impl WatchId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WatchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一条市场观察的 id。和 [`WatchId`] 分开包一层,是因为两者说的根本不是一件事:
/// 蹲价是"这个价出现了叫我",观察是"这批货最后都怎么样了"。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObservationId(pub String);

impl ObservationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ObservationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一条挂单里我们关心的部分。
///
/// `price` 是 `Option`:交易站允许挂"仅供展示"的无价单,那种直接判 `Unpriced`,不提醒。
/// 两个 token 只有带 POESESSID 请求时才有,而且是短命 JWT —— 存下来只为"去藏身处"
/// 按钮点下去那一刻能用,过期就重新 fetch。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListingSummary {
    pub id: String,
    pub item_name: String,
    pub type_line: String,
    pub price: Option<Price>,
    pub account: String,
    pub character: String,
    pub online: bool,
    pub afk: bool,
    pub indexed: String,
    /// 交易站的索引里这条挂单**还对得上游戏里的实物**吗(`item.verified`)。
    ///
    /// 这是"它没了"的真正信号,而不是 `null`:2026-09-08 匿名抓了 20 条实测,
    /// 卖掉的挂单照样整条回来,只是这一格变成 `false`,而 `listing.indexed`
    /// 被顶到发现它不见了的那一趟索引;`null` 只出现在被彻底清掉的 id 上。
    /// 主人的程序因此跑了六个小时 `gone = 0` —— 我们一直在等一个永远不来的
    /// `null`。缺这个键时当 `true`:老响应里没有它,而"看不出没了"就该当还在。
    pub verified: bool,
    pub whisper: String,
    pub whisper_token: Option<String>,
    pub hideout_token: Option<String>,
    pub icon: String,
    /// fetch 回来的 `item` 那一整块的原文,一个字都没动。
    ///
    /// 上面那几个字段是"卡片要显示什么"摘出来的,而市场观察问的是另一个问题:
    /// **这件货身上有哪些词缀**。词缀数组(explicit/implicit/crafted/rune/…)
    /// 形状五花八门,今天摘一遍、明天想统计别的又得改一遍摘法;所以原文原样
    /// 留一份,统计什么时候想改都行。摘要留空(`""`)是合法的 —— 没有 `item`
    /// 那一块(或者这条摘要是测试现编的)就是空串。
    pub item_json: String,
}

impl ListingSummary {
    /// 一行短标签,给探针输出和提醒卡片标题用(界面正文文案仍然走 `pnd-app` 的 i18n)。
    pub fn short_label(&self) -> String {
        let price = match &self.price {
            Some(price) => price.display(),
            None => "no price".to_string(),
        };
        format!("{} · {}", self.item_name, price)
    }
}

#[cfg(test)]
mod listing_tests {
    use super::*;
    use crate::price::Currency;

    fn sample(price: Option<Price>) -> ListingSummary {
        ListingSummary {
            id: "abc".to_string(),
            item_name: "Choir of the Storm".to_string(),
            type_line: "Lapis Amulet".to_string(),
            price,
            account: "SomeSeller".to_string(),
            character: "SomeChar".to_string(),
            online: true,
            afk: false,
            indexed: "2026-09-06T12:00:00Z".to_string(),
            verified: true,
            whisper: "@SomeChar Hi, I'd like to buy...".to_string(),
            whisper_token: None,
            hideout_token: None,
            icon: "https://web.poecdn.com/image/item.png".to_string(),
            item_json: String::new(),
        }
    }

    #[test]
    fn short_label_covers_priced_and_unpriced() {
        let priced = sample(Some(Price::new(15_000, Currency::Divine)));
        assert_eq!(priced.short_label(), "Choir of the Storm · 15 divine");
        assert_eq!(sample(None).short_label(), "Choir of the Storm · no price");
    }

    #[test]
    fn watch_id_is_a_transparent_string() {
        let id = WatchId("w-1".to_string());
        assert_eq!(id.to_string(), "w-1");
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""w-1""#);
        assert_eq!(serde_json::from_str::<WatchId>(r#""w-1""#).unwrap(), id);
    }
}
