//! 选点限制：限定区域（框选）与排除选点（avoid），以及「重发驱动开关」
//! （policy / 候选点级 ownership / PV visits 的 want/sent 两值比对 API）。
//!
//! 区域与排除引擎口径互斥（见 [`AnalysisLimits`] 文档）；任何修改经
//! [`LimitsEpoch`] 版本号驱动 `sync` 重发查询。

use crate::board::{Board, Coord, Size, Stone};
use crate::engine::MoveRules;

use super::AnalysisState;

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
    /// 限定区域（`Some` = 已在棋盘上框出区域，引擎只考虑区域内的空点）。
    pub region: Option<Region>,
    /// 被排除的手（GTP 坐标，按记录时的行棋方分组）。
    pub avoid: Vec<(Stone, Coord)>,
    /// 区域**模式**开关（独立于 `region` 矩形）：开启 = 棋盘进入框选
    /// 交互、引擎按区域限制选点（区域未拖出时视为全盘等待框选状态）。
    /// 之所以独立于 `region`：「开关在开、矩形还没画」是合法中间态
    /// （用户开了开关还没拖框），若用 `region.is_some()` 兼作模式判定，
    /// 开关会被拖框前的空状态弹回去（fa3c804 起的隐性缺陷，菜单重构
    /// 后在 headless 驱动下暴露）。关闭模式时同时清 `region`。
    pub mode: bool,
}

impl AnalysisLimits {
    /// 区域模式是否生效（模式开即生效，区域矩形可以在随后拖出）。
    pub fn has_region(&self) -> bool {
        self.mode
    }
}

/// 限制版本号：任何修改都会递增，`sync` 据此重发查询（局面未变时也重发）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct LimitsEpoch(pub(super) u32);

/// 由棋盘与限制构造查询级 move_rules（协议层 [`MoveRules`]）。
///
/// 规则只发给**当前行棋方**：区域 / 排除是「当前局面怎么选点」的约束，
/// 对手应手不受限（KaTrain 同款口径）。坐标用 `Coord::to_gtp` 组装。
pub(super) fn move_rules_of(limits: &AnalysisLimits, board: &Board) -> Option<MoveRules> {
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

impl AnalysisState {
    // ---- 选点限制（限定区域 / 排除选点）----

    /// 当前生效的选点限制（侧栏显示与状态标识读取）。
    pub fn limits(&self) -> &AnalysisLimits {
        &self.limits
    }

    /// 设置限定区域（`None` = 清除区域）。区域与排除互斥：设置区域后
    /// 排除列表保留但暂不生效（见 [`AnalysisLimits`] 文档）。设置非
    /// `None` 的区域同时进入区域模式（棋盘拖框路径自带模式开启）。
    pub fn set_region(&mut self, region: Option<Region>) {
        let mode = region.is_some();
        if self.limits.region == region && self.limits.mode == mode {
            return;
        }
        self.limits.region = region;
        self.limits.mode = mode;
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

    /// 移除一条排除项（下标 = 侧栏排除列表的行号）。移除的恰是入口
    /// 钉住的那条时，pin 状态同步失效（按钮回到「排除首选」）。
    pub fn remove_avoid(&mut self, index: usize) {
        if index < self.limits.avoid.len() {
            if let Some(pin) = self.avoid_pin
                && self.limits.avoid[index] == pin
            {
                self.avoid_pin = None;
            }
            self.limits.avoid.remove(index);
            self.bump_limits();
        }
    }

    /// 清空全部限制（区域模式 + 区域矩形 + 排除；「一键清除」入口共用）。
    pub fn clear_limits(&mut self) {
        if !self.limits.mode && self.limits.region.is_none() && self.limits.avoid.is_empty() {
            return;
        }
        self.limits = AnalysisLimits::default();
        // 一键清空覆盖一切限制，入口的 pin 状态随之失效。
        self.avoid_pin = None;
        self.bump_limits();
    }

    /// 开启区域模式（不设区域，等用户在棋盘上拖出）。
    /// 恒递增版本号：模式切换本身要触发重发（棋盘交互随之改变）。
    pub fn enable_region_mode(&mut self) {
        if self.limits.mode {
            return;
        }
        self.limits.mode = true;
        self.bump_limits();
    }

    /// 仅清空排除列表（区域与排除互斥，区域开启时不会走到这里）。
    pub fn clear_avoid(&mut self) {
        if self.limits.avoid.is_empty() {
            return;
        }
        self.limits.avoid.clear();
        // 手动清空覆盖一切排除项，入口的 pin 状态随之失效。
        self.avoid_pin = None;
        self.bump_limits();
    }

    // ---- 「排除首选看次优」便捷入口（菜单开关式）----

    /// 便捷入口当前钉住的手（菜单按钮文案判定用：`Some` = 显示「撤销」）。
    pub fn avoid_pin(&self) -> Option<(Stone, Coord)> {
        self.avoid_pin
    }

    /// 便捷入口的开关动作：`at = Some((player, coord))` 加入排除并钉住；
    /// `None` 撤销入口自己钉住的那一次（按 (行棋方, 坐标) 精确匹配，
    /// 只移除这一条——用户手动排除的其它手不受影响）。
    ///
    /// 区域模式下不生效（与 [`Self::toggle_avoid`] 同一互斥语义）；调用方
    /// （菜单）本就会置灰该入口，这里是数据层兜底。
    pub fn set_avoid_pin(&mut self, at: Option<(Stone, Coord)>) {
        if self.limits.has_region() {
            return;
        }
        match at {
            Some((player, coord)) => {
                // 已在列表里（例如用户手动排除过同一个点）：不重复添加，
                // 只把 pin 指向它——再点一次撤销时移除的就是这一条，
                // 状态与列表保持一致，不会出现「pin 有、列表没有」的幻觉。
                if !self.limits.avoid.iter().any(|(p, c)| *p == player && *c == coord) {
                    self.limits.avoid.push((player, coord));
                }
                self.avoid_pin = Some((player, coord));
            }
            None => {
                if let Some((player, coord)) = self.avoid_pin.take()
                    && let Some(pos) = self
                        .limits
                        .avoid
                        .iter()
                        .position(|(p, c)| *p == player && *c == coord)
                {
                    self.limits.avoid.remove(pos);
                }
            }
        }
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

    /// 设置 PV 每手计算量（`includePVVisits`）的请求开关。复刻
    /// [`Self::set_want_policy`] 的机制：值变化即作废快照，`sync` 检测
    /// `want/sent` 不一致后自动重发查询（局面未变也重发）。旧快照的
    /// 候选点不带 `pv_visits`，与新开关矛盾，作废重查才一致。
    pub fn set_want_pv_visits(&mut self, want: bool) {
        if self.want_pv_visits == want {
            return;
        }
        self.want_pv_visits = want;
        self.snapshot = None;
    }
}
