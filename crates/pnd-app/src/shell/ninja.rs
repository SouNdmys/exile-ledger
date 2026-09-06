//! ninja 两页背后的那一半:`ninja.sqlite` 里的缓存怎么读出来,以及"刷新"
//! 按钮怎么把采样线程接上。
//!
//! 页面只管画,数据全从这里来 —— 分开的理由和 [`super::link`] 一样:读库和
//! 画表的错误处理、借用形状都不一样,混在一页里每加一个查询都要重读整页。
//!
//! 三件事在这里定:
//!
//! - **联赛短名。** builds 接口认 `forbiddenrites`,`settings.json` 里存的是
//!   显示名 `Forbidden Rites`,中间差一个 [`league_url_guess`]。界面手上没有
//!   index-state(那是采样线程去取的),所以这里只能猜;真正的答案由
//!   `IndexState::league_url_for_name` 在采样线程里给出。
//! - **一次读齐。** 换筛选器、翻页都不该再打一次库:开一次连接把这一轮的
//!   快照、分区清单、暗金榜、词缀统计全读进内存(几千行而已),之后全在内存里筛。
//! - **采样线程的生死。** 句柄挂在 `AppShell` 上,丢掉它就是取消 + 收摊;
//!   一轮没跑完的时候"刷新"按钮是灰的,免得同一个库上并排跑两条采样线程。

use std::path::PathBuf;

use gpui::Context;

use pnd_ninja::aggregate::SlotModStat;
use pnd_ninja::economy::UNIQUE_TYPES;
use pnd_ninja::index_state::league_url_guess;
use pnd_ninja::plan::query_from_key;
use pnd_runtime::{SamplerConfig, SamplerEvent, SamplerHandle, SamplerStage};
use pnd_settings::AppSettings;
use pnd_storage::{NinjaStore, SnapshotRow, StorageError};

use super::AppShell;
use crate::i18n::{self, Text};

/// 一件暗金在榜上的一行:人气来自 builds 的 `items` 分面,价格来自经济接口。
///
/// 两边在这里就拼好,而不是画表时现拼:表格那边只剩"怎么显示"一件事,
/// 于是它是纯函数,测得动。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UniqueRow {
    pub name: String,
    /// 这个分区里有多少个角色穿着它。
    pub users: u64,
    /// 占这个分区总人数的百分之几。分区没有 total 时是 0。
    pub share_percent: f64,
    /// 参考价,exalted × 1000。`None` = 经济接口里没有这件东西
    /// (新暗金、或者压根没人挂单)。
    pub price_milli: Option<i64>,
    pub listings: Option<i64>,
    /// 7 天涨跌,百分比。
    pub change_percent: Option<f64>,
}

/// 界面手上的那份 ninja 缓存。空的那份(还没采过)也是合法值:
/// 两页照样画得出来,只是表里一句"按刷新采一轮"。
#[derive(Clone, Debug, Default)]
pub struct NinjaData {
    /// builds 接口用的联赛短名,库里所有行都按它分。
    pub league_url: String,
    pub snapshot: Option<SnapshotRow>,
    /// 跑完了的分区键,就是筛选器里那一串。
    pub partition_keys: Vec<String>,
    /// 当前选中的分区键(`""` = 全联赛)。
    pub selected_partition: String,
    /// 选中分区匹配到多少角色 —— 占比的分母。
    pub partition_total: Option<u64>,
    pub uniques: Vec<UniqueRow>,
    /// 角色详情抓到手 / 还在队列里的个数。
    pub characters_done: u32,
    pub characters_pending: u32,
    /// 参考价里最旧的那次抓取时刻。
    pub prices_fetched_at: Option<i64>,
    /// 这一轮全部的词缀统计。筛选是在内存里做的:1,500 行而已,
    /// 每换一次下拉都回去打一次库不值当。
    pub mods: Vec<SlotModStat>,
}

impl NinjaData {
    /// 一份只知道自己盯着哪个联赛的空缓存。
    #[must_use]
    pub fn empty(league_url: String) -> NinjaData {
        NinjaData {
            league_url,
            ..NinjaData::default()
        }
    }

    /// 这一轮的快照号。没有快照就没有版本,库里所有按 version 分的表也都查不到。
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.snapshot.as_ref().map(|row| row.version.as_str())
    }
}

/// 分区键 → 人话。库里存的是查询串(`class=Deadeye&skills=Snipe`),
/// 下拉里得写成看得懂的东西。
#[must_use]
pub fn partition_label(key: &str, text: &'static Text) -> String {
    let query = query_from_key(key);
    let value = |name: &str| {
        query
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.clone())
    };
    match (value("class"), value("skills"), value("items")) {
        (None, None, None) if query.is_empty() => text.uniques_partition_whole.to_owned(),
        (Some(class), Some(skill), _) => i18n::fill(
            text.uniques_partition_class_skill,
            &[&i18n::fill(text.uniques_partition_class, &[&class]), &skill],
        ),
        (Some(class), None, _) => i18n::fill(text.uniques_partition_class, &[&class]),
        (None, Some(skill), _) => i18n::fill(text.uniques_partition_skill, &[&skill]),
        (None, None, Some(item)) => i18n::fill(text.uniques_partition_item, &[&item]),
        // 认不出来的键原样显示:一条看得懂的怪字符串,好过一条编出来的说明。
        _ => key.to_owned(),
    }
}

/// 占比。分母缺席(这个分区还没跑完)时是 0,而不是一个编出来的数。
#[must_use]
pub fn share_percent(count: u64, total: Option<u64>) -> f64 {
    match total {
        Some(total) if total > 0 => count as f64 * 100.0 / total as f64,
        _ => 0.0,
    }
}

/// 一行词缀统计里"带着它的角色占这个部位样本的百分之几"。
#[must_use]
pub fn mod_share_percent(stat: &SlotModStat) -> f64 {
    if stat.sample_size == 0 {
        return 0.0;
    }
    f64::from(stat.characters) * 100.0 / f64::from(stat.sample_size)
}

/// 百分比的写法。小数点后一位:0.5% 和 0.6% 得分得开,11.42% 又没人关心第二位。
#[must_use]
pub fn percent_text(value: f64, text: &'static Text) -> String {
    format!("{value:.1}{}", text.common_percent)
}

// ---------------------------------------------------------------------
// 读库
// ---------------------------------------------------------------------

/// 把这一轮的缓存整份读进内存。
///
/// `want_partition` 是用户上次选的那个分区;它这一轮不在了(换快照了)就退回
/// 全联赛,而不是让筛选器指着一条查不到东西的键。
pub fn load(
    store: &NinjaStore,
    league_url: &str,
    want_partition: &str,
) -> Result<NinjaData, StorageError> {
    let (characters_pending, characters_done, _failed) = store.character_counts(league_url)?;
    let mut data = NinjaData {
        league_url: league_url.to_owned(),
        characters_done,
        characters_pending,
        prices_fetched_at: store.unique_prices_age(league_url)?,
        ..NinjaData::default()
    };

    let Some(snapshot) = store.latest_snapshot(league_url)? else {
        return Ok(data);
    };
    let version = snapshot.version.clone();
    data.partition_keys = store.partition_keys(league_url, &version)?;
    data.selected_partition = if data.partition_keys.iter().any(|key| key == want_partition) {
        want_partition.to_owned()
    } else {
        String::new()
    };
    let (total, uniques) = load_uniques(store, league_url, &version, &data.selected_partition)?;
    data.partition_total = total;
    data.uniques = uniques;
    // 阈值给 0:筛掉哪些行是界面上那个开关的事,库里读全份。
    data.mods = store.slot_mods(league_url, &version, None, None, 0.0)?;
    data.snapshot = Some(snapshot);
    Ok(data)
}

/// 一个分区的暗金榜:人气 × 参考价。换个分区只用重跑这一段。
///
/// 拼价格是库里的 `LEFT JOIN` 干的,不是这里一行一句 SQL:全联赛四百多件暗金,
/// 一行一次查询就是四百多次往返,换一次分区筛选器界面要卡一下。
pub fn load_uniques(
    store: &NinjaStore,
    league_url: &str,
    version: &str,
    partition_key: &str,
) -> Result<(Option<u64>, Vec<UniqueRow>), StorageError> {
    let total = store.partition_total(league_url, version, partition_key)?;
    let rows = store
        .unique_usage_with_prices(league_url, version, partition_key)?
        .into_iter()
        .map(|row| UniqueRow {
            name: row.name,
            users: row.users,
            share_percent: share_percent(row.users, total),
            price_milli: row.price_milli,
            listings: row.listings,
            change_percent: row.change_percent,
        })
        .collect();
    Ok((total, rows))
}

// ---------------------------------------------------------------------
// 采样线程
// ---------------------------------------------------------------------

/// 设置 → 一轮采样的配置。
///
/// `stop_after` 一律到 `Aggregated`:两页要的东西分别在第 4 步和第 7 步,
/// 停在中间会让词缀热度页永远是空的。24 小时内已经跑到这一步的话,
/// 采样管线自己会跳过,一个请求都不发。
#[must_use]
pub fn sampler_config(settings: &AppSettings, db_path: PathBuf) -> SamplerConfig {
    SamplerConfig {
        league_url: league_url_guess(&settings.league),
        league_name: settings.league.clone(),
        tuning: settings.ninja.clone(),
        user_agent: settings.user_agent(),
        db_path,
        stop_after: SamplerStage::Aggregated,
        force: false,
        max_characters: None,
    }
}

/// 阶段名。进度条上写的是"分区 15/61"而不是"facets 15/61"。
#[must_use]
pub fn stage_word(stage: SamplerStage, text: &'static Text) -> &'static str {
    match stage {
        SamplerStage::Planned => text.ninja_stage_planned,
        SamplerStage::Facets => text.ninja_stage_facets,
        SamplerStage::Characters => text.ninja_stage_characters,
        SamplerStage::Aggregated => text.ninja_stage_aggregated,
    }
}

/// 一条采样事件 → 状态行上的一句话。
///
/// 纯函数带测试:这一句是采样跑起来之后用户唯一看得见的东西,而采样一跑
/// 就是半小时,没人有空回头核对每一条事件长什么样。
#[must_use]
pub fn sampler_status(event: &SamplerEvent, text: &'static Text) -> String {
    match event {
        SamplerEvent::Started { version, .. } => i18n::fill(text.ninja_status_started, &[version]),
        SamplerEvent::Skipped { reason } => i18n::fill(text.ninja_status_skipped, &[reason]),
        SamplerEvent::Progress {
            stage,
            done,
            total,
            note,
        } => {
            let head = format!("{} {done}/{total}", stage_word(*stage, text));
            if note.trim().is_empty() {
                head
            } else {
                format!("{head} · {note}")
            }
        }
        SamplerEvent::StageDone(stage) => {
            i18n::fill(text.ninja_status_stage_done, &[stage_word(*stage, text)])
        }
        SamplerEvent::Prices { types_done } => i18n::fill(
            text.ninja_status_prices,
            &[&types_done.to_string(), &UNIQUE_TYPES.len().to_string()],
        ),
        SamplerEvent::Finished { .. } => text.ninja_status_finished.to_owned(),
        SamplerEvent::Failed(reason) => i18n::fill(text.ninja_status_failed, &[reason]),
    }
}

/// 这一轮跑完了没有。跑完了才把"刷新"按钮点亮。
#[must_use]
pub fn is_final(event: &SamplerEvent) -> bool {
    matches!(
        event,
        SamplerEvent::Finished { .. } | SamplerEvent::Failed(_) | SamplerEvent::Skipped { .. }
    )
}

/// 这条事件之后要不要重读一遍库。
///
/// 阶段完成和价格刷新会往库里写新行,不重读的话页面上还是上一轮的数;
/// 进度事件不写库,每条都重读一次会让界面在采样期间一直重建表格。
#[must_use]
pub fn changes_the_cache(event: &SamplerEvent) -> bool {
    matches!(
        event,
        SamplerEvent::StageDone(_) | SamplerEvent::Prices { .. } | SamplerEvent::Finished { .. }
    )
}

impl AppShell {
    /// 把采样线程的事件抽干。返回"有没有东西变了"。
    pub(crate) fn drain_sampler_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            let Some(event) = self
                .sampler
                .as_ref()
                .and_then(SamplerHandle::try_next_event)
            else {
                break;
            };
            self.on_sampler_event(&event);
            changed = true;
        }
        changed
    }

    fn on_sampler_event(&mut self, event: &SamplerEvent) {
        let line = sampler_status(event, self.text());
        // 进度事件一秒一条,全记进日志会把日志冲掉;状态行上照样看得见。
        if !matches!(event, SamplerEvent::Progress { .. }) {
            self.push_log(format!("ninja: {line}"));
        }
        self.sampler_line.clone_from(&line);
        self.set_notice(line);
        if changes_the_cache(event) {
            self.reload_ninja();
        }
        if is_final(event) {
            self.sampler_busy = false;
        }
    }

    /// "刷新":起一条采样线程。已经在跑就什么都不做 —— 按钮那时是灰的,
    /// 走到这里只可能是键盘或者双击。
    pub(crate) fn start_ninja_sampling(&mut self, cx: &mut Context<Self>) {
        if self.sampler_busy {
            return;
        }
        let config = sampler_config(&self.settings, crate::ninja_db_path());
        self.push_log(format!(
            "ninja: sampling {} ({}) into {}",
            config.league_name,
            config.league_url,
            config.db_path.display()
        ));
        // 上一条句柄在这里被丢掉:它的 `Drop` 会取消并等线程收摊。
        // 走到这儿说明上一轮早就结束了,join 是立刻返回的。
        self.sampler = Some(SamplerHandle::start(config));
        self.sampler_busy = true;
        let text = self.text();
        self.sampler_line = text.ninja_status_starting.to_owned();
        self.set_notice(text.ninja_status_starting.to_owned());
        cx.notify();
    }

    /// 整份重读。库打不开就只记一行日志:两页空着,别的功能照常。
    pub(crate) fn reload_ninja(&mut self) {
        let league_url = self.ninja.league_url.clone();
        let wanted = self.ninja.selected_partition.clone();
        let loaded = match &self.ninja_store {
            Some(store) => load(store, &league_url, &wanted),
            None => return,
        };
        match loaded {
            Ok(data) => {
                self.ninja = data;
                self.uniques_dirty = true;
                self.mods_dirty = true;
                self.ninja_filters_dirty = true;
            }
            Err(error) => self.push_log(format!("could not read the ninja cache: {error}")),
        }
    }

    /// 只重读暗金榜。换一次分区筛选器走的是这条路 —— 词缀统计和它无关,
    /// 1,500 行没必要跟着重读一遍。
    pub(crate) fn reload_ninja_uniques(&mut self) {
        let league_url = self.ninja.league_url.clone();
        let partition = self.ninja.selected_partition.clone();
        let Some(version) = self.ninja.version().map(ToOwned::to_owned) else {
            return;
        };
        let loaded = match &self.ninja_store {
            Some(store) => load_uniques(store, &league_url, &version, &partition),
            None => return,
        };
        match loaded {
            Ok((total, rows)) => {
                self.ninja.partition_total = total;
                self.ninja.uniques = rows;
                self.uniques_dirty = true;
            }
            Err(error) => self.push_log(format!("could not read the unique heat: {error}")),
        }
    }

    /// 联赛改了就换一份缓存视图 —— 库里的行是按联赛短名分的。
    pub(crate) fn resync_ninja_league(&mut self) {
        let league_url = league_url_guess(&self.settings.league);
        if league_url == self.ninja.league_url {
            return;
        }
        self.ninja = NinjaData::empty(league_url);
        self.reload_ninja();
    }
}

#[cfg(test)]
mod ninja_tests {
    use super::*;
    use crate::i18n;

    fn stat(slot: &str, characters: u32, sample: u32) -> SlotModStat {
        SlotModStat {
            slot: slot.to_owned(),
            rarity: "Rare".to_owned(),
            mod_kind: "explicit".to_owned(),
            stat_id: "base_maximum_life".to_owned(),
            mod_family: "IncreasedLife".to_owned(),
            characters,
            occurrences: characters,
            sample_size: sample,
            p25: Some(108.0),
            p50: Some(176.0),
            p75: Some(211.0),
        }
    }

    /// 分区键在下拉里必须是人话:一串 `class=X&skills=Y` 没人认得出
    /// 自己刚才选的是什么。
    #[test]
    fn every_partition_shape_reads_like_a_person_wrote_it() {
        let text = &i18n::ENGLISH;
        assert_eq!(partition_label("", text), "Whole league");
        assert_eq!(partition_label("class=Deadeye", text), "Class Deadeye");
        assert_eq!(partition_label("skills=Snipe", text), "Skill Snipe");
        assert_eq!(
            partition_label("items=Wake of Destruction", text),
            "Wearing Wake of Destruction"
        );
        assert_eq!(
            partition_label("class=Deadeye&skills=Snipe", text),
            "Class Deadeye · Snipe"
        );
        // 认不出来的键原样显示,而不是变成"全联赛"那种会骗人的默认值。
        assert_eq!(partition_label("weird=thing", text), "weird=thing");

        let zh = &i18n::SIMPLIFIED_CHINESE;
        assert_eq!(partition_label("", zh), "全联赛");
        assert_eq!(partition_label("class=Deadeye", zh), "职业 Deadeye");
    }

    /// 分母缺席时占比是 0,不是一个编出来的数,也不是 NaN。
    #[test]
    fn a_missing_denominator_gives_a_zero_share() {
        assert!((share_percent(7_464, Some(65_371)) - 11.418).abs() < 0.01);
        assert_eq!(share_percent(10, None), 0.0);
        assert_eq!(share_percent(10, Some(0)), 0.0);

        assert!((mod_share_percent(&stat("BodyArmour", 13, 47)) - 27.66).abs() < 0.01);
        assert_eq!(mod_share_percent(&stat("BodyArmour", 13, 0)), 0.0);
        assert_eq!(percent_text(11.418, &i18n::ENGLISH), "11.4%");
    }

    /// 每一种事件都得说得出一句人话,两种语言都是。
    #[test]
    fn every_sampler_event_has_something_to_say() {
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            for event in [
                SamplerEvent::Started {
                    version: "1733-20260906-24495".to_owned(),
                    snapshot_name: "forbidden-rites".to_owned(),
                    total_characters: 65_371,
                },
                SamplerEvent::Skipped {
                    reason: "already at stage aggregated from 3h00m ago".to_owned(),
                },
                SamplerEvent::Progress {
                    stage: SamplerStage::Facets,
                    done: 15,
                    total: 61,
                    note: String::new(),
                },
                SamplerEvent::StageDone(SamplerStage::Characters),
                SamplerEvent::Prices { types_done: 6 },
                SamplerEvent::Finished {
                    version: "1733".to_owned(),
                },
                SamplerEvent::Failed("no network".to_owned()),
            ] {
                assert!(
                    !sampler_status(&event, text).trim().is_empty(),
                    "{language}: {event:?} 没有话说"
                );
            }
        }
    }

    /// 进度那一条要能直接读出"跑到哪了":阶段 + 几分之几。
    #[test]
    fn a_progress_line_carries_the_stage_and_the_count() {
        let text = &i18n::ENGLISH;
        let bare = sampler_status(
            &SamplerEvent::Progress {
                stage: SamplerStage::Facets,
                done: 15,
                total: 61,
                note: String::new(),
            },
            text,
        );
        assert_eq!(bare, "facets 15/61");

        let noted = sampler_status(
            &SamplerEvent::Progress {
                stage: SamplerStage::Characters,
                done: 120,
                total: 2_000,
                note: "KingPinUwU".to_owned(),
            },
            text,
        );
        assert_eq!(noted, "characters 120/2000 · KingPinUwU");

        assert!(
            sampler_status(
                &SamplerEvent::Skipped {
                    reason: "cached 3h00m ago".to_owned()
                },
                text
            )
            .contains("cached 3h00m ago")
        );
    }

    /// 跳过、跑完、失败都算"这一轮结束了",按钮要重新点亮;
    /// 进度和阶段完成不算 —— 那时线程还在跑。
    #[test]
    fn only_the_last_event_of_a_run_re_enables_the_button() {
        assert!(is_final(&SamplerEvent::Finished {
            version: "1733".to_owned()
        }));
        assert!(is_final(&SamplerEvent::Failed("boom".to_owned())));
        assert!(is_final(&SamplerEvent::Skipped {
            reason: "fresh".to_owned()
        }));
        assert!(!is_final(&SamplerEvent::StageDone(SamplerStage::Facets)));
        assert!(!is_final(&SamplerEvent::Prices { types_done: 6 }));

        // 写过库的那几条才值得重读一遍缓存。
        assert!(changes_the_cache(&SamplerEvent::StageDone(
            SamplerStage::Facets
        )));
        assert!(changes_the_cache(&SamplerEvent::Prices { types_done: 6 }));
        assert!(!changes_the_cache(&SamplerEvent::Progress {
            stage: SamplerStage::Facets,
            done: 1,
            total: 61,
            note: String::new(),
        }));
    }

    /// 界面这一侧永远跑整轮:停在分面那一步的话,词缀热度页永远是空的。
    /// 联赛的两个名字也必须各归各位,混用就是每个接口都 404。
    #[test]
    fn the_refresh_button_asks_for_a_full_run_of_the_settings_league() {
        let mut settings = AppSettings {
            league: "Forbidden Rites".to_owned(),
            ..AppSettings::default()
        };
        settings.ninja.refresh_hours = 12;
        settings.ninja.sample_target = 500;

        let config = sampler_config(&settings, PathBuf::from(r"C:\tmp\ninja.sqlite"));
        assert_eq!(config.league_url, "forbiddenrites");
        assert_eq!(config.league_name, "Forbidden Rites");
        assert_eq!(config.stop_after, SamplerStage::Aggregated);
        // "24 小时内不重跑"是采样管线自己的规矩,界面不许强制绕过它。
        assert!(!config.force);
        assert_eq!(config.max_characters, None);
        assert_eq!(config.tuning.refresh_hours, 12);
        assert_eq!(config.tuning.sample_target, 500);
        assert_eq!(config.db_path, PathBuf::from(r"C:\tmp\ninja.sqlite"));
    }

    /// 库是空的时候读出来的是一份空缓存,而不是一个错。
    #[test]
    fn an_empty_cache_loads_as_an_empty_view() {
        let store = NinjaStore::open_in_memory().expect("store");
        let data = load(&store, "forbiddenrites", "").expect("load");
        assert_eq!(data.league_url, "forbiddenrites");
        assert!(data.snapshot.is_none());
        assert!(data.version().is_none());
        assert!(data.partition_keys.is_empty());
        assert!(data.uniques.is_empty());
        assert!(data.mods.is_empty());
        assert_eq!(data.characters_done, 0);
        assert_eq!(data.prices_fetched_at, None);
    }

    /// 一份跑完的缓存要能整份读回来:分区清单、暗金榜(拼上价格)、词缀统计。
    #[test]
    fn a_finished_run_loads_with_its_prices_attached() {
        let store = NinjaStore::open_in_memory().expect("store");
        let version = "1733-20260906-24495";
        store
            .upsert_snapshot(&SnapshotRow {
                league_url: "forbiddenrites".to_owned(),
                version: version.to_owned(),
                snapshot_name: "forbidden-rites".to_owned(),
                total_characters: 65_371,
                stage: pnd_storage::SnapshotStage::Aggregated,
                started_at: 1_000,
                finished_at: Some(2_000),
            })
            .expect("snapshot");
        let partitions = vec![
            pnd_ninja::plan::Partition::new(pnd_ninja::plan::PartitionTier::Whole, Vec::new()),
            pnd_ninja::plan::Partition::new(
                pnd_ninja::plan::PartitionTier::Class,
                vec![("class".to_owned(), "Deadeye".to_owned())],
            ),
        ];
        store
            .enqueue_partitions("forbiddenrites", version, &partitions)
            .expect("enqueue");
        let facets = vec![
            (
                "items".to_owned(),
                "Wake of Destruction".to_owned(),
                7_464u64,
            ),
            // 稀有度桶不该出现在榜上。
            ("items".to_owned(), "Rare Ring".to_owned(), 62_878),
        ];
        store
            .complete_partition("forbiddenrites", version, "", 65_371, &facets, &[], 2_000)
            .expect("complete");
        let deadeye = vec![("items".to_owned(), "Beira's Anguish".to_owned(), 1_000u64)];
        store
            .complete_partition(
                "forbiddenrites",
                version,
                "class=Deadeye",
                4_000,
                &deadeye,
                &[],
                2_000,
            )
            .expect("complete");
        let line: pnd_ninja::economy::UniquePriceLine = serde_json::from_str(
            r#"{"name":"Wake of Destruction","baseType":"Wrapped Greathelm","category":"Helmet",
                 "primaryValue":29.9,"listingCount":131,"sparkLine":{"totalChange":-4.5,"data":[]}}"#,
        )
        .expect("line");
        store
            .replace_unique_prices("forbiddenrites", "UniqueArmours", &[line], 9_000)
            .expect("prices");
        store
            .replace_item_mods("forbiddenrites", version, &[stat("BodyArmour", 13, 47)])
            .expect("mods");

        let data = load(&store, "forbiddenrites", "").expect("load");
        assert_eq!(data.version(), Some(version));
        assert_eq!(data.partition_keys, vec!["", "class=Deadeye"]);
        assert_eq!(data.selected_partition, "");
        assert_eq!(data.partition_total, Some(65_371));
        assert_eq!(data.prices_fetched_at, Some(9_000));
        assert_eq!(data.mods.len(), 1);

        assert_eq!(data.uniques.len(), 1, "稀有度桶不进榜");
        let top = &data.uniques[0];
        assert_eq!(top.name, "Wake of Destruction");
        assert_eq!(top.users, 7_464);
        assert_eq!(top.price_milli, Some(29_900));
        assert_eq!(top.listings, Some(131));
        assert_eq!(top.change_percent, Some(-4.5));
        assert!((top.share_percent - 11.418).abs() < 0.01);

        // 换一个分区只换榜,分母跟着换。
        let deadeye = load(&store, "forbiddenrites", "class=Deadeye").expect("load");
        assert_eq!(deadeye.selected_partition, "class=Deadeye");
        assert_eq!(deadeye.partition_total, Some(4_000));
        assert_eq!(deadeye.uniques[0].name, "Beira's Anguish");
        assert_eq!(deadeye.uniques[0].price_milli, None, "没挂单就没有参考价");

        // 上次选的分区这一轮没有了(换快照)就退回全联赛,而不是指着一条空键。
        let gone = load(&store, "forbiddenrites", "class=Nobody").expect("load");
        assert_eq!(gone.selected_partition, "");
        assert_eq!(gone.uniques[0].name, "Wake of Destruction");
    }
}
