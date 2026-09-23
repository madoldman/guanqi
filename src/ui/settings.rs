//! 引擎设置窗口（TASKS 3.3）：编辑引擎路径 / 权重 / 后端 / 思考量并持久化。
//!
//! KataGo 是外部依赖：这里列出的都是用户可配置项，不含任何本机预设。
//! 修改引擎路径 / 权重 / 后端后必须**重启引擎进程**才生效——「保存并重启
//! 引擎」按钮一并完成持久化与重启触发（返回 [`SettingsAction::ApplyRestart`]）；
//! 仅「保存」则写入磁盘并更新当前配置，引擎在下次启动时生效。

use std::path::{Path, PathBuf};

use egui::{Color32, Context, Ui};

use crate::engine::{
    find_katago_in_path, save_settings, scan_weights, EngineBackend, EngineConfig,
};

/// 设置窗口状态（编辑草稿 + 权重候选缓存 + 提示）。
pub struct SettingsUi {
    /// 编辑中的草稿；保存成功后才并入生效配置。
    draft: Draft,
    /// 权重目录扫描缓存。
    weights: Vec<PathBuf>,
    /// 最近一次操作的结果提示（是否成功，文本）。
    message: Option<(bool, String)>,
}

    /// 界面编辑态（路径以字符串编辑，保存时转换）。
    struct Draft {
        engine_path: String,
        model_path: Option<PathBuf>,
        weights_dir: String,
        backend: EngineBackend,
        visits: u32,
        /// 规则草稿：`None` = 自动（跟随棋谱）；`Some(规则)` = 显式指定。
        rules: Option<crate::engine::Rules>,
    }

impl SettingsUi {
    pub fn new(cfg: &EngineConfig) -> Self {
        let mut this = Self {
            draft: Draft {
                engine_path: cfg.engine_path.display().to_string(),
                model_path: cfg.model_path.clone(),
                weights_dir: cfg.weights_dir.display().to_string(),
                backend: cfg.backend,
                visits: cfg.visits,
                rules: cfg.rules.as_deref().and_then(crate::engine::Rules::from_wire),
            },
            weights: Vec::new(),
            message: None,
        };
        this.rescan();
        this
    }

    fn rescan(&mut self) {
        self.weights = scan_weights(Path::new(&self.draft.weights_dir));
    }

    /// 草稿 → 配置（界面未暴露的项沿用当前生效值）。
    fn to_config(&self, base: &EngineConfig) -> EngineConfig {
        EngineConfig {
            engine_path: PathBuf::from(self.draft.engine_path.trim()),
            model_path: self.draft.model_path.clone(),
            weights_dir: PathBuf::from(self.draft.weights_dir.trim()),
            backend: self.draft.backend,
            visits: self.draft.visits.max(1),
            search_threads: base.search_threads,
            analysis_cfg: base.analysis_cfg.clone(),
            // 对弈难度不在本窗口编辑（侧栏 / 新对局窗口改），原值保留。
            play_difficulty: base.play_difficulty,
            // 规则存规范名（下拉只产规范名，防自由文本进 settings.json）。
            rules: self.draft.rules.map(|r| r.wire().to_owned()),
            // 时限制式与「新对局规则」不在本窗口编辑（新对局窗口改），
            // 原值保留。
            time_system: base.time_system,
            new_game_rules: base.new_game_rules,
        }
    }

    /// 保存草稿；成功时并入生效配置并返回 `true`。
    fn save(&mut self, cfg: &mut EngineConfig) -> bool {
        let next = self.to_config(cfg);
        match save_settings(&next) {
            Ok(()) => {
                *cfg = next;
                self.message = Some((true, "已保存。".to_owned()));
                true
            }
            Err(text) => {
                self.message = Some((false, text));
                false
            }
        }
    }
}

/// 设置窗口的动作（由调用方执行）。
#[derive(Default)]
pub enum SettingsAction {
    /// 无操作。
    #[default]
    None,
    /// 已保存配置，要求立即用新配置重启引擎。
    ApplyRestart,
}

/// 绘制设置窗口；`open` 为窗口开关（标题栏关闭按钮会写回 `false`）。
pub fn show(
    ctx: &Context,
    open: &mut bool,
    state: &mut SettingsUi,
    cfg: &mut EngineConfig,
) -> SettingsAction {
    let mut action = SettingsAction::None;
    egui::Window::new("引擎设置")
        .open(open)
        .default_width(480.0)
        .show(ctx, |ui| body(ui, state, cfg, &mut action));
    action
}

fn body(ui: &mut Ui, state: &mut SettingsUi, cfg: &mut EngineConfig, action: &mut SettingsAction) {
    egui::Grid::new("settings_grid")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label("引擎路径");
            ui.horizontal(|ui| {
                ui.add_sized(
                    [330.0, 20.0],
                    egui::TextEdit::singleline(&mut state.draft.engine_path),
                );
                if ui.button("在 PATH 中查找").clicked()
                    && let Some(found) = find_katago_in_path()
                {
                    state.draft.engine_path = found.display().to_string();
                }
            });
            ui.end_row();

            ui.label("权重目录");
            ui.horizontal(|ui| {
                ui.add_sized(
                    [330.0, 20.0],
                    egui::TextEdit::singleline(&mut state.draft.weights_dir),
                );
                if ui.button("重新扫描").clicked() {
                    state.rescan();
                }
            });
            ui.end_row();

            ui.label("权重文件");
            weight_selector(ui, state);
            ui.end_row();

            ui.label("后端");
            ui.horizontal(|ui| {
                ui.radio_value(&mut state.draft.backend, EngineBackend::OpenCL, "OpenCL（GPU）");
                ui.radio_value(&mut state.draft.backend, EngineBackend::Eigen, "Eigen（CPU）");
            });
            ui.end_row();

            ui.label("思考量");
            ui.add(
                egui::DragValue::new(&mut state.draft.visits)
                    .range(1..=1_000_000)
                    .suffix(" visits"),
            );
            ui.end_row();

            // 规则下拉：自动（跟随棋谱 RU[]）/ 六种规范规则。改动后随
            // 「保存」持久化；分析层（AnalysisState）检测到规则变化会
            // 自动重发查询并清空旧规则下的历史数据。
            ui.label("规则");
            let auto = "自动（跟随棋谱）";
            let selected = state
                .draft
                .rules
                .map_or(auto, |r| r.name());
            egui::ComboBox::from_id_salt("analysis_rules")
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.draft.rules, None, auto);
                    for rule in crate::engine::Rules::ALL {
                        ui.selectable_value(&mut state.draft.rules, Some(rule), rule.name());
                    }
                });
            ui.end_row();
        });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        // 两按钮等宽（去掉总间距后均分可用宽），主操作「保存并重启引擎」
        // 用琥珀描边强调。
        let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
        if ui.add_sized([width, 0.0], egui::Button::new("保存")).clicked() {
            state.save(cfg);
        }
        let restart = egui::Button::new("保存并重启引擎")
            .stroke(egui::Stroke::new(1.0, crate::ui::theme::colors::ACCENT_BAR));
        if ui.add_sized([width, 0.0], restart).clicked()
            && state.save(cfg)
        {
            *action = SettingsAction::ApplyRestart;
        }
    });
    if let Some((ok, text)) = &state.message {
        let color = if *ok {
            Color32::from_rgb(140, 220, 140)
        } else {
            Color32::from_rgb(255, 120, 110)
        };
        ui.colored_label(color, text);
    }
    ui.add_space(4.0);
    ui.weak("修改引擎路径 / 权重 / 后端后，需要重启引擎进程才能生效。");
}

/// 权重下拉选择：显示文件名与体积，按训练步数新→旧（`scan_weights` 已排序）。
fn weight_selector(ui: &mut Ui, state: &mut SettingsUi) {
    if state.weights.is_empty() {
        ui.weak("目录中未找到 *.bin.gz 权重");
        return;
    }
    let selected = state
        .draft
        .model_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "未选择".to_owned());
    egui::ComboBox::from_id_salt("weight_file")
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for path in &state.weights {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                let label = format!("{}（{:.0} MB）", name, size as f64 / (1024.0 * 1024.0));
                ui.selectable_value(&mut state.draft.model_path, Some(path.clone()), label);
            }
        });
}
