//! SGF 集合（多局文件）的「选择要载入的一局」窗口（竞品移植第四批 Q1）。
//!
//! SGF 集合 = 一个文件多棵顶层游戏树（职业棋谱合集常见）。此前
//! [`parse_str`](crate::sgf::parse_str) 遇集合整体报错（真缺陷）；
//! 现在解析全部对局后弹本窗口，用户点选哪局就载入哪局。确认链
//! （副本研究 / 未保存内容）在打开文件 / 拖放时已经走完，这里的选择
//! 只是「载入哪一份」，不再二次确认；取消 = 不载入任何一局。
//!
//! 形态：列表行规范（`top_down_justified(LEFT)` 全宽左对齐按钮行）；
//! 局数可能很多 → `Window::vscroll(true)` + `max_height` 双限高
//! （0.1.0-12 钉过的规则：Window 内建滚动，绝不再包一层 ScrollArea）。

use super::GuanqiApp;
use crate::ui::theme;

impl GuanqiApp {
    /// 集合选择窗（每帧调用，`collection` 非空时绘制）。
    pub(super) fn collection_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.collection.is_some();
        let mut pick: Option<usize> = None;
        let mut cancel = false;
        let total = self.collection.as_ref().map_or(0, |(_, games)| games.len());
        egui::Window::new("选择要载入的一局")
            .open(&mut open)
            .resizable(false)
            .vscroll(true)
            .max_height(420.0)
            .show(ctx, |ui| {
                let Some((path, games)) = self.collection.as_ref() else {
                    return;
                };
                ui.label(format!("{} 含 {} 局，点选要载入的一局：", path.display(), total));
                ui.add_space(4.0);
                for (i, game) in games.iter().enumerate() {
                    let info = &game.meta.info;
                    let label = format!(
                        "第 {} 局 · {} vs {} · {} · {} 手",
                        i + 1,
                        info.player_black.as_deref().unwrap_or("?"),
                        info.player_white.as_deref().unwrap_or("?"),
                        info.result.as_deref().unwrap_or("结果未记"),
                        game.board.move_count(),
                    );
                    // 列表行规范（形态规范 3）：全宽 + 文字左对齐是语义
                    // 要求，justified 拉满宽（不走 min_size）。
                    let hit = ui
                        .with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                            ui.button(label).clicked()
                        })
                        .inner;
                    if hit {
                        pick = Some(i);
                    }
                }
                ui.add_space(4.0);
                if let Some(0) = theme::action_row(
                    ui,
                    &[theme::Action { label: "取消", primary: false, enabled: true }],
                ) {
                    cancel = true;
                }
            });
        if cancel || !open {
            self.collection = None;
        }
        if let (Some(i), Some((_, games))) = (pick, self.collection.take()) {
            let index = i + 1;
            let loaded = games.into_iter().nth(i).expect("pick 下标来自本列表");
            self.apply_loaded(loaded, Some((index, total)));
        }
    }
}
