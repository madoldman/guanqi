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
    MoveInfo, MoveRules, QueryId, RootInfo,
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
///
/// 每手的**候选表快照**不放在这里：本类型是 `Copy`，在 [`Self::line_points`]
/// （曲线 / 失误标注 / 讲解每帧各调一次）里整段复制流转，塞进 `Vec`
/// 会让每个调用点付出搬运整张表的代价。候选表放独立的
/// [`AnalysisState::candidates`]（同签名键控、同生命周期、同一次覆盖
/// 判定写入，见该字段文档）。
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

/// 一个局面的**候选表快照**（局后统计的吻合度数据源）：与
/// [`HistoryPoint`] 同签名键控、由同一次终态报告同步写入。
///
/// 只存「实际落子是否被引擎想到 / 想到了多少」所需的最小集——存整张
/// (落点, visits) 列表而非只存单点的 visits，是为了与行棋方解耦：
/// 行棋方要重放棋谱才知道，而这里按局面签名直接取用。
#[derive(Clone, Debug)]
pub struct CandidateSnap {
    /// 候选落点及其 visits（按引擎 `order` 排序；弃着不在棋盘上、对
    /// 「实际落子是否被想到」无贡献，不存）。
    pub moves: Vec<(Coord, u64)>,
    /// 候选表 visits 总和（吻合度分母，LizzieYzy `percentsMatch` 口径）。
    pub total_visits: u64,
}

/// LizzieYzy 吻合度的样本门槛（已分析手数，黑白各自独立计数）：
/// 低于此值不给平均数，显示「样本不足」（照搬 LizzieYzy，避免用三四手
/// 的均值冒充整盘水平）。
pub const MATCH_MIN_MOVES: usize = 10;

/// 最差手排行榜长度：LizzieYzy 取 Top10，侧栏空间小取 Top5。
pub const WORST_LIMIT: usize = 5;

/// 局后统计（黑白吻合度 + 最差 N 手，[`AnalysisState::game_summary`]）。
/// 「未知」与「0」严格区分：`analyzed` 只计有候选表快照的手，
/// 未分析的手不进吻合度分母；在候选表里的手才有 visits 占比。
#[derive(Clone, Debug, Default)]
pub struct GameSummary {
    /// 当前线总手数。
    pub total: usize,
    /// 黑方已分析手数（有候选表快照；未分析的不计）。
    pub analyzed_black: usize,
    /// 白方已分析手数。
    pub analyzed_white: usize,
    /// 黑方吻合度 [0,1]（实际落子 visits / 候选表总 visits 的平均；
    /// `analyzed_black == 0` 时无意义）。
    pub match_black: f64,
    /// 白方吻合度 [0,1]。
    pub match_white: f64,
    /// 黑白各自已分析手数是否达到样本门槛（[`MATCH_MIN_MOVES`]）。
    pub enough_black: bool,
    pub enough_white: bool,
    /// 最差 N 手（**按胜率损失降序**，并列按手数升序；LizzieYzy 差异手
    /// `diffWinrate` 口径，见 [`WorstMove`]）。只收走子前后历史点齐全、
    /// 能算出损失的手——未知的不进、不臆造 0。不足 [`WORST_LIMIT`] 条时
    /// 有多少给多少。
    pub worst: Vec<WorstMove>,
}

/// 最差手排行榜的一项。**排序键 = 胜率损失降序**（LizzieYzy 差异手
/// `diffWinrate` 口径：对局与 AI 分歧最大、行棋方胜率损失最大的手），
/// 并列按手数升序。吻合度（`percentsMatch`）在 LizzieYzy 里是与差异手
/// 并列的另一个独立数字，这里只作行内附带显示，**不是排序键**——
/// 40 visits 快扫下吻合度大量并列 0，拿它排序会退化成「最早的几个 0% 手」。
#[derive(Clone, Copy, Debug)]
pub struct WorstMove {
    /// 手数（1 起，跳转直接 [`crate::board::Board::go_to`]）。
    pub turn: usize,
    /// 行棋方。
    pub player: Stone,
    /// 胜率损失（行棋方视角，正 = 亏损；黑方视角差值 × 行棋方符号）。
    /// 排序键。
    pub winrate_loss: f64,
    /// 目差损失（目，正 = 行棋方亏损），严重程度分级的依据。
    pub score_loss: f64,
    /// 严重程度分级（按目差损失定档，与「失误」卡片 / 棋盘标注同档）。
    pub severity: Severity,
    /// 该手吻合度 [0,1]（实际落子 visits / 候选表总 visits；落子不在
    /// 候选表 = 0，真实低吻合，与「未分析」不同）。附带显示，非排序键。
    pub match_ratio: f64,
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
    /// 每个候选点的「走这一手之后」领地图（opt-in
    /// `include_moves_ownership` 才有；随报告整表落位，缺省 `None` =
    /// 未开启或引擎过旧，上层回落根局面 `ownership`）。
    /// 与 `moves` 同下标对齐（按 `order` 排序后的候选列表）。
    pub moves_have_ownership: bool,
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
struct BatchTurn {
    turn: usize,
}

/// 一组**共用同一条着法路径**的待分析局面。一次 `analyzeTurns` 查询
/// 只能带一条路径，组内 turn 即该路径上的下标；同手数的不同分支节点
/// 是不同路径，绝不能并进同一查询（否则引擎按首组路径重放，turn 语义
/// 全错）。
#[derive(Debug)]
struct BatchGroup {
    /// 查询 `moves`（根 → 本组最深处）。
    path: Vec<MoveRecord>,
    /// 组内局面（`analyzeTurns` 升序且不允许重复 turn）。
    turns: Vec<BatchTurn>,
}

/// 快扫计划：发起前一次性算好（[`build_plan`]），预估与派发共用，
/// 保证「界面承诺扫什么」与「实际派发什么」是同一份数据。
#[derive(Debug)]
struct BatchPlan {
    /// 主扫描分组（线性模式恒 1 组；含变着模式按路径分组）。
    groups: Vec<BatchGroup>,
    /// 全树节点数（含根；「含变着」预估展示用）。
    tree_nodes: usize,
    /// 主扫描局面总数（各组 turns 之和，已按「只扫一方」过滤）。
    positions: usize,
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
/// （[`BATCH_VISITS`] 文档引用的实测），按 visits 线性放大。
const BATCH_SECS_PER_HAND_AT_40: f64 = 0.94;

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
struct BatchChunk {
    id: QueryId,
    /// 尚待收到的报告数（每 turn 一条，含 noResults 空报告）。
    expect: usize,
    /// **心跳**：最近一次收到该块报告的时刻。卡死判定只看它——
    /// 「距上次收到报告超过 [`BATCH_CHUNK_STALL`]」才算僵死。
    ///
    /// 理由（不能从派发时刻起算）：快扫期间用户连续浏览会发交互查询
    /// （`priority=10`），引擎逐个让路，20 手一块被拖过 90s 是**正常慢**，
    /// 从派发时刻计时会把慢误判成死、整批误收尾。改为心跳后：只要还在
    /// 断续出报告（哪怕 10 秒一条）就不放弃；真正僵死（查询被拒不补发、
    /// 引擎半死）才是连续 90s 零报告。块收齐时的等待上限自然放宽为
    /// 「90s × 20 手」量级，可接受——兜底的意义是永不悬挂，不是限时完成。
    last_report: Instant,
    /// 该块查询用的着法路径：报告按 `turnNumber` 在**这条路径**上定位
    /// 局面签名（主扫描组与加深组的路径可以不同，不能用任务级单一路径）。
    path: Vec<MoveRecord>,
    /// 是否加深阶段的块（进度分阶段计数）。
    deep: bool,
}

/// 在飞块的**心跳**判定线：距最近一次收到该块报告超过 90s 才算僵死。
/// （非「派发后 90s 未收齐」——快扫期间交互查询插队会把单块拖慢数倍，
/// 慢不是死；判定口径见 [`BatchChunk::last_report]。）数值取法：20 手
/// @0.94s ≈ 19s，留交互插队（实测 3–6s）与引擎波动余量后取 90s。
const BATCH_CHUNK_STALL: std::time::Duration = std::time::Duration::from_secs(90);

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
struct BatchJob {
    /// 尚未派发的主扫描分组下标（组间串行；组内再按
    /// [`BATCH_CHUNK`] 切块）。
    group: usize,
    /// 当前组内尚未派发的 turn 下标（切进 `groups[group].turns`）。
    group_pos: usize,
    /// 在飞块（收齐其全部 turn 报告后才派发下一块）。
    active: Option<BatchChunk>,
    /// 主扫描计划（发起时构造，与预估共用同一实现）。
    plan: BatchPlan,
    /// 发起时**当前线**的着法序列（`line_records()`）。
    ///
    /// 加深阶段必须用**这条**路径，不能用「计划里最后一条极大链」：含变着
    /// 模式下后者可能是一条分支，而加深选中的差异手是按当前线算出来的
    /// ⇒ 会发出 `analyzeTurns` 超出该分支长度 的非法查询（实测引擎回
    /// `Invalid turn number: N`，查询被拒、该块永远收不齐，快扫最后以
    /// 「部分完成」收场、加深全部白做）。批量期间局面一变即取消，故此线
    /// 在任务存续期内稳定。
    line_path: Vec<MoveRecord>,
    /// 发起时的局面（`records()`，到游标）：批量期间游标 / 线一变即取消。
    watch: Vec<MoveRecord>,
    /// 发起时的棋盘尺寸与贴目（块间跨帧，随查询重发）。
    size: Size,
    komi: f64,
    /// 发起时的快扫配置（加深参数从中取）。
    config: BatchConfig,
    /// 阶段：`false` = 主扫描，`true` = 加深（主扫描全部收齐后进入）。
    deepening: bool,
    /// 加深待扫局面（turn 0 基，去重升序；进入加深阶段时一次算好）。
    deepen_turns: Vec<usize>,
    /// 加深尚未派发的 turn 下标。
    deepen_pos: usize,
    /// 主扫描已完成局面数（进度分母 = plan.positions）。
    done: usize,
    /// 加深已完成局面数（进度分母 = deepen_turns.len()）。
    deep_done: usize,
    started: Instant,
}

/// 引擎「顶层未知字段」警告的一条用户可见记录（按字段名去重，见
/// [`AnalysisState::engine_warnings`]）。
#[derive(Clone, Debug)]
pub struct EngineWarning {
    /// 引擎不认识的顶层字段名（`None` = 报文未带，按原文整体去重）。
    pub field: Option<String>,
    /// 展示文本（已拼入引擎原文与处置说明）。
    pub text: String,
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
    /// 引擎「顶层未知字段」警告（会话内保留、按字段名去重）：同一字段
    /// 只提示一条，避免流式查询每 0.5s 一条报告前都警告一次刷屏。
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
    /// 整谱快扫任务（`Some` = 进行中）。与常规在飞查询（`inflight`）完全
    /// 独立：批量报告只回填历史，不落展示快照（`on_report` 按 stage 隔离）。
    batch: Option<BatchJob>,
    /// 整谱快扫配置（侧栏卡片编辑；`App` 经 [`Self::set_batch_config`]
    /// 写入，发起与预估时按当前线重新钳制）。不随换谱重置：`to` 的
    /// 「跟随线尾」哨兵让换到更长的棋时依旧全额。
    batch_config: BatchConfig,
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
            batch: None,
            batch_config: BatchConfig::default(),
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

    /// 设置候选点级 ownership 的请求开关（`App` 随 Overlay 复选框写入）。
    /// 复刻 [`Self::set_want_policy`] 的机制：值变化即作废快照，`sync`
    /// 检测 `want/sent` 不一致后自动重发查询（局面未变也重发——opt-in
    /// 字段是查询级的，引擎不感知「用户改了开关」这一事件）。
    pub fn set_want_moves_ownership(&mut self, want: bool) {
        if self.want_moves_ownership == want {
            return;
        }
        self.want_moves_ownership = want;
        // 旧快照候选点的 ownership 有无与新开关矛盾，作废重查。
        self.snapshot = None;
    }

    /// 引擎「顶层未知字段」警告（按字段名去重，会话内保留）。
    /// 「消息」卡片读取；无警告返回空切片。
    pub fn engine_warnings(&self) -> &[EngineWarning] {
        &self.engine_warnings
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
        let mut summary = LossSummary {
            total: board.line_len(),
            ..LossSummary::default()
        };
        for (i, record) in board.line_records().iter().enumerate() {
            let (Some(before), Some(after)) = (points[i], points[i + 1]) else {
                continue;
            };
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

    /// 局后统计：黑白吻合度与最差 N 手。吻合度为 LizzieYzy
    /// `percentsMatch` 口径（见 [`CandidateSnap`] 与模块内常量文档）；
    /// 排行榜为 LizzieYzy 差异手 `diffWinrate` 口径（按胜率损失降序），
    /// 损失与「失误」卡片同源（[`loss_from_points`]，走子前后历史点）。
    ///
    /// 匹配方向（/tmp/game-summary-notes.md §1 引擎实测钉死）：第 i 手
    /// （0 基）用 **turn = i** 的候选表匹配——`turnNumber=t` 报告的
    /// `moveInfos` 是「走了 t 手之后」局面（行棋方 = 第 t+1 手）的候选。
    ///
    /// 「未知」与「0」严格区分（项目铁律）：
    /// - 吻合度：`turn = i` 无候选表快照 ⇒ 该手**未分析**，不进分母；
    ///   有快照而实际落子不在表内 ⇒ 吻合度按 0 计（真实低吻合）。弃着
    ///   手的落点为 `None`，无法匹配落点：有快照时按 0 计（引擎候选表
    ///   本就不含弃着排序前列，实际弃着多半不合拍；诚实计 0，不臆造）。
    ///   有快照但算不出损失的手仍计入吻合度分母（两种口径的数据源独立）。
    /// - 排行榜：只收**走子前后历史点齐全、能算出损失**的手
    ///   （[`loss_from_points`] 返回 `None` 的不进榜、不臆造 0）。
    pub fn game_summary(&self, board: &Board) -> GameSummary {
        let records = board.line_records();
        let mut summary = GameSummary {
            total: records.len(),
            ..GameSummary::default()
        };
        // 吻合度按方累加；worst 只收「有快照且损失可算」的手再排序截断。
        let mut sum = (0.0f64, 0.0f64);
        let mut worst: Vec<WorstMove> = Vec::new();
        // 与 line_points 同款滚动签名：points[i] = 走 i 手前、points[i+1] =
        // 走 i 手后的历史点（None = 缺），损失取相邻两点差（同 loss_summary）。
        let points = self.line_points(board);
        let mut sig = 0xcbf2_9ce4_8422_2325;
        for (i, record) in records.iter().enumerate() {
            // 第 i 手的行棋方 = 该记录的 player；其候选表 = turn=i 局面
            // （走了 i 手之后）的签名，滚动推进（初始 = 空盘签名）。
            let snap = self.candidates.get(&sig);
            if let Some(snap) = snap {
                // 吻合度 = 实际落子在候选表里的 visits 占比；不在表内 = 0。
                let ratio = if snap.total_visits > 0 {
                    match record.action {
                        Action::Place(at) => snap
                            .moves
                            .iter()
                            .find(|(c, _)| *c == at)
                            .map_or(0.0, |(_, v)| *v as f64 / snap.total_visits as f64),
                        // 弃着无法匹配（见函数文档）：按 0 计。
                        Action::Pass => 0.0,
                    }
                } else {
                    0.0
                };
                match record.player {
                    Stone::Black => {
                        summary.analyzed_black += 1;
                        sum.0 += ratio;
                    }
                    Stone::White => {
                        summary.analyzed_white += 1;
                        sum.1 += ratio;
                    }
                }
                // 排行榜候选：损失可算才进（任一端历史点缺失 = 未知，不臆造 0）。
                if let (Some(before), Some(after)) = (points[i], points[i + 1])
                    && let Some(loss) = loss_from_points(i + 1, record.player, before, after)
                {
                    worst.push(WorstMove {
                        turn: loss.turn,
                        player: record.player,
                        winrate_loss: loss.winrate_loss,
                        score_loss: loss.score_loss,
                        severity: loss.severity,
                        match_ratio: ratio,
                    });
                }
            }
            hash_record(&mut sig, record);
        }
        summary.match_black = if summary.analyzed_black > 0 {
            sum.0 / summary.analyzed_black as f64
        } else {
            0.0
        };
        summary.match_white = if summary.analyzed_white > 0 {
            sum.1 / summary.analyzed_white as f64
        } else {
            0.0
        };
        summary.enough_black = summary.analyzed_black >= MATCH_MIN_MOVES;
        summary.enough_white = summary.analyzed_white >= MATCH_MIN_MOVES;
        // 最差 N 手：胜率损失降序（LizzieYzy diffWinrate 口径），并列按
        // 手数升序（稳定可复现）；截断至 WORST_LIMIT。
        worst.sort_by(|a, b| {
            b.winrate_loss
                .partial_cmp(&a.winrate_loss)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.turn.cmp(&b.turn))
        });
        worst.truncate(WORST_LIMIT);
        summary.worst = worst;
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
        // 限制版本变化（设置区域 / 排除 / 清除）即使局面未变也要重发查询：
        // 限制是查询级字段，引擎不感知「用户改了限制」这一事件。
        // policy 与候选点级 ownership 开关同理：查询级 opt-in 字段，
        // 开关切换必须重发才生效。
        let limits_changed = self.limits_epoch != self.sent_epoch;
        let policy_changed = self.want_policy != self.sent_policy;
        let moves_ownership_changed = self.want_moves_ownership != self.sent_moves_ownership;
        if Some(sig) != self.analyzed_sig.as_deref()
            || limits_changed
            || policy_changed
            || moves_ownership_changed
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
    /// 已在批量中时幂等忽略；引擎未就绪 / 无局面可扫时返回提示不发起。
    pub fn start_batch(&mut self, board: &Board, komi: f64) -> Option<String> {
        if self.batch.is_some() {
            return Some("整谱快扫已在进行中。".to_owned());
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
    fn dispatch_batch_chunk(&mut self) {
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
                        &path,
                        &turns,
                        job.config.visits,
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
            &path,
            &turns,
            job.config.deepen_visits,
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
    /// 候选表，省 60%+ 报告体积）。
    fn send_batch_query(
        handle: &mut Engine,
        size: Size,
        komi: f64,
        path: &[MoveRecord],
        turns: &[usize],
        visits: u32,
    ) -> QueryId {
        let moves: Vec<(Stone, Action)> = path
            .iter()
            .map(|record| (record.player, record.action))
            .collect();
        let mut query = AnalysisQuery::new(size, moves);
        query.komi = komi;
        query.max_visits = Some(visits);
        query.analyze_turns = Some(turns.to_vec());
        query.priority = BATCH_PRIORITY;
        handle.analyze(query)
    }

    /// 批量终态收尾：完成数报满则生成完成提示；未满（卡死兜底被调用）
    /// 则按实际完成数生成部分完成提示。两种情况都结束任务。
    /// 在飞块必须 terminate：卡死收尾时引擎多半还在算（或不算了，二者
    /// 都该停）；正常收尾时块报告恰好收齐、引擎本就空闲，多发一条
    /// terminate 无害（引擎对已完成的查询回 `noResults` 补发，被上层
    /// 按「id 不符」丢弃）。不 terminate 的话异常路径会白烧 GPU 算完
    /// 整块（≤20 手 ≈ 19s），结果却没人收。
    fn finish_batch(&mut self, complete: bool) {
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
    fn on_batch_report(&mut self, job: &mut BatchJob, id: QueryId, report: AnalysisReport) -> bool {
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
            let (moves, total) = Self::candidates_from(&report.move_infos);
            self.record_history(report.turn_number, root, sig, &moves, total);
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
    fn plan_deepen(&self, job: &BatchJob, board: &Board) -> Option<Vec<usize>> {
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
        self.batch_notice = None;
        self.analyzed_sig = None;
        self.snapshot = None;
        self.transient_error = None;
        self.history.clear();
        self.candidates.clear();
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
            EngineEvent::Report {
                id,
                report,
                is_final,
            } => {
                // 批量在飞时报告先按批量 id 归属：批量查询与常规查询的 id
                // 空间互斥（引擎按 id 回传），命中批量即只回填历史不落快照。
                if self
                    .batch
                    .as_ref()
                    .is_some_and(|job| job.active.as_ref().is_some_and(|chunk| chunk.id == id))
                {
                    // 先把任务从 self 里取出来再回填，规避 self 双重可变借用
                    //（回填要访问 self.record_history / self.history）。
                    let Some(mut job) = self.batch.take() else {
                        return;
                    };
                    let chunk_done = Self::on_batch_report(self, &mut job, id, report);
                    if chunk_done {
                        // 块收齐：关闭在飞。主扫描最后一个块收齐时转入加深
                        // 阶段——按「胜率损失降序」取前 N 手差异手（与侧栏
                        // 局后统计完全同一口径：排序复用 game_summary 的
                        // diffWinrate 排序、损失取自 [`loss_from_points`]），
                        // 用更高的 visits 重扫那些局面的走子前后两端；历史
                        // 回填「visits 更高才覆盖」自动完成替换，无需新机制。
                        // 无可加深（未开加深 / 无损失可算的手）时直接收尾。
                        job.active = None;
                        if !job.deepening
                            && let Some(turns) = self.plan_deepen(&job, board)
                        {
                            job.deepen_turns = turns;
                            job.deepening = true;
                        }
                        self.batch = Some(job);
                        self.dispatch_batch_chunk();
                        // 主扫描完成（未转加深）时立即收尾；转了加深的完成
                        // 判定由 sync 每帧复查（加深派发完毕且收齐）。
                        if let Some(job) = self.batch.as_ref()
                            && !job.deepening
                            && job.group >= job.plan.groups.len()
                            && job.done >= job.plan.positions
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
            EngineEvent::Warning { id, field, message } => {
                self.on_warning(id, field, message);
            }
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

    /// 引擎「顶层未知字段」警告：**非破坏性**——实测引擎发完警告后照常
    /// 分析该查询（全部报告正常到达），这里只落用户可见提示，绝不能走
    /// [`Self::on_failed`]（那会清 `inflight`，把仍在工作的查询打死）。
    /// 按字段名去重：流式查询每个中间报告都可能触发一次警告，
    /// 同一字段只保留首条，避免消息卡片刷屏；会话内保留不自动清除。
    fn on_warning(&mut self, id: Option<QueryId>, field: Option<String>, message: String) {
        let _ = id; // 警告不改变任何查询状态，id 仅用于展示定位
        if self.engine_warnings.iter().any(|w| w.field == field) {
            return;
        }
        let name = field.as_deref().unwrap_or("（未知字段）");
        self.engine_warnings.push(EngineWarning {
            field: field.clone(),
            text: format!(
                "引擎不认识查询里的字段「{name}」（可能拼错了）。\
                 本次分析仍会照常进行，但该字段不会生效。引擎原文：{message}"
            ),
        });
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
        // 「候选点是否带候选级领地图」要在 moves 被 move 进快照前判定
        //（流式中间报告与终态同构，实测都带；判定一次 O(候选数)）。
        let moves_have_ownership = moves.iter().any(|info| info.ownership.is_some());
        moves.sort_by_key(|info| info.order);
        // 候选表快照在 moves 被 move 进快照前提取（终态才用得到，
        // 但提前提取成本可忽略：表长 ≤ 候选数）。
        let (candidates, candidates_total) = Self::candidates_from(&moves);
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
            // 候选点级 ownership 随 moves 整表落位（MoveInfo.ownership），
            // 这里只记「有没有」供来源标注与回落判定。
            moves_have_ownership,
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
        if is_final && let Some(root) = &root {
            // 在飞报告的 turn 恒等于发起查询时的游标，局面未变即当前线的全部着法。
            let sig = position_sig(board.records());
            // 候选表随 root 同一次写入（口径一致，见 record_history 文档）；
            // moves 已按 order 排序，快照顺序即引擎序。
            self.record_history(turn, root, sig, &candidates, candidates_total);
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
            && self.inflight.as_ref().is_some_and(|inflight| {
                inflight.stage == Stage::Play && inflight.sig.eq(board.records())
            })
    }

    /// 终态结果转存进逐手历史：键 = 局面签名（该报告对应的着法前缀）。
    /// 同一局面（同签名）visits 不低于旧条目才覆盖（深阶段后到、覆盖快阶段）；
    /// 不同局面各占一个键，同手数的分支互不覆盖。无根节点数据或
    /// 容量已满时跳过。
    ///
    /// **候选表与 root 数据同一次报告写入**（同一覆盖判定、同进退）：
    /// 历史点的 visits 与候选表的 visits 必须是同一次搜索的产物，否则会
    /// 出现「root 是 300 visits 的、候选表是 40 visits 的」混合口径，
    /// 吻合度分母随之失真。`moves` 为该报告的候选点（丢弃弃着后取
    /// 落点与 visits），`total` 为候选表 visits 总和（分母）。
    fn record_history(
        &mut self,
        turn: usize,
        root: &RootInfo,
        sig: u64,
        moves: &[(Coord, u64)],
        total_visits: u64,
    ) {
        if turn > HISTORY_CAP
            || self.history.len() >= HISTORY_CAP && !self.history.contains_key(&sig)
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
            // 候选表为空（引擎对满盘等局面可返回空表）也照写：空表让
            // 「该局面分析过、但无候选信息」可区分于「没分析过」。
            self.candidates.insert(
                sig,
                CandidateSnap {
                    moves: moves.to_vec(),
                    total_visits,
                },
            );
        }
    }

    /// 由一份终态报告提取候选表快照所需的 (落点, visits) 列表与总 visits。
    /// 弃着（`mv == None`）不进表：它不占棋盘交叉点，永远不可能是
    /// 「实际落子」的匹配对象。
    fn candidates_from(infos: &[MoveInfo]) -> (Vec<(Coord, u64)>, u64) {
        let moves: Vec<(Coord, u64)> = infos
            .iter()
            .filter_map(|info| info.mv.map(|at| (at, info.visits)))
            .collect();
        let total = moves.iter().map(|(_, v)| *v).sum();
        (moves, total)
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
        // 候选点级 ownership 同理（opt-in 之最重）：每个候选点 × 361 float
        // 使单条报告 3.6 KB → 34.2 KB（约 +3.4 KB/候选，实测见模块文档），
        // 只在「候选点领地」图层开启时才请求；关闭时零开销。
        query.include_moves_ownership = self.want_moves_ownership;
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
        self.sent_moves_ownership = self.want_moves_ownership;
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

// ---- 局后统计写回 SGF 根注释（任务 2）----

/// 根注释统计块的**起始界标**。写回与替换都以这两个界标定位（同一文件
/// 重复另存必须幂等：已有块时替换块内内容，绝不追加第二份）。
pub const STATS_BLOCK_BEGIN: &str = "【观棋统计】";
/// 根注释统计块的结束界标（见 [`STATS_BLOCK_BEGIN`]）。
pub const STATS_BLOCK_END: &str = "【/观棋统计】";

/// 构造「局后统计」根注释块的内容（含首尾界标，作为整段插入或替换根
/// 注释中的对应区间）。数字**必须**与侧栏「局后统计」卡片一致：全部取自
/// [`AnalysisState::game_summary`] 这一次计算，不另算第二遍。
///
/// 内容（LizzieYzy `SGFParser.appendAiScoreBlunder` 的移植口径：黑白吻合度
/// + 差异手排行，写进根节点 C[]）：
/// - 黑 / 白吻合度：样本不足时如实写「样本不足」（口径与侧栏 `match_cell`
///   相同，`match_cell_text`）；已分析 0 手写「未分析」；
/// - 已分析手数（黑白分开）与总手数（统计可能只覆盖部分手数，如实写清）；
/// - 差异手前 N（`take`，5~10）：手数 + 行棋方 + 胜率损失（百分比），
///   排序即 `game_summary` 的 diffWinrate 口径；
/// - 口径说明（visits 占比 / 随分析深度变化），随档案落盘，防止其它
///   软件或未来的自己误读。
///
/// `total == 0`（无谱）返回 `None`：没有任何棋可统计时不产出块。
pub fn stats_block_text(summary: &GameSummary, take: usize) -> Option<String> {
    if summary.total == 0 {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push(STATS_BLOCK_BEGIN.to_owned());
    // 吻合度行：与侧栏 match_cell 同一呈现（未分析 / 样本不足 / 百分比）。
    let black = match_cell_text(
        summary.match_black,
        summary.analyzed_black,
        summary.enough_black,
    );
    let white = match_cell_text(
        summary.match_white,
        summary.analyzed_white,
        summary.enough_white,
    );
    lines.push(format!(
        "黑吻合度 {black}（已分析 {} 手）",
        summary.analyzed_black
    ));
    lines.push(format!(
        "白吻合度 {white}（已分析 {} 手）",
        summary.analyzed_white
    ));
    lines.push(format!(
        "共 {} 手；吻合度 = 实际落子在候选表中的 visits 占比（整局平均），\
         随分析深度变化（快扫 40 visits 数值系统性偏低），不同深度不可互比。",
        summary.total
    ));
    if summary.worst.is_empty() {
        lines.push("差异手：未分析（无损失数据）".to_owned());
    } else {
        lines.push(format!(
            "差异手前 {}（按胜率损失）：",
            summary.worst.len().min(take)
        ));
        for entry in summary.worst.iter().take(take) {
            lines.push(format!(
                "  第 {} 手 {} 胜率损失 +{:.1}%",
                entry.turn,
                entry.player.name(),
                entry.winrate_loss * 100.0,
            ));
        }
    }
    lines.push(STATS_BLOCK_END.to_owned());
    Some(lines.join("\n"))
}

/// 吻合度单元格文本（与侧栏 `match_cell` 同口径的块内版本；两个实现
/// 必须同步改，口径注释已在两侧互指）。
fn match_cell_text(ratio: f64, analyzed: usize, enough: bool) -> String {
    if analyzed == 0 {
        "未分析".to_owned()
    } else if !enough {
        "样本不足".to_owned()
    } else {
        format!("{:.1}%", ratio * 100.0)
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
