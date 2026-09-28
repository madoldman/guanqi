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
            self.notices
                .push(LoadNotice::Ok("已卸载参照文件。".to_owned()));
        }
    }

    /// 当前参照的只读访问（侧栏卡片与 headless 快照共用）。
    pub fn current_reference(&self) -> Option<&Reference> {
        self.reference.as_ref()
    }
}
