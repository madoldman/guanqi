//! D-Bus 线格式（wire format）的数据模型与解码器。
//!
//! 对齐规则均以**消息首字节**为基准（规范 "Byte order and alignment"）：
//!
//! - STRUCT / DICT_ENTRY 恒按 8 字节对齐；
//! - VARIANT 只按 1 字节对齐（签名字符串紧跟其前），其内部值再按自身类型对齐；
//! - 数组 = u32 长度（4 字节对齐）→ 填充到元素对齐（空数组同样要填充）→ 元素序列，
//!   长度不含长度后的填充，也不含末元素之后的填充；
//! - STRING / OBJECT_PATH 前置 u32 长度，SIGNATURE 前置 u8 长度，均以 NUL 收尾。
//!
//! 只覆盖本项目需要的类型子集；不处理 unix fd（h）。

use super::PortalError;

/// 消息类型码（规范 "Message types"）。
pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;

/// 调用标志：不需要应答（规范 "Flags"）。
pub const FLAG_NO_REPLY_EXPECTED: u8 = 0x1;

/// 头部字段码（规范 "Header fields"）。
const FIELD_PATH: u8 = 1;
const FIELD_INTERFACE: u8 = 2;
const FIELD_MEMBER: u8 = 3;
const FIELD_ERROR_NAME: u8 = 4;
const FIELD_REPLY_SERIAL: u8 = 5;
const FIELD_SIGNATURE: u8 = 8;

/// 单条消息大小上限（规范上限 128 MiB，这里收紧到 64 MiB 防异常分配）。
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// D-Bus 值（覆盖本模块需要的类型子集）。
///
/// 数组的元素签名必须显式给出（空数组无法从元素推断）；字典条目在线上就是
/// 两元素结构，用 [`Value::Struct`] 表达、元素签名写作 `{kv}` 形式即可。
#[derive(Debug, Clone)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    /// STRING（s）。
    Str(String),
    /// OBJECT_PATH（o）。
    Path(String),
    /// SIGNATURE（g）。
    Sig(String),
    /// 数组：`elem` 为元素签名（如 `{sv}`）。
    Array {
        elem: String,
        items: Vec<Value>,
    },
    /// 结构（含字典条目）。
    Struct(Vec<Value>),
    /// 变体（内部值自带签名）。
    Variant(Box<Value>),
}

impl Value {
    /// 值对应的 D-Bus 类型签名。
    pub fn signature(&self) -> String {
        match self {
            Self::Byte(_) => "y".into(),
            Self::Bool(_) => "b".into(),
            Self::I16(_) => "n".into(),
            Self::U16(_) => "q".into(),
            Self::I32(_) => "i".into(),
            Self::U32(_) => "u".into(),
            Self::I64(_) => "x".into(),
            Self::U64(_) => "t".into(),
            Self::F64(_) => "d".into(),
            Self::Str(_) => "s".into(),
            Self::Path(_) => "o".into(),
            Self::Sig(_) => "g".into(),
            Self::Array { elem, .. } => format!("a{elem}"),
            Self::Struct(fields) => {
                let inner: String = fields.iter().map(Self::signature).collect();
                format!("({inner})")
            }
            Self::Variant(_) => "v".into(),
        }
    }
}

/// 一条已解码的完整消息：头部字段展开为具名字段，消息体按 SIGNATURE 字段解码。
#[derive(Debug)]
pub struct Message {
    pub kind: u8,
    pub serial: u32,
    pub reply_serial: Option<u32>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub body: Vec<Value>,
}

/// 解析一条完整消息（固定头 + 头部字段数组 + 补齐 + 消息体在同一缓冲中）。
pub fn parse(buf: &[u8]) -> Result<Message, PortalError> {
    if buf.len() < 16 {
        return Err(short());
    }
    let big = match buf[0] {
        b'l' => false,
        b'B' => true,
        other => {
            return Err(PortalError::Protocol(format!(
                "未知字节序标记 0x{other:02x}"
            )));
        }
    };
    let body_len = read_u32(buf, 4, big) as usize;
    let serial = read_u32(buf, 8, big);
    let fields_len = read_u32(buf, 12, big) as usize;
    if body_len > MAX_MESSAGE_BYTES || fields_len > MAX_MESSAGE_BYTES {
        return Err(PortalError::Protocol("消息超过大小上限".into()));
    }
    let fields_end = 16 + fields_len;
    let body_pos = fields_end.next_multiple_of(8);
    if body_pos + body_len > buf.len() {
        return Err(short());
    }
    let mut r = Reader { buf, pos: 16, big };
    let mut msg = Message {
        kind: buf[1],
        serial,
        reply_serial: None,
        path: None,
        interface: None,
        member: None,
        error_name: None,
        body: Vec::new(),
    };
    let mut body_sig = String::new();
    while r.pos < fields_end {
        r.align(8); // (yv) 结构按 8 对齐
        if r.pos >= fields_end {
            break;
        }
        let code = r.u8()?;
        let value = r.variant()?;
        match code {
            FIELD_PATH => msg.path = Some(field_string(&value)?),
            FIELD_INTERFACE => msg.interface = Some(field_string(&value)?),
            FIELD_MEMBER => msg.member = Some(field_string(&value)?),
            FIELD_ERROR_NAME => msg.error_name = Some(field_string(&value)?),
            FIELD_REPLY_SERIAL => msg.reply_serial = field_u32(&value)?,
            FIELD_SIGNATURE => body_sig = field_string(&value)?,
            _ => {} // 未知字段按规范忽略
        }
    }
    r.pos = body_pos;
    for ty in parse_signature(&body_sig)? {
        msg.body.push(r.value(&ty)?);
    }
    Ok(msg)
}

fn short() -> PortalError {
    PortalError::Protocol("消息被截断".into())
}

/// 头部字段值外层是变体，取其内部标量。
fn field_string(value: &Value) -> Result<String, PortalError> {
    let Value::Variant(inner) = value else {
        return Err(PortalError::Protocol("头部字段值不是变体".into()));
    };
    match inner.as_ref() {
        Value::Str(s) | Value::Path(s) | Value::Sig(s) => Ok(s.clone()),
        _ => Err(PortalError::Protocol("头部字段值类型不符".into())),
    }
}

fn field_u32(value: &Value) -> Result<Option<u32>, PortalError> {
    let Value::Variant(inner) = value else {
        return Err(PortalError::Protocol("头部字段值不是变体".into()));
    };
    match inner.as_ref() {
        Value::U32(n) => Ok(Some(*n)),
        _ => Err(PortalError::Protocol("头部字段值类型不符".into())),
    }
}

fn read_u32(buf: &[u8], at: usize, big: bool) -> u32 {
    let bytes = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
    if big {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    }
}

/// 顺序读取器：位置与对齐均相对消息首字节。
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    big: bool,
}

impl Reader<'_> {
    fn need(&self, extra: usize) -> Result<(), PortalError> {
        if self.pos + extra > self.buf.len() {
            Err(short())
        } else {
            Ok(())
        }
    }

    fn align(&mut self, n: usize) {
        self.pos = self.pos.div_ceil(n) * n;
    }

    fn u8(&mut self) -> Result<u8, PortalError> {
        self.need(1)?;
        let value = self.buf[self.pos];
        self.pos += 1;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, PortalError> {
        self.align(2);
        self.need(2)?;
        let bytes = [self.buf[self.pos], self.buf[self.pos + 1]];
        self.pos += 2;
        Ok(if self.big {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    fn u32(&mut self) -> Result<u32, PortalError> {
        self.align(4);
        self.need(4)?;
        let value = read_u32(self.buf, self.pos, self.big);
        self.pos += 4;
        Ok(value)
    }

    fn u64(&mut self) -> Result<u64, PortalError> {
        self.align(8);
        self.need(8)?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.buf[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(if self.big {
            u64::from_be_bytes(bytes)
        } else {
            u64::from_le_bytes(bytes)
        })
    }

    /// STRING / OBJECT_PATH：u32 长度 + 内容 + NUL，按 4 字节对齐。
    fn str32(&mut self) -> Result<String, PortalError> {
        self.align(4);
        let len = self.u32()? as usize;
        self.need(len + 1)?;
        let text = self.utf8(len)?;
        if self.buf[self.pos + len] != 0 {
            return Err(PortalError::Protocol("字符串缺少 NUL 结尾".into()));
        }
        self.pos += len + 1;
        Ok(text)
    }

    /// SIGNATURE：u8 长度 + 内容 + NUL，按 1 字节对齐（从不填充）。
    fn sig(&mut self) -> Result<String, PortalError> {
        let len = self.u8()? as usize;
        self.need(len + 1)?;
        let text = self.utf8(len)?;
        if self.buf[self.pos + len] != 0 {
            return Err(PortalError::Protocol("签名缺少 NUL 结尾".into()));
        }
        self.pos += len + 1;
        Ok(text)
    }

    fn utf8(&self, len: usize) -> Result<String, PortalError> {
        std::str::from_utf8(&self.buf[self.pos..self.pos + len])
            .map(str::to_owned)
            .map_err(|_| PortalError::Protocol("字符串不是合法 UTF-8".into()))
    }

    /// VARIANT：按 1 字节对齐，签名紧跟其后，内部值按自身类型对齐。
    fn variant(&mut self) -> Result<Value, PortalError> {
        let text = self.sig()?;
        let (ty, used) = parse_one_type(&text)?;
        if used != text.len() {
            return Err(PortalError::Protocol(format!(
                "variant 签名含多余类型：{text}"
            )));
        }
        Ok(Value::Variant(Box::new(self.value(&ty)?)))
    }

    fn value(&mut self, ty: &Type) -> Result<Value, PortalError> {
        Ok(match ty {
            Type::Byte => Value::Byte(self.u8()?),
            Type::Bool => Value::Bool(self.u32()? != 0),
            Type::I16 => Value::I16(self.u16()? as i16),
            Type::U16 => Value::U16(self.u16()?),
            Type::I32 => Value::I32(self.u32()? as i32),
            Type::U32 => Value::U32(self.u32()?),
            Type::I64 => Value::I64(self.u64()? as i64),
            Type::U64 => Value::U64(self.u64()?),
            Type::F64 => Value::F64(f64::from_bits(self.u64()?)),
            Type::Str => Value::Str(self.str32()?),
            Type::Path => Value::Path(self.str32()?),
            Type::Sig => Value::Sig(self.sig()?),
            Type::Variant => self.variant()?,
            Type::Array(elem) => {
                self.align(4); // 长度前缀按 u32 对齐
                let len = self.u32()? as usize;
                self.align(align_of(elem)); // 首元素（含空数组）前填充到元素对齐
                let end = self
                    .pos
                    .checked_add(len)
                    .filter(|&end| end <= self.buf.len())
                    .ok_or_else(short)?;
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.value(elem)?);
                }
                if self.pos != end {
                    return Err(PortalError::Protocol("数组长度与元素序列不符".into()));
                }
                Value::Array {
                    elem: sig_of(elem),
                    items,
                }
            }
            Type::Struct(fields) => {
                self.align(8);
                let mut values = Vec::with_capacity(fields.len());
                for field in fields {
                    values.push(self.value(field)?);
                }
                Value::Struct(values)
            }
            Type::DictEntry(key, val) => {
                self.align(8);
                let key = self.value(key)?;
                let val = self.value(val)?;
                Value::Struct(vec![key, val])
            }
        })
    }
}

/// 签名解析出的类型树（仅用于驱动解码与对齐计算）。
#[derive(Debug)]
enum Type {
    Byte,
    Bool,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F64,
    Str,
    Path,
    Sig,
    Variant,
    Array(Box<Type>),
    Struct(Vec<Type>),
    DictEntry(Box<Type>, Box<Type>),
}

/// 类型自身的对齐字节数（数组按其长度前缀 u32 对齐，VARIANT 按 1 对齐）。
fn align_of(ty: &Type) -> usize {
    match ty {
        Type::Byte | Type::Sig => 1,
        Type::I16 | Type::U16 => 2,
        Type::Bool | Type::I32 | Type::U32 | Type::Str | Type::Path | Type::Array(_) => 4,
        Type::I64
        | Type::U64
        | Type::F64
        | Type::Variant
        | Type::Struct(_)
        | Type::DictEntry(..) => 8,
    }
}

fn sig_of(ty: &Type) -> String {
    match ty {
        Type::Byte => "y".into(),
        Type::Bool => "b".into(),
        Type::I16 => "n".into(),
        Type::U16 => "q".into(),
        Type::I32 => "i".into(),
        Type::U32 => "u".into(),
        Type::I64 => "x".into(),
        Type::U64 => "t".into(),
        Type::F64 => "d".into(),
        Type::Str => "s".into(),
        Type::Path => "o".into(),
        Type::Sig | Type::Variant => "g".into(),
        Type::Array(elem) => format!("a{}", sig_of(elem)),
        Type::Struct(fields) => {
            let inner: String = fields.iter().map(sig_of).collect();
            format!("({inner})")
        }
        Type::DictEntry(key, val) => format!("{{{}{}}}", sig_of(key), sig_of(val)),
    }
}

/// 单完整类型签名的对齐字节数（编码器计算数组元素对齐用）。
pub fn sig_align(sig: &str) -> Result<usize, PortalError> {
    let (ty, used) = parse_one_type(sig)?;
    if used != sig.len() {
        return Err(PortalError::Protocol(format!("签名含多余类型：{sig}")));
    }
    Ok(align_of(&ty))
}

/// 解析完整签名为类型序列（消息体签名可以是多个完整类型的拼接）。
fn parse_signature(sig: &str) -> Result<Vec<Type>, PortalError> {
    let mut types = Vec::new();
    let mut rest = sig;
    while !rest.is_empty() {
        let (ty, used) = parse_one_type(rest)?;
        types.push(ty);
        rest = &rest[used..];
    }
    Ok(types)
}

/// 解析签名中的**一个完整类型**，返回类型与消耗的字节数。
fn parse_one_type(sig: &str) -> Result<(Type, usize), PortalError> {
    let (ty, used) = match sig.as_bytes().first() {
        Some(b'y') => (Type::Byte, 1),
        Some(b'b') => (Type::Bool, 1),
        Some(b'n') => (Type::I16, 1),
        Some(b'q') => (Type::U16, 1),
        Some(b'i') => (Type::I32, 1),
        Some(b'u') => (Type::U32, 1),
        Some(b'x') => (Type::I64, 1),
        Some(b't') => (Type::U64, 1),
        Some(b'd') => (Type::F64, 1),
        Some(b's') => (Type::Str, 1),
        Some(b'o') => (Type::Path, 1),
        Some(b'g') => (Type::Sig, 1),
        Some(b'v') => (Type::Variant, 1),
        Some(b'a') => {
            let (elem, used) = parse_one_type(&sig[1..])?;
            (Type::Array(Box::new(elem)), used + 1)
        }
        Some(b'(') => {
            let (fields, used) = parse_until(sig, b')')?;
            (Type::Struct(fields), used)
        }
        Some(b'{') => {
            let (mut fields, used) = parse_until(sig, b'}')?;
            if fields.len() != 2 {
                return Err(PortalError::Protocol("字典条目必须恰有两个成员".into()));
            }
            let key = fields.remove(0);
            let val = fields.remove(0);
            (Type::DictEntry(Box::new(key), Box::new(val)), used)
        }
        _ => {
            return Err(PortalError::Protocol(format!(
                "签名含未知或残缺类型：{sig}"
            )));
        }
    };
    Ok((ty, used))
}

/// sig 以 `(` 或 `{` 开头：收集到配对的收尾符，返回成员与总消耗字节数。
fn parse_until(sig: &str, close: u8) -> Result<(Vec<Type>, usize), PortalError> {
    let mut fields = Vec::new();
    let mut i = 1;
    while sig.as_bytes().get(i) != Some(&close) {
        let rest = sig.get(i..).ok_or_else(short)?;
        let (ty, used) = parse_one_type(rest)?;
        fields.push(ty);
        i += used;
    }
    Ok((fields, i + 1))
}
