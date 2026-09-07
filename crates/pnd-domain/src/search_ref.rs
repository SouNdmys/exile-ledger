//! 搜索引用:把用户从浏览器地址栏粘进来的东西变成"联赛 + 搜索 id"。
//!
//! 为什么要在本地解码搜索 id:trade2 的搜索 id 是自描述的 —— 它就是查询 JSON
//! 先 gzip 再 base64url(`H4sI` 开头正是 gzip 的 1f 8b 魔数)。官方的"已保存
//! 查询"接口对这种 id 返回 404,所以想反复轮询同一个搜索,只能自己把查询解出来
//! 再 POST 回去。用户在网页上筛好条件,粘一次地址栏就够了。

use std::fmt;
use std::io::Read;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 交易站搜索页 URL 里固定不变的那一段,靠它把联赛和 id 从任意写法的链接里切出来。
const SEARCH_PATH_MARKER: &str = "/trade2/search/poe2/";

/// 一次搜索的最小引用:联赛名(人类可读、未编码)+ 搜索 id。
///
/// 联赛在这里存解码后的原文(`Forbidden Rites`),要拼 URL 时再编码,
/// 这样存进 `settings.json` 的内容和网页上看到的一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRef {
    pub league: String,
    pub search_id: String,
}

/// 解码搜索 id 时可能出的三种岔子,分开报是为了让界面能说清"粘错了什么"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchIdError {
    NotBase64,
    NotGzip,
    NotJson,
}

impl fmt::Display for SearchIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            SearchIdError::NotBase64 => "search id is not valid base64",
            SearchIdError::NotGzip => "search id does not contain gzip data",
            SearchIdError::NotJson => "search id does not decode to a JSON object",
        };
        f.write_str(text)
    }
}

impl std::error::Error for SearchIdError {}

/// 接受两种写法:完整的搜索页 URL,或者光秃秃的搜索 id。
///
/// URL 允许带不带 `https://`、带不带 `www.`、结尾带 `/live`、带 `?query` 或
/// `#fragment` —— 用户从地址栏、从聊天记录里复制来的形态五花八门,能认就认。
/// 只有 id 的时候用 `default_league`(设置页里选的当前联赛)。
pub fn parse_search_reference(input: &str, default_league: &str) -> Option<SearchRef> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // `#fragment` 和 `?query` 都不参与定位,先切掉。
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed);
    let path = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);

    if let Some((head, tail)) = path.split_once(SEARCH_PATH_MARKER) {
        if !host_looks_sane(head) {
            return None;
        }
        return parse_url_tail(tail);
    }

    if is_search_id(path) {
        Some(SearchRef {
            league: default_league.to_string(),
            search_id: path.to_string(),
        })
    } else {
        None
    }
}

/// 搜索页地址。卡片上"打开交易页"按钮就是把这个丢给 `ShellExecuteW`。
pub fn search_page_url(r: &SearchRef) -> String {
    format!(
        "https://www.pathofexile.com{SEARCH_PATH_MARKER}{}/{}",
        encode_league_path(&r.league),
        r.search_id
    )
}

/// Live 搜索页地址;WebSocket 握手的 `Referer` 头要求的正是这一个。
pub fn live_page_url(r: &SearchRef) -> String {
    format!("{}/live", search_page_url(r))
}

/// 按 RFC 3986 的 unreserved 集合做百分号编码(空格 → `%20`)。
///
/// 不用 `+` 代空格:路径段里的 `+` 是字面加号,只有查询串才把 `+` 当空格。
pub fn encode_league_path(league: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(league.len());
    for b in league.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

/// 把搜索 id 还原成查询 JSON 原文。
///
/// 返回的是解压出来的原始文本,不重新序列化 —— 直接塞进 `search_request_body`
/// 再 POST,和网页发出去的字节一模一样,少一处走样的机会。
pub fn decode_search_id(id: &str) -> Result<String, SearchIdError> {
    let raw = decode_base64_relaxed(id.trim()).ok_or(SearchIdError::NotBase64)?;

    // 先看魔数:不是 gzip 就没必要惊动解压器,报错也更准。
    if raw.len() < 2 || raw[0] != 0x1f || raw[1] != 0x8b {
        return Err(SearchIdError::NotGzip);
    }

    let mut bytes = Vec::new();
    GzDecoder::new(raw.as_slice())
        .read_to_end(&mut bytes)
        .map_err(|_| SearchIdError::NotGzip)?;

    let json = String::from_utf8(bytes).map_err(|_| SearchIdError::NotJson)?;
    let value: Value = serde_json::from_str(&json).map_err(|_| SearchIdError::NotJson)?;
    if !value.is_object() {
        return Err(SearchIdError::NotJson);
    }
    Ok(json)
}

/// 搜索自己带的名字,拿来当备注名的默认值。
///
/// 为什么不直接用搜索 id 的前缀:`H4sIAAAAA` 对人来说是一串噪音,而查询里
/// 十有八九已经写着"我在找什么"了 —— 蹲一件暗金就是 `name`,蹲一类底子就是
/// `type`,从关键词搜进来的是 `term`。三个都没有(纯词缀搜索)才轮到 id 顶上。
pub fn default_label_for(query_json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(query_json).ok()?;
    // 两种形状都会递到这儿:搜索 id 解出来的是查询本身,而请求体是把它包在
    // `query` 里的。有那一层就往里走一步,没有就当场用。
    let query = match value.get("query") {
        Some(inner) if inner.is_object() => inner,
        _ => &value,
    };
    ["name", "type", "term"]
        .into_iter()
        .find_map(|key| query.get(key)?.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// 把查询 JSON 包成 search 接口的请求体,排序固定按价格升序 —— 我们只关心最便宜那几件。
///
/// 走 `serde_json::Value` 拼装(而不是字符串拼接),保证出去的一定是合法 JSON;
/// 万一传进来的不是 JSON,退化成 `"query": null`,让服务端去报错而不是这里 panic。
pub fn search_request_body(query_json: &str) -> String {
    let query: Value = serde_json::from_str(query_json).unwrap_or(Value::Null);
    json!({ "query": query, "sort": { "price": "asc" } }).to_string()
}

/// URL 里 marker 之前那一截必须像个主机名(可带 scheme),否则一段随手粘来的
/// 文字只要碰巧含有搜索路径就会被当成链接。
fn host_looks_sane(head: &str) -> bool {
    let host = head.split_once("://").map_or(head, |(_, rest)| rest);
    host.is_empty() || (!host.contains('/') && !host.chars().any(char::is_whitespace))
}

/// marker 之后应当是 `<联赛>/<id>`,最多再跟一个 `live`。
fn parse_url_tail(tail: &str) -> Option<SearchRef> {
    let mut parts = tail.split('/');
    let league_raw = parts.next()?;
    let search_id = parts.next()?;

    let extra: Vec<&str> = parts.filter(|s| !s.is_empty()).collect();
    match extra.as_slice() {
        [] => {}
        [only] if only.eq_ignore_ascii_case("live") => {}
        _ => return None,
    }

    let league = percent_decode_plus(league_raw)?;
    if league.trim().is_empty() || !is_search_id(search_id) {
        return None;
    }
    Some(SearchRef {
        league,
        search_id: search_id.to_string(),
    })
}

/// 搜索 id 只可能是 base64url 的字符;太短的一定不是(gzip 头本身就不止 3 字节)。
fn is_search_id(s: &str) -> bool {
    s.len() >= 4
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 百分号解码,并且把 `+` 当空格。
///
/// 交易站自己写的链接用 `%20`,但从别处转手的链接常见 `+`,两种都得认,
/// 否则 `Forbidden+Rites` 会变成一个不存在的联赛名。
fn percent_decode_plus(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                out.push((hex_value(bytes[i + 1])? << 4) | hex_value(bytes[i + 2])?);
                i += 3;
            }
            // 残缺的 `%X` 说明这串被截断过,不猜。
            b'%' => return None,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 手写的 base64 解码:同时吃 url-safe(`-_`)和标准(`+/`)字母表,`=` 填充可有可无。
///
/// 为这几十行不引一个 crate:domain 层要保持干净,而这里的输入只有一种来源
/// (交易站自己生成的 id),宽松一点比严格更省事。
fn decode_base64_relaxed(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;

    for ch in input.chars() {
        // 填充之后再无数据,提前收工。
        if ch == '=' {
            break;
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1u32 << bits) - 1;
        }
    }

    // 剩下 6 位说明最后一组只有一个字符,base64 不可能这样收尾。
    if bits >= 6 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod search_ref_tests {
    use super::*;

    /// 今天从网页上真抄下来的一个搜索 id(Choir of the Storm)。
    const FIXTURE_ID: &str = "H4sIAAAAAAAAAx2LvQnAIBBGV5GvdgLbjJAyWAhRFPRO9FIEcfdo2vcz0MXJ02EGuEpiggFTTuQxNcgVv8AROTXFQUn06hRuBfof13cNyFt35eheOKQsvm1hp50f6Xdj02AAAAA";
    const FIXTURE_QUERY: &str = r#"{"status":{"option":"online"},"name":"Choir of the Storm","stats":[{"type":"and","filters":[]}]}"#;

    fn parse(input: &str) -> Option<SearchRef> {
        parse_search_reference(input, "Standard")
    }

    #[test]
    fn parses_full_url_with_percent_encoded_league() {
        let r = parse("https://www.pathofexile.com/trade2/search/poe2/Forbidden%20Rites/abcd1234")
            .unwrap();
        assert_eq!(r.league, "Forbidden Rites");
        assert_eq!(r.search_id, "abcd1234");
    }

    #[test]
    fn parses_plus_encoded_league() {
        let r = parse("https://www.pathofexile.com/trade2/search/poe2/Forbidden+Rites/abcd1234")
            .unwrap();
        assert_eq!(r.league, "Forbidden Rites");
    }

    #[test]
    fn parses_live_suffix_query_and_fragment() {
        let r =
            parse("https://www.pathofexile.com/trade2/search/poe2/Forbidden%20Rites/abcd1234/live")
                .unwrap();
        assert_eq!(r.search_id, "abcd1234");

        let r =
            parse("pathofexile.com/trade2/search/poe2/Standard/abcd1234?foo=bar#anchor").unwrap();
        assert_eq!(r.league, "Standard");
        assert_eq!(r.search_id, "abcd1234");

        // 没有 scheme、没有 www. 也认。
        let r = parse("www.pathofexile.com/trade2/search/poe2/Standard/abcd1234/live/").unwrap();
        assert_eq!(r.search_id, "abcd1234");
    }

    #[test]
    fn parses_bare_id_with_default_league() {
        let r = parse("  H4sIAAAA-_09  ").unwrap();
        assert_eq!(r.league, "Standard");
        assert_eq!(r.search_id, "H4sIAAAA-_09");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_none());
        assert!(parse("   ").is_none());
        assert!(parse("abc").is_none(), "id 太短");
        assert!(parse("hello world").is_none());
        assert!(parse("https://example.com/some/page").is_none());
        assert!(parse("https://www.pathofexile.com/trade2/search/poe2/Standard").is_none());
        assert!(parse("https://www.pathofexile.com/trade2/search/poe2//abcd1234").is_none());
        assert!(parse("https://www.pathofexile.com/trade2/search/poe2/Standard/ab").is_none());
        assert!(
            parse("https://www.pathofexile.com/trade2/search/poe2/Standard/abcd1234/nope")
                .is_none()
        );
    }

    #[test]
    fn page_urls_round_trip() {
        let r = SearchRef {
            league: "Forbidden Rites".to_string(),
            search_id: "abcd1234".to_string(),
        };
        let url = search_page_url(&r);
        assert_eq!(
            url,
            "https://www.pathofexile.com/trade2/search/poe2/Forbidden%20Rites/abcd1234"
        );
        assert_eq!(parse(&url).unwrap(), r);

        let live = live_page_url(&r);
        assert!(live.ends_with("/live"));
        assert_eq!(parse(&live).unwrap(), r);
    }

    #[test]
    fn encodes_only_unreserved_characters() {
        assert_eq!(encode_league_path("Forbidden Rites"), "Forbidden%20Rites");
        assert_eq!(encode_league_path("a-b.c_d~e"), "a-b.c_d~e");
        assert_eq!(encode_league_path("HC/SSF"), "HC%2FSSF");
    }

    #[test]
    fn decodes_real_search_id() {
        let json = decode_search_id(FIXTURE_ID).unwrap();
        let got: Value = serde_json::from_str(&json).unwrap();
        let want: Value = serde_json::from_str(FIXTURE_QUERY).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn wraps_decoded_query_into_request_body() {
        let json = decode_search_id(FIXTURE_ID).unwrap();
        let body: Value = serde_json::from_str(&search_request_body(&json)).unwrap();
        assert_eq!(body["query"]["name"], "Choir of the Storm");
        assert_eq!(body["sort"]["price"], "asc");
    }

    /// 蹲一件具体的暗金:查询里写着它的名字,备注名就该是那个名字。
    #[test]
    fn a_named_search_labels_itself_with_the_item_name() {
        assert_eq!(
            default_label_for(r#"{"query":{"name":"Mageblood","status":{"option":"online"}}}"#),
            Some("Mageblood".to_string())
        );
        // 搜索 id 解出来的是查询本身,外面没有 `query` 那一层,两种形状都得认。
        let json = decode_search_id(FIXTURE_ID).unwrap();
        assert_eq!(
            default_label_for(&json),
            Some("Choir of the Storm".to_string())
        );
    }

    /// 没有名字就退到底子,再退到关键词 —— 都比一串 base64 认得出来。
    #[test]
    fn a_type_only_search_falls_back_to_the_type_then_the_term() {
        assert_eq!(
            default_label_for(r#"{"query":{"type":"Sapphire Ring","stats":[]}}"#),
            Some("Sapphire Ring".to_string())
        );
        assert_eq!(
            default_label_for(r#"{"query":{"term":"tornado shot"}}"#),
            Some("tornado shot".to_string())
        );
    }

    /// 纯词缀搜索三样都没有,这时候没有比 id 前缀更好的东西,所以返回 `None`
    /// 让调用方去顶。空字符串也算没有 —— 空的备注名和没填一样难认。
    #[test]
    fn a_search_with_neither_a_name_nor_a_type_has_no_default_label() {
        assert_eq!(
            default_label_for(r#"{"query":{"stats":[{"type":"and","filters":[]}]}}"#),
            None
        );
        assert_eq!(default_label_for(r#"{"query":{"name":"   "}}"#), None);
        assert_eq!(default_label_for("not json at all"), None);
    }

    #[test]
    fn reports_why_a_bad_id_failed() {
        // 合法 base64,但解出来的字节没有 gzip 魔数。
        assert_eq!(
            decode_search_id("aGVsbG8gd29ybGQ="),
            Err(SearchIdError::NotGzip)
        );
        assert_eq!(
            decode_search_id("not base64!"),
            Err(SearchIdError::NotBase64)
        );
    }
}
