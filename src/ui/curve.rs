//! 胜率曲线（TASKS 4.3 / 4.4）：底部面板逐手黑方胜率折线、当前手指示、
//! 悬停数值与失误联动，并可叠加同横轴的**目差折线**（右侧目差刻度）。
//!
//! 数据经 [`AnalysisState::line_points`] 只取**当前线**（根到当前节点沿
//! 选中子分支）上的历史点——历史缓冲按局面签名存储，同手数的不同分支
//! 各存各的，切换分支不会冲掉另一条线的数据，切回来曲线即恢复：
//!
//! - 只画已知点：仅相邻两手（手数差 1）都有数据才连线，缺口留空，
//!   不臆造未知走势；已知点用小圆点标记，颜色复用 [`overlay::winrate_color`]
//!   （与棋盘候选点同一胜率映射）；
//! - 失误联动：损失达到疑问手及以上的手数，曲线点改用
//!   [`overlay::severity_color`] 并稍加大，一眼看出曲线在哪一段跳水；
//! - 胜率一律**黑方视角**（`reportAnalysisWinratesAs = BLACK`），不翻转，
//!   标题与提示文案均注明；
//! - 目差折线（可开关）：与胜率线同一数据源（`HistoryPoint::score_lead`，
//!   黑方视角），**虚线 + 暖色**与胜率实线肉眼可分；纵轴按**对称于 0 的
//!   目差刻度**独立换算（见 [`score_scale`]），右侧标注刻度值，左侧仍是
//!   胜率 0–100%；
//! - 当前手（[`Board::cursor`]）用琥珀色竖线 + 大圆点指示，
//!   该手尚无数据时仍显示竖线位置；
//! - 悬停时磁吸高亮最近已知手数（竖线 + 白环），并用 egui 原生指针提示
//!   显示手数 / 胜率 / 目差 / visits / 该手损失；离开曲线区全部消失。

use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::board::Board;

use super::analysis::{loss_from_points, AnalysisState, HistoryPoint, MoveLoss};
use super::overlay;

/// 曲线区四周留白：轴标注与标题占用（右侧加宽给目差刻度）。
const PAD_LEFT: f32 = 42.0;
const PAD_RIGHT: f32 = 44.0;
const PAD_TOP: f32 = 24.0;
const PAD_BOTTOM: f32 = 16.0;

/// 已知点标记半径 / 当前手指示半径。
const DOT_RADIUS: f32 = 2.5;
const CURSOR_RADIUS: f32 = 4.5;

/// 折线、当前手（与棋盘定位高亮同一琥珀色）、轴线与文字配色。
const LINE: Color32 = Color32::from_rgb(120, 200, 255);
/// 目差折线：偏红橙的暖色 + 虚线，与胜率实线（浅蓝冷色）肉眼可分；
/// 刻意避开失误标注的橙（255,152,82）与恶手红，避免两套语义混色。
const SCORE_LEAD_LINE: Color32 = Color32::from_rgb(255, 122, 77);
const CURSOR: Color32 = Color32::from_rgb(255, 170, 40);
const AXIS: Color32 = Color32::from_rgb(120, 120, 128);
const GRID: Color32 = Color32::from_rgb(60, 62, 70);
const LABEL: Color32 = Color32::from_rgb(160, 160, 168);

/// 目差刻度的**最小跨度**（目，半幅）：可见数据 |目差| 全部小于它时仍用
/// 它做刻度，避免开局 ±零点几目的数据被放大成满幅噪声，一格多少目始终
/// 有稳定概念。
pub(crate) const SCORE_SPAN_MIN: f64 = 2.0;

/// 目差虚线的实段 / 空段长度（逻辑像素）。
const DASH_LEN: f32 = 6.0;
const DASH_GAP: f32 = 4.0;

/// 目差刻度：由可见数据计算**对称于 0** 的刻度半幅。
///
/// `span = max(|目差|).max(SCORE_SPAN_MIN)`，再向上取整到「好看数」
/// （1 / 2 / 5 × 10^k）——刻度值必须标在右侧，±7.3 这类刻度不可读；
/// 取整只会**放大**跨度，数据永不因取整出界。返回值恒 > 0。
/// 目差 y 换算：`y = plot.center().y - v / span * plot.height() / 2`
/// （正目差 = 黑优 = 上方，与胜率「高 = 黑优」方向一致）。
pub fn score_scale(points: &[HistoryPoint]) -> f64 {
    let raw = points
        .iter()
        .map(|p| p.score_lead.abs())
        .fold(SCORE_SPAN_MIN, f64::max);
    nice_ceiling(raw)
}

/// 把值向上取整到 1 / 2 / 5 × 10^k（刻度好看数）。
fn nice_ceiling(v: f64) -> f64 {
    let exp = v.abs().log10().floor();
    let base = 10f64.powf(exp);
    for m in [1.0, 2.0, 5.0, 10.0] {
        let nice = m * base;
        if v <= nice {
            return nice;
        }
    }
    10.0 * base
}

/// 目差值 → 曲线区内的像素 y（正值 = 黑优 = 上方；出界值夹回边线，
/// 与胜率 y 的 clamp 同一纪律——超范围的数据不撕裂坐标系）。
pub fn score_y(plot: Rect, v: f64, span: f64) -> f32 {
    let half = plot.height() / 2.0;
    let t = (v / span).clamp(-1.0, 1.0) as f32;
    plot.center().y - t * half
}

/// 刻度值的显示小数位：好看数（1/2/5×10^k）在 <1 时需要小数（如 0.5），
/// ≥1 时整数足够（10 / 20 / 5 目没有半目刻度）。
fn nice_decimals(span: f64) -> usize {
    if span < 1.0 {
        1
    } else {
        0
    }
}

/// 绘制胜率曲线面板内容。外层底部面板由 `app` 条件创建：
/// 面板隐藏时不进入本函数，零额外计算。`show_score_lead` 为目差折线
/// 开关（关闭时不计算刻度、零绘制开销）。
pub fn show(ui: &mut Ui, analysis: &AnalysisState, board: &Board, show_score_lead: bool) {
    // 横轴取**当前线**长度（棋谱树里变着分支各成一条线，与棋盘显示口径一致）。
    let total = board.line_len();
    // 当前线上的历史点（下标 = 手数，0 = 初始空盘）：历史缓冲按局面签名
    // 存储，这里只取当前线各局面各自的数据，同手数的分支互不掺混。
    let slots = analysis.line_points(board);
    // 已知点（按手数升序）。
    let known: Vec<HistoryPoint> = slots.iter().filter_map(|p| *p).collect();
    // 每手损失（与 known 按下标平行；第 0 手或两端数据不全为 `None`），
    // 曲线着色与悬停提示共用，避免同一手重复派生。
    let losses: Vec<Option<MoveLoss>> = known
        .iter()
        .map(|p| {
            match (p.turn.checked_sub(1).and_then(|i| slots[i]), slots[p.turn]) {
                (Some(before), Some(after)) => board
                    .record_at(p.turn - 1)
                    .and_then(|r| loss_from_points(p.turn, r.player, before, after)),
                _ => None,
            }
        })
        .collect();

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

    // 目差刻度（对称于 0 的半幅）：只在开关开启时计算与绘制。
    let span = show_score_lead.then(|| score_scale(&known));

    draw_axes(&painter, plot, total, span);
    painter.text(
        Pos2::new(plot.left(), avail.min.y + 5.0),
        Align2::LEFT_TOP,
        "黑方胜率",
        FontId::proportional(12.0),
        LABEL,
    );
    // 目差线标题：与折线同色，一眼对应（开关关闭时不占位）。
    if show_score_lead {
        painter.text(
            Pos2::new(plot.center().x, avail.min.y + 5.0),
            Align2::CENTER_TOP,
            "目差（虚线）",
            FontId::proportional(12.0),
            SCORE_LEAD_LINE,
        );
    }

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

    // 目差折线（虚线）：画在胜率线之后、点位标记之前，与胜率线同层。
    // 刻度 span 恒 > 0（score_scale 保证），除法安全。
    if let Some(span) = span {
        let sy = |v: f64| score_y(plot, v, span);
        for pair in known.windows(2) {
            let (t0, t1) = (pair[0].turn, pair[1].turn);
            if t1 - t0 == 1 {
                draw_dashed(
                    &painter,
                    [
                        Pos2::new(x(t0 as f32), sy(pair[0].score_lead)),
                        Pos2::new(x(t1 as f32), sy(pair[1].score_lead)),
                    ],
                    Stroke::new(1.6, SCORE_LEAD_LINE),
                );
            }
        }
        // 目差已知点：小空心方点（与胜率实心圆点区分），同色系。
        for p in &known {
            let center = Pos2::new(x(p.turn as f32), sy(p.score_lead));
            painter.rect_stroke(
                Rect::from_center_size(center, Vec2::splat(DOT_RADIUS * 1.8)),
                1.0,
                Stroke::new(1.2, SCORE_LEAD_LINE),
                StrokeKind::Middle,
            );
        }
    }
    // 已知点标记：失误手（疑问手及以上）用严重程度色并稍加大，
    // 让「曲线跳水段」一眼可辨；其余用胜率色。
    for (p, loss) in known.iter().zip(&losses) {
        let center = Pos2::new(x(p.turn as f32), y(p.winrate as f32));
        match loss.filter(|l| l.severity.is_marked()) {
            Some(l) => {
                let radius = DOT_RADIUS + 1.5;
                painter.circle_filled(center, radius, overlay::severity_color(l.severity));
                painter.circle_stroke(
                    center,
                    radius,
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(20, 20, 20, 150)),
                );
            }
            None => {
                painter.circle_filled(center, DOT_RADIUS, overlay::winrate_color(p.winrate));
            }
        }
    }

    draw_cursor(&painter, plot, &x, &y, board.cursor(), &known);
    draw_hover(&painter, plot, &x, &y, total, response, &known, &losses);
}

/// 虚线线段：把整段按「实 DASH_LEN + 空 DASH_GAP」交替切成小段绘制。
/// 端点方向由线段向量归一化给出；长度不足一个实段时退化为实线（不画空）。
fn draw_dashed(painter: &Painter, [a, b]: [Pos2; 2], stroke: Stroke) {
    let vec = b - a;
    let len = vec.length();
    if len <= DASH_LEN {
        painter.line_segment([a, b], stroke);
        return;
    }
    let dir = vec / len;
    let mut s = 0.0;
    while s < len {
        let e = (s + DASH_LEN).min(len);
        painter.line_segment([a + dir * s, a + dir * e], stroke);
        s = e + DASH_GAP;
    }
}

/// 坐标系：底板、边框、Y 轴 0/50/100% 标注、50% 虚线参考线、X 轴首末手数。
/// `span` 为目差刻度半幅（`Some` 时在右侧对称标目差刻度：+span / 0 / −span，
/// 中间再插 ±span/2 两档——档数固定避免随数据抖动）。
fn draw_axes(painter: &Painter, plot: Rect, total: usize, span: Option<f64>) {
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
    // 右侧目差刻度（对称于 0）：正 = 黑优在上，与胜率高低方向一致；
    // 刻度值永远显式标出（否则读者不知道一格多少目）。
    if let Some(span) = span {
        let font = FontId::proportional(10.0);
        for (v, align) in [
            (span, Align2::LEFT_BOTTOM),
            (span / 2.0, Align2::LEFT_CENTER),
            (0.0, Align2::LEFT_CENTER),
            (-span / 2.0, Align2::LEFT_CENTER),
            (-span, Align2::LEFT_TOP),
        ] {
            // ±span 档贴在框线内侧半行高处，避免文字下半截被裁。
            let y = match align {
                Align2::LEFT_BOTTOM => score_y(plot, v, span) + 5.0,
                Align2::LEFT_TOP => score_y(plot, v, span) - 5.0,
                _ => score_y(plot, v, span),
            };
            let text = if v == 0.0 {
                "0".to_owned()
            } else {
                format!("{:+.*}", nice_decimals(span), v)
            };
            painter.text(
                Pos2::new(plot.right() + 5.0, y),
                align,
                text,
                font.clone(),
                SCORE_LEAD_LINE,
            );
        }
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
/// 且此后调用方不再使用它。`losses` 与 `known` 按下标平行（每手损失）。
#[allow(clippy::too_many_arguments)]
fn draw_hover(
    painter: &Painter,
    plot: Rect,
    x: &impl Fn(f32) -> f32,
    y: &impl Fn(f32) -> f32,
    total: usize,
    response: Response,
    known: &[HistoryPoint],
    losses: &[Option<MoveLoss>],
) {
    let Some(pos) = response.hover_pos().filter(|p| plot.contains(*p)) else {
        return;
    };
    // 指针所在手数（四舍五入夹回区间），再磁吸到最近已知点。
    let total_f = total as f32;
    let hovered =
        ((pos.x - plot.left()) / plot.width() * total_f).round().clamp(0.0, total_f) as usize;
    let Some((index, p)) = known
        .iter()
        .enumerate()
        .min_by_key(|(_, p)| p.turn.abs_diff(hovered))
    else {
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
    // 该手损失（第 0 手或数据不全为 `None`，不显示）。
    let loss_text = losses[index].map(|l| {
        format!("第 {} 手损失 {:+.1} 目 · {:+.0}%（{}）",
            l.turn, l.score_loss, l.winrate_loss * 100.0, l.severity.name())
    });
    response.on_hover_ui_at_pointer(|ui| {
        ui.weak(format!("最近已知：第 {} 手", p.turn));
        ui.label(format!("黑方胜率 {:.1}%", p.winrate * 100.0));
        ui.label(format!("目差 {:+.1}（黑方视角）", p.score_lead));
        ui.label(format!("visits {}", p.visits));
        if let Some(text) = loss_text {
            ui.label(text);
        }
    });
}
