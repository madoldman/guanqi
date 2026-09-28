//! 整谱快扫：批量分析当前线（或全树）的终局局面并回填逐手历史。
//!
//! 两阶段任务（主扫描 → 差异手加深）的计划、预估、派发、进度与收尾，
//! 协议口径见 [`BatchJob`] 文档。

use std::collections::HashMap;
use std::time::Instant;

use crate::board::{Action, Board, Coord, MoveRecord, Size, Stone};
use crate::engine::{AnalysisQuery, AnalysisReport, Engine, QueryId, WinrateView};

use super::display::DisplayView;
use super::types::EngineStatus;
use super::types::{loss_from_points, position_sig};
use super::AnalysisState;

/// 整谱快扫的每手搜索量：实测 b18 OpenCL 下约 0.94s/手（threads=1），
/// 80 手 ≈ 75 秒、250 手 ≈ 4 分钟；40 visits 的胜率/目差已稳定在
/// 好棋/尚可分界（0.3 目）的噪声量级内，够填曲线用（实测算依据见
/// /tmp/batch-notes.md §4）。
pub const BATCH_VISITS: u32 = 40;

/// 整谱快扫的交互查询优先级：实测带 `priority: 10` 的查询可在批量占满
/// 队列时数秒内插队返回（不带则被完全阻塞，见 /tmp/batch-notes.md §3）。
pub(crate) const INTERACTIVE_PRIORITY: i32 = 10;

/// 批量查询优先级：缺省 0（低于 [`INTERACTIVE_PRIORITY`]，排队即可）。
/// 参照打分（`refscan`）复用同一档：同为后台批量，交互查询随时可插队。
pub(super) const BATCH_PRIORITY: i32 = 0;

/// 整谱快扫的分块大小（一次 `analyzeTurns` 的手数）：实测 40 visits 约
/// 0.94s/手，20 手一块 ≈ 19 秒，取消响应与进度刷新粒度都足够；块间串行
/// （引擎 search tree 跨查询缓存还能让相邻块的重叠局面加速）。
/// 参照打分（`refscan`）复用同一块大小（同为「极大链 + turns」的
/// `analyzeTurns` 查询，块粒度口径天然一致）。
pub(super) const BATCH_CHUNK: usize = 20;

// ---- 整谱快扫配置（LizzieYzy「闪电分析设置」的移植口径）----

/// 「只扫一方」：只分析**轮到该方行棋**的局面。线上第 t 手局面的行棋方
/// 由 `line_records()[t].player` 给出（该记录的行棋方 = 走出该手的一方，
/// 亦即「走了 t 手之后」局面的行棋方，与 `game_summary` 的匹配方向注释
/// 同一口径）。只扫一方可把主扫描耗时砍半。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchSide {
    /// 全部局面。
    All,
    /// 只扫轮到黑方的局面。
    BlackOnly,
    /// 只扫轮到白方的局面。
    WhiteOnly,
}

impl BatchSide {
    /// 行棋方为 `player` 的局面是否入选。
    fn accepts(self, player: Stone) -> bool {
        match self {
            Self::All => true,
            Self::BlackOnly => player == Stone::Black,
            Self::WhiteOnly => player == Stone::White,
        }
    }

    /// 侧栏显示名。
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::BlackOnly => "只黑",
            Self::WhiteOnly => "只白",
        }
    }
}

/// 整谱快扫配置（侧栏「整谱快扫」卡片编辑，经 `App` 转存进
/// [`AnalysisState`]；发起与预估时按当前线重新钳制。配置不随换谱重置：
/// `to` 的「跟随线尾」哨兵让换到更长的棋时依旧全额）。
#[derive(Clone, Debug, PartialEq)]
pub struct BatchConfig {
    /// 起手（0 基局面序号：分析「走了 from 手之后」的局面）。侧栏按
    /// 1 基手数显示编辑（起手 = from + 1）。
    pub from: usize,
    /// 止手（**不含**；[`usize::MAX`] = 跟随线尾，显示与发起时按线长
    /// 钳制）。
    pub to: usize,
    /// 主扫描每手 visits（默认 [`BATCH_VISITS`]）。
    pub visits: u32,
    /// 只扫一方（[`BatchSide`]）。
    pub side: BatchSide,
    /// 含变着：走**全树 DFS**，对每个节点分析「到该节点为止的路径 +
    /// turn = 该节点深度」，代价随分支数线性增长 ⇒ 发起前必须看预估。
    /// 参考 LizzieYzy `AnalysisEngine.startRequestAllBranches()`。
    pub include_variations: bool,
    /// 扫完自动加深差异手（默认开）：主扫描跑完后按**胜率损失降序**
    /// 取前 N 手，用更高的 visits 重扫那些局面的走子前后两端。历史
    /// 回填「visits 更高才覆盖」（`record_history`）使加深结果自动
    /// 替换浅扫描的点，无需新机制；候选表与 root 同一次写入的口径
    /// 由同一条回填路径保证。目的：治「40 visits 下吻合度大量并列 0、
    /// 差异手排序噪声大」的毛病。
    pub deepen_enabled: bool,
    /// 加深取前 N 手差异手（默认 10）。
    pub deepen_top: usize,
    /// 加深每手 visits（默认 300）。
    pub deepen_visits: u32,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            from: 0,
            to: usize::MAX,
            visits: BATCH_VISITS,
            side: BatchSide::All,
            include_variations: false,
            deepen_enabled: true,
            deepen_top: 10,
            deepen_visits: 300,
        }
    }
}

/// 一条待分析的批量局面：`turn` 为 0 基局面序号（= 查询 `analyzeTurns`
/// 的元素）。
///
/// 不存行棋方：「只扫一方」的过滤在 [`build_plan`] 建计划时就做完了，
/// 而**加深阶段**的损失口径改为按当前线算（见 `plan_deepen`）后，行棋方
/// 由当前线的着法自己给出——存一份副本只会成为没人读的死字段。
#[derive(Clone, Debug)]
pub(super) struct BatchTurn {
    pub(super) turn: usize,
}

/// 一组**共用同一条着法路径**的待分析局面。一次 `analyzeTurns` 查询
/// 只能带一条路径，组内 turn 即该路径上的下标；同手数的不同分支节点
/// 是不同路径，绝不能并进同一查询（否则引擎按首组路径重放，turn 语义
/// 全错）。
#[derive(Debug)]
pub(super) struct BatchGroup {
    /// 查询 `moves`（根 → 本组最深处）。
    pub(super) path: Vec<MoveRecord>,
    /// 组内局面（`analyzeTurns` 升序且不允许重复 turn）。
    pub(super) turns: Vec<BatchTurn>,
}

/// 快扫计划：发起前一次性算好（[`build_plan`]），预估与派发共用，
/// 保证「界面承诺扫什么」与「实际派发什么」是同一份数据。
#[derive(Debug)]
pub(super) struct BatchPlan {
    /// 主扫描分组（线性模式恒 1 组；含变着模式按路径分组）。
    pub(super) groups: Vec<BatchGroup>,
    /// 全树节点数（含根；「含变着」预估展示用）。
    pub(super) tree_nodes: usize,
    /// 主扫描局面总数（各组 turns 之和，已按「只扫一方」过滤）。
    pub(super) positions: usize,
}

/// 由棋盘与配置构造快扫计划（侧栏预估与 [`AnalysisState::start_batch`]
/// 共用同一实现）。`from`/`to` 在此按当前线钳制；线性模式的 `moves`
/// 取整条线（批量查询的 moves 必须覆盖到最深 analyzeTurn，见
/// [`BatchJob::watch`] 文档）。模块内私有：计划只在本模块消费，
/// 对外仅暴露 [`batch_estimate`] 的标量结果。
fn build_plan(board: &Board, cfg: &BatchConfig) -> BatchPlan {
    let len = board.line_len();
    let from = cfg.from.min(len);
    let to = cfg.to.min(len).max(from);
    let line = board.line_records();
    let tree_nodes = board.nodes().len();
    let mut groups: Vec<BatchGroup> = Vec::new();
    if cfg.include_variations {
        // 全树 DFS（显式栈，避免深谱递归爆栈；LizzieYzy
        // startRequestAllBranches 同构）：每个节点 = 一个待分析局面，
        // turn = 节点深度；行棋方 = 该节点着法方的对方（走出该手的人的
        // 对手），第 0 手局面的行棋方 = 首手行棋方（根节点无着法可推）。
        //
        // 分组：一条 `analyzeTurns` 查询只带一条路径，因此「链互为前缀」
        // 的节点并入同组（组路径 = 其中最长链，短链节点以 turn 下标搭车）。
        // 组 = 不可再延长的极大链。
        let nodes = board.nodes();
        let root_player = line.first().map_or(Stone::Black, |r| r.player);
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        // 收集全部待分析节点：节点 id、深度、节点链（根→节点）。
        // 行棋方只在下面判断「只扫一方」时用一次，判断完即弃（加深阶段的
        // 损失口径改按当前线算，不再需要它随计划留存）。
        let mut entries: Vec<(usize, usize, Vec<usize>)> = Vec::new();
        let mut chains: Vec<Vec<usize>> = Vec::new();
        while let Some((id, depth)) = stack.pop() {
            for &child in nodes[id].children() {
                stack.push((child, depth + 1));
            }
            if depth < from || depth >= to {
                continue;
            }
            let player = if depth == 0 {
                root_player
            } else {
                nodes[id]
                    .record()
                    .map_or(root_player, |record| record.player.opposite())
            };
            if !cfg.side.accepts(player) {
                continue;
            }
            let mut chain: Vec<usize> = Vec::new();
            let mut cur = Some(id);
            while let Some(node) = cur {
                chain.push(node);
                cur = nodes[node].parent();
            }
            chain.reverse();
            entries.push((id, depth, chain.clone()));
            chains.push(chain);
        }
        // 极大链判定：链 C 可延长 = 存在另一条链以 C 为真前缀。
        // 每条链归属其极大延长链所在组（组路径 = 极大链）。O(n²) 前缀
        // 比对在树规模（≤ 全树节点数）下可忽略。
        chains.sort();
        chains.dedup();
        // chain → 组下标（None = 尚未归属）。
        let mut chain_group: HashMap<&Vec<usize>, usize> = HashMap::new();
        // 长链先处理：短链归属时其延长链必已入组。
        for chain in chains.iter().rev() {
            let extended = chains
                .iter()
                .filter(|other| other.len() > chain.len() && other.starts_with(chain))
                .min();
            match extended.and_then(|e| chain_group.get(e)) {
                Some(&gi) => {
                    chain_group.insert(chain, gi);
                }
                None => {
                    // 极大链：新组，路径 = 本链（后续短链成员不改路径）。
                    let gi = groups.len();
                    let path: Vec<MoveRecord> = chain
                        .iter()
                        .filter_map(|&id| nodes[id].record().cloned())
                        .collect();
                    groups.push(BatchGroup {
                        path,
                        turns: Vec::new(),
                    });
                    chain_group.insert(chain, gi);
                }
            }
        }
        for (_, depth, chain) in &entries {
            if let Some(&gi) = chain_group.get(chain) {
                groups[gi].turns.push(BatchTurn { turn: *depth });
            }
        }
        for group in &mut groups {
            group.turns.sort_by_key(|t| t.turn);
        }
    } else {
        // 线性模式：单组、路径 = 整条线，turn 过滤「只扫一方」。
        let turns = line[from..to]
            .iter()
            .enumerate()
            .filter(|(_, record)| cfg.side.accepts(record.player))
            .map(|(i, _)| BatchTurn { turn: from + i })
            .collect();
        groups.push(BatchGroup {
            path: line.to_vec(),
            turns,
        });
    }
    let positions = groups.iter().map(|g| g.turns.len()).sum();
    BatchPlan {
        groups,
        tree_nodes,
        positions,
    }
}

/// 快扫预估（侧栏发起前显示；「含变着」时局面数如实外推到全树，
/// 宁可劝退也不让用户盲等）。
#[derive(Clone, Copy, Debug)]
pub struct BatchEstimate {
    /// 主扫描局面数（过滤后）。
    pub positions: usize,
    /// 全树节点数（含根；线性模式 = 线长 + 1，仅供对照）。
    pub tree_nodes: usize,
    /// 预计总耗时（秒；主扫描 + 加深的最坏情况，单线程口径外推）。
    pub secs: f64,
}

impl BatchEstimate {
    /// 局面是否多到值得劝退（经验线：300 个局面 ≈ 单线程 40 visits
    /// 5 分钟量级，8 线程约减半；超过即提示缩小范围）。
    pub fn heavy(&self) -> bool {
        self.positions > 300
    }
}

/// 单局面耗时外推依据：b18 OpenCL、threads=1 实测 40 visits ≈ 0.94s/手
/// （[`BATCH_VISITS`] 文档引用的实测），按 visits 线性放大。参照打分
/// 的预估复用同一实测基准（同引擎同档 visits，吞吐同源）。
pub(super) const BATCH_SECS_PER_HAND_AT_40: f64 = 0.94;

/// 预估总耗时（主扫描 + 可选加深；加深按「前 N 手 × 走子前后两端」的
/// 最坏情况计）。单线程口径，多线程实际更快——预估偏高是有意的。
pub fn batch_estimate(board: &Board, cfg: &BatchConfig) -> BatchEstimate {
    let plan = build_plan(board, cfg);
    let per = |visits: u32| BATCH_SECS_PER_HAND_AT_40 * f64::from(visits) / 40.0;
    let mut secs = plan.positions as f64 * per(cfg.visits);
    if cfg.deepen_enabled {
        let hands = cfg.deepen_top.min(plan.positions);
        secs += (hands * 2) as f64 * per(cfg.deepen_visits);
    }
    BatchEstimate {
        positions: plan.positions,
        tree_nodes: plan.tree_nodes,
        secs,
    }
}

/// 批量在飞块：一次 `analyzeTurns` 查询的跟踪信息。
pub(super) struct BatchChunk {
    pub(super) id: QueryId,
    /// 尚待收到的报告数（每 turn 一条，含 noResults 空报告）。
    pub(super) expect: usize,
    /// **心跳**：最近一次收到该块报告的时刻。卡死判定只看它——
    /// 「距上次收到报告超过 [`BATCH_CHUNK_STALL`]」才算僵死。
    ///
    /// 理由（不能从派发时刻起算）：快扫期间用户连续浏览会发交互查询
    /// （`priority=10`），引擎逐个让路，20 手一块被拖过 90s 是**正常慢**，
    /// 从派发时刻计时会把慢误判成死、整批误收尾。改为心跳后：只要还在
    /// 断续出报告（哪怕 10 秒一条）就不放弃；真正僵死（查询被拒不补发、
    /// 引擎半死）才是连续 90s 零报告。块收齐时的等待上限自然放宽为
    /// 「90s × 20 手」量级，可接受——兜底的意义是永不悬挂，不是限时完成。
    pub(super) last_report: Instant,
    /// 该块查询用的着法路径：报告按 `turnNumber` 在**这条路径**上定位
    /// 局面签名（主扫描组与加深组的路径可以不同，不能用任务级单一路径）。
    pub(super) path: Vec<MoveRecord>,
    /// 是否加深阶段的块（进度分阶段计数）。
    pub(super) deep: bool,
}

/// 在飞块的**心跳**判定线：距最近一次收到该块报告超过 90s 才算僵死。
/// （非「派发后 90s 未收齐」——快扫期间交互查询插队会把单块拖慢数倍，
/// 慢不是死；判定口径见 [`BatchChunk::last_report]。）数值取法：20 手
/// @0.94s ≈ 19s，留交互插队（实测 3–6s）与引擎波动余量后取 90s。
pub(super) const BATCH_CHUNK_STALL: std::time::Duration = std::time::Duration::from_secs(90);

/// 整谱快扫任务状态（两阶段：主扫描 → 差异手加深）。
///
/// **阶段 A（主扫描）**按 [`BatchPlan`] 的分组逐块派发（一次
/// `analyzeTurns` 一块 = 同一路径上的若干 turn，块间串行；报告按
/// `turnNumber` 回填逐手历史）；**阶段 B（加深）**取胜率损失最大的
/// 前 N 手（与侧栏「局后统计」同一排序口径，来自 [`loss_from_points`]），
/// 用更高的 visits 重扫那些局面的走子前后两端（turn − 1 与 turn）——
/// 历史回填的「visits 更高才覆盖」规则（[`AnalysisState::record_history`]）
/// 使加深结果自动替换浅扫描的点，40 visits 下并列 0 的差异手排序噪声
/// 随之收敛，无需新机制。
///
/// 协议事实（/tmp/batch-notes.md，实测口径沿用原实现）：
/// - 多 turn 查询逐 turn 返回终态，但**到达顺序不保证**（threads≥2 时
///   实测 turn 2 先于 turn 0）⇒ 回填必须按报告自带 `turnNumber` 定位
///   局面签名；
/// - 批量会占满引擎队列，交互查询须带高 `priority` 插队（实测有效）；
/// - terminate 多 turn 查询时，已完成 turn 正常补发、未完成 turn 补发
///   `noResults` 空报告 ⇒ 回填跳过空报告即可。
pub(super) struct BatchJob {
    /// 尚未派发的主扫描分组下标（组间串行；组内再按
    /// [`BATCH_CHUNK`] 切块）。
    pub(super) group: usize,
    /// 当前组内尚未派发的 turn 下标（切进 `groups[group].turns`）。
    pub(super) group_pos: usize,
    /// 在飞块（收齐其全部 turn 报告后才派发下一块）。
    pub(super) active: Option<BatchChunk>,
    /// 主扫描计划（发起时构造，与预估共用同一实现）。
    pub(super) plan: BatchPlan,
    /// 发起时**当前线**的着法序列（`line_records()`）。
    ///
    /// 加深阶段必须用**这条**路径，不能用「计划里最后一条极大链」：含变着
    /// 模式下后者可能是一条分支，而加深选中的差异手是按当前线算出来的
    /// ⇒ 会发出 `analyzeTurns` 超出该分支长度 的非法查询（实测引擎回
    /// `Invalid turn number: N`，查询被拒、该块永远收不齐，快扫最后以
    /// 「部分完成」收场、加深全部白做）。批量期间局面一变即取消，故此线
    /// 在任务存续期内稳定。
    pub(super) line_path: Vec<MoveRecord>,
    /// 发起时的局面（`records()`，到游标）：批量期间游标 / 线一变即取消。
    pub(super) watch: Vec<MoveRecord>,
    /// 发起时的棋盘尺寸与贴目（块间跨帧，随查询重发）。
    pub(super) size: Size,
    pub(super) komi: f64,
    /// 发起时的预设摆子（`root_setup()` 快照）：每块批量查询都随 moves
    /// 一并发出，取值口径见 `send_batch_query`。
    pub(super) setup: Vec<(Stone, Coord)>,
    /// 发起时解析生效的规则规范名（整谱快扫与交互分析同口径）。
    pub(super) rules: String,
    /// 发起时的目数视角（随查询发给引擎）。
    pub(super) view: DisplayView,
    /// 发起时的快扫配置（加深参数从中取）。
    pub(super) config: BatchConfig,
    /// 阶段：`false` = 主扫描，`true` = 加深（主扫描全部收齐后进入）。
    pub(super) deepening: bool,
    /// 加深待扫局面（turn 0 基，去重升序；进入加深阶段时一次算好）。
    pub(super) deepen_turns: Vec<usize>,
    /// 加深尚未派发的 turn 下标。
    pub(super) deepen_pos: usize,
    /// 主扫描已完成局面数（进度分母 = plan.positions）。
    pub(super) done: usize,
    /// 加深已完成局面数（进度分母 = deepen_turns.len()）。
    pub(super) deep_done: usize,
    pub(super) started: Instant,
}

impl AnalysisState {
    // ---- 整谱快扫（批量分析当前线）----

    /// 当前整谱快扫配置（侧栏卡片显示编辑用）。
    pub fn batch_config(&self) -> &BatchConfig {
        &self.batch_config
    }

    /// 更新整谱快扫配置（侧栏卡片编辑后由 App 转存）。
    pub fn set_batch_config(&mut self, cfg: BatchConfig) {
        self.batch_config = cfg;
    }

    /// 发起整谱快扫（两阶段：主扫描 → 差异手加深）。计划按当前配置与
    /// 当前线 / 全树现场构造（[`build_plan`]，与侧栏预估共用同一实现，
    /// 「承诺扫什么 = 实际派发什么」）；配置的起止手数在此按当前线钳制。
    /// `game_rules` 为棋谱 `RU[]` 原始串（规则解析与交互查询同一入口）。
    /// 已在批量中时幂等忽略；引擎未就绪 / 无局面可扫时返回提示不发起。
    pub fn start_batch(&mut self, board: &Board, komi: f64, game_rules: Option<&str>) -> Option<String> {
        if self.batch.is_some() {
            return Some("整谱快扫已在进行中。".to_owned());
        }
        // 与参照打分互斥（对称守卫，口径见 `refscan` 模块文档）：两者都
        // 吃引擎队列，同时跑会把批量排队拖到不可用。
        if self.ref_scan.is_some() {
            return Some("参照库打分正在进行中，无法同时发起整谱快扫（两者共用引擎）。\
                         请先取消或等待打分完成。"
                .to_owned());
        }
        if !matches!(self.engine, EngineStatus::Ready) {
            return Some("引擎未就绪，无法开始整谱快扫。".to_owned());
        }
        // 起止手数在计划构造内按当前线钳制（配置本身保持用户的设置）。
        let plan = build_plan(board, &self.batch_config);
        if plan.positions == 0 {
            return Some("没有可扫描的局面：请检查起止手数与「只扫一方」设置。".to_owned());
        }
        // 记录当前局面（变化即取消）：批量期间局面一变（切分支 / 切副本 /
        // 载谱 / 落子 / 导航）即自动取消，防止把 A 盘面的报告回填进 B
        // 盘面的历史。
        self.batch = Some(BatchJob {
            group: 0,
            group_pos: 0,
            active: None,
            plan,
            line_path: board.line_records().to_vec(),
            watch: board.records().to_vec(),
            size: board.size(),
            komi,
            setup: {
                let (black, white) = board.root_setup();
                black
                    .into_iter()
                    .map(|at| (Stone::Black, at))
                    .chain(white.into_iter().map(|at| (Stone::White, at)))
                    .collect()
            },
            // 规则 / 视角在发起时刻定格：任务存续期间口径不变（局面一变
            // 即取消，不存在口径漂移窗口）。
            rules: self.effective_rules(game_rules),
            view: self.want_view,
            config: self.batch_config.clone(),
            deepening: false,
            deepen_turns: Vec::new(),
            deepen_pos: 0,
            done: 0,
            deep_done: 0,
            started: Instant::now(),
        });
        self.dispatch_batch_chunk();
        None
    }

    /// 取消整谱快扫（用户点取消或局面变化自动取消）：terminate 在飞块，
    /// 未完成 turn 由引擎补发 noResults 空报告，回填时按空报告跳过。
    /// 主扫描与加深两阶段中的任意一处都由此中止（任务整体被取走）。
    pub fn cancel_batch(&mut self) {
        let Some(job) = self.batch.take() else { return };
        if let Some(chunk) = job.active
            && let Some(handle) = self.handle.as_mut()
        {
            handle.terminate(chunk.id);
        }
        let elapsed = job.started.elapsed();
        let stage = if job.deepening { "加深" } else { "主扫描" };
        self.batch_notice = Some(format!(
            "整谱快扫已取消（{stage}阶段）：完成 {}/{} 手，用时 {}。",
            job.done,
            job.plan.positions,
            format_duration(elapsed),
        ));
    }

    /// 批量进度（侧栏显示）。两阶段分别回报：
    /// - 主扫描：`(false, done, plan.positions, elapsed)`；
    /// - 加深：`(true, deep_done, deepen_turns.len(), elapsed)`。
    pub fn batch_progress(&self) -> Option<(bool, usize, usize, std::time::Duration)> {
        self.batch.as_ref().map(|job| {
            if job.deepening {
                (
                    true,
                    job.deep_done,
                    job.deepen_turns.len(),
                    job.started.elapsed(),
                )
            } else {
                (false, job.done, job.plan.positions, job.started.elapsed())
            }
        })
    }

    /// 取走批量结束提示（完成或取消），由 App 转为用户可见消息。
    pub fn take_batch_notice(&mut self) -> Option<String> {
        self.batch_notice.take()
    }

    /// 派发下一块（一次 `analyzeTurns` = 同一路径上的 [`BATCH_CHUNK`]
    /// 个 turn；主扫描按计划分组逐组推进，组收齐后进入加深阶段，加深
    /// 按去重 turn 列表切块，块间串行）。
    pub(super) fn dispatch_batch_chunk(&mut self) {
        let Some(job) = self.batch.as_mut() else {
            return;
        };
        if job.active.is_some() {
            return;
        }
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        if !job.deepening {
            // 主扫描：跳过空组，找到当前组内的下一块。
            while job.group < job.plan.groups.len() {
                let turns_from = job.group_pos;
                let turns_to =
                    (turns_from + BATCH_CHUNK).min(job.plan.groups[job.group].turns.len());
                if turns_from < turns_to {
                    let group = &job.plan.groups[job.group];
                    let turns: Vec<usize> = group.turns[turns_from..turns_to]
                        .iter()
                        .map(|t| t.turn)
                        .collect();
                    let expect = turns.len();
                    let path = group.path.clone();
                    let id = Self::send_batch_query(
                        handle,
                        job.size,
                        job.komi,
                        &job.setup,
                        &path,
                        &turns,
                        job.config.visits,
                        &job.rules,
                        job.view,
                    );
                    job.active = Some(BatchChunk {
                        id,
                        expect,
                        last_report: Instant::now(),
                        path,
                        deep: false,
                    });
                    job.group_pos = turns_to;
                    return;
                }
                job.group += 1;
                job.group_pos = 0;
            }
            // 主扫描全部派发完：等最后一块报告收齐后进入加深（在
            // `on_event` 的收齐分支里转段），这里无事可做。
            return;
        }
        // 加深阶段：每次派发一个局面（走子前后两端），turn 连续两值
        // 同块同路径，块间串行。列表规模 ≤ deepen_top，无需再分大块。
        let Some(&turn) = job.deepen_turns.get(job.deepen_pos) else {
            return;
        };
        // 加深一律在**当前线**上做：选中的损失就是按当前线算的（见
        // `plan_deepen`），且 turn 必然落在该路径长度内 ⇒ 含变着模式下
        // 不会发出越界查询（曾经误用「计划里最后一条极大链」当路径，
        // 那条可能是分支，实测会被引擎以 `Invalid turn number` 拒掉，
        // 该块永远收不齐、快扫最后以「部分完成」收场）。
        let path = job.line_path.clone();
        // 深扫的是「差异手的走子前后两端」：turn 与 turn − 1。
        let turns: Vec<usize> = if turn == 0 {
            vec![0]
        } else {
            vec![turn - 1, turn]
        };
        let expect = turns.len();
        let id = Self::send_batch_query(
            handle,
            job.size,
            job.komi,
            &job.setup,
            &path,
            &turns,
            job.config.deepen_visits,
            &job.rules,
            job.view,
        );
        job.active = Some(BatchChunk {
            id,
            expect,
            last_report: Instant::now(),
            path,
            deep: true,
        });
        job.deepen_pos += 1;
    }

    /// 发送一条批量查询（共用组装：路径 → moves，analyzeTurns，批量
    /// 优先级，不开流式、不要 ownership/policy——批量只填曲线与吻合度
    /// 候选表，省 60%+ 报告体积）。规则 / 视角随查询带上（口径与交互
    /// 查询一致，快扫曲线才能与交互分析的曲线同源比较）。`turn 0` 即
    /// 预设局面，因此摆子必须一并带上（turn 编号以真实着法计，摆子不占
    /// turn），否则含摆子谱的快扫曲线整体错位。
    ///
    /// `pub(super)`：参照打分（`refscan`）的「极大链 + turns」查询与本
    /// 函数同构（同一组装口径：摆子随查询、批量优先级、不开流式），
    /// 只读复用、不改快扫行为。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_batch_query(
        handle: &mut Engine,
        size: Size,
        komi: f64,
        setup: &[(Stone, Coord)],
        path: &[MoveRecord],
        turns: &[usize],
        visits: u32,
        rules: &str,
        view: DisplayView,
    ) -> QueryId {
        let moves: Vec<(Stone, Action)> = path
            .iter()
            .map(|record| (record.player, record.action))
            .collect();
        let mut query = AnalysisQuery::new(size, moves);
        query.initial_stones = setup.to_vec();
        query.komi = komi;
        query.max_visits = Some(visits);
        query.analyze_turns = Some(turns.to_vec());
        query.priority = BATCH_PRIORITY;
        query.rules = rules.to_owned();
        query.view = match view {
            DisplayView::Black => WinrateView::Black,
            DisplayView::Alternating => WinrateView::SideToMove,
        };
        handle.analyze(query)
    }

    /// 批量终态收尾：完成数报满则生成完成提示；未满（卡死兜底被调用）
    /// 则按实际完成数生成部分完成提示。两种情况都结束任务。
    /// 在飞块必须 terminate：卡死收尾时引擎多半还在算（或不算了，二者
    /// 都该停）；正常收尾时块报告恰好收齐、引擎本就空闲，多发一条
    /// terminate 无害（引擎对已完成的查询回 `noResults` 补发，被上层
    /// 按「id 不符」丢弃）。不 terminate 的话异常路径会白烧 GPU 算完
    /// 整块（≤20 手 ≈ 19s），结果却没人收。
    pub(super) fn finish_batch(&mut self, complete: bool) {
        let Some(job) = self.batch.take() else { return };
        // terminate 在飞块（若有）：cancel_batch 同款处理，缺省时 no-op。
        if let Some(chunk) = job.active
            && let Some(handle) = self.handle.as_mut()
        {
            handle.terminate(chunk.id);
        }
        let total = job.plan.positions;
        let elapsed = job.started.elapsed();
        self.batch_notice = Some(if complete {
            format!(
                "整谱快扫完成：{total} 手，用时 {}。",
                format_duration(elapsed)
            )
        } else {
            format!(
                "整谱快扫已结束：完成 {}/{} 手（部分块超时被跳过），用时 {}。",
                job.done,
                total,
                format_duration(elapsed),
            )
        });
    }

    /// 批量报告回填：按报告 `turnNumber` 在**该块查询的路径**上定位局面
    /// 签名（主扫描组与加深组的路径不同，且到达顺序不可信——笔记 §2），
    /// 复用逐手历史的 visits 覆盖规则。
    /// **只回填历史，不落展示快照**——批量分析的是其它手数的局面，
    /// 侧栏数值必须继续反映当前局面的交互分析。
    /// 返回 true = 在飞块已收齐全部 turn 报告（含 noResults），可派发下一块。
    pub(super) fn on_batch_report(&mut self, job: &mut BatchJob, id: QueryId, report: AnalysisReport) -> bool {
        // id 不符 = 已被取消的旧块补发（terminate 后引擎仍会补发终态），丢弃。
        if job.active.as_ref().is_none_or(|chunk| chunk.id != id) {
            return false;
        }
        if !report.no_results
            && let Some(root) = &report.root_info
        {
            // 按报告 turnNumber 在本块路径上重算该局面的签名。
            let path = job
                .active
                .as_ref()
                .map(|chunk| chunk.path.as_slice())
                .unwrap_or(&[]);
            let sig = position_sig(&path[..report.turn_number.min(path.len())]);
            // 候选表随 root 同一次写入（口径一致，见 record_history 文档）。
            let (moves, total, best) = Self::candidates_from(&report.move_infos);
            self.record_history(
                report.turn_number,
                root,
                sig,
                &moves,
                total,
                best,
                None,
            );
            if job.active.as_ref().is_some_and(|chunk| chunk.deep) {
                job.deep_done += 1;
            } else {
                job.done += 1;
            }
        }
        // 空报告（noResults，terminate 后未完成 turn 的补发）无数据，只计数。
        if let Some(chunk) = job.active.as_mut() {
            chunk.expect = chunk.expect.saturating_sub(1);
            // 心跳：每收到一条报告（含空报告）都刷新。卡死判定看的是
            // 「多久没收到任何报告」，所以空报告同样是活着的证据。
            chunk.last_report = Instant::now();
        }
        job.active.as_ref().is_none_or(|chunk| chunk.expect == 0)
    }

    /// 规划加深阶段：主扫描收齐后，按「胜率损失降序」取前 N 手差异手
    /// （与侧栏「局后统计」完全同一口径：损失取自 [`loss_from_points`]、
    /// 排序同 `game_summary` 的 diffWinrate 口径——胜率损失降序、并列按
    /// 手数升序，不另发明排序）。加深手数不足 N（损失可算的手少）时
    /// 有多少加多少；返回 `None` = 无需加深（未开加深 / 一手都算不出
    /// 损失 / 已在加深阶段）。
    ///
    /// 损失数据取**当前线**的逐手历史点（[`Self::line_points`]）：走子
    /// 前后两端都已由主扫描回填。变着分支内的局面不在当前线上、无从
    /// 取损失，天然不进加深榜（文档已注明该局限）。
    pub(super) fn plan_deepen(&self, job: &BatchJob, board: &Board) -> Option<Vec<usize>> {
        if !job.config.deepen_enabled || job.deepening {
            return None;
        }
        let points = self.line_points(board);
        let records = board.line_records();
        let mut worst: Vec<(usize, f64)> = Vec::new();
        // 加深对象一律取**当前线**上的手，口径与侧栏「局后统计」卡片完全一致
        // （同一批差异手）：遍历线的着法而非扫描计划的分组，是因为含变着
        // 模式下计划里混着分支局面——它们的 turn 与当前线的 turn 同号却
        // 不是同一局面，用当前线的历史点算损失会把两个局面记混；而且加深
        // 派发走的是当前线路径（见 `BatchJob::line_path`），选中一个只存在于
        // 分支上的 turn 会发出越界查询（实测引擎回 `Invalid turn number`）。
        for (i, record) in records.iter().enumerate() {
            let turn = i + 1;
            let (Some(before), Some(after)) = (
                points.get(turn - 1).and_then(|p| *p),
                points.get(turn).and_then(|p| *p),
            ) else {
                continue; // 前后两端不齐 = 损失未知，不进榜（不臆造 0）
            };
            let Some(loss) = loss_from_points(turn, record.player, before, after) else {
                continue;
            };
            worst.push((loss.turn, loss.winrate_loss));
        }
        // 与 game_summary 相同的排序：胜率损失降序，并列手数升序。
        worst.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        let mut turns: Vec<usize> = worst
            .into_iter()
            .take(job.config.deepen_top)
            .map(|(turn, _)| turn)
            .collect();
        // 派发按 turn 升序串行；当前线的手互不重复，dedup 为防御性兜底。
        turns.sort_unstable();
        turns.dedup();
        (!turns.is_empty()).then_some(turns)
    }
}

/// 时长的人类可读形式（进度与完成提示共用）：1 分 32 秒 / 45 秒。
fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{secs} 秒")
    }
}

/// 时长的人类可读形式（侧栏进度显示用，与内部提示同一口径）。
pub fn format_batch_elapsed(d: std::time::Duration) -> String {
    format_duration(d)
}
