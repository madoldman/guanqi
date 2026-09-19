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
pub struct AnalysisState {
    /// 引擎状态机（侧栏直接显示）。
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

    /// 每帧调用（`App::logic`）：轮询引擎事件并按局面推进分析。
    pub fn sync(&mut self, board: &Board, cfg: &EngineConfig) {
        // 局面签名 = 游标前的手数记录前缀：落子 / 导航 / 悔棋 / 改着都会使其变化。
        let sig = &board.records()[..board.cursor()];
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
            let sig = position_sig(&board.records()[..turn]);
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
        let moves: Vec<(Stone, Action)> = board.records()[..board.cursor()]
            .iter()
            .map(|record| (record.player, record.action))
            .collect();
        let mut query = AnalysisQuery::new(board.size(), moves);
        query.max_visits = Some(stage.cap(cfg));
        // 热度图需要 ownership（opt-in，引擎缺省不返回该字段）：
        // 此处为单点改动处，快 / 深两阶段都会带回。
        query.include_ownership = true;
        let id = handle.analyze(query);
        self.analyzed_sig = Some(board.records()[..board.cursor()].to_vec());
        self.inflight = Some(Inflight {
            id,
            turn: board.cursor(),
            stage,
            started: Instant::now(),
        });
    }
}
