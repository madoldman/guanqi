//! 分析侧栏：引擎状态、胜率 / 目差、候选点列表、失误统计与提示行
//! （TASKS 4.2 / 4.4）。
//!
//! 侧栏定位是**纯信息展示**：设置与操作（人机对弈开关 / 难度 / 限定选点
//! 开关 / 叠加层开关 / 快扫发起等）已全部规整进顶部菜单（文件 / 对局 /
//! 分析，见 `app.rs` 各菜单方法与快扫对话框 `batch_scan`）。仅存的交互
//! 是**展示固有**的点击行为：候选点行点击定位 / 排除 / 沿主变前进、
//! 文档列表切换、局后统计排行榜跳转与「重试分析」，它们都是「对所展示
//! 内容的直接操作」，不是设置项。
//!
//! 数据只读自 [`AnalysisState`]：胜率 / 目差为**黑方视角**（引擎配置
//! `reportAnalysisWinratesAs = BLACK`，实测对 rootInfo 与 moveInfos 同时生效），
//! 不按行棋方翻转。棋盘候选点叠加 / 热度图 / 失误标注（`overlay` 模块）
//! 直接取 `AnalysisState` 的快照与历史缓冲；层开关在菜单里，本面板只
//! 提供失误汇总、胜率色阶图例与候选点点击定位。
//!
//! 呈现结构：各分区用 [`theme::card_frame`] 包成圆角卡片；棋谱文档与
//! 候选点做成全宽按钮行；全部可交互项都有底色 / 边框 / hover 高亮。

use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::board::{Board, Coord, IllegalReason, Size, Stone};
use crate::engine::EngineConfig;
use crate::play::{PlayState, resign_text};
use crate::sgf::GameMeta;

use super::analysis::{display_values, AnalysisState, EngineStatus, GameSummary, Severity, WORST_LIMIT};
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

/// 侧栏触发的动作（由调用方在绘制结束后执行，避免借用冲突）。
///
/// 瘦身后只剩三类：
/// - **展示固有交互**：候选点点击定位 / 排除 / 沿主变前进、文档切换、
///   排行榜跳转——都是对所展示内容的直接操作，不算设置项；
/// - **故障恢复**：引擎重启 / 重查当前局面（错误重试按钮）；
/// - 其余设置与操作全部走顶部菜单 / 快扫对话框（见模块文档），不再
///   从侧栏发起。
#[derive(Default)]
pub enum PanelAction {
    /// 无操作。
    #[default]
    None,
    /// 请求用当前配置重启引擎（错误重试按钮）。
    RetryEngine,
    /// 重发当前局面的查询（查询超时 / 被拒后的「重试分析」按钮）：
    /// 引擎进程仍可用，作废在飞与已发送口径即重查。
    RetryQuery,
    /// 点击候选点行：在棋盘上定位该点（再点同一行取消，由 App 处理）。
    Focus {
        /// 候选落点。
        at: Coord,
        /// 主变幽灵子（已剔除断着与盘面已有棋子的点由绘制端处理）。
        ghosts: Vec<(Coord, Stone)>,
    },
    /// 从当前手创建研究副本（App 完成实际创建与切换）。
    CreateCopy,
    /// 点击文档列表项：切换到该编号的文档（App 完成整体互换）。
    SwitchDoc(usize),
    /// 点了候选行的「排除」：把该手加入 / 移出 avoidMoves（App 转交
    /// [`AnalysisState::toggle_avoid`]，区域模式下不生效）。
    ToggleAvoid {
        /// 行棋方（记录时取当前行棋方）。
        player: Stone,
        /// 被排除的落点。
        at: Coord,
    },
    /// 点了候选行的「沿主变前进」：沿该候选的 PV 逐手**预览前进**——
    /// 只在已存在的着法上导航（每手要求当前节点已有匹配的子分支，
    /// 否则停住），**不新建分支、不改棋谱树**。App 完成实际导航。
    AdvancePv {
        /// 主变首手（与 PV 同源，弃着行不出现该动作）。
        at: Coord,
        /// 主变序列（首手起，`None` = 弃着），截断到 [`PV_LIMIT`]。
        pv: Vec<Option<Coord>>,
    },
    /// 点了「局后统计」排行榜某行：跳转到该手（App 调
    /// [`crate::board::Board::go_to`]，曲线 / 棋盘定位随局面联动）。
    GotoTurn(usize),
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

/// 事件型提示的排队上限：新消息进队、超出上限挤出最旧。取 3 的理由：
/// 消息卡放在侧栏顶部，条目太多会把下面的卡片挤出首屏；3 条已覆盖
/// 「连续触发两条提示都还能看到」（另存成功不再被后一条顶掉）与常见
/// 连击操作（载入失败 → 重试 → 再失败），再多只会刷屏。
pub const MAX_NOTICES: usize = 3;

/// 事件型提示队列（App 持有，「消息」卡片渲染）：按时间排队，新消息
/// **不顶掉**旧消息，只受 [`MAX_NOTICES`] 上限约束。与常驻类提示
/// （非法落子 / 启动提示 / 引擎警告 / 瞬时错误）分开：后者是「当前
/// 状态」而非「发生过的事」，单槽覆盖语义本来就是对的。
#[derive(Default)]
pub struct NoticeFeed {
    items: std::collections::VecDeque<LoadNotice>,
}

impl NoticeFeed {
    /// 空队列。
    pub fn new() -> Self {
        Self::default()
    }

    /// 入队一条提示。与队尾文本相同则忽略（菜单连点 / 每帧重发的同文
    /// 提示只计一次，防止重复刷屏把其它消息挤出上限）；超出 [`MAX_NOTICES`]
    /// 时挤出最旧一条。
    pub fn push(&mut self, notice: LoadNotice) {
        if self.items.back().is_some_and(|last| last.text() == notice.text()) {
            return;
        }
        if self.items.len() >= MAX_NOTICES {
            self.items.pop_front();
        }
        self.items.push_back(notice);
    }

    /// 是否没有任何提示。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 按入队先后遍历（消息卡按此顺序渲染：最旧在上）。
    pub fn iter(&self) -> impl Iterator<Item = &LoadNotice> {
        self.items.iter()
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

/// 全宽普通按钮（侧栏内统一宽度；仅消息卡「重试分析」在用）。
fn wide_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add_sized([ui.available_width(), 0.0], egui::Button::new(text))
}

/// 信息行：弱色前缀 + 正文值（引擎状态 / 棋谱属性这类「标签：值」行）。
/// 返回整行响应，调用方可再挂 `on_hover_text`（概念解释等）。
fn info_line(ui: &mut Ui, label: &str, value: &str) -> egui::Response {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).weak());
        ui.label(RichText::new(value).size(12.5));
    })
    .response
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

/// 绘制分析侧栏（纯信息展示）。`settings_open` 仅透传给引擎卡的
/// 设置入口（设置窗口开关由本面板与顶部按钮共享）；
/// `overlay` 为棋盘叠加层的层开关与定位状态（本面板只读其定位态）；
/// `manual_revealed` 为候选类手动显示模式下「用户已按 F」标志
/// （App 持有并随局面变化复位，本面板只读）。
/// `board` 提供总手数与各行棋方（失误汇总按手数现场派生）。
/// `game` / `comment` 为「打开棋谱」相关信息：已载入棋谱的元信息与
/// 当前手注释；`load_notice` / `save_notice` 为最近一次打开 / 另存
/// 操作的提示（无则对应段落不显示）。
/// `play` 为人机对弈状态（本面板**只读**：展示对局进度与时钟；
/// 开关与终局动作在「对局」菜单）。
/// `hopeless` 为引擎无望提示文本（`Some` = 展示提示；判认输在菜单）。
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
    manual_revealed: bool,
    game: Option<&GameMeta>,
    comment: Option<&str>,
    load_notice: Option<&LoadNotice>,
    save_notice: Option<&LoadNotice>,
    event_notices: &NoticeFeed,
    persist_notice: Option<&str>,
    play: &PlayState,
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
                manual_revealed,
                game,
                comment,
                load_notice,
                save_notice,
                event_notices,
                persist_notice,
                play,
                hopeless,
                docs,
            )
        })
        .inner
}

/// 侧栏正文（[`show`] 的滚动内容）：各分区卡片化排布。
///
/// 卡片序即阅读序：先「发生了什么」（消息），再「现在进行到哪」
/// （对局状态 / 快扫进度），然后是分析数据（引擎 / 棋谱 / 胜率 /
/// 讲解 / 候选 / 失误 / 局后统计）。
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
    manual_revealed: bool,
    game: Option<&GameMeta>,
    comment: Option<&str>,
    load_notice: Option<&LoadNotice>,
    save_notice: Option<&LoadNotice>,
    event_notices: &NoticeFeed,
    persist_notice: Option<&str>,
    play: &PlayState,
    hopeless: Option<&str>,
    docs: &[DocEntry],
) -> PanelAction {
    let mut action = PanelAction::None;

    // ---- 消息（侧栏首位的用户提示：另存成功等反馈一眼可见；非法落子 /
    // 启动 / 引擎警告等常驻提示与事件型排队提示同卡显示）----
    card_messages(
        ui,
        notice,
        startup_notice,
        load_notice,
        save_notice,
        event_notices,
        persist_notice,
        &analysis.transient_error,
        analysis.engine_warnings(),
        &mut action,
    );

    // ---- 对局状态（纯展示：你执X / 时钟 / 终局原因 / 无望提示文本；
    // 开关与终局动作在「对局」菜单）----
    card_play_status(ui, analysis, board, game, play, hopeless);

    // ---- 整谱快扫进度（进行中才显示；发起 / 取消 / 配置在
    // 「分析 → 整谱快扫…」对话框）----
    card_batch_progress(ui, analysis);

    // ---- 引擎状态 ----
    card_engine(ui, analysis, cfg, game, settings_open, &mut action);

    // ---- 棋谱信息（打开棋谱后显示；属性存在才显示对应行）----
    if let Some(game) = game {
        card_game(ui, game, comment, docs, &mut action);
    }

    // ---- 胜率 / 目差 ----
    card_winrate(ui, analysis);

    // ---- 讲解（中文自动解说；终态数据驱动，流式期间只占位）----
    card_explain(ui, analysis, board);

    // ---- 限定选点信息（区域尺寸 / 排除清单内容；开关与清除在
    // 「分析 → 限定选点」菜单）----
    card_limits_info(ui, analysis, board);

    // ---- 候选点（点击定位 / 排除 / 沿主变前进——展示固有交互）----
    card_candidates(ui, analysis, overlay, manual_revealed, &mut action);

    // ---- 失误统计（TASKS 4.4）----
    card_mistakes(ui, analysis, board);

    // ---- 局后统计（吻合度 + 最差 N 手，与失误卡片信息互补不重复：
    // 失误卡片 = 目差损失分级计数；本卡片 = visits 口径吻合度 + 排行跳转）----
    card_summary(ui, analysis, board, &mut action);

    action
}

/// 时钟行（对弈卡片）：双方各一行——执子 + 读数 + 「正在计时」标记。
/// 无限制制式显示累计用时；包干显示剩余主时间；读秒制显示主时间 /
/// 读秒剩余 × 剩余次数。剩余不多（包干 < 1 分钟；读秒期内一律）用
/// 醒目色但**不闪烁**（一次性换色，不随帧变化）。终局后不再标注
/// 「正在计时」。
fn clock_rows(ui: &mut Ui, play: &PlayState, to_play: Stone, finished: bool) {
    let system = play.clock.system;
    let engine = play.human.opposite();
    for (stone, label) in [(play.human, "你"), (engine, "引擎")] {
        let clock = play.clock.side(stone);
        let spending = !finished && to_play == stone;
        let text = crate::play::clock_text(
            system,
            clock,
            play.clock.total[crate::play::side_index_of(stone)],
        );
        // 醒目判定：无限制永不醒目；包干剩余 < 1 分钟；读秒期一律醒目
        // （读秒期本身就是贴着时限下棋的状态）。
        let urgent = match system {
            crate::play::TimeSystem::Unlimited => false,
            crate::play::TimeSystem::Absolute { .. } => clock.main < 60.0,
            crate::play::TimeSystem::Byoyomi { .. } => clock.in_byoyomi(),
        };
        let color = if urgent { theme::colors::WARN } else { Color32::from_rgb(214, 218, 226) };
        ui.horizontal_wrapped(|ui| {
            let mark = if spending { "▶" } else { "" };
            ui.label(RichText::new(format!(
                "{mark} {label}（执{}）：{text}",
                stone.name()
            ))
            .color(color)
            .size(12.0));
        });
    }
    // AI 最近一手实际思考时长（引擎实际思考时间口径的可见证据）。
    if let Some(think) = play.clock.last_engine_think {
        ui.weak(format!("引擎上一手用时 {:.1} 秒", think.as_secs_f64()));
    }
}

/// 引擎无望提示当前是否可用（与「对局」菜单置灰判定同源）。
/// 无望提示的展示在状态卡（纯文本）；「让引擎认输」在菜单。
fn hopeless_available(play: &PlayState, analysis: &AnalysisState) -> bool {
    crate::play::should_show_hopeless(play, analysis.snapshot.as_ref())
}

/// 「对局状态」卡片（纯展示）：对弈中显示「你执X」、本局规则与时限
/// （只读口径声明）、双方时钟、轮到谁 / 思考中 / 终局原因；引擎无望
/// 提示文本也在此展示。人机开关、难度、认输 / 弃着 / 让引擎认输等
/// 操作在「对局」菜单（含非对弈态的置灰原因），本卡只回答
/// 「现在轮到谁 / 还剩多少时间 / 这局怎么了」。
fn card_play_status(
    ui: &mut Ui,
    analysis: &AnalysisState,
    board: &Board,
    game: Option<&GameMeta>,
    play: &PlayState,
    hopeless: Option<&str>,
) {
    // 非对弈态整卡不渲染：状态卡描述的是「这一局」，没开对局就没有
    // 可展示的状态（开关 / 难度等设置已规整进菜单，不在这里占位）。
    if !play.mode {
        return;
    }
    card(ui, |ui| {
        theme::section_title(ui, "对局状态");
        let engine_ready = matches!(analysis.engine, EngineStatus::Ready);
        let finished = play.finished(board);
        let human_turn = !finished && board.to_play() == play.human;
        let engine_turn =
            !finished && !human_turn && engine_ready && board.cursor() == board.line_len();
        // 「思考中」两种情况：展示查询在飞，或展示已齐而走子口径查询
        // （按难度 visits）还在飞——后者才是应手快慢的决定因素。
        let engine_thinking = engine_turn
            && engine_ready
            && (analysis.analyzing() || analysis.play_pending(board, play_difficulty()));
        ui.label(format!("你执{}", play.human.name()));
        // 无望提示的「让引擎认输」是否可用随行注明（动作在「对局」菜单；
        // 判定与菜单置灰同源，用户不用去菜单里逐项试）。
        if let Some(text) = hopeless {
            ui.add_space(4.0);
            ui.colored_label(theme::colors::WARN, text);
            let menu_hint = if hopeless_available(play, analysis) {
                "可在「对局」菜单选「让引擎认输」。"
            } else {
                "「对局」菜单的「让引擎认输」此刻不可用。"
            };
            ui.weak(menu_hint);
        }
        // 本局规则与时限（对局开始时确定，对局中只读——界面可见
        // 的口径声明）。规则名取棋谱 RU 的解析结果（新对局开局时
        // 已写进 meta.rules，天然一致）。
        let rules_line = match game.and_then(|meta| meta.info.rules.as_deref()) {
            Some(raw) => {
                crate::engine::Rules::rules_name(&crate::engine::resolve_rules(None, Some(raw)).rules)
            }
            None => "中国".to_owned(),
        };
        ui.weak(format!(
            "本局规则：{rules_line} · 时限：{}（开局确定）",
            play.clock.system.name()
        ));
        // ---- 时钟（无限制也显示累计用时；剩余不多用醒目色但
        // 不闪烁——按剩余比例换一次性配色，不随帧变化）----
        clock_rows(ui, play, board.to_play(), finished);
        if finished {
            let reason = if let Some(side) = play.resigned {
                format!("{}认输：{}", side.name(), resign_text(side))
            } else if let Some(side) = play.timeout_loss {
                format!("{}超时，{}方胜", side.name(), side.opposite().name())
            } else {
                "对局结束：双方连续弃着".to_owned()
            };
            ui.colored_label(theme::colors::WARN, reason);
            // 终局出口：结果已写回元信息，此刻存谱正是时候——指明
            // 快捷键（另存入口 = Ctrl+Shift+S / 「文件 → 另存为…」）。
            ui.weak("棋谱可用 Ctrl+Shift+S 另存。");
            // 结束后进入纯复盘浏览：对弈开关保持，但不再自动应手
            // （决策函数的 two_passes / resigned 守卫兜底）。
        } else if engine_thinking {
            ui.colored_label(theme::colors::OK, "引擎思考中…");        } else if human_turn {
            ui.colored_label(theme::colors::OK, "轮到你");
        } else {
            ui.weak("等待引擎…");
        }
    });
}

/// 对弈难度的当前档位（状态卡「引擎思考中」行的等待预期用；从
/// App 的设置读取的路径随卡片瘦身后不再直通这里，等待预期以引擎卡
/// 「思考量」为准，本函数保留难度名可读性）。
fn play_difficulty() -> crate::engine::Difficulty {
    crate::engine::Difficulty::default()
}

/// 「引擎」卡片：状态点 + 权重 / 思考量 / 规则信息与设置入口。
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
            // 引擎退出时 stderr 尾行已并入 message（engine 层带出）；
            // 这里再补最近日志行，双保险可见。
            for line in analysis.log_tail_slice() {
                ui.label(RichText::new(line).size(11.0).weak());
            }
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
        info_line(ui, "思考量", &format!("{} visits", cfg.visits.max(1))).on_hover_text(
            "visits = 引擎搜索时对每个候选点的模拟访问次数，总和即思考量。             越大越强、越慢：本机实测（b18 权重 + OpenCL）约 60 visits/秒，             300 visits ≈ 5–6 秒，2000 visits ≈ 半分钟。",
        );
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
        info_line(ui, "规则", &rules_line).on_hover_text(
            "引擎按哪套规则计算胜率与目差（数子 / 数目口径不同，同一局面差             可达约 1 目）。显式指定时按设置；「自动」按棋谱 RU[] 宽容映射，             谱上未写按中国。",
        );
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
        // 最近一条引擎日志（诊断可见）：引擎崩溃 / 查询异常时用户在
        // 界面上至少能看到一行引擎侧原因（此前 `last_log` 只写不读，
        // 用户只能去翻日志文件）。弱色单行，长行自动折行。
        if let Some(line) = &analysis.last_log {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("引擎日志：{line}"))
                        .size(11.0)
                        .weak(),
                );
            });
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
        // 以下三项（轮次 RO / 地点 PC / 对局名 GN）与队名（BT/WT）、时限
        // （TM/OT）此前「解析 + 另存保留但不显示」；统一在此如实列出，
        // 没有的项不渲染（不留空行 / 占位）。
        if let Some(round) = &info.round {
            info_line(ui, "轮次", round);
        }
        if let Some(place) = &info.place {
            info_line(ui, "地点", place);
        }
        if let Some(name) = &info.game_name {
            info_line(ui, "对局名", name);
        }
        if let Some(team) = &info.team_black {
            info_line(ui, "黑方队伍", team);
        }
        if let Some(team) = &info.team_white {
            info_line(ui, "白方队伍", team);
        }
        if let Some(limit) = &info.time_limit {
            info_line(ui, "时限", limit);
        }
        if let Some(overtime) = &info.overtime {
            info_line(ui, "加时", overtime);
        }
        if let Some(rules) = &info.rules {
            info_line(ui, "规则", rules);
        }
        // 当前手的 `C` 注释（无则不显示）。
        if let Some(text) = comment {
            ui.add_space(2.0);
            ui.label(text);
        }

        // ---- 文档列表（原谱 + 各研究副本；点击切换）----
        // 列表放在侧栏滚动区内，副本多时随侧栏一起滚动。创建 / 丢弃
        // 副本在「文件 → 研究副本」菜单（含置灰原因与确认流程），
        // 列表只承担「看当前是哪份 + 点它切换」的展示职责。
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
            }
        }
    });
}

/// 文档列表的一行：全宽按钮行，活动项琥珀填充 + 左侧强调条。
/// 只承担「点击切换」（创建 / 丢弃在「文件 → 研究副本」菜单）。
fn doc_row(ui: &mut Ui, entry: &DocEntry, action: &mut PanelAction) {
    let row_height = 34.0;
    ui.horizontal(|ui| {
        let row_width = ui.available_width();
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

/// 「限定选点」信息卡（纯展示）：当前限定区域的尺寸与排除清单的
/// **内容**。区域开关 / 清除区域 / 清除排除列表 / 清除全部限制等操作
/// 在「分析 → 限定选点」菜单；单条排除的移除也在该菜单（按坐标列出）。
/// 区域与排除互斥（引擎实测 allowMoves 与 avoidMoves 不能同时给出），
/// 区域优先、排除暂不生效——互斥提示文本保留在此，用户对着清单就能
/// 看懂引擎此刻为什么「不听」某些点。
fn card_limits_info(ui: &mut Ui, analysis: &AnalysisState, board: &Board) {
    let limits = analysis.limits();
    // 无区域也无排除项时整卡不渲染：空清单没有信息量（开关在菜单），
    // 留一张「什么都没有」的卡只会稀释侧栏。
    if !limits.has_region() && limits.avoid.is_empty() {
        return;
    }
    card(ui, |ui| {
        theme::section_title(ui, "限定选点");
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
        if limits.has_region() {
            ui.colored_label(
                theme::colors::WARN,
                "区域模式下排除停用：引擎只允许区域或排除二者其一。\
                 如需排除选点，请先在菜单关闭「限定区域」。",
            );
        }
        // 排除清单内容（只读；与菜单「移除排除项」的坐标一致，用户
        // 对照得上一一对应的坐标）。
        if !limits.avoid.is_empty() {
            if !limits.has_region() {
                ui.weak("排除选点（棋盘右键空点或候选行「排除」加入）：");
            }
            let size = board.size();
            for (player, at) in &limits.avoid {
                ui.label(RichText::new(format!("{} {}", player.name(), at.to_gtp(size)))
                    .monospace());
            }
            if limits.has_region() {
                // 开区域**之前**已收进的存量排除项仍保留在列表里（关区域后
                // 恢复生效），但必须提示它们此刻不起作用——新排除入口已停用，
                // 这行只服务存量项。
                ui.colored_label(theme::colors::WARN, "区域模式下排除列表暂不生效。");
            }
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
/// 整行隐藏（不显示 0、不臆造）。视角切换入口在「分析 → 目数视角」菜单。
fn card_winrate(ui: &mut Ui, analysis: &AnalysisState) {
    let view = analysis.display_view();
    card(ui, |ui| {
        theme::section_title(ui, "胜率");
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
                        super::analysis::DisplayView::Black => "黑方",
                        super::analysis::DisplayView::Alternating => root.current_player.name(),
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
                    candidate_row(ui, analysis, info, overlay, snapshot.size, to_play, action);
                }
                // 胜率色阶图例：候选圆圈颜色的解读说明，离候选列表最近。
                winrate_legend(ui);
            }
            Some(_) => {
                ui.weak("引擎未返回候选点。");
            }
            None => {
                ui.weak("还没有分析数据——引擎就绪并分析当前局面后，候选点会显示在这里。");
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
    analysis: &AnalysisState,
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
    // 区域模式下排除入口停用（引擎 allowMoves/avoidMoves 实测互斥，收进
    // 列表也只会静默失效）：按钮置灰、悬停说明原因，不再派发动作。
    if let Some(at) = info.mv
        && let Some(player) = to_play
    {
        // 两个小钮并排（排除 / 沿主变前进），等宽对齐。
        let btn_w = (tail - 8.0) / 2.0;
        let region_on = analysis.limits().has_region();
        let (brect, btn) = ui.allocate_exact_size(
            Vec2::new(btn_w, height - 4.0),
            if region_on { Sense::hover() } else { Sense::click() },
        );
        let painter = ui.painter_at(brect);
        let hover = !region_on && (btn.hovered() || btn.is_pointer_button_down_on());
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
            if region_on {
                Color32::from_rgb(120, 124, 132)
            } else if hover {
                Color32::from_rgb(255, 150, 140)
            } else {
                Color32::from_rgb(196, 168, 168)
            },
        );
        let btn = if region_on {
            btn.on_disabled_hover_text(
                "限定区域模式下排除不生效（引擎只允许区域或排除二者其一）；\
                 如需排除选点，请先关闭「限定区域」",
            )
        } else {
            btn.on_hover_text(format!("把 {} 加入排除（avoidMoves）", at.to_gtp(size)))
        };
        if !region_on && btn.clicked() {
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
            ui.weak("还没有棋谱——载入棋谱或开始新对局后，这里汇总各严重程度的失误。");
        } else if summary.analyzed == 0 {
            // 有谱但还没算出任何损失（历史点两端不齐）：与「整盘无失误」
            // 严格区分，指引下一步动作而不是留一个空行。
            ui.weak(format!(
                "共 {total} 手，还没有可分析的手——浏览或整谱快扫后逐步积累。",
                total = summary.total
            ));
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
            deep_black,
            deep_white,
            worst,
        } = summary;
        if total == 0 {
            ui.weak("还没有棋谱——载入棋谱或开始新对局后，这里给出黑白吻合度与差异手排行。");
            return;
        }
        // 吻合度行：黑 / 白并列；样本不足（已分析 < 10 手）不给数字，
        // 显示「样本不足」（LizzieYzy 同门槛，避免少数手的均值冒充整盘）。
        // 数字下方正文给出**构成**（深度 / 快扫各多少手）——混深度下光看
        // 百分比分不清是纯快扫、纯交互还是混合（判定线见
        // [`crate::ui::analysis::DEEP_VISITS_THRESHOLD`]）。
        let black_cell = match_cell(match_black, analyzed_black, enough_black);
        let white_cell = match_cell(match_white, analyzed_white, enough_white);
        let composition = |deep: usize, analyzed: usize| -> String {
            let quick = analyzed - deep;
            if deep == 0 {
                "（快扫）".to_owned()
            } else if quick == 0 {
                "（深度分析）".to_owned()
            } else {
                format!("（快扫 {quick} 手 / 深度分析 {deep} 手）")
            }
        };
        ui.horizontal(|ui| {
            let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            let tip = match_tip();
            ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("黑吻合度").weak());
                    ui.label(RichText::new(black_cell).strong());
                    if analyzed_black > 0 {
                        ui.label(RichText::new(composition(deep_black, analyzed_black)).weak());
                    }
                });
            })
            .response
            .on_hover_text(format!("黑方 {tip}"));
            ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("白吻合度").weak());
                    ui.label(RichText::new(white_cell).strong());
                    if analyzed_white > 0 {
                        ui.label(RichText::new(composition(deep_white, analyzed_white)).weak());
                    }
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
        ui.weak(format!(
            "吻合度 ∝ 分析深度：构成按该手搜索量 ≥{} visits 计深度分析，\
             否则快扫；不同深度的数字不可互比。",
            crate::ui::analysis::DEEP_VISITS_THRESHOLD,
        ));
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

/// 胜率色阶图例：与棋盘候选点共用 [`overlay::winrate_color`] 同一映射。
/// 原在「叠加层」卡片底部；该卡摘除后图例随候选点卡展示——圆圈颜色
/// 的解读说明离候选列表最近才是它发挥作用的地方。
fn winrate_legend(ui: &mut Ui) {
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
}

/// 快扫进度卡（纯展示）：批量分析进行中显示两阶段进度（进度条 +
/// 「主扫描 / 加深 x/y」+ 已用时长）。发起、配置与取消在
/// 「分析 → 整谱快扫…」对话框（进度同步显示在对话框内）；空闲态
/// 不渲染本卡——「看起来能点但没有内容的快扫区」正是侧栏要甩掉的。
fn card_batch_progress(ui: &mut Ui, analysis: &AnalysisState) {
    let Some((deep, done, total, elapsed)) = analysis.batch_progress() else {
        return;
    };
    card(ui, |ui| {
        theme::section_title(ui, "整谱快扫");
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
        ui.weak("取消在「分析 → 整谱快扫…」对话框。");
    });
}

/// 「消息」卡片：各类用户可见提示（非法落子 / 载入另存 / 引擎错误 /
/// 引擎字段警告等）。无任何提示时不渲染（原「无提示。」占位行去除，
/// 语义不变）。
///
/// 提示分两类、两个生命周期（见 [`NoticeFeed`] 文档）：
/// - **状态型**（单槽覆盖，显示"当前状态"）：非法落子、启动提示、引擎
///   字段警告、瞬时错误、持久化失败——新状态覆盖旧状态本来就是对的；
/// - **事件型**（`event_notices` 队列，显示"发生过的事"）：载入 / 另存 /
///   快扫 / 规则解析 / 副本操作等一次性反馈，按时间排队、最多保留
///   [`MAX_NOTICES`] 条，新消息不顶掉旧消息——此前「另存成功」常被
///   后到的提示顶掉，用户根本来不及看到。
///
/// 引擎字段警告（WARN 色，引擎重启时清除、按字段名去重——去重在
/// [`AnalysisState`] 侧完成）单独列出：它是「配置可能拼错了」的提醒，
/// 与瞬时错误（红）语义不同，不能混排，也不能只落无人可见的日志。
/// 瞬时错误（查询超时 / 被拒）附「重试分析」按钮：引擎进程仍可用，
/// 点击作废已发送口径重查当前局面（`PanelAction::RetryQuery`）——
/// 此前只能靠改变局面触发重查，界面全空无从下手。
#[allow(clippy::too_many_arguments)]
fn card_messages(
    ui: &mut Ui,
    notice: Option<IllegalReason>,
    startup_notice: Option<&str>,
    load_notice: Option<&LoadNotice>,
    save_notice: Option<&LoadNotice>,
    event_notices: &NoticeFeed,
    persist_notice: Option<&str>,
    transient_error: &Option<String>,
    engine_warnings: &[super::analysis::EngineWarning],
    action: &mut PanelAction,
) {
    let has_any = notice.is_some()
        || startup_notice.is_some()
        || load_notice.is_some()
        || save_notice.is_some()
        || persist_notice.is_some()
        || transient_error.is_some()
        || !engine_warnings.is_empty()
        || !event_notices.is_empty();
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
            if wide_button(ui, "重试分析").clicked() {
                *action = PanelAction::RetryQuery;
            }
        }
        // 事件型提示按入队先后逐条渲染（最旧在上；与状态型提示并存时
        // 排在其后，状态型说明"现在怎样"，事件型说明"发生过什么"）。
        for msg in event_notices.iter() {
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
