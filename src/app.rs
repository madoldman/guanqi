//! [`eframe::App`] 实现：主题、字体、引擎接线、棋谱打开与主窗口布局
//! （TASKS 3.3 / 4.2 / 5.3）。
//!
//! 引擎生命周期由 [`AnalysisState`] 管理：启动即加载配置并拉起引擎，
//! [`eframe::App::logic`] 每帧轮询引擎事件并按局面推进「分段加深」分析。
//!
//! 「打开棋谱」经 portal [`FileDialog`] 门面接线：`open_file` 立即返回、
//! 对话框在专职线程等待，`logic` 每帧 `try_recv` 非阻塞取结果；选中后
//! 读文件 → 解析 → 重放主变着 → 整体替换棋盘（尺寸跟随 SGF），并清空
//! 分析快照与胜率历史（新对局不混旧曲线）。本类型只负责把配置、动作
//! 与界面连起来。

use std::path::PathBuf;
use std::time::Duration;

use crate::board::{Board, IllegalReason, Size};
use crate::engine::{load_settings, EngineConfig};
use crate::portal::{FileDialog, PortalEvent};
use crate::sgf::{GameMeta, load_from_bytes};
use crate::ui;
use crate::ui::{
    analysis::{AnalysisState, EngineStatus, Waker},
    analysis_panel::{self, LoadNotice},
    curve, overlay, settings,
};

/// 观棋主应用。
pub struct GuanqiApp {
    /// 中文字体是否加载成功；失败时在界面上给出可见提示。
    fonts_ok: bool,
    /// 棋盘状态（尺寸跟随当前局面：空盘 19 路，载入棋谱后跟随 SGF）。
    board: Board,
    /// 最近一次非法落子的原因；由棋盘视图写入、跨帧显示，成功操作后清除。
    notice: Option<IllegalReason>,
    /// 引擎接线与分析状态（状态机 + 当前局面快照）。
    analysis: AnalysisState,
    /// 棋盘叠加层状态：层开关与侧栏点击定位（本次运行内保持）。
    overlay: overlay::Overlay,
    /// 胜率曲线底部面板是否显示（面板隐藏时不创建，零开销）。
    curve_open: bool,
    /// 当前生效的引擎配置（设置保存后更新）。
    engine_cfg: EngineConfig,
    /// 设置窗口状态（编辑草稿与权重缓存）。
    settings: settings::SettingsUi,
    /// 设置窗口是否打开。
    settings_open: bool,
    /// 首次读取配置的提示（文件损坏回退等），直到用户保存过新配置。
    startup_notice: Option<String>,
    /// 引擎事件唤醒回调（重启引擎时复用）。
    waker: Waker,
    /// portal 文件对话框不可用时的原因（`None` = 可用，启动时探测一次）。
    portal_unavailable: Option<String>,
    /// 等待中的「打开棋谱」对话框（`Some` = 正在等待用户选择）。
    dialog: Option<FileDialog>,
    /// 已载入棋谱的元信息（`None` = 本次运行尚未打开过棋谱）。
    loaded: Option<GameMeta>,
    /// 最近一次「打开棋谱」的用户可见提示（成功 / 部分载入 / 失败）。
    load_notice: Option<LoadNotice>,
}

impl GuanqiApp {
    /// 在 eframe 创建阶段完成一次性初始化（主题、字体、引擎启动、portal 探测）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // 深色主题，与项目目标系统环境一致。
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let fonts_ok = ui::install_cjk_fonts(&cc.egui_ctx);

        // 事件入队时唤醒重绘（egui::Context 可克隆且 Send + Sync）。
        let ctx = cc.egui_ctx.clone();
        let waker: Waker = std::sync::Arc::new(move || ctx.request_repaint());

        let (engine_cfg, startup_notice) = load_settings();
        let mut analysis = AnalysisState::new();
        analysis.start_engine(&engine_cfg, &waker);
        let settings = settings::SettingsUi::new(&engine_cfg);

        // portal 可用性探测（不弹窗，启动时一次）；
        // 不可用时置灰「打开棋谱」入口，并在入口悬停 / 提示行说明原因。
        let portal_unavailable = FileDialog::available().err().map(|err| err.to_string());

        Self {
            fonts_ok,
            board: Board::new(Size::new(19).expect("19 为固定合法尺寸")),
            notice: None,
            analysis,
            overlay: overlay::Overlay { show_candidates: true, show_heat: true, focus: None },
            curve_open: true,
            engine_cfg,
            settings,
            settings_open: false,
            startup_notice,
            waker,
            portal_unavailable,
            dialog: None,
            loaded: None,
            load_notice: None,
        }
    }

    /// 发起「打开棋谱」对话框（菜单入口与 Ctrl+O 共用）。
    ///
    /// - portal 不可用：不发起，提示原因（入口已置灰，快捷键仍会走到这里）；
    /// - 已有对话框在等待：**忽略本次请求并提示**。portal 每次 `open_file`
    ///   都会真实弹出一个原生对话框并占用一个专职等待线程，叠加调用会让
    ///   多个对话框同时压到用户屏幕上（且先弹的那个仍会投递结果），故必须
    ///   等当前选择完成后再发起新的。
    fn open_file_dialog(&mut self) {
        if let Some(reason) = &self.portal_unavailable {
            self.load_notice =
                Some(LoadNotice::Warn(format!("文件对话框不可用：{reason}")));
            return;
        }
        if self.dialog.is_some() {
            self.load_notice = Some(LoadNotice::Warn(
                "已有「打开棋谱」对话框正在等待选择，请先完成或取消。".to_owned(),
            ));
            return;
        }
        match FileDialog::open_file("打开棋谱（SGF）", Some(self.waker.clone())) {
            Ok(dialog) => {
                self.dialog = Some(dialog);
                self.load_notice = None;
            }
            Err(err) => {
                self.load_notice = Some(LoadNotice::Failed(err.to_string()));
            }
        }
    }

    /// 载入选中的棋谱：读文件 → 解析 → 重放主变着 → 整体替换棋盘。
    /// 读取或解析失败时**保留原棋盘**，只提示错误。
    fn load_game(&mut self, path: PathBuf) {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.load_notice = Some(LoadNotice::Failed(format!(
                    "读取 {} 失败：{err}",
                    path.display()
                )));
                return;
            }
        };
        match load_from_bytes(&path, &bytes) {
            Err(err) => {
                self.load_notice =
                    Some(LoadNotice::Failed(format!("打开棋谱失败：{err}")));
            }
            Ok(loaded) => {
                let warning = loaded.warning.clone();
                let (board, meta) = loaded.into_parts();
                // 新对局：清空分析快照与胜率历史（新对局不混旧曲线）；
                // 定位高亮所指的局面已不存在，一并清除。
                self.analysis.reset();
                self.overlay.focus = None;
                let size = board.size();
                let moves = board.move_count();
                self.board = board;
                self.loaded = Some(meta);
                self.load_notice = Some(match warning {
                    Some(warning) => LoadNotice::Warn(format!(
                        "已部分载入 {size}（共 {moves} 手）：{warning}"
                    )),
                    None => LoadNotice::Ok(format!("已载入 {size} 棋谱，共 {moves} 手。")),
                });
            }
        }
    }

    /// 处理 portal 对话框结果（`logic` 每帧轮询取出，不阻塞）。
    fn on_portal_event(&mut self, event: PortalEvent) {
        match event {
            PortalEvent::Picked(path) => self.load_game(path),
            PortalEvent::Cancelled => {} // 用户取消：静默，界面保持原状
            PortalEvent::Failed(err) => {
                self.load_notice = Some(LoadNotice::Failed(err.to_string()));
            }
        }
    }
}

impl eframe::App for GuanqiApp {
    // eframe 0.36 起不再有 `App::update(&mut self, ctx, frame)`，
    // 改为直接发放根 `Ui`；用 CentralPanel 补上背景与边距。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Ctrl+O 与菜单入口共用同一发起函数（内含等待中 / 不可用守卫）。
        if ui.input(|i| i.key_pressed(egui::Key::O) && i.modifiers.ctrl) {
            self.open_file_dialog();
        }

        // 顶部菜单栏：文件 → 打开棋谱…
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("文件", |ui| {
                    let disabled_reason = self.portal_unavailable.as_deref();
                    let mut entry = ui.add_enabled(
                        disabled_reason.is_none(),
                        egui::Button::new("打开棋谱…").shortcut_text("Ctrl+O"),
                    );
                    if let Some(reason) = disabled_reason {
                        entry = entry.on_disabled_hover_text(reason.to_owned());
                    }
                    if entry.clicked() {
                        self.open_file_dialog();
                        // 菜单内的普通按钮不会自动收起菜单，显式关闭。
                        ui.close();
                    }
                });
            });
        });

        // 分析侧栏（按钮动作在绘制后执行，避免借用冲突）。
        // 当前手注释先借不可变借用取出（随游标联动）。
        let comment = self
            .loaded
            .as_ref()
            .and_then(|meta| meta.comment_at(self.board.cursor()));
        let mut panel_action = analysis_panel::PanelAction::None;
        egui::Panel::right("analysis_panel")
            .default_size(240.0)
            .resizable(true)
            .show(ui, |ui| {
                panel_action = analysis_panel::show(
                    ui,
                    &self.analysis,
                    &self.engine_cfg,
                    self.notice,
                    self.startup_notice.as_deref(),
                    &mut self.settings_open,
                    &mut self.overlay,
                    &mut self.curve_open,
                    self.loaded.as_ref(),
                    comment,
                    self.load_notice.as_ref(),
                );
            });
        match panel_action {
            analysis_panel::PanelAction::RetryEngine => {
                self.analysis.start_engine(&self.engine_cfg, &self.waker);
            }
            analysis_panel::PanelAction::Focus { at, ghosts } => {
                // 再点同一行取消定位。
                let same = self.overlay.focus.as_ref().is_some_and(|f| f.at == at);
                self.overlay.focus =
                    if same { None } else { Some(overlay::Focus { at, ghosts }) };
            }
            analysis_panel::PanelAction::None => {}
        }

        // 胜率曲线底部面板（TASKS 4.3）：隐藏时不创建，零额外计算；
        // 需在 CentralPanel 之前创建，中央区才会让出空间。
        if self.curve_open {
            egui::Panel::bottom("curve_panel")
                .default_size(120.0)
                .resizable(true)
                .show(ui, |ui| {
                    curve::show(ui, &self.analysis, &self.board);
                });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            // 顶部一行标识；设置入口靠右。
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("观棋").size(20.0).strong());
                ui.label(egui::RichText::new("围棋 AI 引擎图形前端").weak());
                if !self.fonts_ok {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 190, 90),
                        "未找到中文字体，中文将显示为方框。\
                         请安装 adobe-source-han-sans-cn 或 noto-fonts-cjk。",
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("设置…").clicked() {
                        self.settings_open = !self.settings_open;
                    }
                });
            });
            ui::show(
                ui,
                &mut self.board,
                &mut self.notice,
                self.analysis.snapshot.as_ref(),
                &self.overlay,
            );
        });

        // 设置窗口（「保存并重启引擎」在此触发引擎进程重启）。
        if self.settings_open {
            let ctx = ui.ctx().clone();
            let action = settings::show(
                &ctx,
                &mut self.settings_open,
                &mut self.settings,
                &mut self.engine_cfg,
            );
            if matches!(action, settings::SettingsAction::ApplyRestart) {
                // 已保存的配置不会再有损坏提示。
                self.startup_notice = None;
                self.analysis.start_engine(&self.engine_cfg, &self.waker);
            }
        }
    }

    // 每帧 UI 之前轮询引擎事件（不阻塞）；窗口隐藏时同样被调用。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 轮询「打开棋谱」对话框结果（结果入队时 waker 已请求立即重绘，
        // 这里的 200ms 兜底刷新覆盖 waker 之外的边界情况）。
        if let Some(event) = self.dialog.as_mut().and_then(FileDialog::try_recv) {
            self.dialog = None;
            self.on_portal_event(event);
        }
        if self.dialog.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        self.analysis.sync(&self.board, &self.engine_cfg);
        // 局面变化会先作废快照（见 AnalysisState::sync），借此时机清除定位高亮。
        if self.analysis.snapshot.is_none() {
            self.overlay.focus = None;
        }
        // 启动 / 分析期间保持低频重绘，让状态与计时可见
        // （事件到达时 waker 已会触发立即重绘）。
        if matches!(self.analysis.engine, EngineStatus::Starting) || self.analysis.analyzing() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    // 退出时优雅关闭引擎进程（关 stdin 引擎自行退出，超时强杀）。
    fn on_exit(&mut self) {
        self.analysis.shutdown();
    }
}
