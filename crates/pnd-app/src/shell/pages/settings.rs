//! 设置页。这一页现在就是真的:填的每一格都对应 `settings.json` 里的一个键,
//! 按保存就原子写盘。
//!
//! 语言是唯一"选了就立刻生效"的东西 —— 界面语言选错了,后面每一格都读不懂,
//! 让人先摸黑找到保存按钮再看见效果说不过去。它仍然要按保存才落盘,
//! 和别的格子一样。

use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{Disableable as _, Sizable as _, Size, StyledExt as _};

use crate::i18n::{self, Text};
use crate::shell::{
    AppShell, Choice, ChoiceSelect, choice_select, field_label, field_row, hint, page_heading,
    panel, picker,
};
use crate::theme::*;

/// 设置页上所有长命的控件状态。
///
/// GPUI 的输入框自己拿着光标和撤销历史,所以它们必须活得和外壳一样久 ——
/// 每帧新造一个,打到一半的字会在下一帧消失。
pub struct SettingsForm {
    pub language: ChoiceSelect,
    pub league: Entity<InputState>,
    pub poesessid: Entity<InputState>,
    pub poll_interval: Entity<InputState>,
    pub poll_interval_when_live: Entity<InputState>,
    pub max_live_connections: Entity<InputState>,
    pub budget_percent: Entity<InputState>,
    pub custom_sound_path: Entity<InputState>,
    pub auto_hide_minutes: Entity<InputState>,
    pub corner: ChoiceSelect,
    pub opacity: Entity<InputState>,
    pub ninja_sample_target: Entity<InputState>,
    pub ninja_refresh_hours: Entity<InputState>,
    pub ninja_request_gap: Entity<InputState>,
    pub user_agent_mode: ChoiceSelect,
}

/// 语言选项。名字永远写它自己那门语言 —— 读不懂当前这门的人也能找到自己的。
fn language_choices() -> Vec<Choice> {
    i18n::LANGUAGES
        .into_iter()
        .map(|language| Choice::new(language, i18n::native_label(language)))
        .collect()
}

/// 卡片角落。存的是 `bottom_right` 这种下划线写法,`pnd-platform-win` 认它。
fn corner_choices(text: &'static Text) -> Vec<Choice> {
    vec![
        Choice::new("top_left", text.corner_top_left),
        Choice::new("top_right", text.corner_top_right),
        Choice::new("bottom_left", text.corner_bottom_left),
        Choice::new("bottom_right", text.corner_bottom_right),
    ]
}

/// User-Agent 两档。`identified` 是默认;哪天交易站开始 403 再切浏览器那档。
fn user_agent_choices(text: &'static Text) -> Vec<Choice> {
    vec![
        Choice::new("identified", text.settings_user_agent_identified),
        Choice::new("browser", text.settings_user_agent_browser),
    ]
}

/// `UserAgentMode` 在 `settings.json` 里的写法。设置结构体没有暴露这个字符串,
/// 而下拉的值必须和它对得上,所以在这里翻译一次。
fn user_agent_value(mode: pnd_settings::UserAgentMode) -> &'static str {
    match mode {
        pnd_settings::UserAgentMode::Identified => "identified",
        pnd_settings::UserAgentMode::Browser => "browser",
    }
}

fn user_agent_mode(value: &str) -> pnd_settings::UserAgentMode {
    if value == "browser" {
        pnd_settings::UserAgentMode::Browser
    } else {
        pnd_settings::UserAgentMode::Identified
    }
}

impl SettingsForm {
    pub fn new(
        settings: &pnd_settings::AppSettings,
        text: &'static Text,
        window: &mut Window,
        cx: &mut Context<AppShell>,
    ) -> Self {
        let mut input = |value: String, placeholder: &'static str, masked: bool| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
                    .default_value(value)
            })
        };

        let league = input(
            settings.league.clone(),
            text.settings_league_placeholder,
            false,
        );
        // 会话 cookie 默认打码:它就摆在一个游戏旁边的窗口里,肩后随时有人。
        let poesessid = input(
            settings.poesessid.clone(),
            text.settings_poesessid_placeholder,
            true,
        );
        let poll_interval = input(
            settings.watcher.poll_interval_seconds.to_string(),
            "300",
            false,
        );
        let poll_interval_when_live = input(
            settings.watcher.poll_interval_when_live_seconds.to_string(),
            "900",
            false,
        );
        let max_live_connections = input(
            settings.watcher.max_live_connections.to_string(),
            "5",
            false,
        );
        let budget_percent = input(settings.watcher.budget_percent.to_string(), "50", false);
        let custom_sound_path = input(
            settings.alert.custom_sound_path.clone(),
            text.settings_custom_sound_placeholder,
            false,
        );
        let auto_hide_minutes = input(settings.alert.auto_hide_minutes.to_string(), "5", false);
        let opacity = input(settings.alert.opacity.to_string(), "235", false);
        let ninja_sample_target = input(settings.ninja.sample_target.to_string(), "2000", false);
        let ninja_refresh_hours = input(settings.ninja.refresh_hours.to_string(), "24", false);
        let ninja_request_gap = input(settings.ninja.min_request_gap_ms.to_string(), "1000", false);

        let language = choice_select(language_choices(), &settings.ui_language, window, cx);
        let corner = choice_select(corner_choices(text), &settings.alert.corner, window, cx);
        let user_agent_select = choice_select(
            user_agent_choices(text),
            user_agent_value(settings.user_agent_mode),
            window,
            cx,
        );

        // 语言选了就换:下一帧 `sync_language` 会把表头和别的下拉重造一遍。
        // 只改内存,落盘仍然靠保存按钮 —— 这一页的规矩是"填完按保存"。
        cx.subscribe(&language, |this: &mut AppShell, _, event, cx| {
            let gpui_component::select::SelectEvent::Confirm(Some(value)) = event else {
                return;
            };
            this.settings.ui_language = value.to_string();
            cx.notify();
        })
        .detach();
        cx.subscribe(&corner, |this: &mut AppShell, _, event, cx| {
            let gpui_component::select::SelectEvent::Confirm(Some(value)) = event else {
                return;
            };
            this.settings.alert.corner = value.to_string();
            cx.notify();
        })
        .detach();
        cx.subscribe(&user_agent_select, |this: &mut AppShell, _, event, cx| {
            let gpui_component::select::SelectEvent::Confirm(Some(value)) = event else {
                return;
            };
            this.settings.user_agent_mode = user_agent_mode(value);
            cx.notify();
        })
        .detach();

        Self {
            language,
            league,
            poesessid,
            poll_interval,
            poll_interval_when_live,
            max_live_connections,
            budget_percent,
            custom_sound_path,
            auto_hide_minutes,
            corner,
            opacity,
            ninja_sample_target,
            ninja_refresh_hours,
            ninja_request_gap,
            user_agent_mode: user_agent_select,
        }
    }

    /// 换语言之后:占位符和两个下拉的选项文字都要重来一遍。
    pub fn relabel(
        &mut self,
        settings: &pnd_settings::AppSettings,
        text: &'static Text,
        window: &mut Window,
        cx: &mut Context<AppShell>,
    ) {
        for (input, placeholder) in [
            (&self.league, text.settings_league_placeholder),
            (&self.poesessid, text.settings_poesessid_placeholder),
            (
                &self.custom_sound_path,
                text.settings_custom_sound_placeholder,
            ),
        ] {
            input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx);
            });
        }
        crate::shell::relabel_select(
            &self.corner,
            corner_choices(text),
            Some(&settings.alert.corner),
            window,
            cx,
        );
        crate::shell::relabel_select(
            &self.user_agent_mode,
            user_agent_choices(text),
            Some(user_agent_value(settings.user_agent_mode)),
            window,
            cx,
        );
    }

    /// 保存之后把 `normalize` 拉回范围的那些值写回框里。
    ///
    /// 不写回的话,屏幕上还留着刚才那个 5 秒的轮询间隔,而盘上存的是 60 —
    /// 用户下次打开程序才会发现自己填的没生效。
    pub fn write_back(
        &self,
        settings: &pnd_settings::AppSettings,
        window: &mut Window,
        cx: &mut Context<AppShell>,
    ) {
        for (input, value) in [
            (&self.league, settings.league.clone()),
            (&self.poesessid, settings.poesessid.clone()),
            (
                &self.poll_interval,
                settings.watcher.poll_interval_seconds.to_string(),
            ),
            (
                &self.poll_interval_when_live,
                settings.watcher.poll_interval_when_live_seconds.to_string(),
            ),
            (
                &self.max_live_connections,
                settings.watcher.max_live_connections.to_string(),
            ),
            (
                &self.budget_percent,
                settings.watcher.budget_percent.to_string(),
            ),
            (
                &self.custom_sound_path,
                settings.alert.custom_sound_path.clone(),
            ),
            (
                &self.auto_hide_minutes,
                settings.alert.auto_hide_minutes.to_string(),
            ),
            (&self.opacity, settings.alert.opacity.to_string()),
            (
                &self.ninja_sample_target,
                settings.ninja.sample_target.to_string(),
            ),
            (
                &self.ninja_refresh_hours,
                settings.ninja.refresh_hours.to_string(),
            ),
            (
                &self.ninja_request_gap,
                settings.ninja.min_request_gap_ms.to_string(),
            ),
        ] {
            input.update(cx, |state, cx| {
                state.set_value(value, window, cx);
            });
        }
    }
}

/// 框里的字。空白算没填,由调用方决定"没填"是什么意思。
fn text_of(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_string()
}

/// 框里的数。读不出来就保留原值 —— 手滑打了个字母不该把设置清零。
fn number_of<T: std::str::FromStr>(input: &Entity<InputState>, current: T, cx: &App) -> T {
    text_of(input, cx).parse::<T>().unwrap_or(current)
}

impl AppShell {
    pub(crate) fn render_settings(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.text();
        let read_only = self.read_only;
        let form = &self.settings_form;
        let sound = self.settings.alert.sound;

        let general = section(
            text.settings_section_general,
            vec![
                field_row()
                    .child(field_label(text.settings_language))
                    .child(picker("", &form.language, 160.)),
                field_row()
                    .child(field_label(text.settings_league))
                    .child(input_box(240., &form.league, read_only)),
            ],
        );

        // 测的是**存下来**的那个 cookie(后台手上就是它),所以框里刚打的字
        // 得先按保存才算数。没存过 cookie 或者上一次检查还没回来时,按钮是灰的。
        let no_session = self.settings.poesessid.trim().is_empty();
        let checking = self.session_check_busy;
        let session_line = self.session_check_line.clone();
        let session = section(
            text.settings_section_session,
            vec![
                field_row()
                    .child(field_label(text.settings_poesessid))
                    .child(input_box(320., &form.poesessid, read_only))
                    .child(
                        Button::new("settings-test-session")
                            .label(text.settings_test_session)
                            .with_size(Size::Small)
                            .disabled(no_session || checking)
                            .on_click(cx.listener(|this, _, _, cx| this.test_session(cx))),
                    )
                    .child(
                        div()
                            .text_size(fs(FS_11))
                            .text_color(muted())
                            .child(SharedString::from(session_line)),
                    ),
                field_row()
                    .child(field_label(""))
                    // 一次检查 = 一次搜索请求,而搜索预算 6 小时只有 299 次:
                    // 按钮的代价要写在按钮旁边。
                    .child(hint(text.settings_test_session_hint)),
                field_row()
                    .child(field_label(""))
                    .child(hint(text.settings_poesessid_hint)),
            ],
        );

        let watcher = section(
            text.settings_section_watcher,
            vec![
                unit_row(
                    text.settings_poll_interval,
                    &form.poll_interval,
                    text.common_seconds_short,
                    read_only,
                ),
                unit_row(
                    text.settings_poll_interval_when_live,
                    &form.poll_interval_when_live,
                    text.common_seconds_short,
                    read_only,
                ),
                unit_row(
                    text.settings_max_live_connections,
                    &form.max_live_connections,
                    "",
                    read_only,
                ),
                unit_row(
                    text.settings_budget_percent,
                    &form.budget_percent,
                    text.common_percent,
                    read_only,
                ),
            ],
        );

        let alert = section(
            text.settings_section_alert,
            vec![
                field_row().child(field_label(text.settings_sound)).child(
                    Switch::new("settings-sound")
                        .checked(sound)
                        .label(SharedString::from(if sound {
                            text.common_on
                        } else {
                            text.common_off
                        }))
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.settings.alert.sound = *checked;
                            cx.notify();
                        })),
                ),
                field_row()
                    .child(field_label(text.settings_custom_sound_path))
                    .child(input_box(320., &form.custom_sound_path, read_only)),
                unit_row(
                    text.settings_auto_hide_minutes,
                    &form.auto_hide_minutes,
                    text.common_minutes_short,
                    read_only,
                ),
                field_row()
                    .child(field_label(text.settings_corner))
                    .child(picker("", &form.corner, 160.)),
                unit_row(text.settings_opacity, &form.opacity, "", read_only),
            ],
        );

        let ninja = section(
            text.settings_section_ninja,
            vec![
                unit_row(
                    text.settings_ninja_sample_target,
                    &form.ninja_sample_target,
                    "",
                    read_only,
                ),
                unit_row(
                    text.settings_ninja_refresh_hours,
                    &form.ninja_refresh_hours,
                    text.common_hours_short,
                    read_only,
                ),
                unit_row(
                    text.settings_ninja_request_gap,
                    &form.ninja_request_gap,
                    text.common_milliseconds_short,
                    read_only,
                ),
                field_row()
                    .child(field_label(text.settings_user_agent_mode))
                    .child(picker("", &form.user_agent_mode, 160.)),
            ],
        );

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .child(page_heading(
                text.settings_heading,
                format!(
                    "{}: {}",
                    text.settings_file_path,
                    self.settings_store.path().display()
                ),
            ))
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(general)
                    .child(session)
                    .child(watcher)
                    .child(alert)
                    .child(ninja),
            )
            .child(
                div()
                    .flex_none()
                    .h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        Button::new("settings-save")
                            .primary()
                            .label(text.settings_save)
                            .with_size(Size::Small)
                            .disabled(read_only)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_settings(window, cx);
                            })),
                    )
                    .children(read_only.then(|| hint(text.settings_read_only))),
            )
    }

    /// 拿存下来的 cookie 去问交易站一次:你还认得它吗。
    ///
    /// 一次点击 = 一次搜索请求。按钮在检查回来之前是灰的,所以手快点两下
    /// 不会变成两次请求 —— 预算是 6 小时 299 次,不该让一个按钮白吃。
    fn test_session(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        if self.send_runtime(pnd_runtime::RuntimeCommand::TestSession) {
            self.session_check_busy = true;
            self.session_check_line = text.settings_session_checking.to_owned();
        }
        cx.notify();
    }

    /// 把表单里的值收回设置结构体、规整、写盘,并把结果说出来。
    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text();
        if self.read_only {
            self.set_sticky_notice(text.settings_read_only.to_owned());
            cx.notify();
            return;
        }
        self.collect_form(cx);
        // 先规整再存:存进去的和界面上写回去的于是是同一份值。
        self.settings.normalize();
        match self.settings_store.save(&self.settings) {
            Ok(()) => {
                self.push_log("settings saved".to_owned());
                self.set_notice(text.settings_saved.to_owned());
                // 存下来还不够:后台那份是启动时给它的旧设置。整份推过去,
                // actor 自己 diff 出哪条搜索变了、要不要换网关。
                self.apply_settings_to_runtime();
                // 卡片的角落、透明度、声音都是起线程时定死的,只能重起一条。
                // 每次保存都重起,免得维护"哪些字段能热改"那张表。
                self.restart_alert_card();
            }
            Err(error) => {
                self.push_log(format!("settings save failed: {error}"));
                self.set_notice(i18n::fill(text.settings_save_failed, &[&error.to_string()]));
            }
        }
        let settings = self.settings.clone();
        self.settings_form.write_back(&settings, window, cx);
        // 联赛、语言之类改完,蹲价表那几列跟着变。
        self.watches_dirty = true;
        // 换了联赛就等于换了一份 ninja 缓存:库里的行都是按联赛短名分的。
        self.resync_ninja_league();
        cx.notify();
    }

    /// 表单 → 设置结构体。只读的那几个键(语言、角落、UA)在下拉的回调里
    /// 已经改过了,这里不再碰。
    fn collect_form(&mut self, cx: &mut Context<Self>) {
        let form = &self.settings_form;
        let league = text_of(&form.league, cx);
        let poesessid = text_of(&form.poesessid, cx);
        let poll_interval = number_of(
            &form.poll_interval,
            self.settings.watcher.poll_interval_seconds,
            cx,
        );
        let poll_interval_when_live = number_of(
            &form.poll_interval_when_live,
            self.settings.watcher.poll_interval_when_live_seconds,
            cx,
        );
        let max_live_connections = number_of(
            &form.max_live_connections,
            self.settings.watcher.max_live_connections,
            cx,
        );
        let budget_percent = number_of(
            &form.budget_percent,
            self.settings.watcher.budget_percent,
            cx,
        );
        let custom_sound_path = text_of(&form.custom_sound_path, cx);
        let auto_hide_minutes = number_of(
            &form.auto_hide_minutes,
            self.settings.alert.auto_hide_minutes,
            cx,
        );
        let opacity = number_of(&form.opacity, self.settings.alert.opacity, cx);
        let sample_target = number_of(
            &form.ninja_sample_target,
            self.settings.ninja.sample_target,
            cx,
        );
        let refresh_hours = number_of(
            &form.ninja_refresh_hours,
            self.settings.ninja.refresh_hours,
            cx,
        );
        let request_gap = number_of(
            &form.ninja_request_gap,
            self.settings.ninja.min_request_gap_ms,
            cx,
        );

        // 联赛留空就保持原样:一个空联赛名会让每一个接口都 404。
        if !league.is_empty() {
            self.settings.league = league;
        }
        self.settings.poesessid = poesessid;
        self.settings.watcher.poll_interval_seconds = poll_interval;
        self.settings.watcher.poll_interval_when_live_seconds = poll_interval_when_live;
        self.settings.watcher.max_live_connections = max_live_connections;
        self.settings.watcher.budget_percent = budget_percent;
        self.settings.alert.custom_sound_path = custom_sound_path;
        self.settings.alert.auto_hide_minutes = auto_hide_minutes;
        self.settings.alert.opacity = opacity;
        self.settings.ninja.sample_target = sample_target;
        self.settings.ninja.refresh_hours = refresh_hours;
        self.settings.ninja.min_request_gap_ms = request_gap;
    }
}

/// 一段设置:标题 + 若干行。
fn section(title: &'static str, rows: Vec<gpui::Div>) -> gpui::Div {
    panel()
        .flex_none()
        .child(
            div()
                .px(px(10.))
                .py(px(6.))
                .bg(c(RAIL))
                .border_b_1()
                .border_color(c(HAIRLINE))
                .text_size(fs(FS_11_5))
                .text_color(c(TEXT_SECONDARY))
                .child(title),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(10.))
                .children(rows),
        )
}

/// 定宽的一个输入框。设置页上的框宽度各不相同,但别的都一样。
fn input_box(width: f32, input: &Entity<InputState>, read_only: bool) -> gpui::Div {
    div()
        .w(px(width))
        .flex_none()
        .child(Input::new(input).disabled(read_only).with_size(Size::Small))
}

/// "名字 + 窄输入框 + 单位"的一行。设置页上大半行都长这样。
fn unit_row(
    label: &'static str,
    input: &Entity<InputState>,
    unit: &'static str,
    read_only: bool,
) -> gpui::Div {
    field_row()
        .child(field_label(label))
        .child(input_box(90., input, read_only))
        .child(div().text_size(fs(FS_11)).text_color(muted()).child(unit))
}

#[cfg(test)]
mod settings_page_tests {
    use super::{user_agent_mode, user_agent_value};

    /// 下拉里的值和 `settings.json` 里的写法必须来回都对得上,否则重启之后
    /// 下拉显示的是"自报家门",而盘上存的是别的。
    #[test]
    fn the_user_agent_choice_round_trips() {
        for mode in [
            pnd_settings::UserAgentMode::Identified,
            pnd_settings::UserAgentMode::Browser,
        ] {
            assert_eq!(user_agent_mode(user_agent_value(mode)), mode);
        }
        // 认不出来的值退回默认档,而不是让下拉空着。
        assert_eq!(
            user_agent_mode("something-else"),
            pnd_settings::UserAgentMode::Identified
        );
    }
}
