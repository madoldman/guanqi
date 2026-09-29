//! 引擎接线与分析状态：生命周期状态机 + 流式查询调度（TASKS 3.3 / 4.2 接线层）。
//!
//! 职责与协议事实的对应关系（均见 `engine` 模块文档）：
//!
//! - [`EngineEvent::Ready`] 之前不能发查询；启动（含模型加载 / OpenCL 调优）
//!   实测可达数十秒，期间 UI 停留在 [`EngineStatus::Starting`]；
//! - `katago analysis` 支持查询级 `reportDuringSearchEvery`：常规分析查询
//!   开启后，搜索期间约每 0.5s 推送一条 `isDuringSearch: true` 的中间报告，
//!   最后一条为 `false` 的终态（v1.18.2 实测）。局面变化 → 发**一次**查询
//!   （visits = 配置值），边搜边把最新报告落 [`AnalysisState::snapshot`]
//!   供界面实时刷新，无需再「先快后深」分段；
//! - **人机对弈的走子查询（`request` 的 `play` 口径）单独成口径**：按难度
//!   档位的 visits 完整搜索，报告落 [`AnalysisState::play_snapshot`]，决策
//!   取自它而非展示快照。轮到引擎时应手局面先发 Play（保证树缓存不会让
//!   低难度查询被更早的深搜索污染），展示分析随后补发；
//! - 局面变化时 [`Engine::terminate`] 旧查询，并按 id 丢弃过期补发报告；
//! - 进程退出（[`EngineEvent::Exited`]）进入可重试的 [`EngineStatus::Failed`]。
//!
//! [`AnalysisState::snapshot`] 是展示分析结果的唯一存放点：侧栏读它显示，
//! 棋盘候选点叠加层 / 热度图也直接取用，避免二次搬运。它现在存放**最新**
//! 报告（含流式中间报告）：胜率 / 目差 / 候选点随搜索推进实时刷新；
//! [`Snapshot::is_final`] 区分中间与终态。
//! 逐手胜率历史（[`AnalysisState::history`]，TASKS 4.3 曲线用）与失误分析
//! **只在终态报告**写入：键为**局面签名**（根到该局面着法前缀的 FNV-1a），
//! 谱树中不同分支的同一手数是不同局面，各存各的，切换分支不互相覆盖；
//! 同局面后到且 visits 更多的终态覆盖先到的。
//! 每手损失（[`loss_from_points`]，TASKS 4.4）不另存状态：读取时由当前线
//! 相邻两个已知历史点现场派生，引擎零额外查询。
//!
//! # 规则口径（`RU[]` 透传）
//!
//! 查询的 `rules` 不再写死 `chinese`：按「设置显式指定 > 棋谱 `RU[]`
//! 宽容映射 > 默认 `chinese`」解析（[`crate::engine::resolve_rules`]），
//! 三条查询路径（交互展示 / 走子口径 / 整谱快扫）都带解析后的规范名。
//! 规则变化会**清空逐手历史与候选表**——历史点按局面签名键控，签名里
//! 不含规则，两种规则的数据混在同一条曲线是错的（数子差 = 数目差 +
//! 盘面双方子数之差，目差系统性偏差可达约 1 目；v1.18.2 实测同一局面
//! chinese +1.12 目 / japanese +0.60 目）；清空比把规则并进哈希更简单、
//! 语义也更诚实。want/sent 两值比对复刻 `want_policy` 机制。
//!
//! # 目数视角口径（`reportAnalysisWinratesAs`）
//!
//! 存储**一律黑视角**：查询可请求 `SIDETOMOVE`（验证过引擎口径），报告
//! 在引擎桥接层按行棋方换算回黑视角落库，全仓读取路径因此零改动；
//! 显示层只在绘制 / 悬停处按 [`DisplayView`] 现场换算（[`HistoryPoint`]
//! 记住了该局面的行棋方）。损失类数字（[`loss_from_points`]）本就是
//! 「行棋方视角的亏损量」，与显示视角无关，不翻转。视角切换作废快照并
//! 重发查询（黑视角归一化后同一份数据两用，历史与候选表不清空）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::board::{Board, Coord, MoveRecord, Stone};
use crate::engine::{Difficulty, Engine, EngineConfig};

mod batch;
mod display;
mod history;
mod limits;
mod query;
pub mod ref_export;
mod refscan;
mod thinking;
mod types;

use query::{Inflight, Stage, TerminalEval};
use limits::LimitsEpoch;
use thinking::ThinkingLog;

pub use refscan::{
    ref_scan_plan_digest_of, RefContinuationScore, RefNodeScore, RefScanConfig, RefScanResults,
    REFSCAN_MAX_MOVE_NO, REFSCAN_OUTDATED_SCORE, REFSCAN_OUTDATED_WINRATE, REFSCAN_VISITS,
};

pub use types::{
    loss_from_points, move_block_text, result_block_text, stats_block_text, terminal_result_string,
    CandidateSnap, EngineStatus, GameSummary, HistoryPoint, LossSummary, MoveLoss, Severity,
    Snapshot, WorstMove, DEEP_VISITS_THRESHOLD, MATCH_MIN_MOVES, MOVE_BLOCK_END_MARK,
    MOVE_BLOCK_MARK, STATS_BLOCK_BEGIN, STATS_BLOCK_END, RESULT_BLOCK_BEGIN, RESULT_BLOCK_END,
    WORST_LIMIT,
};
pub(crate) use types::SEVERITY_GOOD_MAX;
pub(crate) use display::display_values;
pub use display::{CandidateGating, DisplayView};
pub use limits::{AnalysisLimits, Region};
pub use query::EngineWarning;
pub use thinking::{Stability, ThinkingEntryRef};
pub use batch::{
    batch_estimate, format_batch_elapsed, BatchConfig, BatchEstimate, BatchSide, BATCH_VISITS,
};
use batch::{BATCH_CHUNK_STALL, BatchJob};
/// 引擎事件唤醒回调：与 `engine::process::Waker` 同构（类型别名未公开，
/// 此处按相同定义书写，透明等价）。
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// 引擎 stderr 日志的可见保留行数（`EngineEvent::Log` 逐条推入
/// [`AnalysisState::log_tail`] 环形缓冲）：侧栏显示最后一行；保留 5 行
/// 供「最近几条日志」一起展示（诊断时启动期的行早已滚走，5 行足够
/// 覆盖崩溃前的原因行），更多只会放大每帧的 UI 成本。
pub(super) const LOG_TAIL_LINES: usize = 5;

/// 流式中间报告的输出间隔（秒）：协议实测 0.5s 键下 300 visits 产出约
/// 11 条中间报告（visits 27→51→…→303 递增），实时感与序列化压力平衡；
/// 更密的间隔只会放大 JSON 解析与重绘频次，实时感提升有限。
pub(super) const REPORT_EVERY_SECS: f32 = 0.5;



/// 引擎接线与分析状态。
pub struct AnalysisState {
    /// 引擎状态机（侧栏直接显示）。
    pub engine: EngineStatus,
    /// 当前局面的最新分析快照；局面变化即作废。
    pub snapshot: Option<Snapshot>,
    /// 瞬时错误（查询被拒 / 超时等）：引擎进程仍可用，显示但不进错误状态。
    pub transient_error: Option<String>,
    /// 最近一条引擎日志（诊断用）。**侧栏「引擎」卡片直接读取**（弱色
    /// 单行显示）：引擎崩溃 / 查询被拒时用户至少能看到一行引擎侧原因，
    /// 而不必去翻日志文件。全部最近行见 [`Self::log_tail_slice`]。
    pub last_log: Option<String>,
    /// 引擎 stderr 日志的**最近 [`LOG_TAIL_LINES`] 行**环形缓冲：
    /// 只留最后一行时，崩溃前的关键原因常被启动期日志冲掉。
    log_tail: std::collections::VecDeque<String>,
    /// 引擎「顶层未知字段」警告（引擎重启时清除、按字段名去重）：同一
    /// 字段只提示一条，避免流式查询每 0.5s 一条报告前都警告一次刷屏。
    /// 侧栏「消息」卡片直接读取（WARN 色）——不能只落 `last_log`：
    /// 它没有任何界面展示，用户看不见，等于没修。
    engine_warnings: Vec<EngineWarning>,
    /// 逐手胜率历史：键 = 局面签名（根到该局面着法前缀的 FNV-1a，
    /// 见 [`position_sig`]）。谱树中不同分支的同一手数是不同局面，
    /// 各存各的：切换分支不互相覆盖，切回来数据仍在。
    /// 曲线 / 失误统计（`ui::curve` / `ui::overlay`）经 [`Self::line_points`]
    /// 只取当前线上的点，缺口留空。
    history: HashMap<u64, HistoryPoint>,
    /// 每个局面的**候选表快照**（局后统计吻合度用），键与 [`Self::history`]
    /// 相同 = 局面签名。不并入 `HistoryPoint` 的理由：后者是 `Copy` 且在
    /// `line_points()`（曲线 / 失误标注 / 讲解每帧各调一次）里整段复制，
    /// 塞进 `Vec` 会让每帧白搬候选表（见类型文档）。
    /// 生命周期与 `history` 严格一致：写入在同一个覆盖判定里同进退
    /// （见 `record_history`），`reset()` 一并清空。
    candidates: HashMap<u64, CandidateSnap>,
    handle: Option<Engine>,
    inflight: Option<Inflight>,
    /// 上次发起查询时的局面签名（手数记录前缀）；`None` 表示尚未分析过。
    analyzed_sig: Option<Vec<MoveRecord>>,
    /// 当前局面的**走子口径**终态报告（按难度 visits 完整搜索后落位）。
    /// 与展示快照分离：展示可以比走子浅（快阶段 / 配置值），绝不能反过来；
    /// `engine_move_decision` 只认这里的报告，难度设置才真正生效。
    play_snapshot: Option<Snapshot>,
    /// 走子口径报告对应的局面签名与查询难度：签名不符（局面已变）或
    /// 难度不符（对弈中改了难度）即作废并重新发起走子查询。
    play_sig: Option<(Vec<MoveRecord>, Difficulty)>,
    /// 当前生效的选点限制（限定区域 / 排除选点）。由侧栏与棋盘交互写入；
    /// 每次变更递增 [`Self::limits_epoch`] 触发查询重发。
    limits: AnalysisLimits,
    /// 限制版本号：与「上次发起查询时的版本」比对，不一致即重发。
    limits_epoch: LimitsEpoch,
    /// 上次发起查询时的限制版本号。
    sent_epoch: LimitsEpoch,
    /// 策略热度图层的请求开关（由 `App` 随 Overlay 开关写入）。opt-in
    /// 数据必须随开关变化重新查询：引擎不会主动补发/撤回 policy 字段，
    /// 打开后已完成的快照没有 policy，需重发；关闭后继续收 policy 纯属
    /// 浪费（每条报告 +5 KB）。与 limits epoch 同一机制（局面未变也重发）。
    want_policy: bool,
    /// 上次发起查询时的 policy 开关值（比对不一致即重发）。
    sent_policy: bool,
    /// 候选点级 ownership 的请求开关（由 `App` 随 Overlay 复选框写入）。
    /// 与 [`Self::want_policy`] 同一套 opt-in 两值比对 + 重发机制：
    /// 引擎不会主动补发/撤回 per-move ownership，开关切换必须重发查询。
    want_moves_ownership: bool,
    /// 上次发起查询时的候选点级 ownership 开关值。
    sent_moves_ownership: bool,
    /// PV 每手计算量（`includePVVisits`）的请求开关。opt-in 纪律与
    /// policy / moves ownership 一致：查询级字段，引擎不感知开关事件，
    /// want/sent 两值比对不一致即重发（局面未变也重发）。体积代价是
    /// 紧凑整型数组（每项 1–3 字节 × 候选数 × PV 长度，实测见
    /// `engine::protocol` 模块文档），但仍只在需要时 opt-in。
    want_pv_visits: bool,
    /// 上次发起查询时的 `includePVVisits` 开关值。
    sent_pv_visits: bool,
    /// 整谱快扫任务（`Some` = 进行中）。与常规在飞查询（`inflight`）完全
    /// 独立：批量报告只回填历史，不落展示快照（`on_report` 按 stage 隔离）。
    batch: Option<BatchJob>,
    /// 整谱快扫配置（侧栏卡片编辑；`App` 经 [`Self::set_batch_config`]
    /// 写入，发起与预估时按当前线重新钳制）。不随换谱重置：`to` 的
    /// 「跟随线尾」哨兵让换到更长的棋时依旧全额。
    batch_config: BatchConfig,
    /// 批量刚结束时的用户提示（完成 / 取消），侧栏读取后由 App 清除。
    batch_notice: Option<String>,
    /// 参照库打分任务（阶段 22.3；`Some` = 进行中）。与整谱快扫（
    /// [`Self::batch`]）完全独立的批处理：状态不与当前对局耦合（参照
    /// 与对局正交，打分期间用户照常复盘），报告按树内路径回填进
    /// [`Self::ref_scan_results`]。调度模式（组间串行、切块、心跳、
    /// terminate）复用快扫，见 `refscan` 模块文档。
    ref_scan: Option<refscan::RefScanJob>,
    /// 参照打分的发起配置（范围 + 思考量档位）。对话框每帧经
    /// [`Self::ref_scan_config`] 读 / [`Self::set_ref_scan_config`] 写，
    /// 预估与发起共用同一份（快扫 `batch_config` 同款单一来源）。
    /// 发起时整份定格进任务，进行中改配置不影响在飞扫描。
    ref_scan_config: refscan::RefScanConfig,
    /// 参照库打分的已完成结果（按树内路径键）。**本会话内保存、换参照
    /// 即清空**（`detach_ref_scan`），不做持久化。
    ref_scan_results: refscan::RefScanResults,
    /// 参照库打分刚结束时的用户提示（完成 / 取消 / 换参照清空）。
    ref_scan_notice: Option<String>,
    /// 候选类显示的门控模式（第 5 项）。只影响绘制路径的取数判定，
    /// 不影响任何数据接收与走子决策（见 [`CandidateGating`] 文档）。
    gating: CandidateGating,
    /// 期望的分析规则（`None` = 自动跟随棋谱，由 `App` 从引擎配置写入）。
    /// 与解析生效值（want/sent 比对）共同驱动重发；历史清空绑定在
    /// **实际生效规则的变化**上（`data_rules`，见其文档），而非设置值
    /// 变化本身——设置改动可能并不改变解析结果（如自动跟随下换谱）。
    want_rules: Option<String>,
    /// 上次发起查询时实际发给引擎的规则（含自动解析结果，规范名）。
    sent_rules: Option<String>,
    /// 上次发起查询时发给引擎的贴目。贴目是查询级字段（`komi`），换谱 /
    /// 新对局后贴目变化必须重发查询，否则旧贴目的分析结果会一直挂在
    /// 新贴目的局面上（载入 KM 不同的谱时表现最明显）。
    sent_komi: Option<f64>,
    /// 历史与候选表当前**数据的规则口径**（`None` = 无数据）。request
    /// 时刻与解析结果比对：实际变化才清空历史 + 提示（两种规则的
    /// 胜率 / 目差不可混在同一条曲线，见模块文档「规则口径」）。
    data_rules: Option<String>,
    /// 目数视角（显示口径）：变化即作废快照并重发查询（`sent_view`
    /// 两值比对，复刻 `want_policy` 机制）。历史数据不必清空——存储
    /// 恒黑视角，同一份数据两种显示视角两用。
    want_view: DisplayView,
    /// 上次发起查询时请求的视角。
    sent_view: DisplayView,
    /// 最近一次规则解析的「未识别规则串」提示（消息区显示，由 App
    /// 取走转用户可见消息；每帧同步一次，规则未变时保持 `None`）。
    /// 读取方（`take_rules_notice`）走 Cell 之外的直接访问；写入方
    /// `effective_rules` 需要 `&self`（见其文档），故额外持一枚写入
    /// 开关的 Cell。**同帧至多写一次**（sync 与 request 各调一次解析，
    /// 第二次只在开关为真时写入），无覆盖竞态。
    rules_notice: Option<String>,
    /// `effective_rules` 本次 sync 已产生新提示的标记（`&self` 写入
    /// `rules_notice` 的中介，见该函数文档）。
    rules_notice_slot: std::cell::Cell<Option<String>>,
    /// 终局评估任务（双方弃着终局后由 App 请求）：按目标 visits 对
    /// **终局面**做一次加深分析，取当时可用的最深终态 scoreLead 作为
    /// RE 的依据（口径：引擎评估即结果，见 App 的终局路径）。
    /// `None` = 无任务。局面变化（用户浏览）不取消任务——终局后棋盘
    /// 不再变化，签名恒匹配；换谱 / 新对局随 `reset()` 清除。
    terminal_eval: Option<TerminalEval>,
    /// 「排除首选看次优」便捷入口（菜单）**自己**钉住的那一手。
    ///
    /// 便捷入口是开关式：再点一次要撤销**刚才那次**排除，而不是清掉
    /// 全部排除（那会把用户手动排除的手一起清掉）。因此需要单独记住
    /// 「这个入口加入的是哪一手」——只看「avoid 列表非空」没法区分
    /// 「入口加的」与「用户手动右键 / 候选行加的」。`None` = 入口当前
    /// 未排除任何手（或已被用户从菜单手动移除，见 `avoid_top_pinned`）。
    avoid_pin: Option<(Stone, Coord)>,
    /// 当前展示查询的**思考过程记录**（流式中间报告蒸馏的时间线 + 稳定
    /// 性读数，见 [`thinking`] 模块文档）。只描述「这一次搜索」：新的展示
    /// 查询发出即清空重记（`request` 内），换谱 / 新对局随 `reset` 清空；
    /// 走子口径（不开流式）与批量查询（独立口径）不经过本字段。容量
    /// [`THINKING_CAP`] 条，超限丢最早。
    thinking: ThinkingLog,
}




impl AnalysisState {
    pub fn new() -> Self {
        Self {
            engine: EngineStatus::Starting,
            snapshot: None,
            transient_error: None,
            last_log: None,
            log_tail: std::collections::VecDeque::with_capacity(LOG_TAIL_LINES),
            engine_warnings: Vec::new(),
            history: HashMap::new(),
            candidates: HashMap::new(),
            handle: None,
            inflight: None,
            analyzed_sig: None,
            play_snapshot: None,
            play_sig: None,
            limits: AnalysisLimits::default(),
            limits_epoch: LimitsEpoch(0),
            sent_epoch: LimitsEpoch(0),
            want_policy: false,
            sent_policy: false,
            want_moves_ownership: false,
            sent_moves_ownership: false,
            want_pv_visits: false,
            sent_pv_visits: false,
            batch: None,
            batch_config: BatchConfig::default(),
            batch_notice: None,
            ref_scan: None,
            ref_scan_config: refscan::RefScanConfig::default(),
            ref_scan_results: HashMap::new(),
            ref_scan_notice: None,
            gating: CandidateGating::Immediate,
            want_rules: None,
            sent_rules: None,
            sent_komi: None,
            data_rules: None,
            want_view: DisplayView::Black,
            sent_view: DisplayView::Black,
            rules_notice: None,
            rules_notice_slot: std::cell::Cell::new(None),
            terminal_eval: None,
            avoid_pin: None,
            thinking: ThinkingLog::default(),
        }
    }

    // ---- 候选类显示门控（第 5 项）----

    /// 当前门控模式（侧栏显示用）。
    pub fn gating(&self) -> CandidateGating {
        self.gating
    }

    /// 设置门控模式（侧栏切换后由 App 转存）。只改显示判定，不重发查询、
    /// 不作废快照——数据流完全不变。
    pub fn set_gating(&mut self, gating: CandidateGating) {
        self.gating = gating;
    }

    /// 此刻是否允许显示**候选类**内容（候选点圆圈 / PV 幽灵子 / 侧栏候选
    /// 列表 / 候选点领地层）。
    ///
    /// 判定口径：
    /// - **立即**：恒真（现状行为）；
    /// - **延迟 N 秒**：当前查询发起已满 N 秒，或最新展示快照已是**终态**
    ///   （终态一律立即显示——分析已结束，不存在「浅层误导」问题；查询
    ///   结束后快照仍在，`is_final` 恒可读）。快照为非终态而当前又**没有**
    ///   在飞查询（例如查询刚被开关变更作废的那一帧、或引擎已退出）时按
    ///   「未满」处理、继续隐藏：宁可少显示一帧，也不显示可能过期的浅层结果；
    /// - **手动**：只看 App 传入的「用户已按 F」标志；按过键之后局面一变
    ///   由 App 负责清掉该标志（重新要求确认），本层不记忆。
    ///
    /// **绝不影响走子决策**：`play_snapshot` / `engine_move_decision` 的
    /// 取数路径不经过本函数；本函数只在绘制与侧栏渲染前被调用。
    pub fn candidates_visible(&self, manual_revealed: bool) -> bool {
        match self.gating {
            CandidateGating::Immediate => true,
            CandidateGating::Manual => manual_revealed,
            CandidateGating::Delayed { secs } => {
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.is_final)
                {
                    return true;
                }
                self.inflight
                    .as_ref()
                    .is_some_and(|inflight| inflight.started.elapsed().as_secs_f32() >= secs as f32)
            }
        }
    }

    /// 手动模式下「是否正在等待按键确认」（不刺眼的状态提示用）：
    /// 门控为手动、当前有候选数据可显示、但用户尚未按键。
    pub fn awaiting_manual_reveal(&self, manual_revealed: bool) -> bool {
        self.gating == CandidateGating::Manual
            && self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.moves.is_empty())
            && !manual_revealed
    }

    /// 每帧调用（`App::logic`）：轮询引擎事件并按局面推进分析。
    ///
    /// `komi` 为当前对局的贴目（随查询发给引擎；复盘无贴目信息时用 7.5）。
    /// `game_rules` 为当前棋谱的 `RU[]` 原始串（`None` = 无棋谱或谱上未写）；
    /// 与设置里的规则偏好一起按「显式 > 棋谱（宽容映射）> 默认」解析成
    /// 引擎规范名（绝不透传原始串）。`want_play_query`：是否需要为本局面
    /// 准备**走子口径**的依据（对弈模式开启、未结束、轮到引擎且在活子
    /// 位置时为 `true`，由 `app` 判定——本层不感知对弈状态，避免复盘时
    /// 白烧走子预算）。
    ///
    /// 查询顺序：轮到引擎应手的局面**先发 Play（难度值）再补展示查询**——
    /// 引擎的搜索树跨查询存活，若展示查询先按配置值跑过，随后按更低难度值
    /// 的查询会立刻用满缓存返回，难度形同虚设（见任务实测）。展示查询开启
    /// 流式中间报告，边搜边刷新界面。
    pub fn sync(
        &mut self,
        board: &Board,
        cfg: &EngineConfig,
        komi: f64,
        game_rules: Option<&str>,
        want_play_query: bool,
        play_budget: Option<crate::play::TimeBudget>,
    ) {
        // 局面签名 = 根到当前节点的着法序列：落子 / 导航 / 悔棋 / 改着都会使其变化。
        let sig = board.records();
        // 整谱快扫期间局面一变（切分支 / 落子 / 切副本 / 载谱 / 导航）即
        // 自动取消：批量的报告按「发起时的线」回填，线变了继续算只会
        // 污染新盘面的历史（比对游标局面而非整条线）。
        if let Some(job) = self.batch.as_ref()
            && !job.watch.eq(sig)
        {
            self.cancel_batch();
            self.batch_notice = Some("局面已变化，整谱快扫已自动取消。".to_owned());
        }
        // 批量驱动：派发下一块 / 卡死兜底。卡死判定看**心跳**（最近一次
        // 收到该块报告的时刻）：距上次收到报告超过 [`BATCH_CHUNK_STALL`]
        // 才判定僵死收尾（查询被拒后引擎不再补发等异常路径）。不能从
        // 派发时刻计时——快扫期间交互查询插队会把单块拖过 90s，那是
        // 正常的慢，不是死；慢但仍在出报告就绝不放弃，批量永不悬挂。
        if let Some(job) = self.batch.as_ref()
            && let Some(chunk) = job.active.as_ref()
            && chunk.last_report.elapsed() >= BATCH_CHUNK_STALL
        {
            self.finish_batch(false);
        }
        if self.batch.is_some() {
            self.dispatch_batch_chunk();
            // 完成兜底（事件循环外每帧复查）：主扫描 = 有效报告数达到
            // 计划局面数；加深 = 派发完且收齐。卡死兜底在其上方单独收尾。
            if let Some(job) = self.batch.as_ref() {
                let complete = if job.deepening {
                    job.deepen_pos >= job.deepen_turns.len() && job.active.is_none()
                } else {
                    job.group >= job.plan.groups.len()
                        && job.done >= job.plan.positions
                        && job.active.is_none()
                };
                if complete {
                    self.finish_batch(true);
                }
            }
        }
        // ---- 参照库打分（独立批处理；调度与快扫同款，状态互不相干）----
        // 卡死兜底（同一心跳口径）→ 派发 → 完成判定（派发完 + 收齐）。
        if let Some(job) = self.ref_scan.as_ref()
            && job.stalled()
        {
            self.finish_ref_scan(false);
        }
        if self.ref_scan.is_some() {
            self.dispatch_ref_scan_chunk();
            if let Some(job) = self.ref_scan.as_ref()
                && job.chain >= job.plan.chains.len()
                && job.done >= job.plan.positions
                && job.active.is_none()
            {
                self.finish_ref_scan(true);
            }
        }
        // 限制版本变化（设置区域 / 排除 / 清除）即使局面未变也要重发查询：
        // 限制是查询级字段，引擎不感知「用户改了限制」这一事件。
        // policy 与候选点级 ownership 开关同理：查询级 opt-in 字段，
        // 开关切换必须重发才生效。
        // 规则与视角同为查询级口径：want/sent 两值比对（复刻 policy 机制），
        // 不一致即重发（局面未变也重发）。
        let limits_changed = self.limits_epoch != self.sent_epoch;
        let policy_changed = self.want_policy != self.sent_policy;
        let moves_ownership_changed = self.want_moves_ownership != self.sent_moves_ownership;
        let pv_visits_changed = self.want_pv_visits != self.sent_pv_visits;
        let view_changed = self.want_view != self.sent_view;
        // 贴目：查询级字段，want/sent 比对（复刻 policy 机制）——载入
        // KM 不同的谱 / 新对局换贴目后，局面未变也必须重发查询。
        let komi_changed = self.sent_komi != Some(komi);
        // 每帧解析一次生效规则（设置的显式值 > 棋谱 RU 的宽容映射 > 默认）：
        // 换谱 / 改设置后解析结果变化即重发查询。开销可忽略（短串上几次
        // 大小写归一与关键词匹配）。
        let resolved_rules = self.effective_rules(game_rules);
        let rules_changed = self.sent_rules.as_deref() != Some(resolved_rules.as_str());
        if Some(sig) != self.analyzed_sig.as_deref()
            || limits_changed
            || policy_changed
            || moves_ownership_changed
            || pv_visits_changed
            || view_changed
            || rules_changed
            || komi_changed
        {
            self.snapshot = None;
            self.transient_error = None;
            if let Some(inflight) = self.inflight.take()
                && let Some(handle) = self.handle.as_mut()
            {
                handle.terminate(inflight.id);
            }
            if matches!(self.engine, EngineStatus::Ready) {
                let stage = if want_play_query {
                    Stage::Play
                } else {
                    Stage::Analysis
                };
                self.request(
                    board,
                    cfg,
                    stage,
                    komi,
                    game_rules,
                    play_budget.map(|b| b.max_time),
                );
            }
        }
        while let Some(event) = self.handle.as_mut().and_then(Engine::try_recv) {
            self.on_event(event, board, cfg, komi, game_rules);
        }
        // 走子口径补发（同一局面、引擎空闲时）：难度刚改 / 对弈模式后开
        // / 上一份走子报告被丢弃。展示已按配置值搜过的局面受树缓存影响，
        // 本次查询的 visits 可能高于难度值（至多一手，见模块文档）。
        let play_stale = self
            .play_sig
            .as_ref()
            .is_none_or(|(sig, d)| *d != cfg.play_difficulty || !sig.eq(board.records()));
        if matches!(self.engine, EngineStatus::Ready)
            && want_play_query
            && play_stale
            && self.inflight.is_none()
        {
            self.request(
                board,
                cfg,
                Stage::Play,
                komi,
                game_rules,
                play_budget.map(|b| b.max_time),
            );
        }
        // 终局评估派发：有任务、签名匹配、引擎空闲，且「当前签名的最深
        // 终态评估」尚未达到目标 visits 时，以目标 visits 发起一次展示
        // 口径查询（复用既有管线，无新机制；终态落 history 后由
        // `on_report` 回填 best）。签名不匹配（换谱 / 新对局）即丢任务。
        if let Some(job) = self.terminal_eval.as_ref() {
            if !job.sig.eq(sig) {
                self.terminal_eval = None;
            } else if matches!(self.engine, EngineStatus::Ready)
                && self.inflight.is_none()
                && job.best.is_none_or(|(visits, _)| visits < u64::from(job.target_visits))
            {
                self.request(
                    board,
                    cfg,
                    Stage::Analysis,
                    komi,
                    game_rules,
                    None,
                );
            }
        }
    }

    // ---- 思考过程记录与回放（阶段 21 #2）----

    /// 当前查询的思考记录条数（0 = 本次无思考过程：对弈走子查询 / 快扫
    /// / 树缓存秒回都属此类，侧栏如实显示，不区分原因）。
    pub fn thinking_len(&self) -> usize {
        self.thinking.len()
    }

    /// 回放游标取一条思考记录：`index` 为时间线内下标，越界返回 `None`。
    /// 滑杆拖到哪显示哪；「最新一条」由调用方用
    /// `index + 1 == thinking_len()` 判定。
    pub fn thinking_entry(&self, index: usize) -> Option<ThinkingEntryRef<'_>> {
        self.thinking.get(index).map(|entry| ThinkingEntryRef {
            elapsed: entry.elapsed,
            visits: entry.visits,
            best: entry.best,
            top: &entry.top,
            top_len: entry.top_len,
        })
    }

    /// 结论稳定性读数（首选翻转次数与最后翻转耗时，见 [`Stability`]）。
    /// 门控隐藏选点时读数**仍然可显示**：它不含任何具体选点，
    /// 与「不剧透」不冲突（翻转次数本身不告诉用户该走哪里）。
    pub fn thinking_stability(&self) -> Stability {
        self.thinking.stability()
    }

    // ---- 终局评估（双方弃着终局的 RE 依据）----

    /// 请求对当前局面做终局评估（App 在双方弃着终局时调用）：现有终态
    /// 快照 visits 低于 `target` 时以 `target` 发起加深查询，到达后
    /// [`Self::deepest_terminal_eval`] 给出更深的值；已达标则原值直接
    /// 可用，不发查询。
    pub fn request_terminal_eval(&mut self, board: &Board, target: u32) {
        // 手上终态已够深：直接记录为结果，无需查询。
        let have = self
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.is_final && snapshot.turn == board.cursor())
            .and_then(|snapshot| snapshot.root.as_ref())
            .map(|root| (root.visits, root.score_lead));
        self.terminal_eval = Some(TerminalEval {
            sig: board.records().to_vec(),
            target_visits: target,
            best: have,
        });
    }

    /// 当时可用的最深终局评估（黑方视角 scoreLead）：优先取任务记录
    /// 的最深终态（可能比展示快照深），否则回落当前终态快照；都没有
    /// 返回 `None`（RE 写 `?`，不猜）。`Some((visits, lead))` 的
    /// visits 供验证日志与界面口径标注。
    pub fn deepest_terminal_eval(&self, board: &Board) -> Option<(u64, f64)> {
        // 任务存在且签名匹配 ⇒ 用任务的最深记录；任务不存在（未请求）
        // 时回落快照。
        if let Some(job) = self.terminal_eval.as_ref() {
            if !job.sig.eq(board.records()) {
                return None;
            }
            return job.best;
        }
        self.snapshot
            .as_ref()
            .filter(|snapshot| snapshot.is_final && snapshot.turn == board.cursor())
            .and_then(|snapshot| snapshot.root.as_ref())
            .map(|root| (root.visits, root.score_lead))
    }

    /// 终局评估是否仍在加深（界面「终局评估中…」的判定）。
    pub fn terminal_eval_pending(&self, board: &Board) -> bool {
        self.terminal_eval
            .as_ref()
            .is_some_and(|job| job.sig.eq(board.records()) && job.best.is_none_or(|(visits, _)| visits < u64::from(job.target_visits)))
    }

    /// 载入新棋谱时清空全部分析状态：作废在飞查询与快照、清空逐手
    /// 胜率历史（新对局不能混入旧曲线）。引擎进程保持运行，下一帧
    /// `sync` 会因局面签名变化自动对新局面发起查询。
    pub fn reset(&mut self) {
        if let Some(inflight) = self.inflight.take()
            && let Some(handle) = self.handle.as_mut()
        {
            handle.terminate(inflight.id);
        }
        // 载谱 / 新对局：批量静默丢弃（历史即将清空，任务已无意义）。
        // 在飞块同样 terminate：不终止的话引擎会把 ≤20 手（约 19s）算完
        // 才歇，白烧 GPU；残余报告因历史清空自然失效。
        if let Some(job) = self.batch.take()
            && let Some(chunk) = job.active
            && let Some(handle) = self.handle.as_mut()
        {
            handle.terminate(chunk.id);
        }
        // 参照打分与对局正交（载谱不换参照），任务本身照跑；但收尾提示
        // 里的「换参照清空」口径不在此触发（那是 detach_ref_scan 的职责，
        // 由参照加载 / 卸载路径调用）。
        self.batch_notice = None;
        self.analyzed_sig = None;
        self.snapshot = None;
        self.transient_error = None;
        self.history.clear();
        self.candidates.clear();
        self.play_snapshot = None;
        self.play_sig = None;
        self.terminal_eval = None;
        // 思考过程随谱走：换谱 / 新对局后旧记录描述的是别的局面与查询，
        // 保留只会误导（新查询发出时也会清，这里提前清保口径一致）。
        self.thinking.clear();
    }

    /// 退出时优雅关闭引擎进程（`App::on_exit` 调用）。
    pub fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
    }

}

impl Default for AnalysisState {
    fn default() -> Self {
        Self::new()
    }
}
