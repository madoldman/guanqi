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

use std::path::Path;
use std::time::{Duration, Instant};

use crate::board::{Board, IllegalReason, MoveRecord, Size};
use crate::engine::{EngineConfig, load_settings, save_settings};
use crate::play::{self, PlayState};
use crate::portal::FileDialog;
use crate::sgf::GameMeta;
use crate::ui;
use crate::ui::{
    analysis::{AnalysisState, EngineStatus, Waker},
    analysis_panel::self,
    batch_scan, curve, mini_board, new_game, overlay, settings, tree,
};

// ---- 按职责拆出的子模块（菜单 / 对话框 / 读写 / 对弈 / 文档 / 快照）----
// 全部为 `GuanqiApp` 的方法分区实现，随本模块的类型一起构成完整的
// 应用接线；父模块的私有字段对子模块天然可见，无需提升可见性。
mod dialog;
mod doc;
mod io;
mod menu;
mod play_flow;
mod snapshot;

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
    /// 整谱快扫设置对话框是否打开（菜单「分析 → 整谱快扫…」的入口，
    /// 从侧栏卡片搬家而来：设置与预估同屏，看完预估再决定发起）。
    batch_scan_open: bool,
    /// 另存时是否写逐手注释块（「分析」菜单开关，随 ui_prefs 持久化；
    /// 默认开）。只影响「另存为」的写回内容，不改任何分析数据流。
    annotate_moves: bool,
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
    /// 当前活动文档「上次保存 / 载入」时的记录修订号（[`Board::record_rev`]）：
    /// 与活动棋盘的当前值比对即脏判定（见 [`Self::is_dirty`]）。
    /// 活动文档互换时被**新活动文档的基准**覆盖——每份文档各自判脏，
    /// 「副本里改过、切回干净的原谱」不会误报。
    saved_rev: u64,
    /// 未保存内容的待确认动作（关窗 / 新对局 / 打开棋谱，且当前文档
    /// 脏时先经确认）。与 [`Self::pending_confirm`]（副本研究确认）
    /// 分开：二者文案与出口不同，且「打开棋谱」可能先经副本确认、
    /// 再经脏确认，串联而非合并成一种窗口。
    unsaved_confirm: Option<UnsavedConfirm>,
    /// 「先另存再继续」的回调点）：
    /// [`Self::save_game`] 成功分支检查并消费。
    post_save: PostSaveAction,
    /// 已确认退出（脏确认通过或另存后续完成）：下一帧 `ui` 顶部发
    /// [`egui::ViewportCommand::Close`]。确认流程在 UI 帧内进行，而
    /// viewport 命令只能在持有 `Ui`/`Context` 时发出——用标志把「确认」
    /// 与「发命令」解耦到相邻两帧。
    exit_requested: bool,
    /// `Close` 是否已经发出（避免每帧重复发同一条命令，见
    /// [`Self::handle_close_request`]）。
    close_sent: bool,
    /// 最近发出的 viewport 命令（headless 工具 dump 用；应用路径上只追加，
    /// 由工具在 dump 后清空）。
    last_viewport_commands: std::cell::RefCell<Vec<egui::ViewportCommand>>,
    /// 偏好（界面开关 / 窗口几何）脏标记 + 防抖截止时刻：改动后停稳
    /// [`PREFS_SAVE_DEBOUNCE`] 才落盘，退出时无条件补写（子项 1/2 共用）。
    prefs_dirty_since: Option<Instant>,
}

/// 等待中的对话框用途：打开与保存各自独立接结果，互不串线。
/// `PartialEq` 仅供 headless 工具按用途注入 portal 事件。
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PendingDialog {
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

/// 未保存内容的待确认动作：当前文档的棋谱记录已改而未另存时，
/// 「关窗 / 新对局 / 打开棋谱」先经此确认。确认后执行对应原动作。
/// `PartialEq`（无 `Eq`：`GameSetup` 含 f64 贴目）仅复制性需要。
#[derive(Clone, Copy, PartialEq, Debug)]
enum UnsavedConfirm {
    /// 确认后退出应用（再发一次 `ViewportCommand::Close`）。
    Exit,
    /// 确认后开始新对局。
    NewGame(crate::play::GameSetup),
    /// 确认后发起「打开棋谱」对话框（路径由用户此后选择，同
    /// [`PendingConfirm::LoadGame`] 的理由，这里不携带路径）。
    OpenDialog,
}

impl UnsavedConfirm {
    /// 确认框说明文本（三个入口共用同一套语义：当前文档将被替换 /
    /// 应用将退出）。
    fn text(self) -> String {
        match self {
            Self::Exit => "当前棋谱有未保存的改动，关闭窗口将丢失这些改动。".to_owned(),
            Self::NewGame(_) => {
                "开始新对局会替换当前棋谱，未保存的改动将丢失。".to_owned()
            }
            Self::OpenDialog => {
                "打开棋谱会替换当前棋谱，未保存的改动将丢失。".to_owned()
            }
        }
    }
}

/// 「先另存再继续」的回调动作：另存成功后自动继续的原操作。
/// 另存失败 / 用户取消另存对话框则丢弃（用户退回编辑态，可重试）。
/// `PartialEq`（无 `Eq`：`GameSetup` 含 f64 贴目）只用于「是否为
/// [`PostSaveAction::None`]」的判定。
#[derive(Default, PartialEq, Debug)]
enum PostSaveAction {
    #[default]
    None,
    /// 继续开始新对局。
    NewGame(crate::play::GameSetup),
    /// 继续发起「打开棋谱」对话框。
    OpenDialog,
    /// 继续退出应用。
    Exit,
}

/// 未保存确认条的三个出口（绘制期收集，块外执行）。
enum UnsavedVerdict {
    /// 先另存再继续：发起另存对话框，成功后自动继续原操作。
    SaveThenContinue,
    /// 不保存继续：直接执行原操作（改动丢失）。
    Discard,
    /// 取消：回到编辑态。
    Cancel,
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
            show_pv_visits: prefs.show_pv_visits,
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
            batch_scan_open: false,
            annotate_moves: prefs.annotate_moves,
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
            saved_rev: 0,
            unsaved_confirm: None,
            post_save: PostSaveAction::None,
            exit_requested: false,
            close_sent: false,
            last_viewport_commands: std::cell::RefCell::new(Vec::new()),
            prefs_dirty_since: None,
        }
    }

    /// 标记偏好已变（防抖落盘的入口；UI 每次改开关 / 几何时调用）。
    fn mark_prefs_dirty(&mut self) {
        self.prefs_dirty_since.get_or_insert(Instant::now());
    }

    // ---- 未保存内容保护（脏判定）----

    /// 把「上次保存 / 载入」的基准修订号对齐到当前活动文档：载入棋谱、
    /// 另存成功、开始新对局（新对局自身是干净的空白内容）后调用。
    /// 活动文档的棋盘 [`Board::record_rev`] 即基准——修订号记在棋盘
    /// 对象上，随文档互换（创建 / 切换副本）整体移动，各自独立判脏。
    fn mark_saved(&mut self) {
        self.saved_rev = self.board.record_rev();
    }

    /// 当前活动文档是否有未保存的改动：棋谱记录在「上次保存 / 载入」
    /// 之后被改过（落子 / 弃着 / 悔棋 / 建变着）。导航、分支切换、
    /// 回看不改记录，不算脏。对照基准存在 `saved_rev` 里，与棋盘的
    /// `record_rev` 比对即可，无需拼接内容指纹。
    fn is_dirty(&self) -> bool {
        self.board.record_rev() != self.saved_rev
    }

    /// 关窗流程（每帧 `ui` 顶部调用）：收到 close 请求且内容脏时取消
    /// 关闭并弹确认；干净（或已确认 / 另存后续完成）则放行。
    ///
    /// 全部确认出口都通过后，真正关闭经 [`egui::ViewportCommand::Close`]
    /// 显式发出——不能只依赖「不 CancelClose」：同一帧的 close 事件
    /// 若被取消，后续必须由我们主动再请求。`on_exit` 的设置落盘在
    /// eframe 收到 Close、运行时退出时执行，此流程不拦它。
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        // 已确认退出（或另存后续完成）：发真正的关闭命令。
        //
        // **只发一次**：`exit_requested` 是粘性标志，原先每帧都重发一次 Close
        // ——真机上窗口随即关闭、无害，但这会让后续帧持续产生冗余命令
        // （headless 工具里观察到一次退出发了 21 条 Close），既脏也干扰该
        // 字段的观察用途。真正需要重试的是被取消的关闭请求（下方的
        // CancelClose 分支已覆盖），不是这里。
        if self.exit_requested {
            if !self.close_sent {
                self.close_sent = true;
                self.send_viewport(ctx, egui::ViewportCommand::Close);
            }
            return;
        }
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if !close_requested {
            return;
        }
        if self.is_dirty() {
            // 内容脏：取消本次关闭（不发 CancelClose 应用就会直接退出），
            // 弹「未保存」确认条，由用户选出口。
            self.send_viewport(ctx, egui::ViewportCommand::CancelClose);
            if self.unsaved_confirm.is_none() {
                self.unsaved_confirm = Some(UnsavedConfirm::Exit);
            }
        }
        // 干净：不取消，eframe 按默认流程退出（on_exit 落盘设置）。
    }

    /// 发 viewport 命令并留档（headless 工具断言用；留档不影响应用语义）。
    fn send_viewport(&mut self, ctx: &egui::Context, command: egui::ViewportCommand) {
        self.last_viewport_commands.borrow_mut().push(command.clone());
        ctx.send_viewport_cmd(command);
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
            show_pv_visits: overlay.show_pv_visits,
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
            annotate_moves: self.annotate_moves,
            // 窗口几何 / 文件目录记忆在各自路径上单独写入，这里原样保留。
            window_geometry: self.engine_cfg.ui_prefs.window_geometry.take(),
            last_open_dir: self.engine_cfg.ui_prefs.last_open_dir.take(),
            last_save_dir: self.engine_cfg.ui_prefs.last_save_dir.take(),
        };
    }
}

impl eframe::App for GuanqiApp {
    // eframe 0.36 起不再有 `App::update(&mut self, ctx, frame)`，
    // 改为直接发放根 `Ui`；用 CentralPanel 补上背景与边距。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 关窗保护（未保存内容）：脏时取消关闭弹确认，干净放行。
        // 必须在每帧 UI 一开始处理，迟了 eframe 已按默认语义退出。
        self.handle_close_request(ui.ctx());
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
        // 顶部菜单栏：文件（棋谱与副本）/ 对局（人机对弈与终局动作）/
        // 分析（快扫 / 限定选点 / 叠加层 / 候选显示 / 视角 / 面板）。
        // 侧栏只做信息展示，可改的设置一律规整进这三个菜单（见各菜单
        // 方法文档）：同一份状态（Overlay / 门控 / 视角 / 偏好），入口
        // 搬家不改语义。
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                self.file_menu(ui);
                self.play_menu(ui);
                self.analyze_menu(ui);
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
                 可点「让引擎认输」判它中盘负，或继续对局。",
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
                    self.manual_revealed,
                    self.loaded.as_ref(),
                    comment,
                    None,
                    None,
                    &self.notices,
                    self.persist_notice.as_deref(),
                    &self.play,
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
            // 入口：「对局 → 弃着」菜单项（执行体从侧栏按钮搬家而来，
            // 语义不变）。
            analysis_panel::PanelAction::CreateCopy => self.create_copy(),
            // 点侧栏列表项：切换到该文档（整体互换，零拷贝）。
            analysis_panel::PanelAction::SwitchDoc(number) => self.switch_doc(number),
            analysis_panel::PanelAction::ToggleAvoid { player, at } => {
                self.analysis.toggle_avoid(player, at);
            }
            // 沿候选主变前进（预览式）：只在谱上**已存在**的着法上导航。
            // 每手在当前节点的子分支里找与 PV 一致的着法（落点 + 行棋方
            // 都相同，弃着比对 Pass），找不到即停——绝不新建分支，游标
            // 停在哪算哪（用户看到棋盘走到哪）。逐段导航：若 PV 首手
            // 不在子分支里则完全不动。
            analysis_panel::PanelAction::AdvancePv { pv, .. } => {
                self.handle_advance_pv(pv);
            }
            // 局后统计排行榜点击：跳转到该手（1 起手数 → go_to 的 0..=len
            // 口径直接对应：go_to(n) = 第 n 手之后的盘面）。跳转后局面变化
            // 由既有 sync 检测并自动发起新查询，曲线 / 棋盘定位随游标联动。
            analysis_panel::PanelAction::GotoTurn(turn) => {
                self.board.go_to(turn);
            }
            analysis_panel::PanelAction::None => {}
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
                // 副本有研究成果时先确认（中止 = 新对局窗口已关，保持现状）；
                // 无副本研究但当前文档脏时走「未保存」确认。确认路径在
                // 各自的确认框出口续行（此处不动新对局窗口开关——窗口
                // 已随「开始」按钮关闭，确认后由 start_new_game 建盘）。
                if self.research_moves() > 0 {
                    self.pending_confirm = PendingConfirm::NewGame(setup);
                } else if self.is_dirty() {
                    self.unsaved_confirm = Some(UnsavedConfirm::NewGame(setup));
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

        // 整谱快扫对话框（「分析 → 整谱快扫…」入口）：设置 + 预估 +
        // 发起同屏；进行中显示进度与取消。65941bc 建立了对话框模块与
        // 菜单入口但漏了这里的绘制接线，入口打开后无窗可看——接线补齐。
        // 配置改动在对话框内即时写回 AnalysisState（见 batch_scan 模块
        // 文档），偏好持久化照旧生效。
        if self.batch_scan_open {
            let ctx = ui.ctx().clone();
            let action = batch_scan::show(
                &ctx,
                &mut self.batch_scan_open,
                &mut self.analysis,
                &self.board,
            );
            match action {
                batch_scan::BatchScanAction::Start => {
                    // 与原侧栏「开始快扫」同一执行体：发起失败的提示走
                    // 消息区。棋谱 RU 随发起传入（快扫与交互分析的规则
                    // 口径一致）。
                    let game_rules =
                        self.loaded.as_ref().and_then(|meta| meta.info.rules.as_deref());
                    if let Some(reason) =
                        self.analysis.start_batch(&self.board, self.komi, game_rules)
                    {
                        self.notices.push(analysis_panel::LoadNotice::Warn(reason));
                    }
                }
                batch_scan::BatchScanAction::Cancel => {
                    // terminate 在飞块，提示由下一帧 take_batch_notice 落位。
                    self.analysis.cancel_batch();
                }
                batch_scan::BatchScanAction::Close | batch_scan::BatchScanAction::None => {}
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
                }                Some(false) => {
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

        // 未保存内容确认条（关窗 / 新对局 / 打开棋谱，同一套出口）：
        // 「先另存再继续」（另存成功后自动继续原操作）/「不保存继续」/
        // 「取消」。三处入口共用这一个窗口，出口语义一致。
        if let Some(confirm) = self.unsaved_confirm {
            let ctx = ui.ctx().clone();
            let text = confirm.text();
            let mut verdict: Option<UnsavedVerdict> = None;
            let mut confirm_open = true;
            egui::Window::new("有未保存的改动")
                .open(&mut confirm_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.label(text);
                    ui.weak("可先 Ctrl+Shift+S 另存；下面三个出口任选其一。");
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        if ui.button("先另存再继续").clicked() {
                            verdict = Some(UnsavedVerdict::SaveThenContinue);
                        }
                        if ui.button("不保存继续").clicked() {
                            verdict = Some(UnsavedVerdict::Discard);
                        }
                        if ui.button("取消").clicked() {
                            verdict = Some(UnsavedVerdict::Cancel);
                        }
                    });
                });
            match verdict {
                Some(UnsavedVerdict::SaveThenContinue) => {
                    let confirm = self.unsaved_confirm.take().expect("分支内必为 Some");
                    let then = match confirm {
                        UnsavedConfirm::Exit => PostSaveAction::Exit,
                        UnsavedConfirm::NewGame(setup) => PostSaveAction::NewGame(setup),
                        UnsavedConfirm::OpenDialog => PostSaveAction::OpenDialog,
                    };
                    // 另存成功后自动继续 then（save_game 的 Ok 分支）。
                    // 发起失败（等待中 / portal 不可用）时回调留在
                    // post_save 上不生效？——不会：save_file_dialog_then
                    // 失败分支会把 post_save 清回 None 并提示，用户可重试。
                    self.save_file_dialog_then(then);
                    // 发起失败（dialog_guard 拦下）时回调已被清空、确认框
                    // 也已收起：把确认框放回来，用户重新选择出口。
                    if self.post_save == PostSaveAction::None {
                        self.unsaved_confirm = Some(confirm);
                    }
                }
                Some(UnsavedVerdict::Discard) => {
                    let confirm = self.unsaved_confirm.take().expect("分支内必为 Some");
                    match confirm {
                        UnsavedConfirm::Exit => self.exit_requested = true,
                        UnsavedConfirm::NewGame(setup) => self.start_new_game(setup),
                        UnsavedConfirm::OpenDialog => {
                            // 对话框发起被运行时守卫（等待中 / portal 不可用）
                            // 拦下时确认条放回：用户另选出口，原操作没有
                            // 悄悄丢掉，也没有卡死在已收起的确认框上。
                            self.open_file_dialog_now();
                            if self.dialog.is_none() && self.unsaved_confirm.is_none() {
                                self.unsaved_confirm = Some(confirm);
                            }
                        }
                    }
                }
                Some(UnsavedVerdict::Cancel) => {
                    self.unsaved_confirm = None;
                }
                // 点窗口 X（或 ESC）等同取消：确认条不得常驻。
                None if !confirm_open => self.unsaved_confirm = None,
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
