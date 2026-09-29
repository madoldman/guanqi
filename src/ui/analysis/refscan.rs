//! 参照库引擎打分（阶段 22.3）：用 KataGo 批量评估参照库里每个有续法的
//! 局面，标出「已被 AI 淘汰」的分支。
//!
//! # 定位
//!
//! Kogo 那类定式库普遍陈旧，判断哪些老定式已过时是 KataGo 前端独有的
//! 能力。本模块产出「AI 视角下的定式库」：这是 **AI 对库的评估，不是库
//! 本身的对错**——所有界面文案都带这层说明。
//!
//! # 与整谱快扫（`batch`）的关系
//!
//! 心智模型同构：参照树的「极大链」↔ 快扫含变着模式的「一条线的多个
//! turn」，两者都是「一条着法路径 + 若干 turn」的 `analyzeTurns` 查询，
//! 因此**复用** [`super::batch`] 的发送函数与「组间串行、组内切块、收齐
//! 一块再派下一块、心跳判僵死、terminate 取消」的调度模式。但任务状态
//! **完全独立**（[`RefScanJob`] 不与当前对局耦合：无 `watch`、局面变化
//! 不取消——参照与对局本就正交，打分期间用户照常复盘）；绝不改快扫的
//! 行为。结果只在本会话内保存，**换参照即清空**（结果描述的是「那一份
//! 库」，换库后旧结果无从对号）；不做持久化——与项目「不内置数据、
//! 不做库管理」的边界一致。
//!
//! # 互斥口径
//!
//! 与整谱快扫互相拒绝（两侧都吃引擎队列，同时跑会把批量排队拖到不可
//! 用）：发起侧守卫见 [`AnalysisState::start_ref_scan`] 与 `start_batch`。
//! 与**当前对局的常规分析**不互斥：常规查询带高优先级（实测可插队），
//! 参照打分只会互相拖慢，不停对局分析——这一点在界面提示里说明。
//!
//! # 「已淘汰」判定口径
//!
//! 对一个被扫节点：引擎首选 = 报告 `moveInfos` 中 `order` 最小者；库中
//! 每条续法按落点从同一份 `moveInfos` 取评估（黑方视角，存储口径）。
//! 行棋方视角差距 = (首选值 − 续法值) × 行棋方符号（黑 +1 / 白 −1，
//! 黑方视角差对白方行棋要翻转才是「白方亏多少」）。差距满足**目差 ≥
//! [`REFSCAN_OUTDATED_SCORE`] 或 胜率差 ≥ [`REFSCAN_OUTDATED_WINRATE`]**
//! 即判该续法「已被 AI 淘汰」。库中续法不在 `moveInfos` 里 = 「引擎未
//! 列出」（引擎只列访问量较高的候选），**不臆造数值**、不判淘汰。
//! 整节点过时 = 库中全部续法都被淘汰；部分过时 = 有淘汰有仍可。

use std::collections::HashMap;
use std::time::Instant;

use crate::board::{Action, Coord, MoveRecord, Size, Stone, rules};
use crate::engine::QueryId;
use crate::sgf::GameTree;

use super::batch::{BATCH_CHUNK, BATCH_CHUNK_STALL, BATCH_SECS_PER_HAND_AT_40};
use super::display::DisplayView;
use super::types::EngineStatus;
use super::AnalysisState;

/// 参照打分的每节点搜索量：定式判断只需要「坏分支 vs 好分支」的量级
/// 区分（淘汰阈值 3 目远大于 40 visits 的目差噪声），40 visits 与快扫
/// 同档，吞吐按快扫实测外推（b18 单线程约 0.94s/节点）。
pub const REFSCAN_VISITS: u32 = 40;

/// 扫描范围上限（库内手数）：默认只扫 `move_no ≤ 30` 的节点。
///
/// 定式 / 死活题的局部判断几乎全部落在前 30 手内（常见定式 10–20 手、
/// 复杂定式 ≤ 30 手）；整谱式大库 30 手以后的节点数爆炸增长而评估价值
/// 骤降。范围参数化（发起确认框可调），此处只定默认值。实测依据见
/// /tmp/refscan-notes.md 的规模估算：5000 节点 × 0.94s ≈ 79 分钟
/// （40 visits、单线程口径，多线程更快），30 手截断是「可用性」与
/// 「定式覆盖」的折中。
pub const REFSCAN_MAX_MOVE_NO: usize = 30;

/// 参照打分的发起配置：扫描范围（库内手数上限）+ 每局面思考量。
///
/// 快扫 [`super::batch::BatchConfig`] 的小型对应物。引入结构而非两个
/// 散参的理由：预估（[`AnalysisState::ref_scan_estimate`]）与发起
/// （[`AnalysisState::start_ref_scan`]）的签名都同时需要「范围 +
/// visits」，散参既让两处调用点持续膨胀，也拦不住「预估按 A 档算、
/// 发起按 B 档跑」的错配——配置单一来源后对话框改动即时写回（快扫
/// 同款模式），预估与发起读的必然是同一份。默认值 = 阶段 22.3 的既有
/// 行为（30 手 / 40 visits）：未动过档位的用户结果与升级前完全一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefScanConfig {
    /// 扫描范围上限（库内手数）：默认 [`REFSCAN_MAX_MOVE_NO`]。
    pub max_move_no: usize,
    /// 每局面思考量（visits）：默认 [`REFSCAN_VISITS`]。
    ///
    /// 档位的意义与代价（界面文案同口径，必须如实）：visits 越高引擎
    /// 候选表（`moveInfos`）越长，库中续法拿不到访问量就是「引擎未列
    /// 出」（不可判定、不臆造数值）——档位提高后可判定样本变多、「已
    /// 淘汰」检出更充分，**但判定口径（阈值 / 行棋方换算）不变，不代
    /// 表判定更「正确」，只是样本更充分**；耗时按倍数线性增长（预估
    /// 与本值同源外推）。
    pub visits: u32,
}

impl Default for RefScanConfig {
    fn default() -> Self {
        Self {
            max_move_no: REFSCAN_MAX_MOVE_NO,
            visits: REFSCAN_VISITS,
        }
    }
}

/// 「已淘汰」的目差阈值（目，行棋方视角）：库中续法与引擎首选的目差
/// 差达到此值即判「已被 AI 淘汰」。
///
/// 依据：`types.rs` 的失误分级把「失误」定在 3 目（约一个普通官子的
/// 价值、局部选点错误的量级）、「恶手」定在 6 目（约半手棋）。定式库
/// 续法是局部选点，被淘汰的口径取「失误」档：低于它的差异不足以说
/// 「这手被淘汰了」（40 visits 的目差复评噪声约零点几目，0.3 目以下
/// 纯噪声）；胜率差作为或条件兜底大局已定型局面。
pub const REFSCAN_OUTDATED_SCORE: f64 = 3.0;

/// 「已淘汰」的胜率差阈值（行棋方视角，与目差阈值**或**关系）：取
/// 10%——目差 3 目在开局约折 8–12% 胜率，与目差阈值同量级；单独存在
/// 时（实地差大但胜率差小、或反之）也能兜住量级判断。
pub const REFSCAN_OUTDATED_WINRATE: f64 = 0.10;

/// 「引擎未列出」续法在**排行榜排序**里的哨兵差距（目）：它意味着引擎
/// 对该续法几乎没有分配访问量（大概率很坏），但**绝不作为评估值显示**，
/// 只用于「被淘汰分支 Top N」的排序（未列出者排在已知差距之后、按未
/// 列出本身仍靠前）。
const RANK_UNLISTED_GAP: f64 = 30.0;

/// 参照打分结果里的一条续法评估。
#[derive(Clone, Debug)]
pub struct RefContinuationScore {
    /// 库中续法（展示串，「黑 D4」形态）。
    pub text: String,
    /// 引擎评估：`Some((黑方胜率, 黑方目差))` = 从该次报告的 `moveInfos`
    /// 里按落点取出；`None` = 引擎未列出（该续法不在候选表里——引擎只
    /// 列访问量较高的候选，**如实记录，不臆造数值**）。
    pub eval: Option<(f64, f64)>,
    /// 行棋方视角的目差差距（正 = 劣于引擎首选多少目）；引擎未列出为
    /// `None`（差距无从计算，同「不臆造」口径）。
    pub gap: Option<f64>,
    /// 是否被判「已被 AI 淘汰」（口径见模块文档；引擎未列出的不判，
    /// 保守不标）。
    pub outdated: bool,
}

impl RefContinuationScore {
    /// 评估展示串（与全仓「不臆造」口径一致：未列出就写「引擎未列出」）。
    pub fn eval_text(&self) -> String {
        match self.eval {
            Some((wr, lead)) => format!("胜率 {:.1}%，目差 {:+.1}", wr * 100.0, lead),
            None => "引擎未列出".to_owned(),
        }
    }

    /// 差距展示串（未列出无差距可写）。
    pub fn gap_text(&self) -> String {
        self.gap.map_or_else(
            || "—".to_owned(),
            |gap| format!("−{gap:.1} 目"),
        )
    }
}

/// 一个被扫节点（参照树中「有子节点」的局面）的打分结果。
#[derive(Clone, Debug)]
pub struct RefNodeScore {
    /// 库内手数（根 = 0；摆子不计）。
    pub move_no: usize,
    /// 树内路径（各层下标），与 [`crate::refbook::RefEntry::path`] 同口径。
    pub path: Vec<usize>,
    /// 该局面的着法尾部（本手 + 前 2 手，「黑 D4」形态）。
    pub tail: Vec<String>,
    /// 该局面的行棋方（差距换算的依据）。
    pub to_play: Stone,
    /// 引擎首选（`moveInfos` 中 `order` 最小者）：落点展示串 +
    /// 黑方胜率 + 黑方目差。引擎未返回候选（空报告等）为 `None`。
    pub best: Option<(String, f64, f64)>,
    /// 该局面在库中的每条续法的评估（按库内顺序）。
    pub continuations: Vec<RefContinuationScore>,
    /// 引擎搜索量（该报告 `rootInfo.visits`）。
    pub visits: u64,
}

impl RefNodeScore {
    /// 「已被 AI 淘汰」的库内续法数。
    pub fn outdated_count(&self) -> usize {
        self.continuations.iter().filter(|c| c.outdated).count()
    }

    /// 整节点是否过时：库中**全部**续法都被判淘汰（库中此局面给出的
    /// 每一条走法都显著劣于引擎首选）。
    pub fn fully_outdated(&self) -> bool {
        !self.continuations.is_empty() && self.continuations.iter().all(|c| c.outdated)
    }

    /// 部分过时：有淘汰的也有仍可的。
    pub fn partially_outdated(&self) -> bool {
        let outdated = self.outdated_count();
        outdated > 0 && outdated < self.continuations.len()
    }

    /// 排序用已知最大差距（目）：库中续法与引擎首选的最大目差差；
    /// 「引擎未列出」按 [`RANK_UNLISTED_GAP`] 计（仅排序用，展示用
    /// [`RefContinuationScore::gap_text`]）。Top 榜与结果明细都按它
    /// 降序——差距越大越「确定被淘汰」。
    pub fn rank_gap(&self) -> f64 {
        self.continuations
            .iter()
            .map(|c| c.gap.unwrap_or(RANK_UNLISTED_GAP))
            .fold(0.0, f64::max)
    }
}

/// 打分结果表：按树内路径（下标串「0-2-1」）索引。
///
/// 用字符串键而非 `Vec<usize>`：UI / dump 直接以此显示「库内位置」，
/// 与 [`crate::refbook::RefEntry::path`] 的 join 口径一致。
pub type RefScanResults = HashMap<String, RefNodeScore>;

/// 扫描计划里的一条**极大链**：从根（或分叉点）到叶的一整条着法路径 +
/// 该路径上待打分的节点。一次 `analyzeTurns` 查询扫同链的多个节点
/// （快扫同构：链互为前缀的节点并进同组，组路径 = 极大链）。
#[derive(Debug)]
pub(super) struct RefScanChain {
    /// 查询 `moves`（根 → 本链最深处；真实着法，摆子不在内）。
    pub(super) path: Vec<MoveRecord>,
    /// 待打分局面（`analyzeTurns` 升序且不重复）：turn = 库内手数
    /// （0 = 根局面含摆子；真实着法自 1 起，摆子不占 turn）。
    pub(super) turns: Vec<usize>,
    /// 与 `turns` 一一对应的树内路径（结果表的键来源）。
    pub(super) node_paths: Vec<Vec<usize>>,
    /// 与 `turns` 一一对应的库中续法（行棋方 + GTP 着点；打分时按落点
    /// 对回 `moveInfos`）。
    pub(super) libs: Vec<Vec<(Stone, Option<String>)>>,
    /// 与 `turns` 一一对应的展示尾部（本手 + 前 2 手，「黑 D4」形态）。
    pub(super) tails: Vec<Vec<String>>,
}

/// 扫描计划：发起前一次算好（预估与派发共用同一份数据）。
#[derive(Debug)]
pub(super) struct RefScanPlan {
    pub(super) chains: Vec<RefScanChain>,
    /// 待打分节点总数（= 各链 turns 之和）。
    pub(super) positions: usize,
    /// 扫描用的棋盘尺寸（取自参照）。
    pub(super) size: Size,
    /// 参照根局面摆子（黑/白混排），随每条查询作 `initialStones` 发出
    /// （历史上「摆子没发引擎」的坑：摆子必须随查询走，否则引擎看到的
    /// 是另一个少子、先后手错位的局面）。
    pub(super) setup: Vec<(Stone, Coord)>,
}

/// 计划收集的中间形态：一个待打分节点（含引擎侧重放所需路径与库中
/// 续法快照）。
struct PlanNode {
    move_no: usize,
    /// 根到本局面（含）的着法（真实着法；提子明细不参与引擎查询与
    /// 前缀比较，恒空）。
    chain: Vec<MoveRecord>,
    /// 树内路径。
    path: Vec<usize>,
    /// 展示尾部（本手 + 前 2 手）。
    tail: Vec<String>,
    /// 库中续法（行棋方 + GTP 着点，弃着为 `None`）。
    libs: Vec<(Stone, Option<String>)>,
}

/// 参照树展开为统一节点树（与 `refbook::walk::expand` 同口径的两层折叠：
/// `children` = 链延续 + 各分支首节点）。在打分模块内重写而非复用：
/// `expand` 是 refbook 私有函数且返回带生命周期的借用结构，这里只借树
/// 不建索引；两侧口径注释互指防漂移。
struct ScanRNode<'a> {
    node: &'a crate::sgf::Node,
    children: Vec<ScanRNode<'a>>,
}

fn expand_scan_tree(tree: &GameTree) -> Vec<ScanRNode<'_>> {
    let mut current: Vec<ScanRNode<'_>> =
        tree.children.iter().flat_map(expand_scan_tree).collect();
    for node in tree.nodes.iter().rev() {
        current = vec![ScanRNode {
            node,
            children: std::mem::take(&mut current),
        }];
    }
    current
}

/// 一个分支的首手：child 自身有着点取之；否则沿链下探首个有着点后裔
/// （纯注释节点夹在分叉点与着法之间时不丢续法；与 refbook 同口径）。
fn first_move_of(rn: &ScanRNode<'_>, size: Size) -> Option<(Stone, Action)> {
    match rn.node.move_action(size) {
        Ok(Some(pair)) => Some(pair),
        Ok(None) => rn.children.iter().find_map(|c| first_move_of(c, size)),
        Err(_) => None,
    }
}

/// 由参照树构造扫描计划。
///
/// - 待打分节点 = **有子节点**（库中有续法）且 `move_no ≤ max_move_no`
///   的局面；叶子无续法可评，不扫；
/// - 链分解：把全部待打分节点的「根→节点」路径按前缀归属到**极大链**
///   （快扫 `build_plan` 的同款判据：链可延长 = 存在另一条链以它为真
///   前缀），组路径 = 极大链路径，短链节点以 turn 下标搭车；
/// - 重放用 `rules::apply_move`（无劫争判定，与参照建索引同口径，劫争
///   循环着法不丢分支）；非法着法该分支舍弃（与建索引同口径，保证
///   「能匹配的都扫得到」）。
pub(super) fn build_ref_scan_plan(tree: &GameTree, size: Size, max_move_no: usize) -> RefScanPlan {
    // 根摆子与行棋方（口径与 refbook::walk 相同：PL 优先，有摆子白先）。
    let mut grid: Vec<Option<Stone>> = vec![None; size.point_count()];
    let mut setup: Vec<(Stone, Coord)> = Vec::new();
    let root_to_play = tree.root().map_or(Stone::Black, |root| {
        for (ident, stone) in [("AB", Stone::Black), ("AW", Stone::White)] {
            if let Ok(coords) = root.coords(ident, size) {
                for c in coords {
                    grid[c.index(size)] = Some(stone);
                    setup.push((stone, c));
                }
            }
        }
        match root.text("PL").map(str::trim) {
            Some("b") | Some("B") => Stone::Black,
            Some("w") | Some("W") => Stone::White,
            _ if root.get("AB").is_some() || root.get("AW").is_some() => Stone::White,
            _ => Stone::Black,
        }
    });

    // DFS 收集全部待打分节点（树序 = turns 天然升序）。
    let top = expand_scan_tree(tree);
    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut path: Vec<usize> = Vec::new();
    collect_nodes(
        &top,
        size,
        &grid,
        root_to_play,
        0,
        &[],
        &mut path,
        &mut Vec::new(),
        max_move_no,
        &mut nodes,
    );

    // 极大链归属（快扫同款 O(n²) 前缀比对：树规模下可忽略）。长链先
    // 处理：短链归属时其延长链必已入组。
    let mut chains: Vec<RefScanChain> = Vec::new();
    let mut node_chain: HashMap<Vec<usize>, usize> = HashMap::new();
    let mut sorted: Vec<&PlanNode> = nodes.iter().collect();
    sorted.sort_by_key(|node| std::cmp::Reverse(node.chain.len()));
    for node in sorted {
        match chains
            .iter()
            .position(|chain| chain.path.len() > node.chain.len() && starts_with(&chain.path, &node.chain))
        {
            Some(gi) => {
                node_chain.insert(node.path.clone(), gi);
            }
            None => {
                chains.push(RefScanChain {
                    path: node.chain.clone(),
                    turns: Vec::new(),
                    node_paths: Vec::new(),
                    libs: Vec::new(),
                    tails: Vec::new(),
                });
                node_chain.insert(node.path.clone(), chains.len() - 1);
            }
        }
    }
    for node in &nodes {
        if let Some(&gi) = node_chain.get(&node.path) {
            let chain = &mut chains[gi];
            chain.turns.push(node.move_no);
            chain.node_paths.push(node.path.clone());
            chain.libs.push(node.libs.clone());
            chain.tails.push(node.tail.clone());
        }
    }
    let positions = chains.iter().map(|c| c.turns.len()).sum();
    RefScanPlan {
        chains,
        positions,
        size,
        setup,
    }
}

/// `chain` 是否以 `prefix` 为前缀（快扫同款判定；`MoveRecord` 的
/// `captured` 恒空，不参与比较语义）。
fn starts_with(chain: &[MoveRecord], prefix: &[MoveRecord]) -> bool {
    chain.len() >= prefix.len() && chain[..prefix.len()] == *prefix
}

/// DFS 收集待打分节点（有子节点 + 手数在范围内）。
///
/// `chain` 为兄弟分支列表（与 refbook::walk 同构）；注释 / 空节点不产生
/// 局面、链延续到子节点。行棋方不符 / 非法着法该分支舍弃（不递归子树，
/// 兄弟分支继续）；超出 `max_move_no` 的节点不入集合、且**整支到此为
/// 止**——30 手截断之后代必然也超范围，无需再走。
#[allow(clippy::too_many_arguments)]
fn collect_nodes(
    chain: &[ScanRNode<'_>],
    size: Size,
    grid: &[Option<Stone>],
    to_play: Stone,
    move_no: usize,
    moves: &[MoveRecord],
    path: &mut Vec<usize>,
    tail: &mut Vec<String>,
    max_move_no: usize,
    out: &mut Vec<PlanNode>,
) {
    for (i, rn) in chain.iter().enumerate() {
        path.push(i);
        match rn.node.move_action(size) {
            Err(_) => {} // 坐标非法：舍弃该分支（与建索引同口径）。
            Ok(None) => {
                // 纯注释 / 空节点：不产生局面，链延续。
                collect_nodes(
                    &rn.children, size, grid, to_play, move_no, moves, path, tail,
                    max_move_no, out,
                );
            }
            Ok(Some((stone, action))) if stone == to_play => {
                let mut next = grid.to_vec();
                let placed = match action {
                    Action::Place(at) => {
                        rules::apply_move(size, &mut next, stone, at).map(|_| Some(at))
                    }
                    Action::Pass => Ok(None),
                };
                if let Ok(at_opt) = placed {
                    let next_no = move_no + 1;
                    let mut moves2 = moves.to_vec();
                    moves2.push(MoveRecord {
                        player: stone,
                        action,
                        captured: Vec::new(),
                    });
                    let mut tail2 = tail.clone();
                    tail2.push(format!(
                        "{} {}",
                        stone.name(),
                        at_opt.map_or_else(|| "弃着".to_owned(), |at| at.to_gtp(size))
                    ));
                    let start = tail2.len().saturating_sub(3);
                    let mut tail_show: Vec<String> = tail2[start..].to_vec();
                    // 范围内且有库中续法 ⇒ 待打分节点。
                    if next_no <= max_move_no && !rn.children.is_empty() {
                        let libs = rn
                            .children
                            .iter()
                            .filter_map(|child| first_move_of(child, size))
                            .map(|(player, action)| {
                                (
                                    player,
                                    match action {
                                        Action::Place(at) => Some(at.to_gtp(size)),
                                        Action::Pass => None,
                                    },
                                )
                            })
                            .collect();
                        out.push(PlanNode {
                            move_no: next_no,
                            chain: moves2.clone(),
                            path: path.clone(),
                            tail: tail_show.clone(),
                            libs,
                        });
                    }
                    // 链继续深入仅当仍在范围内（超范围整支放弃，见函数文档）。
                    if next_no <= max_move_no {
                        collect_nodes(
                            &rn.children,
                            size,
                            &next,
                            to_play.opposite(),
                            next_no,
                            &moves2,
                            path,
                            &mut tail_show,
                            max_move_no,
                            out,
                        );
                    }
                }
            }
            // 行棋方不符：舍弃该分支。
            Ok(Some(_)) => {}
        }
        path.pop();
    }
}

/// 参照打分在飞块的跟踪信息（快扫 `BatchChunk` 同构，任务独立）。
pub(super) struct RefScanChunk {
    pub(super) id: QueryId,
    /// 尚待收到的报告数（每 turn 一条，含 noResults 空报告）。
    pub(super) expect: usize,
    /// **心跳**：最近一次收到该块报告的时刻（快扫同款口径：距上次收到
    /// 报告超过 [`BATCH_CHUNK_STALL`] 才算僵死——慢不是死）。
    pub(super) last_report: Instant,
    /// 该块所属计划链的下标（报告按 turnNumber 在链路径上定位局面，
    /// 节点路径 / 库中续法都从链上按 turn 查回）。
    pub(super) chain: usize,
}

/// 参照打分任务（发起后驻留 [`AnalysisState`]，收齐即结算完成提示）。
pub(super) struct RefScanJob {
    pub(super) plan: RefScanPlan,
    /// 尚未派发的链下标（组间串行；组内按 [`BATCH_CHUNK`] 切块）。
    pub(super) chain: usize,
    /// 当前链内尚未派发的 turn 下标。
    pub(super) chain_pos: usize,
    pub(super) active: Option<RefScanChunk>,
    /// 已收到的逐节点评分（按树内路径键）。
    pub(super) results: RefScanResults,
    pub(super) done: usize,
    pub(super) started: Instant,
    pub(super) visits: u32,
    pub(super) komi: f64,
    pub(super) rules: String,
    pub(super) view: DisplayView,
}

impl RefScanJob {
    /// 该块是否已僵死（与快扫同一心跳口径，见 [`RefScanChunk::last_report`]）。
    pub(super) fn stalled(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|chunk| chunk.last_report.elapsed() >= BATCH_CHUNK_STALL)
    }
}

/// 离线构造扫描计划并给摘要（headless 验证「极大链分解是否正确」用，
/// 不发起任务；应用路径不调用）。返回
/// `(链数, 每链 turns 数, 总节点数, 根摆子数)`。
#[doc(hidden)]
pub fn ref_scan_plan_digest_of(
    tree: &GameTree,
    size: Size,
    max_move_no: usize,
) -> (usize, Vec<usize>, usize, usize) {
    let plan = build_ref_scan_plan(tree, size, max_move_no);
    (
        plan.chains.len(),
        plan.chains.iter().map(|c| c.turns.len()).collect(),
        plan.positions,
        plan.setup.len(),
    )
}

impl AnalysisState {
    // ---- 参照打分：发起 / 预估 / 进度 / 取消 / 提示 ----

    /// 当前参照打分配置（范围 + 思考量档位；对话框每帧读取）。
    pub fn ref_scan_config(&self) -> RefScanConfig {
        self.ref_scan_config
    }

    /// 写回参照打分配置（对话框改动即时生效；预估与发起共用同一份，
    /// 快扫 `set_batch_config` 同款模式）。
    pub fn set_ref_scan_config(&mut self, config: RefScanConfig) {
        self.ref_scan_config = config;
    }

    /// 构造参照打分预估（发起前确认框展示）：`(节点数, 预估秒数)`。
    ///
    /// 耗时口径与快扫一致：单线程 40 visits ≈ 0.94s/节点的实测外推
    /// （[`BATCH_SECS_PER_HAND_AT_40`] 同源），按所选 visits 线性放大，
    /// 多线程实际更快——预估偏高是有意的。
    pub fn ref_scan_estimate(
        tree: &GameTree,
        size: Size,
        config: RefScanConfig,
    ) -> (usize, f64) {
        let plan = build_ref_scan_plan(tree, size, config.max_move_no);
        let secs = plan.positions as f64 * BATCH_SECS_PER_HAND_AT_40
            * f64::from(config.visits.max(1))
            / 40.0;
        (plan.positions, secs)
    }

    /// 发起参照打分。`komi` / `game_rules` 在发起时刻定格（与快扫同一
    /// 精神：任务存续期间口径不变）。已在打分中 / 引擎未就绪 / 与快扫
    /// 冲突 / 无可扫节点时返回提示不发起（`Ok(())` = 已发起）。
    pub fn start_ref_scan(
        &mut self,
        tree: &GameTree,
        size: Size,
        komi: f64,
        game_rules: Option<&str>,
        config: RefScanConfig,
    ) -> Result<(), String> {
        if self.ref_scan.is_some() {
            return Err("参照库打分已在进行中。".to_owned());
        }
        if self.batch.is_some() {
            // 互斥（口径见模块文档）：两者都吃引擎队列，同时跑会把批量
            // 排队拖到不可用。
            return Err("整谱快扫正在进行中，无法同时发起参照库打分（两者共用引擎）。\
                        请先取消或等待快扫完成。"
                .to_owned());
        }
        if !matches!(self.engine, EngineStatus::Ready) {
            return Err("引擎未就绪，无法开始参照库打分。".to_owned());
        }
        let plan = build_ref_scan_plan(tree, size, config.max_move_no);
        if plan.positions == 0 {
            return Err("参照库里没有可打分的节点：范围内没有「有库中续法」的局面。\
                        可尝试放宽手数范围。"
                .to_owned());
        }
        self.ref_scan = Some(RefScanJob {
            plan,
            chain: 0,
            chain_pos: 0,
            active: None,
            results: HashMap::new(),
            done: 0,
            started: Instant::now(),
            visits: config.visits.max(1),
            komi,
            rules: self.effective_rules(game_rules),
            view: self.want_view,
        });
        self.dispatch_ref_scan_chunk();
        Ok(())
    }

    /// 取消参照打分（用户点取消；terminate 在飞块，未完成 turn 由引擎
    /// 补发 noResults 空报告后按空报告跳过）。**已收结果保留**（与提示
    /// 文案一致）：部分结果合入结果表，用户已可查看已评估的节点。
    pub fn cancel_ref_scan(&mut self) {
        let Some(mut job) = self.ref_scan.take() else { return };
        if let Some(chunk) = job.active.take()
            && let Some(handle) = self.handle.as_mut()
        {
            handle.terminate(chunk.id);
        }
        let elapsed = job.started.elapsed();
        self.ref_scan_results.extend(job.results.drain());
        self.ref_scan_notice = Some(format!(
            "参照库打分已取消：完成 {}/{} 节点，用时 {}（已收结果保留）。",
            job.done,
            job.plan.positions,
            format_ref_duration(elapsed),
        ));
    }

    /// 换参照 / 卸载参照时调用：取消在飞任务并**清空全部结果**（结果
    /// 描述的是「那一份库」，换库后无从对号；换参照即清空是结果生命
    /// 周期的唯一出口，见模块文档）。
    pub fn detach_ref_scan(&mut self) {
        if self.ref_scan.is_some() {
            self.cancel_ref_scan();
            self.ref_scan_notice = Some("参照已更换，参照库打分已取消。".to_owned());
        }
        self.ref_scan_results.clear();
    }

    /// 参照打分进度（侧栏显示）：`(done, total, elapsed)`；`None` = 空闲。
    pub fn ref_scan_progress(&self) -> Option<(usize, usize, std::time::Duration)> {
        self.ref_scan
            .as_ref()
            .map(|job| (job.done, job.plan.positions, job.started.elapsed()))
    }

    /// 取走打分结束提示（完成 / 取消 / 换参照），由 App 转用户可见消息。
    pub fn take_ref_scan_notice(&mut self) -> Option<String> {
        self.ref_scan_notice.take()
    }

    /// 已完成的打分结果（只读；`None` = 本会话尚未扫过或已随换参照清空）。
    pub fn ref_scan_results(&self) -> Option<&RefScanResults> {
        if self.ref_scan_results.is_empty() {
            None
        } else {
            Some(&self.ref_scan_results)
        }
    }

    /// 扫描计划的取证摘要（headless 验证「分解是否正确」用，应用路径
    /// 不调用）：`(链数, 每链 turns 数列表, 总节点数, 根摆子数)`。
    /// 进行中的任务取其计划；空闲时返回 `None`（不另存计划副本）。
    #[doc(hidden)]
    pub fn ref_scan_plan_digest(&self) -> Option<(usize, Vec<usize>, usize, usize)> {
        self.ref_scan.as_ref().map(|job| {
            (
                job.plan.chains.len(),
                job.plan.chains.iter().map(|c| c.turns.len()).collect(),
                job.plan.positions,
                job.plan.setup.len(),
            )
        })
    }

    /// 派发下一块（一次 `analyzeTurns` = 同一极大链上的 [`BATCH_CHUNK`]
    /// 个 turn；组间串行、收齐一块再派下一块——快扫同款调度）。
    pub(super) fn dispatch_ref_scan_chunk(&mut self) {
        let Some(job) = self.ref_scan.as_mut() else {
            return;
        };
        if job.active.is_some() {
            return;
        }
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        // 跳过空链，找到下一块。
        while job.chain < job.plan.chains.len() {
            let chain = &job.plan.chains[job.chain];
            let to = (job.chain_pos + BATCH_CHUNK).min(chain.turns.len());
            if job.chain_pos < to {
                let turns: Vec<usize> = chain.turns[job.chain_pos..to].to_vec();
                let expect = turns.len();
                let path = chain.path.clone();
                let id = Self::send_batch_query(
                    handle,
                    job.plan.size,
                    job.komi,
                    &job.plan.setup,
                    &path,
                    &turns,
                    job.visits,
                    &job.rules,
                    job.view,
                );
                job.active = Some(RefScanChunk {
                    id,
                    expect,
                    last_report: Instant::now(),
                    chain: job.chain,
                });
                job.chain_pos = to;
                return;
            }
            job.chain += 1;
            job.chain_pos = 0;
        }
        // 全部链派发完：等最后一块报告收齐后由 sync 的完成判定收尾。
    }

    /// 参照打分的批量终态收尾：完成数报满则生成完成提示；未满（卡死
    /// 兜底被调用）则按实际完成数生成部分完成提示。在飞块必须 terminate
    /// （快扫同款理由：卡死收尾时引擎多半还在算，结果没人收）。
    pub(super) fn finish_ref_scan(&mut self, complete: bool) {
        let Some(job) = self.ref_scan.take() else { return };
        if let Some(chunk) = job.active
            && let Some(handle) = self.handle.as_mut()
        {
            handle.terminate(chunk.id);
        }
        let total = job.plan.positions;
        let elapsed = job.started.elapsed();
        let outdated = job
            .results
            .values()
            .filter(|score| score.outdated_count() > 0)
            .count();
        self.ref_scan_notice = Some(if complete {
            format!(
                "参照库打分完成：{total} 节点，其中 {outdated} 个节点的库中续法\
                 被 AI 判劣（详见侧栏参照卡片）。用时 {}。",
                format_ref_duration(elapsed)
            )
        } else {
            format!(
                "参照库打分已结束：完成 {done}/{total} 节点（部分块超时被跳过），\
                 其中 {outdated} 个节点的库中续法被 AI 判劣。用时 {}。",
                format_ref_duration(elapsed),
                done = job.done,
            )
        });
        // 结果落位（本会话内保存；换参照由 detach_ref_scan 清空）。
        self.ref_scan_results = job.results;
    }

    /// 参照打分报告回填 + 计数。返回 true = 在飞块已收齐全部 turn 报告
    /// （含 noResults 空报告），可派发下一块（快扫同款协议）。
    pub(super) fn on_ref_scan_report(
        &mut self,
        job: &mut RefScanJob,
        id: QueryId,
        report: crate::engine::AnalysisReport,
    ) -> bool {
        // id 不符 = 已被取消的旧块补发（terminate 后引擎仍会补发终态），丢弃。
        if job.active.as_ref().is_none_or(|chunk| chunk.id != id) {
            return false;
        }
        if !report.no_results
            && let Some(root) = &report.root_info
        {
            // 按 turnNumber 在本块链上定位节点（多 turn 查询的到达顺序
            // 不保证——快扫实测，回填必须按报告自带 turnNumber）。
            let chain_idx = job.active.as_ref().expect("id 已核对").chain;
            let chain = &job.plan.chains[chain_idx];
            if let Some(idx) = chain.turns.iter().position(|&t| t == report.turn_number) {
                let size = job.plan.size;
                let score = score_report(
                    &report.move_infos,
                    root,
                    &chain.libs[idx],
                    size,
                    &chain.node_paths[idx],
                    chain.turns[idx],
                    &chain.tails[idx],
                );
                job.results.insert(score_key(&chain.node_paths[idx]), score);
                job.done += 1;
            }
        }
        // 空报告（noResults，terminate 后未完成 turn 的补发）无数据，只计数。
        if let Some(chunk) = job.active.as_mut() {
            chunk.expect = chunk.expect.saturating_sub(1);
            // 心跳：每收到一条报告（含空报告）都刷新。
            chunk.last_report = Instant::now();
        }
        job.active.as_ref().is_none_or(|chunk| chunk.expect == 0)
    }
}

/// 结果表键：树内路径的下标串（「0-2-1」；根 = 空串）。
fn score_key(path: &[usize]) -> String {
    path.iter().map(ToString::to_string).collect::<Vec<_>>().join("-")
}

/// 由一条终态报告给一个节点打分（纯函数，便于核对口径）。
///
/// - 引擎首选 = `moveInfos` 中 `order` 最小者（落点 + 黑方胜率 + 黑方
///   目差，存储口径恒黑视角）；
/// - 库中续法按 GTP 落点对回 `moveInfos`；不在候选表 = 「引擎未列出」
///   （引擎只列访问量较高的候选），**如实记录、不臆造数值、不判淘汰**；
/// - 行棋方视角差距 = (首选 − 续法) × 行棋方符号（黑方视角差对白行棋
///   局面要翻转才是「行棋方亏多少」；口径与 [`super::types::loss_from_points`]
///   同一换算）；
/// - 差距达 [`REFSCAN_OUTDATED_SCORE`] 目**或** [`REFSCAN_OUTDATED_WINRATE`]
///   胜率 = 「已被 AI 淘汰」。
fn score_report(
    infos: &[crate::engine::MoveInfo],
    root: &crate::engine::RootInfo,
    libs: &[(Stone, Option<String>)],
    size: Size,
    path: &[usize],
    move_no: usize,
    tail: &[String],
) -> RefNodeScore {
    // 引擎首选（order 最小；弃着也可为首选）。
    let best = infos
        .iter()
        .min_by_key(|info| info.order)
        .map(|info| {
            (
                info.mv.map_or_else(|| "弃着".to_owned(), |at| at.to_gtp(size)),
                info.winrate,
                info.score_lead,
            )
        });
    // 行棋方符号：黑 +1 / 白 −1（黑方视角差 → 行棋方视角差）。
    let sign = match root.current_player {
        Stone::Black => 1.0,
        Stone::White => -1.0,
    };
    let continuations = libs
        .iter()
        .map(|(player, at)| {
            let text = at.as_deref().map_or_else(
                || format!("{} 弃着", player.name()),
                |at_text| format!("{} {at_text}", player.name()),
            );
            // 按落点对回候选表（弃着永不命中：候选表的弃着项与库中弃着
            // 即便同现也不构成有效对比，如实记未列出）。
            let eval = at.as_deref().and_then(|at_text| {
                infos.iter().find_map(|info: &crate::engine::MoveInfo| {
                    info.mv
                        .map(|c| c.to_gtp(size) == at_text)
                        .unwrap_or(false)
                        .then_some((info.winrate, info.score_lead))
                })
            });
            let (gap, outdated) = match (eval, &best) {
                (Some((wr, lead)), Some((_, bwr, blead))) => {
                    let gap_wr = (bwr - wr) * sign;
                    let gap_lead = (blead - lead) * sign;
                    // 展示 / 排序只用**目差**（gap_wr 是 0–1 的比例，与目
                    // 量纲不同，绝不能与 gap_lead 取 max——曾经混用导致
                    // 「差距 0.3 目」实为「胜率差 30%」的误读）。
                    (Some(gap_lead), outdated_by(gap_wr, gap_lead))
                }
                _ => (None, false),
            };
            RefContinuationScore { text, eval, gap, outdated }
        })
        .collect();
    RefNodeScore {
        move_no,
        path: path.to_vec(),
        tail: tail.to_vec(),
        to_play: root.current_player,
        best,
        continuations,
        visits: root.visits,
    }
}

/// 「已淘汰」判定（目差或胜率差任一达阈值）。
fn outdated_by(gap_wr: f64, gap_lead: f64) -> bool {
    gap_lead >= REFSCAN_OUTDATED_SCORE || gap_wr >= REFSCAN_OUTDATED_WINRATE
}

/// 时长的人类可读形式（打分进度与完成提示共用）。
fn format_ref_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{secs} 秒")
    }
}
