//! JWT 里"看一眼就够"的那几段。
//!
//! 交易站塞在挂单里的 `whisper_token` / `hideout_token` 是短命 JWT。这里
//! **只解码,不验签** —— 签名那一段从来不碰,也没有任何一处拿这里的结果
//! 做安全判断。要回答的只有一个问题:"我现在按下按钮,这张票还没作废吧?"
//!
//! 为什么放在 `pnd-trade`:token 是这个 crate 从交易站的响应里读出来的,
//! 运行时和两个探针问的是同一个问题,那就只该有一份答案。早先 `live_probe`
//! 自己抄了一份 base64url 解码,于是"探针验证的是不是生产代码"这件事
//! 就说不清了。

use serde_json::Value;

/// JWT 第二段(payload)解出来的 JSON —— 也就是"声明"。
///
/// 解不开(不是 JWT、不是 base64url、不是 JSON)一律 `None`:调用方少印
/// 一行,不该因为服务端换了个格式就炸。
#[must_use]
pub fn jwt_claims(token: &str) -> Option<Value> {
    jwt_section(token, 1)
}

/// JWT 第一段(header)。探针拿它印"这张票是怎么签的"。
#[must_use]
pub fn jwt_header(token: &str) -> Option<Value> {
    jwt_section(token, 0)
}

/// `exp` 声明:这张票作废的那一刻(unix 秒)。
///
/// 没有这个声明就是 `None` —— 那时只能退回"我是什么时候拿到它的"去猜,
/// 而猜出来的答案比 token 自己说的差得远。
#[must_use]
pub fn jwt_expiry(token: &str) -> Option<i64> {
    jwt_claims(token)?.get("exp")?.as_i64()
}

/// 第 `index` 段 → JSON。第三段是签名,谁也不该拿它来调这个函数。
fn jwt_section(token: &str, index: usize) -> Option<Value> {
    let section = token.split('.').nth(index)?;
    let bytes = decode_base64url(section)?;
    serde_json::from_slice(&bytes).ok()
}

/// base64url 解码(无填充)。JWT 的每一段就这一种写法。
///
/// 手写而不是拉一个 base64 crate:三十行、只服务这一个用途,而多一个依赖
/// 就多一份要跟着升级的东西。
fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for ch in input.chars() {
        if ch == '=' {
            break;
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1u32 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod jwt_tests {
    use super::*;

    /// 一段字节 → base64url(无填充)。测试要造 token,总得有个编码器。
    fn encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            let take = chunk.len() + 1;
            for i in 0..take {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            }
        }
        out
    }

    /// 手搓一张 JWT:头和载荷是真的 base64url JSON,签名那一段是占位符
    /// (我们从来不看它)。
    fn token(payload: &str) -> String {
        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"HS256","typ":"JWT"}"#),
            encode(payload.as_bytes()),
            "c2lnbmF0dXJl"
        )
    }

    #[test]
    fn a_well_formed_token_gives_up_its_claims_and_its_expiry() {
        let jwt = token(r#"{"sub":"listing-1","exp":1757260800}"#);
        let claims = jwt_claims(&jwt).expect("claims");
        assert_eq!(claims.get("sub").and_then(Value::as_str), Some("listing-1"));
        assert_eq!(jwt_expiry(&jwt), Some(1_757_260_800));
        // header 也读得出来 —— live_probe 就靠它说"这张票是怎么签的"。
        let header = jwt_header(&jwt).expect("header");
        assert_eq!(header.get("alg").and_then(Value::as_str), Some("HS256"));
    }

    /// 没有 `exp` 的 token 不是坏 token:声明照样读得出来,只是问不出
    /// "什么时候作废" —— 调用方该退回"多久没换过"那条规则。
    #[test]
    fn a_token_without_an_exp_still_has_claims() {
        let jwt = token(r#"{"sub":"listing-1"}"#);
        assert!(jwt_claims(&jwt).is_some());
        assert_eq!(jwt_expiry(&jwt), None);
    }

    /// 各种不是 JWT 的东西一律安静地回 `None`,不 panic 也不瞎猜。
    #[test]
    fn garbage_is_not_mistaken_for_a_token() {
        assert_eq!(jwt_expiry(""), None);
        assert_eq!(jwt_expiry("garbage"), None, "一段都没有");
        assert_eq!(jwt_expiry("not.a.jwt"), None, "解得开 base64 但不是 JSON");
        assert_eq!(jwt_claims("aaa.$$$$.bbb"), None, "第二段不是 base64url");
        assert_eq!(jwt_claims("only-one-section"), None);
        // `exp` 是个字符串的时候也当没有:我们要的是一个时刻,不是一段文字。
        assert_eq!(jwt_expiry(&token(r#"{"exp":"soon"}"#)), None);
    }
}
