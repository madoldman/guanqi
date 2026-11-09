//! 注释文本搜索（竞品补充 S2，Sabaki「按注释找棋」的移植）。
//!
//! 长复盘里「那句笔记写在哪一手」靠翻树面板找很慢：输入关键词，全树
//! （含变着）的节点注释做子串匹配，点结果行即跳转。数据口径：
//! - 注释表键 = 根到节点着法路径的签名（与 [`crate::sgf::GameMeta::
//!   comment_at`] 同一套哈希），路径经 `Board::nodes()` 的父链现场
//!   重算——O(节点 × 深度)，仅窗口输入时跑，长谱也是毫秒级；
//! - 根注释（`root_comment`，游标 0 的唯一来源）单列一条、排最前。
//!
//! 跳转 = `go_to_node` 导航（不改棋谱）；窗口形态双限高（同注释 /
//! 信息编辑窗），结果按手数排序，输入框打开即聚焦。

use super::GuanqiApp;
use crate::board::MoveRecord;
use crate::ui::analysis_panel::LoadNotice;

/// 一条搜索结果：`node = None` 表示根注释（跳转到游标 0）。
struct SearchHit {
    node: Option<usize>,
    label: String,
    comment: String,
}

/// 注释片段：超 32 字截断加省略号（列表行宽有限，全文走跳转后的侧栏）。
fn snippet_of(text: &str) -> String {
    if text.chars().count() > 32 {
        text.chars().take(32).collect::<String>() + "…"
    } else {
        text.to_owned()
    }
}

impl GuanqiApp {
    /// 「分析 → 搜索注释…」入口。
    pub(super) fn open_comment_search(&mut self) {
        self.comment_search_open = true;
        self.comment_search_query.clear();
    }

    /// 汇集全树带注释的节点（路径签名重算；跳过根——根注释单列）。
    fn collect_commented_nodes(&self) -> Vec<(usize, Vec<MoveRecord>, String)> {
        let Some(meta) = self.loaded.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (id, node) in self.board.nodes().iter().enumerate() {
            if node.record().is_none() {
                continue; // 根节点单列（root_comment）
            }
            // 自底向上收路径（含本节点），反转后算签名。
            let mut chain = Vec::new();
            let mut cur = Some(id);
            while let Some(now) = cur {
                let n = &self.board.nodes()[now];
                if let Some(rec) = n.record() {
                    chain.push(rec.clone());
                }
                cur = n.parent();
            }
            chain.reverse();
            let Some(text) = meta.comment_by_records(&chain).map(str::to_owned) else {
                continue;
            };
            out.push((id, chain, text));
        }
        out.sort_by_key(|(_, path, _)| path.len());
        out
    }

    /// 搜索执行体（`&self` 纯查询）：关键词为空 = 列出全部带注释节点。
    fn search_results(&self, query: &str) -> Vec<SearchHit> {
        let Some(meta) = self.loaded.as_ref() else {
            return Vec::new();
        };
        let query_lower = query.to_lowercase();
        let mut out = Vec::new();
        if let Some(root) = meta.info.root_comment.as_deref()
            && (query_lower.is_empty() || root.to_lowercase().contains(&query_lower))
        {
            out.push(SearchHit {
                node: None,
                label: "根注释".to_owned(),
                comment: snippet_of(root),
            });
        }
        for (id, path, text) in self.collect_commented_nodes() {
            if !query_lower.is_empty() && !text.to_lowercase().contains(&query_lower) {
                continue;
            }
            let label = match path.last() {
                Some(record) => {
                    let at = match record.action {
                        crate::board::Action::Place(c) => c.to_gtp(self.board.size()),
                        crate::board::Action::Pass => "弃着".to_owned(),
                    };
                    format!("第 {} 手 · {} {at}", path.len(), record.player.name())
                }
                None => "根注释".to_owned(),
            };
            out.push(SearchHit {
                node: Some(id),
                label,
                comment: snippet_of(&text),
            });
        }
        out
    }

    /// 搜索窗（每帧调用，`comment_search_open` 打开时绘制）。
    ///
    /// 结构说明：查询词在独立字段（闭包内字段级可变捕获），结果每帧在
    /// **闭包外**以 `&self` 算好再进闭包渲染——`search_results` 要读
    /// 整个 `self`（棋盘 + 注释表），与查询词的可变借用不能共存。
    pub(super) fn comment_search_dialog(&mut self, ctx: &egui::Context) {
        if !self.comment_search_open {
            return;
        }
        let results = self.search_results(&self.comment_search_query);
        let mut open = self.comment_search_open;
        let mut jump: Option<Option<usize>> = None;
        egui::Window::new("搜索注释")
            .open(&mut open)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 40.0])
            .max_height(460.0)
            .show(ctx, |ui| {
                ui.weak("按关键词搜全树注释（含变着），点结果跳转。");
                ui.add_space(4.0);
                // 输入框打开即聚焦（与注释 / 信息编辑窗同一体验）。
                let response = egui::TextEdit::singleline(&mut self.comment_search_query)
                    .desired_width(ui.available_width())
                    .hint_text("输入关键词…")
                    .show(ui);
                response.response.request_focus();
                ui.add_space(4.0);
                if results.is_empty() {
                    ui.weak(if self.comment_search_query.is_empty() {
                        "棋谱里还没有注释。用「编辑注释…」写一条试试。"
                    } else {
                        "没有匹配的注释。"
                    });
                } else {
                    // 结果列表（列表行规范形态；滚动区限高在窗体内）。
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for hit in &results {
                                let text = format!("{} · {}", hit.label, hit.comment);
                                let clicked = ui
                                    .with_layout(
                                        egui::Layout::top_down_justified(
                                            egui::Align::LEFT,
                                        ),
                                        |ui| ui.button(text).clicked(),
                                    )
                                    .inner;
                                if clicked {
                                    jump = Some(hit.node);
                                }
                            }
                        });
                }
            });
        self.comment_search_open = open;
        if !open {
            return;
        }
        if let Some(node) = jump {
            match node {
                Some(id) => {
                    let depth = self.board.nodes()[id].depth();
                    self.board.go_to_node(id);
                    self.notices.push(LoadNotice::Ok(format!(
                        "已跳到第 {depth} 手（注释匹配）。"
                    )));
                }
                None => {
                    self.board.go_to(0);
                }
            }
            self.comment_search_open = false;
        }
    }
}
