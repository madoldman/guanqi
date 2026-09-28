//! 历史缓冲与统计派生：逐手胜率历史（局面签名键控）、失误汇总与
//! 局后统计（吻合度 / 最差 N 手），以及终态报告向历史的转存。

use std::collections::HashMap;

use crate::board::{Action, Board, Coord, Stone};
use crate::engine::{MoveInfo, RootInfo};

use super::types::{
    loss_from_points, hash_record, move_block_text, CandidateSnap, GameSummary, HistoryPoint,
    LossSummary, Severity, WorstMove, DEEP_VISITS_THRESHOLD, HISTORY_CAP, MATCH_MIN_MOVES,
    WORST_LIMIT,
};
use super::AnalysisState;

/// 一次终态报告的候选表提取结果（[`AnalysisState::candidates_from`]
/// 的返回）：候选 (落点, visits) 列表、总 visits（吻合度分母）、首选
/// 候选（落点 + 黑方视角胜率；弃着首选 = `None`）。
pub(super) type CandidateExtract = (Vec<(Coord, u64)>, u64, Option<(Coord, f64)>);

impl AnalysisState {
    /// 当前线（根到当前节点沿选中子分支）的逐手历史数据：下标 = 手数
    /// （0 = 初始空盘，长度 = `line_len() + 1`），`None` = 该手尚无终态数据。
    ///
    /// 每项按「根到该手着法前缀」的签名从历史缓冲取——同手数的不同分支
    /// 是不同局面，各取各的数据，互不干扰。O(线长) 滚动计算签名，
    /// 一帧至多调用一次（曲线、失误标注、侧栏汇总各调一次，可接受）。
    pub fn line_points(&self, board: &Board) -> Vec<Option<HistoryPoint>> {
        let records = board.line_records();
        let mut points = Vec::with_capacity(records.len() + 1);
        // 第 0 手局面（预设局面）的行棋方：取棋盘真实的根行棋方——
        // 让子 / 摆子局白先（HA[n] / PL[W]），硬编码黑会让第 0 点的
        // 交替视角换算错（白先局面第 0 点显示值应为 1−黑视角值）。
        points.push(
            self.history
                .get(&0xcbf2_9ce4_8422_2325)
                .copied()
                .map(|p| HistoryPoint { to_play: board.root_to_play(), ..p }),
        );
        let mut sig = 0xcbf2_9ce4_8422_2325;
        for record in records {
            hash_record(&mut sig, record);
            // 「走了该手之后」的行棋方 = 该手行棋方的对方。
            points.push(
                self.history
                    .get(&sig)
                    .copied()
                    .map(|p| HistoryPoint { to_play: record.player.opposite(), ..p }),
            );
        }
        points
    }

    /// 失误汇总统计：遍历当前线现场派生（每手仅两次哈希读取与算术，
    /// 可每帧调用）。「已分析」只计两端数据齐全的手数，缺口不计入分级。
    pub fn loss_summary(&self, board: &Board) -> LossSummary {
        let points = self.line_points(board);
        let mut summary = LossSummary {
            total: board.line_len(),
            ..LossSummary::default()
        };
        for (i, record) in board.line_records().iter().enumerate() {
            let (Some(before), Some(after)) = (points[i], points[i + 1]) else {
                continue;
            };
            let Some(loss) = loss_from_points(i + 1, record.player, before, after) else {
                continue;
            };
            summary.analyzed += 1;
            match loss.severity {
                Severity::Questionable => summary.questionable += 1,
                Severity::Mistake => summary.mistake += 1,
                Severity::Blunder => summary.blunder += 1,
                Severity::Good | Severity::Fine => {}
            }
        }
        summary
    }

    /// 局后统计：黑白吻合度与最差 N 手。吻合度为 LizzieYzy
    /// `percentsMatch` 口径（见 [`CandidateSnap`] 与模块内常量文档）；
    /// 排行榜为 LizzieYzy 差异手 `diffWinrate` 口径（按胜率损失降序），
    /// 损失与「失误」卡片同源（[`loss_from_points`]，走子前后历史点）。
    ///
    /// 匹配方向（/tmp/game-summary-notes.md §1 引擎实测钉死）：第 i 手
    /// （0 基）用 **turn = i** 的候选表匹配——`turnNumber=t` 报告的
    /// `moveInfos` 是「走了 t 手之后」局面（行棋方 = 第 t+1 手）的候选。
    ///
    /// 「未知」与「0」严格区分（项目铁律）：
    /// - 吻合度：`turn = i` 无候选表快照 ⇒ 该手**未分析**，不进分母；
    ///   有快照而实际落子不在表内 ⇒ 吻合度按 0 计（真实低吻合）。弃着
    ///   手的落点为 `None`，无法匹配落点：有快照时按 0 计（引擎候选表
    ///   本就不含弃着排序前列，实际弃着多半不合拍；诚实计 0，不臆造）。
    ///   有快照但算不出损失的手仍计入吻合度分母（两种口径的数据源独立）。
    /// - 排行榜：只收**走子前后历史点齐全、能算出损失**的手
    ///   （[`loss_from_points`] 返回 `None` 的不进榜、不臆造 0）。
    pub fn game_summary(&self, board: &Board) -> GameSummary {
        let records = board.line_records();
        let mut summary = GameSummary {
            total: records.len(),
            ..GameSummary::default()
        };
        // 吻合度按方累加；worst 只收「有快照且损失可算」的手再排序截断。
        let mut sum = (0.0f64, 0.0f64);
        // 深度 / 快扫构成计数（展示口径，见 GameSummary 字段文档）。
        let mut deep = (0usize, 0usize);
        let mut worst: Vec<WorstMove> = Vec::new();
        // 与 line_points 同款滚动签名：points[i] = 走 i 手前、points[i+1] =
        // 走 i 手后的历史点（None = 缺），损失取相邻两点差（同 loss_summary）。
        let points = self.line_points(board);
        let mut sig = 0xcbf2_9ce4_8422_2325;
        for (i, record) in records.iter().enumerate() {
            // 第 i 手的行棋方 = 该记录的 player；其候选表 = turn=i 局面
            // （走了 i 手之后）的签名，滚动推进（初始 = 空盘签名）。
            let snap = self.candidates.get(&sig);
            if let Some(snap) = snap {
                // 吻合度 = 实际落子在候选表里的 visits 占比；不在表内 = 0。
                let ratio = if snap.total_visits > 0 {
                    match record.action {
                        Action::Place(at) => snap
                            .moves
                            .iter()
                            .find(|(c, _)| *c == at)
                            .map_or(0.0, |(_, v)| *v as f64 / snap.total_visits as f64),
                        // 弃着无法匹配（见函数文档）：按 0 计。
                        Action::Pass => 0.0,
                    }
                } else {
                    0.0
                };
                // 构成判定：该手候选表同一次写入的历史点 visits 达到
                // [`DEEP_VISITS_THRESHOLD`] 即按「深度分析」计（历史点与
                // 候选表同签名同覆盖判定，root.visits 就是这次搜索的量）。
                let deep_point = points
                    .get(i + 1)
                    .and_then(|p| p.as_ref())
                    .is_some_and(|p| p.visits >= DEEP_VISITS_THRESHOLD);
                match record.player {
                    Stone::Black => {
                        summary.analyzed_black += 1;
                        sum.0 += ratio;
                        deep.0 += usize::from(deep_point);
                    }
                    Stone::White => {
                        summary.analyzed_white += 1;
                        sum.1 += ratio;
                        deep.1 += usize::from(deep_point);
                    }
                }
                // 排行榜候选：损失可算才进（任一端历史点缺失 = 未知，不臆造 0）。
                if let (Some(before), Some(after)) = (points[i], points[i + 1])
                    && let Some(loss) = loss_from_points(i + 1, record.player, before, after)
                {
                    worst.push(WorstMove {
                        turn: loss.turn,
                        player: record.player,
                        winrate_loss: loss.winrate_loss,
                        score_loss: loss.score_loss,
                        severity: loss.severity,
                        match_ratio: ratio,
                    });
                }
            }
            hash_record(&mut sig, record);
        }
        summary.match_black = if summary.analyzed_black > 0 {
            sum.0 / summary.analyzed_black as f64
        } else {
            0.0
        };
        summary.match_white = if summary.analyzed_white > 0 {
            sum.1 / summary.analyzed_white as f64
        } else {
            0.0
        };
        summary.deep_black = deep.0;
        summary.deep_white = deep.1;
        summary.enough_black = summary.analyzed_black >= MATCH_MIN_MOVES;
        summary.enough_white = summary.analyzed_white >= MATCH_MIN_MOVES;
        // 最差 N 手：胜率损失降序（LizzieYzy diffWinrate 口径），并列按
        // 手数升序（稳定可复现）；截断至 WORST_LIMIT。
        worst.sort_by(|a, b| {
            b.winrate_loss
                .partial_cmp(&a.winrate_loss)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.turn.cmp(&b.turn))
        });
        worst.truncate(WORST_LIMIT);
        summary.worst = worst;
        summary
    }

    /// 终态结果转存进逐手历史：键 = 局面签名（该报告对应的着法前缀）。
    /// 同一局面（同签名）visits 不低于旧条目才覆盖（深阶段后到、覆盖快阶段）；
    /// 不同局面各占一个键，同手数的分支互不覆盖。无根节点数据或
    /// 容量已满时跳过。
    ///
    /// **规则守卫**：报告的规则口径以在飞查询发起时记下的 `sent_rules`
    /// 为准（`game_rules` 参数只是签名可读性）；规则已在发起前经
    /// `sync` 的 want/sent 比对统一，这里无需再判。
    ///
    /// **候选表与 root 数据同一次报告写入**（同一覆盖判定、同进退）：
    /// 历史点的 visits 与候选表的 visits 必须是同一次搜索的产物，否则会
    /// 出现「root 是 300 visits 的、候选表是 40 visits 的」混合口径，
    /// 吻合度分母随之失真。`moves` 为该报告的候选点（丢弃弃着后取
    /// 落点与 visits），`total` 为候选表 visits 总和（分母）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_history(
        &mut self,
        turn: usize,
        root: &RootInfo,
        sig: u64,
        moves: &[(Coord, u64)],
        total_visits: u64,
        best: Option<(Coord, f64)>,
        game_rules: Option<&str>,
    ) {
        let _ = game_rules;
        if turn > HISTORY_CAP
            || self.history.len() >= HISTORY_CAP && !self.history.contains_key(&sig)
        {
            return;
        }
        // 行棋方随点存储：目数视角「黑白交替」显示时按点换算的依据
        // （根报告自带 current_player，与局面行棋方一致）。
        let point = HistoryPoint {
            turn,
            winrate: root.winrate,
            score_lead: root.score_lead,
            visits: root.visits,
            to_play: root.current_player,
        };
        let overwrite = match self.history.get(&sig) {
            Some(old) => root.visits >= old.visits,
            None => true,
        };
        if overwrite {
            self.history.insert(sig, point);
            // 候选表为空（引擎对满盘等局面可返回空表）也照写：空表让
            // 「该局面分析过、但无候选信息」可区分于「没分析过」。
            self.candidates.insert(
                sig,
                CandidateSnap {
                    moves: moves.to_vec(),
                    total_visits,
                    best,
                },
            );
        }
    }

    /// 由一份终态报告提取候选表快照所需的 (落点, visits) 列表与总 visits。
    /// 弃着（`mv == None`）不进表：它不占棋盘交叉点，永远不可能是
    /// 「实际落子」的匹配对象。同时提取**首选**候选（`moves.sort_by_key`
    /// 尚未进行时报告按引擎 order 序到达；这里独立取 order 最小者，
    /// 不依赖调用方的排序时机）——落点与胜率，弃着首选记 `None`。
    pub(super) fn candidates_from(infos: &[MoveInfo]) -> CandidateExtract {
        let moves: Vec<(Coord, u64)> = infos
            .iter()
            .filter_map(|info| info.mv.map(|at| (at, info.visits)))
            .collect();
        let total = moves.iter().map(|(_, v)| *v).sum();
        let best = infos
            .iter()
            .min_by_key(|info| info.order)
            .and_then(|info| info.mv.map(|at| (at, info.winrate)));
        (moves, total, best)
    }

    /// 逐手注释成文（「另存」时写回 SGF 用）：把当前线各手可算的损失
    /// 整理成「局面签名 → 注释块文本」表。键与载入端
    /// `GameMeta::comment_by_sig`、保存端滚动签名是同一套哈希——按
    /// **局面**而非手数挂接，树形谱里不同分支的同一手数不会互相顶替。
    ///
    /// 键语义：签名 = **走子后**局面（注释挂产生该局面的节点，与载入
    /// 端同键）；循环里先滚入该手再成文，`sig_before` 变量到使用点时
    /// 已是走子后签名（走子前签名只活在循环步内，用于查候选表）。
    ///
    /// 收录口径：
    /// - 只收 [`loss_from_points`] 能算出损失的手（两端历史点缺失 =
    ///   未知 = 不写，绝不写 0 或臆造）；
    /// - `all_hands == false`（默认）时再按 [`Severity::is_marked`]
    ///   过滤，只注疑问手及以上；`true` 时全部已分析手都注。
    /// - 一手也收不进（没分析过 / 数据两端不齐）返回**空表**——上层
    ///   对空表不写任何逐手注释，与「关闭开关」同效。
    ///
    /// 纯读函数（`&self`），另存路径随时可调；文本含界标（见
    /// [`move_block_text`]），重复另存的幂等合并由保存端负责。
    pub fn move_annotations(&self, board: &Board, all_hands: bool) -> HashMap<u64, String> {
        let size = board.size();
        let records = board.line_records();
        let points = self.line_points(board);
        let mut notes: HashMap<u64, String> = HashMap::new();
        // 滚动签名与 line_points 同款：进入第 i 轮时 sig_before = 走子前
        // 局面（候选表键），hash_record 一步后 = 走子后局面（注释键）。
        let mut sig = 0xcbf2_9ce4_8422_2325;
        for (i, record) in records.iter().enumerate() {
            let sig_before = sig;
            hash_record(&mut sig, record);
            let turn = i + 1;
            let (Some(before), Some(after)) = (points[i], points[i + 1]) else {
                continue; // 两端不齐：损失未知，不注（不臆造）
            };
            let Some(loss) = loss_from_points(turn, record.player, before, after) else {
                continue;
            };
            if !all_hands && !loss.severity.is_marked() {
                continue; // 默认档位：好棋 / 尚可不注
            }
            // 实战点（弃着如实写「弃着」）与引擎首选（走子前局面的
            // 首选候选；表缺失 / 首选为弃着 / 表为空都省略整段）。
            let actual = match record.action {
                Action::Place(at) => at.to_gtp(size),
                Action::Pass => "弃着".to_owned(),
            };
            let best = self
                .candidates
                .get(&sig_before)
                .and_then(|snap| snap.best)
                .map(|(at, winrate)| format!("{}（{:.1}%）", at.to_gtp(size), winrate * 100.0));
            notes.insert(sig, move_block_text(&loss, Some(&actual), best.as_deref()));
        }
        notes
    }

    /// 差异手报告条目（复盘报告导出用，[`super::super::report`]
    /// [`DiagramEntry`](crate::ui::report::DiagramEntry) 的取数端）：
    /// 排行榜 Top N 逐手给出实战点 / 引擎首选。实战点来自棋盘记录
    /// （弃着 = `None`）；**首选查走子前局面的候选表**——与
    /// [`Self::game_summary`] / [`Self::move_annotations`] 同款滚动签名
    /// （第 i 手的候选表键 = 走 i 手之前的前缀签名），表缺失 / 首选为
    /// 弃着 / 表为空都记 `None`（不臆造坐标）。`board_size` 随条目存储，
    /// 报告侧换算 GTP 坐标无需再借棋盘。
    pub fn diagram_entries(&self, board: &Board) -> Vec<crate::ui::report::DiagramEntry> {
        let size = board.size();
        let summary = self.game_summary(board);
        // 滚动签名与 game_summary 同款：进入第 i 轮时 sig = 走子前局面
        // （候选表键），hash_record 一步后滚到走子后（供下一轮作键）。
        let mut sig = 0xcbf2_9ce4_8422_2325;
        let mut best_by_turn: HashMap<usize, Option<Coord>> = HashMap::new();
        for (i, record) in board.line_records().iter().enumerate() {
            let best = self
                .candidates
                .get(&sig)
                .and_then(|snap| snap.best)
                .map(|(at, _)| at);
            best_by_turn.insert(i + 1, best);
            hash_record(&mut sig, record);
        }
        summary
            .worst
            .iter()
            .filter_map(|worst| {
                let record = board.record_at(worst.turn - 1)?;
                Some(crate::ui::report::DiagramEntry {
                    turn: worst.turn,
                    board_size: size,
                    player: record.player,
                    actual: match record.action {
                        Action::Place(at) => Some(at),
                        Action::Pass => None,
                    },
                    best: best_by_turn.get(&worst.turn).copied().flatten(),
                    severity: worst.severity,
                    score_loss: worst.score_loss,
                })
            })
            .collect()
    }
}
