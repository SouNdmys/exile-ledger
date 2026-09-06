//! 最小 protobuf 线格式读取器。
//!
//! builds 搜索返回 `application/x-protobuf`,但 poe.ninja 从不公开 `.proto`。
//! 为一份随时会漂移的私有 schema 引入 prost + build.rs 是本末倒置:线格式本身
//! 就自带"字段号 + 线类型 + 长度",不认识的字段天生可跳过。所以这里只读线格式,
//! 由 `search.rs` 按字段号自己挑要的东西——对面加字段我们不会崩,改字段号我们会
//! 少一列而不是解析爆炸。

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WireError {
    #[error("input ended in the middle of a value")]
    Truncated,
    #[error("varint is longer than the 10 bytes a u64 can hold")]
    VarintTooLong,
    /// 线类型 3/4 是已废弃的 group。真出现说明我们读错了位置,应当大声失败。
    #[error("wire type {0} is not supported")]
    UnsupportedWireType(u8),
    #[error("{0} is not a valid protobuf field number")]
    InvalidFieldNumber(u64),
    #[error("bytes are not valid UTF-8")]
    Utf8,
}

/// 一个字段的原始值。解释成什么(i32? 字符串? 嵌套消息?)由调用方按字段号决定,
/// 因为线格式本身并不携带这个信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireValue<'a> {
    Varint(u64),
    Fixed64([u8; 8]),
    Bytes(&'a [u8]),
    Fixed32([u8; 4]),
}

impl<'a> WireValue<'a> {
    /// 取 varint;类型不符时给 `None` 而不是报错——上层大多是"能拿就拿"的心态。
    #[must_use]
    pub fn as_varint(&self) -> Option<u64> {
        match self {
            WireValue::Varint(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            WireValue::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }
}

/// 从 `buf[*pos]` 读一个 varint 并把 `pos` 推到它后面。
///
/// 单独暴露是因为 packed 字段的内部就是一串裸 varint,没有 tag 可循。
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64, WireError> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for _ in 0..10 {
        let byte = *buf.get(*pos).ok_or(WireError::Truncated)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(WireError::VarintTooLong)
}

/// packed repeated varint 字段的内容:整段就是紧挨着的 varint,没有分隔符。
pub fn packed_varints(bytes: &[u8]) -> Result<Vec<u64>, WireError> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < bytes.len() {
        out.push(read_varint(bytes, &mut pos)?);
    }
    Ok(out)
}

/// protobuf 的 string 字段就是 length-delimited 的 UTF-8。
pub fn utf8(bytes: &[u8]) -> Result<&str, WireError> {
    std::str::from_utf8(bytes).map_err(|_| WireError::Utf8)
}

/// 顺序扫一遍消息里的字段。嵌套消息就是把它的 `Bytes` 再包一个 `WireReader`。
pub struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// 出过一次错就彻底停下:线格式一旦读歪,后面的字节全是噪声,
    /// 继续吐"字段"只会把错误伪装成数据。
    failed: bool,
}

impl<'a> WireReader<'a> {
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            failed: false,
        }
    }

    fn read_field(&mut self) -> Result<(u32, WireValue<'a>), WireError> {
        let key = read_varint(self.buf, &mut self.pos)?;
        let field_number = key >> 3;
        if field_number == 0 || field_number > u64::from(u32::MAX) {
            return Err(WireError::InvalidFieldNumber(field_number));
        }
        let field = field_number as u32;
        let wire_type = (key & 7) as u8;
        let value = match wire_type {
            0 => WireValue::Varint(read_varint(self.buf, &mut self.pos)?),
            1 => WireValue::Fixed64(self.take_array::<8>()?),
            2 => {
                let len = read_varint(self.buf, &mut self.pos)?;
                let len = usize::try_from(len).map_err(|_| WireError::Truncated)?;
                WireValue::Bytes(self.take_slice(len)?)
            }
            5 => WireValue::Fixed32(self.take_array::<4>()?),
            other => return Err(WireError::UnsupportedWireType(other)),
        };
        Ok((field, value))
    }

    fn take_slice(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(len).ok_or(WireError::Truncated)?;
        let slice = self.buf.get(self.pos..end).ok_or(WireError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let slice = self.take_slice(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }
}

/// 实现 `Iterator` 而不是自己写一个同名方法:签名一模一样,还能白拿 `for` 循环。
impl<'a> Iterator for WireReader<'a> {
    type Item = Result<(u32, WireValue<'a>), WireError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.buf.len() {
            return None;
        }
        let result = self.read_field();
        if result.is_err() {
            self.failed = true;
        }
        Some(result)
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// 测试里手搓字节串比引一个编码库更能说明"我们以为的线格式长什么样"。
    fn tag(field: u32, wire_type: u8) -> Vec<u8> {
        encode_varint(u64::from(field) << 3 | u64::from(wire_type))
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
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

    fn bytes_field(field: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = tag(field, 2);
        out.extend(encode_varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn varint_field(field: u32, value: u64) -> Vec<u8> {
        let mut out = tag(field, 0);
        out.extend(encode_varint(value));
        out
    }

    #[test]
    fn reads_a_nested_message() {
        let mut inner = varint_field(1, 62_108);
        inner.extend(bytes_field(2, b"class"));
        let outer = bytes_field(1, &inner);

        let mut reader = WireReader::new(&outer);
        let (field, value) = reader.next().unwrap().unwrap();
        assert_eq!(field, 1);
        let nested = value.as_bytes().unwrap();
        assert!(reader.next().is_none());

        let decoded: Vec<_> = WireReader::new(nested).map(Result::unwrap).collect();
        assert_eq!(decoded[0], (1, WireValue::Varint(62_108)));
        assert_eq!(decoded[1], (2, WireValue::Bytes(b"class")));
        assert_eq!(utf8(decoded[1].1.as_bytes().unwrap()).unwrap(), "class");
    }

    #[test]
    fn reads_a_packed_varint_list() {
        let mut payload = Vec::new();
        for value in [0u64, 1, 127, 128, 300, 1_000_000] {
            payload.extend(encode_varint(value));
        }
        let message = bytes_field(6, &payload);

        let (field, value) = WireReader::new(&message).next().unwrap().unwrap();
        assert_eq!(field, 6);
        assert_eq!(
            packed_varints(value.as_bytes().unwrap()).unwrap(),
            vec![0, 1, 127, 128, 300, 1_000_000]
        );
    }

    /// 五字节 varint:超过 u32 的值 poe.ninja 是会发的(总人数、dps 都不小)。
    #[test]
    fn reads_a_five_byte_varint() {
        let encoded = encode_varint(4_294_967_295);
        assert_eq!(encoded.len(), 5);
        let mut pos = 0;
        assert_eq!(read_varint(&encoded, &mut pos).unwrap(), 4_294_967_295);
        assert_eq!(pos, 5);
    }

    #[test]
    fn ten_byte_varint_is_the_limit() {
        let all_continuations = [0xffu8; 11];
        let mut pos = 0;
        assert_eq!(
            read_varint(&all_continuations, &mut pos),
            Err(WireError::VarintTooLong)
        );
    }

    #[test]
    fn truncated_length_delimited_field_is_an_error() {
        let mut message = bytes_field(1, b"forbidden-rites");
        message.truncate(message.len() - 3);
        let mut reader = WireReader::new(&message);
        assert_eq!(reader.next().unwrap(), Err(WireError::Truncated));
        // 出错后不再继续吐字段。
        assert!(reader.next().is_none());
    }

    #[test]
    fn truncated_varint_is_an_error() {
        let message = [0x08u8, 0x80];
        let mut reader = WireReader::new(&message);
        assert_eq!(reader.next().unwrap(), Err(WireError::Truncated));
    }

    #[test]
    fn group_wire_types_are_rejected() {
        let message = tag(1, 3);
        let mut reader = WireReader::new(&message);
        assert_eq!(
            reader.next().unwrap(),
            Err(WireError::UnsupportedWireType(3))
        );
    }

    #[test]
    fn field_number_zero_is_rejected() {
        let message = [0x00u8, 0x01];
        let mut reader = WireReader::new(&message);
        assert_eq!(
            reader.next().unwrap(),
            Err(WireError::InvalidFieldNumber(0))
        );
    }

    #[test]
    fn fixed_width_fields_round_trip() {
        let mut message = tag(3, 5);
        message.extend_from_slice(&1.5f32.to_le_bytes());
        message.extend(tag(4, 1));
        message.extend_from_slice(&2.5f64.to_le_bytes());

        let decoded: Vec<_> = WireReader::new(&message).map(Result::unwrap).collect();
        assert_eq!(decoded[0], (3, WireValue::Fixed32(1.5f32.to_le_bytes())));
        assert_eq!(decoded[1], (4, WireValue::Fixed64(2.5f64.to_le_bytes())));
    }
}
