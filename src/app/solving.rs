//! 解题模式（阶段 22.2）：不剧透总闸、落子判定、重试本题与看答案。
//!
//! # 定位与边界（作者定的边界，实现严格遵守）
//!
//! - **题谱完全外部**：解题只是把一份已加载的参照（死活题谱）当作判定
//!   对照，本应用不内置题目、不做题库管理 / 进度记录 / 难度分级；
//! - **不自动识别死活题**：死活题与让子局在结构上无法区分（都是摆子 +
//!   可能白先），解题模式必须是用户显式打开的开关，本模块一切能力都以
//!   `solving_mode == true` 为前提；
//! - **不做引擎死活判定**：判定只对照题谱（[`crate::refbook`] 的
//!   `lookup` 口径），措辞上一律「参照对照」前缀，与引擎结论区分；
//!   绝不断言「正解」——库未标注对错，「参照中有此手」只是「在题谱
//!   收录的续法里」，不是「这手是对的」。
//!
//! # 不剧透的实现方式：总闸而非 CandidateGating 联动
//!
//! [`CandidateGating`](crate::ui::analysis::CandidateGating) 只覆盖「候选类」
//! （候选圆圈 / 幽灵子 / 候选列表），解题模式要藏的是**全部**引擎分析呈现
//! （胜率 / 目差 / 讲解 / 失误 / 局后统计 / 思考过程 / 热度图 / 策略图 /
//! 曲线 / 小棋盘 / 参照续法），覆盖面与门控正交。做成总闸：App 持有
//! `solving_mode` 一处状态，各显示层在渲染判定处各自短路（见
//! `solving_hidden` 的逐项清单与各调用点），切换即时生效、退出完全恢复，
//! 也不与门控状态互写（门控是用户自己的显示偏好，解题模式不该动它）。
//!
//! 开关状态**不持久化**：解题模式绑定「这一份参照题谱」的场景，持久化
//! 会让没开参照的日常使用莫名缺分析呈现。
//!
//! # 判定的触发与去重
//!
//! 落子本身发生在棋盘视图（`ui::board_view`）内部，App 侧无法在落子点
//! 回调；改为 `App::ui` 每帧比对局面签名（`judged_sig`）：签名变化
//! （落子 / 导航 / 切分支）且解题模式开启时判定一次。签名键控天然覆盖
//! 「悔棋后再走」与「切分支」，同一局面不重复提示刷屏。

use super::GuanqiApp;
use crate::board::{Board, Size, Stone};
use crate::refbook::LookupOutcome;
use crate::sgf::GameTree;
use crate::ui::analysis_panel::LoadNotice;

impl GuanqiApp {
    /// 切换解题模式（「分析 → 解题模式」菜单）。
    ///
    /// 开启需已加载参照（菜单入口置灰保证；快捷路径兜底再查一次）。
    /// 关闭时清「看答案」留痕与判定去重签名——下次再开是全新一局。
    pub(super) fn set_solving_mode(&mut self, on: bool) {
        if on && self.reference.is_none() {
            return;
        }
        if self.solving_mode == on {
            return;
        }
        self.solving_mode = on;
        self.answer_shown = false;
        self.judged_sig = None;
        let text = if on {
            "解题模式已开启：分析呈现已隐藏（不剧透），落子后将按参照题谱给出对照判定。"
        } else {
            "解题模式已关闭：分析呈现恢复。"
        };
        self.notices.push(LoadNotice::Ok(text.to_owned()));
    }

    // ---- 判定（客观口径：只对照题谱，不断言死活）----

    /// 每帧调用（`App::ui` 尾部）：解题模式下局面签名变化即判定一次。
    ///
    /// 去重键 = 根到当前节点的着法签名：同一局面只提示一次；悔棋后再走
    /// 同一手、切分支走到同局面都不重复刷屏。判定只在「游标处于活子
    /// 位置」时给出（回看历史局面不判定——那不是用户的解题着法）。
    /// **空盘不判定**：空盘没有「落子判定」语义（如新对局前 / 重试前），
    /// 报「不在题谱中」只会是噪声。**索引为空不判定**（题谱重放全被
    /// 舍弃等）：此时任何局面都查不到，报「不在题谱中」是误导——排查
    /// 线索由「参照」/「解题」卡片的「索引为空」提示承担。
    pub(super) fn judge_if_needed(&mut self) {
        if !self.solving_mode {
            return;
        }
        let sig = self.board.records();
        // 签名未变（含无参照 / 游标回看等判定条件不满足的情形）：不重复判定。
        if self.judged_sig.as_ref().is_some_and(|s| s.eq(sig)) {
            return;
        }
        // 回看历史局面（不在活子位置）：记为已处理但不判定，避免每帧重查。
        if self.board.cursor() != self.board.line_len() {
            self.judged_sig = Some(sig.to_vec());
            return;
        }
        // 空盘（无任何着法）：不判定，只记签名。
        if self.board.cursor() == 0 {
            self.judged_sig = Some(sig.to_vec());
            return;
        }
        let Some(reference) = self.reference.as_ref() else {
            return;
        };
        // 索引为空：判定口径整体不可用，不产生「不在题谱中」的噪声提示。
        if reference.entry_count() == 0 {
            return;
        }
        self.judged_sig = Some(sig.to_vec());
        let notice = judge_text(reference, &self.board);
        // 每帧签名去重已保证同局面只判一次；NoticeFeed 自身的同文去重
        // 只对队尾相邻生效，这里互为双保险。
        self.notices.push(notice);
    }

    // ---- 重试本题（恢复到题谱初始，含摆子）----

    /// 重试本题：整体重建棋盘到**题谱的根局面**（摆子 + 首着方照题谱）。
    ///
    /// 死活题几乎都有 ⚫⚪ 摆子，只清着法会变成空盘——必须连同摆子一起
    /// 恢复。数据源 = 参照树根节点（`AB` / `AW` / `AE` / `SZ` / `PL`），
    /// 与参照索引的重放口径（`refbook::walk::apply_root_setup`）同一套
    /// 属性读取。当前棋盘的着法（用户试过的与走偏的）全部随整体替换
    /// 丢弃；「看答案」留痕一并清除（新的一局）。
    ///
    /// 参照与当前棋盘尺寸不符时拒绝重试（坐标会越界 / 局面必然对不上）。
    /// 不做未保存确认：解题过程本来就是对题谱的随手试错（沙盘语义），
    /// 且「重试」本身就是要丢弃这些试错，弹确认只会打断高频动作。
    pub(super) fn retry_problem(&mut self) {
        if !self.solving_mode {
            return;
        }
        let Some(reference) = self.reference.as_ref() else {
            return;
        };
        if reference.size() != self.board.size() {
            self.notices.push(LoadNotice::Failed(format!(
                "重试失败：题谱为 {} 路而当前棋盘为 {} 路，尺寸不符。",
                reference.size(),
                self.board.size()
            )));
            return;
        }
        let Some(board) = board_from_reference_root(reference.tree(), reference.size()) else {
            self.notices.push(LoadNotice::Failed(
                "重试失败：题谱缺少根节点，无法恢复初始局面。".to_owned(),
            ));
            return;
        };
        self.analysis.reset();
        self.analysis.clear_limits();
        self.clear_board_derived();
        self.board = board;
        self.tree_epoch = self.tree_epoch.wrapping_add(1);
        self.answer_shown = false;
        self.judged_sig = None;
        self.notices
            .push(LoadNotice::Ok("已重试本题：回到题谱初始局面（含摆子）。".to_owned()));
    }

    // ---- 看答案（显式动作；不自动退出解题模式）----

    /// 显示参照续法（「看答案」）：置留痕标志，侧栏「解题」卡片随之
    /// 显示当前局面的题谱续法与注释。**不自动退出解题模式**——看
    /// 之后继续解还是退出由用户决定；留痕「已查看答案（本局）」随
    /// 本局一直展示（重试 / 关闭解题模式才清除）。
    pub(super) fn show_answer(&mut self) {
        if !self.solving_mode || self.answer_shown {
            return;
        }
        self.answer_shown = true;
        self.notices.push(LoadNotice::Warn(
            "已显示参照续法（已查看答案，本局留痕；解题模式仍开启）。".to_owned(),
        ));
    }

    /// 侧栏「解题」卡片的展示数据（`None` = 未开解题模式，卡片不渲染）。
    ///
    /// 题面（当前局面在题谱中的注释）与判定 / 答案的展示口径集中在这
    /// 一个派生函数里，侧栏只负责渲染；常规模式的「参照」卡片不受影响
    /// （解题模式下它的续法 / 偏离隐藏，见 `reference_view`）。
    pub(super) fn solving_view(&self) -> Option<crate::ui::analysis_panel::SolvingView> {
        if !self.solving_mode {
            return None;
        }
        let reference = self.reference.as_ref()?;
        // 当前局面的题谱命中（题面注释 / 续法）；Miss 时题面无从谈起。
        let hit = match reference.lookup(&self.board) {
            LookupOutcome::Hit { entry, .. } => Some(crate::ui::analysis_panel::SolvingHit {
                move_no: entry.move_no,
                // 题面常写在参照节点注释里（如「黑先活」）。
                comment: entry.comment.clone(),
                continuations: entry
                    .continuations
                    .iter()
                    .map(crate::refbook::Continuation::text)
                    .collect(),
            }),
            _ => None,
        };
        // 看答案后：无论之前命中与否，给出当前局面的题谱续法（答案）。
        // 命中条目与上面同一口径（同一局面查同一张表，结果一致）。
        let answer = if self.answer_shown {
            match reference.lookup(&self.board) {
                LookupOutcome::Hit { entry, .. } => {
                    Some(crate::ui::analysis_panel::SolvingAnswer {
                        move_no: entry.move_no,
                        continuations: entry
                            .continuations
                            .iter()
                            .map(crate::refbook::Continuation::text)
                            .collect(),
                        comment: entry.comment.clone(),
                    })
                }
                _ => Some(crate::ui::analysis_panel::SolvingAnswer {
                    move_no: 0,
                    continuations: Vec::new(),
                    comment: None,
                }),
            }
        } else {
            None
        };
        Some(crate::ui::analysis_panel::SolvingView {
            name: reference.file_name(),
            size: reference.size().n(),
            entries: reference.entry_count(),
            // 索引为空时的排查线索（分支舍弃原因），随卡片展示。
            warnings: reference.warnings().iter().map(str::to_owned).collect(),
            hit,
            answer,
            answer_shown: self.answer_shown,
        })
    }
}

/// 由参照树根局面构造初始棋盘（重试本题的落点）。
///
/// 摆子（`AB` / `AW` / `AE`）与首着方（`PL`；缺省按 FF4 惯例——有摆子
/// 白先）与 `refbook::walk` 的重放口径一致：同一份题谱，索引怎么重放、
/// 重试就怎么恢复，两侧局面必然对齐。
fn board_from_reference_root(tree: &GameTree, size: Size) -> Option<Board> {
    let root = tree.root()?;
    // 摆子坐标解析失败（越界 / 黑白同点）时按失败处理：重试是替换动作，
    // 拿半对的局面替换更糟，宁可拒绝。
    let put = |ident: &str| -> Result<Vec<crate::board::Coord>, String> {
        root.coords(ident, size)
            .map_err(|err| format!("题谱根节点 {ident} 无法解析：{err}"))
    };
    let black = put("AB");
    let white = put("AW");
    let removed = put("AE");
    let (black, white, removed) = match (black, white, removed) {
        (Ok(b), Ok(w), Ok(r)) => (b, w, r),
        (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => {
            eprintln!("观棋：重试本题失败：{err}");
            return None;
        }
    };
    let to_play = match root.text("PL").map(str::trim) {
        Some("b") | Some("B") => Stone::Black,
        Some("w") | Some("W") => Stone::White,
        _ => {
            if black.is_empty() && white.is_empty() {
                Stone::Black
            } else {
                Stone::White
            }
        }
    };
    Board::from_setup(size, &black, &white, &removed, to_play).ok()
}

/// 一次落子判定的用户提示文本（客观口径，参照对照而非引擎结论）。
///
/// 口径逐项（与任务书对齐）：
/// - 父局面命中且该手在库内续法中 → 「参照中有此手」（不写「正解」）；
/// - 父局面命中但该手不在续法中（`Deviation`）→ 「参照中无此手」并
///   列出库中续法；
/// - 当前局面命中但该节点**无续法**（题谱写到这里就结束了 / 线性谱
///   无分支）→ 「题谱此处无后续，无法判定」——如实说明，不猜；
/// - `Miss`（题谱里找不到对应局面）→ 「当前局面不在题谱中」；
/// - `SizeMismatch` → 尺寸说明（理论上重试已挡，防御保留）。
fn judge_text(reference: &crate::refbook::Reference, board: &Board) -> LoadNotice {
    let prefix = "参照对照：";
    match reference.lookup(board) {
        LookupOutcome::Hit { entry, .. } => {
            if entry.continuations.is_empty() {
                // 题谱此局面之后没有收录任何续法：判定无从谈起，如实说明。
                LoadNotice::Warn(format!(
                    "{prefix}题谱此处（第 {} 手）无后续，无法判定。",
                    entry.move_no
                ))
            } else {
                // 该手 ∈ 父局面续法（当前局面命中即父局面续法之一达成）。
                // 措辞刻意不写「正解」：库未标注对错。
                LoadNotice::Ok(format!(
                    "{prefix}参照中有此手（题谱第 {} 手处）。继续。",
                    entry.move_no
                ))
            }
        }
        LookupOutcome::Deviation(deviation) => {
            if deviation.book.is_empty() {
                // 父局面在题谱里就没有续法（线性谱叶子 / 题谱写到这里
                // 结束）：任何后续着法都谈不上「偏离」，如实说明不猜。
                LoadNotice::Warn(format!(
                    "{prefix}题谱此处无后续，无法判定（刚走的 {} 已在题谱收录范围之外）。",
                    deviation.played
                ))
            } else {
                LoadNotice::Warn(format!(
                    "{prefix}参照中无此手（{played}）；题谱此局面走：{}。",
                    deviation.book.join(" / "),
                    played = deviation.played,
                ))
            }
        }
        LookupOutcome::Miss => {
            LoadNotice::Warn(format!("{prefix}当前局面不在题谱中（可能已走出题谱范围）。"))
        }
        LookupOutcome::SizeMismatch {
            reference: r_size,
            current: c_size,
        } => LoadNotice::Failed(format!(
            "{prefix}题谱为 {r_size} 路而当前棋盘为 {c_size} 路，无法对照。"
        )),
    }
}

// ---- 取证钩子（headless 驱动；与菜单同一执行体）----

impl GuanqiApp {
    /// 开关解题模式（headless 验证动作落点；菜单「解题模式」同一执行体）。
    #[doc(hidden)]
    pub fn set_solving_mode_pub(&mut self, on: bool) {
        self.set_solving_mode(on);
    }

    /// 重试本题（headless 验证动作落点；菜单「重试本题」同一执行体）。
    #[doc(hidden)]
    pub fn retry_problem_pub(&mut self) {
        self.retry_problem();
    }

    /// 看答案（headless 验证动作落点；菜单「显示参照续法」同一执行体）。
    #[doc(hidden)]
    pub fn show_answer_pub(&mut self) {
        self.show_answer();
    }

    /// 最近一次判定提示文本（headless 取证：notices 之外的定向读取，
    /// 方便脚本逐项断言判定口径；应用路径不调用）。
    #[doc(hidden)]
    pub fn last_judge_notice(&self) -> Option<String> {
        // NoticeFeed 只有正向迭代；收集后反向找最近一条（队列 ≤ 3 条，开销可忽略）。
        let all: Vec<&LoadNotice> = self.notices.iter().collect();
        all.into_iter()
            .rev()
            .find(|n| n.text().starts_with("参照对照："))
            .map(|n| n.text().to_owned())
    }

    /// 最近一帧棋盘区域的只读克隆（headless 工具 GTP → 点击坐标换算
    /// 用；应用语义不读取）。
    #[doc(hidden)]
    pub fn board_area_probe(&self) -> Option<egui::Rect> {
        *self.board_area.borrow()
    }
}
