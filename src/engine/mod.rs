//! 引擎模块：驱动 `katago analysis` 子进程并解析其 JSON 行协议
//! （胜率 / 目差 / 候选点 / ownership）。
//!
//! # 对上层（UI 接线）的约定
//!
//! - [`Engine::spawn`] 立即返回，模型加载 / OpenCL 调优（实测最长约 60s）
//!   在子进程内进行；收到 [`EngineEvent::Ready`] 即可用，超时见
//!   [`EngineError::StartupTimeout`]。
//! - UI 每帧调用 [`Engine::try_recv`]（非阻塞）取事件；引擎思考期间
//!   `try_recv` 立即返回 `None`，不阻塞 UI 线程。
//! - 构造 [`Engine`] 时可传 waker（如 `egui::Context` 的 `request_repaint`
//!   包装），事件入队时唤醒界面重绘。
//! - 局面变化时先 [`Engine::terminate`] 旧查询再发新查询；被终止的查询
//!   仍会补发终态报告（实测），由上层按 id 丢弃过期结果。
//! - 引擎死亡以 [`EngineEvent::Exited`] 上报；重启 = 丢弃 [`Engine`]
//!   重新 [`Engine::spawn`]。
//!
//! 协议事实与线程模型见子模块文档；全部来自 v1.18.2 实测。

// 模块统一位于 lib 目标后，公开类型即导出 API，无需整体放宽 dead_code。
mod config;
mod process;
mod protocol;

// 面向 UI 接线任务的门面 re-export。
pub use config::{
    config_dir, default_analysis_cfg_path, default_weights_dir, effective_analysis_cfg,
    ensure_analysis_cfg, find_katago_in_path, load_settings, save_settings, scan_weights,
    settings_path, Difficulty, EngineBackend, EngineConfig, LoadedSettings,
};
pub use protocol::{AnalysisQuery, AnalysisReport, MoveInfo, MoveRule, MoveRules, QueryId, RootInfo};

use crate::board::Size;
use process::{PipeEvent, Process, Waker};
use protocol::{ControlRequest, Incoming};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// 单个查询无终态报告的等待上限。
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// 启动可用性判定上限（模型加载 + 可能的 OpenCL 调优实测可达约 60s）。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);
/// 优雅关闭的等待上限，超时后强杀。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// 引擎侧错误。
#[allow(dead_code)]
#[derive(Debug)]
pub enum EngineError {
    /// 引擎可执行文件不存在或不可执行。
    EngineNotFound(PathBuf),
    /// 创建进程失败（含权限、缺库等）。
    SpawnFailed(std::io::Error),
    /// 向引擎 stdin 写入失败（通常意味着进程已死）。
    StdinWrite(std::io::Error),
    /// 进程已退出，无法再接受查询。
    ProcessGone,
    /// 尚未配置权重文件，无法启动。
    ModelNotConfigured,
    /// 引擎配置文件生成失败。
    Config(String),
    /// 启动后在限定时间内未见就绪标记（区分「启动中」与「卡死」）。
    StartupTimeout(Duration),
    /// 查询超时无终态（引擎会收到 terminate 止损）。
    QueryTimeout { id: QueryId, elapsed: Duration },
    /// 引擎拒绝了查询（非法坐标、非法规则串等）。
    QueryRejected { id: Option<QueryId>, message: String },
    /// 协议异常（无法解析的输出行等）。
    Protocol(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EngineNotFound(p) => write!(f, "引擎可执行文件不存在：{}", p.display()),
            Self::SpawnFailed(e) => write!(f, "启动引擎进程失败：{e}"),
            Self::StdinWrite(e) => write!(f, "向引擎写入查询失败：{e}"),
            Self::ProcessGone => write!(f, "引擎进程已退出"),
            Self::ModelNotConfigured => {
                write!(f, "尚未配置权重文件，请在设置中选择一个网络权重")
            }
            Self::Config(m) => write!(f, "引擎配置问题：{m}"),
            Self::StartupTimeout(d) => {
                write!(f, "引擎在 {d:.0?} 内未就绪，可能已卡死（如 OpenCL 调优异常）")
            }
            Self::QueryTimeout { id, elapsed } => write!(f, "查询 {id} 超时（{elapsed:.0?} 无终态）"),
            Self::QueryRejected { id, message } => match id {
                Some(id) => write!(f, "查询 {id} 被拒绝：{message}"),
                None => write!(f, "请求被拒绝：{message}"),
            },
            Self::Protocol(m) => write!(f, "协议异常：{m}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// 引擎事件（UI 每帧经 [`Engine::try_recv`] 取出）。
#[allow(dead_code)]
#[derive(Debug)]
pub enum EngineEvent {
    /// 引擎就绪，可以接受查询（stderr 就绪标记，实测可靠）。
    Ready,
    /// 一份分析报告（含渐进中间报告与终态报告；被 terminate 的查询
    /// 会补发终态，上层按 id 丢弃过期项）。
    Report { id: QueryId, report: AnalysisReport, is_final: bool },
    /// 引擎对顶层未知字段的警告（「字段名可能拼错了」）。**非破坏性**：
    /// 实测引擎发完警告后照常分析该查询（全部报告正常到达），因此
    /// 该事件绝不映射为 [`EngineEvent::Failed`]，只做用户可见提示。
    Warning {
        /// 被警告的查询（警告报文自带 id；实测总是回带）。
        id: Option<QueryId>,
        /// 引擎不认识的顶层字段名（提示里点名用）。
        field: Option<String>,
        /// 引擎原文（含 `warnUnusedFields=false` 关闭提示）。
        message: String,
    },
    /// 引擎 stderr 日志行或内部说明。
    Log(String),
    /// 结构化失败（启动超时 / 查询超时 / 查询被拒等）。
    Failed(EngineError),
    /// 进程退出（崩溃、被 kill 或优雅关闭；用 `ExitStatus` 判定方式）。
    Exited(std::process::ExitStatus),
}

/// 在飞的查询信息：棋盘尺寸（坐标解码用）与超时线。
struct Pending {
    size: Size,
    deadline: Instant,
}

/// KataGo analysis 桥接器。
///
/// UI 侧典型用法：
/// ```ignore
/// let engine = Engine::spawn_with_waker(&cfg, Some(waker))?; // 不阻塞
/// let id = engine.analyze(query);            // 局面变化时先 terminate(id_old)
/// while let Some(event) = engine.try_recv() { match event { .. } } // 每帧
/// ```
#[allow(dead_code)]
pub struct Engine {
    process: Process,
    /// 引擎事件通道自身的发送端：引擎侧自查事件（写失败等）也走同一队列。
    self_tx: Sender<PipeEvent>,
    next_id: u64,
    pending: HashMap<QueryId, Pending>,
    /// 最近一次查询的尺寸（未知 id 报告的解码兜底）。
    last_size: Option<Size>,
    ready_seen: bool,
    startup_timeout_emitted: bool,
    exit_reported: bool,
}

impl Engine {
    /// 启动引擎（不等待模型加载完成）。配置 / 权重路径来自 [`EngineConfig`]；
    /// 若引擎配置文件缺失会先按当前配置生成（已存在则不覆盖）。
    pub fn spawn(cfg: &EngineConfig) -> Result<Engine, EngineError> {
        Self::spawn_with_waker(cfg, None)
    }

    /// 同 [`Engine::spawn`]，另注册一个事件唤醒回调（如
    /// `egui::Context::request_repaint` 的包装），在事件入队时于**读线程**
    /// 被调用。`egui::Context` 可克隆且 `Send + Sync`，可直接捕获。
    pub fn spawn_with_waker(cfg: &EngineConfig, waker: Option<Waker>) -> Result<Engine, EngineError> {
        let model_path = cfg
            .model_path
            .clone()
            .ok_or(EngineError::ModelNotConfigured)?;
        let cfg_path = effective_analysis_cfg(cfg);
        ensure_analysis_cfg(&cfg_path, cfg).map_err(EngineError::Config)?;
        let args = vec![
            "analysis".to_owned(),
            "-config".to_owned(),
            cfg_path.display().to_string(),
            "-model".to_owned(),
            model_path.display().to_string(),
        ];
        let process = Process::spawn(&cfg.engine_path, &args, waker.as_ref())?;
        let self_tx = process.self_sender();
        Ok(Self {
            process,
            self_tx,
            next_id: 1,
            pending: HashMap::new(),
            last_size: None,
            ready_seen: false,
            startup_timeout_emitted: false,
            exit_reported: false,
        })
    }

    /// 提交分析查询，返回其 id（单调递增）。错误经 [`EngineEvent::Failed`]
    /// 异步上报，不在此处阻塞或 panic。
    pub fn analyze(&mut self, query: AnalysisQuery) -> QueryId {
        let id = self.alloc_id();
        self.last_size = Some(query.board_size);
        self.pending.insert(id, Pending {
            size: query.board_size,
            deadline: Instant::now() + QUERY_TIMEOUT,
        });
        if let Err(e) = self.process.write_line(&query.encode(id)) {
            self.pending.remove(&id);
            self.push_external(EngineEvent::Failed(e));
        }
        id
    }

    /// 终止在飞的查询（局面变化时调用；引擎会为该查询补发终态报告）。
    pub fn terminate(&mut self, id: QueryId) {
        self.send_control(ControlRequest::Terminate(id));
    }

    /// 终止全部在飞查询。
    pub fn terminate_all(&mut self) {
        self.send_control(ControlRequest::TerminateAll);
    }

    /// 非阻塞取事件；UI 每帧调用。引擎思考期间立即返回 `None`。
    pub fn try_recv(&mut self) -> Option<EngineEvent> {
        // 1. 启动可用性判定：超过上限仍未见就绪标记 → 上报失败（仅一次）。
        if !self.ready_seen
            && !self.startup_timeout_emitted
            && self.process.elapsed() >= STARTUP_TIMEOUT
        {
            self.startup_timeout_emitted = true;
            return Some(EngineEvent::Failed(EngineError::StartupTimeout(STARTUP_TIMEOUT)));
        }
        // 2. 查询超时判定：上报并对引擎发 terminate 止损。
        if let Some((id, overdue)) = self.next_timed_out() {
            self.pending.remove(&id);
            self.send_control(ControlRequest::Terminate(id));
            return Some(EngineEvent::Failed(EngineError::QueryTimeout {
                id,
                elapsed: QUERY_TIMEOUT + overdue,
            }));
        }
        // 3. 通道事件（顺序化）。
        match self.process.try_event() {
            Some(PipeEvent::Stdout(incoming)) => return self.handle_incoming(incoming),
            Some(PipeEvent::StderrLine(line)) => return Some(EngineEvent::Log(line)),
            Some(PipeEvent::Ready) => {
                self.ready_seen = true;
                return Some(EngineEvent::Ready);
            }
            Some(PipeEvent::External(event)) => return Some(event),
            None => {}
        }
        // 4. 进程退出检测（`Child::try_wait`，无专职 reap 线程）。
        self.poll_exit()
    }

    /// 强杀引擎并回收进程（用于异常路径；正常关闭用 [`Engine::shutdown`]）。
    pub fn kill(&mut self) {
        self.process.kill_and_reap();
    }

    /// 优雅关闭：关闭 stdin（引擎自行退出，实测 EXIT=0），限时等待，
    /// 超时强杀，避免僵尸进程。消费自身。
    pub fn shutdown(mut self) {
        self.process.close_stdin();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if self.process.try_exit().is_some() {
                return;
            }
            // 等待期间继续消化事件，避免通道积压（也覆盖 Exited 事件路径）。
            let _ = self.process.wait_event(Duration::from_millis(100));
        }
        self.process.kill_and_reap();
    }

    // ---- 内部 ----

    fn alloc_id(&mut self) -> QueryId {
        let id = QueryId::new(self.next_id);
        self.next_id += 1;
        id
    }

    fn push_external(&mut self, event: EngineEvent) {
        let _ = self.self_tx.send(PipeEvent::External(event));
    }

    /// 发送控制请求（terminate 等也必须带字符串 id，实测缺失会被拒绝）。
    fn send_control(&mut self, request: ControlRequest) {
        let line = request.encode(self.alloc_id());
        if let Err(e) = self.process.write_line(&line) {
            self.push_external(EngineEvent::Failed(e));
        }
    }

    fn next_timed_out(&self) -> Option<(QueryId, Duration)> {
        let now = Instant::now();
        self.pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, p)| (*id, now.duration_since(p.deadline)))
            .min_by_key(|(id, _)| *id)
    }

    fn handle_incoming(&mut self, incoming: Incoming) -> Option<EngineEvent> {
        match incoming {
            Incoming::Report { id, report: raw } => {
                let Some(id) = id else {
                    return Some(EngineEvent::Log("收到无 id 的报告，已忽略".to_owned()));
                };
                let is_final = raw.is_final();
                let size = self
                    .pending
                    .get(&id)
                    .map(|p| p.size)
                    .or(self.last_size)
                    .unwrap_or_else(|| Size::new(19).expect("19 为合法尺寸"));
                if is_final {
                    self.pending.remove(&id);
                }
                Some(EngineEvent::Report { id, report: raw.decode(size), is_final })
            }
            Incoming::Error { id, message, .. } => {
                if let Some(id) = id {
                    self.pending.remove(&id);
                }
                Some(EngineEvent::Failed(EngineError::QueryRejected { id, message }))
            }
            Incoming::Warning { id, field, message } => {
                // 非破坏性提示：**绝不能**走 `Incoming::Error` 那条路径——
                // Error 会 `pending.remove(&id)`（随后的查询超时判定失效），
                // 上层 `on_failed` 还会清 `inflight`，等于把引擎仍在正常
                // 分析的查询打死（实测警告后 10 条报告照常到达）。
                // 这里只转述为日志事件，不动 pending / inflight 任何状态。
                Some(EngineEvent::Warning {
                    id,
                    field,
                    message,
                })
            }
            Incoming::TerminateEcho { terminate_id, .. } => Some(EngineEvent::Log(match terminate_id {
                Some(target) => format!("引擎已确认终止查询 {target}"),
                None => "引擎已确认终止".to_owned(),
            })),
            Incoming::Version { version, .. } => Some(EngineEvent::Log(format!("引擎版本 {version}"))),
        }
    }

    fn poll_exit(&mut self) -> Option<EngineEvent> {
        if self.exit_reported {
            return None;
        }
        if let Some(status) = self.process.try_exit() {
            self.exit_reported = true;
            self.pending.clear();
            return Some(EngineEvent::Exited(status));
        }
        None
    }
}
