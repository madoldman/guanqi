//! SGF 模块：棋谱解析与序列化（纯 std，零第三方依赖）。
//!
//! 子模块分工：[`lexer`] 词法与转义；[`error`] 结构化错误与位置；
//! [`tree`] 游戏树结构、序列化与属性访问器（含对局信息汇总
//! [`GameInfo`]）。本文件提供字节解码入口与语法解析器。
//!
//! # 编码处理（扩展点）
//!
//! [`parse_bytes`] 先经 [`decode`] 把字节流转为字符串再解析：
//! 剥离 UTF-8 BOM 后按 UTF-8 校验；不是合法 UTF-8 时，嗅探根节点
//! `CA` 属性声明的字符集名，返回 [`SgfErrorKind::UnsupportedCharset`]
//! （错误内含字符集名，供 UI 提示"该文件是 GB2312 编码，请转存为
//! UTF-8"）。**将来接入 GBK / GB2312 等解码器时只需替换 [`decode`]
//! 这一处**——纯 std 下不实现大体积编码转换表，故把它留作唯一扩展点。
//!
//! # 宽容策略（兼容真实世界的 SGF 文件）
//!
//! - 未知属性原样保留（属性名统一大写），写出时不丢数据；
//! - 未知转义取字符本身，不报错（规则见 [`lexer`] 模块文档）；
//! - `AB` / `AW` / `AE` 的压缩矩形（`AB[aa:cc]`）在解析期原地展开；
//! - 空节点（`;` 后无属性）允许；属性顺序与同名多值顺序按文档保留。
//!
//! # 往返一致
//!
//! 属性顺序不重排、值解码后再转义写出，因此
//! `parse_str(&tree.write())? == tree`（未知属性与转义都不变）。

// 模块统一位于 lib 目标后，公开类型即导出 API，无需整体放宽 dead_code。
mod error;
mod lexer;
mod load;
mod tree;

pub use error::{Position, SgfError, SgfErrorKind};
pub use load::{GameMeta, LoadError, LoadedGame, load_from_bytes};
pub use tree::{GameInfo, GameTree, Node, Property};

use std::borrow::Cow;

use lexer::Cursor;

// ---- 字节解码（编码扩展点） ----

/// 字节流 → 字符串的唯一入口（编码扩展点，见模块文档）。
/// 当前实现：剥离 UTF-8 BOM → UTF-8 校验；失败时嗅探 `CA` 声明，
/// 返回带字符集名的结构化错误。
pub fn decode(bytes: &[u8]) -> Result<Cow<'_, str>, SgfError> {
    let bytes = bytes.strip_prefix(&b"\xEF\xBB\xBF"[..]).unwrap_or(bytes);
    // UTF-16 BOM：按"声明的其它字符集"报错，交给将来的解码器处理。
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(SgfError::new(
            SgfErrorKind::UnsupportedCharset(String::from("UTF-16")),
            None,
        ));
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(Cow::Borrowed(s)),
        Err(_) => match sniff_charset(bytes) {
            Some(name) if !is_utf8_name(&name) => Err(SgfError::new(
                SgfErrorKind::UnsupportedCharset(name),
                None,
            )),
            // 未声明字符集；或声明即 UTF-8 但字节非法（标注失实）。
            _ => Err(SgfError::new(SgfErrorKind::InvalidUtf8, None)),
        },
    }
}

/// 字符集名是否指 UTF-8（容忍大小写与连字符差异）。
fn is_utf8_name(name: &str) -> bool {
    name.to_ascii_lowercase().replace('-', "") == "utf8"
}

/// 在原始字节流中嗅探根节点 `CA[..]` 声明的字符集名。
/// 只在"非 UTF-8"报错路径使用，故为宽松的 ASCII 扫描：取第一处
/// `CA[`（属性名与字符集名必为 ASCII），足够生成错误提示。
fn sniff_charset(bytes: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"CA[" {
            let start = i + 3;
            let end = start + bytes[start..].iter().take(64).position(|&b| b == b']')?;
            return Some(String::from_utf8_lossy(&bytes[start..end]).trim().to_string());
        }
        i += 1;
    }
    None
}

// ---- 语法解析 ----

/// 解析 SGF 字节流（编码处理见 [`decode`]）。
pub fn parse_bytes(bytes: &[u8]) -> Result<GameTree, SgfError> {
    parse_str(&decode(bytes)?)
}

/// 解析已解码的 SGF 文本；顶层只允许一棵游戏树。
pub fn parse_str(s: &str) -> Result<GameTree, SgfError> {
    let mut cur = Cursor::new(s);
    cur.skip_ws();
    if cur.peek().is_none() {
        return Err(SgfError::new(SgfErrorKind::EmptyInput, Some(cur.position())));
    }
    let tree = parse_game_tree(&mut cur)?;
    cur.skip_ws();
    if cur.peek().is_some() {
        return Err(SgfError::new(
            SgfErrorKind::UnexpectedByte {
                expected: "文件结尾（顶层只允许一棵游戏树）",
                found: cur.peek(),
            },
            Some(cur.position()),
        ));
    }
    Ok(tree)
}

/// 游戏树：`"(" 节点序列 { 子树 } ")"`，子树即变着分支。
fn parse_game_tree(cur: &mut Cursor) -> Result<GameTree, SgfError> {
    cur.skip_ws();
    cur.expect('(', "游戏树起始的“(”")?;
    let mut nodes = Vec::new();
    let mut children = Vec::new();
    loop {
        cur.skip_ws();
        match cur.peek() {
            Some(';') => nodes.push(parse_node(cur)?),
            Some('(') => children.push(parse_game_tree(cur)?),
            Some(')') => {
                cur.bump();
                break;
            }
            found => {
                return Err(SgfError::new(
                    SgfErrorKind::UnexpectedByte {
                        expected: "节点“;”、子树“(”或树结尾“)”",
                        found,
                    },
                    Some(cur.position()),
                ));
            }
        }
    }
    if nodes.is_empty() {
        return Err(SgfError::new(SgfErrorKind::EmptyGameTree, Some(cur.position())));
    }
    Ok(GameTree { nodes, children })
}

/// 节点：`";" 属性 { 属性 }`（无属性的空节点允许）。
fn parse_node(cur: &mut Cursor) -> Result<Node, SgfError> {
    cur.skip_ws();
    cur.expect(';', "节点起始的“;”")?;
    let mut props = Vec::new();
    loop {
        cur.skip_ws();
        if !matches!(cur.peek(), Some(c) if c.is_ascii_alphabetic()) {
            break;
        }
        let ident = cur.read_ident();
        let mut values = Vec::new();
        while cur.peek_after_ws() == Some('[') {
            values.push(cur.read_value()?);
        }
        if values.is_empty() {
            return Err(SgfError::new(
                SgfErrorKind::PropNeedsValue { ident },
                Some(cur.position()),
            ));
        }
        expand_point_rects(&ident, &mut values);
        props.push(Property { ident, values });
    }
    Ok(Node { props })
}

/// 展开摆放属性的压缩矩形写法（如 `AB[aa:cc]`），原地替换为逐点值。
/// 仅作用于 `AB` / `AW` / `AE`；端点不是两个小写字母时原样保留，
/// 留给 [`Node::coords`] 报结构化坐标错误。
fn expand_point_rects(ident: &str, values: &mut Vec<String>) {
    if !matches!(ident, "AB" | "AW" | "AE") {
        return;
    }
    let mut expanded = Vec::with_capacity(values.len());
    for value in values.drain(..) {
        match parse_rect(&value) {
            Some(((x1, y1), (x2, y2))) => {
                // 端点为对角点，先归一化，再按行展开、行内自左向右
                //（与 FF4 规范示例 AB[aa:cc] = AB[aa][ba][ca][ab]... 一致）。
                let (x1, x2) = (x1.min(x2), x1.max(x2));
                let (y1, y2) = (y1.min(y2), y1.max(y2));
                for y in y1..=y2 {
                    for x in x1..=x2 {
                        expanded.push(format!("{}{}", (b'a' + x) as char, (b'a' + y) as char));
                    }
                }
            }
            None => expanded.push(value),
        }
    }
    *values = expanded;
}

/// 识别 `aa:cc` 形式的压缩矩形端点；非此写法返回 `None`。
fn parse_rect(value: &str) -> Option<((u8, u8), (u8, u8))> {
    let b = value.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return None;
    }
    let point = |p: [u8; 2]| -> Option<(u8, u8)> {
        (p[0].is_ascii_lowercase() && p[1].is_ascii_lowercase())
            .then(|| (p[0] - b'a', p[1] - b'a'))
    };
    Some((point([b[0], b[1]])?, point([b[3], b[4]])?))
}
