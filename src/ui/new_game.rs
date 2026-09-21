//! 「新对局」设置窗口：棋盘尺寸 / 贴目 / 让子 / 执子 / 难度选择。
//!
//! 与引擎设置窗口（[`super::settings`]）同一形态：独立 `egui::Window`、
//! 编辑草稿与状态分离、动作交回调用方执行。确认后由 `app` 用
//! `Board::from_setup` 建盘（让子按标准星位摆放并置白先）、清空分析
//! 状态与胜率历史、进入对弈模式。难度只决定引擎走子的 visits 档位，
//! 随配置持久化，新对局窗口与侧栏「对局」分区改的是同一份设置。

use crate::board::Size;
use crate::engine::Difficulty;
use crate::play::GameSetup;
use crate::ui::analysis::EngineStatus;

/// 新对局窗口的动作（由调用方执行）。
#[derive(Default)]
pub enum NewGameAction {
    /// 无操作。
    #[default]
    None,
    /// 按当前草稿开始新对局。
    Start(GameSetup),
}

/// 新对局设置窗口状态：编辑草稿（独立保存，重复打开不丢）。
#[derive(Default)]
pub struct NewGameUi {
    draft: Option<GameSetup>,
}

impl NewGameUi {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前草稿（无则按 `current_difficulty` 补默认值）；「开始」按钮
    /// 直接返回它。难度默认值取当前生效配置（打开窗口前用户已在侧栏
    /// 选过的档位不被草稿覆盖回硬编码默认）。
    fn draft_mut(&mut self, current_difficulty: Difficulty) -> &mut GameSetup {
        self.draft
            .get_or_insert_with(|| GameSetup { difficulty: current_difficulty, ..GameSetup::default() })
    }
}

/// 绘制新对局窗口；`open` 为窗口开关（标题栏关闭按钮会写回 `false`）。
///
/// `engine` 用于入口守卫：引擎未配置 / 启动失败时禁用「开始新对局」
/// （给原因，不静默失败）；「启动中」允许——对局可先摆好，就绪后
/// 自动开始分析并应手。
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    state: &mut NewGameUi,
    engine: &EngineStatus,
    current_difficulty: Difficulty,
) -> NewGameAction {
    let mut action = NewGameAction::None;
    egui::Window::new("新对局")
        .open(open)
        .default_width(320.0)
        .show(ctx, |ui| body(ui, state, engine, current_difficulty, &mut action));
    action
}

fn body(
    ui: &mut egui::Ui,
    state: &mut NewGameUi,
    engine: &EngineStatus,
    current_difficulty: Difficulty,
    action: &mut NewGameAction,
) {
    let draft = state.draft_mut(current_difficulty);
    egui::Grid::new("new_game_grid")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label("棋盘尺寸");
            ui.horizontal(|ui| {
                for n in [9u8, 13, 19] {
                    let size = Size::new(n).expect("枚举值均为合法尺寸");
                    ui.radio_value(&mut draft.size, size, format!("{n}路"));
                }
            });
            ui.end_row();

            ui.label("贴目");
            ui.add(
                egui::DragValue::new(&mut draft.komi)
                    .speed(0.5)
                    .range(0.0..=30.0)
                    .suffix(" 目"),
            );
            ui.end_row();

            ui.label("让子");
            ui.horizontal(|ui| {
                // 0 / 1 语义相同（无让子黑先），只给 0 与 2..=9。
                let options = [0usize, 2, 3, 4, 5, 6, 7, 8, 9];
                for h in options {
                    ui.selectable_value(
                        &mut draft.handicap,
                        h,
                        if h == 0 { "无".to_owned() } else { h.to_string() },
                    );
                }
            });
            ui.end_row();

            ui.label("我执");
            ui.horizontal(|ui| {
                ui.radio_value(&mut draft.human, crate::board::Stone::Black, "黑");
                ui.radio_value(&mut draft.human, crate::board::Stone::White, "白");
            });
            ui.end_row();

            ui.label("难度");
            ui.vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    for d in Difficulty::ALL {
                        ui.radio_value(&mut draft.difficulty, d, d.name());
                    }
                });
                ui.weak(format!(
                    "引擎走子 {} visits，预计每手约 {} 秒",
                    draft.difficulty.visits(),
                    draft.difficulty.estimate_secs()
                ));
            });
            ui.end_row();
        });

    ui.add_space(8.0);
    // 让子 > 1 时白先（标准让子棋惯例），提前说明避免误解。
    if draft.effective_handicap() > 0 {
        ui.weak("让子局：黑棋按星位预摆，白方先行。");
    }

    // 主按钮：琥珀填充全宽（与侧栏「新对局…」同一强调风格）。
    let start_button = egui::Button::new(
        egui::RichText::new("开始新对局").strong().color(crate::ui::theme::colors::ACCENT_BAR),
    )
    .fill(crate::ui::theme::colors::ACCENT_DIM)
    .stroke(egui::Stroke::new(1.0, crate::ui::theme::colors::ACCENT_BAR))
    .min_size(egui::vec2(ui.available_width(), 0.0));
    let start = ui
        .add_enabled(disabled_reason(engine).is_none(), start_button)
        .on_disabled_hover_text(
            disabled_reason(engine).map_or_else(|| "".to_owned(), |reason| reason.to_owned()),
        );
    if start.clicked() {
        *action = NewGameAction::Start(*draft);
    }
}

/// 「开始新对局」禁用原因；`None` = 可用。引擎未配置权重或处于可重试的
/// 失败态时给出可读提示（启动中不禁：局面可先建好，就绪后自动分析）。
fn disabled_reason(engine: &EngineStatus) -> Option<&'static str> {
    match engine {
        EngineStatus::Unconfigured => Some("尚未配置引擎权重，无法对弈。请先在设置中选择权重文件。"),
        EngineStatus::Failed(_) => Some("引擎不可用，请先在设置中修复并重启引擎。"),
        EngineStatus::Starting | EngineStatus::Ready => None,
    }
}
