//! 参照树重放与索引构建（加载期 O(节点数) 的一次遍历）。
//!
//! # 结构转换
//!
//! SGF 的 `GameTree` 是「主链 + 末节点分叉」的两层结构，续法判断要同时看
//! 「链的下一个节点」与「各分支首节点」，两处口径容易漂移。加载期先把它
//! 展开成**统一节点树**（[`RNode`]：每个节点的 `children` = 链延续 +
//! 各分支首节点），此后「节点 X 的续法」恒等于「X.children 的首手」，
//! 深搜也变成纯树递归。展开为 O(节点数)（每节点恰被 move 一次）。
//!
//! # 现场管理
//!
//! 重放不共享可变盘面、不做撤销回填（提子回填易错），而是在每个节点
//! **克隆**父盘面后落子（19 路一份 361 字节，节点数 ×361 B 的瞬时分配，
//! 量级远小于索引本身）。分支间天然互不污染，无需任何恢复逻辑。
//!
//! # 失败口径
//!
//! 行棋方不符 / 坐标非法 / 占位自杀：警告一次并**舍弃该分支**（不递归
//! 其子树），兄弟分支不受影响；整份加载仍成功。刻意**不判劫**（用
//! `rules::apply_move`）——定式库中的劫争循环着法必须能重放，否则
//! 「劫争定式」整支丢失。

use std::collections::HashMap;

use super::entry::RefEntry;
use super::match_::{Continuation, all_cluster_keys};
use crate::board::rules::{self, Stone};
use crate::board::{Action, Size};
use crate::sgf::{GameTree, Node};

/// 局面键：比较区域内、以区域左上角为原点的棋子分布 + 行棋方。
///
/// 字节布局见 `match_::key_of_cluster`。**刻意做成 `Vec<u8>` 结构体键
/// 而非 u64 哈希**：`HashMap` 相等判定即逐字节相等，天然无碰撞，省去
/// 「哈希相等后再精确比对盘面」的二次复核；键长 = 3 + 簇内子数×3
/// （定式簇常 < 30 子，即 < 100 B/条）。将来若要压缩内存可改 u64 哈希，
/// 届时在 [`RefEntry::stones`] 上做二次精确复核即可（字段已留）。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct PositionKey(pub(crate) Vec<u8>);

impl PositionKey {
    /// 键的字节内容（取证 / debug 用）。
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// 比较区域：闭矩形（含两端），分量独立、不必回 `Coord`（`Coord` 绑定
/// 运行时 `Size`，键计算用不到；UI / headless 取证时再按盘转 GTP 两角）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    /// 左上角 x（含）。
    pub min_x: u8,
    /// 左上角 y（含）。
    pub min_y: u8,
    /// 右下角 x（含）。
    pub max_x: u8,
    /// 右下角 y（含）。
    pub max_y: u8,
}

impl Rect {
    /// 是否包含该点。
    pub(crate) fn contains(&self, x: u8, y: u8) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    /// 宽（交叉点数）。
    pub(crate) fn w(&self) -> u8 {
        self.max_x - self.min_x + 1
    }

    /// 高（交叉点数）。
    pub(crate) fn h(&self) -> u8 {
        self.max_y - self.min_y + 1
    }
}

/// 展开后的统一节点树：`children` = 链延续 + 各分支首节点（见模块文档）。
/// 借用 SGF 树的节点，生命周期随加载函数；深搜结束即释放，只留索引条目。
/// `Clone` 供根条目入索引时复制分支壳层（纯指针拷贝，见 `build` 注释）。
#[derive(Clone)]
struct RNode<'a> {
    node: &'a Node,
    children: Vec<RNode<'a>>,
}

/// 把一棵 `GameTree` 展开为「首节点及其全部后继」的统一树列表。
/// 顶层调用返回单元素列表（根）；每层从后往前折叠，O(节点数)。
fn expand(tree: &GameTree) -> Vec<RNode<'_>> {
    // 末节点的分支后继（每棵子树展开为首节点带嵌套后继）。
    let mut current: Vec<RNode<'_>> =
        tree.children.iter().flat_map(expand).collect();
    // 主链从后往前折叠：nodes[i] 的后继 = [nodes[i+1]]（末节点为分支）。
    for node in tree.nodes.iter().rev() {
        let me = RNode {
            node,
            children: std::mem::take(&mut current),
        };
        current = vec![me];
    }
    current
}

/// 建索引：重放整棵树并为每个有着点的节点算键入表；**根局面在盘面非空
/// （有摆子/预置子）时同样入索引**——死活题 / 定式谱的第一手之前的父
/// 局面正是根局面，它缺席会让「第一手走偏」的偏离判定（父局面命中 +
/// 最后一手不在续法中）无从成立，退化为「不在题谱中」的误导提示。空盘
/// 根（无摆子）没有索引价值：任何实战局面的父局面链都不会经过它，且
/// 「无子的父局面」没有可比较的局部簇，跳过。
/// 返回（索引表，警告，着法节点总数）。每节点一次键计算，整体 O(节点数)。
pub(super) fn build(
    tree: &GameTree,
    size: Size,
) -> (HashMap<PositionKey, RefEntry>, Vec<String>, usize) {
    let mut grid: Vec<Option<Stone>> = vec![None; size.point_count()];
    let mut warnings = Vec::new();
    let mut move_nodes = 0usize;
    let mut root_node: Option<&Node> = None;
    if let Some(root) = tree.root() {
        apply_root_setup(root, size, &mut grid, &mut warnings);
        root_node = Some(root);
    }
    let to_play = root_to_play(tree);
    let mut index = HashMap::new();
    let mut path: Vec<usize> = Vec::new();
    // 根局面（0 手）：有摆子才入索引。条目的「续法」= 首着手法（root
    // 的链延续 + 各分支首节点，统一树展开后即 `top` 各首手），
    // 注释 = 根注释（题面常写在这里，UI 的题面展示依赖它）。
    // 构造根 RNode 时克隆 `top`（仅分支壳层的浅拷贝量级：每项是一个
    // 借用 SGF 节点的结构体，克隆 = 指针拷贝 × 分支数，可忽略）。
    let top = expand(tree);
    if grid.iter().any(|stone| stone.is_some())
        && let Some(root) = root_node
    {
        index_entry(
            &RNode {
                node: root,
                children: top.clone(),
            },
            size,
            &grid,
            to_play,
            &[],
            &path,
            0,
            &mut index,
        );
    }
    walk(
        &top,
        size,
        &grid,
        to_play,
        0,
        &[],
        &mut path,
        &mut index,
        &mut warnings,
        &mut move_nodes,
    );
    (index, warnings, move_nodes)
}

/// 根节点预设摆子（AB/AW/AE）铺盘；坐标非法只警告不失败（宽容真实文件）。
fn apply_root_setup(
    root: &Node,
    size: Size,
    grid: &mut [Option<Stone>],
    warnings: &mut Vec<String>,
) {
    let mut put = |ident: &str, stone: Option<Stone>| {
        match root.coords(ident, size) {
            Ok(coords) => {
                for c in coords {
                    grid[c.index(size)] = stone;
                }
            }
            Err(err) => warnings.push(format!("参照根节点摆子 {ident} 无法解析：{err}")),
        }
    };
    put("AB", Some(Stone::Black));
    put("AW", Some(Stone::White));
    put("AE", None);
}

/// 根节点的首着方：`PL` 优先；缺省按 FF4 惯例——有摆子白先，否则黑先
/// （与 `sgf::load` 的载入口径一致）。
fn root_to_play(tree: &GameTree) -> Stone {
    if let Some(root) = tree.root() {
        match root.text("PL").map(str::trim) {
            Some("b") | Some("B") => return Stone::Black,
            Some("w") | Some("W") => return Stone::White,
            _ => {}
        }
        if root.get("AB").is_some() || root.get("AW").is_some() {
            return Stone::White;
        }
    }
    Stone::Black
}

/// 深搜统一树：`chain` 为同一父局面下的兄弟分支列表。
///
/// - `grid` / `to_play` / `move_no` / `tail`：父局面的现场（不可变借用，
///   落子节点克隆后推进）；
/// - `path`：统一树中每层「在各自 children 列表里的下标」（下一批
///   「跳进参照树浏览」按同一套 `expand` 重构即可按 path 定位节点）。
#[allow(clippy::too_many_arguments)]
fn walk(
    chain: &[RNode<'_>],
    size: Size,
    grid: &[Option<Stone>],
    to_play: Stone,
    move_no: usize,
    tail: &[(Stone, String)],
    path: &mut Vec<usize>,
    index: &mut HashMap<PositionKey, RefEntry>,
    warnings: &mut Vec<String>,
    move_nodes: &mut usize,
) {
    for (i, rn) in chain.iter().enumerate() {
        path.push(i);
        match rn.node.move_action(size) {
            Err(err) => {
                // 坐标非法：舍弃该分支（不递归），兄弟分支继续。
                warnings.push(format!(
                    "参照第 {} 手无法载入：{err}，该分支已舍弃",
                    move_no + 1
                ));
            }
            Ok(None) => {
                // 纯注释 / 空节点：不产生局面、不索引，链延续到子节点。
                walk(
                    &rn.children, size, grid, to_play, move_no, tail, path, index, warnings,
                    move_nodes,
                );
            }
            Ok(Some((stone, action))) => {
                *move_nodes += 1;
                if stone != to_play {
                    warnings.push(format!(
                        "参照第 {} 手行棋方为{}，实际轮到{}，该分支已舍弃",
                        move_no + 1,
                        stone.name(),
                        to_play.name()
                    ));
                } else {
                    // 克隆父盘面后落子（现场管理见模块文档）。
                    let mut next = grid.to_vec();
                    let placed = match action {
                        Action::Place(at) => rules::apply_move(size, &mut next, stone, at)
                            .map(|_| Some(at)),
                        Action::Pass => Ok(None),
                    };
                    match placed {
                        Err(reason) => warnings.push(format!(
                            "参照第 {} 手 {}：{reason}，该分支已舍弃",
                            move_no + 1,
                            match action {
                                Action::Place(at) => at.to_gtp(size),
                                Action::Pass => "弃着".to_owned(),
                            }
                        )),
                        Ok(at_opt) => {
                            // 尾部（展示用）：父尾部 + 本手，截到最后 TAIL_LEN 项。
                            let mut tail2 = tail.to_vec();
                            tail2.push((
                                stone,
                                match at_opt {
                                    Some(at) => at.to_gtp(size),
                                    None => "弃着".to_owned(),
                                },
                            ));
                            let start = tail2.len().saturating_sub(super::TAIL_LEN);
                            let tail_show = tail2[start..].to_vec();
                            // 本节点的局面 = 落子后盘面，行棋方 = 对方。
                            index_entry(
                                rn,
                                size,
                                &next,
                                to_play.opposite(),
                                &tail_show,
                                path,
                                move_no + 1,
                                index,
                            );
                            walk(
                                &rn.children,
                                size,
                                &next,
                                to_play.opposite(),
                                move_no + 1,
                                &tail_show,
                                path,
                                index,
                                warnings,
                                move_nodes,
                            );
                        }
                    }
                }
            }
        }
        path.pop();
    }
}

/// 为一个成功落子的节点建键并写入索引。
///
/// 续法 = 该节点 children 的首手（统一树结构，见模块文档）；child 自身
/// 无着点（罕见纯注释节点）时沿其链下探首个有着点后裔。
///
/// **全簇入索引**（`all_cluster_keys`）：参照节点通常只有一个局部簇，
/// 但带预设摆子 / 多局部同谱（整盘死活题）的节点会有多个簇——逐簇建键
/// 保证任一簇在实战盘面重现都能命中。同一节点的多个键映射到同一份
/// `RefEntry`（条目本体只存首个簇的区域 / 棋子，供取证展示）。
#[allow(clippy::too_many_arguments)]
fn index_entry(
    rn: &RNode<'_>,
    size: Size,
    grid: &[Option<Stone>],
    to_play: Stone,
    tail: &[(Stone, String)],
    path: &[usize],
    move_no: usize,
    index: &mut HashMap<PositionKey, RefEntry>,
) {
    let clusters = all_cluster_keys(size, grid, to_play);
    let Some(first) = clusters.first() else {
        return; // 空盘无键（刚落一子，理论不可达）。
    };
    let continuations = rn
        .children
        .iter()
        .filter_map(|child| first_move_of(child, size))
        .map(|(player, action)| Continuation {
            player,
            at: match action {
                Action::Place(at) => Some(at.to_gtp(size)),
                Action::Pass => None,
            },
        })
        .collect();
    let entry = RefEntry {
        path: path.to_vec(),
        move_no,
        comment: rn.node.comment().filter(|c| !c.is_empty()).map(str::to_owned),
        continuations,
        tail: tail.to_vec(),
        region: first.rect,
        stones: first.inside.clone(),
    };
    // 同键多节点：首个胜出（深搜先到主变）——同键即同局面，续法取哪条
    // 都是库内有效信息，取首个展示稳定。多簇键共享同一份条目内容
    // （克隆写入；单节点簇数少，克隆开销可忽略）。
    for cluster in clusters {
        index.entry(cluster.key).or_insert(clone_entry(&entry));
    }
}

/// 条目浅拷贝辅助（`RefEntry` 未派生 Clone：`stones` / `tail` 等字段
/// 克隆即完整语义，手写避免误拷贝未来的非克隆字段）。
fn clone_entry(entry: &RefEntry) -> RefEntry {
    RefEntry {
        path: entry.path.clone(),
        move_no: entry.move_no,
        comment: entry.comment.clone(),
        continuations: entry
            .continuations
            .iter()
            .map(|c| Continuation {
                player: c.player,
                at: c.at.clone(),
            })
            .collect(),
        tail: entry.tail.clone(),
        region: entry.region,
        stones: entry.stones.clone(),
    }
}

/// 一个分支的首手：child 自身有着点取之；否则沿链下探首个有着点后裔。
/// 用于续法收集（纯注释节点夹在分叉点与着法之间时不丢续法）。
fn first_move_of(rn: &RNode<'_>, size: Size) -> Option<(Stone, Action)> {
    match rn.node.move_action(size) {
        Ok(Some(pair)) => Some(pair),
        Ok(None) => rn.children.iter().find_map(|c| first_move_of(c, size)),
        Err(_) => None,
    }
}
