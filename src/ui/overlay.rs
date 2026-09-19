//! 棋盘分析叠加层（TASKS 4.1 / 4.2）：候选点圆圈、ownership 热度图、
//! 侧栏点击定位高亮与主变幽灵子。
//!
//! 数据只读自 [`Snapshot`]（分析结果唯一存放点），几何换算复用
//! [`Layout`]；无数据 / 数据不合法（`ownership` 缺失或长度不符）时
//! 静默跳过，绝不 panic。层开关关闭时调用方直接跳过绘制调用，
//! 不产生任何每帧开销。
//!
//! 映射规则（与侧栏图例共用 [`winrate_color`]）：
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
//!   浅色块，绘制在棋子之下，棋子保持清晰。

use egui::{Align2, Color32, FontId, Painter, Rect, Stroke, Vec2};

use crate::board::{Board, Coord, Stone};

use super::analysis::Snapshot;
use super::board_view::Layout;

/// 棋盘上绘制的候选点条数（与侧栏 `MOVE_LIMIT` 解耦，各自维护）。
const CANDIDATE_LIMIT: usize = 8;
/// 主变幽灵子最多绘制的手数。
const GHOST_LIMIT: usize = 8;
/// 热度归一化分母下限（目）：开局 |ownership| 很小，保底分母避免噪声放大过度。
const HEAT_SCALE_MIN: f32 = 2.0;
/// 热度块最大不透明度（0-255），为网格与棋子留出辨识度。
const HEAT_MAX_ALPHA: f32 = 140.0;

/// 叠加层状态：层开关与侧栏定位（App 持有，本次运行内保持）。
#[derive(Debug)]
pub struct Overlay {
    /// 候选点圆圈层开关。
    pub show_candidates: bool,
    /// 局势热度图层开关。
    pub show_heat: bool,
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

/// ownership 热度图：整格色块铺在网格之上、棋子之下。
/// `None` / 长度不符 / 空局面（turn == 0）安全跳过。
pub(crate) fn draw_heat(painter: &Painter, layout: &Layout, snapshot: &Snapshot) {
    if snapshot.turn == 0 {
        return; // 空局面不渲染热度
    }
    let Some(ownership) = &snapshot.ownership else {
        return;
    };
    let size = snapshot.size;
    if ownership.len() != size.point_count() {
        return; // 长度不符：数据不可信，整层跳过
    }
    // 分母取当前快照最大 |ownership|（下限保底），线性映射：
    // 最强点取最大不透明度，弱值回落，避免开局小值被放大成整盘雾感。
    let scale = ownership.iter().fold(HEAT_SCALE_MIN, |a, &b| a.max(b.abs()));
    for (i, v) in ownership.iter().enumerate() {
        let alpha = (v.abs() / scale * HEAT_MAX_ALPHA).round() as u8;
        if alpha == 0 {
            continue; // 接近零的点不画，省去大半填充
        }
        let Some(c) = Coord::from_index(size, i) else { continue };
        // 外扩半像素，避免相邻色块之间出现细缝。
        let rect = Rect::from_center_size(layout.point(c), Vec2::splat(layout.spacing + 0.5));
        // 正值 = 黑势（引擎约定）：黑势叠深色，白势叠浅色。
        let (r, g, b) = if *v > 0.0 { (16, 22, 30) } else { (246, 249, 253) };
        painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(r, g, b, alpha));
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
