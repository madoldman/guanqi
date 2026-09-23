//! 分析侧栏：引擎状态、胜率 / 目差、候选点列表、失误统计、叠加层开关
//! 与提示行（TASKS 4.2 / 4.4）。
//!
//! 数据只读自 [`AnalysisState`]：胜率 / 目差为**黑方视角**（引擎配置
//! `reportAnalysisWinratesAs = BLACK`，实测对 rootInfo 与 moveInfos 同时生效），
//! 不按行棋方翻转。棋盘候选点叠加 / 热度图 / 失误标注（`overlay` 模块）
//! 直接取 `AnalysisState` 的快照与历史缓冲，本面板只提供层开关、失误汇总、
//! 胜率色阶图例与候选点点击定位。
//!
//! 呈现结构（纯外观，动作仍经 [`PanelAction`] 交回 `App` 执行）：各分区用
//! [`theme::card_frame`] 包成圆角卡片；难度做成分段选择器；棋谱文档与
//! 候选点做成全宽按钮行；全部可交互项都有底色 / 边框 / hover 高亮。

use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::board::{Board, Coord, IllegalReason, Size, Stone};
use crate::engine::{Difficulty, EngineConfig, RootInfo};
use crate::play::{PlayState, resign_text};
use crate::sgf::GameMeta;

use super::analysis::{
    display_values, AnalysisState, BatchSide, DisplayView, EngineStatus, GameSummary, Severity,
    WORST_LIMIT, batch_estimate,
};
use super::explain;
use super::overlay::{self, Overlay};
use super::theme;

/// 侧栏展示的候选点条数（空盘时引擎可回上百条，只取前几条；
/// 与棋盘叠加层 `CANDIDATE_LIMIT` 解耦，各自维护）。
const MOVE_LIMIT: usize = 6;

/// 主变（PV）显示 / 幽灵子虚影共用的截断手数：侧栏 PV 文本行与棋盘
/// 预览（`overlay::GHOST_LIMIT`）必须一致——用户看到的文本和棋盘上
/// 摆出的预览是同一个主变的前缀，两处各写一个数迟早漂移。
pub(crate) const PV_LIMIT: usize = 8;

/// 侧栏按钮触发的动作（由调用方在绘制结束后执行，避免借用冲突）。
#[derive(Default)]
pub enum PanelAction {
    /// 无操作。
    #[default]
    None,
    /// 请求用当前配置重启引擎（错误重试按钮）。
    RetryEngine,
    /// 点击候选点行：在棋盘上定位该点（再点同一行取消，由 App 处理）。
    Focus {
        /// 候选落点。
        at: Coord,
        /// 主变幽灵子（已剔除断着与盘面已有棋子的点由绘制端处理）。
        ghosts: Vec<(Coord, Stone)>,
    },
    /// 人类点了「弃着」按钮。
    HumanPass,
    /// 人类点了「认输」按钮。
    HumanResign,
    /// 从当前手创建研究副本（App 完成实际创建与切换）。
    CreateCopy,
    /// 点击文档列表项：切换到该编号的文档（App 完成整体互换）。
    SwitchDoc(usize),
    /// 丢弃该编号的研究副本（有研究成果时由 App 先弹确认框）。
    DropCopy(usize),
    /// 确认「引擎无望」提示（不再重复提示；是否判引擎认输由用户另行决定）。
    AckHopeless,
    /// 切换人机对弈难度档位（App 持久化；下一手应手即生效）。
    SetDifficulty(Difficulty),
    /// 打开「新对局」设置窗口。
    OpenNewGame,
    /// 候选点行点了「排除」：把该手加入 / 移出 avoidMoves（App 转交
    /// [`AnalysisState::toggle_avoid`]，区域模式下不生效）。
    ToggleAvoid {
        /// 行棋方（记录时取当前行棋方）。
        player: Stone,
        /// 被排除的落点。
        at: Coord,
    },
    /// 点了排除列表某行的「移除」按钮（下标 = 列表行号）。
    RemoveAvoid(usize),
    /// 「清空排除」按钮：清空全部 avoidMoves。
    ClearAvoid,
    /// 「清除全部限制」按钮：区域与排除一并清除。
    ClearLimits,
    /// 区域开关切换（`Some(())` = 请求开启，`None` = 请求关闭并清除区域）。
    /// 开启只需改模式（区域等用户在棋盘上拖出）。
    SetRegion(Option<()>),
    /// 策略热度图层开关切换：opt-in 数据，需转入 [`AnalysisState`]
    /// 重发查询（`sync` 比对开关状态后重查，局面未变也重发）。
    SetWantPolicy(bool),
    /// 「候选点领地」开关切换（opt-in `includeMovesOwnership`）：转入
    /// [`AnalysisState::set_want_moves_ownership`]，机制与策略层相同。
    SetWantMovesHeat(bool),
    /// 点了候选行的「沿主变前进」：沿该候选的 PV 逐手**预览前进**——
    /// 只在已存在的着法上导航（每手要求当前节点已有匹配的子分支，
    /// 否则停住），**不新建分支、不改棋谱树**。App 完成实际导航。
    AdvancePv {
        /// 主变首手（与 PV 同源，弃着行不出现该动作）。
        at: Coord,
        /// 主变序列（首手起，`None` = 弃着），截断到 [`PV_LIMIT`]。
        pv: Vec<Option<Coord>>,
    },
    /// 点了「整谱快扫」：按侧栏配置批量分析（App 转交
    /// [`AnalysisState::start_batch`]，报告按 turnNumber 回填逐手历史）。
    StartBatch,
    /// 整谱快扫配置有改动：转入 [`AnalysisState::set_batch_config`]
    /// （预估与发起共用这份配置）。
    SetBatchConfig(crate::ui::analysis::BatchConfig),
    /// 点了「取消快扫」：terminate 在飞批量查询并结束任务。
    CancelBatch,
    /// 点了「局后统计」排行榜某行：跳转到该手（App 调
    /// [`crate::board::Board::go_to`]，曲线 / 棋盘定位随局面联动）。
    GotoTurn(usize),
    /// 候选类显示门控模式切换（第 5 项）：转入
    /// [`crate::ui::analysis::AnalysisState::set_gating`]。只改显示判定，
    /// 不重发查询、不影响走子决策。
    SetGating(crate::ui::analysis::CandidateGating),
    /// 目数视角切换（永远黑视角 / 黑白交替）：转入
    /// [`crate::ui::analysis::AnalysisState::set_display_view`]。变化会
    /// 作废快照并重发查询（视角是查询级 overrideSettings 字段）；历史
    /// 数据两用，不清空。
    SetDisplayView(crate::ui::analysis::DisplayView),
}

/// 从当前门控模式提取延迟秒数（分段选择器构造「延迟」段时沿用当前值，
/// 避免用户调好的秒数在切模式时被重置）。
fn gating_delay(gating: super::analysis::CandidateGating) -> u32 {
    match gating {
        super::analysis::CandidateGating::Delayed { secs } => secs,
        _ => 3,
    }
}

/// 「打开棋谱」流程的用户可见提示（App 写入，随侧栏提示行显示）。
#[derive(Clone, Debug)]
pub enum LoadNotice {
    /// 载入成功（完整）。
    Ok(String),
    /// 需要留意：部分载入（非法着法提前停止）、已有对话框在等待等。
    Warn(String),
    /// 打开失败（原棋盘保留不动）。
    Failed(String),
}

impl LoadNotice {
    /// 提示文本。
    pub fn text(&self) -> &str {
        match self {
            Self::Ok(text) | Self::Warn(text) | Self::Failed(text) => text,
        }
    }

    /// 提示配色（与侧栏既有提示一致：绿 = 成功，橙 = 留意，红 = 失败）。
    fn color(&self) -> Color32 {
        match self {
            Self::Ok(_) => theme::colors::OK,
            Self::Warn(_) => theme::colors::WARN,
            Self::Failed(_) => theme::colors::ERROR,
        }
    }
}

/// 侧栏「棋谱」区的文档列表项（由 App 从活动文档 + 驻留副本现场派生）。
pub struct DocEntry {
    /// 文档的稳定编号（原谱 0，副本创建时单调分配；切换 / 丢弃动作
    /// 以此定位，丢弃其它副本后编号不变）。
    pub number: usize,
    /// 列表显示名：原谱 = 文件名；副本 = 「研究副本 N」。
    pub name: String,
    /// 创建前缀手数（`None` = 原谱；副本为 `Some(n)`）。
    pub from_move: Option<usize>,
    /// 该副本的研究成果手数（原谱恒 0）。
    pub research: usize,
    /// 是否当前活动文档（列表高亮）。
    pub active: bool,
}

/// 状态文本与配色。流式分析时显示实时 visits 进度（随中间报告刷新），
/// 比单纯「分析中…」更能反映引擎正在推进。
fn status_label(analysis: &AnalysisState) -> (String, Color32) {
    match &analysis.engine {
        EngineStatus::Unconfigured => ("未配置权重".to_owned(), theme::colors::WARN),
        EngineStatus::Starting => ("引擎启动中…".to_owned(), theme::colors::WARN),
        EngineStatus::Ready => {
            if let Some(snapshot) = analysis
                .snapshot
                .as_ref()
                .filter(|snapshot| !snapshot.is_final)
                .or(analysis
                    .analyzing()
                    .then_some(analysis.snapshot.as_ref())
                    .flatten())
            {
                // 在飞查询的中间报告实时可达（root.visits 随搜索推进递增）。
                let visits = snapshot.root.as_ref().map_or(0, |root| root.visits);
                (
                    format!("分析中 {} visits（上限 {}）", visits, snapshot.visits_cap),
                    theme::colors::OK,
                )
            } else if analysis.analyzing() {
                ("分析中…".to_owned(), theme::colors::OK)
            } else {
                ("就绪".to_owned(), theme::colors::OK)
            }
        }
        EngineStatus::Failed(_) => ("引擎错误".to_owned(), theme::colors::ERROR),
    }
}

/// 权重文件名（无路径）。
fn model_name(cfg: &EngineConfig) -> String {
    cfg.model_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map_or_else(|| "未配置".to_owned(), |n| n.to_string_lossy().into_owned())
}

/// 胜率 / 目差文本（黑方视角）。
///
/// 显示换算统一走 `card_winrate` 内的 [`display_values`]（按当前视角）；
/// 本函数保留给不按视角换算的黑视角文本场景。
#[allow(dead_code)]
fn eval_lines(root: &RootInfo) -> (String, String) {
    (
        format!("黑方胜率 {:.1}%", root.winrate * 100.0),
        format!("目差 {:+.1}", root.score_lead),
    )
}

/// 用分区卡片包住一段内容：卡片底色 + 细边框 + 圆角 + 内边距。
///
/// 布局注意：这里用 `ui.vertical`（默认 top_down 布局）即可 —— 容器 Ui
/// 的 `max_rect` 已经是面板可用宽，Frame 会自然横向铺满；**不要**用
/// `allocate_ui_with_layout + with_cross_justify + set_min_width` 的组合
/// 造一个固定宽度的子区域（egui 0.36 下该模式会把 Panel 的自然宽度
/// 反复撑大，面板被逐帧挤满整个窗口）。
fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    ui.vertical(|ui| theme::card_frame().show(ui, add_contents).inner)
        .inner
}

/// 全宽强调按钮（主操作：新对局 / 复制副本 / 重试引擎等）。
/// 不固定填充色，保留全局 hover / active 三态底色反馈。
fn primary_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 0.0],
        egui::Button::new(
            RichText::new(text)
                .strong()
                .color(Color32::from_rgb(255, 196, 96)),
        ),
    )
}

/// 全宽普通按钮（侧栏内统一宽度，行动作走 `Response`）。
fn wide_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add_sized([ui.available_width(), 0.0], egui::Button::new(text))
}

/// 信息行：弱色前缀 + 正文值（引擎状态 / 棋谱属性这类「标签：值」行）。
fn info_line(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).weak());
        ui.label(RichText::new(value).size(12.5));
    });
}

/// 主变（PV）文本：`D4 → Q16 → …`，截断到 [`PV_LIMIT`] 手。
/// 坐标用 GTP 格式（跳 I）；`None`（弃着）显示「弃着」；
/// 被截断时以「 …」结尾提示还有后续。空 PV 返回 `None`（不显示该行）。
pub(crate) fn pv_text(pv: &[Option<Coord>], size: Size) -> Option<String> {
    if pv.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = pv
        .iter()
        .take(PV_LIMIT)
        .map(|c| c.map_or_else(|| "弃着".to_owned(), |c| c.to_gtp(size)))
        .collect();
    if pv.len() > PV_LIMIT {
        parts.push("…".to_owned());
    }
    Some(parts.join(" → "))
}

/// 绘制分析侧栏。`settings_open` 由本面板与顶部按钮共享；
/// `overlay` 为棋盘叠加层的层开关与定位状态（本面板读写）；
/// `curve_open` / `tree_open` 为胜率曲线 / 棋谱树底部面板的显示开关。
/// `manual_revealed` 为候选类手动显示模式下「用户已按 F」标志
/// （App 持有并随局面变化复位，本面板只读）。
/// `board` 提供总手数与各行棋方（失误汇总按手数现场派生）。
/// `game` / `comment` 为「打开棋谱」相关信息：已载入棋谱的元信息与
/// 当前手注释；`load_notice` / `save_notice` 为最近一次打开 / 另存
/// 操作的提示（无则对应段落不显示）。
/// `play` 为人机对弈状态；`new_game_open` 为新对局窗口开关（共享）；
/// `hopeless` 为引擎无望提示文本（`Some` = 显示提示与确认按钮）。
/// `docs` 为文档列表（原谱 + 各研究副本，活动项高亮；空盘时为空）。
/// 注释可能很长，整体包一层垂直滚动，避免侧栏内容被裁剪。
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    analysis: &AnalysisState,
    board: &Board,
    cfg: &EngineConfig,
    notice: Option<IllegalReason>,
    startup_notice: Option<&str>,
    settings_open: &mut bool,
    overlay: &mut Overlay,
    curve_open: &mut bool,
    tree_open: &mut bool,
    manual_revealed: bool,
    game: Option<&GameMeta>,
    comment: Option<&str>,
    load_notice: Option<&LoadNotice>,
    save_notice: Option<&LoadNotice>,
    persist_notice: Option<&str>,
    play: &mut PlayState,
    new_game_open: &mut bool,
    hopeless: Option<&str>,
    docs: &[DocEntry],
) -> PanelAction {
    egui::ScrollArea::vertical()
        .show(ui, |ui| {
            panel_body(
                ui,
                analysis,
                board,
                cfg,
                notice,
                startup_notice,
                settings_open,
                overlay,
                curve_open,
                tree_open,
                manual_revealed,
                game,
                comment,
                load_notice,
                save_notice,
                persist_notice,
                play,
                new_game_open,
                hopeless,
                docs,
            )
        })
        .inner
}

/// 侧栏正文（[`show`] 的滚动内容）：各分区卡片化排布。
#[allow(clippy::too_many_arguments)]
fn panel_body(
    ui: &mut Ui,
    analysis: &AnalysisState,
    board: &Board,
    cfg: &EngineConfig,
    notice: Option<IllegalReason>,
    startup_notice: Option<&str>,
    settings_open: &mut bool,
    overlay: &mut Overlay,
    curve_open: &mut bool,
    tree_open: &mut bool,
    manual_revealed: bool,
    game: Option<&GameMeta>,
    comment: Option<&str>,
    load_notice: Option<&LoadNotice>,
    save_notice: Option<&LoadNotice>,
    persist_notice: Option<&str>,
    play: &mut PlayState,
    new_game_open: &mut bool,
    hopeless: Option<&str>,
    docs: &[DocEntry],
) -> PanelAction {
    let mut action = PanelAction::None;

    // ---- 对局（人机开关 / 难度 / 对局操作）----
    action = card_play(
        ui,
        analysis,
        board,
        cfg,
        play,
        new_game_open,
        hopeless,
        action,
    );

    // ---- 引擎状态 ----
    card_engine(ui, analysis, cfg, game, settings_open, &mut action);

    // ---- 棋谱信息（打开棋谱后显示；属性存在才显示对应行）----
    if let Some(game) = game {
        card_game(ui, game, comment, docs, &mut action);
    }

    // ---- 胜率 / 目差（含视角切换与网络直觉行）----
    card_winrate(ui, analysis, &mut action);

    // ---- 讲解（中文自动解说；终态数据驱动，流式期间只占位）----
    card_explain(ui, analysis, board);

    // ---- 限定选点（限定区域 / 排除选点）----
    card_limits(ui, analysis, board, &mut action);

    // ---- 候选点 ----
    card_candidates(ui, analysis, overlay, manual_revealed, &mut action);

    // ---- 失误统计（TASKS 4.4）----
    card_mistakes(ui, analysis, board);

    // ---- 局后统计（吻合度 + 最差 N 手，与失误卡片信息互补不重复：
    // 失误卡片 = 目差损失分级计数；本卡片 = visits 口径吻合度 + 排行跳转）----
    card_summary(ui, analysis, board, &mut action);

    // ---- 整谱快扫（批量分析当前线，曲线自动填满）----
    card_batch(ui, analysis, board, &mut action);

    // ---- 叠加层（返回策略层 / 候选点领地层开关是否变化，交由 App 转发重查）----
    let overlay_toggles = card_overlay(ui, overlay, analysis, curve_open, tree_open, &mut action);
    if overlay_toggles.policy_toggled {
        action = PanelAction::SetWantPolicy(overlay.show_policy);
    }
    if overlay_toggles.moves_heat_toggled {
        action = PanelAction::SetWantMovesHeat(overlay.show_moves_heat);
    }

    // ---- 消息（非法落子 / 载入另存 / 引擎错误 / 引擎字段警告等；无则不占位）----
    card_messages(
        ui,
        notice,
        startup_notice,
        load_notice,
        save_notice,
        persist_notice,
        &analysis.transient_error,
        analysis.engine_warnings(),
    );

    action
}

/// 「对局」卡片：人机对弈开关、难度分段选择器、对局状态与操作按钮。
#[allow(clippy::too_many_arguments)]
fn card_play(
    ui: &mut Ui,
    analysis: &AnalysisState,
    board: &Board,
    cfg: &EngineConfig,
    play: &mut PlayState,
    new_game_open: &mut bool,
    hopeless: Option<&str>,
    action: PanelAction,
) -> PanelAction {
    let mut action = action;
    card(ui, |ui| {
        theme::section_title(ui, "对局");
        ui.checkbox(&mut play.mode, "人机对弈");

        // 难度选择（对弈模式外也可预选）：五档 visits 预设的分段选择器，
        // 改变后引擎**下一手应手即生效**（无需重开对局）。显示各档 visits
        // 与预计等待，用户对「较强/最强要等十几秒到半分钟」有预期。
        ui.label(RichText::new("难度").weak());
        ui.add_space(2.0);
        egui::Frame::default()
            .fill(theme::colors::STATUS_BG)
            .corner_radius(6.0)
            .inner_margin(3.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                // 各段等分整行（预留 4 个段间空隙）。注意不能用
                // `Layout::with_main_justify` —— 水平布局下它会把每个
                // 控件都拉伸到整行宽，5 段就会把面板宽度棘轮式撑大。
                let seg_w = (ui.available_width() - 4.0 * ui.spacing().item_spacing.x)
                    / Difficulty::ALL.len() as f32;
                ui.horizontal(|ui| {
                    for &d in Difficulty::ALL.iter() {
                        let selected = cfg.play_difficulty == d;
                        let mut button = egui::Button::selectable(selected, d.name())
                            .min_size(Vec2::new(seg_w, 0.0));
                        if selected {
                            // 选中段：琥珀系填充 + 深琥珀描边（比默认
                            // selection 底更醒目，呈「实心段」观感）。
                            button = button
                                .fill(theme::colors::ACCENT_DIM)
                                .stroke(Stroke::new(1.0, theme::colors::ACCENT_BAR));
                        }
                        let response = ui.add(button).on_hover_text(format!(
                            "{} visits，预计每手约 {} 秒",
                            d.visits(),
                            d.estimate_secs()
                        ));
                        if response.clicked() {
                            action = PanelAction::SetDifficulty(d);
                        }
                    }
                });
            });
        ui.weak(format!(
            "当前：{}（{} visits，引擎每手预计约 {} 秒）",
            cfg.play_difficulty.name(),
            cfg.play_difficulty.visits(),
            cfg.play_difficulty.estimate_secs()
        ));

        if play.mode {
            let engine_ready = matches!(analysis.engine, EngineStatus::Ready);
            let finished = play.finished(board);
            let human_turn = !finished && board.to_play() == play.human;
            let engine_turn =
                !finished && !human_turn && engine_ready && board.cursor() == board.line_len();
            // 「思考中」两种情况：展示查询在飞，或展示已齐而走子口径查询
            // （按难度 visits）还在飞——后者才是应手快慢的决定因素。
            let engine_thinking = engine_turn
                && engine_ready
                && (analysis.analyzing() || analysis.play_pending(board, cfg.play_difficulty));
            ui.label(format!("你执{}", play.human.name()));
            if finished {
                let reason = match play.resigned {
                    Some(side) => format!("{}认输：{}", side.name(), resign_text(side)),
                    None => "对局结束：双方连续弃着".to_owned(),
                };
                ui.colored_label(theme::colors::WARN, reason);
                // 结束后进入纯复盘浏览：对弈开关保持，但不再自动应手
                // （决策函数的 two_passes / resigned 守卫兜底）。
            } else if engine_thinking {
                ui.colored_label(
                    theme::colors::OK,
                    format!(
                        "引擎思考中…（{}，约 {} 秒/手）",
                        cfg.play_difficulty.name(),
                        cfg.play_difficulty.estimate_secs()
                    ),
                );
            } else if human_turn {
                ui.colored_label(theme::colors::OK, "轮到你");
            } else {
                ui.weak("等待引擎…");
            }
            if !finished && human_turn && board.cursor() == board.line_len() {
                ui.add_space(2.0);
                // 弃着 / 认输等宽并排，像一组操作按钮而非文字。
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                    if ui
                        .add_sized([width, 0.0], egui::Button::new("弃着"))
                        .clicked()
                    {
                        action = PanelAction::HumanPass;
                    }
                    if ui
                        .add_sized([width, 0.0], egui::Button::new("认输"))
                        .clicked()
                    {
                        action = PanelAction::HumanResign;
                    }
                });
            }
        }
        ui.add_space(2.0);
        if primary_button(ui, "新对局…").clicked() {
            *new_game_open = true;
            action = PanelAction::OpenNewGame;
        }
        if let Some(text) = hopeless {
            ui.add_space(4.0);
            ui.colored_label(theme::colors::WARN, text);
            if wide_button(ui, "确认，继续对局").clicked() {
                action = PanelAction::AckHopeless;
            }
        }
    });
    action
}

/// 「引擎」卡片：状态点 + 权重 / 后端 / 思考量 / 规则信息与设置入口。
fn card_engine(
    ui: &mut Ui,
    analysis: &AnalysisState,
    cfg: &EngineConfig,
    game: Option<&GameMeta>,
    settings_open: &mut bool,
    action: &mut PanelAction,
) {
    card(ui, |ui| {
        theme::section_title(ui, "引擎");
        let (status_text, status_color) = status_label(analysis);
        ui.horizontal(|ui| {
            // 状态点：圆形色标，一眼可辨引擎健康状态。
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            ui.painter_at(rect)
                .circle_filled(rect.center(), 4.0, status_color);
            ui.label(RichText::new(status_text).color(status_color).strong());
        });
        if let EngineStatus::Failed(message) = &analysis.engine {
            ui.colored_label(status_color, message);
            let retry = ui
                .add(
                    egui::Button::new("重试启动引擎")
                        .stroke(Stroke::new(1.0, theme::colors::ERROR)),
                )
                .on_hover_text("用当前配置重新拉起引擎进程");
            if retry.clicked() {
                *action = PanelAction::RetryEngine;
            }
        }
        if matches!(analysis.engine, EngineStatus::Unconfigured) {
            ui.weak("请在设置中选择一个网络权重文件。");
        }
        if matches!(analysis.engine, EngineStatus::Starting) {
            ui.weak("模型加载或显卡调优可能需要数十秒，请稍候。");
        }
        info_line(ui, "权重", &model_name(cfg));
        info_line(ui, "后端", cfg.backend.name());
        info_line(ui, "思考量", &format!("{} visits", cfg.visits.max(1)));
        // 当前生效的规则：用户必须知道数字按哪套规则算（数子 / 数目差
        // 可达约 1 目）。显式指定显示规则名；自动则注明跟随棋谱。
        let rules_line = match cfg.rules.as_deref().and_then(crate::engine::Rules::from_wire) {
            Some(rule) => rule.name().to_owned(),
            None => match game.and_then(|meta| meta.info.rules.as_deref()) {
                Some(raw) => {
                    let mapped = crate::engine::resolve_rules(None, Some(raw));
                    format!("自动（棋谱「{raw}」→ {}）", mapped.rules)
                }
                None => "自动（棋谱未写规则，按中国）".to_owned(),
            },
        };
        info_line(ui, "规则", &rules_line);
        if let Some(snapshot) = &analysis.snapshot {
            info_line(
                ui,
                "最近耗时",
                &format!(
                    "{:.1} 秒（visits 上限 {}）",
                    snapshot.elapsed.as_secs_f32(),
                    snapshot.visits_cap
                ),
            );
        }
        ui.add_space(2.0);
        if wide_button(ui, "设置…").clicked() {
            *settings_open = !*settings_open;
        }
    });
}

/// 「棋谱」卡片：对局信息、当前手注释与文档列表（原谱 + 研究副本）。
fn card_game(
    ui: &mut Ui,
    game: &GameMeta,
    comment: Option<&str>,
    docs: &[DocEntry],
    action: &mut PanelAction,
) {
    card(ui, |ui| {
        theme::section_title(ui, "棋谱");

        // 当前文档标签（原谱 / 研究副本 N）：让「现在看的是哪份」一眼可见。
        let tag = match docs.iter().find(|entry| entry.active) {
            Some(entry) if entry.from_move.is_some() => {
                format!(
                    "研究副本 {}（自第 {} 手起）",
                    entry.number,
                    entry.from_move.unwrap_or(0)
                )
            }
            _ => "原谱".to_owned(),
        };
        let tag_color = if tag == "原谱" {
            Color32::from_rgb(120, 200, 255)
        } else {
            theme::colors::OK
        };
        ui.label(RichText::new(tag).color(tag_color).strong());

        let file = game.source.file_name().map_or_else(
            || "（无文件名）".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let file_label = ui.label(RichText::new(file).strong());
        file_label.on_hover_text(game.source.display().to_string());
        let info = &game.info;
        if let Some(line) = player_line(&info.player_black, &info.rank_black) {
            info_line(ui, "黑", &line);
        }
        if let Some(line) = player_line(&info.player_white, &info.rank_white) {
            info_line(ui, "白", &line);
        }
        if let Some(result) = &info.result {
            info_line(ui, "结果", result);
        }
        if let Some(komi) = info.komi {
            info_line(ui, "贴目", &komi.to_string());
        }
        if info.handicap > 0 {
            info_line(ui, "让子", &info.handicap.to_string());
        }
        if let Some(date) = &info.date {
            info_line(ui, "日期", date);
        }
        if let Some(event) = &info.event {
            info_line(ui, "赛事", event);
        }
        if let Some(rules) = &info.rules {
            info_line(ui, "规则", rules);
        }
        // 当前手的 `C` 注释（无则不显示）。
        if let Some(text) = comment {
            ui.add_space(2.0);
            ui.label(text);
        }

        // ---- 文档列表（原谱 + 各研究副本；点击切换，副本可丢弃）----
        // 列表放在侧栏滚动区内，副本多时随侧栏一起滚动。
        ui.add_space(4.0);
        match docs {
            [] => {
                ui.weak("打开棋谱后可从当前手复制研究副本。");
            }
            entries => {
                ui.weak("点击切换文档：");
                for entry in entries {
                    doc_row(ui, entry, action);
                }
                ui.add_space(4.0);
                // 创建入口：任何已载入文档（原谱或副本）的当前手皆可再开副本。
                if primary_button(ui, "复制为研究副本").clicked() {
                    *action = PanelAction::CreateCopy;
                }
            }
        }
    });
}

/// 文档列表的一行：全宽按钮行，活动项琥珀填充 + 左侧强调条；
/// 行尾「丢弃」按钮仅副本有，与整行点击区域互不重叠。
fn doc_row(ui: &mut Ui, entry: &DocEntry, action: &mut PanelAction) {
    let row_height = 34.0;
    // 行尾动作按钮宽度；原谱无丢弃按钮，右侧留空 54px 对齐。
    let tail = 54.0;
    ui.horizontal(|ui| {
        let row_width = (ui.available_width() - tail).max(60.0);
        let (rect, row) = ui.allocate_exact_size(Vec2::new(row_width, row_height), Sense::click());
        let row = row.on_hover_text(if entry.active {
            "当前文档".to_owned()
        } else {
            "点击切换到该文档".to_owned()
        });
        let painter = ui.painter_at(rect);
        // 三态：活动 = 琥珀暗底；hover = 控件亮底；常态 = 卡片内凹底。
        let fill = if entry.active {
            theme::colors::ACCENT_DIM
        } else if row.hovered() || row.is_pointer_button_down_on() {
            Color32::from_rgb(52, 57, 70)
        } else {
            Color32::from_rgb(39, 43, 53)
        };
        painter.rect_filled(rect, 6.0, fill);
        if entry.active {
            // 左侧强调条（琥珀），与棋盘分支选择器同源。
            let bar = Rect::from_min_size(
                rect.min + Vec2::new(3.0, 4.0),
                Vec2::new(3.0, row_height - 8.0),
            );
            painter.rect_filled(bar, 1.5, theme::colors::ACCENT_BAR);
        }
        if entry.active || row.is_pointer_button_down_on() {
            painter.rect_stroke(
                rect,
                6.0,
                Stroke::new(1.0, theme::colors::ACCENT_DEEP),
                StrokeKind::Middle,
            );
        } else if row.hovered() {
            painter.rect_stroke(
                rect,
                6.0,
                Stroke::new(1.0, Stroke::new(1.0, theme::colors::ACCENT_DEEP).color),
                StrokeKind::Middle,
            );
        }
        // 两行排版：文档名 + 副行（来源手数 / 研究成果）。
        let name_color = if entry.active {
            Color32::from_rgb(255, 214, 140)
        } else if entry.from_move.is_some() {
            Color32::from_rgb(208, 212, 220)
        } else {
            Color32::from_rgb(126, 196, 250)
        };
        painter.text(
            rect.min + Vec2::new(12.0, 6.0),
            Align2::LEFT_TOP,
            &entry.name,
            FontId::proportional(12.5),
            name_color,
        );
        let sub = match entry.from_move {
            None => "原谱".to_owned(),
            Some(from) if entry.research > 0 => {
                format!("自第 {from} 手起 · 研究成果 {} 手", entry.research)
            }
            Some(from) => format!("自第 {from} 手起"),
        };
        painter.text(
            rect.min + Vec2::new(12.0, row_height - 13.0),
            Align2::LEFT_TOP,
            &sub,
            FontId::proportional(10.0),
            Color32::from_rgb(150, 156, 166),
        );
        if row.clicked() {
            *action = PanelAction::SwitchDoc(entry.number);
        }

        // 行尾「丢弃」按钮：仅副本有；独立交互区，不与整行点击冲突。
        if entry.from_move.is_some() {
            let (drect, drop) = ui.allocate_exact_size(Vec2::new(44.0, row_height), Sense::click());
            let drop_hover = drop.hovered() || drop.is_pointer_button_down_on();
            let painter = ui.painter_at(drect);
            painter.rect_filled(
                drect,
                6.0,
                if drop_hover {
                    Color32::from_rgb(72, 44, 46)
                } else {
                    Color32::from_rgb(48, 42, 46)
                },
            );
            painter.rect_stroke(
                drect,
                6.0,
                Stroke::new(
                    1.0,
                    if drop_hover {
                        theme::colors::ERROR
                    } else {
                        Color32::from_rgb(78, 60, 62)
                    },
                ),
                StrokeKind::Middle,
            );
            painter.text(
                drect.center(),
                Align2::CENTER_CENTER,
                "丢弃",
                FontId::proportional(11.0),
                if drop_hover {
                    Color32::from_rgb(255, 150, 140)
                } else {
                    Color32::from_rgb(196, 168, 168)
                },
            );
            let drop = drop.on_hover_text(if entry.research > 0 {
                format!(
                    "丢弃研究副本 {}（含 {} 手研究成果，将确认）",
                    entry.number, entry.research
                )
            } else {
                format!("丢弃研究副本 {}", entry.number)
            });
            if drop.clicked() {
                *action = PanelAction::DropCopy(entry.number);
            }
        }
    });
}

/// 「讲解」卡片：中文自动解说（`ui::explain` 逐行渲染，按档位着色）。
/// 数据全部来自引擎终态报告与逐手历史；流式中间报告期间只显示占位。
fn card_explain(ui: &mut Ui, analysis: &AnalysisState, board: &Board) {
    card(ui, |ui| {
        theme::section_title(ui, "讲解");
        for line in explain::explain(analysis, board) {
            let color = match line.tone {
                explain::Tone::Normal => Color32::from_rgb(214, 218, 226),
                explain::Tone::Good => theme::colors::OK,
                explain::Tone::Warn => theme::colors::WARN,
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(line.text).size(12.5).color(color));
            });
        }
    });
}

/// 「限定选点」卡片：限定区域开关（开启后在棋盘上拖框）、被排除的手
/// 列表（逐项移除 + 清空）与一键清除。区域与排除互斥（引擎实测
/// allowMoves 与 avoidMoves 不能同时给出），区域优先、排除暂不生效。
fn card_limits(ui: &mut Ui, analysis: &AnalysisState, board: &Board, action: &mut PanelAction) {
    let limits = analysis.limits();
    card(ui, |ui| {
        theme::section_title(ui, "限定选点");
        // 区域开关：开启后棋盘进入框选模式（拖框 / 点两下对角 / 右键清除），
        // 关闭即清除区域。开启时忽略落子，候选点随新查询实时刷新。
        let mut region_on = limits.has_region();
        if ui
            .checkbox(&mut region_on, "限定区域（在棋盘上拖框）")
            .changed()
        {
            *action = PanelAction::SetRegion(region_on.then_some(()).and(None));
        }
        ui.weak("拖框选定区域后，引擎只考虑区域内的空点。");
        if let Some(region) = limits.region {
            let size = board.size();
            ui.label(format!(
                "区域 {}–{}（{}×{}）",
                region.min.to_gtp(size),
                region.max.to_gtp(size),
                region.max.x() - region.min.x() + 1,
                region.max.y() - region.min.y() + 1,
            ));
        }
        ui.separator();
        ui.weak("排除选点：棋盘右键空点，或候选点行「排除」按钮。");
        if limits.avoid.is_empty() {
            ui.weak("（无）");
        } else {
            let size = board.size();
            for (i, (player, at)) in limits.avoid.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{} {}", player.name(), at.to_gtp(size))).monospace(),
                    );
                    if ui.small_button("移除").clicked() {
                        *action = PanelAction::RemoveAvoid(i);
                    }
                });
            }
            if ui.button("清空排除").clicked() {
                *action = PanelAction::ClearAvoid;
            }
        }
        if limits.has_region() && !limits.avoid.is_empty() {
            ui.colored_label(theme::colors::WARN, "区域模式下排除列表暂不生效。");
        }
        ui.add_space(2.0);
        if wide_button(ui, "清除全部限制").clicked() {
            *action = PanelAction::ClearLimits;
        }
    });
}

/// 「胜率」卡片：大字号胜率与目差（按当前显示视角换算）+ 网络直觉行。
///
/// 显示口径：存储恒黑视角；显示按 [`AnalysisState::display_view`] 现场换算
/// （[`display_values`] 全仓唯一入口）——黑视角恒按存储值显示，黑白交替时
/// 轮白显示 `1−w` / 目差取负（v1.18.2 实测 SIDETOMOVE 报文的换算恒等式，
/// 见 `engine::protocol::WinrateView` 文档）。
/// 「网络直觉」行 = `rootInfo.rawWinrate` / `rawLead`（策略网络未搜索输出，
/// v1.18.2 实测无条件携带、同样归一化为黑视角后按视角显示）；字段缺失时
/// 整行隐藏（不显示 0、不臆造）。
fn card_winrate(ui: &mut Ui, analysis: &AnalysisState, action: &mut PanelAction) {
    let view = analysis.display_view();
    card(ui, |ui| {
        theme::section_title(ui, "胜率");
        // 视角切换入口（LizzieYzy「目数视角」）：两枚分段按钮。切换经
        // App 转入 AnalysisState（作废快照 + 重发查询），历史数据两用
        // 不清空（存储恒黑视角）。
        ui.horizontal(|ui| {
            ui.label(RichText::new("视角").weak());
            for mode in [DisplayView::Black, DisplayView::Alternating] {
                if ui
                    .add(egui::Button::selectable(view == mode, mode.name()))
                    .clicked()
                {
                    *action = PanelAction::SetDisplayView(mode);
                }
            }
        });
        match &analysis.snapshot {
            Some(snapshot) => match &snapshot.root {
                Some(root) => {
                    let (wr, lead) = display_values(
                        view,
                        root.current_player,
                        root.winrate,
                        root.score_lead,
                    );
                    let side = match view {
                        DisplayView::Black => "黑方",
                        DisplayView::Alternating => root.current_player.name(),
                    };
                    ui.label(RichText::new(format!("{}胜率 {:.1}%", side, wr * 100.0)).size(24.0).strong());
                    ui.label(RichText::new(format!("目差 {:+.1}", lead)).size(15.0).weak());
                    // 网络直觉：原始网络输出的胜率 / 目差（未搜索），与
                    // 搜索后结论并列，用户可直接看出搜索修正了多少。
                    // raw 值与 winrate 同视角口径，同一函数换算显示。
                    if let Some((raw_wr, raw_lead)) = snapshot.raw_eval() {
                        let (raw_wr, raw_lead) = display_values(
                            view,
                            root.current_player,
                            raw_wr,
                            raw_lead,
                        );
                        ui.label(
                            RichText::new(format!(
                                "网络直觉（未搜索）：{:.1}%，{raw_lead:+.1}",
                                raw_wr * 100.0
                            ))
                            .size(12.0)
                            .color(Color32::from_rgb(150, 156, 166)),
                        )
                        .on_hover_text(
                            "策略网络对当前局面的第一直觉（原始网络输出，未搜索），\
                             与上面的搜索结论对比可以看出搜索修正了多少。",
                        );
                    }
                }
                None => {
                    ui.weak("引擎未返回数据（空报告）。");
                }
            },
            None => {
                ui.weak(if matches!(analysis.engine, EngineStatus::Ready) {
                    "正在分析当前局面…"
                } else {
                    "等待引擎可用后自动分析。"
                });
            }
        }
    });
}

/// 「候选点」卡片：全宽按钮行，点 / 胜率 / visits 三段排版，点击定位；
/// 行下方弱色小字显示该候选的主变（PV）前缀。
///
/// 整张卡片属**候选类**内容（第 5 项门控）：延迟 / 手动模式下未到时机
/// 时整体不渲染（含「排除 / 前进」按钮——按钮以候选行为载体，候选都
/// 藏了按钮自然也不该在）。胜率 / 目差卡片与曲线不受门控。
fn card_candidates(
    ui: &mut Ui,
    analysis: &AnalysisState,
    overlay: &Overlay,
    manual_revealed: bool,
    action: &mut PanelAction,
) {
    card(ui, |ui| {
        theme::section_title(ui, "候选点");
        if !analysis.candidates_visible(manual_revealed) {
            // 门控未放行：如实说明隐藏原因，不假装「引擎未返回候选点」。
            match analysis.gating() {
                super::analysis::CandidateGating::Delayed { secs } => {
                    ui.weak(format!(
                        "候选点延迟 {} 秒显示（避免浅层搜索结果误导）。胜率 / 目差数字仍在实时刷新。",
                        secs
                    ));
                }
                super::analysis::CandidateGating::Manual => {
                    ui.colored_label(
                        Color32::from_rgb(150, 156, 166),
                        "候选点已隐藏——按 F 显示（手动模式）。",
                    );
                }
                super::analysis::CandidateGating::Immediate => {}
            }
            return;
        }
        match &analysis.snapshot {
            Some(snapshot) if !snapshot.moves.is_empty() => {
                ui.weak("点 / 黑方胜率 / visits（点击定位）");
                // 幽灵子推导所需的行棋方（主变序列交替推演的起点）。
                let to_play = snapshot.root.as_ref().map(|root| root.current_player);
                for info in snapshot.moves.iter().take(MOVE_LIMIT) {
                    candidate_row(ui, info, overlay, snapshot.size, to_play, action);
                }
            }
            Some(_) => {
                ui.weak("引擎未返回候选点。");
            }
            None => {
                ui.weak("—");
            }
        }
    });
}

/// 候选点行：自绘全宽按钮（左坐标 / 中胜率 / 右 visits），选中态与
/// 棋盘定位高亮呼应（琥珀填充）；弃着行不可点。行尾「排除」小钮把
/// 该手加入 avoidMoves（再次点击同点行间互斥由 `AnalysisState` 去重）。
/// 行体下方为该候选的主变（PV）弱色小字行；行尾「沿主变前进」小钮
/// 只沿**已存在**的着法导航（预览式，不新建分支，见 [`PanelAction::AdvancePv`]）。
#[allow(clippy::too_many_arguments)]
fn candidate_row(
    ui: &mut Ui,
    info: &crate::engine::MoveInfo,
    overlay: &Overlay,
    size: crate::board::Size,
    to_play: Option<Stone>,
    action: &mut PanelAction,
) {
    let mv = info
        .mv
        .map_or_else(|| "弃着".to_owned(), |c| c.to_gtp(size));
    let winrate = format!("{:.1}%", info.winrate * 100.0);
    let visits = format!("{}", info.visits);
    // 当前被定位的行保持选中态，与棋盘高亮呼应。
    let selected = overlay
        .focus
        .as_ref()
        .is_some_and(|f| Some(f.at) == info.mv);
    let interactive = info.mv.is_some();
    // PV 文本行：空 PV（罕见，引擎至少回首手）不占位。
    let pv_line = pv_text(&info.pv, size);

    let height = 24.0;
    // 行尾「排除」「前进」按钮占宽（弃着行没有，行体占满整行）。
    let tail = if interactive { 92.0 } else { 0.0 };
    let (rect, mut response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width() - tail, height),
        if interactive {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let painter = ui.painter_at(rect);
    let fill = if selected {
        theme::colors::ACCENT_DIM
    } else if interactive && (response.hovered() || response.is_pointer_button_down_on()) {
        Color32::from_rgb(52, 57, 70)
    } else {
        Color32::from_rgb(39, 43, 53)
    };
    painter.rect_filled(rect, 5.0, fill);
    if selected {
        painter.rect_stroke(
            rect,
            5.0,
            Stroke::new(1.0, theme::colors::ACCENT_DEEP),
            StrokeKind::Middle,
        );
    }
    let text_color = if selected {
        Color32::from_rgb(255, 214, 140)
    } else {
        Color32::from_rgb(214, 218, 226)
    };
    painter.text(
        rect.min + Vec2::new(10.0, height / 2.0),
        Align2::LEFT_CENTER,
        &mv,
        FontId::monospace(13.0),
        text_color,
    );
    painter.text(
        Pos2::new(rect.left() + 64.0, rect.center().y),
        Align2::LEFT_CENTER,
        &winrate,
        FontId::monospace(12.0),
        text_color,
    );
    painter.text(
        rect.max - Vec2::new(10.0, height / 2.0),
        Align2::RIGHT_CENTER,
        &visits,
        FontId::monospace(11.0),
        Color32::from_rgb(150, 156, 166),
    );
    response = if info.mv.is_some() {
        response.on_hover_text("点击在棋盘上定位该点")
    } else {
        response.on_hover_text("弃着无处定位")
    };
    if response.clicked()
        && let Some(at) = info.mv
    {
        let ghosts = to_play.map_or_else(Vec::new, |to_play| {
            overlay::ghosts_from_pv(&info.pv, to_play)
        });
        *action = PanelAction::Focus { at, ghosts };
    }

    // 行尾「排除」「前进」按钮：独立交互区（排除另可右键棋盘空点）。
    if let Some(at) = info.mv
        && let Some(player) = to_play
    {
        // 两个小钮并排（排除 / 沿主变前进），等宽对齐。
        let btn_w = (tail - 8.0) / 2.0;
        let (brect, btn) = ui.allocate_exact_size(Vec2::new(btn_w, height - 4.0), Sense::click());
        let painter = ui.painter_at(brect);
        let hover = btn.hovered() || btn.is_pointer_button_down_on();
        painter.rect_filled(
            brect,
            4.0,
            if hover {
                Color32::from_rgb(72, 44, 46)
            } else {
                Color32::from_rgb(48, 42, 46)
            },
        );
        painter.text(
            brect.center(),
            Align2::CENTER_CENTER,
            "排除",
            FontId::proportional(10.5),
            if hover {
                Color32::from_rgb(255, 150, 140)
            } else {
                Color32::from_rgb(196, 168, 168)
            },
        );
        let btn = btn.on_hover_text(format!("把 {} 加入排除（avoidMoves）", at.to_gtp(size)));
        if btn.clicked() {
            *action = PanelAction::ToggleAvoid { player, at };
        }

        let (frect, fwd) = ui.allocate_exact_size(Vec2::new(btn_w, height - 4.0), Sense::click());
        let painter = ui.painter_at(frect);
        let hover = fwd.hovered() || fwd.is_pointer_button_down_on();
        painter.rect_filled(
            frect,
            4.0,
            if hover {
                Color32::from_rgb(44, 58, 72)
            } else {
                Color32::from_rgb(42, 48, 56)
            },
        );
        painter.text(
            frect.center(),
            Align2::CENTER_CENTER,
            "前进",
            FontId::proportional(10.5),
            if hover {
                Color32::from_rgb(150, 200, 255)
            } else {
                Color32::from_rgb(168, 186, 206)
            },
        );
        let fwd =
            fwd.on_hover_text("沿该候选的主变逐手前进（只走谱上已有的着法，缺处即停；不新建分支）");
        if fwd.clicked() {
            *action = PanelAction::AdvancePv {
                at,
                pv: info.pv.iter().take(PV_LIMIT).copied().collect(),
            };
        }
    }

    // PV 文本行：小字号 + 弱化色，紧贴候选行下方；点击不与上方行体冲突。
    if let Some(text) = pv_line {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(10.0);
            ui.label(
                RichText::new(text)
                    .monospace()
                    .size(10.5)
                    .color(Color32::from_rgb(140, 148, 160)),
            )
            .on_hover_text("该候选的主变（PV）前缀，与棋盘预览截断一致");
        });
    }
}

/// 「失误」卡片：已分析进度与各严重程度计数。
fn card_mistakes(ui: &mut Ui, analysis: &AnalysisState, board: &Board) {
    card(ui, |ui| {
        theme::section_title(ui, "失误");
        let summary = analysis.loss_summary(board);
        if summary.total == 0 {
            ui.weak("—");
        } else {
            // 「已分析 / 总手数」必须显式给出：数据随浏览逐步积累，
            // 不写清楚会被误认为整盘都算过了。
            ui.weak(format!(
                "已分析 {} / {} 手（随浏览逐步积累）",
                summary.analyzed, summary.total
            ));
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                // 计数与棋盘标记共用同一套严重程度配色。
                for (label, count, severity) in [
                    ("疑问手", summary.questionable, Severity::Questionable),
                    ("失误", summary.mistake, Severity::Mistake),
                    ("恶手", summary.blunder, Severity::Blunder),
                ] {
                    ui.label(
                        RichText::new(format!("{label} {count}"))
                            .color(overlay::severity_color(severity))
                            .strong(),
                    );
                }
            });
            ui.weak("棋盘标注：目差损失 ≥1 目疑问手 / ≥3 目失误 / ≥6 目恶手");
        }
    });
}

/// 「局后统计」卡片：黑白吻合度 + 最差 N 手排行榜（可点击跳转）。
///
/// 与「失误」卡片互补不重复：失误卡片按**目差损失**分级计数（走子前后
/// 局面差），本卡片按 **visits 占比**（吻合度，LizzieYzy `percentsMatch`）
/// 汇总整局。排行榜排序键 = **胜率损失降序**（LizzieYzy 差异手
/// `diffWinrate` 口径），损失与失误卡片同源（走子前后历史点差），
/// 行内附带显示吻合度；行色用与失误卡片 / 棋盘标注同一套严重程度配色。
/// 数据来源取决于分析深度——快扫 40 visits 的候选表比交互深度
/// （如 300 visits）短，吻合度系统性偏低，tooltip 明示。
fn card_summary(ui: &mut Ui, analysis: &AnalysisState, board: &Board, action: &mut PanelAction) {
    card(ui, |ui| {
        theme::section_title(ui, "局后统计");
        let summary = analysis.game_summary(board);
        let GameSummary {
            total,
            analyzed_black,
            analyzed_white,
            match_black,
            match_white,
            enough_black,
            enough_white,
            worst,
        } = summary;
        if total == 0 {
            ui.weak("—");
            return;
        }
        // 吻合度行：黑 / 白并列；样本不足（已分析 < 10 手）不给数字，
        // 显示「样本不足」（LizzieYzy 同门槛，避免少数手的均值冒充整盘）。
        let black_cell = match_cell(match_black, analyzed_black, enough_black);
        let white_cell = match_cell(match_white, analyzed_white, enough_white);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            let tip = match_tip();
            ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("黑吻合度").weak());
                    ui.label(RichText::new(black_cell).strong());
                });
            })
            .response
            .on_hover_text(format!("黑方 {tip}"));
            ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("白吻合度").weak());
                    ui.label(RichText::new(white_cell).strong());
                });
            })
            .response
            .on_hover_text(format!("白方 {tip}"));
        });
        // 计数口径如实展示（沿用失误卡片「已分析 N / M 手」写法）：
        // 未分析的手不进吻合度分母，样本门槛 10 手。
        ui.weak(format!(
            "黑 {analyzed_black} 手 / 白 {analyzed_white} 手已分析（共 {total} 手，\
             未分析的不计入）"
        ));
        ui.weak("不足 10 手不给平均值（样本不足）。");

        // 最差 N 手排行榜：胜率损失降序（LizzieYzy 差异手 diffWinrate
        // 口径），行内附吻合度；行点击跳转。
        ui.add_space(2.0);
        if worst.is_empty() {
            ui.weak("最差手：暂无已分析的手。");
        } else {
            ui.weak(format!(
                "最差 {} 手（按胜率损失，点击跳转）：",
                worst.len().min(WORST_LIMIT)
            ));
            for entry in &worst {
                worst_row(ui, *entry, action);
            }
        }
        ui.weak(
            "吻合度 ∝ 分析深度：快扫（40 visits）候选表短，数值偏低；\
             不同深度的数字不可互比。",
        );
    });
}

/// 吻合度单元格文本：样本不足时如实写「样本不足」，否则给百分比。
/// 数字与「未分析」严格区分（未分析的手不进分母，见 `game_summary`）。
fn match_cell(ratio: f64, analyzed: usize, enough: bool) -> String {
    if analyzed == 0 {
        "未分析".to_owned()
    } else if !enough {
        "样本不足".to_owned()
    } else {
        format!("{:.1}%", ratio * 100.0)
    }
}

/// 吻合度的口径说明（hover 文案，LizzieYzy 原文 + 数据来源警示）。
fn match_tip() -> String {
    "吻合度：以计算量为标准，衡量实际棋局与 AI 的差别（实际落子在候选表中的 \
     visits 占比，整局平均）。数据来源取决于分析深度——整谱快扫（40 visits）\
     的候选表比深度分析（如 300 visits）短，吻合度系统性偏低，不同深度的数字\
     不要互比。"
        .to_owned()
}

/// 排行榜一行：手数 / 行棋方 / 胜率损失 + 吻合度，自绘全宽按钮行
/// （点击跳转该手）。文字用 [`overlay::severity_color`] 上色——与
/// 「失误」卡片计数、棋盘失误标注同一套严重程度配色，两张卡片与棋盘
/// 三处一眼对上；三种档位色（黄 / 橙 / 红）在深色底上均已可读，无需提亮。
/// Good / Fine 兜底色偏灰暗，这里统一提亮为中性灰（见行内说明）。
fn worst_row(ui: &mut Ui, entry: crate::ui::analysis::WorstMove, action: &mut PanelAction) {
    // 胜率损失为行棋方视角、正 = 亏损：负值意味着该手实际不亏（排序垫底
    // 的噪声手），带符号如实显示。
    let loss_text = if entry.winrate_loss >= 0.0 {
        format!("+{:.1}%", entry.winrate_loss * 100.0)
    } else {
        format!("{:.1}%", entry.winrate_loss * 100.0)
    };
    let text = format!(
        "第 {} 手 {}　胜率 {}　吻合度 {:.1}%",
        entry.turn,
        entry.player.name(),
        loss_text,
        entry.match_ratio * 100.0
    );
    let height = 22.0;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let painter = ui.painter_at(rect);
    let hover = response.hovered() || response.is_pointer_button_down_on();
    painter.rect_filled(
        rect,
        5.0,
        if hover {
            Color32::from_rgb(52, 57, 70)
        } else {
            Color32::from_rgb(39, 43, 53)
        },
    );
    // Good / Fine 的 severity_color 兜底灰（140,140,148）在行底色上偏暗、
    // 与 hover 亮字冲突，提亮为中性灰；疑问手及以上用原档位色不动。
    let text_color = match entry.severity {
        Severity::Good | Severity::Fine => Color32::from_rgb(214, 218, 226),
        _ => overlay::severity_color(entry.severity),
    };
    painter.text(
        rect.min + Vec2::new(10.0, height / 2.0),
        Align2::LEFT_CENTER,
        &text,
        FontId::monospace(12.0),
        if hover {
            Color32::from_rgb(255, 214, 140)
        } else {
            text_color
        },
    );
    let response = response.on_hover_text(format!(
        "第 {} 手（{}）：胜率损失 {:.1}%、目差损失 {:.1} 目，{}（{}）。\
         排序口径 = LizzieYzy 差异手 diffWinrate：胜率损失最大的手排最前，\
         正 = 行棋方亏损；吻合度只是附带显示，不是排序键。点击跳转到该手",
        entry.turn,
        entry.player.name(),
        entry.winrate_loss * 100.0,
        entry.score_loss,
        entry.severity.name(),
        if entry.severity.is_marked() {
            "棋盘有标注"
        } else {
            "棋盘不标注"
        },
    ));
    if response.clicked() {
        *action = PanelAction::GotoTurn(entry.turn);
    }
}

/// 「叠加层」卡片：各层开关（复选框）与胜率色阶图例。
/// 返回值：opt-in 层（策略 / 候选点领地）开关是否被本帧改动
/// （重查由 App 转发 [`crate::ui::analysis::AnalysisState`]）。
struct OverlayToggles {
    policy_toggled: bool,
    moves_heat_toggled: bool,
}

/// 热度图来源行：显示当前热度图画的是哪路数据（候选 X 走后 / 当前局面）。
/// 「候选点领地」层激活时加紫色「走后假想」标注；回落当前局面时如实体明。
///
/// 判定**直接复用棋盘绘制所调的 [`overlay::heat_source`]**（唯一真相），
/// 不另写一套条件：否则「热度图关闭 / 候选无 ownership / 向量长度不符」
/// 这些组合下标注会与棋盘不一致（标注说有层、棋盘上什么也没画）。
fn heat_source_line(ui: &mut Ui, overlay: &Overlay, analysis: &AnalysisState) {
    let Some(snapshot) = &analysis.snapshot else {
        return;
    };
    if !overlay.show_heat {
        return; // 热度图本身关闭：没有层需要标注
    }
    // 与 board_view 同款：只有「候选点领地」开关开启时才把聚焦点交给
    // heat_source（否则聚焦只是定位高亮，不该被当成热度图数据源）。
    let focus = overlay
        .show_moves_heat
        .then_some(overlay.focus.as_ref())
        .flatten();
    let Some((source, _)) = overlay::heat_source(snapshot, focus) else {
        return;
    };
    match source {
        overlay::HeatSource::Candidate(at) => {
            ui.label(
                RichText::new(format!("热度图：候选 {} 走后", at.to_gtp(snapshot.size)))
                    .color(Color32::from_rgb(196, 168, 240))
                    .size(12.0),
            )
            .on_hover_text(
                "紫色层 = 「走这一手之后」的假想领地（候选点级 ownership），\
                 与当前局面的暖黑/暖白层色相不同",
            );
        }
        overlay::HeatSource::Position => {
            ui.label(
                RichText::new("热度图：当前局面")
                    .color(Color32::from_rgb(150, 156, 166))
                    .size(12.0),
            );
        }
    }
}

fn card_overlay(
    ui: &mut Ui,
    overlay: &mut Overlay,
    analysis: &AnalysisState,
    curve_open: &mut bool,
    tree_open: &mut bool,
    action: &mut PanelAction,
) -> OverlayToggles {
    let mut toggles = OverlayToggles {
        policy_toggled: false,
        moves_heat_toggled: false,
    };
    card(ui, |ui| {
        theme::section_title(ui, "叠加层");
        ui.checkbox(&mut overlay.show_candidates, "候选点圆圈");

        // 候选类显示门控（第 5 项）：三选一分段选择器。只影响候选类内容
        // （候选圆圈 / PV 幽灵子 / 候选列表 / 候选点领地）何时显示；
        // 胜率 / 目差数字与曲线永远实时。切换即时生效，不重发查询。
        ui.label(RichText::new("候选点显示").weak());
        ui.horizontal(|ui| {
            let gating = analysis.gating();
            for mode in [
                super::analysis::CandidateGating::Immediate,
                super::analysis::CandidateGating::Delayed { secs: gating_delay(gating) },
                super::analysis::CandidateGating::Manual,
            ] {
                let selected = std::mem::discriminant(&gating) == std::mem::discriminant(&mode);
                let button = egui::Button::selectable(selected, mode.name());
                let response = ui
                    .add(button)
                    .on_hover_text(match mode {
                        super::analysis::CandidateGating::Immediate => {
                            "收到报告即显示候选（默认）"
                        }
                        super::analysis::CandidateGating::Delayed { .. } => {
                            "引擎开始思考满 N 秒后才显示候选，避免浅层搜索结果误导"
                        }
                        super::analysis::CandidateGating::Manual => {
                            "按 F 显示候选；局面一变需重新按键"
                        }
                    });
                if response.clicked() {
                    *action = PanelAction::SetGating(mode);
                }
            }
        });
        // 延迟秒数（仅延迟模式显示编辑框）。
        if let super::analysis::CandidateGating::Delayed { secs } = analysis.gating() {
            ui.horizontal(|ui| {
                ui.weak("延迟");
                let mut secs_edit = secs as i32;
                if ui
                    .add(egui::DragValue::new(&mut secs_edit).range(1..=30).suffix(" 秒"))
                    .changed()
                {
                    *action = PanelAction::SetGating(super::analysis::CandidateGating::Delayed {
                        secs: secs_edit.max(1) as u32,
                    });
                }
            });
        }
        ui.checkbox(&mut overlay.show_heat, "局势热度图");
        let policy = ui
            .checkbox(&mut overlay.show_policy, "策略热度图")
            .on_hover_text(
                "引擎还没搜索时的第一直觉（策略网络先验），\
                 不是搜索后的推荐——推荐看「候选点」。",
            );
        toggles.policy_toggled = policy.changed();
        let moves_heat = ui
            .checkbox(&mut overlay.show_moves_heat, "候选点领地")
            .on_hover_text(
                "开启后点击候选点定位时，热度图切换为「走这一手之后」的领地\
                 （紫色层，候选点级 ownership）。代价：每条报告按候选数增重\
                 （约 +3.4 KB/候选，300 visits 流式一次约 340 KB），\
                 引擎需重查一次才生效；未聚焦候选点时仍显示当前局面。",
            );
        toggles.moves_heat_toggled = moves_heat.changed();
        ui.checkbox(&mut overlay.show_mistakes, "失误标注");
        ui.checkbox(&mut overlay.show_score_lead, "曲线叠加目差")
            .on_hover_text(
                "在胜率曲线上叠加目差折线（虚线暖色，右侧目差刻度对称于 0）。\
                 复盘看形势主要看目差，默认开启。",
            );
        ui.checkbox(&mut overlay.show_mini_board, "小棋盘回放")
            .on_hover_text(
                "底部打开一个小棋盘，把聚焦候选点（未聚焦用引擎首选）的\
                 主变逐手自动回放。只做预览摆子，不改棋谱树。",
            );
        ui.checkbox(curve_open, "胜率曲线面板");
        ui.checkbox(tree_open, "棋谱树面板");
        // 胜率色阶图例：与棋盘候选点共用 overlay::winrate_color 同一映射。
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.weak("白优");
            let (rect, _) = ui.allocate_exact_size(Vec2::new(80.0, 10.0), Sense::hover());
            let painter = ui.painter_at(rect);
            const SEGMENTS: usize = 24;
            let w = rect.width() / SEGMENTS as f32;
            for i in 0..SEGMENTS {
                let t = i as f64 / (SEGMENTS - 1) as f64;
                let x = rect.min.x + i as f32 * w;
                painter.rect_filled(
                    Rect::from_min_size(
                        Pos2::new(x, rect.min.y),
                        Vec2::new(w + 0.5, rect.height()),
                    ),
                    0.0,
                    overlay::winrate_color(t),
                );
            }
            ui.weak("黑优");
        });
        ui.weak(
            "圆圈大小 ∝ √visits，白环为主选点；热度深色 = 黑势、浅色 = 白势；\
                 策略层靛蓝色块 ∝ 先验概率，只铺空点",
        );
        // 热度图来源行：与棋盘同源判定，说明当前画的是哪路数据。
        heat_source_line(ui, overlay, analysis);
    });
    toggles
}

/// 「整谱快扫」卡片：可配置的批量分析（LizzieYzy「闪电分析设置」口径）。
///
/// 空闲态：折叠的设置区（起止手数 / 每手 visits / 只扫一方 / 含变着 /
/// 扫完自动加深）+ 实时预估（局面数 / 预计耗时；含变着时局面数如实外推
/// 到全树，宁可劝退）+ 发起按钮（空盘置灰）。
/// 进行中：两阶段进度条（「主扫描 x/y」与「加深 z/N」分开显示）+ 取消。
/// 配置编辑经 [`PanelAction::SetBatchConfig`] 转存进 [`AnalysisState`]，
/// 发起与预估共用同一份配置、同一套钳制（`build_plan`）。
fn card_batch(ui: &mut Ui, analysis: &AnalysisState, board: &Board, action: &mut PanelAction) {
    card(ui, |ui| {
        theme::section_title(ui, "整谱快扫");
        if let Some((deep, done, total, elapsed)) = analysis.batch_progress() {
            // 进度条 + 阶段标注：主扫描与加深分开计数（批量口径变更后
            // 总数是「过滤后的局面数」，不再恒等于手数）。
            ui.add(
                egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                    .show_percentage()
                    .desired_height(14.0),
            );
            ui.weak(format!(
                "{} {done} / {total} · 已用 {}",
                if deep { "加深" } else { "主扫描" },
                crate::ui::analysis::format_batch_elapsed(elapsed),
            ));
            ui.add_space(2.0);
            if wide_button(ui, "取消快扫").clicked() {
                *action = PanelAction::CancelBatch;
            }
            return;
        }
        // ---- 配置编辑（空闲态；草稿在本地逐项改，帧末一次性转存）----
        let mut draft = analysis.batch_config().clone();
        let len = board.line_len();
        let mut changed = false;
        ui.collapsing("快扫设置", |ui| {
            // 起止手数（1 基手数编辑；内部 0 基局面序号）。止手 ≤ 起手时
            // 由发起端钳制（这里显示时也做同样钳制，保持所见即所扫）。
            ui.horizontal(|ui| {
                ui.weak("起手");
                let from = (draft.from.min(len.saturating_sub(1)) + 1) as i32;
                let mut from_edit = from;
                if ui
                    .add(egui::DragValue::new(&mut from_edit).range(1..=len as i32))
                    .changed()
                {
                    draft.from = (from_edit.max(1) as usize - 1).min(len.saturating_sub(1));
                    changed = true;
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
                    changed = true;
                }
            });
            // 每手 visits：常用档位分段选择（当前值不在档位内则追加一个
            // 「自定义」段显示真实值）。
            ui.horizontal(|ui| {
                ui.weak("每手visits");
                for &visits in &[20u32, 40, 100, 300] {
                    let selected = draft.visits == visits;
                    if ui
                        .add(egui::Button::selectable(selected, visits.to_string()))
                        .clicked()
                    {
                        draft.visits = visits;
                        changed = true;
                    }
                }
                // 常用档位之外的可编辑数值：当前值不在档位内时格式化带
                // 「自定」后缀提示（DragValue 无独立文字着色 API，不再硬造）。
                let label = egui::DragValue::new(&mut draft.visits)
                    .range(1..=10_000)
                    .custom_formatter(|v, _| {
                        if BATCH_VISITS_PRESETS.contains(&(v as u32)) {
                            format!("{v}")
                        } else {
                            format!("{v} 自定")
                        }
                    })
                    .custom_parser(|s| s.trim().parse::<f64>().ok().map(|v| v as u32 as f64));
                if ui.add(label).changed() {
                    draft.visits = draft.visits.max(1);
                    changed = true;
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
                        changed = true;
                    }
                }
            });
            if ui
                .checkbox(&mut draft.include_variations, "含变着（全树逐节点，慢）")
                .changed()
            {
                changed = true;
            }
            // 扫完自动加深差异手（默认开）：治 40 visits 下吻合度并列 0、
            // 差异手排序噪声大的毛病。
            if ui
                .checkbox(&mut draft.deepen_enabled, "扫完自动加深差异手")
                .changed()
            {
                changed = true;
            }
            if draft.deepen_enabled {
                ui.horizontal(|ui| {
                    ui.weak("前");
                    if ui
                        .add(egui::DragValue::new(&mut draft.deepen_top).range(1..=50))
                        .changed()
                    {
                        changed = true;
                    }
                    ui.weak("手，加深到");
                    if ui
                        .add(egui::DragValue::new(&mut draft.deepen_visits).range(1..=10_000))
                        .changed()
                    {
                        changed = true;
                    }
                    ui.weak("visits");
                });
            }
            ui.weak(
                "含变着时对每个节点分析「到该节点为止 + 该节点深度」，\
                     局面数随分支线性增长；起止手数同样按节点深度过滤。",
            );
        });
        // 预估（与发起共用同一实现）：局面数按当前配置如实展示，
        // 「含变着」多分支时数字可能远大于手数，配以劝退提示。
        let estimate = batch_estimate(board, &draft);
        ui.weak(format!(
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
        ui.add_space(2.0);
        if changed {
            *action = PanelAction::SetBatchConfig(draft);
        }
        let empty = len == 0;
        let entry = ui.add_enabled(!empty, egui::Button::new("开始快扫"));
        if empty {
            entry.on_disabled_hover_text("空盘无谱可扫");
        } else if entry.clicked() {
            *action = PanelAction::StartBatch;
        }
    });
}

/// 「每手 visits」的常用档位（侧栏分段选择 + 「自定义」编辑共存）。
const BATCH_VISITS_PRESETS: [u32; 4] = [20, 40, 100, 300];

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

/// 「消息」卡片：各类用户可见提示（非法落子 / 载入另存 / 引擎错误 /
/// 引擎字段警告等）。无任何提示时不渲染（原「无提示。」占位行去除，
/// 语义不变）。
/// 引擎字段警告（WARN 色，会话内保留、按字段名去重——去重在
/// [`AnalysisState`] 侧完成）单独列出：它是「配置可能拼错了」的提醒，
/// 与瞬时错误（红）语义不同，不能混排，也不能只落无人可见的日志。
#[allow(clippy::too_many_arguments)]
fn card_messages(
    ui: &mut Ui,
    notice: Option<IllegalReason>,
    startup_notice: Option<&str>,
    load_notice: Option<&LoadNotice>,
    save_notice: Option<&LoadNotice>,
    persist_notice: Option<&str>,
    transient_error: &Option<String>,
    engine_warnings: &[super::analysis::EngineWarning],
) {
    let has_any = notice.is_some()
        || startup_notice.is_some()
        || load_notice.is_some()
        || save_notice.is_some()
        || persist_notice.is_some()
        || transient_error.is_some()
        || !engine_warnings.is_empty();
    if !has_any {
        return;
    }
    card(ui, |ui| {
        theme::section_title(ui, "消息");
        if let Some(reason) = notice {
            ui.colored_label(
                Color32::from_rgb(255, 152, 82),
                format!("非法落子：{reason}"),
            );
        }
        if let Some(text) = startup_notice {
            ui.colored_label(theme::colors::WARN, text);
        }
        for warning in engine_warnings {
            ui.colored_label(theme::colors::WARN, &warning.text);
        }
        if let Some(text) = transient_error {
            ui.colored_label(theme::colors::WARN, text);
        }
        if let Some(msg) = load_notice {
            ui.colored_label(msg.color(), msg.text());
        }
        if let Some(msg) = save_notice {
            ui.colored_label(msg.color(), msg.text());
        }
        if let Some(text) = persist_notice {
            ui.colored_label(theme::colors::ERROR, text);
        }
    });
}

/// 棋手与段位拼成一行显示文本（如 `聂卫平 九段`）；两者都缺返回 `None`。
fn player_line(name: &Option<String>, rank: &Option<String>) -> Option<String> {
    match (name, rank) {
        (None, None) => None,
        (name, rank) => Some(
            name.as_deref()
                .into_iter()
                .chain(rank.as_deref())
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}
