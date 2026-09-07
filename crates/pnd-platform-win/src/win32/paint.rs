//! 两个原生窗口(提醒卡片、登录窗)共用的一小套 GDI 画具。
//!
//! 单独一个文件的理由是所有权:`HBRUSH` / `HFONT` 用完不还就是句柄泄漏,而
//! "记得删"这件事写两遍就会漏一遍。这里把它们各包一层 `Drop`,调用方只管造。
//! 颜色也放这儿:两个窗口摆在同一块屏幕上,配色分家看着就像两个程序。

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
    DRAW_TEXT_FORMAT, DeleteObject, DrawTextW, HBRUSH, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS,
    SelectObject, SetTextColor,
};
use windows::core::w;

pub(crate) const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF((red as u32) | ((green as u32) << 8) | ((blue as u32) << 16))
}

/// 深色配色,和 POE-Trade-Tracker 的 HUD 用同一套 token,免得两个工具
/// 摆在一起像两家做的。
pub(crate) const PANEL: COLORREF = rgb(0x17, 0x1B, 0x23);
pub(crate) const BORDER: COLORREF = rgb(0x39, 0x42, 0x4F);
pub(crate) const RAIL: COLORREF = rgb(0x1C, 0x21, 0x2B);
pub(crate) const HAIRLINE: COLORREF = rgb(0x22, 0x28, 0x34);
pub(crate) const GOLD: COLORREF = rgb(0xD9, 0xB9, 0x78);
pub(crate) const TEXT_PRIMARY: COLORREF = rgb(0xE6, 0xE9, 0xEF);
pub(crate) const TEXT_SECONDARY: COLORREF = rgb(0xA9, 0xB1, 0xBE);
pub(crate) const TEXT_META: COLORREF = rgb(0x78, 0x82, 0x8F);
pub(crate) const BUTTON_FILL: COLORREF = rgb(0x22, 0x28, 0x34);
pub(crate) const BUTTON_HOVER: COLORREF = rgb(0x2E, 0x36, 0x44);
pub(crate) const BUTTON_PRESSED: COLORREF = rgb(0x3A, 0x44, 0x54);
pub(crate) const BUTTON_TEXT: COLORREF = rgb(0xD9, 0xE0, 0xEA);

pub(crate) struct Brush(pub(crate) HBRUSH);

impl Brush {
    pub(crate) fn new(color: COLORREF) -> Self {
        // SAFETY: CreateSolidBrush 无前置条件;Drop 里 DeleteObject。
        Self(unsafe { CreateSolidBrush(color) })
    }
}

impl Drop for Brush {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: 本类型独占这个画刷,至多删一次。
            let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
        }
    }
}

pub(crate) struct Font(pub(crate) HFONT);

impl Font {
    pub(crate) fn new(pixels: i32, weight: i32) -> Self {
        // 用雅黑:窗口上会出现中文物品名和界面文案,Segoe UI 画中文要靠字体回退。
        // SAFETY: 固定字体名的 CreateFontW;Drop 里 DeleteObject。
        Self(unsafe {
            CreateFontW(
                -pixels.max(1),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Microsoft YaHei UI"),
            )
        })
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: 本类型独占这个字体,至多删一次。
            let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
        }
    }
}

pub(crate) fn draw_text(
    dc: HDC,
    font: HFONT,
    color: COLORREF,
    text: &str,
    mut bounds: RECT,
    format: DRAW_TEXT_FORMAT,
) {
    // 空串的 Vec<u16> 是悬垂指针,DrawTextW 会访问违例;直接跳过。
    if text.is_empty() {
        return;
    }
    let mut utf16 = text.encode_utf16().collect::<Vec<_>>();
    // SAFETY: dc/font 在本次绘制内有效;DrawTextW 只在 bounds 内绘制。
    unsafe {
        let previous = SelectObject(dc, HGDIOBJ(font.0));
        SetTextColor(dc, color);
        DrawTextW(dc, &mut utf16, &mut bounds, format);
        SelectObject(dc, previous);
    }
}
