//! 人机对弈：对局状态、引擎自动应手决策与对局结束判定。
//!
//! 引擎走子**不引入第二套协议**：复用 analysis 引擎——对当前局面发起
//! 分析查询，取**深阶段（Deep）终态报告**的首选着法
//! （[`crate::ui::analysis::Snapshot::moves`] 的第 0 条；`mv` 为 `None`
//! 表示引擎选择弃着）。引擎的「思考量」即用户配置的 visits。
//!
//! 自动应手的触发条件（全部满足才落子，见 [`engine_move_decision`]）：
//! 对弈模式开启、引擎就绪、轮到引擎、棋盘处于活子位置（回看历史时
//! **绝不**自动落子）、对局未结束、且收到的是该局面的深阶段终态报告
//! （快阶段报告只用于渐进显示，绝不能拿浅搜索结果走子）。
//!
//! 模块刻意不含 UI 与引擎接线：决策函数为纯函数，输入即状态、输出即
//! 动作（或 `None`），便于逐条件验证；对局结束（认输 / 双方连续弃着）
//! 与悔棋语义也在此收敛，`app` / `board_view` 只做转接。

use crate::board::{Action, Board, Coord, Size, Stone};
use crate::ui::analysis::Snapshot;

/// 标准让子星位表（内部坐标，`x` 列、`y` 行自上向下，与 SGF 行序一致）。
///
/// 逐尺寸列出，取前 `n` 个作为 `n` 让子的摆子位置：
/// - 9 / 13 路：对角双星 + 天元 + 另两角；
/// - 19 路：四角星、四边星、天元（顺序按角 → 边 → 天元，
///   让 2 / 3 取对角，让 5 以上补边星与天元，符合通行惯例）。
fn handicap_stars(n: u8) -> &'static [(u8, u8)] {
    match n {
        9 => &[(2, 2), (6, 6), (4, 4), (6, 2), (2, 6)],
        13 => &[(3, 3), (9, 9), (6, 6), (9, 3), (3, 9)],
        19 => &[
            (3, 3),
            (15, 15),
            (3, 15),
            (15, 3),
            (3, 9),
            (15, 9),
            (9, 3),
            (9, 15),
            (9, 9),
        ],
        _ => &[],
    }
}

/// 让子摆子坐标：`handicap` 个星位（0 或 1 时为空表——让 1 不摆子，
/// 由调用方按普通对局处理）。非法让子数（1 或 >9）返回空表，调用方
/// 应在上游界面限制取值范围。
///
/// 星位表条目数少于让子数时按表截断（当前三档尺寸不会发生，防御性约束）。
pub fn handicap_stones(size: Size, handicap: usize) -> Vec<Coord> {
    if handicap < 2 {
        return Vec::new();
    }
    handicap_stars(size.n())
        .iter()
        .take(handicap)
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
}

impl Default for GameSetup {
    fn default() -> Self {
        Self {
            size: Size::new(19).expect("19 为固定合法尺寸"),
            komi: 7.5,
            handicap: 0,
            human: Stone::Black,
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
    pub resigned: Option<Stone>,
    /// 引擎「无望」提示是否已给过（每次对局至多提示一次，确认后不再打扰）。
    hopeless_shown: bool,
}

impl PlayState {
    /// 复盘初始态（对弈模式关闭）。
    pub fn review() -> Self {
        Self { mode: false, human: Stone::Black, resigned: None, hopeless_shown: true }
    }

    /// 开始新对局：模式开启、无人认输、无望提示复位。
    pub fn new_game(setup: &GameSetup) -> Self {
        Self {
            mode: true,
            human: setup.human,
            resigned: None,
            hopeless_shown: false,
        }
    }

    /// 对局是否已结束（认输，或棋盘上出现双方连续弃着）。
    pub fn finished(&self, board: &Board) -> bool {
        self.resigned.is_some() || two_passes(board)
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
/// - `snapshot`：最新分析快照（`None` = 尚无任何报告）；
/// - `engine_ready`：引擎状态是否为 [`crate::ui::analysis::EngineStatus::Ready`]。
///
/// 命中条件时返回 `Some(Action::Place(c))`（落子）或 `Some(Action::Pass)`
/// （引擎首选着法为弃着），由调用方直接喂给 `board.play` / `board.pass`；
/// 落子后的局面变化交给既有的「局面变化 → 重新分析」机制接手。
pub fn engine_move_decision(
    mode: bool,
    human: Stone,
    board: &Board,
    snapshot: Option<&Snapshot>,
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
    // 6. 快照必须是**当前局面**的深阶段终态报告：
    //    - turn 对不上 = 旧局面的报告（局面已变、快照未作废前的间隙）；
    //    - 非深阶段 = 快查询（低 visits）结果，拿它走子等于让引擎欠思考；
    //    - 非终态 = 渐进中间报告，后续还会有更完整的终态。
    let snapshot = snapshot?;
    if snapshot.turn != board.cursor() || !snapshot.deep || !snapshot.is_final {
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

/// 引擎方在深阶段终态报告中的胜率（黑方视角 → 引擎方视角换算）。
/// `snapshot.root` 缺失（空报告）时返回 `None`。
pub fn engine_winrate(engine: Stone, snapshot: &Snapshot) -> Option<f64> {
    let root = snapshot.root.as_ref()?;
    Some(match engine {
        Stone::Black => root.winrate,
        // 黑方视角胜率取反即白方视角。
        Stone::White => 1.0 - root.winrate,
    })
}

/// 引擎「无望」提示是否应当给出：深阶段终态报告里引擎方胜率 ≤ 5%，
/// 且本次对局尚未提示过。提示**不自动认输**——是否判引擎认输由人类确认。
pub fn should_show_hopeless(state: &PlayState, snapshot: Option<&Snapshot>) -> bool {
    if !state.mode || state.resigned.is_some() || state.hopeless_shown {
        return false;
    }
    snapshot.is_some_and(|s| {
        s.deep && s.is_final && engine_winrate(state.human.opposite(), s).is_some_and(|wr| wr <= 0.05)
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
