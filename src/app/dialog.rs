//! portal 文件对话框与确认流：打开 / 另存的发起（含各类守卫）、
//! portal 事件分发、目录记忆。
//!
//! 全部从原 `app.rs` 单体拆出，逐字节照搬：守卫顺序、提示文案与
//! 「先另存再继续」的回调语义都不变。

use std::path::Path;

use super::{GuanqiApp, OpenSaveDir, PendingConfirm, PendingDialog, PostSaveAction, UnsavedConfirm, default_sgf_name};
use crate::portal::{FileDialog, PortalEvent};
use crate::ui::analysis_panel::LoadNotice;

impl GuanqiApp {
/// 发起「打开棋谱」对话框（菜单入口与 Ctrl+O 共用）。
///
/// - portal 不可用：不发起，提示原因（入口已置灰，快捷键仍会走到这里）；
/// - 已有对话框在等待：**忽略本次请求并提示**。portal 每次调用都会
///   真实弹出一个原生对话框并占用一个专职等待线程，叠加调用会让
///   多个对话框同时压到用户屏幕上（且先弹的那个仍会投递结果），故必须
///   等当前选择完成后再发起新的；
/// - 当前文档有未保存改动（脏）：先经「未保存」确认（与丢弃副本研究
///   的确认**合并为一条**：先弹副本确认，确认后按干净态重新走本函数
///   的脏判定，用户至多确认一次——两条守卫的文案不同，不能并成
///   一个窗口，但入口收敛到一次交互）。
pub(super) fn open_file_dialog(&mut self) {
    // 载入会替换原谱：所有副本（含活动若是副本）连同研究一起丢弃，
    // 用户选中的棋谱路径在确认前尚未取得，确认即重新发起对话框。
    if self.research_moves() > 0 {
        self.pending_confirm = PendingConfirm::LoadGame;
        return;
    }
    // 无副本研究成果但当前文档脏：同样先确认再发起对话框。
    if self.is_dirty() {
        self.unsaved_confirm = Some(UnsavedConfirm::OpenDialog);
        return;
    }
    self.open_file_dialog_now();
}

/// 直接发起「打开棋谱」对话框（无任何内容守卫；对话框等待中 /
/// portal 不可用等运行时守卫仍生效）。守卫通过后的实际发起路径。
pub(super) fn open_file_dialog_now(&mut self) {
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
/// 研究成果已随确认清除（`drop_copy` 无副作用地放行了守卫）；
/// 此时再查脏标记——脏则走「未保存」确认（同一入口第二次判定，
/// 用户在副本确认时未被告知未保存风险，不能在这里跳过）。
pub(super) fn open_file_dialog_after_confirm(&mut self) {
    if self.research_moves() == 0 {
        self.open_file_dialog();
    }
}

/// 发起「另存为」对话框（菜单入口与 Ctrl+Shift+S 共用）。
/// 默认文件名：原谱取档案名；研究副本取「原名-副本N.sgf」
///（[`Self::copy_default_name`]），提示文件尚未真实存盘。
///
/// `then` 为「先另存再继续」的回调动作：`Some` 时记入
/// [`Self::post_save`]，另存成功分支自动继续原操作；普通另存传
/// [`PostSaveAction::None`]。
pub(super) fn save_file_dialog(&mut self) {
    self.save_file_dialog_then(PostSaveAction::None);
}

/// 带回调的另存发起（「先另存再继续」出口的落点）。
pub(super) fn save_file_dialog_then(&mut self, then: PostSaveAction) {
    if let Some(notice) = self.dialog_guard() {
        self.notices.push(notice);
        return;
    }
    self.post_save = then;
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
            self.post_save = PostSaveAction::None;
            self.notices.push(LoadNotice::Failed(err.to_string()));
        }
    }
}

/// 对话框发起前的公共守卫：portal 不可用 / 已有对话框在等待时
/// 返回提示（等待中的对话框不区分用途——任一在等都不允许叠加）。
pub(super) fn dialog_guard(&self) -> Option<LoadNotice> {
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

/// 处理 portal 对话框结果（`logic` 每帧轮询取出，不阻塞）。
/// 选中即记住该文件所在目录（子项 3）：下次打开 / 另存的 portal
/// `current_folder` 用它（随防抖落盘持久化）。
pub(super) fn on_portal_event(&mut self, kind: PendingDialog, event: PortalEvent) {
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
        PendingDialog::ExportReport => match event {
            PortalEvent::Picked(path) => {
                // 导出位置同样记进「另存目录」槽位：报告通常与棋谱、
                // 另存的 SGF 放在同一批目录里。
                self.remember_dir(OpenSaveDir::Save, &path);
                self.export_report(path);
            }
            // 用户取消导出：静默（无回调动作需要清理）。
            PortalEvent::Cancelled => {}
            PortalEvent::Failed(err) => {
                self.notices.push(LoadNotice::Failed(err.to_string()));
            }
        },
        PendingDialog::Save => match event {
            PortalEvent::Picked(path) => {
                self.remember_dir(OpenSaveDir::Save, &path);
                self.save_game(path);
            }
            PortalEvent::Cancelled => {
                // 用户取消另存：若这是「先另存再继续」的那次另存，
                // 回调一并丢弃（确认条早已收起；用户重新发起原操作
                // 会再次看到确认）。普通另存本就无回调，清空无副作用。
                self.post_save = PostSaveAction::None;
            }
            PortalEvent::Failed(err) => {
                self.post_save = PostSaveAction::None;
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
}
