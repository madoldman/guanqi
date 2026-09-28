//! 复盘报告的**内容生成与写出**（阶段 21 #6）：把局后统计与失误分析
//! 拼装成一份可带走的 Markdown 报告；棋盘图 PNG 由 [`super::report_image`]
//! 绘制，随报告写进同一目录、以相对路径引用。
//!
//! 职责切分：本模块负责「报告长什么样 + 写盘」；数字全部来自调用方
//! 传入的 [`GameSummary`]（与侧栏「局后统计」卡片同一次计算），差异手
//! 的实战点 / 首选坐标来自调用方从棋盘与候选表换算好的 [`DiagramEntry`]。
//! 本模块不接触 `AnalysisState`，也不重算任何统计——报告是统计的
//! **转述者**，不生产数字。
//!
//! # 「未知」与「0」的口径（沿用统计侧铁律）
//!
//! 没有数据的部分如实写「未分析 / —」，绝不编造：
//! - 吻合度：`analyzed == 0` 写「未分析」；不足样本门槛写「样本不足」
//!   （与侧栏 `match_cell` 同口径，两处实现需同步改）；
//! - 差异手表：只列 `game_summary` 排行榜里已给出的手（该榜本就只收
//!   损失可算的手）；引擎首选缺失写「—」；实战点为弃着时如实写。
//!
//! # 文件布局
//!
//! ```text
//! <目录>/
//!   <报告名>.md              ← 用户在保存对话框选的路径
//!   <报告名>-m<N>.png        ← 差异手各手数的图（N = 手数）
//! ```
//! Markdown 里 `![](./<图片名>)` 相对引用，整目录可整体搬走；单独的
//! PNG 也自足可读（图内水印带手数与结论）。

use std::path::Path;

use crate::board::{Coord, Size, Stone};
use crate::engine::Rules;
use crate::sgf::GameMeta;
use crate::ui::analysis::{GameSummary, WORST_LIMIT};

/// 一张差异手图 / 一行表格的全部上下文：由接线层从棋盘与统计组装
/// （纯数据；图与表消费同一份，保证图、表数字必然同源）。
pub struct DiagramEntry {
    /// 该手手数（1 起；与文件名 `<报告名>-m<N>.png` 对应）。
    pub turn: usize,
    /// 棋盘尺寸（GTP 坐标换算用；随条目存储，表格行换算无需再借棋盘）。
    pub board_size: Size,
    /// 该手行棋方。
    pub player: Stone,
    /// 实战落点（弃着为 `None`：表格写「弃着」、图不画实战框）。
    pub actual: Option<Coord>,
    /// 引擎首选（候选表缺失 / 首选为弃着时 `None`，表格写「—」、图不画圈）。
    pub best: Option<Coord>,
    /// 该手严重程度分级（损失目数与分级取自 [`WorstMove`]，不在此重复）。
    pub severity: crate::ui::analysis::Severity,
    /// 该手目差损失（正 = 行棋方亏损；取自 [`WorstMove::score_loss`]）。
    pub score_loss: f64,
}

/// 图注（写进报告、同时作为 PNG 顶部水印）：如
/// 「第 10 手 · 白 · 损失 6.0 目（恶手）」。图注由图表共用的
/// [`DiagramEntry`] 生成，保证图内文字与表格行必然一致。
pub fn diagram_caption(entry: &DiagramEntry) -> String {
    format!(
        "第 {} 手 · {} · 损失 {:.1} 目（{}）",
        entry.turn,
        entry.player.name(),
        entry.score_loss,
        entry.severity.name(),
    )
}

/// 吻合度单元格文本（与侧栏 `match_cell` 同口径的独立实现：未分析 /
/// 样本不足 / 百分比。两处改动需同步，样本判定由 summary 的 enough
/// 字段承载，这里不重复数数）。
fn match_text(ratio: f64, analyzed: usize, enough: bool) -> String {
    if analyzed == 0 {
        "未分析".to_owned()
    } else if !enough {
        "样本不足".to_owned()
    } else {
        format!("{:.1}%", ratio * 100.0)
    }
}

/// 吻合度数据构成（快扫 N 手 / 深度分析 M 手，与侧栏 composition 同口径；
/// 仅在已分析 > 0 时调用）。
fn composition(deep: usize, analyzed: usize) -> String {
    let quick = analyzed - deep;
    if deep == 0 {
        "纯快扫".to_owned()
    } else if quick == 0 {
        "纯深度分析".to_owned()
    } else {
        format!("快扫 {quick} 手 / 深度分析 {deep} 手")
    }
}

/// 报告的棋谱元信息（接线层经 [`report_meta`] 从 `GameMeta` 取出）。
pub struct ReportMeta {
    /// 来源文件名（含扩展名；新对局为空串，标题区省略该行）。
    pub source_name: String,
    /// 黑方棋手（缺失省略）。
    pub player_black: Option<String>,
    /// 白方棋手（缺失省略）。
    pub player_white: Option<String>,
    /// 对局结果（缺失省略）。
    pub result: Option<String>,
    /// 贴目（缺失省略）。
    pub komi: Option<f64>,
    /// 规则中文名（缺失省略）。
    pub rules_name: Option<String>,
    /// 对局日期（缺失省略）。
    pub date: Option<String>,
    /// 当前线总手数。
    pub total_moves: usize,
}

/// 从 `GameMeta` 组装报告元信息。规则按与查询发起同一条解析链
/// （设置显式 > 棋谱 RU 宽容映射 > 默认，[`resolve_rules`]）换算成
/// 中文名——报告写规范名，不透传 SGF 自由文本。`total_moves` 由调用方
/// 传 [`Board::line_len`]（本模块不持棋盘）。
pub fn report_meta(meta: Option<&GameMeta>, total_moves: usize) -> ReportMeta {
    let Some(meta) = meta else {
        return ReportMeta {
            source_name: String::new(),
            player_black: None,
            player_white: None,
            result: None,
            komi: None,
            rules_name: None,
            date: None,
            total_moves,
        };
    };
    let info = &meta.info;
    let rules_name = Rules::from_wire(&crate::engine::resolve_rules(None, info.rules.as_deref()).rules)
        .map(|r| r.name().to_owned());
    ReportMeta {
        source_name: meta
            .source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        player_black: info.player_black.clone(),
        player_white: info.player_white.clone(),
        result: info.result.clone(),
        komi: info.komi,
        rules_name,
        date: info.date.clone(),
        total_moves,
    }
}

/// 拼装 Markdown 报告正文（不含图片引用；图片行由 [`append_diagrams`]
/// 在写盘时按实际文件名追加）。
pub fn render_markdown(meta: &ReportMeta, summary: &GameSummary) -> String {
    let mut md = String::new();
    // ---- 标题与对局信息（存在才写）----
    md.push_str("# 观棋复盘报告\n\n");
    if !meta.source_name.is_empty() {
        md.push_str(&format!("- 来源：{}\n", meta.source_name));
    }
    let players = match (&meta.player_black, &meta.player_white) {
        (Some(b), Some(w)) => Some(format!("黑 {b} / 白 {w}")),
        (Some(b), None) => Some(format!("黑方 {b}")),
        (None, Some(w)) => Some(format!("白方 {w}")),
        (None, None) => None,
    };
    if let Some(line) = players {
        md.push_str(&format!("- 对局者：{line}\n"));
    }
    if let Some(result) = &meta.result {
        md.push_str(&format!("- 结果：{result}\n"));
    }
    if let Some(komi) = meta.komi {
        md.push_str(&format!("- 贴目：{komi}\n"));
    }
    if let Some(rules) = &meta.rules_name {
        md.push_str(&format!("- 规则：{rules}\n"));
    }
    if let Some(date) = &meta.date {
        md.push_str(&format!("- 日期：{date}\n"));
    }
    md.push_str(&format!("- 手数：共 {} 手（当前线）\n\n", meta.total_moves));

    // ---- 局后统计：黑白吻合度（数据构成沿用侧栏口径）----
    md.push_str("## 局后统计\n\n");
    if summary.total == 0 {
        md.push_str("尚无棋谱，无统计。\n");
        return md;
    }
    let black = match_text(summary.match_black, summary.analyzed_black, summary.enough_black);
    let white = match_text(summary.match_white, summary.analyzed_white, summary.enough_white);
    md.push_str("| 方 | 吻合度 | 已分析 | 数据构成 |\n");
    md.push_str("|---|---|---|---|\n");
    md.push_str(&format!(
        "| 黑 | {black} | {} 手 | {} |\n",
        summary.analyzed_black,
        if summary.analyzed_black > 0 {
            composition(summary.deep_black, summary.analyzed_black)
        } else {
            "—".to_owned()
        },
    ));
    md.push_str(&format!(
        "| 白 | {white} | {} 手 | {} |\n\n",
        summary.analyzed_white,
        if summary.analyzed_white > 0 {
            composition(summary.deep_white, summary.analyzed_white)
        } else {
            "—".to_owned()
        },
    ));
    md.push_str(&format!(
        "吻合度 = 实际落子在候选表中的 visits 占比（整局平均，LizzieYzy \
         `percentsMatch` 口径）；未分析的手不计入。已分析 {} 手 / 共 {} 手\
         （黑 {} + 白 {}）。已分析不足 {} 手不给平均值（样本不足）。\n\n",
        summary.analyzed_black + summary.analyzed_white,
        summary.total,
        summary.analyzed_black,
        summary.analyzed_white,
        crate::ui::analysis::MATCH_MIN_MOVES,
    ));
    md.push_str(&format!(
        "数据构成按该手搜索量 ≥ {} visits 计「深度分析」，否则「快扫」；\
         快扫（默认 40 visits）的候选表比深度分析短，吻合度系统性偏低，\
         不同深度的数字不可互比。\n\n",
        crate::ui::analysis::DEEP_VISITS_THRESHOLD,
    ));

    // ---- 差异手排行（按胜率损失降序 Top N，只列有数据的行）----
    md.push_str("## 差异手排行\n\n");
    if summary.worst.is_empty() {
        md.push_str("尚无分析数据（快扫后重试）。\n\n");
        return md;
    }
    md.push_str(&format!(
        "按胜率损失降序，前 {} 条（与侧栏「局后统计」同源；损失为行棋方\
         视角的目差损失，正 = 亏损）。\n\n",
        WORST_LIMIT,
    ));
    md.push_str("| 手数 | 行棋方 | 实战点 | 损失目数 | 分级 | 引擎首选 |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    for entry in summary.worst.iter().take(WORST_LIMIT) {
        md.push_str(&format!(
            "| {} | {} | {} | {:.1} | {} | {} |\n",
            entry.turn,
            entry.player.name(),
            "—", // 实战点 / 首选在 append_diagrams 里与图同源填充。
            entry.score_loss,
            entry.severity.name(),
            "—",
        ));
    }
    md
}

/// 把差异手表格行**替换**进报告并追加图引用与图例。
///
/// [`render_markdown`] 先按统计拼出骨架（实战点 / 首选列留占位），本
/// 函数用与图同源的 [`DiagramEntry`] 重写表格行（保证图表数字一致），
/// 并在每个局面后追加 `![图注](./文件名)` 与图例说明。`entries` 与
/// `summary.worst` 前若干条一一对应、`image_names` 为各手图片文件名。
pub fn append_diagrams(
    md: &mut String,
    entries: &[DiagramEntry],
    image_names: &[String],
) {
    let Some(table_head) = md.find("| 手数 | 行棋方 | 实战点 |") else {
        return;
    };
    // 表头之后整段替换（骨架的占位行在表头之后直到文末）。
    md.truncate(table_head);
    md.push_str("| 手数 | 行棋方 | 实战点 | 损失目数 | 分级 | 引擎首选 |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    for entry in entries {
        let actual = match entry.actual {
            Some(at) => at.to_gtp(entry.size()),
            None => "弃着".to_owned(),
        };
        let best = match entry.best {
            Some(at) => at.to_gtp(entry.size()),
            None => "—".to_owned(),
        };
        md.push_str(&format!(
            "| {} | {} | {} | {:.1} | {} | {} |\n",
            entry.turn,
            entry.player.name(),
            actual,
            entry.score_loss,
            entry.severity.name(),
            best,
        ));
    }
    // 图区块在**表格之后**统一追加（图行插进表格中间会切断 Markdown
    // 表格连续性）：每手一节「### 第 N 手」，图内水印与图注同文，
    // 首节附标注图例（橙框实战点 / 蓝圈引擎首选 / 色点失误分级）。
    md.push_str("\n### 差异手局面图\n");
    for (i, entry) in entries.iter().enumerate() {
        if let Some(image) = image_names.get(i) {
            md.push_str(&format!(
                "\n#### 第 {} 手\n\n{}\n\n![{}]({})\n",
                entry.turn,
                diagram_caption(entry),
                diagram_caption(entry),
                image,
            ));
            if i == 0 {
                md.push_str(
                    "\n图例：橙方框 = 实战点，蓝圆圈 = 引擎首选，棋子中心色点 = \
                     失误标记（黄疑问手 / 橙失误 / 红恶手）；琥珀小方点 = 最后一手。\n",
                );
            }
        }
    }
    md.push('\n');
}

/// 把报告与差异手图写出。`report_path` 为用户选定的 Markdown 路径，
/// 图片写同目录、按 `<报告名>-m<N>.png` 命名；`draw` 负责单张图的绘制
/// 与编码（接线层闭包：持有字体上下文与盘面）。返回 (报告字节, 图片
/// 实际文件名列表)；写出失败（目录不可写等）返回 `Err`，不 panic。
///
/// 先写图片再写报告：报告是最后落盘的「目录已完整」信号，中断时不留
/// 引用着不存在图片的报告。
pub fn write_report(
    report_path: &Path,
    markdown: &str,
    entries: &[DiagramEntry],
    draw: impl Fn(&DiagramEntry) -> Result<Vec<u8>, String>,
) -> Result<Vec<String>, String> {
    let dir = report_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("路径没有目录：{}", report_path.display()))?;
    let stem = report_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "复盘报告".to_owned());
    let mut names = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = format!("{stem}-m{}.png", entry.turn);
        let bytes = draw(entry)?;
        std::fs::write(dir.join(&name), bytes)
            .map_err(|err| format!("写入 {} 失败：{err}", dir.join(&name).display()))?;
        names.push(name);
    }
    std::fs::write(report_path, markdown)
        .map_err(|err| format!("写入 {} 失败：{err}", report_path.display()))?;
    Ok(names)
}

/// `DiagramEntry` 自带尺寸换算（接线层组装时棋盘尺寸已知；这里存一份
/// 避免表格行换算 GTP 时再借棋盘）。
impl DiagramEntry {
    /// 组装时记下的棋盘尺寸（GTP 坐标换算用）。
    pub fn size(&self) -> Size {
        self.board_size
    }
}
