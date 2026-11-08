//! 当前手注释编辑窗（竞品移植第四批 Q2）。
//!
//! 复盘笔记直接写进当前文档（`GameMeta::set_comment`），「另存为」时
//! 随棋谱写入 SGF——不再需要手改文件。游标 0 = 根注释（写
//! `root_comment`），其余 = 该局面节点注释（按局面签名，切分支自动
//! 跟随，与显示端同一套键）。编辑置 `meta_dirty`，进未保存内容保护
//! 的覆盖范围；切文档随 `saved_rev` 同一口径复位。
//!
//! 形态：`Window::vscroll(true)` + `max_height` 双限高（长注释滚动，
//! 窗口不被撑爆）；动作条 = 保存（主）/ 清除 / 取消。

use super::GuanqiApp;
use crate::ui::analysis_panel::LoadNotice;
use crate::ui::theme;

impl GuanqiApp {
    /// 打开编辑窗：草稿 = 当前局面的既有注释（无则空）。
    pub(super) fn open_comment_editor(&mut self) {
        let text = self
            .loaded
            .as_ref()
            .and_then(|meta| meta.comment_at(&self.board))
            .unwrap_or("")
            .to_owned();
        self.comment_edit = Some(text);
        self.comment_edit_focus = true;
    }

    /// 编辑窗（每帧调用，`comment_edit` 非空时绘制）。
    pub(super) fn comment_dialog(&mut self, ctx: &egui::Context) {
        if self.comment_edit.is_none() {
            return;
        }
        // 借用整理：草稿（可变，仅 comment_edit 字段）与游标（共享，
        // 读取后即 Copy）在窗口闭包里字段级隔离。
        let cursor = self.board.cursor();
        let mut open = true;
        let mut verdict: Option<u8> = None;
        egui::Window::new("编辑当前手注释")
            .open(&mut open)
            .resizable(false)
            .vscroll(true)
            .max_height(360.0)
            .show(ctx, |ui| {
                let where_text = if cursor == 0 {
                    "根注释（本局根节点的 C[]，另存时写在文件头节点）"
                } else {
                    "本局面节点的 C[]（按局面签名，切分支 / 导航自动跟随）"
                };
                ui.weak(format!(
                    "写在第 {cursor} 手：{where_text}。"
                ));
                ui.add_space(4.0);
                if let Some(draft) = self.comment_edit.as_mut() {
                    let response = egui::TextEdit::multiline(draft)
                        .desired_width(ui.available_width())
                        .show(ui);
                    // 打开即聚焦（字段级隔离：focus 标记与草稿不同字段）。
                    if self.comment_edit_focus {
                        response.response.request_focus();
                        self.comment_edit_focus = false;
                    }
                }
                ui.add_space(4.0);
                verdict = theme::action_row(
                    ui,
                    &[
                        theme::Action { label: "保存", primary: true, enabled: true },
                        theme::Action { label: "清除", primary: false, enabled: true },
                        theme::Action { label: "取消", primary: false, enabled: true },
                    ],
                )
                .map(|i| i as u8);
            });
        // 窗口 X / ESC 关闭 = 取消：丢弃草稿、不动文档。
        // 其余帧草稿必须留在原地（此前每帧无条件 take 会把窗口
        // 一帧收掉——headless 验证抓到的真 bug）。
        let closed = !open;
        if closed {
            self.comment_edit = None;
            return;
        }
        match verdict {
            Some(0) => {
                let text = self.comment_edit.take().unwrap_or_default();
                self.apply_comment_edit(&text);
            }
            Some(1) => {
                self.comment_edit = None;
                self.apply_comment_edit("");
            }
            _ => {}
        }
    }

    /// 把编辑结果写进当前文档（保存 / 清除共用；取消不走这里）。
    fn apply_comment_edit(&mut self, text: &str) {
        let Some(meta) = self.loaded.as_mut() else {
            return;
        };
        // 借用整理：set_comment 需要棋盘的只读信息（游标 + 着法序列），
        // 而 meta 是可变借用——先把签名输入克隆出来再调用。
        let cursor = self.board.cursor();
        let records = self.board.records().to_vec();
        meta.set_comment_parts(cursor, &records, text);
        self.meta_dirty = true;
        self.notices.push(LoadNotice::Ok(if text.is_empty() {
            "已清除该局面的注释（「另存为」时从文件中去掉）。".to_owned()
        } else {
            "注释已写入当前文档（「另存为」时随棋谱写入文件）。".to_owned()
        }));
    }
}
