//! `trade_probe` —— 交易站那条链路的手动验证探针。
//!
//! 它把生产代码里的每一环都串一遍,然后把中间结果原样打出来:
//! 搜索 id 本地解码 → 拼请求体 → 限速器算下次可请求时刻 → search →
//! 学限速头 → fetch 前 10 个 id → 摘要 → 判定。**没有一行判断逻辑是这里写的**,
//! 全是调 `pnd-domain` / `pnd-trade` 里的函数;探针自己抄一份逻辑,
//! 就等于验证了一个和生产不一样的东西。
//!
//! 用法:
//! ```text
//! # 一次性模式:自己走一遍 search → fetch → 判定,把每一步都印出来
//! cargo run -p pnd-runtime --bin trade_probe -- \
//!     --league "Forbidden Rites" --search <搜索URL或id> \
//!     [--cap 20 --currency divine] [--rounds 3] [--session <POESESSID>] \
//!     [--rates chaos=25.21,exalted=83.42]
//!
//! # 蹲价模式:起真的 actor(网关 + 轮询 + live + 判定 + 汇率线程),把事件流印出来
//! cargo run -p pnd-runtime --bin trade_probe -- \
//!     --watch --minutes 3 --poll-seconds 60 \
//!     --search <搜索URL或id> [--search <第二条>] [--cap 20 --currency divine] \
//!     [--hideout <alert_id>]
//!
//! # 观察模式:市场观察(Phase 3)开工前要核实的三条接口事实,一趟跑完
//! cargo run -p pnd-runtime --bin trade_probe -- \
//!     --observe --league "Forbidden Rites" --search <搜索URL或id>
//!
//! # 观察运行模式:起真的 actor 跑市场观察,把兜底轮询和回查都走一遍
//! cargo run -p pnd-runtime --bin trade_probe -- \
//!     --observe-run --search <搜索URL或id> --minutes 1 \
//!     [--live --session <POESESSID>] [--sample-every 1] \
//!     [--discover-seconds 300] [--recheck-seconds 3600]
//! ```
//!
//! `--observe-run` 和 `--watch` 一样,一行判定逻辑都没有:它造一份带一条
//! `ObservationEntry` 的设置、起 [`RuntimeHandle`]、把事件翻译成人话,最后
//! 直接读一遍库把攒下来的东西打出来(见过几条、没了几条、按词缀聚合的结果)。
//! 库开在临时文件里(actor 和探针各开一个连接读同一个文件),跑完就删掉 ——
//! 探针不该往用户真正的 `watch.sqlite` 里塞东西。
//!
//! 一分钟的窗口里等不到阶梯上最快的那一档(10 分钟),所以第一轮 discover
//! 一跑完,探针就发一条 `RecheckNow`:这样一趟就能看到"新挂单入库 → 回查 →
//! 判定"整条链路。花掉的请求是 1 次 search + 新面孔那几批 fetch + 回查那几批。
//!
//! 加上 `--live`(要 `--session`)就再开一条 WebSocket:挂单一上架就被推过来,
//! 当场去抓详情 —— 抓回来是 null 的那些就是"我们还没看它第一眼就被买走了",
//! 那批货正是这条功能想量的东西。不给 `--live` 就是原来那条只轮询的路。
//!
//! 蹲价模式每条状态行里都带着 live 那一头的档位(`live off` / `live connecting` /
//! `live up 42s` / `live retry #2 in 18s` / `live disabled: no session`)。
//! 没有 POESESSID 的机器上它就是 `live disabled: no session`,轮询照常跑 ——
//! 这正是"没会话也要能安静地退回轮询"的取证。
//!
//! `--hideout <alert_id>` 发一条 `TravelToHideout` 命令。同样,没有会话的
//! 机器上唯一能看到的是 `no session`,那就是预期输出。
//!
//! 蹲价模式里,探针**一行判定逻辑都没有**:它只是造一份内存里的 `AppSettings`、
//! 起一个 [`RuntimeHandle`],然后把收到的事件翻译成人话。界面将来做的事和
//! 这里一模一样,所以这个模式跑通,界面接线就只剩画画面了。
//!
//! `--session` 里的 POESESSID **永远不会被打印出来**,只会以"带会话/匿名"
//! 一个词的形式出现在输出里。

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{self, ExitCode};
use std::thread::sleep;
use std::time::{Duration, Instant};

use pnd_domain::{
    Currency, CurrencyRates, ListingSummary, ObservationId, Price, PriceCap, SearchRef, Verdict,
    decode_search_id, judge, parse_search_reference, search_page_url, search_request_body,
    with_seller_filter, with_sort,
};
use pnd_ninja::client::NinjaClient;
use pnd_runtime::actor::{
    HideoutOutcome, ObservationStatus, RuntimeCommand, RuntimeEvent, RuntimeHandle, RuntimePaths,
    WatchStatus,
};
use pnd_runtime::live_worker::{LiveOffReason, LiveRunState};
use pnd_runtime::{describe_token, now_secs};
use pnd_settings::{AppSettings, ObservationEntry, WatchEntry};
use pnd_storage::{ObservedListingRow, WatchStore};
use pnd_trade::client::{
    MAX_FETCH_IDS, SearchResponse, TradeClient, TradeResponse, ggg_error, parse_search_response,
};
use pnd_trade::listing::{parse_fetch_response, parse_fetch_response_by_id};
use pnd_trade::rate_limit::{
    Bucket, BucketUsage, FETCH_POLICY, RateHeaders, RateLimiter, SEARCH_POLICY,
};

/// 计划里定的 User-Agent:能认出是谁、留了联系方式。今天实测交易站接受它。
/// (以后这个字符串会从设置里来,可以切成浏览器样式;探针写死是为了少一个变量。)
const USER_AGENT: &str = concat!(
    "PoeNinjaData/",
    env!("CARGO_PKG_VERSION"),
    " (contact: soundmys1994@gmail.com)"
);

/// 两轮之间至少歇这么久。限速器算出来的等待通常更长,这只是个下限,
/// 免得在预算宽松的时候连着捅接口。
const MIN_ROUND_GAP_SECS: u64 = 5;

const USAGE: &str = "usage: trade_probe --search <url|id> [--league \"Forbidden Rites\"] \
[--cap 20] [--currency divine] [--rounds 1] [--session <POESESSID>] \
[--rates chaos=25.21,exalted=83.42]\n       \
trade_probe --watch --minutes 3 [--poll-seconds 60] --search <url|id> [--search <url|id>] \
[--cap 20] [--currency divine] [--session <POESESSID>] [--hideout <alert_id>]\n       \
trade_probe --observe --search <url|id> [--league \"Forbidden Rites\"]\n       \
trade_probe --observe-run --search <url|id> [--minutes 1] \
[--live --session <POESESSID>] [--sample-every 1] \
[--discover-seconds 300] [--recheck-seconds 3600]";

/// 蹲价模式里多久抽一次事件。事件通道是无界的,抽得慢只是屏幕上晚一点看到,
/// 不会丢。
const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("trade_probe: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    if args.observe_run {
        return run_observe_run(args);
    }
    if args.observe {
        return run_observe(args);
    }
    if args.watch {
        return run_watch(args);
    }
    let raw = &args.searches[0];
    if args.searches.len() > 1 {
        println!("(one-shot mode only uses the first --search; pass --watch to run several)");
    }
    let search_ref = parse_search_reference(raw, &args.league)
        .ok_or_else(|| format!("could not read a search reference out of {raw:?}"))?;
    let query_json = decode_search_id(&search_ref.search_id).map_err(|e| e.to_string())?;
    let request_body = search_request_body(&query_json);

    let rates = resolve_rates(args, &search_ref);
    let cap = args.cap();

    println!("league      {}", search_ref.league);
    println!("search id   {}", search_ref.search_id);
    println!("search page {}", search_page_url(&search_ref));
    println!(
        "session     {}",
        if args.session.is_some() {
            "yes (POESESSID sent, never printed)"
        } else {
            "no (anonymous)"
        }
    );
    match &cap {
        Some(cap) => println!("price cap   {}", cap.display()),
        None => println!("price cap   (none — every listing is reported as-is)"),
    }
    println!("rates       {}", describe_rates(&rates));
    println!("rounds      {}", args.rounds);

    let client = TradeClient::new(USER_AGENT.to_string());
    let mut limiter = RateLimiter::default();

    for round in 1..=args.rounds {
        println!(
            "\n================ round {round}/{} ================",
            args.rounds
        );
        if round == 1 {
            // 第一轮把解码出来的查询原样打出来:这是"粘进来的 id 到底是什么"
            // 的唯一证据,后面每轮发的都是同一个 body。
            println!("decoded query  {query_json}");
        }

        if round > 1 {
            // 两轮之间的间隔取"限速器说的"和"最少 5 秒"的大者。
            let wait = limiter_wait(&mut limiter, SEARCH_POLICY).max(MIN_ROUND_GAP_SECS);
            println!("sleeping {wait} s before the next round");
            sleep(Duration::from_secs(wait));
        }

        match one_round(
            &client,
            &mut limiter,
            args,
            &search_ref,
            &request_body,
            &rates,
            &cap,
        )? {
            RoundOutcome::Ok => {}
            RoundOutcome::RateLimited => {
                println!("\nstopped: the trade site answered 429, backing off for good.");
                return Err("rate limited (429)".to_string());
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------
// 观察模式:市场观察开工前要核实的三条接口事实
// ---------------------------------------------------------------------

/// 观察模式一趟最多花掉的请求数。
///
/// 主人的程序常年在同一个 IP 上跑着,和它共用服务端那份预算 —— 探针捅一次
/// 就是从他的额度里拿一次。正常一趟只花 2 次 search + 2 次 fetch,这两个上限
/// 是给"排序被拒了要换个方向再试"之类的岔路留的余量,超了就停,不硬跑。
const OBSERVE_MAX_SEARCHES: u32 = 4;
const OBSERVE_MAX_FETCHES: u32 = 4;

/// 花出去的请求数。带着它到处走,是为了让"还剩几次"这件事在屏幕上一直看得见。
struct Spend {
    searches: u32,
    fetches: u32,
}

/// `--observe`:一趟跑完 Phase 3 step 0 的三个问题,每个都把原始形状印出来。
///
/// 1. 服务端认不认 `sort: {"indexed": "desc"}`(观察要的是"最新 100 条",
///    不是"最便宜 100 条")
/// 2. 已经没了的挂单 id 去 fetch,回来长什么样(`null`?少一条?还是整个 404?)
/// 3. 按卖家账号筛选能不能用(将来用它区分"卖掉了"和"整批撤了")
///
/// 一行判断逻辑都不在这里:排序体、卖家筛选体、按 id 对号的解析全是
/// `pnd-domain` / `pnd-trade` 里的生产函数,探针只负责印。
fn run_observe(args: &Args) -> Result<(), String> {
    let raw = &args.searches[0];
    let search_ref = parse_search_reference(raw, &args.league)
        .ok_or_else(|| format!("could not read a search reference out of {raw:?}"))?;
    let query_json = decode_search_id(&search_ref.search_id).map_err(|e| e.to_string())?;

    println!("league        {}", search_ref.league);
    println!("search id     {}", search_ref.search_id);
    println!("search page   {}", search_page_url(&search_ref));
    println!(
        "session       {}",
        if args.session.is_some() {
            "yes (POESESSID sent, never printed)"
        } else {
            "no (anonymous)"
        }
    );
    println!(
        "budget        at most {OBSERVE_MAX_SEARCHES} searches + {OBSERVE_MAX_FETCHES} fetches"
    );
    println!("decoded query {query_json}");

    let client = TradeClient::new(USER_AGENT.to_string());
    let mut limiter = RateLimiter::default();
    let mut spend = Spend {
        searches: 0,
        fetches: 0,
    };

    // ---- 事实 1:排序键 -------------------------------------------------
    println!("\n=========== fact 1 — does the server take sort by `indexed`? ===========");
    let (direction, search) = observe_sort(
        &client,
        &mut limiter,
        &mut spend,
        args,
        &search_ref,
        &query_json,
    )?;
    println!(
        "\naccepted sort  {{\"indexed\": \"{direction}\"}}   total {}   returned {} ids",
        search.total,
        search.result.len()
    );
    let head: Vec<String> = search.result.iter().take(MAX_FETCH_IDS).cloned().collect();
    if head.is_empty() {
        return Err("the search returned no ids — nothing to observe".to_string());
    }
    for (index, id) in head.iter().enumerate() {
        println!("  [{}] {id}", index + 1);
    }

    // 排序到底生效没有,只有把这 10 条的上架时间拉出来才算数。
    println!(
        "\nfetching those {} ids to read their `indexed` stamps",
        head.len()
    );
    let response = observe_fetch(&client, &mut limiter, &mut spend, args, &head, &search.id)?;
    if !response.is_success() {
        return Err(format!(
            "fetch failed with status {}: {}",
            response.status,
            response.body_excerpt()
        ));
    }
    let pairs = parse_fetch_response_by_id(&head, &response.body).map_err(|e| e.to_string())?;
    let mut stamps: Vec<String> = Vec::new();
    for (index, (id, listing)) in pairs.iter().enumerate() {
        match listing {
            Some(listing) => {
                println!(
                    "  [{}] {}  {}  {}",
                    index + 1,
                    listing.indexed,
                    short_id(id),
                    listing.item_name
                );
                stamps.push(listing.indexed.clone());
            }
            None => println!("  [{}] (not in the answer)  {}", index + 1, short_id(id)),
        }
    }
    // RFC 3339 的 Z 时间戳按字典序比就是按时间比,所以直接比字符串。
    let newest_first = stamps.windows(2).all(|pair| pair[0] >= pair[1]);
    println!(
        "\nverdict: the stamps are {}",
        if newest_first {
            "newest-first (non-increasing) — the sort took effect"
        } else {
            "NOT in newest-first order — the server ignored the sort"
        }
    );

    // ---- 事实 2:已经没了的 id ------------------------------------------
    println!("\n=========== fact 2 — what does a gone listing id look like? ===========");
    let real = head[0].clone();
    let fake = fake_listing_id(&real);
    println!("real id  {real}");
    println!("fake id  {fake}   (same shape, last 4 characters bumped — cannot exist)");
    let response = observe_fetch(
        &client,
        &mut limiter,
        &mut spend,
        args,
        &[real.clone(), fake.clone()],
        &search.id,
    )?;
    print_fetch_shape(&response);
    if response.is_success() {
        let pairs = parse_fetch_response_by_id(&[real.clone(), fake.clone()], &response.body)
            .map_err(|e| e.to_string())?;
        println!("\nparse_fetch_response_by_id says:");
        for (id, listing) in &pairs {
            println!(
                "  {} -> {}",
                short_id(id),
                match listing {
                    Some(listing) => format!("Some({})", listing.short_label()),
                    None => "None (gone)".to_string(),
                }
            );
        }
    }

    // ---- 事实 3:卖家筛选 -----------------------------------------------
    println!("\n=========== fact 3 — is the seller filter accepted? ===========");
    let seller = pairs
        .iter()
        .find_map(|(_, listing)| listing.as_ref())
        .map(|listing| listing.account.clone())
        .filter(|account| !account.is_empty())
        .ok_or_else(|| "no seller name in the fetch answer — cannot test the filter".to_string())?;
    println!("seller        {seller}");
    let body = with_seller_filter(&query_json, &seller);
    println!("request body  {body}");
    let response = observe_search(&client, &mut limiter, &mut spend, args, &search_ref, &body)?;
    if response.is_success() {
        let filtered = parse_search_response(&response.body).map_err(|e| e.to_string())?;
        println!(
            "\nverdict: accepted — total {} (unfiltered total was {})",
            filtered.total, search.total
        );
    } else {
        // 说好了失败就不重试:这一格是可选的,不值得再花一次预算。
        println!("\nverdict: rejected — body: {}", response.body_excerpt());
        if let Some(message) = ggg_error(&response.body_text()) {
            println!("         {message}");
        }
    }

    println!(
        "\nspent {} searches + {} fetches",
        spend.searches, spend.fetches
    );
    Ok(())
}

/// 事实 1 的那次(或那两次)search。
///
/// 先试 `desc` —— 观察真正想要的就是它。被拒了才补一次 `asc`,只为一个目的:
/// 分清"这个排序键不认"和"这个方向不认"。
fn observe_sort(
    client: &TradeClient,
    limiter: &mut RateLimiter,
    spend: &mut Spend,
    args: &Args,
    search_ref: &SearchRef,
    query_json: &str,
) -> Result<(&'static str, SearchResponse), String> {
    for direction in ["desc", "asc"] {
        let body = with_sort(query_json, "indexed", direction);
        println!("\nrequest body  {body}");
        let response = observe_search(client, limiter, spend, args, search_ref, &body)?;
        if response.is_success() {
            return Ok((
                direction,
                parse_search_response(&response.body).map_err(|e| e.to_string())?,
            ));
        }
        println!("rejected — body: {}", response.body_excerpt());
        if let Some(message) = ggg_error(&response.body_text()) {
            println!("           {message}");
        }
        if response.status == 429 || response.looks_like_html {
            return Err(format!(
                "stopping: status {} looks like a block, not a bad sort key",
                response.status
            ));
        }
    }
    Err("the server refused both `indexed` directions".to_string())
}

/// 一次 search,带预算闸门和原样打印。
fn observe_search(
    client: &TradeClient,
    limiter: &mut RateLimiter,
    spend: &mut Spend,
    args: &Args,
    search_ref: &SearchRef,
    body: &str,
) -> Result<TradeResponse, String> {
    if spend.searches >= OBSERVE_MAX_SEARCHES {
        return Err(format!(
            "out of search budget ({OBSERVE_MAX_SEARCHES} used) — stopping instead of spending more"
        ));
    }
    wait_for_budget(limiter, SEARCH_POLICY, "search");
    let ticket = limiter.insert_request(SEARCH_POLICY, now_secs());
    let response = client
        .search(&search_ref.league, body, args.session.as_deref())
        .map_err(|e| e.to_string())?;
    spend.searches += 1;
    limiter.finish_request(ticket, response.rate.as_ref(), now_secs());
    println!(
        "POST search -> status {}   (search {}/{OBSERVE_MAX_SEARCHES})",
        response.status, spend.searches
    );
    print_rate_headers(response.rate.as_ref());
    print_budget(limiter, SEARCH_POLICY, "search");
    Ok(response)
}

/// 一次 fetch,带预算闸门和原样打印。
fn observe_fetch(
    client: &TradeClient,
    limiter: &mut RateLimiter,
    spend: &mut Spend,
    args: &Args,
    ids: &[String],
    search_id: &str,
) -> Result<TradeResponse, String> {
    if spend.fetches >= OBSERVE_MAX_FETCHES {
        return Err(format!(
            "out of fetch budget ({OBSERVE_MAX_FETCHES} used) — stopping instead of spending more"
        ));
    }
    wait_for_budget(limiter, FETCH_POLICY, "fetch");
    let ticket = limiter.insert_request(FETCH_POLICY, now_secs());
    let response = client
        .fetch(ids, search_id, args.session.as_deref())
        .map_err(|e| e.to_string())?;
    spend.fetches += 1;
    limiter.finish_request(ticket, response.rate.as_ref(), now_secs());
    println!(
        "GET fetch -> status {}   (fetch {}/{OBSERVE_MAX_FETCHES})",
        response.status, spend.fetches
    );
    print_rate_headers(response.rate.as_ref());
    print_budget(limiter, FETCH_POLICY, "fetch");
    Ok(response)
}

/// 把 fetch 回来的 `result` 数组按原样描述一遍:几条、每一格是 null 还是对象、
/// 对象上有哪些键。事实 2 要的就是这个形状本身,不是我们对它的理解。
fn print_fetch_shape(response: &TradeResponse) {
    let root: serde_json::Value = match serde_json::from_slice(&response.body) {
        Ok(root) => root,
        Err(error) => {
            println!("body is not JSON ({error}): {}", response.body_excerpt());
            return;
        }
    };
    let Some(entries) = root.get("result").and_then(serde_json::Value::as_array) else {
        println!("no `result` array — raw body: {}", response.body_excerpt());
        return;
    };
    println!("raw `result` array has {} entries:", entries.len());
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            serde_json::Value::Null => println!("  [{index}] null"),
            serde_json::Value::Object(map) => {
                let id = map
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(no id)");
                let keys: Vec<&str> = map.keys().map(String::as_str).collect();
                println!("  [{index}] object id={} keys={:?}", short_id(id), keys);
            }
            other => println!("  [{index}] {other}"),
        }
    }
}

/// 挂单 id 有 64 个字符,屏幕上摆一列全长的没法看,留头尾就够认。
fn short_id(id: &str) -> String {
    let count = id.chars().count();
    if count <= 20 {
        return id.to_string();
    }
    let head: String = id.chars().take(8).collect();
    let tail: String = id.chars().skip(count - 8).collect();
    format!("{head}…{tail}")
}

/// 造一个"形状对但不存在"的挂单 id:把最后 4 个十六进制字符各挪一位。
///
/// 为什么要自己造而不是拿一条真的等它消失:那得等上几小时,而这里要问的
/// 只是"服务端拿一个查不到的 id 怎么回话"。改动限制在最后 4 位,是为了让
/// 长度和字母表和真 id 完全一样 —— 不然可能只是撞上"id 格式不对"的报错。
fn fake_listing_id(id: &str) -> String {
    let mut chars: Vec<char> = id.chars().collect();
    let start = chars.len().saturating_sub(4);
    for ch in &mut chars[start..] {
        *ch = match ch.to_digit(16) {
            Some(value) => char::from_digit((value + 1) % 16, 16).expect("0..16 is a hex digit"),
            // 不是十六进制的字符(万一 id 换成 base64url 了)也得变一下,
            // 换成 '0' 一定和原字符不同 —— '0' 本身是十六进制字符。
            None => '0',
        };
    }
    chars.into_iter().collect()
}

// ---------------------------------------------------------------------
// 观察运行模式:开真的 actor 跑一轮 discover + 一轮 recheck
// ---------------------------------------------------------------------

/// `--observe-run`:把市场观察那条链路整个跑一遍。
///
/// 和 `--watch` 一样,这里一行判定逻辑都没有 —— 造设置、起 actor、印事件,
/// 最后直接读库把攒下来的东西摆出来。判定("这条挂单是卖掉了还是撤了")、
/// 聚合、词缀模板全是生产代码算的。
fn run_observe_run(args: &Args) -> Result<(), String> {
    let raw = &args.searches[0];
    let search_ref = parse_search_reference(raw, &args.league)
        .ok_or_else(|| format!("could not read a search reference out of {raw:?}"))?;
    let query_json = decode_search_id(&search_ref.search_id).map_err(|e| e.to_string())?;
    let label = label_from_query(&query_json, &search_ref);

    let mut settings = AppSettings {
        league: search_ref.league.clone(),
        poesessid: args.session.clone().unwrap_or_default(),
        ..AppSettings::default()
    };
    let mut entry = ObservationEntry::new(label.clone(), &search_ref);
    entry.discover_interval_secs = args.discover_seconds;
    entry.recheck_interval_secs = args.recheck_seconds;
    entry.sample_every = args.sample_every;
    let obs_id = entry.id.clone();
    settings.observations.push(entry);
    // 没给 --live 就一条 WebSocket 都不开:观察只要有会话就会自己去连,
    // 而这条探针跑的是哪条路,应当由命令行说了算,不该由"你恰好粘了个
    // cookie"决定。额度设成 0 是关掉它最诚实的写法 —— 走的还是生产那条路。
    if !args.live {
        settings.watcher.max_live_connections = 0;
    }
    // 几个下限由它兜住(300 / 3600 秒、采样至少 1),所以印出来的数就是
    // 真正会用的数。
    settings.normalize();

    // 库开在临时文件而不是内存里:actor 和探针各开一个连接读同一个文件,
    // 跑完探针才能把攒下来的东西读出来给人看。
    let db_path = std::env::temp_dir().join(format!("pnd-observe-probe-{}.sqlite", process::id()));
    let _ = std::fs::remove_file(&db_path);

    println!("league        {}", settings.league);
    println!("search id     {}", search_ref.search_id);
    println!("search page   {}", search_page_url(&search_ref));
    println!(
        "session       {}",
        if settings.poesessid.is_empty() {
            "no (anonymous)"
        } else {
            "yes (POESESSID sent, never printed)"
        }
    );
    println!("observation   {label}   ({obs_id})");
    println!(
        "discover      every {} s   (1 search + fetches for ids we have not seen)",
        settings.observations[0].discover_interval_secs
    );
    println!(
        "recheck       ladder +10m/+30m/+2h/+6h/+1d/+3d, then every {} s until 7 days",
        settings.observations[0].recheck_interval_secs
    );
    println!(
        "live          {}",
        if args.live {
            "yes (one WebSocket for this observation; pushed ids are fetched at once)"
        } else {
            "no (--live is off, so this run is backstop-poll only)"
        }
    );
    println!(
        "sample        every {} pushed listing(s)",
        settings.observations[0].sample_every
    );
    println!("database      {}", db_path.display());
    println!("running for   {} minutes\n", args.minutes);

    let handle = RuntimeHandle::start(settings, RuntimePaths::new(db_path.clone()))
        .map_err(|error| error.to_string())?;

    let labels: BTreeMap<String, String> =
        [(obs_id.to_string(), label.clone())].into_iter().collect();
    let deadline = Instant::now() + Duration::from_secs(args.minutes * 60);
    // 回查现在是每条挂单自己的阶梯,最快的一档也要 10 分钟,一分钟的窗口里
    // 等不到。第一轮 discover 一落地就手动排一次 —— 这条命令正是界面上那个
    // "立刻回查"按钮发的东西(把在册的挂单全部推到此刻到期)。
    let mut recheck_asked = false;
    while Instant::now() < deadline {
        match handle.try_next_event() {
            Some(event) => {
                if !recheck_asked && matches!(event, RuntimeEvent::ObservationChanged { .. }) {
                    recheck_asked = true;
                    println!(
                        "{} observe   the first discover landed — asking for a recheck now",
                        stamp()
                    );
                    handle
                        .try_send(RuntimeCommand::RecheckNow {
                            obs_id: obs_id.clone(),
                        })
                        .map_err(|error| error.to_string())?;
                }
                print_event(&event, &labels);
            }
            None => sleep(DRAIN_INTERVAL),
        }
    }
    // actor 先走干净,再去读它写的那个库。
    drop(handle);
    println!("\n{} observe window finished.", stamp());

    let report = observation_report(&db_path, &obs_id);
    // 库故意留在原地:聚合表要是空的,里面存着的物品原文就是唯一的线索。
    // 看完自己删掉就行(还有 -wal / -shm 两个附属文件)。
    println!(
        "
database kept at {}",
        db_path.display()
    );
    report
}

/// 跑完之后读一遍库:这条观察到底攒下了什么。
fn observation_report(db_path: &Path, obs_id: &ObservationId) -> Result<(), String> {
    let store = WatchStore::open(db_path).map_err(|error| error.to_string())?;
    let summary = store
        .observation_summary(obs_id)
        .map_err(|error| error.to_string())?;
    println!(
        "\n=========== what this observation has on file ===========\n\
         active {}   gone {}   (sold_likely {} · sold_after_cuts {} · unknown {})",
        summary.active, summary.gone, summary.sold_likely, summary.sold_after_cuts, summary.unknown
    );

    let gone = store
        .recent_gone(obs_id, 10)
        .map_err(|error| error.to_string())?;
    println!("\nrecently gone ({}):", gone.len());
    for row in &gone {
        println!("  {}", describe_observed(row));
    }
    if gone.is_empty() {
        println!("  (none — nothing vanished between the discover and the recheck)");
    }

    let active = store
        .oldest_active(obs_id, 10)
        .map_err(|error| error.to_string())?;
    println!("\nlongest listed ({} shown):", active.len());
    for row in &active {
        println!("  {}", describe_observed(row));
    }

    // 词缀是从物品原文(`item` 那一块)里读出来的,而那一块真长什么样,只有
    // 对着真交易站跑一次才看得见。聚合表空着的时候,下面这几行是唯一能回答
    // "为什么空"的东西:原文存下来没有、里面有哪些键、从里面读出了几条词缀。
    if let Some(row) = active.first().or_else(|| gone.first()) {
        let mods = store
            .observed_mods(obs_id, &row.listing_id)
            .map_err(|error| error.to_string())?;
        let keys = match serde_json::from_str::<serde_json::Value>(&row.item_json) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect::<Vec<_>>(),
            Ok(_) => vec!["(the item block is not an object)".to_string()],
            Err(_) => vec!["(no item block was stored)".to_string()],
        };
        println!(
            "
first item block: {} bytes, {} modifier rows read out of it",
            row.item_json.len(),
            mods.len()
        );
        println!("  keys: {}", keys.join(", "));
        for entry in mods.iter().take(8) {
            println!("  {:<10}{}", entry.mod_kind, entry.template);
        }
    }

    let aggregate = store
        .mod_aggregate(obs_id, 1)
        .map_err(|error| error.to_string())?;
    println!(
        "\nby modifier ({} templates, min 1 sample):",
        aggregate.len()
    );
    println!(
        "  {:<44}{:<12}{:<6}{:<6}{:<6}{:<14}{:<14}alive",
        "template", "kind", "seen", "gone", "sold", "median gone", "median listed"
    );
    for outcome in aggregate.iter().take(20) {
        println!(
            "  {:<44}{:<12}{:<6}{:<6}{:<6}{:<14}{:<14}{}",
            truncate(&outcome.template, 42),
            outcome.mod_kind,
            outcome.seen,
            outcome.gone,
            outcome.sold_likely,
            describe_milli(outcome.median_gone_price_milli, &outcome.currency),
            describe_milli(outcome.median_active_price_milli, &outcome.currency),
            outcome
                .median_hours_alive
                .map_or_else(|| "-".to_string(), |hours| format!("{hours:.1} h"))
        );
    }
    Ok(())
}

/// 一条观察到的挂单,一行说清:价格轨迹、活了多久、判成了什么。
fn describe_observed(row: &ObservedListingRow) -> String {
    let price = |price: &Option<Price>| {
        price
            .as_ref()
            .map_or_else(|| "no price".to_string(), Price::display)
    };
    let track = if row.price_changes == 0 {
        price(&row.last_price)
    } else {
        format!(
            "{} → {} ({} changes)",
            price(&row.first_price),
            price(&row.last_price),
            row.price_changes
        )
    };
    let verdict = match row.gone_class {
        Some(class) => format!("  gone: {}", class.as_str()),
        None => String::new(),
    };
    format!(
        "{}  {}  {}  seen for {} min{verdict}",
        short_id(&row.listing_id),
        row.item_name,
        track,
        row.observed_lifetime_secs() / 60
    )
}

fn describe_milli(milli: Option<i64>, currency: &str) -> String {
    match milli {
        Some(milli) => Price::new(milli, Currency::parse(currency)).display(),
        None => "-".to_string(),
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    text.chars().take(width - 1).chain(['…']).collect()
}

// ---------------------------------------------------------------------
// 蹲价模式:开真的 actor,只负责把事件翻译成人话
// ---------------------------------------------------------------------

/// `--watch`:造一份内存里的设置,起 actor,盯 N 分钟。
///
/// 库开在内存里(`RuntimePaths::in_memory`),所以跑完什么都不留 ——
/// 这是探针,不该往用户真正的提醒历史里塞测试数据。
fn run_watch(args: &Args) -> Result<(), String> {
    let cap = args
        .cap()
        .unwrap_or_else(|| Price::new(0, Currency::parse(&args.currency)));
    let mut settings = AppSettings {
        league: args.league.clone(),
        poesessid: args.session.clone().unwrap_or_default(),
        ..AppSettings::default()
    };
    settings.watcher.poll_interval_seconds = args.poll_seconds;

    // 搜索列表和"哪个 uuid 是哪条搜索"的对照表一起建:事件里只带 WatchId,
    // 屏幕上要显示的是人能认出来的标签。
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    for raw in &args.searches {
        let search_ref = parse_search_reference(raw, &args.league)
            .ok_or_else(|| format!("could not read a search reference out of {raw:?}"))?;
        let query_json = decode_search_id(&search_ref.search_id).map_err(|e| e.to_string())?;
        let label = label_from_query(&query_json, &search_ref);
        let entry = WatchEntry::new(label.clone(), &search_ref, cap.clone());
        labels.insert(entry.id.to_string(), label);
        settings.watches.push(entry);
    }
    settings.normalize();

    println!("league       {}", settings.league);
    println!(
        "session      {}",
        if settings.poesessid.is_empty() {
            "no (anonymous)"
        } else {
            "yes (POESESSID sent, never printed)"
        }
    );
    println!("price cap    {}", cap.display());
    if cap.amount_milli <= 0 {
        println!("             (a cap of 0 can never be hit — pass --cap to see ListingMatched)");
    }
    println!(
        "poll every   {} s   (watch database: in memory, nothing is kept)",
        settings.watcher.poll_interval_seconds
    );
    for entry in &settings.watches {
        println!("watch        {} · {}", entry.label, entry.search_id);
    }
    println!("running for  {} minutes\n", args.minutes);

    let handle = RuntimeHandle::start(settings, RuntimePaths::in_memory())
        .map_err(|error| error.to_string())?;

    // `--hideout` 是给"点一次去藏身处"这条链路取证用的。库开在内存里,
    // 所以这个号一般查不到东西 —— 没有会话的机器上会先撞上 NoSession,
    // 那正是这台机器上的预期输出。
    if let Some(alert_id) = args.hideout {
        println!("{} hideout   asking for alert {alert_id}", stamp());
        handle
            .try_send(RuntimeCommand::TravelToHideout { alert_id })
            .map_err(|error| error.to_string())?;
    }

    let deadline = Instant::now() + Duration::from_secs(args.minutes * 60);
    while Instant::now() < deadline {
        match handle.try_next_event() {
            Some(event) => print_event(&event, &labels),
            None => sleep(DRAIN_INTERVAL),
        }
    }
    // 关机的路上还会掉出几个事件(最后一轮的状态),一并抽干再走。
    drop(handle);
    println!("\n{} watch window finished.", stamp());
    Ok(())
}

/// 一行事件。时间戳是本地时钟,方便和游戏里、网页上看到的时间对上。
fn print_event(event: &RuntimeEvent, labels: &BTreeMap<String, String>) {
    let at = stamp();
    match event {
        RuntimeEvent::Ready => println!("{at} ready"),
        RuntimeEvent::RatesUpdated(rates) => println!("{at} rates     {}", describe_rates(rates)),
        RuntimeEvent::WatchStatus { watch_id, status } => {
            println!(
                "{at} watch     {} {}",
                labels
                    .get(watch_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| watch_id.to_string()),
                describe_status(status)
            );
        }
        RuntimeEvent::Budget {
            policy,
            usage,
            next_allowed_in_secs,
        } => {
            println!(
                "{at} budget    {policy}: {}{}",
                describe_budget(usage),
                match next_allowed_in_secs {
                    Some(secs) => format!("  next allowed in {secs}s"),
                    None => String::new(),
                }
            );
        }
        RuntimeEvent::HideoutResult { alert_id, outcome } => {
            println!(
                "{at} hideout   alert {alert_id}: {}",
                describe_hideout(outcome)
            );
        }
        RuntimeEvent::ListingMatched(matched) => {
            let listing = &matched.headline;
            println!(
                "{at} MATCH     {} · {} · {} ({}){}  [cap {}]",
                matched.label,
                listing
                    .price
                    .as_ref()
                    .map_or_else(|| "no price".to_string(), Price::display),
                listing.item_name,
                listing.account,
                if matched.extra > 0 {
                    format!("  +{} more", matched.extra)
                } else {
                    String::new()
                },
                matched.cap.display()
            );
            println!("{at}           whisper: {}", listing.whisper);
            // "去藏身处"能不能成,第一个问题就是这张票还活着没有。印的是
            // 生产代码算出来的那句话(`pnd_runtime::describe_token`),
            // token 本身一个字符都不出现。
            println!(
                "{at}           hideout_token {}",
                describe_token(listing.hideout_token.as_deref(), now_secs())
            );
        }
        RuntimeEvent::SessionChecked { valid, detail } => {
            println!(
                "{at} session   {} — {detail}",
                if *valid {
                    "recognised"
                } else {
                    "not recognised"
                }
            );
        }
        RuntimeEvent::ObservationStatus { obs_id, status } => {
            println!(
                "{at} observe   {} {}",
                labels
                    .get(obs_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| obs_id.to_string()),
                describe_observation(status)
            );
        }
        RuntimeEvent::ObservationChanged { obs_id } => {
            println!(
                "{at} observe   {} — a cycle finished, the page would re-read the database now",
                labels
                    .get(obs_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| obs_id.to_string())
            );
        }
        RuntimeEvent::SessionInvalid => println!("{at} session   the POESESSID was rejected"),
        RuntimeEvent::CloudflareBlocked { until } => {
            println!(
                "{at} blocked   cloudflare — holding for {} s",
                (until - now_secs()).max(0)
            );
        }
        RuntimeEvent::Log(message) => println!("{at} log       {message}"),
        RuntimeEvent::Fault(message) => println!("{at} FAULT     {message}"),
    }
}

/// 一次"去藏身处"的结果翻译成人话。这个探针跑在没有 POESESSID 的机器上时,
/// 唯一能看到的就是 `NoSession` —— 那正是预期输出。
fn describe_hideout(outcome: &HideoutOutcome) -> String {
    match outcome {
        HideoutOutcome::Sent => "sent — the game should show a travel invite".to_string(),
        HideoutOutcome::NoSession => {
            "no session — /whisper needs a POESESSID in settings.json".to_string()
        }
        // 为什么没有 token(货没了 / 卖家离线 / 这次 fetch 没带 cookie)是一条
        // 单独的 `log` 事件,就印在这一行的上面 —— `TokenMissing` 本身带不了消息。
        HideoutOutcome::TokenMissing => {
            "no hideout_token for that listing — see the log line just above for why".to_string()
        }
        HideoutOutcome::Refreshed => "token was stale — fetched a fresh one, retrying".to_string(),
        // 状态码 0 的意思是"这封请求压根没上过网",不是"服务端回了 0"。
        // 说清楚这一点,不然屏幕上和卡片上一样看不出发生了什么。
        HideoutOutcome::Failed { status, message } => {
            let answered = if *status == 0 {
                "no HTTP answer".to_string()
            } else {
                format!("HTTP {status}")
            };
            format!("failed ({answered}) — {message}")
        }
    }
}

/// live 那一头的档位。`describe_status` 会把它接在轮询档位后面。
fn describe_live(state: LiveRunState) -> String {
    let now = now_secs();
    match state {
        LiveRunState::Off => "live off".to_string(),
        LiveRunState::Disabled(reason) => format!("live disabled: {}", live_reason(reason)),
        LiveRunState::Connecting => "live connecting".to_string(),
        LiveRunState::Connected { since } => format!("live up {}s", (now - since).max(0)),
        LiveRunState::Backoff { until, attempt } => {
            format!("live retry #{attempt} in {}s", (until - now).max(0))
        }
        LiveRunState::Held { until } => {
            format!("live held (cloudflare) {}s", (until - now).max(0))
        }
    }
}

fn live_reason(reason: LiveOffReason) -> &'static str {
    match reason {
        LiveOffReason::NoSession => "no session",
        LiveOffReason::TooMany => "too many live connections",
        LiveOffReason::SessionInvalid => "session invalid",
    }
}

/// 一条观察的状态行。两条时间线各说各的,所以两个"还有多久"都要印。
fn describe_observation(status: &ObservationStatus) -> String {
    let now = now_secs();
    let mut parts = vec![
        format!("{} active", status.active),
        format!("{} gone", status.gone),
    ];
    if let Some(at) = status.next_discover_at {
        parts.push(format!("discover in {}s", (at - now).max(0)));
    }
    if let Some(at) = status.next_recheck_at {
        parts.push(format!("recheck in {}s", (at - now).max(0)));
    }
    if let Some(error) = &status.last_error {
        parts.push(format!("error: {error}"));
    }
    parts.join("  ")
}

fn describe_status(status: &WatchStatus) -> String {
    let now = now_secs();
    let mut parts = vec![format!("{:?}", status.state), describe_live(status.live)];
    if let Some(at) = status.next_poll_at {
        parts.push(format!("next in {}s", (at - now).max(0)));
    }
    if let Some(total) = status.last_total {
        parts.push(format!("{total} listed"));
    }
    parts.push(format!("hits today {}", status.hits_today));
    if status.failures > 0 {
        parts.push(format!("failures {}", status.failures));
    }
    if let Some(error) = &status.last_error {
        parts.push(format!("error: {error}"));
    }
    parts.join("  ")
}

/// 预算行只印 6 小时那个桶:短窗口的桶几乎总是 1/1,长的那个才是
/// "今天还能查多少次"的答案。
fn describe_budget(usage: &[BucketUsage]) -> String {
    let six_hours = usage.iter().find(|bucket| bucket.window_secs == 21_600);
    match six_hours {
        Some(bucket) => format!(
            "{}s {}/{} (server {})",
            bucket.window_secs, bucket.used, bucket.allowed, bucket.server_limit
        ),
        None if usage.is_empty() => "(nothing learned yet)".to_string(),
        None => usage
            .iter()
            .map(render_usage)
            .collect::<Vec<_>>()
            .join("  "),
    }
}

/// 搜索 id 解出来的查询里通常带着物品名,拿它当标签比印一串 uuid 强。
fn label_from_query(query_json: &str, search_ref: &SearchRef) -> String {
    serde_json::from_str::<serde_json::Value>(query_json)
        .ok()
        .and_then(|query| {
            query
                .get("name")
                .and_then(|name| name.as_str().map(str::to_string))
                .or_else(|| {
                    query
                        .get("type")
                        .and_then(|kind| kind.as_str().map(str::to_string))
                })
        })
        .unwrap_or_else(|| search_ref.search_id.chars().take(12).collect())
}

fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

enum RoundOutcome {
    Ok,
    RateLimited,
}

#[allow(clippy::too_many_arguments)]
fn one_round(
    client: &TradeClient,
    limiter: &mut RateLimiter,
    args: &Args,
    search_ref: &SearchRef,
    request_body: &str,
    rates: &CurrencyRates,
    cap: &Option<PriceCap>,
) -> Result<RoundOutcome, String> {
    // ---- search -------------------------------------------------------
    wait_for_budget(limiter, SEARCH_POLICY, "search");
    let ticket = limiter.insert_request(SEARCH_POLICY, now_secs());
    let response = client
        .search(&search_ref.league, request_body, args.session.as_deref())
        .map_err(|e| e.to_string())?;
    limiter.finish_request(ticket, response.rate.as_ref(), now_secs());

    println!("\nPOST search -> status {}", response.status);
    print_rate_headers(response.rate.as_ref());
    print_budget(limiter, SEARCH_POLICY, "search");
    if response.status == 429 {
        return Ok(RoundOutcome::RateLimited);
    }
    if response.looks_like_html {
        return Err("search answered HTML — looks like a Cloudflare block page".to_string());
    }
    if !response.is_success() {
        return Err(format!(
            "search failed with status {}: {}",
            response.status,
            first_line(&response.body_text())
        ));
    }

    let search = parse_search_response(&response.body).map_err(|e| e.to_string())?;
    let head: Vec<String> = search.result.iter().take(MAX_FETCH_IDS).cloned().collect();
    println!(
        "total {}  complexity {}  returned {} ids",
        search.total,
        search
            .complexity
            .map_or_else(|| "-".to_string(), |c| c.to_string()),
        search.result.len()
    );
    println!("first {} ids: {}", head.len(), head.join(", "));

    if head.is_empty() {
        println!("nothing listed right now — no fetch this round.");
        return Ok(RoundOutcome::Ok);
    }

    // ---- fetch --------------------------------------------------------
    wait_for_budget(limiter, FETCH_POLICY, "fetch");
    let ticket = limiter.insert_request(FETCH_POLICY, now_secs());
    let response = client
        .fetch(&head, &search.id, args.session.as_deref())
        .map_err(|e| e.to_string())?;
    limiter.finish_request(ticket, response.rate.as_ref(), now_secs());

    println!("\nGET fetch -> status {}", response.status);
    print_rate_headers(response.rate.as_ref());
    print_budget(limiter, FETCH_POLICY, "fetch");
    if response.status == 429 {
        return Ok(RoundOutcome::RateLimited);
    }
    if response.looks_like_html {
        return Err("fetch answered HTML — looks like a Cloudflare block page".to_string());
    }
    if !response.is_success() {
        return Err(format!(
            "fetch failed with status {}: {}",
            response.status,
            first_line(&response.body_text())
        ));
    }

    let listings = parse_fetch_response(&response.body).map_err(|e| e.to_string())?;
    print_listings(&listings, cap, rates);
    Ok(RoundOutcome::Ok)
}

// ---------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------

fn print_listings(listings: &[ListingSummary], cap: &Option<PriceCap>, rates: &CurrencyRates) {
    println!(
        "\n{:<3}{:<14}{:<18}{:<22}{:<10}{:<22}item",
        "#", "price", "verdict", "account", "presence", "indexed"
    );
    let mut first_hit: Option<&ListingSummary> = None;
    for (index, listing) in listings.iter().enumerate() {
        let verdict = cap
            .as_ref()
            .map(|cap| judge(cap, listing.price.as_ref(), rates));
        if first_hit.is_none() && verdict == Some(Verdict::Hit) {
            first_hit = Some(listing);
        }
        println!(
            "{:<3}{:<14}{:<18}{:<22}{:<10}{:<22}{}",
            index + 1,
            listing
                .price
                .as_ref()
                .map_or_else(|| "no price".to_string(), Price::display),
            verdict.map_or_else(|| "-".to_string(), |v| format!("{v:?}")),
            listing.account,
            presence(listing),
            listing.indexed,
            listing.item_name
        );
    }

    match first_hit {
        Some(listing) => {
            println!("\nfirst hit whisper: {}", listing.whisper);
            if listing.hideout_token.is_some() {
                println!("(this listing carries a hideout_token — phase 2 can travel to it)");
            }
        }
        None if cap.is_some() => println!("\nno hit this round."),
        None => println!("\n(no --cap given, so nothing is judged)"),
    }
}

fn presence(listing: &ListingSummary) -> &'static str {
    match (listing.online, listing.afk) {
        (true, true) => "afk",
        (true, false) => "online",
        (false, _) => "offline",
    }
}

/// 把解析好的限速头按响应头的原样式印回来 —— 这样看到的就是服务端说的话,
/// 而不是我们对它的理解。
fn print_rate_headers(rate: Option<&RateHeaders>) {
    let Some(rate) = rate else {
        println!("    (this response carried no X-Rate-Limit-* headers)");
        return;
    };
    println!("    X-Rate-Limit-Policy: {}", rate.policy);
    println!("    X-Rate-Limit-Rules: {}", rate.rules.join(","));
    for rule in &rate.rules {
        if let Some(buckets) = rate.limits.get(rule) {
            println!("    X-Rate-Limit-{rule}: {}", render_buckets(buckets));
        }
        if let Some(buckets) = rate.state.get(rule) {
            println!("    X-Rate-Limit-{rule}-State: {}", render_buckets(buckets));
        }
    }
    if let Some(secs) = rate.retry_after_secs {
        println!("    Retry-After: {secs}");
    }
    if !rate.mentions_account() {
        println!("    (rules have no `Account` — this request was anonymous to the server)");
    }
}

fn render_buckets(buckets: &[Bucket]) -> String {
    buckets
        .iter()
        .map(|b| format!("{}:{}:{}", b.requests, b.window_secs, b.timeout_secs))
        .collect::<Vec<_>>()
        .join(",")
}

/// 限速器自己的账:每个桶用了多少、我们允许自己用多少、服务端真正的上限是多少。
fn print_budget(limiter: &mut RateLimiter, policy: &str, label: &str) {
    let view = limiter.budget_view(policy, now_secs());
    if view.is_empty() {
        println!("    budget: (nothing learned yet)");
        return;
    }
    let rendered: Vec<String> = view.iter().map(render_usage).collect();
    println!("    budget {label}: {}", rendered.join("  "));
    let now = now_secs();
    let next = limiter.next_request_time(policy, now);
    println!("    next {label} allowed in {} s", (next - now).max(0));
}

fn render_usage(usage: &BucketUsage) -> String {
    format!(
        "{}{}s {}/{} (server {})",
        if usage.rule.eq_ignore_ascii_case("ip") {
            String::new()
        } else {
            format!("{}:", usage.rule)
        },
        usage.window_secs,
        usage.used,
        usage.allowed,
        usage.server_limit
    )
}

fn describe_rates(rates: &CurrencyRates) -> String {
    let parts: Vec<String> = [
        ("chaos", rates.chaos_per_divine_milli),
        ("exalted", rates.exalted_per_divine_milli),
        ("mirror", rates.mirror_per_divine_milli),
    ]
    .into_iter()
    .filter_map(|(name, milli)| milli.map(|m| format!("{name}={}", m as f64 / 1000.0)))
    .collect();
    if parts.is_empty() {
        "(none — cross-currency listings will read DifferentCurrency)".to_string()
    } else {
        format!("per divine: {}", parts.join(", "))
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

// ---------------------------------------------------------------------
// 汇率
// ---------------------------------------------------------------------

/// 汇率来源的优先级:命令行给的 > poe.ninja 的经济接口 > 没有。
///
/// 手动传 `--rates` 是为了能在不打扰 ninja 的情况下反复跑这个探针;
/// 拿不到汇率不是错误,异币种的挂单会老老实实标 `DifferentCurrency`。
fn resolve_rates(args: &Args, search_ref: &SearchRef) -> CurrencyRates {
    if let Some(rates) = &args.rates {
        return rates.clone();
    }
    let client = NinjaClient::new();
    match client.currency_rates(&search_ref.league) {
        Ok(overview) => match overview.rates_per_divine() {
            Some(rates) => {
                println!("(rates from poe.ninja economy api)");
                CurrencyRates {
                    chaos_per_divine_milli: to_milli(rates.chaos),
                    exalted_per_divine_milli: to_milli(rates.exalted),
                    mirror_per_divine_milli: to_milli(rates.mirror),
                }
            }
            None => {
                println!("(poe.ninja quotes in something other than divine — no rates)");
                CurrencyRates::none()
            }
        },
        Err(error) => {
            println!("(poe.ninja rates unavailable: {error} — pass --rates to compare currencies)");
            CurrencyRates::none()
        }
    }
}

fn to_milli(value: Option<f64>) -> Option<i64> {
    value
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| (v * 1000.0).round() as i64)
}

// ---------------------------------------------------------------------
// 限速器 + 时钟
// ---------------------------------------------------------------------

/// 限速器说这条策略还要等几秒。
fn limiter_wait(limiter: &mut RateLimiter, policy: &str) -> u64 {
    let now = now_secs();
    let next = limiter.next_request_time(policy, now);
    (next - now).max(0) as u64
}

/// 等到限速器放行为止。真正的程序里这一步归网关的队列管;
/// 探针是单线程的,直接睡就是最诚实的等法。
fn wait_for_budget(limiter: &mut RateLimiter, policy: &str, label: &str) {
    let wait = limiter_wait(limiter, policy);
    if wait > 0 {
        println!("waiting {wait} s for {label} budget");
        sleep(Duration::from_secs(wait));
    }
}

// ---------------------------------------------------------------------
// 命令行
// ---------------------------------------------------------------------

struct Args {
    league: String,
    /// 可以给多条:蹲价模式里每条就是一行搜索,一次性模式只看第一条。
    searches: Vec<String>,
    cap: Option<f64>,
    currency: String,
    rounds: u32,
    session: Option<String>,
    rates: Option<CurrencyRates>,
    watch: bool,
    /// `--observe`:市场观察那三条接口事实的一次性取证,见 [`run_observe`]。
    observe: bool,
    /// `--observe-run`:起真的 actor 跑市场观察,见 [`run_observe_run`]。
    observe_run: bool,
    /// `--observe-run --live`:让这条观察也开一条 live WebSocket。
    ///
    /// 要会话(接口不接待匿名连接)。不给这个开关就是原来那条只轮询的路,
    /// 而且**一条 WebSocket 都不开** —— 探针跑的是哪条路,应当由命令行说了算。
    live: bool,
    /// `--sample-every N`:秒推来的挂单每 N 条抓一条(1 = 全抓)。
    sample_every: u32,
    /// 观察的两个节奏(秒)。默认就是设置里的下限 —— 探针一趟只有几分钟,
    /// 跑的是"最勤能多勤",而 `normalize` 会兜住不让它更勤。
    discover_seconds: u64,
    recheck_seconds: u64,
    minutes: u64,
    poll_seconds: u64,
    /// `--hideout <alert_id>`:蹲价模式下发一条 `TravelToHideout` 命令。
    hideout: Option<i64>,
}

impl Args {
    fn cap(&self) -> Option<PriceCap> {
        self.cap
            .map(|amount| Price::from_trade(amount, &self.currency))
    }

    /// 手写的参数解析。这里只有七个开关,拉一个 clap 进来不划算;
    /// 而且探针的参数形状随时会变,少一个依赖少一次对齐。
    fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut league = "Forbidden Rites".to_string();
        let mut searches: Vec<String> = Vec::new();
        let mut cap: Option<f64> = None;
        let mut currency = Currency::Divine.code().to_string();
        let mut rounds: u32 = 1;
        let mut session: Option<String> = None;
        let mut rates: Option<CurrencyRates> = None;
        let mut watch = false;
        let mut observe = false;
        let mut observe_run = false;
        let mut live = false;
        let mut sample_every: u32 = 1;
        let mut discover_seconds = pnd_settings::MIN_DISCOVER_INTERVAL_SECS;
        let mut recheck_seconds = pnd_settings::MIN_RECHECK_INTERVAL_SECS;
        let mut minutes: u64 = 3;
        let mut poll_seconds: u64 = 300;
        let mut hideout: Option<i64> = None;

        let mut args = args.peekable();
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--league" => league = value()?,
                "--search" => searches.push(value()?),
                "--watch" => watch = true,
                "--observe" => observe = true,
                "--observe-run" => observe_run = true,
                "--live" => live = true,
                "--sample-every" => {
                    let raw = value()?;
                    sample_every = raw
                        .parse::<u32>()
                        .map_err(|_| format!("--sample-every wants a whole number, got {raw:?}"))?;
                }
                "--discover-seconds" => {
                    let raw = value()?;
                    discover_seconds = raw.parse::<u64>().map_err(|_| {
                        format!("--discover-seconds wants a whole number, got {raw:?}")
                    })?;
                }
                "--recheck-seconds" => {
                    let raw = value()?;
                    recheck_seconds = raw.parse::<u64>().map_err(|_| {
                        format!("--recheck-seconds wants a whole number, got {raw:?}")
                    })?;
                }
                "--minutes" => {
                    let raw = value()?;
                    minutes = raw
                        .parse::<u64>()
                        .map_err(|_| format!("--minutes wants a whole number, got {raw:?}"))?;
                    if minutes == 0 {
                        return Err("--minutes must be at least 1".to_string());
                    }
                }
                "--poll-seconds" => {
                    let raw = value()?;
                    poll_seconds = raw
                        .parse::<u64>()
                        .map_err(|_| format!("--poll-seconds wants a whole number, got {raw:?}"))?;
                }
                "--cap" => {
                    let raw = value()?;
                    cap = Some(
                        raw.parse::<f64>()
                            .map_err(|_| format!("--cap wants a number, got {raw:?}"))?,
                    );
                }
                "--currency" => currency = value()?,
                "--rounds" => {
                    let raw = value()?;
                    rounds = raw
                        .parse::<u32>()
                        .map_err(|_| format!("--rounds wants a whole number, got {raw:?}"))?;
                    if rounds == 0 {
                        return Err("--rounds must be at least 1".to_string());
                    }
                }
                "--hideout" => {
                    let raw = value()?;
                    hideout = Some(
                        raw.parse::<i64>()
                            .map_err(|_| format!("--hideout wants an alert id, got {raw:?}"))?,
                    );
                }
                "--session" => session = Some(value()?),
                "--rates" => rates = Some(parse_rates(&value()?)?),
                "-h" | "--help" => return Err("help".to_string()),
                other => return Err(format!("unknown flag {other:?}")),
            }
        }

        if searches.is_empty() {
            return Err("--search is required".to_string());
        }
        // live 接口不接待匿名连接:没有会话一条也连不上。与其跑满一分钟再报
        // "连不上",不如现在就说清楚缺什么。
        if live && session.as_deref().unwrap_or_default().trim().is_empty() {
            return Err("--live needs --session <POESESSID>".to_string());
        }
        Ok(Args {
            league,
            searches,
            cap,
            currency,
            rounds,
            session,
            rates,
            watch,
            observe,
            observe_run,
            live,
            sample_every,
            discover_seconds,
            recheck_seconds,
            minutes,
            // 低于 60 秒对交易站不礼貌;`AppSettings::normalize` 也会兜这一下,
            // 这里先兜是为了打印出来的数就是真正会用的数。
            poll_seconds: poll_seconds.max(60),
            hideout,
        })
    }
}

/// `chaos=25.21,exalted=83.42` —— 每 1 divine 换多少个它,和 ninja 的口径一致。
fn parse_rates(raw: &str) -> Result<CurrencyRates, String> {
    let mut rates = CurrencyRates::none();
    for pair in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("--rates wants name=value pairs, got {pair:?}"))?;
        let value: f64 = value
            .trim()
            .parse()
            .map_err(|_| format!("--rates value for {name:?} is not a number"))?;
        let milli = to_milli(Some(value))
            .ok_or_else(|| format!("--rates value for {name:?} must be positive"))?;
        match Currency::parse(name.trim()) {
            Currency::Chaos => rates.chaos_per_divine_milli = Some(milli),
            Currency::Exalted => rates.exalted_per_divine_milli = Some(milli),
            Currency::Mirror => rates.mirror_per_divine_milli = Some(milli),
            other => return Err(format!("--rates does not know {:?}", other.code())),
        }
    }
    Ok(rates)
}

#[cfg(test)]
mod trade_probe_tests {
    use super::*;

    /// 假 id 必须和真 id **一样长、一样是十六进制**,只有最后 4 位不同 ——
    /// 否则服务端可能是在抱怨格式,而不是在回答"这条挂单没了"。
    #[test]
    fn a_fake_listing_id_only_differs_in_its_last_four_characters() {
        let real = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let fake = fake_listing_id(real);
        assert_eq!(fake.len(), real.len());
        assert!(fake.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(&fake[..60], &real[..60]);
        assert_eq!(&fake[60..], "def0", "cdef 每位加一,f 回到 0");
        assert_ne!(fake, real);
    }

    #[test]
    fn short_ids_keep_both_ends() {
        assert_eq!(short_id("abcd1234"), "abcd1234");
        assert_eq!(
            short_id("0123456789abcdef0123456789abcdef"),
            "01234567…89abcdef"
        );
    }

    fn parse(flags: &[&str]) -> Result<Args, String> {
        Args::parse(flags.iter().map(|flag| (*flag).to_string()))
    }

    /// `--live` 必须带会话:live 接口不接待匿名连接,而"跑满一分钟才发现
    /// 一条都连不上"是最难查的那种失败。
    #[test]
    fn live_without_a_session_is_refused_before_anything_goes_out() {
        // 不用 `expect_err`:那要求 `Args` 能 Debug 打印,而它手里攥着
        // POESESSID —— 让它可打印,迟早会有一条日志把 cookie 印出来。
        let error = match parse(&["--observe-run", "--search", "abc", "--live"]) {
            Ok(_) => panic!("--live without --session must not be accepted"),
            Err(error) => error,
        };
        assert!(error.contains("--live needs --session"), "{error}");
        // 空串的 cookie 也一样不算数。
        assert!(
            parse(&[
                "--observe-run",
                "--search",
                "abc",
                "--live",
                "--session",
                "  "
            ])
            .is_err()
        );

        let args = parse(&[
            "--observe-run",
            "--search",
            "abc",
            "--live",
            "--session",
            "cookie",
            "--sample-every",
            "5",
        ])
        .unwrap_or_else(|error| panic!("--live with a session is fine: {error}"));
        assert!(args.live);
        assert_eq!(args.sample_every, 5);

        // 不给 --live 就还是原来那条只轮询的路,会话可给可不给。
        let args = parse(&["--observe-run", "--search", "abc"])
            .unwrap_or_else(|error| panic!("poll-only: {error}"));
        assert!(!args.live);
        assert_eq!(args.sample_every, 1, "默认全抓");
    }
}
