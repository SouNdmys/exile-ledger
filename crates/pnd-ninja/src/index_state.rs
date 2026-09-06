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
