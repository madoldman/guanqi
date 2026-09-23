//! 人机对弈：对局状态、引擎自动应手决策与对局结束判定。
//!
//! 引擎走子**不引入第二套协议**：复用 analysis 引擎——对当前局面发起
//! 分析查询，取**按难度档位 visits 完整搜索**的终态报告首选着法
//! （[`AnalysisState::play_snapshot`]；`mv` 为 `None` 表示引擎选择弃着）。
//! 引擎的「思考量」即难度档位对应的 visits，与展示分析（配置值 visits 的
//! 流式查询）口径分离：展示可以比走子浅，走子绝不能比难度浅。
//!
//! 自动应手的触发条件（全部满足才落子，见 [`engine_move_decision`]）：
//! 对弈模式开启、引擎就绪、轮到引擎、棋盘处于活子位置（回看历史时
//! **绝不**自动落子）、对局未结束、且收到的是该局面的走子口径终态报告
//! （流式中间报告绝不触发落子）。
//!
//! 模块刻意不含 UI 与引擎接线：决策函数为纯函数，输入即状态、输出即
//! 动作（或 `None`），便于逐条件验证；对局结束（认输 / 双方连续弃着）
//! 与悔棋语义也在此收敛，`app` / `board_view` 只做转接。

use crate::board::{Action, Board, Coord, Size, Stone};
use crate::ui::analysis::Snapshot;

pub mod timer;
pub use timer::{Clock, SideClock, TimeBudget, TimeSystem, clock_text, engine_budget, side_index_of};

/// 让子摆子坐标：按 KataGo `PlayUtils::placeFixedHandicap`（对弈软件
/// 事实标准，master 分支 playutils.cpp）的惯例逐让子数给出：
///
/// - 让 2 对角双角，让 3 加相邻角，让 4 四角；
/// - 让 5 加天元；让 6 改为四角 + 左右边星（**天元让位**）；
/// - 让 7 再补天元；让 8 四边星全上（天元再次让位）；让 9 再补天元。
///
/// 天元在 5 → 6、7 → 8 时先有后无，不是单一前缀序列，故按 n 分段写出。
/// 角线：9 路在第 3 线（索引 2），13 / 19 路在第 4 线（索引 3）；
/// 边星与天元取中线（索引 n/2）。
///
/// 让子数不在 2..=9 时返回空表（0 / 1 无摆子，调用方按普通对局处理；
/// UI 已把可选取值限制在 0 与 2..=9，超界属防御分支）。
pub fn handicap_stones(size: Size, handicap: usize) -> Vec<Coord> {
    if !(2..=9).contains(&handicap) {
        return Vec::new();
    }
    let n = size.n();
    let (corner, mid) = match n {
        9 => (2u8, 4),
        13 => (3, 6),
        19 => (3, 9),
        _ => return Vec::new(),
    };
    let far = n - 1 - corner; // 对侧角线
    let (a, b) = ((corner, corner), (far, far)); // 对角双角
    let (c, d) = ((corner, far), (far, corner)); // 另两角
    let (l, r) = ((corner, mid), (far, mid)); // 左右两边星
    let (t, bo) = ((mid, corner), (mid, far)); // 上下两边星
    let center = (mid, mid); // 天元
    let points: &[(u8, u8)] = match handicap {
        2 => &[a, b],
        3 => &[a, b, c],
        4 => &[a, b, c, d],
        5 => &[a, b, c, d, center],
        6 => &[a, b, c, d, l, r],
        7 => &[a, b, c, d, l, r, center],
        8 => &[a, b, c, d, l, r, t, bo],
        _ => &[a, b, c, d, l, r, t, bo, center],
    };
    points
        .iter()
        .filter_map(|&(x, y)| Coord::new(size, x, y))
        .collect()
}

/// 新对局的设置（新对局窗口的编辑结果，由 `app` 执行）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameSetup {
    /// 棋盘尺寸。
    pub size: Size,
    /// 贴目（黑方贴给白方的目数，缺省 7.5）。
    pub komi: f64,
    /// 让子数（0 或 1 = 无让子，2..=9 = 摆星位且白先）。
    pub handicap: usize,
    /// 人类执子。
    pub human: Stone,
    /// 人机对弈难度（引擎走子的 visits 档位）。落位到
    /// [`crate::engine::EngineConfig::play_difficulty`] 持久化。
    pub difficulty: crate::engine::Difficulty,
    /// 对局时限制式（对局开始时确定，对局中只读）。落位到
    /// [`crate::engine::EngineConfig::time_system`] 持久化，下次
    /// 新对局默认带出。
    pub time_system: TimeSystem,
    /// 新对局的规则（对话框六选一）。落位到
    /// [`crate::engine::EngineConfig::new_game_rules`] 持久化，并随
    /// 另存写进 SGF `RU[]`（棋谱不再丢规则）。
    pub rules: crate::engine::Rules,
}

impl Default for GameSetup {
    fn default() -> Self {
        Self {
            size: Size::new(19).expect("19 为固定合法尺寸"),
            komi: 7.5,
            handicap: 0,
            human: Stone::Black,
            difficulty: crate::engine::Difficulty::default(),
            time_system: TimeSystem::default(),
            rules: crate::engine::Rules::Chinese,
        }
    }
}

impl GameSetup {
    /// 让子数归一：1 视作无让子（围棋惯例让 1 不摆子、不换先），
    /// 超过 9 或超过星位表长度的部分截断。
    pub fn effective_handicap(&self) -> usize {
        if self.handicap < 2 {
            0
        } else {
            self.handicap
        }
    }
}

/// 对局状态：对弈模式开关与终局信息（棋盘谱树本身不在此处复制）。
pub struct PlayState {
    /// 对弈模式是否开启。关闭时行为与纯复盘完全一致（不自动应手）。
    pub mode: bool,
    /// 对弈模式开启时人类执哪方。
    pub human: Stone,
    /// 认输方；`Some` = 对局已因认输结束（含人类认输与判定引擎认输）。
    /// **超时判负不走这里**：超时落 [`timeout_loss`]，与认输并列的
    /// 独立终局路径（SGF 结果 `B+T` / `W+T`，与认输的 `+R` 区分）。
    pub resigned: Option<Stone>,
    /// 超时判负方；`Some` = 对局已因超时结束（包干用尽 / 读秒次数用尽）。
    /// 结束语义与认输同一条路径（自动应手停止、复盘浏览），仅结果串
    /// 与 SGF 标记不同。
    pub timeout_loss: Option<Stone>,
    /// 对局时钟（含制式）。对局中只读推进；复盘态为无限制空钟。
    pub clock: Clock,
    /// 引擎「无望」提示是否已给过（每次对局至多提示一次，确认后不再打扰）。
    hopeless_shown: bool,
}

impl PlayState {
    /// 复盘初始态（对弈模式关闭）。
    pub fn review() -> Self {
        Self {
            mode: false,
            human: Stone::Black,
            resigned: None,
            timeout_loss: None,
            clock: Clock::new(TimeSystem::Unlimited),
            hopeless_shown: true,
        }
    }

    /// 开始新对局：模式开启、无人认输、时钟按制式初始化、无望提示复位。
    pub fn new_game(setup: &GameSetup) -> Self {
        Self {
            mode: true,
            human: setup.human,
            resigned: None,
            timeout_loss: None,
            clock: Clock::new(setup.time_system),
            hopeless_shown: false,
        }
    }

    /// 对局是否已结束（认输 / 超时判负，或棋盘上出现双方连续弃着）。
    pub fn finished(&self, board: &Board) -> bool {
        self.resigned.is_some() || self.timeout_loss.is_some() || two_passes(board)
    }
}

/// 双方连续弃着判定：当前线末两手均为弃着且行棋方相异（一手黑弃 +
/// 一手白弃 = 双方都表示终局）。手数不足两手为 `false`。
pub fn two_passes(board: &Board) -> bool {
    let records = board.records();
    let len = records.len();
    len >= 2
        && records[len - 2].is_pass()
        && records[len - 1].is_pass()
        && records[len - 2].player != records[len - 1].player
}

/// 引擎自动应手决策（纯函数，逐条件返回 `None` 或应手动作）。
///
/// 参数展开说明：
/// - `mode`：对弈模式开关（关闭 = 纯复盘，永不自动走子）；
/// - `human`：人类执子，轮到人类时引擎不走；
/// - `board`：当前棋盘（取行棋方 / 游标 / 当前线与着法记录）；
/// - `play_snapshot`：**走子口径**的最新分析快照（`None` = 尚无符合难度
///   的终态报告）。注意与展示快照（`AnalysisState::snapshot`）区分：
///   展示允许是流式中间报告（搜索未完成），走子必须来自按难度 visits 的
///   完整搜索；
/// - `engine_ready`：引擎状态是否为 [`crate::ui::analysis::EngineStatus::Ready`]。
///
/// 命中条件时返回 `Some(Action::Place(c))`（落子）或 `Some(Action::Pass)`
/// （引擎首选着法为弃着），由调用方直接喂给 `board.play` / `board.pass`；
/// 落子后的局面变化交给既有的「局面变化 → 重新分析」机制接手。
pub fn engine_move_decision(
    mode: bool,
    human: Stone,
    board: &Board,
    play_snapshot: Option<&Snapshot>,
    engine_ready: bool,
) -> Option<Action> {
    // 1. 对弈模式未开启：纯复盘，绝不自动走子。
    if !mode {
        return None;
    }
    // 2. 引擎未就绪（启动中 / 未配置 / 失败）：无报告可依，不走。
    if !engine_ready {
        return None;
    }
    // 3. 轮到人类：引擎不越权。
    if board.to_play() == human {
        return None;
    }
    // 4. 回看历史（游标不在当前线末端）：绝不能自动落子，
    //    否则会在用户浏览的旧局面上新建变着分支。
    if board.cursor() != board.line_len() {
        return None;
    }
    // 5. 对局已结束（双方连续弃着）：停止应手。
    //    认输状态（resigned）在调用方守卫——进入本函数前应已短路。
    if two_passes(board) {
        return None;
    }
    // 6. 快照必须是**当前局面**的走子口径终态报告（按难度 visits 完整
    //    搜索，由 `AnalysisState::play_snapshot` 按签名与难度把关）：
    //    - turn 对不上 = 旧局面的报告（局面已变、快照未作废前的间隙）；
    //    - 非终态 = 渐进中间报告，后续还会有更完整的终态，绝不据此落子。
    //    展示口径的流式中间报告只进展示快照，不会出现在这里；
    //    is_final 守卫对走子口径本已恒真，此处保留为决策函数的兜底口径。
    let snapshot = play_snapshot?;
    if snapshot.turn != board.cursor() || !snapshot.is_final {
        return None;
    }
    // 7. 取引擎首选着法；`mv = None` 即引擎认为当前最好的一手是弃着。
    //    空报告（noResults）没有候选点，无从决策。
    let best = snapshot.moves.first()?;
    Some(match best.mv {
        Some(at) => Action::Place(at),
        None => Action::Pass,
    })
}

/// 对弈模式下的悔棋：回到「人类该走」的**上一个决策点**。
///
/// - 人类刚落子、引擎尚未应手时（轮到引擎）：撤 1 手 = 撤掉人类上一手；
/// - 引擎刚应手完（轮到人类）：撤 2 手 = 撤掉引擎应手 + 人类上一手，
///   回到人类上一手之前的局面重新选择。
///
/// 实现为「先撤一手，再连续 `undo()` 直到轮到人类（合计至多 2 手）」；
/// 第一步就撤不了（根节点 / 回看中的非叶子节点）时返回 `false` 且棋盘
/// 状态不变，由调用方提示「无棋可悔」。
///
/// 复盘模式的悔棋语义（单步 `undo`）不经过本函数。
pub fn undo_to_human(board: &mut Board, human: Stone) -> bool {
    // 第一步：无论当前轮到谁，先撤一手；撤不了即无棋可悔。
    if board.undo().is_none() {
        return false;
    }
    // 撤一手后已轮到人类（撤掉的是人类上一手）即停；
    // 否则（撤掉的是引擎应手）再撤一手——合计至多 2 手。
    for _ in 0..1 {
        if board.to_play() == human {
            return true;
        }
        if board.undo().is_none() {
            return false;
        }
    }
    board.to_play() == human
}

/// 引擎方在终态报告中的胜率（黑方视角 → 引擎方视角换算）。
/// `snapshot.root` 缺失（空报告）时返回 `None`。
pub fn engine_winrate(engine: Stone, snapshot: &Snapshot) -> Option<f64> {
    let root = snapshot.root.as_ref()?;
    Some(match engine {
        Stone::Black => root.winrate,
        // 黑方视角胜率取反即白方视角。
        Stone::White => 1.0 - root.winrate,
    })
}

/// 引擎「无望」提示是否应当给出：走子口径终态报告里引擎方胜率 ≤ 5%，
/// 且本次对局尚未提示过。提示**不自动认输**——是否判引擎认输由人类确认。
pub fn should_show_hopeless(state: &PlayState, snapshot: Option<&Snapshot>) -> bool {
    if !state.mode || state.resigned.is_some() || state.hopeless_shown {
        return false;
    }
    snapshot.is_some_and(|s| {
        s.is_final && engine_winrate(state.human.opposite(), s).is_some_and(|wr| wr <= 0.05)
    })
}

/// 标记无望提示已给过（界面展示该提示时调用）。
pub fn mark_hopeless_shown(state: &mut PlayState) {
    state.hopeless_shown = true;
}

/// 认输结果文本（`side` 为认输方）。
pub fn resign_text(side: Stone) -> String {
    format!("{}方中盘胜", side.opposite().name())
}
