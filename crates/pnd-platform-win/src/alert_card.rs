//! 屏幕角落的置顶提醒小卡片:平台无关的对外接口。
//!
//! 这张卡片和 POE-Alarm 的红色全屏警报是两种东西:那个要把你从游戏里拽出来,
//! 所以锁住鼠标;这个只是"有好价了,有空再看",所以**永远不抢焦点、永远不挡
//! 卡片以外的点击**。真正的 Win32 实现在 `win32::alert_card`,这里只放
//!
//! - 调用方需要的类型(卡片文字、按钮、配置、事件);
//! - 一个把命令丢给卡片线程、把事件收回来的 [`AlertCardService`];
//! - 三个纯函数几何helper(角落定位、按钮排布、命中判定),它们带单元测试,
//!   免得"按钮画在这里、点在那里"这种错要靠肉眼发现。

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::PlatformError;
use crate::hotkey::Hotkey;
use crate::wave::ValidatedWave;

/// 卡片线程启动握手的上限。超过就当线程没起来,和 POE-Alarm 同一个数。
const STARTUP_TIMEOUT: Duration = Duration::from_millis(750);

/// 卡片默认不透明度(0-255)。235 ≈ 92%:看得清,又不像贴在屏幕上。
pub const DEFAULT_CARD_OPACITY: u8 = 235;

/// 卡片默认自动收起时间,和计划里的 `alert.auto_hide_minutes` 一致。
pub const DEFAULT_AUTO_HIDE: Duration = Duration::from_secs(5 * 60);

/// 卡片在 96 dpi 下的逻辑尺寸与外边距。
const CARD_LOGICAL_WIDTH: i32 = 420;
const CARD_LOGICAL_HEIGHT: i32 = 180;
const CARD_LOGICAL_MARGIN: i32 = 16;

/// 按钮行(卡片底部)在 96 dpi 下的逻辑尺寸。
const BUTTON_ROW_PADDING: i32 = 12;
const BUTTON_ROW_GAP: i32 = 8;
const BUTTON_HEIGHT: i32 = 32;

/// 一个整数矩形。这里只需要 x/y/w/h 四个数,不值得为它引一整套几何库。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RectI {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl RectI {
    #[must_use]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    #[must_use]
    pub const fn right(self) -> i32 {
        self.x + self.w
    }

    #[must_use]
    pub const fn bottom(self) -> i32 {
        self.y + self.h
    }

    #[must_use]
    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// 卡片贴哪个角。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

impl Corner {
    /// 从 `settings.json` 里的字符串解析(`"bottom_right"` 这种)。
    ///
    /// 这个 crate 不认识 serde:平台层不该为了一个四选一的枚举去依赖序列化框架,
    /// 设置文件由上层(`pnd-settings`)负责读写。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "top_left" | "topleft" => Some(Self::TopLeft),
            "top_right" | "topright" => Some(Self::TopRight),
            "bottom_left" | "bottomleft" => Some(Self::BottomLeft),
            "bottom_right" | "bottomright" => Some(Self::BottomRight),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TopLeft => "top_left",
            Self::TopRight => "top_right",
            Self::BottomLeft => "bottom_left",
            Self::BottomRight => "bottom_right",
        }
    }
}

/// 卡片上的四个按钮。含义由上层决定(打开交易页 / 复制私聊 / 去藏身处 / 忽略),
/// 平台层只认下标 0..3。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CardButton {
    Primary,
    Secondary,
    Tertiary,
    Dismiss,
}

impl CardButton {
    /// 四个按钮的固定顺序,画和命中都用它,免得两边各排一次。
    pub const ALL: [Self; 4] = [
        Self::Primary,
        Self::Secondary,
        Self::Tertiary,
        Self::Dismiss,
    ];

    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Primary => 0,
            Self::Secondary => 1,
            Self::Tertiary => 2,
            Self::Dismiss => 3,
        }
    }

    #[must_use]
    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Primary),
            1 => Some(Self::Secondary),
            2 => Some(Self::Tertiary),
            3 => Some(Self::Dismiss),
            _ => None,
        }
    }
}

/// 文字太长的拒绝原因。卡片宽度固定,过长的字符串只会被裁掉,
/// 不如在进平台层之前就说清楚哪一行超了多少。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CardTextError {
    pub field: &'static str,
    pub limit: usize,
    pub actual: usize,
}

impl fmt::Display for CardTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "alert card {} is {} characters, the limit is {}",
            self.field, self.actual, self.limit
        )
    }
}

impl std::error::Error for CardTextError {}

const TITLE_LIMIT: usize = 80;
const LINE_LIMIT: usize = 160;
const FOOTER_LIMIT: usize = 160;
const BUTTON_LIMIT: usize = 24;

/// 卡片上显示的全部文字。平台层不拼接任何文案:双语目录在 `pnd-app`。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CardText {
    pub title: String,
    pub line1: String,
    pub line2: String,
    pub footer: String,
    pub buttons: [String; 4],
}

impl CardText {
    /// 校验长度后构造。字数按 Unicode 字符算,中文一个字就是一个字。
    pub fn new(
        title: impl Into<String>,
        line1: impl Into<String>,
        line2: impl Into<String>,
        footer: impl Into<String>,
        buttons: [impl Into<String>; 4],
    ) -> Result<Self, CardTextError> {
        let title = title.into();
        let line1 = line1.into();
        let line2 = line2.into();
        let footer = footer.into();
        let buttons = buttons.map(Into::into);
        check_length("title", &title, TITLE_LIMIT)?;
        check_length("line1", &line1, LINE_LIMIT)?;
        check_length("line2", &line2, LINE_LIMIT)?;
        check_length("footer", &footer, FOOTER_LIMIT)?;
        for (index, button) in buttons.iter().enumerate() {
            let field = match index {
                0 => "buttons[0]",
                1 => "buttons[1]",
                2 => "buttons[2]",
                _ => "buttons[3]",
            };
            check_length(field, button, BUTTON_LIMIT)?;
        }
        Ok(Self {
            title,
            line1,
            line2,
            footer,
            buttons,
        })
    }
}

fn check_length(field: &'static str, value: &str, limit: usize) -> Result<(), CardTextError> {
    let actual = value.chars().count();
    if actual > limit {
        return Err(CardTextError {
            field,
            limit,
            actual,
        });
    }
    Ok(())
}

/// 卡片外观里可以随时改的部分(设置页调一下就该生效,不必重启线程)。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CardStyle {
    pub corner: Corner,
    /// 0-255 的整体不透明度(`SetLayeredWindowAttributes` 的 alpha)。
    pub opacity: u8,
    pub auto_hide: Duration,
}

impl Default for CardStyle {
    fn default() -> Self {
        Self {
            corner: Corner::default(),
            opacity: DEFAULT_CARD_OPACITY,
            auto_hide: DEFAULT_AUTO_HIDE,
        }
    }
}

/// 启动卡片线程需要的全部配置。
#[derive(Clone, Debug)]
pub struct CardConfig {
    pub corner: Corner,
    pub opacity: u8,
    pub auto_hide: Duration,
    /// `None` = 静音。声音归卡片线程所有:显示即响,任何按钮/收起即停。
    pub sound: Option<ValidatedWave>,
    /// `None` = 不注册热键。注册也归卡片线程所有:它本来就有一条消息泵,
    /// 而 `RegisterHotKey` 的登记和注销必须在同一条线程上。
    ///
    /// 这条热键**只会收起我们自己那张卡片**:它不往游戏里发任何按键,
    /// 收到 `WM_HOTKEY` 之后做的事和点一下"忽略"一模一样。
    pub dismiss_hotkey: Option<Hotkey>,
}

impl Default for CardConfig {
    /// 手写而不是 `derive`:`derive` 会把不透明度写成 0(全透明)、自动收起写成
    /// 0 秒,那是两个能让卡片"看不见"的默认值。
    fn default() -> Self {
        Self::new()
    }
}

impl CardConfig {
    /// 默认配置(右下角、alpha 235、5 分钟自动收起、静音、无热键)。
    #[must_use]
    pub fn new() -> Self {
        Self {
            corner: Corner::default(),
            opacity: DEFAULT_CARD_OPACITY,
            auto_hide: DEFAULT_AUTO_HIDE,
            sound: None,
            dismiss_hotkey: None,
        }
    }

    #[must_use]
    pub fn with_sound(mut self, wave: ValidatedWave) -> Self {
        self.sound = Some(wave);
        self
    }

    #[must_use]
    pub fn with_dismiss_hotkey(mut self, hotkey: Hotkey) -> Self {
        self.dismiss_hotkey = Some(hotkey);
        self
    }

    #[must_use]
    pub const fn style(&self) -> CardStyle {
        CardStyle {
            corner: self.corner,
            opacity: self.opacity,
            auto_hide: self.auto_hide,
        }
    }
}

/// 发给卡片线程的命令。外部调用只通过 [`AlertCardService`] 的方法产生它们。
#[derive(Clone, Debug)]
pub(crate) enum CardCommand {
    Show { alert_id: i64, text: CardText },
    Update { alert_id: i64, text: CardText },
    Hide,
    SetStyle(CardStyle),
    Shutdown,
}

/// 卡片线程报回来的事情。调用方每帧 `try_next_event` 抽干即可。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CardEvent {
    /// 用户按下并在同一个按钮上松手。
    Clicked { alert_id: i64, button: CardButton },
    /// 到点自动收起(没有点任何按钮)。
    AutoHidden { alert_id: i64 },
    /// 用户按了那条全局热键,而且当时屏幕上确实有一张卡片。
    ///
    /// 语义和点"忽略"完全一样,分成两个事件只是为了日志上看得出来是键盘
    /// 还是鼠标。没有卡片时**不会**有这个事件:热键在没提醒的时候什么都不做。
    HotkeyDismiss { alert_id: i64 },
    /// 用户拖动标题条后的新位置(屏幕坐标)。
    Moved { x: i32, y: i32 },
    /// 非致命的兼容性问题(比如置顶样式被别的程序改掉了)。卡片仍在工作。
    Warning(String),
}

/// 卡片服务的失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CardError {
    /// 本进程已经有一个卡片服务了。
    AlreadyInUse,
    /// 线程没起来 / 没在 750 ms 内就绪。
    Thread(String),
    /// 卡片线程已经退出,命令投递不出去。
    Disconnected,
    /// 底层 Win32 失败(或非 Windows 平台上的"不支持")。
    Platform(PlatformError),
}

impl fmt::Display for CardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInUse => {
                formatter.write_str("an alert card service already owns this process")
            }
            Self::Thread(detail) => write!(formatter, "alert card thread failed: {detail}"),
            Self::Disconnected => formatter.write_str("the alert card thread is gone"),
            Self::Platform(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CardError {}

impl From<PlatformError> for CardError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

static SERVICE_OWNED: AtomicBool = AtomicBool::new(false);

/// 进程内唯一性令牌:线程退出时归还,所以服务崩了也不会把名额锁死。
pub(crate) struct CardOwnership;

impl CardOwnership {
    fn acquire() -> Result<Self, CardError> {
        SERVICE_OWNED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| CardError::AlreadyInUse)
    }
}

impl Drop for CardOwnership {
    fn drop(&mut self) {
        SERVICE_OWNED.store(false, Ordering::Release);
    }
}

/// 调用线程和卡片线程之间的全部共享状态。
///
/// 命令走 `Mutex<VecDeque>`,再用 `PostThreadMessageW` 敲一下卡片线程把它从
/// `GetMessageW` 里叫醒——这样调用方永远不阻塞,卡片线程也不用空转轮询。
#[derive(Debug, Default)]
pub(crate) struct CardShared {
    commands: Mutex<VecDeque<CardCommand>>,
    native_thread_id: AtomicU64,
}

impl CardShared {
    pub(crate) fn push(&self, command: CardCommand) {
        self.commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_back(command);
    }

    pub(crate) fn pop(&self) -> Option<CardCommand> {
        self.commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .pop_front()
    }

    pub(crate) fn native_thread_id(&self) -> u32 {
        self.native_thread_id.load(Ordering::Acquire) as u32
    }

    pub(crate) fn set_native_thread_id(&self, id: u32) {
        self.native_thread_id
            .store(u64::from(id), Ordering::Release);
    }
}

/// 拥有一条卡片线程。所有方法都不等待窗口,调用方(UI 线程)不会被卡住。
pub struct AlertCardService {
    shared: Arc<CardShared>,
    events: mpsc::Receiver<CardEvent>,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for AlertCardService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AlertCardService")
            .field("native_thread_id", &self.shared.native_thread_id())
            .finish_non_exhaustive()
    }
}

impl AlertCardService {
    /// 起一条名为 `pnd-alert-card` 的线程,等它把窗口建好再返回。
    ///
    /// 只有这一步会等(最多 750 ms):窗口建不出来必须当场知道,不能等到第一次
    /// 提醒时才发现没有卡片。
    pub fn start(config: CardConfig) -> Result<Self, CardError> {
        let ownership = CardOwnership::acquire()?;
        let shared = Arc::new(CardShared::default());
        let (event_sender, event_receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread = platform_spawn_worker(
            config,
            Arc::clone(&shared),
            event_sender,
            ready_sender,
            ownership,
        )?;
        match ready_receiver.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                shared,
                events: event_receiver,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                // 线程还活着但没就绪:先请它退出,再把超时报上去。
                shared.push(CardCommand::Shutdown);
                let _ = platform_wake(shared.native_thread_id());
                Err(CardError::Thread(format!(
                    "the alert card window was not ready within {} ms: {error}",
                    STARTUP_TIMEOUT.as_millis()
                )))
            }
        }
    }

    /// 显示(或换成)一张卡片:重新贴角、置顶、开始播声、重置自动收起计时。
    pub fn show(&self, alert_id: i64, text: CardText) -> Result<(), CardError> {
        self.dispatch(CardCommand::Show { alert_id, text })
    }

    /// 卡片还在屏幕上时原地改字("+2 件"这种),声音继续循环。
    /// 卡片已经收起时等同于 [`show`](Self::show)。
    pub fn update(&self, alert_id: i64, text: CardText) -> Result<(), CardError> {
        self.dispatch(CardCommand::Update { alert_id, text })
    }

    /// 收起卡片并停声。
    pub fn hide(&self) -> Result<(), CardError> {
        self.dispatch(CardCommand::Hide)
    }

    /// 改角落 / 透明度 / 自动收起时长(设置页改完立刻生效)。
    pub fn set_style(&self, style: CardStyle) -> Result<(), CardError> {
        self.dispatch(CardCommand::SetStyle(style))
    }

    /// 取一条已经发生的事件,没有就返回 `None`,永不阻塞。
    #[must_use]
    pub fn try_next_event(&self) -> Option<CardEvent> {
        self.events.try_recv().ok()
    }

    fn dispatch(&self, command: CardCommand) -> Result<(), CardError> {
        let thread_id = self.shared.native_thread_id();
        if thread_id == 0 {
            return Err(CardError::Disconnected);
        }
        self.shared.push(command);
        platform_wake(thread_id)
    }
}

impl Drop for AlertCardService {
    fn drop(&mut self) {
        self.shared.push(CardCommand::Shutdown);
        let _ = platform_wake(self.shared.native_thread_id());
        if let Some(thread) = self.thread.take() {
            // 窗口和声音都归那条线程,等它自己收干净比在这里遥控销毁安全。
            let _ = thread.join();
        }
    }
}

fn platform_spawn_worker(
    config: CardConfig,
    shared: Arc<CardShared>,
    events: mpsc::Sender<CardEvent>,
    ready: mpsc::SyncSender<Result<(), CardError>>,
    ownership: CardOwnership,
) -> Result<JoinHandle<()>, CardError> {
    #[cfg(windows)]
    {
        crate::win32::spawn_card_worker(config, shared, events, ready, ownership)
    }
    #[cfg(not(windows))]
    {
        crate::non_windows::spawn_card_worker(config, shared, events, ready, ownership)
    }
}

fn platform_wake(thread_id: u32) -> Result<(), CardError> {
    #[cfg(windows)]
    {
        crate::win32::wake_card(thread_id)
    }
    #[cfg(not(windows))]
    {
        crate::non_windows::wake_card(thread_id)
    }
}

/// 按 dpi 缩放一个 96 dpi 下的逻辑长度。绘制代码也要用同一把尺子,
/// 否则按钮画的位置和算出来的矩形会差几个像素。
pub(crate) const fn scale_for_dpi(logical: i32, dpi: u32) -> i32 {
    (logical * dpi as i32) / 96
}

/// 算出卡片贴在工作区某个角时的屏幕矩形。
///
/// 纯函数:窗口还没建出来就得知道摆哪儿,而且这是最容易算错的一段
/// (右下角要减掉自己的宽高,高 dpi 下每个数都要缩放),所以单独拿出来带测试。
#[must_use]
pub fn card_geometry_for_corner(work_area: RectI, dpi: u32, corner: Corner) -> RectI {
    let width = scale_for_dpi(CARD_LOGICAL_WIDTH, dpi).min(work_area.w.max(1));
    let height = scale_for_dpi(CARD_LOGICAL_HEIGHT, dpi).min(work_area.h.max(1));
    let margin = scale_for_dpi(CARD_LOGICAL_MARGIN, dpi);
    let (x, y) = match corner {
        Corner::TopLeft => (work_area.x + margin, work_area.y + margin),
        Corner::TopRight => (work_area.right() - margin - width, work_area.y + margin),
        Corner::BottomLeft => (work_area.x + margin, work_area.bottom() - margin - height),
        Corner::BottomRight => (
            work_area.right() - margin - width,
            work_area.bottom() - margin - height,
        ),
    };
    // 工作区比"卡片 + 两倍边距"还小时(极小的屏幕或超大缩放),宁可贴边也不要跑出去。
    let x = x.clamp(work_area.x, (work_area.right() - width).max(work_area.x));
    let y = y.clamp(work_area.y, (work_area.bottom() - height).max(work_area.y));
    RectI::new(x, y, width, height)
}

/// 卡片底部一排四个等宽按钮的矩形,坐标相对传入的 `card` 原点。
///
/// 画和命中共用这一个函数——命中一个画在别处的按钮,比没有按钮更糟。
#[must_use]
pub fn button_rects(card: RectI, dpi: u32) -> [RectI; 4] {
    let padding = scale_for_dpi(BUTTON_ROW_PADDING, dpi);
    let gap = scale_for_dpi(BUTTON_ROW_GAP, dpi);
    let height = scale_for_dpi(BUTTON_HEIGHT, dpi);
    let row_width = (card.w - padding * 2).max(4);
    let width = ((row_width - gap * 3) / 4).max(1);
    let top = card.y + (card.h - padding - height).max(0);
    let left = card.x + padding;
    std::array::from_fn(|index| {
        RectI::new(
            left + (width + gap) * index as i32,
            top,
            width,
            height.max(1),
        )
    })
}

/// 点 (x, y) 落在哪个按钮上。
#[must_use]
pub fn hit_button(buttons: &[RectI; 4], x: i32, y: i32) -> Option<CardButton> {
    buttons
        .iter()
        .position(|rect| rect.contains(x, y))
        .and_then(CardButton::from_index)
}

#[cfg(test)]
mod alert_card_tests {
    use super::*;

    const WORK: RectI = RectI::new(0, 0, 1920, 1040);

    #[test]
    fn corner_strings_round_trip() {
        for corner in [
            Corner::TopLeft,
            Corner::TopRight,
            Corner::BottomLeft,
            Corner::BottomRight,
        ] {
            assert_eq!(Corner::parse(corner.as_str()), Some(corner));
        }
        assert_eq!(Corner::parse(" Bottom_Right "), Some(Corner::BottomRight));
        assert_eq!(Corner::parse("middle"), None);
    }

    #[test]
    fn card_sits_inside_each_corner_of_the_work_area() {
        let top_left = card_geometry_for_corner(WORK, 96, Corner::TopLeft);
        assert_eq!(top_left, RectI::new(16, 16, 420, 180));
        let bottom_right = card_geometry_for_corner(WORK, 96, Corner::BottomRight);
        assert_eq!(
            bottom_right,
            RectI::new(1920 - 16 - 420, 1040 - 16 - 180, 420, 180)
        );
        let top_right = card_geometry_for_corner(WORK, 96, Corner::TopRight);
        assert_eq!(top_right, RectI::new(1920 - 16 - 420, 16, 420, 180));
        let bottom_left = card_geometry_for_corner(WORK, 96, Corner::BottomLeft);
        assert_eq!(bottom_left, RectI::new(16, 1040 - 16 - 180, 420, 180));
    }

    #[test]
    fn geometry_scales_with_dpi_and_respects_a_shifted_work_area() {
        // 150% 缩放 = 144 dpi:420×180 变 630×270,边距 16 变 24。
        let work = RectI::new(-1920, 100, 1920, 1000);
        let card = card_geometry_for_corner(work, 144, Corner::BottomRight);
        assert_eq!(card.w, 630);
        assert_eq!(card.h, 270);
        assert_eq!(card.right(), work.right() - 24);
        assert_eq!(card.bottom(), work.bottom() - 24);
    }

    #[test]
    fn a_work_area_smaller_than_the_card_still_yields_an_on_screen_card() {
        let tiny = RectI::new(0, 0, 300, 120);
        let card = card_geometry_for_corner(tiny, 96, Corner::BottomRight);
        assert_eq!(card, RectI::new(0, 0, 300, 120));
    }

    #[test]
    fn buttons_form_one_row_of_four_equal_boxes_inside_the_card() {
        let card = RectI::new(0, 0, 420, 180);
        let buttons = button_rects(card, 96);
        assert!(buttons.iter().all(|rect| rect.w == buttons[0].w));
        assert!(buttons.iter().all(|rect| rect.y == buttons[0].y));
        assert_eq!(buttons[0].x, 12);
        assert_eq!(buttons[3].right(), 420 - 12);
        assert_eq!(buttons[0].h, 32);
        assert_eq!(buttons[3].bottom(), 180 - 12);
        for pair in buttons.windows(2) {
            assert_eq!(pair[1].x - pair[0].right(), 8);
        }
    }

    #[test]
    fn hit_testing_matches_the_drawn_rectangles() {
        let buttons = button_rects(RectI::new(0, 0, 420, 180), 96);
        for button in CardButton::ALL {
            let rect = buttons[button.index()];
            let hit = hit_button(&buttons, rect.x + rect.w / 2, rect.y + rect.h / 2);
            assert_eq!(hit, Some(button));
            // 右/下边界是开区间:两个相邻按钮不能同时命中。
            assert_ne!(hit_button(&buttons, rect.right(), rect.y), Some(button));
        }
        assert_eq!(hit_button(&buttons, 200, 10), None);
        assert_eq!(hit_button(&buttons, -5, 150), None);
    }

    #[test]
    fn card_text_rejects_only_the_field_that_is_too_long() {
        let ok = CardText::new(
            "Choir of the Storm · 15 divine",
            "Tongzii#6639 · online",
            "cap 20 divine",
            "search 6h 41/299",
            ["Open trade", "Copy whisper", "Hideout", "Dismiss"],
        );
        assert!(ok.is_ok());

        let long_title = "字".repeat(81);
        let error = CardText::new(long_title, "", "", "", ["", "", "", ""]).unwrap_err();
        assert_eq!(error.field, "title");
        assert_eq!(error.limit, 80);
        assert_eq!(error.actual, 81);

        let error =
            CardText::new("t", "", "", "", ["ok", "ok", "ok", &"x".repeat(25)]).unwrap_err();
        assert_eq!(error.field, "buttons[3]");
        assert_eq!(error.limit, 24);
    }

    #[test]
    fn button_indexes_round_trip() {
        for button in CardButton::ALL {
            assert_eq!(CardButton::from_index(button.index()), Some(button));
        }
        assert_eq!(CardButton::from_index(4), None);
    }
}
