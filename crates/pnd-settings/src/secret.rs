//! 交易站的会话 cookie 在盘上长什么样,以及为什么不是明文。
//!
//! **盘上的写法是 `dpapi:<base64>`。** `settings.json` 是一份谁都能双击打开的
//! 普通 JSON 文件,而 POESESSID 等于一次登录 —— 拿到它的人不用密码就能以你的
//! 身份用交易站。所以存进去的不是 cookie 本身,而是先交给 Windows 的 DPAPI
//! 加密、再按 base64 写成一行的密文。
//!
//! 挑 DPAPI 而不是自己想一个口令来加密,是因为"自己想的口令"最后总得存在
//! 同一台机器的某个地方,那只是把同一个问题挪了个位置。DPAPI 的密钥挂在
//! Windows 账户上,由系统保管:同一台机器、同一个用户才解得开,文件拷到别处
//! 就是一堆没用的字节 —— 这正是一个会话 cookie 该有的性质。也正因为密钥跟着
//! 账户走,这里不需要再额外传一份"熵"(那又是一个得找地方存的秘密)。
//!
//! 读的时候两种写法都认:带 `dpapi:` 前缀的解密,不带的当成老版本写下的明文
//! 原样返回。于是升级不用重新登录一次,而下一次保存就把它换成密文
//! (钩子挂在 [`crate::AppSettings::poesessid`] 上)。
//!
//! 非 Windows 上两个函数都是恒等的:这个程序只在 Windows 上跑,别的平台只要
//! 编得过就行。

/// 密文那一行的前缀。没有它的一律按明文读。
pub const PROTECTED_PREFIX: &str = "dpapi:";

/// 明文 → 盘上的写法。空串还是空串:没有会话就没什么可藏的,
/// 而一行空的 `""` 比一段谁也看不懂的密文更容易一眼看出"这里没填"。
#[cfg(windows)]
#[must_use]
pub fn protect(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    // 这里故意不兜底:退回明文等于把这次改动整个作废,而悄悄存一个空串
    // 又会看起来像"设置丢了"。当前用户的 DPAPI 加密失败是系统级的异常,
    // 该让它响。
    let sealed = seal(plain.as_bytes()).expect("DPAPI could not protect the session cookie");
    format!("{PROTECTED_PREFIX}{}", encode_base64(&sealed))
}

/// 盘上的写法 → 明文。
///
/// `None` 只有一个意思:**那儿有一段密文,但这台机器/这个用户解不开**
/// (文件是从别处拷来的,或者被改坏了)。调用方该把它当成"没有会话",
/// 而不是把一串 base64 当 cookie 发出去。
#[cfg(windows)]
#[must_use]
pub fn unprotect(stored: &str) -> Option<String> {
    let Some(encoded) = stored.strip_prefix(PROTECTED_PREFIX) else {
        // 老版本写下的明文。
        return Some(stored.to_string());
    };
    let sealed = decode_base64(encoded)?;
    let plain = open(&sealed)?;
    String::from_utf8(plain).ok()
}

#[cfg(not(windows))]
#[must_use]
pub fn protect(plain: &str) -> String {
    plain.to_string()
}

#[cfg(not(windows))]
#[must_use]
pub fn unprotect(stored: &str) -> Option<String> {
    Some(stored.to_string())
}

// ---------------------------------------------------------------------
// DPAPI
// ---------------------------------------------------------------------

#[cfg(windows)]
fn seal(plain: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData};

    let input = blob(plain);
    let mut output = empty_blob();
    // SAFETY: `input` 指着 `plain` 的字节,调用期间一直活着;`output` 由
    // CryptProtectData 填,成功之后由 `take` 负责 LocalFree。
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),          // 描述串:不需要
            std::ptr::null(),          // 额外的熵:不需要,见模块注释
            std::ptr::null(),          // 保留
            std::ptr::null(),          // 提示窗:不要
            CRYPTPROTECT_UI_FORBIDDEN, // 绝不弹窗 —— 保存是后台干的,没人在看
            &raw mut output,
        )
    };
    take(ok, output)
}

#[cfg(windows)]
fn open(sealed: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{
        CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let input = blob(sealed);
    let mut output = empty_blob();
    // SAFETY: 同 `seal`。解不开时函数返回 0,`output` 不会被填。
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(), // 描述串:不问
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output,
        )
    };
    take(ok, output)
}

#[cfg(windows)]
fn blob(bytes: &[u8]) -> windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
    windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    }
}

#[cfg(windows)]
fn empty_blob() -> windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
    windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    }
}

/// 把 DPAPI 填出来的那块内存抄成 `Vec` 再还回去。
///
/// 两个 API 都是用 `LocalAlloc` 分配的,所以必须 `LocalFree`,不能交给 Rust 的
/// 分配器 —— 这也是这一段单独抽出来的理由:两条调用路径共用同一次释放。
#[cfg(windows)]
fn take(
    ok: windows_sys::core::BOOL,
    output: windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
) -> Option<Vec<u8>> {
    if ok == 0 {
        return None;
    }
    // SAFETY: 调用成功时 pbData 指着 cbData 个可读字节,而且这块内存归我们。
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let copied = bytes.to_vec();
    // SAFETY: 同上,而且只释放这一次。
    unsafe { windows_sys::Win32::Foundation::LocalFree(output.pbData.cast()) };
    Some(copied)
}

// ---------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------

/// 标准字母表(最后两位是 `+/`),带 `=` 填充。密文只在这个文件里进出,
/// 和搜索 id 那套 url-safe 的写法没有关系。
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 手写而不是拉一个 base64 crate:三十行、只服务这一个用途 —— 和
/// `pnd_trade::jwt` 那一份同样的取舍。
fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let packed = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(ALPHABET[((packed >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((packed >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((packed >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(packed & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 认不出来的字符一律 `None` —— 这一步宽容不了:解出半段字节喂给 DPAPI,
/// 换来的也只是同一个"解不开"。
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0_u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = ALPHABET.iter().position(|candidate| *candidate == byte)? as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // 剩 6 位说明最后一组只有一个字符,base64 不可能这样收尾。
    if bits >= 6 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    /// 没有会话就是没有:空串进、空串出,盘上也就写一个空串。
    #[test]
    fn an_empty_session_stays_empty() {
        assert_eq!(protect(""), "");
        assert_eq!(unprotect("").as_deref(), Some(""));
    }

    /// 不带前缀的一律是老版本写下的明文,原样读出来。
    #[test]
    fn a_bare_string_is_read_as_legacy_plain_text() {
        assert_eq!(
            unprotect("test-session-0000").as_deref(),
            Some("test-session-0000")
        );
    }

    #[test]
    fn base64_round_trips() {
        for bytes in [
            vec![],
            vec![0],
            vec![0, 1],
            vec![0, 1, 2],
            vec![0xff; 5],
            (0..=255_u8).collect::<Vec<_>>(),
        ] {
            let text = encode_base64(&bytes);
            assert_eq!(text.len() % 4, 0, "带填充的 base64 长度必是 4 的倍数");
            assert_eq!(decode_base64(&text), Some(bytes));
        }
        assert_eq!(decode_base64("!!!!"), None, "字母表外的字符不认");
        assert_eq!(decode_base64("A"), None, "base64 不会以一个字符收尾");
    }

    /// 加密再解密拿回同一串,而盘上那一行里找不到明文的影子。
    #[cfg(windows)]
    #[test]
    fn a_session_survives_a_round_trip_through_dpapi() {
        let stored = protect("test-session-0000");
        assert!(stored.starts_with(PROTECTED_PREFIX), "{stored}");
        assert!(!stored.contains("test-session-0000"), "{stored}");
        assert_eq!(unprotect(&stored).as_deref(), Some("test-session-0000"));
    }

    /// 解不开就是 `None`,不是 panic:那份文件可能是从别的机器拷来的。
    #[cfg(windows)]
    #[test]
    fn a_blob_we_cannot_open_is_none_rather_than_a_panic() {
        assert_eq!(
            unprotect("dpapi:AAAA"),
            None,
            "合法 base64,但不是我们的密文"
        );
        assert_eq!(unprotect("dpapi:!!!!"), None, "连 base64 都不是");
        assert_eq!(unprotect("dpapi:"), None, "空密文");
    }
}
