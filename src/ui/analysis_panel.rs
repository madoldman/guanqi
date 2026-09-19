//! 分析侧栏：引擎状态、胜率 / 目差、候选点列表、失误统计、叠加层开关
//! 与提示行（TASKS 4.2 / 4.4）。
//!
//! 数据只读自 [`AnalysisState`]：胜率 / 目差为**黑方视角**（引擎配置
//! `reportAnalysisWinratesAs = BLACK`，实测对 rootInfo 与 moveInfos 同时生效），
//! 不按行棋方翻转。棋盘候选点叠加 / 热度图 / 失误标注（`overlay` 模块）
//! 直接取 `AnalysisState` 的快照与历史缓冲，本面板只提供层开关、失误汇总、
//! 胜率色阶图例与候选点点击定位。

use egui::{Color32, Pos2, Rect, RichText, Sense, Ui, Vec2};

use crate::board::{Board, Coord, IllegalReason, Stone};
use crate::engine::{EngineConfig, RootInfo};
use crate::sgf::GameMeta;

use super::analysis::{AnalysisState, EngineStatus, Severity};
use super::overlay::{self, Overlay};

/// 侧栏展示的候选点条数（空盘时引擎可回上百条，只取前几条；
/// 与棋盘叠加层 `CANDIDATE_LIMIT` 解耦，各自维护）。
const MOVE_LIMIT: usize = 6;

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
            Self::Ok(_) => Color32::from_rgb(140, 220, 140),
            Self::Warn(_) => Color32::from_rgb(255, 190, 90),
            Self::Failed(_) => Color32::from_rgb(255, 120, 110),
        }
    }
}

/// 状态文本与配色。
fn status_label(status: &EngineStatus, analyzing: bool) -> (String, Color32) {
    match status {
        EngineStatus::Unconfigured => {
            ("未配置权重".to_owned(), Color32::from_rgb(255, 190, 90))
        }
        EngineStatus::Starting => ("引擎启动中…".to_owned(), Color32::from_rgb(255, 190, 90)),
        EngineStatus::Ready => {
            if analyzing {
                ("分析中…".to_owned(), Color32::from_rgb(140, 220, 140))
            } else {
                ("就绪".to_owned(), Color32::from_rgb(140, 220, 140))
            }
        }
        EngineStatus::Failed(_) => ("引擎错误".to_owned(), Color32::from_rgb(255, 120, 110)),
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
fn eval_lines(root: &RootInfo) -> (String, String) {
    (
        format!("黑方胜率 {:.1}%", root.winrate * 100.0),
        format!("目差 {:+.1}", root.score_lead),
    )
}

/// 绘制分析侧栏。`settings_open` 由本面板与顶部按钮共享；
/// `overlay` 为棋盘叠加层的层开关与定位状态（本面板读写）；
/// `curve_open` 为胜率曲线底部面板的显示开关。
/// `board` 提供总手数与各行棋方（失误汇总按手数现场派生）。
/// `game` / `comment` / `load_notice` 为「打开棋谱」相关信息：已载入棋谱
/// 的元信息、当前手注释与最近一次打开操作的提示（无则对应段落不显示）。
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
    game: Option<&GameMeta>,
    comment: Option<&str>,
    load_notice: Option<&LoadNotice>,
) -> PanelAction {
    egui::ScrollArea::vertical().show(ui, |ui| {
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
            game,
            comment,
            load_notice,
        )
    })
    .inner
}

/// 侧栏正文（[`show`] 的滚动内容）。
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
    game: Option<&GameMeta>,
    comment: Option<&str>,
    load_notice: Option<&LoadNotice>,
) -> PanelAction {
    let mut action = PanelAction::None;
    ui.heading("分析");
    ui.add_space(4.0);

    // ---- 引擎状态 ----
    let (status_text, status_color) = status_label(&analysis.engine, analysis.analyzing());
    ui.horizontal(|ui| {
        ui.label(RichText::new(status_text).color(status_color).strong());
    });
    if let EngineStatus::Failed(message) = &analysis.engine {
        ui.colored_label(status_color, message);
        if ui.button("重试启动引擎").clicked() {
            action = PanelAction::RetryEngine;
        }
    }
    if matches!(analysis.engine, EngineStatus::Unconfigured) {
        ui.weak("请在设置中选择一个网络权重文件。");
    }
    if matches!(analysis.engine, EngineStatus::Starting) {
        ui.weak("模型加载或显卡调优可能需要数十秒，请稍候。");
    }
    ui.label(format!("权重：{}", model_name(cfg)));
    ui.label(format!("后端：{}", cfg.backend.name()));
    ui.label(format!("思考量：{} visits", cfg.visits.max(1)));
    if let Some(snapshot) = &analysis.snapshot {
        ui.label(format!(
            "最近耗时：{:.1} 秒（visits 上限 {}）",
            snapshot.elapsed.as_secs_f32(),
            snapshot.visits_cap
        ));
    }
    ui.horizontal(|ui| {
        if ui.button("设置…").clicked() {
            *settings_open = !*settings_open;
        }
    });

    // ---- 棋谱信息（打开棋谱后显示；属性存在才显示对应行）----
    if let Some(game) = game {
        ui.add_space(6.0);
        ui.separator();
        ui.heading("棋谱");
        let file = game
            .source
            .file_name()
            .map_or_else(|| "（无文件名）".to_owned(), |n| n.to_string_lossy().into_owned());
        let file_label = ui.label(RichText::new(file).strong());
        file_label.on_hover_text(game.source.display().to_string());
        let info = &game.info;
        if let Some(line) = player_line(&info.player_black, &info.rank_black) {
            ui.label(format!("黑：{line}"));
        }
        if let Some(line) = player_line(&info.player_white, &info.rank_white) {
            ui.label(format!("白：{line}"));
        }
        if let Some(result) = &info.result {
            ui.label(format!("结果：{result}"));
        }
        if let Some(komi) = info.komi {
            ui.label(format!("贴目：{komi}"));
        }
        if info.handicap > 0 {
            ui.label(format!("让子：{}", info.handicap));
        }
        if let Some(date) = &info.date {
            ui.label(format!("日期：{date}"));
        }
        if let Some(event) = &info.event {
            ui.label(format!("赛事：{event}"));
        }
        if let Some(rules) = &info.rules {
            ui.label(format!("规则：{rules}"));
        }
        // 当前手的 `C` 注释（无则不显示）。
        if let Some(text) = comment {
            ui.add_space(4.0);
            ui.label(text);
        }
    }

    ui.add_space(6.0);
    ui.separator();

    // ---- 胜率 / 目差 ----
    ui.heading("胜率");
    match &analysis.snapshot {
        Some(snapshot) => match &snapshot.root {
            Some(root) => {
                let (winrate, lead) = eval_lines(root);
                ui.label(RichText::new(winrate).size(22.0).strong());
                ui.label(RichText::new(lead).size(16.0));
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

    ui.add_space(6.0);
    ui.separator();

    // ---- 候选点 ----
    ui.heading("候选点");
    match &analysis.snapshot {
        Some(snapshot) if !snapshot.moves.is_empty() => {
            ui.weak("点 / 黑方胜率 / visits（点击定位）");
            for info in snapshot.moves.iter().take(MOVE_LIMIT) {
                let mv = info.mv.map_or_else(|| "弃着".to_owned(), |c| c.to_gtp(snapshot.size));
                let row = format!(
                    "{:<4}{:>7} {:>7}",
                    mv,
                    format!("{:.1}%", info.winrate * 100.0),
                    info.visits
                );
                // 当前被定位的行保持选中态，与棋盘高亮呼应。
                let selected = overlay.focus.as_ref().is_some_and(|f| Some(f.at) == info.mv);
                let response =
                    ui.selectable_label(selected, RichText::new(row).monospace());
                let response = if info.mv.is_some() {
                    response.on_hover_text("点击在棋盘上定位该点")
                } else {
                    response.on_hover_text("弃着无处定位")
                };
                if response.clicked()
                    && let Some(at) = info.mv
                {
                    let ghosts = snapshot.root.as_ref().map_or_else(Vec::new, |root| {
                        overlay::ghosts_from_pv(&info.pv, root.current_player)
                    });
                    action = PanelAction::Focus { at, ghosts };
                }
            }
        }
        Some(_) => {
            ui.weak("引擎未返回候选点。");
        }
        None => {
            ui.weak("—");
        }
    }

    ui.add_space(6.0);
    ui.separator();

    // ---- 失误统计（TASKS 4.4）----
    ui.heading("失误");
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

    ui.add_space(6.0);
    ui.separator();

    // ---- 叠加层 ----
    ui.heading("叠加层");
    ui.checkbox(&mut overlay.show_candidates, "候选点圆圈");
    ui.checkbox(&mut overlay.show_heat, "局势热度图");
    ui.checkbox(&mut overlay.show_mistakes, "失误标注");
    ui.checkbox(curve_open, "胜率曲线面板");
    // 胜率色阶图例：与棋盘候选点共用 overlay::winrate_color 同一映射。
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
                Rect::from_min_size(Pos2::new(x, rect.min.y), Vec2::new(w + 0.5, rect.height())),
                0.0,
                overlay::winrate_color(t),
            );
        }
        ui.weak("黑优");
    });
    ui.weak("圆圈大小 ∝ √visits，白环为主选点；热度深色 = 黑势、浅色 = 白势");

    ui.add_space(6.0);
    ui.separator();

    // ---- 提示行 ----
    let mut hint = false;
    if let Some(reason) = notice {
        ui.colored_label(
            Color32::from_rgb(255, 152, 82),
            format!("非法落子：{reason}"),
        );
        hint = true;
    }
    if let Some(text) = startup_notice {
        ui.colored_label(Color32::from_rgb(255, 190, 90), text);
        hint = true;
    }
    if let Some(text) = &analysis.transient_error {
        ui.colored_label(Color32::from_rgb(255, 190, 90), text);
        hint = true;
    }
    if let Some(msg) = load_notice {
        ui.colored_label(msg.color(), msg.text());
        hint = true;
    }
    if !hint {
        ui.weak("无提示。");
    }
    action
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
