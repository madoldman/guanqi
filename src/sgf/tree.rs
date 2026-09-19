//! SGF 游戏树：数据结构、序列化与属性访问器。
//!
//! # 表示约定（"写出后重解析结果不变"的前提）
//!
//! - 属性值保存解码后的语义值（转义已还原、软换行已删除），
//!   写出时经词法层重新转义；
//! - 属性按文档出现顺序、同名多值按原顺序保留，序列化不重排；
//! - 未知属性原样保存（属性名统一大写），写出不丢数据；
//! - `AB` / `AW` / `AE` 的压缩矩形已在解析期展开（见 `mod.rs`），
//!   树内不再出现矩形写法。

use std::fmt;

use super::error::{SgfError, SgfErrorKind};
use super::lexer::escape_value;
use crate::board::{Action, Coord, Size, Stone};

/// 一个属性：属性名（统一大写）+ 解码后的值序列（同名多值按原顺序）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Property {
    /// 属性名，如 `B`、`AB`。
    pub ident: String,
    /// 全部值（解码后内容）：坐标形如 `aa`，注释为多行文本等。
    pub values: Vec<String>,
}

/// 一个节点：按出现顺序排列的属性集合（空节点允许，宽容真实文件）。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Node {
    /// 属性列表，按文档出现顺序。
    pub props: Vec<Property>,
}

/// 游戏树：节点序列 + 末节点分出的子树（变着分支）。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct GameTree {
    /// 本层主链上的节点，依次相接。
    pub nodes: Vec<Node>,
    /// 从末节点分出的各分支；每个子树以自己的节点开头。
    pub children: Vec<GameTree>,
}

impl GameTree {
    /// 根节点（解析产物至少有一个节点；手工构造的空树为 `None`）。
    pub fn root(&self) -> Option<&Node> {
        self.nodes.first()
    }

    /// 主变着链上的全部节点：本层 `nodes` 之后沿每层第一个子树递归。
    pub fn mainline(&self) -> Vec<&Node> {
        let mut out: Vec<&Node> = self.nodes.iter().collect();
        let mut tree = self;
        while let Some(first) = tree.children.first() {
            out.extend(first.nodes.iter());
            tree = first;
        }
        out
    }

    /// 序列化为 SGF 文本（顶层带一对括号；变着分支按原结构嵌套写出）。
    pub fn write(&self) -> String {
        let mut out = String::new();
        self.write_tree(&mut out);
        out
    }

    fn write_tree(&self, out: &mut String) {
        out.push('(');
        for node in &self.nodes {
            node.write_node(out);
        }
        for child in &self.children {
            child.write_tree(out);
        }
        out.push(')');
    }
}

impl fmt::Display for GameTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.write())
    }
}

impl Node {
    fn write_node(&self, out: &mut String) {
        out.push(';');
        for prop in &self.props {
            out.push_str(&prop.ident);
            for value in &prop.values {
                out.push('[');
                escape_value(value, out);
                out.push(']');
            }
        }
    }

    /// 取第一个同名属性（参数大小写不敏感）。
    pub fn get(&self, ident: &str) -> Option<&Property> {
        let ident = ident.to_ascii_uppercase();
        self.props.iter().find(|p| p.ident == ident)
    }

    /// 单值文本属性的第一个值（原样返回，不整理空白）。
    pub fn text(&self, ident: &str) -> Option<&str> {
        self.get(ident)
            .and_then(|p| p.values.first())
            .map(String::as_str)
    }

    /// 节点注释（`C`），首尾空白已整理。
    pub fn comment(&self) -> Option<&str> {
        self.text("C").map(str::trim)
    }

    /// 节点名（`N`），首尾空白已整理。
    pub fn name(&self) -> Option<&str> {
        self.text("N").map(str::trim)
    }

    /// 解读行棋属性（`B` / `W`，取先出现者）。
    /// 空值与旧写法 `tt` 均按弃着；其余按 SGF 坐标解析，
    /// 非法或越界时返回 [`SgfErrorKind::BadCoord`]。
    pub fn move_action(&self, size: Size) -> Result<Option<(Stone, Action)>, SgfError> {
        for prop in &self.props {
            let stone = match prop.ident.as_str() {
                "B" => Stone::Black,
                "W" => Stone::White,
                _ => continue,
            };
            let value = prop.values.first().map(String::as_str).unwrap_or("");
            let action = match value {
                "" | "tt" => Action::Pass,
                _ => Coord::from_sgf(size, value)
                    .map(Action::Place)
                    .map_err(|_| bad_coord(&prop.ident, value))?,
            };
            return Ok(Some((stone, action)));
        }
        Ok(None)
    }

    /// 取摆放属性（`AB` / `AW` / `AE`）的全部坐标。
    /// 压缩矩形已在解析期展开，此处只做逐值坐标解析与校验。
    pub fn coords(&self, ident: &str, size: Size) -> Result<Vec<Coord>, SgfError> {
        let mut out = Vec::new();
        if let Some(prop) = self.get(ident) {
            for value in &prop.values {
                out.push(
                    Coord::from_sgf(size, value).map_err(|_| bad_coord(&prop.ident, value))?,
                );
            }
        }
        Ok(out)
    }
}

/// 构造坐标类结构化错误。
fn bad_coord(ident: &str, value: &str) -> SgfError {
    SgfError::new(
        SgfErrorKind::BadCoord {
            ident: ident.to_string(),
            value: value.to_string(),
        },
        None,
    )
}

// ---- 对局信息（根节点常用属性汇总） ----

/// 从根节点提取的对局信息；未提供的属性为 `None`（文本取首个值并
/// 整理首尾空白）。
#[derive(Clone, PartialEq, Debug)]
pub struct GameInfo {
    /// 棋盘尺寸（`SZ`，缺省 19 路）。
    pub size: Size,
    /// 贴目（`KM`）。
    pub komi: Option<f64>,
    /// 让子数（`HA`，缺省 0）。
    pub handicap: u8,
    /// 首着方（`PL`；缺失或非法为 `None`）。
    pub initial_player: Option<Stone>,
    /// 黑 / 白棋手（`PB` / `PW`）、段位（`BR` / `WR`）、队名（`BT` / `WT`）。
    pub player_black: Option<String>,
    pub player_white: Option<String>,
    pub rank_black: Option<String>,
    pub rank_white: Option<String>,
    pub team_black: Option<String>,
    pub team_white: Option<String>,
    /// 结果 / 日期 / 赛事 / 轮次 / 地点 / 对局名
    /// （`RE` / `DT` / `EV` / `RO` / `PC` / `GN`）。
    pub result: Option<String>,
    pub date: Option<String>,
    pub event: Option<String>,
    pub round: Option<String>,
    pub place: Option<String>,
    pub game_name: Option<String>,
    /// 规则 / 时限 / 加时 / 录入程序 / 字符集 / SGF 版本
    /// （`RU` / `TM` / `OT` / `AP` / `CA` / `FF`）。
    pub rules: Option<String>,
    pub time_limit: Option<String>,
    pub overtime: Option<String>,
    pub application: Option<String>,
    pub charset: Option<String>,
    pub format: Option<u8>,
    /// 根节点注释（`C`，首尾空白已整理）。
    pub root_comment: Option<String>,
}

impl GameInfo {
    /// 汇总根节点属性。`GM` / `FF` / `SZ` / `KM` / `HA` 有值但非法时
    /// 报结构化错误；其余属性尽力提取，缺失、空串记为 `None`。
    pub fn from_tree(tree: &GameTree) -> Result<Self, SgfError> {
        let root = tree
            .root()
            .ok_or_else(|| SgfError::new(SgfErrorKind::EmptyGameTree, None))?;
        let number = |ident: &str| -> Result<Option<u8>, SgfError> {
            match root.text(ident).map(str::trim) {
                None => Ok(None),
                Some(v) => v.parse::<u8>().map(Some).map_err(|_| bad_number(ident, v)),
            }
        };
        if let Some(gm) = number("GM")?
            && gm != 1
        {
            return Err(SgfError::new(SgfErrorKind::BadGameType(gm), None));
        }
        let format = number("FF")?;
        let size = parse_size(root)?;
        let komi = match root.text("KM").map(str::trim) {
            None => None,
            Some(v) => v.parse::<f64>().map(Some).map_err(|_| bad_number("KM", v))?,
        };
        let handicap = number("HA")?.unwrap_or(0);
        let initial_player = match root.text("PL").map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("b") => Some(Stone::Black),
            Some(v) if v.eq_ignore_ascii_case("w") => Some(Stone::White),
            _ => None,
        };
        let s = |ident: &str| -> Option<String> {
            root.text(ident)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        Ok(Self {
            size,
            komi,
            handicap,
            initial_player,
            player_black: s("PB"),
            player_white: s("PW"),
            rank_black: s("BR"),
            rank_white: s("WR"),
            team_black: s("BT"),
            team_white: s("WT"),
            result: s("RE"),
            date: s("DT"),
            event: s("EV"),
            round: s("RO"),
            place: s("PC"),
            game_name: s("GN"),
            rules: s("RU"),
            time_limit: s("TM"),
            overtime: s("OT"),
            application: s("AP"),
            charset: s("CA"),
            format,
            root_comment: root.comment().filter(|v| !v.is_empty()).map(str::to_owned),
        })
    }
}

/// 解析 `SZ`：支持 `SZ[19]` 与 `SZ[19:19]`；缺省 19 路（FF4 默认值）。
/// 非正方形与不支持的路数分别报 `NonSquareBoard` / `UnsupportedBoardSize`。
fn parse_size(root: &Node) -> Result<Size, SgfError> {
    let Some(raw) = root.text("SZ").map(str::trim) else {
        // FF4 缺省 19 路；常量构造不可能失败，错误分支只为不 panic。
        return Size::new(19)
            .map_err(|_| SgfError::new(SgfErrorKind::UnsupportedBoardSize(19), None));
    };
    let (w, h) = match raw.split_once(':') {
        None => (raw, raw),
        Some((a, b)) => (a.trim(), b.trim()),
    };
    let (Ok(w), Ok(h)) = (w.parse::<u8>(), h.parse::<u8>()) else {
        return Err(bad_number("SZ", raw));
    };
    if w != h {
        return Err(SgfError::new(
            SgfErrorKind::NonSquareBoard { width: w, height: h },
            None,
        ));
    }
    Size::new(w).map_err(|_| SgfError::new(SgfErrorKind::UnsupportedBoardSize(w), None))
}

/// 构造数值类结构化错误。
fn bad_number(ident: &str, value: &str) -> SgfError {
    SgfError::new(
        SgfErrorKind::BadNumber {
            ident: ident.to_string(),
            value: value.to_string(),
        },
        None,
    )
}
