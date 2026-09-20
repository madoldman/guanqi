//! 棋盘模块：坐标、规则与带历史的棋盘状态（9 / 13 / 19 路）。
//!
//! # 数据表示
//!
//! - 盘面 [`Grid`]：按行优先的一维数组（索引见 [`Coord::index`]），
//!   空点用 `None` 表示。选择 `Option<Stone>` 而非位棋盘的理由：
//!   本模块只为 GUI 服务，规模 ≤ 19×19，规则引擎只做泛洪搜索，
//!   克隆快照廉价、语义直白；`Option<Stone>` 经编译器优化后仅占 1 字节。
//! - 历史：**快照式谱树**。`nodes` 按创建顺序存放全部节点，节点 id 即下标；
//!   根节点（id 0）是预设局面，其余节点各存一手 [`MoveRecord`] 与该手
//!   之后的完整盘面。由此"按手数取局面"只需一次借用，简单劫判定为一次
//!   向量比较，与线性历史时代价相同。
//!
//! # 谱树
//!
//! ```text
//!   0 预设局面 ── 1 黑D4 ── 2 白Q16 ── 3 黑D16
//!                 └────── 4 黑Q4  ── 5 白D16
//! ```
//!
//! - **游标** `current` 是节点 id（不再是手数）；[`Board::cursor`] 返回该节点
//!   到根的路径长度，即"第几手"（根为 0）。
//! - 每个节点记一个**选中子节点**下标（第 0 个是主变）：自根沿各节点选中
//!   子节点下行得到一条**当前线**，游标始终落在当前线上。
//! - 每个节点记其子树中游标到过的**最深手数**，在子分支之间切换时藉此把手数
//!   恢复回来（见 [`Board::next_branch`]）。
//!
//! # 公开 API 在树形下的语义
//!
//! - [`Board::cursor`]：根到当前节点的路径长度（"第几手"，根为 0）。
//! - [`Board::records`]：根到当前节点的着法序列（长度 = `cursor()`）。
//! - [`Board::line_records`] / [`Board::line_len`]：**当前线**的全部着法与
//!   总手数；界面「第 X / Y 手」的 Y 取 `line_len()`（只有主变时与旧行为一致）。
//! - [`Board::record_at`] / [`Board::position_at`]：**当前线**第 i 手的记录 /
//!   第 n 手之后的盘面（n 从 0 起，0 为初始盘面）。
//! - [`Board::move_count`]：**全树**着法总数（所有非根节点数），与当前线无关；
//!   "载入棋谱共 N 手"取它，界面手数分母取 [`Board::line_len`]。
//! - [`Board::step_back`] / [`Board::step_forward`]：回父节点 / 走**当前选中的**
//!   子节点（无子节点返回 `false`）。
//! - [`Board::go_to`]：沿**当前线**定位到第 n 手（`0..=line_len()`），
//!   越界返回 `false` 且状态不变。
//! - [`Board::undo`]：**仅叶子可悔棋**——删除该叶子、游标回父节点、返回该手；
//!   当前节点非叶（含根）时返回 `None` 且状态不变。
//! - [`Board::play`] / [`Board::pass`]：**在非叶节点落子即创建变着分支**
//!   （不再返回 `IllegalReason::NotAtLatestMove`）；若该着法已存在于某个
//!   子节点，直接切换到它，不重复建分支。
//! - [`Board::captured_by`] / [`Board::lost_by`]：沿根到当前节点的路径累计。
//!
//! # 分支导航（供棋谱树控件使用）
//!
//! - 查询：[`Board::child_count`] / [`Board::child_move`] /
//!   [`Board::selected_child`] / [`Board::is_branch_point`] /
//!   [`Board::branch_index`] / [`Board::current_node`] / [`Board::nodes`]。
//! - 切换：[`Board::select_child`]（选第 i 个子分支）、[`Board::next_branch`] /
//!   [`Board::prev_branch`]（相邻子分支循环切换，并尽量恢复原手数）、
//!   [`Board::go_to_node`]（跳到任意节点）。
//!
//! # 劫争（简单劫）
//!
//! 仅实现**简单劫**：落子后盘面不得回到**上一手之前**的盘面，
//! 即禁止立即回提形成单步循环。树形下"上一手之前的盘面"即当前节点的
//! 父节点盘面（父节点盘面 = 对方上一手之前），判定与线性历史完全一致。
//! 弃着不改变盘面，因此对方弃着后"上一手之前的盘面"与当前盘面相同，
//! 任何落子都不会与之相等，简单劫禁着自然解除——这是该定义的固有结果。
//! **超级劫（全局同形禁止）本期不实现**，三劫循环等长循环一律放行。
//!
//! # 预设局面（让子局 / 摆子局，[`Board::from_setup`]）
//!
//! SGF 棋谱允许在非空初始局面上开始（`AB` / `AW` / `AE` 摆子，
//! `PL` 指定首着方）。预设局面的语义约定：
//!
//! - 摆子**不计入手数**（`cursor()` 从 0 起）、**不计提子统计**、
//!   **不参与劫争判定**（劫争仍只比较相邻两手真实着法间的盘面）；
//! - 根节点就是预设局面，[`Board::records`] 只含预设之后的真实着法；
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

/// 谱树节点：一手棋（根节点除外）及其之后的盘面快照。
///
/// 节点 id 即 [`Board::nodes`] 中的下标；字段私有，经访问器读取。
#[derive(Clone, Debug)]
pub struct Node {
    /// 产生本节点的着法；预设局面（根节点）为 `None`。
    record: Option<MoveRecord>,
    /// 父节点 id；根节点为 `None`。
    parent: Option<usize>,
    /// 子节点 id，第 0 个为默认主变。
    children: Vec<usize>,
    /// 本手之后的完整盘面。
    grid: Grid,
    /// 到根的路径长度（手数，根为 0）。
    depth: usize,
    /// 当前选中的子分支下标（0 = 主变）；无子节点时无意义。
    selected: usize,
    /// 本节点子树中游标到达过的最深手数（分支切换时用于恢复手数）。
    last_depth: usize,
}

impl Node {
    /// 产生本节点的着法；根节点（预设局面）为 `None`。
    pub fn record(&self) -> Option<&MoveRecord> {
        self.record.as_ref()
    }

    /// 父节点 id；根节点为 `None`。
    pub fn parent(&self) -> Option<usize> {
        self.parent
    }

    /// 子节点 id（第 0 个为默认主变）。
    pub fn children(&self) -> &[usize] {
        &self.children
    }

    /// 到根的路径长度（手数，根为 0）。
    pub fn depth(&self) -> usize {
        self.depth
    }
}

/// 棋盘状态：谱树 + 游标 + 提子计数。
///
/// 游标支持在不销毁后续手数（变着）的情况下回看与切换分支；
/// [`Board::undo`] 才是真正悔棋（删掉叶子节点）。
pub struct Board {
    size: Size,
    /// 根节点（预设局面）的行棋方。
    root_to_play: Stone,
    /// 谱树全部节点，id 即下标，根节点为 0。
    nodes: Vec<Node>,
    /// 游标：当前节点 id。
    current: usize,
    /// 当前线（自根沿各节点选中子节点下行）的全部着法，随游标刷新。
    line_records: Vec<MoveRecord>,
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
    /// - 根节点即预设局面，`records()` 只含其后的真实着法；
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
            root_to_play: to_play,
            nodes: vec![Node {
                record: None,
                parent: None,
                children: Vec::new(),
                grid,
                depth: 0,
                selected: 0,
                last_depth: 0,
            }],
            current: 0,
            line_records: Vec::new(),
            captured_by_black: 0,
            captured_by_white: 0,
        })
    }

    // ---- 只读查询 ----

    /// 棋盘尺寸。
    pub fn size(&self) -> Size {
        self.size
    }

    /// 当前行棋方（游标处）：当前节点的对方（根节点取预设局面的首着方）。
    pub fn to_play(&self) -> Stone {
        match self.nodes[self.current].record.as_ref() {
            Some(record) => record.player.opposite(),
            None => self.root_to_play,
        }
    }

    /// 游标处的盘面切片。
    pub fn grid(&self) -> &[Option<Stone>] {
        &self.nodes[self.current].grid
    }

    /// 取某点棋子；该点不在盘上时返回 `None`。
    pub fn get(&self, at: Coord) -> Option<Stone> {
        if at.on_board(self.size) {
            self.nodes[self.current].grid[at.index(self.size)]
        } else {
            None
        }
    }

    /// **全树**着法总数（含弃着，不含根节点）；与当前线无关。
    /// 界面手数分母请用 [`Board::line_len`]。
    pub fn move_count(&self) -> usize {
        self.nodes.len() - 1
    }

    /// 当前手数：根到当前节点的路径长度（0 = 预设局面）。
    pub fn cursor(&self) -> usize {
        self.nodes[self.current].depth
    }

    /// 根到当前节点的着法序列（复盘列表用；长度 = [`Board::cursor`]）。
    pub fn records(&self) -> &[MoveRecord] {
        &self.line_records[..self.cursor()]
    }

    /// 当前线（自根沿各节点选中子分支下行）的全部着法。
    /// 切片长度 = [`Board::line_len`]。
    pub fn line_records(&self) -> &[MoveRecord] {
        &self.line_records
    }

    /// 当前线总手数：界面「第 X / Y 手」的 Y（只有主变时等于旧的总手数）。
    pub fn line_len(&self) -> usize {
        self.line_records.len()
    }

    /// 当前线第 `i` 手（0 起）的记录。
    pub fn record_at(&self, i: usize) -> Option<&MoveRecord> {
        self.line_records.get(i)
    }

    /// 当前线第 `n` 手之后的完整盘面快照；`n == 0` 为初始盘面。
    // 阶段 5 谱树跳转时复核局面使用；接入前无调用方。
    #[allow(dead_code)]
    pub fn position_at(&self, n: usize) -> Option<&Grid> {
        self.line_node(n).map(|id| &self.nodes[id].grid)
    }

    /// `color` 方累计提走的对方子数（沿根到当前节点的路径，导航时随之回退）。
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

    // ---- 谱树查询与分支导航 ----

    /// 当前节点 id。
    pub fn current_node(&self) -> usize {
        self.current
    }

    /// 谱树全部节点（id 即下标，根节点为 0）。
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// 当前节点的子分支数（下一手的变着数量）。
    pub fn child_count(&self) -> usize {
        self.nodes[self.current].children.len()
    }

    /// 第 `i` 个子分支的首手记录（做「变着 1/2/3」选择器用）。
    #[allow(dead_code)]
    pub fn child_move(&self, i: usize) -> Option<&MoveRecord> {
        let child = *self.nodes[self.current].children.get(i)?;
        self.nodes[child].record.as_ref()
    }

    /// 当前选中的子分支下标（0 = 主变）；无子分支时为 0。
    pub fn selected_child(&self) -> usize {
        self.nodes[self.current].selected
    }

    /// 当前节点是否为分支点（子分支 ≥ 2）。
    pub fn is_branch_point(&self) -> bool {
        self.nodes[self.current].children.len() >= 2
    }

    /// 当前节点在其父节点子分支中的下标；根节点为 `None`。
    /// 用于显示「变着 i / N」，或判断当前手本身是不是一条变着。
    #[allow(dead_code)]
    pub fn branch_index(&self) -> Option<usize> {
        let node = &self.nodes[self.current];
        let parent = &self.nodes[node.parent?];
        parent.children.iter().position(|&c| c == self.current)
    }

    /// 选择第 `i` 个子分支：游标进入该子分支，并尽量恢复到本节点子树
    /// 记录的最深手数（走不到就停在叶子）。越界返回 `false` 且状态不变。
    pub fn select_child(&mut self, i: usize) -> bool {
        let cur = self.current;
        let Some(&child) = self.nodes[cur].children.get(i) else {
            return false;
        };
        self.nodes[cur].selected = i;
        let want = self.nodes[cur].last_depth;
        let mut node = child;
        // 沿各节点的选中子分支下走到记忆手数；中途是叶子就停下。
        while self.nodes[node].depth < want {
            let next = self.nodes[node].children.get(self.nodes[node].selected).copied();
            match next {
                Some(next) => node = next,
                None => break,
            }
        }
        self.set_current(node);
        true
    }

    /// 切到下一条子分支（在子分支间循环）。
    pub fn next_branch(&mut self) -> bool {
        self.switch_branch(1)
    }

    /// 切到上一条子分支（在子分支间循环）。
    pub fn prev_branch(&mut self) -> bool {
        self.switch_branch(-1)
    }

    /// 跳到任意节点 `id`（棋谱树控件用）：沿路径重建各层选中子分支。
    /// `id` 越界返回 `false` 且状态不变。
    #[allow(dead_code)]
    pub fn go_to_node(&mut self, id: usize) -> bool {
        if id >= self.nodes.len() {
            return false;
        }
        // 自底向上收集根到 id 的路径，再反转成自顶向下。
        let mut path = Vec::new();
        let mut cur = Some(id);
        while let Some(now) = cur {
            path.push(now);
            cur = self.nodes[now].parent;
        }
        path.reverse();
        for pair in path.windows(2) {
            let (parent, child) = (pair[0], pair[1]);
            if let Some(i) = self.nodes[parent].children.iter().position(|&c| c == child) {
                self.nodes[parent].selected = i;
            }
        }
        self.set_current(id);
        true
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
    ///
    /// 当前节点已有该着法的子节点时**直接切换到它**（不重复建分支）；
    /// 否则新建一个变着分支并把游标移过去——非叶节点落子因此不再被拒绝。
    pub fn play(&mut self, at: Coord) -> Result<(), IllegalReason> {
        let (grid, captured) = self.trial(at)?;
        if self.goto_existing_child(|record| record.action == Action::Place(at)) {
            return Ok(());
        }
        let player = self.to_play();
        // 先算好新节点的盘面再挂进树：grid 已是落子后的盘面。
        self.push_node(
            MoveRecord {
                player,
                action: Action::Place(at),
                captured,
            },
            grid,
        );
        Ok(())
    }

    /// 弃着：手数推进，盘面不变。弃着总是合法。
    /// 已有弃着子节点时同样直接切换过去，不重复建分支。
    // 阶段 3 引擎对局（AI 弃着 / 终局流程）使用；接入前无调用方。
    #[allow(dead_code)]
    pub fn pass(&mut self) {
        if self.goto_existing_child(MoveRecord::is_pass) {
            return;
        }
        let player = self.to_play();
        let grid = self.nodes[self.current].grid.clone();
        self.push_node(
            MoveRecord {
                player,
                action: Action::Pass,
                captured: Vec::new(),
            },
            grid,
        );
    }

    /// 试算落子结果：返回落子后的盘面与提子，不修改自身状态。
    /// 劫争在此判定：落子后不得回到"上一手之前"的盘面，
    /// 即当前节点的**父节点**盘面（父节点盘面 = 对方上一手之前）。
    /// 根节点盘面同样适用：根的第一手真实着法正是从根盘面出发下的，
    /// 根盘面即"该手之前"的盘面（预设局面也不例外）。
    fn trial(&self, at: Coord) -> Result<(Grid, Vec<Coord>), IllegalReason> {
        let cur = self.current;
        let mut grid = self.nodes[cur].grid.clone();
        let captured = rules::apply_move(self.size, &mut grid, self.to_play(), at)?;
        if let Some(parent) = self.nodes[cur].parent
            && grid == self.nodes[parent].grid
        {
            return Err(IllegalReason::Ko);
        }
        Ok((grid, captured))
    }

    /// 当前节点的子节点中若已存在满足 `pred` 的着法，切换过去并返回 `true`。
    /// 同一节点的子节点行棋方相同，故只需比较着法本身。
    fn goto_existing_child(&mut self, pred: impl Fn(&MoveRecord) -> bool) -> bool {
        let cur = self.current;
        let found = self.nodes[cur]
            .children
            .iter()
            .position(|&c| self.nodes[c].record.as_ref().is_some_and(&pred));
        let Some(i) = found else { return false };
        self.nodes[cur].selected = i;
        let child = self.nodes[cur].children[i];
        self.set_current(child);
        true
    }

    /// 在当前节点下挂一个新分支（变着），并把游标移过去。
    fn push_node(&mut self, record: MoveRecord, grid: Grid) {
        let parent = self.current;
        let id = self.nodes.len();
        let depth = self.nodes[parent].depth + 1;
        self.nodes.push(Node {
            record: Some(record),
            parent: Some(parent),
            children: Vec::new(),
            grid,
            depth,
            selected: 0,
            last_depth: depth,
        });
        let children = &mut self.nodes[parent].children;
        children.push(id);
        let last = children.len() - 1;
        self.nodes[parent].selected = last;
        self.set_current(id);
    }

    // ---- 历史导航 ----

    /// 是否可以后退（当前节点不是根节点）。
    pub fn can_step_back(&self) -> bool {
        self.nodes[self.current].parent.is_some()
    }

    /// 是否可以前进（当前节点有子节点，走当前选中的那个）。
    pub fn can_step_forward(&self) -> bool {
        let node = &self.nodes[self.current];
        node.children.get(node.selected).is_some()
    }

    /// 后退一手（回父节点，保留后续手数与变着）。返回是否发生移动。
    pub fn step_back(&mut self) -> bool {
        let Some(parent) = self.nodes[self.current].parent else {
            return false;
        };
        self.set_current(parent);
        true
    }

    /// 前进一手（走当前选中的子节点）。返回是否发生移动。
    pub fn step_forward(&mut self) -> bool {
        let cur = self.current;
        let Some(&child) = self.nodes[cur].children.get(self.nodes[cur].selected) else {
            return false;
        };
        self.set_current(child);
        true
    }

    /// 沿**当前线**跳到第 `n` 手之后的盘面（`0..=line_len()`，0 为初始盘面）。
    /// 返回是否成功（越界则原地不动）。
    pub fn go_to(&mut self, n: usize) -> bool {
        match self.line_node(n) {
            Some(node) => {
                self.set_current(node);
                true
            }
            None => false,
        }
    }

    /// 悔棋：仅在**叶子**上可用——删除当前节点并返回其记录；
    /// 当前节点非叶（还有后续手数或变着）或为根节点时返回 `None` 且状态不变。
    pub fn undo(&mut self) -> Option<MoveRecord> {
        let cur = self.current;
        let parent = self.nodes[cur].parent?;
        if !self.nodes[cur].children.is_empty() {
            return None;
        }
        let record = self.nodes[cur].record.clone()?;
        let last = self.nodes.len() - 1;
        let pos = self.nodes[parent].children.iter().position(|&c| c == cur)?;
        self.nodes[parent].children.remove(pos);
        // 选中子分支随删除左移；删掉的正是选中项时退回主变（或新的末项）。
        let count = self.nodes[parent].children.len();
        let sel = self.nodes[parent].selected;
        self.nodes[parent].selected = if count == 0 {
            0
        } else if sel > pos {
            sel - 1
        } else if sel == pos {
            pos.min(count - 1)
        } else {
            sel
        };
        self.nodes.swap_remove(cur);
        if cur != last {
            // 末尾节点被搬到 cur 的位置：修正父与子的反向引用。
            self.relocate(last, cur);
        }
        self.set_current(parent);
        Some(record)
    }

    // ---- 内部辅助 ----

    /// 把游标移到节点 `id`：记录访问深度并刷新缓存。
    fn set_current(&mut self, id: usize) {
        self.current = id;
        self.touch(id);
        self.refresh();
    }

    /// 记录游标到达的深度：`id` 及其所有祖先的最深手数取该深度
    /// （只增不减，即"本子树中到过的最深手数"），供分支切换恢复手数。
    fn touch(&mut self, id: usize) {
        let depth = self.nodes[id].depth;
        let mut cur = Some(id);
        while let Some(now) = cur {
            if self.nodes[now].last_depth < depth {
                self.nodes[now].last_depth = depth;
            }
            cur = self.nodes[now].parent;
        }
    }

    /// 在子分支间循环切换：以当前节点的选中子分支为基准偏移 `step` 步。
    fn switch_branch(&mut self, step: isize) -> bool {
        let count = self.nodes[self.current].children.len();
        if count == 0 {
            return false;
        }
        let sel = self.nodes[self.current].selected.min(count - 1);
        let target = (sel as isize + step).rem_euclid(count as isize) as usize;
        self.select_child(target)
    }

    /// 重建随游标变化的缓存：当前线着法与提子计数。
    /// 盘面与行棋方直接取当前节点，无需缓存。
    fn refresh(&mut self) {
        self.line_records.clear();
        let mut cur = 0usize;
        while let Some(&child) = self.nodes[cur].children.get(self.nodes[cur].selected) {
            match &self.nodes[child].record {
                Some(record) => self.line_records.push(record.clone()),
                None => break, // 非根节点必有 record；防御性分支
            }
            cur = child;
        }
        self.recount_captures();
    }

    /// 当前线上第 `n` 个节点（0 = 根）；`n` 超出线长返回 `None`。
    fn line_node(&self, n: usize) -> Option<usize> {
        let mut cur = 0usize;
        for _ in 0..n {
            let node = &self.nodes[cur];
            cur = *node.children.get(node.selected)?;
        }
        Some(cur)
    }

    /// `swap_remove` 后修正被搬动节点的正反向引用
    /// （父节点的 `children` / `selected` 与全部子节点的 `parent`）。
    fn relocate(&mut self, from: usize, to: usize) {
        if let Some(parent) = self.nodes[to].parent {
            if self.nodes[parent].selected == from {
                self.nodes[parent].selected = to;
            }
            for child in &mut self.nodes[parent].children {
                if *child == from {
                    *child = to;
                }
            }
        }
        let children = self.nodes[to].children.clone();
        for child in children {
            self.nodes[child].parent = Some(to);
        }
    }

    /// 按根到当前节点的着法序列重建提子计数。O(手数) 但实现直白，
    /// 与导航路径共用同一口径，避免增量维护出错。
    fn recount_captures(&mut self) {
        let end = self.cursor();
        let (mut by_black, mut by_white) = (0u32, 0u32);
        for record in &self.line_records[..end] {
            match record.player {
                Stone::Black => by_black += record.captured.len() as u32,
                Stone::White => by_white += record.captured.len() as u32,
            }
        }
        self.captured_by_black = by_black;
        self.captured_by_white = by_white;
    }
}
