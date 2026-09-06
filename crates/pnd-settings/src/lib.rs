//! 版本化、原子写的 `settings.json` 存储,读取宽容、写入严格。
//!
//! 读得宽容:文件不在就给默认值;文件读不动就先改名成
//! `settings.json.corrupt-<时间戳>` 挪到一边再报告,这样后面任何一次保存都不可能
//! 把那份还有救的文件盖掉。写得严格且原子:先把整份 JSON 写进同目录的临时文件、
//! `sync_all` 落盘,再改名顶上去;发现盘上是更新的 schema 就拒绝写,不覆盖。
//! (posture 照搬 POE-Trade-Tracker 的 `ptt-settings`,schema 从 v1 重新起。)

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use pnd_domain::{Currency, Price, SearchRef, WatchId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
const APP_DIR_NAME: &str = "PoeNinjaData";
const SETTINGS_FILE_NAME: &str = "settings.json";

/// 切成浏览器 UA 时用的那一串。写死一个具体版本而不是拼当前时间:
/// UA 是给对面看的指纹,随程序自己变反而更显眼。
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 请求头里怎么自报家门。
///
/// `Identified` 是默认:ninja 的文档要求带联系方式,交易站今天也接受。
/// 哪天交易站开始 403,用户能在设置页切成 `Browser` 应急。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UserAgentMode {
    #[default]
    Identified,
    Browser,
}

/// 一条蹲价搜索:用户在网页筛好条件粘进来之后,程序要记住的全部东西。
///
/// `search_id` 是自描述的(gzip + base64url 的查询 JSON),所以不用另存查询条件;
/// 联赛单独存是因为同一个 id 换个联赛就是另一次搜索。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WatchEntry {
    pub id: WatchId,
    pub label: String,
    pub league: String,
    pub search_id: String,
    pub price_cap: Price,
    pub enabled: bool,
    pub live: bool,
    /// 新建这条搜索之后的第一次轮询,要不要对已经挂着的好价提醒一次。
    /// 默认要 —— 不然你刚加的搜索得等到下一件新货上架才响。
    pub alert_on_first_poll: bool,
    /// RFC3339;空串合法(手写的设置文件可以不填)。
    pub created_at: String,
}

impl Default for WatchEntry {
    fn default() -> Self {
        Self {
            id: WatchId(String::new()),
            label: String::new(),
            league: String::new(),
            search_id: String::new(),
            price_cap: Price::new(0, Currency::Divine),
            enabled: true,
            live: true,
            alert_on_first_poll: true,
            created_at: String::new(),
        }
    }
}

impl WatchEntry {
    /// 从"用户刚粘进来的那个搜索"造一条新记录:id 现取一个 uuid v4,
    /// 时间戳取当下。这是唯一一处碰时钟的地方,别处的时间都由调用方传进来。
    pub fn new(label: impl Into<String>, search_ref: &SearchRef, price_cap: Price) -> WatchEntry {
        WatchEntry {
            id: WatchId(uuid::Uuid::new_v4().to_string()),
            label: label.into(),
            league: search_ref.league.clone(),
            search_id: search_ref.search_id.clone(),
            price_cap,
            created_at: chrono::Utc::now().to_rfc3339(),
            ..WatchEntry::default()
        }
    }
}

/// 轮询和限速预算的旋钮。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct WatcherTuning {
    pub poll_interval_seconds: u64,
    /// WebSocket 健康时的轮询档位。有秒推兜着,轮询只是保险,可以放慢。
    pub poll_interval_when_live_seconds: u64,
    pub max_live_connections: u32,
    /// 只用服务端限速头允许量的这个百分比,不贴着上限跑。
    pub budget_percent: u32,
    /// 算完百分比再减掉这么多次,给"同一个 IP 上你自己开着浏览器"留出余量。
    pub limit_margin: u32,
    /// 一次 fetch 最多带几个挂单 id(交易站上限就是 10)。
    pub fetch_batch: u32,
}

impl Default for WatcherTuning {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 300,
            poll_interval_when_live_seconds: 900,
            max_live_connections: 5,
            budget_percent: 50,
            limit_margin: 1,
            fetch_batch: 10,
        }
    }
}

/// 提醒卡片和声音的旋钮。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct AlertTuning {
    pub sound: bool,
    /// 空串 = 用内置合成音。
    pub custom_sound_path: String,
    pub auto_hide_minutes: u32,
    /// `bottom_right` / `bottom_left` / `top_right` / `top_left`。
    pub corner: String,
    /// `LWA_ALPHA` 的 0–255 值。低于 60 卡片上的小字就糊了,`normalize` 会兜住。
    pub opacity: u8,
}

impl Default for AlertTuning {
    fn default() -> Self {
        Self {
            sound: true,
            custom_sound_path: String::new(),
            auto_hide_minutes: 5,
            corner: "bottom_right".to_string(),
            opacity: 235,
        }
    }
}

/// poe.ninja 采样的旋钮。默认值就是计划里定的那套(目标 2000 个角色、1 秒一个请求)。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct NinjaTuning {
    pub sample_target: u32,
    /// 占比低于这个数的职业不单独开一个分区 —— 采样预算花在有人玩的 BD 上。
    pub min_class_share_percent: f64,
    pub skills_per_class: u32,
    pub top_global_skills: u32,
    pub top_uniques: u32,
    /// 同一个联赛多久才重新采一次;24 小时内重开程序不该再打一遍 ninja。
    pub refresh_hours: u32,
    pub min_request_gap_ms: u64,
    pub hardcore: bool,
}

impl Default for NinjaTuning {
    fn default() -> Self {
        Self {
            sample_target: 2000,
            min_class_share_percent: 1.0,
            skills_per_class: 2,
            top_global_skills: 10,
            top_uniques: 20,
            refresh_hours: 24,
            min_request_gap_ms: 1000,
            hardcore: false,
        }
    }
}

/// 整份 `settings.json`。
///
/// 每个结构都挂 `#[serde(default)]`:老文件缺哪个键就用默认值补,
/// 而不是整份读不出来退回全盘默认 —— 那等于用户的搜索列表和会话一起没了。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AppSettings {
    pub schema_version: u32,
    /// `zh` 或 `en`。存字符串不存枚举:以后加语言只是多一个值,不动 schema。
    pub ui_language: String,
    pub league: String,
    /// 明文存本机。程序不登录、不存密码、不把它写进日志。
    pub poesessid: String,
    pub watches: Vec<WatchEntry>,
    pub watcher: WatcherTuning,
    pub alert: AlertTuning,
    pub ninja: NinjaTuning,
    pub user_agent_mode: UserAgentMode,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            ui_language: "zh".to_string(),
            league: "Forbidden Rites".to_string(),
            poesessid: String::new(),
            watches: Vec::new(),
            watcher: WatcherTuning::default(),
            alert: AlertTuning::default(),
            ninja: NinjaTuning::default(),
            user_agent_mode: UserAgentMode::default(),
        }
    }
}

impl AppSettings {
    /// 把手改坏的值拉回可用范围,而不是拒绝整份文件。
    ///
    /// 这些下限都不是洁癖:预算百分比过小会让轮询几乎不动、过大等于贴着限速跑;
    /// 轮询间隔低于 60 秒对交易站不礼貌;不透明度太低卡片上的字看不清;
    /// 语言认不出来就退回中文,总比界面一片空白强。
    pub fn normalize(&mut self) {
        self.watcher.budget_percent = self.watcher.budget_percent.clamp(10, 100);
        self.watcher.poll_interval_seconds = self.watcher.poll_interval_seconds.max(60);
        self.watcher.poll_interval_when_live_seconds =
            self.watcher.poll_interval_when_live_seconds.max(60);
        self.alert.opacity = self.alert.opacity.max(60);
        if self.ui_language != "zh" && self.ui_language != "en" {
            self.ui_language = "zh".to_string();
        }
    }

    pub fn watch(&self, id: &WatchId) -> Option<&WatchEntry> {
        self.watches.iter().find(|entry| &entry.id == id)
    }

    /// 所有出站请求的 User-Agent。ninja 的文档要求能识别到人 + 联系方式。
    pub fn user_agent(&self) -> String {
        match self.user_agent_mode {
            UserAgentMode::Identified => format!(
                "PoeNinjaData/{} (contact: soundmys1994@gmail.com)",
                env!("CARGO_PKG_VERSION")
            ),
            UserAgentMode::Browser => BROWSER_USER_AGENT.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadStatus {
    /// 文件读出来了,schema 也支持。
    Loaded,
    /// 没有文件 —— 给默认值。
    Defaults,
    /// 文件是更新的 schema 写的 —— 给默认值,并且拒绝保存。
    FutureSchemaReadOnly { detected: u32 },
    /// 文件在,但读不动。它已经被改名挪到 `backup_path`,免得下一次保存
    /// 悄悄盖掉一份也许手工还能救回来的文件;这次给默认值。
    Corrupt {
        backup_path: PathBuf,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct LoadedSettings {
    pub settings: AppSettings,
    pub status: LoadStatus,
}

#[derive(Debug, Error)]
pub enum SaveError {
    #[error("settings on disk use schema {detected}, newer than supported {supported}")]
    SchemaTooNew { detected: u32, supported: u32 },
    #[error("settings write failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// 正式位置:`%LOCALAPPDATA%\PoeNinjaData\settings.json`。
    pub fn release_default() -> Self {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        Self::release_default_from(Path::new(&local))
    }

    /// 同上,但根目录由调用方给 —— 测试因此永远不会碰到真的用户目录。
    pub fn release_default_from(local_app_data: &Path) -> Self {
        Self {
            path: local_app_data.join(APP_DIR_NAME).join(SETTINGS_FILE_NAME),
        }
    }

    pub fn at_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> LoadedSettings {
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return LoadedSettings {
                settings: AppSettings::default(),
                status: LoadStatus::Defaults,
            };
        };
        match serde_json::from_str::<AppSettings>(&raw) {
            Ok(settings) if settings.schema_version <= CURRENT_SCHEMA_VERSION => LoadedSettings {
                settings,
                status: LoadStatus::Loaded,
            },
            Ok(settings) => LoadedSettings {
                status: LoadStatus::FutureSchemaReadOnly {
                    detected: settings.schema_version,
                },
                settings: AppSettings::default(),
            },
            Err(error) => {
                // 文件可能只是新版本加了我们读不懂的东西:先单独把版本号捞出来看一眼,
                // 是未来 schema 就报未来 schema,而不是冤枉它"坏了"。
                if let Some(detected) = detect_schema_version(&raw)
                    && detected > CURRENT_SCHEMA_VERSION
                {
                    return LoadedSettings {
                        settings: AppSettings::default(),
                        status: LoadStatus::FutureSchemaReadOnly { detected },
                    };
                }
                // 存在但读不出来,不等于"没有文件":抢在任何人保存之前把它挪开,
                // 坏掉的那份就还在。
                let backup_path = self.move_corrupt_aside();
                LoadedSettings {
                    settings: AppSettings::default(),
                    status: LoadStatus::Corrupt {
                        backup_path,
                        reason: error.to_string(),
                    },
                }
            }
        }
    }

    /// 把读不动的文件改名成 `settings.json.corrupt-<unix 秒>`。
    /// 连改名都失败就返回原路径,好歹让用户看到的提示指向一个真实存在的文件。
    pub fn move_corrupt_aside(&self) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file_name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| SETTINGS_FILE_NAME.to_string());
        let backup = self
            .path
            .with_file_name(format!("{file_name}.corrupt-{stamp}"));
        match fs::rename(&self.path, &backup) {
            Ok(()) => backup,
            Err(_) => self.path.clone(),
        }
    }

    /// 原子保存。改名顶上去之前再查一次盘上的 schema —— 中途要是有个更新版本的
    /// 进程写了这个文件,让它赢。
    pub fn save(&self, settings: &AppSettings) -> Result<(), SaveError> {
        let mut normalized = settings.clone();
        normalized.schema_version = CURRENT_SCHEMA_VERSION;
        let body =
            serde_json::to_vec_pretty(&normalized).expect("settings model always serializes");

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.refuse_future_schema()?;

        let temp_path = self.path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&temp_path)?;
            file.write_all(&body)?;
            file.sync_all()?;
        }
        if let Err(error) = self.refuse_future_schema() {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        fs::rename(&temp_path, &self.path).map_err(|error| {
            let _ = fs::remove_file(&temp_path);
            SaveError::Io(error)
        })
    }

    /// 盘上那份是更新的 schema 吗?设置页据此把所有输入框置灰。
    pub fn is_read_only(&self) -> bool {
        self.refuse_future_schema().is_err()
    }

    fn refuse_future_schema(&self) -> Result<(), SaveError> {
        if let Ok(raw) = fs::read_to_string(&self.path)
            && let Some(detected) = detect_schema_version(&raw)
            && detected > CURRENT_SCHEMA_VERSION
        {
            return Err(SaveError::SchemaTooNew {
                detected,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        Ok(())
    }
}

/// 只把版本号捞出来 —— 整份读不懂的时候还能问一句"这是谁写的"。
pub fn detect_schema_version(raw: &str) -> Option<u32> {
    #[derive(Deserialize)]
    struct VersionOnly {
        schema_version: u32,
    }
    serde_json::from_str::<VersionOnly>(raw)
        .ok()
        .map(|value| value.schema_version)
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    fn temp_store(name: &str) -> SettingsStore {
        let dir = std::env::temp_dir()
            .join("pnd-settings-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        SettingsStore::at_path(dir.join(SETTINGS_FILE_NAME))
    }

    fn write_file(store: &SettingsStore, body: &str) {
        fs::create_dir_all(store.path().parent().expect("parent")).expect("mkdir");
        fs::write(store.path(), body).expect("write");
    }

    #[test]
    fn missing_file_yields_defaults() {
        let store = temp_store("missing");
        let loaded = store.load();
        assert_eq!(loaded.status, LoadStatus::Defaults);
        assert_eq!(loaded.settings, AppSettings::default());
    }

    #[test]
    fn corrupt_file_is_backed_up_and_reported() {
        let store = temp_store("corrupt");
        write_file(&store, "{ this is not json");
        let loaded = store.load();
        let LoadStatus::Corrupt {
            backup_path,
            reason,
        } = loaded.status
        else {
            panic!("expected Corrupt");
        };
        assert!(
            backup_path.exists(),
            "backup must exist at {}",
            backup_path.display()
        );
        assert!(!store.path().exists(), "the broken file must be moved away");
        assert!(!reason.is_empty());
        assert_eq!(loaded.settings, AppSettings::default());

        // 挪开之后这个存储照样能写,下一次保存不会碰到备份。
        store.save(&AppSettings::default()).expect("save");
        assert_eq!(store.load().status, LoadStatus::Loaded);
    }

    #[test]
    fn round_trip_preserves_settings() {
        let store = temp_store("round-trip");
        let search = SearchRef {
            league: "Forbidden Rites".to_string(),
            search_id: "H4sIAAAA-_09".to_string(),
        };
        let mut settings = AppSettings {
            ui_language: "en".to_string(),
            poesessid: "deadbeef".to_string(),
            user_agent_mode: UserAgentMode::Browser,
            ..AppSettings::default()
        };
        settings.watches.push(WatchEntry::new(
            "Choir of the Storm",
            &search,
            Price::new(20_000, Currency::Divine),
        ));
        settings.watcher.budget_percent = 40;
        settings.ninja.sample_target = 500;
        settings.alert.corner = "top_left".to_string();

        store.save(&settings).expect("save");
        let loaded = store.load();
        assert_eq!(loaded.status, LoadStatus::Loaded);
        assert_eq!(loaded.settings, settings);

        let id = settings.watches[0].id.clone();
        assert_eq!(
            loaded.settings.watch(&id).map(|entry| entry.label.as_str()),
            Some("Choir of the Storm")
        );
    }

    #[test]
    fn future_schema_is_read_only() {
        let store = temp_store("future");
        write_file(&store, r#"{"schema_version": 99}"#);
        match store.load().status {
            LoadStatus::FutureSchemaReadOnly { detected } => assert_eq!(detected, 99),
            other => panic!("expected read-only, got {other:?}"),
        }
        assert!(store.is_read_only());
        assert!(matches!(
            store.save(&AppSettings::default()),
            Err(SaveError::SchemaTooNew { detected: 99, .. })
        ));
    }

    /// 空对象就得是一整套默认值 —— 第一次启动写出来的文件就长这样。
    #[test]
    fn defaults_deserialize_from_an_empty_object() {
        let settings: AppSettings = serde_json::from_str("{}").expect("parse");
        assert_eq!(settings, AppSettings::default());
        assert_eq!(settings.league, "Forbidden Rites");
        assert_eq!(settings.watcher.poll_interval_seconds, 300);
        assert_eq!(settings.alert.opacity, 235);
        assert_eq!(settings.ninja.sample_target, 2000);
        assert_eq!(settings.user_agent_mode, UserAgentMode::Identified);
    }

    /// 以后加的键、或者手工写错的键,都不该让整份设置读不出来。
    #[test]
    fn a_file_with_an_unknown_key_still_loads() {
        let store = temp_store("unknown-key");
        write_file(
            &store,
            r#"{"schema_version": 1, "league": "Standard", "future_knob": {"a": 1},
                "watcher": {"budget_percent": 30}}"#,
        );
        let loaded = store.load();
        assert_eq!(loaded.status, LoadStatus::Loaded);
        assert_eq!(loaded.settings.league, "Standard");
        assert_eq!(loaded.settings.watcher.budget_percent, 30);
        // 没写的键仍然是默认值,不会被那个未知键带塌。
        assert_eq!(loaded.settings.watcher.poll_interval_seconds, 300);
    }

    #[test]
    fn normalize_clamps_hand_edited_values() {
        let mut settings = AppSettings {
            ui_language: "fr".to_string(),
            ..AppSettings::default()
        };
        settings.watcher.budget_percent = 500;
        settings.watcher.poll_interval_seconds = 5;
        settings.watcher.poll_interval_when_live_seconds = 0;
        settings.alert.opacity = 10;
        settings.normalize();

        assert_eq!(settings.watcher.budget_percent, 100);
        assert_eq!(settings.watcher.poll_interval_seconds, 60);
        assert_eq!(settings.watcher.poll_interval_when_live_seconds, 60);
        assert_eq!(settings.alert.opacity, 60);
        assert_eq!(settings.ui_language, "zh");

        settings.watcher.budget_percent = 0;
        settings.ui_language = "en".to_string();
        settings.normalize();
        assert_eq!(settings.watcher.budget_percent, 10);
        assert_eq!(settings.ui_language, "en", "认识的语言不该被改掉");
    }

    #[test]
    fn user_agent_follows_the_mode() {
        let mut settings = AppSettings::default();
        let identified = settings.user_agent();
        assert!(identified.starts_with("PoeNinjaData/"));
        assert!(identified.contains("soundmys1994@gmail.com"));

        settings.user_agent_mode = UserAgentMode::Browser;
        assert!(settings.user_agent().starts_with("Mozilla/5.0"));
    }

    /// 新建的记录要有 uuid、有时间戳,三个开关都默认开着。
    #[test]
    fn a_new_watch_entry_is_ready_to_run() {
        let search = SearchRef {
            league: "Forbidden Rites".to_string(),
            search_id: "H4sIAAAA-_09".to_string(),
        };
        let entry = WatchEntry::new("Choir", &search, Price::new(20_000, Currency::Divine));
        assert_eq!(entry.id.as_str().len(), 36, "uuid v4 带连字符是 36 个字符");
        assert_eq!(entry.league, "Forbidden Rites");
        assert_eq!(entry.search_id, "H4sIAAAA-_09");
        assert!(entry.enabled && entry.live && entry.alert_on_first_poll);
        assert!(entry.created_at.starts_with("20"));

        let other = WatchEntry::new("Choir", &search, Price::new(20_000, Currency::Divine));
        assert_ne!(entry.id, other.id, "每条记录一个自己的 id");
    }

    /// 货币走普通字符串、金额走千分整数 —— 手工看设置文件时要能读懂。
    #[test]
    fn a_watch_entry_serializes_readably() {
        let entry = WatchEntry {
            id: WatchId("w-1".to_string()),
            price_cap: Price::new(20_000, Currency::Divine),
            ..WatchEntry::default()
        };
        let text = serde_json::to_string(&entry).expect("serialize");
        assert!(text.contains(r#""id":"w-1""#), "{text}");
        assert!(
            text.contains(r#""price_cap":{"amount_milli":20000,"currency":"divine"}"#),
            "{text}"
        );
    }
}
