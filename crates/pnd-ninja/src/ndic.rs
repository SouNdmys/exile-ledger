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
//! u32 count              再写一遍条目数
//! block_count × (u32 字节偏移, u32 起始下标)      随机访问用的跳表,我们顺序读,跳过
//! count × u8             每条字符串的字节长度
//! UTF-8 字符串           按顺序首尾相接,没有分隔符
//! ```
//!
//! 长度是单字节,所以任何一条都不能超过 255 字节——对物品名/职业名足够。

use thiserror::Error;

/// 头部固定 36 字节:4 魔数 + 4 版本 + 4 零 + 4 条目数 + 8 哈希 + 4 块大小
/// + 4 块数 + 4 条目数。
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
    #[error("the two entry counts in the header disagree")]
    CountMismatch,
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
    let count2 = read_u32(header, 32) as usize;
    if count != count2 {
        return Err(NdicError::CountMismatch);
    }

    // 跳表只服务随机访问;我们从头读到尾,只需要知道它有多长。
    let block_table_len = block_count.checked_mul(8).ok_or(NdicError::Truncated)?;
    let lengths_start = HEADER_LEN
        .checked_add(block_table_len)
        .ok_or(NdicError::Truncated)?;
    let strings_start = lengths_start
        .checked_add(count)
        .ok_or(NdicError::Truncated)?;
    let lengths = bytes
        .get(lengths_start..strings_start)
        .ok_or(NdicError::Truncated)?;

    let mut entries = Vec::with_capacity(count);
    let mut pos = strings_start;
    for &len in lengths {
        let end = pos.checked_add(len as usize).ok_or(NdicError::Truncated)?;
        let slice = bytes.get(pos..end).ok_or(NdicError::Truncated)?;
        entries.push(
            std::str::from_utf8(slice)
                .map_err(|_| NdicError::Utf8)?
                .to_owned(),
        );
        pos = end;
    }
    // 长度表把整段字符串区正好用完是这份格式的自证:剩下字节说明我们读歪了。
    if pos != bytes.len() {
        return Err(NdicError::TrailingBytes);
    }
    Ok(entries)
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

    /// 按上面文档的布局造一张字典。写一遍编码器,解码器的每个偏移量就都被钉死了。
    fn build(
        version: u32,
        entries: &[&str],
        block_size: usize,
        count_override: Option<u32>,
    ) -> Vec<u8> {
        let count = entries.len();
        let block_count = count.div_ceil(block_size);
        let mut out = Vec::new();
        out.extend_from_slice(b"NDIC");
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(count as u32).to_le_bytes());
        out.extend_from_slice(&0x85ad_a30f_e321_7fc7u64.to_le_bytes());
        out.extend_from_slice(&(block_size as u32).to_le_bytes());
        out.extend_from_slice(&(block_count as u32).to_le_bytes());
        out.extend_from_slice(&count_override.unwrap_or(count as u32).to_le_bytes());
        assert_eq!(out.len(), HEADER_LEN);

        let mut offset = 0u32;
        for block in 0..block_count {
            let start_index = block * block_size;
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(start_index as u32).to_le_bytes());
            offset += entries[start_index..(start_index + block_size).min(count)]
                .iter()
                .map(|entry| entry.len() as u32)
                .sum::<u32>();
        }
        for entry in entries {
            out.push(entry.len() as u8);
        }
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

    #[test]
    fn rejects_disagreeing_counts() {
        let bytes = build(2, &["a", "b"], 16, Some(3));
        assert_eq!(parse_ndic(&bytes), Err(NdicError::CountMismatch));
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
