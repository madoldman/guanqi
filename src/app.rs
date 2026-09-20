//! [`eframe::App`] 实现：主题、字体、引擎接线、棋谱打开 / 另存与主窗口
//! 布局（TASKS 3.3 / 4.2 / 5.3 / 6.x）。
//!
//! 引擎生命周期由 [`AnalysisState`] 管理：启动即加载配置并拉起引擎，
//! [`eframe::App::logic`] 每帧轮询引擎事件并按局面推进「分段加深」分析。
//!
//! 「打开棋谱」与「另存为」经 portal [`FileDialog`] 门面接线：发起立即
//! 返回、对话框在专职线程等待，`logic` 每帧 `try_recv` 非阻塞取结果；
//! 选中后读文件 → 解析 → 递归挂载整棵谱树（含变着分支）→ 整体替换棋盘
//! （尺寸跟随 SGF），并清空分析快照与胜率历史（新对局不混旧曲线）。
//! 另存把当前棋盘（含用户新建的变着）序列化为 SGF 写盘，未载入棋谱时
//! 允许存出空盘谱。本类型只负责把配置、动作与界面连起来。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::board::{Board, IllegalReason, Size, Stone};
use crate::engine::{load_settings, EngineConfig};
use crate::play::{self, GameSetup, PlayState};
use crate::portal::{FileDialog, PortalEvent};
use crate::sgf::{GameMeta, load_from_bytes, save_to_file};
use crate::ui;
use crate::ui::{
    analysis::{AnalysisState, EngineStatus, Waker},
    analysis_panel::{self, LoadNotice},
    curve, new_game, overlay, settings, tree,
};

/// 另存对话框的默认文件名：取档案路径的文件名，无法取得时退回
/// `guanqi.sgf`（新对局的档案路径为占位符，走此默认）。
fn default_sgf_name(source: &Path) -> String {
    source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "guanqi.sgf".to_owned())
}

/// 观棋主应用。
pub struct GuanqiApp {
    /// 中文字体是否加载成功；失败时在界面上给出可见提示。
    fonts_ok: bool,
    /// 棋盘状态（尺寸跟随当前局面：空盘 19 路，载入棋谱后跟随 SGF）。
    board: Board,
    /// 最近一次非法落子的原因；由棋盘视图写入、跨帧显示，成功操作后清除。
    notice: Option<IllegalReason>,
    /// 最近一次「回看中落子新建变着分支」的轻提示；成功导航 / 切分支后清除。
    branch_notice: Option<String>,
    /// 引擎接线与分析状态（状态机 + 当前局面快照）。
    analysis: AnalysisState,
    /// 棋盘叠加层状态：层开关与侧栏点击定位（本次运行内保持）。
    overlay: overlay::Overlay,
    /// 胜率曲线底部面板是否显示（面板隐藏时不创建，零开销）。
    curve_open: bool,
    /// 棋谱树底部面板是否显示（面板隐藏时不创建，零开销）。
    tree_open: bool,
    /// 棋谱树控件跨帧状态（布局缓存与自动滚动记忆）。
    tree_ui: tree::TreeUi,
    /// 棋盘替换代数：整体替换棋盘（载谱 / 新对局）时递增，混入树布局
    /// 指纹，防止「着法序列恰好相同的另一盘棋」复用过期布局。
    tree_epoch: u64,
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
    /// 等待中的 portal 对话框及其用途（`Some` = 正在等待用户操作）。
    dialog: Option<(FileDialog, PendingDialog)>,
    /// 已载入棋谱的元信息（`None` = 本次运行尚未打开过棋谱）。
    loaded: Option<GameMeta>,
    /// 最近一次「打开棋谱」的用户可见提示（成功 / 部分载入 / 失败）。
    load_notice: Option<LoadNotice>,
    /// 最近一次「另存为」的用户可见提示（成功 / 失败）。
    save_notice: Option<LoadNotice>,
    /// 人机对弈状态：模式开关、人类执子、认输与无望提示（复盘初始态）。
    play: PlayState,
    /// 新对局设置窗口是否打开。
    new_game_open: bool,
    /// 新对局设置窗口状态（编辑草稿跨窗口开关保留）。
    new_game: new_game::NewGameUi,
    /// 当前生效的贴目（新对局时设置；查询随局面发给引擎）。
    komi: f64,
}

/// 等待中的对话框用途：打开与保存各自独立接结果，互不串线。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingDialog {
    /// 等待用户选择要打开的棋谱。
    Open,
    /// 等待用户确认另存位置。
    Save,
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
            branch_notice: None,
            analysis,
            overlay: overlay::Overlay {
                show_candidates: true,
                show_heat: true,
                show_mistakes: true,
                focus: None,
            },
            curve_open: true,
            tree_open: false,
            tree_ui: tree::TreeUi::default(),
            tree_epoch: 0,
            engine_cfg,
            settings,
            settings_open: false,
            startup_notice,
            waker,
            portal_unavailable,
            dialog: None,
            loaded: None,
            load_notice: None,
            save_notice: None,
            play: PlayState::review(),
            new_game_open: false,
            new_game: new_game::NewGameUi::new(),
            komi: 7.5,
        }
    }

    /// 发起「打开棋谱」对话框（菜单入口与 Ctrl+O 共用）。
    ///
    /// - portal 不可用：不发起，提示原因（入口已置灰，快捷键仍会走到这里）；
    /// - 已有对话框在等待：**忽略本次请求并提示**。portal 每次调用都会
    ///   真实弹出一个原生对话框并占用一个专职等待线程，叠加调用会让
    ///   多个对话框同时压到用户屏幕上（且先弹的那个仍会投递结果），故必须
    ///   等当前选择完成后再发起新的。
    fn open_file_dialog(&mut self) {
        if let Some(notice) = self.dialog_guard() {
            self.load_notice = Some(notice);
            return;
        }
        match FileDialog::open_file("打开棋谱（SGF）", Some(self.waker.clone())) {
            Ok(dialog) => {
                self.dialog = Some((dialog, PendingDialog::Open));
                self.load_notice = None;
            }
            Err(err) => {
                self.load_notice = Some(LoadNotice::Failed(err.to_string()));
            }
        }
    }

    /// 发起「另存为」对话框（菜单入口与 Ctrl+Shift+S 共用）。
    /// 默认文件名取当前档案名（已含 `.sgf` 后缀），从未存过时 `guanqi.sgf`。
    fn save_file_dialog(&mut self) {
        if let Some(notice) = self.dialog_guard() {
            self.save_notice = Some(notice);
            return;
        }
        let default_name = self
            .loaded
            .as_ref()
            .map(|meta| default_sgf_name(&meta.source))
            .unwrap_or_else(|| "guanqi.sgf".to_owned());
        match FileDialog::save_file("另存棋谱（SGF）", &default_name, Some(self.waker.clone())) {
            Ok(dialog) => {
                self.dialog = Some((dialog, PendingDialog::Save));
                self.save_notice = None;
            }
            Err(err) => {
                self.save_notice = Some(LoadNotice::Failed(err.to_string()));
            }
        }
    }

    /// 对话框发起前的公共守卫：portal 不可用 / 已有对话框在等待时
    /// 返回提示（等待中的对话框不区分用途——任一在等都不允许叠加）。
    fn dialog_guard(&self) -> Option<LoadNotice> {
        if let Some(reason) = &self.portal_unavailable {
            return Some(LoadNotice::Warn(format!("文件对话框不可用：{reason}")));
        }
        if self.dialog.is_some() {
            return Some(LoadNotice::Warn(
                "已有文件对话框正在等待选择，请先完成或取消。".to_owned(),
            ));
        }
        None
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
                let (warning, partial) = (loaded.warning.clone(), loaded.partial);
                let (board, meta) = loaded.into_parts();
                // 新对局：清空分析快照与胜率历史（新对局不混旧曲线）；
                // 定位高亮所指的局面已不存在，一并清除。
                self.analysis.reset();
                self.overlay.focus = None;
                let size = board.size();
                let moves = board.move_count();
                self.board = board;
                self.loaded = Some(meta);
                // 棋盘整体替换：树布局指纹换代，不复用旧谱布局。
                self.tree_epoch = self.tree_epoch.wrapping_add(1);
                // 主变停止为「部分载入」（橙色留意）；仅变着分支被舍弃时
                // 提示照样给出，但按成功（绿色）显示。
                let notice = match warning {
                    Some(warning) if partial => LoadNotice::Warn(format!(
                        "已部分载入 {size}（全树共 {moves} 手）：{warning}"
                    )),
                    Some(warning) => LoadNotice::Ok(format!(
                        "已载入 {size}（全树共 {moves} 手，{warning}）"
                    )),
                    None => LoadNotice::Ok(format!("已载入 {size} 棋谱，全树共 {moves} 手。")),
                };
                self.load_notice = Some(notice);
            }
        }
    }

    /// 处理 portal 对话框结果（`logic` 每帧轮询取出，不阻塞）。
    fn on_portal_event(&mut self, kind: PendingDialog, event: PortalEvent) {
        match kind {
            PendingDialog::Open => match event {
                PortalEvent::Picked(path) => self.load_game(path),
                PortalEvent::Cancelled => {} // 用户取消：静默，界面保持原状
                PortalEvent::Failed(err) => {
                    self.load_notice = Some(LoadNotice::Failed(err.to_string()));
                }
            },
            PendingDialog::Save => match event {
                PortalEvent::Picked(path) => self.save_game(path),
                PortalEvent::Cancelled => {} // 用户取消：静默，界面保持原状
                PortalEvent::Failed(err) => {
                    self.save_notice = Some(LoadNotice::Failed(err.to_string()));
                }
            },
        }
    }

    /// 把当前棋盘（含用户新建的变着）写到用户确认的位置。
    /// 新对局后未另存过时用默认文件名存；写失败时提示可读原因，原状态不变。
    fn save_game(&mut self, path: PathBuf) {
        match save_to_file(&path, &self.board, self.loaded.as_ref()) {
            Ok(()) => {
                let size = self.board.size();
                let branches = self.board.nodes().len() - 1;
                self.loaded
                    .as_mut()
                    // 记住新路径：再次「另存为」默认名跟随最新位置。
                    .map(|meta| meta.source = path.clone())
                    .unwrap_or_else(|| {
                        self.loaded = Some(GameMeta::for_path(&path, self.board.size()));
                    });
                self.save_notice = Some(LoadNotice::Ok(format!(
                    "已另存到 {}（{size}，{branches} 手）。",
                    path.display()
                )));
            }
            Err(err) => {
                self.save_notice = Some(LoadNotice::Failed(format!(
                    "保存到 {} 失败：{err}",
                    path.display()
                )));
            }
        }
    }
    /// 开始新对局（新对局窗口「开始」按钮）：
    /// 让子按标准星位预摆（白先），整体替换棋盘；清空分析快照与胜率
    /// 历史（新对局不混旧曲线）；进入对弈模式并记录贴目。
    fn start_new_game(&mut self, setup: GameSetup) {
        let handicap = setup.effective_handicap();
        let stones = play::handicap_stones(setup.size, handicap);
        // 让子 > 1 时白先（标准让子棋惯例）；摆子只摆黑方星位。
        let (black, white, to_play) = if handicap > 0 {
            (stones, Vec::new(), Stone::White)
        } else {
            (Vec::new(), Vec::new(), Stone::Black)
        };
        let board = Board::from_setup(setup.size, &black, &white, &[], to_play)
            .expect("新对局的尺寸与星位坐标均合法，构造必然成功");
        self.analysis.reset();
        self.overlay.focus = None;
        self.board = board;
        // 棋盘整体替换：树布局指纹换代（与载谱同理）。
        self.tree_epoch = self.tree_epoch.wrapping_add(1);
        // 元信息同样整体替换：旧棋谱的对局信息与逐手注释不能混进新对局
        // 的另存文件；让子数记入 info，「另存」才能写出 HA[n]
        // （普通对局为 0，不写 HA）。
        let mut meta = GameMeta::for_path(Path::new("guanqi.sgf"), setup.size);
        meta.info.komi = Some(setup.komi);
        meta.info.handicap = handicap.min(9) as u8;
        self.loaded = Some(meta);
        self.komi = setup.komi;
        self.play = PlayState::new_game(&setup);
        let size = setup.size;
        let desc = if handicap > 0 {
            format!("{size} 让{handicap}子")
        } else {
            size.to_string()
        };
        self.load_notice = Some(LoadNotice::Ok(format!(
            "新对局已开始：{desc}，你执{}。",
            setup.human.name()
        )));
    }
}

impl eframe::App for GuanqiApp {
    // eframe 0.36 起不再有 `App::update(&mut self, ctx, frame)`，
    // 改为直接发放根 `Ui`；用 CentralPanel 补上背景与边距。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Ctrl+O / Ctrl+Shift+S 与菜单入口共用同一批发起函数
        // （内含等待中 / 不可用守卫）。
        if ui.input(|i| i.key_pressed(egui::Key::O) && i.modifiers.ctrl) {
            self.open_file_dialog();
        }
        if ui.input(|i| i.key_pressed(egui::Key::S) && i.modifiers.ctrl && i.modifiers.shift) {
            self.save_file_dialog();
        }
        // 顶部菜单栏：文件 → 打开棋谱… / 另存为…；对局 → 新对局…
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("文件", |ui| {
                    let disabled_reason = self.portal_unavailable.clone();
                    let mut entry = ui.add_enabled(
                        disabled_reason.is_none(),
                        egui::Button::new("打开棋谱…").shortcut_text("Ctrl+O"),
                    );
                    if let Some(reason) = disabled_reason.as_deref() {
                        entry = entry.on_disabled_hover_text(reason.to_owned());
                    }
                    if entry.clicked() {
                        self.open_file_dialog();
                        // 菜单内的普通按钮不会自动收起菜单，显式关闭。
                        ui.close();
                    }
                    ui.separator();
                    let mut entry = ui.add_enabled(
                        disabled_reason.is_none(),
                        egui::Button::new("另存为…").shortcut_text("Ctrl+Shift+S"),
                    );
                    if let Some(reason) = disabled_reason.as_deref() {
                        entry = entry.on_disabled_hover_text(reason.to_owned());
                    }
                    if entry.clicked() {
                        self.save_file_dialog();
                        ui.close();
                    }
                });
                ui.menu_button("对局", |ui| {
                    if ui.button("新对局…").clicked() {
                        self.new_game_open = true;
                        ui.close();
                    }
                });
            });
        });

        // 分析侧栏（按钮动作在绘制后执行，避免借用冲突）。
        // 当前手注释先借不可变借用取出（随游标联动，按局面签名查询）。
        let comment = self
            .loaded
            .as_ref()
            .and_then(|meta| meta.comment_at(&self.board));
        // 引擎无望提示文本（深阶段终态报告里引擎方胜率过低时给出，
        // 本帧检出后立即标记已提示，避免重复；确认按钮只清当前提示）。
        let hopeless = play::should_show_hopeless(&self.play, self.analysis.snapshot.as_ref());
        let hopeless_text = hopeless.then(|| {
            let engine = self.play.human.opposite();
            let wr = self
                .analysis
                .snapshot
                .as_ref()
                .and_then(|s| play::engine_winrate(engine, s))
                .map_or_else(|| "—".to_owned(), |wr| format!("{:.1}%", wr * 100.0));
            format!(
                "引擎认为{}方已无望（胜率 {wr}）。\
                 你可以判它认输（点「认输」并选择引擎认输），或继续对局。",
                engine.name()
            )
        });
        if hopeless {
            // 展示即记录，本次对局不再重复提示。
            play::mark_hopeless_shown(&mut self.play);
        }
        let mut panel_action = analysis_panel::PanelAction::None;
        egui::Panel::right("analysis_panel")
            .default_size(240.0)
            .resizable(true)
            .show(ui, |ui| {
                panel_action = analysis_panel::show(
                    ui,
                    &self.analysis,
                    &self.board,
                    &self.engine_cfg,
                    self.notice,
                    self.startup_notice.as_deref(),
                    &mut self.settings_open,
                    &mut self.overlay,
                    &mut self.curve_open,
                    &mut self.tree_open,
                    self.loaded.as_ref(),
                    comment,
                    self.load_notice.as_ref(),
                    self.save_notice.as_ref(),
                    &mut self.play,
                    &mut self.new_game_open,
                    hopeless_text.as_deref(),
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
            // 人类弃着：与引擎弃着走同一入口（谱树挂弃着子节点）。
            analysis_panel::PanelAction::HumanPass => {
                if self.play.mode && !self.play.finished(&self.board) {
                    self.board.pass();
                }
            }
            // 人类认输：记录认输方并给出结果提示；之后自动应手停止。
            analysis_panel::PanelAction::HumanResign => {
                if self.play.mode && !self.play.finished(&self.board) {
                    self.play.resigned = Some(self.play.human);
                }
            }
            // 确认「引擎无望」提示：只收起提示，不自动替引擎认输。
            analysis_panel::PanelAction::AckHopeless => {}
            analysis_panel::PanelAction::OpenNewGame => {}
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

        // 棋谱树底部面板：整棵对局树可视化 + 点击跳转（见 ui::tree 模块
        // 文档）。与曲线面板同为可开关面板，隐藏时不创建，零额外计算。
        // 注释索引随 `loaded` 传入（无棋谱时无注释标记）。
        if self.tree_open {
            egui::Panel::bottom("tree_panel")
                .default_size(160.0)
                .resizable(true)
                .show(ui, |ui| {
                    tree::show(
                        ui,
                        &mut self.board,
                        self.loaded.as_ref(),
                        &mut self.tree_ui,
                        self.tree_epoch,
                    );
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
                    if ui.button("新对局…").clicked() {
                        self.new_game_open = true;
                    }
                });
            });
            ui::show(
                ui,
                &mut self.board,
                &mut self.notice,
                &mut self.branch_notice,
                &self.analysis,
                &self.overlay,
                Some(&self.play),
            );
        });

        // 新对局设置窗口（确认后建盘、清分析状态并进入对弈模式）。
        if self.new_game_open {
            let ctx = ui.ctx().clone();
            let action = new_game::show(&ctx, &mut self.new_game_open, &mut self.new_game, &self.analysis.engine);
            if let new_game::NewGameAction::Start(setup) = action {
                self.start_new_game(setup);
            }
        }

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
        // 每帧轮询 portal 对话框结果（结果入队时 waker 已请求立即重绘，
        // 这里的 200ms 兜底刷新覆盖 waker 之外的边界情况）。
        let event = match &mut self.dialog {
            Some((dialog, kind)) => dialog.try_recv().map(|event| (*kind, event)),
            None => None,
        };
        if let Some((kind, event)) = event {
            self.dialog = None;
            self.on_portal_event(kind, event);
        }
        if self.dialog.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        self.analysis.sync(&self.board, &self.engine_cfg, self.komi);
        // 局面变化会先作废快照（见 AnalysisState::sync），借此时机清除定位高亮。
        if self.analysis.snapshot.is_none() {
            self.overlay.focus = None;
        }

        // 人机对弈自动应手：轮到引擎且收到该局面的深阶段终态报告时，
        // 取首选着法落子（`mv = None` 即引擎弃着）。回看历史 / 快阶段 /
        // 非终态等一切不该走的情况都由决策函数守卫（见 play 模块文档）。
        let engine_ready = matches!(self.analysis.engine, EngineStatus::Ready);
        let decision = play::engine_move_decision(
            self.play.mode,
            self.play.human,
            &self.board,
            self.analysis.snapshot.as_ref(),
            engine_ready,
        );
        // 认输状态独立短路：决策函数只看棋盘，看不到 resigned。
        // 首选着法由引擎对当前盘面搜索得出；除非盘面在报告间隙被人为
        // 改过（对弈流程内不会发生），落子必然合法，非法结果忽略。
        if self.play.resigned.is_none()
            && let Some(action) = decision
        {
            match action {
                crate::board::Action::Place(at) => {
                    let _ = self.board.play(at);
                }
                crate::board::Action::Pass => self.board.pass(),
            }
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
