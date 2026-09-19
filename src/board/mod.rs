//! 棋盘模块：坐标、规则与带历史的棋盘状态（9 / 13 / 19 路）。
//!
//! # 数据表示
//!
//! - 盘面 [`Grid`]：按行优先的一维数组（索引见 [`Coord::index`]），
//!   空点用 `None` 表示。选择 `Option<Stone>` 而非位棋盘的理由：
//!   本模块只为 GUI 服务，规模 ≤ 19×19，规则引擎只做泛洪搜索，
//!   克隆快照廉价、语义直白；`Option<Stone>` 经编译器优化后仅占 1 字节。
//! - 历史：快照式。`positions[i]` 为第 `i` 手之后的完整盘面
//!   （`positions[0]` 为初始空盘），与 `moves` 一一对应。
//!   由此"按手数取局面"为 O(1) 克隆，简单劫判定为一次向量比较。
//!
//! # 劫争（简单劫）
//!
//! 仅实现**简单劫**：落子后盘面不得回到**上一手之前**的盘面，
//! 即禁止立即回提形成单步循环。弃着不改变盘面，因此对方弃着后
//! "上一手之前的盘面"与当前盘面相同，任何落子都不会与之相等，
//! 简单劫禁着自然解除——这是该定义的固有结果。
//! **超级劫（全局同形禁止）本期不实现**，三劫循环等长循环一律放行。
//!
//! # 阶段 5 分支扩展路径（SGF 变着）
//!
//! 当前为线性历史：`moves` / `positions` 平行数组 + `cursor` 手数游标，
//! 已刻意把所有状态查询收敛到"按游标取快照"，未缓存任何差量。
//! 扩展为分支树时的改法（公开 API 语义尽量不变）：
//!
//! 1. 引入 `nodes: Vec<Node>`，`Node { record: MoveRecord, grid: Grid,
//!    parent: Option<usize>, children: Vec<usize> }`；
//! 2. `cursor` 换成当前节点 id `current`，`go_to(n)` 改为沿
//!    "主变着链"（每层默认第一个 child）寻址；
//! 3. `play` / `pass` 在非叶节点落子时改为新建兄弟 child（即变着），
//!    不再截断未来；
//! 4. `MoveRecord` 与 `Grid` 结构无需变更，迁移面集中在 `Board` 内部。
//!
//! # 预设局面（让子局 / 摆子局，[`Board::from_setup`]）
//!
//! SGF 棋谱允许在非空初始局面上开始（`AB` / `AW` / `AE` 摆子，
//! `PL` 指定首着方）。预设局面的语义约定：
//!
//! - 摆子**不计入手数**（`move_count` 从 0 起）、**不计提子统计**、
//!   **不参与劫争判定**（劫争仍只比较相邻两手真实着法间的盘面）；
//! - 历史 `positions[0]` 就是预设局面，`records()` 只含预设之后的真实着法；
//! - 摆子中的**无气块允许存在**（记谱软件常导出死子未提的谱，
//!   显示与分析均无碍），不报错、不 panic；仅坐标越界或黑白冲突时报
//!   [`SetupError`]；
//! - [`Board::new`] 等价于「空盘 + 黑先」的预设局面，二者共用同一构造路径。
//!
//! # 未实现的规则边界
//!
//! 超级劫与循环禁着、双活、终局计子；禁着点仅覆盖自杀。
//! GTP / SGF 的弃着记法（`pass` / `tt`）不在本模块解析，
//! 由引擎与 SGF 层转换成 [`Action::Pass`]。

// coord / rules 两个冻结文件内仍留有为阶段 3 / 5 预备的公开 API
// （GTP / SGF 坐标转换、提子信息查询等），在接入前无调用方，allow 收窄到此。
#[allow(dead_code)]
pub mod coord;
#[allow(dead_code)]
pub mod rules;

pub use coord::{Coord, Size};
// bin crate 中 CoordError 在引擎 / SGF 接入前无内部使用者，属预期。
#[allow(unused_imports)]
pub use coord::CoordError;
pub use rules::{IllegalReason, Stone};
// 规则函数由 Board 内部以 `rules::` 路径调用，re-export 供阶段 3 引擎层使用。
#[allow(unused_imports)]
pub use rules::{apply_move, chain_info, ChainInfo};

/// 盘面存储类型：按行优先的一维数组，空点为 `None`。
pub type Grid = Vec<Option<Stone>>;

/// 着法内容。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// 在该点落子。
    Place(Coord),
    /// 弃着（手数照常推进，盘面不变）。
    Pass,
}

/// 一手棋的记录。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MoveRecord {
    /// 行棋方。
    pub player: Stone,
    /// 落子或弃着。
    pub action: Action,
    /// 本手提走的对方子坐标（弃着为空；多块同提时全部列出）。
    pub captured: Vec<Coord>,
}

impl MoveRecord {
    /// 是否为弃着。
    pub fn is_pass(&self) -> bool {
        matches!(self.action, Action::Pass)
    }
}

/// 预设局面构造失败的原因（坐标问题；无气块不在此列，见模块文档）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetupError {
    /// 摆子 / 清除坐标不在棋盘上。
    OffBoard(Coord),
    /// 同一坐标被同时指派为黑子与白子。
    Conflicting(Coord),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OffBoard(c) => write!(f, "摆子坐标 {c} 超出棋盘范围"),
            Self::Conflicting(c) => write!(f, "坐标 {c} 被同时摆为黑子与白子"),
        }
    }
}

impl std::error::Error for SetupError {}

/// 棋盘状态：盘面 + 当前行棋方 + 提子计数 + 线性历史与游标。
///
/// 游标（`cursor`）支持在不销毁后续手数的情况下回看（复盘）；
/// [`Board::undo`] 才是真正悔棋（截断最新一手）。
pub struct Board {
    size: Size,
    /// 游标处的盘面（`positions[cursor]` 的缓存）。
    grid: Grid,
    /// 游标处的行棋方。
    to_play: Stone,
    /// 每手记录，长度即已下的手数（含弃着）。
    moves: Vec<MoveRecord>,
    /// `positions[i]`：第 `i` 手之后的盘面；`positions[0]` 为初始盘面。
    positions: Vec<Grid>,
    /// 当前所处手数，`0..=moves.len()`。
    cursor: usize,
    /// 黑方累计提走的对方子数（截至游标）。
    captured_by_black: u32,
    /// 白方累计提走的对方子数（截至游标）。
    captured_by_white: u32,
}

impl Board {
    /// 创建空盘，黑先。等价于「空盘 + 黑先」的预设局面，与
    /// [`Board::from_setup`] 共用同一构造路径。
    pub fn new(size: Size) -> Self {
        Self::from_setup(size, &[], &[], &[], Stone::Black)
            .expect("空盘预设没有任何坐标，构造必然成功")
    }

    /// 由预设局面构造棋盘（让子局 / 摆子局，语义见模块文档）：
    ///
    /// - 摆子不计手数、不计提子统计、不参与劫争判定；
    /// - `positions[0]` 即预设局面，`records()` 只含其后的真实着法；
    /// - 摆子中的无气块允许存在（不报错、不提取）；
    /// - 先摆黑 (`black`)、再摆白 (`white`)，最后清除 (`removed`)，
    ///   即清除与摆子同点时清除生效；
    /// - 仅坐标越界或同一坐标黑白冲突时返回 [`SetupError`]。
    pub fn from_setup(
        size: Size,
        black: &[Coord],
        white: &[Coord],
        removed: &[Coord],
        to_play: Stone,
    ) -> Result<Self, SetupError> {
        for &at in black.iter().chain(white).chain(removed) {
            if !at.on_board(size) {
                return Err(SetupError::OffBoard(at));
            }
        }
        for &at in black {
            if white.contains(&at) {
                return Err(SetupError::Conflicting(at));
            }
        }
        let mut grid: Grid = vec![None; size.point_count()];
        for &at in black {
            grid[at.index(size)] = Some(Stone::Black);
        }
        for &at in white {
            grid[at.index(size)] = Some(Stone::White);
        }
        for &at in removed {
            grid[at.index(size)] = None;
        }
        Ok(Self {
            size,
            grid,
            to_play,
            moves: Vec::new(),
            positions: Vec::new(),
            cursor: 0,
            captured_by_black: 0,
            captured_by_white: 0,
        }
        .with_initial_position())
    }

    /// 内部辅助：写入初始盘面快照，保证 `positions` 不变量。
    fn with_initial_position(mut self) -> Self {
        self.positions.push(self.grid.clone());
        self
    }

    // ---- 只读查询 ----

    /// 棋盘尺寸。
    pub fn size(&self) -> Size {
        self.size
    }

    /// 当前行棋方（游标处）。
    pub fn to_play(&self) -> Stone {
        self.to_play
    }

    /// 游标处的盘面切片。
    pub fn grid(&self) -> &[Option<Stone>] {
        &self.grid
    }

    /// 取某点棋子；该点不在盘上时返回 `None`。
    pub fn get(&self, at: Coord) -> Option<Stone> {
        if at.on_board(self.size) {
            self.grid[at.index(self.size)]
        } else {
            None
        }
    }

    /// 已下总手数（含弃着）。
    pub fn move_count(&self) -> usize {
        self.moves.len()
    }

    /// 当前所处手数（0 = 初始盘面）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 全部手数记录（复盘列表用）。
    // 阶段 5 棋谱列表使用；接入前无调用方。
    #[allow(dead_code)]
    pub fn records(&self) -> &[MoveRecord] {
        &self.moves
    }

    /// 第 `i` 手（0 起）的记录。
    pub fn record_at(&self, i: usize) -> Option<&MoveRecord> {
        self.moves.get(i)
    }

    /// 第 `n` 手之后的完整盘面快照；`n == 0` 为初始盘面。
    // 阶段 5 谱树跳转时复核局面使用；接入前无调用方。
    #[allow(dead_code)]
    pub fn position_at(&self, n: usize) -> Option<&Grid> {
        self.positions.get(n)
    }

    /// `color` 方累计提走的对方子数（截至游标，导航时随之回退）。
    pub fn captured_by(&self, color: Stone) -> u32 {
        match color {
            Stone::Black => self.captured_by_black,
            Stone::White => self.captured_by_white,
        }
    }

    /// `color` 方累计被提走的子数（即对方提走的，截至游标）。
    // 阶段 4 侧栏（双方损失统计）使用；接入前无调用方。
    #[allow(dead_code)]
    pub fn lost_by(&self, color: Stone) -> u32 {
        self.captured_by(color.opposite())
    }

    // ---- 合法性与落子 ----

    /// 查询在当前盘面（游标处）落子是否合法，非法时给出可显示的原因。
    /// 不修改任何状态，回看历史局面时同样可用。
    // 阶段 4 候选点合法性校验使用；接入前无调用方。
    #[allow(dead_code)]
    pub fn is_legal(&self, at: Coord) -> Result<(), IllegalReason> {
        self.trial(at).map(|_| ())
    }

    /// 在当前盘面落子（由 [`Board::to_play`] 行棋）。
    /// 仅在游标处于最新一手时可用；回看中落子会被拒绝（分支留待阶段 5）。
    pub fn play(&mut self, at: Coord) -> Result<(), IllegalReason> {
        if self.cursor != self.moves.len() {
            // 阶段 5 扩展点：此处应创建变着分支（见模块文档）。
            return Err(IllegalReason::NotAtLatestMove);
        }
        let (grid, captured) = self.trial(at)?;
        let player = self.to_play;
        // 先更新盘面再提交：commit 会把当前盘面快照推入历史。
        self.grid = grid;
        self.commit(MoveRecord {
            player,
            action: Action::Place(at),
            captured,
        });
        Ok(())
    }

    /// 弃着：手数推进，盘面不变。弃着总是合法。
    // 阶段 3 引擎对局（AI 弃着 / 终局流程）使用；接入前无调用方。
    #[allow(dead_code)]
    pub fn pass(&mut self) {
        let player = self.to_play;
        self.commit(MoveRecord {
            player,
            action: Action::Pass,
            captured: Vec::new(),
        });
    }

    /// 试算落子结果：返回落子后的盘面与提子，不修改自身状态。
    /// 劫争在此判定：落子后不得回到上一手之前的盘面。
    fn trial(&self, at: Coord) -> Result<(Grid, Vec<Coord>), IllegalReason> {
        let mut grid = self.grid.clone();
        let captured = rules::apply_move(self.size, &mut grid, self.to_play, at)?;
        if self.cursor > 0 && grid == self.positions[self.cursor - 1] {
            return Err(IllegalReason::Ko);
        }
        Ok((grid, captured))
    }

    /// 把记录追加为最新一手（游标必须已在末端；弃着盘面不变）。
    fn commit(&mut self, record: MoveRecord) {
        let player = record.player;
        self.moves.push(record);
        self.positions.push(self.grid.clone());
        self.cursor += 1;
        self.to_play = player.opposite();
        self.recount_captures();
    }

    // ---- 历史导航 ----

    /// 是否可以后退（游标 > 0）。
    pub fn can_step_back(&self) -> bool {
        self.cursor > 0
    }

    /// 是否可以前进（游标 < 最新手数）。
    pub fn can_step_forward(&self) -> bool {
        self.cursor < self.moves.len()
    }

    /// 后退一手（保留后续手数，可再前进）。返回是否发生移动。
    pub fn step_back(&mut self) -> bool {
        if !self.can_step_back() {
            return false;
        }
        self.go_to(self.cursor - 1)
    }

    /// 前进一手（回放已悔掉/回看过的手数）。返回是否发生移动。
    pub fn step_forward(&mut self) -> bool {
        if !self.can_step_forward() {
            return false;
        }
        self.go_to(self.cursor + 1)
    }

    /// 跳到第 `n` 手之后的盘面（`0..=move_count`，0 为初始盘面）。
    /// 返回是否成功（越界则原地不动）。
    pub fn go_to(&mut self, n: usize) -> bool {
        if n > self.moves.len() {
            return false;
        }
        self.cursor = n;
        self.grid = self.positions[n].clone();
        self.to_play = self.calc_to_play(n);
        self.recount_captures();
        true
    }

    /// 悔棋：删除最新一手并返回其记录。仅在游标处于最新一手时可用；
    /// 回看中请先 [`Board::go_to`] 到末端。
    pub fn undo(&mut self) -> Option<MoveRecord> {
        if self.cursor != self.moves.len() {
            return None;
        }
        let record = self.moves.pop()?;
        self.positions.pop();
        self.cursor -= 1;
        self.grid = self.positions[self.cursor].clone();
        self.to_play = record.player;
        self.recount_captures();
        Some(record)
    }

    // ---- 内部辅助 ----

    /// 第 `n` 手之后的行棋方：下一手已记录则取其行棋方，
    /// 否则取最后一手的对方（空盘为黑先）。
    fn calc_to_play(&self, n: usize) -> Stone {
        match self.moves.get(n) {
            Some(next) => next.player,
            None => self
                .moves
                .last()
                .map_or(Stone::Black, |last| last.player.opposite()),
        }
    }

    /// 按 `moves[..cursor]` 重建提子计数。O(n) 但实现直白，
    /// 与导航路径共用同一口径，避免增量维护出错。
    fn recount_captures(&mut self) {
        let (mut by_black, mut by_white) = (0u32, 0u32);
        for record in &self.moves[..self.cursor] {
            match record.player {
                Stone::Black => by_black += record.captured.len() as u32,
                Stone::White => by_white += record.captured.len() as u32,
            }
        }
        self.captured_by_black = by_black;
        self.captured_by_white = by_white;
    }
}
