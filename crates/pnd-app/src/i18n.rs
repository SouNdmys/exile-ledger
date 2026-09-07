//! 双语界面文案目录(English / 简体中文)。
//!
//! 一个结构体两份值,而不是一张查找表:少写一个键是**编译错误**,不是用户
//! 某天看见的一块空白。`AppShell` 按存下来的语言挑一份,换语言下一帧就生效,
//! 不用重建任何东西。
//!
//! 带 `{}` 的是模板,插值走本文件的 `fill`。同一条模板两种语言的 `{}` 个数
//! 必须一样 —— 少一个那个值就丢在屏幕外,多一个屏幕上就印着一对花括号。
//! 底部的测试守这条。

/// 声明目录:一处写字段名,同时长出 `Text` 结构体和测试用的 `fields()`。
///
/// (给不熟悉 Rust 的读者:`macro_rules!` 是"写代码的代码"。下面这段的作用
/// 只是把 `catalogue! { a, b, c }` 展开成 `pub struct Text { pub a: ..., }`
/// 外加一个"把每个字段和它的名字配成对"的函数。之所以要它,是因为兄弟项目
/// 那份手写的字段清单落后了结构体二十条 —— 清单和结构体是同一份信息,
/// 写两遍就一定会分家。)
macro_rules! catalogue {
    ($( $(#[$meta:meta])* $field:ident ),+ $(,)?) => {
        /// 界面画出来的每一条字符串。
        pub struct Text {
            $( $(#[$meta])* pub $field: &'static str, )+
        }

        impl Text {
            /// 每个字段配上它的名字。测试遍历它,所以清单不可能落后于结构体。
            #[cfg(test)]
            fn fields(&self) -> Vec<(&'static str, &'static str)> {
                vec![ $( (stringify!($field), self.$field), )+ ]
            }
        }
    };
}

catalogue! {
    // -- 程序本身 --
    app_title,

    // -- 左导航 --
    nav_watches,
    nav_alerts,
    nav_ninja_uniques,
    nav_ninja_mods,
    nav_settings,

    // -- 通用词 --
    common_all,
    common_on,
    common_off,
    common_none,
    common_seconds_short,
    common_minutes_short,
    common_hours_short,
    common_milliseconds_short,
    common_percent,
    common_days_short,
    /// 表格上的动作按钮作用在选中的那一行,没选中就说这一句。
    common_select_row,
    /// 两张热度榜上那个"连冷门的也铺出来"开关。
    common_show_all,
    /// "已隐藏 {} 条占比不足 {} 的" —— 藏了东西必须说出来。
    common_hidden_rows,
    /// 一分钟以内的"多久以前"。
    common_age_just_now,
    /// "{} 分钟前"
    common_age_minutes,
    /// "{} 小时前"
    common_age_hours,
    /// "{} 天前"
    common_age_days,

    // -- 蹲价页 --
    watches_heading,
    watches_subtitle,
    /// "粘一条搜索 URL 或 id"
    watches_add_search_label,
    watches_add_search_placeholder,
    watches_label_label,
    watches_label_placeholder,
    /// 表单里的联赛格。留空就用设置页那个联赛。
    watches_league_label,
    watches_league_placeholder,
    watches_price_cap_label,
    watches_price_cap_placeholder,
    /// 上限格旁边那句话:上限本身也算命中("≤",不是"<")。
    watches_cap_hint,
    watches_currency_label,
    watches_add_button,
    watches_enable_toggle,
    watches_live_toggle,
    watches_poll_now,
    watches_col_label,
    watches_col_league,
    watches_col_cap,
    watches_col_status,
    watches_col_last_poll,
    watches_col_hits_today,
    watches_empty,
    /// 表格里"下一轮什么时候":"{} 后再轮询"
    watches_next_in,
    /// 表格里"到底多久一次":"每 {} 秒"。倒计时只说下一轮,说不出节奏 ——
    /// 而节奏会因为秒推连上、退避、多加一条搜索而变。
    watches_poll_every,
    /// 还没轮询过的那一格。
    watches_never,
    /// 选中一行之后那一排按钮的标题。
    watches_row_actions,
    watches_remove,
    watches_added,
    watches_removed,
    watches_invalid_search,
    watches_invalid_cap,
    watches_poll_requested,
    /// 选中一行之后表单变成"改这一条":按钮换字,旁边多一个取消。
    /// "正在编辑 {}"
    watches_editing,
    watches_save_changes,
    watches_cancel_edit,
    watches_changes_saved,
    /// 改搜索本身要走"删了重加",因为 runtime 的一切都挂在这条搜索的 id 上。
    watches_edit_search_locked,
    /// live 连上之后轮询会自动放慢。这句写在表格下面而不是状态那一格里:
    /// 每条连上的搜索都是同一句话,抄进 430 像素的格子里只会把倒计时挤掉。
    watches_live_relaxed,
    /// "1 divine = {} chaos / {} exalted"
    watches_rates,
    watches_rates_unknown,
    /// 状态词,和 `pnd-runtime` 的搜索状态一一对应。
    status_disabled,
    status_polling,
    status_backoff,
    status_live,
    status_held,
    /// live 那一头的档位,和 `LiveRunState` 一一对应。轮询状态只说"在不在跑",
    /// 这几条才说得清"秒推为什么没连上"。
    live_off,
    /// "live disabled: {}" —— 想跑但跑不了,后面接原因。
    live_disabled,
    live_connecting,
    /// "live since {}"
    live_connected_since,
    /// "live retry in {} (attempt {})"
    live_backoff,
    /// "live held until {}"
    live_held_until,
    live_reason_no_session,
    live_reason_too_many,
    live_reason_session_invalid,
    budget_strip_title,
    budget_search,
    budget_fetch,
    /// "{} 秒后可再请求"
    budget_next_allowed,
    /// "{} / {} 次(6 小时预算)"
    budget_used_of,

    // -- 提醒记录页 --
    alerts_heading,
    alerts_subtitle,
    alerts_col_time,
    alerts_col_item,
    alerts_col_price,
    alerts_col_seller,
    alerts_col_source,
    alerts_col_action,
    alerts_open,
    alerts_copy_whisper,
    alerts_hideout,
    alerts_dismiss,
    alerts_empty,
    alerts_source_poll,
    alerts_source_live,
    alerts_action_none,
    alerts_action_opened,
    alerts_action_dismissed,
    alerts_action_copied,
    /// 没有 POESESSID 时,"去藏身处"那个按钮是灰的,旁边写这一句。
    alerts_hideout_needs_session,

    // -- 暗金热度页 --
    uniques_heading,
    uniques_subtitle,
    uniques_partition_label,
    /// 分区键 `""`。
    uniques_partition_whole,
    /// "职业 {}"
    uniques_partition_class,
    /// "技能 {}"
    uniques_partition_skill,
    /// "穿着 {}"
    uniques_partition_item,
    /// "{} · {}" —— 职业那半句 · 技能名。
    uniques_partition_class_skill,
    uniques_col_rank,
    uniques_col_name,
    uniques_col_characters,
    uniques_col_share,
    uniques_col_reference_price,
    uniques_col_listings,
    uniques_col_seven_day,
    uniques_refresh,
    /// "快照 {}"
    uniques_snapshot,
    /// "已采 {} 个角色"
    uniques_sampled,
    uniques_no_snapshot,
    /// "{} ex"
    uniques_price_exalted,
    /// "{} ex ≈ {} div"
    uniques_price_with_divine,
    /// "参考价取自 {}"
    uniques_price_age,
    uniques_empty,

    // -- 词缀热度页 --
    mods_heading,
    mods_subtitle,
    mods_slot_label,
    mods_rarity_label,
    mods_kind_label,
    mods_col_stat,
    mods_col_family,
    mods_col_characters_percent,
    mods_col_characters,
    mods_col_occurrences,
    mods_col_p25,
    mods_col_p50,
    mods_col_p75,
    /// "统计自 {} 个穿了 {} 的角色(一共采样 {} 个)"
    mods_sample_line,
    /// 筛选器停在"全部"时的那一句:"所有部位合起来,一共采样 {} 个角色"
    mods_sample_any,
    mods_empty,
    mods_rarity_unique,
    mods_rarity_rare,
    mods_rarity_magic,
    mods_rarity_normal,
    mods_kind_explicit,
    mods_kind_implicit,
    mods_kind_crafted,
    mods_kind_desecrated,
    mods_kind_rune,

    // -- ninja 采样 --
    ninja_stage_planned,
    ninja_stage_facets,
    ninja_stage_characters,
    ninja_stage_aggregated,
    ninja_status_starting,
    /// "开始采样快照 {}"
    ninja_status_started,
    /// "这一轮跳过:{}"
    ninja_status_skipped,
    /// "{} 完成"
    ninja_status_stage_done,
    /// "参考价 {} / {} 类"
    ninja_status_prices,
    ninja_status_finished,
    /// "采样失败:{}"
    ninja_status_failed,
    ninja_store_missing,

    // -- 设置页 --
    settings_heading,
    settings_section_general,
    settings_section_session,
    settings_section_watcher,
    settings_section_alert,
    settings_section_ninja,
    settings_language,
    settings_league,
    settings_league_placeholder,
    settings_poesessid,
    settings_poesessid_placeholder,
    /// 设置页那个"登录官网"按钮,以及它带出来的那扇窗和那条状态字。
    settings_login,
    settings_login_hint,
    /// 自动那条没触发时的退路:回到设置页按一下,程序重新去核对一次。
    settings_login_check_now,
    settings_login_get_webview2,
    /// 登录窗自己的标题和顶上那条提示,由 `pnd-app` 传给平台层 ——
    /// 平台层不认识任何一门界面语言。
    settings_login_window_title,
    settings_login_window_hint,
    settings_login_opening,
    settings_login_rechecking,
    settings_login_waiting,
    settings_login_captured,
    settings_login_closed,
    /// "登录窗出错:{}"
    settings_login_failed,
    /// 没有 WebView2 时那一句:一行说清手动那条路怎么走。
    settings_login_runtime_missing,
    settings_test_session,
    /// 明说这一下要花掉一次搜索额度 —— 按钮不该有看不见的代价。
    settings_test_session_hint,
    settings_session_checking,
    settings_session_ok,
    settings_session_bad,
    /// 明说它是明文存在本机的 —— 用户有权先知道再决定要不要粘。
    settings_poesessid_hint,
    settings_poll_interval,
    settings_poll_interval_when_live,
    settings_max_live_connections,
    settings_budget_percent,
    settings_sound,
    settings_custom_sound_path,
    settings_custom_sound_placeholder,
    settings_auto_hide_minutes,
    settings_corner,
    settings_opacity,
    settings_ninja_sample_target,
    settings_ninja_refresh_hours,
    settings_ninja_request_gap,
    settings_user_agent_mode,
    settings_user_agent_identified,
    settings_user_agent_browser,
    settings_save,
    settings_saved,
    /// "保存失败:{}"
    settings_save_failed,
    settings_read_only,
    settings_file_path,
    corner_top_left,
    corner_top_right,
    corner_bottom_left,
    corner_bottom_right,

    // -- 提醒卡片(第 7b 步接线,文案先在这儿备好) --
    /// "{} · {}" —— 物品名 · 价格
    card_title,
    /// "卖家 {} · {}"
    card_line_seller,
    /// "上架 {}"
    card_line_listed,
    /// "上限 {} · 判定 {}"
    card_line_cap,
    card_open_trade,
    card_copy_whisper,
    card_hideout,
    card_dismiss,
    /// "另有 {} 件"
    card_more,
    card_verdict_hit,
    card_verdict_different_currency,
    card_whisper_copied,
    card_online,
    card_afk,
    card_offline,

    // -- 去藏身处的结局 --
    // 一次点击的结果要出现在三个地方(卡片脚注、状态行、提醒记录的动作列),
    // 三处用同一句话:同一件事在两个地方措辞不同,只会让人以为是两件事。
    hideout_sent,
    hideout_no_session,
    hideout_token_missing,
    hideout_refreshing,
    hideout_failed,
    /// "hideout: not sent — {}" —— 状态码 0 = 请求压根没出门(网络断了、
    /// 会话拼不出来)。这时候写个 "HTTP 0" 只会让人去查一个不存在的状态码。
    hideout_not_sent,
    /// "hideout: failed HTTP {}: {}" —— 状态码 + 交易站自己说的那句话。
    /// 失败时那两样是唯一的线索,而提醒记录里只存得下动作码,所以要单列一条。
    hideout_failed_http,

    // -- 状态行上的一句话 --
    notice_opened_trade,
    /// "打不开浏览器:{}"
    notice_open_failed,
    notice_no_whisper,
    notice_dismissed,
    notice_session_invalid,
    /// "…请求要等到 {} 才继续"
    notice_cloudflare,
    /// "后台运行时没起来:{}"
    notice_runtime_failed,
    notice_runtime_gone,
    /// "提醒卡片没起来:{}"
    notice_card_failed,
    notice_card_missing,

    // -- 日志抽屉 --
    // 状态行只放得下最后一条。真出事的时候要看的是前面那几条,所以给它
    // 一个抽屉,并且每一条都落到盘上 —— 程序关掉之后还查得回来。
    log_toggle,
    log_copy,
    log_copied,
    log_empty,
    /// "写到 {}"
    log_file_path,
}

/// 支持的语言码。设置里存的就是这两个字符串。
pub const LANGUAGES: [&str; 2] = ["en", "zh"];

/// 挑一份目录。`"zh"` 给中文,别的一律英文 —— 认不出来的语言码不该让
/// 界面一片空白。
#[must_use]
pub fn text(language: &str) -> &'static Text {
    if language == "zh" {
        &SIMPLIFIED_CHINESE
    } else {
        &ENGLISH
    }
}

/// 语言在语言选择器里的名字,永远写它自己那门语言:读不懂当前这门的人
/// 也能找到自己的。
#[must_use]
pub fn native_label(language: &str) -> &'static str {
    if language == "zh" {
        "简体中文"
    } else {
        "English"
    }
}

/// 把模板里的 `{}` 按顺序换成给的值。
///
/// 多出来的 `{}` 原样留着(屏幕上看得见,好过悄悄吞掉),值多了就忽略。
#[must_use]
pub fn fill(template: &str, values: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    let mut values = values.iter();
    while let Some((before, after)) = rest.split_once("{}") {
        let Some(value) = values.next() else {
            break;
        };
        out.push_str(before);
        out.push_str(value);
        rest = after;
    }
    out.push_str(rest);
    out
}

pub static ENGLISH: Text = Text {
    app_title: "POE Ninja Data",

    nav_watches: "Watches",
    nav_alerts: "Alerts",
    nav_ninja_uniques: "Unique heat",
    nav_ninja_mods: "Modifier heat",
    nav_settings: "Settings",

    common_all: "All",
    common_on: "On",
    common_off: "Off",
    common_none: "—",
    common_seconds_short: "s",
    common_minutes_short: "min",
    common_hours_short: "h",
    common_milliseconds_short: "ms",
    common_percent: "%",
    common_days_short: "d",
    common_select_row: "Select a row first.",
    common_show_all: "Show all",
    common_hidden_rows: "{} rows below {} hidden",
    common_age_just_now: "just now",
    common_age_minutes: "{} min ago",
    common_age_hours: "{} h ago",
    common_age_days: "{} d ago",

    watches_heading: "Watches",
    watches_subtitle: "Filter on the trade site, paste the search here, and the poller takes it from there.",
    watches_add_search_label: "Search URL or id",
    watches_add_search_placeholder: "https://www.pathofexile.com/trade2/search/poe2/... or H4sI...",
    watches_label_label: "Label",
    watches_label_placeholder: "Choir of the Storm",
    watches_league_label: "League",
    watches_league_placeholder: "empty = the league from Settings",
    watches_price_cap_label: "Price cap",
    watches_price_cap_placeholder: "20",
    watches_cap_hint: "alerts when price ≤ cap",
    watches_currency_label: "Currency",
    watches_add_button: "Add watch",
    watches_enable_toggle: "Enabled",
    watches_live_toggle: "Live search",
    watches_poll_now: "Poll now",
    watches_col_label: "Label",
    watches_col_league: "League",
    watches_col_cap: "Cap",
    watches_col_status: "Status",
    watches_col_last_poll: "Last poll",
    watches_col_hits_today: "Hits today",
    watches_empty: "No watches yet. Paste a search above to start one.",
    watches_next_in: "next in {}",
    watches_poll_every: "every {} s",
    watches_never: "never",
    watches_row_actions: "Selected watch",
    watches_remove: "Remove",
    watches_added: "Watch added.",
    watches_removed: "Watch removed.",
    watches_invalid_search: "That is not a trade search URL or id.",
    watches_invalid_cap: "The price cap has to be a number above zero.",
    watches_poll_requested: "Polling that watch now.",
    watches_editing: "Editing {}",
    watches_save_changes: "Save changes",
    watches_cancel_edit: "Cancel",
    watches_changes_saved: "Changes saved.",
    watches_edit_search_locked: "The search itself cannot be edited. To point this watch somewhere else, remove it and add the new search.",
    watches_live_relaxed: "Live search is connected, so those watches poll on the slower interval.",
    watches_rates: "1 divine = {} chaos / {} exalted",
    watches_rates_unknown: "rates not loaded yet",
    status_disabled: "disabled",
    status_polling: "polling",
    status_backoff: "backoff",
    status_live: "live",
    status_held: "held",
    live_off: "live off",
    live_disabled: "live disabled: {}",
    live_connecting: "live connecting",
    live_connected_since: "live since {}",
    live_backoff: "live retry in {} (attempt {})",
    live_held_until: "live held until {}",
    live_reason_no_session: "no session",
    live_reason_too_many: "too many",
    live_reason_session_invalid: "session invalid",
    budget_strip_title: "Rate budget",
    budget_search: "search",
    budget_fetch: "fetch",
    budget_next_allowed: "next allowed in {} s",
    budget_used_of: "{} / {} in 6 h",

    alerts_heading: "Alerts",
    alerts_subtitle: "Every card that fired, and what you did about it.",
    alerts_col_time: "Time",
    alerts_col_item: "Item",
    alerts_col_price: "Price",
    alerts_col_seller: "Seller",
    alerts_col_source: "Source",
    alerts_col_action: "Action",
    alerts_open: "Open trade",
    alerts_copy_whisper: "Copy whisper",
    alerts_hideout: "Hideout",
    alerts_dismiss: "Dismiss",
    alerts_empty: "Nothing has fired yet. Alerts land here the moment a watch finds a good price.",
    alerts_source_poll: "poll",
    alerts_source_live: "live",
    alerts_action_none: "—",
    alerts_action_opened: "opened",
    alerts_action_dismissed: "dismissed",
    alerts_action_copied: "whisper copied",
    alerts_hideout_needs_session: "Travel to hideout needs a POESESSID — paste one on the Settings page.",

    uniques_heading: "Unique heat",
    uniques_subtitle: "What the popular builds wear, priced against the economy feed.",
    uniques_partition_label: "Partition",
    uniques_partition_whole: "Whole league",
    uniques_partition_class: "Class {}",
    uniques_partition_skill: "Skill {}",
    uniques_partition_item: "Wearing {}",
    uniques_partition_class_skill: "{} · {}",
    uniques_col_rank: "#",
    uniques_col_name: "Unique",
    uniques_col_characters: "Characters",
    uniques_col_share: "Share",
    uniques_col_reference_price: "Reference price",
    uniques_col_listings: "Listings",
    uniques_col_seven_day: "7d",
    uniques_refresh: "Refresh",
    uniques_snapshot: "snapshot {}",
    uniques_sampled: "{} characters sampled",
    uniques_no_snapshot: "no snapshot yet",
    uniques_price_exalted: "{} ex",
    uniques_price_with_divine: "{} ex ≈ {} div",
    uniques_price_age: "reference prices from {}",
    uniques_empty: "No snapshot yet. Refresh to take one.",

    mods_heading: "Modifier heat",
    mods_subtitle: "Which modifiers the sampled characters actually carry, by slot.",
    mods_slot_label: "Slot",
    mods_rarity_label: "Rarity",
    mods_kind_label: "Kind",
    mods_col_stat: "Stat",
    mods_col_family: "Family",
    mods_col_characters_percent: "Share",
    mods_col_characters: "Characters",
    mods_col_occurrences: "Occurrences",
    mods_col_p25: "p25",
    mods_col_p50: "p50",
    mods_col_p75: "p75",
    mods_sample_line: "from {} characters wearing {}, of {} sampled",
    mods_sample_any: "every slot together, out of {} sampled characters",
    mods_empty: "No modifier stats yet. Refresh on the Unique heat page samples characters and builds them.",
    mods_rarity_unique: "Unique",
    mods_rarity_rare: "Rare",
    mods_rarity_magic: "Magic",
    mods_rarity_normal: "Normal",
    mods_kind_explicit: "Explicit",
    mods_kind_implicit: "Implicit",
    mods_kind_crafted: "Crafted",
    mods_kind_desecrated: "Desecrated",
    mods_kind_rune: "Rune",

    ninja_stage_planned: "planning",
    ninja_stage_facets: "facets",
    ninja_stage_characters: "characters",
    ninja_stage_aggregated: "modifiers",
    ninja_status_starting: "Sampling started.",
    ninja_status_started: "Sampling snapshot {}",
    ninja_status_skipped: "Skipped: {}",
    ninja_status_stage_done: "{} done",
    ninja_status_prices: "reference prices {} / {}",
    ninja_status_finished: "Sampling finished.",
    ninja_status_failed: "Sampling failed: {}",
    ninja_store_missing: "ninja.sqlite could not be opened, so the two heat pages stay empty.",

    settings_heading: "Settings",
    settings_section_general: "General",
    settings_section_session: "Trade session",
    settings_section_watcher: "Polling and budget",
    settings_section_alert: "Alert card",
    settings_section_ninja: "poe.ninja sampling",
    settings_language: "Language",
    settings_league: "League",
    settings_league_placeholder: "Forbidden Rites",
    settings_poesessid: "POESESSID",
    settings_poesessid_placeholder: "paste your session cookie",
    settings_login: "Log in on pathofexile.com",
    settings_login_hint: "Opens the official login page in a window of its own. Your password goes to the site, never through this program; only the POESESSID cookie is read back when the login succeeds.",
    settings_login_check_now: "I'm logged in — check now",
    settings_login_get_webview2: "Get WebView2",
    settings_login_window_title: "POE Ninja Data — pathofexile.com login",
    settings_login_window_hint: "Log in as usual. This window closes by itself once the session is read.",
    settings_login_opening: "opening the login window…",
    settings_login_rechecking: "checking whether you are logged in…",
    settings_login_waiting: "not logged in yet",
    settings_login_captured: "session captured, checking it…",
    settings_login_closed: "the login window is closed",
    settings_login_failed: "the login window failed: {}",
    settings_login_runtime_missing: "No WebView2 on this machine. Install it, or log in with your browser and copy POESESSID from DevTools → Application → Cookies → pathofexile.com into the box above.",
    settings_test_session: "Test session",
    settings_test_session_hint: "Sends one search with the saved cookie — one request out of the 6 h search budget.",
    settings_session_checking: "checking…",
    settings_session_ok: "session recognised",
    settings_session_bad: "session not recognised",
    settings_poesessid_hint: "Stored in plain text in settings.json on this machine. Needed only for live search and hideout travel; polling works without it.",
    settings_poll_interval: "Poll interval",
    settings_poll_interval_when_live: "Poll interval when live",
    settings_max_live_connections: "Max live connections",
    settings_budget_percent: "Rate budget",
    settings_sound: "Sound",
    settings_custom_sound_path: "Custom sound file",
    settings_custom_sound_placeholder: "empty = built-in tone",
    settings_auto_hide_minutes: "Auto-hide after",
    settings_corner: "Card corner",
    settings_opacity: "Card opacity",
    settings_ninja_sample_target: "Sample target",
    settings_ninja_refresh_hours: "Refresh every",
    settings_ninja_request_gap: "Request gap",
    settings_user_agent_mode: "User agent",
    settings_user_agent_identified: "Identified",
    settings_user_agent_browser: "Browser",
    settings_save: "Save",
    settings_saved: "Saved.",
    settings_save_failed: "Save failed: {}",
    settings_read_only: "settings.json on disk was written by a newer version. Reading it is fine; saving is refused so nothing of yours gets overwritten.",
    settings_file_path: "File",
    corner_top_left: "Top left",
    corner_top_right: "Top right",
    corner_bottom_left: "Bottom left",
    corner_bottom_right: "Bottom right",

    card_title: "{} · {}",
    card_line_seller: "seller {} · {}",
    card_line_listed: "listed {}",
    card_line_cap: "cap {} · {}",
    card_open_trade: "Open trade",
    card_copy_whisper: "Copy whisper",
    card_hideout: "Hideout",
    card_dismiss: "Dismiss",
    card_more: "+{} more",
    card_verdict_hit: "under cap",
    card_verdict_different_currency: "different currency",
    card_whisper_copied: "Whisper copied to the clipboard.",
    card_online: "online",
    card_afk: "afk",
    card_offline: "offline",

    hideout_sent: "hideout: sent",
    hideout_no_session: "hideout: no session",
    hideout_token_missing: "hideout: no token",
    hideout_refreshing: "hideout: refreshing the token",
    hideout_failed: "hideout: failed",
    hideout_not_sent: "hideout: not sent — {}",
    hideout_failed_http: "hideout: failed HTTP {}: {}",

    notice_opened_trade: "Opened the trade page in your browser.",
    notice_open_failed: "Could not open the browser: {}",
    notice_no_whisper: "That listing has no whisper text.",
    notice_dismissed: "Dismissed.",
    notice_session_invalid: "The trade site did not recognise your POESESSID, so it has been dropped. Polling carries on anonymously.",
    notice_cloudflare: "Cloudflare is blocking the trade site. Requests are held until {}.",
    notice_runtime_failed: "The background runtime did not start: {}",
    notice_runtime_gone: "The background runtime is not running.",
    notice_card_failed: "The alert card did not start: {}",
    notice_card_missing: "No alert card on screen — the alert is on the Alerts page.",

    log_toggle: "Log",
    log_copy: "Copy",
    log_copied: "Log copied to the clipboard.",
    log_empty: "Nothing logged yet.",
    log_file_path: "written to {}",
};

pub static SIMPLIFIED_CHINESE: Text = Text {
    app_title: "POE Ninja Data",

    nav_watches: "蹲价",
    nav_alerts: "提醒记录",
    nav_ninja_uniques: "暗金热度",
    nav_ninja_mods: "词缀热度",
    nav_settings: "设置",

    common_all: "全部",
    common_on: "开",
    common_off: "关",
    common_none: "—",
    common_seconds_short: "秒",
    common_minutes_short: "分",
    common_hours_short: "小时",
    common_milliseconds_short: "毫秒",
    common_percent: "%",
    common_days_short: "天",
    common_select_row: "先在表里选中一行。",
    common_show_all: "显示全部",
    common_hidden_rows: "已隐藏 {} 条占比不足 {} 的",
    common_age_just_now: "刚刚",
    common_age_minutes: "{} 分钟前",
    common_age_hours: "{} 小时前",
    common_age_days: "{} 天前",

    watches_heading: "蹲价",
    watches_subtitle: "在网页上筛好条件,把搜索粘进来,剩下的交给轮询。",
    watches_add_search_label: "搜索 URL 或 id",
    watches_add_search_placeholder: "https://www.pathofexile.com/trade2/search/poe2/… 或 H4sI…",
    watches_label_label: "备注名",
    watches_label_placeholder: "风暴合唱",
    watches_league_label: "联赛",
    watches_league_placeholder: "留空 = 用设置页那个联赛",
    watches_price_cap_label: "价格上限",
    watches_price_cap_placeholder: "20",
    watches_cap_hint: "价格 ≤ 上限就提醒",
    watches_currency_label: "货币",
    watches_add_button: "新增搜索",
    watches_enable_toggle: "启用",
    watches_live_toggle: "Live 秒推",
    watches_poll_now: "立即轮询",
    watches_col_label: "备注名",
    watches_col_league: "联赛",
    watches_col_cap: "上限",
    watches_col_status: "状态",
    watches_col_last_poll: "上次轮询",
    watches_col_hits_today: "今日命中",
    watches_empty: "还没有搜索。在上面粘一条进来就开始蹲。",
    watches_next_in: "{} 后再轮询",
    watches_poll_every: "每 {} 秒",
    watches_never: "还没跑过",
    watches_row_actions: "选中的搜索",
    watches_remove: "删除",
    watches_added: "已新增搜索。",
    watches_removed: "已删除搜索。",
    watches_invalid_search: "这不是一条交易站搜索 URL 或 id。",
    watches_invalid_cap: "价格上限得是个大于 0 的数。",
    watches_poll_requested: "这就去轮询一次。",
    watches_editing: "正在改「{}」",
    watches_save_changes: "保存修改",
    watches_cancel_edit: "取消",
    watches_changes_saved: "已保存修改。",
    watches_edit_search_locked: "搜索本身改不了。要让这条搜索盯别的东西,把它删掉,重新粘一条进来。",
    watches_live_relaxed: "秒推已连上,连上的那几条按放宽后的间隔轮询。",
    watches_rates: "1 divine = {} chaos / {} exalted",
    watches_rates_unknown: "还没读到汇率",
    status_disabled: "已停用",
    status_polling: "轮询中",
    status_backoff: "退避中",
    status_live: "秒推中",
    status_held: "已暂停",
    live_off: "秒推关",
    live_disabled: "秒推停用:{}",
    live_connecting: "秒推连接中",
    live_connected_since: "秒推自 {}",
    live_backoff: "{} 后重连秒推(第 {} 次)",
    live_held_until: "秒推暂停到 {}",
    live_reason_no_session: "没有会话",
    live_reason_too_many: "超出连接上限",
    live_reason_session_invalid: "会话失效",
    budget_strip_title: "限速预算",
    budget_search: "搜索",
    budget_fetch: "抓取",
    budget_next_allowed: "{} 秒后可再请求",
    budget_used_of: "6 小时内 {} / {} 次",

    alerts_heading: "提醒记录",
    alerts_subtitle: "弹过的每一张卡片,以及你当时做了什么。",
    alerts_col_time: "时间",
    alerts_col_item: "物品",
    alerts_col_price: "价格",
    alerts_col_seller: "卖家",
    alerts_col_source: "来源",
    alerts_col_action: "动作",
    alerts_open: "打开交易页",
    alerts_copy_whisper: "复制私聊",
    alerts_hideout: "去藏身处",
    alerts_dismiss: "忽略",
    alerts_empty: "还没有提醒。哪条搜索捡到好价,记录就落在这里。",
    alerts_source_poll: "轮询",
    alerts_source_live: "秒推",
    alerts_action_none: "—",
    alerts_action_opened: "已打开",
    alerts_action_dismissed: "已忽略",
    alerts_action_copied: "已复制私聊",
    alerts_hideout_needs_session: "去藏身处需要 POESESSID —— 在设置页粘一个进来。",

    uniques_heading: "暗金热度",
    uniques_subtitle: "热门 BD 在穿什么,拼上经济接口的参考价。",
    uniques_partition_label: "分区",
    uniques_partition_whole: "全联赛",
    uniques_partition_class: "职业 {}",
    uniques_partition_skill: "技能 {}",
    uniques_partition_item: "穿着 {}",
    uniques_partition_class_skill: "{} · {}",
    uniques_col_rank: "#",
    uniques_col_name: "暗金",
    uniques_col_characters: "人数",
    uniques_col_share: "占比",
    uniques_col_reference_price: "参考价",
    uniques_col_listings: "挂单数",
    uniques_col_seven_day: "7 天",
    uniques_refresh: "刷新",
    uniques_snapshot: "快照 {}",
    uniques_sampled: "已采 {} 个角色",
    uniques_no_snapshot: "还没有快照",
    uniques_price_exalted: "{} ex",
    uniques_price_with_divine: "{} ex ≈ {} div",
    uniques_price_age: "参考价取自 {}",
    uniques_empty: "还没有快照。按刷新采一轮。",

    mods_heading: "词缀热度",
    mods_subtitle: "采样到的角色身上,各个部位实际带着哪些词缀。",
    mods_slot_label: "部位",
    mods_rarity_label: "稀有度",
    mods_kind_label: "词缀类型",
    mods_col_stat: "词缀",
    mods_col_family: "归并到",
    mods_col_characters_percent: "携带比例",
    mods_col_characters: "人数",
    mods_col_occurrences: "出现次数",
    mods_col_p25: "p25",
    mods_col_p50: "p50",
    mods_col_p75: "p75",
    mods_sample_line: "统计自 {} 个穿了 {} 的角色(一共采样 {} 个)",
    mods_sample_any: "所有部位合起来,一共采样 {} 个角色",
    mods_empty: "还没有词缀统计。去暗金热度页按刷新,采完角色就会有。",
    mods_rarity_unique: "暗金",
    mods_rarity_rare: "稀有",
    mods_rarity_magic: "魔法",
    mods_rarity_normal: "普通",
    mods_kind_explicit: "后缀词缀",
    mods_kind_implicit: "固定词缀",
    mods_kind_crafted: "工艺词缀",
    mods_kind_desecrated: "亵渎词缀",
    mods_kind_rune: "符文词缀",

    ninja_stage_planned: "排分区",
    ninja_stage_facets: "分区",
    ninja_stage_characters: "角色",
    ninja_stage_aggregated: "词缀",
    ninja_status_starting: "开始采样。",
    ninja_status_started: "开始采样快照 {}",
    ninja_status_skipped: "这一轮跳过:{}",
    ninja_status_stage_done: "{}采完了",
    ninja_status_prices: "参考价 {} / {} 类",
    ninja_status_finished: "采样跑完了。",
    ninja_status_failed: "采样失败:{}",
    ninja_store_missing: "打不开 ninja.sqlite,两张热度榜会一直是空的。",

    settings_heading: "设置",
    settings_section_general: "通用",
    settings_section_session: "交易站会话",
    settings_section_watcher: "轮询与预算",
    settings_section_alert: "提醒卡片",
    settings_section_ninja: "poe.ninja 采样",
    settings_language: "语言",
    settings_league: "联赛",
    settings_league_placeholder: "Forbidden Rites",
    settings_poesessid: "POESESSID",
    settings_poesessid_placeholder: "把会话 cookie 粘进来",
    settings_login: "登录官网",
    settings_login_hint: "开一个单独的窗口让你登官网。密码只进官网自己的页面,不经过本程序;登录成功后程序只读走 POESESSID 这一个 cookie。",
    settings_login_check_now: "我已登录,现在核对",
    settings_login_get_webview2: "去装 WebView2",
    settings_login_window_title: "POE Ninja Data —— 官网登录",
    settings_login_window_hint: "照平常那样登录。读到会话之后这个窗口会自己关掉。",
    settings_login_opening: "正在打开登录窗…",
    settings_login_rechecking: "正在核对有没有登录…",
    settings_login_waiting: "还没登录",
    settings_login_captured: "已读到会话,正在测试…",
    settings_login_closed: "登录窗已关闭",
    settings_login_failed: "登录窗出错:{}",
    settings_login_runtime_missing: "这台机器上没有 WebView2。装一个,或者用浏览器登录后从 开发者工具 → Application → Cookies → pathofexile.com 里把 POESESSID 复制到上面那个框里。",
    settings_test_session: "测试会话",
    settings_test_session_hint: "拿存下来的 cookie 发一次搜索 —— 花掉 6 小时搜索预算里的一次。",
    settings_session_checking: "正在测…",
    settings_session_ok: "会话有效",
    settings_session_bad: "交易站不认这个会话",
    settings_poesessid_hint: "明文存在本机的 settings.json 里。只有 Live 秒推和去藏身处需要它,轮询没有它照样跑。",
    settings_poll_interval: "轮询间隔",
    settings_poll_interval_when_live: "秒推健康时的间隔",
    settings_max_live_connections: "最多 live 连接",
    settings_budget_percent: "限速预算",
    settings_sound: "报警音",
    settings_custom_sound_path: "自定义音频文件",
    settings_custom_sound_placeholder: "留空 = 内置合成音",
    settings_auto_hide_minutes: "自动收起",
    settings_corner: "卡片位置",
    settings_opacity: "卡片不透明度",
    settings_ninja_sample_target: "采样目标",
    settings_ninja_refresh_hours: "多久重采一次",
    settings_ninja_request_gap: "请求间隔",
    settings_user_agent_mode: "User-Agent",
    settings_user_agent_identified: "自报家门",
    settings_user_agent_browser: "浏览器",
    settings_save: "保存",
    settings_saved: "已保存。",
    settings_save_failed: "保存失败:{}",
    settings_read_only: "盘上的 settings.json 是更新的版本写的。读没问题,保存会被拒绝,免得盖掉你的东西。",
    settings_file_path: "文件",
    corner_top_left: "左上",
    corner_top_right: "右上",
    corner_bottom_left: "左下",
    corner_bottom_right: "右下",

    card_title: "{} · {}",
    card_line_seller: "卖家 {} · {}",
    card_line_listed: "上架于 {}",
    card_line_cap: "上限 {} · {}",
    card_open_trade: "打开交易页",
    card_copy_whisper: "复制私聊",
    card_hideout: "去藏身处",
    card_dismiss: "忽略",
    card_more: "另有 {} 件",
    card_verdict_hit: "在上限内",
    card_verdict_different_currency: "币种不同",
    card_whisper_copied: "私聊内容已复制到剪贴板。",
    card_online: "在线",
    card_afk: "挂机",
    card_offline: "离线",

    hideout_sent: "去藏身处:已发出",
    hideout_no_session: "去藏身处:没有会话",
    hideout_token_missing: "去藏身处:没有 token",
    hideout_refreshing: "去藏身处:正在换 token",
    hideout_failed: "去藏身处:失败",
    hideout_not_sent: "去藏身处:没发出去 —— {}",
    hideout_failed_http: "去藏身处:失败 HTTP {}:{}",

    notice_opened_trade: "已在浏览器里打开交易页。",
    notice_open_failed: "打不开浏览器:{}",
    notice_no_whisper: "这条挂单没有私聊内容。",
    notice_dismissed: "已忽略。",
    notice_session_invalid: "交易站不认这个 POESESSID,程序已经停用它。轮询照常匿名跑。",
    notice_cloudflare: "Cloudflare 把交易站拦住了。请求要等到 {} 才继续。",
    notice_runtime_failed: "后台运行时没起来:{}",
    notice_runtime_gone: "后台运行时没在跑。",
    notice_card_failed: "提醒卡片没起来:{}",
    notice_card_missing: "屏幕上没有卡片 —— 这条提醒记在提醒记录页里。",

    log_toggle: "日志",
    log_copy: "复制",
    log_copied: "日志已复制到剪贴板。",
    log_empty: "还没有日志。",
    log_file_path: "写到 {}",
};

#[cfg(test)]
mod i18n_tests {
    use super::{ENGLISH, LANGUAGES, SIMPLIFIED_CHINESE, fill, native_label, text};

    #[test]
    fn both_catalogues_are_fully_populated() {
        // 结构体保证不会**少**一个字段,但保证不了字段是空的:空字符串照样
        // 编译得过,照样以一块空白发到用户手上。
        for language in LANGUAGES {
            for (field, value) in text(language).fields() {
                assert!(
                    !value.trim().is_empty(),
                    "{language} 的 {field} 是空的,那会以一块空白显示出去"
                );
            }
        }
    }

    #[test]
    fn both_catalogues_have_the_same_slots() {
        // 同一条模板两种语言要有同样多的 `{}`:少一个,那个值就丢在屏幕外;
        // 多一个,屏幕上就印着一对花括号。
        for ((field, en), (_, zh)) in ENGLISH
            .fields()
            .into_iter()
            .zip(SIMPLIFIED_CHINESE.fields())
        {
            assert_eq!(
                en.matches("{}").count(),
                zh.matches("{}").count(),
                "{field}: {en:?} vs {zh:?}"
            );
        }
    }

    #[test]
    fn the_two_catalogues_actually_differ() {
        // 把英文那份复制过来忘了翻译,能过上面两条测试,只坑用户。
        assert_ne!(ENGLISH.nav_watches, SIMPLIFIED_CHINESE.nav_watches);
        assert_ne!(
            ENGLISH.watches_add_button,
            SIMPLIFIED_CHINESE.watches_add_button
        );
        assert_ne!(ENGLISH.status_backoff, SIMPLIFIED_CHINESE.status_backoff);
        assert_ne!(ENGLISH.settings_save, SIMPLIFIED_CHINESE.settings_save);
        assert_ne!(ENGLISH.card_open_trade, SIMPLIFIED_CHINESE.card_open_trade);
    }

    #[test]
    fn only_zh_gets_the_chinese_catalogue() {
        assert_eq!(text("zh").nav_settings, SIMPLIFIED_CHINESE.nav_settings);
        for other in ["en", "", "EN", "zh-CN", "ja"] {
            assert_eq!(
                text(other).nav_settings,
                ENGLISH.nav_settings,
                "{other} 不是 zh,应该拿到英文"
            );
        }
    }

    #[test]
    fn every_language_the_settings_can_hold_has_a_native_label() {
        // `text` 认不出来的语言退回英文,所以这里守的是反向:加了语言就得
        // 加进 LANGUAGES,否则设置页的切换器悄悄不再提供它。
        assert_eq!(LANGUAGES.len(), 2);
        for language in LANGUAGES {
            assert!(!native_label(language).trim().is_empty());
        }
        assert_ne!(native_label("en"), native_label("zh"));
    }

    #[test]
    fn fill_replaces_slots_in_order() {
        assert_eq!(
            fill("{} · {}", &["Choir of the Storm", "18 div"]),
            "Choir of the Storm · 18 div"
        );
        assert_eq!(fill(ENGLISH.card_more, &["3"]), "+3 more");
        assert_eq!(fill(SIMPLIFIED_CHINESE.card_more, &["3"]), "另有 3 件");
        // 值不够就填到哪算哪,剩下的槽位原样留在屏幕上 —— 看得见的坏,
        // 好过悄悄吞掉一整句。
        assert_eq!(fill("{} / {}", &["1"]), "1 / {}");
        // 值多了就忽略。
        assert_eq!(fill("{}", &["a", "b"]), "a");
        // 没有槽位的普通句子原样通过。
        assert_eq!(fill(ENGLISH.settings_saved, &[]), "Saved.");
    }
}
