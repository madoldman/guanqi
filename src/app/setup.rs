//! 摆子开局模式（竞品移植第四批 Q5，katsura 中盘摆子编辑 / Hoshi
//! 「右键编辑摆子」的最小可用版）。
//!
//! # 定位与流程
//!
//! 「对局 → 摆子开局…」：确认链（副本研究 / 未保存内容，与新对局同一
//! 套）→ **整体替换为空盘文档**并进入摆子模式 → 在棋盘上点击放子
//! （黑笔 / 白笔 / 擦除三种笔，点已有子即替换 / 清除）→ 窗口里选
//! 先行方 → 「完成摆子」退出，引擎按摆子局面分析（`initialStones`
//! 随查询发送，通路自 2026-09-28 的摆子修复起就绪）。
//!
//! # 边界
//!
//! - **摆子 ≠ 着手**：改动只落在根盘面（[`Board::setup_place`] 系列，
//!   AB/AW/PL 语义），不进棋谱树、手数恒 0；
//! - 摆子模式中棋盘点击被拦截（一律是摆子动作，绝不落子建分支）；
//! - 「进入」不可撤销（进入前的确认已声明替换）——「退出摆子模式」
//!   保留当前摆子，不提供「还原进入前局面」（要回去：载谱 / 新对局）；
//! - 完成后文档脏（`record_rev` 已随每次摆子递增），另存写 AB/AW/PL
//!   （`build_root` 从根盘面重建，既有通路）。

use super::{PendingConfirm, UnsavedConfirm, DEFAULT_KOMI, GuanqiApp};
use crate::board::Stone;
use crate::sgf::GameMeta;
use crate::ui::analysis_panel::LoadNotice;
use crate::ui::theme;
use std::path::Path;

/// 摆子模式的「笔」：棋盘点击的语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupPen {
    /// 黑笔：放 / 换黑子。
    Black,
    /// 白笔：放 / 换白子。
    White,
    /// 擦除：清除该点。
    Erase,
}

impl SetupPen {
    /// 笔名（窗口 radio 与 dump 取证用）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::White => "white",
            Self::Erase => "erase",
        }
    }
}

impl GuanqiApp {
    /// 「摆子开局…」入口：破坏性替换先走确认链（与新对局同一套语义）。
    pub(super) fn request_setup_start(&mut self) {
        if self.play.mode {
            self.notices.push(LoadNotice::Warn(
                "对弈模式下不可摆子开局：先结束对局（认输 / 弃着终局 / 关闭人机对弈）。".to_owned(),
            ));
            return;
        }
        if self.research_moves() > 0 {
            self.pending_confirm = PendingConfirm::SetupStart;
            return;
        }
        if self.is_dirty() {
            self.unsaved_confirm = Some(UnsavedConfirm::SetupStart);
            return;
        }
        self.start_setup_mode();
    }

    /// 执行替换：整体换成空盘文档并进入摆子模式。
    ///
    /// 替换序列与 [`Self::apply_loaded`] / `start_new_game` 同一套语义
    /// （分析复位、副本清空、基准对齐、树换代）；三处各自有少量差异
    /// （元信息构造 / 提示文案 / 对局态），刻意不复用同一条函数——
    /// 三条路径都已各自验证，不为求同而引入行为漂移。
    pub(super) fn start_setup_mode(&mut self) {
        let size = self.board.size();
        let board = crate::board::Board::new(size);
        let meta = GameMeta::for_path(Path::new("guanqi.sgf"), size);
        self.komi = DEFAULT_KOMI;
        self.active_rules = None;
        self.analysis.reset();
        self.analysis.clear_limits();
        self.clear_board_derived();
        let dropped = self.research_moves();
        self.others.clear();
        self.active_from_move = None;
        self.active_number = 0;
        self.pending_confirm = PendingConfirm::None;
        self.unsaved_confirm = None;
        self.board = board;
        self.tree_epoch = self.tree_epoch.wrapping_add(1);
        self.mark_saved();
        self.loaded = Some(meta);
        // 摆子盘不是对局（入口已禁对弈态，这里兜底；时钟等对局态由
        // play.mode = false 一并冻结）。
        self.play.mode = false;
        self.setup_mode = Some(SetupPen::Black);
        let notice_text =
            "摆子模式已开启：点击棋盘放子（黑笔），窗口可换笔 / 擦除 / 选先行方；\
             「完成摆子」后引擎按摆子局面分析。"
                .to_owned();
        if dropped > 0 {
            self.notices.push(LoadNotice::Warn(format!(
                "{notice_text}（研究副本连同 {dropped} 手研究成果已丢弃。）"
            )));
        } else {
            self.notices.push(LoadNotice::Ok(notice_text));
        }
    }

    /// 「完成摆子」：退出摆子模式，引擎按摆子局面分析。
    pub(super) fn finish_setup_mode(&mut self) {
        let Some(_pen) = self.setup_mode.take() else {
            return;
        };
        let (black, white) = self.board.root_setup();
        let to_play = self.board.root_to_play();
        let text = if black.is_empty() && white.is_empty() {
            "摆子模式已退出：盘面为空（与普通空盘无异）。".to_owned()
        } else {
            format!(
                "摆子完成：黑 {} 子 / 白 {} 子，{}先。\
                 引擎将按摆子局面分析（摆子随查询发送）。",
                black.len(),
                white.len(),
                to_play.name(),
            )
        };
        self.notices.push(LoadNotice::Ok(text));
    }

    /// 摆子窗口（每帧调用，摆子模式进行中绘制）：笔 / 先行方 / 计数 /
    /// 完成按钮。棋盘点击在棋盘层拦截（`ui::board_view`），窗口只管
    /// 模式控制与状态展示。
    pub(super) fn setup_dialog(&mut self, ctx: &egui::Context) {
        let Some(pen) = self.setup_mode else {
            return;
        };
        let mut open = true;
        let mut finish = false;
        egui::Window::new("摆子开局")
            .open(&mut open)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [260.0, 40.0])
            .show(ctx, |ui| {
                ui.weak(
                    "点击棋盘放 / 换子（同点换色），擦除笔清点。\
                     摆子不是着手：手数恒 0，另存时写 AB/AW。",
                );
                ui.add_space(4.0);
                // ---- 笔（三选一）----
                ui.horizontal(|ui| {
                    ui.label("笔：");
                    for (candidate, label) in [
                        (SetupPen::Black, "黑笔"),
                        (SetupPen::White, "白笔"),
                        (SetupPen::Erase, "擦除"),
                    ] {
                        if ui
                            .selectable_label(self.setup_mode == Some(candidate), label)
                            .clicked()
                        {
                            self.setup_mode = Some(candidate);
                        }
                    }
                });
                // ---- 先行方（即时生效：PL 语义）----
                ui.horizontal(|ui| {
                    ui.label("先行方：");
                    for (stone, label) in [(Stone::Black, "黑先"), (Stone::White, "白先")] {
                        if ui
                            .selectable_label(self.board.root_to_play() == stone, label)
                            .clicked()
                        {
                            self.board.setup_set_to_play(stone);
                        }
                    }
                });
                // ---- 计数（root_setup 实时反推）----
                let (black, white) = self.board.root_setup();
                ui.weak(format!("当前：黑 {} 子 / 白 {} 子。", black.len(), white.len()));
                ui.add_space(4.0);
                if let Some(0) = theme::action_row(
                    ui,
                    &[theme::Action { label: "完成摆子", primary: true, enabled: true }],
                ) {
                    finish = true;
                }
            });
        // X / ESC 关闭 = 退出摆子模式（保留当前摆子，同「完成」；
        // 进入前的替换不可撤销，见模块文档边界）。
        if finish || !open {
            self.finish_setup_mode();
        }
        let _ = pen; // 笔只读棋盘层使用；窗口内由 setup_mode 直接判定
    }
}
