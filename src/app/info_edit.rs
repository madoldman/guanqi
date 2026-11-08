//! 对局信息编辑窗（竞品移植第四批 Q3，katsura Game Info / Sabaki 的移植）。
//!
//! 只开放**纯元数据**文本字段（PB / PW / BR / WR / RE / DT / EV / RO /
//! PC / GN）：改完即随「另存为」写入 SGF，不碰任何分析口径。
//! **刻意不含 KM（贴目）**：载入谱的贴目参与分析（查询 komi 取谱上
//! KM），改动它要作废逐手历史并重发查询（PDA 变更同款 data_bias），
//! 与「纯元数据随手改」的窗口定位不符——要做须单独立项带口径迁移。
//!
//! 形态：`Window::vscroll(true)` + `max_height` 双限高；每行
//! `theme::param_row`（标签 + 单行输入吃剩余宽）；首个字段打开即聚焦
//! （与注释编辑窗同一体验）。空串 = 删除该属性（保存端不写）。

use super::GuanqiApp;
use crate::sgf::GameInfo;
use crate::ui::analysis_panel::LoadNotice;
use crate::ui::theme;

/// 编辑草稿：全部为「未编辑时的原值文本」，空串 = 无 / 删除。
#[derive(Default)]
pub(super) struct InfoDraft {
    player_black: String,
    player_white: String,
    rank_black: String,
    rank_white: String,
    result: String,
    date: String,
    event: String,
    round: String,
    place: String,
    game_name: String,
}

impl InfoDraft {
    fn from_info(info: &GameInfo) -> Self {
        Self {
            player_black: info.player_black.clone().unwrap_or_default(),
            player_white: info.player_white.clone().unwrap_or_default(),
            rank_black: info.rank_black.clone().unwrap_or_default(),
            rank_white: info.rank_white.clone().unwrap_or_default(),
            result: info.result.clone().unwrap_or_default(),
            date: info.date.clone().unwrap_or_default(),
            event: info.event.clone().unwrap_or_default(),
            round: info.round.clone().unwrap_or_default(),
            place: info.place.clone().unwrap_or_default(),
            game_name: info.game_name.clone().unwrap_or_default(),
        }
    }

    fn apply(&self, info: &mut GameInfo) {
        info.player_black = trimmed_or_none(&self.player_black);
        info.player_white = trimmed_or_none(&self.player_white);
        info.rank_black = trimmed_or_none(&self.rank_black);
        info.rank_white = trimmed_or_none(&self.rank_white);
        info.result = trimmed_or_none(&self.result);
        info.date = trimmed_or_none(&self.date);
        info.event = trimmed_or_none(&self.event);
        info.round = trimmed_or_none(&self.round);
        info.place = trimmed_or_none(&self.place);
        info.game_name = trimmed_or_none(&self.game_name);
    }
}

/// 文本字段的统一口径：去首尾空白，空 = `None`（该属性删除，保存端
/// `Option` 为空即不写）。
fn trimmed_or_none(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

impl GuanqiApp {
    /// 打开编辑窗：草稿 = 当前对局信息。
    pub(super) fn open_info_editor(&mut self) {
        let Some(meta) = self.loaded.as_ref() else {
            return;
        };
        self.info_edit = Some((InfoDraft::from_info(&meta.info), true));
    }

    /// 编辑窗（每帧调用，`info_edit` 非空时绘制）。第二个槽位 =
    /// 「首个字段待聚焦」标记（打开后首个渲染帧消耗，见注释编辑窗）。
    pub(super) fn info_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.info_edit.is_some();
        let mut save = false;
        // 形态规范 5 的双限高（vscroll Window 内动作行会跨出窗框、点击
        // 落到下层棋盘——headless 实证）：内层 ScrollArea 限高滚动，
        // 动作条在滚动区**外**、窗口体内，恒可见可点。
        egui::Window::new("编辑对局信息")
            .open(&mut open)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .max_height(560.0)
            .show(ctx, |ui| {
                let Some((draft, focus_pending)) = self.info_edit.as_mut() else {
                    return;
                };
                ui.weak(
                    "纯元数据，随「另存为」写入 SGF；留空 = 删除该属性。\
                     贴目（KM）不在此改：它参与分析口径。",
                );
                ui.add_space(4.0);
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    let mut first = true;
                    for (label, field) in [
                        ("黑方", &mut draft.player_black),
                        ("白方", &mut draft.player_white),
                        ("黑方段位", &mut draft.rank_black),
                        ("白方段位", &mut draft.rank_white),
                        ("结果", &mut draft.result),
                        ("日期", &mut draft.date),
                        ("赛事", &mut draft.event),
                        ("轮次", &mut draft.round),
                        ("地点", &mut draft.place),
                        ("对局名", &mut draft.game_name),
                    ] {
                        theme::param_row(ui, |ui| {
                            ui.label(label);
                            let response = egui::TextEdit::singleline(field).desired_width(
                                (ui.available_width() - 76.0).max(120.0),
                            );
                            let out = ui.add(response);
                            if first && *focus_pending {
                                out.request_focus();
                                *focus_pending = false;
                            }
                        });
                        first = false;
                    }
                });
                ui.add_space(4.0);
                if let Some(0) = theme::action_row(
                    ui,
                    &[
                        theme::Action { label: "保存", primary: true, enabled: true },
                        theme::Action { label: "取消", primary: false, enabled: true },
                    ],
                ) {
                    save = true;
                }
            });
        // X / ESC 关闭 = 取消：丢弃草稿。
        if !open {
            self.info_edit = None;
            return;
        }
        if save
            && let Some((draft, _)) = self.info_edit.take()
            && let Some(meta) = self.loaded.as_mut()
        {
            draft.apply(&mut meta.info);
            self.meta_dirty = true;
            self.notices.push(LoadNotice::Ok(
                "对局信息已写入当前文档（「另存为」时随棋谱写入文件）。".to_owned(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::trimmed_or_none;

    #[test]
    fn 空白归空其余保留() {
        assert_eq!(trimmed_or_none("  "), None);
        assert_eq!(trimmed_or_none(""), None);
        assert_eq!(trimmed_or_none(" 柯洁 "), Some("柯洁".to_owned()));
        assert_eq!(trimmed_or_none("B+2.5"), Some("B+2.5".to_owned()));
    }
}
