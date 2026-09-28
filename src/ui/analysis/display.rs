//! 界面偏好枚举与显示换算：候选类显示门控（第 5 项）与目数视角。
//!
//! 两枚举只影响**显示层判定**，不改数据接收与走子决策（口径见各类型文档）。

use crate::board::Stone;

/// 候选类显示的门控模式（第 5 项）：**只影响界面何时显示「候选类」内容**
/// （候选点圆圈 / PV 幽灵子 / 侧栏候选列表 / 候选点领地层），不影响任何
/// 数据接收与走子决策——流式报告照常落 `snapshot`，胜率 / 目差数字照常
/// 实时刷新，引擎应手只认 `play_snapshot`（走子口径），与门控完全正交。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CandidateGating {
    /// 立即显示（默认，与流式分析实时性一致）。
    #[default]
    Immediate,
    /// 延迟 N 秒：引擎对当前局面开始思考满 N 秒后才显示候选类内容。
    /// 治「第一条流式中间报告（40–300 visits 浅层）画满候选点误导用户」：
    /// 满 N 秒时搜索已推进到可参考的深度。计时基准 = 当前查询的发起时刻
    /// （`inflight.started`），终态报告一律立即显示（分析已结束无浅层问题）。
    Delayed { secs: u32 },
    /// 手动：按快捷键（F）才显示；局面一变（游标 / 分支 / 落子）即重新
    /// 要求确认。「已确认」状态由 App 持有（键盘输入在 UI 层），本层只
    /// 提供判定函数。
    Manual,
}

impl CandidateGating {
    /// 界面显示名（侧栏选择器用）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Immediate => "立即",
            Self::Delayed { .. } => "延迟",
            Self::Manual => "手动",
        }
    }
}

/// 显示视角（LizzieYzy「目数视角」的移植）：**永远黑视角**（默认）/
/// **黑白交替**（轮到谁就显示谁的胜率 / 目差）。
///
/// 只作用于**显示层**；数据存储恒为黑视角（见模块文档「目数视角口径」）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayView {
    /// 永远黑视角（默认，与全部历史数据同口径）。
    #[default]
    Black,
    /// 黑白交替：轮到黑显示黑视角，轮到白显示白视角。
    Alternating,
}

impl DisplayView {
    /// 界面显示名。
    pub fn name(self) -> &'static str {
        match self {
            Self::Black => "永远黑视角",
            Self::Alternating => "黑白交替",
        }
    }
}

/// 显示换算（共用小函数）：黑视角值 + 该局面行棋方 + 当前显示视角 →
/// 显示值。**本仓唯一的视角换算入口**，胜率与目差共用（目差取负，
/// 胜率取 1−）。
///
/// - 轮黑 / 黑视角：原样；
/// - 轮白 + 交替视角：`winrate ← 1 − winrate`、`score_lead ← −score_lead`
///   （v1.18.2 实测 SIDETOMOVE 报文满足的同一组恒等式，见
///   `protocol::WinrateView`）；
/// - **损失类数字不经过本函数**：损失（`loss_from_points` 等）本来就是
///   「行棋方亏损多少」，与显示视角无关。
pub(crate) fn display_values(
    view: DisplayView,
    player: Stone,
    winrate: f64,
    score_lead: f64,
) -> (f64, f64) {
    match view {
        DisplayView::Black => (winrate, score_lead),
        DisplayView::Alternating if player == Stone::White => (1.0 - winrate, -score_lead),
        DisplayView::Alternating => (winrate, score_lead),
    }
}
