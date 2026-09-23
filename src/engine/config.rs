//! 引擎配置数据层与持久化（阶段 3.3 的非 UI 部分）。
//!
//! KataGo 是**外部依赖**而非项目自身的一部分：用户用什么引擎路径、哪个权重、
//! 哪个后端、多少思考量，全部是可配置项。本模块只负责给出**可移植的默认值
//! 来源**与持久化：
//!
//! - 引擎路径：先在 `PATH` 中查找 `katago`，查不到再回退 `/usr/bin/katago`；
//! - 权重：扫描 [`EngineConfig::weights_dir`]（默认 `~/.local/share/katago/`）
//!   下的 `*.bin.gz`，默认取训练步数最新的一份；目录为空则权重为「未配置」，
//!   由上层提示用户设置；
//! - 搜索线程数：默认按 [`std::thread::available_parallelism()`] 推导。
//!
//! 持久化到 `~/.config/guanqi/settings.json`；读取失败 / 文件损坏时回退默认值
//! 并返回提示信息，不 panic。引擎配置文件（`analysis.cfg`）生成见
//! [`ensure_analysis_cfg`]，**必须包含无默认值的必填键 `numAnalysisThreads`
//! 与 `nnMaxBatchSize`**（缺任一引擎直接抛 IOError 退出，实测见任务实测记录）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::play::TimeSystem;

/// 推理后端。这是对**引擎二进制**的描述（OpenCL 版 / Eigen 版），
/// 不影响命令行参数；供界面展示与降级提示用。可配置项。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum EngineBackend {
    /// GPU（OpenCL）。
    #[default]
    OpenCL,
    /// CPU（Eigen），降级方案。
    Eigen,
}

impl EngineBackend {
    /// 用户可读名称（界面直接显示）。
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenCL => "OpenCL（GPU）",
            Self::Eigen => "Eigen（CPU）",
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

/// 引擎配置（持久化到 `settings.json`）。
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
    /// 推理后端（描述性字段，见 [`EngineBackend`]）。
    pub backend: EngineBackend,
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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            engine_path: find_katago_in_path().unwrap_or_else(fallback_engine_path),
            model_path: pick_newest_weights(&default_weights_dir()),
            weights_dir: default_weights_dir(),
            backend: EngineBackend::OpenCL,
            visits: 500,
            search_threads: default_search_threads(),
            analysis_cfg: None,
            play_difficulty: Difficulty::default(),
            rules: None,
            time_system: TimeSystem::default(),
            new_game_rules: Rules::Chinese,
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

/// 从 `settings.json` 读取配置。文件不存在视为首次运行（静默用默认值）；
/// 读取失败 / 解析损坏时回退默认值并返回提示，不 panic。
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
    match serde_json::from_str::<EngineConfig>(&text) {
        Ok(cfg) => (cfg, None),
        Err(e) => (
            EngineConfig::default(),
            Some(format!(
                "{} 无法解析（{e}），已回退默认引擎配置。",
                path.display()
            )),
        ),
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

/// `analysis.cfg` 不存在时按当前配置生成一份可用的默认配置；
/// 已存在时**仅当其中的 `numSearchThreads` 与当前配置不一致**才原地
/// 更新线程数（其余内容不动——用户可自由修改该文件，程序只接管线
/// 需要的那一个键）。返回是否新写入 / 更新了文件。
///
/// 背景：`search_threads` 此前只用于首次生成配置——文件已存在时改了
/// 设置等于没改。保存设置时调用本函数即可让新线程数真的生效
///（引擎重启后读新配置）。
pub fn ensure_analysis_cfg(path: &Path, cfg: &EngineConfig) -> Result<bool, String> {
    let threads = cfg.search_threads.max(1);
    if !path.exists() {
        write_atomically(path, &default_analysis_cfg_text(cfg))?;
        return Ok(true);
    }
    // 已存在：仅同步 numSearchThreads（管线接管的键），其余行原样保留。
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
    if seen && !changed {
        return Ok(false);
    }
    if !seen {
        // 配置被用户删掉了该键：KataGo 缺省 numSearchThreads = 1，
        // 把当前值追加到必填键区块，保证设置改动有处落。
        lines.push(format!("numSearchThreads = {threads}"));
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
