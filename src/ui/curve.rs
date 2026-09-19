//! 胜率曲线（TASKS 4.3）：底部面板逐手黑方胜率折线、当前手指示与悬停数值。
//!
//! 数据只读自 [`AnalysisState::history`]（逐手历史缓冲，随浏览逐步积累）：
//!
//! - 只画已知点：仅相邻两手（手数差 1）都有数据才连线，缺口留空，
//!   不臆造未知走势；已知点用小圆点标记，颜色复用 [`overlay::winrate_color`]
//!   （与棋盘候选点同一胜率映射）；
//! - 胜率一律**黑方视角**（`reportAnalysisWinratesAs = BLACK`），不翻转，
//!   标题与提示文案均注明；
//! - 当前手（[`Board::cursor`]）用琥珀色竖线 + 大圆点指示，
//!   该手尚无数据时仍显示竖线位置；
//! - 悬停时磁吸高亮最近已知手数（竖线 + 白环），并用 egui 原生指针提示
//!   显示手数 / 胜率 / 目差 / visits；离开曲线区全部消失。

use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::board::Board;

use super::analysis::{AnalysisState, HistoryPoint};
use super::overlay;

/// 曲线区四周留白：轴标注与标题占用。
const PAD_LEFT: f32 = 42.0;
const PAD_RIGHT: f32 = 12.0;
const PAD_TOP: f32 = 24.0;
const PAD_BOTTOM: f32 = 16.0;

/// 已知点标记半径 / 当前手指示半径。
const DOT_RADIUS: f32 = 2.5;
const CURSOR_RADIUS: f32 = 4.5;

/// 折线、当前手（与棋盘定位高亮同一琥珀色）、轴线与文字配色。
const LINE: Color32 = Color32::from_rgb(120, 200, 255);
const CURSOR: Color32 = Color32::from_rgb(255, 170, 40);
const AXIS: Color32 = Color32::from_rgb(120, 120, 128);
const GRID: Color32 = Color32::from_rgb(60, 62, 70);
const LABEL: Color32 = Color32::from_rgb(160, 160, 168);

/// 绘制胜率曲线面板内容。外层底部面板由 `app` 条件创建：
/// 面板隐藏时不进入本函数，零额外计算。
pub fn show(ui: &mut Ui, analysis: &AnalysisState, board: &Board) {
    let total = board.move_count();
    // 已知点（按手数升序，含 0 = 初始空盘）；总手数以外的陈旧条目不取。
    let known: Vec<HistoryPoint> =
        (0..=total).filter_map(|turn| analysis.history_point(turn)).collect();

    // 无论有无数据都先占满面板内容区：egui 0.36 的 Panel 收缩到内容尺寸，
    // 空状态只画一行小字会把面板塌缩成一条缝并记住该尺寸。
    let avail = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(avail, Sense::hover());
    let painter = ui.painter_at(avail);

    if total == 0 || known.is_empty() {
        painter.text(
            avail.center(),
            Align2::CENTER_CENTER,
            "尚无胜率数据：随浏览逐步积累。",
            FontId::proportional(13.0),
            LABEL,
        );
        return;
    }

    let plot = Rect::from_min_max(
        avail.min + Vec2::new(PAD_LEFT, PAD_TOP),
        avail.max - Vec2::new(PAD_RIGHT, PAD_BOTTOM),
    );
    if !plot.is_positive() {
        return; // 面板被拖得过窄：整体跳过，不画半截坐标系
    }

    // 坐标换算：X = 手数 [0, total]，Y = 黑方胜率 [0,1]（上高下低）。
    let x = |turn: f32| plot.left() + turn / total as f32 * plot.width();
    let y = |winrate: f32| plot.bottom() - winrate.clamp(0.0, 1.0) * plot.height();

    draw_axes(&painter, plot, total);
    painter.text(
        Pos2::new(plot.left(), avail.min.y + 5.0),
        Align2::LEFT_TOP,
        "黑方胜率",
        FontId::proportional(12.0),
        LABEL,
    );

    // 折线：仅相邻两手都有数据才连线，缺口留空。
    for pair in known.windows(2) {
        let (t0, t1) = (pair[0].turn, pair[1].turn);
        if t1 - t0 == 1 {
            painter.line_segment(
                [
                    Pos2::new(x(t0 as f32), y(pair[0].winrate as f32)),
                    Pos2::new(x(t1 as f32), y(pair[1].winrate as f32)),
                ],
                Stroke::new(1.8, LINE),
            );
        }
    }
    // 已知点标记。
    for p in &known {
        let center = Pos2::new(x(p.turn as f32), y(p.winrate as f32));
        painter.circle_filled(center, DOT_RADIUS, overlay::winrate_color(p.winrate));
    }

    draw_cursor(&painter, plot, &x, &y, board.cursor(), &known);
    draw_hover(&painter, plot, &x, &y, total, response, &known);
}

/// 坐标系：底板、边框、Y 轴 0/50/100% 标注、50% 虚线参考线、X 轴首末手数。
fn draw_axes(painter: &Painter, plot: Rect, total: usize) {
    painter.rect_filled(plot, 3.0, Color32::from_rgb(28, 30, 34));
    painter.rect_stroke(plot, 3.0, Stroke::new(1.0, AXIS), StrokeKind::Middle);
    // Y 轴百分比标注（黑方视角）。
    for (wr, label) in [(0.0f32, "0%"), (0.5, "50%"), (1.0, "100%")] {
        painter.text(
            Pos2::new(plot.left() - 5.0, plot.bottom() - wr * plot.height()),
            Align2::RIGHT_CENTER,
            label,
            FontId::proportional(10.0),
            LABEL,
        );
    }
    // 50% 虚线参考线。
    let mid = plot.center().y;
    let mut px = plot.left();
    while px < plot.right() {
        let end = (px + 5.0).min(plot.right());
        painter.line_segment([Pos2::new(px, mid), Pos2::new(end, mid)], Stroke::new(1.0, GRID));
        px += 9.0;
    }
    // X 轴首末手数。
    let font = FontId::proportional(10.0);
    painter.text(
        Pos2::new(plot.left(), plot.bottom() + 4.0),
        Align2::LEFT_TOP,
        "0",
        font.clone(),
        LABEL,
    );
    painter.text(
        Pos2::new(plot.right(), plot.bottom() + 4.0),
        Align2::RIGHT_TOP,
        total.to_string(),
        font,
        LABEL,
    );
}

/// 当前手指示：琥珀色竖线贯穿曲线区；该手有数据时再画大圆点。
fn draw_cursor(
    painter: &Painter,
    plot: Rect,
    x: &impl Fn(f32) -> f32,
    y: &impl Fn(f32) -> f32,
    cursor: usize,
    known: &[HistoryPoint],
) {
    let cx = x(cursor as f32);
    painter.line_segment(
        [Pos2::new(cx, plot.top()), Pos2::new(cx, plot.bottom())],
        Stroke::new(1.5, CURSOR),
    );
    if let Some(p) = known.iter().find(|p| p.turn == cursor) {
        let center = Pos2::new(cx, y(p.winrate as f32));
        painter.circle_filled(center, CURSOR_RADIUS, CURSOR);
        painter.circle_stroke(
            center,
            CURSOR_RADIUS + 1.5,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 200)),
        );
    }
}

/// 悬停反馈：磁吸到最近已知手数，画淡竖线 + 白环高亮，并弹原生指针提示。
/// 按值收 `Response`：`on_hover_ui_at_pointer` 需要 ownership，
/// 且此后调用方不再使用它。
fn draw_hover(
    painter: &Painter,
    plot: Rect,
    x: &impl Fn(f32) -> f32,
    y: &impl Fn(f32) -> f32,
    total: usize,
    response: Response,
    known: &[HistoryPoint],
) {
    let Some(pos) = response.hover_pos().filter(|p| plot.contains(*p)) else {
        return;
    };
    // 指针所在手数（四舍五入夹回区间），再磁吸到最近已知点。
    let total_f = total as f32;
    let hovered =
        ((pos.x - plot.left()) / plot.width() * total_f).round().clamp(0.0, total_f) as usize;
    let Some(p) = known.iter().copied().min_by_key(|p| p.turn.abs_diff(hovered)) else {
        return;
    };
    let hx = x(p.turn as f32);
    painter.line_segment(
        [Pos2::new(hx, plot.top()), Pos2::new(hx, plot.bottom())],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 60)),
    );
    painter.circle_stroke(
        Pos2::new(hx, y(p.winrate as f32)),
        CURSOR_RADIUS + 2.0,
        Stroke::new(1.5, Color32::WHITE),
    );
    response.on_hover_ui_at_pointer(|ui| {
        ui.weak(format!("最近已知：第 {} 手", p.turn));
        ui.label(format!("黑方胜率 {:.1}%", p.winrate * 100.0));
        ui.label(format!("目差 {:+.1}（黑方视角）", p.score_lead));
        ui.label(format!("visits {}", p.visits));
    });
}
