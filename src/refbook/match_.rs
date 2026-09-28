//! 局面键计算与匹配查询（参照模块的核心口径）。
//!
//! 索引侧（`walk`）与查询侧共用 [`all_cluster_keys`] / [`key_of_cluster`]：
//! 两侧键字节完全同构，`PositionKey` 是 `Vec<u8>` 结构体键，`HashMap` 相等
//! 判定即逐字节相等，**无哈希碰撞问题**（不做 u64 压缩的理由见
//! `walk::PositionKey` 文档）。

use super::entry::RefEntry;
use super::walk::{PositionKey, Rect};
use crate::board::rules::{self, Stone};
use crate::board::{Action, Board, Coord, Size};

/// 簇聚类与区域外扩的边距（交叉点数）。
///
/// 取 2 的理由：定式 / 死活的局部形状里，最宽的常见间隔（跳 / 飞）为
/// 2 路，边距 2 能把同一形状的各变化兜进同一簇；再大会把相邻定式并成
/// 一簇（区域虚大、键变长），过小会把一个定式拆成两簇（都匹不上）。
pub const MARGIN: u8 = 2;

/// 两点切比雪夫距离是否 ≤ 2×MARGIN（簇聚类判定）。
fn close(ax: u8, ay: u8, bx: u8, by: u8) -> bool {
    ax.abs_diff(bx) <= 2 * MARGIN && ay.abs_diff(by) <= 2 * MARGIN
}

/// 一条库内续法（子节点着法）。
#[derive(Debug)]
pub struct Continuation {
    /// 行棋方。
    pub player: Stone,
    /// 着点（弃着为 `None`），GTP 坐标文本。
    pub at: Option<String>,
}

impl Continuation {
    /// 展示串：「黑 D4」/「白弃着」。
    pub fn text(&self) -> String {
        match self.at {
            Some(ref at) => format!("{} {at}", self.player.name()),
            None => format!("{} 弃着", self.player.name()),
        }
    }
}

/// 一条偏离信息（父局面在库内、实战刚走的一手不在其续法中）。
#[derive(Debug)]
pub struct Deviation {
    /// 实战刚走的一手（「白 E15」形态，弃着不触发偏离）。
    pub played: String,
    /// 库中该局面的续法列表（展示串）。
    pub book: Vec<String>,
}

/// 一次查询的结果。
///
/// 「偏离」与「命中」并列而非嵌套：走偏之后的局面必然不在库中（除非
/// 转置），若把偏离作为 `Hit` 的附加字段，则偏离提示永不触发。故判定
/// 口径（与任务书「当前局面命中参照、但实战刚走的那一手不在参照续法中」
/// 对齐到可实现语义）：
///
/// - `Hit`：当前局面在参照中有同形（deviation 字段再查父局面，在库内
///   行棋时不提示）；
/// - `Deviation`：当前局面不在库中，但**父局面**命中、且实战最后一手
///   不在其库内续法中（含落点守卫，见 `deviation_of`）——这正是「库中
///   走 A / B，实战走了 C」的提示场景；
/// - `Miss`：当前与父局面都无库内同形（或已完全离开库内路径）。
pub enum LookupOutcome<'a> {
    /// 当前局面命中参照。
    Hit {
        /// 命中的索引条目（库内注释 / 续法 / 手数 / 位置）。
        entry: &'a RefEntry,
        /// 当前局面的着法尾部（与条目口径一致，UI 并排展示「实战 vs 库」）。
        tail: Vec<(Stone, String)>,
        /// 偏离提示：`Some` = 实战刚走的那一手不在父局面的库内续法中
        /// 但当前局面仍命中（转置等达成的同形）；`None` = 在库内行棋。
        deviation: Option<String>,
    },
    /// 当前局面不在库中，但父局面命中且实战最后一手偏离了库内续法。
    Deviation(Deviation),
    /// 当前局面在参照里没有同形（含：当前盘面无子）。
    Miss,
    /// 参照尺寸与当前盘面不同（无法匹配，UI 应提示）。
    SizeMismatch {
        /// 参照尺寸。
        reference: Size,
        /// 当前盘面尺寸。
        current: Size,
    },
}

/// 盘面上的全部棋子（行优先序，稳定性利于测试与取证）。
fn stones_of(size: Size, grid: &[Option<Stone>]) -> Vec<(u8, u8, Stone)> {
    let mut out = Vec::new();
    for (idx, stone) in grid.iter().enumerate().take(size.point_count()) {
        if let Some(stone) = *stone {
            let c = Coord::from_index(size, idx).expect("grid 下标必在盘内");
            out.push((c.x(), c.y(), stone));
        }
    }
    out
}

/// 从剩余棋子中摘出一个簇（种子 = 剩余的第一个；任一点与簇内某点切比
/// 雪夫距离 ≤ 2×MARGIN 即反复并入，直到不再增长）。
///
/// 扩散口径让「一串间隔 ≤ 4 路的子」连成一体：定式谱里双方棋子交错，
/// 双方一并入同簇（键含双方分布），这正是「局部形状」的粒度。
fn take_cluster(all: &mut Vec<(u8, u8, Stone)>) -> Vec<(u8, u8, Stone)> {
    let seed = all.remove(0);
    let mut cluster = vec![seed];
    loop {
        let mut grew = false;
        let mut i = 0;
        while i < all.len() {
            if cluster
                .iter()
                .any(|&(x, y, _): &(u8, u8, Stone)| close(x, y, all[i].0, all[i].1))
            {
                let stone = all.remove(i);
                cluster.push(stone);
                grew = true;
            } else {
                i += 1;
            }
        }
        if !grew {
            break;
        }
    }
    cluster
}

/// 簇 → 比较区域：包围盒四周各扩 MARGIN，截断到盘内。
fn cluster_rect(size: Size, cluster: &[(u8, u8, Stone)]) -> Rect {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u8::MAX, u8::MAX, u8::MIN, u8::MIN);
    for &(x, y, _) in cluster {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    Rect {
        min_x: min_x.saturating_sub(MARGIN),
        min_y: min_y.saturating_sub(MARGIN),
        max_x: max_x.saturating_add(MARGIN).min(size.n() - 1),
        max_y: max_y.saturating_add(MARGIN).min(size.n() - 1),
    }
}

/// 簇 + 行棋方 → 局面键与比较区域。
///
/// 字节布局：`[区域宽, 区域高, 行棋方(0黑/1白), (x,y,色)*子数 …]`。
/// 坐标以**区域左上角为原点**（平移不变的核心）；棋子按行优先序写入
/// （两侧同序 ⇒ 键逐字节可比）。**不含尺寸 / 规则**（理由见模块文档）。
fn key_of_cluster(
    size: Size,
    cluster: &[(u8, u8, Stone)],
    to_play: Stone,
) -> (PositionKey, Rect) {
    let rect = cluster_rect(size, cluster);
    let mut key = Vec::with_capacity(3 + cluster.len() * 3);
    key.push(rect.w());
    key.push(rect.h());
    key.push(u8::from(matches!(to_play, Stone::Black)));
    let mut stones: Vec<(u8, u8, Stone)> = cluster
        .iter()
        .filter(|&&(x, y, _)| rect.contains(x, y))
        .map(|&(x, y, s)| (x - rect.min_x, y - rect.min_y, s))
        .collect();
    stones.sort_by_key(|&(x, y, _)| (x, y));
    for (x, y, s) in stones {
        key.push(x);
        key.push(y);
        key.push(u8::from(matches!(s, Stone::Black)));
    }
    (PositionKey(key), rect)
}

/// 一个簇的完整信息（键 + 比较区域 + 区域内棋子）。
pub(crate) struct ClusterKey {
    /// 局面键。
    pub key: PositionKey,
    /// 比较区域（盘上绝对坐标）。
    pub rect: Rect,
    /// 区域内的棋子（盘上绝对坐标）。
    pub inside: Vec<(u8, u8, Stone)>,
}

/// 盘面的**全部**簇键（种子按行优先序依次取，每簇聚完再取下一个种子）。
///
/// **索引侧与查询侧同用本函数**——这是「其他角有子不干扰命中」的正确
/// 实现：实战盘面上定式簇与散子簇各自成键，逐簇查表时定式簇命中与否
/// 与散子簇无关。曾用过「只取首个簇」的口径（种子 = 行优先第一子），
/// 但实战散子若在行优先序靠前的角上，首簇会是散子簇、定式漏匹配——
/// 故两侧都必须用全簇。真实局面簇数 ≤ 数个（每个连通局部一个），查询
/// 仍是近常数级。
pub(crate) fn all_cluster_keys(
    size: Size,
    grid: &[Option<Stone>],
    to_play: Stone,
) -> Vec<ClusterKey> {
    let mut stones = stones_of(size, grid);
    let mut out = Vec::new();
    while !stones.is_empty() {
        let cluster = take_cluster(&mut stones);
        let (key, rect) = key_of_cluster(size, &cluster, to_play);
        let inside: Vec<(u8, u8, Stone)> = cluster
            .into_iter()
            .filter(|&(x, y, _)| rect.contains(x, y))
            .collect();
        out.push(ClusterKey {
            key,
            rect,
            inside,
        });
    }
    out
}

/// 尾部着法（展示用）：当前线最后 [`super::TAIL_LEN`] 手（含本手）。
pub(crate) fn tail_of(board: &Board) -> Vec<(Stone, String)> {
    let size = board.size();
    let records = board.records();
    let start = records.len().saturating_sub(super::TAIL_LEN);
    records[start..]
        .iter()
        .map(|record| {
            let at = match record.action {
                Action::Pass => "弃着".to_owned(),
                Action::Place(c) => c.to_gtp(size),
            };
            (record.player, at)
        })
        .collect()
}

impl super::Reference {
    /// 在参照中查找当前局面（`board` 游标处）。
    ///
    /// 命中即给出库内注释 / 续法 / 手数 / 位置与偏离提示（口径见
    /// [`LookupOutcome::Hit`]）。查询 = 全簇键各一次哈希查表（簇数 ≤ 数个，
    /// 近常数级；偏离判定另含一次父局面重放，见 `deviation_of`）。
    pub fn lookup(&self, board: &Board) -> LookupOutcome<'_> {
        if board.size() != self.size {
            return LookupOutcome::SizeMismatch {
                reference: self.size,
                current: board.size(),
            };
        }
        if let Some((entry, _region)) = self.lookup_grid(board.grid(), board.to_play()) {
            // 当前局面命中：若实战最后一手已偏离父局面续法（经由转置等
            // 回到库内同形的路径），同样给出提示（一般不会发生，转置是
            // 唯一回库路径）。
            let deviation = self
                .check_deviation(board)
                .map(|d| {
                    format!(
                        "实战走 {}，库中此局面走：{}。",
                        d.played,
                        d.book.join(" / ")
                    )
                });
            return LookupOutcome::Hit {
                entry,
                tail: tail_of(board),
                deviation,
            };
        }
        // 当前局面不在库中：检查是否刚走偏（父局面命中 + 最后一手偏离）。
        if let Some(deviation) = self.check_deviation(board) {
            return LookupOutcome::Deviation(deviation);
        }
        LookupOutcome::Miss
    }

    /// 盘面（+行棋方）→ 逐簇查表的首个命中（条目 + 该簇的比较区域）。
    /// 索引查询与偏离判定的父局面查询共用（键口径单点保证）。
    ///
    /// 返回区域的原因：偏离提示必须知道**命中是由哪个局部簇决定的**
    /// （多簇局面下不同簇可命中不同节点），见 `deviation_of`。
    pub(super) fn lookup_grid(
        &self,
        grid: &[Option<Stone>],
        to_play: Stone,
    ) -> Option<(&RefEntry, Rect)> {
        all_cluster_keys(self.size, grid, to_play)
            .iter()
            .find_map(|cluster| self.index.get(&cluster.key).map(|entry| (entry, cluster.rect)))
    }

    /// 偏离判定（口径见模块级 [`LookupOutcome`] 文档）：父局面命中参照、
    /// 实战最后一手 ∉ 父局面的库内续法 → 偏离信息。
    ///
    /// **落点守卫**：实战最后一手的落点必须落在父局面命中簇的比较区域
    /// 内才对比——最后一手落在区域外说明它属于另一局部（实战其它角的
    /// 棋），与该命中的库内续法无关，硬比只会产生「实战走右上角、库中
    /// 走左下定式」这类驴唇不对马嘴的提示。
    fn check_deviation(&self, board: &Board) -> Option<Deviation> {
        let records = board.records();
        let last = records.last()?;
        if last.is_pass() {
            return None;
        }
        let at = match last.action {
            Action::Place(c) => c,
            Action::Pass => return None,
        };
        // 父局面（重放到最后一手之前）的命中簇。
        let (parent, region) = self.lookup_grid(
            &self.parent_grid(board, records)?,
            last.player,
        )?;
        // 落点在父局面命中簇的区域外：这一手与本局部无关，不作库内对比。
        if !region.contains(at.x(), at.y()) {
            return None;
        }
        let at_text = at.to_gtp(board.size());
        let in_book = parent.continuations.iter().any(|c| {
            c.player == last.player && c.at.as_deref() == Some(at_text.as_str())
        });
        if in_book {
            return None;
        }
        Some(Deviation {
            played: format!("{} {at_text}", last.player.name()),
            book: parent
                .continuations
                .iter()
                .map(Continuation::text)
                .collect(),
        })
    }

    /// 父局面（去掉最后一手）的完整盘面。
    ///
    /// 提子不可逆，「当前盘面减一手」得不到父盘面；沿当前线**从根重放**
    /// `records[..n-1]`（`rules::apply_move`，无劫争，与建索引同口径）。
    /// 根摆子照实铺上：簇匹配只看局部，摆子若在簇内则两侧一致、键对齐。
    /// 重放 O(手数×盘面)，仅命中路径触发一次。
    fn parent_grid(
        &self,
        board: &Board,
        records: &[crate::board::MoveRecord],
    ) -> Option<Vec<Option<Stone>>> {
        let size = board.size();
        let mut grid: Vec<Option<Stone>> = vec![None; size.point_count()];
        let (black, white) = board.root_setup();
        for c in &black {
            grid[c.index(size)] = Some(Stone::Black);
        }
        for c in &white {
            grid[c.index(size)] = Some(Stone::White);
        }
        for record in &records[..records.len() - 1] {
            if let Action::Place(c) = record.action {
                let _ = rules::apply_move(size, &mut grid, record.player, c);
            }
        }
        Some(grid)
    }
}
