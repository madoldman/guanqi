//! 中文自动解说：把引擎终态报告与逐手历史拼成准确、克制的中文说明。
//!
//! 原则：每句话都能对应到具体字段，宁可少说也不编；数字未收敛
//! （流式中间报告）不产解说。口径说明（依据见各处注释与 /tmp/explain-notes.md）：
//!
//! - 「是否首选」：实际下一手在**当前局面终态报告**候选中的 `order`
//!   （官方文档：0 = 引擎认为最优，1 = 次优，以此类推）；
//! - 「损失」主口径 = 同一份终态报告中首选与该手的 `score_lead` 之差
//!   （黑方视角，按行棋方换算符号）——同一次搜索内两个分支的比较，
//!   不跨搜索；该手不在候选（visits 不足 / 被排除）时退回
//!   [`crate::ui::analysis::loss_from_points`] 的历史两点差
//!   （与失误统计同一口径），两端不齐则不报损失；
//! - 「不确定」判据 = 首选 `lcb`（该手胜率的置信下界，官方文档：与
//!   winrate 同单位）低于次选的 `winrate` 点估计——置信下界意义上
//!   首选不能排除与次选相当，即思考量不足以把两者分开；
//! - `prior`（策略网络先验概率）**不使用**：语义虽经官方文档确认，
//!   但单独展示容易被读成「引擎对好坏的判断」，当前句式没有与它
//!   严格对应的表述，跳过（克制原则）。

use crate::board::{Action, Board, Coord, Size, Stone};
use crate::engine::MoveInfo;
use crate::ui::analysis::{AnalysisState, Severity, loss_from_points};
use crate::ui::analysis_panel::pv_text;
// 搜索噪声分界（目）：复用失误分级「好棋/尚可」的同一常量，解说与
// 失误统计对「差距很小」的说法不会漂移。
use crate::ui::analysis::SEVERITY_GOOD_MAX;

/// 解说行的着色档位（只分三档：普通 / 正面 / 留意，不引入更多颜色）。
#[derive(Clone, Copy)]
pub(crate) enum Tone {
    /// 普通陈述。
    Normal,
    /// 正面结论（实际下一手就是首选、与首选差距在噪声内）。
    Good,
    /// 留意（明显亏损、引擎不确定）。
    Warn,
}

/// 解说的一行：文本 + 着色档位。
pub(crate) struct ExplainLine {
    pub text: String,
    pub tone: Tone,
}

/// 生成当前局面的中文解说，每行一条（侧栏「讲解」卡片逐行显示）。
///
/// 只认**终态**快照（`is_final`）；流式中间报告数字未收敛，一律占位。
pub(crate) fn explain(analysis: &AnalysisState, board: &Board) -> Vec<ExplainLine> {
    // 流式 / 无快照：占位，不产任何数字（中间报告未收敛，写出来是噪声）。
    let Some(snapshot) = analysis.snapshot.as_ref().filter(|s| s.is_final) else {
        let text = if analysis.analyzing() {
            "分析中…（数字未收敛，稍候）".to_owned()
        } else {
            "等待引擎分析后生成解说。".to_owned()
        };
        return vec![ExplainLine { text, tone: Tone::Normal }];
    };
    // 快照对应的局面与当前浏览点不一致（导航切换的一瞬）：不拿旧数字说话。
    if snapshot.turn != board.cursor() {
        return vec![ExplainLine {
            text: "局面已变化，分析更新中…".to_owned(),
            tone: Tone::Normal,
        }];
    }
    let Some(root) = snapshot.root.as_ref() else {
        return vec![ExplainLine {
            text: "引擎未返回数据，暂无解说。".to_owned(),
            tone: Tone::Normal,
        }];
    };

    let mut lines = Vec::new();

    // ---- 行 1：当前局面（轮到谁 + 黑方视角胜率/目差，与「胜率」卡片同口径）----
    let side_name = match root.current_player {
        Stone::Black => "黑方",
        Stone::White => "白方",
    };
    lines.push(ExplainLine {
        text: format!(
            "轮到{side_name}。引擎评估（{} visits）：黑方胜率 {:.1}%，目差 {:+.1}。",
            root.visits,
            root.winrate * 100.0,
            root.score_lead
        ),
        tone: Tone::Normal,
    });

    // 引擎首选：order 最小者（报告内候选已按 order 排序，这里仍显式取
    // order 字段，与「第 N 候选」的判定同一依据）。
    let best = snapshot.moves.iter().min_by_key(|info| info.order);

    // ---- 实际下一手评价：当前线（沿选中分支）上谱上真实存在的那一手 ----
    // 口径：`line_records()[turn]` 是被分析局面（第 `turn` 手后）的下一手。
    // 注意不能用 `records()`（截断到 cursor，get(turn) 恒为 None）。
    let next_record = board.line_records().get(snapshot.turn);
    if let Some(record) = next_record {
        // 弃着 → 候选表中的 mv == None。
        let actual = match record.action {
            Action::Place(c) => Some(c),
            Action::Pass => None,
        };
        let nth = snapshot.turn + 1;
        let info = snapshot.moves.iter().find(|info| info.mv == actual);
        // 「不是首选」的分支先在守卫里解开 Option，避免反复 expect。
        match info {
            Some(info) if best.is_some_and(|b| b.order < info.order) => {
                // 不是首选：给出「第 N 候选 + 首选 + 目差差（同报告口径）」。
                let best = best.expect("守卫已确认 order 更小的候选存在");
                let gap = move_gap(best, info, record.player);
                let best_text = best_label(best, snapshot.size);
                let (text, tone) = if gap <= SEVERITY_GOOD_MAX {
                    (
                        format!(
                            "第 {nth} 手 {} 是引擎第 {} 候选，与首选 {best_text} 差距很小（{gap:.1} 目，在搜索噪声内）。",
                            move_label(actual, snapshot.size),
                            info.order + 1
                        ),
                        Tone::Good,
                    )
                } else {
                    (
                        format!(
                            "第 {nth} 手 {} 是引擎第 {} 候选，较首选 {best_text} 目差低 {gap:.1} 目（同一次搜索内的比较）。",
                            move_label(actual, snapshot.size),
                            info.order + 1
                        ),
                        Tone::Warn,
                    )
                };
                lines.push(ExplainLine { text, tone });
            }
            Some(info) if info.order == 0 => {
                // 就是首选（order == 0，官方文档：0 = 引擎认为最优）。
                lines.push(ExplainLine {
                    text: format!(
                        "第 {nth} 手 {} 就是引擎首选（visits {}）。",
                        move_label(actual, snapshot.size),
                        info.visits
                    ),
                    tone: Tone::Good,
                });
            }
            Some(info) => {
                // 候选表有它但没有 order 更小者、order 又非 0（例如首选被
                // 排除后 order 重排的中间态）：不归入前两类，如实说明。
                lines.push(ExplainLine {
                    text: format!(
                        "第 {nth} 手 {} 在本轮引擎候选中（第 {} 候选）。",
                        move_label(actual, snapshot.size),
                        info.order + 1
                    ),
                    tone: Tone::Normal,
                });
            }
            None => {
                // 该手不在候选：visits 不足被剪枝或被排除。有历史两点差
                // （与失误统计同口径）时报目差下降，两端不齐就不报。
                let base = format!(
                    "第 {nth} 手 {} 不在本轮引擎候选中。",
                    move_label(actual, snapshot.size)
                );
                let points = analysis.line_points(board);
                // 闭包内先把两端的 Option<&HistoryPoint> 解开，再传给
                // loss_from_points（它是拷贝语义的小结构体）。
                let loss = match (points.get(snapshot.turn), points.get(snapshot.turn + 1)) {
                    (
                        Some(Some(before)),
                        Some(Some(after)),
                    ) => loss_from_points(nth, record.player, *before, *after),
                    _ => None,
                };
                let text = match loss {
                    Some(loss) if loss.score_loss > SEVERITY_GOOD_MAX => format!(
                        "{base}该手后目差较此前下降 {:.1} 目（与失误统计同口径，{}）。",
                        loss.score_loss,
                        loss.severity.name()
                    ),
                    Some(loss) => format!(
                        "{base}该手后目差变化 {:.1} 目，在搜索噪声内。",
                        loss.score_loss
                    ),
                    None => base,
                };
                let tone = match loss.map(|l| l.severity) {
                    Some(Severity::Questionable | Severity::Mistake | Severity::Blunder) => {
                        Tone::Warn
                    }
                    _ => Tone::Normal,
                };
                lines.push(ExplainLine { text, tone });
            }
        }
    } else if let Some(best) = best {
        // 停在末尾（无实际下一手）：只描述引擎首选，不预测评价。
        lines.push(ExplainLine {
            text: format!(
                "引擎首选 {}（胜率 {:.1}%，目差 {:+.1}）。",
                move_label(best.mv, snapshot.size),
                best.winrate * 100.0,
                best.score_lead
            ),
            tone: Tone::Normal,
        });
    }

    // ---- 不确定度：首选 lcb（胜率置信下界）低于次选 winrate 点估计 ----
    // 判据依据：官方文档 lcb = 该手胜率的 LCB、与 winrate 同单位；
    // 下界意义上首选仍不高于次选的点估计，说明搜索量不足以把两者分开。
    // 阈值本身无自由参数（直接比较两个给定字段），不凭印象设数。
    let second = best.and_then(|best| {
        snapshot
            .moves
            .iter()
            .filter(|i| i.order > best.order)
            .min_by_key(|i| i.order)
    });
    if let (Some(best), Some(second)) = (best, second)
        && f64::from(best.lcb) < second.winrate
    {
        lines.push(ExplainLine {
            text: format!(
                "首选胜率的置信下界（lcb {:.1}%）低于次选 {} 的胜率（{:.1}%）——在这个思考量下，引擎对两点的取舍并不确定。",
                best.lcb * 100.0,
                move_label(second.mv, snapshot.size),
                second.winrate * 100.0
            ),
            tone: Tone::Warn,
        });
    }

    // ---- 主变：首选候选的 PV 前缀（与候选点行 / 棋盘预览同一截断）----
    if let Some(best) = best
        && let Some(pv) = pv_text(&best.pv, snapshot.size)
    {
        lines.push(ExplainLine {
            text: format!("主变：{pv}"),
            tone: Tone::Normal,
        });
    }

    lines
}

/// 落点 / 弃着显示（GTP 坐标跳 I；弃着用「弃着」）。
fn move_label(mv: Option<Coord>, size: Size) -> String {
    mv.map_or_else(|| "弃着".to_owned(), |c| c.to_gtp(size))
}

/// 首选显示（落点 + 目差，黑方视角）。
fn best_label(info: &MoveInfo, size: Size) -> String {
    format!("{}（目差 {:+.1}）", move_label(info.mv, size), info.score_lead)
}

/// 实际下一手相对首选的目差差（行棋方视角，正 = 比首选亏）。
///
/// 口径：`winrate` / `score_lead` 一律黑方视角（`reportAnalysisWinratesAs
/// = BLACK` 对 moveInfos 生效，见 `engine` 模块注释），黑方行棋时
/// 差 = 首选目差 − 该手目差；白方行棋时符号相反。
fn move_gap(
    best: &MoveInfo,
    actual: &MoveInfo,
    player: Stone,
) -> f64 {
    let side = match player {
        Stone::Black => 1.0,
        Stone::White => -1.0,
    };
    (best.score_lead - actual.score_lead) * side
}
