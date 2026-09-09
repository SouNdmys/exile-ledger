//! 搜索引用:把用户从浏览器地址栏粘进来的东西变成"哪一代游戏 + 联赛 + 搜索 id"。
//!
//! 为什么要在本地解码搜索 id:trade2 的搜索 id 是自描述的 —— 它就是查询 JSON
//! 先 gzip 再 base64url(`H4sI` 开头正是 gzip 的 1f 8b 魔数)。官方的"已保存
//! 查询"接口对这种 id 返回 404,所以想反复轮询同一个搜索,只能自己把查询解出来
//! 再 POST 回去。用户在网页上筛好条件,粘一次地址栏就够了。
//!
//! **PoE1 大多数时候也是这样,但不保证。** 2026-09-09 实测:PoE1 的接口
//! `POST /api/trade/search/<联赛>` 回来的 id 同样是 gzip+base64url 的自描述
//! 长串,本地解得开;而拿那种长 id 去 `GET /api/trade/search/<联赛>/<id>`
//! 换查询,服务端回 404。老式的短把手(`Rj3mL5Sw` 这种)则相反 —— 本地解不开,
//! 只能去服务端换。所以运行时的顺序是**先本地解,解不开再问服务端**,换回来的
//! 存进 `watch.sqlite` 的 `saved_queries`。[`encode_search_id`](一键做成蹲价)
//! 仍然只用在 PoE2 上:那条路要自己造一个 id,而 PoE1 会不会认没验证过。

use std::fmt;
use std::io::{Read, Write as _};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// PoE2 搜索页 URL 里固定不变的那一段,靠它把联赛和 id 从任意写法的链接里切出来。
const SEARCH_PATH_MARKER_POE2: &str = "/trade2/search/poe2/";

/// PoE1 的同一段。注意它**不是** PoE2 那一段的子串(`trade2/` ≠ `trade/`),
/// 所以两个 marker 谁先匹配都不会认错;先试 PoE2 只是为了读起来顺。
const SEARCH_PATH_MARKER_POE1: &str = "/trade/search/";

/// 哪一代流放之路。两代的交易站是两套接口(`/api/trade` 和 `/api/trade2`),
/// 联赛名却会撞车(`Standard` 和 `Hardcore` 两边都有),所以凡是拿联赛当键的
/// 地方都得把它拼进去。
///
/// 默认是 PoE2:这个程序原本只认 trade2,老的 `settings.json` 里没有这个键,
/// 读出来必须还是它原来的那一代。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Game {
    Poe1,
    #[default]
    Poe2,
}

impl Game {
    /// 库里、设置文件里、URL 里统一的写法。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Game::Poe1 => "poe1",
            Game::Poe2 => "poe2",
        }
    }

    /// 认不出来的一律当 PoE2:手改过的设置、老库里的空值,都不该让整条记录
    /// 读不出来 —— 退回默认的那一代最多是"这条搜索发错了接口",改回去就好。
    #[must_use]
    pub fn parse(raw: &str) -> Game {
        match raw.trim() {
            "poe1" => Game::Poe1,
            _ => Game::Poe2,
        }
    }

    /// 搜索页 URL 里"域名之后、联赛之前"的那一段。
    #[must_use]
    pub fn search_path_marker(self) -> &'static str {
        match self {
            Game::Poe1 => SEARCH_PATH_MARKER_POE1,
            Game::Poe2 => SEARCH_PATH_MARKER_POE2,
        }
    }
}

impl fmt::Display for Game {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次搜索的最小引用:哪一代游戏 + 联赛名(人类可读、未编码)+ 搜索 id。
///
/// 联赛在这里存解码后的原文(`Forbidden Rites`),要拼 URL 时再编码,
/// 这样存进 `settings.json` 的内容和网页上看到的一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRef {
    /// 老的 `settings.json` 里没有这个键 —— 缺了就是 PoE2,那是这个程序
    /// 一开始唯一认识的那一代。
    #[serde(default)]
    pub game: Game,
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

    // 两个 marker 挨个试。谁先谁后其实无所谓 —— `trade2/search` 里不含
    // `trade/search`,一条链接只可能命中其中一个。
    for game in [Game::Poe2, Game::Poe1] {
        if let Some((head, tail)) = path.split_once(game.search_path_marker()) {
            if !host_looks_sane(head) {
                return None;
            }
            return parse_url_tail(game, tail);
        }
    }

    if is_search_id(path) {
        Some(SearchRef {
            game: Game::default(),
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
        "https://www.pathofexile.com{}{}/{}",
        r.game.search_path_marker(),
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
///
/// PoE2 的 id 一定解得开;PoE1 的看形状(见模块头注释):自描述的长 id 解得开,
/// 老式短把手只会得到 [`SearchIdError::NotBase64`] / [`SearchIdError::NotGzip`],
/// 那时候才去服务端换。
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

/// 查询 JSON → 搜索 id,[`decode_search_id`] 的逆:先 gzip,再 base64url(不填 `=`)。
/// 同样**只对 PoE2 成立** —— PoE1 的 id 只有服务端发得出来。
///
/// 为什么要在本地造 id:交易站没有"帮我存一条查询"的接口(见本文件开头),
/// 而"一键蹲价"要做的正是**在观察自己的查询上多加一格词缀筛选,再变成一条
/// 新搜索**。压出来的字节和网页压的不一定一模一样(压缩级别是我们自己的),
/// 但解开之后是同一段 JSON —— 交易站认的就是这个。
///
/// 不填 `=`:id 要粘进地址栏,而填充号在 URL 里会被转义或截断。
pub fn encode_search_id(query_json: &str) -> String {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(query_json.as_bytes())
        .expect("writing into a Vec never fails");
    let raw = encoder.finish().expect("finishing a Vec never fails");
    encode_base64_url(&raw)
}

/// 在查询上多加一格"必须带这条词缀",其余条件原样保留。
///
/// 交易站的词缀条件放在 `stats`:一个数组,每一项是一组(`{"type":"and",
/// "filters":[…]}`)。网页上没加过任何词缀条件的查询里可能整段都没有,
/// 所以缺了就自己建一组。
///
/// 同一个 id 不重复加:交易站会把两格一样的筛选当成"这条词缀要有两次",
/// 于是搜出来一件都没有。
///
/// id 的写法是 **`explicit.stat_1671376347`**(库里存的 `hash` 去掉开头那个
/// `stat.`)。2026-09-09 匿名跑过一趟核实:把这一格加在"Choir of the Storm"
/// 上再压成搜索 id,服务端答 200、`total 25`,回的每一件都带着那条词缀。
///
/// 和 [`with_sort`] / [`with_seller_filter`] 不一样,这里返回的是**查询本身**
/// 而不是请求体 —— 它下一步要交给 [`encode_search_id`] 压成一条搜索 id,
/// 而搜索 id 里装的就是查询本身,外面没有 `query` 那一层。
pub fn with_stat_filter(query_json: &str, stat_id: &str) -> String {
    let mut query = query_part(query_json);
    if !query.is_object() {
        query = json!({});
    }
    let stats = query
        .as_object_mut()
        .expect("just made it an object")
        .entry("stats".to_string())
        .or_insert_with(|| json!([]));
    if !stats.is_array() {
        *stats = json!([]);
    }
    let groups = stats.as_array_mut().expect("just made it an array");
    if groups.is_empty() {
        groups.push(json!({ "type": "and", "filters": [] }));
    }
    if !groups[0].is_object() {
        groups[0] = json!({ "type": "and", "filters": [] });
    }
    let filters = groups[0]
        .as_object_mut()
        .expect("just made it an object")
        .entry("filters".to_string())
        .or_insert_with(|| json!([]));
    if !filters.is_array() {
        *filters = json!([]);
    }
    let list = filters.as_array_mut().expect("just made it an array");
    let already_there = list
        .iter()
        .any(|entry| entry.get("id").and_then(Value::as_str) == Some(stat_id));
    if !already_there {
        list.push(json!({ "id": stat_id }));
    }
    query.to_string()
}

/// 一组词缀条件的口径:那一组里的每一条都要有,还是凑够几条就行。
///
/// 交易站的 `stats` 是一个数组,每一项是一"组",而组自己带一个 `type`:
/// `and` 是全都要,`count` 配 `{"value":{"min":N}}` 是"这组里至少中 N 条"。
/// 碑牌最多只带两条后缀,而值钱的后缀有四五种 —— 那种货天生就是
/// "这四条里凑够两条",不是"这四条都要"(那样一件都搜不出来)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatMatch {
    #[default]
    All,
    AtLeast(u32),
}

/// 在查询上**新加一组**词缀条件,原有的组一个字不动。
///
/// 为什么是新加一组而不是往第一组里塞([`with_stat_filter`] 做的那样):
/// 观察自己那条查询里的那一组是用户在网页上筛好的("这是什么货"),而这里
/// 加的是"我这次要它带哪几条词缀" —— 塞进同一组就会和人家的条件搅在一起,
/// 而 `count` 那种口径更是只对整组成立。
///
/// 组内去重:交易站把两格一样的筛选当成"这条词缀要有两次",于是搜出来一件
/// 都没有。要求的条数两头夹回 `1..=id 个数` —— 比篮子还多的条数永远凑不齐,
/// 而 0 条等于没筛。
///
/// 一条 id 都没有时一组都不加:空的 `count` 组一件都搜不出来,而空的 `and`
/// 组是句废话,两种都不如原样把查询交回去。
pub fn with_stat_group(query_json: &str, stat_ids: &[String], matching: StatMatch) -> String {
    let mut query = query_part(query_json);
    if !query.is_object() {
        query = json!({});
    }
    // 保序去重:篮子里的先后顺序就是用户加进去的顺序,读起来比排序过的顺手。
    let mut ids: Vec<&str> = Vec::new();
    for id in stat_ids {
        if !ids.contains(&id.as_str()) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return query.to_string();
    }
    let filters: Vec<Value> = ids.iter().map(|id| json!({ "id": id })).collect();
    let group = match matching {
        StatMatch::All => json!({ "type": "and", "filters": filters }),
        StatMatch::AtLeast(min) => {
            let count = u32::try_from(ids.len()).unwrap_or(u32::MAX);
            json!({ "type": "count", "value": { "min": min.clamp(1, count) }, "filters": filters })
        }
    };
    let stats = query
        .as_object_mut()
        .expect("just made it an object")
        .entry("stats".to_string())
        .or_insert_with(|| json!([]));
    if !stats.is_array() {
        *stats = json!([]);
    }
    stats
        .as_array_mut()
        .expect("just made it an array")
        .push(group);
    query.to_string()
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

/// 把查询 JSON 包成 search 接口的请求体,排序固定按价格升序 —— 蹲价只关心最便宜那几件。
pub fn search_request_body(query_json: &str) -> String {
    with_sort(query_json, "price", "asc")
}

/// 同上,但排序键由调用方指定:市场观察要的是"最新挂上来的 100 条"
/// (`with_sort(q, "indexed", "desc")`),不是"最便宜的 100 条"。
///
/// 走 `serde_json::Value` 拼装(而不是字符串拼接),保证出去的一定是合法 JSON;
/// 万一传进来的不是 JSON,退化成 `"query": null`,让服务端去报错而不是这里 panic。
///
/// 两种输入形状都认:搜索 id 解出来的是查询本身,而请求体是把它包在 `query`
/// 里的。后者还可能自带一个 `sort` —— 我们只取 `query` 那一半,原来的排序
/// 自然就被换掉了,这正是"替换或插入"想要的效果。
pub fn with_sort(query_json: &str, field: &str, direction: &str) -> String {
    json!({ "query": query_part(query_json), "sort": { field: direction } }).to_string()
}

/// 在查询上加一格"只看这个卖家",其余条件原样保留。
///
/// 交易站把卖家筛选放在 `filters.trade_filters.filters.account.input`(网页上
/// 那个 Seller 输入框)。市场观察靠它区分"这单卖掉了"和"卖家整批撤了" ——
/// 一条挂单消失时回头查一次卖家,他其它单还在就更像是卖掉了。
pub fn with_seller_filter(query_json: &str, account: &str) -> String {
    let mut query = query_part(query_json);
    let trade_filters = object_entry(
        object_entry(object_entry(&mut query, "filters"), "trade_filters"),
        "filters",
    );
    *object_entry(trade_filters, "account") = json!({ "input": account });
    json!({ "query": query, "sort": { "price": "asc" } }).to_string()
}

/// 取"查询"那一半:外面套着 `query` 就往里走一步,没套就是它自己。
fn query_part(query_json: &str) -> Value {
    let value: Value = serde_json::from_str(query_json).unwrap_or(Value::Null);
    match value.get("query") {
        Some(inner) if inner.is_object() => inner.clone(),
        _ => value,
    }
}

/// 往对象里挖出(必要时建出)一个子对象,好把新格子塞进去而不动别的键。
fn object_entry<'a>(parent: &'a mut Value, key: &str) -> &'a mut Value {
    if !parent.is_object() {
        *parent = json!({});
    }
    parent
        .as_object_mut()
        .expect("just replaced it with an object")
        .entry(key.to_string())
        .or_insert_with(|| json!({}))
}

/// URL 里 marker 之前那一截必须像个主机名(可带 scheme),否则一段随手粘来的
/// 文字只要碰巧含有搜索路径就会被当成链接。
fn host_looks_sane(head: &str) -> bool {
    let host = head.split_once("://").map_or(head, |(_, rest)| rest);
    host.is_empty() || (!host.contains('/') && !host.chars().any(char::is_whitespace))
}

/// marker 之后应当是 `<联赛>/<id>`,最多再跟一个 `live`。
fn parse_url_tail(game: Game, tail: &str) -> Option<SearchRef> {
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
        game,
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

/// base64url 的字母表(最后两位是 `-_`,不是 `+/`)。编码只用这一套:
/// 出去的 id 要能直接粘进地址栏。
const BASE64_URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// 手写的 base64url 编码,不填 `=`。理由同解码那一份:domain 层不为几十行
/// 引一个 crate,而这里的输出只有一个去处(交易站的搜索 id)。
fn encode_base64_url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let block = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        // 3 字节写 4 个字符,2 字节写 3 个,1 字节写 2 个 —— 剩下的位是补的 0,
        // 不该写出来(写出来就得靠 `=` 说明"那几位不算数")。
        for shift in [18, 12, 6, 0].into_iter().take(chunk.len() + 1) {
            out.push(BASE64_URL_ALPHABET[((block >> shift) & 63) as usize] as char);
        }
    }
    out
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
            game: Game::Poe2,
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

    /// PoE1 的链接少一段 `poe2/`、多一个字符的差别(`trade` 而不是 `trade2`),
    /// 但它是**另一套接口**,所以粘进来的那一刻就得认出来是哪一代。
    #[test]
    fn a_poe1_url_is_recognised_as_poe1() {
        let r = parse("https://www.pathofexile.com/trade/search/Standard/Rj3mL5Sw").unwrap();
        assert_eq!(r.game, Game::Poe1);
        assert_eq!(r.league, "Standard");
        assert_eq!(r.search_id, "Rj3mL5Sw");

        // 和 PoE2 那边一样宽容:没有 scheme、联赛百分号编码、结尾 `/live` 都认。
        let r =
            parse("www.pathofexile.com/trade/search/Hardcore%20Settlers/Rj3mL5Sw/live").unwrap();
        assert_eq!(r.game, Game::Poe1);
        assert_eq!(r.league, "Hardcore Settlers");

        // PoE2 的链接绝不能被认成 PoE1:`trade2/search` 里没有 `trade/search`。
        let r = parse("https://www.pathofexile.com/trade2/search/poe2/Standard/abcd1234").unwrap();
        assert_eq!(r.game, Game::Poe2);
    }

    /// 光秃秃一个 id 认不出代数,只能沿用这个程序原来那一代(PoE2)——
    /// 猜错的后果是发到另一套接口上,而用户随时可以改粘完整链接。
    #[test]
    fn a_bare_id_stays_on_poe2() {
        assert_eq!(parse("  H4sIAAAA-_09  ").unwrap().game, Game::Poe2);
        assert_eq!(parse("Rj3mL5Sw").unwrap().game, Game::Poe2);
    }

    /// 两代各回各的页面地址,而且都能被自己再读回来。
    #[test]
    fn page_urls_follow_the_game() {
        let poe1 = SearchRef {
            game: Game::Poe1,
            league: "Standard".to_string(),
            search_id: "Rj3mL5Sw".to_string(),
        };
        assert_eq!(
            search_page_url(&poe1),
            "https://www.pathofexile.com/trade/search/Standard/Rj3mL5Sw"
        );
        assert_eq!(
            live_page_url(&poe1),
            "https://www.pathofexile.com/trade/search/Standard/Rj3mL5Sw/live"
        );
        assert_eq!(parse(&search_page_url(&poe1)).unwrap(), poe1);
        assert_eq!(parse(&live_page_url(&poe1)).unwrap(), poe1);
    }

    /// 存进 `settings.json` 的写法是 `"poe1"` / `"poe2"`,而**缺这个键**
    /// 读出来是 PoE2 —— 盘上已经有的那份文件里一条都没写。
    #[test]
    fn the_game_serialises_as_a_plain_word_and_defaults_to_poe2() {
        assert_eq!(
            serde_json::to_string(&Game::Poe1).unwrap(),
            r#""poe1""#.to_string()
        );
        assert_eq!(Game::default(), Game::Poe2);
        assert_eq!(Game::Poe1.to_string(), "poe1");
        assert_eq!(Game::parse("poe1"), Game::Poe1);
        assert_eq!(Game::parse("whatever"), Game::Poe2, "认不出来就退回默认");

        let old: SearchRef =
            serde_json::from_str(r#"{"league":"Standard","search_id":"abcd1234"}"#).unwrap();
        assert_eq!(old.game, Game::Poe2);
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

    /// 市场观察要"最新的 100 条",所以排序键得能换,查询本身一个字不能动。
    #[test]
    fn a_custom_sort_replaces_the_price_sort_and_keeps_the_query() {
        let json = decode_search_id(FIXTURE_ID).unwrap();
        let body: Value = serde_json::from_str(&with_sort(&json, "indexed", "desc")).unwrap();
        assert_eq!(body["sort"], json!({ "indexed": "desc" }));
        assert!(body["sort"].get("price").is_none(), "价格排序被换掉了");
        assert_eq!(body["query"]["name"], "Choir of the Storm");
        assert_eq!(body["query"]["status"]["option"], "online");
    }

    /// 请求体形状的输入(外面套着 `query`、自带一个 `sort`)也得认:
    /// 不然会拼出 `{"query":{"query":…}}` 这种服务端读不懂的东西。
    #[test]
    fn a_request_shaped_input_is_unwrapped_before_the_sort_is_swapped() {
        let body: Value = serde_json::from_str(&with_sort(
            r#"{"query":{"name":"Mageblood"},"sort":{"price":"asc"}}"#,
            "indexed",
            "desc",
        ))
        .unwrap();
        assert_eq!(body["query"], json!({ "name": "Mageblood" }));
        assert_eq!(body["sort"], json!({ "indexed": "desc" }));
    }

    /// 卖家筛选塞在 `filters.trade_filters.filters.account.input`,
    /// 查询里原有的条件(名字、词缀、别的 filters)一个都不能掉。
    #[test]
    fn a_seller_filter_is_added_without_dropping_the_rest_of_the_query() {
        let body: Value = serde_json::from_str(&with_seller_filter(
            r#"{"name":"Choir of the Storm","filters":{"type_filters":{"filters":{"ilvl":{"min":68}}}}}"#,
            "山箏#5319",
        ))
        .unwrap();
        let query = &body["query"];
        assert_eq!(query["name"], "Choir of the Storm");
        assert_eq!(
            query["filters"]["type_filters"]["filters"]["ilvl"]["min"],
            68
        );
        assert_eq!(
            query["filters"]["trade_filters"]["filters"]["account"],
            json!({ "input": "山箏#5319" })
        );
        assert_eq!(body["sort"], json!({ "price": "asc" }));
    }

    /// 查询里本来就有 `trade_filters`(比如"只看在线卖家")的时候,
    /// 只加 account 这一格,兄弟格子留着。
    #[test]
    fn an_existing_trade_filter_keeps_its_siblings() {
        let body: Value = serde_json::from_str(&with_seller_filter(
            r#"{"filters":{"trade_filters":{"filters":{"price":{"max":5}}}}}"#,
            "Seller#1234",
        ))
        .unwrap();
        let trade = &body["query"]["filters"]["trade_filters"]["filters"];
        assert_eq!(trade["price"]["max"], 5);
        assert_eq!(trade["account"]["input"], "Seller#1234");
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

    /// 编码是解码的逆:出去的 id 得能被自己(以及交易站)读回同一段查询。
    ///
    /// 不比对 id 字符串本身 —— 同一段 JSON 用不同的压缩级别会压出不同的字节,
    /// 而交易站要的只是"解开之后是这段查询"。
    #[test]
    fn an_encoded_query_decodes_back_to_itself() {
        let id = encode_search_id(FIXTURE_QUERY);
        assert_eq!(decode_search_id(&id).unwrap(), FIXTURE_QUERY);
        // 交易站的 id 里没有填充号,粘进地址栏才不会被截断。
        assert!(!id.contains('='), "{id}");
        assert!(!id.contains('+') && !id.contains('/'), "{id}");
        // 网页上真抄下来的那个 id 解开再压回去,还是同一段查询。
        let decoded = decode_search_id(FIXTURE_ID).unwrap();
        assert_eq!(
            decode_search_id(&encode_search_id(&decoded)).unwrap(),
            decoded
        );
    }

    /// 一键蹲价:在观察自己的查询上多加一格词缀筛选,别的条件一个不动。
    #[test]
    fn a_stat_filter_is_appended_to_the_first_stat_group() {
        let query: Value =
            serde_json::from_str(&with_stat_filter(FIXTURE_QUERY, "explicit.stat_1671376347"))
                .unwrap();
        assert_eq!(query["name"], "Choir of the Storm", "原来的条件一个不能掉");
        assert_eq!(query["status"]["option"], "online");
        assert_eq!(
            query["stats"][0]["filters"],
            json!([{ "id": "explicit.stat_1671376347" }])
        );
        assert_eq!(query["stats"][0]["type"], "and");
        // 出来的是查询本身(不是请求体),因为它下一步要被压成搜索 id。
        assert!(query.get("query").is_none(), "{query}");
        assert_eq!(
            decode_search_id(&encode_search_id(&query.to_string())).unwrap(),
            query.to_string()
        );
    }

    /// 查询里根本没有 `stats` 那一段(网页上没加过词缀条件)时自己建一组,
    /// 而同一个 id 加两遍只留一格 —— 交易站会把重复的筛选当成"两条都要有"。
    #[test]
    fn a_missing_stat_group_is_created_and_an_id_is_never_added_twice() {
        let fresh: Value = serde_json::from_str(&with_stat_filter(
            r#"{"type":"Sapphire Ring"}"#,
            "explicit.a",
        ))
        .unwrap();
        assert_eq!(fresh["type"], "Sapphire Ring");
        assert_eq!(
            fresh["stats"],
            json!([{ "type": "and", "filters": [{ "id": "explicit.a" }] }])
        );

        let twice = with_stat_filter(&with_stat_filter(FIXTURE_QUERY, "explicit.a"), "explicit.a");
        let twice: Value = serde_json::from_str(&twice).unwrap();
        assert_eq!(
            twice["stats"][0]["filters"],
            json!([{ "id": "explicit.a" }])
        );

        // 已经有一格别的筛选时是追加,不是替换。
        let both = with_stat_filter(&with_stat_filter(FIXTURE_QUERY, "explicit.a"), "explicit.b");
        let both: Value = serde_json::from_str(&both).unwrap();
        assert_eq!(
            both["stats"][0]["filters"],
            json!([{ "id": "explicit.a" }, { "id": "explicit.b" }])
        );

        // 请求体形状的输入也认:只取 `query` 那一半,和 `with_sort` 一个规矩。
        let unwrapped: Value = serde_json::from_str(&with_stat_filter(
            r#"{"query":{"name":"Mageblood"},"sort":{"price":"asc"}}"#,
            "explicit.a",
        ))
        .unwrap();
        assert_eq!(unwrapped["name"], "Mageblood");
        assert!(unwrapped.get("sort").is_none(), "{unwrapped}");
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_string()).collect()
    }

    /// 一篮子词缀做成一条搜索,"全都要"就是一组 `and`。
    ///
    /// 新的一组追加在后面,而不是塞进第一组:第一组是观察自己那条查询带的
    /// (用户在网页上筛好的"这是什么货"),两件事不能搅在一起。
    #[test]
    fn a_basket_of_ids_becomes_one_and_group() {
        let query: Value = serde_json::from_str(&with_stat_group(
            FIXTURE_QUERY,
            &ids(&["explicit.a", "explicit.b"]),
            StatMatch::All,
        ))
        .unwrap();
        assert_eq!(query["name"], "Choir of the Storm", "原来的条件一个不能掉");
        assert_eq!(query["status"]["option"], "online");
        assert_eq!(query["stats"][0], json!({ "type": "and", "filters": [] }));
        assert_eq!(
            query["stats"][1],
            json!({
                "type": "and",
                "filters": [{ "id": "explicit.a" }, { "id": "explicit.b" }]
            })
        );
        // 出来的是查询本身(不是请求体),因为它下一步要被压成搜索 id。
        assert!(query.get("query").is_none(), "{query}");
    }

    /// "四条里凑够两条"是交易站的 `count` 组。碑牌最多只带两条后缀,而
    /// 认下的好后缀有四条 —— 写成 `and` 一件都搜不出来。
    #[test]
    fn at_least_n_becomes_a_count_group() {
        let query: Value = serde_json::from_str(&with_stat_group(
            FIXTURE_QUERY,
            &ids(&["explicit.a", "explicit.b", "explicit.c", "explicit.d"]),
            StatMatch::AtLeast(2),
        ))
        .unwrap();
        assert_eq!(query["stats"][1]["type"], "count");
        assert_eq!(query["stats"][1]["value"], json!({ "min": 2 }));
        assert_eq!(
            query["stats"][1]["filters"],
            json!([
                { "id": "explicit.a" },
                { "id": "explicit.b" },
                { "id": "explicit.c" },
                { "id": "explicit.d" }
            ])
        );
    }

    /// 要求的条数比篮子还多就永远凑不齐,0 条又等于没筛 —— 两头夹回来。
    /// 同一个 id 只留一格:重复的筛选会被当成"这条词缀要有两次"。
    #[test]
    fn the_minimum_is_clamped_and_the_ids_are_deduplicated() {
        let two = ids(&["explicit.a", "explicit.b"]);
        let too_many: Value =
            serde_json::from_str(&with_stat_group(FIXTURE_QUERY, &two, StatMatch::AtLeast(9)))
                .unwrap();
        assert_eq!(too_many["stats"][1]["value"], json!({ "min": 2 }));
        let none_at_all: Value =
            serde_json::from_str(&with_stat_group(FIXTURE_QUERY, &two, StatMatch::AtLeast(0)))
                .unwrap();
        assert_eq!(none_at_all["stats"][1]["value"], json!({ "min": 1 }));

        let duplicated = ids(&["explicit.a", "explicit.a", "explicit.b"]);
        let deduped: Value = serde_json::from_str(&with_stat_group(
            FIXTURE_QUERY,
            &duplicated,
            StatMatch::AtLeast(3),
        ))
        .unwrap();
        assert_eq!(
            deduped["stats"][1]["filters"],
            json!([{ "id": "explicit.a" }, { "id": "explicit.b" }])
        );
        assert_eq!(
            deduped["stats"][1]["value"],
            json!({ "min": 2 }),
            "去重之后只剩两条,要求的条数也得跟着降"
        );

        // 一条 id 都没有就一组都不加:空的 count 组一件都搜不出来。
        let empty: Value =
            serde_json::from_str(&with_stat_group(FIXTURE_QUERY, &[], StatMatch::AtLeast(2)))
                .unwrap();
        assert_eq!(empty["stats"].as_array().unwrap().len(), 1);
    }

    /// 查询里本来就有的那一组一个字不动:那是"这是什么货",而新的一组是
    /// "我要它带哪几条词缀"。
    #[test]
    fn an_existing_stat_group_is_left_alone() {
        let existing = r#"{"type":"Precursor Tablet","stats":[{"type":"and","filters":[{"id":"explicit.keep"}]}]}"#;
        let json = with_stat_group(existing, &ids(&["explicit.a"]), StatMatch::All);
        let query: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(query["type"], "Precursor Tablet");
        assert_eq!(query["stats"].as_array().unwrap().len(), 2, "{query}");
        assert_eq!(
            query["stats"][0]["filters"],
            json!([{ "id": "explicit.keep" }])
        );
        assert_eq!(
            query["stats"][1]["filters"],
            json!([{ "id": "explicit.a" }])
        );
        // 下一步就是压成搜索 id,压回来得还是同一段。
        assert_eq!(decode_search_id(&encode_search_id(&json)).unwrap(), json);
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
