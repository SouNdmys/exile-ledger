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
//! cargo run -p pnd-runtime --bin trade_probe -- \
//!     --league "Forbidden Rites" --search <搜索URL或id> \
//!     [--cap 20 --currency divine] [--rounds 3] [--session <POESESSID>] \
//!     [--rates chaos=25.21,exalted=83.42]
//! ```
//!
//! `--session` 里的 POESESSID **永远不会被打印出来**,只会以"带会话/匿名"
//! 一个词的形式出现在输出里。

use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pnd_domain::{
    Currency, CurrencyRates, ListingSummary, Price, PriceCap, SearchRef, Verdict, decode_search_id,
    judge, parse_search_reference, search_page_url, search_request_body,
};
use pnd_ninja::client::NinjaClient;
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
[--rates chaos=25.21,exalted=83.42]";

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
    let search_ref = parse_search_reference(&args.search, &args.league)
        .ok_or_else(|| format!("could not read a search reference out of {:?}", args.search))?;
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

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
    search: String,
    cap: Option<f64>,
    currency: String,
    rounds: u32,
    session: Option<String>,
    rates: Option<CurrencyRates>,
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
        let mut search: Option<String> = None;
        let mut cap: Option<f64> = None;
        let mut currency = Currency::Divine.code().to_string();
        let mut rounds: u32 = 1;
        let mut session: Option<String> = None;
        let mut rates: Option<CurrencyRates> = None;

        let mut args = args.peekable();
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--league" => league = value()?,
                "--search" => search = Some(value()?),
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
                "--session" => session = Some(value()?),
                "--rates" => rates = Some(parse_rates(&value()?)?),
                "-h" | "--help" => return Err("help".to_string()),
                other => return Err(format!("unknown flag {other:?}")),
            }
        }

        Ok(Args {
            league,
            search: search.ok_or("--search is required")?,
            cap,
            currency,
            rounds,
            session,
            rates,
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
