//! 导出「AI 标注版库」（阶段 23.3）：保存对话框发起 + 选中后的合并
//! 写盘执行体。
//!
//! 流程与「导出复盘报告」（`export.rs`）同构：portal 对话框在专职线程
//! 等待，选中后在本方法内完成取数与写盘。**最重要的边界：参照文件本体
//! 绝不写回**——合并与落盘全部走 [`crate::ui::analysis::ref_export`]
//! 的数据层（在参照树**副本**上合并、只写用户选定的新路径），本文件只
//! 负责「从 App 取数 + 发起对话框 + 提示结果」。

use std::path::PathBuf;

use super::{GuanqiApp, PendingDialog};
use crate::ui::analysis_panel::LoadNotice;

impl GuanqiApp {
    /// 发起「导出 AI 标注版…」对话框（侧栏「参照打分」卡片按钮的落点）。
    ///
    /// 守卫：无参照 / 无打分结果时提示并返回（侧栏按钮只在有结果时渲染，
    /// 这里是运行时兜底——结果可能在渲染帧与执行帧之间被换参照清空）。
    /// portal 不可用 / 已有对话框在等待复用 [`Self::dialog_guard`]。
    /// 对 examples/headless.rs 的可达性走文件尾的 [`Self::export_ai_dialog_pub`]。
    pub(super) fn export_ai_dialog(&mut self) {
        if let Some(notice) = self.dialog_guard() {
            self.notices.push(notice);
            return;
        }
        let Some(reference) = self.reference.as_ref() else {
            self.notices.push(LoadNotice::Warn(
                "导出 AI 标注版被拒：未加载参照文件，请先完成打分。".to_owned(),
            ));
            return;
        };
        if self.analysis.ref_scan_results().is_none() {
            self.notices.push(LoadNotice::Warn(
                "导出 AI 标注版被拒：还没有打分结果，请先完成打分。".to_owned(),
            ));
            return;
        }
        // 默认名：`<参照名去扩展名>-AI标注.sgf`（与另存 / 导出报告同一
        // 来源习惯：取来源路径的文件名）。
        let stem = reference
            .source()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "参照".to_owned());
        let default_name = format!("{stem}-AI标注.sgf");
        match crate::portal::FileDialog::save_file(
            "导出 AI 标注版库（SGF）",
            &default_name,
            // 初始目录与另存为同一槽位（标注版通常想放在参照文件旁边）。
            self.engine_cfg
                .ui_prefs
                .last_save_dir
                .as_deref()
                .or(self.engine_cfg.ui_prefs.last_open_dir.as_deref()),
            Some(self.waker.clone()),
        ) {
            Ok(dialog) => {
                self.dialog = Some((dialog, PendingDialog::ExportAiSgf));
            }
            Err(err) => {
                self.notices.push(LoadNotice::Failed(err.to_string()));
            }
        }
    }

    /// 生成并写出「AI 标注版库」（对话框选中路径后的执行体）。
    ///
    /// 取数（参照树 + 打分结果）与合并全部委托给数据层
    /// （[`crate::ui::analysis::ref_export::export_ai_annotated`]）：
    /// 副本上合并、只写 `path`。成功提示带块数（标了几个局面），失败
    /// 只提示、不 panic（目录不可写是用户环境问题）。
    pub(super) fn export_ai_annotated(&mut self, path: PathBuf) {
        let Some(reference) = self.reference.as_ref() else {
            self.notices.push(LoadNotice::Warn(
                "导出 AI 标注版失败：参照已卸载（打分结果随换参照清空）。".to_owned(),
            ));
            return;
        };
        let Some(results) = self.analysis.ref_scan_results() else {
            self.notices.push(LoadNotice::Warn(
                "导出 AI 标注版失败：打分结果已清空（换参照即清空），请重新打分。".to_owned(),
            ));
            return;
        };
        match crate::ui::analysis::ref_export::export_ai_annotated(
            &path,
            reference.tree(),
            results,
        ) {
            Ok(written) => {
                self.notices.push(LoadNotice::Ok(format!(
                    "AI 标注版已导出到 {path_display}（{written} 个局面带评估块，\
                     参照原文件未修改）。",
                    path_display = path.display(),
                )));
            }
            Err(err) => {
                self.notices
                    .push(LoadNotice::Failed(format!("导出 AI 标注版失败：{err}")));
            }
        }
    }
}

impl GuanqiApp {
    /// [`Self::export_ai_dialog`] 的 headless 驱动入口（仅供
    /// `examples/headless.rs` 的 `export_ai` 指令；应用路径不调用）。
    /// 与 `export_report_dialog_pub` 同一处理：菜单外入口是 `pub(super)`，
    /// example 无法触达，经 `#[doc(hidden)]` pub 包装开放。
    #[doc(hidden)]
    pub fn export_ai_dialog_pub(&mut self) {
        self.export_ai_dialog();
    }
}
