//! 分析的**值类型与判据**：逐手历史点、候选表快照、局后统计、引擎状态
//! 与快照、局面签名、每手损失派生，以及局后统计写回 SGF 根注释的块文本。
//!
//! 本文件只放「数据长什么样、损失怎么算」的纯定义与纯函数，不持有引擎
//! 状态；状态机见 [`super::AnalysisState`]。

use crate::board::{Action, Coord, MoveRecord, Size, Stone};
use crate::engine::{MoveInfo, RootInfo};

/// 逐手历史容量上限（条目数，各分支分开计数）。19 路盘的实用对局
/// 远小于此，超出部分不再写入，避免无界增长。
pub(super) const HISTORY_CAP: usize = 999;

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
    /// 该局面的行棋方（第 `turn` 手之后的局面轮到谁）：目数视角「黑白
    /// 交替」显示时按点换算的依据（[`display_values`]）。空盘（turn 0）
    /// 恒黑（与开局黑先一致；该局面本身从不按行棋方换算显示的场景
    /// 不受影响——换算只发生在轮白的点上）。
    pub to_play: Stone,
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

/// 吻合度「深度分析」的判定线：某手所用候选表对应的局面终态点
/// `visits >= DEEP_VISITS_THRESHOLD` 时该手按「深度分析」计，否则按
/// 「快扫」计。取 150 的理由：落在快扫默认 40 与加深/走子常用 300 之间，
/// 恰好把两档分开；40–100 的点（快扫 / 低难度走子）候选表短、吻合度
/// 系统性偏低，混进均值里说不清构成。该判定**只用于展示构成说明**，
/// 不改动吻合度数值本身的任何计算。
pub const DEEP_VISITS_THRESHOLD: u64 = 150;

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
    /// 黑方已分析手中来自「深度分析」的手数（判定线见
    /// [`DEEP_VISITS_THRESHOLD`]；其余即来自快扫）。展示吻合度构成用，
    /// 黑 + 深度 = analyzed_black，白同理。
    pub deep_black: usize,
    /// 白方已分析手中来自「深度分析」的手数。
    pub deep_white: usize,
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
    /// 是否为终态报告（`false` = 引擎的渐进中间报告，后续还会更新）。
    /// 人机对弈的自动应手只认走子口径的终态快照——中间报告只用于渐进
    /// 显示，不能拿去走子。早先另有 `deep` 字段（「快 / 深两段式」时代的
    /// 是否搜满标志），流式改造后与 `is_final` 恒等、成为只写不读的
    /// 重复字段，已删除。
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
    // 「候选点是否带候选级领地图」的 `moves_have_ownership` 标志已删除：
    // 它与 `moves.iter().any(|m| m.ownership.is_some())` 恒等、只写不读
    //（判定函数 `overlay::heat_source` 直接查 moves 本身），存一份快照
    // 只会成为过期的第二事实来源。
}

impl Snapshot {
    /// 策略网络的**未搜索直觉**（原始网络输出，v1.18.2 实测 rootInfo
    /// 无条件携带 `rawWinrate` / `rawLead`，已随报告归一化为黑视角）。
    /// 返回 `(黑方胜率, 黑方目差)`；任一字段缺失（引擎过旧等）返回
    /// `None`，调用方**隐藏整行**而非显示 0。
    pub fn raw_eval(&self) -> Option<(f64, f64)> {
        let root = self.root.as_ref()?;
        Some((root.raw_winrate?, root.raw_lead?))
    }
}

/// 局面签名：对手数记录前缀逐字节做 FNV-1a（行棋方 / 着法 / 提子）。
/// 仅用于本模块的覆盖判定与历史取数，不要求抗碰撞。
pub(super) fn position_sig(records: &[MoveRecord]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    for r in records {
        hash_record(&mut h, r);
    }
    h
}

/// 把一手棋混入局面签名（[`position_sig`] 与 [`AnalysisState::line_points`]
/// 的滚动计算共用，保证两条路径对同一前缀算出同一签名）。
pub(super) fn hash_record(h: &mut u64, record: &MoveRecord) {
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

// ---- 局后统计写回 SGF 根注释（任务 2）----

/// 根注释统计块的**起始界标**。写回与替换都以这两个界标定位（同一文件
/// 重复另存必须幂等：已有块时替换块内内容，绝不追加第二份）。
pub const STATS_BLOCK_BEGIN: &str = "【观棋统计】";
/// 根注释统计块的结束界标（见 [`STATS_BLOCK_BEGIN`]）。
pub const STATS_BLOCK_END: &str = "【/观棋统计】";

/// 根注释「终局估计」块的起始界标（与统计块同一界标幂等机制，见
/// `save::merge_delimited_block`）：双方弃着终局时写引擎对最后局面的
/// 目差估计——**估计值不是点目结果**，绝不冒充胜负（SGF `RE` 照实写
/// `?`）。独立成块而非并入统计块：终局可能没做过任何分析（统计块在
/// 无数据时不生成），估计行不能因此丢失。
pub const RESULT_BLOCK_BEGIN: &str = "【观棋终局】";
/// 根注释「终局估计」块的结束界标（见 [`RESULT_BLOCK_BEGIN`]）。
pub const RESULT_BLOCK_END: &str = "【/观棋终局】";

/// 由最深终局评估构造「终局判定」块正文（含首尾界标）。弃着终局的
/// `RE` 与本块**同源**（同一份最深终态 scoreLead）：`score_lead` 为
/// **黑方视角**原始值（存储口径），规则名随行标注（评估按棋谱 RU 的
/// 规则算，口径随档案落盘）。`None` 规则名 = 未记录（罕见，防御分支）。
///
/// 文案自证口径：明确标注「引擎判定，非点目结果」——本项目不做点目，
/// 评估按当前规则网络给出，误差概率低但不是数子；其它软件与未来的
/// 自己不得把它当成数子结果读。
pub fn result_block_text(score_lead: f64, rules_name: Option<&str>) -> String {
    let lead_text = if score_lead.abs() < 0.5 {
        "形势接近（0）".to_owned()
    } else if score_lead > 0.0 {
        format!("黑胜 约{score_lead:.1} 目")
    } else {
        format!("白胜 约{:.1} 目", -score_lead)
    };
    let rules = rules_name.unwrap_or("（未记录）");
    format!(
        "{RESULT_BLOCK_BEGIN}\n\
         终局判定（引擎）：{lead_text} · 按规则「{rules}」评估，非点目结果\n\
         {RESULT_BLOCK_END}"
    )
}

/// 弃着终局的 SGF 结果串：由最深终态评估的黑方视角 scoreLead 给出。
/// `|lead| ≤ 0.05` 判和（`RE[0]`）；无评估返回 `None`（RE 写 `?`）。
/// 一位小数直接取引擎值（不凑半目——评估不是数子，伪造精度没有意义）。
pub fn terminal_result_string(lead: f64) -> String {
    if lead.abs() <= 0.05 {
        "0".to_owned()
    } else if lead > 0.0 {
        format!("B+{lead:.1}")
    } else {
        format!("W+{:.1}", -lead)
    }
}

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
