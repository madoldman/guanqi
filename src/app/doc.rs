//! 文档与研究副本：`others` 驻留列表、创建 / 切换 / 丢弃、侧栏文档
//! 条目与显示名。
//!
//! 全部从原 `app.rs` 单体拆出，逐字节照搬：互换语义（零拷贝）、稳定
//! 编号与确认文案都不变。

use super::{Doc, GuanqiApp, default_sgf_name};
use crate::play::PlayState;
use crate::ui::analysis_panel::{self, LoadNotice};

impl GuanqiApp {
// ---- 研究副本 ----

/// 按编号在 `others` 里定位文档的下标。
pub(super) fn other_index(&self, number: usize) -> Option<usize> {
    self.others.iter().position(|doc| doc.number == number)
}

/// 所有副本（含活动文档若是副本）各自超出创建前缀的研究着法总数。
/// 判据只看树规模：从既有节点上「切换分支 / 回看」不算新研究，
/// 落子与建分支（含后续整棵试验子树）都会增加全树着法数。
pub(super) fn research_moves(&self) -> usize {
    let mut total = self
        .others
        .iter()
        .filter_map(|doc| {
            doc.from_move
                .map(|from| doc.board.move_count().saturating_sub(from))
        })
        .sum::<usize>();
    if let Some(from) = self.active_from_move {
        total += self.board.move_count().saturating_sub(from);
    }
    total
}

/// 副本个数（载入 / 新对局的确认文案用；活动文档若是副本也计入）。
pub(super) fn copy_count(&self) -> usize {
    self.others
        .iter()
        .filter(|doc| doc.from_move.is_some())
        .count()
        + usize::from(self.active_from_move.is_some())
}

/// 副本另存的默认文件名：**原谱**文件名主干 + 「-副本编号」（编号使
/// 多份副本可区分，如 `xxx-副本1.sgf`）。从原谱主干取（而非活动文档
/// 的名字），避免「在副本里再开副本」时嵌套出 `-副本1-副本2`。
pub(super) fn copy_default_name(&self, number: usize) -> String {
    self.loaded
        .as_ref()
        .map(|meta| default_sgf_name(&meta.source))
        .unwrap_or_else(|| "guanqi.sgf".to_owned())
        .split_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or_else(
            || format!("研究副本{number}.sgf"),
            |(stem, ext)| format!("{stem}-副本{number}.{ext}"),
        )
}

/// 从当前手创建研究副本：预设局面 + 当前线前缀重放（独立树），元信息
/// 克隆自当前活动文档（注释按局面签名自动跟随）。当前活动文档
/// （原谱或某份副本皆可）整体推入 `others`，新副本进主槽并成为
/// 活动文档。对弈模式在此关闭：多份文档不能同时自动应手。
///
/// 副本元信息的 `source` **保持原谱真实路径**：它只是「另存为」默认名
/// 的来源，此前换成虚构的「-副本N.sgf」会让 UI 显示一个不存在的文件，
/// 用户误以为副本已存盘——如实显示来源（另存前显示原谱名，另存后
/// 由 `save_game` 更新为真实落盘路径）。
///
/// 创建副本：把当前活动文档（含其未保存状态）推入 `others` 的工作
/// 区段；副本文档自身的基准由 [`Self::switch_doc`] / 本函数对
/// `saved_rev` 的重置给出（副本树重放自原谱，修订号从 0 起算，
/// 「刚从干净内容复制而来」＝干净；此后在副本里落子才变脏）。
///
/// 前置条件（UI 已守卫，此处 assert 兜底）：已载入棋谱。
pub(super) fn create_copy(&mut self) {
    assert!(
        self.loaded.is_some(),
        "创建副本的前置条件不满足（UI 入口应已置灰）"
    );
    // 对弈中切到副本同样是「切文档」：结束对局（子项 B）。
    self.end_play_for_doc_switch();
    let number = self.next_number;
    self.next_number += 1;
    let from_move = self.board.cursor();
    let board = self.board.linear_prefix(from_move);
    // 元信息克隆：source 不改（保持来源谱的真实路径）。另存默认名
    // 单独从编号推导（见 copy_default_name），不再借道 meta.source。
    let meta = self
        .loaded
        .as_ref()
        .expect("前置条件已检查 loaded 存在")
        .clone();
    // 当前文档退入 others（带上身份），新副本进主槽。
    self.others.push(Doc {
        board: std::mem::replace(&mut self.board, board),
        meta: self.loaded.take(),
        from_move: self.active_from_move,
        number: self.active_number,
    });
    self.loaded = Some(meta);
    self.active_from_move = Some(from_move);
    self.active_number = number;
    // 新副本进主槽：它的树刚由前缀重放搭出（record_rev 从 0 起算），
    // 基准同样对齐到 0 —— 副本创建即干净，之后在副本里落子才变脏。
    self.saved_rev = self.board.record_rev();
    self.tree_epoch = self.tree_epoch.wrapping_add(1);
    self.clear_board_derived();
    let notice = LoadNotice::Ok(format!(
        "已创建研究副本 {number}（自第 {from_move} 手起），原谱保持不变；\
         当前在研究副本 {number} 中。"
    ));
    self.notices.push(notice);
}

/// 切换文档时结束进行中的对局（子项 B：任何文档互切、含切回原谱
/// 都算「离开对局现场」）：对弈模式退回复盘、清时钟与认输 / 超时
/// 进行态。**不改棋谱内容、不写 RE**——对局没有走完，胜负未判，
/// 悄悄补结果等于替用户认输；用户要继续可重新开启人机对弈。
pub(super) fn end_play_for_doc_switch(&mut self) {
    if self.play.mode {
        self.play = PlayState::review();
        self.last_frame = None;
        let notice = LoadNotice::Warn(
            "已切换文档，对局结束（未判胜负）；要继续对弈请重新开启人机对弈。".to_owned(),
        );
        self.notices.push(notice);
    }
}

/// 切换到指定编号的文档：主显示槽位与 `others` 中该项**整体互换**
/// （棋盘、元信息、身份四字段一起走，零拷贝）。
///
/// **不 `analysis.reset()`**：胜率历史按局面签名索引，同一局面跨文档
/// 共享同一曲线；切换后由既有 `sync()` 检测局面签名变化并自动发起新
/// 查询。棋盘整体替换 → `tree_epoch` 换代（树布局不复用）；旧文档的
/// 临时提示与定位高亮一并清除。
///
/// 未保存基准随文档走：每个 [`Board`] 携带自己的 `record_rev`，切入
/// 新文档时基准对齐到它的当前值（`mark_saved` 语义），各文档独立
/// 判脏——「副本里改过、切回干净的原谱」不会误报。
///
/// `swap_remove` 会让 `others` 内部顺序变化，但侧栏列表按编号排序
/// 显示（见 `doc_entries`），内部顺序不影响任何可见行为。
pub(super) fn switch_doc(&mut self, number: usize) {
    let Some(index) = self.other_index(number) else {
        return;
    };
    // 对弈中切文档（任何互切含切回原谱）即结束对局（子项 B）。
    self.end_play_for_doc_switch();
    let incoming = self.others.swap_remove(index);
    // 当前活动文档退回列表（带上身份），目标文档换进主槽。
    self.others.push(Doc {
        board: std::mem::replace(&mut self.board, incoming.board),
        meta: self.loaded.take(),
        from_move: self.active_from_move,
        number: self.active_number,
    });
    self.loaded = incoming.meta;
    self.active_from_move = incoming.from_move;
    self.active_number = incoming.number;
    // 基准对齐到新活动文档的当前修订号：每份文档各自判脏。
    self.saved_rev = self.board.record_rev();
    self.tree_epoch = self.tree_epoch.wrapping_add(1);
    self.clear_board_derived();
}

/// 「沿主变前进」的执行体（预览式语义 (a)）：只在谱上**已存在**的
/// 着法上导航。每手在当前节点的子分支里找与 PV 一致的着法（落点 +
/// 行棋方都相同，弃着比对 Pass），找不到即停——绝不新建分支，
/// `move_count()` 不变。游标停在哪算哪（用户看到棋盘走到哪）；
/// 若 PV 首手不在子分支里则完全不动。
pub(super) fn handle_advance_pv(&mut self, pv: Vec<Option<crate::board::Coord>>) {
    for step in pv {
        let target = match step {
            Some(at) => crate::board::Action::Place(at),
            None => crate::board::Action::Pass,
        };
        // 子分支着法 = 子节点记录；只认当前行棋方产生的记录，
        // 防止 PV 与实际轮转错位时误走对手的着法。
        let matched = (0..self.board.child_count()).find(|&i| {
            self.board
                .child_move(i)
                .is_some_and(|r| r.action == target && r.player == self.board.to_play())
        });
        match matched {
            Some(i) if self.board.select_child(i) => {}
            _ => break, // 谱上没有这手：预览到此为止
        }
    }
}

/// 切换到原谱（`others` 中 `from_move == None` 的那份）；已在原谱或
/// 原谱不在列表时静默不动。
pub(super) fn switch_to_original(&mut self) {
    if let Some(index) = self.others.iter().position(|doc| doc.from_move.is_none()) {
        let number = self.others[index].number;
        self.switch_doc(number);
    }
}

/// 丢弃指定编号的研究副本（研究成果由 UI 层先经确认框守卫）。
/// 丢的是当前活动的副本时先切回原谱再移除，保证主槽最终显示有效文档。
pub(super) fn drop_copy(&mut self, number: usize) {
    if self.active_number == number && self.active_from_move.is_some() {
        // 已载入棋谱时原谱必在 others（活动是副本的充要条件），
        // 切换后目标副本随互换退入列表，统一按编号移除。
        self.switch_to_original();
    }
    self.others.retain(|doc| doc.number != number);
    let count = self.copy_count();
    let notice = if count > 0 {
        format!("研究副本 {number} 已丢弃，其余 {count} 份副本保持不变。")
    } else {
        format!("研究副本 {number} 已丢弃。")
    };
    self.notices.push(LoadNotice::Ok(notice));
}

/// 指定编号文档的研究成果手数；编号不存在或为原谱时 `None`。
pub(super) fn doc_research(&self, number: usize) -> Option<usize> {
    if number == self.active_number {
        return self
            .active_from_move
            .map(|from| self.board.move_count().saturating_sub(from));
    }
    self.others
        .iter()
        .find(|doc| doc.number == number)
        .and_then(|doc| {
            doc.from_move
                .map(|from| doc.board.move_count().saturating_sub(from))
        })
}

/// 侧栏文档列表（原谱在前、副本按编号升序）：活动文档 + `others`
/// 合并而成，每项携带编号、显示名、创建前缀与研究成果。
/// `number` 即切换 / 丢弃动作的定位键；`active` 驱动列表高亮。
pub(super) fn doc_entries(&self) -> Vec<analysis_panel::DocEntry> {
    let mut entries = Vec::with_capacity(self.others.len() + 1);
    // 活动文档：原谱显示文件名，副本显示「研究副本 N（自第 M 手起）」。
    entries.push(analysis_panel::DocEntry {
        number: self.active_number,
        name: self.active_name(),
        from_move: self.active_from_move,
        research: self
            .active_from_move
            .map_or(0, |from| self.board.move_count().saturating_sub(from)),
        active: true,
    });
    for doc in &self.others {
        entries.push(analysis_panel::DocEntry {
            number: doc.number,
            name: match doc.from_move {
                None => self.original_name(),
                Some(from) => {
                    let research = doc.board.move_count().saturating_sub(from);
                    format!("研究副本 {}（自第 {from} 手起）", doc.number)
                        + &if research > 0 {
                            format!("，含 {research} 手研究成果")
                        } else {
                            String::new()
                        }
                }
            },
            from_move: doc.from_move,
            research: doc
                .from_move
                .map_or(0, |from| doc.board.move_count().saturating_sub(from)),
            active: false,
        });
    }
    // 原谱（number 0）置顶，副本按编号升序；编号创建时单调递增，
    // 排序稳定即创建顺序。
    entries.sort_by_key(|entry| entry.number);
    entries
}

/// 活动文档的显示名（原谱 = 文件名；副本 = 「研究副本 N」）。
fn active_name(&self) -> String {
    if self.active_from_move.is_none() {
        return self.original_name();
    }
    format!("研究副本 {}", self.active_number)
}

/// 原谱的显示名（文件名，无文件名时给占位；未载入棋谱时不显示列表）。
fn original_name(&self) -> String {
    self.loaded
        .as_ref()
        .and_then(|meta| meta.source.file_name())
        .map_or_else(
            || "（无文件名）".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
}

}
