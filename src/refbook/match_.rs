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
        /// 对称命中的「库坐标 → 实战坐标」映射（竞品移植第四批 R1）：
        /// `Some` = 实战局面是库内同形的某种对称副本（转置 / 镜像 /
        /// 旋转），库内续法坐标必须经它换到实战方向才是「该下的点」
        /// （照库落子 / 续法展示 / 偏离对比都走它）；`None` = 同向命中
        /// （含平移），坐标直用。字段恒 `Some`（同向命中也是恒等映射）
        /// 以免调用方双分支——展示与落子一律 `sym.coord(at)`。
        sym: SymMap,
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
    let local: Vec<(u8, u8, Stone)> = cluster
        .iter()
        .filter(|&&(x, y, _)| rect.contains(x, y))
        .map(|&(x, y, s)| (x - rect.min_x, y - rect.min_y, s))
        .collect();
    // 键 = 八种二面体变换下逐字节最小的那个（规范化，见对称一节）；
    // rect 仍返回**原方向**的盘上区域（展示 / 落点守卫都用实战侧坐标）。
    // 坐标映射不用「获胜变换」——对称形状常有多个变换取得同一规范键，
    // 两侧各选其一的组合会把空白点（续法）带到不同位置；映射改为
    // [`SymMap::build`] 直接求解「库棋子 → 实战棋子」的保色变换对。
    let (key, _) = canonical_variant(&local, rect.w(), rect.h(), to_play);
    (key, rect)
}

// ---- 对称归一化（竞品移植第四批 R1，22.1 既定扩展点）----
//
// 动机：定式库通常只记录某一个角的某种朝向；实战里同形出现在任意角、
// 任意朝向。平移不变（区域键以包围盒左上为原点）已覆盖「任意角」，
// 本节覆盖「任意朝向」——键取八种二面体变换下逐字节最小者。键两侧
// 同构（索引侧与查询侧共用 `key_of_cluster`），单点改键即两侧生效。
// 「转置感知」是八分之一：正方形区域上转置 / 镜像 / 旋转全并进同一键。

/// 变换总数（二面体群 D4）。
const VARIANTS: usize = 8;

/// 变换 `v` 把区域局部坐标 `(x, y)`（`0..w × 0..h`）映到新区域局部
/// 坐标，返回 `(新 x, 新 y, 新 w, 新 h)`（`w ≠ h` 时 90° 系变换交换
/// 宽高）。定义与 [`variant_inverse`] 一一对应。
fn variant_map(v: usize, x: u8, y: u8, w: u8, h: u8) -> (u8, u8, u8, u8) {
    match v {
        0 => (x, y, w, h),                     // 恒等
        1 => (w - 1 - x, y, w, h),             // 水平镜像
        2 => (x, h - 1 - y, w, h),             // 垂直镜像
        3 => (w - 1 - x, h - 1 - y, w, h),     // 旋转 180°
        4 => (y, x, h, w),                     // 转置
        5 => (h - 1 - y, w - 1 - x, h, w),     // 反对角转置
        6 => (h - 1 - y, x, h, w),             // 旋转 90°
        _ => (y, w - 1 - x, h, w),             // 旋转 270°
    }
}

/// 变换的逆下标（0–5 对合，6 / 7 互逆；非正方形盒的宽高互换由
/// [`variant_map`] 的显式 dims 参数处理，见其文档后的推演）。
fn variant_inverse(v: usize) -> usize {
    match v {
        6 => 7,
        7 => 6,
        other => other,
    }
}

/// 一次命中（同向或对称）的「库坐标 → 实战坐标」映射。
///
/// 照库落子 / 续法展示 / 偏离对比**必须**经它：库内续法坐标在库朝向
/// 上，实战局面可能是库内同形的任意对称副本——同向命中是恒等映射，
/// 对称命中则把库局部坐标经「库的规范变换 → 实战的规范逆变换」搬到
/// 实战方向。构造在命中时完成（两侧的规范变换各求一次，区域 ≤ 数点，
/// 开销可忽略），映射本身 O(1)。
#[derive(Clone, Debug)]
pub struct SymMap {
    lib_region: Rect,
    live_region: Rect,
    v_lib: usize,
    v_live: usize,
    lib_w: u8,
    lib_h: u8,
    size: crate::board::Size,
}

impl SymMap {
    /// 由命中两侧构建：库侧（条目留存的区域与棋子）与实战侧（命中的
    /// 簇）。求解 = 找一对变换 `(i, j)` 使「库棋子经 i」与「实战棋子经
    /// j」为同一带色点集（键相等的直接推论，必然存在）；取第一对。
    ///
    /// **为什么不用两侧各自的「规范获胜变换」**：对称形状常有多个变换
    /// 取得同一规范键（该形状的自同构），库侧与实战侧各自的首胜组合
    /// 会把**空白点**（续法所在）带到不同位置——首胜组合只保证棋子对
    /// 齐。而任何「保全部棋子（带色）」的变换对在语义上都等价：其差
    /// 异自由度只作用于空白边缘，且差异两侧相对棋盘形状互为该形状的
    /// 对称副本（续法落点等价）。首对即确定、确定性可复现。
    pub(super) fn build(
        entry: &RefEntry,
        live: &ClusterKey,
        size: crate::board::Size,
    ) -> Self {
        let lib_w = entry.region.w();
        let lib_h = entry.region.h();
        let lib_local: Vec<(u8, u8, Stone)> = entry
            .stones
            .iter()
            .map(|&(x, y, s)| (x - entry.region.min_x, y - entry.region.min_y, s))
            .collect();
        let live_w = live.rect.w();
        let live_h = live.rect.h();
        let live_local: Vec<(u8, u8, Stone)> = live
            .inside
            .iter()
            .map(|&(x, y, s)| (x - live.rect.min_x, y - live.rect.min_y, s))
            .collect();
        let variant_stones = |v: usize,
                              stones: &[(u8, u8, Stone)],
                              w: u8,
                              h: u8|
         -> (u8, u8, Vec<(u8, u8, Stone)>) {
            let (nw, nh) = if v >= 4 { (h, w) } else { (w, h) };
            let mut transformed: Vec<(u8, u8, Stone)> = stones
                .iter()
                .map(|&(x, y, s)| {
                    let (nx, ny, _, _) = variant_map(v, x, y, w, h);
                    (nx, ny, s)
                })
                .collect();
            transformed.sort_by_key(|&(x, y, _)| (x, y));
            (nw, nh, transformed)
        };
        // 首个「库变换 i 的带色点集 == 实战变换 j 的带色点集」对（含
        // 变换后宽高——键相等的必要部分）。键命中保证存在。
        let mut found: Option<(usize, usize)> = None;
        for i in 0..VARIANTS {
            let (lw, lh, lib_set) = variant_stones(i, &lib_local, lib_w, lib_h);
            for j in 0..VARIANTS {
                let (lw2, lh2, live_set) =
                    variant_stones(j, &live_local, live_w, live_h);
                if lw == lw2 && lh == lh2 && lib_set == live_set {
                    found = Some((i, j));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let (v_lib, v_live) =
            found.expect("键命中则必存在保色变换对（规范键相等的直接推论）");
        eprintln!(
            "[sym] entry.region={:?} stones={:?} live.rect={:?} inside={:?} pair=({v_lib},{v_live})",
            entry.region, entry.stones, live.rect, live.inside
        );
        Self {
            lib_region: entry.region,
            live_region: live.rect,
            v_lib,
            v_live,
            lib_w,
            lib_h,
            size,
        }
    }

    /// 库内坐标 → 实战坐标。区域外的点（续法理论上都在区域内，防御）
    /// 原样返回。
    pub fn coord(&self, at: Coord) -> Coord {
        if !self.lib_region.contains(at.x(), at.y()) {
            return at;
        }
        let (x, y) = (at.x() - self.lib_region.min_x, at.y() - self.lib_region.min_y);
        // 库局部 → 规范区域（c*）→ 实战局部（实战规范逆变换的源区域
        // 是规范区域，dims 由 variant_map 显式传入）→ 实战盘上坐标。
        let (cx, cy, cw, ch) = variant_map(self.v_lib, x, y, self.lib_w, self.lib_h);
        let (lx, ly, _, _) =
            variant_map(variant_inverse(self.v_live), cx, cy, cw, ch);
        Coord::new(
            self.size,
            self.live_region.min_x + lx,
            self.live_region.min_y + ly,
        )
        .unwrap_or(at)
    }
}

/// 规范键 = 八个变换键中**逐字节最小**者；返回键与获胜变换下标
/// （坐标映射用它，见 [`SymMap`]）。
fn canonical_variant(
    stones: &[(u8, u8, Stone)],
    w: u8,
    h: u8,
    to_play: Stone,
) -> (PositionKey, usize) {
    let mut best: Option<(PositionKey, usize)> = None;
    for v in 0..VARIANTS {
        let (nw, nh) = if v >= 4 { (h, w) } else { (w, h) };
        let mut transformed: Vec<(u8, u8, Stone)> = stones
            .iter()
            .map(|&(x, y, s)| {
                let (nx, ny, _, _) = variant_map(v, x, y, w, h);
                (nx, ny, s)
            })
            .collect();
        transformed.sort_by_key(|&(x, y, _)| (x, y));
        let mut key = Vec::with_capacity(3 + transformed.len() * 3);
        key.push(nw);
        key.push(nh);
        key.push(u8::from(matches!(to_play, Stone::Black)));
        for (x, y, s) in transformed {
            key.push(x);
            key.push(y);
            key.push(u8::from(matches!(s, Stone::Black)));
        }
        if best
            .as_ref()
            .is_none_or(|(current, _)| key < current.0)
        {
            best = Some((PositionKey(key), v));
        }
    }
    best.expect("VARIANTS ≥ 1，循环至少产生一个候选")
}

/// 一个簇的完整信息（键 + 比较区域 + 区域内棋子）。
pub(crate) struct ClusterKey {
    /// 局面键（规范化，见对称一节）。
    pub key: PositionKey,
    /// 比较区域（盘上绝对坐标，**实战侧原方向**）。
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
        if let Some((entry, cluster)) = self.lookup_grid(board.grid(), board.to_play()) {
            let sym = SymMap::build(entry, &cluster, board.size());
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
                sym,
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
    ) -> Option<(&RefEntry, ClusterKey)> {
        all_cluster_keys(self.size, grid, to_play)
            .into_iter()
            .find_map(|cluster| {
                self.index.get(&cluster.key).map(|entry| (entry, cluster))
            })
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
        let (parent, cluster) = self.lookup_grid(
            &self.parent_grid(board, records)?,
            last.player,
        )?;
        // 落点在父局面命中簇的区域外：这一手与本局部无关，不作库内对比。
        // region 是实战侧原方向区域，对称命中同样适用（守卫在实战方向）。
        if !cluster.rect.contains(at.x(), at.y()) {
            return None;
        }
        // 库内续法坐标经对称映射搬到实战方向再与实战落点比对（R1）：
        // 镜像 / 转置命中时库里的 D4 在实战盘上是另一个点。
        let sym = SymMap::build(parent, &cluster, board.size());
        let at_text = at.to_gtp(board.size());
        let mapped_book = parent.continuations.iter().map(|c| match c.at.as_deref() {
            Some(at_lib) => {
                let live = crate::board::Coord::from_gtp(board.size(), at_lib)
                    .ok()
                    .map(|at_lib| sym.coord(at_lib))
                    .map(|at_live| at_live.to_gtp(board.size()));
                match live {
                    Some(at_live) => format!("{} {at_live}", c.player.name()),
                    None => c.text(),
                }
            }
            None => c.text(),
        });
        let in_book = parent.continuations.iter().any(|c| {
            c.player == last.player
                && c.at
                    .as_deref()
                    .and_then(|at_lib| crate::board::Coord::from_gtp(board.size(), at_lib).ok())
                    .is_some_and(|at_lib| sym.coord(at_lib) == at)
        });
        if in_book {
            return None;
        }
        Some(Deviation {
            played: format!("{} {at_text}", last.player.name()),
            book: mapped_book.collect(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Size;

    #[test]
    fn 八种变换全部可逆且宽高互换正确() {
        for v in 0..VARIANTS {
            for &(w, h) in &[ (3u8, 2u8), (5, 5), (1, 4) ] {
                for x in 0..w {
                    for y in 0..h {
                        let (tx, ty, tw, th) = variant_map(v, x, y, w, h);
                        assert!(tx < tw && ty < th, "变换后坐标须在新区域内");
                        let (bx, by, bw, bh) =
                            variant_map(variant_inverse(v), tx, ty, tw, th);
                        assert_eq!((bx, by, bw, bh), (x, y, w, h), "v={v}");
                    }
                }
            }
        }
    }

    #[test]
    fn 同形的不同朝向规范化为同一键() {
        // 基形：3×2 盒内 (0,0) 黑、(2,1) 白。八个朝向分别构造，
        // 规范键必须逐字节相同（且与基形本身相同）。
        let base: Vec<(u8, u8, Stone)> =
            vec![(0, 0, Stone::Black), (2, 1, Stone::White)];
        let (expected, _) = canonical_variant(&base, 3, 2, Stone::Black);
        for v in 0..VARIANTS {
            let (nw, nh) = if v >= 4 { (2, 3) } else { (3, 2) };
            let transformed: Vec<(u8, u8, Stone)> = base
                .iter()
                .map(|&(x, y, s)| {
                    let (nx, ny, _, _) = variant_map(v, x, y, 3, 2);
                    (nx, ny, s)
                })
                .collect();
            let (key, _) = canonical_variant(&transformed, nw, nh, Stone::Black);
            assert_eq!(key.0, expected.0, "v={v}");
        }
        // 不同形状不得撞键。
        let other: Vec<(u8, u8, Stone)> =
            vec![(0, 0, Stone::Black), (1, 1, Stone::White)];
        let (other_key, _) = canonical_variant(&other, 3, 2, Stone::Black);
        assert_ne!(other_key.0, expected.0);
    }

    fn entry_for_test() -> RefEntry {
        RefEntry {
            path: vec![0, 1],
            move_no: 2,
            comment: None,
            continuations: Vec::new(),
            tail: Vec::new(),
            region: Rect { min_x: 2, min_y: 2, max_x: 4, max_y: 3 },
            // 3×2 盒内同款基形（盘上绝对坐标，库朝向）。
            stones: vec![(2, 2, Stone::Black), (4, 3, Stone::White)],
        }
    }

    #[test]
    fn 对称命中映射把库坐标搬到实战方向() {
        // 实战侧 = 库形状的转置（3×2 → 2×3 盒，放在别处）。
        let live_stones: Vec<(u8, u8, Stone)> = vec![(10, 10, Stone::Black), (11, 12, Stone::White)];
        let live = ClusterKey {
            key: canonical_variant(
                &[(0, 0, Stone::Black), (1, 2, Stone::White)],
                2,
                3,
                Stone::Black,
            )
            .0,
            rect: Rect { min_x: 10, min_y: 10, max_x: 11, max_y: 12 },
            inside: live_stones
                .iter()
                .map(|&(x, y, s)| (x, y, s))
                .collect(),
        };
        let entry = entry_for_test();
        // 先确认两侧键确实同字节（转置是规范赢家之一）。
        let (lib_key, _) = canonical_variant(
            &[(0, 0, Stone::Black), (2, 1, Stone::White)],
            3,
            2,
            Stone::Black,
        );
        assert_eq!(live.key.0, lib_key.0, "测试数据须构成转置命中");
        let sym = SymMap::build(&entry, &live, Size::new(19).unwrap());
        // 库内空点 (3,3)（区域中点）→ 经两侧变换映射到实战方向。
        let mapped = sym
            .coord(crate::board::Coord::new(Size::new(19).unwrap(), 3, 3).unwrap());
        // 转置侧：库局部 (1,1) → 规范 → 实战逆变换；手工推演基形转置：
        // 库 (1,1) 处于 3×2 盒，规范赢家计算后与实战 2×3 盒互逆，结果
        // 必落在实战区域内。
        assert!(
            mapped.x() >= 10
                && mapped.x() <= 11
                && mapped.y() >= 10
                && mapped.y() <= 12,
            "映射结果必须落在实战区域：{mapped:?}"
        );
        // 库区域外的点原样返回（防御口径）。
        let outside = sym
            .coord(crate::board::Coord::new(Size::new(19).unwrap(), 0, 0).unwrap());
        assert_eq!(outside.x(), 0);
        // 棋子对应严格：库内两颗子必须逐一映射到实战两颗子上。
        let b = sym
            .coord(crate::board::Coord::new(Size::new(19).unwrap(), 2, 2).unwrap());
        assert_eq!((b.x(), b.y()), (10, 10), "库黑子 → 实战黑子");
        let w = sym
            .coord(crate::board::Coord::new(Size::new(19).unwrap(), 4, 3).unwrap());
        assert_eq!((w.x(), w.y()), (11, 12), "库白子 → 实战白子");
        // 空点：库局部 (1,1) 经转置对应 → 实战局部 (1,1)。
        let mid = sym
            .coord(crate::board::Coord::new(Size::new(19).unwrap(), 3, 3).unwrap());
        assert_eq!((mid.x(), mid.y()), (11, 11), "转置对应空点");
    }

    #[test]
    fn 转置库的转置实战局面命中且续法映射唯一() {
        // 库（9 路）：C3 / C7 / E3 / G7 四手（转置基形的原始朝向，
        // 三子非共线 ⇒ 保色二面体映射唯一）。实战按转置落 C3 / C7 /
        // E3 后命中节点 3，其续法 ge=(6,4) 的转置 = (4,6) = E7，
        // 映射必须严格等于它。
        let lib = crate::sgf::parse_str("(;GM[1]FF[4]SZ[9];B[cc];W[gc];B[ce];W[ge])")
            .expect("测试库 SGF 合法");
        let reference = super::super::build_index(
            std::path::PathBuf::from("test.sgf"),
            lib,
            Size::new(9).unwrap(),
        );
        let mut board = crate::board::Board::new(Size::new(9).unwrap());
        for gtp in ["C3", "C7", "E3"] {
            board
                .play(crate::board::Coord::from_gtp(Size::new(9).unwrap(), gtp).unwrap())
                .unwrap();
        }
        match reference.lookup(&board) {
            LookupOutcome::Hit { entry, sym, .. } => {
                assert_eq!(entry.move_no, 3, "命中第 3 手节点");
                let mapped = entry.continuations.iter().find_map(|cont| {
                    cont.at
                        .as_deref()
                        .and_then(|at| {
                            crate::board::Coord::from_gtp(Size::new(9).unwrap(), at).ok()
                        })
                        .map(|at| (cont.player, sym.coord(at)))
                });
                let (player, at) = mapped.expect("节点 3 有续法");
                assert_eq!(player, Stone::White);
                assert_eq!(
                    at.to_gtp(Size::new(9).unwrap()),
                    "E7",
                    "转置映射：ge(6,4)→(4,6)"
                );
            }
            LookupOutcome::Miss => panic!("转置局面应命中，实际 Miss"),
            LookupOutcome::Deviation(d) => panic!("转置局面应命中，实际偏离 {d:?}"),
            LookupOutcome::SizeMismatch { .. } => panic!("尺寸一致却报不匹配"),
        }
    }

    #[test]
    fn 对称形状的续法映射落在等价点集合内() {
        // 两子共线形状关于其轴镜像对称：A3 与 E3 都是与库内 ce 等价的
        // 续法点（实现取确定性首解）。断言映射落在等价集合内，且棋子
        // 对应严格正确（映射必须把库内棋子逐一搬到实战棋子上）。
        let lib = crate::sgf::parse_str("(;GM[1]FF[4]SZ[9];B[cc];W[gc];B[ce])")
            .expect("测试库 SGF 合法");
        let reference = super::super::build_index(
            std::path::PathBuf::from("test.sgf"),
            lib,
            Size::new(9).unwrap(),
        );
        let mut board = crate::board::Board::new(Size::new(9).unwrap());
        for gtp in ["C3", "C7"] {
            board
                .play(crate::board::Coord::from_gtp(Size::new(9).unwrap(), gtp).unwrap())
                .unwrap();
        }
        match reference.lookup(&board) {
            LookupOutcome::Hit { entry, sym, .. } => {
                let mapped = entry.continuations.iter().find_map(|cont| {
                    cont.at
                        .as_deref()
                        .and_then(|at| {
                            crate::board::Coord::from_gtp(Size::new(9).unwrap(), at).ok()
                        })
                        .map(|at| sym.coord(at))
                });
                let at = mapped.expect("节点 2 有续法");
                assert!(
                    ["A3", "E3"].contains(&at.to_gtp(Size::new(9).unwrap()).as_str()),
                    "映射落在等价点集合：{}",
                    at.to_gtp(Size::new(9).unwrap())
                );
                // 棋子对应严格（GTP 行号自下而上：库 cc = C7、gc = G7；
                // 实战黑在 C3、白在 C7）：库黑子 C7 → 实战黑子 C3，
                // 库白子 G7 → 实战白子 C7。
                assert_eq!(
                    sym.coord(crate::board::Coord::from_gtp(Size::new(9).unwrap(), "C7").unwrap())
                        .to_gtp(Size::new(9).unwrap()),
                    "C3",
                    "库黑子 → 实战黑子"
                );
                assert_eq!(
                    sym.coord(crate::board::Coord::from_gtp(Size::new(9).unwrap(), "G7").unwrap())
                        .to_gtp(Size::new(9).unwrap()),
                    "C7",
                    "库白子 → 实战白子"
                );
            }
            _ => panic!("两子转置局面应命中"),
        }
    }
}


#[cfg(test)]
mod e2e_probe {
    use super::*;
    use crate::board::{Board, Size};

    #[test]
    fn e2e_同形转置命中探针() {
        let lib = crate::sgf::parse_str("(;GM[1]FF[4]SZ[9];B[cc];W[gc];B[ce];W[ge])")
            .unwrap();
        let reference = super::super::build_index(
            std::path::PathBuf::from("t.sgf"),
            lib,
            Size::new(9).unwrap(),
        );
        // E2E 顺序：C7（黑）、C3（白）、E3（黑）。
        let mut board = Board::new(Size::new(9).unwrap());
        for gtp in ["C7", "C3", "E3"] {
            board
                .play(crate::board::Coord::from_gtp(Size::new(9).unwrap(), gtp).unwrap())
                .unwrap();
        }
        let outcome = reference.lookup(&board);
        match &outcome {
            LookupOutcome::Hit { entry, sym, .. } => {
                eprintln!("HIT move_no={} sym.v_lib/v_live 见下", entry.move_no);
                let mapped = entry.continuations.iter().find_map(|cont| {
                    cont.at.as_deref().and_then(|at| {
                        crate::board::Coord::from_gtp(Size::new(9).unwrap(), at)
                            .ok()
                            .map(|at| (cont.player, sym.coord(at)))
                    })
                });
                eprintln!("continuation mapped = {mapped:?}");
            }
            LookupOutcome::Deviation(d) => eprintln!("DEVIATION played={} book={:?}", d.played, d.book),
            LookupOutcome::Miss => eprintln!("MISS"),
            _ => eprintln!("other"),
        }
    }
}
