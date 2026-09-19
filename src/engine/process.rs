//! 引擎子进程管理（阶段 3.1）：spawn、管道、读线程与事件通道。
//!
//! 线程模型（仅 std 线程 + `mpsc`，无任何异步运行时）：
//!
//! - **stdout 读线程**：按行读 JSON → [`protocol::parse_line`] → 通道；
//! - **stderr 读线程**：按行读 → 通道作日志事件。引擎启动期 stderr
//!   日志量大，**必须持续排空**，否则子进程写满管道缓冲会整体卡死；
//!   同时识别就绪标记（实测 stderr 输出 `Started, ready to begin
//!   handling requests`），用于区分「启动中」与「可用」。
//! - UI 线程不阻塞：全部事件经通道投递，由上层每帧 [`super::Engine::try_recv`]
//!   非阻塞轮询；提供可选 waker（如 `egui::Context::request_repaint` 的包装）
//!   在事件入队时唤醒界面。
//!
//! 进程退出检测不占线程：由 [`Process::try_exit`]（`Child::try_wait`）在
//! [`super::Engine::try_recv`] 中轮询，避免多一个专职 reap 线程。

use super::{EngineError, EngineEvent};
use crate::engine::protocol::{self, Incoming};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// stderr 中的就绪标记（v1.18.2 实测原文）。
const READY_MARKER: &str = "Started, ready to begin handling requests";

/// 事件入队后唤醒 UI 的回调（如 `move || ctx.request_repaint()`）。
pub(crate) type Waker = Arc<dyn Fn() + Send + Sync>;

/// 读线程 → UI 线程的通道事件。
#[derive(Debug)]
pub(crate) enum PipeEvent {
    /// stdout 一行的解析结果。
    Stdout(Incoming),
    /// stderr 一行（日志）。
    StderrLine(String),
    /// stderr 出现就绪标记。
    Ready,
    /// 引擎层自查产生的事件（如写失败、已死进程上的查询）。
    External(EngineEvent),
}

/// 子进程句柄：持有 `Child`（退出检测 / kill）与 stdin（写查询）。
pub(crate) struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<PipeEvent>,
    /// 通道发送端的备份：引擎层自查事件（写失败等）也从同一队列走。
    tx: Sender<PipeEvent>,
    spawn_instant: Instant,
}

fn emit(tx: &Sender<PipeEvent>, waker: Option<&Waker>, event: PipeEvent) {
    if tx.send(event).is_ok()
        && let Some(waker) = waker
    {
        waker();
    }
}

impl Process {
    /// 启动 `katago analysis` 并建立三个管道与两个读线程。
    /// 立即返回（模型加载 / OpenCL 调优在子进程内异步进行）。
    pub(crate) fn spawn(
        engine: &std::path::Path,
        args: &[String],
        waker: Option<&Waker>,
    ) -> Result<Self, EngineError> {
        if !is_executable_file(engine) {
            return Err(EngineError::EngineNotFound(engine.to_owned()));
        }
        let mut child = Command::new(engine)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(EngineError::SpawnFailed)?;

        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| EngineError::SpawnFailed(std::io::Error::other("stdout 管道缺失")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| EngineError::SpawnFailed(std::io::Error::other("stderr 管道缺失")))?;

        let (tx, rx) = channel::<PipeEvent>();
        spawn_stdout_thread(tx.clone(), stdout, waker.cloned());
        spawn_stderr_thread(tx.clone(), stderr, waker.cloned());

        Ok(Self {
            child,
            stdin,
            rx,
            tx,
            spawn_instant: Instant::now(),
        })
    }

    /// 引擎层自查事件用的通道发送端。
    pub(crate) fn self_sender(&self) -> Sender<PipeEvent> {
        self.tx.clone()
    }

    /// 限时阻塞等一个事件（仅优雅关闭路径使用）。
    pub(crate) fn wait_event(&self, timeout: Duration) -> Option<PipeEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// 向引擎 stdin 写一行 JSON（含换行）并冲刷。
    pub(crate) fn write_line(&mut self, line: &str) -> Result<(), EngineError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or(EngineError::ProcessGone)?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(EngineError::StdinWrite)
    }

    /// 非阻塞取一个事件。
    pub(crate) fn try_event(&self) -> Option<PipeEvent> {
        self.rx.try_recv().ok()
    }

    /// 非阻塞检测进程退出（并 reap）。
    pub(crate) fn try_exit(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// 启动至今的时长（启动超时判定用）。
    pub(crate) fn elapsed(&self) -> std::time::Duration {
        self.spawn_instant.elapsed()
    }

    /// 关闭 stdin（优雅关闭路径：引擎读完 stdin 后自行退出，实测 EXIT=0）。
    pub(crate) fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// 强杀（SIGKILL）并回收，避免僵尸进程。
    pub(crate) fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn spawn_stdout_thread(tx: Sender<PipeEvent>, stdout: std::process::ChildStdout, waker: Option<Waker>) {
    std::thread::Builder::new()
        .name("katago-stdout".into())
        .spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let incoming = match protocol::parse_line(trimmed) {
                            Ok(incoming) => incoming,
                            Err(message) => {
                                emit(&tx, waker.as_ref(), PipeEvent::External(EngineEvent::Log(message)));
                                continue;
                            }
                        };
                        emit(&tx, waker.as_ref(), PipeEvent::Stdout(incoming));
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("创建 stdout 读线程失败");
}

fn spawn_stderr_thread(tx: Sender<PipeEvent>, stderr: std::process::ChildStderr, waker: Option<Waker>) {
    std::thread::Builder::new()
        .name("katago-stderr".into())
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if line.contains(READY_MARKER) {
                            emit(&tx, waker.as_ref(), PipeEvent::Ready);
                        }
                        emit(&tx, waker.as_ref(), PipeEvent::StderrLine(line));
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("创建 stderr 读线程失败");
}
