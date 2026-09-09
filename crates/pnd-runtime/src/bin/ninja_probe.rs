//! poe.ninja 接口探针:把 `pnd-ninja` 的生产函数指到线上,把它看见的东西原样打出来。
//!
//! ```text
//! cargo run -p pnd-runtime --bin ninja_probe -- --index
//! cargo run -p pnd-runtime --bin ninja_probe -- --search --league forbiddenrites
//! cargo run -p pnd-runtime --bin ninja_probe -- --search --league forbiddenrites --class "Gemling Legionnaire"
//! cargo run -p pnd-runtime --bin ninja_probe -- --search --skills "Lightning Arrow" --items "Wake of Destruction"
//! cargo run -p pnd-runtime --bin ninja_probe -- --economy --league "Forbidden Rites"
//! cargo run -p pnd-runtime --bin ninja_probe -- --character --account player-0416 --name ExileCharacter
//! cargo run -p pnd-runtime --bin ninja_probe -- --raw https://poe.ninja/poe1/api/data/index-state
//! ```
//!
//! `--game poe1` 把上面这些全部指到 PoE1 那一半(默认是 poe2)。**采样那几个模式
//! 现在也认 PoE1**:两代各写各的库文件(`ninja.sqlite` / `ninja-poe1.sqlite`),
//! 所以 `--db` 不给时也不会互相盖。PoE1 的 `--league` 不给就用当季挑战联赛。
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
//!
//! # PoE1 那一半(2026-09-09 用本探针跑出来的)
//!
//! **是同一条管线,只差一个前缀**:`/poe1/api/…` 对 `/poe2/api/…`。
//! (不带前缀的老路径 `https://poe.ninja/api/data/index-state` 是 404。)
//!
//! - `index-state` / `build-index-state`:JSON 形状一模一样。当季联赛是
//!   Allflame(短名 `allflame`,snapshotName 也是 `allflame`),124,459 个角色。
//!   注意 `snapshotVersions` 里同一个联赛会出现两条(两个 version),
//!   `snapshot_for_url` 取的是先出现那条。
//! - `search`:同一份 protobuf,**28 列一个不多一个不少**,列 id、group、
//!   没消费的字段号全和 PoE2 对得上。字典引用 15 条(PoE2 是 7 条),多出来的
//!   八张是 `secondascendancy` / `bandit` / `atlasskill` / `mastery` /
//!   `runegraft` / `tattoo` / `vestigialmod` / `pantheon`;PoE1 没有 `spiritgems`。
//!   条目数:class 28、gem 823、keypassive 625、item 1380、anointed 448。
//! - `dictionary`:同一种 NDIC v2,但 PoE1 第一次让我们撞上了**两字节长度**
//!   (`mastery` 表里有 8 条超过 127 字节)。老解析器读到就报错,现在修好了。
//! - 经济:路径也只差前缀,但**分类名是单数**(`UniqueWeapon`,写复数就是 404),
//!   而且物品榜的 JSON 是老那套:没有 `core`,每行直接写
//!   `chaosValue`/`divineValue`/`exaltedValue`。交易所以 **chaos** 计价
//!   (`core.primary` = "chaos",1 divine = 358.9 chaos)。
//! - 角色详情:**和 PoE2 是同一种形状**(2026-09-09 实测,Allflame,
//!   快照 1707-20260908-44259,182KB)。`items[].itemData.mods` 底下还是
//!   `{"id":…,"stats":{…}}` 这种结构化词缀,配一份纯字符串的 `explicitMods`
//!   显示文本 —— 也就是说词缀统计那条链路两代通用。三处小差别:
//!   * `mods` 多两组 PoE1 才有的:`fractured`(裂隙)和 `enchant`(迷宫附魔),
//!     各配 `fracturedMods` / `enchantMods`。**漏声明不会报错,那一组只是
//!     静悄悄不进统计**,所以 `ModGroups` 现在把七组全列上了。
//!   * 关键天赋那个键叫 `keyStones`(大写 S),PoE2 叫 `keystones`。
//!   * 另有 `crucibleMods` / `scourgeMods` / `mutatedMods` / `vestigialMods`
//!     等几个只有显示文本、没有结构化对应物的数组,一律忽略。
//!
//!   顶层还多出 `atlasTreeName`、`banditChoice`、`masteries`、`tattoos`、
//!   `runegrafts`、`pantheonMajor/Minor`、`clusterJewels` 这些 PoE1 独有的字段,
//!   我们不声明,serde 照常忽略。

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use pnd_ninja::aggregate::{SlotModStat, unique_usage_from_facet};
use pnd_ninja::client::{
    Game, NinjaClient, character_url, currency_rates_url, index_state_url, search_url,
    unique_prices_url,
};
use pnd_ninja::economy::unique_types_for;
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
    game: Game,
    league: Option<String>,
    league_name: Option<String>,
    account: Option<String>,
    name: Option<String>,
    url: Option<String>,
    out: Option<PathBuf>,
    version: Option<String>,
    db: Option<PathBuf>,
    force: bool,
    limit: Option<u32>,
    filters: Vec<(String, String)>,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut args = Args {
        mode: None,
        game: Game::default(),
        league: None,
        league_name: None,
        account: None,
        name: None,
        url: None,
        out: None,
        version: None,
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
            "--character" => args.mode = Some("character"),
            "--raw" => {
                args.mode = Some("raw");
                args.url = Some(raw.next().ok_or("--raw needs a url")?);
            }
            "--out" => {
                args.out = Some(PathBuf::from(raw.next().ok_or("--out needs a path")?));
            }
            "--plan" => args.mode = Some("plan"),
            "--facets" => args.mode = Some("facets"),
            "--sample" => args.mode = Some("sample"),
            "--aggregate" => args.mode = Some("aggregate"),
            "--prices" => args.mode = Some("prices"),
            "--force" => args.force = true,
            "--game" => {
                let value = raw.next().ok_or("--game needs poe1 or poe2")?;
                // 不走 `Game::parse`:那一个认不出来就退回 PoE2(设置文件读得宽容
                // 是对的)。探针反过来 —— 打错一个字母还照跑,只会让人对着一份
                // PoE2 的输出琢磨半天 PoE1 为什么长这样。
                args.game = match value.trim().to_ascii_lowercase().as_str() {
                    "poe1" | "1" => Game::Poe1,
                    "poe2" | "2" => Game::Poe2,
                    other => return Err(format!("unknown game {other}").into()),
                };
            }
            "--account" => {
                args.account = Some(raw.next().ok_or("--account needs a value")?);
            }
            "--name" => {
                args.name = Some(raw.next().ok_or("--name needs a value")?);
            }
            "--version" => {
                args.version = Some(raw.next().ok_or("--version needs a value")?);
            }
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
    let client = || NinjaClient::for_game(args.game);
    match args.mode {
        Some("index") => print_index(&client(), &args),
        Some("search") => print_search(&client(), &args),
        Some("economy") => print_economy(&client(), &args),
        Some("character") => print_character(&client(), &args),
        Some("raw") => print_raw(&client(), &args),
        Some("plan") => print_plan(&args),
        Some("facets") => run_facets(&args),
        Some("sample") => run_sample(&args),
        Some("aggregate") => run_aggregate(&args),
        Some("prices") => run_prices(&args),
        _ => {
            eprintln!(
                "usage: ninja_probe [--game poe1|poe2]\n  \
                 --index\n  \
                 --search [--league <url>] [--class X] [--skills Y] [--items Z]\n  \
                 --economy [--league <name>]\n  \
                 --character [--league <url>] [--account A] [--name N]\n  \
                 --raw <url>\n  \
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

/// 每一个请求都先把 URL 印出来。这些路径没有文档,报告里那一列 URL + 状态
/// 就是"我们到底问了什么"的唯一证据。
fn hit(url: &str) {
    println!("GET {url}");
}

/// 一个还没有拼装函数的路径存不存在。404 本身就是答案,所以这里不把它当失败。
fn print_raw(client: &NinjaClient, args: &Args) -> Result<(), Box<dyn Error>> {
    let url = args.url.clone().ok_or("--raw needs a url")?;
    hit(&url);
    match client.raw_get(&url) {
        Ok(bytes) => {
            println!("  status 200, {} bytes", bytes.len());
            if let Some(path) = &args.out {
                std::fs::write(path, &bytes)?;
                println!("  saved to {}", path.display());
            }
            describe_json(&bytes);
        }
        Err(error) => println!("  {error}"),
    }
    Ok(())
}

/// 把一份未知 JSON 的形状讲清楚:顶层键、`lines`/`items` 第一行的键。
/// 不是 JSON 就印开头几十个字节 —— 那通常意味着我们拿到了一张 HTML 错误页。
fn describe_json(bytes: &[u8]) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(160)]);
        println!("  (not JSON) {head}");
        return;
    };
    match &value {
        serde_json::Value::Object(map) => {
            println!("  top-level keys: {}", keys(map));
            for key in ["lines", "currencyDetails", "items", "core"] {
                match map.get(key) {
                    Some(serde_json::Value::Array(rows)) => {
                        println!("  {key}: {} rows", rows.len());
                        if let Some(serde_json::Value::Object(first)) = rows.first() {
                            println!("    first row keys: {}", keys(first));
                            println!("    first row: {}", trim(&rows[0]));
                        }
                    }
                    Some(serde_json::Value::Object(inner)) => {
                        println!("  {key} keys: {}", keys(inner));
                    }
                    _ => {}
                }
            }
        }
        serde_json::Value::Array(rows) => println!("  top-level array with {} rows", rows.len()),
        other => println!("  scalar {other}"),
    }
}

fn keys(map: &serde_json::Map<String, serde_json::Value>) -> String {
    map.keys().cloned().collect::<Vec<_>>().join(", ")
}

/// 一行原文只印开头:探针是给人看的,一整条 4KB 的角色装备贴上来没人读得下去。
fn trim(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.len() <= 400 {
        return text;
    }
    format!("{}…", &text[..400])
}

/// 这一轮该问哪个快照。
///
/// `--version` 是给探路省配额用的:给了它就**不问 index-state**,直接拿
/// `--league` 当 overview 名(PoE1 的联赛短名和 snapshotName 恰好同名)。
/// 平时不该用它 —— 快照一天变好几次,写死的 version 隔天就查不到东西。
fn resolve_snapshot(
    client: &NinjaClient,
    args: &Args,
) -> Result<(String, String, String), Box<dyn Error>> {
    let league = args
        .league
        .clone()
        .unwrap_or_else(|| default_build_league(args));
    if let Some(version) = &args.version {
        return Ok((league.clone(), version.clone(), league));
    }
    hit(&index_state_url(args.game));
    let index = client.index_state()?;
    let snapshot = index
        .snapshot_for_url(&league)
        .ok_or_else(|| format!("no snapshot for league url {league}"))?;
    Ok((
        league,
        snapshot.version.clone(),
        snapshot.snapshot_name.clone(),
    ))
}

/// 一个角色详情的原文形状。词缀统计要的就是 `items[].itemData.mods.explicit[]`,
/// 所以这里印的是"那条路存不存在、长什么样",不是整包 JSON。
fn print_character(client: &NinjaClient, args: &Args) -> Result<(), Box<dyn Error>> {
    let (_league, version, snapshot_name) = resolve_snapshot(client, args)?;

    let (account, name) = match (&args.account, &args.name) {
        (Some(account), Some(name)) => (account.clone(), name.clone()),
        _ => {
            // 没给账号就自己去搜一个:探针不该逼人先跑一次别的模式抄个名字。
            pause();
            let filters: Vec<(&str, &str)> = Vec::new();
            hit(&search_url(args.game, &version, &snapshot_name, &filters));
            let response = client.search(&version, &snapshot_name, &filters)?;
            let first = response
                .character_refs(&[])
                .first()
                .cloned()
                .ok_or("the search returned no characters")?;
            println!("  picked {} / {}", first.account, first.name);
            (first.account, first.name)
        }
    };

    pause();
    let url = character_url(args.game, &version, &account, &name, &snapshot_name);
    hit(&url);
    let raw = client.character_raw(&version, &account, &name, &snapshot_name)?;
    println!("  status 200, {} bytes", raw.len());
    // `--out` 在这里也管用:一份角色详情要花一个请求配额,而配额一小时才
    // 一百来个。落一次盘,后面所有"这个字段到底长什么样"的问题都能离线问。
    if let Some(path) = &args.out {
        std::fs::write(path, raw.as_bytes())?;
        println!("  saved to {}", path.display());
    }
    describe_character(raw.as_bytes());

    println!();
    println!("== the same bytes through the production model ==");
    // 原文已经在手上,再打一个请求只为拿同一份 JSON 是浪费配额:
    // `NinjaClient::character` 做的也就是把这段字节喂给同一个 `CharacterDetail`。
    let detail: pnd_ninja::character::CharacterDetail = serde_json::from_str(&raw)?;
    println!(
        "  account {} name {} class {} level {} league {} items {} jewels {}",
        detail.account,
        detail.name,
        detail.class,
        detail.level,
        detail.league,
        detail.items.len(),
        detail.jewels.len()
    );
    for entry in detail.items.iter().take(4) {
        let data = &entry.item_data;
        println!(
            "  slot {:<3} {:<28} {:<26} rarity {:<8} frame {} inventoryId {:<12} \
             mods e/i/c {}/{}/{}  explicitMods {}",
            entry.item_slot,
            data.name,
            data.base_type,
            data.rarity,
            data.frame_type,
            data.inventory_id,
            data.mods.explicit.len(),
            data.mods.implicit.len(),
            data.mods.crafted.len(),
            data.explicit_mods.len()
        );
    }
    Ok(())
}

/// 只钻到 `items[0].itemData` 那一层:两代的差别如果有,就在这里。
fn describe_character(bytes: &[u8]) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        println!("  (not JSON)");
        return;
    };
    let Some(map) = value.as_object() else {
        return;
    };
    println!("  top-level keys: {}", keys(map));
    let Some(serde_json::Value::Array(items)) = map.get("items") else {
        println!("  (no items array)");
        return;
    };
    println!("  items: {} entries", items.len());
    let Some(first) = items.first().and_then(|item| item.as_object()) else {
        return;
    };
    println!("  items[0] keys: {}", keys(first));
    let Some(data) = first.get("itemData").and_then(|data| data.as_object()) else {
        println!("  (items[0] has no itemData)");
        return;
    };
    println!("  items[0].itemData keys: {}", keys(data));
    match data.get("mods").and_then(|mods| mods.as_object()) {
        Some(mods) => {
            println!("  items[0].itemData.mods keys: {}", keys(mods));
            if let Some(serde_json::Value::Array(explicit)) = mods.get("explicit") {
                println!(
                    "    mods.explicit[0]: {}",
                    trim(explicit.first().unwrap_or(&serde_json::Value::Null))
                );
            }
        }
        None => println!("  (items[0].itemData has no mods object)"),
    }
    if let Some(serde_json::Value::Array(explicit)) = data.get("explicitMods") {
        println!(
            "  items[0].itemData.explicitMods[0]: {}",
            trim(explicit.first().unwrap_or(&serde_json::Value::Null))
        );
    }
}

/// 联赛短名的默认值分代:PoE2 盯的是计划里那个联赛,PoE1 没有默认盯的,
/// 用 `standard` 兜底(它永远存在),真要看当季联赛就 `--league` 明说。
fn default_build_league(args: &Args) -> String {
    match args.game {
        Game::Poe1 => "standard".to_owned(),
        Game::Poe2 => DEFAULT_BUILD_LEAGUE.to_owned(),
    }
}

fn default_economy_league(args: &Args) -> String {
    match args.game {
        Game::Poe1 => "Standard".to_owned(),
        Game::Poe2 => DEFAULT_ECONOMY_LEAGUE.to_owned(),
    }
}

fn print_index(client: &NinjaClient, args: &Args) -> Result<(), Box<dyn Error>> {
    println!("game {}", args.game.as_str());
    hit(&index_state_url(args.game));
    let index = client.index_state()?;
    println!("  status 200");
    pause();
    hit(&pnd_ninja::client::build_index_state_url(args.game));
    let builds = client.build_index_state()?;
    println!("  status 200");
    println!();
    println!("== economy leagues ({}) ==", index.economy_leagues.len());
    for league in &index.economy_leagues {
        println!(
            "  {:<24} url {:<24} display {:<24} hardcore {} indexed {}",
            league.name, league.url, league.display_name, league.hardcore, league.indexed
        );
    }
    println!();

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
    leagues.sort_by_key(|league| std::cmp::Reverse(league.total));

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
        .unwrap_or_else(|| default_build_league(args));

    println!("game {}", args.game.as_str());
    hit(&index_state_url(args.game));
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
    hit(&search_url(
        args.game,
        &snapshot.version,
        &snapshot.snapshot_name,
        &filters,
    ));
    let response = client.search(&snapshot.version, &snapshot.snapshot_name, &filters)?;
    println!();
    println!("total matching characters: {}", response.total);

    print_columns(&response);
    print_dictionaries(&response);

    let dictionaries = fetch_dictionaries(client, args.game, &response)?;
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
    game: Game,
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
        hit(&pnd_ninja::client::dictionary_url(game, &reference.sha1));
        // 一张字典读不动就跳过它,别把整轮探针带走:探针的价值就在于"对面
        // 又变成什么样了",而那个答案往往就写在后面还没打出来的那几张表里。
        let entries = match client.dictionary(&reference.sha1) {
            Ok(entries) => entries,
            Err(error) => {
                println!("  FAILED dictionary {}: {error}", reference.key);
                continue;
            }
        };
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
        .unwrap_or_else(|| default_economy_league(args));
    println!("game {}  league {league}", args.game.as_str());

    hit(&currency_rates_url(args.game, &league));
    let exchange = client.currency_rates(&league)?;
    println!("  status 200");
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

    for type_name in unique_types_for(args.game) {
        pause();
        hit(&unique_prices_url(args.game, &league, type_name));
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
    // PoE1 不给默认联赛:留空的意思是"用当季挑战联赛",而那正是采样管线
    // 自己会从 index-state 里认出来的东西(`--league` 明说了就以它为准)。
    let league_url = args.league.clone().unwrap_or_else(|| match args.game {
        Game::Poe1 => String::new(),
        Game::Poe2 => DEFAULT_BUILD_LEAGUE.to_owned(),
    });
    let league_name = args.league_name.clone().unwrap_or_else(|| {
        if league_url == DEFAULT_BUILD_LEAGUE {
            DEFAULT_ECONOMY_LEAGUE.to_owned()
        } else {
            String::new()
        }
    });
    let mut config = SamplerConfig::for_game(args.game, league_url, league_name);
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
        SamplerEvent::Prices { types_done } => {
            println!("[prices]     {types_done} unique types refreshed");
        }
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
        rows.sort_by_key(|row| std::cmp::Reverse(row.characters));
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
