//! 参照文件的加载 / 卸载（App 侧接线）。
//!
//! 参照与「当前对局 / 当前棋谱」是两回事：加载参照**绝不替换**棋盘与
//! 文档，不碰脏标记、副本、分析状态（不需要——它不改变任何对局数据）；
//! 同一时刻只保留一份（[`GuanqiApp::reference`] 为 `Option`），再加载即
//! 整体替换并提示。失败时保留旧参照不动。

use std::path::PathBuf;

use super::GuanqiApp;
use crate::refbook::{Reference, load_reference};
use crate::ui::analysis_panel::{self, LoadNotice};
use crate::ui::ref_tree;

impl GuanqiApp {
    /// 派生侧栏「参照」卡片的展示数据（每帧一次；命中查询是 O(1)
    /// 哈希查表，偏离判定只在命中时多一次父局面重放，量级可忽略）。
    ///
    /// **解题模式下返回 `None`**（阶段 22.2）：命中详情里的续法 / 注释 /
    /// 偏离提示正是答案（剧透内容），卡片整体不渲染（结构信息由「解题」
    /// 卡片承担）；取证 dump（`state_snapshot`）取的是同一数据源，若
    /// 解题模式仍派生续法文本，取证面会带出答案——数据与显示一起藏。
    pub(super) fn reference_view(&self) -> Option<analysis_panel::ReferenceView> {
        if self.solving_mode {
            return None;
        }
        let reference = self.reference.as_ref()?;
        let mut deviation_top = None;
        // 照库落子的守卫与结构化续法（与命中详情同一数据源，每帧现场
        // 派生）：
        // - 末端守卫：续法只描述「当前这个库中局面」的后续，只在游标
        //   处于活子位置（cursor == line_len，与手点在活子上续棋同口径）
        //   时可落；回看中点续法会落到旧局面上建变着分支，不是用户要的
        //   「照着库走」，入口置灰并说明。
        // - 行棋方：局面键含行棋方字节 ⇒ 能命中的局面 to_play 必与库内
        //   节点一致，库内续法（children 首手）行棋方也恒等于 to_play，
        //   正常数据不会不符；这里仍把实况带给 UI 做防御性对照（注释
        //   节点链里的着法不查行棋方，构造谱可造出不一致）。
        let place_blocked = (self.board.cursor() != self.board.line_len())
            .then(|| "当前在回看历史（非棋谱末端）。".to_owned());
        let hit = match reference.lookup(&self.board) {
            crate::refbook::LookupOutcome::Hit { entry, tail, deviation } => {
                Some(analysis_panel::ReferenceHit {
                    move_no: entry.move_no,
                    path: entry
                        .path
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("-"),
                    tail: entry.tail_text(self.board.size()),
                    comment: entry.comment.clone(),
                    continuations: entry
                        .continuations
                        .iter()
                        .map(crate::refbook::Continuation::text)
                        .collect(),
                    // 结构化落子信息：把 GTP 串按棋盘尺寸换回 Coord（弃着
                    // 或串非法 = None，UI 置灰）。解析口径只在派生侧一处。
                    playable: entry
                        .continuations
                        .iter()
                        .map(|cont| {
                            cont.at
                                .as_deref()
                                .and_then(|at| crate::board::Coord::from_gtp(self.board.size(), at).ok())
                                .map(|at| (cont.player, at))
                        })
                        .collect(),
                    deviation,
                    live_tail: tail
                        .into_iter()
                        .map(|(stone, at)| format!("{} {at}", stone.name()))
                        .collect(),
                    region: {
                        let size = reference.size();
                        let r = entry.region;
                        format!(
                            "{}-{}",
                            crate::board::Coord::new(size, r.min_x, r.min_y)
                                .map(|c| c.to_gtp(size))
                                .unwrap_or_default(),
                            crate::board::Coord::new(size, r.max_x, r.max_y)
                                .map(|c| c.to_gtp(size))
                                .unwrap_or_default(),
                        )
                    },
                    to_play: Some(self.board.to_play()),
                    place_blocked,
                })
            }
            crate::refbook::LookupOutcome::Deviation(deviation) => {
                // 偏离：当前局面不在库中（父局面命中）。hit = None，
                // 偏离信息放顶层字段。
                deviation_top = Some((deviation.played, deviation.book));
                None
            }
            _ => None,
        };
        Some(analysis_panel::ReferenceView {
            name: reference.file_name(),
            size: reference.size().n(),
            entries: reference.entry_count(),
            nodes: reference.move_node_count(),
            warnings: reference.warnings().iter().map(str::to_owned).collect(),
            hit,
            deviation: deviation_top,
        })
    }

    /// 载入选中的参照文件：读文件 → 解析 → 建局面键索引。
    /// 与「打开棋谱」完全正交：不改棋盘 / 文档 / 脏标记 / 分析状态。
    /// 再加载即替换旧参照（提示中说明）；失败时旧参照保留。
    pub(super) fn load_reference_file(&mut self, path: PathBuf) {
        let replaced = self.reference.is_some();
        match load_reference(&path) {
            Err(err) => {
                // 失败：保留旧参照（若有），只提示原因。
                self.notices.push(LoadNotice::Failed(format!(
                    "加载参照文件失败：{err}"
                )));
            }
            Ok(reference) => {
                // 结果生命周期出口：换参照即清空打分结果（取消在飞任务、
                // 清结果表——旧结果描述的是「上一份库」，无从对号）。
                self.analysis.detach_ref_scan();
                let size = reference.size();
                let entries = reference.entry_count();
                let nodes = reference.move_node_count();
                let elapsed = reference.index_elapsed();
                let name = reference.file_name();
                let warn_text = match reference.warnings().is_empty() {
                    true => String::new(),
                    false => format!(
                        "；另有 {} 条载入警告（分支舍弃）",
                        reference.warnings().iter().count()
                    ),
                };
                let base = if replaced {
                    format!(
                        "已替换参照：{name}（{size}，索引 {entries} 个局面 / \
                         全树 {nodes} 手，建索引 {:.1} ms{warn_text}）。",
                        elapsed.as_secs_f64() * 1000.0
                    )
                } else {
                    format!(
                        "已加载参照：{name}（{size}，索引 {entries} 个局面 / \
                         全树 {nodes} 手，建索引 {:.1} ms{warn_text}）。",
                        elapsed.as_secs_f64() * 1000.0
                    )
                };
                // 载入警告明细在侧栏「参照」卡片里给出（排队提示放不下）。
                self.notices.push(LoadNotice::Ok(base));
                self.reference = Some(reference);
            }
        }
    }

    /// 卸载参照（无参照时静默：菜单入口已置灰，快捷路径不提示）。
    pub(super) fn unload_reference(&mut self) {
        if self.reference.take().is_some() {
            // 结果生命周期出口：卸载即清空打分结果（同「换参照即清空」）。
            self.analysis.detach_ref_scan();
            self.notices
                .push(LoadNotice::Ok("已卸载参照文件。".to_owned()));
        }
    }

    /// 当前参照的只读访问（侧栏卡片与 headless 快照共用）。
    pub fn current_reference(&self) -> Option<&Reference> {
        self.reference.as_ref()
    }

    /// 派生「库中树」浏览视图（阶段 23.2）：窗口渲染与 headless dump
    /// 共用同一数据源（渲染什么、取证什么，两面对账天然一致）。
    ///
    /// 返回 `None` = 窗口不该有内容（无参照；解题模式由调用方先行短路）。
    /// 锚点：命中 → 命中节点（祖先链 + 后继）；未命中 / 偏离 → 库根
    /// （浏览库结构也是本功能的合法用途），并给出窗口顶部的原因说明。
    pub(super) fn ref_tree_view(
        &self,
    ) -> Option<(ref_tree::RefTreeView, Option<String>)> {
        let reference = self.reference.as_ref()?;
        let hit_path = self
            .reference_view()
            .and_then(|view| view.hit)
            .map(|hit| {
                hit.path
                    .split('-')
                    .filter_map(|part| part.parse::<usize>().ok())
                    .collect::<Vec<_>>()
            });
        let (anchor, reason) = match hit_path {
            Some(path) => (ref_tree::RefTreeAnchor::Hit(path), None),
            None => (
                ref_tree::RefTreeAnchor::Root,
                Some(
                    "当前局面未在库中命中（或已偏离）——从库根浏览；\
                     走回库内分支后视图会跳到命中位置。"
                        .to_owned(),
                ),
            ),
        };
        let view = ref_tree::derive_view(
            reference.tree(),
            reference.size(),
            &anchor,
            reference.tree().root().and_then(|root| root.comment()),
        );
        Some((view, reason))
    }

    /// 照库落子（侧栏「参照」卡片续法点击的执行体）：在当前棋盘**真实
    /// 落该子**，走手点同一条 `play_with_feedback`（[`Board::play`]：
    /// 计入棋谱、可 Ctrl+Z 悔棋、局面变化触发既有分析重发）——参照只
    /// 给指路，落子是对局 / 复盘动作，不走「跳进参照树 / 伪造局面」的
    /// 旁路。
    ///
    /// 守卫（与卡片入口置灰同口径，此处复核防派生与点击之间的局面漂移）：
    /// - **只允许末端**：`cursor == line_len`（回看中点续法会落到旧局面
    ///   上建变着分支，不是「照着库走」）；被拒给原因。
    /// - **行棋方相符**：续法 `player` ≠ 当前 `to_play()` 即拒（说明该
    ///   续法是为哪一方写的）。
    ///   落子非法（已被占用 / 劫争等）由 `play_with_feedback` 写入非法
    ///   提示，与手点同一条错误路径。
    pub(super) fn play_from_book(&mut self, player: crate::board::Stone, at: crate::board::Coord) {
        if self.board.cursor() != self.board.line_len() {
            self.notices.push(LoadNotice::Warn(
                "照库落子被拒：当前在回看历史（非棋谱末端），只在末端可照库落子。".to_owned(),
            ));
            return;
        }
        if self.board.to_play() != player {
            self.notices.push(LoadNotice::Warn(format!(
                "照库落子被拒：该续法是给{}方走的，当前轮到{}方。",
                player.name(),
                self.board.to_play().name(),
            )));
            return;
        }
        // 走子口径与手点等价：同一函数给出成功反馈（清提示 / 读秒 /
        // 变着提示）与失败原因（侧栏「非法落子：…」）。
        crate::ui::board_view::play_with_feedback(
            &mut self.board,
            &mut self.notice,
            &mut self.branch_notice,
            at,
            Some(&mut self.play),
        );
    }
}
