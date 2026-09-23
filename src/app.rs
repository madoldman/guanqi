//! [`eframe::App`] 实现：主题、字体、引擎接线、棋谱打开 / 另存与主窗口
//! 布局（TASKS 3.3 / 4.2 / 5.3 / 6.x）。
//!
//! 引擎生命周期由 [`AnalysisState`] 管理：启动即加载配置并拉起引擎，
//! [`eframe::App::logic`] 每帧轮询引擎事件并推进流式分析（边搜边显示）。
//!
//! 「打开棋谱」与「另存为」经 portal [`FileDialog`] 门面接线：发起立即
//! 返回、对话框在专职线程等待，`logic` 每帧 `try_recv` 非阻塞取结果；
//! 选中后读文件 → 解析 → 递归挂载整棵谱树（含变着分支）→ 整体替换棋盘
//! （尺寸跟随 SGF），并清空分析快照与胜率历史（新对局不混旧曲线）。
//! 另存把当前棋盘（含用户新建的变着）序列化为 SGF 写盘，未载入棋谱时
//! 允许存出空盘谱。本类型只负责把配置、动作与界面连起来。
//!
//! 「研究副本」（[`Doc`] / `others` 列表）：从当前手把棋谱复制成一份**
//! 独立文档**来随便试、随便研究，原谱完全不受影响；支持任意多份，且允许
//! 「在副本里再开副本」。副本内容 = 预设局面 + 当前线前缀重放
//! （[`Board::linear_prefix`]）。活动文档始终驻留主槽（`board` / `loaded`），
//! 其余文档退入 `others`；切换即两份文档整体互换（零拷贝）。每份副本有
//! 创建时分配的**稳定编号**（`number`，单调递增、丢弃不重编），标签
//! 「研究副本 2」永远指同一份；`from_move` 固定为创建时前缀手数，研究
//! 成果 = 树着法数超出它的部分。任何副本已有研究着法时，丢弃该副本 /
//! 载入新谱 / 新对局都先经 [`PendingConfirm`] 确认。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::board::{Board, IllegalReason, MoveRecord, Size, Stone};
use crate::engine::{Difficulty, EngineConfig, load_settings, save_settings};
use crate::play::{self, GameSetup, PlayState};
use crate::portal::{FileDialog, PortalEvent};
use crate::sgf::{GameMeta, load_from_bytes, save_to_file};
use crate::ui;
use crate::ui::{
    analysis::{AnalysisState, EngineStatus, Waker},
    analysis_panel::{self, LoadNotice},
    curve, mini_board, new_game, overlay, settings, tree,
};

/// 弃着终局加深评估的目标 visits：手上终态低于此值时自动发起加深
/// （800 ≈ 默认展示档 300 的 2.7 倍、40 visits 快扫的 20 倍，目差
/// 噪声收敛到远低于半目；一局已终，多花几秒把结果定稳值得）。
const TERMINAL_EVAL_TARGET_VISITS: u32 = 800;

/// 复盘 / 无对局信息时的默认贴目（`KM` 缺失时载入谱也用它分析）。
const DEFAULT_KOMI: f64 = 7.5;

/// 另存对话框的默认文件名：取档案路径的文件名，无法取得时退回
/// `guanqi.sgf`（新对局的档案路径为占位符，走此默认）。
fn default_sgf_name(source: &Path) -> String {
    source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "guanqi.sgf".to_owned())
}

/// 界面偏好与窗口几何的落盘防抖时长：用户拖窗口 / 切开关是连续事件，
/// 每次都写盘会放大 I/O；停稳 1.5 秒后一次性落盘（退出时无条件再写）。
const PREFS_SAVE_DEBOUNCE: Duration = Duration::from_millis(1500);

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
    /// 小棋盘 PV 回放状态机（第 6 项；开关在叠加层卡片，默认关）。
    mini_board: mini_board::MiniBoard,
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
    /// 最近一次难度/设置保存失败的用户提示（成功时不显示）。
    persist_notice: Option<String>,
    /// 引擎事件唤醒回调（重启引擎时复用）。
    waker: Waker,
    /// portal 文件对话框不可用时的原因（`None` = 可用，启动时探测一次）。
    portal_unavailable: Option<String>,
    /// 等待中的 portal 对话框及其用途（`Some` = 正在等待用户操作）。
    dialog: Option<(FileDialog, PendingDialog)>,
    /// 已载入棋谱的元信息（`None` = 本次运行尚未打开过棋谱）。
    loaded: Option<GameMeta>,
    /// 事件型用户提示队列（载入 / 另存 / 快扫 / 规则解析 / 副本操作等
    /// 一次性反馈）：按时间排队、保留最近 [`analysis_panel::MAX_NOTICES`]
    /// 条，新消息不顶掉旧消息（此前单槽设计会让「另存成功」被后到的
    /// 提示顶掉）。渲染在侧栏「消息」卡。
    notices: analysis_panel::NoticeFeed,
    /// 人机对弈状态：模式开关、人类执子、认输与无望提示（复盘初始态）。
    play: PlayState,
    /// 人类计时推进的上一帧时刻：每帧 `logic` 用真实墙钟差推进当前
    /// 计时方（轮到人类且在活子位置时才推进，见 `logic` 的守卫）。
    /// 仅对局进行中有效；帧间隔极端大（挂起恢复）时按实际差值扣。
    last_frame: Option<Instant>,
    /// 本局生效规则（新对局时锁定为对话框所选，`Some(规范名)`）；
    /// `None` = 复盘 / 载谱态，规则走既有「设置显式 > 棋谱 RU > 默认」
    /// 解析。**本局规则在对局开始时确定**（与时限同一精神，对局中不可
    /// 改）：设置面板的规则偏好只作用于载入的棋谱，不作用于自己开的
    /// 对局——否则「对话框选日本、设置面板显式中国」会把查询口径与
    /// 棋谱 RU 拧开。
    active_rules: Option<String>,
    /// 新对局设置窗口是否打开。
    new_game_open: bool,
    /// 新对局设置窗口状态（编辑草稿跨窗口开关保留）。
    new_game: new_game::NewGameUi,
    /// 当前生效的贴目（新对局时设置；查询随局面发给引擎）。
    komi: f64,
    /// 候选类手动显示模式下「用户已按 F」标志（第 5 项）。局面一变
    /// （签名变化）即复位，重新要求按键确认。
    manual_revealed: bool,
    /// 上次按 F 时的局面签名：比对检测「局面已变」，变化即清
    /// `manual_revealed`。
    manual_sig: Option<Vec<MoveRecord>>,
    /// 驻留内存的**非活动**文档列表（原谱与各研究副本轮流退入）。
    ///
    /// 主槽始终显示当前文档；创建副本时当前文档整体推入本列表、新副本
    /// 进主槽，切换即目标文档与主槽整体互换（零拷贝）。每项携带自己的
    /// 身份（`from_move` / `number`），互换时身份随文档走，永不混淆。
    others: Vec<Doc>,
    /// 当前活动文档的创建前缀手数（`None` = 原谱，`Some(n)` = 自第 n 手
    /// 起的副本）；与 `others` 里的文档互换时随文档走。
    active_from_move: Option<usize>,
    /// 当前活动文档的稳定编号（0 = 原谱；副本编号创建时分配、不复用）。
    active_number: usize,
    /// 下一个副本编号（单调递增；丢弃别的副本也不重编，避免用户混淆）。
    next_number: usize,
    /// 待确认的破坏性动作（丢弃副本 / 带副本研究载谱 / 带副本研究开新局）。
    pending_confirm: PendingConfirm,
    /// 偏好（界面开关 / 窗口几何）脏标记 + 防抖截止时刻：改动后停稳
    /// [`PREFS_SAVE_DEBOUNCE`] 才落盘，退出时无条件补写（子项 1/2 共用）。
    prefs_dirty_since: Option<Instant>,
}

/// 等待中的对话框用途：打开与保存各自独立接结果，互不串线。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingDialog {
    /// 等待用户选择要打开的棋谱。
    Open,
    /// 等待用户确认另存位置。
    Save,
}

/// 目录记忆的槽位（打开 / 另存各记各的，见 `remember_dir`）。
#[derive(Clone, Copy)]
enum OpenSaveDir {
    /// 「打开棋谱」的目录（`last_open_dir`）。
    Open,
    /// 「另存为」的目录（`last_save_dir`）。
    Save,
}

/// 驻留的非活动文档：与主显示槽位（`board` / `loaded`）互换的一份完整
/// 文档（原谱或研究副本），身份随文档存储、互换时一起移动。
struct Doc {
    /// 该文档的棋盘（独立谱树，与主槽互换）。
    board: Board,
    /// 该文档的元信息（`Option` 仅为与主槽 `loaded` 同型，互换零成本）。
    meta: Option<GameMeta>,
    /// 创建前缀手数：`None` = 原谱；`Some(n)` = 自第 n 手起的研究副本，
    /// 树着法数超出 n 的部分即研究成果。
    from_move: Option<usize>,
    /// 创建时分配的稳定编号（原谱恒 0；副本从 1 起单调递增，丢弃其它
    /// 副本后不重编，保证「研究副本 2」始终指同一份）。
    number: usize,
}

/// 待确认的破坏性动作（涉及丢弃副本研究成果时先经用户确认）。
/// 取消则不做任何事；确认则执行对应原动作。
#[derive(Default)]
enum PendingConfirm {
    #[default]
    None,
    /// 丢弃指定编号的研究副本（编号在发起确认时锁定，确认时定位）。
    DropCopy { number: usize },
    /// 确认后重新发起「打开棋谱」对话框。不带路径：用户选中的棋谱
    /// 路径在确认后才产生（载入对话框在确认后重新发起），早先放的
    /// `PathBuf` 恒为空串占位、从未被读取——正是本项目两次栽跟头的
    /// 「只写不读」字段，删除。
    LoadGame,
    /// 确认后开始新对局。
    NewGame(crate::play::GameSetup),
}

impl PendingConfirm {
    /// 确认框说明文本：`copies` 为将丢弃的副本份数、`moves` 为其中
    /// 无法恢复的研究着法总数（丢弃单份副本时 copies = 1）。
    fn text(&self, copies: usize, moves: usize) -> String {
        let base = match self {
            Self::DropCopy { .. } => "丢弃该研究副本后".to_owned(),
            Self::LoadGame => "载入新棋谱会替换原谱".to_owned(),
            Self::NewGame(_) => "开始新对局会替换原谱".to_owned(),
            Self::None => String::new(),
        };
        format!("{base}，{copies} 份研究副本将被丢弃，其中 {moves} 手研究成果无法恢复。继续吗？")
    }
}

impl GuanqiApp {
    /// 在 eframe 创建阶段完成一次性初始化（主题、字体、引擎启动、portal 探测）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::new_with_prefs(cc, None, Vec::new())
    }

    /// 可注入构造（headless 探针 / 验证用）：显式传入加载结果。
    /// `notice` 会作为启动提示落进消息区（与正常加载路径一致）；空
    /// `notices` 走 [`Self::new`] 同款「加载即所得」路径。
    ///
    /// eframe 的 `CreationContext` 无法在外部构造（字段含 `pub(crate)`），
    /// headless 驱动只能绕开 `new`；本构造把「读设置 → 应用偏好 → 引擎
    /// 启动」与 eframe 解耦，使两条路径共享同一初始化代码。引擎启动
    /// 失败（无权重）不影响偏好逻辑验证——引擎状态机自己显示错误。
    pub fn new_with_prefs(
        cc: &eframe::CreationContext<'_>,
        loaded: Option<(EngineConfig, Option<String>)>,
        notices: Vec<String>,
    ) -> Self {
        // 深色主题：在默认深色基础上应用观棋的统一视觉（琥珀强调、
        // 分层背景、按钮三态与圆角），启动时一次性设置，不逐帧重设。
        cc.egui_ctx.set_visuals(ui::theme::visuals());
        let fonts_ok = ui::install_cjk_fonts(&cc.egui_ctx);
        // 字号 / 间距表依赖字体就位后设置（覆盖 egui 默认值）。
        ui::theme::apply_spacing(&cc.egui_ctx);

        // 事件入队时唤醒重绘（egui::Context 可克隆且 Send + Sync）。
        let ctx = cc.egui_ctx.clone();
        let waker: Waker = std::sync::Arc::new(move || ctx.request_repaint());

        let (engine_cfg, startup_notice) = loaded.unwrap_or_else(load_settings);
        let mut startup_notice = startup_notice;
        for extra in notices {
            // 注入提示与既有启动提示并列（都属「启动期须知」，换行串接）。
            startup_notice = Some(match startup_notice {
                Some(existing) => format!("{existing}\n{extra}"),
                None => extra,
            });
        }
        let mut analysis = AnalysisState::new();
        // 设置里的规则偏好（含旧 settings.json 的存量值）进分析状态机。
        analysis.set_want_rules(engine_cfg.rules.clone());
        analysis.start_engine(&engine_cfg, &waker);
        let settings = settings::SettingsUi::new(&engine_cfg);

        // 界面偏好：从 settings.json 恢复（子项 1）。快扫配置整份带出；
        // 门控 / 视角走解析（None = 用户手改出了未知值，用默认并在消息区
        // 提示——提示文案由 load_settings 的字段级容错统一给出）。
        let prefs = engine_cfg.ui_prefs.clone();
        let batch_config = crate::ui::analysis::BatchConfig {
            visits: prefs.batch_visits.max(1),
            side: prefs.batch_side().unwrap_or(crate::ui::analysis::BatchSide::All),
            include_variations: prefs.batch_variations,
            deepen_enabled: prefs.batch_deepen,
            deepen_top: prefs.batch_deepen_top.max(1),
            deepen_visits: prefs.batch_deepen_visits.max(1),
            ..crate::ui::analysis::BatchConfig::default()
        };
        analysis.set_batch_config(batch_config);
        if let Some(gating) = prefs.gating() {
            analysis.set_gating(gating);
        }
        if let Some(view) = prefs.display_view() {
            analysis.set_display_view(view);
        }

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
                show_candidates: prefs.show_candidates,
                show_heat: prefs.show_heat,
                show_policy: prefs.show_policy,
                show_moves_heat: prefs.show_moves_heat,
                show_mistakes: prefs.show_mistakes,
                show_score_lead: prefs.show_score_lead,
                show_mini_board: prefs.show_mini_board,
                focus: None,
            },
            curve_open: prefs.curve_open,
            tree_open: prefs.tree_open,
            tree_ui: tree::TreeUi::default(),
            mini_board: mini_board::MiniBoard::default(),
            tree_epoch: 0,
            engine_cfg,
            settings,
            settings_open: false,
            startup_notice,
            persist_notice: None,
            waker,
            portal_unavailable,
            dialog: None,
            loaded: None,
            notices: analysis_panel::NoticeFeed::new(),
            play: PlayState::review(),
            last_frame: None,
            active_rules: None,
            new_game_open: false,
            new_game: new_game::NewGameUi::new(),
            komi: DEFAULT_KOMI,
            manual_revealed: false,
            manual_sig: None,
            others: Vec::new(),
            active_from_move: None,
            active_number: 0,
            next_number: 1,
            pending_confirm: PendingConfirm::None,
            prefs_dirty_since: None,
        }
    }

    /// 标记偏好已变（防抖落盘的入口；UI 每次改开关 / 几何时调用）。
    fn mark_prefs_dirty(&mut self) {
        self.prefs_dirty_since.get_or_insert(Instant::now());
    }

    /// 每帧调用：把界面开关状态快照进 `ui_prefs`，与上一帧比对——
    /// 有变化才标脏（防抖计时从真正变化起算）。窗口几何与文件目录
    /// 记忆不在此同步（各有专门的写入时机），`sync_prefs` 会保留它们。
    fn sync_prefs_and_track(&mut self) {
        let before = self.engine_cfg.ui_prefs.clone();
        self.sync_prefs();
        // 比对排除「保留型」字段后仍不等 ⇒ 有开关真变了。
        let mut now = self.engine_cfg.ui_prefs.clone();
        now.window_geometry = before.window_geometry;
        now.last_open_dir = before.last_open_dir.clone();
        now.last_save_dir = before.last_save_dir.clone();
        if now != before || self.engine_cfg.ui_prefs.gating_delay_secs != before.gating_delay_secs
        {
            self.mark_prefs_dirty();
        }
    }

    /// 把当前 UI 状态快照进 `engine_cfg.ui_prefs`（不落盘；落盘由
    /// `logic` 的防抖与 `on_exit` 负责）。
    fn sync_prefs(&mut self) {
        let overlay = &self.overlay;
        self.engine_cfg.ui_prefs = crate::engine::UiPrefs {
            show_candidates: overlay.show_candidates,
            show_heat: overlay.show_heat,
            show_policy: overlay.show_policy,
            show_moves_heat: overlay.show_moves_heat,
            show_mistakes: overlay.show_mistakes,
            show_score_lead: overlay.show_score_lead,
            show_mini_board: overlay.show_mini_board,
            curve_open: self.curve_open,
            tree_open: self.tree_open,
            candidate_gating: match self.analysis.gating() {
                crate::ui::analysis::CandidateGating::Immediate => "immediate".to_owned(),
                crate::ui::analysis::CandidateGating::Delayed { secs } => {
                    self.engine_cfg.ui_prefs.gating_delay_secs = secs;
                    "delayed".to_owned()
                }
                crate::ui::analysis::CandidateGating::Manual => "manual".to_owned(),
            },
            gating_delay_secs: self.engine_cfg.ui_prefs.gating_delay_secs,
            display_view: match self.analysis.display_view() {
                crate::ui::analysis::DisplayView::Black => "black".to_owned(),
                crate::ui::analysis::DisplayView::Alternating => "alternating".to_owned(),
            },
            batch_visits: self.analysis.batch_config().visits,
            batch_side: match self.analysis.batch_config().side {
                crate::ui::analysis::BatchSide::All => "all".to_owned(),
                crate::ui::analysis::BatchSide::BlackOnly => "black".to_owned(),
                crate::ui::analysis::BatchSide::WhiteOnly => "white".to_owned(),
            },
            batch_variations: self.analysis.batch_config().include_variations,
            batch_deepen: self.analysis.batch_config().deepen_enabled,
            batch_deepen_top: self.analysis.batch_config().deepen_top,
            batch_deepen_visits: self.analysis.batch_config().deepen_visits,
            // 窗口几何 / 文件目录记忆在各自路径上单独写入，这里原样保留。
            window_geometry: self.engine_cfg.ui_prefs.window_geometry.take(),
            last_open_dir: self.engine_cfg.ui_prefs.last_open_dir.take(),
            last_save_dir: self.engine_cfg.ui_prefs.last_save_dir.take(),
        };
    }

    /// 发起「打开棋谱」对话框（菜单入口与 Ctrl+O 共用）。
    ///
    /// - portal 不可用：不发起，提示原因（入口已置灰，快捷键仍会走到这里）；
    /// - 已有对话框在等待：**忽略本次请求并提示**。portal 每次调用都会
    ///   真实弹出一个原生对话框并占用一个专职等待线程，叠加调用会让
    ///   多个对话框同时压到用户屏幕上（且先弹的那个仍会投递结果），故必须
    ///   等当前选择完成后再发起新的。
    fn open_file_dialog(&mut self) {
        // 载入会替换原谱：所有副本（含活动若是副本）连同研究一起丢弃，
        // 用户选中的棋谱路径在确认前尚未取得，确认即重新发起对话框。
        if self.research_moves() > 0 {
            self.pending_confirm = PendingConfirm::LoadGame;
            return;
        }
        if let Some(notice) = self.dialog_guard() {
            self.notices.push(notice);
            return;
        }
        match FileDialog::open_file(
            "打开棋谱（SGF）",
            // 从上次记住的目录打开（子项 3）；目录已不存在时 portal 侧
            // 自行回退默认目录，这里不做额外校验。
            self.engine_cfg.ui_prefs.last_open_dir.as_deref(),
            Some(self.waker.clone()),
        ) {
            Ok(dialog) => {
                self.dialog = Some((dialog, PendingDialog::Open));
                // 队列不清空：先前的事件提示仍保留展示（排队上限挤出）。
            }
            Err(err) => {
                self.notices.push(LoadNotice::Failed(err.to_string()));
            }
        }
    }

    /// 确认「丢弃副本研究后载入」之后重新发起打开对话框。
    /// 此时研究成果已随确认清除（`drop_copy` 无副作用地放行了守卫）。
    fn open_file_dialog_after_confirm(&mut self) {
        if self.research_moves() == 0 {
            self.open_file_dialog();
        }
    }

    /// 发起「另存为」对话框（菜单入口与 Ctrl+Shift+S 共用）。
    /// 默认文件名：原谱取档案名；研究副本取「原名-副本N.sgf」
    ///（[`Self::copy_default_name`]），提示文件尚未真实存盘。
    fn save_file_dialog(&mut self) {
        if let Some(notice) = self.dialog_guard() {
            self.notices.push(notice);
            return;
        }
        let default_name = if self.active_from_move.is_some() {
            // 活动文档是副本：默认名带编号（来源路径仍是原谱，不能用原名）。
            self.copy_default_name(self.active_number)
        } else {
            self.loaded
                .as_ref()
                .map(|meta| default_sgf_name(&meta.source))
                .unwrap_or_else(|| "guanqi.sgf".to_owned())
        };
        match FileDialog::save_file(
            "另存棋谱（SGF）",
            &default_name,
            // 另存的初始目录：记住上次另存位置（子项 3）；无记录时用
            // 上次打开目录兜底（谱的来源目录通常就是想存过去的地方），
            // 都没有则交 portal 自选。
            self.engine_cfg
                .ui_prefs
                .last_save_dir
                .as_deref()
                .or(self.engine_cfg.ui_prefs.last_open_dir.as_deref()),
            Some(self.waker.clone()),
        ) {
            Ok(dialog) => {
                self.dialog = Some((dialog, PendingDialog::Save));
                // 队列不清空：先前的事件提示仍保留展示（排队上限挤出）。
            }
            Err(err) => {
                self.notices.push(LoadNotice::Failed(err.to_string()));
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
    /// 副本有研究成果时先替换掉它（确认框在 `open_file_dialog` 前已守卫）。
    fn load_game(&mut self, path: PathBuf) {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.notices.push(LoadNotice::Failed(format!(
                    "读取 {} 失败：{err}",
                    path.display()
                )));
                return;
            }
        };
        match load_from_bytes(&path, &bytes) {
            Err(err) => {
                self.notices.push(LoadNotice::Failed(format!("打开棋谱失败：{err}")));
            }
            Ok(loaded) => {
                let (warning, partial) = (loaded.warning.clone(), loaded.partial);
                let unplaced_props = loaded.unplaced_props;
                let (board, meta) = loaded.into_parts();
                // 载入棋谱的贴目进入分析口径：查询 komi 改取谱上的 KM
                // （缺失时默认 7.5）。此前 `komi` 只在 start_new_game 赋值，
                // 载入 KM[6.5] 的谱仍按 7.5 分析（或按上一盘的值），侧栏
                // 显示的却是谱里的真值——显示与分析口径不一致。
                self.komi = meta.info.komi.unwrap_or(DEFAULT_KOMI);
                // 载入棋谱：退出「新对局锁定规则」态，规则恢复走
                // 「设置显式 > 棋谱 RU > 默认」解析链。
                self.active_rules = None;
                // 新对局：清空分析快照与胜率历史（新对局不混旧曲线）；
                // 定位高亮所指的局面已不存在，一并清除；旧棋盘的临时提示
                //（建分支等）随棋盘替换失效，同样清除。
                self.analysis.reset();
                self.analysis.clear_limits();
                self.overlay.focus = None;
                self.branch_notice = None;
                self.notice = None;
                // 副本是原谱的附属品，离开这份棋谱即弃。带研究成果时
                // 用户已在确认框里同意（见 `open_file_dialog`），无成果
                // 时静默丢弃并在提示里带一句。
                let dropped = self.research_moves();
                self.others.clear();
                self.active_from_move = None;
                self.active_number = 0;
                self.pending_confirm = PendingConfirm::None;
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
                    Some(warning) => {
                        LoadNotice::Ok(format!("已载入 {size}（全树共 {moves} 手，{warning}）"))
                    }
                    None => LoadNotice::Ok(format!("已载入 {size} 棋谱，全树共 {moves} 手。")),
                };
                // 顺带说明副本去向：副本的「-副本」元信息随棋盘替换失效，
                // 一并清掉（副本已在上面的 `self.study = None` 丢弃）。
                let notice = if dropped > 0 {
                    LoadNotice::Warn(format!(
                        "{}（研究副本连同 {dropped} 手研究成果已丢弃。）",
                        notice.text()
                    ))
                } else {
                    notice
                };
                // 挂在被载入丢弃节点上的未识别属性（非法着法节点 /
                // 纯注释节点上的第三方属性）：另存写不出，如实提示。
                let notice = if unplaced_props > 0 {
                    LoadNotice::Warn(format!(
                        "{}（谱上另有 {unplaced_props} 条标记/扩展属性挂在无法载入的\
                         节点上，另存时将无法保留。）",
                        notice.text()
                    ))
                } else {
                    notice
                };
                self.notices.push(notice);
            }
        }
    }

    /// 处理 portal 对话框结果（`logic` 每帧轮询取出，不阻塞）。
    /// 选中即记住该文件所在目录（子项 3）：下次打开 / 另存的 portal
    /// `current_folder` 用它（随防抖落盘持久化）。
    fn on_portal_event(&mut self, kind: PendingDialog, event: PortalEvent) {
        match kind {
            PendingDialog::Open => match event {
                PortalEvent::Picked(path) => {
                    self.remember_dir(OpenSaveDir::Open, &path);
                    self.load_game(path);
                }
                PortalEvent::Cancelled => {} // 用户取消：静默，界面保持原状
                PortalEvent::Failed(err) => {
                    self.notices.push(LoadNotice::Failed(err.to_string()));
                }
            },
            PendingDialog::Save => match event {
                PortalEvent::Picked(path) => {
                    self.remember_dir(OpenSaveDir::Save, &path);
                    self.save_game(path);
                }
                PortalEvent::Cancelled => {} // 用户取消：静默，界面保持原状
                PortalEvent::Failed(err) => {
                    self.notices.push(LoadNotice::Failed(err.to_string()));
                }
            },
        }
    }

    /// 记住文件对话框选中文件所在的目录（子项 3）。目录随 `ui_prefs`
    /// 持久化（防抖 / 退出落盘），下次对话框从它打开。
    fn remember_dir(&mut self, which: OpenSaveDir, path: &Path) {
        let Some(dir) = path.parent() else { return };
        if dir.as_os_str().is_empty() {
            return;
        }
        let dir = dir.to_path_buf();
        let slot = match which {
            OpenSaveDir::Open => &mut self.engine_cfg.ui_prefs.last_open_dir,
            OpenSaveDir::Save => &mut self.engine_cfg.ui_prefs.last_save_dir,
        };
        if *slot != Some(dir.clone()) {
            *slot = Some(dir);
            self.mark_prefs_dirty();
        }
    }

    /// 把当前棋盘（含用户新建的变着）写到用户确认的位置。
    /// 新对局后未另存过时用默认文件名存；写失败时提示可读原因，原状态不变。
    ///
    /// 局后统计写回（LizzieYzy appendAiScoreBlunder 口径）：已载入棋谱时
    /// 把当前统计（[`AnalysisState::game_summary`]，与侧栏「局后统计」卡片
    /// 同一次计算口径）合成进根节点 `C[]`——界标块幂等替换，用户原有根
    /// 注释不覆盖。未载入棋谱（空盘 / 新对局）不写：新对局的统计属于
    /// 「本盘」，快扫结束后按普通另存自然带出。
    fn save_game(&mut self, path: PathBuf) {
        let stats_block = if self.loaded.is_some() {
            crate::ui::analysis::stats_block_text(
                &self.analysis.game_summary(&self.board),
                crate::ui::analysis::WORST_LIMIT,
            )
        } else {
            None
        };
        match save_to_file(
            &path,
            &self.board,
            self.loaded.as_ref(),
            stats_block.as_deref(),
        ) {
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
                self.notices.push(LoadNotice::Ok(format!(
                    "已另存到 {}（{size}，{branches} 手）。",
                    path.display()
                )));
            }
            Err(err) => {
                self.notices.push(LoadNotice::Failed(format!(
                    "保存到 {} 失败：{err}",
                    path.display()
                )));
            }
        }
    }
    /// 开始新对局（新对局窗口「开始」按钮）：
    /// 让子按标准星位预摆（白先），整体替换棋盘；清空分析快照与胜率
    /// 历史（新对局不混旧曲线）；进入对弈模式并记录贴目。
    /// 副本有研究成果时用户已在确认框里同意（见「新对局」入口）。
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
        self.analysis.clear_limits();
        self.overlay.focus = None;
        // 旧棋盘的临时提示随棋盘替换失效。
        self.branch_notice = None;
        self.notice = None;
        // 副本同样随原谱离开（研究成果已在「新对局」入口确认过）。
        let kept_research = self.research_moves();
        self.others.clear();
        self.active_from_move = None;
        self.active_number = 0;
        self.pending_confirm = PendingConfirm::None;
        self.board = board;
        // 棋盘整体替换：树布局指纹换代（与载谱同理）。
        self.tree_epoch = self.tree_epoch.wrapping_add(1);
        // 元信息同样整体替换：旧棋谱的对局信息（含上一盘的结果 RE[] /
        // 双方 PB[]/PW[]）与逐手注释不能混进新对局的另存文件；让子数
        // 记入 info，「另存」才能写出 HA[n]（普通对局为 0，不写 HA）。
        // 对局双方按执子写入（任务 B：人机对弈的 SGF 缺 PB/PW 的补口）；
        // 时限写 TM/OT（TM = 包干或读秒主时间，OT = 读秒描述）。
        let mut meta = GameMeta::for_path(Path::new("guanqi.sgf"), setup.size);
        meta.info.komi = Some(setup.komi);        meta.info.handicap = handicap.min(9) as u8;
        let (human_name, engine_name) = match setup.human {
            Stone::Black => (&mut meta.info.player_black, &mut meta.info.player_white),
            Stone::White => (&mut meta.info.player_white, &mut meta.info.player_black),
        };
        *human_name = Some("人类".to_owned());
        *engine_name = Some("观棋(KataGo)".to_owned());
        let (time_limit, overtime) = time_meta_text(setup.time_system);
        meta.info.time_limit = time_limit;
        meta.info.overtime = overtime;
        // 规则写进 RU（规范名）：打完的棋谱不再丢规则。本局规则在对局
        // 开始时确定（与时限同一精神）——设置面板的规则偏好只作用于
        // 载入的棋谱，绝不覆盖本局查询口径。
        let rules_wire = setup.rules.wire().to_owned();
        meta.info.rules = Some(rules_wire.clone());
        self.loaded = Some(meta);
        self.komi = setup.komi;
        // 本局生效规则：新对局直接取对话框所选（new_game_rules），设置
        // 面板的 rules（None = 自动 / Some = 强制）不参与本局解析。
        self.active_rules = Some(rules_wire);
        // 时限制式与新对局规则是新对局设置的一部分：落位到引擎配置并
        // 持久化（与难度同一落点），下次新对局默认带出。
        if self.engine_cfg.time_system != setup.time_system
            || self.engine_cfg.new_game_rules != setup.rules
        {
            self.engine_cfg.time_system = setup.time_system;
            self.engine_cfg.new_game_rules = setup.rules;
            if let Err(text) = save_settings(&self.engine_cfg) {
                self.persist_notice = Some(text);
            } else {
                self.persist_notice = None;
            }
        }
        // 难度是新对局设置的一部分：落位到引擎配置并持久化（与侧栏切换
        // 同一落点），下一手应手即按该档搜索。
        self.set_difficulty(setup.difficulty);
        self.play = PlayState::new_game(&setup);
        self.last_frame = None;
        let size = setup.size;
        let desc = if handicap > 0 {
            format!("{size} 让{handicap}子")
        } else {
            size.to_string()
        };
        self.notices.push(LoadNotice::Ok(format!(
            "新对局已开始：{desc}，你执{}，难度{}（{} visits），时限{}。{}",
            setup.human.name(),
            setup.difficulty.name(),
            setup.difficulty.visits(),
            setup.time_system.name(),
            if kept_research > 0 {
                format!("（研究副本连同 {kept_research} 手研究成果已丢弃。）")
            } else {
                String::new()
            }
        )));
    }

    /// 切换人机对弈难度：写回引擎配置并持久化。引擎的走子查询按当前
    /// 难度发起（`AnalysisState::sync` 逐帧比对），**下一手应手即生效**，
    /// 无需重开对局。持久化失败不影响本运行内的生效值，只提示。
    fn set_difficulty(&mut self, difficulty: Difficulty) {
        if self.engine_cfg.play_difficulty == difficulty {
            return;
        }
        self.engine_cfg.play_difficulty = difficulty;
        if let Err(text) = save_settings(&self.engine_cfg) {
            self.persist_notice = Some(text);
        } else {
            self.persist_notice = None;
        }
    }

    /// 结束对局：写回对局结果（任务 B 的 RE[] 补口）并提示。
    /// `result` 为 SGF 标准结果串（`B+R` / `W+T` / `B+3.5` / `?`…）。
    /// 结果写进 `loaded.info.result`（新对局的元信息由 `start_new_game`
    /// 整体重建，串不进下一盘；载入棋谱的对局结果原值被本盘结果覆盖
    /// ——本盘确实结束了，覆盖是事实修正）。
    /// 弃着终局的引擎判定（`Some` = 最深终态评估的 (visits, 黑方视角
    /// scoreLead)；`None` = 无可用评估）同时写进根注释界标块
    /// 「【观棋终局】」（幂等，见 `save::merge_delimited_block`）——存
    /// meta 的 root_comment，另存路径把它合入 C[]。评估**就是**本盘
    /// 结果（口径见 `terminal_result_string`），但明确标注非点目。
    fn finish_game(
        &mut self,
        result: String,
        text: String,
        evaluation: Option<(u64, f64)>,
    ) {
        // 规则名先取（借用于 loaded 之外），再进 meta 的可变借用。
        let rules_name = self.current_rules_name();
        if let Some(meta) = self.loaded.as_mut() {
            meta.info.result = Some(result);
            if let Some((_, lead)) = evaluation {
                // 写进**独立字段**而不是 root_comment：root_comment 是棋谱
                // 自带内容（载入的谱里可能有用户写的文字），另存时原样写回。
                meta.info.result_block =
                    Some(crate::ui::analysis::result_block_text(lead, rules_name.as_deref()));
            }
        }
        self.notices.push(analysis_panel::LoadNotice::Ok(text));
    }

    /// 当前生效规则的中文名（终局判定块的口径标注；取本局锁定规则或
    /// 载入棋谱的解析规则）。
    fn current_rules_name(&self) -> Option<String> {
        let wire = self.active_rules.clone().or_else(|| {
            self.loaded
                .as_ref()
                .and_then(|meta| meta.info.rules.as_deref())
                .map(|raw| crate::engine::resolve_rules(None, Some(raw)).rules)
        })?;
        crate::engine::Rules::from_wire(&wire).map(|r| r.name().to_owned())
    }

    /// 弃着终局的收尾：取当时可用的最深终局评估定 RE（无评估写 `?`），
    /// 评估不足 [`TERMINAL_EVAL_TARGET_VISITS`] 时自动发起加深评估
    /// （复用既有查询管线；到达后 [`refresh_terminal_result`] 更新）。
    /// 界面提示语按有无评估分两态（评估中 / 终局判定）。
    fn finish_two_passes(&mut self) {
        // 手上评估已够深：直接定结果。
        let deepest = self.analysis.deepest_terminal_eval(&self.board);
        if let Some((visits, lead)) = deepest
            && visits >= u64::from(TERMINAL_EVAL_TARGET_VISITS)
        {
            let result = crate::ui::analysis::terminal_result_string(lead);
            let rules = self.current_rules_name().unwrap_or_else(|| "未知".to_owned());
            let side = if result == "0" {
                "和棋".to_owned()
            } else if result.starts_with('B') {
                "黑胜".to_owned()
            } else {
                "白胜".to_owned()
            };
            let text = format!(
                "对局结束：双方连续弃着。终局判定（引擎）：{side}，{result} · \
                 按规则「{rules}」评估（{visits} visits），非点目结果。"
            );
            self.finish_game(result, text, deepest);
            return;
        }
        // 评估缺失 / 不够深：先按现状写（有浅评估也先给出暂定结果，
        // 无评估写 `?`），同时发起加深评估——到达后 refresh 更新 RE。
        let result = deepest
            .as_ref()
            .map(|(_, lead)| crate::ui::analysis::terminal_result_string(*lead))
            .unwrap_or_else(|| "?".to_owned());
        let text = if deepest.is_some() {
            "对局结束：双方连续弃着。终局评估中…（评估到达后更新结果）。".to_owned()
        } else {
            "对局结束：双方连续弃着。暂无引擎评估，SGF 记 RE[?]（评估中…）。".to_owned()
        };
        self.analysis
            .request_terminal_eval(&self.board, TERMINAL_EVAL_TARGET_VISITS);
        self.finish_game(result, text, deepest);
    }

    /// 加深评估到达后刷新弃着终局的结果（`logic` 每帧检查）：最深
    /// 评估达标时更新 RE 与根注释块，并给出终局判定提示。
    fn refresh_terminal_result(&mut self) {
        if !self.play.finished(&self.board) || !play::two_passes(&self.board) {
            return;
        }
        let Some((visits, lead)) = self.analysis.deepest_terminal_eval(&self.board) else {
            return;
        };
        if visits < u64::from(TERMINAL_EVAL_TARGET_VISITS) {
            return;
        }
        let result = crate::ui::analysis::terminal_result_string(lead);
        let already = self
            .loaded
            .as_ref()
            .and_then(|meta| meta.info.result.as_deref())
            .is_some_and(|r| r == result);
        if already {
            return;
        }
        let rules = self.current_rules_name().unwrap_or_else(|| "未知".to_owned());
        let side = if result == "0" {
            "和棋".to_owned()
        } else if result.starts_with('B') {
            "黑胜".to_owned()
        } else {
            "白胜".to_owned()
        };
        let text = format!(
            "终局判定（引擎）：{side}，{result} · 按棋谱规则「{rules}」评估 \
             （{visits} visits），非点目结果。"
        );
        self.finish_game(result, text, Some((visits, lead)));
    }

    /// 超时判负的结束路径：与认输同一条结束语义（自动应手停止、进入
    /// 复盘浏览），SGF 结果串用 `B+T` / `W+T`（时间超时）。
    fn finish_timeout(&mut self, loser: Stone) {
        self.play.timeout_loss = Some(loser);
        let winner = loser.opposite();
        let result = format!("{}+T", match winner {
            Stone::Black => "B",
            Stone::White => "W",
        });
        let text = format!(
            "{}方超时，{}方胜（{}）。对局已结束，可继续复盘浏览。",
            loser.name(),
            winner.name(),
            result
        );
        self.finish_game(result, text, None);
    }


    // ---- 研究副本 ----

    /// 按编号在 `others` 里定位文档的下标。
    fn other_index(&self, number: usize) -> Option<usize> {
        self.others.iter().position(|doc| doc.number == number)
    }

    /// 所有副本（含活动文档若是副本）各自超出创建前缀的研究着法总数。
    /// 判据只看树规模：从既有节点上「切换分支 / 回看」不算新研究，
    /// 落子与建分支（含后续整棵试验子树）都会增加全树着法数。
    fn research_moves(&self) -> usize {
        let mut total = self
            .others
            .iter()
            .filter_map(|doc| {
                doc.from_move
                    .map(|from| doc.board.move_count().saturating_sub(from))
            })
            .sum::<usize>();
        if let Some(from) = self.active_from_move {
            total += self.board.move_count().saturating_sub(from);
        }
        total
    }

    /// 副本个数（载入 / 新对局的确认文案用；活动文档若是副本也计入）。
    fn copy_count(&self) -> usize {
        self.others
            .iter()
            .filter(|doc| doc.from_move.is_some())
            .count()
            + usize::from(self.active_from_move.is_some())
    }

    /// 副本另存的默认文件名：**原谱**文件名主干 + 「-副本编号」（编号使
    /// 多份副本可区分，如 `xxx-副本1.sgf`）。从原谱主干取（而非活动文档
    /// 的名字），避免「在副本里再开副本」时嵌套出 `-副本1-副本2`。
    fn copy_default_name(&self, number: usize) -> String {
        self.loaded
            .as_ref()
            .map(|meta| default_sgf_name(&meta.source))
            .unwrap_or_else(|| "guanqi.sgf".to_owned())
            .split_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map_or_else(
                || format!("研究副本{number}.sgf"),
                |(stem, ext)| format!("{stem}-副本{number}.{ext}"),
            )
    }

    /// 从当前手创建研究副本：预设局面 + 当前线前缀重放（独立树），元信息
    /// 克隆自当前活动文档（注释按局面签名自动跟随）。当前活动文档
    /// （原谱或某份副本皆可）整体推入 `others`，新副本进主槽并成为
    /// 活动文档。对弈模式在此关闭：多份文档不能同时自动应手。
    ///
    /// 副本元信息的 `source` **保持原谱真实路径**：它只是「另存为」默认名
    /// 的来源，此前换成虚构的「-副本N.sgf」会让 UI 显示一个不存在的文件，
    /// 用户误以为副本已存盘——如实显示来源（另存前显示原谱名，另存后
    /// 由 `save_game` 更新为真实落盘路径）。
    ///
    /// 前置条件（UI 已守卫，此处 assert 兜底）：已载入棋谱。
    fn create_copy(&mut self) {
        assert!(
            self.loaded.is_some(),
            "创建副本的前置条件不满足（UI 入口应已置灰）"
        );
        let number = self.next_number;
        self.next_number += 1;
        let from_move = self.board.cursor();
        let board = self.board.linear_prefix(from_move);
        // 元信息克隆：source 不改（保持来源谱的真实路径）。另存默认名
        // 单独从编号推导（见 copy_default_name），不再借道 meta.source。
        let meta = self
            .loaded
            .as_ref()
            .expect("前置条件已检查 loaded 存在")
            .clone();
        self.play = PlayState::review();
        // 当前文档退入 others（带上身份），新副本进主槽。
        self.others.push(Doc {
            board: std::mem::replace(&mut self.board, board),
            meta: self.loaded.take(),
            from_move: self.active_from_move,
            number: self.active_number,
        });
        self.loaded = Some(meta);
        self.active_from_move = Some(from_move);
        self.active_number = number;
        self.tree_epoch = self.tree_epoch.wrapping_add(1);
        self.overlay.focus = None;
        self.branch_notice = None;
        self.notice = None;
        let notice = LoadNotice::Ok(format!(
            "已创建研究副本 {number}（自第 {from_move} 手起），原谱保持不变；\
             当前在研究副本 {number} 中。"
        ));
        self.notices.push(notice);
    }

    /// 切换到指定编号的文档：主显示槽位与 `others` 中该项**整体互换**
    /// （棋盘、元信息、身份四字段一起走，零拷贝）。
    ///
    /// **不 `analysis.reset()`**：胜率历史按局面签名索引，同一局面跨文档
    /// 共享同一曲线；切换后由既有 `sync()` 检测局面签名变化并自动发起新
    /// 查询。棋盘整体替换 → `tree_epoch` 换代（树布局不复用）；旧文档的
    /// 临时提示与定位高亮一并清除。
    ///
    /// `swap_remove` 会让 `others` 内部顺序变化，但侧栏列表按编号排序
    /// 显示（见 `doc_entries`），内部顺序不影响任何可见行为。
    fn switch_doc(&mut self, number: usize) {
        let Some(index) = self.other_index(number) else {
            return;
        };
        let incoming = self.others.swap_remove(index);
        // 当前活动文档退回列表（带上身份），目标文档换进主槽。
        self.others.push(Doc {
            board: std::mem::replace(&mut self.board, incoming.board),
            meta: self.loaded.take(),
            from_move: self.active_from_move,
            number: self.active_number,
        });
        self.loaded = incoming.meta;
        self.active_from_move = incoming.from_move;
        self.active_number = incoming.number;
        self.tree_epoch = self.tree_epoch.wrapping_add(1);
        self.overlay.focus = None;
        self.branch_notice = None;
        self.notice = None;
    }

    /// 「沿主变前进」的执行体（预览式语义 (a)）：只在谱上**已存在**的
    /// 着法上导航。每手在当前节点的子分支里找与 PV 一致的着法（落点 +
    /// 行棋方都相同，弃着比对 Pass），找不到即停——绝不新建分支，
    /// `move_count()` 不变。游标停在哪算哪（用户看到棋盘走到哪）；
    /// 若 PV 首手不在子分支里则完全不动。
    fn handle_advance_pv(&mut self, pv: Vec<Option<crate::board::Coord>>) {
        for step in pv {
            let target = match step {
                Some(at) => crate::board::Action::Place(at),
                None => crate::board::Action::Pass,
            };
            // 子分支着法 = 子节点记录；只认当前行棋方产生的记录，
            // 防止 PV 与实际轮转错位时误走对手的着法。
            let matched = (0..self.board.child_count()).find(|&i| {
                self.board
                    .child_move(i)
                    .is_some_and(|r| r.action == target && r.player == self.board.to_play())
            });
            match matched {
                Some(i) if self.board.select_child(i) => {}
                _ => break, // 谱上没有这手：预览到此为止
            }
        }
    }

    /// 切换到原谱（`others` 中 `from_move == None` 的那份）；已在原谱或
    /// 原谱不在列表时静默不动。
    fn switch_to_original(&mut self) {
        if let Some(index) = self.others.iter().position(|doc| doc.from_move.is_none()) {
            let number = self.others[index].number;
            self.switch_doc(number);
        }
    }

    /// 丢弃指定编号的研究副本（研究成果由 UI 层先经确认框守卫）。
    /// 丢的是当前活动的副本时先切回原谱再移除，保证主槽最终显示有效文档。
    fn drop_copy(&mut self, number: usize) {
        if self.active_number == number && self.active_from_move.is_some() {
            // 已载入棋谱时原谱必在 others（活动是副本的充要条件），
            // 切换后目标副本随互换退入列表，统一按编号移除。
            self.switch_to_original();
        }
        self.others.retain(|doc| doc.number != number);
        let count = self.copy_count();
        let notice = if count > 0 {
            format!("研究副本 {number} 已丢弃，其余 {count} 份副本保持不变。")
        } else {
            format!("研究副本 {number} 已丢弃。")
        };
        self.notices.push(LoadNotice::Ok(notice));
    }

    /// 指定编号文档的研究成果手数；编号不存在或为原谱时 `None`。
    fn doc_research(&self, number: usize) -> Option<usize> {
        if number == self.active_number {
            return self
                .active_from_move
                .map(|from| self.board.move_count().saturating_sub(from));
        }
        self.others
            .iter()
            .find(|doc| doc.number == number)
            .and_then(|doc| {
                doc.from_move
                    .map(|from| doc.board.move_count().saturating_sub(from))
            })
    }

    /// 侧栏文档列表（原谱在前、副本按编号升序）：活动文档 + `others`
    /// 合并而成，每项携带编号、显示名、创建前缀与研究成果。
    /// `number` 即切换 / 丢弃动作的定位键；`active` 驱动列表高亮。
    fn doc_entries(&self) -> Vec<analysis_panel::DocEntry> {
        let mut entries = Vec::with_capacity(self.others.len() + 1);
        // 活动文档：原谱显示文件名，副本显示「研究副本 N（自第 M 手起）」。
        entries.push(analysis_panel::DocEntry {
            number: self.active_number,
            name: self.active_name(),
            from_move: self.active_from_move,
            research: self
                .active_from_move
                .map_or(0, |from| self.board.move_count().saturating_sub(from)),
            active: true,
        });
        for doc in &self.others {
            entries.push(analysis_panel::DocEntry {
                number: doc.number,
                name: match doc.from_move {
                    None => self.original_name(),
                    Some(from) => {
                        let research = doc.board.move_count().saturating_sub(from);
                        format!("研究副本 {}（自第 {from} 手起）", doc.number)
                            + &if research > 0 {
                                format!("，含 {research} 手研究成果")
                            } else {
                                String::new()
                            }
                    }
                },
                from_move: doc.from_move,
                research: doc
                    .from_move
                    .map_or(0, |from| doc.board.move_count().saturating_sub(from)),
                active: false,
            });
        }
        // 原谱（number 0）置顶，副本按编号升序；编号创建时单调递增，
        // 排序稳定即创建顺序。
        entries.sort_by_key(|entry| entry.number);
        entries
    }

    /// 活动文档的显示名（原谱 = 文件名；副本 = 「研究副本 N」）。
    fn active_name(&self) -> String {
        if self.active_from_move.is_none() {
            return self.original_name();
        }
        format!("研究副本 {}", self.active_number)
    }

    /// 原谱的显示名（文件名，无文件名时给占位；未载入棋谱时不显示列表）。
    fn original_name(&self) -> String {
        self.loaded
            .as_ref()
            .and_then(|meta| meta.source.file_name())
            .map_or_else(
                || "（无文件名）".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            )
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
        // F 键：候选类手动显示模式（第 5 项）的「显示」确认。仅手动门控
        // 下消费该按键（其它模式 F 无绑定，不干扰）；局面签名记在按下时
        // 刻，此后局面一变由每帧的签名比对复位（重新要求确认）。
        let manual_gating =
            self.analysis.gating() == crate::ui::analysis::CandidateGating::Manual;
        if manual_gating && ui.input(|i| i.key_pressed(egui::Key::F)) {
            self.manual_revealed = true;
            self.manual_sig = Some(self.board.records().to_vec());
        }
        // 局面一变即撤回手动确认：游标 / 分支 / 落子 / 换谱都会改签名。
        // 顺序（先处理按键、再比对签名）意味着「同帧内局面刚变又按 F」时
        // 以新局面的签名记录确认 ⇒ 按键对当前局面生效，符合直觉。
        if self.manual_revealed
            && self
                .manual_sig
                .as_ref()
                .is_none_or(|sig| !sig.eq(self.board.records()))
        {
            self.manual_revealed = false;
            self.manual_sig = None;
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
                    ui.separator();
                    // 研究副本：仅空盘置灰（原谱 / 任意副本里都可再开副本）。
                    let copy_disabled = if self.loaded.is_none() {
                        Some("未载入棋谱，无谱可复制")
                    } else {
                        None
                    };
                    let mut entry = ui.add_enabled(
                        copy_disabled.is_none(),
                        egui::Button::new("从当前手复制为研究副本"),
                    );
                    if let Some(reason) = copy_disabled {
                        entry = entry.on_disabled_hover_text(reason.to_owned());
                    }
                    if entry.clicked() {
                        self.create_copy();
                        ui.close();
                    }
                    // 副本存在时提供「切换到原谱」快捷入口；丢弃入口唯一
                    // 化在侧栏文档列表（多副本下菜单项无法指向具体某份）。
                    if self.active_from_move.is_some() && ui.button("切换到原谱").clicked() {
                        self.switch_to_original();
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
        // 引擎无望提示文本（走子口径终态报告里引擎方胜率过低时给出，
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
        // 文档列表（侧栏「棋谱」区切换入口；空盘时为空列表）。
        let doc_entries = self.doc_entries();
        egui::Panel::right("analysis_panel")
            .default_size(280.0)
            // 宽度硬限制：卡片化内容下某些子控件会逐帧把面板自然宽度
            // 撑大（棘轮效应），不设上限时面板会吃满整个窗口。限制后
            // 拖拽仍可在 180~420 间调整，但永远不会挤没棋盘。
            .size_range(180.0..=420.0)
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
                    self.manual_revealed,
                    self.loaded.as_ref(),
                    comment,
                    None,
                    None,
                    &self.notices,
                    self.persist_notice.as_deref(),
                    &mut self.play,
                    &mut self.new_game_open,
                    hopeless_text.as_deref(),
                    &doc_entries,
                );
            });
        match panel_action {
            analysis_panel::PanelAction::RetryEngine => {
                self.analysis.start_engine(&self.engine_cfg, &self.waker);
            }
            // 查询超时 / 被拒后的「重试分析」：引擎仍可用，作废已发送
            // 口径让下一帧 sync 重新发起当前局面的查询。
            analysis_panel::PanelAction::RetryQuery => self.analysis.retry_query(),
            analysis_panel::PanelAction::Focus { at, ghosts } => {
                // 再点同一行取消定位。
                let same = self.overlay.focus.as_ref().is_some_and(|f| f.at == at);
                self.overlay.focus = if same {
                    None
                } else {
                    Some(overlay::Focus { at, ghosts })
                };
            }
            // 人类弃着：与引擎弃着走同一入口（谱树挂弃着子节点）。
            // 落子后重置该方读秒（读秒制口径：本手 M 秒内完成）；
            // 若构成双方连续弃着，对局结束——结果未知（本项目不做点目，
            // 不能拿引擎估计冒充结果），SGF 记 `?`（未知）。
            analysis_panel::PanelAction::HumanPass => {
                if self.play.mode && !self.play.finished(&self.board) {
                    let mover = self.play.human;
                    self.board.pass();
                    self.play.clock.on_human_move(mover);
                    if play::two_passes(&self.board) {
                        self.finish_two_passes();
                    }
                }
            }
            // 人类认输：记录认输方并给出结果提示；之后自动应手停止。
            // 结果写回元信息（任务 B：`B+R` / `W+R`，SGF 标准认输方
            // 记 +R，胜方为对方）。
            analysis_panel::PanelAction::HumanResign => {
                if self.play.mode && !self.play.finished(&self.board) {
                    let loser = self.play.human;
                    self.play.resigned = Some(loser);
                    let winner = loser.opposite();
                    let result = format!("{}+R", result_letter(winner));
                    let text = format!(
                        "{}认输：{}（{}）。对局已结束，可继续复盘浏览。",
                        loser.name(),
                        play::resign_text(loser),
                        result
                    );
                    self.finish_game(result, text, None);
                }
            }
            // 确认「引擎无望」提示：只收起提示，不自动替引擎认输。
            analysis_panel::PanelAction::AckHopeless => {}
            // 切换难度：立即生效（下一手应手即按新难度搜索）并持久化；
            // 保存失败只提示，当前运行内仍按新难度对弈。
            analysis_panel::PanelAction::SetDifficulty(d) => {
                self.set_difficulty(d);
            }
            analysis_panel::PanelAction::OpenNewGame => {}
            // 从当前手创建研究副本（前置条件由入口置灰与 assert 双重守卫）。
            analysis_panel::PanelAction::CreateCopy => self.create_copy(),
            // 点侧栏列表项：切换到该文档（整体互换，零拷贝）。
            analysis_panel::PanelAction::SwitchDoc(number) => self.switch_doc(number),
            // 丢弃指定副本；有研究成果时先弹确认框（无成果直接丢弃）。
            analysis_panel::PanelAction::DropCopy(number) => {
                // 编号即身份：发起确认时锁定该副本的研究手数（确认期间
                // 用户可能继续改动，但文案取确认框弹出时刻的值即可）。
                let research = self.doc_research(number).unwrap_or(0);
                if research > 0 {
                    self.pending_confirm = PendingConfirm::DropCopy { number };
                } else {
                    self.drop_copy(number);
                }
            }
            analysis_panel::PanelAction::None => {}
            // 限定选点：区域开关 / 排除增删 / 一键清除，全部转交 AnalysisState
            // （限制变更会递增版本号，sync 检测后自动重发查询）。
            analysis_panel::PanelAction::SetRegion(on) => {
                if on.is_some() {
                    // 开启只切模式；区域矩形等用户在棋盘上拖出。
                    self.analysis.enable_region_mode();
                } else {
                    self.analysis.set_region(None);
                }
            }
            // 策略热度图开关：转交 AnalysisState（want/sent 比对驱动重查）。
            analysis_panel::PanelAction::SetWantPolicy(want) => {
                self.analysis.set_want_policy(want);
            }
            // 候选点领地开关（opt-in includeMovesOwnership）：同一套
            // want/sent 比对驱动重查；聚焦候选点不重发（切焦点只换
            // 已到手的候选向量，见 overlay 模块文档）。
            analysis_panel::PanelAction::SetWantMovesHeat(want) => {
                self.analysis.set_want_moves_ownership(want);
            }
            analysis_panel::PanelAction::ToggleAvoid { player, at } => {
                self.analysis.toggle_avoid(player, at);
            }
            analysis_panel::PanelAction::RemoveAvoid(index) => {
                self.analysis.remove_avoid(index);
            }
            analysis_panel::PanelAction::ClearAvoid => {
                self.analysis.clear_avoid();
            }
            analysis_panel::PanelAction::ClearLimits => {
                self.analysis.clear_limits();
            }
            // 沿候选主变前进（预览式）：只在谱上**已存在**的着法上导航。
            // 每手在当前节点的子分支里找与 PV 一致的着法（落点 + 行棋方
            // 都相同，弃着比对 Pass），找不到即停——绝不新建分支，游标
            // 停在哪算哪（用户看到棋盘走到哪）。逐段导航：若 PV 首手
            // 不在子分支里则完全不动。
            analysis_panel::PanelAction::AdvancePv { pv, .. } => {
                self.handle_advance_pv(pv);
            }
            // 整谱快扫：按侧栏配置批量分析（报告按 turnNumber 回填逐手
            // 历史，曲线自动填满）。发起失败的提示走消息区。棋谱 RU 随
            // 发起传入（快扫与交互分析的规则口径一致）。
            analysis_panel::PanelAction::StartBatch => {
                let game_rules =
                    self.loaded.as_ref().and_then(|meta| meta.info.rules.as_deref());
                if let Some(reason) =
                    self.analysis.start_batch(&self.board, self.komi, game_rules)
                {
                    self.notices.push(analysis_panel::LoadNotice::Warn(reason));
                }
            }
            // 整谱快扫配置编辑（起止 / visits / 单方 / 含变着 / 加深）：
            // 转存进 AnalysisState，发起与预估共用同一份配置。
            analysis_panel::PanelAction::SetBatchConfig(cfg) => {
                self.analysis.set_batch_config(cfg);
            }
            // 取消整谱快扫：terminate 在飞块，提示由下一帧 take_batch_notice 落位。
            analysis_panel::PanelAction::CancelBatch => {
                self.analysis.cancel_batch();
            }
            // 局后统计排行榜点击：跳转到该手（1 起手数 → go_to 的 0..=len
            // 口径直接对应：go_to(n) = 第 n 手之后的盘面）。跳转后局面变化
            // 由既有 sync 检测并自动发起新查询，曲线 / 棋盘定位随游标联动。
            analysis_panel::PanelAction::GotoTurn(turn) => {
                self.board.go_to(turn);
            }
            // 候选类显示门控切换（第 5 项）：只改显示判定，数据流与走子
            // 决策不受影响。切回立即 / 延迟时清掉手动确认残留。
            analysis_panel::PanelAction::SetGating(gating) => {
                self.analysis.set_gating(gating);
                if gating != crate::ui::analysis::CandidateGating::Manual {
                    self.manual_revealed = false;
                    self.manual_sig = None;
                }
            }
            // 目数视角切换：转入 AnalysisState（作废快照 + want/sent 比对
            // 重发查询；历史数据存储恒黑视角，两用不清空）。
            analysis_panel::PanelAction::SetDisplayView(view) => {
                self.analysis.set_display_view(view);
            }
            // 终局提示的「另存为…」：与菜单 / Ctrl+Shift+S 完全同一动作
            // （同一发起函数，含 portal 不可用 / 等待中守卫）。
            analysis_panel::PanelAction::SaveGame => self.save_file_dialog(),
        }

        // 偏好持久化（子项 1）：面板动作 / 复选框可能改了任何界面开关。
        // 每帧把开关状态快照进 ui_prefs；内容相对上次快照有变化才标脏
        // （防抖计时从「真正变化」起算，而不是每次重绘）。
        self.sync_prefs_and_track();

        // 窗口几何记录（子项 2）：每帧从 ViewportInfo 读当前内容区矩形。
        // Wayland 下 egui-winit 会给出位置（估算）；拿不到（最小化 / 个别
        // WM）时保留几何尺寸、位置清空——下次启动交给 WM 摆放。几何变化
        // 走与开关同一套 1.5s 防抖落盘。
        let info = ui.ctx().input(|i| i.viewport().clone());
        if let Some(rect) = info.inner_rect {
            let geo = crate::engine::WindowGeometry {
                width: rect.width(),
                height: rect.height(),
                position: Some([rect.min.x, rect.min.y]),
            };
            let changed = self.engine_cfg.ui_prefs.window_geometry != Some(geo);
            self.engine_cfg.ui_prefs.window_geometry = Some(geo);
            if changed {
                self.mark_prefs_dirty();
            }
        }

        // 胜率曲线底部面板（TASKS 4.3）：隐藏时不创建，零额外计算；
        // 需在 CentralPanel 之前创建，中央区才会让出空间。
        if self.curve_open {
            egui::Panel::bottom("curve_panel")
                .default_size(120.0)
                .resizable(true)
                .show(ui, |ui| {
                    curve::show(ui, &self.analysis, &self.board, self.overlay.show_score_lead);
                });
        }

        // 小棋盘 PV 回放面板（第 6 项）：与曲线面板并列的底部面板。
        // 面板里只做预览式摆子（本地合成网格），绝不改棋谱树；数据源
        // 优先聚焦候选点、回落引擎首选，来源标注在面板标题行。
        // 隐藏时不创建，不占主棋盘空间。
        if self.overlay.show_mini_board {
            egui::Panel::bottom("mini_board_panel")
                .default_size(mini_board::MINI_SIDE + 46.0)
                .size_range(120.0..=460.0)
                .resizable(true)
                .show(ui, |ui| {
                    self.mini_board.show(
                        ui,
                        self.board.grid(),
                        self.board.size(),
                        self.board.to_play(),
                        self.analysis.snapshot.as_ref(),
                        &self.overlay,
                    );
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

        egui::CentralPanel::default().show(ui, |ui| {            // 顶部一行主操作按钮：主按钮（新对局）用琥珀填充强调，设置为普通按钮。
            // 不再在这里重复应用名——窗口标题栏已经有「观棋」。
            ui.horizontal(|ui| {
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
                    let new_game = egui::Button::new(egui::RichText::new("新对局…").strong())
                        .fill(ui::theme::colors::ACCENT_DIM)
                        .stroke(egui::Stroke::new(1.0, ui::theme::colors::ACCENT_BAR));
                    if ui.add(new_game).clicked() {
                        self.new_game_open = true;
                    }
                });
            });
            ui::show(
                ui,
                &mut self.board,
                &mut self.notice,
                &mut self.branch_notice,
                &mut self.analysis,
                &self.overlay,
                self.manual_revealed,
                Some(&mut self.play),
            );
        });

        // 新对局设置窗口（确认后建盘、清分析状态并进入对弈模式）。
        if self.new_game_open {
            let ctx = ui.ctx().clone();
            let action = new_game::show(
                &ctx,
                &mut self.new_game_open,
                &mut self.new_game,
                &self.analysis.engine,
                self.engine_cfg.play_difficulty,
                self.engine_cfg.time_system,
                self.engine_cfg.new_game_rules,
            );
            if let new_game::NewGameAction::Start(setup) = action {
                // 副本有研究成果时先确认（中止 = 新对局窗口已关，保持现状）。
                if self.research_moves() > 0 {
                    self.pending_confirm = PendingConfirm::NewGame(setup);
                } else {
                    // 面板完成使命即关闭：否则继续浮在棋盘左上角遮挡落子。
                    self.new_game_open = false;
                    self.start_new_game(setup);
                }
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
            // 规则偏好可能随「保存」变化：同步进 AnalysisState。变化会
            // 清空逐手历史 / 候选表（两种规则的数字不可混在一条曲线），
            // 并经 want/sent 比对自动重发查询（无需重启引擎——规则是
            // 查询级字段）。
            self.analysis.set_want_rules(self.engine_cfg.rules.clone());
            if matches!(action, settings::SettingsAction::ApplyRestart) {
                // 已保存的配置不会再有损坏提示。
                self.startup_notice = None;
                // 搜索线程等 cfg 级改动落到 analysis.cfg（文件已存在时
                // 也同步 numSearchThreads——此前该键只在首次生成时写入，
                // 改了等于没改）。失败不阻断重启：引擎用旧配置仍可跑。
                let cfg_path = crate::engine::effective_analysis_cfg(&self.engine_cfg);
                if let Err(text) = crate::engine::ensure_analysis_cfg(&cfg_path, &self.engine_cfg) {
                    self.persist_notice = Some(text);
                }
                self.analysis.start_engine(&self.engine_cfg, &self.waker);
            }
        }

        // 破坏性动作确认框（丢弃副本 / 带副本研究载谱 / 开新局）：
        // 模态小窗，确认才执行原动作；取消即清除，保持现状。
        if !matches!(self.pending_confirm, PendingConfirm::None) {
            // 确认框文案动态取当前值：丢弃单份副本时列该副本份数与手数；
            // 载谱 / 新对局列全部将丢弃的副本与研究成果。
            let (copies, moves) = match &self.pending_confirm {
                PendingConfirm::DropCopy { number } => (1, self.doc_research(*number).unwrap_or(0)),
                _ => (self.copy_count(), self.research_moves()),
            };
            let ctx = ui.ctx().clone();
            let text = self.pending_confirm.text(copies, moves);
            let mut verdict: Option<bool> = None;
            let mut confirm_open = true;
            egui::Window::new("丢弃研究副本？")
                .open(&mut confirm_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.label(text);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("继续（丢弃研究成果）").clicked() {
                            verdict = Some(true);
                        }
                        if ui.button("取消").clicked() {
                            verdict = Some(false);
                        }
                    });
                });
            match verdict {
                Some(true) => {
                    let action = std::mem::take(&mut self.pending_confirm);
                    match action {
                        PendingConfirm::DropCopy { number } => self.drop_copy(number),
                        // 载入的路径在确认后才由用户选择，此处重新发起对话框。
                        PendingConfirm::LoadGame => self.open_file_dialog_after_confirm(),
                        PendingConfirm::NewGame(setup) => self.start_new_game(setup),
                        PendingConfirm::None => {}
                    }
                }
                Some(false) => {
                    // 取消：若是「载入新谱」则连等待中的文件对话框一起收起，
                    // 用户重新点「打开棋谱」即可（对话框结果被忽略）。
                    if matches!(self.pending_confirm, PendingConfirm::LoadGame) {
                        self.dialog = None;
                    }
                    self.pending_confirm = PendingConfirm::None;
                }
                // 点窗口 X（或 ESC）等同取消：确认框不得常驻。
                None if !confirm_open => self.pending_confirm = PendingConfirm::None,
                None => {}
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

        // ---- 时限计时推进（真实墙钟差；推进条件 = 计时口径，见
        // play::timer 模块文档：对弈模式、未结束、轮到该方、游标在活子
        // 位置——导航 / 回看 / 浏览不计时）----
        let now = Instant::now();
        // 帧差**限幅 10 秒**：本应用会在窗口不重绘时长时间不跑帧
        // （KWin 不派发 frame callback、DPMS 熄灭、锁屏都会如此），若不限幅，
        // 恢复后的第一帧会一次性扣掉几十秒甚至几分钟，等于人类被环境判负。
        // 限幅后每次停顿最多扣 10 秒（偏向人类、有界），引擎一侧不受影响
        // ——AI 的用时按实际思考时间结算，不按帧推进。
        let frame_dt = self.last_frame.replace(now).map_or(0.0, |t| {
            now.duration_since(t).as_secs_f64().min(10.0)
        });
        let finished = self.play.finished(&self.board);
        let timing_side = self.play.mode
            && !finished
            && self.board.cursor() == self.board.line_len()
            // 引擎按实际思考时间结算（on_engine_move），不按帧推进。
            && self.board.to_play() == self.play.human;
        if timing_side {
            let human = self.play.human;
            // 读秒扣穿消耗一次但次数尚余时不判负（tick 内部已重置读秒，
            // 该手继续计时）；次数用尽或包干用尽返回超时方。
            if let Some(loser) = self.play.clock.tick(human, frame_dt) {
                self.finish_timeout(loser);
            }
        }

        // ---- 引擎时间预算：AI 出招查询带 overrideSettings.maxTime =
        // max(0.2, 该方剩余可用 − 余量)；AI 剩余 ≤ 0 ⇒ 判 AI 超时负
        // （不给引擎 0 时间；预算由 play::engine_budget 计算，难度档
        // maxVisits 仍是上限——时间只是另一条截止线）----
        let engine = self.play.human.opposite();
        let engine_budget = if self.play.mode
            && !finished
            && self.play.clock.side(engine).usable().is_some_and(|t| t <= 0.0)
        {
            self.finish_timeout(engine);
            None
        } else {
            play::engine_budget(self.play.clock.side(engine))
        };

        // 走子查询需求（App 判定，AnalysisState 不感知对弈状态）：对弈
        // 模式开启、未认输、未双方连续弃着、轮到引擎且在活子位置。
        let want_play_query = self.play.mode
            && self.play.resigned.is_none()
            && self.play.timeout_loss.is_none()
            && !play::two_passes(&self.board)
            && self.board.to_play() != self.play.human
            && self.board.cursor() == self.board.line_len();
        // 规则来源（口径见 active_rules 字段文档）：新对局锁定为开局
        // 所选（active_rules），绕过设置面板与棋谱 RU 的解析链；复盘 /
        // 载谱态走既有「设置显式 > 棋谱 RU（宽容映射）> 默认」。
        let game_rules = match self.active_rules.as_deref() {
            Some(rules) => Some(rules),
            None => self.loaded.as_ref().and_then(|meta| meta.info.rules.as_deref()),
        };
        self.analysis.sync(
            &self.board,
            &self.engine_cfg,
            self.komi,
            game_rules,
            want_play_query,
            engine_budget,
        );
        // 弃着终局的加深评估到达后刷新结果（RE / 根注释 / 提示）。
        self.refresh_terminal_result();
        // 整谱快扫结束提示（完成 / 取消 / 局面变化自动取消）落位到消息区。
        if let Some(notice) = self.analysis.take_batch_notice() {
            self.notices.push(analysis_panel::LoadNotice::Ok(notice));
        }
        // 规则解析提示（未识别的 RU 串已按默认规则分析）落位到消息区。
        if let Some(text) = self.analysis.take_rules_notice() {
            self.notices.push(analysis_panel::LoadNotice::Warn(text));
        }
        // 局面变化会先作废快照（见 AnalysisState::sync），借此时机清除定位高亮。
        if self.analysis.snapshot.is_none() {
            self.overlay.focus = None;
        }

        // 人机对弈自动应手：轮到引擎且收到该局面**按当前难度完整搜索**的
        // 终态报告时，取首选着法落子（`mv = None` 即引擎弃着）。走子依据
        // 取 `play_snapshot`（走子口径）而非展示快照——展示允许比走子浅，
        // 否则难度设置形同虚设。回看历史 / 非终态等一切不该走的情况都由
        // 决策函数守卫（见 play 模块文档）。
        let engine_ready = matches!(self.analysis.engine, EngineStatus::Ready);
        let decision = play::engine_move_decision(
            self.play.mode,
            self.play.human,
            &self.board,
            self.analysis
                .play_snapshot(&self.board, self.engine_cfg.play_difficulty),
            engine_ready,
        );
        // 认输状态独立短路：决策函数只看棋盘，看不到 resigned。落子的
        // 同时按引擎**实际思考时间**结算 AI 方时钟（口径：从发走子口径
        // 查询到收到终态；play_snapshot 就绪即终态已到）。
        // 首选着法由引擎对当前盘面搜索得出；除非盘面在报告间隙被人为
        // 改过（对弈流程内不会发生），落子必然合法，非法结果忽略。
        if self.play.resigned.is_none()
            && self.play.timeout_loss.is_none()
            && let Some(action) = decision
        {
            let think = self
                .analysis
                .play_snapshot(&self.board, self.engine_cfg.play_difficulty)
                .map(|s| s.elapsed)
                .unwrap_or_default();
            self.play.clock.on_engine_move(engine, think);
            match action {
                crate::board::Action::Place(at) => {
                    let _ = self.board.play(at);
                }
                // 引擎弃着：若构成双方连续弃着，对局结束（结果未知，
                // RE[?]；引擎估计的口径与人类弃着路径一致）。
                crate::board::Action::Pass => {
                    self.board.pass();
                    if self.play.mode && play::two_passes(&self.board) {
                        self.finish_two_passes();
                    }
                }
            }
        }

        // 启动 / 分析期间保持低频重绘，让状态与计时可见
        // （事件到达时 waker 已会触发立即重绘）。对弈中人类计时需要
        // 秒级可见的推进，空闲兜底同样保持低频即可（无限制 / 读秒的
        // 剩余显示 1s 粒度足够，无需高频重绘费电）。
        // 偏好防抖兜底唤醒：标脏后停稳 PREFS_SAVE_DEBOUNCE 落盘一次；
        // 没有这行，空闲态（700ms 兜底帧）虽也会到点，但把到点判断
        // 集中放在这里便于统一安排下一次唤醒。
        let repaint_after = if matches!(self.analysis.engine, EngineStatus::Starting)
            || self.analysis.analyzing()
            || self.analysis.batch_progress().is_some()
        {
            Duration::from_millis(500)
        } else if self.play.mode && !self.play.finished(&self.board) {
            // 对局进行中：时钟显示需要持续刷新（每 500ms 一帧，读秒
            // 剩余 / 累计用时的秒位跳动可见）。
            Duration::from_millis(500)
        } else {
            // 空闲兜底：复盘浏览时若没有任何 repaint 源（引擎空闲、无对话框、
            // 无输入），egui 会进入无限期 idle；此时落子 / 导航等在「输入唤醒的
            // 帧串」里推进的状态可能停在没有新帧可画的状态（用户可见「假死」，
            // 改一次窗口大小才刷新）。低频唤醒保证最终状态总能落到屏幕上。
            Duration::from_millis(700)
        };

        // ---- 偏好 / 窗口几何防抖落盘（子项 1/2 共用）----
        if let Some(since) = self.prefs_dirty_since
            && since.elapsed() >= PREFS_SAVE_DEBOUNCE
        {
            self.prefs_dirty_since = None;
            if let Err(text) = save_settings(&self.engine_cfg) {
                self.persist_notice = Some(text);
            }
            // 落盘后仍保证有下一帧（当前帧可能就是兜底唤醒帧）。
            ctx.request_repaint_after(repaint_after);
        } else {
            let wait = self
                .prefs_dirty_since
                .map(|since| {
                    PREFS_SAVE_DEBOUNCE
                        .saturating_sub(since.elapsed())
                        .max(repaint_after)
                })
                .unwrap_or(repaint_after);
            ctx.request_repaint_after(wait);
        }
    }

    // 退出时优雅关闭引擎进程（关 stdin 引擎自行退出，超时强杀）。
    // 偏好与窗口几何无条件补写一次（防抖未到点的改动不丢）。
    fn on_exit(&mut self) {
        self.sync_prefs();
        let _ = save_settings(&self.engine_cfg);
        self.analysis.shutdown();
    }
}

/// 时限制式的 SGF 元信息文本（`TM` / `OT`）：包干与读秒的主时间都进
/// `TM`；读秒描述（每手秒数 × 次数）进 `OT`。无限制不写（`None`，
/// 与 SGF 惯例一致——未配置时限的谱不落 TM/OT）。
fn time_meta_text(system: play::TimeSystem) -> (Option<String>, Option<String>) {
    match system {
        play::TimeSystem::Unlimited => (None, None),
        play::TimeSystem::Absolute { seconds } => {
            (Some(format!("{} 分钟", (seconds / 60.0).round() as u64)), None)
        }
        play::TimeSystem::Byoyomi { main_seconds, period_seconds, periods } => (
            Some(format!("{} 分钟", (main_seconds / 60.0).round() as u64)),
            Some(format!("{} 次 × {} 秒读秒", periods, period_seconds.round() as u64)),
        ),
    }
}

/// 胜方的 SGF 结果串字母（`B+R` / `W+T` 等的 `B` / `W` 部分）。
fn result_letter(winner: Stone) -> &'static str {
    match winner {
        Stone::Black => "B",
        Stone::White => "W",
    }
}

