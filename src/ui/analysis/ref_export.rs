//! 导出「AI 标注版库」（阶段 23.3）：把参照库打分结果
//! ([`RefScanResults`]) 写进**参照树副本**的节点注释，另存为一个新的
//! SGF 文件。
//!
//! # 定位
//!
//! 产物用途：拿着这份「AI 标注版」去别的软件看（例如给老定式库标出
//! 哪些分支已被 AI 淘汰）。写回机制沿用 [`crate::sgf::save`] 的界标块
//! 幂等合并精神（「【观棋 N】…【/观棋 N】」逐手块同款）：**不覆盖用户
//! 注释、重复导出幂等**——但 [`crate::sgf::save::board_to_sgf`] 的合并
//! 函数是该模块私有实现且只作用于「根节点 / 逐手节点」两条固定路径，
//! 本模块要对**参照树的任意打分节点**合并，故在本地重写同语义的合并
//! （注释互指防漂移）；[`crate::sgf`] 目录按批次边界不改动。
//!
//! # 最重要的边界：参照文件本体绝不写回
//!
//! 合并在**克隆**的参照树副本上进行（[`GameTree`] 是纯数据结构，克隆
//! 即深拷贝），写盘只写用户在保存对话框里选定的**新路径**；原参照文件
//! 的路径在本模块根本不会出现。原子写（同目录临时文件 + `rename`）防
//! 半截文件。
//!
//! # 挂接键与未打分节点
//!
//! 打分结果按「树内路径」键（[`score_key`]，与
//! [`crate::refbook::RefEntry::path`] 同口径的下标串「0-2-1」）。合并时
//! 深搜副本树、按同一口径重算各节点的路径，命中结果表才写块——**未打
//! 分的节点不动**（扫描范围默认 ≤ 30 手，范围外的节点保持原样）；根
//! 节点不在任何结果里（打分只针对「有库中续法」的局面），自然不写。
//!
//! # 数字口径（与侧栏卡片 / dump 同源）
//!
//! 块内数字全部取自 [`RefNodeScore`] / [`RefContinuationScore`]，展示串
//! 复用 `eval_text` / `gap_text`（侧栏「参照打分」明细同一来源），导出
//! 内容与 dump 的 `ref_scan` 结果逐项一致。「引擎未列出」如实表述、
//! 不臆造数值；「这是 AI 对库的评估，不是库本身的对错」这层定位写进
//! 每个块（外部软件里没人再补这层说明）。

use std::path::Path;

use crate::sgf::{GameTree, Node as SgfNode, Property};

use super::refscan::{RefNodeScore, RefScanResults};

/// AI 评估块的**起始界标**。逐节点块挂在各自的 `C[]` 上，界标即「这是
/// 观棋为本节点写的块」的自证：重复导出按界标定位替换（幂等），界标
/// 外的用户文字一字不动。风格与 [`super::types::STATS_BLOCK_BEGIN`] /
/// `MOVE_BLOCK_MARK` 一致（`【观棋 …】` 家族）。
pub const AI_BLOCK_BEGIN: &str = "【AI 评估】";
/// AI 评估块的结束界标（见 [`AI_BLOCK_BEGIN`]）。
pub const AI_BLOCK_END: &str = "【/AI 评估】";

/// 结果表键：树内路径的下标串（「0-2-1」；根 = 空串）。
///
/// 与 `ui::analysis::refscan` 的私有 `score_key` 同构（该函数不 pub，
/// 重写一份并注释互指）：键语义 = [`crate::refbook::RefEntry::path`]
/// 各层下标的 join。
fn score_key(path: &[usize]) -> String {
    path.iter().map(ToString::to_string).collect::<Vec<_>>().join("-")
}

/// 一个节点的 AI 评估块正文（含首尾界标，整段合入该节点 `C[]`）。
///
/// 行文口径（措辞与侧栏「参照打分」明细一致，数字同源）：
/// - 首行：当次档位（visits/局面）+ 扫描日期 + 定位说明（AI 对库的
///   评估，外部软件里没人再补这层说明，块必须自证）；
/// - 引擎首选（参照外的事实）：`best` 为 `None`（空报告等）时如实写
///   「无候选」，不臆造；
/// - 每条库中续法的评估（含「引擎未列出」的如实表述）与判定（淘汰 /
///   仍可）；差距 = 行棋方视角目差（`gap_text` 同源），未列出无差距；
/// - 该节点的判定：仍可 / 部分过时 / 全部过时（淘汰数写实）。
pub fn ai_block_text(score: &RefNodeScore, date: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(AI_BLOCK_BEGIN.to_owned());
    lines.push(format!(
        "引擎评估（{} visits/局面，扫描于 {date}）：\
         这是 AI 对库的评估，不是库本身的对错。",
        score.visits,
    ));
    // 引擎首选（黑方视角原始值：存储口径恒黑视角，与 dump 一致）。
    lines.push(match &score.best {
        Some((at, wr, lead)) => format!(
            "引擎首选：{at}（黑方视角：胜率 {:.1}%，目差 {lead:+.1}）",
            wr * 100.0,
        ),
        None => "引擎首选：无候选（引擎未返回评估）。".to_owned(),
    });
    // 每条库中续法：评估 + 差距 + 判定。「引擎未列出」不判淘汰（与
    // 打分口径一致：不臆造数值，保守不标）。差距串 `gap_text` 自带
    // 「−N.N 目」形态（负号是它的展示符号，非负值语义），不再二次加工
    // ——早先手写 `-gap` 曾拼出「−-0.2 目」的双重负号。
    for cont in &score.continuations {
        let verdict = if cont.outdated { "已被 AI 淘汰" } else { "仍可" };
        lines.push(format!(
            "库中走法：{} —— {}，差距 {}（{verdict}）",
            cont.text,
            cont.eval_text(),
            cont.gap_text(),
        ));
    }
    // 节点判定（与 `fully_outdated` / `partially_outdated` 同一函数，
    // 侧栏 Top N 的口径）。
    let outdated = score.outdated_count();
    let verdict = if score.fully_outdated() {
        "全部过时"
    } else if score.partially_outdated() {
        "部分过时"
    } else {
        "仍可"
    };
    lines.push(format!(
        "本局面判定：{verdict}（{outdated}/{} 条库中走法被 AI 淘汰）",
        score.continuations.len(),
    ));
    lines.push(AI_BLOCK_END.to_owned());
    lines.join("\n")
}

/// 把一块**界标定界**的文本幂等合入节点 `C[]`（[`crate::sgf::save`]
/// 里 `merge_delimited_block` 的任意节点版，语义逐条对齐）：节点注释
/// 原有的用户内容**原样保留**，块以 `begin`…`end` 界标定位——
/// - 注释不存在 / 为空：新建注释，只含该块；
/// - 注释里没有块：在原注释与块之间以一个空行衔接后**追加**；
/// - 注释里已有块（重复导出）：**替换**界标区间内的全部内容（含两个
///   界标本身），区间外用户内容不动——绝不出现两份同界标块。
///
/// 节点可能存在多个 C 属性（罕见但合法）；写入目标取第一个，替换 /
/// 追加都在它身上进行，其余原样保留，避免重排用户数据。
fn merge_delimited_block(node: &mut SgfNode, begin: &str, end: &str, block: &str) {
    let pos = node.props.iter().position(|prop| prop.ident == "C");
    let Some(pos) = pos else {
        node.props
            .push(Property {
                ident: "C".to_owned(),
                values: vec![block.to_owned()],
            });
        return;
    };
    // 解析器保证属性至少有一个值；这里仍用 first_mut 兜底（与
    // save.rs 同款防御：不给未来某条构造路径留 panic 面）。
    let values = &mut node.props[pos].values;
    if values.is_empty() {
        values.push(String::new());
    }
    let Some(comment) = values.first_mut() else { return };
    if let Some(start) = comment.find(begin) {
        // 已有块：替换到结束界标为止（结束界标缺失时替换到注释末尾
        // ——半截块同样按幂等处理，不留下孤儿开头）。
        let tail_start = comment[start..]
            .find(end)
            .map_or(comment.len(), |off| start + off + end.len());
        let mut merged = String::with_capacity(comment.len() + block.len());
        merged.push_str(&comment[..start]);
        merged.push_str(block);
        merged.push_str(&comment[tail_start..]);
        *comment = merged;
    } else {
        // 没有块：追加（原注释与块之间空一行，保持可读）。
        comment.push_str(if comment.ends_with('\n') || comment.is_empty() {
            "\n"
        } else {
            "\n\n"
        });
        comment.push_str(block);
    }
}

/// 深搜副本树，把打分结果按路径键合并进各节点的注释。
///
/// 路径口径 = **统一树**（refscan 打分与 refbook 索引共用，`ui::ref_tree`
/// 同口径展开）：把 `GameTree` 折叠成「每节点一层下标」的节点树——
/// 链延续与分支都是「children 列表里的一项」，路径 = 每层在自己
/// children 列表里的下标；**无着点节点**（纯注释 / 空节点）不产生局面
/// 却照样占据一层下标（索引 path 口径：照实累积，见 walk.rs `walk`）。
/// 因此合并必须走同一展开：GameTree 的「主链 + 分支」两层结构若按
/// 「链内下标 + 分支下标」各开一层，链上第二个着法节点的路径就会比
/// 统一树口径**少一层**（headless 实测 12 节点只命中 4 个，正是这个
/// 漂移——固化为教训：口径以 refscan / refbook 为准，合并端对齐）。
///
/// 就地展开而不先克隆出统一树：GameTree 与统一树天然同构——`nodes`
/// 链上第 k 个节点（k ≥ 1）在统一树里 = 第 k−1 节点的 children 的
/// **唯一项**（链延续），分支子树 j = 末节点 children 的第 j 项。递归
/// 时按这个口径累积 path。
///
/// 结果表键为空串（refscan 对根局面理论上可产出——带摆子根无着法、
/// 实际不会出现；防御兜底）：命中空串键时把块合入根节点。
///
/// 返回写入的块数（= 命中结果表的节点数；用于导出成功提示）。
fn merge_tree(tree: &mut GameTree, results: &RefScanResults, date: &str) -> usize {
    let mut written = 0usize;
    // 防御兜底：空串键（根局面）的块合入根节点。refscan 的计划只对
    // 有着法的节点计 move_no，正常不会有此键；写上不亏（幂等）。
    if let Some(score) = results.get("")
        && let Some(root) = tree.nodes.first_mut()
    {
        merge_delimited_block(root, AI_BLOCK_BEGIN, AI_BLOCK_END, &ai_block_text(score, date));
        written += 1;
    }
    let mut path = Vec::new();
    // 统一树顶层恒单元素列表（根壳层），walk 对它 push(0)——链首的
    // 下标在这里预先放好。
    path.push(0);
    merge_chain_nodes(&mut tree.nodes, &mut tree.children, &mut path, results, date, &mut written);
    written
}

/// 合并一棵 GameTree 的主链与其分支（统一树口径）。
///
/// `path` 进入时**已含**链首节点自己的下标（顶层由 [`merge_tree`] 预先
/// push 根壳层的 0；分支由父层 push j）。链上第 k>0 个节点是统一树里
/// 「前节点 children 的唯一链延续」，再 push(0)；末节点的分支子树 j 是
/// 「末节点 children 的第 j 项」，push(j) 后递归（子树链首不再 push）。
/// 与 `walk` 的 `path.push(i)` / `path.pop()` 逐条对应。
fn merge_chain_nodes(
    nodes: &mut [SgfNode],
    children: &mut [GameTree],
    path: &mut Vec<usize>,
    results: &RefScanResults,
    date: &str,
    written: &mut usize,
) {
    let chain_len = nodes.len();
    for (k, node) in nodes.iter_mut().enumerate() {
        if k > 0 {
            // 链延续：统一树 children 里的唯一项（下标恒 0）。
            path.push(0);
        }
        if let Some(score) = results.get(&score_key(path)) {
            merge_delimited_block(node, AI_BLOCK_BEGIN, AI_BLOCK_END, &ai_block_text(score, date));
            *written += 1;
        }
    }
    // 分支子树：末节点的 children 各开一个下标；进入前 push(j)，子树
    // 链首（= 该分支的统一树节点本身）领这个下标，递归返回后 pop。
    for (j, child) in children.iter_mut().enumerate() {
        path.push(j);
        merge_chain_nodes(
            &mut child.nodes,
            &mut child.children,
            path,
            results,
            date,
            written,
        );
        path.pop();
    }
    // 离开本链：退掉**链延续**的下标（chain_len − 1 次）。链首下标是
    // 父层 push 的，由父层的 children 循环 pop——子树内部若把它一起
    // 退掉（曾写成 chain_len 次），父层第二个分支就会丢掉链首那层
    // 下标（headless 实测：children[1] 被查成「1」而非「0-1」，正中
    // 此坑——push 一次 pop 两次，兄弟分支路径整体少一层）。
    if chain_len > 0 {
        for _ in 0..(chain_len - 1) {
            path.pop();
        }
    }
}

/// 今日日期（`YYYY-MM-DD`）：块里的「扫描于」用。
///
/// 标准库无日期类型且本批不新增依赖：`SystemTime` 天数 + 公历换算
/// （Howard Hinnant 的 civil_from_days 算法，`chrono` 内部同款）。
fn today_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    // civil_from_days：把「自 1970-01-01 的天数」换算成年月日。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// 导出「AI 标注版库」：克隆参照树 → 合并打分块 → 序列化 → 原子写盘。
///
/// - **只写 `path`**（用户在保存对话框选定的另存路径）；原参照文件
///   的路径不经过本函数——「参照文件本体绝不写回」由签名保证；
/// - 返回写入的块数（成功提示给用户「标了几个局面」）；
/// - 原子写：先写同目录临时文件再 `rename`（save.rs 同款，写到一半
///   崩溃只损失临时文件）。
pub fn export_ai_annotated(
    path: &Path,
    source_tree: &GameTree,
    results: &RefScanResults,
) -> Result<usize, String> {
    let date = today_string();
    let mut copy = source_tree.clone();
    let written = merge_tree(&mut copy, results, &date);
    let text = copy.write();
    write_atomically(path, &text)?;
    Ok(written)
}

/// 原子落盘（save.rs `write_atomically` 的独立重写，注释互指）：
/// 先写**同目录**的临时文件（`原名.tmp`），再 `rename` 覆盖目标；
/// 失败时删除临时文件不留垃圾。错误转 `String`（调用方直接进消息区）。
fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    let name = path
        .file_name()
        .ok_or_else(|| "目标路径没有文件名".to_owned())?;
    let tmp = path.with_file_name(format!("{}.tmp", name.to_string_lossy()));
    let result = std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        // 临时文件可能尚未建出，清理失败（已不存在）无碍，静默即可。
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|err| format!("写入文件失败：{err}"))
}

/// headless / 取证：离线合并（不写盘），返回块数与合并后的树
/// （验证「幂等 / 用户注释保真 / parse_bytes 回读」用；应用路径不调用）。
#[doc(hidden)]
pub fn merge_preview(
    source_tree: &GameTree,
    results: &RefScanResults,
) -> (usize, GameTree) {
    let date = today_string();
    let mut copy = source_tree.clone();
    let written = merge_tree(&mut copy, results, &date);
    (written, copy)
}

/// 打分结果按手数排序的只读摘要（导出提示与验证对账用）：把
/// 结果表转成稳定序列（HashMap 遍历无序，展示与断言都需要稳定）。
pub fn sorted_scores(results: &RefScanResults) -> Vec<&RefNodeScore> {
    let mut scores: Vec<&RefNodeScore> = results.values().collect();
    scores.sort_by(|a, b| a.move_no.cmp(&b.move_no).then(a.path.cmp(&b.path)));
    scores
}
