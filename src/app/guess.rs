//! 猜下一手模式（竞品移植第三批 P3，Sabaki guess mode 的移植）。
//!
//! # 定位与语义
//!
//! 载入一份棋谱后**遮住后续着手**，用户自己点出棋谱的每一手：猜对
//! 前进（导航进已有节点），猜错不落子。是「看谱」到「读谱」之间的一档
//! 练习——与本应用已有的解题模式（对照外部题谱）同属学习类，因此
//! 「不剧透」复用同一总闸口径：开启时所有分析呈现按
//! [`GuanqiApp::study_hidden`] 短路（候选 / 胜率 / 目差 / 讲解 / 失误 /
//! 局后统计 / 思考过程 / 热度图 / 策略图 / 曲线 / 小棋盘）。
//!
//! # 边界（实现严格遵守）
//!
//! - **棋谱绝不被改动**：点击判定拦截在棋盘层（`ui::board_view` 的
//!   `guess_click`）——猜对是 `go_to_node` 跳进**已有**节点（预览式，
//!   与「沿主变前进」同一语义），猜错不落子；Ctrl+Z 悔棋在猜谱中
//!   被拦（悔棋会摘节点 = 改谱）；照库落子在 App 侧守卫拒绝。
//! - **与解题模式互斥**：两个学习模式都开只会互相混淆（一个对照
//!   题谱、一个对照本谱），入口置灰 + `set_solving_mode` 反向守卫。
//! - **不持久化**：绑定「这一份棋谱」的使用场景（同解题模式）。
//! - **换盘即退出**：载谱 / 新对局 / 切副本后猜的对象已变
//!   （`clear_board_derived` 钩子，同自动播放）。
//! - 手数标注（P4）在本模式下**可用**且很有用——它展示已猜过部分的
//!   手数，不剧透后续着手。
//!
//! # 判定的「正确答案」口径
//!
//! 当前节点**第一个子分支**的首手 = SGF 载入时挂载主变的那一手
//! （每层首子树延续主变，见 5.4 谱树改造）。用户在变着上猜谱时，
//! 答案跟着当前线走；Ctrl+←/→ 切分支后猜的就是新线的下一手。
//! 弃着用 P 键猜（点盘面必然落空）。

use super::GuanqiApp;
use crate::board::Action;
use crate::ui::analysis_panel::LoadNotice;

impl GuanqiApp {
    /// 学习类模式共用的「不剧透」总闸：解题 / 猜下一手任一开启即隐藏
    /// 全部分析呈现。两模式语义同源——藏的就是「答案」（解题藏题谱
    /// 续法的引擎对照，猜谱藏棋谱后续着手的引擎对照）；各显示层的
    /// 短路判定继续收这一个布尔（原 `solving` 参数，含义扩为学习类）。
    pub(super) fn study_hidden(&self) -> bool {
        self.solving_mode || self.guess_mode
    }

    /// 切换猜下一手模式（「分析 → 猜下一手」菜单）。
    ///
    /// 开启条件（菜单入口置灰保证，快捷路径兜底再查）：已载入有着手的
    /// 棋谱、不在对弈模式、不在解题模式。关闭时无需清理——判定全在
    /// 点击路径上，棋谱本身从未被改动。
    pub(super) fn set_guess_mode(&mut self, on: bool) {
        if self.guess_mode == on {
            return;
        }
        if on && (self.play.mode || self.solving_mode || self.board.line_len() == 0) {
            return;
        }
        self.guess_mode = on;
        let text = if on {
            "猜下一手已开启：分析呈现已隐藏。点击你认为的下一手——猜对前进，\
             猜错不落子（棋谱不会改动）；下一手是弃着时按 P 猜。"
        } else {
            "猜下一手已关闭：分析呈现恢复。"
        };
        self.notices.push(LoadNotice::Ok(text.to_owned()));
    }

    /// P 键猜弃着（棋盘层管不了键盘，App 层判定；与 `guess_click`
    /// 同一口径的弃着分支）。
    pub(super) fn guess_pass(&mut self) {
        if !self.guess_mode {
            return;
        }
        let Some(expected) = self.board.child_move(0).map(|record| record.action) else {
            self.branch_notice =
                Some("已到当前线末尾：棋谱没有更多着手了。".to_owned());
            return;
        };
        match expected {
            Action::Pass => {
                let child = self.board.child_id(0).expect("child_move 有值则 child_id 必有");
                self.board.go_to_node(child);
                self.branch_notice = (self.board.cursor() == self.board.line_len())
                    .then(|| "已猜完当前线的全部着手。".to_owned());
            }
            Action::Place(_) => {
                self.branch_notice = Some("下一手不是弃着：在棋盘上点出你认为的落点。".to_owned());
            }
        }
    }
}
