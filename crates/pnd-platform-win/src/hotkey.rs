//! 设置里那一行 `ctrl+alt+d` → `RegisterHotKey` 认得的两个数字。
//!
//! 为什么单独一个模块、而且是纯函数:热键是**全局**的,按下去的时候前台窗口
//! 多半是游戏。写错一个字母(比如把 `ctrl+alt+d` 打成 `ctr+alt+d`)时,唯一
//! 安全的行为是"这条热键不注册",而不是注册成别的键——那等于在游戏里埋了
//! 一个会吞按键的陷阱。所以解析这一步不碰系统,能在任何机器上跑测试。
//!
//! 这里只把字符串变成数字。真正的 `RegisterHotKey` 在 `win32::alert_card` 里,
//! 和卡片共用同一条消息泵线程。

use std::fmt;

/// `RegisterHotKey` 的 `MOD_*` 位。这个 crate 不引 `windows` 的类型进来:
/// 解析要在非 Windows 上也编得过、也测得了。
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;

/// `VK_F1`。F2..F24 就是它往后一个一个数。
const VK_F1: u32 = 0x70;
const MAX_FUNCTION_KEY: u32 = 24;

/// 一条解析好的全局热键。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hotkey {
    /// `MOD_ALT | MOD_CONTROL | …`,不含 `MOD_NOREPEAT`(注册时才补上)。
    pub modifiers: u32,
    /// 虚拟键码。字母和数字就是它们的大写 ASCII 码,F 键是 `VK_F1 + n - 1`。
    pub vk: u32,
}

impl fmt::Display for Hotkey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "modifiers 0x{:04X} + vk 0x{:02X}",
            self.modifiers, self.vk
        )
    }
}

/// `"ctrl+alt+d"` → [`Hotkey`]。认不出来的一律 `None`。
///
/// 规矩只有三条,都是为了不在游戏里埋雷:
///
/// - **必须至少带一个修饰键。** 光一个 `d` 注册成全局热键,游戏里再也打不出
///   这个字母;
/// - **只能有一个主键**(字母 / 数字 / F1–F24);
/// - 空串、多余的加号、不认识的词,全都返回 `None` —— 调用方把 `None`
///   当成"这条热键不开"。
#[must_use]
pub fn parse_hotkey(text: &str) -> Option<Hotkey> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut modifiers = 0_u32;
    let mut vk = None;
    for token in text.split('+') {
        let token = token.trim().to_ascii_lowercase();
        if token.is_empty() {
            return None;
        }
        match token.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "alt" => modifiers |= MOD_ALT,
            "shift" => modifiers |= MOD_SHIFT,
            "win" | "windows" => modifiers |= MOD_WIN,
            other => {
                // 两个主键(`ctrl+a+b`)不是"最后一个说了算",是打错了。
                if vk.is_some() {
                    return None;
                }
                vk = Some(virtual_key(other)?);
            }
        }
    }
    // 没有修饰键的全局热键会把那个键从整个系统里抢走,包括游戏。
    if modifiers == 0 {
        return None;
    }
    Some(Hotkey { modifiers, vk: vk? })
}

/// 主键那一段 → 虚拟键码。只认字母、数字和 F1–F24。
fn virtual_key(token: &str) -> Option<u32> {
    let mut chars = token.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        // 单个字符:字母和数字的虚拟键码就是它的大写 ASCII 码。
        if first.is_ascii_alphanumeric() {
            return Some(u32::from(first.to_ascii_uppercase() as u8));
        }
        return None;
    }
    let number = token.strip_prefix('f')?.parse::<u32>().ok()?;
    if (1..=MAX_FUNCTION_KEY).contains(&number) {
        return Some(VK_F1 + number - 1);
    }
    None
}

#[cfg(test)]
mod hotkey_tests {
    use super::*;

    #[test]
    fn the_default_binding_parses() {
        assert_eq!(
            parse_hotkey("ctrl+alt+d"),
            Some(Hotkey {
                modifiers: MOD_CONTROL | MOD_ALT,
                vk: u32::from(b'D'),
            })
        );
    }

    #[test]
    fn spacing_and_case_and_spelling_do_not_matter() {
        let expected = parse_hotkey("ctrl+alt+d");
        for spelling in ["  CTRL + ALT + D ", "Control+Alt+D", "alt+ctrl+d"] {
            assert_eq!(parse_hotkey(spelling), expected, "{spelling}");
        }
    }

    #[test]
    fn letters_digits_and_function_keys_are_all_allowed() {
        assert_eq!(parse_hotkey("shift+7").unwrap().vk, u32::from(b'7'));
        assert_eq!(parse_hotkey("win+q").unwrap().modifiers, MOD_WIN);
        assert_eq!(parse_hotkey("ctrl+shift+f12").unwrap().vk, 0x7B);
        assert_eq!(parse_hotkey("ctrl+f1").unwrap().vk, 0x70);
        assert_eq!(parse_hotkey("ctrl+f24").unwrap().vk, 0x70 + 23);
        assert_eq!(
            parse_hotkey("ctrl+shift+alt+win+a").unwrap().modifiers,
            MOD_CONTROL | MOD_SHIFT | MOD_ALT | MOD_WIN
        );
    }

    /// 一个没有修饰键的热键会把那个键从游戏里抢走 —— 宁可不开。
    #[test]
    fn a_bare_key_is_refused() {
        assert_eq!(parse_hotkey("d"), None);
        assert_eq!(parse_hotkey("f12"), None);
    }

    #[test]
    fn garbage_is_refused_instead_of_guessed() {
        for garbage in [
            "",
            "   ",
            "+",
            "ctrl+",
            "+d",
            "ctrl++d",
            "ctr+alt+d",
            "ctrl+alt",
            "ctrl+alt+d+e",
            "ctrl+f0",
            "ctrl+f25",
            "ctrl+esc",
            "ctrl+中",
            "ctrl+alt+dd",
        ] {
            assert_eq!(parse_hotkey(garbage), None, "{garbage:?} 不该解析成热键");
        }
    }
}
