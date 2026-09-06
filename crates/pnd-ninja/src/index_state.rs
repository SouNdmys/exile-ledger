//! `/poe2/api/data/index-state` 和 `/build-index-state` 的模型。
//!
//! 这两个是每轮采样的起点:快照 `version` 一天变好几次,而 builds 的所有其它
//! 接口都要把它拼进 URL,所以每轮都得先来这里问一次。
//!
//! 所有字段都是 `#[serde(default)]`:poe.ninja 没有文档也没有承诺,少一个字段
//! 不该让整轮采样失败;未知字段 serde 默认就忽略。

use serde::Deserialize;

/// 联赛引用。`url` 是接口里用的短名("forbiddenrites"),`name` 是经济接口
/// 要的显示名("Forbidden Rites")——两者不能混用。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeagueRef {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub hardcore: bool,
    #[serde(default)]
    pub indexed: bool,
}

/// 一个联赛当前的快照。`version` 每次重新索引都变,`snapshot_name` 稳定。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotVersion {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub snapshot_name: String,
    #[serde(default)]
    pub time_machine_labels: Vec<String>,
    #[serde(default)]
    pub overview_type: i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexState {
    #[serde(default)]
    pub economy_leagues: Vec<LeagueRef>,
    #[serde(default)]
    pub snapshot_versions: Vec<SnapshotVersion>,
    #[serde(default)]
    pub build_leagues: Vec<LeagueRef>,
    #[serde(default)]
    pub old_build_leagues: Vec<LeagueRef>,
}

impl IndexState {
    /// 按联赛短名找快照。找不到就是这个联赛这轮没被索引,调用方该跳过它。
    #[must_use]
    pub fn snapshot_for_url(&self, url: &str) -> Option<&SnapshotVersion> {
        self.snapshot_versions
            .iter()
            .find(|snapshot| snapshot.url == url)
    }

    /// 显示名 → 接口用的短名,**问 poe.ninja 自己要答案**。
    ///
    /// `snapshotVersions` 每一条都同时带着两个名字,所以只要这一轮拿到过
    /// index-state,短名就不用猜。手上没有 index-state 的调用方(比如界面,
    /// 它只有 `settings.json` 里那个显示名)才退回 [`league_url_guess`]。
    ///
    /// 大小写不敏感:用户在设置里打的是"forbidden rites"也该认出来。
    #[must_use]
    pub fn league_url_for_name(&self, name: &str) -> Option<&str> {
        self.snapshot_versions
            .iter()
            .find(|snapshot| snapshot.name.eq_ignore_ascii_case(name))
            .map(|snapshot| snapshot.url.as_str())
    }
}

/// 显示名 → 短名的**猜法**:去掉所有非字母数字,再全小写。
/// `Forbidden Rites` → `forbiddenrites`,`Berek's Grip` 里的撇号一样掉。
///
/// 这是猜测不是事实:poe.ninja 的短名由它自己定,规律上一直是这个,但没有
/// 任何接口承诺过。手上有 [`IndexState`] 时一律用 [`IndexState::league_url_for_name`],
/// 这个函数只在拿不到 index-state 时兜底。
#[must_use]
pub fn league_url_guess(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// 一个职业在某联赛里的占比。`trend` 是 poe.ninja 自己的涨跌标记。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassShare {
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub percentage: f64,
    #[serde(default)]
    pub trend: i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeagueBuild {
    #[serde(default)]
    pub league_name: String,
    #[serde(default)]
    pub league_url: String,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub hardcore: bool,
    #[serde(default)]
    pub statistics: Vec<ClassShare>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildIndexState {
    #[serde(default)]
    pub league_builds: Vec<LeagueBuild>,
}

#[cfg(test)]
mod index_state_tests {
    use super::*;

    /// 线上原文的形状,只是删掉了几十个联赛。注意 `passiveTree` 这个我们不认识的
    /// 字段留在里面:它就是"未知字段必须被忽略"的守门测试。
    const INDEX_STATE_JSON: &str = r#"{
        "economyLeagues": [
            {"name":"Forbidden Rites","url":"forbiddenrites","displayName":"Forbidden Rites","hardcore":false,"indexed":false}
        ],
        "oldEconomyLeagues": [
            {"name":"Fate of the Vaal","url":"vaal","displayName":"Fate of the Vaal","hardcore":false,"indexed":false}
        ],
        "snapshotVersions": [
            {"url":"forbiddenrites","name":"Forbidden Rites","timeMachineLabels":["hour-6","day-1"],
             "version":"1508-20260906-55820","snapshotName":"forbidden-rites","overviewType":0,
             "passiveTree":"PassiveTree-0.5"},
            {"url":"runesofaldur","name":"Runes of Aldur","timeMachineLabels":[],
             "version":"1511-20260906-26201","snapshotName":"runes-of-aldur","overviewType":0}
        ],
        "buildLeagues": [
            {"name":"Forbidden Rites","url":"forbiddenrites","displayName":"Forbidden Rites","hardcore":false,"indexed":false},
            {"name":"HC Forbidden Rites","url":"forbiddenriteshc","displayName":"HC Forbidden Rites","hardcore":true,"indexed":false}
        ],
        "oldBuildLeagues": [
            {"name":"Fate of the Vaal","url":"vaal","displayName":"Fate of the Vaal","hardcore":false,"indexed":false}
        ]
    }"#;

    #[test]
    fn parses_index_state() {
        let state: IndexState = serde_json::from_str(INDEX_STATE_JSON).unwrap();
        assert_eq!(state.economy_leagues.len(), 1);
        assert_eq!(state.economy_leagues[0].name, "Forbidden Rites");
        assert_eq!(state.build_leagues.len(), 2);
        assert!(state.build_leagues[1].hardcore);
        assert_eq!(state.old_build_leagues[0].url, "vaal");
    }

    #[test]
    fn finds_the_snapshot_for_a_league_url() {
        let state: IndexState = serde_json::from_str(INDEX_STATE_JSON).unwrap();
        let snapshot = state.snapshot_for_url("forbiddenrites").unwrap();
        assert_eq!(snapshot.version, "1508-20260906-55820");
        assert_eq!(snapshot.snapshot_name, "forbidden-rites");
        assert_eq!(snapshot.time_machine_labels, ["hour-6", "day-1"]);
        assert!(state.snapshot_for_url("nosuchleague").is_none());
    }

    /// 猜法的规矩:空格、撇号、连字符全掉,剩下的全小写。
    #[test]
    fn the_guessed_short_name_drops_everything_but_letters_and_digits() {
        assert_eq!(league_url_guess("Forbidden Rites"), "forbiddenrites");
        assert_eq!(league_url_guess("Standard"), "standard");
        assert_eq!(league_url_guess("Hardcore SSF"), "hardcoressf");
        // 撇号、连字符、点号都不在短名里(poe.ninja 自己就是这么拼的)。
        assert_eq!(league_url_guess("Berek's Grip League"), "bereksgripleague");
        assert_eq!(league_url_guess("Rise of the Abyssal"), "riseoftheabyssal");
        assert_eq!(league_url_guess("Fate-of the Vaal"), "fateofthevaal");
        assert_eq!(league_url_guess("Settlers 2"), "settlers2");
        assert_eq!(league_url_guess(""), "");
    }

    /// 有 index-state 在手就不用猜:两个名字本来就并排写在 `snapshotVersions` 里。
    #[test]
    fn a_known_league_resolves_its_short_name_instead_of_guessing() {
        let state: IndexState = serde_json::from_str(INDEX_STATE_JSON).unwrap();
        assert_eq!(
            state.league_url_for_name("Forbidden Rites"),
            Some("forbiddenrites")
        );
        assert_eq!(
            state.league_url_for_name("Runes of Aldur"),
            Some("runesofaldur")
        );
        // 用户在设置里打的大小写不该决定采样能不能跑起来。
        assert_eq!(
            state.league_url_for_name("forbidden rites"),
            Some("forbiddenrites")
        );
        // 这一轮没被索引的联赛给 `None`,而不是一个查不到东西的猜测。
        assert_eq!(state.league_url_for_name("Fate of the Vaal"), None);
        assert_eq!(IndexState::default().league_url_for_name("Standard"), None);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let state: IndexState = serde_json::from_str("{}").unwrap();
        assert!(state.snapshot_versions.is_empty());
        let snapshot: SnapshotVersion = serde_json::from_str(r#"{"url":"x"}"#).unwrap();
        assert_eq!(snapshot.url, "x");
        assert_eq!(snapshot.version, "");
        assert_eq!(snapshot.overview_type, 0);
    }

    #[test]
    fn parses_build_index_state() {
        let json = r#"{"leagueBuilds":[
            {"leagueName":"Forbidden Rites","leagueUrl":"forbiddenrites","total":61390,
             "status":0,"category":0,"hardcore":false,
             "statistics":[
                {"class":"Gemling Legionnaire","percentage":35.86102345151909,"trend":1},
                {"class":"Disciple of Varashta","percentage":10.01175669822412,"trend":1}
             ]}
        ]}"#;
        let state: BuildIndexState = serde_json::from_str(json).unwrap();
        let league = &state.league_builds[0];
        assert_eq!(league.league_url, "forbiddenrites");
        assert_eq!(league.total, 61_390);
        assert_eq!(league.statistics[0].class, "Gemling Legionnaire");
        assert!((league.statistics[0].percentage - 35.861_023_451_519_09).abs() < 1e-9);
        assert_eq!(league.statistics[1].trend, 1);
    }
}
