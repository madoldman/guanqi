//! SGF 词法层：字符光标、跳白与属性值转义的解码 / 编码。
//!
//! 只做字符级扫描，不理解节点与游戏树结构（语法在 `mod.rs`）。
//!
//! # 属性值转义（FF4 Text 口径，宽松执行）
//!
//! - `\]` → `]`，`\\` → `\`；
//! - 其它转义（含规范未定义的 `\x`）取字符本身，不报错；
//! - `\` + 换行（含 `\` + `\r\n`）是软换行，整体删除；
//! - 值内的裸换行原样保留，由上层按需整理。
//!
//! 序列化侧 [`escape_value`] 只转义 `\` 与 `]`，与 [`Cursor::read_value`]
//! 互逆，是"写出后重解析结果不变"的基础。

use super::error::{Position, SgfError, SgfErrorKind};

/// 文本光标：在 `&str` 上按字符推进，可汇报当前位置（行列从 1 计）。
pub(crate) struct Cursor<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    /// 当前位置：字节偏移 + 行列（按字符统计，多字节字符记 1 列）。
    pub(crate) fn position(&self) -> Position {
        let before = &self.input[..self.pos];
        let line = 1 + before.matches('\n').count();
        let col = match before.rfind('\n') {
            Some(i) => before[i + 1..].chars().count() + 1,
            None => before.chars().count() + 1,
        };
        Position { offset: self.pos, line, col }
    }

    /// 窥视当前字符。
    pub(crate) fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    /// 跳过空白后窥视当前字符（属性名与值之间的空白无意义）。
    pub(crate) fn peek_after_ws(&mut self) -> Option<char> {
        self.skip_ws();
        self.peek()
    }

    /// 消费并返回当前字符；到末尾返回 `None`。
    pub(crate) fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// 跳过 SGF 空白（空格、制表、回车、换行，均为单字节字符）。
    pub(crate) fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r' | '\n')) {
            self.pos += 1;
        }
    }

    /// 消费一个指定字符；不符时返回带位置的结构化错误。
    pub(crate) fn expect(&mut self, want: char, expected: &'static str) -> Result<(), SgfError> {
        match self.peek() {
            Some(c) if c == want => {
                self.pos += want.len_utf8();
                Ok(())
            }
            found => Err(SgfError::new(
                SgfErrorKind::UnexpectedByte { expected, found },
                Some(self.position()),
            )),
        }
    }

    /// 读取属性名：连续 ASCII 字母（调用方保证当前字符是字母），
    /// 统一转为大写以便查找。
    pub(crate) fn read_ident(&mut self) -> String {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        self.input[start..self.pos].to_ascii_uppercase()
    }

    /// 读取一个属性值：消费 `[...]`，返回转义解码后的内容。
    pub(crate) fn read_value(&mut self) -> Result<String, SgfError> {
        self.expect('[', "属性值开头的“[”")?;
        let mut out = String::new();
        loop {
            match self.bump() {
                Some(']') => return Ok(out),
                Some('\\') => match self.bump() {
                    // 软换行：反斜杠 + 换行（含 CRLF）整体删除。
                    Some('\n') => {}
                    Some('\r') => {
                        if self.peek() == Some('\n') {
                            self.pos += 1;
                        }
                    }
                    // 其余转义（`\]`、`\\` 及未知转义）取字符本身。
                    Some(escaped) => out.push(escaped),
                    None => break,
                },
                Some(c) => out.push(c),
                None => break,
            }
        }
        Err(SgfError::new(
            SgfErrorKind::UnexpectedByte { expected: "属性值结尾的“]”", found: None },
            Some(self.position()),
        ))
    }
}

/// 序列化用：把解码后的值重新转义并追加到输出。
/// 只转义 `\` 与 `]`（换行原样写出），与 [`Cursor::read_value`] 互逆。
pub(crate) fn escape_value(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ']' => out.push_str("\\]"),
            _ => out.push(c),
        }
    }
}
