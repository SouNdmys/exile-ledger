//! `live_probe` —— Live Search WebSocket 那条链路的手动验证探针。
//!
//! 它把 `pnd-trade/src/live.rs` 里的东西原样串一遍:解析搜索引用 → 拼 wss
//! 地址 → 握手 → 一条条读消息 →(可选)拿推来的挂单 id 去走生产的
//! `TradeClient::fetch`。**没有一行协议逻辑是这里写的**,握手头、消息解析、
//! 错误分类、重连退避全是调 `pnd-trade` 的函数;探针自己抄一份,
//! 就等于验证了一个和生产不一样的东西。
//!
//! 用法:
//! ```text
//! cargo run -p pnd-runtime --bin live_probe -- \
//!     --league "Forbidden Rites" --search <搜索URL或id> \
//!     [--minutes 5] [--session] [--fetch]
//! ```
//!
//! `--session` 从 `settings.json` 里读 POESESSID(环境变量 `POESESSID` 优先)。
//! 那串东西**永远不会被打印出来**,只会以"带会话/匿名"一个词的形式出现;
//! fetch 回来的 `whisper_token`/`hideout_token` 同理,只印长度和过期时间。
//!
//! 这个探针**只握手一次,不自动重连**:重连是 `live_worker.rs` 的活,
//! 探针反复捅握手接口没有意义,只会平白多几次失败记录。握手失败时它会把
//! worker 将会用的退避时间算给你看(`reconnect_delay`),然后以非零码退出。
//!
//! Ctrl+C 或 `--minutes` 到点都会走同一条收尾路径:发一个 Close 帧再散伙。

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pnd_domain::{ListingSummary, Price, SearchRef, live_page_url, parse_search_reference};
use pnd_runtime::now_secs;
use pnd_settings::SettingsStore;
use pnd_trade::client::{MAX_FETCH_IDS, TradeClient};
use pnd_trade::jwt::{jwt_claims, jwt_header};
use pnd_trade::listing::parse_fetch_response;
use pnd_trade::live::{
    LiveConfig, LiveError, LiveMessage, LiveSession, live_ws_url, reconnect_delay,
};
use pnd_trade::rate_limit::{FETCH_POLICY, RateLimiter};

const USAGE: &str = "usage: live_probe --search <url|id> [--league \"Forbidden Rites\"] \
[--minutes 5] [--session] [--fetch]";

/// 探针用的读超时比生产的 30 秒短。
///
/// 读超时决定"多久能看一眼取消标志":30 秒对后台 worker 刚好,但在命令行上
/// 意味着 Ctrl+C 之后要愣半分钟。5 秒是给人用的手感。
const PROBE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// 连着安静这么久就报一行"还连着",免得屏幕上什么都没有让人以为挂了。
const HEARTBEAT_SECS: u64 = 60;

/// 打印退避时间时用的抖动值。取正中间,这样屏幕上的数就是那一档的标称值
/// (真正的 worker 会摇一个随机数,结果在 ±20% 里)。
const MID_JITTER: f64 = 0.5;

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
            eprintln!("live_probe: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    let search_ref = parse_search_reference(&args.search, &args.league)
        .ok_or_else(|| format!("could not read a search reference out of {:?}", args.search))?;

    let (poesessid, session_source) = resolve_session(args);
    let user_agent = user_agent();
    let mut config = LiveConfig::new(&search_ref, poesessid, &user_agent);
    config.read_timeout = PROBE_READ_TIMEOUT;

    println!("league       {}", search_ref.league);
    println!("search id    {}", search_ref.search_id);
    println!(
        "wss url      {}",
        live_ws_url(&search_ref.league, &search_ref.search_id)
    );
    println!("referer      {}", live_page_url(&search_ref));
    println!("user agent   {user_agent}");
    println!("session      {session_source}");
    println!(
        "fetch        {}",
        if args.fetch {
            "yes (new ids go through TradeClient::fetch)"
        } else {
            "no (ids only)"
        }
    );
    println!(
        "running for  {:.1} minutes  (Ctrl+C stops early)",
        args.minutes
    );
    // 这一行同时证明 LiveConfig 的 Debug 是不漏会话的。
    println!("config       {config:?}\n");

    let mut session = match LiveSession::connect(&config) {
        Ok(session) => session,
        Err(error) => {
            report_failure(&error, config.has_session());
            return Err("handshake failed".to_string());
        }
    };
    println!("{} handshake  ok — the socket is open", stamp());
    if !config.has_session() {
        println!(
            "{}            (this connection is anonymous; GGG normally refuses those)",
            stamp()
        );
    }

    let outcome = pump(&mut session, args, &search_ref, &config);
    session.close();
    println!("{} closed     sent a close frame, socket released", stamp());
    outcome
}

/// 读消息的主循环。返回 `Err` 表示"这次是异常结束的"(服务端关了、读挂了),
/// 到点或者 Ctrl+C 都算正常。
fn pump(
    session: &mut LiveSession,
    args: &Args,
    search_ref: &SearchRef,
    config: &LiveConfig,
) -> Result<(), String> {
    let stop = install_ctrl_c();
    let deadline = Instant::now() + Duration::from_secs_f64(args.minutes * 60.0);

    let client = TradeClient::new(config.user_agent.clone());
    let mut limiter = RateLimiter::default();
    let mut idle_secs = 0u64;
    let mut pushes = 0u32;

    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        match session.next() {
            Ok(LiveMessage::New(ids)) => {
                idle_secs = 0;
                pushes += 1;
                println!(
                    "{} push       {} new listing(s): {}",
                    stamp(),
                    ids.len(),
                    ids.join(", ")
                );
                if args.fetch && !ids.is_empty() {
                    fetch_and_print(&client, &mut limiter, config, search_ref, &ids);
                }
            }
            // 连上之后服务端立刻发的订阅回执。探针是这条链路上唯一该把它
            // 拆开看的地方:下一次真跑之后,我们就知道那张 JWT 是干什么用的了。
            Ok(LiveMessage::Subscribed { token }) => {
                idle_secs = 0;
                println!("{} subscribed the server acknowledged this search", stamp());
                print_subscription_token(&token);
            }
            // 已经是"键名 + 值长度"的描述,不是原文 —— 不认识的消息里
            // 可能有凭证,屏幕和日志都不该替服务端保管它。
            Ok(LiveMessage::Other(description)) => {
                idle_secs = 0;
                println!("{} message    (shape only) {description}", stamp());
            }
            Ok(LiveMessage::Idle) => {
                // 一次超时 = 一个读超时那么久的沉默。攒够一分钟才吭一声,
                // 不然 5 秒一行的 "idle" 会把真正的推送冲出屏幕。
                idle_secs += PROBE_READ_TIMEOUT.as_secs();
                if idle_secs % HEARTBEAT_SECS == 0 {
                    println!(
                        "{} idle       still connected, {idle_secs}s without a push",
                        stamp()
                    );
                }
            }
            Err(error) => {
                println!("{} ENDED      {error}", stamp());
                report_failure(&error, config.has_session());
                return Err("the live connection ended early".to_string());
            }
        }
    }

    let reason = if stop.load(Ordering::Relaxed) {
        "Ctrl+C"
    } else {
        "--minutes elapsed"
    };
    println!("\n{} done       {reason} — {pushes} push(es) seen", stamp());
    Ok(())
}

/// 把推来的 id 按交易站规定的 10 个一批走生产的 fetch,顺便让限速器记账。
///
/// 这里的等待是老实睡出来的:探针是单线程的,真正的程序里这一步归网关的
/// 队列管(`pnd-runtime/src/gateway.rs`)。
fn fetch_and_print(
    client: &TradeClient,
    limiter: &mut RateLimiter,
    config: &LiveConfig,
    search_ref: &SearchRef,
    ids: &[String],
) {
    let session = if config.has_session() {
        Some(config.poesessid.as_str())
    } else {
        None
    };

    for batch in ids.chunks(MAX_FETCH_IDS) {
        let wait = {
            let now = now_secs();
            (limiter.next_request_time(FETCH_POLICY, now) - now).max(0) as u64
        };
        if wait > 0 {
            println!("{}            waiting {wait}s for fetch budget", stamp());
            std::thread::sleep(Duration::from_secs(wait));
        }

        let ticket = limiter.insert_request(FETCH_POLICY, now_secs());
        let response = match client.fetch(batch, &search_ref.search_id, session) {
            Ok(response) => response,
            Err(error) => {
                println!("{}            fetch failed: {error}", stamp());
                return;
            }
        };
        limiter.finish_request(ticket, response.rate.as_ref(), now_secs());

        if response.looks_like_html {
            println!(
                "{}            fetch answered HTML — cloudflare block page?",
                stamp()
            );
            return;
        }
        if !response.is_success() {
            println!("{}            fetch -> status {}", stamp(), response.status);
            return;
        }
        // 服务端有没有认出会话:Rules 里没有 `Account` 就说明 cookie 白带了。
        if session.is_some()
            && response
                .rate
                .as_ref()
                .is_some_and(|rate| !rate.mentions_account())
        {
            println!(
                "{}            (rate-limit rules have no `Account` — the POESESSID was ignored)",
                stamp()
            );
        }

        match parse_fetch_response(&response.body) {
            Ok(listings) => {
                for listing in &listings {
                    print_listing(listing);
                }
            }
            Err(error) => println!("{}            fetch body unreadable: {error}", stamp()),
        }
    }
}

fn print_listing(listing: &ListingSummary) {
    println!(
        "{}   listing  {:<14} {:<28} {:<20} {}",
        stamp(),
        listing
            .price
            .as_ref()
            .map_or_else(|| "no price".to_string(), Price::display),
        truncate(&listing.item_name, 28),
        truncate(&listing.account, 20),
        presence(listing)
    );
    println!(
        "{}            whisper_token {}   hideout_token {}",
        stamp(),
        describe_token(listing.whisper_token.as_deref()),
        describe_token(listing.hideout_token.as_deref())
    );
}

fn presence(listing: &ListingSummary) -> &'static str {
    match (listing.online, listing.afk) {
        (true, true) => "afk",
        (true, false) => "online",
        (false, _) => "offline",
    }
}

// ---------------------------------------------------------------------
// token:只说形状,不说内容
// ---------------------------------------------------------------------

/// 一个 token 的"体检报告"。解码、算剩余时间都在生产代码里
/// ([`pnd_runtime::describe_token`] → `pnd_trade::jwt`),探针只负责摆版面 ——
/// 早先这里自己抄了一份 base64url 解码,那就等于验证了一个和生产不一样的东西。
///
/// **绝不打印 token 本身**:它是能替你私聊、替你传送的凭证。
fn describe_token(token: Option<&str>) -> String {
    pnd_runtime::describe_token(token, now_secs())
}

/// 把订阅回执那张 JWT 拆开印出来:长度、还有多久过期、header、payload。
///
/// **签名那一段从不解码、也不打印。** 这里印的是"这张票是发给谁的、什么时候
/// 作废",不是那张票本身 —— 有了它就能冒充这次订阅。
///
/// 为什么要印:2026-09-07 那次真跑里,这条消息整条掉进了状态栏(当时它还是
/// `Other`),只看得见前几十个字符。下一次跑完,我们就能凭 `iss`/`exp`
/// 这些声明说清楚它到底是什么,而不是靠猜。
fn print_subscription_token(token: &str) {
    println!(
        "{}            token    {}",
        stamp(),
        describe_token(Some(token))
    );
    println!(
        "{}            header   {}",
        stamp(),
        section(jwt_header(token))
    );
    println!(
        "{}            payload  {}",
        stamp(),
        section(jwt_claims(token))
    );
    println!(
        "{}            (the signature is never decoded or printed)",
        stamp()
    );
}

/// 解得开就印 JSON 原文,解不开就照实说一句 —— 别装作没这一段。
fn section(value: Option<serde_json::Value>) -> String {
    value.map_or_else(
        || "(not readable base64url JSON)".to_string(),
        |value| value.to_string(),
    )
}

// ---------------------------------------------------------------------
// 失败报告
// ---------------------------------------------------------------------

/// 把一次失败翻译成人话:属于哪一类、worker 会怎么处置、下次什么时候再试。
///
/// `had_session` 只影响措辞。同样是 401,"你的会话过期了"和"你压根没给会话"
/// 该说的话不一样 —— 前者要你换一个,后者要你先粘一个。
fn report_failure(error: &LiveError, had_session: bool) {
    println!("\n--- why it failed ---------------------------------------");
    println!("error       {error}");
    if let LiveError::Handshake {
        status,
        looks_like_html,
        retry_after,
        body_excerpt,
    } = error
    {
        println!("http status {status}");
        println!(
            "body        {} — {}",
            if *looks_like_html {
                "looks like an HTML page"
            } else {
                "not HTML"
            },
            if body_excerpt.is_empty() {
                "(empty)".to_string()
            } else {
                format!("{body_excerpt:?}")
            }
        );
        match retry_after {
            Some(secs) => println!("retry-after {secs} s (the server asked for this; it wins)"),
            None => println!("retry-after (not sent)"),
        }
    }

    let verdict = if error.is_session_problem() {
        if had_session {
            "session problem — the worker drops the cookie, stops live, and asks for a new POESESSID"
        } else {
            "no session — live search needs a POESESSID; paste one into settings.json \
             (polling keeps working anonymously either way)"
        }
    } else if error.is_cloudflare() {
        "cloudflare block — the worker holds for 5 minutes and keeps polling anonymously"
    } else {
        "transient — the worker backs off and reconnects"
    };
    println!("verdict     {verdict}");

    let delay = error
        .retry_after()
        .map_or_else(|| reconnect_delay(0, MID_JITTER).as_secs(), |secs| secs);
    println!(
        "next try    {delay} s from now (attempt 1 of the 5/10/20/40/80/160/300 s ladder, ±20% jitter)"
    );
    println!("---------------------------------------------------------\n");
}

// ---------------------------------------------------------------------
// 会话、时钟、杂项
// ---------------------------------------------------------------------

/// POESESSID 的来源:环境变量 > `settings.json` > 没有。
///
/// 环境变量排第一是为了"不动设置文件也能试一把";只有 `--session` 才会去
/// 读盘,不然一个纯匿名的探针没道理去碰用户的设置。
fn resolve_session(args: &Args) -> (String, String) {
    if !args.session {
        return (
            String::new(),
            "no (anonymous — pass --session to use the one in settings.json)".to_string(),
        );
    }
    if let Ok(value) = std::env::var("POESESSID")
        && !value.trim().is_empty()
    {
        return (
            value.trim().to_string(),
            "yes (from the POESESSID environment variable, never printed)".to_string(),
        );
    }
    let store = SettingsStore::release_default();
    let poesessid = store.load().settings.poesessid.trim().to_string();
    if poesessid.is_empty() {
        (
            String::new(),
            format!(
                "no — {} has an empty poesessid (the handshake will be anonymous)",
                store.path().display()
            ),
        )
    } else {
        (
            poesessid,
            "yes (from settings.json, never printed)".to_string(),
        )
    }
}

/// 和 trade_probe 一样的 UA。以后会从设置里来,探针写死是为了少一个变量。
fn user_agent() -> String {
    format!(
        "PoeNinjaData/{} (contact: soundmys1994@gmail.com)",
        env!("CARGO_PKG_VERSION")
    )
}

/// 装一个 Ctrl+C 处理器,只翻一个标志位。
///
/// 循环最多每 `PROBE_READ_TIMEOUT` 看一次这个标志,所以按下去到退出最多
/// 隔那么久 —— 这正是给底层 socket 设读超时换来的东西。
fn install_ctrl_c() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    // 装不上(比如没有控制台)只是 Ctrl+C 不好使,不该拦着探针跑。
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed));
    stop
}

fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

// ---------------------------------------------------------------------
// 命令行
// ---------------------------------------------------------------------

struct Args {
    league: String,
    search: String,
    /// 小数:`--minutes 0.2` 就是 12 秒,方便做一次性的取证。
    minutes: f64,
    session: bool,
    fetch: bool,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut league = "Forbidden Rites".to_string();
        let mut search: Option<String> = None;
        let mut minutes: f64 = 5.0;
        let mut session = false;
        let mut fetch = false;

        let mut args = args;
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--league" => league = value()?,
                "--search" => search = Some(value()?),
                "--minutes" => {
                    let raw = value()?;
                    minutes = raw
                        .parse::<f64>()
                        .map_err(|_| format!("--minutes wants a number, got {raw:?}"))?;
                    if !(minutes.is_finite() && minutes > 0.0) {
                        return Err("--minutes must be a positive number".to_string());
                    }
                }
                "--session" => session = true,
                "--fetch" => fetch = true,
                "-h" | "--help" => return Err("help".to_string()),
                other => return Err(format!("unknown flag {other:?}")),
            }
        }

        Ok(Args {
            league,
            search: search.ok_or("--search is required")?,
            minutes,
            session,
            fetch,
        })
    }
}
