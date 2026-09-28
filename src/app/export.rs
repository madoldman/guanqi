//! 复盘报告导出（阶段 21 #6）：保存对话框 → 统计取数 → 棋盘图绘制 →
//! Markdown 写盘 → 消息区提示。
//!
//! 流程与「另存为」同构（`io::save_game`）：portal 对话框在专职线程等
//! 待，选中后在本方法内完成取数与写盘（文件 I/O 都是本地的几 KB 到几
//! 百 KB，不值得一根线程）。守卫（portal 不可用 / 已有对话框在等待）
//! 复用 [`Self::dialog_guard`]；报告与数字的全部口径见
//! [`crate::ui::report`] 与 [`crate::ui::report_image`] 模块文档。

use std::path::PathBuf;

use super::{GuanqiApp, PendingDialog};
use crate::ui::analysis_panel::LoadNotice;
use crate::ui::report::{self, DiagramEntry};
use crate::ui::report_image::{self, BoardDiagram};

impl GuanqiApp {
    /// 发起「导出复盘报告」对话框（菜单入口）。未载入棋谱置灰（菜单
    /// 侧守卫），这里只挡运行时态（portal 不可用 / 对话框等待中）。
    /// 对 examples/headless.rs 的可达性走文件尾的 [`Self::export_report_dialog_pub`]。
    pub(super) fn export_report_dialog(&mut self) {
        if let Some(notice) = self.dialog_guard() {
            self.notices.push(notice);
            return;
        }
        // 默认名：`<棋谱名>-复盘.md`（来源目录记忆沿用 last_save_dir，
        // 与另存为同一槽位——导出的报告通常也想放在棋谱旁边）。
        let default_name = self
            .loaded
            .as_ref()
            .map(|meta| default_report_name(&meta.source))
            .unwrap_or_else(|| "复盘报告.md".to_owned());
        match crate::portal::FileDialog::save_file(
            "导出复盘报告（Markdown）",
            &default_name,
            self.engine_cfg
                .ui_prefs
                .last_save_dir
                .as_deref()
                .or(self.engine_cfg.ui_prefs.last_open_dir.as_deref()),
            Some(self.waker.clone()),
        ) {
            Ok(dialog) => {
                self.dialog = Some((dialog, PendingDialog::ExportReport));
            }
            Err(err) => {
                self.notices.push(LoadNotice::Failed(err.to_string()));
            }
        }
    }

    /// 生成并写出复盘报告（对话框选中路径后的执行体）。
    ///
    /// 统计与差异手**只算一次**：`game_summary` 的结果同时喂报告骨架、
    /// 条目组装与 dump 侧口径（与侧栏卡片同一函数，数字必然一致）；
    /// 图片按同一批条目绘制，图与表同源。任何一步失败只提示、不 panic
    /// （目录不可写是用户环境问题，不是程序错误）。
    pub(super) fn export_report(&mut self, path: PathBuf) {
        // 图片的 PNG 顶部水印需要中文字形：`render_board_png` 经
        // `ctx.fonts_mut` 光栅化。`egui::Context` 是 Arc 型廉价克隆，
        // `new_with_prefs` 时留一份在 App 上（`report_ctx` 字段，与运行
        // 实例同一上下文），对话框结果在 `logic` 帧处理时仍可用。
        let ctx = self.report_ctx.clone();
        let board = &self.board;
        let summary = self.analysis.game_summary(board);
        let meta = report::report_meta(self.loaded.as_ref(), board.line_len());
        let entries: Vec<DiagramEntry> = self.analysis.diagram_entries(board);
        // 各手「走子后」盘面：position_at(n) 含预设摆子（只读快照），
        // 棋盘本体克隆（浅拷贝树，`Board` 是廉价 Clone）后按线重放即可；
        // 为不依赖游标，直接用记录重放——记录来自当前线，与统计同线。
        let mut markdown = report::render_markdown(&meta, &summary);
        // 表格行替换 + 图引用：把与图同源的条目写进差异手表（骨架里的
        // 占位行在 render_markdown 只留表头，行内容在这里填）。
        let image_names: Vec<String> = entries
            .iter()
            .map(|entry| {
                format!(
                    "{}-m{}.png",
                    path.file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "复盘报告".to_owned()),
                    entry.turn
                )
            })
            .collect();
        report::append_diagrams(&mut markdown, &entries, &image_names);
        let draw = |entry: &DiagramEntry| -> Result<Vec<u8>, String> {
            let Some(grid) = board.position_at(entry.turn) else {
                return Err(format!("第 {} 手局面不可得", entry.turn));
            };
            let diagram = BoardDiagram {
                size: board.size(),
                grid,
                last_move: entry.actual,
                severity: entry.severity,
                actual: entry.actual,
                best: entry.best,
            };
            Ok(report_image::render_board_png(
                &ctx,
                &diagram,
                &report::diagram_caption(entry),
            ))
        };
        match report::write_report(&path, &markdown, &entries, draw) {
            Ok(images) => {
                let n = images.len();
                self.notices.push(LoadNotice::Ok(format!(
                    "复盘报告已导出到 {}（{} 张棋盘图）。",
                    path.display(),
                    n
                )));
            }
            Err(err) => {
                self.notices
                    .push(LoadNotice::Failed(format!("导出复盘报告失败：{err}")));
            }
        }
    }
}

/// 报告默认文件名：`<棋谱名去扩展名>-复盘.md`（与另存对话框的默认名
/// 同一来源习惯：取档案路径文件名）。
fn default_report_name(source: &std::path::Path) -> String {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "棋谱".to_owned());
    format!("{stem}-复盘.md")
}

impl GuanqiApp {
    /// [`Self::export_report_dialog`] 的 headless 驱动入口（仅供
    /// `examples/headless.rs` 的 `export` 指令；应用路径不调用）。
    /// 菜单入口是 `pub(super)`，example 无法触达，与 snapshot.rs 的
    /// `#[doc(hidden)]` 取证面同一处理。
    #[doc(hidden)]
    pub fn export_report_dialog_pub(&mut self) {
        self.export_report_dialog();
    }
}
