//! 引擎配置与应用设置数据层（阶段 3.3 的非 UI 部分）。
//!
//! KataGo 是**外部依赖**而非项目自身的一部分，由用户自备：用哪个引擎
//! 路径、哪个权重、多少思考量是可配置项；而引擎的**构建/后端**（OpenCL
//! 版还是 Eigen 版）由用户安装的引擎二进制决定，不属于本项目设置项——
//! 历史上这里曾有一个 `backend` 字段，但它从未传给引擎，是个假开关，
//! 已删除（旧配置文件里的 `"backend"` 键按退休项静默忽略）。
//! 本模块只负责给出**可移植的默认值来源**与持久化：
//!
//! - 引擎路径：先在 `PATH` 中查找 `katago`，查不到再回退 `/usr/bin/katago`；
//! - 权重：扫描 [`EngineConfig::weights_dir`]（默认 `~/.local/share/katago/`）
//!   下的 `*.bin.gz`，默认取训练步数最新的一份；目录为空则权重为「未配置」，
//!   由上层提示用户设置；
//! - 搜索线程数：默认按 [`std::thread::available_parallelism()`] 推导。
//!
//! 持久化到 `~/.config/guanqi/settings.json`。该文件是**应用设置**：引擎
//! 配置 + 界面偏好（[`UiPrefs`]：叠加层开关、面板开关、窗口几何、文件
//! 目录记忆等）共用一份；读取失败 / 文件损坏时回退默认值并返回提示信息，
//! 不 panic。与**棋谱绑定**的进行态（游标、副本列表、时钟等）不持久化。
//! 引擎配置文件（`analysis.cfg`）生成见 [`ensure_analysis_cfg`]，**必须
//! 包含无默认值的必填键 `numAnalysisThreads` 与 `nnMaxBatchSize`**（缺任一
//! 引擎直接抛 IOError 退出，实测见任务实测记录）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::play::TimeSystem;

/// 窗口几何（启动时恢复用）：内容区尺寸（egui points）与窗口位置。
///
/// 位置是 egui-winit 的 `inner_rect.min`（Wayland 下为窗口内容区坐标，
/// KDE/Wayland 会给出估算值）；拿不到位置时 `None`，启动交由窗口管理器
/// 自行摆放。不区分显示器、不做越界校正——交给 `ViewportBuilder` 与
/// WM 的 clamp 处理。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct WindowGeometry {
    /// 窗口内容区宽（points）。
    pub width: f32,
    /// 窗口内容区高（points）。
    pub height: f32,
    /// 窗口位置（x, y，points）；`None` = 上次未取得（Wayland 可能拿不到），
    /// 启动时不指定位置。
    pub position: Option<[f32; 2]>,
}

/// 界面偏好（随 `settings.json` 持久化）：叠加层与面板开关、候选显示
/// 门控、目数视角、快扫设置、窗口几何、文件目录记忆。
///
/// 只存「用户改起来麻烦、重开又希望还在」的偏好；与**棋谱绑定**的状态
/// （游标、副本列表、时钟、分析数据）一律不进设置文件。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    /// ---- 叠加层各层开关（`overlay::Overlay` 的可持久化子集）----
    /// 候选点圆圈层。
    pub show_candidates: bool,
    /// 局势热度图层。
    pub show_heat: bool,
    /// 策略热度图层。
    pub show_policy: bool,
    /// 「候选点领地」层（opt-in includeMovesOwnership）。
    pub show_moves_heat: bool,
    /// 失误标注层。
    pub show_mistakes: bool,
    /// 胜率曲线面板叠加**目差线**开关。
    pub show_score_lead: bool,
    /// 小棋盘 PV 回放面板开关。
    pub show_mini_board: bool,
    /// ---- 面板开关 ----
    /// 胜率曲线底部面板。
    pub curve_open: bool,
    /// 棋谱树底部面板。
    pub tree_open: bool,
    /// ---- 候选类显示门控（立即 / 延迟 N 秒 / 手动）----
    /// `immediate` / `delayed` / `manual`；未识别值由加载层回退
    /// `immediate` 并提示（见 `load_settings` 的逐字段容错）。
    pub candidate_gating: String,
    /// 延迟模式的秒数（仅 `candidate_gating == "delayed"` 时生效）。
    pub gating_delay_secs: u32,
    /// ---- 目数视角 ----
    /// `black`（永远黑视角）/ `alternating`（黑白交替）。
    pub display_view: String,
    /// ---- 整谱快扫设置 ----
    /// 主扫描每手 visits。
    pub batch_visits: u32,
    /// 只扫一方：`all` / `black` / `white`。
    pub batch_side: String,
    /// 含变着（全树逐节点）。
    pub batch_variations: bool,
    /// 扫完自动加深差异手。
    pub batch_deepen: bool,
    /// 加深取前 N 手。
    pub batch_deepen_top: usize,
    /// 加深每手 visits。
    pub batch_deepen_visits: u32,
    /// ---- 窗口几何 ----
    /// 上次退出的窗口尺寸与位置（`None` = 从未记录，用程序默认）。
    pub window_geometry: Option<WindowGeometry>,
    /// ---- 文件目录记忆 ----
    /// 上次「打开棋谱」成功选中的目录（portal `current_folder`）。
    pub last_open_dir: Option<PathBuf>,
    /// 上次「另存」成功写入的目录（另存对话框的 `current_folder`）。
    pub last_save_dir: Option<PathBuf>,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            // 叠加层 / 面板默认值与 `GuanqiApp::new` 的既有初值一致：
            // 持久化是「记住改动」，不是改默认观感。
            show_candidates: true,
            show_heat: true,
            show_policy: false,
            show_moves_heat: false,
            show_mistakes: true,
            show_score_lead: true,
            show_mini_board: false,
            curve_open: true,
            tree_open: false,
            candidate_gating: "immediate".to_owned(),
            gating_delay_secs: 3,
            display_view: "black".to_owned(),
            batch_visits: 40,
            batch_side: "all".to_owned(),
            batch_variations: false,
            batch_deepen: true,
            batch_deepen_top: 10,
            batch_deepen_visits: 300,
            window_geometry: None,
            last_open_dir: None,
            last_save_dir: None,
        }
    }
}

impl UiPrefs {
    /// 门控字符串 → 枚举；未识别值回退立即显示（`None`）。
    /// 加载层据此对坏值出提示。
    pub fn gating(&self) -> Option<crate::ui::analysis::CandidateGating> {
        match self.candidate_gating.as_str() {
            "immediate" => Some(crate::ui::analysis::CandidateGating::Immediate),
            "delayed" => Some(crate::ui::analysis::CandidateGating::Delayed {
                secs: self.gating_delay_secs.max(1),
            }),
            "manual" => Some(crate::ui::analysis::CandidateGating::Manual),
            _ => None,
        }
    }

    /// 目数视角字符串 → 枚举；未识别值回退黑视角（`None`）。
    pub fn display_view(&self) -> Option<crate::ui::analysis::DisplayView> {
        match self.display_view.as_str() {
            "black" => Some(crate::ui::analysis::DisplayView::Black),
            "alternating" => Some(crate::ui::analysis::DisplayView::Alternating),
            _ => None,
        }
    }

    /// 快扫「只扫一方」字符串 → 枚举；未识别值回退全部（`None`）。
    pub fn batch_side(&self) -> Option<crate::ui::analysis::BatchSide> {
        match self.batch_side.as_str() {
            "all" => Some(crate::ui::analysis::BatchSide::All),
            "black" => Some(crate::ui::analysis::BatchSide::BlackOnly),
            "white" => Some(crate::ui::analysis::BatchSide::WhiteOnly),
            _ => None,
        }
    }
}

/// 人机对弈难度档位：预设**引擎走子**使用的访问量（visits）。
///
/// 只影响引擎应手的思考量，不影响展示分析（展示口径仍是「快查询 →
/// 配置的 visits」）。档位幅度依据（b18 权重、本机 OpenCL 实测约
/// 60 visits/s，见任务实测记录）：visits 有限时引擎更容易选中次优着法，
/// 9 路战斗局面实测 30/100 → 首选 C2、300 → B7、800/2000 → D2，
/// 各档首选着法有实际区分。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Difficulty {
    /// 入门：30 visits，很弱，会明显失误。
    Beginner,
    /// 简单：100 visits，弱。
    Easy,
    /// 中等：300 visits（缺省档）。
    #[default]
    Medium,
    /// 较强：800 visits，每手需等十几秒。
    Strong,
    /// 最强：2000 visits，每手需等半分钟以上。
    Max,
}

/// 等待时间估算用的搜索速率：b18 权重 OpenCL 后端本机实测约 60 visits/s。
/// 仅用于界面提示（「预计每手约 N 秒」），非保证值。
const ESTIMATED_VISITS_PER_SEC: f64 = 60.0;

impl Difficulty {
    /// 全部档位（界面按此顺序罗列）。
    pub const ALL: [Difficulty; 5] =
        [Self::Beginner, Self::Easy, Self::Medium, Self::Strong, Self::Max];

    /// 该档引擎走子使用的 visits 上限。
    pub fn visits(self) -> u32 {
        match self {
            Self::Beginner => 30,
            Self::Easy => 100,
            Self::Medium => 300,
            Self::Strong => 800,
            Self::Max => 2000,
        }
    }

    /// 档位中文名（界面直接显示）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Beginner => "入门",
            Self::Easy => "简单",
            Self::Medium => "中等",
            Self::Strong => "较强",
            Self::Max => "最强",
        }
    }

    /// 按实测速率估算的每手等待秒数（向上取整，提示用）。
    pub fn estimate_secs(self) -> u32 {
        (f64::from(self.visits()) / ESTIMATED_VISITS_PER_SEC).ceil() as u32
    }
}

/// 引擎与应用设置（持久化到 `settings.json`）。
///
/// 文档更正：本结构早已不只是「引擎配置」——`play_difficulty` /
/// `time_system` / `new_game_rules` 与 [`ui_prefs`] 都是应用级设置，
/// 恰好共用这一份文件。字段级容错见 [`load_settings`]。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// KataGo 可执行文件路径。
    pub engine_path: PathBuf,
    /// 权重文件路径；`None` 表示尚未配置（如权重目录为空），
    /// 上层应提示用户选择，此时无法启动引擎。
    pub model_path: Option<PathBuf>,
    /// 权重扫描目录（供界面列出候选项）。
    pub weights_dir: PathBuf,
    /// 默认思考量（visits）。查询级 `maxVisits` 可再覆盖。
    pub visits: u32,
    /// 引擎搜索线程数，写入生成的 `analysis.cfg`。
    pub search_threads: u32,
    /// 自用引擎配置文件（`-config` 参数）路径；`None` 用默认生成路径。
    pub analysis_cfg: Option<PathBuf>,
    /// 人机对弈难度（引擎走子的 visits 档位）。旧配置文件缺此键时
    /// 回退中等档（struct 级 `#[serde(default)]`）。
    pub play_difficulty: Difficulty,
    /// 分析规则：`None` = 自动跟随棋谱 `RU[]`（默认）；`Some(规范名)` =
    /// 用户在设置面板显式指定（优先级最高）。规范名即 KataGo 规则串
    /// （`chinese` / `japanese` / …），由 [`resolve_rules`] 产出。
    pub rules: Option<String>,
    /// 人机对弈的时限制式（新对局窗口编辑；「上次设置持久化，下次新
    /// 对局默认带出」的载体）。旧配置缺此键时回退缺省制式
    /// （struct 级 `#[serde(default)]`）。
    pub time_system: TimeSystem,
    /// 新对局的规则（新对局窗口六选一，总是具体值；serde 默认中国）。
    /// 与设置面板的 [`EngineConfig::rules`]（`None` = 自动跟随棋谱）
    /// **语义独立**：本字段只决定「新开的对局是什么规则」并随另存写进
    /// `RU[]`；复盘 / 载谱的规则口径仍由设置面板字段管（自动跟随 /
    /// 显式指定），互不覆盖。旧配置缺此键回退中国规则。
    pub new_game_rules: Rules,
    /// 界面偏好（叠加层 / 面板开关、门控、视角、快扫设置、窗口几何、
    /// 文件目录记忆）。旧配置缺此键时整体回退默认值
    /// （struct 级 `#[serde(default)]`）；见 [`UiPrefs`]。
    pub ui_prefs: UiPrefs,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            engine_path: find_katago_in_path().unwrap_or_else(fallback_engine_path),
            model_path: pick_newest_weights(&default_weights_dir()),
            weights_dir: default_weights_dir(),
            // 默认展示思考量 300（作者确认下调；仅影响未配置过的新用户，
            // 已有 settings.json 里显式的 visits 值不受影响）。
            visits: 300,
            search_threads: default_search_threads(),
            analysis_cfg: None,
            play_difficulty: Difficulty::default(),
            rules: None,
            time_system: TimeSystem::default(),
            new_game_rules: Rules::Chinese,
            ui_prefs: UiPrefs::default(),
        }
    }
}

// ---- 规则串解析（SGF 自由文本 → KataGo 规范名） ----

/// 规则解析的完整结果：发给引擎的规范名 + 未识别时的提示。
#[derive(Clone, Debug, PartialEq)]
pub struct RulesResolution {
    /// 发给引擎的 KataGo 规则串（规范名，绝不是 SGF 原始串）。
    pub rules: String,
    /// 「未识别的规则串」提示（`Some` 时消息区如实展示已按何规则分析）。
    /// 显式设置 / 成功映射 / 无规则串时为 `None`。
    pub notice: Option<String>,
}

/// 解析本次分析应使用的规则（优先级：设置显式指定 > 棋谱 `RU[]` 宽容
/// 映射 > 默认 `chinese`）。
///
/// **绝不把 SGF 原始字符串发给引擎**：`RU[]` 是自由文本（实测名局中
/// 存在 `"Japanese (1989)"`、`"中国规则"`、`"GOE"` 等写法），直接透传会
/// 被引擎拒绝（查询级错误，见 `EngineError::QueryRejected`）。本函数
/// 只产出 [`Rules::ALL`] 里的规范名。
pub fn resolve_rules(cfg_rule: Option<&str>, sgf_rule: Option<&str>) -> RulesResolution {
    if let Some(rule) = cfg_rule
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(canonical_rules)
    {
        return RulesResolution { rules: rule.to_owned(), notice: None };
    }
    if let Some(raw) = sgf_rule.map(str::trim).filter(|v| !v.is_empty()) {
        return match lenient_rules(raw) {
            Some(rule) => RulesResolution { rules: rule.to_owned(), notice: None },
            None => RulesResolution {
                rules: Rules::DEFAULT.to_owned(),
                notice: Some(format!(
                    "未识别的规则串「{raw}」，已按 中国规则 分析。"
                )),
            },
        };
    }
    RulesResolution { rules: Rules::DEFAULT.to_owned(), notice: None }
}

/// 规则的下拉选项与宽容映射的目标（KataGo 规则串规范名）。
///
/// 项目只用这一份列表：设置面板的下拉与 `lenient_rules` 的关键词映射
/// 都以 [`Rules::ALL`] 为界，映射结果绝不逃出引擎可接受的集合。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rules {
    /// 中国规则（数子）。
    Chinese,
    /// 日本规则（数目）。
    Japanese,
    /// 韩国规则（数目，与日本规则同串族）。
    Korean,
    /// 美国规则（AGA）。
    Aga,
    /// 新西兰规则。
    NewZealand,
    /// Tromp-Taylor 规则。
    TrompTaylor,
}

impl Rules {
    /// 全部可选规则（设置下拉按此顺序罗列）。
    pub const ALL: [Rules; 6] = [
        Self::Chinese,
        Self::Japanese,
        Self::Korean,
        Self::Aga,
        Self::NewZealand,
        Self::TrompTaylor,
    ];

    /// 未配置且棋谱无 `RU` 时的默认规则。
    pub const DEFAULT: &'static str = "chinese";

    /// KataGo 规则串（查询 `rules` 字段用）。
    pub fn wire(self) -> &'static str {
        match self {
            Self::Chinese => "chinese",
            Self::Japanese => "japanese",
            Self::Korean => "korean",
            Self::Aga => "aga",
            Self::NewZealand => "new-zealand",
            Self::TrompTaylor => "tromp-taylor",
        }
    }

    /// 中文名（界面显示）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Chinese => "中国",
            Self::Japanese => "日本",
            Self::Korean => "韩国",
            Self::Aga => "AGA",
            Self::NewZealand => "新西兰",
            Self::TrompTaylor => "Tromp-Taylor",
        }
    }

    /// 由 KataGo 规范名反查中文名（映射不出返回规范名原文——防御
    /// 手改 settings.json 塞进未知串时的显示兜底）。
    pub fn rules_name(wire: &str) -> String {
        Self::from_wire(wire).map_or_else(|| wire.to_owned(), |r| r.name().to_owned())
    }

    /// 由 KataGo 规范名反查（宽容大小写 / 连字符-空格），设置面板持久化
    /// 值的回收路径；映射不出返回 `None`（例如用户手改了 settings.json）。
    pub fn from_wire(s: &str) -> Option<Self> {
        let norm = s.trim().to_ascii_lowercase().replace(['-', '_', ' '], "");
        Self::ALL.into_iter().find(|r| r.wire().replace('-', "") == norm)
    }
}

/// 规范名回收：设置里的显式值必须也是规范名（防用户手改 settings.json
/// 塞进任意串）。只认 [`Rules::ALL`] 的规范名。
fn canonical_rules(s: &str) -> Option<&'static str> {
    Rules::from_wire(s).map(Rules::wire)
}

/// SGF `RU[]` 自由文本的宽容映射：大小写无关 + 关键词（中英文）。
///
/// 关键词表覆盖实测与常见写法：`"Japanese (1989)"`（japan）、
/// `"中国规则"`（中国）、`"GOE"`（无关键词 → 未识别）、`"jpn"`、
/// `"tt"`（Tromp-Taylor 社区缩写）等。映射不出返回 `None`，由
/// [`resolve_rules`] 落默认值并出提示。
fn lenient_rules(raw: &str) -> Option<&'static str> {
    let lower = raw.to_lowercase();
    let has = |needle: &str| lower.contains(needle);
    // 中日韩按日语/韩国、中国等关键词；英文按国名/缩写。
    if has("japan") || has("日本") || has("jpn") || has("日韩") {
        return Some(Rules::Japanese.wire());
    }
    if has("korea") || has("韩国") || has("kor") {
        return Some(Rules::Korean.wire());
    }
    if has("china") || has("中国") || has("数子") || has("chn") {
        return Some(Rules::Chinese.wire());
    }
    if has("aga") {
        return Some(Rules::Aga.wire());
    }
    if has("new zealand") || has("new-zealand") || has("新西兰") || has("newzealand") {
        return Some(Rules::NewZealand.wire());
    }
    if has("tromp") || has("tt") {
        return Some(Rules::TrompTaylor.wire());
    }
    // 全部关键词落空：有些谱直接写规范名本身（"japanese" 等），先试规范名。
    canonical_rules(&lower)
}

/// 配置目录 `~/.config/guanqi`（尊重 `XDG_CONFIG_HOME`）。
pub fn config_dir() -> PathBuf {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("guanqi"),
        _ => home_dir().join(".config/guanqi"),
    }
}

/// `settings.json` 路径。
pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

/// 默认引擎配置文件路径（`analysis.cfg`）。
pub fn default_analysis_cfg_path() -> PathBuf {
    config_dir().join("analysis.cfg")
}

/// 本桥接层实际使用的引擎配置文件路径。
pub fn effective_analysis_cfg(cfg: &EngineConfig) -> PathBuf {
    cfg.analysis_cfg
        .clone()
        .unwrap_or_else(default_analysis_cfg_path)
}

/// 默认权重目录（外部依赖的常规存放位置，本身可配置）。
pub fn default_weights_dir() -> PathBuf {
    home_dir().join(".local/share/katago")
}

/// `PATH` 查找 `katago` 的兜底路径。
fn fallback_engine_path() -> PathBuf {
    PathBuf::from("/usr/bin/katago")
}

/// 默认搜索线程数：按机器可用并行度推导。
fn default_search_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 语义同 `which katago`：遍历 `PATH` 各目录，返回第一个可执行命中。
pub fn find_katago_in_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("katago"))
        .find(|cand| is_executable_file(cand))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// 扫描权重目录下的 `*.bin.gz`，按训练步数从新到旧排序（供界面列表与默认选择）。
/// 文件名无法解析步数时排在最后。目录不存在时返回空表。
pub fn scan_weights(dir: &Path) -> Vec<PathBuf> {
    let mut weights: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "gz"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    weights.sort_by(|a, b| {
        network_steps(b)
            .cmp(&network_steps(a))
            .then_with(|| b.file_name().cmp(&a.file_name()))
    });
    weights
}

/// 目录中最新的权重文件；目录为空 / 不存在时返回 `None`（未配置状态）。
pub fn pick_newest_weights(dir: &Path) -> Option<PathBuf> {
    scan_weights(dir).into_iter().next()
}

/// 从 KataGo 权重文件名提取训练步数（如 `...-s9996604416-d....bin.gz`）。
/// 步数超过 10 亿时惯例用 `M` / `K` 后缀缩写（如 `s2941M`）。
fn network_steps(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let start = name.find("-s")? + "-s".len();
    let rest = &name[start..];
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, suffix) = rest.split_at(digits_end);
    let base: u64 = digits.parse().ok()?;
    Some(match suffix.chars().next() {
        Some('G') => base.saturating_mul(1_000_000_000),
        Some('M') => base.saturating_mul(1_000_000),
        Some('K') => base.saturating_mul(1_000),
        _ => base,
    })
}

/// 读取配置的返回：配置 + 读取异常时的用户提示（正常为 `None`）。
pub type LoadedSettings = (EngineConfig, Option<String>);

/// 从 `settings.json` 读取配置。文件不存在视为首次运行（静默用默认值）。
///
/// **字段级容错**：任一字段值非法（如 `"play_difficulty": "Expert"`、
/// `visits` 写成字符串）只回退**该字段**默认值，其余字段（含引擎路径 /
/// 权重）原样保留，并针对每个坏字段给出「哪个字段、原值是什么、已按
/// 什么处理」的提示——整份 JSON 结构损坏（无法解析为对象）才整体回退
/// 默认配置，不 panic。实现：先按 `serde_json::Value` 松散解析，再用
/// `Value → EngineConfig` 的镜像反序列化逐字段收割错误。
pub fn load_settings() -> LoadedSettings {
    let path = settings_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (EngineConfig::default(), None)
        }
        Err(e) => {
            return (
                EngineConfig::default(),
                Some(format!(
                    "读取 {} 失败（{e}），已使用默认引擎配置。",
                    path.display()
                )),
            )
        }
    };
    // 整体无法解析（截断 / 非 JSON）：文件级损坏，整份回退（与旧行为一致）。
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => {
            return (
                EngineConfig::default(),
                Some(format!(
                    "{} 无法解析（{e}），已回退默认引擎配置。",
                    path.display()
                )),
            )
        }
    };
    let mut notices: Vec<String> = Vec::new();
    let cfg = config_from_value(&value, &mut notices);
    let notice = (!notices.is_empty()).then(|| {
        format!(
            "{} 有 {} 处设置值无法识别（其余设置已保留）：{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("settings.json"),
            notices.len(),
            notices.join("；")
        )
    });
    (cfg, notice)
}

/// `serde_json::Value` → [`EngineConfig`] 的逐字段收割：每个字段先试从
/// 原值反序列化，失败即取默认并记一条提示。这是 [`EngineConfig`]
/// 序列化形态的镜像（新增字段时同步加一行），不引入第二个配置文件。
fn config_from_value(value: &serde_json::Value, notices: &mut Vec<String>) -> EngineConfig {
    let default = EngineConfig::default();
    // 顶层不是对象（数组 / 字符串 / 数字等）：无字段可收割，整份默认。
    let Some(map) = value.as_object() else {
        notices.push("文件内容不是设置对象".to_owned());
        return default;
    };
    // 路径类字段（engine_path / weights_dir / model_path 等）在下方内联
    // 处理：提示文案各自点名回退目标，不走通用闭包。

    /// 单个标量字段的收割：反序列化失败即回退 `fallback`（默认值串）并
    /// 记一条提示。提示里如实点名字段、原值与回退结果（子项 4 的要求）。
    /// `fallback` 为该类型 serde 形式的默认值串（如 `"Medium"`）。
    fn field<T: serde::de::DeserializeOwned + serde::Serialize>(
        notices: &mut Vec<String>,
        key: &str,
        raw: &serde_json::Value,
        desc: &str,
        fallback: &str,
    ) -> T {
        let fallback: T = serde_json::from_str(fallback).expect("默认值串与本类型恒匹配");
        match serde_json::from_value(raw.clone()) {
            Ok(v) => v,
            Err(_) => {
                notices.push(format!(
                    "「{key}」的值 {raw} 无法识别，已按{desc}「{}」处理",
                    serde_json::to_string(&fallback).unwrap_or_default()
                ));
                fallback
            }
        }
    }

    // ---- 引擎核心字段（坏值回退默认，路径与权重必须活下来）----
    let engine_path = match map.get("engine_path") {
        Some(raw) => match serde_json::from_value::<PathBuf>(raw.clone()) {
            Ok(p) => p,
            Err(_) => {
                notices.push(format!(
                    "「engine_path」的值 {raw} 无法识别，已回退默认引擎路径"
                ));
                default.engine_path.clone()
            }
        },
        None => default.engine_path.clone(),
    };
    let model_path = match map.get("model_path") {
        Some(raw) if !raw.is_null() => {
            match serde_json::from_value::<Option<PathBuf>>(raw.clone()) {
                // 显式 null = 未配置（合法，语义见 EngineConfig::model_path）。
                Ok(p) => p,
                Err(_) => {
                    notices.push(format!(
                        "「model_path」的值 {raw} 无法识别，已按未配置处理"
                    ));
                    None
                }
            }
        }
        _ => default.model_path.clone(),
    };
    let weights_dir = match map.get("weights_dir") {
        Some(raw) => match serde_json::from_value::<PathBuf>(raw.clone()) {
            Ok(p) => p,
            Err(_) => {
                notices.push(format!(
                    "「weights_dir」的值 {raw} 无法识别，已回退默认权重目录"
                ));
                default.weights_dir.clone()
            }
        },
        None => default.weights_dir.clone(),
    };
    // `backend` 是退休项（2026-09 删除）：它描述引擎二进制的构建
    // （OpenCL / Eigen），由用户安装的 katago 决定，本程序无从也无需
    // 运行时切换，更从未传给引擎。旧 settings.json 里遗留的该键**静默
    // 丢弃**——它不是错误，所以不产生提示（提示只用于「值无法识别、
    // 已按某某处理」这类会改变用户观感的场合）。
    // 本函数逐字段收割已知键，未列出的键（含退休的 `backend`）自然被忽略。
    let visits: u32 = match map.get("visits") {
        Some(raw) => field(notices, "visits", raw, "默认", "300"),
        None => default.visits,
    };
    let search_threads: u32 = match map.get("search_threads") {
        Some(raw) => field(notices, "search_threads", raw, "默认", "4"),
        None => default.search_threads,
    };
    let analysis_cfg = match map.get("analysis_cfg") {
        Some(raw) if !raw.is_null() => {
            serde_json::from_value::<Option<PathBuf>>(raw.clone()).unwrap_or_else(|_| {
                notices.push(format!(
                    "「analysis_cfg」的值 {raw} 无法识别，已按未指定处理"
                ));
                None
            })
        }
        _ => default.analysis_cfg,
    };
    let play_difficulty: Difficulty = match map.get("play_difficulty") {
        Some(raw) => field(notices, "play_difficulty", raw, "默认", r#""Medium""#),
        None => default.play_difficulty,
    };
    let rules: Option<String> = match map.get("rules") {
        Some(raw) if !raw.is_null() => {
            serde_json::from_value::<Option<String>>(raw.clone()).unwrap_or_else(|_| {
                notices.push(format!("「rules」的值 {raw} 无法识别，已按自动跟随棋谱处理"));
                None
            })
        }
        _ => default.rules,
    };
    let time_system: TimeSystem = match map.get("time_system") {
        Some(raw) => field(notices, "time_system", raw, "无限制", r#""Unlimited""#),
        None => default.time_system,
    };
    let new_game_rules: Rules = match map.get("new_game_rules") {
        Some(raw) => field(notices, "new_game_rules", raw, "默认", r#""chinese""#),
        None => default.new_game_rules,
    };

    // ---- 界面偏好（UiPrefs）：整体坏值提示 + 内部字段兜底 ----
    let ui_prefs = match map.get("ui_prefs") {
        Some(raw) => match serde_json::from_value::<UiPrefs>(raw.clone()) {
            Ok(mut prefs) => {
                // 语义校验：字符串型枚举值必须可解析（serde 拦不住未知串）。
                if prefs.gating().is_none() {
                    notices.push(format!(
                        "「ui_prefs.candidate_gating」的值 {:?} 无法识别，已按默认「immediate」处理",
                        prefs.candidate_gating
                    ));
                    prefs.candidate_gating = "immediate".to_owned();
                }
                if prefs.display_view().is_none() {
                    notices.push(format!(
                        "「ui_prefs.display_view」的值 {:?} 无法识别，已按默认「black」处理",
                        prefs.display_view
                    ));
                    prefs.display_view = "black".to_owned();
                }
                if prefs.batch_side().is_none() {
                    notices.push(format!(
                        "「ui_prefs.batch_side」的值 {:?} 无法识别，已按默认「all」处理",
                        prefs.batch_side
                    ));
                    prefs.batch_side = "all".to_owned();
                }
                prefs
            }
            Err(_) => {
                // 结构坏了（非对象 / 字段类型不符）：回退整体默认并提示。
                // serde 的 struct 级 default 只兜缺字段，兜不了类型错值，
                // 所以这里只能整块回退——ui_prefs 里没有会丢引擎的键，
                // 损失可控，提示讲清楚即可。
                notices.push("「ui_prefs」内容无法识别，界面偏好已回退默认值".to_owned());
                UiPrefs::default()
            }
        },
        None => UiPrefs::default(), // 旧配置文件缺此键：静默默认（兼容）
    };

    EngineConfig {
        engine_path,
        model_path,
        weights_dir,
        visits: visits.max(1),
        search_threads: search_threads.max(1),
        analysis_cfg,
        play_difficulty,
        rules,
        time_system,
        new_game_rules,
        ui_prefs,
    }
}

/// 保存配置到 `settings.json`（目录不存在则创建）。
pub fn save_settings(cfg: &EngineConfig) -> Result<(), String> {
    let path = settings_path();
    write_atomically(
        &path,
        &serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("保存 {} 失败：{e}", path.display()))
}

/// 生成配置的注释已按实测改写；已存在的旧文件不会被重写，于是出现
/// 「值是 16、注释还说 8 核 16 线程物理核最优」的自相矛盾。对策：文件
/// 中**精确**存在下面两行旧注释原文时，替换为新注释；不匹配（用户改过
/// / 删过）一律保持原样，其余内容不动，多次执行幂等。
const CFG_LEGACY_COMMENT: [&str; 2] = [
    "# 8 cores / 16 threads host: one search thread per physical core is the",
    "# efficiency sweet spot for KataGo MCTS.",
];
/// 替换旧注释用的**新注释**（与 `default_analysis_cfg_text` 的实测口径
/// 一致：16 线程比 8 快 17–21%，界面未被饿死，发滞可降物理核数）。
const CFG_NEW_COMMENT: &str = "\
    # 搜索线程：OpenCL 只做网络前向、搜索全在 CPU 线程上，线程越多越能\n\
    # 喂满 GPU 批。实测（b18 + 680M iGPU）：16 线程比 8 快 17–21%\n\
    # （2000 visits：23.8s vs 28.6s），界面未被饿死；若发滞可降到物理核数。";

/// `analysis.cfg` 不存在时按当前配置生成一份可用的默认配置；
/// 已存在时做两类**受限**更新，其余内容一律不动（用户可自由修改该文件）：
/// 1. 仅同步 `numSearchThreads`（管线接管的键）与当前配置不一致时的值；
/// 2. 仅当文件里存在 [`CFG_LEGACY_COMMENT`] 两行**精确原文**时，替换为
///    [`CFG_NEW_COMMENT`]（旧注释与新实测口径矛盾，见其文档）。
///
/// 返回是否新写入 / 更新了文件（两次运行间内容不变则 `false`，幂等）。
pub fn ensure_analysis_cfg(path: &Path, cfg: &EngineConfig) -> Result<bool, String> {
    let threads = cfg.search_threads.max(1);
    if !path.exists() {
        write_atomically(path, &default_analysis_cfg_text(cfg))?;
        return Ok(true);
    }
    // 已存在：仅同步 numSearchThreads 与旧注释升级（若精确匹配），其余行原样保留。
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut changed = false;
    let mut seen = false;
    for line in &mut lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("numSearchThreads") && trimmed.contains('=') {
            seen = true;
            if trimmed != format!("numSearchThreads = {threads}") {
                *line = format!("numSearchThreads = {threads}");
                changed = true;
            }
        }
    }
    if !seen {
        // 配置被用户删掉了该键：KataGo 缺省 numSearchThreads = 1，
        // 把当前值追加到必填键区块，保证设置改动有处落。
        lines.push(format!("numSearchThreads = {threads}"));
        changed = true;
    }
    // 旧注释升级（子项 5）：逐行扫描相邻两行是否**精确**等于旧注释原文。
    // 只替换这两行；找不到 / 改过就保持原样，绝不整段重排。
    if let Some(start) = lines
        .windows(2)
        .position(|w| w[0] == CFG_LEGACY_COMMENT[0] && w[1] == CFG_LEGACY_COMMENT[1])
    {
        // 新注释为 3 行，替换 2 行旧文；其余行下标不变（先替换，再在
        // 同一个 Vec 上继续，无需二次扫描）。
        let mut new_lines: Vec<String> = Vec::with_capacity(lines.len() + 1);
        new_lines.extend_from_slice(&lines[..start]);
        new_lines.extend(CFG_NEW_COMMENT.lines().map(str::to_owned));
        new_lines.extend_from_slice(&lines[start + 2..]);
        lines = new_lines;
        changed = true;
    }
    if changed {
        let mut out = lines.join("\n");
        out.push('\n');
        write_atomically(path, &out)?;
    }
    Ok(changed)
}

fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("落盘 {} 失败：{e}", path.display()))
}

/// 生成的默认引擎配置正文。键值取舍以实测为准：
/// - `numAnalysisThreads` / `nnMaxBatchSize`：无默认值的必填键；
/// - `numSearchThreads`：搜索线程数（来自 [`EngineConfig::search_threads`]）。
///   **实测口径**（b18 + 680M iGPU / OpenCL，本机）：16 线程比 8 线程快
///   17–21%（2000 visits 走子查询 23.8s vs 28.6s），未测出界面被饿死
///   （KataGo 的搜索线程与 UI 无关，界面在独立进程里）。若个别机器上
///   出现卡顿可降到物理核数。OpenCL 只做网络前向、搜索全在 CPU 线程上：
///   线程越多越能喂满 GPU 批，这也是「搜索线程 ≈ 物理核数」比
///   「每个物理核一线程更省」传说更快的原因；
/// - `reportAnalysisWinratesAs = BLACK`：胜率固定黑方视角；查询级
///   `overrideSettings` 会逐查询覆盖它（本项目每条查询都显式发送该
///   字段，cfg 值只是兜底），前端不翻转；
/// - `logToStderr/stdout = false`：stdout 保留给 JSON 行协议；
/// - `reportDuringSearchEvery` 等 GTP `kata-analyze` 专用键不写入
///   （实测对 analysis 引擎无效）；
/// - 线程 / 批量等参数来自 [`EngineConfig`]，用户可直接编辑本文件覆盖
///   （`numSearchThreads` 除外——设置保存时会把它同步回当前配置值）。
fn default_analysis_cfg_text(cfg: &EngineConfig) -> String {
    let log_dir = home_dir().join(".local/state/guanqi/katago");
    let threads = cfg.search_threads.max(1);
    // 批量大小与搜索线程数同量级即可充分利用推理；限制在合理区间。
    let batch = threads.clamp(8, 64).max(16);
    format!(
        "# 观棋 (Guanqi) 生成的 KataGo analysis 引擎配置。\
         \n# 可自由修改；本文件在缺失时生成，设置保存时会按程序内配置\
         \n# 同步 numSearchThreads 一项，其余内容不会被覆盖。\
         \n\
         \n# ---- 搜索预算（查询级 maxVisits 可覆盖此处的 maxVisits）----\
         \n# 搜索线程：OpenCL 只做网络前向、搜索全在 CPU 线程上，线程越多\
         \n# 越能喂满 GPU 批。实测（b18 + 680M iGPU）：16 线程比 8 快 17–21%\
         \n# （2000 visits：23.8s vs 28.6s），界面未被饿死；若发滞可降到物理核数。\
         \nmaxVisits = {visits}\
         \nnumSearchThreads = {threads}\
         \n\
         \n# ---- 必填键（无默认值，缺失会直接导致引擎退出）----\
         \nnumAnalysisThreads = 1\
         \nnnMaxBatchSize = {batch}\
         \n\
         \n# ---- 输出口径 ----\
         \nreportAnalysisWinratesAs = BLACK\
         \nanalysisPVLen = 15\
         \nlogDir = {log_dir}\
         \nlogToStderr = false\
         \nlogToStdout = false\
         \n",
        visits = cfg.visits,
        threads = threads,
        batch = batch,
        log_dir = log_dir.display()
    )
}
