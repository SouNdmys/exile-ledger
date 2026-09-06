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
//! `--search` 的 `--league` 用 builds 的短名(`forbiddenrites`),`--economy` 的
//! `--league` 用经济接口的显示名(`Forbidden Rites`)——这两个接口要的就是不同的东西。
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
use std::time::Duration;

use pnd_ninja::client::NinjaClient;
use pnd_ninja::economy::UNIQUE_TYPES;
use pnd_ninja::search::{SearchResponse, dictionary_key_for_facet};

/// 默认盯这个联赛:计划里的采样目标就是它。
const DEFAULT_BUILD_LEAGUE: &str = "forbiddenrites";
const DEFAULT_ECONOMY_LEAGUE: &str = "Forbidden Rites";

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

/// 手搓参数解析:探针只有三个子命令和四个筛选,引一个 CLI 框架不值当。
struct Args {
    mode: Option<&'static str>,
    league: Option<String>,
    filters: Vec<(String, String)>,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut args = Args {
        mode: None,
        league: None,
        filters: Vec::new(),
    };
    let mut raw = std::env::args().skip(1);
    while let Some(argument) = raw.next() {
        match argument.as_str() {
            "--index" => args.mode = Some("index"),
            "--search" => args.mode = Some("search"),
            "--economy" => args.mode = Some("economy"),
            "--league" => {
                args.league = Some(raw.next().ok_or("--league needs a value")?);
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
    let client = NinjaClient::new();
    match args.mode {
        Some("index") => print_index(&client),
        Some("search") => print_search(&client, &args),
        Some("economy") => print_economy(&client, &args),
        _ => {
            eprintln!(
                "usage: ninja_probe --index | --search [--league <url>] [--class X] [--skills Y] [--items Z] | --economy [--league <name>]"
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
