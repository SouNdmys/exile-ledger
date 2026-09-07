//! 外壳和后台之间的那根线:`pnd-runtime` 的 actor、`pnd-platform-win` 的
//! 提醒卡片,以及提醒历史那条只读的库连接。
//!
//! 页面代码只管画,命令和事件都在这里过一遍。分开的理由是方向不同:
//! 页面是"用户点了什么",这里是"后台发生了什么"——两条路的错误处理、
//! 借用形状都不一样,混在一个文件里每加一个事件都要重读整页。
//!
//! 三条纪律:
//! - **抽干,不阻塞。** 三个事件源(actor、卡片、ninja 采样)都是 `try_*`,
//!   一次 tick 取到 `None` 为止;界面线程不等任何后台线程。
//! - **后台起不来不等于程序完蛋。** actor 或卡片没起来,程序照常开窗,
//!   状态行上说清楚哪一半没了。
//! - **游戏内的动作永远是用户点的。** "去藏身处"按一下发一条命令,发完就
//!   等结果;界面自己永远不发第二次,后台也不会替谁重试到底。

use std::time::{Duration, Instant};

use gpui::{ClipboardItem, Context};
use pnd_domain::{ListingSummary, SearchRef, search_page_url};
use pnd_platform_win::{
    AlertCardService, CardButton, CardConfig, CardError, CardEvent, CardText, Corner, LoginConfig,
    LoginEvent, LoginFailure, LoginService, ValidatedWave, built_in_alert_wave, open_url,
};
use pnd_runtime::{
    HideoutOutcome, MatchedListing, RuntimeCommand, RuntimeEvent, RuntimeHandle, RuntimePaths,
    now_secs,
};
use pnd_settings::AppSettings;
use pnd_trade::{BucketUsage, FETCH_POLICY, SEARCH_POLICY};

use super::{AppShell, LoginPhase};
use crate::i18n::{self, Text};

/// 装 WebView2 的官方页面。没有 Edge 内核时那个按钮开的就是它。
pub(crate) const WEBVIEW2_DOWNLOAD_URL: &str =
    "https://developer.microsoft.com/microsoft-edge/webview2/";

/// 提醒记录页一次读多少行。多到"这周的都在",少到一次查询感觉不到。
pub(crate) const RECENT_ALERTS: u32 = 200;

/// 发出命令之后过多久重读一次提醒历史。
///
/// 忽略 / 记动作都是发给 actor 线程去写库的,发完立刻重读会读到旧值;
/// 250ms 之后再读,那条命令早就落盘了。
const WRITE_SETTLE: Duration = Duration::from_millis(250);

/// 同时记得多少张卡片。卡片一次只显示一张,留几张是为了收起之后
/// 迟到的按钮事件还能找到它是谁。
const REMEMBERED_CARDS: usize = 16;

/// 卡片一行字最多显示多少个字,超了就截断。
///
/// 平台层的上限是 80 / 160,但那是"塞得进结构体",不是"看得清"——
/// 420 像素宽的卡片上一行放不下 160 个字,与其让 Win32 那边裁,不如
/// 在这里裁掉并留个省略号,至少看得出来后面还有东西。
const CARD_TITLE_CHARS: usize = 46;
const CARD_LINE_CHARS: usize = 60;

// ---------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------

/// 起 actor 线程。失败不致命:程序照常开窗,只是不会自己去轮询。
pub(crate) fn start_runtime(
    settings: &AppSettings,
    log: &mut Vec<String>,
) -> Option<RuntimeHandle> {
    let paths = RuntimePaths::new(crate::watch_db_path());
    log.push(format!("watch db: {}", paths.watch_db.display()));
    match RuntimeHandle::start(settings.clone(), paths) {
        Ok(handle) => Some(handle),
        Err(error) => {
            log.push(format!("runtime failed to start: {error}"));
            None
        }
    }
}

/// 起卡片线程。
pub(crate) fn start_alert_card(
    settings: &AppSettings,
    log: &mut Vec<String>,
) -> Result<AlertCardService, CardError> {
    AlertCardService::start(card_config(settings, log))
}

/// 设置 → 卡片配置。
fn card_config(settings: &AppSettings, log: &mut Vec<String>) -> CardConfig {
    CardConfig {
        // 角落认不出来(手改坏的设置)就用默认的右下角,不是不显示卡片。
        corner: Corner::parse(&settings.alert.corner).unwrap_or_default(),
        opacity: settings.alert.opacity,
        auto_hide: Duration::from_secs(u64::from(settings.alert.auto_hide_minutes) * 60),
        sound: card_sound(settings, log),
    }
}

/// 报警音:关了就是 `None`,自定义文件读不动就退回内置合成音并说一声。
///
/// 为什么要说一声:静悄悄地换成另一个声音,用户下次听到的不是自己选的那个,
/// 却没有任何地方告诉他为什么。
fn card_sound(settings: &AppSettings, log: &mut Vec<String>) -> Option<ValidatedWave> {
    if !settings.alert.sound {
        return None;
    }
    let custom = settings.alert.custom_sound_path.trim();
    if !custom.is_empty() {
        match ValidatedWave::open(custom) {
            Ok(wave) => return Some(wave),
            Err(error) => log.push(format!(
                "custom alert sound unusable ({error}) — falling back to the built-in tone"
            )),
        }
    }
    match built_in_alert_wave() {
        Ok(wave) => Some(wave),
        Err(error) => {
            log.push(format!("the built-in alert tone failed to build: {error}"));
            None
        }
    }
}

// ---------------------------------------------------------------------
// 每一拍
// ---------------------------------------------------------------------

impl AppShell {
    /// 把两个事件源抽干。返回"有没有东西变了",由 tick 决定要不要重画。
    pub(crate) fn drain_events(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        loop {
            let Some(event) = self
                .runtime
                .as_ref()
                .and_then(RuntimeHandle::try_next_event)
            else {
                break;
            };
            self.on_runtime_event(event);
            changed = true;
        }
        loop {
            let Some(event) = self
                .alert_card
                .as_ref()
                .and_then(AlertCardService::try_next_event)
            else {
                break;
            };
            self.on_card_event(event, cx);
            changed = true;
        }
        loop {
            let Some(event) = self.login.as_ref().and_then(LoginService::try_next_event) else {
                break;
            };
            self.on_login_event(event);
            changed = true;
        }
        // ninja 采样是第三个事件源。它和交易那两条完全无关(不碰交易站、
        // 不碰 actor),只是同样需要有人定期来取。
        changed |= self.drain_sampler_events();
        changed
    }

    fn on_runtime_event(&mut self, event: RuntimeEvent) {
        let text = self.text();
        match event {
            RuntimeEvent::Ready => self.push_log("runtime: ready".to_owned()),
            RuntimeEvent::WatchStatus { watch_id, status } => {
                self.watch_status.insert(watch_id, status);
                self.watches_dirty = true;
            }
            RuntimeEvent::Budget {
                policy,
                usage,
                next_allowed_in_secs,
            } => {
                // "还要等几秒"只在真的要等的时候留着:等 0 秒不值得占预算条
                // 上的一格,而一个过期的秒数比没有更糟。
                match next_allowed_in_secs {
                    Some(seconds) => {
                        self.budget_next_allowed.insert(policy.clone(), seconds);
                    }
                    None => {
                        self.budget_next_allowed.remove(&policy);
                    }
                }
                self.budget.insert(policy, usage);
            }
            RuntimeEvent::ListingMatched(matched) => {
                self.push_log(format!(
                    "alert: {} · {}",
                    matched.label,
                    matched.headline.short_label()
                ));
                self.show_card(*matched);
                // 命中会往 alerts 表里写行,但写的是 actor 线程,等它落盘再读。
                self.refresh_alerts_soon();
            }
            RuntimeEvent::SessionInvalid => {
                self.push_log("runtime: POESESSID rejected, now anonymous".to_owned());
                self.set_sticky_notice(text.notice_session_invalid.to_owned());
            }
            RuntimeEvent::CloudflareBlocked { until } => {
                self.push_log(format!("runtime: cloudflare hold until {until}"));
                self.set_sticky_notice(i18n::fill(text.notice_cloudflare, &[&local_clock(until)]));
            }
            RuntimeEvent::RatesUpdated(rates) => {
                self.rates = rates;
                self.watches_dirty = true;
                // 暗金榜那一列写的是"N exalted ≈ M divine",换算就靠这份汇率。
                self.uniques_dirty = true;
            }
            RuntimeEvent::HideoutResult { alert_id, outcome } => {
                self.push_log(format!("hideout: alert {alert_id} → {outcome:?}"));
                self.on_hideout_result(alert_id, &outcome);
                // 结果落在提醒历史的 `last_action` 上,但写的是 actor 线程。
                self.refresh_alerts_soon();
            }
            RuntimeEvent::SessionChecked { valid, detail } => {
                self.push_log(format!("session check: valid={valid} ({detail})"));
                self.session_check_busy = false;
                let word = if valid {
                    text.settings_session_ok
                } else {
                    text.settings_session_bad
                };
                // 技术细节(状态码 + 规则名)跟在后面:测出来不认的时候,
                // 那一串是唯一能拿去查的东西。
                self.session_check_line = format!("{word} · {detail}");
                self.set_notice(word.to_owned());
                // 这一次检查是登录窗抓完会话自己发起的:那句"正在测试"已经有了
                // 答案,擦掉,别让它永远挂在登录那一行上。别的状态字(比如
                // "没有 WebView2,请手动粘")还得留着给人看。
                if self.login_line == text.settings_login_captured {
                    self.login_line.clear();
                }
            }
            RuntimeEvent::Log(line) => self.push_log(line),
            RuntimeEvent::Fault(line) => {
                self.push_log(format!("runtime fault: {line}"));
                self.set_sticky_notice(i18n::fill(text.notice_runtime_failed, &[&line]));
            }
        }
    }

    // ---- 程序内登录 --------------------------------------------------

    /// 点了"登录官网"。
    ///
    /// 线程是懒起的:大多数启动根本用不上它。窗口标题和顶上那条提示是起线程
    /// 时定死的,所以换过语言就先把旧线程扔掉再起一条 —— 中文界面配一句
    /// 英文提示,看着像别人的窗口。
    pub(crate) fn open_login_window(&mut self) {
        let text = self.text();
        if self.login.is_some() && self.login_language != self.settings.ui_language {
            self.login = None;
        }
        if self.login.is_none() {
            let config = LoginConfig {
                user_data_dir: crate::webview2_data_dir(),
                title: text.settings_login_window_title.to_owned(),
                hint_text: text.settings_login_window_hint.to_owned(),
            };
            self.push_log(format!(
                "login: webview2 data dir {}",
                config.user_data_dir.display()
            ));
            match LoginService::start(config) {
                Ok(service) => {
                    self.login = Some(service);
                    self.login_language = self.settings.ui_language.clone();
                }
                Err(error) => {
                    self.push_log(format!("login thread failed to start: {error}"));
                    self.login_phase = LoginPhase::Idle;
                    self.login_line = i18n::fill(text.settings_login_failed, &[&error.to_string()]);
                    return;
                }
            }
        }
        if let Some(login) = &self.login
            && let Err(error) = login.open()
        {
            self.push_log(format!("login open failed: {error}"));
            self.login_phase = LoginPhase::Idle;
            self.login_line = i18n::fill(text.settings_login_failed, &[&error.to_string()]);
            return;
        }
        // 乐观地当成"开着":建 WebView2 环境要一两秒,这期间按钮该是灰的,
        // 否则手快点两下就是两次开窗请求。真开不出来时 `Failed` 会把它拨回来。
        self.login_phase = LoginPhase::Open;
        self.login_line = text.settings_login_opening.to_owned();
    }

    /// 点了"我已登录,现在核对"。
    pub(crate) fn recheck_login(&mut self) {
        let text = self.text();
        let Some(login) = &self.login else {
            return;
        };
        match login.capture() {
            Ok(()) => self.login_line = text.settings_login_rechecking.to_owned(),
            Err(error) => {
                self.push_log(format!("login recheck failed: {error}"));
                self.login_line = i18n::fill(text.settings_login_failed, &[&error.to_string()]);
            }
        }
    }

    fn on_login_event(&mut self, event: LoginEvent) {
        let text = self.text();
        match event {
            LoginEvent::Opened => {
                self.login_phase = LoginPhase::Open;
                self.login_line = text.settings_login_opening.to_owned();
            }
            LoginEvent::Navigated { url, http_status } => {
                // 只进日志。每一跳都往状态行上写一句,会把"已读到会话"这种
                // 真正要看的话冲掉。
                self.push_log(format!("login: {url} → {http_status:?}"));
            }
            // 名字不是秘密,值一个字符都没有出现在这个事件里。
            LoginEvent::CookieNames(names) => {
                self.push_log(format!("login: cookies {names:?}"));
            }
            LoginEvent::SessionCaptured { poesessid } => self.store_captured_session(poesessid),
            LoginEvent::NotLoggedIn => self.login_line = text.settings_login_waiting.to_owned(),
            LoginEvent::Closed => {
                self.login_phase = LoginPhase::Idle;
                // 抓到会话那条路已经把状态字写成"正在测试"了,别盖掉它。
                if !self.session_check_busy {
                    self.login_line = text.settings_login_closed.to_owned();
                }
            }
            LoginEvent::Failed(failure) => {
                self.push_log(format!("login failed: {failure}"));
                self.login_phase = match failure {
                    LoginFailure::RuntimeMissing => LoginPhase::RuntimeMissing,
                    _ => LoginPhase::Idle,
                };
                self.login_line = match failure {
                    LoginFailure::RuntimeMissing => text.settings_login_runtime_missing.to_owned(),
                    other => i18n::fill(text.settings_login_failed, &[&other.to_string()]),
                };
            }
        }
    }

    /// 存下刚登出来的那个会话,然后立刻拿它问一次交易站。
    ///
    /// 为什么马上就测:登录窗只能证明"官网认这个 cookie",而这个程序真正要用
    /// 它的地方是交易站。两边偶尔不是一回事,与其等到下一次 live 连不上才发现,
    /// 不如当场花掉一次搜索额度问清楚。
    fn store_captured_session(&mut self, poesessid: String) {
        let text = self.text();
        if self.read_only {
            self.set_sticky_notice(text.settings_read_only.to_owned());
            self.login_line = text.settings_read_only.to_owned();
            return;
        }
        self.settings.poesessid = poesessid;
        self.settings.normalize();
        if let Err(error) = self.settings_store.save(&self.settings) {
            self.push_log(format!("settings save failed: {error}"));
            self.login_line = i18n::fill(text.settings_save_failed, &[&error.to_string()]);
            return;
        }
        // 值永远不进日志 —— 这一行只说"存了一个多长的东西"。
        self.push_log(format!(
            "login: stored a session of {} chars",
            self.settings.poesessid.chars().count()
        ));
        // 框里还是空的,不写回去的话下一次按保存就把它清掉了。
        self.poesessid_dirty = true;
        self.apply_settings_to_runtime();
        if self.send_runtime(pnd_runtime::RuntimeCommand::TestSession) {
            self.session_check_busy = true;
            self.session_check_line = text.settings_session_checking.to_owned();
        }
        self.login_line = text.settings_login_captured.to_owned();
        self.set_notice(text.settings_login_captured.to_owned());
    }

    fn on_card_event(&mut self, event: CardEvent, cx: &mut Context<Self>) {
        match event {
            CardEvent::Clicked { alert_id, button } => self.on_card_button(alert_id, button, cx),
            CardEvent::AutoHidden { alert_id } => {
                // 自己收起来的卡片也算"看过了":不记的话下一轮同样的挂单
                // 会被当成没处理过。窗口已经不在屏幕上,不用再 hide 一次。
                self.dismiss_card_batch(alert_id);
            }
            CardEvent::Moved { x, y } => self.push_log(format!("alert card moved to {x},{y}")),
            CardEvent::Warning(detail) => {
                self.push_log(format!("alert card: {detail}"));
                self.set_notice(detail);
            }
        }
    }

    /// 卡片上的四个按钮。含义在这里定,平台层只认下标。
    fn on_card_button(&mut self, alert_id: i64, button: CardButton, cx: &mut Context<Self>) {
        let text = self.text();
        let Some(matched) = self.shown_cards.get(&alert_id).cloned() else {
            self.push_log(format!("alert card: unknown alert {alert_id}"));
            return;
        };
        match button {
            CardButton::Primary => {
                self.open_trade_page(&matched.league, &matched.search_id);
                self.record_action(alert_id, "open");
            }
            CardButton::Secondary => {
                self.copy_whisper(&matched.headline.whisper, cx);
                self.record_action(alert_id, "copy");
            }
            CardButton::Tertiary => {
                // **一次点击 = 一条命令**。程序自己永远不发这条命令,也永远
                // 不重试到底 —— 计划里那句"每个游戏内动作都是你自己点一次"
                // 就是这里。结果由 `HideoutResult` 事件写回卡片脚注。
                self.travel_to_hideout(alert_id);
            }
            CardButton::Dismiss => {
                self.dismiss_card_batch(alert_id);
                if let Some(card) = &self.alert_card
                    && let Err(error) = card.hide()
                {
                    self.push_log(format!("alert card hide failed: {error}"));
                }
                self.set_notice(text.notice_dismissed.to_owned());
            }
        }
    }

    /// 请后台给这条提醒的卖家发一次传送请求。
    ///
    /// 卡片按钮和提醒记录页那个按钮都走这一句。它只投一条命令就返回:
    /// 交易站那边要跑几个来回(token 过期了还要先换一个)是 actor 的事,
    /// 界面等 `HideoutResult` 就好。
    /// 没有会话时也照发不误 —— runtime 会立刻回一句 `NoSession`,**一个请求
    /// 都不会出去**。让它走一遍的好处是答案落在同一个地方:卡片脚注、状态行、
    /// 提醒历史的动作列,而不是界面自己编一句、库里什么都没记。
    pub(crate) fn travel_to_hideout(&mut self, alert_id: i64) {
        self.send_runtime(RuntimeCommand::TravelToHideout { alert_id });
    }

    /// 一次"去藏身处"有了进展。
    ///
    /// 结局(发出去了 / 没会话 / 没 token / 失败)写进卡片脚注 —— 卡片就在
    /// 屏幕角落上,而按钮是在那儿按的,答案理应回到同一个地方。中间步骤
    /// ("正在换 token")只进状态行:为了一句过程去改卡片,下一秒又被结局
    /// 盖掉,只会闪一下。
    fn on_hideout_result(&mut self, alert_id: i64, outcome: &HideoutOutcome) {
        let text = self.text();
        let line = hideout_text(outcome, text);
        self.set_notice(line.clone());
        if !outcome.is_final() {
            return;
        }
        let Some(matched) = self.shown_cards.get(&alert_id).cloned() else {
            // 卡片早就收起来了(或者这一下是在提醒记录页点的):
            // 状态行和提醒历史里都有,不用再找一张卡片出来。
            return;
        };
        let card_text = card_text_for(&matched, text, &line);
        if let Some(card) = &self.alert_card
            && let Err(error) = card.update(alert_id, card_text)
        {
            self.push_log(format!("alert card update failed: {error}"));
        }
    }

    /// 这一批命中(一张卡片可能代表好几条)全部标记为已忽略。
    fn dismiss_card_batch(&mut self, alert_id: i64) {
        let ids = match self.shown_cards.get(&alert_id) {
            Some(matched) => matched.alert_ids.clone(),
            None => vec![alert_id],
        };
        for id in ids {
            self.send_runtime(RuntimeCommand::Dismiss { alert_id: id });
        }
        self.refresh_alerts_soon();
    }

    /// 弹一张卡片,并记住它对应哪一批命中。
    fn show_card(&mut self, matched: MatchedListing) {
        let text = self.text();
        let Some(alert_id) = matched.alert_ids.first().copied() else {
            return;
        };
        let footer = self.budget_line();
        let card_text = card_text_for(&matched, text, &footer);
        match &self.alert_card {
            Some(card) => {
                if let Err(error) = card.show(alert_id, card_text) {
                    self.push_log(format!("alert card show failed: {error}"));
                    self.set_notice(i18n::fill(text.notice_card_failed, &[&error.to_string()]));
                }
            }
            None => self.set_notice(text.notice_card_missing.to_owned()),
        }
        self.shown_cards.insert(alert_id, matched);
        // 只留最近几张:卡片事件迟到不会迟到几个小时。
        while self.shown_cards.len() > REMEMBERED_CARDS {
            let Some(oldest) = self.shown_cards.keys().next().copied() else {
                break;
            };
            self.shown_cards.remove(&oldest);
        }
    }

    // ---- 命令 --------------------------------------------------------

    /// 投一条命令给 actor。线程没了就在状态行上说一句,不假装成功。
    pub(crate) fn send_runtime(&mut self, command: RuntimeCommand) -> bool {
        let text = self.text();
        let Some(runtime) = &self.runtime else {
            self.set_notice(text.notice_runtime_gone.to_owned());
            return false;
        };
        match runtime.try_send(command) {
            Ok(()) => true,
            Err(error) => {
                self.push_log(format!("runtime command failed: {error}"));
                self.set_notice(text.notice_runtime_gone.to_owned());
                false
            }
        }
    }

    /// 设置改完之后:整份推给 actor,让它自己 diff。
    pub(crate) fn apply_settings_to_runtime(&mut self) {
        let settings = self.settings.clone();
        self.send_runtime(RuntimeCommand::ApplySettings(Box::new(settings)));
    }

    /// 记一笔"用户对这条提醒做了什么"。
    pub(crate) fn record_action(&mut self, alert_id: i64, action: &str) {
        self.send_runtime(RuntimeCommand::Action {
            alert_id,
            action: action.to_owned(),
        });
        self.refresh_alerts_soon();
    }

    /// 卡片外观改了就换一条卡片线程。
    ///
    /// 为什么整条重起而不是 `set_style`:声音是在起线程时装进去的,
    /// 换音频文件只能重来。保存设置不是高频操作,重起一条线程比"哪些字段
    /// 能热改、哪些不能"这张表可靠。
    pub(crate) fn restart_alert_card(&mut self) {
        let mut log = Vec::new();
        // 先 drop 再 start:进程内只允许一个卡片服务,顺序反了新的那个
        // 会拿到 `AlreadyInUse`。
        self.alert_card = None;
        match start_alert_card(&self.settings, &mut log) {
            Ok(card) => self.alert_card = Some(card),
            Err(error) => {
                let text = self.text();
                log.push(format!("alert card failed to start: {error}"));
                self.set_sticky_notice(i18n::fill(text.notice_card_failed, &[&error.to_string()]));
            }
        }
        for line in log {
            self.push_log(line);
        }
    }

    // ---- 提醒历史 ----------------------------------------------------

    /// 重读提醒历史。库读不动只记一行日志:界面上少几行历史,
    /// 比整个页面炸掉好。
    pub(crate) fn refresh_alerts(&mut self) {
        let Some(store) = &self.alerts_store else {
            return;
        };
        match store.recent_alerts(RECENT_ALERTS) {
            Ok(rows) => {
                self.alert_rows = rows;
                self.alerts_dirty = true;
            }
            Err(error) => self.push_log(format!("could not read the alert history: {error}")),
        }
        self.alerts_refresh_at = None;
    }

    /// 过一会儿再重读(等 actor 线程把命令写进库)。
    pub(crate) fn refresh_alerts_soon(&mut self) {
        self.alerts_refresh_at = Some(Instant::now() + WRITE_SETTLE);
    }

    /// 到点了就重读。tick 每拍问一次。
    pub(crate) fn refresh_alerts_if_due(&mut self) -> bool {
        if self
            .alerts_refresh_at
            .is_some_and(|at| Instant::now() >= at)
        {
            self.refresh_alerts();
            return true;
        }
        false
    }

    // ---- 两个共用的动作 ----------------------------------------------

    /// 打开官方交易页。按价升序,想要那件在最上面。
    pub(crate) fn open_trade_page(&mut self, league: &str, search_id: &str) {
        let text = self.text();
        let url = search_page_url(&SearchRef {
            league: league.to_owned(),
            search_id: search_id.to_owned(),
        });
        match open_url(&url) {
            Ok(()) => {
                self.push_log(format!("opened {url}"));
                self.set_notice(text.notice_opened_trade.to_owned());
            }
            Err(error) => {
                self.push_log(format!("could not open {url}: {error}"));
                self.set_notice(i18n::fill(text.notice_open_failed, &[&error.to_string()]));
            }
        }
    }

    /// 复制私聊内容。程序不发私聊,只把话给你,粘到游戏里的是你。
    pub(crate) fn copy_whisper(&mut self, whisper: &str, cx: &mut Context<Self>) {
        let text = self.text();
        if whisper.trim().is_empty() {
            self.set_notice(text.notice_no_whisper.to_owned());
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(whisper.to_owned()));
        self.set_notice(text.card_whisper_copied.to_owned());
    }

    // ---- 预算 --------------------------------------------------------

    /// 卡片脚注上那行预算。取不到就退回搜索的名字 —— 脚注空着更难看。
    pub(crate) fn budget_line(&self) -> String {
        let text = self.text();
        let mut parts = Vec::new();
        for (name, policy) in [
            (text.budget_search, SEARCH_POLICY),
            (text.budget_fetch, FETCH_POLICY),
        ] {
            if let Some(usage) = self.budget_usage(policy) {
                parts.push(format!(
                    "{name} {}",
                    i18n::fill(
                        text.budget_used_of,
                        &[&usage.used.to_string(), &usage.allowed.to_string()]
                    )
                ));
            }
        }
        parts.join(" · ")
    }

    /// 一条策略里窗口最长的那个桶。
    ///
    /// 为什么是最长的:短窗口(10 秒 5 次)每分钟都归零,看着永远很闲;
    /// 真正会限制一天能跑多少轮的是 6 小时那个桶。
    pub(crate) fn budget_usage(&self, policy: &str) -> Option<&BucketUsage> {
        self.budget
            .get(policy)?
            .iter()
            .max_by_key(|bucket| bucket.window_secs)
    }
}

// ---------------------------------------------------------------------
// 文字
// ---------------------------------------------------------------------

/// 一批命中 → 卡片上的五行字。
///
/// 纯函数,带测试:卡片只在真的命中时才出现,而那时候没人有空回头看
/// "标题是不是拼错了"。
pub(crate) fn card_text_for(
    matched: &MatchedListing,
    text: &'static Text,
    footer: &str,
) -> CardText {
    let listing = &matched.headline;
    let price = match &listing.price {
        Some(price) => price.display(),
        None => text.common_none.to_owned(),
    };
    let title = i18n::fill(text.card_title, &[&listing.item_name, &price]);

    let seller = i18n::fill(
        text.card_line_seller,
        &[&listing.account, presence(listing, text)],
    );
    let line1 = format!(
        "{seller} · {}",
        i18n::fill(text.card_line_listed, &[&age_text(&listing.indexed, text)])
    );

    let mut line2 = i18n::fill(
        text.card_line_cap,
        &[&matched.cap.display(), text.card_verdict_hit],
    );
    if matched.extra > 0 {
        line2.push_str(" · ");
        line2.push_str(&i18n::fill(text.card_more, &[&matched.extra.to_string()]));
    }

    let footer = if footer.is_empty() {
        matched.label.clone()
    } else {
        footer.to_owned()
    };

    CardText::new(
        clip(&title, CARD_TITLE_CHARS),
        clip(&line1, CARD_LINE_CHARS),
        clip(&line2, CARD_LINE_CHARS),
        clip(&footer, CARD_LINE_CHARS),
        [
            text.card_open_trade,
            text.card_copy_whisper,
            text.card_hideout,
            text.card_dismiss,
        ],
    )
    // 上面每一段都已经裁到平台层上限以内,构造不可能失败;真失败了也
    // 不该把提醒吞掉,退回一张只有标题的卡片。
    .unwrap_or_else(|_| CardText {
        title: clip(&listing.item_name, CARD_TITLE_CHARS),
        ..CardText::default()
    })
}

/// 一次"去藏身处"的结局 → 一句人话。
///
/// 纯函数带测试:这句话会同时出现在卡片脚注、状态行和提醒记录页,而这四种
/// 结局里有三种是"没成",用户凭这一句判断下一步该干嘛(去网页点 Travel、
/// 还是先去设置页粘 cookie)。
#[must_use]
pub(crate) fn hideout_text(outcome: &HideoutOutcome, text: &'static Text) -> String {
    match outcome {
        HideoutOutcome::Sent => text.hideout_sent.to_owned(),
        HideoutOutcome::NoSession => text.hideout_no_session.to_owned(),
        HideoutOutcome::TokenMissing => text.hideout_token_missing.to_owned(),
        HideoutOutcome::Refreshed => text.hideout_refreshing.to_owned(),
        // 状态码是失败时唯一的线索(503 = token 过期,403 = 会话不对):
        // 吞掉它等于让人只能重试到死。
        HideoutOutcome::Failed { status, .. } => {
            i18n::fill(text.hideout_failed_status, &[&status.to_string()])
        }
    }
}

/// 提醒历史里那个动作码 → 同一句人话。
///
/// 码是 `HideoutOutcome::action()` 写进库的,所以这里认得全;认不出来的
/// 交给调用方原样显示。
#[must_use]
pub(crate) fn hideout_action_text(action: &str, text: &'static Text) -> Option<&'static str> {
    match action {
        "hideout_sent" => Some(text.hideout_sent),
        "hideout_no_session" => Some(text.hideout_no_session),
        "hideout_token_missing" => Some(text.hideout_token_missing),
        "hideout_refreshing" => Some(text.hideout_refreshing),
        // 库里只有一个"失败"的码,状态码没存下来,所以这一条不带括号。
        "hideout_failed" => Some(text.hideout_failed),
        _ => None,
    }
}

/// 卖家在不在线。
fn presence(listing: &ListingSummary, text: &'static Text) -> &'static str {
    match (listing.online, listing.afk) {
        (true, true) => text.card_afk,
        (true, false) => text.card_online,
        (false, _) => text.card_offline,
    }
}

/// "上架于多久以前"。挂单的 `indexed` 是 RFC3339;解不出来就照原样显示,
/// 不猜也不吞。
pub(crate) fn age_text(indexed: &str, text: &'static Text) -> String {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(indexed) else {
        return indexed.to_owned();
    };
    ago_text(now_secs() - at.timestamp(), text)
}

/// 秒数 → "刚刚 / 3 分钟前 / 2 小时前 / 5 天前"。
pub(crate) fn ago_text(seconds: i64, text: &'static Text) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return text.common_age_just_now.to_owned();
    }
    if seconds < 3_600 {
        return i18n::fill(text.common_age_minutes, &[&(seconds / 60).to_string()]);
    }
    if seconds < 86_400 {
        return i18n::fill(text.common_age_hours, &[&(seconds / 3_600).to_string()]);
    }
    i18n::fill(text.common_age_days, &[&(seconds / 86_400).to_string()])
}

/// 倒计时:秒 / 分 / 小时,只留一位。"还有 87 秒"比"还有 1 分 27 秒"好读。
pub(crate) fn countdown_text(seconds: i64, text: &'static Text) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return format!("{seconds} {}", text.common_seconds_short);
    }
    if seconds < 3_600 {
        return format!("{} {}", seconds / 60, text.common_minutes_short);
    }
    format!("{} {}", seconds / 3_600, text.common_hours_short)
}

/// unix 秒 → 本地 `%H:%M:%S`。时间戳坏了就显示原样的数字,不 panic。
pub(crate) fn local_clock(at: i64) -> String {
    local_format(at, "%H:%M:%S")
}

/// unix 秒 → 本地 `%m-%d %H:%M`。提醒历史用它:只有时分的话,
/// 昨天和今天的提醒长得一模一样。
pub(crate) fn local_stamp(at: i64) -> String {
    local_format(at, "%m-%d %H:%M")
}

fn local_format(at: i64, pattern: &str) -> String {
    chrono::DateTime::from_timestamp(at, 0).map_or_else(
        || at.to_string(),
        |utc| {
            utc.with_timezone(&chrono::Local)
                .format(pattern)
                .to_string()
        },
    )
}

/// 按字符裁断,裁掉了就留一个省略号。
///
/// 按 `chars()` 不按字节:中文一个字三个字节,按字节裁会把一个字劈成两半,
/// 而半个字在屏幕上是一个乱码方块。
pub(crate) fn clip(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let head: String = value.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod link_tests {
    use pnd_domain::{Currency, Price, WatchId};
    use pnd_storage::AlertSource;

    use super::*;
    use crate::i18n;

    fn listing() -> ListingSummary {
        ListingSummary {
            id: "one".to_string(),
            item_name: "Choir of the Storm".to_string(),
            type_line: "Lapis Amulet".to_string(),
            price: Some(Price::new(15_000, Currency::Divine)),
            account: "Exile#1234".to_string(),
            character: "ExileChar".to_string(),
            online: true,
            afk: false,
            indexed: "2026-09-06T12:00:00Z".to_string(),
            whisper: "@ExileChar hi".to_string(),
            whisper_token: None,
            hideout_token: None,
            icon: String::new(),
        }
    }

    fn matched(extra: usize) -> MatchedListing {
        MatchedListing {
            alert_ids: vec![7, 8, 9, 10],
            watch_id: WatchId("w-1".to_string()),
            label: "Choir of the Storm".to_string(),
            league: "Forbidden Rites".to_string(),
            search_id: "H4sIAAAA-_09".to_string(),
            headline: listing(),
            extra,
            cap: Price::new(20_000, Currency::Divine),
            source: AlertSource::Poll,
        }
    }

    /// 卡片上第一眼看到的两件事:是什么、多少钱。
    #[test]
    fn the_card_title_is_the_item_and_its_price() {
        let card = card_text_for(&matched(3), &i18n::ENGLISH, "search 1 / 299 in 6 h");
        assert_eq!(card.title, "Choir of the Storm · 15 divine");
        assert_eq!(card.footer, "search 1 / 299 in 6 h");
        assert_eq!(card.buttons[0], "Open trade");
        assert_eq!(card.buttons[3], "Dismiss");
    }

    /// 一轮里命中好几件时,第二行末尾要写清楚"另有几件"——不然你只会去买
    /// 标题上那一件,剩下的更便宜的就错过了。
    #[test]
    fn extra_hits_are_counted_on_the_second_line() {
        let card = card_text_for(&matched(3), &i18n::ENGLISH, "");
        assert!(card.line2.contains("cap 20 divine"), "{}", card.line2);
        assert!(card.line2.ends_with("+3 more"), "{}", card.line2);

        let single = card_text_for(&matched(0), &i18n::ENGLISH, "");
        assert!(!single.line2.contains("more"), "{}", single.line2);
    }

    /// 脚注取不到预算就写搜索的名字,不留空。
    #[test]
    fn an_unknown_budget_falls_back_to_the_watch_label() {
        let card = card_text_for(&matched(0), &i18n::ENGLISH, "");
        assert_eq!(card.footer, "Choir of the Storm");
    }

    /// 中文目录下每一行也得是中文,而且照样裁得住长度。
    #[test]
    fn the_card_speaks_the_selected_language() {
        let card = card_text_for(&matched(2), &i18n::SIMPLIFIED_CHINESE, "");
        assert_eq!(card.buttons[0], "打开交易页");
        assert!(card.line1.starts_with("卖家 Exile#1234"), "{}", card.line1);
        assert!(card.line2.ends_with("另有 2 件"), "{}", card.line2);
    }

    #[test]
    fn every_line_fits_the_card() {
        let mut long = matched(1);
        long.headline.item_name = "极长的物品名".repeat(30);
        long.headline.account = "x".repeat(200);
        let card = card_text_for(&long, &i18n::ENGLISH, &"f".repeat(400));
        assert!(card.title.chars().count() <= CARD_TITLE_CHARS);
        assert!(card.line1.chars().count() <= CARD_LINE_CHARS);
        assert!(card.footer.chars().count() <= CARD_LINE_CHARS);
        assert!(card.title.ends_with('…'));
    }

    #[test]
    fn ages_and_countdowns_read_like_a_person_wrote_them() {
        let text = &i18n::ENGLISH;
        assert_eq!(ago_text(0, text), "just now");
        assert_eq!(ago_text(59, text), "just now");
        assert_eq!(ago_text(60, text), "1 min ago");
        assert_eq!(ago_text(3_600, text), "1 h ago");
        assert_eq!(ago_text(90_000, text), "1 d ago");
        // 时钟往回跳(或者挂单的时间戳比本机时间新)不该出现负数。
        assert_eq!(ago_text(-5, text), "just now");

        assert_eq!(countdown_text(42, text), "42 s");
        assert_eq!(countdown_text(90, text), "1 min");
        assert_eq!(countdown_text(7_200, text), "2 h");
    }

    /// 解不出来的时间戳原样显示:显示一个看得懂的怪字符串,好过显示
    /// 一个编出来的时间。
    #[test]
    fn an_unparsable_timestamp_is_shown_as_is() {
        assert_eq!(age_text("whenever", &i18n::ENGLISH), "whenever");
    }

    /// 五种进展各说各的话,而且失败那句必须带上状态码:503(token 过期)
    /// 和 403(会话不对)要做的下一步完全不同。
    #[test]
    fn every_hideout_outcome_says_something_of_its_own() {
        let outcomes = [
            HideoutOutcome::Sent,
            HideoutOutcome::NoSession,
            HideoutOutcome::TokenMissing,
            HideoutOutcome::Refreshed,
            HideoutOutcome::Failed {
                status: 503,
                message: "busy".to_string(),
            },
        ];
        for language in i18n::LANGUAGES {
            let text = i18n::text(language);
            let mut lines: Vec<String> = outcomes
                .iter()
                .map(|outcome| hideout_text(outcome, text))
                .collect();
            assert!(lines.iter().all(|line| !line.trim().is_empty()));
            assert!(lines[4].contains("503"), "{}", lines[4]);
            lines.sort();
            let count = lines.len();
            lines.dedup();
            assert_eq!(lines.len(), count, "{language} 里有两种结局撞词了");
        }
    }

    /// 结局要能塞进卡片脚注 —— 那一行有长度上限,超了整张卡片就构造不出来。
    #[test]
    fn a_hideout_result_fits_the_card_footer() {
        let text = &i18n::ENGLISH;
        let footer = hideout_text(&HideoutOutcome::TokenMissing, text);
        let card = card_text_for(&matched(0), text, &footer);
        assert_eq!(card.footer, "hideout: no token");
    }

    #[test]
    fn clipping_counts_characters_not_bytes() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "ab…");
        // 中文一个字三个字节:按字节裁会劈出半个字。
        assert_eq!(clip("风暴合唱之歌", 4), "风暴合…");
    }

    #[test]
    fn local_times_do_not_panic_on_odd_values() {
        assert!(!local_clock(0).is_empty());
        assert!(!local_stamp(i64::MAX).is_empty());
    }
}
