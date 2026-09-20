//! 棋盘视图：`egui::Painter` 自绘棋盘与鼠标 / 键盘交互（TASKS 2.2 / 2.3），
//! 并按开关挂载分析叠加层（候选点 / 热度图 / 定位高亮，TASKS 4.1 / 4.2）、
//! 失误标注（TASKS 4.4）与分支选择器（变着分支切换）。
//!
//! 设计要点：
//!
//! - 屏幕坐标与交叉点的换算集中在 [`Layout`]，其余代码只跟 [`Coord`] 打交道；
//! - 坐标注释的列字母与行号取自 [`Coord::to_gtp`] 的输出，
//!   与 GTP 口径（列字母跳过 `I`、行号自下向上）由同一份实现保证一致；
//! - [`show`] 收 `&mut Board`：落子 / 导航 / 悔棋 / 切换分支都会修改棋盘状态，
//!   各绘制函数只借用 `&Board` 顺带读取；
//! - 非法落子原因写入 `notice`、建分支提示写入 `branch_notice`
//!   （均由调用方持有），跨帧显示直到下一次成功操作。

use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use crate::board::{Action, Board, Coord, IllegalReason, Size, Stone};

use super::analysis::AnalysisState;
use super::overlay::{self, Overlay};

// ---- 配色 ----

/// 棋盘木底色。
const WOOD: Color32 = Color32::from_rgb(211, 168, 92);
/// 木底描边（先画大一圈的深色层，再盖内层形成边框）。
const WOOD_EDGE: Color32 = Color32::from_rgb(118, 88, 44);
/// 网格线与星位。
const LINE: Color32 = Color32::from_rgb(70, 49, 25);
/// 坐标注释文字。
const LABEL: Color32 = Color32::from_rgb(52, 36, 17);
/// 非法落子提示。
const NOTICE: Color32 = Color32::from_rgb(255, 152, 82);
/// 分支选择器文字与「已创建变着」提示（同一琥珀色系）。
const BRANCH: Color32 = Color32::from_rgb(255, 170, 40);

/// 交叉点到棋盘边缘的留白，以间距为单位（容纳坐标标注）。
const MARGIN_IN_SPACING: f32 = 1.2;
/// 间距上限：大窗口下避免小棋盘被拉得过于稀疏。
const MAX_SPACING: f32 = 40.0;
/// 底部状态行预留高度。
const STATUS_HEIGHT: f32 = 26.0;
/// 分支选择器行预留高度（0 = 不显示时不占位）。
const BRANCH_HEIGHT: f32 = 26.0;

/// 绘制棋盘视图并处理交互。
///
/// `notice` / `branch_notice` 由调用方持有：非法落子时写入前者、
/// 回看中新建变着分支时写入后者，任何一次成功操作后清除，
/// 使提示能跨帧稳定显示。`analysis` 提供当前局面快照（候选点 / 热度图）
/// 与当前线历史（失误标注）；`overlay` 持有层开关与侧栏定位状态。
pub fn show(
    ui: &mut Ui,
    board: &mut Board,
    notice: &mut Option<IllegalReason>,
    branch_notice: &mut Option<String>,
    analysis: &AnalysisState,
    overlay: &Overlay,
) {
    handle_keyboard(ui, board, notice, branch_notice);

    // 分支点时在棋盘上方给一行选择器（按钮动作在落子处理前执行，
    // 因为选择器只读棋盘、落子要可变借用，二者借用不冲突）；
    // 非分支点该行不创建，不留占位。
    let mut branch_sel = BranchSel::default();
    if board.is_branch_point() {
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), BRANCH_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                branch_sel = draw_branch_selector(ui, board);
            },
        );
    }

    // 状态行固定在底部，其余空间全部给棋盘。
    let avail = ui.available_rect_before_wrap();
    let board_area = Rect::from_min_max(avail.min, avail.max - Vec2::new(0.0, STATUS_HEIGHT));
    let response = ui.allocate_rect(board_area, Sense::click());

    let size = board.size();
    if let Some(layout) = Layout::fit(response.rect, size) {
        let snapshot = analysis.snapshot.as_ref();
        let painter = ui.painter_at(layout.rect.expand(2.0));
        draw_board(&painter, &layout);
        draw_grid(&painter, &layout, size);
        draw_stars(&painter, &layout, size);
        draw_coordinates(&painter, &layout, size);
        // 热度图压在棋子之下：只染交叉点格块，棋子保持清晰。
        if overlay.show_heat
            && let Some(snapshot) = snapshot
        {
            overlay::draw_heat(&painter, &layout, snapshot);
        }
        draw_stones(&painter, &layout, board);
        draw_last_move_mark(&painter, &layout, board);
        // 失误标注画在棋子之上：小色点不遮棋子辨识。
        if overlay.show_mistakes {
            overlay::draw_mistakes(&painter, &layout, board, analysis);
        }
        // 候选点与定位高亮画在棋子之上。
        if overlay.show_candidates
            && let Some(snapshot) = snapshot
        {
            overlay::draw_candidates(&painter, &layout, snapshot);
        }
        if let Some(focus) = &overlay.focus {
            overlay::draw_focus(&painter, &layout, board, focus);
        }
        if let Some(pos) = response.hover_pos() {
            draw_hover(&painter, &layout, board, pos);
        }
        if response.clicked() {
            handle_click(board, notice, branch_notice, &layout, response.interact_pointer_pos());
        }
    }

    // 选择器动作在落子之后统一执行：二者都改棋盘，避免借用在先。
    match branch_sel {
        BranchSel::Select(i) => {
            if board.select_child(i) {
                *branch_notice = None;
            }
        }
        BranchSel::Step(step) => {
            let moved = match step {
                BranchStep::Next => board.next_branch(),
                BranchStep::Prev => board.prev_branch(),
            };
            if moved {
                *branch_notice = None;
            }
        }
        BranchSel::None => {}
    }

    draw_status(ui, board, *notice, branch_notice.as_deref());
}

/// 分支选择器的一帧交互结果（绘制期间收集，绘制后统一执行）。
#[derive(Default)]
enum BranchSel {
    /// 无操作。
    #[default]
    None,
    /// 点击了第 `i` 个子分支。
    Select(usize),
    /// 点击了上 / 下一条分支按钮。
    Step(BranchStep),
}

/// 相邻分支切换方向。
enum BranchStep {
    Next,
    Prev,
}

/// 绘制分支选择器行：「变着 X/Y」+ ◀ + 各子分支首手（GTP 坐标）+ ▶。
/// 返回本帧被点击的动作（由调用方执行，见 [`BranchSel`]）。
fn draw_branch_selector(ui: &mut Ui, board: &Board) -> BranchSel {
    let mut action = BranchSel::None;
    let size = board.size();
    let count = board.child_count();
    let selected = board.selected_child();
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("变着 {}/{}", selected + 1, count)).color(BRANCH).strong());
        if ui.button("◀").clicked() {
            action = BranchSel::Step(BranchStep::Prev);
        }
        for i in 0..count {
            // 子分支首手：GTP 坐标显示（Display 是调试格式，不用）；弃着显示文字。
            let label = match board.child_move(i) {
                Some(record) => match record.action {
                    Action::Place(c) => c.to_gtp(size),
                    Action::Pass => "弃着".to_owned(),
                },
                None => "?".to_owned(),
            };
            if ui.selectable_label(i == selected, label).clicked() {
                action = BranchSel::Select(i);
            }
        }
        if ui.button("▶").clicked() {
            action = BranchSel::Step(BranchStep::Next);
        }
        ui.weak("Ctrl+← / Ctrl+→ 切换");
    });
    action
}

// ---- 交互 ----

/// ← 后退一手，→ 前进一手，Ctrl+Z 悔棋；
/// Ctrl+← / Ctrl+→ 在相邻子分支间切换（不占用 ←/→ 的前进后退）；
/// 任何一次成功的导航都清除非法落子与建分支两类提示。
fn handle_keyboard(
    ui: &Ui,
    board: &mut Board,
    notice: &mut Option<IllegalReason>,
    branch_notice: &mut Option<String>,
) {
    let (back, forward, undo, prev, next) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowLeft) && !i.modifiers.ctrl,
            i.key_pressed(egui::Key::ArrowRight) && !i.modifiers.ctrl,
            i.key_pressed(egui::Key::Z) && i.modifiers.ctrl,
            i.key_pressed(egui::Key::ArrowLeft) && i.modifiers.ctrl,
            i.key_pressed(egui::Key::ArrowRight) && i.modifiers.ctrl,
        )
    });
    let moved = if back {
        board.step_back()
    } else if forward {
        board.step_forward()
    } else if undo {
        board.undo().is_some()
    } else if prev {
        board.prev_branch()
    } else if next {
        board.next_branch()
    } else {
        false
    };
    if moved {
        *notice = None;
        *branch_notice = None;
    }
}

/// 点击交叉点落子；非法时记录原因供状态行显示。
///
/// 回看中（非叶节点）落子不再被拒绝，而是**新建变着分支**：
/// 建了新分支（全树节点增加）时给轻提示；落到已有分支（直接切换过去）
/// 或叶子上续棋则不打扰。
fn handle_click(
    board: &mut Board,
    notice: &mut Option<IllegalReason>,
    branch_notice: &mut Option<String>,
    layout: &Layout,
    pos: Option<Pos2>,
) {
    let Some(pos) = pos else { return };
    let Some(at) = layout.hit_test(pos) else { return };
    let nodes_before = board.move_count();
    match board.play(at) {
        Ok(()) => {
            *notice = None;
            // 全树着法数增加 = 这次落子新建了分支（而非切进已有分支）。
            if board.move_count() > nodes_before {
                *branch_notice = Some(format!("已在第 {} 手创建变着", board.cursor() - 1));
            } else {
                *branch_notice = None;
            }
        }
        Err(reason) => *notice = Some(reason),
    }
}

// ---- 几何 ----

/// 棋盘几何：屏幕坐标与交叉点的唯一换算点。
/// `point` / `spacing` 供叠加层（`overlay` 模块）复用，其余仅供本模块。
pub(crate) struct Layout {
    size: Size,
    /// 交叉点 (0, 0) 的屏幕坐标。
    origin: Pos2,
    /// 相邻交叉点间距。
    pub(crate) spacing: f32,
    /// 交叉点区域到棋盘边缘的留白。
    margin: f32,
    /// 棋盘整体（含留白）所占的区域。
    rect: Rect,
}

impl Layout {
    /// 在 `avail` 内居中放置棋盘；空间过小时返回 `None`（不绘制，不 panic）。
    fn fit(avail: Rect, size: Size) -> Option<Self> {
        let side = avail.width().min(avail.height());
        if !side.is_finite() || side <= 8.0 {
            return None;
        }
        // n - 1 个间距 + 两侧留白，恰好铺满可用边长的正方形。
        let spacing = (side / (f32::from(size.n()) - 1.0 + 2.0 * MARGIN_IN_SPACING))
            .min(MAX_SPACING);
        let margin = spacing * MARGIN_IN_SPACING;
        let rect = Rect::from_center_size(
            avail.center(),
            Vec2::splat(spacing * (f32::from(size.n()) - 1.0) + 2.0 * margin),
        );
        Some(Self {
            size,
            origin: rect.min + Vec2::splat(margin),
            spacing,
            margin,
            rect,
        })
    }

    /// 交叉点的屏幕坐标。
    pub(crate) fn point(&self, c: Coord) -> Pos2 {
        self.origin + Vec2::new(f32::from(c.x()) * self.spacing, f32::from(c.y()) * self.spacing)
    }

    /// 屏幕坐标命中的交叉点：就近取整到最近交叉点，
    /// 距边缘交叉点超过半格（留白深处）视为未命中。
    fn hit_test(&self, pos: Pos2) -> Option<Coord> {
        let fx = (pos.x - self.origin.x) / self.spacing;
        let fy = (pos.y - self.origin.y) / self.spacing;
        let (x, y) = (fx.round(), fy.round());
        let n = f32::from(self.size.n());
        if !(0.0..n).contains(&x) || !(0.0..n).contains(&y) {
            return None;
        }
        Some(coord(self.size, x as u8, y as u8))
    }
}

/// 盘内坐标构造（0 <= x, y < n 时必然成功）。
fn coord(size: Size, x: u8, y: u8) -> Coord {
    Coord::new(size, x, y).expect("盘内坐标构造必然成功")
}

// ---- 绘制 ----

fn draw_board(painter: &Painter, layout: &Layout) {
    painter.rect_filled(layout.rect.expand(2.0), 5.0, WOOD_EDGE);
    painter.rect_filled(layout.rect, 4.0, WOOD);
}

fn draw_grid(painter: &Painter, layout: &Layout, size: Size) {
    let n = size.n();
    let width = (layout.spacing * 0.05).max(1.0);
    for i in 0..n {
        painter.line_segment(
            [layout.point(coord(size, 0, i)), layout.point(coord(size, n - 1, i))],
            Stroke::new(width, LINE),
        );
        painter.line_segment(
            [layout.point(coord(size, i, 0)), layout.point(coord(size, i, n - 1))],
            Stroke::new(width, LINE),
        );
    }
    // 边线加粗，贴近实物棋盘观感。
    let stroke = Stroke::new(width * 2.0, LINE);
    let a = layout.point(coord(size, 0, 0));
    let b = layout.point(coord(size, n - 1, n - 1));
    painter.line_segment([Pos2::new(a.x, a.y), Pos2::new(b.x, a.y)], stroke);
    painter.line_segment([Pos2::new(a.x, b.y), Pos2::new(b.x, b.y)], stroke);
    painter.line_segment([Pos2::new(a.x, a.y), Pos2::new(a.x, b.y)], stroke);
    painter.line_segment([Pos2::new(b.x, a.y), Pos2::new(b.x, b.y)], stroke);
}

/// 星位（内部坐标）：9 / 13 路各五处（四角星 + 天元），
/// 19 路按标准棋盘为九处（四角星 + 四边星 + 天元）。
fn star_points(n: u8) -> &'static [(u8, u8)] {
    match n {
        9 => &[(2, 2), (6, 2), (4, 4), (2, 6), (6, 6)],
        13 => &[(3, 3), (9, 3), (6, 6), (3, 9), (9, 9)],
        19 => &[
            (3, 3),
            (9, 3),
            (15, 3),
            (3, 9),
            (9, 9),
            (15, 9),
            (3, 15),
            (9, 15),
            (15, 15),
        ],
        _ => &[],
    }
}

fn draw_stars(painter: &Painter, layout: &Layout, size: Size) {
    let radius = (layout.spacing * 0.11).max(1.5);
    for &(x, y) in star_points(size.n()) {
        painter.circle_filled(layout.point(coord(size, x, y)), radius, LINE);
    }
}

/// 坐标注释：列字母在棋盘下方，行号在左侧。
/// 字母与行号一律取自 [`Coord::to_gtp`] 的输出，与 GTP 口径共用同一实现。
fn draw_coordinates(painter: &Painter, layout: &Layout, size: Size) {
    if layout.spacing < 14.0 {
        return; // 空间过小时标注会互相重叠，直接省略
    }
    let n = size.n();
    let font = FontId::proportional((layout.spacing * 0.36).clamp(9.0, 15.0));
    let offset = layout.margin * 0.55;
    for i in 0..n {
        // 列字母：GTP 坐标的首字符（A..H 之后跳过 I 接 J..T）。
        let letter = coord(size, i, 0).to_gtp(size);
        let bottom = layout.point(coord(size, i, n - 1));
        painter.text(
            Pos2::new(bottom.x, bottom.y + offset),
            Align2::CENTER_CENTER,
            &letter[..1],
            font.clone(),
            LABEL,
        );
        // 行号：GTP 坐标去掉列字母后的数字（自下向上 1..n）。
        let number = coord(size, 0, i).to_gtp(size);
        let left = layout.point(coord(size, 0, i));
        painter.text(
            Pos2::new(left.x - offset, left.y),
            Align2::CENTER_CENTER,
            &number[1..],
            font.clone(),
            LABEL,
        );
    }
}

fn draw_stones(painter: &Painter, layout: &Layout, board: &Board) {
    let size = board.size();
    let radius = layout.spacing * 0.47;
    for (i, stone) in board.grid().iter().enumerate() {
        let Some(stone) = *stone else { continue };
        let Some(c) = Coord::from_index(size, i) else { continue };
        draw_stone(painter, layout.point(c), radius, stone);
    }
}

/// 单颗棋子：落影 + 明暗两层，做出基础立体感（不引入图片资源）。
fn draw_stone(painter: &Painter, center: Pos2, radius: f32, stone: Stone) {
    painter.circle_filled(
        center + Vec2::new(radius * 0.10, radius * 0.16),
        radius,
        Color32::from_rgba_unmultiplied(0, 0, 0, 64),
    );
    let (base, edge, sheen, shade) = match stone {
        Stone::Black => (
            Color32::from_rgb(26, 26, 28),
            Color32::BLACK,
            Color32::from_rgba_unmultiplied(255, 255, 255, 34),
            None,
        ),
        Stone::White => (
            Color32::from_rgb(237, 235, 228),
            Color32::from_rgb(128, 126, 120),
            Color32::from_rgba_unmultiplied(255, 255, 255, 128),
            Some(Color32::from_rgba_unmultiplied(96, 92, 84, 34)),
        ),
    };
    painter.circle_filled(center, radius, base);
    if let Some(shade) = shade {
        // 白子的右下暗部（范围收在棋子内部，无需裁剪）。
        painter.circle_filled(
            center + Vec2::new(radius * 0.30, radius * 0.34),
            radius * 0.40,
            shade,
        );
    }
    painter.circle_filled(center - Vec2::new(radius * 0.28, radius * 0.32), radius * 0.32, sheen);
    painter.circle_stroke(center, radius, Stroke::new(1.0, edge));
}

/// 最后一手标记：在游标所指一手（若有落点）的棋子上画一圈。
fn draw_last_move_mark(painter: &Painter, layout: &Layout, board: &Board) {
    let Some(record) = board.cursor().checked_sub(1).and_then(|i| board.record_at(i)) else {
        return;
    };
    if record.is_pass() {
        return; // 弃着盘面不变，无处可标
    }
    let Action::Place(at) = record.action else { return };
    let color = match record.player {
        Stone::Black => Color32::from_rgba_unmultiplied(255, 255, 255, 220),
        Stone::White => Color32::from_rgba_unmultiplied(20, 20, 22, 220),
    };
    painter.circle_stroke(
        layout.point(at),
        layout.spacing * 0.47 * 0.55,
        Stroke::new((layout.spacing * 0.06).max(1.5), color),
    );
}

/// 悬停反馈：十字光标贯穿悬停点；空点再叠一层当前行棋方的幽灵子。
fn draw_hover(painter: &Painter, layout: &Layout, board: &Board, pos: Pos2) {
    let Some(at) = layout.hit_test(pos) else { return };
    let p = layout.point(at);
    let half = layout.spacing * 0.5;
    let cross = Stroke::new(1.0, Color32::from_rgba_unmultiplied(24, 16, 6, 110));
    painter.line_segment([Pos2::new(p.x - half, p.y), Pos2::new(p.x + half, p.y)], cross);
    painter.line_segment([Pos2::new(p.x, p.y - half), Pos2::new(p.x, p.y + half)], cross);
    match board.get(at) {
        Some(stone) => {
            // 已有棋子：环状高亮。
            let color = match stone {
                Stone::Black => Color32::from_rgba_unmultiplied(255, 255, 255, 190),
                Stone::White => Color32::from_rgba_unmultiplied(20, 20, 22, 190),
            };
            painter.circle_stroke(p, layout.spacing * 0.47 * 0.62, Stroke::new(1.5, color));
        }
        None => {
            let ghost = match board.to_play() {
                Stone::Black => Color32::from_rgba_unmultiplied(26, 26, 28, 72),
                Stone::White => Color32::from_rgba_unmultiplied(255, 255, 255, 130),
            };
            painter.circle_filled(p, layout.spacing * 0.47, ghost);
        }
    }
}

/// 最小状态行：手数 / 行棋方 / 提子 / 非法提示 / 建分支提示 / 快捷键说明
/// （完整侧栏在阶段 4）。
fn draw_status(ui: &mut Ui, board: &Board, notice: Option<IllegalReason>, branch_notice: Option<&str>) {
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("第 {} / {} 手", board.cursor(), board.line_len()));
        if board.cursor() < board.line_len() {
            ui.weak("（回看中）");
        }
        ui.separator();
        ui.label(format!("轮到{}", board.to_play().name()));
        ui.separator();
        ui.label(format!(
            "黑提 {} · 白提 {}",
            board.captured_by(Stone::Black),
            board.captured_by(Stone::White),
        ));
        if let Some(text) = branch_notice {
            ui.separator();
            ui.colored_label(BRANCH, text);
        }
        if let Some(reason) = notice {
            ui.separator();
            ui.colored_label(NOTICE, format!("非法落子：{reason}"));
        }
        ui.separator();
        ui.weak("点击落子 · ← 后退 · → 前进 · Ctrl+←/→ 切分支 · Ctrl+Z 悔棋");
    });
}
