//! Portal 模块：经 XDG Desktop Portal 调用**系统原生**文件对话框
//! （本机 KDE Wayland 下由 `org.freedesktop.impl.portal.desktop.kde` 弹出）。
//!
//! D-Bus 客户端为纯 std 手写、零第三方依赖：[`wire`] / [`marshal`] 是线格式
//! 编解码，[`dbus`] 是连接层，[`filechooser`] 是 portal 语义层。上层 UI 一律
//! 通过 [`FileDialog`] 门面使用，不直接接触 D-Bus；子模块公开仅供诊断联调
//! （见 `examples/portal_probe.rs`）。
//!
//! # 对上层（UI 接线）的约定（与 engine 模块一致）
//!
//! - [`FileDialog::available`] 先行探测（不弹窗），无 portal 环境可降级提示；
//! - [`FileDialog::open_file`] / [`FileDialog::save_file`] 立即返回，
//!   对话框在专职线程阻塞等待；
//! - UI 每帧 [`FileDialog::try_recv`] 非阻塞取结果；
//! - 构造时可传 waker（如 `egui::Context::request_repaint` 包装），结果入队即唤醒。

pub mod dbus;
pub mod filechooser;
pub mod marshal;
pub mod wire;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// 事件入队后唤醒 UI 的回调（如 `move || ctx.request_repaint()`）。
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Portal 侧错误（区分「环境不可用」与「单次调用失败」，便于 UI 提示与降级）。
#[derive(Debug)]
pub enum PortalError {
    /// 没有可用的 D-Bus 会话总线（地址缺失、连接失败、认证被拒、连接中断）。
    NoDBus(String),
    /// 会话总线可达，但 portal 未提供 FileChooser 接口（无 portal 服务或后端过旧）。
    NoPortal(String),
    /// 调用被对端拒绝（含 D-Bus 错误名与错误消息）。
    CallFailed { name: String, message: String },
    /// 用户长时间未操作，等待超时（超时前已尽力关闭对话框窗口）。
    Timeout(Duration),
    /// 协议异常（对端消息无法解析、应答结构不符预期）。
    Protocol(String),
    /// 本地 socket I/O 错误。
    Io(std::io::Error),
}

impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDBus(m) => write!(f, "D-Bus 会话总线不可用：{m}"),
            Self::NoPortal(m) => write!(f, "portal 文件对话框服务不可用：{m}"),
            Self::CallFailed { name, message } => write!(f, "portal 调用失败（{name}）：{message}"),
            Self::Timeout(wait) => write!(f, "等待用户选择超时（{wait:.0?}），已尽力关闭对话框"),
            Self::Protocol(m) => write!(f, "portal 协议异常：{m}"),
            Self::Io(e) => write!(f, "与 D-Bus 的通信失败：{e}"),
        }
    }
}

impl std::error::Error for PortalError {}

/// 文件对话框的最终结果（UI 每帧经 [`FileDialog::try_recv`] 取出）。
#[derive(Debug)]
pub enum PortalEvent {
    /// 用户选中了一个文件。
    Picked(PathBuf),
    /// 用户取消（关闭了对话框）。
    Cancelled,
    /// 失败（超时、portal 不可用、协议异常等）。
    Failed(PortalError),
}

/// 系统原生文件对话框句柄：一次 [`FileDialog::open_file`] 对应一个专职等待线程。
pub struct FileDialog {
    rx: Receiver<PortalEvent>,
}

impl FileDialog {
    /// 探测 portal 文件对话框是否可用（**不弹任何窗口**），返回 FileChooser 接口版本。
    /// 失败即返回 [`PortalError::NoDBus`] / [`PortalError::NoPortal`]，UI 可据此降级。
    pub fn available() -> Result<u32, PortalError> {
        let mut conn = dbus::Connection::connect()?;
        filechooser::probe(&mut conn)
    }

    /// 发起「打开文件」对话框，**立即返回**；等待在专职线程进行。
    ///
    /// - 选中 → [`PortalEvent::Picked`]；取消 → [`PortalEvent::Cancelled`]；
    ///   失败（含 300s 等待超时，超时前会尽力关闭对话框）→ [`PortalEvent::Failed`]。
    /// - `current_folder`：对话框初始目录（上次记住的目录；`None` = 交
    ///   portal 自选）。
    /// - `waker` 在结果入队时于**工作线程**被调用（如 `move || ctx.request_repaint()`）。
    /// - 连接 / 探测失败会**同步**返回 Err，便于 UI 立即提示。
    /// - 一个实例只产生一个事件；丢弃实例后结果仍会投递（发送失败被忽略）。
    pub fn open_file(
        title: &str,
        current_folder: Option<&std::path::Path>,
        waker: Option<Waker>,
    ) -> Result<FileDialog, PortalError> {
        let current_folder = current_folder.map(std::path::Path::to_path_buf);
        spawn_dialog(title, waker, move |conn, title| {
            filechooser::open_file_blocking(conn, title, current_folder.as_deref())
        })
    }

    /// 发起「保存文件」对话框（默认文件名 `default_name`），**立即返回**；
    /// 等待在专职线程进行。事件语义与 [`FileDialog::open_file`] 一致：
    /// 确认位置 → [`PortalEvent::Picked`]，取消 → [`PortalEvent::Cancelled`]。
    /// `current_folder` 为初始目录（`None` = 交 portal 自选）。
    pub fn save_file(
        title: &str,
        default_name: &str,
        current_folder: Option<&std::path::Path>,
        waker: Option<Waker>,
    ) -> Result<FileDialog, PortalError> {
        let default_name = default_name.to_owned();
        let current_folder = current_folder.map(std::path::Path::to_path_buf);
        spawn_dialog(title, waker, move |conn, title| {
            filechooser::save_file_blocking(
                conn,
                title,
                &default_name,
                current_folder.as_deref(),
            )
        })
    }

    /// 非阻塞取结果；用户未操作完时立即返回 `None`（UI 每帧调用）。
    pub fn try_recv(&mut self) -> Option<PortalEvent> {
        self.rx.try_recv().ok()
    }
}

/// 连接会话总线 → 探测 portal → 专职线程阻塞等待对话框结果（打开与
/// 保存共用：连接 / 探测留在调用线程同步报错，阻塞等待交给工作线程，
/// 结果经 waker 唤醒 UI）。
fn spawn_dialog(
    title: &str,
    waker: Option<Waker>,
    wait: impl FnOnce(&mut dbus::Connection, &str) -> Result<Option<PathBuf>, PortalError>
    + Send
    + 'static,
) -> Result<FileDialog, PortalError> {
    let mut conn = dbus::Connection::connect()?;
    filechooser::probe(&mut conn)?;
    let title = title.to_owned();
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("portal-dialog".into())
        .spawn(move || {
            let event = match wait(&mut conn, &title) {
                Ok(Some(path)) => PortalEvent::Picked(path),
                Ok(None) => PortalEvent::Cancelled,
                Err(error) => PortalEvent::Failed(error),
            };
            if tx.send(event).is_ok()
                && let Some(waker) = waker
            {
                waker();
            }
        })
        .map_err(PortalError::Io)?;
    Ok(FileDialog { rx })
}
