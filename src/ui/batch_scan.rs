//! 整谱快扫设置对话框（原侧栏「整谱快扫」卡片设置区的搬家）。
//!
//! # 为什么是独立对话框而不是侧栏折叠区
//!
//! 侧栏的定位是**纯信息展示**（胜率 / 候选点 / 失误 / 统计……），
//! 快扫设置是一组「发起前才调一次」的参数，常驻侧栏既挤占纵向空间
//! 又让侧栏混进控件。改为菜单「分析 → 整谱快扫…」打开的对话框：
//! 设置与预估同屏，**看完预估再决定发起**。
//!
//! # 状态单一来源
//!
//! 对话框**不持有**跨帧的配置副本：每帧从 [`AnalysisState::batch_config`]
//! 读、改动即刻写回 [`AnalysisState::set_batch_config`]，与偏好持久化共用
//! 同一份。好处有三：
//! - 偏好持久化（`UiPrefs.batch_visits` / `batch_side` / `batch_variations`
//!   / `batch_deepen*`）由既有 `sync_prefs` 每帧自动生效，对话框改的
//!   visits / 只扫一方 / 含变着 / 加深参数照旧落盘；
//! - 对话框重开看到的一定是最新的那份（上次的调整不会丢）；
//! - 「预估」与「发起」读的是同一个 `batch_config()`，不存在「预估按
//!   A 配置算、发起按 B 配置跑」的错配。
//!
//! # 取消语义
//!
//! 出口三个：开始快扫 / 取消 / 关闭（X 与 Esc）。参数改动**即时生效**
//! 且无「编辑中草稿」概念，因此取消 / 关闭只关窗、不回滚参数——用户
//! 看得见的每一步改动都留下更符合直觉，也避免「关了窗才发现刚才调的
//! visits 没保存」的困惑。

use egui::{Align2, Ui, Vec2};

use super::analysis::{AnalysisState, BatchConfig, BatchSide, batch_estimate};
use super::theme;
use crate::board::Board;

/// 「每手 visits」的常用档位（分段选择 + 「自定义」编辑共存）。
const BATCH_VISITS_PRESETS: [u32; 4] = [20, 40, 100, 300];

/// 整谱快扫对话框的执行结果（由 `App` 在绘制结束后执行）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BatchScanAction {
    /// 无操作。
    #[default]
    None,
    /// 用户点了「开始快扫」：App 转交 [`AnalysisState::start_batch`]。
    Start,
    /// 快扫进行中用户点了「取消快扫」：terminate 在飞块并结束任务。
    Cancel,
    /// 用户点了「取消」或按 Esc / 关窗：App 收起对话框。
    Close,
}

/// 画整谱快扫对话框：`open` 开关由调用方持有，egui 在按 Esc / 点关闭
/// 时置 `false`（`keep_open` 中转），等价于「取消」。
///
/// `analysis` 取可变借用：用户在对话框里的每次改动都即时写回配置，
/// 让下面的「预估」立刻反映新值。`board` 只读，用于线长钳制与局面数
/// 预估（与发起共用同一套 `build_plan` 钳制）。
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    analysis: &mut AnalysisState,
    board: &Board,
) -> BatchScanAction {
    let mut action = BatchScanAction::None;
    // egui 的 Window::open 需要一个 &mut bool：关闭请求（Esc / X）经它
    // 回传，再同步给调用方的开关（否则菜单项的勾选态会对不上）。
    let mut keep_open = true;
    egui::Window::new("整谱快扫")
        .open(&mut keep_open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            action = body(ui, analysis, board);
        });
    if !keep_open {
        *open = false;
    }
    action
}

/// 对话框正文：配置编辑 → 预估 → 底部动作条。
fn body(ui: &mut Ui, analysis: &mut AnalysisState, board: &Board) -> BatchScanAction {
    let mut action = BatchScanAction::None;
    ui.set_min_width(360.0);

    // ---- 进行中：只显示进度与取消。配置此时已冻结在任务里，改了也
    // 不会影响在飞的那一批，显示出来反而误导 ----
    if let Some((deep, done, total, elapsed)) = analysis.batch_progress() {
        ui.add(
            egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                .show_percentage()
                .desired_height(14.0),
        );
        ui.label(format!(
            "{} {done} / {total} · 已用 {}",
            if deep { "加深" } else { "主扫描" },
            crate::ui::analysis::format_batch_elapsed(elapsed),
        ));
        ui.weak("快扫进行中，切到别的局面会自动取消。");
        ui.add_space(4.0);
        if wide_button(ui, "取消快扫").clicked() {
            action = BatchScanAction::Cancel;
        }
        return action;
    }

    // ---- 配置编辑：每帧取最新值，改动即时写回（见模块文档）----
    let mut draft = analysis.batch_config().clone();
    let len = board.line_len();
    let mut changed = false;
    settings_body(ui, &mut draft, len, &mut changed);
    if changed {
        analysis.set_batch_config(draft.clone());
    }

    // ---- 预估（与发起共用同一份配置、同一套 build_plan 钳制）----
    ui.add_space(4.0);
    ui.separator();
    let estimate = batch_estimate(board, &draft);
    ui.label(format!(
        "预计 {} 个局面，约 {}",
        estimate.positions,
        format_estimate_secs(estimate.secs),
    ));
    if estimate.heavy() {
        ui.colored_label(
            theme::colors::WARN,
            "局面较多，建议缩小起止范围或关闭「含变着」。",
        );
    }

    // ---- 底部动作条：发起（空盘置灰并说明）/ 取消 ----
    ui.add_space(6.0);
    let empty = len == 0;
    ui.horizontal(|ui| {
        let start = if empty {
            ui.add_enabled(false, egui::Button::new("开始快扫"))
                .on_disabled_hover_text("空盘无谱可扫")
        } else {
            ui.add(egui::Button::new("开始快扫"))
        };
        let cancel = ui.add(egui::Button::new("取消"));
        if start.clicked() {
            action = BatchScanAction::Start;
        } else if cancel.clicked() {
            action = BatchScanAction::Close;
        }
    });
    action
}

/// 等宽铺满的动作按钮（与侧栏 `wide_button` 同款观感）。
fn wide_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add_sized(Vec2::new(ui.available_width(), 0.0), egui::Button::new(text))
}

/// 配置编辑区（起止手数 / 每手 visits / 只扫一方 / 含变着 / 自动加深），
/// 逐项改动置 `changed`（由调用方一次性写回）。
fn settings_body(ui: &mut Ui, draft: &mut BatchConfig, len: usize, changed: &mut bool) {
    // 起止手数（1 基手数编辑；内部 0 基局面序号）。止手 ≤ 起手时由
    // 发起端钳制（这里显示时也做同样钳制，保持所见即所扫）。
    ui.horizontal(|ui| {
        ui.weak("起手");
        let from = (draft.from.min(len.saturating_sub(1)) + 1) as i32;
        let mut from_edit = from;
        if ui
            .add(egui::DragValue::new(&mut from_edit).range(1..=len as i32))
            .changed()
        {
            draft.from = (from_edit.max(1) as usize - 1).min(len.saturating_sub(1));
            *changed = true;
        }
        ui.weak("止手");
        // to = usize::MAX 显示为线尾（编辑后落为具体值）。
        let to_disp = draft.to.min(len) as i32;
        let mut to_edit = to_disp;
        if ui
            .add(egui::DragValue::new(&mut to_edit).range(1..=len as i32))
            .changed()
        {
            draft.to = (to_edit.max(1) as usize).min(len);
            *changed = true;
        }
    });
    // 每手 visits：常用档位分段选择（当前值不在档位内则由后面的
    // DragValue 显示真实值并带「自定」后缀）。
    ui.horizontal(|ui| {
        ui.weak("每手visits");
        for &visits in &BATCH_VISITS_PRESETS {
            let selected = draft.visits == visits;
            if ui
                .add(egui::Button::selectable(selected, visits.to_string()))
                .clicked()
            {
                draft.visits = visits;
                *changed = true;
            }
        }
        // 常用档位之外的可编辑数值（DragValue 无独立文字着色 API，
        // 用「自定」后缀提示当前不在档位内）。
        let value = egui::DragValue::new(&mut draft.visits)
            .range(1..=10_000)
            .custom_formatter(|v, _| {
                if BATCH_VISITS_PRESETS.contains(&(v as u32)) {
                    format!("{v}")
                } else {
                    format!("{v} 自定")
                }
            })
            .custom_parser(|s| s.trim().parse::<f64>().ok().map(|v| v as u32 as f64));
        if ui.add(value).changed() {
            draft.visits = draft.visits.max(1);
            *changed = true;
        }
    });
    // 只扫一方：只分析轮到该方行棋的局面（耗时砍半）。
    ui.horizontal(|ui| {
        ui.weak("扫");
        for side in [BatchSide::All, BatchSide::BlackOnly, BatchSide::WhiteOnly] {
            let selected = draft.side == side;
            if ui
                .add(egui::Button::selectable(selected, side.name()))
                .clicked()
            {
                draft.side = side;
                *changed = true;
            }
        }
    });
    if ui
        .checkbox(&mut draft.include_variations, "含变着（全树逐节点，慢）")
        .changed()
    {
        *changed = true;
    }
    // 扫完自动加深差异手（默认开）：治 40 visits 下吻合度并列 0、
    // 差异手排序噪声大的毛病。
    if ui
        .checkbox(&mut draft.deepen_enabled, "扫完自动加深差异手")
        .changed()
    {
        *changed = true;
    }
    if draft.deepen_enabled {
        ui.horizontal(|ui| {
            ui.weak("前");
            if ui
                .add(egui::DragValue::new(&mut draft.deepen_top).range(1..=50))
                .changed()
            {
                *changed = true;
            }
            ui.weak("手，加深到");
            if ui
                .add(egui::DragValue::new(&mut draft.deepen_visits).range(1..=10_000))
                .changed()
            {
                *changed = true;
            }
            ui.weak("visits");
        });
    }
    ui.weak(
        "含变着时对每个节点分析「到该节点为止 + 该节点深度」，\
             局面数随分支线性增长；起止手数同样按节点深度过滤。",
    );
}

/// 预估耗时的人类可读形式（秒级 < 60 直接给秒；超过给分钟 / 小时）。
fn format_estimate_secs(secs: f64) -> String {
    if secs < 60.0 {
        format!("约 {secs:.0} 秒")
    } else if secs < 3600.0 {
        format!("约 {} 分", (secs / 60.0).ceil() as u32)
    } else {
        format!("约 {:.1} 小时", secs / 3600.0)
    }
}
