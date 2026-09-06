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
//! ```
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
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use pnd_domain::{
    Currency, CurrencyRates, ListingSummary, Price, PriceCap, SearchRef, Verdict, decode_search_id,
    judge, parse_search_reference, search_page_url, search_request_body,
};
use pnd_ninja::client::NinjaClient;
use pnd_runtime::actor::{
    HideoutOutcome, RuntimeCommand, RuntimeEvent, RuntimeHandle, RuntimePaths, WatchStatus,
};
use pnd_runtime::live_worker::{LiveOffReason, LiveRunState};
use pnd_runtime::now_secs;
use pnd_settings::{AppSettings, WatchEntry};
use pnd_trade::client::{MAX_FETCH_IDS, TradeClient, parse_search_response};
use pnd_trade::listing::parse_fetch_response;
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
[--cap 20] [--currency divine] [--session <POESESSID>] [--hideout <alert_id>]";

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
        HideoutOutcome::TokenMissing => {
            "no hideout_token on that listing (it was fetched anonymously)".to_string()
        }
        HideoutOutcome::Refreshed => "token was stale — fetched a fresh one, retrying".to_string(),
        HideoutOutcome::Failed { status, message } => {
            format!("failed (status {status}): {message}")
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
        Ok(Args {
            league,
            searches,
            cap,
            currency,
            rounds,
            session,
            rates,
            watch,
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
