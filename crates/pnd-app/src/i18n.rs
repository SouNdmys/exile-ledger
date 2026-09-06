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

    // -- 蹲价页 --
    watches_heading,
    watches_subtitle,
    /// "粘一条搜索 URL 或 id"
    watches_add_search_label,
    watches_add_search_placeholder,
    watches_label_label,
    watches_label_placeholder,
    watches_price_cap_label,
    watches_price_cap_placeholder,
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
    /// 状态词,和 `pnd-runtime` 的搜索状态一一对应。
    status_disabled,
    status_polling,
    status_backoff,
    status_live,
    status_held,
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

    // -- 暗金热度页 --
    uniques_heading,
    uniques_subtitle,
    uniques_league_label,
    uniques_class_label,
    uniques_skill_label,
    uniques_col_name,
    uniques_col_characters,
    uniques_col_share,
    uniques_col_reference_price,
    uniques_col_listings,
    uniques_col_seven_day,
    uniques_refresh,
    /// "已采 {} / {} 个分区"
    uniques_progress,
    /// "快照是 {} 之前的,按刷新重新采一轮"
    uniques_stale_hint,
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
    mods_col_occurrences,
    mods_col_p25,
    mods_col_p50,
    mods_col_p75,
    mods_phase_two_placeholder,
    mods_rarity_unique,
    mods_rarity_rare,
    mods_rarity_magic,
    mods_kind_explicit,
    mods_kind_implicit,
    mods_kind_crafted,
    mods_kind_desecrated,
    mods_kind_rune,

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
    settings_test_session,
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
    card_hideout_sent,
    card_hideout_failed,
    card_whisper_copied,
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

    watches_heading: "Watches",
    watches_subtitle: "Filter on the trade site, paste the search here, and the poller takes it from there.",
    watches_add_search_label: "Search URL or id",
    watches_add_search_placeholder: "https://www.pathofexile.com/trade2/search/poe2/... or H4sI...",
    watches_label_label: "Label",
    watches_label_placeholder: "Choir of the Storm",
    watches_price_cap_label: "Price cap",
    watches_price_cap_placeholder: "20",
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
    status_disabled: "disabled",
    status_polling: "polling",
    status_backoff: "backoff",
    status_live: "live",
    status_held: "held",
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

    uniques_heading: "Unique heat",
    uniques_subtitle: "What the popular builds wear, priced against the economy feed.",
    uniques_league_label: "League",
    uniques_class_label: "Class",
    uniques_skill_label: "Skill",
    uniques_col_name: "Unique",
    uniques_col_characters: "Characters",
    uniques_col_share: "Share",
    uniques_col_reference_price: "Reference price",
    uniques_col_listings: "Listings",
    uniques_col_seven_day: "7d",
    uniques_refresh: "Refresh",
    uniques_progress: "{} / {} partitions sampled",
    uniques_stale_hint: "Snapshot is {} old. Refresh to sample again.",
    uniques_empty: "No snapshot yet. Refresh to take one.",

    mods_heading: "Modifier heat",
    mods_subtitle: "Which modifiers the sampled characters actually carry, by slot.",
    mods_slot_label: "Slot",
    mods_rarity_label: "Rarity",
    mods_kind_label: "Kind",
    mods_col_stat: "Stat",
    mods_col_family: "Family",
    mods_col_characters_percent: "Characters %",
    mods_col_occurrences: "Occurrences",
    mods_col_p25: "p25",
    mods_col_p50: "p50",
    mods_col_p75: "p75",
    mods_phase_two_placeholder: "Phase 2. Character sampling and modifier aggregation land in step 12; this page shows their shape so the layout is settled first.",
    mods_rarity_unique: "Unique",
    mods_rarity_rare: "Rare",
    mods_rarity_magic: "Magic",
    mods_kind_explicit: "Explicit",
    mods_kind_implicit: "Implicit",
    mods_kind_crafted: "Crafted",
    mods_kind_desecrated: "Desecrated",
    mods_kind_rune: "Rune",

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
    settings_test_session: "Test session",
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
    card_hideout_sent: "hideout request sent",
    card_hideout_failed: "hideout request failed",
    card_whisper_copied: "whisper copied",
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

    watches_heading: "蹲价",
    watches_subtitle: "在网页上筛好条件,把搜索粘进来,剩下的交给轮询。",
    watches_add_search_label: "搜索 URL 或 id",
    watches_add_search_placeholder: "https://www.pathofexile.com/trade2/search/poe2/… 或 H4sI…",
    watches_label_label: "备注名",
    watches_label_placeholder: "风暴合唱",
    watches_price_cap_label: "价格上限",
    watches_price_cap_placeholder: "20",
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
    status_disabled: "已停用",
    status_polling: "轮询中",
    status_backoff: "退避中",
    status_live: "秒推中",
    status_held: "已暂停",
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

    uniques_heading: "暗金热度",
    uniques_subtitle: "热门 BD 在穿什么,拼上经济接口的参考价。",
    uniques_league_label: "联赛",
    uniques_class_label: "职业",
    uniques_skill_label: "技能",
    uniques_col_name: "暗金",
    uniques_col_characters: "人数",
    uniques_col_share: "占比",
    uniques_col_reference_price: "参考价",
    uniques_col_listings: "挂单数",
    uniques_col_seven_day: "7 天",
    uniques_refresh: "刷新",
    uniques_progress: "已采 {} / {} 个分区",
    uniques_stale_hint: "快照是 {} 之前的。按刷新重新采一轮。",
    uniques_empty: "还没有快照。按刷新采一轮。",

    mods_heading: "词缀热度",
    mods_subtitle: "采样到的角色身上,各个部位实际带着哪些词缀。",
    mods_slot_label: "部位",
    mods_rarity_label: "稀有度",
    mods_kind_label: "词缀类型",
    mods_col_stat: "词缀",
    mods_col_family: "归并到",
    mods_col_characters_percent: "携带比例",
    mods_col_occurrences: "出现次数",
    mods_col_p25: "p25",
    mods_col_p50: "p50",
    mods_col_p75: "p75",
    mods_phase_two_placeholder: "第二阶段的页面。角色采样和词缀聚合排在第 12 步;这里先把版式定下来,数据到位就直接填。",
    mods_rarity_unique: "暗金",
    mods_rarity_rare: "稀有",
    mods_rarity_magic: "魔法",
    mods_kind_explicit: "后缀词缀",
    mods_kind_implicit: "固定词缀",
    mods_kind_crafted: "工艺词缀",
    mods_kind_desecrated: "亵渎词缀",
    mods_kind_rune: "符文词缀",

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
    settings_test_session: "测试会话",
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
    card_hideout_sent: "已发去藏身处请求",
    card_hideout_failed: "去藏身处失败",
    card_whisper_copied: "私聊内容已复制",
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
