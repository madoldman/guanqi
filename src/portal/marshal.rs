//! D-Bus 线格式编码器：本模块只会发送 METHOD_CALL 消息。
//!
//! 对齐基准与规则见 [`super::wire`] 模块文档；所有填充字节必须为 0。

use super::PortalError;
use super::wire::{Value, sig_align};

/// 封包一条 METHOD_CALL 消息，返回完整字节流（头部 + 补齐 + 消息体）。
pub(crate) fn marshal_call(
    serial: u32,
    flags: u8,
    destination: &str,
    path: &str,
    interface: &str,
    member: &str,
    body: &[Value],
) -> Result<Vec<u8>, PortalError> {
    let body_sig: String = body.iter().map(Value::signature).collect();
    let mut buf = Vec::with_capacity(96 + body_sig.len() * 4);
    // 固定头 yyyyuuu：小端 / METHOD_CALL / 标志 / 协议版本 1；两个长度最后回填。
    buf.push(b'l');
    buf.push(1); // METHOD_CALL
    buf.push(flags);
    buf.push(1); // 协议版本
    buf.extend_from_slice(&0u32.to_le_bytes()); // 消息体长度占位
    buf.extend_from_slice(&serial.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // 头部字段数组长度占位
    let fields_start = buf.len();
    field(&mut buf, 1, &Value::Path(path.to_owned()))?;
    field(&mut buf, 2, &Value::Str(interface.to_owned()))?;
    field(&mut buf, 3, &Value::Str(member.to_owned()))?;
    if !destination.is_empty() {
        field(&mut buf, 6, &Value::Str(destination.to_owned()))?;
    }
    if !body_sig.is_empty() {
        field(&mut buf, 8, &Value::Sig(body_sig))?;
    }
    let fields_len = (buf.len() - fields_start) as u32;
    buf[12..16].copy_from_slice(&fields_len.to_le_bytes());
    // 头部总长补齐到 8 的倍数，使消息体从 8 字节边界开始（规范 "Message Format"）。
    align(&mut buf, 8);
    let body_start = buf.len();
    for value in body {
        marshal_value(&mut buf, value)?;
    }
    let body_len = (buf.len() - body_start) as u32;
    buf[4..8].copy_from_slice(&body_len.to_le_bytes());
    Ok(buf)
}

/// 写一个头部字段 (yv)：结构按 8 对齐；变体按 1 对齐（签名 g 紧跟字段码之后）。
fn field(buf: &mut Vec<u8>, code: u8, value: &Value) -> Result<(), PortalError> {
    align(buf, 8);
    buf.push(code);
    let sig = value.signature();
    buf.push(sig.len() as u8);
    buf.extend_from_slice(sig.as_bytes());
    buf.push(0);
    marshal_value(buf, value)
}

fn align(buf: &mut Vec<u8>, n: usize) {
    buf.resize(buf.len().div_ceil(n) * n, 0);
}

fn marshal_value(buf: &mut Vec<u8>, value: &Value) -> Result<(), PortalError> {
    match value {
        Value::Byte(v) => buf.push(*v),
        Value::Bool(v) => {
            align(buf, 4);
            buf.extend_from_slice(&u32::from(*v).to_le_bytes());
        }
        Value::I16(v) => {
            align(buf, 2);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Value::U16(v) => {
            align(buf, 2);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Value::I32(v) => {
            align(buf, 4);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Value::U32(v) => {
            align(buf, 4);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Value::I64(v) => {
            align(buf, 8);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Value::U64(v) => {
            align(buf, 8);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Value::F64(v) => {
            align(buf, 8);
            buf.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        Value::Str(v) | Value::Path(v) => {
            align(buf, 4);
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            buf.extend_from_slice(v.as_bytes());
            buf.push(0);
        }
        Value::Sig(v) => {
            // SIGNATURE 前置 u8 长度，按 1 字节对齐，从不填充。
            buf.push(v.len() as u8);
            buf.extend_from_slice(v.as_bytes());
            buf.push(0);
        }
        Value::Array { elem, items } => {
            let elem_align = sig_align(elem)?;
            align(buf, 4); // 长度前缀按 u32 对齐
            let len_at = buf.len();
            buf.extend_from_slice(&0u32.to_le_bytes());
            align(buf, elem_align); // 空数组也必须填充到元素对齐
            let start = buf.len();
            for item in items {
                marshal_value(buf, item)?;
            }
            let len = (buf.len() - start) as u32;
            buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        }
        Value::Struct(fields) => {
            align(buf, 8); // 结构（含字典条目）恒按 8 对齐
            for field in fields {
                marshal_value(buf, field)?;
            }
        }
        Value::Variant(inner) => {
            // 变体按 1 字节对齐：签名紧跟，内部值再按自身类型对齐。
            let sig = inner.signature();
            buf.push(sig.len() as u8);
            buf.extend_from_slice(sig.as_bytes());
            buf.push(0);
            marshal_value(buf, inner)?;
        }
    }
    Ok(())
}
