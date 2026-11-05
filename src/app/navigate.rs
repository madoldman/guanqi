//! 复盘导航补全（竞品移植第三批 P1，作者 2026-11-05 无人值守时段指示）。
//!
//! 来源与口径：
//! - **下一处 / 上一处失误**（KaTrain `n` / `Shift+n` 的移植）：在
//!   「疑问手及以上」（[`Severity::is_marked`]，与棋盘失误标注同一
//!   口径）的失误之间跳转。数据 = [`AnalysisState::marked_losses`]——
//!   只认**当前线、已分析**的手（两端历史点齐全才可算损失），缺口
//!   不臆造；因此能跳多远取决于浏览 / 快扫积累的分析数据，跳不到
//!   未分析的手是如实行为而非缺陷（提示里说明）。
//! - **跳到手数**（Sabaki 的 jump-to-move）：输入 0..=线长的手数直接
//!   跳转，`0` = 开局盘面、`N` = 第 N 手之后的盘面，与局后统计排行榜
//!   点击的 `go_to` 同一口径（`PanelAction::GotoTurn` 同款）。
//!
//! 键位归属：`N` / `Shift+N` 在 App 层处理（要读分析数据，见
//! `App::ui` 的按键段），`Home` / `End` / `Shift+←/→` 在棋盘层
//! （`ui::board_view::handle_keyboard`，与裸箭头同源、同守卫）。

use super::GuanqiApp;
use crate::ui::{analysis_panel::LoadNotice, theme};

impl GuanqiApp {
    /// 跳到下一处（`forward`）或上一处失误。
    ///
    /// 「下一处」= 当前游标之后（不含）最小的失误手数；「上一处」=
    /// 游标之前（不含）最大的失误手数——停在一处失误上再按同向键
    /// 不会原地打转。跳转后局面变化由既有 sync 检测并自动重发查询，
    /// 曲线 / 失误标注 / 树面板随游标联动（与任何导航一致）。
    pub(super) fn goto_mistake(&mut self, forward: bool) {
        let losses = self.analysis.marked_losses(&self.board);
        let cursor = self.board.cursor();
        let target = if forward {
            losses.iter().find(|loss| loss.turn > cursor)
        } else {
            losses.iter().rev().find(|loss| loss.turn < cursor)
        };
        let Some(loss) = target else {
            self.notices.push(LoadNotice::Warn(if forward {
                "当前线后面没有已分析的疑问手及以上失误。\
                 分析数据随浏览 / 整谱快扫积累，未分析的手无从判定。"
            } else {
                "当前线前面没有已分析的疑问手及以上失误。\
                 分析数据随浏览 / 整谱快扫积累，未分析的手无从判定。"
            }
            .to_owned()));
            return;
        };
        self.board.go_to(loss.turn);
        self.notices.push(LoadNotice::Ok(format!(
            "已跳到第 {} 手：{}方损失 {:.1} 目（{}）",
            loss.turn,
            loss.player.name(),
            loss.score_loss,
            loss.severity.name(),
        )));
    }

    /// 「跳到手数…」对话框（每帧调用，`goto_open` 打开时绘制）。
    ///
    /// 形态遵循表单规范：`theme::param_row` 参数行 + `theme::action_row`
    /// 动作条；范围钳制到当前线长度（切分支 / 换谱后超界值由 range
    /// 拒绝输入，已超界的存量值由「跳转」入口置灰兜底）。
    pub(super) fn goto_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.goto_open;
        let mut jump: Option<usize> = None;
        let mut cancel = false;
        egui::Window::new("跳到手数")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                let max = self.board.line_len();
                ui.label(format!(
                    "当前线共 {max} 手。0 = 开局盘面，N = 第 N 手之后的盘面。"
                ));
                theme::param_row(ui, |ui| {
                    ui.label("手数");
                    ui.add(
                        egui::DragValue::new(&mut self.goto_target)
                            .range(0..=max)
                            .suffix(" 手"),
                    );
                });
                let valid = self.goto_target <= max;
                if let Some(index) = theme::action_row(
                    ui,
                    &[
                        theme::Action { label: "跳转", primary: true, enabled: valid },
                        theme::Action { label: "取消", primary: false, enabled: true },
                    ],
                ) {
                    match index {
                        0 if valid => jump = Some(self.goto_target),
                        _ => cancel = true,
                    }
                }
            });
        self.goto_open = open && !cancel;
        if let Some(target) = jump {
            self.board.go_to(target);
            self.goto_open = false;
        }
    }
}
