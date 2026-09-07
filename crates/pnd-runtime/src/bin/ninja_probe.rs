//! poe.ninja 接口探针:把 `pnd-ninja` 的生产函数指到线上,把它看见的东西原样打出来。
//!
//! ```text
//! cargo run -p pnd-runtime --bin ninja_probe -- --index
//! cargo run -p pnd-runtime --bin ninja_probe -- --search --league forbiddenrites
//! cargo run -p pnd-runtime --bin ninja_probe -- --search --league forbiddenrites --class "Gemling Legionnaire"
//! cargo run -p pnd-runtime --bin ninja_probe -- --search --skills "Lightning Arrow" --items "Wake of Destruction"
//! cargo run -p pnd-runtime --bin ninja_probe -- --economy --league "Forbidden Rites"
//! ```
//!
//! 上面四个只读不写。下面五个跑的是 `pnd_runtime::ninja_sampler` 那条真管线,
//! 会往 `--db` 指的那个 `ninja.sqlite` 里写东西(不给就是默认库):
//!
//! ```text
//! cargo run -p pnd-runtime --bin ninja_probe -- --plan
//! cargo run -p pnd-runtime --bin ninja_probe -- --facets --db C:\tmp\ninja-test.sqlite
//! cargo run -p pnd-runtime --bin ninja_probe -- --sample --limit 30 --db C:\tmp\ninja-test.sqlite
//! cargo run -p pnd-runtime --bin ninja_probe -- --aggregate --db C:\tmp\ninja-test.sqlite
//! cargo run -p pnd-runtime --bin ninja_probe -- --prices --db C:\tmp\ninja-test.sqlite
//! ```
//!
//! 采样这几个模式**中途杀掉是安全的**:每个分区、每个角色都是库里一行带状态的
//! 工作单,下一次 `--facets` 会沿用同一个 version 把剩下的跑完。跑到目标阶段
//! 且不满 `refresh_hours`(默认 24 小时)时会直接报 `[skipped]`,`--force` 无视它。
//!
//! `--facets` / `--sample` / `--aggregate` 跑的步骤顺序是
//! **分面 → 参考价 → 角色详情 → 词缀统计**(见 `ninja_sampler::stage_plan`),
//! 所以哪怕角色详情那一步被 429 卡住,暗金页的参考价也已经落库了。
//!
//! 撞上 429 时管线**只等不放弃**:听服务端的 `Retry-After`,它没说就按
//! 60 → 120 → 300 → 600 秒往上退,并且把这一轮剩下的请求间隔翻一倍。
//! 探针照样把每一条 `[progress]` 打出来,想看它在等就盯那一行。
//!
//! `--sample --limit N` 是"这一次最多再抓 N 个角色详情"(不给就补到设置里的
//! `sample_target`,2,000 个人 1 秒一个 ≈ 35 分钟)。`--aggregate` 反过来:
//! 默认一个都不补,只把库里已有的详情重新数一遍,想顺手多抓就写 `--limit N`。
//!
//! `--search` 的 `--league` 用 builds 的短名(`forbiddenrites`),`--economy` 的
//! `--league` 用经济接口的显示名(`Forbidden Rites`)——这两个接口要的就是不同的东西。
//! 采样模式两个都要:`--league` 是短名,经济接口那个显示名默认从 index-state 里
//! 读回来,拿不到时用 `--league-name` 指定。
//!
//! # 实测的列(2026-09-06 跑本探针所见,Forbidden Rites,快照 1541-20260906-49582)
//!
//! 搜索响应固定给 **28 列 × 前 100 名角色**(没有分页)。承载方式只有三种,
//! 列 id 决定该读哪种:
//!
//! | 列 id | group | 承载 | 字典 | 我们没消费的字段号 |
//! |---|---|---|---|---|
//! | `name`, `account` | 同名 | 100 strings(field 7) | - | 4, 13 |
//! | `class` | `class` | 100 varints(field 6,packed) | `class` | 13 |
//! | `skills`, `keypassives` | 同名 | 100 lists(field 9,每人一个 packed 列表) | `gem` / `keypassive` | 3, 4, 13 |
//! | `level`, `life`, `energyshield` | 同名 | 100 varints | - | 13 |
//! | `ehp__str`, `dps.total` | `ehp` / `dps` | 100 strings | - | 4, 13 |
//! | `dps.skill` | `dps` | 100 varints | `gem` | 13 |
//! | `dps.physical` | `dps` | 100 varints | - | 3, 13 |
//! | `dps.lightning`, `dps.cold`, `dps.fire`, `dps.chaos`, `dps.mode` | `dps` | 100 varints | - | 3, 5, 13 |
//! | `rate.total`, `critchance.total`, `critmulti.total`, `hitchance.total`, `projectiles.total`, `forks.total`, `chains.total`, `splits.total`, `pierces.total`, `aoeradius.total`, `duration.total` | 各自 | 100 strings | - | 4, 13 |
//!
//! 两个坑:
//!
//! - `dps.total`、`ehp__str` 这些看着是数字的列其实是**字符串**(对面已经格式化过)。
//!   要参与计算得自己解析,别指望它是 varint。
//! - 字段 3/4/5/13 我们一律不消费,只把号码记进 `Column::other_fields`。
//!   格式漂移时第一个该看的就是这里。
//!
//! # 实测的分面(同一次运行)
//!
//! 9 个分面。`kind` 恰好就是它该查的 NDIC 字典键,但 `dictionary_key_for_facet`
//! 仍然显式写死映射:`kind` 是对面的字段,不该拿它当我们的契约。
//!
//! | 分面 | 字典键 | 分面行数 | 字典条目数 |
//! |---|---|---|---|
//! | `class` | `class` | 31 | 31 |
//! | `weaponmode` | `weaponmode` | 54 | 54 |
//! | `items` | `item` | 490 | 490 |
//! | `skills` | `gem` | 294 | 1235 |
//! | `traits` | `skilltrait` | 9 | 9 |
//! | `keypassives` | `keypassive` | 261 | 261 |
//! | `anointed` | `anointed` | 330 | 330 |
//! | `allskills` | `gem` | 1235 | 1235 |
//! | `spiritgems` | `gem` | 422 | 1235 |
//!
//! 响应带 7 条字典引用(class / gem / keypassive / weaponmode / item /
//! skilltrait / anointed),其中 gem / keypassive / item / skilltrait 各还有一张
//! 我们暂时不用的叠加表。三个宝石分面共用同一张 gem 表,所以探针按 sha1 缓存,
//! 7 条引用只打 7 个请求。
//!
//! `items` 分面里既有暗金名,也有 `Rare Ring` / `Magic Flask` 这种稀有度桶——
//! 前十名全是桶,热门暗金榜要往下翻,或者直接用 `--items <暗金名>` 拿它的总数。

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use pnd_ninja::aggregate::{SlotModStat, unique_usage_from_facet};
use pnd_ninja::client::NinjaClient;
use pnd_ninja::economy::UNIQUE_TYPES;
use pnd_ninja::search::{SearchResponse, dictionary_key_for_facet};
use pnd_runtime::ninja_sampler::{
    SamplerConfig, SamplerEvent, SamplerStage, plan_partitions, refresh_prices, run_sampler,
};
use pnd_storage::{NinjaStore, SnapshotRow};

/// 默认盯这个联赛:计划里的采样目标就是它。
const DEFAULT_BUILD_LEAGUE: &str = "forbiddenrites";
const DEFAULT_ECONOMY_LEAGUE: &str = "Forbidden Rites";

/// 表格一律只印这么多行:探针是给人看的,不是给人翻页的。
const TOP_ROWS: usize = 15;
const TOP_MOD_ROWS: usize = 10;

/// `--aggregate` 之后重点看这三个部位。胸甲的 `base_maximum_life` p50
/// 落在 100–200 之间就说明整条链路(抓取 → 存原文 → 聚合)是通的。
const SLOTS_OF_INTEREST: [&str; 3] = ["BodyArmour", "Ring", "Amulet"];

/// 对 poe.ninja 礼貌:两个请求之间至少隔 1 秒。探针跑一次几十个请求,
/// 不加这个就是在人家 CDN 前面刷屏。
fn pause() {
    std::thread::sleep(Duration::from_secs(1));
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ninja_probe failed: {error}");
        std::process::exit(1);
    }
}

/// 手搓参数解析:探针只有几个子命令和几个筛选,引一个 CLI 框架不值当。
struct Args {
    mode: Option<&'static str>,
    league: Option<String>,
    league_name: Option<String>,
    db: Option<PathBuf>,
    force: bool,
    limit: Option<u32>,
    filters: Vec<(String, String)>,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut args = Args {
        mode: None,
        league: None,
        league_name: None,
        db: None,
        force: false,
        limit: None,
        filters: Vec::new(),
    };
    let mut raw = std::env::args().skip(1);
    while let Some(argument) = raw.next() {
        match argument.as_str() {
            "--index" => args.mode = Some("index"),
            "--search" => args.mode = Some("search"),
            "--economy" => args.mode = Some("economy"),
            "--plan" => args.mode = Some("plan"),
            "--facets" => args.mode = Some("facets"),
            "--sample" => args.mode = Some("sample"),
            "--aggregate" => args.mode = Some("aggregate"),
            "--prices" => args.mode = Some("prices"),
            "--force" => args.force = true,
            "--league" => {
                args.league = Some(raw.next().ok_or("--league needs a value")?);
            }
            "--league-name" => {
                args.league_name = Some(raw.next().ok_or("--league-name needs a value")?);
            }
            "--db" => {
                args.db = Some(PathBuf::from(raw.next().ok_or("--db needs a path")?));
            }
            "--limit" => {
                let value = raw.next().ok_or("--limit needs a number")?;
                args.limit = Some(value.parse()?);
            }
            key @ ("--class" | "--skills" | "--items" | "--keypassives" | "--spiritgems") => {
                let value = raw.next().ok_or_else(|| format!("{key} needs a value"))?;
                args.filters
                    .push((key.trim_start_matches("--").to_owned(), value));
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    Ok(args)
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    match args.mode {
        Some("index") => print_index(&NinjaClient::new()),
        Some("search") => print_search(&NinjaClient::new(), &args),
        Some("economy") => print_economy(&NinjaClient::new(), &args),
        Some("plan") => print_plan(&args),
        Some("facets") => run_facets(&args),
        Some("sample") => run_sample(&args),
        Some("aggregate") => run_aggregate(&args),
        Some("prices") => run_prices(&args),
        _ => {
            eprintln!(
                "usage: ninja_probe\n  \
                 --index\n  \
                 --search [--league <url>] [--class X] [--skills Y] [--items Z]\n  \
                 --economy [--league <name>]\n  \
                 --plan [--league <url>]\n  \
                 --facets [--league <url>] [--db <path>] [--force]\n  \
                 --sample [--limit N] [--db <path>] [--force]\n  \
                 --aggregate [--db <path>] [--limit N] [--force]\n  \
                 --prices [--league-name <name>] [--db <path>]"
            );
            Err("no subcommand given".into())
        }
    }
}

fn print_index(client: &NinjaClient) -> Result<(), Box<dyn Error>> {
    let index = client.index_state()?;
    pause();
    let builds = client.build_index_state()?;

    println!(
        "== snapshot versions ({}) ==",
        index.snapshot_versions.len()
    );
    for snapshot in &index.snapshot_versions {
        println!(
            "  {:<22} {:<26} version {}  overview {}  timeMachine [{}]",
            snapshot.url,
            snapshot.snapshot_name,
            snapshot.version,
            snapshot.overview_type,
            snapshot.time_machine_labels.join(", ")
        );
    }

    let mut leagues = builds.league_builds.clone();
    leagues.sort_by(|left, right| right.total.cmp(&left.total));

    println!();
    println!("== build leagues ({}) ==", leagues.len());
    for league in &leagues {
        let snapshot = index
            .snapshot_for_url(&league.league_url)
            .map_or_else(|| "-".to_owned(), |snapshot| snapshot.version.clone());
        println!(
            "  {:<22} {:<26} total {:>7}  hardcore {:<5} snapshot {}",
            league.league_url, league.league_name, league.total, league.hardcore, snapshot
        );
        for share in league.statistics.iter().take(8) {
            println!(
                "      {:<28} {:>6.2}%  trend {}",
                share.class, share.percentage, share.trend
            );
        }
    }
    Ok(())
}

fn print_search(client: &NinjaClient, args: &Args) -> Result<(), Box<dyn Error>> {
    let league = args
        .league
        .clone()
        .unwrap_or_else(|| DEFAULT_BUILD_LEAGUE.to_owned());

    let index = client.index_state()?;
    let snapshot = index
        .snapshot_for_url(&league)
        .ok_or_else(|| format!("no snapshot for league url {league}"))?
        .clone();
    println!(
        "league {} ({})  version {}  snapshot {}",
        league, snapshot.name, snapshot.version, snapshot.snapshot_name
    );

    let filters: Vec<(&str, &str)> = args
        .filters
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    if filters.is_empty() {
        println!("filters: (none)");
    } else {
        for (key, value) in &filters {
            println!("filters: {key} = {value}");
        }
    }

    pause();
    let response = client.search(&snapshot.version, &snapshot.snapshot_name, &filters)?;
    println!();
    println!("total matching characters: {}", response.total);

    print_columns(&response);
    print_dictionaries(&response);

    let dictionaries = fetch_dictionaries(client, &response)?;
    print_facets(&response, &dictionaries);
    print_characters(&response, &dictionaries);
    Ok(())
}

fn print_columns(response: &SearchResponse) {
    println!();
    println!("== columns ({}) ==", response.columns.len());
    println!(
        "  {:<20} {:<14} {:<34} {:<12} other fields",
        "id", "group", "payload", "dict"
    );
    for column in &response.columns {
        let mut payload = Vec::new();
        if !column.varints.is_empty() {
            payload.push(format!("{} varints", column.varints.len()));
        }
        if !column.strings.is_empty() {
            payload.push(format!("{} strings", column.strings.len()));
        }
        if !column.lists.is_empty() {
            payload.push(format!("{} lists", column.lists.len()));
        }
        if payload.is_empty() {
            payload.push("(empty)".to_owned());
        }
        let others: Vec<String> = column
            .other_fields
            .iter()
            .map(ToString::to_string)
            .collect();
        println!(
            "  {:<20} {:<14} {:<34} {:<12} {}",
            column.id,
            column.group,
            payload.join(" / "),
            column.dictionary_key.as_deref().unwrap_or("-"),
            others.join(", ")
        );
    }
}

fn print_dictionaries(response: &SearchResponse) {
    println!();
    println!("== dictionary refs ({}) ==", response.dictionaries.len());
    for reference in &response.dictionaries {
        println!(
            "  {:<14} {}  overlay {}",
            reference.key,
            reference.sha1,
            reference.overlay_sha1.as_deref().unwrap_or("-")
        );
    }
}

/// 按 sha1 抓一次就够:`skills`/`allskills`/`spiritgems` 共用同一张 gem 表。
fn fetch_dictionaries(
    client: &NinjaClient,
    response: &SearchResponse,
) -> Result<BTreeMap<String, Vec<String>>, Box<dyn Error>> {
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut by_sha1: BTreeMap<String, Vec<String>> = BTreeMap::new();
    println!();
    for reference in &response.dictionaries {
        if let Some(cached) = by_sha1.get(&reference.sha1) {
            by_key.insert(reference.key.clone(), cached.clone());
            continue;
        }
        pause();
        let entries = client.dictionary(&reference.sha1)?;
        println!(
            "fetched dictionary {:<14} {} entries",
            reference.key,
            entries.len()
        );
        by_sha1.insert(reference.sha1.clone(), entries.clone());
        by_key.insert(reference.key.clone(), entries);
    }
    Ok(by_key)
}

fn print_facets(response: &SearchResponse, dictionaries: &BTreeMap<String, Vec<String>>) {
    for facet in &response.facets {
        let key = dictionary_key_for_facet(&facet.name);
        let dictionary: &[String] = dictionaries.get(key).map_or(&[], Vec::as_slice);
        println!();
        println!(
            "-- facet {} (kind {}, dict {} with {} entries, {} rows) --",
            facet.name,
            facet.kind,
            key,
            dictionary.len(),
            facet.entries.len()
        );
        for (rank, (label, count)) in response
            .resolve_facet(&facet.name, dictionary)
            .iter()
            .take(15)
            .enumerate()
        {
            println!("  {:>2}. {:<44} {:>8}", rank + 1, label, count);
        }
    }
}

fn print_characters(response: &SearchResponse, dictionaries: &BTreeMap<String, Vec<String>>) {
    let classes: &[String] = dictionaries.get("class").map_or(&[], Vec::as_slice);
    let refs = response.character_refs(classes);
    println!();
    println!("== first 10 of {} character refs ==", refs.len());
    for (rank, entry) in refs.iter().take(10).enumerate() {
        println!(
            "  {:>2}. {:<22} {:<24} {:<24} lvl {}",
            rank + 1,
            entry.account,
            entry.name,
            entry.class,
            entry.level
        );
    }
}

fn print_economy(client: &NinjaClient, args: &Args) -> Result<(), Box<dyn Error>> {
    let league = args
        .league
        .clone()
        .unwrap_or_else(|| DEFAULT_ECONOMY_LEAGUE.to_owned());
    println!("league {league}");

    let exchange = client.currency_rates(&league)?;
    println!(
        "exchange base: primary {} secondary {}",
        exchange.core.primary, exchange.core.secondary
    );
    match exchange.rates_per_divine() {
        Some(rates) => {
            println!(
                "  1 divine = {} chaos / {} exalted / {} mirror",
                show(rates.chaos),
                show(rates.exalted),
                show(rates.mirror)
            );
        }
        None => println!("  (base currency is not divine, no conversion)"),
    }

    for type_name in UNIQUE_TYPES {
        pause();
        let overview = client.unique_prices(&league, type_name)?;
        let mut lines = overview.lines.clone();
        lines.sort_by(|left, right| {
            right
                .primary_value
                .partial_cmp(&left.primary_value)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        println!();
        println!(
            "== {} - top 10 of {} by price in {} ==",
            type_name,
            lines.len(),
            overview.core.primary
        );
        for (rank, line) in lines.iter().take(10).enumerate() {
            println!(
                "  {:>2}. {:<32} {:<24} {:>10.1}  {:>5} listings  7d {:+.1}%",
                rank + 1,
                line.name,
                line.base_type,
                line.primary_value,
                line.listing_count,
                line.spark_line.total_change
            );
        }
    }
    Ok(())
}

fn show(value: Option<f64>) -> String {
    value.map_or_else(|| "?".to_owned(), |value| format!("{value:.4}"))
}

// ---------------------------------------------------------------------
// 采样管线(全部经由 pnd_runtime::ninja_sampler,探针自己不重写一份逻辑)
// ---------------------------------------------------------------------

/// 探针版的 [`SamplerConfig`]。
///
/// 经济接口那个显示名留空时由采样管线自己从 index-state 里读——所以
/// `--facets --league somethingelse` 也能问到对的价格,不用记两个名字。
/// 只有 `--prices` 不读 index-state,那时候必须给得出名字。
fn sampler_config(args: &Args, stop_after: SamplerStage) -> SamplerConfig {
    let league_url = args
        .league
        .clone()
        .unwrap_or_else(|| DEFAULT_BUILD_LEAGUE.to_owned());
    let league_name = args.league_name.clone().unwrap_or_else(|| {
        if league_url == DEFAULT_BUILD_LEAGUE {
            DEFAULT_ECONOMY_LEAGUE.to_owned()
        } else {
            String::new()
        }
    });
    let mut config = SamplerConfig::new(league_url, league_name);
    config.stop_after = stop_after;
    config.force = args.force;
    config.max_characters = args.limit;
    if let Some(db) = &args.db {
        config.db_path = db.clone();
    }
    config
}

/// 事件原样打出来。界面上是进度条,这里就是一行行日志。
fn print_event(event: &SamplerEvent) {
    match event {
        SamplerEvent::Started {
            version,
            snapshot_name,
            total_characters,
        } => println!(
            "[started]    version {version}  overview {snapshot_name}  league total {total_characters}"
        ),
        SamplerEvent::Skipped { reason } => println!("[skipped]    {reason}"),
        SamplerEvent::Progress {
            stage,
            done,
            total,
            note,
        } => println!("[{:<10}] {done:>4}/{total:<4} {note}", stage.as_str()),
        SamplerEvent::StageDone(stage) => println!("[stage done] {}", stage.as_str()),
        SamplerEvent::Sampled {
            characters,
            target,
            used_this_hour,
            hourly_budget,
            eta_secs,
        } => println!(
            "[sampled]    {characters}/{target} characters  {used_this_hour}/{hourly_budget} \
             requests this hour  eta {}h{:02}m",
            eta_secs / 3_600,
            (eta_secs % 3_600) / 60
        ),
        SamplerEvent::Prices { types_done } => println!(
            "[prices]     {types_done}/{} unique types refreshed",
            UNIQUE_TYPES.len()
        ),
        SamplerEvent::Finished { version } => println!("[finished]   version {version}"),
        SamplerEvent::Failed(reason) => println!("[failed]     {reason}"),
    }
}

/// 每个模式都是"跑一轮 → 从库里把结果读回来打印"。取消标志留着不动:
/// 探针是前台跑的,要停就 Ctrl+C / 直接杀进程——那也正是断点续跑要验的东西。
fn drive(config: &SamplerConfig, stop_after: SamplerStage) -> Result<(), Box<dyn Error>> {
    println!(
        "league {} (economy name {})  stop after {}  db {}",
        config.league_url,
        if config.league_name.is_empty() {
            "<from index-state>"
        } else {
            &config.league_name
        },
        stop_after.as_str(),
        config.db_path.display()
    );
    println!();
    let cancel = AtomicBool::new(false);
    run_sampler(config, &cancel, &|event| print_event(&event))?;
    Ok(())
}

/// 打开库、找到这一轮挂在哪个 version 上。没有快照行说明这一轮压根没开工。
fn latest(config: &SamplerConfig) -> Result<Option<(NinjaStore, SnapshotRow)>, Box<dyn Error>> {
    let store = NinjaStore::open(&config.db_path)?;
    let Some(snapshot) = store.latest_snapshot(&config.league_url)? else {
        println!();
        println!("(no snapshot row for {} yet)", config.league_url);
        return Ok(None);
    };
    Ok(Some((store, snapshot)))
}

fn print_plan(args: &Args) -> Result<(), Box<dyn Error>> {
    let config = sampler_config(args, SamplerStage::Facets);
    println!("league {}  (no database is touched)", config.league_url);
    println!();

    let cancel = AtomicBool::new(false);
    let plan = plan_partitions(&config, &cancel, &|event| print_event(&event))?;

    println!();
    println!(
        "== first-pass partitions ({}) for version {} ==",
        plan.partitions.len(),
        plan.version
    );
    let mut per_tier: BTreeMap<&str, usize> = BTreeMap::new();
    for partition in &plan.partitions {
        *per_tier.entry(partition.tier.as_str()).or_default() += 1;
        println!(
            "  {:<12} {}",
            partition.tier.as_str(),
            partition_label(&partition.key)
        );
    }

    println!();
    println!("== counts ==");
    for (tier, count) in &per_tier {
        println!("  {tier:<12} {count}");
    }
    let classes = per_tier.get("class").copied().unwrap_or_default();
    let second_pass = classes * config.tuning.skills_per_class as usize;
    println!(
        "  {:<12} {} (second pass: {} classes x {} skills each)",
        "class_skill", second_pass, classes, config.tuning.skills_per_class
    );
    println!(
        "  {:<12} {} searches for the whole facet stage",
        "total",
        plan.partitions.len() + second_pass
    );
    Ok(())
}

fn run_facets(args: &Args) -> Result<(), Box<dyn Error>> {
    let config = sampler_config(args, SamplerStage::Facets);
    drive(&config, SamplerStage::Facets)?;

    let Some((store, snapshot)) = latest(&config)? else {
        return Ok(());
    };
    let keys = store.partition_keys(&config.league_url, &snapshot.version)?;
    let (pending, done, failed) = store.character_counts(&config.league_url)?;
    println!();
    println!("== after the run ==");
    println!(
        "  snapshot   {} stage {} started_at {}",
        snapshot.version,
        snapshot.stage.as_str(),
        snapshot.started_at
    );
    println!("  partitions {} done", keys.len());
    println!("  characters {pending} pending / {done} done / {failed} failed");

    print_unique_table(&store, &config, &snapshot.version, "")?;
    match keys.iter().find(|key| key.starts_with("class=")) {
        Some(class_key) => print_unique_table(&store, &config, &snapshot.version, class_key)?,
        None => println!("\n(no class= partition finished yet)"),
    }
    Ok(())
}

/// 热门暗金榜 + 参考价:计划里"一张表就是你要的热门暗金市集价格追踪"那张表。
fn print_unique_table(
    store: &NinjaStore,
    config: &SamplerConfig,
    version: &str,
    partition_key: &str,
) -> Result<(), Box<dyn Error>> {
    let entries = store.unique_usage(&config.league_url, version, partition_key)?;
    let total = store
        .partition_total(&config.league_url, version, partition_key)?
        .unwrap_or_default();
    let usage = unique_usage_from_facet(&entries, total);

    println!();
    println!(
        "== top {} uniques in partition {} ({} characters matched) ==",
        TOP_ROWS,
        partition_label(partition_key),
        total
    );
    println!(
        "  {:>2}  {:<34} {:>8} {:>7}  {:>12} {:>9}  {:>7}",
        "#", "unique", "chars", "share", "price", "listings", "7d"
    );
    for (rank, row) in usage.iter().take(TOP_ROWS).enumerate() {
        let price = store.unique_price(&config.league_url, &row.name)?;
        let (value, listings, change) = price.map_or_else(
            || ("-".to_owned(), "-".to_owned(), "-".to_owned()),
            |row| {
                (
                    format!(
                        "{:.1} {}",
                        row.primary_value_milli as f64 / 1000.0,
                        row.primary_currency
                    ),
                    row.listing_count.to_string(),
                    row.total_change
                        .map_or_else(|| "-".to_owned(), |change| format!("{change:+.1}%")),
                )
            },
        );
        println!(
            "  {:>2}. {:<34} {:>8} {:>6.2}%  {:>12} {:>9}  {:>7}",
            rank + 1,
            row.name,
            row.count,
            row.share_percent,
            value,
            listings,
            change
        );
    }
    Ok(())
}

fn run_sample(args: &Args) -> Result<(), Box<dyn Error>> {
    let config = sampler_config(args, SamplerStage::Characters);
    drive(&config, SamplerStage::Characters)?;

    let Some((store, snapshot)) = latest(&config)? else {
        return Ok(());
    };
    let (pending, done, failed) = store.character_counts(&config.league_url)?;
    println!();
    println!("== after the run ==");
    println!(
        "  snapshot   {} stage {}",
        snapshot.version,
        snapshot.stage.as_str()
    );
    println!("  characters {pending} pending / {done} done / {failed} failed");
    Ok(())
}

fn run_aggregate(args: &Args) -> Result<(), Box<dyn Error>> {
    let mut config = sampler_config(args, SamplerStage::Aggregated);
    // 聚合阶段本身要先经过角色阶段(词缀是从角色详情里数出来的),而角色阶段
    // 默认会把样本补到 `sample_target` —— 2,000 个人、1 秒一个,半个多小时。
    // 探针里的 `--aggregate` 应该是"把库里已经有的重新数一遍",所以默认一个都不补;
    // 想顺手多抓一些就显式写 `--limit N`。
    config.max_characters = args.limit.or(Some(0));
    drive(&config, SamplerStage::Aggregated)?;

    let Some((store, snapshot)) = latest(&config)? else {
        return Ok(());
    };
    println!();
    println!(
        "== mods by slot (snapshot {}, stage {}) ==",
        snapshot.version,
        snapshot.stage.as_str()
    );
    for slot in SLOTS_OF_INTEREST {
        // 阈值给 0:探针要看的是原始统计,过滤是界面的事。
        let mut rows =
            store.slot_mods(&config.league_url, &snapshot.version, Some(slot), None, 0.0)?;
        rows.sort_by(|left, right| right.characters.cmp(&left.characters));
        println!();
        println!("-- {slot} ({} rows) --", rows.len());
        println!(
            "  {:>2}  {:<10} {:<10} {:<46} {:<34} {:>6} {:>7} {:>7}  {:>9} {:>9} {:>9}",
            "#",
            "rarity",
            "kind",
            "in-game text",
            "stat id",
            "chars",
            "sample",
            "share",
            "p25",
            "p50",
            "p75"
        );
        for (rank, row) in rows.iter().take(TOP_MOD_ROWS).enumerate() {
            let share = if row.sample_size == 0 {
                0.0
            } else {
                f64::from(row.characters) * 100.0 / f64::from(row.sample_size)
            };
            println!(
                "  {:>2}. {:<10} {:<10} {:<46} {:<34} {:>6} {:>7} {:>6.1}%  {:>9} {:>9} {:>9}",
                rank + 1,
                row.rarity,
                row.mod_kind,
                in_game_text(row),
                row.stat_id,
                row.characters,
                row.sample_size,
                share,
                percentile(row.p25),
                percentile(row.p50),
                percentile(row.p75)
            );
        }
    }
    Ok(())
}

fn run_prices(args: &Args) -> Result<(), Box<dyn Error>> {
    let config = sampler_config(args, SamplerStage::Facets);
    if config.league_name.is_empty() {
        return Err("--prices needs --league-name (it never reads index-state)".into());
    }
    println!(
        "league {}  db {}",
        config.league_name,
        config.db_path.display()
    );
    println!();
    let cancel = AtomicBool::new(false);
    refresh_prices(&config, &cancel, &|event| print_event(&event))?;

    let store = NinjaStore::open(&config.db_path)?;
    println!();
    match store.unique_prices_age(&config.league_url)? {
        Some(oldest) => println!(
            "oldest price row fetched at {oldest} ({}s ago)",
            pnd_runtime::now_secs().saturating_sub(oldest)
        ),
        None => println!("(no price rows stored)"),
    }
    Ok(())
}

/// 游戏里那句话,配不上就写 `(no text)` —— 空一格看着像列错位了。
///
/// "要不要补第几个数"这条规则问的是 [`SlotModStat::needs_value_marker`],
/// 和界面同一个答案;探针只给自己看,所以标记写成 `[#2]` 而不走 i18n。
fn in_game_text(row: &SlotModStat) -> String {
    if row.display.is_empty() {
        return "(no text)".to_owned();
    }
    if !row.needs_value_marker() {
        return row.display.clone();
    }
    format!("{} [#{}]", row.display, row.value_index)
}

fn percentile(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.1}"))
}

/// 空分区键在表头里长得像个 bug,给它一个名字。
fn partition_label(key: &str) -> &str {
    if key.is_empty() {
        "(whole league)"
    } else {
        key
    }
}
