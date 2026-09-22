//! 棋盘分析叠加层（TASKS 4.1 / 4.2 / 4.4）：候选点圆圈、ownership 热度图、
//! 「候选点走后」领地图、失误标注、侧栏点击定位高亮与主变幽灵子。
//!
//! 数据只读自 [`Snapshot`] 与 [`AnalysisState`]（分析结果唯一存放点，
//! 失误标注按手数从历史缓冲现场派生），几何换算复用 [`Layout`]；
//! 无数据 / 数据不合法（`ownership` 缺失或长度不符）时静默跳过，绝不
//! panic。层开关关闭时调用方直接跳过绘制调用，不产生任何每帧开销。
//!
//! 映射规则（与侧栏图例共用 [`winrate_color`] / [`severity_color`]）：
//!
//! - 候选点大小：`sqrt(visits / 可见候选最大 visits)` 线性映射到
//!   `[0.16, 0.42] × 间距` 的半径；首个候选为主选点，额外画白环；
//! - 候选点颜色与文字：按**黑方视角**胜率（引擎 cfg
//!   `reportAnalysisWinratesAs = BLACK`，不按行棋方翻转）三段渐变
//!   `0.0 蓝（白大优）→ 0.5 黄（均势）→ 1.0 绿（黑大优）`，
//!   圆心标注百分比（如 `37.0%`）；
//! - 热度图：`ownership` 为各点目差（正 = 黑势），按当前快照最大 |值|
//!   （下限 [`HEAT_SCALE_MIN`]）线性归一化后乘最大不透明度（开局各点
//!   目差实测 |v| < 1，绝对刻度会整层不可见）；黑势叠深色块、白势叠
//!   浅色块，绘制在棋子之下，棋子保持清晰；
//! - **候选点领地图**（opt-in `includeMovesOwnership`）：侧栏聚焦某候选点
//!   且该候选带 ownership 时，热度图数据源从「当前局面」切换为「走该手
//!   之后」的领地，并**换一套色相**（紫灰 vs 根层暖黑/暖白）让两种层
//!   肉眼可辨；无聚焦 / 候选无数据时回落根局面并如实标注来源。
//!   **聚焦切换不重发查询**（设计决策）：聚焦是点击触发而非 hover，无
//!   查询风暴风险；且引擎搜索树跨查询存活，同 visits 重查虽快，但一次
//!   查询的 moveInfos 已携带**全部**候选点的 ownership——切焦点只是从
//!   已到手的候选表里换一条向量，重查纯属浪费，还能保持流式刷新连续。
//! - 策略热度图：`policy` 为策略网络的选点先验（「还没搜索时的第一直觉」，
//!   与候选点的「搜索后结论」是不同维度），只取前 size² 项、只画**空点**
//!   （已有棋子的点没有落子意义，policy 在那里是噪声）；按当前快照空点
//!   最大值（下限 [`POLICY_SCALE_MIN`]）做**相对刻度**归一化（绝对概率
//!   常在 1e-4～1e-1，不归一化会看不见），透明度走 sqrt 曲线（policy 分布
//!   高度尖锐，线性刻度下除 argmax 外全部不可见，与候选点圆圈的
//!   √visits 同理）；色用深靛蓝小圆角块，与 ownership 的暖黑/暖白
//!   整格方块同开时可分辨。
//! - 失误标注（KaTrain 风格）：疑问手及以上的落子处叠小色点
//!   （黄疑问手 / 橙失误 / 红恶手），画在棋子之上但不遮棋子辨识；
//!   当前手恰为失误手时额外加外环。回看中同样针对所有已知手数绘制。

use egui::{Align2, Color32, FontId, Painter, Rect, Stroke, Vec2};

use crate::board::{Action, Board, Coord, Stone};

use super::analysis::{loss_from_points, AnalysisState, Severity, Snapshot};
use super::board_view::Layout;

/// 棋盘上绘制的候选点条数（与侧栏 `MOVE_LIMIT` 解耦，各自维护）。
const CANDIDATE_LIMIT: usize = 8;
/// 主变幽灵子最多绘制的手数（与侧栏 PV 文本行共用 [`PV_LIMIT`]，
/// 保证「看到的主变文本」与「棋盘预览」截断一致）。
const GHOST_LIMIT: usize = super::analysis_panel::PV_LIMIT;
/// 热度归一化分母下限（目）：开局 |ownership| 很小，保底分母避免噪声放大过度。
const HEAT_SCALE_MIN: f32 = 2.0;
/// 热度块最大不透明度（0-255），为网格与棋子留出辨识度。
const HEAT_MAX_ALPHA: f32 = 140.0;
/// 策略概率归一化分母下限：相对刻度按快照空点最大值缩放，但最大值
/// 过小（接近全盘均匀分布的 1/362 ≈ 0.0028）时按均匀分布取分母，
/// 避免纯噪声被放大成满盘亮斑。
const POLICY_SCALE_MIN: f32 = 1.0 / 362.0;
/// 策略热度块最大不透明度：与 ownership 热度图同档。颜色选深靛蓝，
/// 混色实测：木底（211,168,92）上 alpha 200 混合后蓝红差 44、蓝绿差 66，
/// 色相可辨；淡紫（96,74,190）低透明度混合后偏灰不可辨（已否决）。
const POLICY_MAX_ALPHA: f32 = 200.0;
/// 策略热度块的最大不透明度之下限：低于此 alpha 的点不画。policy 分布
/// 高度尖锐（top1 常占过半），sqrt 刻度会把 1e-4 量级的长尾也拉成可见块，
/// 满盘噪点反而淹没亮区；截断后可见点集中在前几名，与候选点列表可对照。
const POLICY_MIN_ALPHA: f32 = 36.0;
/// 策略热度块相对间距的边长：接近整格（略缩避免相邻相接），与
/// ownership 的整格方块观感接近；小圆角 + 靛蓝色相负责区分两者。
const POLICY_FILL_RATIO: f32 = 0.86;

/// 叠加层状态：层开关与侧栏定位（App 持有，本次运行内保持）。
#[derive(Debug)]
pub struct Overlay {
    /// 候选点圆圈层开关。
    pub show_candidates: bool,
    /// 局势热度图层开关。
    pub show_heat: bool,
    /// 策略热度图层开关（引擎还没搜索时的第一直觉，非推荐）。
    /// 默认关闭：opt-in 数据，打开时 `App` 同步 `AnalysisState` 重发查询。
    pub show_policy: bool,
    /// 「候选点领地」开关（opt-in `includeMovesOwnership`）：开启后聚焦
    /// 候选点时热度图切换为「走该手之后」的领地（紫灰色相）。
    /// 默认关闭：每候选 × 361 float 的查询字段，打开时 `App` 同步
    /// `AnalysisState` 重发查询。
    pub show_moves_heat: bool,
    /// 失误标注层开关（疑问手及以上才画，关闭时零绘制开销）。
    pub show_mistakes: bool,
    /// 侧栏点击定位；局面变化（快照作废）时由 App 清除。
    pub focus: Option<Focus>,
}

/// 侧栏点击定位的候选点。
#[derive(Debug)]
pub struct Focus {
    /// 棋盘上高亮的落点。
    pub at: Coord,
    /// 主变幽灵子（弃着已剔除，截断到 [`GHOST_LIMIT`]）。
    pub ghosts: Vec<(Coord, Stone)>,
}

/// 由主变与行棋方构造幽灵子序列（侧栏点击定位时调用）。
pub fn ghosts_from_pv(pv: &[Option<Coord>], to_play: Stone) -> Vec<(Coord, Stone)> {
    let players = std::iter::successors(Some(to_play), |s| Some(s.opposite()));
    pv.iter().filter_map(|c| *c).zip(players).take(GHOST_LIMIT).collect()
}

/// 胜率 → 颜色（黑方视角，不翻转）：
/// `0.0 蓝（白大优）→ 0.5 黄（均势）→ 1.0 绿（黑大优）`。
/// 侧栏图例与棋盘候选点共用，保证两处观感一致。
pub(crate) fn winrate_color(winrate: f64) -> Color32 {
    const BLUE: Color32 = Color32::from_rgb(56, 118, 235);
    const YELLOW: Color32 = Color32::from_rgb(248, 196, 42);
    const GREEN: Color32 = Color32::from_rgb(44, 190, 88);
    let (from, to, t) = if winrate <= 0.5 {
        (BLUE, YELLOW, winrate / 0.5)
    } else {
        (YELLOW, GREEN, (winrate - 0.5) / 0.5)
    };
    let t = t.clamp(0.0, 1.0) as f32;
    Color32::from_rgb(
        lerp(from.r(), to.r(), t),
        lerp(from.g(), to.g(), t),
        lerp(from.b(), to.b(), t),
    )
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
}

/// 严重程度 → 标注色（棋盘标记、侧栏统计与曲线联动共用）：
/// 黄疑问手 / 橙失误 / 红恶手，黄橙取自既有提示色系，保证观感一致。
pub(crate) fn severity_color(severity: Severity) -> Color32 {
    match severity {
        // 不标注的档位无对应色（调用方先用 `is_marked` 过滤），给中性灰兜底。
        Severity::Good | Severity::Fine => Color32::from_rgb(140, 140, 148),
        Severity::Questionable => Color32::from_rgb(248, 196, 42),
        Severity::Mistake => Color32::from_rgb(255, 152, 82),
        Severity::Blunder => Color32::from_rgb(235, 77, 61),
    }
}

/// 热度图数据源：聚焦候选点的「走后领地」，或回落当前局面。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HeatSource {
    /// 当前局面（根 ownership）。
    Position,
    /// 聚焦候选点（`at` = 该候选落点）走之后的领地。
    Candidate(Coord),
}

impl HeatSource {
    /// 侧栏 / 图例的来源标注文案。
    pub fn label(self, size: crate::board::Size) -> String {
        match self {
            Self::Position => "当前局面".to_owned(),
            Self::Candidate(c) => format!("候选 {} 走后", c.to_gtp(size)),
        }
    }
}

/// 候选点领地层的色相与透明度（与根局面层明显区分）：
/// - 根局面层：暖黑 / 暖白（贴近「实地成片」的直观感受）；
/// - 候选点层：紫灰相（冷色，一看即知「这不是现在的领地，是走那手之后
///   的假想图」），且整体透明度上调一档——层内最大 |ownership| 通常比
///   根局面大（走一手后空点更多成空），同刻度会显得更刺眼，压一档平衡。
const CAND_HEAT_DARK: (u8, u8, u8) = (44, 20, 66);
const CAND_HEAT_LIGHT: (u8, u8, u8) = (232, 222, 246);
const CAND_HEAT_MAX_ALPHA: f32 = 170.0;

/// 解析热度图的数据源：优先聚焦候选点（侧栏点击设置）的
/// `MoveInfo::ownership`；无聚焦 / 候选无数据 / 长度不符时回落根局面。
/// 返回 `(数据源标识, 向量)`；两路都无数据返回 `None`（整层跳过）。
pub(crate) fn heat_source<'a>(
    snapshot: &'a Snapshot,
    focus: Option<&Focus>,
) -> Option<(HeatSource, &'a [f32])> {
    let size = snapshot.size;
    let count = size.point_count();
    if let Some(focus) = focus {
        // 只认「聚焦点 = 该候选落点」的条目：局面刷新后 focus 被 App 清除，
        // 不会出现跨局面错配；弃着（mv = None）不参与。
        if let Some(info) = snapshot
            .moves
            .iter()
            .find(|info| Some(focus.at) == info.mv)
            && let Some(vec) = info.ownership.as_deref()
            && vec.len() == count
        {
            return Some((HeatSource::Candidate(focus.at), vec));
        }
    }
    // 回落：根局面 ownership（开关未开 / 未聚焦 / 该候选无数据均到此）。
    snapshot
        .ownership
        .as_deref()
        .filter(|vec| vec.len() == count)
        .map(|vec| (HeatSource::Position, vec))
}

/// ownership 热度图：整格色块铺在网格之上、棋子之下。
/// 数据源 = [`heat_source`]（聚焦候选点的「走后领地」优先，回落当前
/// 局面）；候选点层用紫灰色相与根局面层（暖黑/暖白）肉眼区分。
/// `None` / 空局面（turn == 0）安全跳过。
pub(crate) fn draw_heat(
    painter: &Painter,
    layout: &Layout,
    snapshot: &Snapshot,
    focus: Option<&Focus>,
) -> Option<HeatSource> {
    if snapshot.turn == 0 {
        return None; // 空局面不渲染热度
    }
    let (source, ownership) = heat_source(snapshot, focus)?;
    let candidate_layer = matches!(source, HeatSource::Candidate(_));
    // 分母取当前数据源最大 |ownership|（下限保底），线性映射：
    // 最强点取最大不透明度，弱值回落，避免开局小值被放大成整盘雾感。
    let scale = ownership.iter().fold(HEAT_SCALE_MIN, |a, &b| a.max(b.abs()));
    let (dark, light, max_alpha) = if candidate_layer {
        (CAND_HEAT_DARK, CAND_HEAT_LIGHT, CAND_HEAT_MAX_ALPHA)
    } else {
        ((16, 22, 30), (246, 249, 253), HEAT_MAX_ALPHA)
    };
    for (i, v) in ownership.iter().enumerate() {
        let alpha = (v.abs() / scale * max_alpha).round() as u8;
        if alpha == 0 {
            continue; // 接近零的点不画，省去大半填充
        }
        let Some(c) = Coord::from_index(snapshot.size, i) else { continue };
        // 外扩半像素，避免相邻色块之间出现细缝。
        let rect = Rect::from_center_size(layout.point(c), Vec2::splat(layout.spacing + 0.5));
        // 正值 = 黑势（引擎约定）：黑势叠深色，白势叠浅色。
        let (r, g, b) = if *v > 0.0 { dark } else { light };
        painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(r, g, b, alpha));
    }
    Some(source)
}

/// 策略热度图：策略网络的选点先验铺在空点上（棋子之下）。
/// `None` / 长度不符 / 空局面（turn == 0）安全跳过；末位（推定弃着）
/// 与已有棋子的点不画。概率按当前快照空点最大值做相对刻度归一化。
pub(crate) fn draw_policy(
    painter: &Painter,
    layout: &Layout,
    board: &Board,
    snapshot: &Snapshot,
) {
    if snapshot.turn == 0 {
        return; // 空局面不渲染（开局直觉图信息量低且全是空点噪声）
    }
    let Some(policy) = &snapshot.policy else {
        return;
    };
    let size = snapshot.size;
    if policy.len() != size.point_count() + 1 {
        return; // 长度不符：数据不可信，整层跳过
    }
    let board_policy = &policy[..size.point_count()];
    // 分母取**空点**的最大值（已有棋子的点不参与，否则死子附近的噪声
    // 值会压低全部有效点的亮度）；下限按均匀分布保底，防纯噪声放大。
    let mut scale = POLICY_SCALE_MIN;
    for (i, &v) in board_policy.iter().enumerate() {
        if Coord::from_index(size, i).is_some_and(|c| board.get(c).is_none()) {
            scale = scale.max(v);
        }
    }
    for (i, &v) in board_policy.iter().enumerate() {
        let Some(c) = Coord::from_index(size, i) else { continue };
        if board.get(c).is_some() {
            continue; // 只画空点：已有棋子的点没有落子意义
        }
        let alpha = ((v / scale).sqrt().clamp(0.0, 1.0) * POLICY_MAX_ALPHA).round() as u8;
        if (alpha as f32) < POLICY_MIN_ALPHA {
            continue; // 长尾噪声不画：截断后亮区集中在前几名
        }
        // 深靛蓝：与 ownership 的暖黑/暖白色块、候选点的蓝黄绿圆圈都不同相；
        // 小圆角方块与 ownership 的整格大方块拉开形状辨识度。
        let center = layout.point(c);
        let radius = layout.spacing * 0.5 * POLICY_FILL_RATIO;
        let rect = Rect::from_center_size(center, Vec2::splat(radius * 2.0));
        painter.rect_filled(rect, radius * 0.4, Color32::from_rgba_unmultiplied(52, 36, 140, alpha));
    }
}

/// 候选点圆圈：大小随 visits、颜色随黑方胜率、圆心标注百分比；
/// 主选点（首个候选）额外白环。
pub(crate) fn draw_candidates(painter: &Painter, layout: &Layout, snapshot: &Snapshot) {
    let max_visits = snapshot
        .moves
        .iter()
        .take(CANDIDATE_LIMIT)
        .map(|info| info.visits)
        .max()
        .unwrap_or(1)
        .max(1);
    let (r_min, r_max) = (layout.spacing * 0.16, layout.spacing * 0.42);
    let mut main_seen = false;
    for info in snapshot.moves.iter().take(CANDIDATE_LIMIT) {
        let Some(c) = info.mv else { continue }; // 弃着无处可画
        let is_main = !main_seen;
        main_seen = true;
        let t = ((info.visits as f32) / (max_visits as f32)).sqrt().clamp(0.0, 1.0);
        let radius = r_min + (r_max - r_min) * t;
        let center = layout.point(c);
        if is_main {
            painter.circle_stroke(
                center,
                radius + 3.0,
                Stroke::new(2.0, Color32::from_rgba_unmultiplied(255, 255, 255, 220)),
            );
        }
        let fill = winrate_color(info.winrate);
        painter.circle_filled(
            center,
            radius,
            Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 200),
        );
        painter.circle_stroke(center, radius, Stroke::new(1.0, Color32::from_rgba_unmultiplied(25, 25, 25, 130)));
        // 胜率文字：深色描底 + 浅色主体，任何底色都可读。
        let label = format!("{:.1}%", info.winrate * 100.0);
        let font =
            FontId::proportional((layout.spacing * 0.26).clamp(8.0, 12.0) + if is_main { 1.5 } else { 0.0 });
        painter.text(
            center + Vec2::splat(1.0),
            Align2::CENTER_CENTER,
            &label,
            font.clone(),
            Color32::from_rgba_unmultiplied(0, 0, 0, 160),
        );
        painter.text(center, Align2::CENTER_CENTER, &label, font, Color32::from_rgba_unmultiplied(255, 255, 255, 245));
    }
}

/// 失误标注（KaTrain 风格）：疑问手及以上的落子处叠小色点，颜色按
/// [`severity_color`] 分档；当前手恰为失误手时额外加一圈外环更醒目。
/// 回看中针对**所有已知手数**绘制，不只当前手；只沿**当前线**取数
/// （[`AnalysisState::line_points`]），其它分支的手不与当前线混排；
/// 数据不全（未知）的手数由 [`loss_from_points`] 返回 `None`，自然跳过。
pub(crate) fn draw_mistakes(
    painter: &Painter,
    layout: &Layout,
    board: &Board,
    analysis: &AnalysisState,
) {
    let slots = analysis.line_points(board);
    for (i, record) in board.line_records().iter().enumerate() {
        let Action::Place(at) = record.action else { continue }; // 弃着无处可标
        let (Some(before), Some(after)) = (slots[i], slots[i + 1]) else { continue };
        let Some(loss) = loss_from_points(i + 1, record.player, before, after) else { continue };
        if !loss.severity.is_marked() {
            continue; // 好棋 / 尚可不标，避免满盘花花绿绿
        }
        let color = severity_color(loss.severity);
        let center = layout.point(at);
        let radius = (layout.spacing * 0.16).max(2.5);
        painter.circle_filled(center, radius, color);
        // 深色细描边：白子上定形，黑子上靠亮色填充保持辨识。
        painter.circle_stroke(
            center,
            radius,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(20, 20, 20, 150)),
        );
        if loss.turn == board.cursor() {
            // 当前手恰为失误手：棋子外缘加同色环，与末手内环标记呼应。
            painter.circle_stroke(
                center,
                layout.spacing * 0.56,
                Stroke::new((layout.spacing * 0.07).max(2.0), color),
            );
        }
    }
}

/// 定位高亮：主变幽灵子（半透明，纯绘制不挡交互）+ 目标点琥珀色双环。
pub(crate) fn draw_focus(painter: &Painter, layout: &Layout, board: &Board, focus: &Focus) {
    let radius = layout.spacing * 0.47;
    for (i, (c, stone)) in focus.ghosts.iter().enumerate() {
        if board.get(*c).is_some() {
            continue; // 盘面已有棋子的点不叠加，保持整洁
        }
        let alpha = 132u8.saturating_sub(i as u8 * 12); // 越往后越淡
        let (fill, edge) = match stone {
            Stone::Black => (
                Color32::from_rgba_unmultiplied(26, 26, 28, alpha),
                Color32::from_rgba_unmultiplied(255, 255, 255, alpha / 2),
            ),
            Stone::White => (
                Color32::from_rgba_unmultiplied(250, 250, 246, alpha),
                Color32::from_rgba_unmultiplied(60, 60, 60, alpha / 2),
            ),
        };
        let center = layout.point(*c);
        painter.circle_filled(center, radius, fill);
        painter.circle_stroke(center, radius, Stroke::new(1.0, edge));
    }
    // 目标点双环：琥珀色，与候选点配色明显区分。
    let center = layout.point(focus.at);
    let width = (layout.spacing * 0.08).max(2.5);
    painter.circle_stroke(center, layout.spacing * 0.60, Stroke::new(width, Color32::from_rgb(255, 170, 40)));
    painter.circle_stroke(
        center,
        layout.spacing * 0.70,
        Stroke::new(width * 0.5, Color32::from_rgba_unmultiplied(255, 170, 40, 150)),
    );
}
