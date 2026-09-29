//! 参照树浏览（阶段 23.2）：在应用内查看外部参照（定式库 / 题谱）的
//! 树结构 —— 当前局面在库中的位置、祖先路径、各分支的后续与库内注释。
//!
//! # 定位与边界（与既有功能的分工）
//!
//! - **只读浏览**：点击节点只展开 / 收起该节点的注释与后续（视图内交互），
//!   **不落当前棋盘、不切对局、不改任何应用状态**；「照库落子」是另一条
//!   入口（参照卡片的续法行，走 `Board::play`），两者绝不混淆。
//! - **另一棵树**：与「当前对局树」控件（`ui::tree`）数据源、语义都不同
//!   ——对局树是棋谱树、点击跳转游标；本树是参照库树、点击只展开注释。
//!   因此不复用其布局缓存与跳转状态，避免两棵树的状态搅在一起。
//! - **局部树**：从锚点（命中节点或库根）出发的局部视图：祖先路径 +
//!   有限深度的后继分支。不做整库全景（大库会卡 UI），展开量由
//!   [`REF_TREE_DEPTH`] / [`REF_TREE_NODES`] 具名上限控制。
//!
//! # 结构口径（与 refbook::walk 的 expand 完全同构）
//!
//! SGF 的 `GameTree` 是「主链 + 末节点分叉」两层结构，直接遍历的续法判断
//! 容易漂移；这里按 `refbook::walk::expand` 的同一口径展开成统一节点树
//! （每个节点的 children = 链延续 + 各分支首节点），命中条目的 `path`
//! （`RefEntry::path`）即本展开结构的每层下标——两侧口径必须一致，
//! 改动需同步。
//!
//! # 注释节点
//!
//! 无着点的节点（纯注释 / 空节点）在索引重放时不产生局面，但链会延续；
//! 展示层同样处理：无着点节点**并入下一着法节点**展示（取其注释），不
//! 单独成行——库内注释通常写在着法节点上，空节点注释沿链带到下一个
//! 着法节点符合阅读直觉。

use crate::board::{Action, Size, Stone};
use crate::sgf::{GameTree, Node};

/// 从锚点向下展开的**着法层数**上限（防大库卡 UI 的具名常量）。
///
/// 取 4：定式研究「这一路变化接下来几手怎么走、库怎么评」，4 层着法
/// 已覆盖常见分支展示需求；更深的分支点击节点行后以该节点为新锚点
/// 逐段展开（视图内交互，不落棋盘）。
pub const REF_TREE_DEPTH: usize = 4;

/// 视图内**节点总数**上限（含祖先路径与后继展开）。
///
/// 分支极多的合成大库按广度截断：到量即停、截断处标「…」，绝不整库
/// 展开（几百节点库全部展开会让每帧布局与绘制都失控）。
pub const REF_TREE_NODES: usize = 120;

/// 视图锚点：从哪里出发浏览参照树。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefTreeAnchor {
    /// 当前局面命中：祖先路径 = 库根到命中节点，后继 = 命中节点向下。
    Hit(Vec<usize>),
    /// 未命中 / 偏离 / 空盘：从库根浏览（顶部说明原因）。
    Root,
}

/// 视图里的一行（先序展开的扁平列表，绘制侧按行渲染）。
#[derive(Clone, Debug)]
pub struct RefTreeRow {
    /// 统一树路径（各层 children 下标）；库根 = 空路径。
    pub path: Vec<usize>,
    /// 缩进层级（0 = 库根行；每层着法 +1）。
    pub depth: usize,
    /// 行文本：库根 = 摆子摘要，其余 = 「黑 D4」形态着法。
    pub label: String,
    /// 该节点的库内注释（`C`，已整理；沿链收集空节点注释）。
    pub comment: Option<String>,
    /// 该节点是否有后继着法（决定可否点击展开后续）。
    pub has_children: bool,
    /// 是否当前命中节点（琥珀高亮：「我走到库里的哪里」）。
    pub is_hit: bool,
    /// 该行是否默认展开（命中节点的祖先链展开，其余收起）。
    pub expanded: bool,
    /// 展开被数量 / 深度上限截断（行尾标「…」）。
    pub truncated: bool,
}

/// 局部树视图数据（每帧派生；窗口打开时才派生，零常驻成本）。
#[derive(Clone, Debug)]
pub struct RefTreeView {
    /// 锚点说明行（「库内位置 0-2」/「未命中：从库根浏览」等）。
    pub anchor_text: String,
    /// 命中节点的统一树路径（无命中为 `None`）。
    pub hit_path: Option<Vec<usize>>,
    /// 先序展开的行列表（绘制侧直接渲染；受两个上限约束）。
    pub rows: Vec<RefTreeRow>,
    /// 是否因上限截断（UI 提示「已截断」）。
    pub truncated: bool,
}

/// 从参照树展开统一节点（与 `refbook::walk::expand` 同口径）。
fn expand(tree: &GameTree) -> Vec<RefNode<'_>> {
    let mut current: Vec<RefNode<'_>> =
        tree.children.iter().flat_map(expand).collect();
    for node in tree.nodes.iter().rev() {
        current = vec![RefNode {
            node,
            children: std::mem::take(&mut current),
        }];
    }
    current
}

/// 统一节点（借用 SGF 节点；与 walk::RNode 同构，独立定义避免依赖
/// refbook 的私有类型）。
struct RefNode<'a> {
    node: &'a Node,
    children: Vec<RefNode<'a>>,
}

/// 节点的首手（有着点取之；纯注释节点沿链下探）与沿途注释。
/// 返回 `None` = 该子树无任何着法（纯注释尾巴，展示为叶）。
fn first_move_down(
    rn: &RefNode<'_>,
    size: Size,
) -> Option<(Stone, Action, Option<String>)> {
    let comment = rn.node.comment().filter(|c| !c.is_empty()).map(str::to_owned);
    match rn.node.move_action(size) {
        Ok(Some((stone, action))) => Some((stone, action, comment)),
        Ok(None) => rn.children.iter().find_map(|child| {
            let carried = comment.clone();
            first_move_down(child, size).map(|(stone, action, sub)| {
                // 沿途注释并入子节点（不丢空节点上的解说）。
                (stone, action, carried.or(sub))
            })
        }),
        Err(_) => None,
    }
}

/// 派生局部树视图。
///
/// `anchor` 决定祖先路径与展开起点；`comment_of_root` 为库根注释
/// （题面 / 库说明）。每帧调用：O(锚点子树上限) 展开量（两上限内
/// ≤ [`REF_TREE_NODES`] 行），命中查询已在 `reference_view` 完成时
/// 由调用方传入路径，这里不再查表。
pub fn derive_view(
    tree: &GameTree,
    size: Size,
    anchor: &RefTreeAnchor,
    root_comment: Option<&str>,
) -> RefTreeView {
    let top = expand(tree);
    match anchor {
        RefTreeAnchor::Hit(path) => derive_hit_view(&top, size, path, root_comment),
        RefTreeAnchor::Root => derive_root_view(&top, size, root_comment),
    }
}

/// 命中态视图：祖先链（库根 → 命中节点）+ 命中节点向下的后继展开。
///
/// 祖先链沿 `path` 下探；途经**无着点节点**（纯注释 / SGF 根壳层）不
/// 单开一行（与索引侧「无着点节点不产生局面」口径一致），注释并入下
/// 一个着法行，路径下标照实累积——行 `path` 与 [`crate::refbook::RefEntry`]
/// 的 path 口径逐层一致，命中行 `is_hit` 的判定才对得上。
fn derive_hit_view(
    top: &[RefNode<'_>],
    size: Size,
    path: &[usize],
    root_comment: Option<&str>,
) -> RefTreeView {
    let mut rows = Vec::new();
    // 库根行（摆子摘要 + 根注释）。
    rows.push(RefTreeRow {
        path: Vec::new(),
        depth: 0,
        label: "库根".to_owned(),
        comment: root_comment.map(str::to_owned),
        has_children: !top.is_empty(),
        is_hit: path.is_empty(),
        expanded: true,
        truncated: false,
    });
    // 祖先链：沿 path 逐层下探，途经节点全部展开。
    let mut chain: &[RefNode<'_>] = top;
    let mut carried = None; // 沿途折叠的无着点节点注释。
    for (level, &idx) in path.iter().enumerate() {
        let Some(rn) = chain.get(idx) else {
            // path 与树不吻合（参照被替换等竞态）：退化为库根浏览。
            return derive_root_view(top, size, root_comment);
        };
        // 无着点节点：不开行，注释带上下探（自己的手交给后续层级）。
        if rn.node.move_action(size).map(|mv| mv.is_none()).unwrap_or(true) {
            if let Some(text) = rn.node.comment().filter(|c| !c.is_empty()) {
                carried = Some(carried.take().unwrap_or_else(|| text.to_owned()));
            }
            chain = &rn.children;
            continue;
        }
        let is_last = level + 1 == path.len();
        if is_last {
            // 命中行必有着点（索引条目只建于有着点节点）。
            let (stone, action) = rn
                .node
                .move_action(size)
                .ok()
                .flatten()
                .unwrap_or((Stone::Black, Action::Pass));
            rows.push(RefTreeRow {
                path: path[..=level].to_vec(),
                depth: rows.len(),
                label: move_text(stone, action, size),
                comment: carried.take().or_else(|| {
                    rn.node.comment().filter(|c| !c.is_empty()).map(str::to_owned)
                }),
                has_children: !rn.children.is_empty(),
                is_hit: true,
                expanded: true,
                truncated: false,
            });
        } else {
            let (stone, action, comment) = first_move_down(rn, size)
                .unwrap_or((Stone::Black, Action::Pass, None));
            rows.push(RefTreeRow {
                path: path[..=level].to_vec(),
                depth: rows.len(),
                label: move_text(stone, action, size),
                comment: comment.or(carried.take()),
                has_children: true,
                is_hit: false,
                expanded: true,
                truncated: false,
            });
        }
        chain = &rn.children;
    }
    // 命中节点向下的后继展开：视觉深度从命中行续起；层数守卫以命中
    // 行为基准（base = 命中行行深）。
    let hit_depth = rows.last().map_or(0, |row| row.depth);
    let budget = REF_TREE_NODES.saturating_sub(rows.len());
    let (sub, sub_truncated) =
        expand_successors(chain, size, path, hit_depth, hit_depth, budget);
    rows.extend(sub);
    RefTreeView {
        anchor_text: format!("库内位置 {}", path_text(path)),
        hit_path: Some(path.to_vec()),
        rows,
        truncated: sub_truncated,
    }
}

/// 未命中态视图：从库根出发（顶部说明由调用方拼装，这里给锚点行）。
fn derive_root_view(
    top: &[RefNode<'_>],
    size: Size,
    root_comment: Option<&str>,
) -> RefTreeView {
    let mut rows = vec![RefTreeRow {
        path: Vec::new(),
        depth: 0,
        label: "库根".to_owned(),
        comment: root_comment.map(str::to_owned),
        has_children: !top.is_empty(),
        is_hit: false,
        expanded: true,
        truncated: false,
    }];
    let budget = REF_TREE_NODES.saturating_sub(rows.len());
    let (sub, sub_truncated) = expand_successors(top, size, &[], 0, 0, budget);
    rows.extend(sub);
    RefTreeView {
        anchor_text: "从库根浏览".to_owned(),
        hit_path: None,
        rows,
        truncated: sub_truncated,
    }
}

/// 先序展开后继：每行 = 一个**有着点**节点的着法（无着点节点折叠：
/// 不开行、注释沿链并入下探后的首个着法行）；相对 `base` 的着法层数
/// 超过 [`REF_TREE_DEPTH`] 或节点数超出 `budget` 即停（截断处标「…」）。
/// `depth` 为当前行深（视觉缩进用，命中态由祖先链行深续起）；`base`
/// 为深度守卫的零点（锚点行深）——层数按 `depth − base` 相对计算，
/// 与祖先链行深绝对值解耦；`prefix` 为真实路径下标前缀（无着点层也
/// 累积，与索引 path 口径一致）。
/// 返回（行列表, 是否截断）。
fn expand_successors(
    chain: &[RefNode<'_>],
    size: Size,
    prefix: &[usize],
    depth: usize,
    base: usize,
    budget: usize,
) -> (Vec<RefTreeRow>, bool) {
    let mut rows: Vec<RefTreeRow> = Vec::new();
    if depth.saturating_sub(base) >= REF_TREE_DEPTH || budget == 0 {
        // 层数 / 数量到限：不展开，由父行标截断。
        return (rows, true);
    }
    for (i, rn) in chain.iter().enumerate() {
        if rows.len() >= budget {
            return (rows, true);
        }
        let mut path = prefix.to_vec();
        path.push(i);
        // 无着点节点（纯注释 / 空节点）：折叠不开行，注释沿链并入
        // 首个着法后裔；子树无着法时整支跳过（纯注释尾巴不占行）。
        if rn.node.move_action(size).map(|mv| mv.is_none()).unwrap_or(true) {
            let (mut sub, truncated) =
                expand_successors(&rn.children, size, &path, depth, base, budget);
            let carried = rn.node.comment().filter(|c| !c.is_empty()).map(str::to_owned);
            if let Some(first) = sub.first_mut() {
                first.comment = first.comment.clone().or(carried);
            }
            rows.append(&mut sub);
            if truncated {
                return (rows, true);
            }
            continue;
        }
        let Some((stone, action, comment)) = first_move_down(rn, size) else {
            continue; // 不可重放的分支（索引侧同样舍弃）。
        };
        let has_children = rn
            .children
            .iter()
            .any(|child| first_move_down(child, size).is_some());
        rows.push(RefTreeRow {
            path: path.clone(),
            depth: depth + 1,
            label: move_text(stone, action, size),
            comment,
            has_children,
            is_hit: false,
            // 首分支默认展开一瞥（用户研究主变最常见），其余收起。
            expanded: i == 0,
            truncated: false,
        });
        // 分支内部继续下探（先序展开；数量预算在递归入口核对）。
        if rows.len() >= budget {
            return (rows, true);
        }
        let (sub, sub_truncated) = expand_successors(
            &rn.children,
            size,
            &path,
            depth + 1,
            base,
            budget - rows.len(),
        );
        rows.extend(sub);
        if sub_truncated {
            return (rows, true);
        }
    }
    (rows, false)
}

/// 着法文本：「黑 D4」/「白弃着」。
fn move_text(stone: Stone, action: Action, size: Size) -> String {
    let at = match action {
        Action::Place(c) => c.to_gtp(size),
        Action::Pass => "弃着".to_owned(),
    };
    format!("{} {at}", stone.name())
}

/// 路径展示串：「根」/「0-2-1」。
pub fn path_text(path: &[usize]) -> String {
    if path.is_empty() {
        "根".to_owned()
    } else {
        path.iter().map(ToString::to_string).collect::<Vec<_>>().join("-")
    }
}

/// 根节点摆子摘要（库根行悬停用；无摆子返回 `None`）。
pub fn root_setup_text(tree: &GameTree, size: Size) -> Option<String> {
    let root = tree.root()?;
    let mut parts: Vec<String> = Vec::new();
    for (ident, name) in [("AB", "黑"), ("AW", "白"), ("AE", "空")] {
        let coords = root.coords(ident, size).ok()?;
        if !coords.is_empty() {
            parts.push(format!(
                "{name} {}",
                coords.iter().map(|c| c.to_gtp(size)).collect::<Vec<_>>().join(" ")
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

// ---- 窗口呈现 ----

use egui::{Color32, Pos2, RichText, Sense, Stroke, Ui, Vec2};

use super::theme;

/// 行高（祖先链与后继行统一；注释区自适应）。
const ROW_H: f32 = 22.0;
/// 每层缩进（着法层级）。
const INDENT: f32 = 26.0;
/// 命中节点高亮（与对局树当前节点同一琥珀）。
const HIT: Color32 = Color32::from_rgb(255, 170, 40);
/// 普通着法文字。
const LABEL: Color32 = Color32::from_rgb(205, 205, 210);
/// 连线（弱灰）。
const LINE: Color32 = Color32::from_rgb(90, 96, 108);
/// 注释文字（弱绿，与对局树注释点同源色系）。
const COMMENT: Color32 = Color32::from_rgb(150, 200, 150);

/// 跨帧状态：浏览窗口开关与视图内展开集合（egui memory 之外的显式
/// 状态——展开集合要随「锚点变化 / 换参照」复位，egui 匿名记忆做不到）。
#[derive(Default)]
pub struct RefTreeUi {
    /// 浏览窗口是否打开（侧栏入口按钮翻转）。
    pub open: bool,
    /// 视图内展开的节点路径集合（点击行翻转；`None` = 用派生侧默认值）。
    expanded: Option<std::collections::HashSet<Vec<usize>>>,
    /// 展开集合针对的锚点（锚点变化即失效复位——命中点移动后旧展开
    /// 集合的语义已漂移）。
    anchor_key: Option<Vec<usize>>,
}

impl RefTreeUi {
    /// 用户显式展开的路径（取证用；`None` 集合 = 尚未点击过任何行）。
    pub fn expanded_paths(&self) -> Option<Vec<Vec<usize>>> {
        self.expanded
            .as_ref()
            .map(|set| set.iter().cloned().collect())
    }
}

/// 绘制「库中树」浏览窗口（`App::ui` 每帧调用；未打开时零开销返回）。
///
/// 只读：点击分支行翻转该节点注释 / 后续的展开态，**不产生任何落子 /
/// 导航动作**。`view` 每帧由派生侧给出（打开时才有）；`tree` / `size`
/// 供库根行悬停摘要。
pub fn show_window(
    ctx: &egui::Context,
    state: &mut RefTreeUi,
    view: &RefTreeView,
    tree: &GameTree,
    size: Size,
    no_hit_reason: Option<&str>,
) {
    if !state.open {
        return;
    }
    // 锚点变化（命中点移动 / 换锚）即复位展开集合：旧集合针对旧局面。
    let key = view.hit_path.clone().unwrap_or_default();
    if state.anchor_key.as_ref() != Some(&key) {
        state.expanded = None;
        state.anchor_key = Some(key);
    }
    let mut open = state.open;
    egui::Window::new("库中树")
        .open(&mut open)
        .default_width(420.0)
        .default_height(420.0)
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            // 锚点说明行：命中态给库内位置；未命中态给原因。
            match no_hit_reason {
                Some(reason) => {
                    ui.colored_label(theme::colors::WARN, reason);
                }
                None => {
                    ui.label(
                        RichText::new(view.anchor_text.as_str())
                            .color(theme::colors::OK)
                            .strong(),
                    );
                }
            }
            ui.weak(format!(
                "只读浏览：点击分支展开注释与后续；深度上限 {} 手 / 节点上限 {}。",
                REF_TREE_DEPTH, REF_TREE_NODES
            ));
            if view.truncated {
                ui.colored_label(theme::colors::WARN, "分支较多，超出上限的部分已截断（…）。");
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                // 折叠过滤：某行未展开（且非命中 / 祖先链强制展开行）时，
                // 其子树行（path 以该行 path 为前缀且更长）不渲染——
                // 「收起」必须是真实的折叠，而非只换行首箭头。
                let mut fold_prefix: Option<Vec<usize>> = None;
                for row in &view.rows {
                    if let Some(prefix) = &fold_prefix {
                        if row.path.len() > prefix.len()
                            && row.path.starts_with(prefix.as_slice())
                        {
                            continue; // 折叠节点的后代：跳过。
                        }
                        fold_prefix = None;
                    }
                    let expanded = state
                        .expanded
                        .as_ref()
                        .is_none_or(|set| set.contains(&row.path) != row.expanded)
                        || row.is_hit;
                    if !expanded && row.has_children && !row.path.is_empty() {
                        // 收起有子节点的行：本行照画（箭头 ▸），后代折叠。
                        fold_prefix = Some(row.path.clone());
                    }
                    row_ui(ui, state, row, tree, size);
                }
            });
        });
    state.open = open;
}

/// 一行的绘制：缩进连线 + 行按钮（egui::Button 承载文本——自带
/// accesskit 标签，headless click_label 与读屏都可靠；自绘底色通过
/// Button 的 fill / stroke 表达）+ 注释展开区。
///
/// 展开集合的语义 = **「与派生默认值不同的翻转」**：默认展开的行点一次
/// 收起、默认收起的行点一次展开——集合记录被翻转过的路径，`effective`
/// = 默认值 XOR 在集合中。
fn row_ui(ui: &mut Ui, state: &mut RefTreeUi, row: &RefTreeRow, tree: &GameTree, size: Size) {
    let effective = |row: &RefTreeRow| -> bool {
        match state.expanded.as_ref() {
            Some(set) => set.contains(&row.path) != row.expanded,
            None => row.expanded,
        }
    };
    let expanded = effective(row);
    // 注释可见性：祖先链 / 命中节点 / 默认展开行直接展示，其余点开。
    let show_comment = expanded || row.is_hit;
    // 行首展开指示。
    let marker = if !row.has_children && row.comment.is_some() {
        "◦"
    } else if expanded {
        "▾"
    } else {
        "▸"
    };
    // 命中节点文本带「▶ 当前位置」标记：读屏用户与截图都能一眼定位
    // 「我走到库里的哪里」。
    let label = if row.is_hit {
        format!("{marker} {}（▶ 当前位置）", row.label)
    } else {
        format!("{marker} {}", row.label)
    };
    // 层级连线：在按钮之外的缩进区画「竖 + 横」短折线（与分支层级对应）。
    if row.depth > 0 {
        let (indent_rect, _) =
            ui.allocate_exact_size(Vec2::new(row.depth as f32 * INDENT, ROW_H), Sense::hover());
        let painter = ui.painter_at(indent_rect);
        let x = indent_rect.left() + INDENT * 0.4;
        painter.line_segment(
            [Pos2::new(x, indent_rect.top()), Pos2::new(x, indent_rect.center().y)],
            Stroke::new(1.0, LINE),
        );
        painter.line_segment(
            [
                Pos2::new(x, indent_rect.center().y),
                Pos2::new(indent_rect.right(), indent_rect.center().y),
            ],
            Stroke::new(1.0, LINE),
        );
    }
    // 行按钮：三态底色（命中琥珀暗底 / hover 亮底 / 常态透明），
    // 文本等宽字体（坐标对齐）。空文本行（纯缩进）不设按钮。
    let text_color = if row.is_hit { HIT } else { LABEL };
    let mut button =
        egui::Button::new(RichText::new(&label).monospace().color(text_color))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::NONE)
        .corner_radius(3.0);
    if row.is_hit {
        button = button
            .fill(theme::colors::ACCENT_DIM)
            .stroke(Stroke::new(1.6, HIT));
    }
    let response = ui.add_sized(
        [ui.available_width().max(60.0), ROW_H],
        button,
    );
    // 截断标记：行尾弱橙「…」（按钮文本尾部追加，随文本走）。
    let response = if row.truncated {
        response.on_hover_text("该分支在深度 / 数量上限处截断")
    } else {
        response
    };
    // 悬停说明：库内路径 + 摆子摘要（库根行）。
    let mut tip = format!("库内位置 {}", path_text(&row.path));
    if row.path.is_empty()
        && let Some(setup) = root_setup_text(tree, size)
    {
        tip.push_str(&format!("；摆子 {setup}"));
    }
    let response = response.on_hover_text(tip);
    // 只读展开交互：翻转该节点的展开集合（不落棋盘、不导航）。
    let interactive = row.has_children || row.comment.is_some();
    if interactive && response.clicked() {
        let set = state.expanded.get_or_insert_with(Default::default);
        if set.contains(&row.path) {
            set.remove(&row.path);
        } else {
            set.insert(row.path.clone());
        }
    }
    // 注释区（库内解说是浏览的核心价值）：展开态或命中态都展示。
    if show_comment
        && let Some(text) = &row.comment
    {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(row.depth as f32 * INDENT + 14.0);
            ui.label(RichText::new(text.as_str()).size(12.0).color(COMMENT));
        });
    }
}
