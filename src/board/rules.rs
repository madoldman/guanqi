//! 规则核心：连通块、气、提子与落子合法性（自杀 / 打劫）。
//!
//! 本模块只操作"裸盘面"（`Vec<Option<Stone>>`），
//! 不含历史与手数概念；劫争需要回看历史盘面，由 [`super::Board`] 负责。
//!
//! 规则边界（本期实现范围）：
//! - 落子后**先提对方无气块，再判自杀**；多个对方块同时无气时一并提取。
//! - 气（liberty）以**连通块为单位共享**：同块的相邻空点只计一次。
//! - 打劫采用**简单劫**：仅禁止"落子后盘面回到上一手之前的盘面"
//!   （即不得立即回提形成单步循环）。**超级劫（全局同形禁止）本期不实现**，
//!   长循环、三劫循环等均不判禁。
//! - 不实现双活、禁全局同形、终局计子等规则。

use super::coord::{Coord, Size};

/// 棋子（兼作行棋方标识：围棋只有黑白两色）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Stone {
    /// 黑方。
    Black,
    /// 白方。
    White,
}

impl Stone {
    /// 对方颜色。
    pub fn opposite(self) -> Self {
        match self {
            Self::Black => Self::White,
            Self::White => Self::Black,
        }
    }

    /// 用户可读名称。
    pub fn name(self) -> &'static str {
        match self {
            Self::Black => "黑",
            Self::White => "白",
        }
    }
}

/// 一次连通块扫描的结果：块内棋子与块的气（均已去重）。
///
/// 注意气的语义：**整块共享气**，[`ChainInfo::liberties`] 是全块气点的并集，
/// 不是单子的气。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChainInfo {
    /// 块内所有棋子坐标。
    pub stones: Vec<Coord>,
    /// 块的所有气点（相邻空点，已去重）。
    pub liberties: Vec<Coord>,
}

impl ChainInfo {
    /// 块的气数。
    pub fn liberty_count(&self) -> usize {
        self.liberties.len()
    }
}

/// 落子非法的原因（信息可直接显示给用户）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IllegalReason {
    /// 该点已有棋子。
    Occupied,
    /// 自杀：落子（且未提对方子）后自身连通块无气。
    Suicide,
    /// 简单劫：落子后盘面回到上一手之前的盘面（不得立即回提）。
    Ko,
    /// 坐标不在当前棋盘上。
    OffBoard,
    /// 游标处于历史回看中：只有在最新一手之后才能落子
    /// （变着分支留待阶段 5，见 [`super`] 模块文档）。
    NotAtLatestMove,
}

impl std::fmt::Display for IllegalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Occupied => write!(f, "该点已有棋子"),
            Self::Suicide => write!(f, "禁着点：落子后自身无气（自杀）"),
            Self::Ko => write!(f, "打劫：不得立即回提，需先在别处落子"),
            Self::OffBoard => write!(f, "坐标超出棋盘"),
            Self::NotAtLatestMove => write!(f, "正在回看历史，不能在历史局面落子"),
        }
    }
}

impl std::error::Error for IllegalReason {}

/// 计算包含 `start` 的连通块及其全部气。
///
/// `grid` 按行优先存储盘面（索引见 [`Coord::index`]）。
/// `start` 为空点时返回 `None`（空点没有连通块）。
pub fn chain_info(size: Size, grid: &[Option<Stone>], start: Coord) -> Option<ChainInfo> {
    let color = grid[start.index(size)]?;
    let mut stones = Vec::new();
    let mut liberties = Vec::new();
    // 去重标记：同一条气、同一颗子只入列一次。
    let mut seen_stone = vec![false; size.point_count()];
    let mut seen_liberty = vec![false; size.point_count()];

    seen_stone[start.index(size)] = true;
    let mut stack = vec![start];
    while let Some(c) = stack.pop() {
        stones.push(c);
        for nb in c.neighbors(size) {
            match grid[nb.index(size)] {
                Some(s) if s == color => {
                    if !seen_stone[nb.index(size)] {
                        seen_stone[nb.index(size)] = true;
                        stack.push(nb);
                    }
                }
                None => {
                    if !seen_liberty[nb.index(size)] {
                        seen_liberty[nb.index(size)] = true;
                        liberties.push(nb);
                    }
                }
                Some(_) => {}
            }
        }
    }
    Some(ChainInfo { stones, liberties })
}

/// 在 `grid` 上落子并结算：先提取所有无气的对方连通块（可多块同提），
/// 再判自杀。
///
/// - 合法：就地修改 `grid`，返回被提走的对方子坐标（供提子计数与显示）。
/// - 非法（已占用 / 自杀）：**恢复 `grid` 原状**后返回错误。
///
/// 劫争不在此判定（需历史盘面），见 [`super::Board`]。
pub fn apply_move(
    size: Size,
    grid: &mut [Option<Stone>],
    player: Stone,
    at: Coord,
) -> Result<Vec<Coord>, IllegalReason> {
    if !at.on_board(size) {
        return Err(IllegalReason::OffBoard);
    }
    if grid[at.index(size)].is_some() {
        return Err(IllegalReason::Occupied);
    }

    grid[at.index(size)] = Some(player);

    // 提对方无气块。四个邻居可能属于同一块，用 seen 去重，避免重复扫描
    // 或把同一块提两次。
    let mut seen = vec![false; size.point_count()];
    let opponent = player.opposite();
    let mut to_remove = Vec::new();
    for nb in at.neighbors(size) {
        let idx = nb.index(size);
        if seen[idx] || grid[idx] != Some(opponent) {
            continue;
        }
        if let Some(info) = chain_info(size, grid, nb) {
            for &s in &info.stones {
                seen[s.index(size)] = true;
            }
            if info.liberties.is_empty() {
                to_remove.extend(info.stones);
            }
        }
    }
    for &c in &to_remove {
        grid[c.index(size)] = None;
    }
    let captured = to_remove;

    // 自杀：未提任何子，且落子后自身块无气。
    // 提子后自身块可能由"无气"变"有气"（经典陷阱），故必须先提再判。
    if captured.is_empty() {
        let dead = chain_info(size, grid, at).is_some_and(|info| info.liberties.is_empty());
        if dead {
            grid[at.index(size)] = None;
            return Err(IllegalReason::Suicide);
        }
    }
    Ok(captured)
}
