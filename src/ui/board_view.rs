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

use egui::{
    Align2, Color32, FontId, Painter, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use crate::board::{Action, Board, Coord, IllegalReason, Size, Stone};
use crate::play::{PlayState, resign_text, undo_to_human};

use super::analysis::{AnalysisState, Region};
use super::overlay::{self, Overlay};
use super::theme;

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
/// 限定区域的填充与边框（青色系，与琥珀系交互色区分）。
const REGION_FILL: Color32 = Color32::from_rgba_premultiplied(80, 200, 220, 42);
const REGION_EDGE: Color32 = Color32::from_rgba_premultiplied(80, 200, 220, 200);
/// 被排除选点的标记色（红色系，示意「不走这里」）。
const AVOID_MARK: Color32 = Color32::from_rgba_premultiplied(235, 77, 61, 210);

/// 交叉点到棋盘边缘的留白，以间距为单位（容纳坐标标注）。
const MARGIN_IN_SPACING: f32 = 1.2;
/// 棋盘边长绝对上限（逻辑像素）：仅在「巨大屏幕 + 9 路小盘」这类极端
/// 组合下兜底，避免小棋盘被放大到荒诞；常见分辨率下 19 / 13 路碰不到
/// （2560 宽时 19 路满铺约 2350px，远低于此值），故棋盘实际上随窗口
/// 自由缩放（此前 40px 的间距上限会把 19 路盘卡在约 816px，大屏只占
/// 中间一小块，已移除）。
const MAX_BOARD_SIDE: f32 = 3200.0;
/// 底部状态栏预留高度（分段块 + 容器内边距）。
const STATUS_HEIGHT: f32 = 42.0;
/// 分支选择器行预留高度（0 = 不显示时不占位）。
const BRANCH_HEIGHT: f32 = 42.0;

/// 绘制棋盘视图并处理交互。
///
/// `notice` / `branch_notice` 由调用方持有：非法落子时写入前者、
/// 回看中新建变着分支时写入后者，任何一次成功操作后清除，
/// 使提示能跨帧稳定显示。`analysis` 提供当前局面快照（候选点 / 热度图）
/// 与当前线历史（失误标注）；`overlay` 持有层开关与侧栏定位状态。
/// `play` 为人机对弈状态（`None` = 尚未初始化，按纯复盘处理）；
/// 对弈模式开启时 Ctrl+Z 悔棋回到「轮到人类」的状态（见 [`undo_to_human`]）。
pub fn show(
    ui: &mut Ui,
    board: &mut Board,
    notice: &mut Option<IllegalReason>,
    branch_notice: &mut Option<String>,
    analysis: &mut AnalysisState,
    overlay: &Overlay,
    play: Option<&PlayState>,
) {
    handle_keyboard(ui, board, notice, branch_notice, play);

    // 限制状态整帧共用一份快照（状态栏标识与棋盘绘制同源）。
    let limits = analysis.limits().clone();

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
    // 限定区域开启时需要拖拽框选，因此升级为 click_and_drag 感知。
    let region_mode = analysis.limits().has_region();
    let sense = if region_mode { Sense::click_and_drag() } else { Sense::click() };
    let response = ui.allocate_rect(board_area, sense);

    let size = board.size();
    // 热度图数据源标识（棋盘块内赋值，块外供状态栏标注读取）。
    let mut heat_tag = None;
    if let Some(layout) = Layout::fit(response.rect, size) {
        let snapshot = analysis.snapshot.as_ref();
        let painter = ui.painter_at(layout.rect.expand(2.0));
        draw_board(&painter, &layout);
        draw_grid(&painter, &layout, size);
        draw_stars(&painter, &layout, size);
        draw_coordinates(&painter, &layout, size);
        // 热度图压在棋子之下：只染交叉点格块，棋子保持清晰。
        // 「候选点领地」开关开启时热度图可切换到聚焦候选点的「走后」
        // 领地（无聚焦回落当前局面）；实际用哪路数据由 draw_heat 内的
        // heat_source 判定，返回来源供状态栏 / 图例标注。
        if overlay.show_heat
            && let Some(snapshot) = snapshot
        {
            let focus = overlay
                .show_moves_heat
                .then_some(overlay.focus.as_ref())
                .flatten();
            heat_tag = overlay::draw_heat(&painter, &layout, snapshot, focus);
        }
        // 策略热度图同样压在棋子之下（棋子之上于热度图，同开时策略层
        // 的小圆片叠在 ownership 大方块上，两种数据都可见）。
        if overlay.show_policy
            && let Some(snapshot) = snapshot
        {
            overlay::draw_policy(&painter, &layout, board, snapshot);
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
        // 选点限制的可视化：区域遮罩 + 排除点标记（画在候选点之上更醒目）。
        if let Some(region) = limits.region {
            draw_region(&painter, &layout, region);
        }
        if !limits.avoid.is_empty() {
            draw_avoid_marks(&painter, &layout, &limits.avoid, board.to_play());
        }
        // 区域模式下拖拽框选进行中：以按下点为对角实时预览框。
        if region_mode
            && response.is_pointer_button_down_on()
        {
            let press = ui.input(|i| i.pointer.press_origin());
            if let (Some(start), Some(cur)) = (press, response.hover_pos())
                && let (Some(a), Some(b)) = (layout.hit_test(start), layout.hit_test(cur))
            {
                draw_region(&painter, &layout, Region::from_corners(size, a, b));
            }
        }
        if let Some(pos) = response.hover_pos() {
            draw_hover(&painter, &layout, board, pos);
        }
        // 交互分发：区域模式 = 框选（拖拽或点击），右键清区域；
        // 普通模式 = 左键落子，右键空点 = 排除 / 恢复该手（支路探查）。
        if region_mode {
            handle_region_input(ui, &response, &layout, size, analysis);
        } else {
            if response.secondary_clicked()
                && let Some(at) = response.interact_pointer_pos().and_then(|pos| layout.hit_test(pos))
                && board.get(at).is_none()
            {
                analysis.toggle_avoid(board.to_play(), at);
            }
            if response.clicked() {
                handle_click(board, notice, branch_notice, &layout, response.interact_pointer_pos());
            }
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

    // 热度图数据源标识（候选点走后 / 当前局面）：有内容时在状态栏
    // 显式标出，让「现在这层紫灰色画的是哪手的领地」永远可判读。
    if let Some(source) = heat_tag {
        draw_heat_source_tag(ui, board.size(), source);
    }

    draw_status(ui, board, *notice, branch_notice.as_deref(), play, &limits);
}

/// 热度图数据源标识行：棋盘上方小字（琥珀/紫色弱化），只在候选点层
/// 激活时显示「候选 X 走后」；回落当前局面时保持沉默（默认态无需解释，
/// 但候选点层有「走后」假想语义，必须显式标注防误读）。
fn draw_heat_source_tag(ui: &mut Ui, size: Size, source: overlay::HeatSource) {
    if let overlay::HeatSource::Candidate(at) = source {
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), 0.0),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                ui.label(
                    RichText::new(format!("热度图：候选 {} 走后（紫灰 = 假想领地）", at.to_gtp(size)))
                        .color(Color32::from_rgb(196, 168, 240))
                        .size(11.0),
                );
            },
        );
    }
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

/// 绘制分支选择器行：「变着 X/Y」标签 + ◀/▶ 小方钮 + 各子分支首手
/// 分段按钮（GTP 坐标，弃着显示文字）+ 快捷键说明。
/// 返回本帧被点击的动作（由调用方执行，见 [`BranchSel`]）。
fn draw_branch_selector(ui: &mut Ui, board: &Board) -> BranchSel {
    let mut action = BranchSel::None;
    let size = board.size();
    let count = board.child_count();
    let selected = board.selected_child();
    ui.horizontal(|ui| {
        // 「变着 X/Y」标签块：琥珀文字 + 暗底小圆角，与其余按钮区分。
        ui.label(
            RichText::new(format!(" 变着 {}/{} ", selected + 1, count))
                .color(BRANCH)
                .strong(),
        );
        // ◀ / ▶：固定宽度小方钮，视觉上是一组导航控件。
        let nav = |ui: &mut Ui, label: &str| -> bool {
            let width = ui.spacing().interact_size.y * 1.4;
            ui.add_sized([width, 0.0], egui::Button::new(label)).clicked()
        };
        if nav(ui, "◀") {
            action = BranchSel::Step(BranchStep::Prev);
        }
        // 子分支分段按钮：选中琥珀填充，未选可点；首手用等宽字体。
        for i in 0..count {
            // 子分支首手：GTP 坐标显示（Display 是调试格式，不用）；弃着显示文字。
            let label = match board.child_move(i) {
                Some(record) => match record.action {
                    Action::Place(c) => c.to_gtp(size),
                    Action::Pass => "弃着".to_owned(),
                },
                None => "?".to_owned(),
            };
            let mut button = egui::Button::selectable(i == selected, RichText::new(label).monospace());
            if i == selected {
                button = button
                    .fill(theme::colors::ACCENT_DIM)
                    .stroke(Stroke::new(1.0, theme::colors::ACCENT_BAR));
            }
            if ui.add(button).clicked() {
                action = BranchSel::Select(i);
            }
        }
        if nav(ui, "▶") {
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
///
/// 对弈模式开启时 Ctrl+Z 语义变化：回到人类该走的**上一个决策点**
/// （撤掉引擎应手 + 人类上一手，或仅撤人类刚落的一手，至多 2 手，
/// 见 [`undo_to_human`]）；复盘模式保持原单步语义。
/// 悔棋失败（无棋可悔）不弹窗——状态行无变化即反馈。
fn handle_keyboard(
    ui: &Ui,
    board: &mut Board,
    notice: &mut Option<IllegalReason>,
    branch_notice: &mut Option<String>,
    play: Option<&PlayState>,
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
    let play_undo = undo && play.is_some_and(|p| p.mode);
    let moved = if back {
        board.step_back()
    } else if forward {
        board.step_forward()
    } else if play_undo {
        // 对弈悔棋：撤到人类该走的状态（不可能时棋盘保持原状）。
        let human = play.map_or(Stone::Black, |p| p.human);
        undo_to_human(board, human)
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
/// 当前节点的子分支数增加（确实新建了兄弟分支）时给轻提示；落到已有
/// 分支（直接切换过去）或叶子上续棋则不打扰。
fn handle_click(
    board: &mut Board,
    notice: &mut Option<IllegalReason>,
    branch_notice: &mut Option<String>,
    layout: &Layout,
    pos: Option<Pos2>,
) {
    let Some(pos) = pos else { return };
    let Some(at) = layout.hit_test(pos) else { return };
    let children_before = board.child_count();
    let moves_before = board.move_count();
    match board.play(at) {
        Ok(()) => {
            *notice = None;
            // 「新建了变着分支」= 当前节点**确实多了一个兄弟分支**：
            // play 后全树节点数增加（排除「切进已有分支」），且落子前当前
            // 节点已有别的子（根 / 叶的第一个子是正常续棋，不是变着）。
            // 注意 play 成功后游标已移到新节点，child_count 只能落子前取。
            if board.move_count() > moves_before && children_before > 0 {
                *branch_notice = Some(format!("已在第 {} 手创建变着", board.cursor() - 1));
            } else {
                *branch_notice = None;
            }
        }
        Err(reason) => *notice = Some(reason),
    }
}

/// 限定区域的框选交互：拖拽松开成框（单点拖拽 = 单点区域），
/// 纯点击也成单点区域（点两下对角即可框矩形）。右键清除区域。
fn handle_region_input(
    ui: &Ui,
    response: &egui::Response,
    layout: &Layout,
    size: Size,
    analysis: &mut AnalysisState,
) {
    // 右键：清除当前区域（再点开关也可整体关闭）。
    if response.secondary_clicked() {
        analysis.set_region(None);
        return;
    }
    // 拖拽松开：以按下点与松开点为对角成框。press_origin 在 Release 事件
    // 处理完才清空，因此本帧仍能读到拖拽起点。
    if response.drag_stopped()
        && let Some(start) = ui.input(|i| i.pointer.press_origin())
        && let (Some(a), Some(b)) = (
            layout.hit_test(start),
            response.interact_pointer_pos().and_then(|pos| layout.hit_test(pos)),
        )
    {
        analysis.set_region(Some(Region::from_corners(size, a, b)));
        return;
    }
    // 纯点击（egui 已排除「明显拖拽」的释放）：单点区域，点两下对角即可框出矩形。
    if response.clicked()
        && let Some(at) = response.interact_pointer_pos().and_then(|pos| layout.hit_test(pos))
    {
        analysis.set_region(Some(Region::from_corners(size, at, at)));
    }
}

/// 限定区域可视化：整块半透明遮罩 + 青色描边 + 四角短杠。
fn draw_region(painter: &Painter, layout: &Layout, region: Region) {
    let a = layout.point(region.min);
    let b = layout.point(region.max);
    let rect = Rect::from_min_max(a, b).expand(layout.spacing * 0.5);
    painter.rect_filled(rect, 4.0, REGION_FILL);
    painter.rect_stroke(rect, 4.0, Stroke::new(2.0, REGION_EDGE), StrokeKind::Outside);
    // 四角短杠加强「这是一块被框定的区域」的观感。
    let arm = layout.spacing * 0.7;
    for (cx, cy, dx, dy) in [
        (rect.left(), rect.top(), 1.0, 1.0),
        (rect.right(), rect.top(), -1.0, 1.0),
        (rect.left(), rect.bottom(), 1.0, -1.0),
        (rect.right(), rect.bottom(), -1.0, -1.0),
    ] {
        painter.line_segment(
            [
                Pos2::new(cx + dx * arm, cy),
                Pos2::new(cx, cy + dy * arm),
            ],
            Stroke::new(2.5, REGION_EDGE),
        );
    }
}

/// 被排除选点标记：红圈 + 斜杠（「不走这里」），画在棋子之上。
/// 只标记**当前行棋方**名下的排除项（对手的排除项对当前候选无影响）。
fn draw_avoid_marks(painter: &Painter, layout: &Layout, avoid: &[(Stone, Coord)], to_play: Stone) {
    let radius = layout.spacing * 0.40;
    let stroke = Stroke::new((layout.spacing * 0.07).max(2.0), AVOID_MARK);
    for (_, c) in avoid.iter().filter(|(p, _)| *p == to_play) {
        let center = layout.point(*c);
        painter.circle_stroke(center, radius, stroke);
        let d = radius * std::f32::consts::FRAC_1_SQRT_2; // 45° 斜杠端点到圆心距离
        painter.line_segment(
            [center - Vec2::new(d, d), center + Vec2::new(d, d)],
            stroke,
        );
    }
}

// ---- 几何 ----/// 棋盘几何：屏幕坐标与交叉点的唯一换算点。
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
        // n - 1 个间距 + 两侧留白，恰好铺满可用边长的正方形；
        // 间距只受「棋盘边长绝对上限」约束（见 [`MAX_BOARD_SIDE`]），
        // 常见分辨率下棋盘随窗口自由缩放。
        let spacing = ((side / (f32::from(size.n()) - 1.0 + 2.0 * MARGIN_IN_SPACING))
            .min(MAX_BOARD_SIDE / (f32::from(size.n()) - 1.0 + 2.0 * MARGIN_IN_SPACING)))
        .max(1.0);
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

/// 最小状态栏：手数 / 行棋方 / 提子 / 对弈轮次 / 非法提示 / 建分支提示 /
/// 快捷键说明（完整侧栏在阶段 4）。
///
/// 对弈模式开启时行棋方一栏改为明确的「轮到你 / 引擎思考中…」提示
/// （含对局已结束的原因）；复盘模式保持原「轮到黑/白」文案。
///
/// 呈现为分段状态栏：底色容器内并排若干「标签块」（暗底圆角小片），
/// 用分隔线区分；不再是一整行终端式的裸文字。每段是否显示仍按数据
/// 有无决定（回看 / 弃着提示 / 非法提示为空则不占段）。
fn draw_status(
    ui: &mut Ui,
    board: &mut Board,
    notice: Option<IllegalReason>,
    branch_notice: Option<&str>,
    play: Option<&PlayState>,
    limits: &super::analysis::AnalysisLimits,
) {
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), STATUS_HEIGHT - 10.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            // 状态栏容器：暗底圆角条，与棋盘 / 面板底色分层。
            egui::Frame::default()
                .fill(theme::colors::STATUS_BG)
                .corner_radius(7.0)
                .inner_margin(egui::Margin::symmetric(6, 4))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.spacing_mut().item_spacing.y = 2.0;

                    // 段容器：每段一个暗底圆角块，块内横排文字。
                    let segment = |ui: &mut Ui, add: &dyn Fn(&mut Ui)| {
                        egui::Frame::default()
                            .fill(theme::colors::STATUS_SEG)
                            .corner_radius(5.0)
                            .inner_margin(egui::Margin::symmetric(6, 2))
                            .show(ui, |ui| add(ui));
                    };

                    // 段 1：手数（含回看标记）。
                    segment(ui, &|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "第 {} / {} 手",
                                    board.cursor(),
                                    board.line_len()
                                ))
                                .strong(),
                            );
                            if board.cursor() < board.line_len() {
                                ui.label(RichText::new("回看中").color(BRANCH).size(11.0));
                            }
                        });
                    });

                    // 段 2：行棋方 / 对弈轮次。
                    segment(ui, &|ui| match play {
                        Some(play) if play.mode => {
                            if play.finished(board) {
                                let text = match play.resigned {
                                    Some(side) => {
                                        format!("{}认输，{}", side.name(), resign_text(side))
                                    }
                                    None => "对局结束：双方连续弃着".to_owned(),
                                };
                                ui.colored_label(theme::colors::WARN, text);
                            } else if board.to_play() == play.human {
                                ui.label(
                                    RichText::new(format!(
                                        "轮到你（你执{}）",
                                        play.human.name()
                                    ))
                                    .color(theme::colors::OK)
                                    .strong(),
                                );
                            } else {
                                ui.colored_label(
                                    theme::colors::OK,
                                    format!("引擎思考中…（引擎执{}）", play.human.opposite().name()),
                                );
                            }
                        }
                        _ => {
                            ui.label(format!("轮到{}", board.to_play().name()));
                        }
                    });

                    // 段 3：提子。
                    segment(ui, &|ui| {
                        ui.label(RichText::new("提子").weak().size(11.0));
                        ui.label(format!(
                            "黑 {} · 白 {}",
                            board.captured_by(Stone::Black),
                            board.captured_by(Stone::White),
                        ));
                    });

                    // 段 4：建分支提示（有才显示）。
                    if let Some(text) = branch_notice {
                        segment(ui, &|ui| {
                            ui.colored_label(BRANCH, text);
                        });
                    }

                    // 段 5：选点限制标识（区域 / 排除，有任一才显示）。
                    // 用户必须随时知道引擎的候选是被约束过的，避免把
                    // 「限定下的首选」误读为全局最优。
                    if limits.region.is_some() || !limits.avoid.is_empty() {
                        segment(ui, &|ui| {
                            ui.colored_label(REGION_EDGE, "限定选点中");
                        });
                    }

                    // 段 6：非法落子提示（有才显示）。
                    if let Some(reason) = notice {
                        segment(ui, &|ui| {
                            ui.colored_label(NOTICE, format!("非法落子：{reason}"));
                        });
                    }

                    // 段 7：快捷键说明（弱色小字）。
                    ui.weak("点击落子 · ← 后退 · → 前进 · Ctrl+←/→ 切分支 · Ctrl+Z 悔棋");
                });
        },
    );
}
