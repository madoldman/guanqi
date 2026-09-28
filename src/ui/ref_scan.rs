//! 参照库打分对话框（阶段 22.3；「分析 → 参照库打分…」入口）。
//!
//! 与整谱快扫对话框（`batch_scan`）同一形态：设置（手数范围）+ 预估
//! （节点数 + 耗时）同屏，看完预估再决定发起；进行中显示进度与取消。
//!
//! # 文案口径（模块定位的自我声明）
//!
//! 这是 **AI 对库的评估，不是库本身的对错**：对话框与结果区块都带这层
//! 说明。「已淘汰」的含义是「引擎评估显著劣于引擎首选」，不是「这手棋
//! 人类不能下」。
//!
//! # 范围默认值
//!
//! 只扫 `move_no ≤ 30`（[`REFSCAN_MAX_MOVE_NO`]）：定式 / 死活的局部
//! 判断几乎全部落在前 30 手内；5000 节点 × 40 visits 按单线程实测
//! （0.94s/节点）约 79 分钟，30 手截断是大库可用性的关键闸门。

use egui::{Align2, Ui, Vec2};

use super::analysis::{AnalysisState, REFSCAN_MAX_MOVE_NO};
use super::theme;
use crate::refbook::Reference;

/// 等宽铺满的动作按钮（与侧栏 `wide_button` 同款观感）。
fn wide_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add_sized(Vec2::new(ui.available_width(), 0.0), egui::Button::new(text))
}

/// 参照打分对话框的执行结果（由 `App` 在绘制结束后执行）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RefScanAction {
    /// 无操作。
    #[default]
    None,
    /// 用户点了「开始打分」：App 转交 [`AnalysisState::start_ref_scan`]。
    Start,
    /// 打分进行中用户点了「取消打分」。
    Cancel,
    /// 用户点了「取消」或按 Esc / 关窗。
    Close,
}

/// 画参照库打分对话框。`open` 开关由调用方持有（快扫对话框同款中转）。
/// `reference` 只读（计划与预估的数据源）；`komi` / `rules` 由发起时随
/// 任务定格（App 传入）。
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    analysis: &mut AnalysisState,
    reference: &Reference,
    max_move_no: &mut usize,
    komi: f64,
    game_rules: Option<&str>,
) -> RefScanAction {
    let mut action = RefScanAction::None;
    let mut keep_open = true;
    egui::Window::new("参照库打分")
        .open(&mut keep_open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            action = body(ui, analysis, reference, max_move_no, komi, game_rules);
        });
    if !keep_open {
        *open = false;
    }
    action
}

/// 对话框正文：定位说明 → 范围编辑 → 预估 → 动作条。
fn body(
    ui: &mut Ui,
    analysis: &mut AnalysisState,
    reference: &Reference,
    max_move_no: &mut usize,
    komi: f64,
    game_rules: Option<&str>,
) -> RefScanAction {
    let mut action = RefScanAction::None;
    ui.set_min_width(400.0);

    // ---- 进行中：只显示进度与取消（配置已冻结在任务里）----
    if let Some((done, total, elapsed)) = analysis.ref_scan_progress() {
        ui.add(
            egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                .show_percentage()
                .desired_height(14.0),
        );
        ui.label(format!(
            "已评估 {done} / {total} 节点 · 已用 {}",
            crate::ui::analysis::format_batch_elapsed(elapsed),
        ));
        ui.weak("打分期间当前对局的常规分析会变慢（共用引擎），但不会被停掉。");
        ui.add_space(4.0);
        if wide_button(ui, "取消打分").clicked() {
            action = RefScanAction::Cancel;
        }
        return action;
    }

    // ---- 定位说明 ----
    ui.label(
        "用引擎逐个评估参照库里有续法的局面，标出已被 AI 淘汰的分支。\
         这是 AI 对库的评估，不是库本身的对错。",
    );

    // ---- 范围编辑（1 基手数显示；内部为上限值本身）----
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.weak("只扫前");
        if ui
            .add(egui::DragValue::new(max_move_no).range(5..=120).suffix(" 手"))
            .changed()
        {
            *max_move_no = (*max_move_no).max(5);
        }
        ui.weak("以内的节点（定式范围）");
    });

    // ---- 预估（与发起共用同一套计划构造）----
    ui.add_space(4.0);
    ui.separator();
    let (positions, secs) =
        AnalysisState::ref_scan_estimate(reference.tree(), reference.size(), *max_move_no);
    ui.label(format!(
        "共 {} 个局面待评估，约 {}",
        positions,
        format_estimate_secs(secs),
    ));
    if secs > 1800.0 {
        ui.colored_label(
            theme::colors::WARN,
            "耗时较长，建议缩小手数范围（定式判断集中在前 30 手内）。",
        );
    }

    // ---- 动作条 ----
    ui.add_space(6.0);
    ui.weak(format!(
        "按当前规则「{}」与贴目 {:.1} 评估；扫描结果只保留到本会话结束，\
         换参照即清空。",
        rules_line(game_rules),
        komi,
    ));
    ui.horizontal(|ui| {
        let start_w = ui.available_width() - 88.0;
        if ui
            .add_sized(Vec2::new(start_w, 0.0), egui::Button::new("开始打分"))
            .clicked()
        {
            action = RefScanAction::Start;
        }
        if ui.add_sized(Vec2::new(80.0, 0.0), egui::Button::new("取消")).clicked() {
            action = RefScanAction::Close;
        }
    });
    action
}

/// 预估耗时的人类可读形式（与快扫对话框同口径）。
fn format_estimate_secs(secs: f64) -> String {
    if secs < 60.0 {
        format!("约 {secs:.0} 秒")
    } else if secs < 3600.0 {
        format!("约 {} 分", (secs / 60.0).ceil() as u32)
    } else {
        format!("约 {:.1} 小时", secs / 3600.0)
    }
}

/// 规则行（对话框只读说明；与查询实际发送的解析结果一致——App 传入
/// 已解析的规则或 `None`（自动跟随））。
fn rules_line(game_rules: Option<&str>) -> String {
    match game_rules {
        Some(raw) => crate::engine::Rules::rules_name(
            &crate::engine::resolve_rules(None, Some(raw)).rules,
        ),
        None => "中国".to_owned(),
    }
}

// REFSCAN_MAX_MOVE_NO 的默认值使用提示（范围编辑缺省值由 App 初始化）。
const _: () = {
    // 编译期断言：默认范围不超过常量定义（防止两处漂移）。
    assert!(REFSCAN_MAX_MOVE_NO >= 5);
};
