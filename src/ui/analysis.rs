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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::board::{Action, Board, Coord, MoveRecord, Size, Stone};
use crate::engine::{
    AnalysisQuery, AnalysisReport, Difficulty, Engine, EngineConfig, EngineError, EngineEvent,
    MoveRules, QueryId, RootInfo, MoveInfo,
};
/// 引擎事件唤醒回调：与 `engine::process::Waker` 同构（类型别名未公开，
/// 此处按相同定义书写，透明等价）。
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// 流式中间报告的输出间隔（秒）：协议实测 0.5s 键下 300 visits 产出约
/// 11 条中间报告（visits 27→51→…→303 递增），实时感与序列化压力平衡；
/// 更密的间隔只会放大 JSON 解析与重绘频次，实时感提升有限。
const REPORT_EVERY_SECS: f32 = 0.5;

/// 整谱快扫的每手搜索量：实测 b18 OpenCL 下约 0.94s/手（threads=1），
/// 80 手 ≈ 75 秒、250 手 ≈ 4 分钟；40 visits 的胜率/目差已稳定在
/// 好棋/尚可分界（0.3 目）的噪声量级内，够填曲线用（实测算依据见
/// /tmp/batch-notes.md §4）。
pub const BATCH_VISITS: u32 = 40;

/// 整谱快扫的交互查询优先级：实测带 `priority: 10` 的查询可在批量占满
/// 队列时数秒内插队返回（不带则被完全阻塞，见 /tmp/batch-notes.md §3）。
pub(crate) const INTERACTIVE_PRIORITY: i32 = 10;

/// 批量查询优先级：缺省 0（低于 [`INTERACTIVE_PRIORITY`]，排队即可）。
const BATCH_PRIORITY: i32 = 0;

/// 整谱快扫的分块大小（一次 `analyzeTurns` 的手数）：实测 40 visits 约
/// 0.94s/手，20 手一块 ≈ 19 秒，取消响应与进度刷新粒度都足够；块间串行
/// （引擎 search tree 跨查询缓存还能让相邻块的重叠局面加速）。
const BATCH_CHUNK: usize = 20;

/// 逐手历史容量上限（条目数，各分支分开计数）。19 路盘的实用对局
/// 远小于此，超出部分不再写入，避免无界增长。
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
/// （解说模块 [`super::explain`] 复用同一分界，避免两处阈值漂移。）
pub(crate) const SEVERITY_GOOD_MAX: f64 = 0.3;
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
/// 两个**已知**历史点现场派生（[`loss_from_points`]），任一端缺失
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
    /// 当前线总手数。
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

/// 当前局面的最新一份分析快照（流式中间报告或终态报告）。
///
/// 界面实时数值（胜率 / 目差 / 候选点 / 热度图）取**最新**报告——含
/// 中间报告，随搜索推进实时刷新；逐手胜率历史与失误分析只由终态写入
/// （见 [`AnalysisState::on_report`]），曲线不被中间值反复改写。
/// 阶段 4 的叠加层直接读取 `moves`（候选点圆圈）与 `ownership`（热度图）。
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// 被分析的手数（= 发起查询时的游标）。叠加层空局面判定（`turn == 0`）
    /// 与阶段 4 失误分析用。
    pub turn: usize,
    /// 查询时的棋盘尺寸（坐标 GTP 显示与阶段 4 叠加层换算用）。
    pub size: Size,
    /// 该查询的 visits 上限（常规分析为配置值，走子口径为难度值）。
    pub visits_cap: u32,
    /// 是否按该查询的 visits 上限**搜完的终态**（流式改造后的语义：
    /// 不再有快 / 深两段，`deep == is_final`）。人机对弈的自动应手只认
    /// 走子口径的终态快照——中间报告只用于渐进显示，不能拿去走子。
    pub deep: bool,
    /// 是否为终态报告（`false` = 引擎的渐进中间报告，后续还会更新）。
    pub is_final: bool,
    /// 从发起到收到报告的耗时。
    pub elapsed: std::time::Duration,
    /// 根节点统计；空报告（`noResults`）为 `None`。
    pub root: Option<RootInfo>,
    /// 候选点，已按引擎排序（`order` 升序）。
    pub moves: Vec<MoveInfo>,
    /// 各点局势值（opt-in，查询时已开启）：下标与 `Coord::index` 一致，
    /// 正值 = 黑势。叠加层热度图直接取用。
    pub ownership: Option<Vec<f32>>,
    /// 策略网络先验（opt-in，策略热度图层开启时才请求）：长度 = size²+1，
    /// 前 size² 项下标与 `Coord::index` 一致，末位推定为弃着（渲染忽略）。
    /// 策略头一次前向即完整，第一条流式中间报告里就有，是「搜索早期
    /// 即可显示」的独特数据。
    pub policy: Option<Vec<f32>>,
}

/// 局面签名：对手数记录前缀逐字节做 FNV-1a（行棋方 / 着法 / 提子）。
/// 仅用于本模块的覆盖判定与历史取数，不要求抗碰撞。
fn position_sig(records: &[MoveRecord]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    for r in records {
        hash_record(&mut h, r);
    }
    h
}

/// 把一手棋混入局面签名（[`position_sig`] 与 [`AnalysisState::line_points`]
/// 的滚动计算共用，保证两条路径对同一前缀算出同一签名）。
fn hash_record(h: &mut u64, record: &MoveRecord) {
    fn byte(h: &mut u64, b: u8) {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    byte(h, u8::from(matches!(record.player, Stone::Black)));
    match record.action {
        Action::Place(c) => {
            byte(h, 1);
            byte(h, c.x());
            byte(h, c.y());
        }
        Action::Pass => byte(h, 0),
    }
    let n = record.captured.len();
    byte(h, n as u8);
    byte(h, (n >> 8) as u8);
    for c in &record.captured {
        byte(h, c.x());
        byte(h, c.y());
    }
}

/// 第 `turn` 手（1 起）的行棋方视角损失：由走子前后两个局面点的数据派生；
/// `turn == 0` 或任一端缺失时为 `None`（未知），不插值、不臆造 0。
///
/// 视角换算（`winrate` / `score_lead` 均为黑方视角，见 `HistoryPoint`）：
/// 黑方行棋时损失 = 走子前黑方值 − 走子后黑方值；白方行棋时符号相反
/// （黑方值升 = 白方亏）。结果一律为「行棋方失去了多少」，正 = 亏。
pub fn loss_from_points(
    turn: usize,
    player: Stone,
    before: HistoryPoint,
    after: HistoryPoint,
) -> Option<MoveLoss> {
    if turn == 0 {
        return None; // 空盘没有「走子前」，无从谈损失
    }
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
// ---- 选点限制（限定区域 / 排除选点）----

/// 一块限定区域：矩形对角交叉点（含两端，`min`/`max` 已归一化）。
/// `Coord` 不绑定尺寸，区域随局面使用时以所在棋盘为准。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// 左上角交叉点（x、y 较小者）。
    pub min: Coord,
    /// 右下角交叉点（x、y 较大者）。
    pub max: Coord,
}

impl Region {
    /// 由任意两个对角构造（自动归一化 min/max）；两角必须在同一尺寸的盘内。
    pub fn from_corners(size: Size, a: Coord, b: Coord) -> Self {
        Self {
            min: Coord::new(size, a.x().min(b.x()), a.y().min(b.y()))
                .expect("两个盘内交叉点的 min 分量仍在盘内"),
            max: Coord::new(size, a.x().max(b.x()), a.y().max(b.y()))
                .expect("两个盘内交叉点的 max 分量仍在盘内"),
        }
    }

    /// 是否包含该交叉点。
    pub fn contains(&self, c: Coord) -> bool {
        c.x() >= self.min.x()
            && c.x() <= self.max.x()
            && c.y() >= self.min.y()
            && c.y() <= self.max.y()
    }

    /// 区域内的全部交叉点（行优先）。需要棋盘尺寸参数（`Coord` 不绑定尺寸）。
    pub fn points(&self, size: Size) -> impl Iterator<Item = Coord> {
        let (min, max) = (self.min, self.max);
        (min.y()..=max.y())
            .flat_map(move |y| (min.x()..=max.x()).map(move |x| (x, y)))
            .filter_map(move |(x, y)| Coord::new(size, x, y))
    }

    /// 区域是否为单点（点击而非拖拽形成的「框」）。
    pub fn is_single(&self) -> bool {
        self.min == self.max
    }
}

/// 当前的选点限制。**allowMoves 与 avoidMoves 引擎实测互斥**（同时给出
/// 会被拒绝），因此区域与排除列表也互斥：区域开启时排除列表保留但不生效。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnalysisLimits {
    /// 限定区域（`Some` = 区域模式，只允许区域内的空点）。
    pub region: Option<Region>,
    /// 被排除的手（GTP 坐标，按记录时的行棋方分组）。
    pub avoid: Vec<(Stone, Coord)>,
}

impl AnalysisLimits {
    /// 区域模式是否生效。
    pub fn has_region(&self) -> bool {
        self.region.is_some()
    }
}

/// 限制版本号：任何修改都会递增，`sync` 据此重发查询（局面未变时也重发）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct LimitsEpoch(u32);

/// 由棋盘与限制构造查询级 move_rules（协议层 [`MoveRules`]）。
///
/// 规则只发给**当前行棋方**：区域 / 排除是「当前局面怎么选点」的约束，
/// 对手应手不受限（KaTrain 同款口径）。坐标用 `Coord::to_gtp` 组装。
fn move_rules_of(limits: &AnalysisLimits, board: &Board) -> Option<MoveRules> {
    let player = board.to_play();
    if let Some(region) = limits.region {
        // 只收区域内的空点；过滤后为空则不发限制（引擎对空 allowMoves
        // 返回空 moveInfos，见第 0 步实测，不把这种查询发出去）。
        let moves: Vec<String> = region
            .points(board.size())
            .filter(|c| board.get(*c).is_none())
            .map(|c| c.to_gtp(board.size()))
            .collect();
        if moves.is_empty() {
            return None;
        }
        return Some(MoveRules::allow(player, moves));
    }
    if limits.avoid.is_empty() {
        return None;
    }
    let moves: Vec<String> = limits
        .avoid
        .iter()
        .filter(|(p, _)| *p == player)
        .map(|(_, c)| c.to_gtp(board.size()))
        .collect();
    // 当前行棋方名下没有排除项（例如排除的全是对手的点）时等同无限制。
    if moves.is_empty() {
        return None;
    }
    Some(MoveRules::avoid(player, moves))
}

/// 查询口径：常规展示分析（配置值 visits + 流式中间报告），另有对局走子
/// 的独立口径。原「快 / 深两段式」已由流式中间报告取代（查询级
/// `reportDuringSearchEvery`，v1.18.2 实测），不再需要低 visits 预热。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// 常规展示分析：visits = 配置值，开启流式中间报告。
    Analysis,
    /// 人机对弈的走子查询：按**难度档位**的 visits 完整搜索，不开启流式
    /// （走子只认终态，中间报告无意义）。与展示口径分开的理由：
    /// - 走子报告不进展示快照（`Snapshot`），避免侧栏/叠加层随难度值变浅；
    /// - 轮到引擎时**先发 Play 再补展示查询**：若展示查询先按配置值搜过，
    ///   引擎的搜索树跨查询存活，随后按更低难度值的查询会立刻用满缓存
    ///   返回，难度形同虚设（实测 protocol.rs 注明「同局面再次查询即使更小
    ///   maxVisits 也立刻返回」）。
    Play,
}

impl Stage {
    /// 该口径的 visits 上限。配置值为 0 时夹到 1，避免无效查询。
    fn cap(self, cfg: &EngineConfig) -> u32 {
        let visits = cfg.visits.max(1);
        match self {
            Self::Analysis => visits,
            // 走子口径永远按**难度值**（任务口径：难度低于展示值时按难度
            // 搜完走子再补展示；高于展示值时同样以难度为准）。
            Self::Play => cfg.play_difficulty.visits().max(1),
        }
    }

    /// 展示口径开启流式中间报告；走子口径只认终态，不开。
    fn streaming(self) -> bool {
        match self {
            Self::Analysis => true,
            Self::Play => false,
        }
    }
}

/// 在飞查询。
struct Inflight {
    id: QueryId,
    turn: usize,
    stage: Stage,
    started: Instant,
    /// 查询发起时的局面签名（手数记录前缀）。走子口径的查询以此判断
    /// 「该查询还算不算当前局面的应手依据」——对弈中难度可被用户随时
    /// 改变，改难度后旧口径的终态报告不得再驱动应手。
    sig: Vec<MoveRecord>,
}

/// 批量在飞块：一次 `analyzeTurns` 查询的跟踪信息。
struct BatchChunk {
    id: QueryId,
    /// 尚待收到的报告数（每 turn 一条，含 noResults 空报告）。
    expect: usize,
    /// 派发时刻（卡死兜底：超过 [`BATCH_CHUNK_STALL`] 未收齐即放弃该块）。
    since: Instant,
}

/// 在飞块的最长等待：20 手 @0.94s ≈ 19s，留交互插队（实测 3–6s）与
/// 引擎波动的余量后取 90s；超时放弃该块继续推进，批量永不悬挂。
const BATCH_CHUNK_STALL: std::time::Duration = std::time::Duration::from_secs(90);

/// 整谱快扫任务状态：对当前线（根 → 叶子）逐手低 visits 分析，报告按
/// turnNumber 回填逐手历史（曲线自动填满）。协议事实（/tmp/batch-notes.md）：
/// - 多 turn 查询逐 turn 返回终态，但 **到达顺序不保证**（threads≥2 时实测
///   turn 2 先于 turn 0）⇒ 回填必须按报告自带 `turnNumber` 定位局面签名；
/// - 批量会占满引擎队列，交互查询须带高 `priority` 插队（实测有效）；
/// - terminate 多 turn 查询时，已完成 turn 正常补发、未完成 turn 补发
///   `noResults` 空报告 ⇒ 回填跳过空报告即可。
struct BatchJob {
    /// 尚未派发的 turn 区间（每块 [`BATCH_CHUNK`] 个，块间串行）。
    pending: std::ops::Range<usize>,
    /// 在飞块（收齐其全部 turn 报告后才派发下一块）。
    active: Option<BatchChunk>,
    /// 批量覆盖的 turn 总区间（0..len，len = 手数）。
    total: std::ops::Range<usize>,
    /// 批量覆盖的整条线（`line_records`，含游标之后的着法）：查询的
    /// `moves` 必须覆盖到最深的 analyzeTurn，否则引擎重放缺着即报
    /// `Invalid turn number`（实测：回看中发起、line 只取到游标时的错误）。
    line: Vec<MoveRecord>,
    /// 发起时的局面（`records()`，到游标）：批量期间游标 / 线一变即取消。
    watch: Vec<MoveRecord>,
    /// 发起时的棋盘尺寸与贴目（块间跨帧，随查询重发）。
    size: Size,
    komi: f64,
    started: Instant,
    /// 已回填的有效终态数（进度 = done / total）。
    done: usize,
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
    /// 逐手胜率历史：键 = 局面签名（根到该局面着法前缀的 FNV-1a，
    /// 见 [`position_sig`]）。谱树中不同分支的同一手数是不同局面，
    /// 各存各的：切换分支不互相覆盖，切回来数据仍在。
    /// 曲线 / 失误统计（`ui::curve` / `ui::overlay`）经 [`Self::line_points`]
    /// 只取当前线上的点，缺口留空。
    history: HashMap<u64, HistoryPoint>,
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
    /// 整谱快扫任务（`Some` = 进行中）。与常规在飞查询（`inflight`）完全
    /// 独立：批量报告只回填历史，不落展示快照（`on_report` 按 stage 隔离）。
    batch: Option<BatchJob>,
    /// 批量刚结束时的用户提示（完成 / 取消），侧栏读取后由 App 清除。
    batch_notice: Option<String>,
}

impl AnalysisState {
    pub fn new() -> Self {
        Self {
            engine: EngineStatus::Starting,
            snapshot: None,
            transient_error: None,
            last_log: None,
            history: HashMap::new(),
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
            batch: None,
            batch_notice: None,
        }
    }

    // ---- 选点限制（限定区域 / 排除选点）----

    /// 当前生效的选点限制（侧栏显示与状态标识读取）。
    pub fn limits(&self) -> &AnalysisLimits {
        &self.limits
    }

    /// 设置限定区域（`None` = 清除区域）。区域与排除互斥：设置区域后
    /// 排除列表保留但暂不生效（见 [`AnalysisLimits`] 文档）。
    pub fn set_region(&mut self, region: Option<Region>) {
        if self.limits.region == region {
            return;
        }
        self.limits.region = region;
        self.bump_limits();
    }

    /// 切换一手棋的排除状态（棋盘右键 / 候选行排除按钮共用）。
    /// 区域模式开启时排除不生效（保持互斥），直接忽略。
    pub fn toggle_avoid(&mut self, player: Stone, at: Coord) {
        if self.limits.has_region() {
            return;
        }
        if let Some(pos) = self
            .limits
            .avoid
            .iter()
            .position(|(p, c)| *p == player && *c == at)
        {
            self.limits.avoid.remove(pos);
        } else {
            self.limits.avoid.push((player, at));
        }
        self.bump_limits();
    }

    /// 移除一条排除项（下标 = 侧栏排除列表的行号）。
    pub fn remove_avoid(&mut self, index: usize) {
        if index < self.limits.avoid.len() {
            self.limits.avoid.remove(index);
            self.bump_limits();
        }
    }

    /// 清空全部限制（区域 + 排除；「一键清除」入口共用）。
    pub fn clear_limits(&mut self) {
        if self.limits.region.is_none() && self.limits.avoid.is_empty() {
            return;
        }
        self.limits = AnalysisLimits::default();
        self.bump_limits();
    }

    /// 开启区域模式（不设区域，等用户在棋盘上拖出）。
    /// 恒递增版本号：模式切换本身要触发重发（棋盘交互随之改变）。
    pub fn enable_region_mode(&mut self) {
        self.bump_limits();
    }

    /// 仅清空排除列表（区域与排除互斥，区域开启时不会走到这里）。
    pub fn clear_avoid(&mut self) {
        if self.limits.avoid.is_empty() {
            return;
        }
        self.limits.avoid.clear();
        self.bump_limits();
    }

    fn bump_limits(&mut self) {
        self.limits_epoch.0 += 1;
    }

    /// 设置策略热度图层的请求开关（`App` 随 Overlay 复选框写入）。
    /// 值变化即作废已完成的快照（旧快照缺 policy 或已无必要携带），
    /// `sync` 检测到 `sent_policy` 不一致后自动重发查询。
    pub fn set_want_policy(&mut self, want: bool) {
        if self.want_policy == want {
            return;
        }
        self.want_policy = want;
        // 开关切换后旧快照的 policy 有无与新开关矛盾，直接作废重查。
        self.snapshot = None;
    }

    /// 用当前配置（重）启动引擎；设置面板「应用并重启」与错误重试共用。
    /// 任何失败都进入可显示的状态，不 panic。
    pub fn start_engine(&mut self, cfg: &EngineConfig, waker: &Waker) {
        if let Some(old) = self.handle.take() {
            old.shutdown();
        }
        self.inflight = None;
        self.analyzed_sig = None;
        self.play_sig = None;
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

    /// 当前线（根到当前节点沿选中子分支）的逐手历史数据：下标 = 手数
    /// （0 = 初始空盘，长度 = `line_len() + 1`），`None` = 该手尚无终态数据。
    ///
    /// 每项按「根到该手着法前缀」的签名从历史缓冲取——同手数的不同分支
    /// 是不同局面，各取各的数据，互不干扰。O(线长) 滚动计算签名，
    /// 一帧至多调用一次（曲线、失误标注、侧栏汇总各调一次，可接受）。
    pub fn line_points(&self, board: &Board) -> Vec<Option<HistoryPoint>> {
        let records = board.line_records();
        let mut points = Vec::with_capacity(records.len() + 1);
        let mut sig = 0xcbf2_9ce4_8422_2325;
        points.push(self.history.get(&sig).copied());
        for record in records {
            hash_record(&mut sig, record);
            points.push(self.history.get(&sig).copied());
        }
        points
    }

    /// 失误汇总统计：遍历当前线现场派生（每手仅两次哈希读取与算术，
    /// 可每帧调用）。「已分析」只计两端数据齐全的手数，缺口不计入分级。
    pub fn loss_summary(&self, board: &Board) -> LossSummary {
        let points = self.line_points(board);
        let mut summary =
            LossSummary { total: board.line_len(), ..LossSummary::default() };
        for (i, record) in board.line_records().iter().enumerate() {
            let (Some(before), Some(after)) = (points[i], points[i + 1]) else { continue };
            let Some(loss) = loss_from_points(i + 1, record.player, before, after) else {
                continue;
            };
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
    ///
    /// `komi` 为当前对局的贴目（随查询发给引擎；复盘无贴目信息时用 7.5）。
    /// `want_play_query`：是否需要为本局面准备**走子口径**的依据
    /// （对弈模式开启、未结束、轮到引擎且在活子位置时为 `true`，
    /// 由 `app` 判定——本层不感知对弈状态，避免复盘时白烧走子预算）。
    ///
    /// 查询顺序：轮到引擎应手的局面**先发 Play（难度值）再补展示查询**——
    /// 引擎的搜索树跨查询存活，若展示查询先按配置值跑过，随后按更低难度值
    /// 的查询会立刻用满缓存返回，难度形同虚设（见任务实测）。展示查询开启
    /// 流式中间报告，边搜边刷新界面。
    pub fn sync(&mut self, board: &Board, cfg: &EngineConfig, komi: f64, want_play_query: bool) {
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
        // 批量驱动：派发下一块 / 卡死兜底。在飞块超过 [`BATCH_CHUNK_STALL`]
        // 未收齐（查询被拒后引擎不再补发等异常路径）即放弃剩余部分收尾，
        // 批量永不悬挂、UI 永远可操作。
        if let Some(job) = self.batch.as_ref()
            && let Some(chunk) = job.active.as_ref()
            && chunk.since.elapsed() >= BATCH_CHUNK_STALL
        {
            self.finish_batch(false);
        }
        if self.batch.is_some() {
            self.dispatch_batch_chunk();
            if let Some(job) = self.batch.as_ref()
                && job.done >= job.total.end - job.total.start
                && job.active.is_none()
            {
                self.finish_batch(true);
            }
        }
        // 限制版本变化（设置区域 / 排除 / 清除）即使局面未变也要重发查询：
        // 限制是查询级字段，引擎不感知「用户改了限制」这一事件。
        // policy 开关同理：查询级 opt-in 字段，开关切换必须重发才生效。
        let limits_changed = self.limits_epoch != self.sent_epoch;
        let policy_changed = self.want_policy != self.sent_policy;
        if Some(sig) != self.analyzed_sig.as_deref() || limits_changed || policy_changed {
            self.snapshot = None;
            self.transient_error = None;
            if let Some(inflight) = self.inflight.take()
                && let Some(handle) = self.handle.as_mut()
            {
                handle.terminate(inflight.id);
            }
            if matches!(self.engine, EngineStatus::Ready) {
                let stage = if want_play_query { Stage::Play } else { Stage::Analysis };
                self.request(board, cfg, stage, komi);
            }
        }
        while let Some(event) = self.handle.as_mut().and_then(Engine::try_recv) {
            self.on_event(event, board, cfg, komi);
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
            self.request(board, cfg, Stage::Play, komi);
        }
    }

    // ---- 整谱快扫（批量分析当前线）----

    /// 发起整谱快扫：对当前线（根 → 当前节点）0..=手数的每个局面按
    /// [`BATCH_VISITS`] 低 visits 分析，报告按 turnNumber 回填逐手历史。
    /// 已在批量中时幂等忽略；引擎未就绪时返回提示不发起。
    pub fn start_batch(&mut self, board: &Board, komi: f64) -> Option<String> {
        if self.batch.is_some() {
            return Some("整谱快扫已在进行中。".to_owned());
        }
        if !matches!(self.engine, EngineStatus::Ready) {
            return Some("引擎未就绪，无法开始整谱快扫。".to_owned());
        }
        // 记录整条线（moves 需覆盖最深 turn）与当前局面（变化即取消）：
        // 批量期间局面一变（切分支 / 切副本 / 载谱 / 落子 / 导航）即自动
        // 取消，防止把 A 盘面的报告回填进 B 盘面的历史。
        self.batch = Some(BatchJob {
            pending: 0..board.line_len(),
            active: None,
            total: 0..board.line_len(),
            line: board.line_records().to_vec(),
            watch: board.records().to_vec(),
            size: board.size(),
            komi,
            started: Instant::now(),
            done: 0,
        });
        self.dispatch_batch_chunk();
        None
    }

    /// 取消整谱快扫（用户点取消或局面变化自动取消）：terminate 在飞块，
    /// 未完成 turn 由引擎补发 noResults 空报告，回填时按空报告跳过。
    pub fn cancel_batch(&mut self) {
        let Some(job) = self.batch.take() else { return };
        if let Some(chunk) = job.active
            && let Some(handle) = self.handle.as_mut()
        {
            handle.terminate(chunk.id);
        }
        let elapsed = job.started.elapsed();
        self.batch_notice = Some(format!(
            "整谱快扫已取消：完成 {}/{} 手，用时 {}。",
            job.done,
            job.total.end - job.total.start,
            format_duration(elapsed),
        ));
    }

    /// 批量进度（侧栏显示）：完成数 / 总数、已用时长与在飞标记。
    pub fn batch_progress(&self) -> Option<(usize, usize, std::time::Duration)> {
        self.batch
            .as_ref()
            .map(|job| (job.done, job.total.end - job.total.start, job.started.elapsed()))
    }

    /// 取走批量结束提示（完成或取消），由 App 转为用户可见消息。
    pub fn take_batch_notice(&mut self) -> Option<String> {
        self.batch_notice.take()
    }

    /// 派发下一块（`analyzeTurns` 20 手一块；块间串行）。剩余块派发完但
    /// 报告未收齐时保持任务直至 done==total 或卡死兜底收尾。
    fn dispatch_batch_chunk(&mut self) {
        let Some(job) = self.batch.as_mut() else { return };
        if job.active.is_some() {
            return;
        }
        let start = job.pending.start.min(job.pending.end);
        let end = start + BATCH_CHUNK.min(job.pending.end - start);
        if start >= end {
            return; // 无剩余块：等最后一批报告收尾
        }
        let Some(handle) = self.handle.as_mut() else { return };
        // moves = 整条线（引擎按 analyzeTurns 逐局面分析）。
        let moves: Vec<(Stone, Action)> = job
            .line
            .iter()
            .map(|record| (record.player, record.action))
            .collect();
        let mut query = AnalysisQuery::new(job.size, moves);
        query.komi = job.komi;
        query.max_visits = Some(BATCH_VISITS);
        // 批量只填曲线：不开流式、不要 ownership/policy（省 60%+ 报告体积）。
        query.analyze_turns = Some((start..end).collect());
        query.priority = BATCH_PRIORITY;
        let id = handle.analyze(query);
        job.active = Some(BatchChunk {
            id,
            expect: end - start,
            since: Instant::now(),
        });
        job.pending.start = end;
    }

    /// 批量终态收尾：完成数报满则生成完成提示；未满（卡死兜底被调用）
    /// 则按实际完成数生成部分完成提示。两种情况都结束任务。
    fn finish_batch(&mut self, complete: bool) {
        let Some(job) = self.batch.take() else { return };
        let total = job.total.end - job.total.start;
        let elapsed = job.started.elapsed();
        self.batch_notice = Some(if complete {
            format!("整谱快扫完成：{total} 手，用时 {}。", format_duration(elapsed))
        } else {
            format!(
                "整谱快扫已结束：完成 {}/{} 手（部分块超时被跳过），用时 {}。",
                job.done,
                total,
                format_duration(elapsed),
            )
        });
    }

    /// 批量报告回填：按报告 `turnNumber` 定位局面签名（批量期间线不变，
    /// 签名 = 前 turn 手记录的滚动哈希），复用逐手历史的 visits 覆盖规则。
    /// **只回填历史，不落展示快照**——批量分析的是其它手数的局面，
    /// 侧栏数值必须继续反映当前局面的交互分析。
    /// 返回 true = 在飞块已收齐全部 turn 报告（含 noResults），可派发下一块。
    fn on_batch_report(&mut self, job: &mut BatchJob, id: QueryId, report: AnalysisReport) -> bool {
        // id 不符 = 已被取消的旧块补发（terminate 后引擎仍会补发终态），丢弃。
        if job.active.as_ref().is_none_or(|chunk| chunk.id != id) {
            return false;
        }
        if !report.no_results
            && let Some(root) = &report.root_info
        {
            // 按报告 turnNumber 重算该局面的签名（到达顺序不可信，笔记 §2）。
            let line = job.line.clone();
            let sig = position_sig(&line[..report.turn_number.min(line.len())]);
            self.record_history(report.turn_number, root, sig);
            job.done += 1;
        }
        // 空报告（noResults，terminate 后未完成 turn 的补发）无数据，只计数。
        if let Some(chunk) = job.active.as_mut() {
            chunk.expect = chunk.expect.saturating_sub(1);
        }
        job.active.as_ref().is_none_or(|chunk| chunk.expect == 0)
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
        // 载谱 / 新对局：批量静默丢弃（历史即将清空，任务已无意义），
        // 在飞块随局面替换 terminate，残余报告因历史清空自然失效。
        self.batch = None;
        self.batch_notice = None;
        self.analyzed_sig = None;
        self.snapshot = None;
        self.transient_error = None;
        self.history.clear();
        self.play_snapshot = None;
        self.play_sig = None;
    }

    /// 退出时优雅关闭引擎进程（`App::on_exit` 调用）。
    pub fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
    }

    // ---- 内部 ----

    fn on_event(&mut self, event: EngineEvent, board: &Board, cfg: &EngineConfig, komi: f64) {
        match event {
            EngineEvent::Ready => {
                self.engine = EngineStatus::Ready;
                self.transient_error = None;
                self.request(board, cfg, Stage::Analysis, komi);
            }
            EngineEvent::Report { id, report, is_final } => {
                // 批量在飞时报告先按批量 id 归属：批量查询与常规查询的 id
                // 空间互斥（引擎按 id 回传），命中批量即只回填历史不落快照。
                if self
                    .batch
                    .as_ref()
                    .is_some_and(|job| job.active.as_ref().is_some_and(|chunk| chunk.id == id))
                {
                    // 先把任务从 self 里取出来再回填，规避 self 双重可变借用
                    //（回填要访问 self.record_history / self.history）。
                    let Some(mut job) = self.batch.take() else { return };
                    let chunk_done = Self::on_batch_report(self, &mut job, id, report);
                    if chunk_done {
                        // 块收齐：关闭在飞并派发下一块（串行推进）。
                        job.active = None;
                        self.batch = Some(job);
                        self.dispatch_batch_chunk();
                        if let Some(job) = self.batch.as_ref()
                            && job.done >= job.total.end - job.total.start
                        {
                            self.finish_batch(true);
                        }
                    } else {
                        self.batch = Some(job);
                    }
                    return;
                }
                self.on_report(id, report, is_final, board, cfg);
            }
            EngineEvent::Log(line) => self.last_log = Some(line),
            EngineEvent::Failed(err) => self.on_failed(err),
            EngineEvent::Exited(status) => {
                self.inflight = None;
                // 引擎退出批量随之失效（错误状态已可见，不再另发提示）。
                self.batch = None;
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

    /// 接收报告：按 id 丢弃过期补发，落快照；流式中间报告实时刷新展示，
    /// 终态关闭在飞并转存逐手历史。
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
        let snapshot = Snapshot {
            turn,
            size: board.size(),
            visits_cap: stage.cap(cfg),
            // 语义（流式改造后）：是否按该查询的 visits 上限搜完的终态。
            // 展示快照可以比终态浅（中间报告），走子决策另见 play_snapshot。
            deep: is_final,
            is_final,
            elapsed: started.elapsed(),
            root: root.clone(),
            moves,
            ownership: report.ownership,
            policy: report.policy,
        };
        // 走子口径落位：只认终态（中间报告对决策无意义，守卫见
        // `play::engine_move_decision` 的 is_final 判断）。局面签名随查询
        // 记录，难度变更由 `sync` 的 want_play 比对触发重新查询。
        if stage == Stage::Play && is_final {
            self.play_snapshot = Some(snapshot.clone());
            self.play_sig = Some((inflight.sig.clone(), cfg.play_difficulty));
        }
        // 展示快照实时更新（含流式中间报告）：胜率 / 目差 / 候选点 / 热度图
        // 随搜索推进刷新。走子口径的查询不覆盖展示快照，侧栏不随难度值变浅。
        if stage == Stage::Analysis {
            self.snapshot = Some(snapshot);
        }
        // 仅终态转存进逐手历史：局面未变时 visits 不降者胜。
        // 曲线与失误分析不被流式中间值反复改写。
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
        }
    }

    /// 引擎当前是否应该应手、走子依据是否已就绪（`App::logic` 转接用）。
    ///
    /// 返回「该局面按当前难度完整搜索」的终态报告；`None` = 尚未就绪
    /// （查询在飞 / 未发起 / 难度刚变 / 局面已变）。**决策必须取自这里
    /// 而非展示快照**：展示口径可以是流式中间报告，拿来走子会让难度设置
    /// 形同虚设（且中间结果远未收敛）。走子口径查询不开流式，
    /// `Snapshot.is_final` 恒为真，终态守卫仍由决策函数保留兜底。
    pub fn play_snapshot(&self, board: &Board, difficulty: Difficulty) -> Option<&Snapshot> {
        let (sig, d) = self.play_sig.as_ref()?;
        if *d != difficulty || !sig.eq(board.records()) {
            return None;
        }
        self.play_snapshot.as_ref()
    }

    /// 走子口径的查询是否在飞（侧栏「引擎思考中」的判定依据之一：
    /// 展示快照已齐但走子查询未回时，引擎实际仍在为应手思考）。
    pub fn play_pending(&self, board: &Board, difficulty: Difficulty) -> bool {
        self.play_snapshot(board, difficulty).is_none()
            && self
                .inflight
                .as_ref()
                .is_some_and(|inflight| inflight.stage == Stage::Play && inflight.sig.eq(board.records()))
    }

    /// 终态结果转存进逐手历史：键 = 局面签名（该报告对应的着法前缀）。
    /// 同一局面（同签名）visits 不低于旧条目才覆盖（深阶段后到、覆盖快阶段）；
    /// 不同局面各占一个键，同手数的分支互不覆盖。无根节点数据或
    /// 容量已满时跳过。
    fn record_history(&mut self, turn: usize, root: &RootInfo, sig: u64) {
        if turn > HISTORY_CAP || self.history.len() >= HISTORY_CAP && !self.history.contains_key(&sig)
        {
            return;
        }
        let point = HistoryPoint {
            turn,
            winrate: root.winrate,
            score_lead: root.score_lead,
            visits: root.visits,
        };
        let overwrite = match self.history.get(&sig) {
            Some(old) => root.visits >= old.visits,
            None => true,
        };
        if overwrite {
            self.history.insert(sig, point);
        }
    }

    /// 对当前局面发起查询（就绪且无在飞时才生效）。`komi` 随查询发给引擎。
    fn request(&mut self, board: &Board, cfg: &EngineConfig, stage: Stage, komi: f64) {
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
        query.komi = komi;
        query.max_visits = Some(stage.cap(cfg));
        // 热度图需要 ownership（opt-in，引擎缺省不返回该字段）：协议实测
        // 中间报告同样携带（每条约 7–9 KB，0.5s 键下 300 visits 共约 11 条，
        // 见 /tmp/stream-notes.md），开销可忽略，热度图因此也能边搜边显示。
        query.include_ownership = true;
        // policy 只在策略热度图层开启时才请求（opt-in）：362 个浮点使每条
        // 流式报告增约 5 KB（300 visits 整次查询约 +50 KB，实测见
        // /tmp/policy-notes.md），关闭时必须零开销。开关切换由 `sync` 的
        // want/sent 比对触发重发（局面未变也重发）。
        query.include_policy = self.want_policy;
        // 展示口径开启流式中间报告（边搜边刷新界面）；走子口径只认终态。
        if stage.streaming() {
            query.report_during_search_every = Some(REPORT_EVERY_SECS);
        }
        // 选点限制（限定区域 / 排除选点）随查询发给引擎，展示与走子口径
        // 都受限（区域模式研究局部时，引擎应手也应在局部走才自然）。
        query.move_rules = move_rules_of(&self.limits, board);
        // 交互查询带高优先级：整谱快扫占满引擎队列时，实测不带 priority 的
        // 交互查询会被无限期阻塞（90 秒无响应），带 10 可数秒内插队返回
        // （/tmp/batch-notes.md §3/§7）。
        query.priority = INTERACTIVE_PRIORITY;
        let id = handle.analyze(query);
        self.analyzed_sig = Some(board.records().to_vec());
        self.sent_epoch = self.limits_epoch;
        self.sent_policy = self.want_policy;
        self.inflight = Some(Inflight {
            id,
            turn: board.cursor(),
            stage,
            started: Instant::now(),
            sig: board.records().to_vec(),
        });
    }
}

impl Default for AnalysisState {
    fn default() -> Self {
        Self::new()
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
