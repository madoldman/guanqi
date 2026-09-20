//! 引擎接线与分析状态：生命周期状态机 + 「分段加深」查询调度（TASKS 3.3 / 4.2 接线层）。
//!
//! 职责与协议事实的对应关系（均见 `engine` 模块文档）：
//!
//! - [`EngineEvent::Ready`] 之前不能发查询；启动（含模型加载 / OpenCL 调优）
//!   实测可达数十秒，期间 UI 停留在 [`EngineStatus::Starting`]；
//! - `katago analysis` 对每个 turn 只回一份**终态**报告，渐进体验靠
//!   「同局面先发低 visits 快查询、出结果后自动加深到配置值」实现
//!   （引擎搜索树跨查询复用，实测加深几乎免费）；
//! - 局面变化时 [`Engine::terminate`] 旧查询，并按 id 丢弃过期补发报告；
//! - 进程退出（[`EngineEvent::Exited`]）进入可重试的 [`EngineStatus::Failed`]。
//!
//! [`AnalysisState::snapshot`] 是分析结果的唯一存放点：侧栏读它显示，
//! 棋盘候选点叠加层 / 热度图也直接取用，避免二次搬运。
//! 逐手胜率历史（[`AnalysisState::history`]，TASKS 4.3 曲线用）只是
//! 终态快照的按手数转存：随浏览逐步积累，切换局面不清空，
//! 同一手数后到且 visits 更多的终态覆盖先到的。
//! 每手损失（[`AnalysisState::move_loss`]，TASKS 4.4）不另存状态：
//! 读取时由相邻两个已知历史点现场派生，引擎零额外查询。

use std::sync::Arc;
use std::time::Instant;

use crate::board::{Board, MoveRecord, Size, Stone, Action};
use crate::engine::{
    AnalysisQuery, AnalysisReport, Engine, EngineConfig, EngineError, EngineEvent, QueryId,
    RootInfo, MoveInfo,
};

/// 引擎事件唤醒回调：与 `engine::process::Waker` 同构（类型别名未公开，
/// 此处按相同定义书写，透明等价）。
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// 快速阶段的 visits 上限：局面刚变化时先小预算出结果，再自动加深。
const FAST_VISITS: u32 = 100;

/// 逐手历史的容量上限（手数）。19 路盘的实用对局远小于此，
/// 超出部分不再写入，避免无界增长。
const HISTORY_CAP: usize = 999;

// ---- 每手损失分级阈值（目差口径，正 = 行棋方亏损）----
//
// 参考主流 AI 复盘工具按目差损失分档的惯例（1 / 3 / 6 目为常用分界）：
//
// - 0.3 目以下视为搜索噪声：visits 有限时引擎对同一点位的 scoreLead
//   复评存在零点几目的抖动，不应据此判亏；
// - 1 目：贴目制下连续出现即足以翻盘，「明显亏损」的下限；
// - 3 目：约一个普通官子的价值，通常是局部选点错误的量级；
// - 6 目：约半手棋（中盘一手价值 10–15 目），多为漏看或死活误判。

/// 好棋与尚可的分界（目）：低于此值视为搜索噪声。
const SEVERITY_GOOD_MAX: f64 = 0.3;
/// 疑问手下限（目）。
const SEVERITY_QUESTIONABLE_MIN: f64 = 1.0;
/// 失误下限（目）。
const SEVERITY_MISTAKE_MIN: f64 = 3.0;
/// 恶手下限（目）。
const SEVERITY_BLUNDER_MIN: f64 = 6.0;

/// 逐手历史的一个数据点：第 `turn` 手之后局面的终态分析结果。
/// 胜率 / 目差为**黑方视角**（与 [`Snapshot`] 同一口径，不翻转）。
#[derive(Clone, Copy, Debug)]
pub struct HistoryPoint {
    /// 手数（0 = 初始空盘）。
    pub turn: usize,
    /// 黑方胜率 [0,1]。
    pub winrate: f64,
    /// 黑方目差（正 = 黑领先）。
    pub score_lead: f64,
    /// 产出该结果的搜索量（覆盖判定依据：visits 更多的后到覆盖先到的）。
    pub visits: u64,
}

/// 第 `turn` 手（1 起）的行棋方视角损失：由第 `turn − 1` 与第 `turn` 手后
/// 两个**已知**历史点现场派生（[`AnalysisState::move_loss`]），任一端缺失
/// 即为「未知」，不插值、不臆造 0。
#[derive(Clone, Copy, Debug)]
pub struct MoveLoss {
    /// 手数（1 起）。
    pub turn: usize,
    /// 行棋方。
    pub player: Stone,
    /// 目差损失（目，正 = 行棋方亏损），严重程度分级的依据。
    pub score_loss: f64,
    /// 胜率损失 [0, 1]（正 = 行棋方亏损），辅助信息。
    pub winrate_loss: f64,
    /// 严重程度分级。
    pub severity: Severity,
}

/// 每手损失的严重程度（按目差损失分档，阈值依据见各常量文档）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    /// 好棋：不亏（含搜索噪声以内）。
    Good,
    /// 尚可：小亏，常规次优。
    Fine,
    /// 疑问手：明显亏损。
    Questionable,
    /// 失误：局部量级亏损。
    Mistake,
    /// 恶手：全局量级亏损。
    Blunder,
}

impl Severity {
    /// 由目差损失定档。
    fn from_score_loss(loss: f64) -> Self {
        if loss < SEVERITY_GOOD_MAX {
            Self::Good
        } else if loss < SEVERITY_QUESTIONABLE_MIN {
            Self::Fine
        } else if loss < SEVERITY_MISTAKE_MIN {
            Self::Questionable
        } else if loss < SEVERITY_BLUNDER_MIN {
            Self::Mistake
        } else {
            Self::Blunder
        }
    }

    /// 中文名（侧栏汇总与曲线悬停显示）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Good => "好棋",
            Self::Fine => "尚可",
            Self::Questionable => "疑问手",
            Self::Mistake => "失误",
            Self::Blunder => "恶手",
        }
    }

    /// 是否在棋盘上标注：只标疑问手及以上，好棋 / 尚可不标。
    pub fn is_marked(self) -> bool {
        self >= Self::Questionable
    }
}

/// 失误汇总统计（[`AnalysisState::loss_summary`]，侧栏显示用）。
#[derive(Clone, Copy, Debug, Default)]
pub struct LossSummary {
    /// 总手数。
    pub total: usize,
    /// 两端数据齐全、可算损失的手数；其余手数状态为「未知」。
    pub analyzed: usize,
    /// 疑问手 / 失误 / 恶手计数（好棋与尚可不统计，避免噪声凑数）。
    pub questionable: u32,
    pub mistake: u32,
    pub blunder: u32,
}

/// 引擎生命周期状态机（用户可见部分）。
///
/// 「分析中」不单独设状态：就绪且存在在飞查询即视为分析中，
/// 见 [`AnalysisState::analyzing`]。
#[derive(Debug)]
pub enum EngineStatus {
    /// 权重未配置，无法启动（提示去设置面板选择）。
    Unconfigured,
    /// 子进程已启动，等待就绪标记（模型加载 / 显卡调优可能数十秒）。
    Starting,
    /// 就绪，可接受查询。
    Ready,
    /// 引擎不可用（启动失败 / 启动超时 / 进程退出），可重试。
    Failed(String),
}

/// 当前局面的一份分析快照（终态报告）。
///
/// 阶段 4 的叠加层直接读取 `moves`（候选点圆圈）与 `ownership`（热度图）。
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// 被分析的手数（= 发起查询时的游标）。叠加层空局面判定（`turn == 0`）
    /// 与阶段 4 失误分析用。
    pub turn: usize,
    /// 查询时的棋盘尺寸（坐标 GTP 显示与阶段 4 叠加层换算用）。
    pub size: Size,
    /// 本快照的 visits 上限（快阶段为 [`FAST_VISITS`]，深阶段为配置值）。
    pub visits_cap: u32,
    /// 从发起到收到报告的耗时。
    pub elapsed: std::time::Duration,
    /// 根节点统计；空报告（`noResults`）为 `None`。
    pub root: Option<RootInfo>,
    /// 候选点，已按引擎排序（`order` 升序）。
    pub moves: Vec<MoveInfo>,
    /// 各点局势值（opt-in，查询时已开启）：下标与 `Coord::index` 一致，
    /// 正值 = 黑势。叠加层热度图直接取用。
    pub ownership: Option<Vec<f32>>,
}

/// 逐手历史条目：数据点 + 写入时的局面签名（覆盖判定用）。
#[derive(Clone, Copy, Debug)]
struct HistoryEntry {
    point: HistoryPoint,
    /// 该手数写入时的局面签名（[`Self::position_sig`]）：
    /// 悔棋后另行走子使同手数对应新局面，旧条目须无条件让位。
    sig: u64,
}

/// 局面签名：对手数记录前缀逐字节做 FNV-1a（行棋方 / 着法 / 提子）。
/// 仅用于本模块的覆盖判定，不要求抗碰撞。
fn position_sig(records: &[MoveRecord]) -> u64 {
    fn byte(h: &mut u64, b: u8) {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut h = 0xcbf2_9ce4_8422_2325;
    for r in records {
        byte(&mut h, u8::from(matches!(r.player, Stone::Black)));
        match r.action {
            Action::Place(c) => {
                byte(&mut h, 1);
                byte(&mut h, c.x());
                byte(&mut h, c.y());
            }
            Action::Pass => byte(&mut h, 0),
        }
        let n = r.captured.len();
        byte(&mut h, n as u8);
        byte(&mut h, (n >> 8) as u8);
        for c in &r.captured {
            byte(&mut h, c.x());
            byte(&mut h, c.y());
        }
    }
    h
}

/// 查询阶段：先快后深。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Fast,
    Deep,
}

impl Stage {
    /// 该阶段的 visits 上限（配置值为 0 时夹到 1，避免无效查询）。
    fn cap(self, cfg: &EngineConfig) -> u32 {
        let visits = cfg.visits.max(1);
        match self {
            Self::Fast => FAST_VISITS.min(visits),
            Self::Deep => visits,
        }
    }
}

/// 在飞查询。
struct Inflight {
    id: QueryId,
    turn: usize,
    stage: Stage,
    started: Instant,
}

/// 引擎接线与分析状态。
pub struct AnalysisState {    /// 引擎状态机（侧栏直接显示）。
    pub engine: EngineStatus,
    /// 当前局面的最新分析快照；局面变化即作废。
    pub snapshot: Option<Snapshot>,
    /// 瞬时错误（查询被拒 / 超时等）：引擎进程仍可用，显示但不进错误状态。
    pub transient_error: Option<String>,
    /// 最近一条引擎日志（诊断用）。
    pub last_log: Option<String>,
    /// 逐手胜率历史：下标 = 手数（0 = 初始空盘），`None` = 该手尚无终态数据。
    /// 随浏览逐步积累，切换局面（落子 / 导航 / 悔棋）不清空；
    /// 曲线（`ui::curve`）按手数取用，缺口留空。
    history: Vec<Option<HistoryEntry>>,
    handle: Option<Engine>,
    inflight: Option<Inflight>,
    /// 上次发起查询时的局面签名（手数记录前缀）；`None` 表示尚未分析过。
    analyzed_sig: Option<Vec<MoveRecord>>,
}

impl AnalysisState {
    pub fn new() -> Self {
        Self {
            engine: EngineStatus::Starting,
            snapshot: None,
            transient_error: None,
            last_log: None,
            history: Vec::new(),
            handle: None,
            inflight: None,
            analyzed_sig: None,
        }
    }

    /// 用当前配置（重）启动引擎；设置面板「应用并重启」与错误重试共用。
    /// 任何失败都进入可显示的状态，不 panic。
    pub fn start_engine(&mut self, cfg: &EngineConfig, waker: &Waker) {
        if let Some(old) = self.handle.take() {
            old.shutdown();
        }
        self.inflight = None;
        self.analyzed_sig = None;
        self.snapshot = None;
        self.transient_error = None;
        self.engine = if cfg.model_path.is_some() {
            match Engine::spawn_with_waker(cfg, Some(waker.clone())) {
                Ok(handle) => {
                    self.handle = Some(handle);
                    EngineStatus::Starting
                }
                Err(err) => EngineStatus::Failed(err.to_string()),
            }
        } else {
            EngineStatus::Unconfigured
        };
    }

    /// 是否有在飞查询（就绪 + 在飞 = 界面显示「分析中」）。
    pub fn analyzing(&self) -> bool {
        self.inflight.is_some()
    }

    /// 查询某手的逐手历史数据（0 = 初始空盘）；该手尚无终态数据时为 `None`。
    pub fn history_point(&self, turn: usize) -> Option<HistoryPoint> {
        self.history.get(turn).and_then(|slot| slot.map(|e| e.point))
    }

    /// 第 `turn` 手（1 起）的行棋方视角损失；`None` = 两端数据不全（未知）。
    ///
    /// 视角换算（`winrate` / `score_lead` 均为黑方视角，见 `HistoryPoint`）：
    /// 黑方行棋时损失 = 走子前黑方值 − 走子后黑方值；白方行棋时符号相反
    /// （黑方值升 = 白方亏）。结果一律为「行棋方失去了多少」，正 = 亏。
    /// 行棋方由调用方从 `Board::record_at(turn - 1)` 取。
    pub fn move_loss(&self, turn: usize, player: Stone) -> Option<MoveLoss> {
        if turn == 0 {
            return None; // 空盘没有「走子前」，无从谈损失
        }
        let before = self.history_point(turn - 1)?;
        let after = self.history_point(turn)?;
        let side = match player {
            Stone::Black => 1.0,
            Stone::White => -1.0,
        };
        let score_loss = (before.score_lead - after.score_lead) * side;
        let winrate_loss = (before.winrate - after.winrate) * side;
        Some(MoveLoss {
            turn,
            player,
            score_loss,
            winrate_loss,
            severity: Severity::from_score_loss(score_loss),
        })
    }

    /// 失误汇总统计：遍历全部手数现场派生（每手仅两次下标读取与算术，
    /// 可每帧调用）。「已分析」只计两端数据齐全的手数，缺口不计入分级。
    pub fn loss_summary(&self, board: &Board) -> LossSummary {
        // 只统计**当前线**：变着分支的手数不在当前线的复盘中。
        let mut summary = LossSummary { total: board.line_len(), ..LossSummary::default() };
        for i in 0..summary.total {
            let Some(record) = board.record_at(i) else { continue };
            let Some(loss) = self.move_loss(i + 1, record.player) else { continue };
            summary.analyzed += 1;
            match loss.severity {
                Severity::Questionable => summary.questionable += 1,
                Severity::Mistake => summary.mistake += 1,
                Severity::Blunder => summary.blunder += 1,
                Severity::Good | Severity::Fine => {}
            }
        }
        summary
    }

    /// 每帧调用（`App::logic`）：轮询引擎事件并按局面推进分析。
    pub fn sync(&mut self, board: &Board, cfg: &EngineConfig) {
        // 局面签名 = 根到当前节点的着法序列：落子 / 导航 / 悔棋 / 改着都会使其变化。
        let sig = board.records();
        if Some(sig) != self.analyzed_sig.as_deref() {
            self.snapshot = None;
            self.transient_error = None;
            if let Some(inflight) = self.inflight.take()
                && let Some(handle) = self.handle.as_mut()
            {
                handle.terminate(inflight.id);
            }
            if matches!(self.engine, EngineStatus::Ready) {
                self.request(board, cfg, Stage::Fast);
            }
        }
        while let Some(event) = self.handle.as_mut().and_then(Engine::try_recv) {
            self.on_event(event, board, cfg);
        }
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
        self.analyzed_sig = None;
        self.snapshot = None;
        self.transient_error = None;
        self.history.clear();
    }

    /// 退出时优雅关闭引擎进程（`App::on_exit` 调用）。
    pub fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
    }

    // ---- 内部 ----

    fn on_event(&mut self, event: EngineEvent, board: &Board, cfg: &EngineConfig) {
        match event {
            EngineEvent::Ready => {
                self.engine = EngineStatus::Ready;
                self.transient_error = None;
                self.request(board, cfg, Stage::Fast);
            }
            EngineEvent::Report { id, report, is_final } => {
                self.on_report(id, report, is_final, board, cfg);
            }
            EngineEvent::Log(line) => self.last_log = Some(line),
            EngineEvent::Failed(err) => self.on_failed(err),
            EngineEvent::Exited(status) => {
                self.inflight = None;
                self.engine = EngineStatus::Failed(format!("引擎进程已退出（{status}）"));
            }
        }
    }

    /// 查询级失败不影响引擎进程；其余（启动超时等）进入可重试错误状态。
    fn on_failed(&mut self, err: EngineError) {
        self.inflight = None;
        match err {
            EngineError::QueryTimeout { .. } | EngineError::QueryRejected { .. } => {
                self.transient_error = Some(err.to_string());
            }
            other => self.engine = EngineStatus::Failed(other.to_string()),
        }
    }

    /// 接收报告：按 id 丢弃过期补发，落快照；快查询终态后自动加深。
    fn on_report(
        &mut self,
        id: QueryId,
        report: AnalysisReport,
        is_final: bool,
        board: &Board,
        cfg: &EngineConfig,
    ) {
        let Some(inflight) = self.inflight.as_ref() else {
            return;
        };
        if inflight.id != id {
            return; // 旧查询被 terminate 后的补发终态：局面已变，丢弃
        }
        let (stage, turn, started) = (inflight.stage, inflight.turn, inflight.started);
        let mut moves = report.move_infos;
        moves.sort_by_key(|info| info.order);
        let root = report.root_info;
        self.snapshot = Some(Snapshot {
            turn,
            size: board.size(),
            visits_cap: stage.cap(cfg),
            elapsed: started.elapsed(),
            root: root.clone(),
            moves,
            ownership: report.ownership,
        });
        // 终态转存进逐手历史：局面未变时 visits 更高者胜（深阶段覆盖快阶段），
        // 局面已变（悔棋后另行走子）则无条件覆盖。
        if is_final
            && let Some(root) = &root
        {
            // 在飞报告的 turn 恒等于发起查询时的游标，局面未变即当前线的全部着法。
            let sig = position_sig(board.records());
            self.record_history(turn, root, sig);
        }
        if !report.no_results {
            self.transient_error = None;
        }
        if is_final {
            self.inflight = None;
            // 分段加深：快查询与深查询预算相同（配置值很小）时无需重复。
            if stage == Stage::Fast && stage.cap(cfg) < cfg.visits.max(1) {
                self.request(board, cfg, Stage::Deep);
            }
        }
    }

    /// 终态结果转存进逐手历史：下标 = 手数（0 = 初始空盘）。
    /// 同一手数：局面不同（`sig` 不符，悔棋后另行走子）无条件覆盖；
    /// 局面相同则 visits 不低于旧条目才覆盖（深阶段后到、覆盖快阶段）。
    /// 无根节点数据或超容量时跳过。
    fn record_history(&mut self, turn: usize, root: &RootInfo, sig: u64) {
        if turn > HISTORY_CAP {
            return;
        }
        if self.history.len() <= turn {
            self.history.resize(turn + 1, None);
        }
        let point = HistoryPoint {
            turn,
            winrate: root.winrate,
            score_lead: root.score_lead,
            visits: root.visits,
        };
        let overwrite = match self.history[turn].as_ref() {
            Some(entry) => entry.sig != sig || root.visits >= entry.point.visits,
            None => true,
        };
        if overwrite {
            self.history[turn] = Some(HistoryEntry { point, sig });
        }
    }

    /// 对当前局面发起查询（就绪且无在飞时才生效）。
    fn request(&mut self, board: &Board, cfg: &EngineConfig, stage: Stage) {
        if self.inflight.is_some() {
            return;
        }
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        let moves: Vec<(Stone, Action)> = board
            .records()
            .iter()
            .map(|record| (record.player, record.action))
            .collect();
        let mut query = AnalysisQuery::new(board.size(), moves);
        query.max_visits = Some(stage.cap(cfg));
        // 热度图需要 ownership（opt-in，引擎缺省不返回该字段）：
        // 此处为单点改动处，快 / 深两阶段都会带回。
        query.include_ownership = true;
        let id = handle.analyze(query);
        self.analyzed_sig = Some(board.records().to_vec());
        self.inflight = Some(Inflight {
            id,
            turn: board.cursor(),
            stage,
            started: Instant::now(),
        });
    }
}

impl Default for AnalysisState {
    fn default() -> Self {
        Self::new()
    }
}
