//! 深色配色的唯一出处,以及把它装进 gpui-component 主题结构体的那一步。
//!
//! 只有深色一套。兄弟项目 POE-Trade-Tracker 里那份是双主题的,因为它要在
//! 白天看表格;这个工具是游戏旁边挂着的,浅色主题没有用户,所以不做,也就
//! 不需要"当前是哪一套"的全局状态 —— 常量直接就是值。
//!
//! 页面代码只写这里的名字(`ACCENT`、`hit_green()`),不写十六进制:改配色
//! 时改这一个文件,别处一行不动。

use gpui::{App, Hsla, Pixels, Rgba, px};
use gpui_component::theme::{Theme, ThemeColor, ThemeMode};

// ---------------------------------------------------------------------------
// 颜色槽位(深色「石板灰蓝 + 暖金」,值取自 POE-Trade-Tracker 的定稿深色盘)
// ---------------------------------------------------------------------------

/// 窗口底色。
pub const CANVAS: u32 = 0x12151B;
/// 面板底色,比窗口底亮一档。
pub const PANEL: u32 = 0x171B23;
/// 左导航、表头这类"框架"底色。
pub const RAIL: u32 = 0x1C212B;
/// 表格斑马行。
pub const ZEBRA: u32 = 0x1A1F28;
/// 输入框、进度槽这类"凹进去"的底。
pub const WELL: u32 = 0x0E1116;
/// 悬停底。
pub const HOVER: u32 = 0x1E242E;
/// 选中底。
pub const SELECTED: u32 = 0x232B3A;
/// 按下底。
pub const PRESSED: u32 = 0x283040;

/// 最轻的分隔线(表格行线)。
pub const HAIRLINE_SOFT: u32 = 0x222834;
/// 常规边框。
pub const HAIRLINE: u32 = 0x2B323F;
/// 需要看得见的边框(窗口边、拖动把手)。
pub const HAIRLINE_STRONG: u32 = 0x39424F;

/// 标题栏底、字、边。
pub const TITLEBAR: u32 = 0x1B1B1D;
pub const TITLEBAR_TEXT: u32 = 0xB8BCC4;
pub const TITLEBAR_BORDER: u32 = 0x000000;

/// 正文。
pub const TEXT_PRIMARY: u32 = 0xE6E9EF;
/// 次要说明。
pub const TEXT_SECONDARY: u32 = 0xA9B1BE;
/// 列标题、脚注这类"元信息"。
pub const TEXT_META: u32 = 0x78828F;
/// 禁用态的字。
pub const TEXT_DISABLED: u32 = 0x59616E;
/// 占位横杠。
pub const TEXT_GHOST: u32 = 0x3F4650;
/// 数字(等宽字体那一列)。
pub const TEXT_DATA: u32 = 0xD9E0EA;

/// 主题强调色:暖金。
pub const ACCENT: u32 = 0xD9B978;
/// 金色的字(比实底亮一点,压在深底上才读得清)。
pub const ACCENT_TEXT: u32 = 0xE7C88C;
pub const ACCENT_LINE: u32 = 0x6B5A34;
pub const ACCENT_WASH: u32 = 0x211D13;
pub const ACCENT_HOVER: u32 = 0xE3C98F;
pub const ACCENT_PRESSED: u32 = 0xC7A863;
/// 金底上的字。金是亮色,只能配深字。
pub const ON_ACCENT: u32 = 0x12151B;

/// 命中(价格进入上限之内)、Live 连接健康。
pub const FRESH: u32 = 0x45A96B;
/// 退避中、预算快见底、会话可疑。
pub const WARN: u32 = 0xE08A3C;
pub const WARN_TEXT: u32 = 0xE5A24E;
/// 出错、被 Cloudflare 拦、会话失效。
pub const DANGER: u32 = 0xD0564B;
pub const DANGER_TEXT: u32 = 0xE0705F;

// ---------------------------------------------------------------------------
// 页面共用的几个名字
// ---------------------------------------------------------------------------

/// 槽位 → gpui 颜色。页面里写 `c(ACCENT)`。
pub fn c(hex: u32) -> Rgba {
    gpui::rgb(hex)
}

fn h(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
}

/// 强调色(金):选中的导航项、主按钮、进度条。
pub fn accent() -> Rgba {
    c(ACCENT)
}

/// 命中绿:蹲价页的 `live`、提醒页"价格在上限内"。
pub fn hit_green() -> Rgba {
    c(FRESH)
}

/// 警告琥珀:退避、预算吃紧、数据过期。
pub fn warn_amber() -> Rgba {
    c(WARN)
}

/// 安静的灰:列标题、脚注、单位。
pub fn muted() -> Rgba {
    c(TEXT_META)
}

// ---------------------------------------------------------------------------
// 字体与尺寸
// ---------------------------------------------------------------------------

/// 界面字体。必须是能显示中文的那一款 —— 中文目录是一等公民,不是附赠。
pub const FONT_UI: &str = "Microsoft YaHei UI";
/// 数字用等宽,价格和倒计时才对得齐。
pub const FONT_MONO: &str = "Cascadia Mono";

pub const FS_10_5: f32 = 10.5;
pub const FS_11: f32 = 11.0;
pub const FS_11_5: f32 = 11.5;
pub const FS_12: f32 = 12.0;
pub const FS_13: f32 = 13.0;
pub const FS_16: f32 = 16.0;

/// 全局字号缩放。设计稿的字在 1440p 上偏小,整体放大一成。
const UI_SCALE: f32 = 1.1;

/// 字号 → 像素,过一遍缩放并对齐到半个像素。
pub fn fs(value: f32) -> Pixels {
    px((value * UI_SCALE * 2.0).round() / 2.0)
}

/// 控件行高。输入框、按钮、下拉都按这个高度排,一行里才不会参差。
pub const H_INPUT: f32 = 28.0;

// ---------------------------------------------------------------------------
// gpui-component 主题覆盖
// ---------------------------------------------------------------------------

/// 把上面这套颜色装到 gpui-component 的库存主题上,让每个上游组件
/// (Input、Select、Button、Switch、Table、滚动条……)都用我们的颜色画。
/// 开窗之前调一次。
pub fn apply_app_theme(cx: &mut App) {
    let theme = Theme::global_mut(cx);
    // 上游的幽灵按钮拿 `mode` 决定悬停是提亮还是压暗,深色写 Dark 才对。
    theme.mode = ThemeMode::Dark;
    theme.font_family = FONT_UI.into();
    theme.font_size = fs(FS_12);
    theme.mono_font_family = FONT_MONO.into();
    theme.mono_font_size = fs(FS_10_5);
    // 面板/输入 0 圆角:这套界面是"账本"风格,不是圆角卡片风格。
    theme.radius = px(0.);
    theme.radius_lg = px(0.);
    theme.shadow = false;
    theme.tile_shadow = false;
    theme.tile_radius = px(0.);

    apply_app_colors(&mut theme.colors);
}

/// 把调色板写进一个 `ThemeColor`。
///
/// 从 `apply_app_theme` 里拆出来,是因为 `Theme::global_mut` 要一个活的
/// `App`,而这些赋值本身不需要 —— 拆开之后普通单元测试也能查它。
fn apply_app_colors(colors: &mut ThemeColor) {
    // 面
    colors.background = h(CANVAS);
    colors.foreground = h(TEXT_PRIMARY);
    colors.popover = h(RAIL);
    colors.popover_foreground = h(TEXT_PRIMARY);
    colors.title_bar = h(TITLEBAR);
    colors.title_bar_border = h(TITLEBAR_BORDER);
    colors.window_border = h(HAIRLINE_STRONG);
    colors.tiles = h(CANVAS);
    // 遮罩的职责是把身后压下去,永远是黑的,不跟调色板走。
    colors.overlay = gpui::hsla(0., 0., 0., 0.45);

    // 次级 / 安静
    colors.muted = h(RAIL);
    colors.muted_foreground = h(TEXT_META);
    colors.secondary = h(PANEL);
    colors.secondary_foreground = h(TEXT_PRIMARY);
    colors.secondary_hover = h(HOVER);
    colors.secondary_active = h(PRESSED);

    // 边框 / 输入
    colors.border = h(HAIRLINE);
    colors.input = h(HAIRLINE);
    colors.ring = h(ACCENT);
    colors.caret = h(ACCENT);
    // 文字选区:金色 wash 在深底上几乎看不见,用选中行底。
    colors.selection = h(SELECTED);

    // 主色(金)。金底必须配深字。
    colors.primary = h(ACCENT);
    colors.primary_foreground = h(ON_ACCENT);
    colors.primary_hover = h(ACCENT_HOVER);
    colors.primary_active = h(ACCENT_PRESSED);
    colors.accent = h(SELECTED);
    colors.accent_foreground = h(TEXT_PRIMARY);

    // 危险(砖红)
    colors.danger = h(DANGER);
    colors.danger_foreground = h(ON_ACCENT);
    colors.danger_hover = h(DANGER_TEXT);
    colors.danger_active = h(DANGER_TEXT);

    // 警告(琥珀)
    colors.warning = h(WARN);
    colors.warning_foreground = h(ON_ACCENT);
    colors.warning_hover = h(WARN_TEXT);
    colors.warning_active = h(WARN_TEXT);

    // 成功 = 命中绿
    colors.success = h(FRESH);
    colors.success_foreground = h(ON_ACCENT);
    colors.success_hover = h(FRESH);
    colors.success_active = h(FRESH);

    // 提示信息收进金色体系
    colors.info = h(ACCENT_WASH);
    colors.info_foreground = h(ACCENT_TEXT);
    colors.info_hover = h(ACCENT_LINE);
    colors.info_active = h(ACCENT_LINE);

    // 列表
    colors.list = h(PANEL);
    colors.list_active = h(SELECTED);
    colors.list_active_border = h(ACCENT);
    colors.list_even = h(ZEBRA);
    colors.list_head = h(RAIL);
    colors.list_hover = h(HOVER);

    // 表格(斑马行 = ZEBRA)
    colors.table = h(PANEL);
    colors.table_active = h(SELECTED);
    colors.table_active_border = h(ACCENT);
    colors.table_even = h(ZEBRA);
    colors.table_head = h(RAIL);
    colors.table_head_foreground = h(TEXT_META);
    colors.table_hover = h(HOVER);
    colors.table_row_border = h(HAIRLINE_SOFT);

    // 页签
    colors.tab = gpui::transparent_black();
    colors.tab_active = h(PANEL);
    colors.tab_active_foreground = h(ACCENT_TEXT);
    colors.tab_bar = h(RAIL);
    colors.tab_bar_segmented = h(RAIL);
    colors.tab_foreground = h(TEXT_META);

    // 左导航
    colors.sidebar = h(RAIL);
    colors.sidebar_accent = h(PANEL);
    colors.sidebar_accent_foreground = h(ACCENT_TEXT);
    colors.sidebar_border = h(HAIRLINE);
    colors.sidebar_foreground = h(TEXT_SECONDARY);
    colors.sidebar_primary = h(ACCENT);
    colors.sidebar_primary_foreground = h(ON_ACCENT);

    // 滚动条:低存在感
    colors.scrollbar = gpui::transparent_black();
    colors.scrollbar_thumb = h(HAIRLINE);
    colors.scrollbar_thumb_hover = h(HAIRLINE_STRONG);

    // 杂项
    colors.link = h(ACCENT_TEXT);
    colors.link_hover = h(ACCENT);
    colors.link_active = h(ACCENT_PRESSED);
    colors.drag_border = h(ACCENT);
    colors.drop_target = h(ACCENT_WASH);
    colors.progress_bar = h(ACCENT);
    colors.skeleton = h(RAIL);
    colors.switch = h(HAIRLINE);
    colors.switch_thumb = h(TEXT_PRIMARY);
    colors.slider_bar = h(ACCENT);
    colors.slider_thumb = h(TEXT_PRIMARY);
    colors.accordion = h(PANEL);
    colors.accordion_hover = h(HOVER);
    colors.group_box = h(PANEL);
    colors.group_box_foreground = h(TEXT_PRIMARY);
    colors.description_list_label = h(RAIL);
    colors.description_list_label_foreground = h(TEXT_META);

    // 上游还按操作系统亮暗播种了一批 `chart_*` 和 red/green/blue……
    // 这个程序一个都没用上,但这三个有语义对应的钉死:哪天真用上 Badge,
    // 一个跟着 Windows 主题走的"红"会和这里的砖红对不上。
    colors.red = h(DANGER);
    colors.green = h(FRESH);
    colors.yellow = h(WARN);
}

#[cfg(test)]
mod theme_tests {
    use super::{
        ACCENT, CANVAS, DANGER, FRESH, PANEL, TEXT_META, TEXT_PRIMARY, WARN, accent,
        apply_app_colors, c, hit_green, muted, warn_amber,
    };

    /// 正文对面板必须读得清。深色盘只有一套,没人替它把关就没人把关了。
    ///
    /// 用 sRGB 相对亮度算对比度,和 WCAG 一个口径;4.5:1 是正文的线。
    #[test]
    fn body_text_reads_against_the_panel() {
        assert!(
            contrast(TEXT_PRIMARY, PANEL) >= 4.5,
            "正文 {:.2}:1",
            contrast(TEXT_PRIMARY, PANEL)
        );
        assert!(
            contrast(TEXT_META, PANEL) >= 3.0,
            "元信息 {:.2}:1",
            contrast(TEXT_META, PANEL)
        );
    }

    /// 三个语义色必须互不相同,否则"命中"和"退避"在屏幕上是一个东西。
    #[test]
    fn the_semantic_colours_are_distinguishable() {
        let semantics = [ACCENT, FRESH, WARN, DANGER];
        for (index, one) in semantics.iter().enumerate() {
            for other in &semantics[index + 1..] {
                assert_ne!(one, other);
            }
        }
        assert_eq!(accent(), c(ACCENT));
        assert_eq!(hit_green(), c(FRESH));
        assert_eq!(warn_amber(), c(WARN));
        assert_eq!(muted(), c(TEXT_META));
    }

    /// 写进上游主题的值就是这里的值 —— 中间没有第二套颜色。
    #[test]
    fn the_upstream_theme_gets_our_colours() {
        let mut colors = gpui_component::theme::ThemeColor::default();
        apply_app_colors(&mut colors);
        assert_eq!(colors.background, super::h(CANVAS));
        assert_eq!(colors.primary, super::h(ACCENT));
        assert_eq!(colors.success, super::h(FRESH));
    }

    fn channel(value: f32) -> f32 {
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(hex: u32) -> f32 {
        let red = channel(((hex >> 16) & 0xFF) as f32 / 255.0);
        let green = channel(((hex >> 8) & 0xFF) as f32 / 255.0);
        let blue = channel((hex & 0xFF) as f32 / 255.0);
        0.2126 * red + 0.7152 * green + 0.0722 * blue
    }

    fn contrast(one: u32, other: u32) -> f32 {
        let (a, b) = (luminance(one), luminance(other));
        let (light, dark) = if a > b { (a, b) } else { (b, a) };
        (light + 0.05) / (dark + 0.05)
    }
}
