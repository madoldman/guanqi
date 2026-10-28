//! 引擎设置窗口（TASKS 3.3）：编辑引擎路径 / 权重 / 思考量并持久化。
//!
//! KataGo 是外部依赖，由用户自备：这里列出的都是用户可配置项，不含任何
//! 本机预设。引擎的构建/后端（OpenCL 版 / Eigen 版）取决于用户安装的
//! 引擎二进制，不是本项目的设置项，故不在此处暴露。
//! 修改引擎路径 / 权重后必须**重启引擎进程**才生效——「保存并重启
//! 引擎」按钮一并完成持久化与重启触发（返回 [`SettingsAction::ApplyRestart`]）；
//! 仅「保存」则写入磁盘并更新当前配置，引擎在下次启动时生效。

use std::path::{Path, PathBuf};

use egui::{Context, Ui};

use crate::engine::{find_katago_in_path, save_settings, scan_weights, EngineConfig};
use crate::ui::theme;

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
        visits: u32,
        /// 搜索线程数（写入 `analysis.cfg` 的 `numSearchThreads`）。
        search_threads: u32,
        /// 规则草稿：`None` = 自动（跟随棋谱）；`Some(规则)` = 显式指定。
        rules: Option<crate::engine::Rules>,
        /// 激进度偏置（查询级 overrideSettings，0 = 引擎默认）。
        playout_doubling_advantage: f64,
        /// 分析广度（查询级 overrideSettings，0 = 引擎默认）。
        wide_root_noise: f64,
    }

impl SettingsUi {
    pub fn new(cfg: &EngineConfig) -> Self {
        let mut this = Self {
            draft: Draft {
                engine_path: cfg.engine_path.display().to_string(),
                model_path: cfg.model_path.clone(),
                weights_dir: cfg.weights_dir.display().to_string(),
                visits: cfg.visits,
                search_threads: cfg.search_threads,
                rules: cfg.rules.as_deref().and_then(crate::engine::Rules::from_wire),
                playout_doubling_advantage: cfg.playout_doubling_advantage,
                wide_root_noise: cfg.wide_root_noise,
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
            visits: self.draft.visits.max(1),
            search_threads: self.draft.search_threads.max(1),
            analysis_cfg: base.analysis_cfg.clone(),
            // 对弈难度不在本窗口编辑（侧栏 / 新对局窗口改），原值保留。
            play_difficulty: base.play_difficulty,
            // 规则存规范名（下拉只产规范名，防自由文本进 settings.json）。
            rules: self.draft.rules.map(|r| r.wire().to_owned()),
            // 分析偏置随草稿持久化；查询级字段，保存后 sync 比对自动重发
            // （PDA 变化会清空历史，无需重启引擎进程）。
            playout_doubling_advantage: self.draft.playout_doubling_advantage.clamp(-3.0, 3.0),
            wide_root_noise: self.draft.wide_root_noise.clamp(0.0, 3.0),
            // 时限制式与「新对局规则」不在本窗口编辑（新对局窗口改），
            // 原值保留。
            time_system: base.time_system,
            new_game_rules: base.new_game_rules,
            // 界面偏好不在本窗口编辑（App 侧管理），原值保留。
            ui_prefs: base.ui_prefs.clone(),
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
        .default_width(520.0)
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
                if path_row(ui, &mut state.draft.engine_path, "在 PATH 中查找")
                    && let Some(found) = find_katago_in_path()
                {
                    state.draft.engine_path = found.display().to_string();
                }
            });
            ui.end_row();

            ui.label("权重目录");
            ui.horizontal(|ui| {
                if path_row(ui, &mut state.draft.weights_dir, "重新扫描") {
                    state.rescan();
                }
            });
            ui.end_row();

            ui.label("权重文件");
            weight_selector(ui, state);
            ui.end_row();

            ui.label("思考量");
            ui.vertical(|ui| {
                ui.add_sized(
                    [96.0, 0.0],
                    egui::DragValue::new(&mut state.draft.visits)
                        .range(1..=1_000_000)
                        .suffix(" visits"),
                )
                .on_hover_text(
                    "visits = 引擎搜索时对每个候选点的模拟访问次数，总和即思考量。\
                     越大越强、越慢：本机实测（b18 权重 + OpenCL）约 60 visits/秒，\
                     300 visits ≈ 5–6 秒，2000 visits ≈ 半分钟。",
                );
                ui.weak("访问次数，越大越强越慢（悬停看量级）");
            });
            ui.end_row();

            // 搜索线程数：写入 analysis.cfg 的 numSearchThreads。此前无
            // 控件（只能手改配置文件），且该值只在首次生成配置时生效。
            // 现在「保存并重启引擎」会把它同步进配置文件，重启即生效。
            ui.label("搜索线程");
            ui.vertical(|ui| {
                ui.add_sized(
                    [96.0, 0.0],
                    egui::DragValue::new(&mut state.draft.search_threads)
                        .range(1..=256)
                        .suffix(" 线程"),
                )
                .on_hover_text(
                    "KataGo 搜索线程数（写入 analysis.cfg 的 numSearchThreads）。\
                     OpenCL 只做网络前向、搜索全在 CPU 线程上，线程越多越能喂满 GPU 批。\
                     实测（b18 + 680M iGPU）：16 线程比 8 快 17–21%（2000 visits：\
                     23.8s vs 28.6s），界面未被饿死；若个别机器发滞可降到物理核数。\
                     修改后需「保存并重启引擎」生效。",
                );
                ui.weak("实测 16 比 8 快 17–21%；发滞可降物理核数");
            });
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
            let rules_combo = egui::ComboBox::from_id_salt("analysis_rules")
                .selected_text(selected)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.draft.rules, None, auto);
                    for rule in crate::engine::Rules::ALL {
                        ui.selectable_value(&mut state.draft.rules, Some(rule), rule.name());
                    }
                });
            // 悬停说明挂在返回的按钮响应上：解释规则口径为何影响胜率数字。
            rules_combo
                .response
                .on_hover_text(
                    "分析引擎按哪套规则计算胜率与目差（数子 / 数目口径不同，\
                     同一局面差可达约 1 目）。「自动」按载入棋谱的 RU[] 属性\
                     宽容映射；谱上未写则按中国规则。",
                );
            ui.end_row();

            // 激进度偏置（PDA）：查询级 overrideSettings，恒随查询显式发送。
            // ⚠️ 非 0 扭曲评估（探针实测 ±2 → 同局面目差 ±8–10 目）——
            // 这是搜索偏置不是「换视角」，复盘求准确数字时必须保持 0。
            ui.label("激进度");
            ui.vertical(|ui| {
                ui.add_sized(
                    [96.0, 0.0],
                    egui::DragValue::new(&mut state.draft.playout_doubling_advantage)
                        .speed(0.1)
                        .range(-3.0..=3.0),
                )
                .on_hover_text(
                    "playoutDoublingAdvantage：正值使引擎偏实地 / 激进，负值偏厚势 / 缓慢。\
                     ⚠️ 非 0 会直接扭曲胜率与目差（实测 +2 → 目差 +9.7），仅用于\
                     对弈风格与研究向实验；改动会清空逐手历史并重查，无需重启引擎。",
                );
                ui.weak("0 = 关闭；非 0 会扭曲评估（研究用）");
            });
            ui.end_row();

            // 分析广度（WRN）：非 0 时候选分布变宽而评估几乎不变（实测
            // 2.0 → 首选占比 0.68→0.14、目差漂移在噪声内）。
            ui.label("分析广度");
            ui.vertical(|ui| {
                ui.add_sized(
                    [96.0, 0.0],
                    egui::DragValue::new(&mut state.draft.wide_root_noise)
                        .speed(0.1)
                        .range(0.0..=3.0),
                )
                .on_hover_text(
                    "wideRootNoise：加大根节点噪声，让搜索探索更多候选点。\
                     实测 2.0 时首选 visits 占比从 68% 降到 14%，胜率 / 目差几乎不变。\
                     改动后自动重查，无需重启引擎。",
                );
                ui.weak("0 = 默认；调大可看更多候选（评估不变）");
            });
            ui.end_row();
        });

    ui.add_space(8.0);
    // 底部动作条（主题层统一形态：自然宽度、右对齐、主操作琥珀描边）。
    if let Some(index) = theme::action_row(
        ui,
        &[
            theme::Action { label: "保存并重启引擎", primary: true, enabled: true },
            theme::Action { label: "保存", primary: false, enabled: true },
        ],
    ) {
        match index {
            0 => {
                if state.save(cfg) {
                    *action = SettingsAction::ApplyRestart;
                }
            }
            _ => {
                state.save(cfg);
            }
        }
    }
    if let Some((ok, text)) = &state.message {
        let color = if *ok {
            theme::colors::OK
        } else {
            theme::colors::ERROR
        };
        ui.colored_label(color, text);
    }
    ui.add_space(4.0);
    ui.weak("修改引擎路径 / 权重 / 搜索线程后，需要重启引擎进程才能生效。");
}

/// 「输入框吃剩余宽 + 按钮自然宽」的路径编辑行，返回按钮是否被点击。
///
/// 原实现是固定 330px 的输入框硬塞进格子：窗口偏窄时挤压同行按钮、
/// 偏宽时右侧留一截空白。改为右到左布局先落按钮（自然宽），输入框
/// 自动占满剩余宽度，窗口怎么拉都贴合。
fn path_row(ui: &mut Ui, text: &mut String, button_label: &str) -> bool {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let clicked = ui.button(button_label).clicked();
        // 输入框在 right_to_left 布局中后画（占据按钮左侧的剩余宽）。
        ui.add_sized([ui.available_width(), 0.0], egui::TextEdit::singleline(text));
        clicked
    })
    .inner
}

/// 权重下拉选择：显示文件名与体积，按训练步数新→旧（`scan_weights` 已排序）。
/// 已配置路径**不存在**时（权重被移动 / 删除）以红色如实标注——此前
/// 只回显文件名，失效配置被一直当成有效值，引擎启动必然失败也看不出来。
fn weight_selector(ui: &mut Ui, state: &mut SettingsUi) {
    // 失效标注独立于下拉：即便目录扫描不出候选，也显示路径状态行。
    if let Some(path) = &state.draft.model_path
        && !path.is_file()
    {
        ui.colored_label(
            theme::colors::ERROR,
            format!("权重文件不存在：{}", path.display()),
        );
    }
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
        .width(ui.available_width())
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
