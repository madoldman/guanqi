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
    ensure_analysis_cfg, find_katago_in_path, load_settings, resolve_rules, save_settings,
    scan_weights, settings_path, Difficulty, EngineBackend, EngineConfig, LoadedSettings, Rules,
    RulesResolution, UiPrefs, WindowGeometry,
};
pub use protocol::{
    AnalysisQuery, AnalysisReport, MoveInfo, MoveRule, MoveRules, QueryId, RootInfo, WinrateView,
};

use crate::board::Size;
use process::{PipeEvent, Process, Waker};
use protocol::{ControlRequest, Incoming};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// 超时缩放的**实测锚点**：2000 visits 走子查询在本机（b18 + 680M iGPU
/// / OpenCL、16 线程）实测 26.8s 完成（8 线程 33.8s，探针实测）。
/// 缩放公式取「2×(visits/60) + 15s 基准」，锚点处约 82s——旧固定 30s
/// 与「最强」档自报的每手约 34s 公开矛盾，8 线程下实测已超线（33.8s
/// > 30s），复杂位置必然被超时误杀。
const QUERY_TIMEOUT_BASE_SECS: f64 = 15.0;
/// 超时缩放的 visits 速率分母：与难度档等待估算同一实测口径（约 60
/// visits/s，见 `config::ESTIMATED_VISITS_PER_SEC` 文档）。
const QUERY_TIMEOUT_VISITS_PER_SEC: f64 = 60.0;

/// 按查询 visits 计算超时线（秒）：`2×(visits/60) + 15`，下限 30s、
/// 上限 10 分钟（`visits_max` 哨兵 = 不限 visits 的批量/默认查询）。
/// 系数取 2× 是给「复杂位置 + 慢机」留余量——超时误杀的代价（界面
/// 全空、分析缺失）远大于晚几秒报警。
fn query_timeout_secs(visits: u32) -> u64 {
    if visits == u32::MAX {
        return 30;
    }
    let estimate = f64::from(visits) / QUERY_TIMEOUT_VISITS_PER_SEC;
    ((2.0 * estimate + QUERY_TIMEOUT_BASE_SECS) as u64).clamp(30, 600)
}
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
    /// 权重文件不存在（被移动 / 删除）：启动必然失败，提前拦截。
    ModelMissing(PathBuf),
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
            Self::ModelMissing(p) => write!(
                f,
                "权重文件不存在：{}（可能已被移动或删除，请到设置中重新选择）",
                p.display()
            ),
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
    /// 携带退出前**最后若干行 stderr**（[`STDERR_TAIL_LINES`] 条）：崩溃
    /// 时 stderr 尾部几乎总有直接原因（OpenCL 编译失败、配置键非法、
    /// panic 信息），事件里不带的话 UI 只能显示「已退出（信号 11）」，
    /// 用户无从查起。
    Exited(std::process::ExitStatus, Vec<String>),
}

/// 在飞的查询信息：棋盘尺寸（坐标解码用）、视角（SIDETOMOVE 报告归一化
/// 用）与超时线（按该查询的 maxVisits 缩放，随查询记录——超时错误要
/// 报出真实等待时长，光有 deadline 反推不回超时线）。
struct Pending {
    size: Size,
    /// 该查询请求的报告视角：`SideToMove` 时报告在 Engine 出口按行棋方
    /// 归一化回黑视角（口径见 `protocol::WinrateView` 实测文档）。
    view: WinrateView,
    deadline: Instant,
    /// 本查询的超时线（[`query_timeout_secs`] 的结果）。
    timeout: Duration,
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
    /// 进程退出前的**最后若干行 stderr**（[`STDERR_TAIL_LINES`] 条环形
    /// 缓冲）：由 [`Engine::try_recv`] 在消费 stderr 日志事件时顺手记录，
    /// 进程退出时随 [`EngineEvent::Exited`] 一并带出。stderr 读线程本身
    /// 不缓存——它只管转发，缓存放消费侧可同时覆盖「读线程转发」与
    /// 「退出时取尾」两个时机，无需加锁。
    stderr_tail: std::collections::VecDeque<String>,
}

/// [`EngineEvent::Exited`] 携带的 stderr 尾行数：崩溃原因几乎总在最后
/// 几行（panic 摘要 / OpenCL 报错），取 8 行足够；再多只会淹没 UI。
const STDERR_TAIL_LINES: usize = 8;

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
        // 存在性检查：只看 is_some() 会「成功启动」一个必然失败的引擎
        //（权重被删后引擎进程立刻退出，用户只看到一个含糊的崩溃），且
        // 设置面板一直把失效路径当有效配置回显。提前拦下并给出可读错误。
        if !model_path.is_file() {
            return Err(EngineError::ModelMissing(model_path));
        }
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
            stderr_tail: std::collections::VecDeque::with_capacity(STDERR_TAIL_LINES),
        })
    }

    /// 提交分析查询，返回其 id（单调递增）。错误经 [`EngineEvent::Failed`]
    /// 异步上报，不在此处阻塞或 panic。
    ///
    /// 超时线按查询的 maxVisits 缩放（见 [`query_timeout_secs`]）：固定
    /// 30s 会误杀「最强」档（2000 visits，实测 26.8–33.8s、复杂位置更久）
    /// 的合法查询。
    pub fn analyze(&mut self, query: AnalysisQuery) -> QueryId {
        let id = self.alloc_id();
        self.last_size = Some(query.board_size);
        let timeout = Duration::from_secs(query_timeout_secs(query.max_visits.unwrap_or(u32::MAX)));
        self.pending.insert(id, Pending {
            size: query.board_size,
            view: query.view,
            deadline: Instant::now() + timeout,
            timeout,
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
            // 先取该查询的超时线再移除（错误文案要报真实等待时长）。
            let timeout = self.pending.get(&id).map(|p| p.timeout);
            self.pending.remove(&id);
            self.send_control(ControlRequest::Terminate(id));
            return Some(EngineEvent::Failed(EngineError::QueryTimeout {
                id,
                elapsed: timeout.unwrap_or_default() + overdue,
            }));
        }
        // 3. 通道事件（顺序化）。
        match self.process.try_event() {
            Some(PipeEvent::Stdout(incoming)) => return self.handle_incoming(incoming),
            Some(PipeEvent::StderrLine(line)) => {
                // 尾行缓存（环形，留最后 N 条）：进程退出时随 Exited 带出，
                // 崩溃原因可见（见 STDERR_TAIL_LINES 文档）。
                if self.stderr_tail.len() == STDERR_TAIL_LINES {
                    self.stderr_tail.pop_front();
                }
                self.stderr_tail.push_back(line.clone());
                return Some(EngineEvent::Log(line));
            }
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
            Incoming::Report { id, report: mut raw } => {
                let Some(id) = id else {
                    return Some(EngineEvent::Log("收到无 id 的报告，已忽略".to_owned()));
                };
                let is_final = raw.is_final();
                // 视角归一化在**解码前**完成：查询带 SIDETOMOVE 时按行棋方
                // 把 winrate / scoreLead / raw* / moveInfos 换算回黑视角
                // （口径与公式见 protocol::WinrateView 的实测文档）。换算
                // 只依赖报告自带 current_player，全仓读取路径因此恒拿黑
                // 视角值、零改动。
                //
                // 判据只能查 **pending 里记录的查询视角**：报文本身不带视角。
                // 早先这里查的是报文自带的 view 字段，而它恒为 Black ⇒ 判据
                // 恒假、归一化从不执行（作者用真实引擎探针复现：交替视角取回
                // 的 winrate 仍是 1−w）。该字段已删除，判据只剩 pending 一处。
                if self
                    .pending
                    .get(&id)
                    .is_some_and(|p| p.view == WinrateView::SideToMove)
                {
                    raw.normalize();
                }
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
            Incoming::Error { id, message, field: _ } => {
                if let Some(id) = id {
                    self.pending.remove(&id);
                }
                // 非法规则串会被引擎直接拒绝（规则是查询必填字段）：
                // 错误原文里几乎总带 "rules" 字样，翻成中文后按字段提示。
                let message = if message.contains("rules") {
                    format!(
                        "查询被拒绝（规则串问题，请检查规则设置）：{message}"
                    )
                } else {
                    message
                };
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
            // 临终 stderr 尾行随事件带出（取最后 STDERR_TAIL_LINES 行）：
            // 崩溃原因（OpenCL 失败 / 配置非法 / panic）几乎总在尾部。
            let tail: Vec<String> = self.stderr_tail.drain(..).collect();
            return Some(EngineEvent::Exited(status, tail));
        }
        None
    }
}
