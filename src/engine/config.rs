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
        }
    }
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
/// 已存在则**不覆盖**（用户可自由修改）。返回是否新写入了文件。
pub fn ensure_analysis_cfg(path: &Path, cfg: &EngineConfig) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    write_atomically(path, &default_analysis_cfg_text(cfg))?;
    Ok(true)
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
/// - `reportAnalysisWinratesAs = BLACK`：胜率固定黑方视角，前端不翻转；
/// - `logToStderr/stdout = false`：stdout 保留给 JSON 行协议；
/// - `reportDuringSearchEvery` 等 GTP `kata-analyze` 专用键不写入
///   （实测对 analysis 引擎无效）；
/// - 线程 / 批量等参数来自 [`EngineConfig`]，用户可直接编辑本文件覆盖。
fn default_analysis_cfg_text(cfg: &EngineConfig) -> String {
    let log_dir = home_dir().join(".local/state/guanqi/katago");
    let threads = cfg.search_threads.max(1);
    // 批量大小与搜索线程数同量级即可充分利用推理；限制在合理区间。
    let batch = threads.clamp(8, 64).max(16);
    format!(
        "# 观棋 (Guanqi) 生成的 KataGo analysis 引擎配置。\
         \n# 可自由修改；本文件只在缺失时生成，程序不会覆盖已有内容。\
         \n\
         \n# ---- 搜索预算（查询级 maxVisits 可覆盖此处的 maxVisits）----\
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
