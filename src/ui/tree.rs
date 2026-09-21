//! 棋谱树控件：可视化整棵对局树（含变着分支）并支持点击跳转到任意节点。
//!
//! - **布局**：主变沿列 0 自上而下（行 = 手数），每个变着从其分叉节点处
//!   另起一列，整棵变着子树留在该列内延伸；列号按下一条空闲列分配，
//!   因此任意两节点永不重叠（同行不同列 / 同列不同行）。
//! - **缓存**：布局按「树指纹」缓存（棋盘替换代数 + 盘面尺寸 + 全部节点的
//!   父子拓扑），树未变时跨帧复用，只在载谱 / 落子 / 悔棋等树变化时重算。
//!   指纹逐帧 O(节点数) 扫描但不分配内存，300+ 节点开销可忽略。
//! - **滚动**：纵向用 `ScrollArea::show_rows` 按行虚拟化——只对可见行的
//!   节点创建交互区，节点再多滚动条也不失真；横向独立滚动（列多时用）。
//! - **联动**：当前节点琥珀色高亮（与棋盘定位 / 曲线指示同色），游标变化
//!   时自动滚到可见区；有注释的节点带小绿点（注释按局面签名查询，
//!   签名算法与 `sgf::load` 的 `position_sig` 逐字节一致）。
//! - **跳转**：点击节点即 `go_to_node`；已在此节点上时不做无谓操作。

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::board::{Action, Board, MoveRecord, Stone};
use crate::sgf::GameMeta;

/// 节点尺寸与间距（行高 = NODE_H + ROW_GAP）。
const NODE_W: f32 = 92.0;
const NODE_H: f32 = 20.0;
const ROW_GAP: f32 = 2.0;
const COL_GAP: f32 = 12.0;

/// 当前节点高亮（与棋盘定位 / 曲线指示同一琥珀色）。
const CURSOR: Color32 = Color32::from_rgb(255, 170, 40);
/// 主变连线颜色。
const MAIN: Color32 = Color32::from_rgb(120, 200, 255);
/// 变着连线颜色。
const ALT: Color32 = Color32::from_rgb(150, 150, 158);
/// 节点文字颜色。
const LABEL: Color32 = Color32::from_rgb(205, 205, 210);
/// 注释标记（小圆点，与侧栏成功提示同色）。
const COMMENT_DOT: Color32 = Color32::from_rgb(140, 220, 140);

/// 一个节点在树视图中的位置（布局缓存的最小单元）。
#[derive(Clone, Copy)]
pub struct TreeNode {
    /// 节点 id（即 `Board::nodes` 下标，点击跳转的目标）。
    pub id: usize,
    /// 列号（0 = 主变列）。
    pub col: usize,
    /// 行号（= 手数 depth，根为 0）。
    pub row: usize,
    /// 是否带注释（SGF `C` 属性，含根注释）。
    pub commented: bool,
}

/// 整棵树的布局结果。
pub struct TreeLayout {
    /// 全部节点位置（下标 = 节点 id）。
    pub nodes: Vec<TreeNode>,
    /// 行 → 该行各节点的 id（按列序），供按行虚拟化绘制。
    pub rows: Vec<Vec<usize>>,
    /// 总列数。
    pub cols: usize,
}

/// 计算整棵树的布局：深度优先分配列（见模块文档）。
///
/// 复杂度 O(节点数)，仅在树指纹变化时调用（见 [`TreeUi`]）。
pub fn layout_tree(board: &Board, meta: Option<&GameMeta>) -> TreeLayout {
    let nodes = board.nodes();
    let mut layout = TreeLayout { nodes: Vec::new(), rows: Vec::new(), cols: 0 };
    if nodes.is_empty() {
        return layout;
    }
    layout.nodes.resize(
        nodes.len(),
        TreeNode { id: 0, col: 0, row: 0, commented: false },
    );
    // next_col：下一条空闲列；列 0 已被主变占用，从 1 起分配。
    let mut next_col = 1usize;
    walk(board, meta, 0, SIG_INIT, 0, &mut next_col, &mut layout);
    layout.cols = next_col;
    // 行索引：行号即 depth，行数 = 最大深度 + 1。
    let max_row = nodes.iter().map(|n| n.depth()).max().unwrap_or(0);
    layout.rows = vec![Vec::new(); max_row + 1];
    for node in &layout.nodes {
        layout.rows[node.row].push(node.id);
    }
    // 同行内按列号排序（遍历顺序已保证：主变先入列，此处仅防御）。
    for row in &mut layout.rows {
        row.sort_by_key(|&id| layout.nodes[id].col);
    }
    layout
}

/// 递归遍历：把 `id` 子树放进 `col` 列；`sig` 为根到本节点父节点的局面签名。
fn walk(
    board: &Board,
    meta: Option<&GameMeta>,
    id: usize,
    sig: u64,
    col: usize,
    next_col: &mut usize,
    layout: &mut TreeLayout,
) {
    let node = &board.nodes()[id];
    // 局面签名逐手增量混入（与 sgf::load 的 position_sig 同一口径）。
    let sig = match node.record() {
        Some(record) => sig_with_record(sig, record),
        None => sig,
    };
    layout.nodes[id] = TreeNode {
        id,
        col,
        row: node.depth(),
        commented: meta.is_some_and(|meta| meta.comment_by_sig(sig).is_some()),
    };
    // 第 0 个子节点留在本列（主变延续），其余各占一列并整树延伸；
    // 叶子节点（无子）到此结束。
    if let Some((first, rest)) = node.children().split_first() {
        walk(board, meta, *first, sig, col, next_col, layout);
        for &child in rest {
            let alt = *next_col;
            *next_col += 1;
            walk(board, meta, child, sig, alt, next_col, layout);
        }
    }
}

/// 局面签名 FNV-1a 初始值（与 `sgf::load::SIG_INIT` 一致）。
const SIG_INIT: u64 = 0xcbf2_9ce4_8422_2325;

/// 把一手棋混入局面签名。与 `sgf::load::sig_with_record` 逐字节一致
/// （行棋方 + 着点 / 弃着，**不含提子**）——注释键必须完全同构才能查中；
/// 两处语义独立，改动需同步。
fn sig_with_record(mut sig: u64, record: &MoveRecord) -> u64 {
    fn byte(sig: &mut u64, b: u8) {
        *sig ^= u64::from(b);
        *sig = sig.wrapping_mul(0x0000_0100_0000_01b3);
    }
    byte(&mut sig, u8::from(matches!(record.player, Stone::Black)));
    match record.action {
        Action::Place(c) => {
            byte(&mut sig, 1);
            byte(&mut sig, c.x());
            byte(&mut sig, c.y());
        }
        Action::Pass => byte(&mut sig, 0),
    }
    sig
}

/// 树指纹：棋盘替换代数 + 盘面尺寸 + 全部节点的父子拓扑。
/// 树的任何结构变化（载谱 / 落子 / 弃着 / 悔棋）都会改变指纹；
/// 导航（go_to_node / 切分支）不改变拓扑，指纹不变——布局无需重算。
fn fingerprint(board: &Board, epoch: u64) -> u64 {
    let mut h = SIG_INIT ^ epoch;
    fn byte(h: &mut u64, b: u64) {
        *h ^= b;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    byte(&mut h, u64::from(board.size().n()));
    let nodes = board.nodes();
    byte(&mut h, nodes.len() as u64);
    for node in nodes {
        byte(&mut h, node.parent().map_or(u64::MAX, |p| p as u64));
        byte(&mut h, node.children().len() as u64);
    }
    h
}

/// 树控件跨帧状态：布局缓存与自动滚动所需的游标记忆。
#[derive(Default)]
pub struct TreeUi {
    cache: Option<(u64, TreeLayout)>,
    /// 布局重算次数（诊断 / 验证缓存生效用）。
    rebuilds: usize,
    /// 上一帧的当前节点（游标变化时触发一次自动滚动）。
    last_current: Option<usize>,
}

impl TreeUi {
    /// 布局重算次数（缓存有效性验证用）。
    pub fn rebuild_count(&self) -> usize {
        self.rebuilds
    }
}

impl TreeUi {
    /// 取布局：指纹命中缓存则复用，否则重算一次。
    fn layout(&mut self, board: &Board, meta: Option<&GameMeta>, epoch: u64) -> &TreeLayout {
        let fp = fingerprint(board, epoch);
        if self.cache.as_ref().is_none_or(|(cached, _)| *cached != fp) {
            let layout = layout_tree(board, meta);
            self.rebuilds += 1;
            self.cache = Some((fp, layout));
        }
        &self.cache.as_ref().expect("上一步刚写入缓存").1
    }
}

/// 节点显示文本：手数（每 10 手标注一次）+ 行棋方 + 着法（GTP 坐标）。
fn node_text(board: &Board, id: usize) -> String {
    let node = &board.nodes()[id];
    let label = match node.record() {
        None => format!("开局（{}）", board.size()),
        Some(record) => {
            let side = match record.player {
                Stone::Black => "黑",
                Stone::White => "白",
            };
            let mv = match record.action {
                Action::Place(c) => c.to_gtp(board.size()),
                Action::Pass => "弃着".to_owned(),
            };
            format!("{side}{mv}")
        }
    };
    let step = node.depth();
    if step.is_multiple_of(10) {
        format!("{step} {label}")
    } else {
        label
    }
}

/// 节点行高。
const ROW_H: f32 = NODE_H + ROW_GAP;

/// 绘制棋谱树并处理点击跳转（见模块文档）。
///
/// `meta` 提供注释索引（无棋谱传 `None`，全部节点无注释标记）；
/// `epoch` 为棋盘替换代数（载谱 / 新对局时递增，见 `App`），混入指纹
/// 防止「着法序列恰好相同的另一盘棋」复用过期布局。
pub fn show(
    ui: &mut Ui,
    board: &mut Board,
    meta: Option<&GameMeta>,
    state: &mut TreeUi,
    epoch: u64,
) {
    let current = board.current_node();
    let (nodes, rows, cols) = {
        let layout = state.layout(board, meta, epoch);
        (layout.nodes.clone(), layout.rows.clone(), layout.cols)
    };
    if nodes.is_empty() {
        ui.weak("（空树）");
        return;
    }
    // 游标变化（棋盘落子 / ← → / 跳转）时记住新节点，本帧滚动到可见区。
    let scroll_to = (state.last_current != Some(current)).then_some(current);
    state.last_current = Some(current);

    let content_w = cols as f32 * (NODE_W + COL_GAP);
    // 绘制期间收集的点击跳转动作（绘制后统一执行，避免借用冲突）。
    let mut jump = None;
    // 游标变化时本帧把当前节点滚到可见区：直接设置下一帧的滚动偏移
    // （`vertical_scroll_offset` 经 State 持久化，在 begin 时生效），
    // 居中对齐；手动行虚拟化下 `scroll_to_rect` 的 target 机制
    // 只在内容被实际创建时才能计算，这里预先给出确定位置更可靠。
    let scroll_offset = scroll_to.map(|id| {
        let y = nodes[id].row as f32 * ROW_H;
        (y - ui.available_height() * 0.5 + ROW_H * 0.5).max(0.0)
    });
    // 外层横向（列多时滚列）+ 内层纵向按行虚拟化：先按可见行筛选出
    // 非空行，再只对这些行 allocate 内容——不可见节点零 allocate，
    // 行数极多时滚动条也不失真。
    let h_scroll = egui::ScrollArea::horizontal().auto_shrink(false);
    let mut v_scroll = egui::ScrollArea::vertical().auto_shrink(false);
    if let Some(offset) = scroll_offset {
        v_scroll = v_scroll.vertical_scroll_offset(offset);
    }
    let h_out = h_scroll.show(ui, |ui| {
        v_scroll.show_viewport(ui, |ui, viewport| {
            ui.set_height(rows.len() as f32 * ROW_H);
            // viewport 在内容坐标系里（min = 当前滚动位置，见 egui 文档）。
            let first = (viewport.min.y / ROW_H).floor().max(0.0) as usize;
            let last = (viewport.max.y / ROW_H).ceil() as usize;
            let last = last.min(rows.len());
            // 每行一个绝对定位的子 Ui：content_ui 的原点在内容顶部（屏幕位置 =
            // inner.min − offset，随滚动平移），行必须放在内容坐标 y = row*ROW_H
            // 对应的屏幕位置——直接顺序 allocate 会全部叠在内容顶部（滚动后
            // 可见区空白）。
            let top = ui.min_rect().top();
            let left = ui.min_rect().left();
            for (row, ids) in rows[first..last]
                .iter()
                .enumerate()
                .filter(|(_, ids)| !ids.is_empty())
            {
                let y = top + (first + row) as f32 * ROW_H;
                let rect = Rect::from_min_size(
                    Pos2::new(left, y),
                    Vec2::new(content_w.max(ui.available_width()), ROW_H),
                );
                let line = ui
                    .new_child(egui::UiBuilder::new().max_rect(rect))
                    .allocate_exact_size(rect.size(), Sense::hover())
                    .0;
                jump = jump.or(draw_line(ui, board, &nodes, ids, line, current));
            }
        });
    });

    // 点击跳转（幂等：绘制时已排除当前节点）。
    if let Some(id) = jump {
        board.go_to_node(id);
    }
    let _ = h_out;
}

/// 绘制一行内的各节点（每个分支列至多一个）与父连线。
#[allow(clippy::too_many_arguments)]
fn draw_line(
    ui: &mut Ui,
    board: &Board,
    nodes: &[TreeNode],
    ids: &[usize],
    line: Rect,
    current: usize,
) -> Option<usize> {
    let mut jump = None;
    let painter = ui.painter_at(line);
    for &id in ids {
        let node = &nodes[id];
        let x = line.min.x + node.col as f32 * (NODE_W + COL_GAP);
        let rect = Rect::from_min_size(Pos2::new(x, line.min.y), Vec2::new(NODE_W, NODE_H));

        // 父连线：同列竖线；跨列「下-右-下」折线（变着从分叉点岔开）。
        if let Some(parent) = board.nodes()[id].parent() {
            let pcol = nodes[parent].col;
            let px = line.min.x + pcol as f32 * (NODE_W + COL_GAP) + NODE_W * 0.5;
            let color = if node.col == 0 { MAIN } else { ALT };
            let stroke = Stroke::new(1.2, color);
            if pcol == node.col {
                painter.line_segment([Pos2::new(px, line.min.y), Pos2::new(px, rect.top())], stroke);
            } else {
                let mid_y = rect.center().y;
                painter.line_segment([Pos2::new(px, line.min.y), Pos2::new(px, mid_y)], stroke);
                painter.line_segment(
                    [Pos2::new(px, mid_y), Pos2::new(rect.left(), mid_y)],
                    stroke,
                );
                painter.line_segment(
                    [Pos2::new(rect.left(), mid_y), Pos2::new(rect.left(), rect.top())],
                    stroke,
                );
            }
        }

        let is_current = id == current;
        let text = node_text(board, id);
        let response = ui
            .interact(rect, egui::Id::new(("tree_node", id)), Sense::click())
            .on_hover_text(text.clone());
        if is_current {
            painter.rect_filled(rect, 3.0, Color32::from_rgba_unmultiplied(255, 170, 40, 42));
            painter.rect_stroke(rect, 3.0, Stroke::new(1.6, CURSOR), StrokeKind::Middle);
        } else if response.hovered() {
            painter.rect_stroke(
                rect,
                3.0,
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 70)),
                StrokeKind::Middle,
            );
        }
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(12.0),
            if is_current { CURSOR } else { LABEL },
        );
        // 注释标记：右上角小圆点。
        if node.commented {
            painter.circle_filled(
                Pos2::new(rect.right() - 4.0, rect.top() + 4.0),
                2.5,
                COMMENT_DOT,
            );
        }
        // 点击跳转；已在此节点上时不做无谓操作（幂等）。动作交由
        // [`show`] 在本帧绘制后统一执行（本函数只读棋盘，避免借用冲突）。
        if response.clicked() && id != current {
            jump = Some(id);
        }
    }
    jump
}
