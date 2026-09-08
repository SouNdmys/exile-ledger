//! poe.ninja 自制的 "NDIC" 字符串表。
//!
//! 搜索返回的分面和列里存的都是下标,真正的名字("Gemling Legionnaire"、
//! "Wake of Destruction"、"Rare Ring")在这张表里。表按 sha1 寻址、内容不可变,
//! 所以可以放心长期缓存。
//!
//! 布局(全小端):
//!
//! ```text
//! "NDIC"                 4 字节魔数
//! u32 version            = 2
//! u32 zero               恒为 0
//! u32 count              条目数
//! u64 hash               内容哈希,我们不校验
//! u32 block_size         = 16,即每 16 条一个索引块
//! u32 block_count        索引块数
//! u32 length_bytes       长度表占多少字节
//! block_count × (u32 字符串区字节偏移, u32 长度表字节偏移)   随机访问用的跳表,我们顺序读,跳过
//! length_bytes 字节      count 个 LEB128 变长整数,每条字符串的字节长度
//! UTF-8 字符串           按顺序首尾相接,没有分隔符
//! ```
//!
//! **长度是 LEB128 变长整数,不是单字节**:127 字节以内占一个字节,再长就占两个。
//! 所以 `length_bytes` 通常等于 `count`(短名字的表里每条一字节),但只要表里有
//! 一条长词条,它就会比 `count` 大 —— PoE1 的 `mastery` 表 351 条、长度表 359
//! 字节,差的 8 就是 8 条超过 127 字节的天赋精通说明。
//!
//! 早先把最后那个 u32 读成"再写一遍条目数"、又按一条一字节读长度表,PoE2 的
//! 物品名/宝石名恰好全都短,所以一直没露馅;PoE1 一上来就撞上了。

use thiserror::Error;

/// 头部固定 36 字节:4 魔数 + 4 版本 + 4 零 + 4 条目数 + 8 哈希 + 4 块大小
/// + 4 块数 + 4 长度表字节数。
const HEADER_LEN: usize = 36;

/// 目前只见过 v2。格式无文档,版本一变就必须有人来看,不能猜着解析。
const SUPPORTED_VERSION: u32 = 2;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum NdicError {
    #[error("payload does not start with the NDIC magic")]
    BadMagic,
    #[error("NDIC version {0} is not supported")]
    UnsupportedVersion(u32),
    #[error("payload ended before the dictionary did")]
    Truncated,
    #[error("the length table does not hold exactly the promised number of entries")]
    LengthTableMismatch,
    #[error("payload has bytes left over after the last string")]
    TrailingBytes,
    #[error("a dictionary entry is not valid UTF-8")]
    Utf8,
}

/// 解析一整张字典。下标即数组下标,调用方直接 `dict[index]`。
pub fn parse_ndic(bytes: &[u8]) -> Result<Vec<String>, NdicError> {
    let header = bytes.get(..HEADER_LEN).ok_or(NdicError::Truncated)?;
    if &header[..4] != b"NDIC" {
        return Err(NdicError::BadMagic);
    }
    let version = read_u32(header, 4);
    if version != SUPPORTED_VERSION {
        return Err(NdicError::UnsupportedVersion(version));
    }
    let count = read_u32(header, 12) as usize;
    let block_count = read_u32(header, 28) as usize;
    let length_bytes = read_u32(header, 32) as usize;
    // 每条至少占一个长度字节,所以条目数比长度表还多是自相矛盾。先挡一道,
    // 下面的 `with_capacity` 就不会被一个乱写的头部骗去申请几 GB 内存。
    if count > length_bytes {
        return Err(NdicError::LengthTableMismatch);
    }

    // 跳表只服务随机访问;我们从头读到尾,只需要知道它有多长。
    let block_table_len = block_count.checked_mul(8).ok_or(NdicError::Truncated)?;
    let lengths_start = HEADER_LEN
        .checked_add(block_table_len)
        .ok_or(NdicError::Truncated)?;
    let strings_start = lengths_start
        .checked_add(length_bytes)
        .ok_or(NdicError::Truncated)?;
    let lengths = bytes
        .get(lengths_start..strings_start)
        .ok_or(NdicError::Truncated)?;

    let mut entries = Vec::with_capacity(count);
    let mut cursor = 0usize;
    let mut pos = strings_start;
    for _ in 0..count {
        let len = read_leb128(lengths, &mut cursor).ok_or(NdicError::LengthTableMismatch)?;
        let end = pos.checked_add(len).ok_or(NdicError::Truncated)?;
        let slice = bytes.get(pos..end).ok_or(NdicError::Truncated)?;
        entries.push(
            std::str::from_utf8(slice)
                .map_err(|_| NdicError::Utf8)?
                .to_owned(),
        );
        pos = end;
    }
    // count 个长度必须把长度表正好读完 —— 剩下字节说明我们把某一条读窄了。
    if cursor != lengths.len() {
        return Err(NdicError::LengthTableMismatch);
    }
    // 长度表把整段字符串区正好用完是这份格式的自证:剩下字节说明我们读歪了。
    if pos != bytes.len() {
        return Err(NdicError::TrailingBytes);
    }
    Ok(entries)
}

/// 从 `cursor` 读一个 LEB128,读完把 `cursor` 推到下一个字节。
///
/// 越界或者长到不像个字符串长度(5 个字节就够 u32 了)都给 `None`。
fn read_leb128(bytes: &[u8], cursor: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[cfg(test)]
mod ndic_tests {
    use super::*;

    /// LEB128:低 7 位一组,还有后续就把最高位置 1。
    fn leb128(mut value: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    /// 按上面文档的布局造一张字典。写一遍编码器,解码器的每个偏移量就都被钉死了。
    ///
    /// `length_bytes_override` 只给"头部说的长度表字节数是错的"那个测试用。
    fn build(
        version: u32,
        entries: &[&str],
        block_size: usize,
        length_bytes_override: Option<u32>,
    ) -> Vec<u8> {
        let count = entries.len();
        let block_count = count.div_ceil(block_size);
        let lengths: Vec<u8> = entries
            .iter()
            .flat_map(|entry| leb128(entry.len()))
            .collect();

        let mut out = Vec::new();
        out.extend_from_slice(b"NDIC");
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(count as u32).to_le_bytes());
        out.extend_from_slice(&0x85ad_a30f_e321_7fc7u64.to_le_bytes());
        out.extend_from_slice(&(block_size as u32).to_le_bytes());
        out.extend_from_slice(&(block_count as u32).to_le_bytes());
        out.extend_from_slice(
            &length_bytes_override
                .unwrap_or(lengths.len() as u32)
                .to_le_bytes(),
        );
        assert_eq!(out.len(), HEADER_LEN);

        // 跳表的两个 u32 是"这一块的第一条字符串从第几个字节开始"和
        // "它的长度从长度表的第几个字节开始"(线上就是这么写的:后者遇到
        // 两字节长度会比条目数走得快)。我们顺序读,所以只要长度对得上就行。
        let mut string_offset = 0u32;
        let mut length_offset = 0u32;
        for block in 0..block_count {
            let start_index = block * block_size;
            let end_index = (start_index + block_size).min(count);
            out.extend_from_slice(&string_offset.to_le_bytes());
            out.extend_from_slice(&length_offset.to_le_bytes());
            for entry in &entries[start_index..end_index] {
                string_offset += entry.len() as u32;
                length_offset += leb128(entry.len()).len() as u32;
            }
        }
        out.extend_from_slice(&lengths);
        for entry in entries {
            out.extend_from_slice(entry.as_bytes());
        }
        out
    }

    #[test]
    fn round_trips_a_three_entry_dictionary() {
        let entries = ["Rare Ring", "Magic Flask", "Wake of Destruction"];
        let bytes = build(2, &entries, 16, None);
        assert_eq!(parse_ndic(&bytes).unwrap(), entries);
    }

    /// 真实的 class 表:31 条、块大小 16、两个索引块。头部前 16 字节和线上
    /// 抓到的完全一致,这个测试就是在钉住"跳表要跳过 block_count × 8 字节"。
    #[test]
    fn round_trips_a_thirty_one_entry_dictionary_with_two_blocks() {
        let entries: Vec<String> = (0..31).map(|index| format!("Class {index}")).collect();
        let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
        let bytes = build(2, &refs, 16, None);

        assert_eq!(&bytes[..4], b"NDIC");
        assert_eq!(&bytes[4..8], &[0x02, 0x00, 0x00, 0x00]);
        assert_eq!(&bytes[8..12], &[0x00, 0x00, 0x00, 0x00]);
        assert_eq!(&bytes[12..16], &[0x1f, 0x00, 0x00, 0x00]);
        assert_eq!(read_u32(&bytes, 24), 16);
        assert_eq!(read_u32(&bytes, 28), 2);
        assert_eq!(read_u32(&bytes, 32), 31);

        let parsed = parse_ndic(&bytes).unwrap();
        assert_eq!(parsed.len(), 31);
        assert_eq!(parsed[0], "Class 0");
        assert_eq!(parsed[16], "Class 16");
        assert_eq!(parsed[30], "Class 30");
    }

    #[test]
    fn handles_multibyte_utf8() {
        let entries = ["Hand of Wisdom and Action", "护甲", "Δelta"];
        let bytes = build(2, &entries, 16, None);
        assert_eq!(parse_ndic(&bytes).unwrap(), entries);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = build(2, &["a"], 16, None);
        bytes[1] = b'X';
        assert_eq!(parse_ndic(&bytes), Err(NdicError::BadMagic));
    }

    #[test]
    fn rejects_unknown_version() {
        let bytes = build(3, &["a"], 16, None);
        assert_eq!(parse_ndic(&bytes), Err(NdicError::UnsupportedVersion(3)));
    }

    /// 条目数和长度表必须严丝合缝地互相用完:多一个字节、少一个字节,都说明
    /// 我们把某一条的长度读窄了(那正是 LEB128 那个 bug 的症状)。
    #[test]
    fn rejects_a_length_table_that_does_not_add_up() {
        // 头部说 1 条,长度表里躺着两条的长度。
        let mut short = build(2, &["a", "b"], 16, None);
        short[12] = 1;
        assert_eq!(parse_ndic(&short), Err(NdicError::LengthTableMismatch));
        // 反过来:头部说 3 条,长度表只够 2 条。
        let mut long = build(2, &["a", "b"], 16, None);
        long[12] = 3;
        assert_eq!(parse_ndic(&long), Err(NdicError::LengthTableMismatch));
    }

    /// 长度是 **LEB128 变长整数**,不是单字节。
    ///
    /// 128 字节以上的条目要占两个字节,这时头部最后那个 u32 会比条目数大 ——
    /// 它是"长度表有多少字节",不是"再写一遍条目数"。老解析器把它当条目数、
    /// 又按一条一字节读长度表,于是从第一条长词缀起整张表错位一格。
    #[test]
    fn an_entry_longer_than_127_bytes_takes_a_two_byte_length() {
        let long = "x".repeat(136);
        let entries = ["Rare Ring", long.as_str(), "Magic Flask"];
        let bytes = build(2, &entries, 16, None);
        // 3 条却有 4 个长度字节:长的那条自己占两个。
        assert_eq!(read_u32(&bytes, 12), 3);
        assert_eq!(read_u32(&bytes, 32), 4);
        assert_eq!(parse_ndic(&bytes).unwrap(), entries);
    }

    /// PoE1 的 `mastery` 字典,2026-09-09 从
    /// `/poe1/api/builds/dictionary/2d095f5e0a67b1097e5585378646aa72d83ab06a` 原样存下来。
    ///
    /// 它是这条 bug 的第一现场:351 条词条里有 8 条超过 127 字节(天赋精通的
    /// 说明本来就长),头部写着 count 351、长度表 359 字节。老解析器读到
    /// `351 != 359` 就报"两个条目数对不上",整个 PoE1 的搜索因此走不下去。
    const POE1_MASTERY: &[u8] = include_bytes!("../fixtures/poe1_mastery_dictionary.ndic");

    #[test]
    fn parses_the_real_poe1_mastery_dictionary() {
        let entries = parse_ndic(POE1_MASTERY).unwrap();
        assert_eq!(entries.len(), 351);
        assert_eq!(entries[0], "+0.3 metres to Melee Strike Range");
        // 63 是那条 136 字节的长词条,64 就是老解析器开始错位的地方
        // (它把 64 读成了一个字符 "1")。
        assert!(entries[63].len() > 127);
        assert_eq!(
            entries[64],
            "100% increased Armour from Equipped Boots and Gloves"
        );
        assert_eq!(
            entries[350],
            "Your Movement Speed is equal to the highest Movement Speed among Linked Players"
        );
    }

    #[test]
    fn rejects_truncated_payload() {
        let mut bytes = build(2, &["Rare Ring", "Magic Flask"], 16, None);
        bytes.truncate(bytes.len() - 4);
        assert_eq!(parse_ndic(&bytes), Err(NdicError::Truncated));
        assert_eq!(parse_ndic(b"NDI"), Err(NdicError::Truncated));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = build(2, &["Rare Ring"], 16, None);
        bytes.push(0);
        assert_eq!(parse_ndic(&bytes), Err(NdicError::TrailingBytes));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut bytes = build(2, &["ab"], 16, None);
        let last = bytes.len() - 1;
        bytes[last] = 0xff;
        assert_eq!(parse_ndic(&bytes), Err(NdicError::Utf8));
    }
}
