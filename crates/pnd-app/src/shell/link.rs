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
//! - **游戏内的动作永远是用户点的。** "去藏身处"在这里只是一句提示 +
//!   一条历史记录,不发任何私聊/传送请求(那是第二阶段的事)。

use std::time::{Duration, Instant};

use gpui::{ClipboardItem, Context};
use pnd_domain::{ListingSummary, SearchRef, search_page_url};
use pnd_platform_win::{
    AlertCardService, CardButton, CardConfig, CardError, CardEvent, CardText, Corner,
    ValidatedWave, built_in_alert_wave, open_url,
};
use pnd_runtime::{
    MatchedListing, RuntimeCommand, RuntimeEvent, RuntimeHandle, RuntimePaths, now_secs,
};
use pnd_settings::AppSettings;
use pnd_trade::{BucketUsage, FETCH_POLICY, SEARCH_POLICY};

use super::AppShell;
use crate::i18n::{self, Text};

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
            RuntimeEvent::Budget { policy, usage, .. } => {
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
            // 去藏身处的结果先只进日志;卡片脚注和提醒表的显示是下一步。
            RuntimeEvent::HideoutResult { alert_id, outcome } => {
                self.push_log(format!("hideout: alert {alert_id} → {outcome:?}"));
                self.refresh_alerts_soon();
            }
            RuntimeEvent::Log(line) => self.push_log(line),
            RuntimeEvent::Fault(line) => {
                self.push_log(format!("runtime fault: {line}"));
                self.set_sticky_notice(i18n::fill(text.notice_runtime_failed, &[&line]));
            }
        }
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
                // 第二阶段才有的按钮。**不发任何请求** —— 程序永远不自动私聊、
                // 不自动传送,这一条在计划里是硬规矩。
                self.set_notice(text.notice_hideout_phase_two.to_owned());
                self.record_action(alert_id, "hideout_unavailable");
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
