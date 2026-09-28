//! 棋谱的载入与另存（读文件 → 解析 → 换盘 / 序列化 → 写盘），以及
//! 换棋盘时的派生状态清理。
//!
//! 全部从原 `app.rs` 单体拆出，逐字节照搬：提示文案、副本丢弃说明与
//! 局后统计写回逻辑都不变。

use std::path::PathBuf;

use super::{DEFAULT_KOMI, GuanqiApp, PendingConfirm, PostSaveAction};
use crate::sgf::{GameMeta, load_from_bytes, save_to_file};
use crate::ui::analysis_panel::LoadNotice;

impl GuanqiApp {
/// 载入选中的棋谱：读文件 → 解析 → 重放主变着 → 整体替换棋盘。
/// 读取或解析失败时**保留原棋盘**，只提示错误。
/// 副本有研究成果时先替换掉它（确认框在 `open_file_dialog` 前已守卫）。
pub(super) fn load_game(&mut self, path: PathBuf) {
    match std::fs::read(&path) {
        Err(err) => {
            self.notices.push(LoadNotice::Failed(format!(
                "读取 {} 失败：{err}",
                path.display()
            )));
        }
        Ok(bytes) => self.load_game_from_bytes(path, bytes),
    }
}

/// 清掉所有**从属于当前棋盘**的派生状态（定位高亮与临时提示）。
///
/// 换棋盘时必走（载谱 / 新对局 / 建副本 / 切文档四处共用的同一组
/// 清理，此前各自手写了 3 行）：`Overlay.focus` 指向的坐标在新棋盘上
/// 是别的意思（甚至越界），旧棋盘的「已建分支」等临时提示同样失效。
/// 调用方各自还要做的事（`tree_epoch` 换代、`saved_rev` 对齐等）语义
/// 不同，不在本函数里拢。
pub(super) fn clear_board_derived(&mut self) {
    self.overlay.focus = None;
    self.branch_notice = None;
    self.notice = None;
}

/// 从字节流载入棋谱（[`Self::load_game`] 的读盘后半段；headless 工具
/// 复用以绕开 portal 对话框）。解析失败时**保留原棋盘**，只提示错误。
pub(super) fn load_game_from_bytes(&mut self, path: PathBuf, bytes: Vec<u8>) {
    match load_from_bytes(&path, &bytes) {
        Err(err) => {
            self.notices.push(LoadNotice::Failed(format!("打开棋谱失败：{err}")));
        }
        Ok(loaded) => {
            let (warning, partial) = (loaded.warning.clone(), loaded.partial);
            let unplaced_props = loaded.unplaced_props;
            let dropped_comments = loaded.dropped_comments;
            let (board, meta) = loaded.into_parts();
            // 载入棋谱的贴目进入分析口径：查询 komi 改取谱上的 KM
            // （缺失时默认 7.5）。此前 `komi` 只在 start_new_game 赋值，
            // 载入 KM[6.5] 的谱仍按 7.5 分析（或按上一盘的值），侧栏
            // 显示的却是谱里的真值——显示与分析口径不一致。
            self.komi = meta.info.komi.unwrap_or(DEFAULT_KOMI);
            // 载入棋谱：退出「新对局锁定规则」态，规则恢复走
            // 「设置显式 > 棋谱 RU > 默认」解析链。
            self.active_rules = None;
            // 新对局：清空分析快照与胜率历史（新对局不混旧曲线）；
            // 定位高亮所指的局面已不存在，一并清除；旧棋盘的临时提示
            //（建分支等）随棋盘替换失效，同样清除。
            self.analysis.reset();
            self.analysis.clear_limits();
            self.clear_board_derived();
            // 副本是原谱的附属品，离开这份棋谱即弃。带研究成果时
            // 用户已在确认框里同意（见 `open_file_dialog`），无成果
            // 时静默丢弃并在提示里带一句。
            let dropped = self.research_moves();
            self.others.clear();
            self.active_from_move = None;
            self.active_number = 0;
            self.pending_confirm = PendingConfirm::None;
            let size = board.size();
            let moves = board.move_count();
            self.board = board;
            // 新载入的谱是「刚保存过的内容」：对齐基准，脏标记复位。
            self.mark_saved();
            self.loaded = Some(meta);
            // 棋盘整体替换：树布局指纹换代，不复用旧谱布局。
            self.tree_epoch = self.tree_epoch.wrapping_add(1);
            // 主变停止为「部分载入」（橙色留意）；仅变着分支被舍弃时
            // 提示照样给出，但按成功（绿色）显示。
            let notice = match warning {
                Some(warning) if partial => LoadNotice::Warn(format!(
                    "已部分载入 {size}（全树共 {moves} 手）：{warning}"
                )),
                Some(warning) => {
                    LoadNotice::Ok(format!("已载入 {size}（全树共 {moves} 手，{warning}）"))
                }
                None => LoadNotice::Ok(format!("已载入 {size} 棋谱，全树共 {moves} 手。")),
            };
            // 顺带说明副本去向：副本的「-副本」元信息随棋盘替换失效，
            // 一并清掉（副本已在上面的 `self.study = None` 丢弃）。
            let notice = if dropped > 0 {
                LoadNotice::Warn(format!(
                    "{}（研究副本连同 {dropped} 手研究成果已丢弃。）",
                    notice.text()
                ))
            } else {
                notice
            };
            // 载入过程中真的丢了的用户数据，合并进同一条提示：
            // - 纯注释节点（所在节点没有着法）上的 `C` 注释：逐手注释
            //   按局面签名索引，这类节点不产生棋盘节点、拿不到签名，
            //   注释无处可挂；
            // - 挂在同一类 / 非法着法节点上的标记与扩展属性（另存写不出）。
            // 二者都不可挽回，必须让用户知道，不能静默丢。
            let notice = match (dropped_comments, unplaced_props) {
                (0, 0) => notice,
                (n, 0) => LoadNotice::Warn(format!(
                    "{}（谱上有 {n} 条注释因所在节点没有着法，无法保留。）",
                    notice.text()
                )),
                (0, m) => LoadNotice::Warn(format!(
                    "{}（谱上另有 {m} 条标记/扩展属性挂在无法载入的\
                     节点上，另存时将无法保留。）",
                    notice.text()
                )),
                (n, m) => LoadNotice::Warn(format!(
                    "{}（谱上有 {n} 条注释因所在节点没有着法无法保留；\
                     另有 {m} 条标记/扩展属性挂在无法载入的节点上，另存时将无法保留。）",
                    notice.text()
                )),
            };
            self.notices.push(notice);
        }
    }
}

/// 把当前棋盘（含用户新建的变着）写到用户确认的位置。
/// 新对局后未另存过时用默认文件名存；写失败时提示可读原因，原状态不变。
///
/// 局后统计写回（LizzieYzy appendAiScoreBlunder 口径）：已载入棋谱时
/// 把当前统计（[`AnalysisState::game_summary`]，与侧栏「局后统计」卡片
/// 同一次计算口径）合成进根节点 `C[]`——界标块幂等替换，用户原有根
/// 注释不覆盖。未载入棋谱（空盘 / 新对局）不写：新对局的统计属于
/// 「本盘」，快扫结束后按普通另存自然带出。
pub(super) fn save_game(&mut self, path: PathBuf) {
    // 写出前先记下原谱的 SGF 版本：写出端恒按 FF[4] 写（见
    // `save::build_root`），原谱是别的值就意味着**降级另存**，必须
    // 如实告知，不能无声无息把 FF[5] 的文件换成 FF[4]。
    let source_format = self.loaded.as_ref().and_then(|meta| meta.info.format);
    let stats_block = if self.loaded.is_some() {
        crate::ui::analysis::stats_block_text(
            &self.analysis.game_summary(&self.board),
            crate::ui::analysis::WORST_LIMIT,
        )
    } else {
        None
    };
    match save_to_file(
        &path,
        &self.board,
        self.loaded.as_ref(),
        stats_block.as_deref(),
    ) {
        Ok(()) => {
            let size = self.board.size();
            let branches = self.board.nodes().len() - 1;
            self.loaded
                .as_mut()
                // 记住新路径：再次「另存为」默认名跟随最新位置。
                .map(|meta| meta.source = path.clone())
                .unwrap_or_else(|| {
                    self.loaded = Some(GameMeta::for_path(&path, self.board.size()));
                });
            // 另存成功：内容已落盘，对齐基准清脏；若这是「先另存再
            // 继续」的那次另存，此处同时自动继续被暂挂的原操作。
            self.mark_saved();
            let post_save = std::mem::take(&mut self.post_save);
            self.notices.push(LoadNotice::Ok(format!(
                "已另存到 {}（{size}，{branches} 手）。",
                path.display()
            )));
            // 原谱不是 FF[4] 而我们恒按 FF[4] 写：降级另存，如实说明
            // （写出值本身不改，见 `save::build_root`）。
            if let Some(n) = source_format
                && n != 4
            {
                self.notices.push(LoadNotice::Warn(format!(
                    "原谱 SGF 版本为 FF[{n}]，已按 FF[4] 另存。"
                )));
            }
            match post_save {
                PostSaveAction::None => {}
                PostSaveAction::NewGame(setup) => {
                    self.start_new_game(setup);
                }
                PostSaveAction::OpenDialog => {
                    self.open_file_dialog();
                }
                PostSaveAction::Exit => {
                    // 落盘后放行退出：下一帧 ui 顶部重新发 Close
                    // （`request_close`，此时 `is_dirty` 已为 false）。
                    self.exit_requested = true;
                }
            }
        }
        Err(err) => {
            self.notices.push(LoadNotice::Failed(format!(
                "保存到 {} 失败：{err}",
                path.display()
            )));
        }
    }
}
}
