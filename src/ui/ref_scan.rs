//! 参照库打分对话框（阶段 22.3；「分析 → 参照库打分…」入口）。
//!
//! 与整谱快扫对话框（`batch_scan`）同一形态：设置（手数范围 + 思考量
//! 档位）+ 预估（节点数 + 耗时）同屏，看完预估再决定发起；进行中显示
//! 进度与取消。
//!
//! # 状态单一来源（快扫同款模式）
//!
//! 对话框**不持有**跨帧的配置副本：每帧从 [`AnalysisState::ref_scan_config`]
//! 读、改动即刻写回 [`AnalysisState::set_ref_scan_config`]。预估与发起读
//! 的必然是同一份配置，「预估按 A 档算、发起按 B 档跑」的错配不可能发生。
//!
//! # 文案口径（模块定位的自我声明）
//!
//! 这是 **AI 对库的评估，不是库本身的对错**：对话框与结果区块都带这层
//! 说明。「已淘汰」的含义是「引擎评估显著劣于引擎首选」，不是「这手棋
//! 人类不能下」。档位同理：提高 visits 只是让引擎候选表更长、可判定
//! 样本更充分，**不代表判定更「正确」**。
//!
//! # 默认值
//!
//! 手数上限 30（[`REFSCAN_MAX_MOVE_NO`]）、每局面 40 visits
//! （[`REFSCAN_VISITS`]）：与阶段 22.3 的既有行为一致，未动过档位的
//! 用户扫描结果不变。5000 节点 × 40 visits 按单线程实测（0.94s/节点）
//! 约 79 分钟，30 手截断是大库可用性的关键闸门。

use egui::{Align2, Ui};

use super::analysis::{AnalysisState, REFSCAN_VISITS};
use super::theme;
use crate::refbook::Reference;

/// 参照打分「每局面 visits」的预设档位（分段选择 + 「自定义」编辑共存，
/// 快扫 `BATCH_VISITS_PRESETS` 同款模式）。
///
/// 40 = 默认档（与快扫同源，吞吐实测基准）；100 / 200 = 提检出率档
/// （候选表更长、「引擎未列出」更少）。不再往上铺细档的理由：定式库
/// 扫描本就以「节点数 × 线性耗时」计，档位过细只会让预估数字眼花，
/// 检出率收益在 200 以上边际递减；需要非整档的用户走自定义编辑。
const REFSCAN_VISITS_PRESETS: [u32; 3] = [40, 100, 200];

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
            action = body(ui, analysis, reference, komi, game_rules);
        });
    if !keep_open {
        *open = false;
    }
    action
}

/// 对话框正文：定位说明 → 范围 / 档位编辑 → 预估 → 动作条。
fn body(
    ui: &mut Ui,
    analysis: &mut AnalysisState,
    reference: &Reference,
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
        if theme::wide_button(ui, "取消打分").clicked() {
            action = RefScanAction::Cancel;
        }
        return action;
    }

    // ---- 定位说明 ----
    ui.label(
        "用引擎逐个评估参照库里有续法的局面，标出已被 AI 淘汰的分支。\
         这是 AI 对库的评估，不是库本身的对错。",
    );

    // ---- 配置编辑：每帧取最新值，改动即时写回（快扫同款，见模块文档）----
    let mut config = analysis.ref_scan_config();
    let mut changed = false;

    // ---- 范围编辑（1 基手数显示；内部为上限值本身）----
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.weak("只扫前");
        if ui
            .add_sized(
                [72.0, 0.0],
                egui::DragValue::new(&mut config.max_move_no).range(5..=120).suffix(" 手"),
            )
            .changed()
        {
            config.max_move_no = config.max_move_no.max(5);
            changed = true;
        }
        ui.weak("以内的节点（定式范围）");
    });

    // ---- 思考量档位（预设档位 + 自定义编辑共存；预估随档位线性变化）----
    ui.horizontal(|ui| {
        ui.weak("每局面");
        for &visits in &REFSCAN_VISITS_PRESETS {
            let selected = config.visits == visits;
            if ui
                .add(egui::Button::selectable(selected, visits.to_string()))
                .clicked()
            {
                config.visits = visits;
                changed = true;
            }
        }
        // 预设档之外的可编辑数值（DragValue 无独立文字着色 API，
        // 用「自定」后缀提示当前不在预设档内——快扫同款做法）。
        let value = egui::DragValue::new(&mut config.visits)
            .range(20..=500)
            .custom_formatter(|v, _| {
                if REFSCAN_VISITS_PRESETS.contains(&(v as u32)) {
                    format!("{v}")
                } else {
                    format!("{v} 自定")
                }
            })
            .custom_parser(|s| s.trim().parse::<f64>().ok().map(|v| v as u32 as f64));
        if ui.add_sized([88.0, 0.0], value).changed() {
            config.visits = config.visits.max(20);
            changed = true;
        }
        ui.weak("visits");
    });
    ui.weak(
        "档位越高候选表越长，「引擎未列出」越少、可判定样本越多；\
         耗时按倍数增加，但不代表判定更正确。",
    );

    if changed {
        analysis.set_ref_scan_config(config);
    }

    // ---- 预估（与发起共用同一套计划构造与配置）----
    ui.add_space(4.0);
    ui.separator();
    let (positions, secs) =
        AnalysisState::ref_scan_estimate(reference.tree(), reference.size(), config);
    ui.label(format!(
        "共 {} 个局面待评估，约 {}",
        positions,
        format_estimate_secs(secs),
    ));
    if secs > 1800.0 {
        ui.colored_label(
            theme::colors::WARN,
            "耗时较长，建议缩小手数范围或降低每局面 visits。",
        );
    }

    // ---- 动作条（主题层统一形态：右对齐、自然宽度、主操作琥珀描边；
    // 原先「取消」硬编码 80px 而主按钮被拉满剩余宽，长短失衡）----
    ui.add_space(6.0);
    ui.weak(format!(
        "按当前规则「{}」与贴目 {komi:.1} 评估；扫描结果只保留到本会话结束，\
         换参照即清空。",
        rules_line(game_rules),
    ));
    match theme::action_row(
        ui,
        &[
            theme::Action { label: "开始打分", primary: true, enabled: true },
            theme::Action { label: "取消", primary: false, enabled: true },
        ],
    ) {
        Some(0) => action = RefScanAction::Start,
        Some(_) => action = RefScanAction::Close,
        None => {}
    }
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

// 默认档常量的编译期自检（防档位表与默认值漂移；`contains` 非 const，
// 手写比较）。
const _: () = {
    assert!(REFSCAN_VISITS_PRESETS[0] == REFSCAN_VISITS
        || REFSCAN_VISITS_PRESETS[1] == REFSCAN_VISITS
        || REFSCAN_VISITS_PRESETS[2] == REFSCAN_VISITS);
};
