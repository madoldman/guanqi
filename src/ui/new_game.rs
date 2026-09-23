//! 「新对局」设置窗口：棋盘尺寸 / 贴目 / 让子 / 执子 / 难度 / 时限 / 规则。
//!
//! 与引擎设置窗口（[`super::settings`]）同一形态：独立 `egui::Window`、
//! 编辑草稿与状态分离、动作交回调用方执行。确认后由 `app` 用
//! `Board::from_setup` 建盘（让子按标准星位摆放并置白先）、清空分析
//! 状态与胜率历史、进入对弈模式。难度只决定引擎走子的 visits 档位，
//! 随配置持久化，新对局窗口与侧栏「对局」分区改的是同一份设置。
//!
//! 时限（制式三选一：无限制 / 包干 / 读秒）与规则（六选一）同为新对局
//! 设置的一部分：**制式本身就是开关**——选了包干 / 读秒就按规则判负，
//! 不另设「是否判负」勾选项；**本局规则与时限都在对局开始时确定，
//! 对局中只读**（窗口里显式说明）。上次选择随 settings.json 持久化
//! （`EngineConfig::time_system` / `new_game_rules`），首次运行缺省
//! **无限制 + 中国规则**——默认值绝不能悄悄引入会判负的时限，用户
//! 主动选择才是时限的正确入口。新对局的规则**不经过**设置面板的
//! 「自动跟随棋谱」解析（新对局没有棋谱可跟随），查询直接用它，
//! 设置面板的规则偏好只作用于载入的棋谱。
//!
//! 贴目便利项：切到日本 / 韩国规则时贴目自动调 6.5、切回中国调 7.5
//! （数目规则的社区惯例）；用户手动改过贴目（[`NewGameUi::komi_touched`]）
//! 后不再自动调——便利项只服务「没在意贴目」的用户，不覆盖显式选择。

use crate::board::Size;
use crate::engine::{Difficulty, Rules};
use crate::play::{GameSetup, TimeSystem};
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
    /// 贴目是否被用户手动改过（便利项停用标记）。判定：帧末比对草稿
    /// 贴目与上一帧值（`prev_komi`）——不直接在控件回调里置位：草稿的
    /// 可变借用活跨整个编辑区，`state` 无法在编辑区内写回；帧末比对
    /// 语义等价且借用干净。
    komi_touched: bool,
    /// 上一帧的草稿贴目（`None` = 尚未建草稿）。
    prev_komi: Option<f64>,
}

impl NewGameUi {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前草稿（无则按当前生效配置补默认值）；难度与时限制式 / 规则的
    /// 默认值取当前生效配置（打开窗口前用户选过的档位 / 上次新对局的
    /// 选择不被草稿覆盖回硬编码默认）。
    fn draft_mut(
        &mut self,
        current_difficulty: Difficulty,
        current_time_system: TimeSystem,
        current_rules: Rules,
    ) -> &mut GameSetup {
        self.draft.get_or_insert_with(|| GameSetup {
            difficulty: current_difficulty,
            time_system: current_time_system,
            rules: current_rules,
            ..GameSetup::default()
        })
    }
}

/// 绘制新对局窗口；`open` 为窗口开关（标题栏关闭按钮会写回 `false`）。
///
/// `engine` 用于入口守卫：引擎未配置 / 启动失败时禁用「开始新对局」
/// （给原因，不静默失败）；「启动中」允许——对局可先摆好，就绪后
/// 自动开始分析并应手。`current_time_system` / `current_rules` 为
/// 持久化的上一次选择（草稿未建时的缺省选中项）。
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    state: &mut NewGameUi,
    engine: &EngineStatus,
    current_difficulty: Difficulty,
    current_time_system: TimeSystem,
    current_rules: Rules,
) -> NewGameAction {
    let mut action = NewGameAction::None;
    egui::Window::new("新对局")
        .open(open)
        .default_width(320.0)
        .show(ctx, |ui| {
            body(
                ui,
                state,
                engine,
                current_difficulty,
                current_time_system,
                current_rules,
                &mut action,
            )
        });
    // 帧末比对贴目（手改检测）：草稿借用已随编辑块结束，state 可写。
    if let Some(draft) = state.draft.as_ref() {
        match state.prev_komi {
            None => state.prev_komi = Some(draft.komi),
            Some(prev) if prev != draft.komi => {
                state.komi_touched = true;
                state.prev_komi = Some(draft.komi);
            }
            _ => {}
        }
    }
    action
}

fn body(
    ui: &mut egui::Ui,
    state: &mut NewGameUi,
    engine: &EngineStatus,
    current_difficulty: Difficulty,
    current_time_system: TimeSystem,
    current_rules: Rules,
    action: &mut NewGameAction,
) {
    // 贴目便利项开关读进局部（草稿可变借用只活到编辑块结束，开始动作
    // 在块外执行）。
    let komi_touched = state.komi_touched;
    let start_clicked;
    {
        let draft = state.draft_mut(current_difficulty, current_time_system, current_rules);
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
                // 手改贴目的检测在 show 的帧末比对（见 komi_touched 文档）。
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

                ui.label("规则");
                ui.vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        for rule in Rules::ALL {
                            let selected = draft.rules == rule;
                            if ui
                                .add(egui::Button::selectable(selected, rule.name()))
                                .clicked()
                            {
                                draft.rules = rule;
                                // 便利项：未手改贴目时，日本 / 韩国规则
                                // 默认 6.5 目、中国 7.5（数目规则的社区
                                // 惯例）；其余规则不动贴目。
                                if !komi_touched {
                                    match rule {
                                        Rules::Japanese | Rules::Korean => draft.komi = 6.5,
                                        Rules::Chinese => draft.komi = 7.5,
                                        _ => {}
                                    }
                                }
                            }
                        }
                    });
                    ui.weak("本局按此规则分析，另存写入 RU[]。");
                });
                ui.end_row();

                // 时限制式（三选一）：制式本身就是「是否判负」的开关——
                // 选包干 / 读秒即按规则判负，无另设勾选项。读秒参数只在
                // 读秒制下展开编辑。
                ui.label("时限");
                ui.vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        for system in [
                            TimeSystem::Unlimited,
                            TimeSystem::Absolute { seconds: 0.0 },
                            TimeSystem::Byoyomi {
                                main_seconds: 0.0,
                                period_seconds: 0.0,
                                periods: 0,
                            },
                        ] {
                            // 逐制式比对变体类型（参数不参与相等性——编辑
                            // 中的草稿值不该让段落失焦）。
                            let selected = std::mem::discriminant(&draft.time_system)
                                == std::mem::discriminant(&system);
                            if ui
                                .add(egui::Button::selectable(selected, system.name()))
                                .clicked()
                            {
                                // 切制式时保留用户已调好的参数：包干沿用
                                // 旧总时长，读秒沿用旧参数（首次切换从该
                                // 制式的常规缺省起步）。
                                let old = draft.time_system;
                                draft.time_system = match system {
                                    TimeSystem::Unlimited => TimeSystem::Unlimited,
                                    TimeSystem::Absolute { .. } => match old {
                                        TimeSystem::Absolute { seconds } => {
                                            TimeSystem::Absolute { seconds }
                                        }
                                        _ => TimeSystem::Absolute { seconds: 600.0 },
                                    },
                                    TimeSystem::Byoyomi { .. } => match old {
                                        TimeSystem::Byoyomi {
                                            main_seconds,
                                            period_seconds,
                                            periods,
                                        } => TimeSystem::Byoyomi {
                                            main_seconds,
                                            period_seconds,
                                            periods,
                                        },
                                        _ => TimeSystem::Byoyomi {
                                            main_seconds: 600.0,
                                            period_seconds: 30.0,
                                            periods: 3,
                                        },
                                    },
                                };
                            }
                        }
                    });
                    match draft.time_system {
                        TimeSystem::Unlimited => {
                            ui.weak("不封顶、不判负；界面显示双方累计用时。");
                        }
                        TimeSystem::Absolute { seconds } => {
                            ui.horizontal(|ui| {
                                ui.weak("每方总时长");
                                let mut mins = (seconds / 60.0).max(1.0);
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut mins)
                                            .range(1..=180)
                                            .suffix(" 分"),
                                    )
                                    .changed()
                                {
                                    draft.time_system =
                                        TimeSystem::Absolute { seconds: mins * 60.0 };
                                }
                            });
                            ui.weak("用尽即判负（B+T / W+T）。");
                        }
                        TimeSystem::Byoyomi { main_seconds, period_seconds, periods } => {
                            ui.horizontal(|ui| {
                                ui.weak("主时间");
                                let mut mins = (main_seconds / 60.0).max(1.0);
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut mins)
                                            .range(1..=180)
                                            .suffix(" 分"),
                                    )
                                    .changed()
                                {
                                    draft.time_system = TimeSystem::Byoyomi {
                                        main_seconds: mins * 60.0,
                                        period_seconds,
                                        periods,
                                    };
                                }
                                ui.weak("读秒");
                                let mut secs = period_seconds.max(1.0);
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut secs)
                                            .range(3..=60)
                                            .suffix(" 秒"),
                                    )
                                    .changed()
                                {
                                    draft.time_system = TimeSystem::Byoyomi {
                                        main_seconds,
                                        period_seconds: secs,
                                        periods,
                                    };
                                }
                                ui.weak("×");
                                let mut times = periods.max(1);
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut times)
                                            .range(1..=10)
                                            .suffix(" 次"),
                                    )
                                    .changed()
                                {
                                    draft.time_system = TimeSystem::Byoyomi {
                                        main_seconds,
                                        period_seconds,
                                        periods: times,
                                    };
                                }
                            });
                            ui.weak(
                                "主时间用尽后每手须在读秒内落子，超一手耗一次，次数用完判负。",
                            );
                        }
                    }
                    ui.weak("时限与规则在对局开始时确定，对局中不可修改。");
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
        start_clicked = start.clicked();
    }
    // 开始动作在草稿借用结束后执行（GameSetup 是 Copy，直接读）。
    if start_clicked {
        *action = NewGameAction::Start(*state.draft.as_ref().expect("草稿已在上面的编辑块建立"));
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
