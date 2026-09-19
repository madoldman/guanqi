//! SGF 错误类型：结构化原因 + 位置信息。

use std::fmt;

/// 错误在解析文本中的位置（行列从 1 计，偏移为字节下标）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Position {
    /// 距文本开头的字节偏移。
    pub offset: usize,
    /// 行号，从 1 起。
    pub line: usize,
    /// 列号（按字符计），从 1 起。
    pub col: usize,
}

/// 解析 / 取值失败的原因。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SgfErrorKind {
    /// 输入为空（没有任何游戏树）。
    EmptyInput,
    /// 期待特定内容时遇到别的字符；`found` 为 `None` 表示输入提前结束。
    UnexpectedByte {
        /// 期待内容的描述（用于错误消息）。
        expected: &'static str,
        /// 实际遇到的字符；到输入末尾则为 `None`。
        found: Option<char>,
    },
    /// 空游戏树：一对括号内没有任何节点。
    EmptyGameTree,
    /// 属性缺少属性值（如 `;PB` 后没有 `[...]`）。
    PropNeedsValue {
        /// 出错的属性名。
        ident: String,
    },
    /// 非 UTF-8 输入，且根节点声明了其它字符集（如 GB2312）。
    /// 携带声明的字符集名，供 UI 提示用户转存为 UTF-8。
    UnsupportedCharset(String),
    /// 非 UTF-8 输入，且未声明其它字符集（或声明即 UTF-8）。
    InvalidUtf8,
    /// 矩形棋盘 `SZ[宽:高]`——本项目仅支持方形棋盘。
    NonSquareBoard {
        /// 声明的列数。
        width: u8,
        /// 声明的行数。
        height: u8,
    },
    /// 方形但非 9 / 13 / 19 路。
    UnsupportedBoardSize(u8),
    /// 游戏类型不是围棋（`GM` 存在且不为 1）。
    BadGameType(u8),
    /// 数值属性（`GM` / `FF` / `SZ` / `KM` / `HA`）无法解析。
    BadNumber {
        /// 出错的属性名。
        ident: String,
        /// 原始值。
        value: String,
    },
    /// 坐标值无法解析或超出棋盘（弃着的空值与 `tt` 除外）。
    BadCoord {
        /// 出错的属性名。
        ident: String,
        /// 原始值。
        value: String,
    },
}

impl fmt::Display for SgfErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInput => write!(f, "输入为空：未找到任何 SGF 游戏树"),
            Self::UnexpectedByte { expected, found } => match found {
                None => write!(f, "应为{expected}，但输入在此提前结束"),
                Some(c) => write!(f, "应为{expected}，但遇到意外内容 {c:?}"),
            },
            Self::EmptyGameTree => {
                write!(f, "空的 SGF 游戏树（一对括号内没有任何节点）")
            }
            Self::PropNeedsValue { ident } => {
                write!(f, "属性 {ident} 缺少属性值（[...]）")
            }
            Self::UnsupportedCharset(name) => write!(
                f,
                "文件不是合法 UTF-8，且声明使用字符集 {name}；请转存为 UTF-8 后再打开"
            ),
            Self::InvalidUtf8 => write!(
                f,
                "文件不是合法 UTF-8 编码，且未声明其它字符集；请转存为 UTF-8 后再打开"
            ),
            Self::NonSquareBoard { width, height } => write!(
                f,
                "不支持的矩形棋盘 {width}×{height}（本项目仅支持方形 9 / 13 / 19 路棋盘）"
            ),
            Self::UnsupportedBoardSize(n) => {
                write!(f, "不支持的棋盘尺寸：{n}（仅支持 9 / 13 / 19 路）")
            }
            Self::BadGameType(n) => {
                write!(f, "不支持的 SGF 游戏类型 GM[{n}]（仅支持围棋 GM[1]）")
            }
            Self::BadNumber { ident, value } => {
                write!(f, "属性 {ident} 的值“{value}”不是合法数字")
            }
            Self::BadCoord { ident, value } => {
                write!(f, "属性 {ident} 的坐标“{value}”无法解析或超出棋盘范围")
            }
        }
    }
}

/// SGF 解析 / 取值错误：原因 + 位置（部分语义错误无位置）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SgfError {
    /// 失败原因。
    pub kind: SgfErrorKind,
    /// 文本中的位置；语义类错误（棋盘尺寸、字符集等）为 `None`。
    pub position: Option<Position>,
}

impl SgfError {
    pub(crate) fn new(kind: SgfErrorKind, position: Option<Position>) -> Self {
        Self { kind, position }
    }
}

impl fmt::Display for SgfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.position {
            Some(pos) => write!(f, "{}（第 {} 行第 {} 列）", self.kind, pos.line, pos.col),
            None => write!(f, "{}", self.kind),
        }
    }
}

impl std::error::Error for SgfError {}
