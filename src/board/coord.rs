//! 坐标与棋盘尺寸。
//!
//! 内部坐标与外部表示的约定（务必区分两套字母表，规则不同）：
//!
//! - 内部坐标 [`Coord`]：`(x, y)`，`x` 为列（0 起，自左向右），
//!   `y` 为行（**0 起自上向下**，与 SGF 行序一致）。
//! - GTP 坐标（阶段 3 与 KataGo 通信用）：列字母 **跳过 `I`**
//!   （`A..H` 之后直接 `J..T`，19 路恰好 19 个字母），行号自下向上
//!   从 1 开始。例：19 路天元为 `K10`，`Q16`、`D4` 均为合法坐标。
//! - SGF 坐标（阶段 5 存取档用）：两个小写字母，**不跳过 `i`**，
//!   依次为列、行，行自上向下递增。例：19 路天元为 `jj`，
//!   19 路列字母为 `a..s`，9 路为 `a..i`。
//!
//! 两套字母表仅"跳不跳 `I/i`"这一点不同，极易混淆；
//! 因此 GTP 与 SGF 的解析/格式化各自独立实现，不做相互复用。

use std::fmt;

/// 棋盘尺寸。仅支持 9 / 13 / 19 路，非法尺寸在构造期被拒绝，不会 panic。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Size(u8);

impl Size {
    /// 创建尺寸。非 9 / 13 / 19 时返回 [`CoordError::UnsupportedSize`]。
    pub fn new(n: u8) -> Result<Self, CoordError> {
        match n {
            9 | 13 | 19 => Ok(Self(n)),
            _ => Err(CoordError::UnsupportedSize(n)),
        }
    }

    /// 边长（路数）。
    pub fn n(self) -> u8 {
        self.0
    }

    /// 交叉点总数。
    pub fn point_count(self) -> usize {
        self.0 as usize * self.0 as usize
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}路", self.0)
    }
}

/// 内部坐标。不绑定尺寸（`Copy` 且可跨棋盘复用），
/// 因此涉及具体棋盘的方法都显式传入 [`Size`]。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Coord {
    /// 列，0 起，自左向右。
    x: u8,
    /// 行，0 起，自上向下。
    y: u8,
}

impl Coord {
    /// 按尺寸构造坐标；越界时返回 `None`。
    pub fn new(size: Size, x: u8, y: u8) -> Option<Self> {
        (x < size.n() && y < size.n()).then_some(Self { x, y })
    }

    /// 列（0 起，自左向右）。
    pub fn x(self) -> u8 {
        self.x
    }

    /// 行（0 起，自上向下）。
    pub fn y(self) -> u8 {
        self.y
    }

    /// 是否落在该尺寸的棋盘上。
    pub fn on_board(self, size: Size) -> bool {
        self.x < size.n() && self.y < size.n()
    }

    /// 一维索引（按行优先），用于 `Vec<Option<Stone>>` 棋盘存储。
    pub fn index(self, size: Size) -> usize {
        self.y as usize * size.n() as usize + self.x as usize
    }

    /// 由一维索引还原坐标；越界返回 `None`。
    pub fn from_index(size: Size, index: usize) -> Option<Self> {
        (index < size.point_count()).then(|| Self {
            x: (index % size.n() as usize) as u8,
            y: (index / size.n() as usize) as u8,
        })
    }

    /// 上下左右四个相邻点，自动剔除盘外。
    pub fn neighbors(self, size: Size) -> impl Iterator<Item = Coord> {
        let n = size.n() as i16;
        let (x, y) = (self.x as i16, self.y as i16);
        [(x, y - 1), (x + 1, y), (x, y + 1), (x - 1, y)]
            .into_iter()
            .filter_map(move |(nx, ny)| {
                if nx >= 0 && ny >= 0 && nx < n && ny < n {
                    Some(Self { x: nx as u8, y: ny as u8 })
                } else {
                    None
                }
            })
    }

    /// 解析 GTP 坐标（如 `Q16`、`d4`，大小写不敏感，列字母跳过 `I`）。
    pub fn from_gtp(size: Size, s: &str) -> Result<Self, CoordError> {
        let s = s.trim();
        let bytes = s.as_bytes();
        if bytes.len() < 2 {
            return Err(CoordError::BadFormat);
        }
        // GTP 列字母跳过 I，I 单独拒绝以便用户察觉写法问题。
        let x = match bytes[0].to_ascii_uppercase() {
            b'A'..=b'H' => bytes[0].to_ascii_uppercase() - b'A',
            b'J'..=b'T' => bytes[0].to_ascii_uppercase() - b'A' - 1,
            _ => return Err(CoordError::BadFormat),
        };
        if x >= size.n() {
            return Err(CoordError::OffBoard);
        }
        let row: u8 = s[1..]
            .trim()
            .parse()
            .map_err(|_| CoordError::BadFormat)?;
        if row == 0 || row > size.n() {
            return Err(CoordError::OffBoard);
        }
        // GTP 行号自下向上，内部 y 自上向下。
        Ok(Self { x, y: size.n() - row })
    }

    /// 格式化为 GTP 坐标（大写列字母，行号自下向上）。
    pub fn to_gtp(self, size: Size) -> String {
        let letter = if self.x < 8 {
            b'A' + self.x
        } else {
            // 跳过 I。
            b'A' + self.x + 1
        };
        format!("{}{}", letter as char, size.n() - self.y)
    }

    /// 解析 SGF 坐标（两个小写字母，不跳过 `i`，行自上向下）。
    /// 空串 / 非两点（如弃着的 `"tt"` 语义）不在本函数处理范围。
    pub fn from_sgf(size: Size, s: &str) -> Result<Self, CoordError> {
        let bytes = s.as_bytes();
        if bytes.len() != 2
            || !bytes[0].is_ascii_lowercase()
            || !bytes[1].is_ascii_lowercase()
        {
            return Err(CoordError::BadFormat);
        }
        let (x, y) = (bytes[0] - b'a', bytes[1] - b'a');
        if x >= size.n() || y >= size.n() {
            return Err(CoordError::OffBoard);
        }
        Ok(Self { x, y })
    }

    /// 格式化为 SGF 坐标（不跳过 `i`，行自上向下，与内部行序一致）。
    pub fn to_sgf(self) -> String {
        format!("{}{}", (b'a' + self.x) as char, (b'a' + self.y) as char)
    }
}

impl fmt::Display for Coord {
    /// 以 GTP 形式显示；但没有尺寸时无法算出行号，
    /// 这里按 19 路显示仅用于日志等非关键场合，程序逻辑请用 [`Coord::to_gtp`]。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let letter = if self.x < 8 { b'A' + self.x } else { b'A' + self.x + 1 };
        write!(f, "{}({},{})", letter as char, self.x, self.y)
    }
}

/// 坐标与尺寸相关的错误。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoordError {
    /// 不支持的棋盘尺寸（仅支持 9 / 13 / 19）。
    UnsupportedSize(u8),
    /// 坐标超出棋盘范围。
    OffBoard,
    /// 外部坐标字符串格式非法（含 GTP 中被禁用的字母 `I`）。
    BadFormat,
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSize(n) => {
                write!(f, "不支持的棋盘尺寸：{n}（仅支持 9 / 13 / 19 路）")
            }
            Self::OffBoard => write!(f, "坐标超出棋盘范围"),
            Self::BadFormat => write!(f, "坐标格式非法"),
        }
    }
}

impl std::error::Error for CoordError {}
