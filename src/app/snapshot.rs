//! headless 驱动 API 与结构化状态快照（仅供 `examples/headless.rs`
//! 取证；应用自身路径绝不调用）。
//!
//! 刻意保持极小：无窗口构造复用 [`super::GuanqiApp::new_with_prefs`]，
//! 请求退出与状态快照两个只读/单向操作，不含任何行为捷径——脚本驱动
//! 的落子 / 保存 / 对话框事件一律走真实 UI 事件（RawInput / portal 注入）。

use super::{GuanqiApp, PendingDialog};
use crate::portal::PortalEvent;

// ---- headless 工具入口（仅供 examples/headless.rs 驱动验证；应用自身
// 路径绝不调用）。刻意保持极小：无窗口构造复用 [`Self::new_with_prefs`]，
// 请求退出与状态快照两个只读/单向操作，不含任何行为捷径——脚本驱动的
// 落子 / 保存 / 对话框事件一律走真实 UI 事件（RawInput / portal 注入）。----

/// [`GuanqiApp`] 的结构化状态快照（headless 工具的 dump 数据源）。
///
/// 库内不做任何打印：格式化 / 汇报由 `examples/` 的工具负责。字段覆盖
/// 「够用即可」的黑盒验证面：文档与脏标记、引擎状态与最近读数、对弈
/// 模式与时钟、快扫进度、候选显示门控、消息区文本、面板开关与最近
/// 发出的 viewport 命令。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct StateSnapshot {
    /// 当前文档来源路径（未载入棋谱 / 新对局时为占位或实际存盘路径）。
    pub source: Option<String>,
    /// 棋盘尺寸（路数）。
    pub size: usize,
    /// 当前线总手数。
    pub line_len: usize,
    /// 游标位置（0 基局面序号；== line_len 表示在活子位置）。
    pub cursor: usize,
    /// 全树着法数（含变着分支）。
    pub tree_moves: usize,
    /// 未保存改动标记（record_rev != saved_rev）。
    pub dirty: bool,
    /// 引擎状态机的字符串形态（`unconfigured` / `starting` / `ready` /
    /// `failed`）。
    pub engine_status: String,
    /// 引擎最近读数（展示快照的根信息；流式中间报告也会出现）。
    pub engine_reads: Option<EngineReads>,
    /// 当前局面候选点数。
    pub candidates: usize,
    /// 引擎瞬时错误（查询被拒 / 超时等；引擎仍可用）。
    pub transient_error: Option<String>,
    /// 最近一条引擎日志（诊断用）。
    pub last_log: Option<String>,
    /// 引擎「顶层未知字段」警告的展示文本（按字段名去重；空 = 无警告）。
    /// 验证「发出的字段名引擎是否认识」的关键旁证（引擎对未知顶层字段
    /// 会发 warning 且静默忽略，字段拼错时这里必有记录）。
    pub engine_warnings: Vec<String>,
    /// 最近一次**实际写入引擎 stdin** 的查询原文（含真实 id）。用于
    /// 核对「发出的 JSON 字段名」与引擎语义（headless 验证工具取证）。
    pub last_query: Option<String>,
    /// 当前快照里带候选级 ownership 的候选数（`includeMovesOwnership`
    /// 生效与否的直接证据）。
    pub moves_with_ownership: usize,
    /// 首个带候选级 ownership 的候选的数组长度（= size²）。
    pub moves_ownership_len: Option<usize>,
    /// 当前快照里带 `pvVisits` 的候选数（`includePVVisits` 生效与否的
    /// 直接证据）。
    pub moves_with_pv_visits: usize,
    /// 首个带 `pvVisits` 的候选的数组长度（= 该候选 PV 长度）。
    pub pv_visits_len: Option<usize>,
    /// 根盘面的预设摆子（GTP 坐标，黑 / 白两组）：与查询里的
    /// `initialStones` 逐点对照取证用。
    pub root_setup: (Vec<String>, Vec<String>),
    /// 思考过程记录（阶段 21 #2）：条数、稳定性读数与全部条目的蒸馏
    /// （验证记录增长 / cap / 稳定性复算 / 门控联动 / 清空路径的取证源）。
    pub thinking: ThinkingSnapshot,
    /// 对弈模式是否开启。
    pub play_mode: bool,
    /// 人类执子（`B` / `W`）。
    pub human: String,
    /// 「引擎方无望（人类大优）」判定：与「让引擎认输」菜单项的启用
    /// 条件同源（[`crate::play::engine_hopeless`]）。`true` = 菜单项
    /// 此刻应可用。
    pub hopeless: bool,
    /// 「无望提示已给过」标记（[`crate::play::PlayState::hopeless_shown`]）。
    /// 提示是一次性的，而菜单入口的可用性不该被它消费——两者并列取证
    /// 可判别「条件从未满足」与「满足过但入口被标记污染」。
    pub hopeless_shown: bool,
    /// 认输方（`B` / `W`）；对局因认输结束时非空。
    pub resigned: Option<String>,
    /// 超时判负方；与认输并列的独立终局路径。
    pub timeout_loss: Option<String>,
    /// 对局是否已结束（认输 / 超时 / 双方连续弃着）。
    pub play_finished: bool,
    /// 双方主时间（黑, 白，秒）。
    pub clock_main: (f64, f64),
    /// 双方累计用时（黑, 白，秒）。
    pub clock_total: (f64, f64),
    /// 快扫进度：`(加深阶段, 已完成, 总数, 已用时)`；`None` = 空闲。
    pub batch_progress: Option<(bool, usize, usize, f64)>,
    /// 候选类显示门控（`immediate` / `delayed` / `manual`）。
    pub gating: String,
    /// 手动门控下「已按 F」标志。
    pub manual_revealed: bool,
    /// 门控判定：候选类内容当前是否可见（与绘制路径同一判定函数）。
    pub candidates_visible: bool,
    /// 消息区文本（按入队先后，最旧在前；上限 [`analysis_panel::MAX_NOTICES`]）。
    pub notices: Vec<String>,
    /// 各面板 / 窗口开关。
    pub toggles: Toggles,
    /// 当前生效贴目。
    pub komi: f64,
    /// 局后统计（复盘报告导出的数据源，与侧栏卡片同一
    /// [`AnalysisState::game_summary`] 计算）：报告数值对照的取证源。
    pub game_summary: Option<GameSummaryDump>,
    /// 最近发出的 viewport 命令（读取即清空；与驱动「dump 后清」的旧
    /// 语义一致）。
    pub viewport_commands: Vec<String>,
    /// headless 取证：最近一次棋盘视图的可用区域（`App::ui` 每帧记录；
    /// 应用语义不读取）。GTP → 点击坐标换算用。
    pub board_area: std::cell::RefCell<Option<egui::Rect>>,
    /// 参照文件状态与当前局面命中情况（阶段 22；`None` = 未加载参照）。
    pub reference: Option<ReferenceSnapshot>,
    /// 参照库打分状态（阶段 22.3；`None` = 本会话无结果且无在飞任务）。
    pub ref_scan: Option<RefScanSnapshot>,
    /// 解题模式是否开启（阶段 22.2；不剧透总闸的原始状态）。
    pub solving: bool,
    /// 解题模式「已查看答案」留痕。
    pub answer_shown: bool,
    /// 最近一条「参照对照：」判定文本（解题模式落子判定的取证口径；
    /// `None` = 本运行尚无判定）。
    pub judge_notice: Option<String>,
    /// 解题模式下各剧透显示层的实际渲染判定（全 false = 不剧透生效）。
    /// 每项与真实绘制路径同一判定（与 App::ui / panel_body 的短路条件
    /// 同源），对照取证用。
    pub solving_hidden: SolvingHiddenDump,
    /// 棋盘区域的屏幕矩形（points，left top right bottom）。headless
    /// 工具把 GTP 坐标换算成点击坐标用（19 路网格等分、含边缘留白，
    /// 与 `ui::board_view::Layout` 同一套几何口径的近似重建——留白比例
    /// MARGIN_IN_SPACING = 1.2）。
    pub board_rect: Option<[f32; 4]>,
    /// 当前线最后一手（GTP，弃着为 "pass"；无着法为 `None`）。headless
    /// 核实「点击实际落到哪一点」用。
    pub last_move: Option<String>,
}

/// 解题模式「不剧透」各显示层的取证形态（阶段 22.2）。
///
/// 语义 = **该显示层此刻是否实际渲染**（true = 可见）：解题模式开启时
/// 全部应为 false；关闭时恢复各自开关的真实值。逐项对应任务书清单：
/// 候选圆圈 / 热度图 / 策略图 / 失误标注（棋盘叠加层）、候选列表 /
/// 胜率目差 / 讲解 / 思考过程 / 失误卡 / 局后统计（侧栏卡片）、
/// 曲线 / 目差线 / 小棋盘（底部面板）、参照续法（参照卡片）。
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct SolvingHiddenDump {
    /// 棋盘候选点圆圈层。
    pub board_candidates: bool,
    /// 棋盘局势热度图（含候选点领地）。
    pub board_heat: bool,
    /// 棋盘策略热度图。
    pub board_policy: bool,
    /// 棋盘失误标注。
    pub board_mistakes: bool,
    /// 侧栏候选点卡片。
    pub panel_candidates: bool,
    /// 侧栏胜率 / 目差卡片。
    pub panel_winrate: bool,
    /// 侧栏讲解卡片。
    pub panel_explain: bool,
    /// 侧栏思考过程卡片。
    pub panel_thinking: bool,
    /// 侧栏失误卡片。
    pub panel_mistakes: bool,
    /// 侧栏局后统计卡片。
    pub panel_summary: bool,
    /// 胜率曲线面板（含目差折线）。
    pub curve_panel: bool,
    /// 小棋盘 PV 回放面板。
    pub mini_board_panel: bool,
    /// 参照卡片的续法 / 偏离详情（true = 详情可见）。
    pub reference_details: bool,
}

/// 局后统计的取证形态（dump 用；字段与 [`GameSummary`] 一一对应，
/// worst 全量带出供报告逐项比对）。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct GameSummaryDump {
    /// 当前线总手数。
    pub total: usize,
    /// 黑 / 白已分析手数。
    pub analyzed_black: usize,
    pub analyzed_white: usize,
    /// 黑 / 白吻合度。
    pub match_black: f64,
    pub match_white: f64,
    /// 黑 / 白样本是否达标。
    pub enough_black: bool,
    pub enough_white: bool,
    /// 黑 / 白「深度分析」手数（构成判定线 DEEP_VISITS_THRESHOLD）。
    pub deep_black: usize,
    pub deep_white: usize,
    /// 差异手排行榜（胜率损失降序）。
    pub worst: Vec<WorstMoveDump>,
}

/// 差异手一行的取证形态。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct WorstMoveDump {
    pub turn: usize,
    pub player: String,
    pub winrate_loss: f64,
    pub score_loss: f64,
    pub severity: String,
    pub match_ratio: f64,
}

/// 引擎最近一次读数（展示快照的根信息）。
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct EngineReads {
    /// 该快照对应的手数（发起查询时的游标）。
    pub turn: usize,
    /// 报告是否为终态（`false` = 流式中间报告，还会更新）。
    pub is_final: bool,
    /// 搜索量。
    pub visits: u64,
    /// 黑方胜率 [0,1]。
    pub winrate: f64,
    /// 黑方目差（正 = 黑领先）。
    pub score_lead: f64,
}

/// 参照匹配结果的取证形态（阶段 22；headless dump 用）。
/// 字段与侧栏「参照」卡片同一数据源（[`super::GuanqiApp::reference_view`]），
/// 数字必然一致——「卡片显示什么」与「dump 取什么」对账的锚点。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ReferenceSnapshot {
    /// 参照文件来源路径。
    pub source: String,
    /// 参照棋盘尺寸（路数）。
    pub size: u8,
    /// 索引条目数（不同局面键数）。
    pub entries: usize,
    /// 全树有着点的节点数。
    pub nodes: usize,
    /// 建索引用时（毫秒）。
    pub index_ms: f64,
    /// 载入警告（分支舍弃等）。
    pub warnings: Vec<String>,
    /// 当前局面命中详情；`None` = 未命中 / 尺寸不符 / 空盘。
    pub hit: Option<RefHitSnapshot>,
    /// 偏离提示（父局面命中、实战最后一手偏离）：`(实战手, 库中续法)`。
    pub deviation: Option<(String, Vec<String>)>,
    /// 尺寸不符标记（`true` = 参照与当前盘尺寸不同，无法匹配）。
    pub size_mismatch: bool,
}

/// 命中详情的取证形态。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct RefHitSnapshot {
    /// 命中局面的库内手数。
    pub move_no: usize,
    /// 库内位置路径（各层分支下标，「0-2-1」形态）。
    pub path: String,
    /// 库内着法尾部展示串。
    pub tail: String,
    /// 库内注释。
    pub comment: Option<String>,
    /// 库内续法（展示串）。
    pub continuations: Vec<String>,
    /// 偏离提示（实战最后一手不在库内续法中）。
    pub deviation: Option<String>,
    /// 当前局面的着法尾部（实战口径；与库内 tail 并排核对用）。
    pub live_tail: Vec<String>,
    /// 命中条目的比较区域两角（GTP）。
    pub region: String,
}

/// 参照库打分的取证形态（阶段 22.3；headless dump 用）。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct RefScanSnapshot {
    /// 在飞任务进度：`(已完成, 总节点数, 已用秒)`；`None` = 空闲。
    pub progress: Option<(usize, usize, f64)>,
    /// 在飞任务的计划摘要：`(链数, 每链 turns 数, 总节点数, 摆子数)`。
    /// 转置 / 多分支库的「极大链分解是否正确」的验证数据源。
    pub plan: Option<(usize, Vec<usize>, usize, usize)>,
    /// 已完成节点的评分明细（每节点：路径、手数、首选、各续法的
    /// 评估 / 差距 / 是否判淘汰）。
    pub results: Vec<RefNodeScoreDump>,
    /// 会被判淘汰的节点数（与 results 同源，报告核对用）。
    pub outdated_nodes: usize,
}

/// 一个被扫节点的评分取证形态。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct RefNodeScoreDump {
    /// 树内路径（下标串）。
    pub path: String,
    /// 库内手数。
    pub move_no: usize,
    /// 展示尾部。
    pub tail: Vec<String>,
    /// 行棋方（`B` / `W`）。
    pub to_play: String,
    /// 引擎首选：`(落点, 黑方胜率, 黑方目差)`；无候选为 `None`。
    pub best: Option<(String, f64, f64)>,
    /// 各库中续法的评估。
    pub continuations: Vec<RefContinuationDump>,
    /// 引擎搜索量。
    pub visits: u64,
}

/// 一条续法评估的取证形态。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct RefContinuationDump {
    /// 库中续法展示串。
    pub text: String,
    /// 引擎评估 `(黑方胜率, 黑方目差)`；`None` = 引擎未列出。
    pub eval: Option<(f64, f64)>,
    /// 行棋方视角目差差距；`None` = 引擎未列出（无从计算）。
    pub gap: Option<f64>,
    /// 是否判「已被 AI 淘汰」。
    pub outdated: bool,
}

/// 思考过程记录的取证快照（阶段 21 #2；headless dump 用）。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ThinkingSnapshot {
    /// 记录条数。
    pub count: usize,
    /// 首选翻转次数（增量维护，见 `analysis::thinking`）。
    pub flips: u32,
    /// 最后一次翻转的耗时（秒）；从未翻转为 `None`。
    pub last_flip_secs: Option<f32>,
    /// 稳定性文案（与侧栏同一 `Stability::text`，对账用）。
    pub stability_text: String,
    /// 全部条目的蒸馏（按时间线序）。
    pub entries: Vec<ThinkingEntryDump>,
}

/// 一条思考记录的取证形态。
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ThinkingEntryDump {
    /// 距查询发出的耗时（秒）。
    pub secs: f32,
    /// 该时刻根节点 visits。
    pub visits: u64,
    /// 当时首选（GTP 坐标；弃着 / 无候选为 `None`）。
    pub best: Option<String>,
    /// 首选黑方胜率 [0,1]。
    pub best_wr: Option<f64>,
    /// 前几名概要（GTP 坐标, 胜率, visits）。
    pub top: Vec<(String, f64, u64)>,
}

/// 面板 / 窗口开关集合（dump 用）。
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Toggles {
    /// 叠加层各开关。
    pub show_candidates: bool,
    pub show_heat: bool,
    pub show_policy: bool,
    pub show_moves_heat: bool,
    pub show_mistakes: bool,
    pub show_score_lead: bool,
    pub show_mini_board: bool,
    /// 底部面板。
    pub curve_open: bool,
    pub tree_open: bool,
    /// 侧栏「思考过程」卡片开关。
    pub thinking_open: bool,
    /// 浮动窗口。
    pub settings_open: bool,
    pub new_game_open: bool,
}

impl GuanqiApp {
    /// 请求退出应用：下一帧 `ui` 顶部经未保存守卫后发出
    /// [`egui::ViewportCommand::Close`]（脏文档会先被既有确认条拦下，
    /// 与真实关窗语义一致）。headless 工具的 `quit` 动作落点。
    #[doc(hidden)]
    pub fn request_exit(&mut self) {
        self.exit_requested = true;
    }

    /// 注入 portal 对话框事件（headless 工具的 load / save 落点）：
    /// 等价用户在原生对话框选中 / 取消。先清等待槽再分发——与 `logic`
    /// 每帧取事件的顺序一致（取出即清槽，事件处理器内的后续动作不会
    /// 被「已有对话框在等待」守卫拦下）。
    #[doc(hidden)]
    pub fn inject_portal_event(&mut self, kind: PendingDialog, event: PortalEvent) {
        self.dialog = None;
        self.on_portal_event(kind, event);
    }

    /// 卸载参照（headless 工具的 `unref` 动作落点；与菜单「卸载参照」
    /// 同一执行体）。
    #[doc(hidden)]
    pub fn unload_reference_pub(&mut self) {
        self.unload_reference();
    }

    /// 发起参照库打分（headless 工具的 `refscan on` 落点；与对话框
    /// 「开始打分」同一执行体，规则 / 贴目口径一致）。
    #[doc(hidden)]
    pub fn start_ref_scan_pub(
        &mut self,
        max_move_no: usize,
    ) -> Result<(), String> {
        let Some(reference) = self.reference.as_ref() else {
            return Err("未加载参照文件".to_owned());
        };
        let tree = reference.tree();
        let size = reference.size();
        let game_rules = self
            .loaded
            .as_ref()
            .and_then(|meta| meta.info.rules.as_deref());
        self.analysis
            .start_ref_scan(tree, size, self.komi, game_rules, max_move_no)
    }

    /// 取消参照库打分（headless 工具的 `refscan off` 落点）。
    #[doc(hidden)]
    pub fn cancel_ref_scan_pub(&mut self) {
        self.analysis.cancel_ref_scan();
    }

    /// 离线构造扫描计划并打印摘要（headless 工具的 `refscan plan` 落点；
    /// 不发起任务——「计划是否正确分解」的验证与「实际执行」解耦）。
    #[doc(hidden)]
    pub fn ref_scan_plan_digest_pub(
        &self,
        max_move_no: usize,
    ) -> Option<(usize, Vec<usize>, usize, usize)> {
        let reference = self.reference.as_ref()?;
        let (chains, turns, positions, setup) =
            crate::ui::analysis::ref_scan_plan_digest_of(reference.tree(), reference.size(), max_move_no);
        Some((chains, turns, positions, setup))
    }

    /// 退出收尾（headless 工具跑完脚本后调用）：与 eframe 运行时退出
    /// 时同款的偏好补写与引擎进程关闭。
    #[doc(hidden)]
    pub fn on_exit_shim(&mut self) {
        eframe::App::on_exit(self);
    }

    /// 取当前状态的结构化快照（headless 工具的 dump 数据源）。
    /// `viewport_commands` 读取即清空（发送留档只在两次 dump 之间累积）。
    #[doc(hidden)]
    pub fn state_snapshot(&self) -> StateSnapshot {
        use crate::ui::analysis::CandidateGating;
        let gating = self.analysis.gating();
        StateSnapshot {
            source: self
                .loaded
                .as_ref()
                .map(|meta| meta.source.display().to_string()),
            size: usize::from(self.board.size().n()),
            line_len: self.board.line_len(),
            cursor: self.board.cursor(),
            tree_moves: self.board.move_count(),
            dirty: self.is_dirty(),
            engine_status: match self.analysis.engine {
                crate::ui::analysis::EngineStatus::Unconfigured => "unconfigured",
                crate::ui::analysis::EngineStatus::Starting => "starting",
                crate::ui::analysis::EngineStatus::Ready => "ready",
                crate::ui::analysis::EngineStatus::Failed(_) => "failed",
            }
            .to_owned(),
            engine_reads: self.analysis.snapshot.as_ref().map(|snap| EngineReads {
                turn: snap.turn,
                is_final: snap.is_final,
                visits: snap.root.as_ref().map_or(0, |r| r.visits),
                winrate: snap.root.as_ref().map_or(0.0, |r| r.winrate),
                score_lead: snap.root.as_ref().map_or(0.0, |r| r.score_lead),
            }),
            candidates: self
                .analysis
                .snapshot
                .as_ref()
                .map_or(0, |snap| snap.moves.len()),
            transient_error: self.analysis.transient_error.clone(),
            last_log: self.analysis.last_log.clone(),
            engine_warnings: self
                .analysis
                .engine_warnings()
                .iter()
                .map(|w| w.text.clone())
                .collect(),
            last_query: self.analysis.last_query().map(str::to_owned),
            moves_with_ownership: self.analysis.snapshot.as_ref().map_or(0, |snap| {
                snap.moves.iter().filter(|m| m.ownership.is_some()).count()
            }),
            moves_ownership_len: self
                .analysis
                .snapshot
                .as_ref()
                .and_then(|snap| {
                    snap.moves
                        .iter()
                        .find_map(|m| m.ownership.as_ref().map(|v| v.len()))
                }),
            moves_with_pv_visits: self.analysis.snapshot.as_ref().map_or(0, |snap| {
                snap.moves.iter().filter(|m| m.pv_visits.is_some()).count()
            }),
            pv_visits_len: self
                .analysis
                .snapshot
                .as_ref()
                .and_then(|snap| {
                    snap.moves
                        .iter()
                        .find_map(|m| m.pv_visits.as_ref().map(|v| v.len()))
                }),
            root_setup: {
                let (black, white) = self.board.root_setup();
                (
                    black.iter().map(|c| c.to_gtp(self.board.size())).collect::<Vec<_>>(),
                    white.iter().map(|c| c.to_gtp(self.board.size())).collect::<Vec<_>>(),
                )
            },
            thinking: {
                let stability = self.analysis.thinking_stability();
                let count = self.analysis.thinking_len();
                let entries = (0..count)
                    .filter_map(|i| self.analysis.thinking_entry(i))
                    .map(|entry| ThinkingEntryDump {
                        secs: entry.elapsed.as_secs_f32(),
                        visits: entry.visits,
                        best: entry.best.map(|(at, _)| at.to_gtp(self.board.size())),
                        best_wr: entry.best.map(|(_, wr)| wr),
                        top: entry
                            .top
                            .iter()
                            .take(entry.top_len)
                            .filter_map(|slot| {
                                slot.map(|(at, wr, visits)| {
                                    (at.to_gtp(self.board.size()), wr, visits)
                                })
                            })
                            .collect(),
                    })
                    .collect();
                ThinkingSnapshot {
                    count,
                    flips: stability.flips,
                    last_flip_secs: stability.last_flip.map(|d| d.as_secs_f32()),
                    stability_text: stability.text(),
                    entries,
                }
            },
            hopeless: crate::play::engine_hopeless(
                &self.play,
                self.analysis.snapshot.as_ref(),
            ),
            hopeless_shown: self.play.hopeless_shown(),
            play_mode: self.play.mode,
            human: self.play.human.name().to_owned(),
            resigned: self.play.resigned.map(|s| s.name().to_owned()),
            timeout_loss: self.play.timeout_loss.map(|s| s.name().to_owned()),
            play_finished: self.play.finished(&self.board),
            clock_main: (
                self.play.clock.sides[0].main,
                self.play.clock.sides[1].main,
            ),
            clock_total: (self.play.clock.total[0], self.play.clock.total[1]),
            batch_progress: self
                .analysis
                .batch_progress()
                .map(|(deep, done, total, elapsed)| (deep, done, total, elapsed.as_secs_f64())),
            gating: match gating {
                CandidateGating::Immediate => "immediate",
                CandidateGating::Delayed { .. } => "delayed",
                CandidateGating::Manual => "manual",
            }
            .to_owned(),
            manual_revealed: self.manual_revealed,
            candidates_visible: self.analysis.candidates_visible(self.manual_revealed),
            notices: self.notices.iter().map(|n| n.text().to_owned()).collect(),
            toggles: Toggles {
                show_candidates: self.overlay.show_candidates,
                show_heat: self.overlay.show_heat,
                show_policy: self.overlay.show_policy,
                show_moves_heat: self.overlay.show_moves_heat,
                show_mistakes: self.overlay.show_mistakes,
                show_score_lead: self.overlay.show_score_lead,
                show_mini_board: self.overlay.show_mini_board,
                curve_open: self.curve_open,
                tree_open: self.tree_open,
                thinking_open: self.thinking_open,
                settings_open: self.settings_open,
                new_game_open: self.new_game_open,
            },
            komi: self.komi,
            game_summary: self.loaded.as_ref().map(|_| {
                let summary = self.analysis.game_summary(&self.board);
                GameSummaryDump {
                    total: summary.total,
                    analyzed_black: summary.analyzed_black,
                    analyzed_white: summary.analyzed_white,
                    match_black: summary.match_black,
                    match_white: summary.match_white,
                    enough_black: summary.enough_black,
                    enough_white: summary.enough_white,
                    deep_black: summary.deep_black,
                    deep_white: summary.deep_white,
                    worst: summary
                        .worst
                        .iter()
                        .map(|w| WorstMoveDump {
                            turn: w.turn,
                            player: w.player.name().to_owned(),
                            winrate_loss: w.winrate_loss,
                            score_loss: w.score_loss,
                            severity: format!("{:?}", w.severity),
                            match_ratio: w.match_ratio,
                        })
                        .collect(),
                }
            }),
            viewport_commands: std::mem::take(
                &mut *self.last_viewport_commands.borrow_mut(),
            )
            .into_iter()
            .map(|c| format!("{c:?}"))
            .collect(),
            board_area: std::cell::RefCell::new(None),
            reference: self.reference_view().map(|view| ReferenceSnapshot {
                source: self
                    .reference
                    .as_ref()
                    .map(|r| r.source().display().to_string())
                    .unwrap_or_default(),
                size: view.size,
                entries: view.entries,
                nodes: view.nodes,
                index_ms: self
                    .reference
                    .as_ref()
                    .map(|r| r.index_elapsed().as_secs_f64() * 1000.0)
                    .unwrap_or(0.0),
                warnings: view.warnings,
                size_mismatch: self
                    .reference
                    .as_ref()
                    .is_some_and(|r| r.size() != self.board.size()),
                deviation: view.deviation,
                hit: view.hit.map(|hit| RefHitSnapshot {
                    move_no: hit.move_no,
                    path: hit.path,
                    tail: hit.tail,
                    comment: hit.comment,
                    continuations: hit.continuations,
                    deviation: hit.deviation,
                    live_tail: hit.live_tail,
                    region: hit.region,
                }),
            }),
            solving: self.solving_mode,
            answer_shown: self.answer_shown,
            judge_notice: self.last_judge_notice(),
            ref_scan: {
                // 打分取证：进度 / 计划摘要 / 结果明细三段。results 排序
                // 稳定（按手数）便于 diff；空结果且无任务 = None。
                let results = self.analysis.ref_scan_results();
                let progress = self.analysis.ref_scan_progress();
                if results.is_none() && progress.is_none() {
                    None
                } else {
                    let mut nodes: Vec<RefNodeScoreDump> = results
                        .map(|m| m.values())
                        .into_iter()
                        .flatten()
                        .map(|score| RefNodeScoreDump {
                            path: score.path.iter().map(ToString::to_string).collect::<Vec<_>>().join("-"),
                            move_no: score.move_no,
                            tail: score.tail.clone(),
                            to_play: score.to_play.name().to_owned(),
                            best: score.best.clone(),
                            continuations: score
                                .continuations
                                .iter()
                                .map(|cont| RefContinuationDump {
                                    text: cont.text.clone(),
                                    eval: cont.eval,
                                    gap: cont.gap,
                                    outdated: cont.outdated,
                                })
                                .collect(),
                            visits: score.visits,
                        })
                        .collect();
                    nodes.sort_by_key(|n| n.move_no);
                    Some(RefScanSnapshot {
                        progress: progress
                            .map(|(done, total, elapsed)| (done, total, elapsed.as_secs_f64())),
                        plan: self.analysis.ref_scan_plan_digest(),
                        outdated_nodes: nodes
                            .iter()
                            .filter(|n| n.continuations.iter().any(|c| c.outdated))
                            .count(),
                        results: nodes,
                    })
                }
            },
            board_rect: self.board_area.borrow().as_ref().map(|r| {
                [r.left(), r.top(), r.right(), r.bottom()]
            }),
            last_move: self.board.records().last().map(|record| {
                match record.action {
                    crate::board::Action::Place(at) => {
                        format!("{}{}", record.player.name(), at.to_gtp(self.board.size()))
                    }
                    crate::board::Action::Pass => {
                        format!("{}pass", record.player.name())
                    }
                }
            }),
            solving_hidden: {
                // 各显示层的实际渲染判定：与绘制路径同一条件（App::ui 的
                // 面板创建条件 / panel_body 的卡片短路 / board_view 的
                // 叠加层短路），取证值即真实可见性，无第二套口径。
                let candidates_visible =
                    self.analysis.candidates_visible(self.manual_revealed);
                let solving = self.solving_mode;
                let snapshot_exists = self.analysis.snapshot.is_some();
                SolvingHiddenDump {
                    board_candidates: self.overlay.show_candidates
                        && candidates_visible
                        && !solving
                        && snapshot_exists,
                    board_heat: self.overlay.show_heat && !solving && snapshot_exists,
                    board_policy: self.overlay.show_policy && !solving && snapshot_exists,
                    board_mistakes: self.overlay.show_mistakes && !solving,
                    panel_candidates: !solving,
                    panel_winrate: !solving,
                    panel_explain: !solving,
                    panel_thinking: self.thinking_open && !solving,
                    panel_mistakes: !solving,
                    panel_summary: !solving,
                    curve_panel: self.curve_open && !solving,
                    mini_board_panel: self.overlay.show_mini_board && !solving,
                    // 「参照卡片的续法/偏离详情」在解题模式打开时**数据层
                    // 也隐藏**（reference_view 返回 None）：续法文本若仍
                    // 派生进结构体，取证 dump 会带出答案——取证面与显示
                    // 面必须一致，故这里以 reference_view 的实际产出为准。
                    reference_details: !solving
                        && self.reference_view().is_some_and(|v| {
                            v.hit.is_some() || v.deviation.is_some()
                        }),
                }
            },
        }
    }
}

impl StateSnapshot {
    /// 单行 JSON 序列化（`serde_json` 已在依赖树内；库类型本身不派生
    /// serde，序列化面集中在这里，格式化与展示仍归 examples 工具）。
    pub fn to_json(&self) -> String {
        // 取证字段单独成一个子对象：`json!` 宏按顶层键数递归展开，顶层键
        // 加到一定数量会撞编译器的宏递归上限（本宏原本已接近），拆成第二
        // 个宏调用后各自的展开深度独立，无需提高 crate 的 recursion_limit。
        // 根盘面摆子（headless 取证「查询里的 initialStones 是否与谱一致」）
        // 已在 `state_snapshot` 填入 self.root_setup，这里直接引用。
        let probe = serde_json::json!({
            "engine_warnings": self.engine_warnings,
            "last_query": self.last_query,
            "moves_with_ownership": self.moves_with_ownership,
            "moves_ownership_len": self.moves_ownership_len,
            "moves_with_pv_visits": self.moves_with_pv_visits,
            "pv_visits_len": self.pv_visits_len,
            "hopeless": self.hopeless,
            "hopeless_shown": self.hopeless_shown,
            "root_setup": { "black": self.root_setup.0, "white": self.root_setup.1 },
            "thinking": {
                "count": self.thinking.count,
                "flips": self.thinking.flips,
                "last_flip_secs": self.thinking.last_flip_secs,
                "stability_text": self.thinking.stability_text,
                "entries": self.thinking.entries.iter().map(|e| serde_json::json!({
                    "secs": e.secs,
                    "visits": e.visits,
                    "best": e.best,
                    "best_wr": e.best_wr,
                    "top": e.top.iter().map(|(at, wr, v)| serde_json::json!({
                        "at": at, "wr": wr, "visits": v,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            },
        });
        let game_summary = self.game_summary.as_ref().map(|g| {
            serde_json::json!({
                "total": g.total,
                "analyzed_black": g.analyzed_black,
                "analyzed_white": g.analyzed_white,
                "match_black": g.match_black,
                "match_white": g.match_white,
                "enough_black": g.enough_black,
                "enough_white": g.enough_white,
                "deep_black": g.deep_black,
                "deep_white": g.deep_white,
                "worst": g.worst.iter().map(|w| serde_json::json!({
                    "turn": w.turn, "player": w.player,
                    "winrate_loss": w.winrate_loss, "score_loss": w.score_loss,
                    "severity": w.severity, "match_ratio": w.match_ratio,
                })).collect::<Vec<_>>(),
            })
        });
        let reference = self.reference.as_ref().map(|r| {
            // 同上：命中详情再拆一层，避免单次宏展开深度超限。
            let hit = r.hit.as_ref().map(|h| {
                serde_json::json!({
                    "move_no": h.move_no,
                    "path": h.path,
                    "tail": h.tail,
                    "comment": h.comment,
                    "continuations": h.continuations,
                    "deviation": h.deviation,
                    "live_tail": h.live_tail,
                    "region": h.region,
                })
            });
            serde_json::json!({
                "source": r.source,
                "size": r.size,
                "entries": r.entries,
                "nodes": r.nodes,
                "index_ms": r.index_ms,
                "warnings": r.warnings,
                "size_mismatch": r.size_mismatch,
                "deviation": r.deviation.as_ref().map(|(played, book)| serde_json::json!({
                    "played": played,
                    "book": book,
                })),
                "hit": hit,
            })
        });
        let solving_hidden = serde_json::json!({
            "board_candidates": self.solving_hidden.board_candidates,
            "board_heat": self.solving_hidden.board_heat,
            "board_policy": self.solving_hidden.board_policy,
            "board_mistakes": self.solving_hidden.board_mistakes,
            "panel_candidates": self.solving_hidden.panel_candidates,
            "panel_winrate": self.solving_hidden.panel_winrate,
            "panel_explain": self.solving_hidden.panel_explain,
            "panel_thinking": self.solving_hidden.panel_thinking,
            "panel_mistakes": self.solving_hidden.panel_mistakes,
            "panel_summary": self.solving_hidden.panel_summary,
            "curve_panel": self.solving_hidden.curve_panel,
            "mini_board_panel": self.solving_hidden.mini_board_panel,
            "reference_details": self.solving_hidden.reference_details,
        });
        let ref_scan = self.ref_scan.as_ref().map(|rs| {
            // 三层再拆（同 reference 的理由：宏展开深度）。
            let plan = rs.plan.as_ref().map(|(chains, turns, positions, setup)| {
                serde_json::json!({
                    "chains": chains, "turns_per_chain": turns,
                    "positions": positions, "setup_stones": setup,
                })
            });
            let results = rs.results.iter().map(|node| {
                serde_json::json!({
                    "path": node.path, "move_no": node.move_no,
                    "tail": node.tail, "to_play": node.to_play,
                    "best": node.best.as_ref().map(|(at, wr, lead)| serde_json::json!({
                        "at": at, "winrate": wr, "score_lead": lead,
                    })),
                    "visits": node.visits,
                    "continuations": node.continuations.iter().map(|cont| serde_json::json!({
                        "text": cont.text,
                        "eval": cont.eval.map(|(wr, lead)| serde_json::json!({
                            "winrate": wr, "score_lead": lead,
                        })),
                        "gap": cont.gap,
                        "outdated": cont.outdated,
                    })).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>();
            serde_json::json!({
                "progress": rs.progress.map(|(done, total, secs)| serde_json::json!({
                    "done": done, "total": total, "elapsed_secs": secs,
                })),
                "plan": plan,
                "outdated_nodes": rs.outdated_nodes,
                "results": results,
            })
        });
        serde_json::json!({
            "source": self.source,
            "size": self.size,
            "line_len": self.line_len,
            "cursor": self.cursor,
            "tree_moves": self.tree_moves,
            "dirty": self.dirty,
            "engine_status": self.engine_status,
            "engine_reads": self.engine_reads.map(|r| serde_json::json!({
                "turn": r.turn,
                "is_final": r.is_final,
                "visits": r.visits,
                "winrate": r.winrate,
                "score_lead": r.score_lead,
            })),
            "candidates": self.candidates,
            "transient_error": self.transient_error,
            "last_log": self.last_log,
            "probe": probe,
            "play_mode": self.play_mode,
            "human": self.human,
            "resigned": self.resigned,
            "timeout_loss": self.timeout_loss,
            "play_finished": self.play_finished,
            "clock_main": self.clock_main,
            "clock_total": self.clock_total,
            "batch_progress": self
                .batch_progress
                .map(|(deep, done, total, secs)| serde_json::json!({
                    "deepening": deep, "done": done, "total": total,
                    "elapsed_secs": secs,
                })),
            "gating": self.gating,
            "manual_revealed": self.manual_revealed,
            "candidates_visible": self.candidates_visible,
            "notices": self.notices,
            "toggles": {
                "show_candidates": self.toggles.show_candidates,
                "show_heat": self.toggles.show_heat,
                "show_policy": self.toggles.show_policy,
                "show_moves_heat": self.toggles.show_moves_heat,
                "show_mistakes": self.toggles.show_mistakes,
                "show_score_lead": self.toggles.show_score_lead,
                "show_mini_board": self.toggles.show_mini_board,
                "curve_open": self.toggles.curve_open,
                "tree_open": self.toggles.tree_open,
                "thinking_open": self.toggles.thinking_open,
                "settings_open": self.toggles.settings_open,
                "new_game_open": self.toggles.new_game_open,
            },
            "komi": self.komi,
            "game_summary": game_summary,
            "viewport_commands": self.viewport_commands,
            "reference": reference,
            "ref_scan": ref_scan,
            "solving": self.solving,
            "answer_shown": self.answer_shown,
            "judge_notice": self.judge_notice,
            "board_rect": self.board_rect,
            "last_move": self.last_move,
            "solving_hidden": solving_hidden,
        })
        .to_string()
    }
}
