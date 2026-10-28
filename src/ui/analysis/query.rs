//! 查询机制：查询口径（展示 / 走子）、在飞跟踪、引擎事件消费与查询
//! 组装（规则 / 视角 / 贴目 / opt-in 字段 / 优先级 / 时间预算）。

use std::time::Instant;

use crate::board::{Action, Board, MoveRecord, Stone};
use crate::engine::{
    resolve_rules, AnalysisQuery, AnalysisReport, Difficulty, Engine, EngineConfig, EngineError,
    EngineEvent, QueryId, WinrateView,
};

use super::batch::INTERACTIVE_PRIORITY;
use super::display::DisplayView;
use super::limits::move_rules_of;
use super::thinking;
use super::types::{position_sig, EngineStatus, Snapshot};
use super::{AnalysisState, Waker, LOG_TAIL_LINES, REPORT_EVERY_SECS};

/// 查询口径：常规展示分析（配置值 visits + 流式中间报告），另有对局走子
/// 的独立口径。原「快 / 深两段式」已由流式中间报告取代（查询级
/// `reportDuringSearchEvery`，v1.18.2 实测），不再需要低 visits 预热。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Stage {
    /// 常规展示分析：visits = 配置值，开启流式中间报告。
    Analysis,
    /// 人机对弈的走子查询：按**难度档位**的 visits 完整搜索，不开启流式
    /// （走子只认终态，中间报告无意义）。与展示口径分开的理由：
    /// - 走子报告不进展示快照（`Snapshot`），避免侧栏/叠加层随难度值变浅；
    /// - 轮到引擎时**先发 Play 再补展示查询**：若展示查询先按配置值搜过，
    ///   引擎的搜索树跨查询存活，随后按更低难度值的查询会立刻用满缓存
    ///   返回，难度形同虚设（实测 protocol.rs 注明「同局面再次查询即使更小
    ///   maxVisits 也立刻返回」）。
    Play,
}

impl Stage {
    /// 该口径的 visits 上限。配置值为 0 时夹到 1，避免无效查询。
    fn cap(self, cfg: &EngineConfig) -> u32 {
        let visits = cfg.visits.max(1);
        match self {
            Self::Analysis => visits,
            // 走子口径永远按**难度值**（任务口径：难度低于展示值时按难度
            // 搜完走子再补展示；高于展示值时同样以难度为准）。
            Self::Play => cfg.play_difficulty.visits().max(1),
        }
    }

    /// 展示口径开启流式中间报告；走子口径只认终态，不开。
    fn streaming(self) -> bool {
        match self {
            Self::Analysis => true,
            Self::Play => false,
        }
    }
}

/// 在飞查询。
pub(super) struct Inflight {
    pub(super) id: QueryId,
    pub(super) turn: usize,
    pub(super) stage: Stage,
    pub(super) started: Instant,
    /// 查询发起时的局面签名（手数记录前缀）。走子口径的查询以此判断
    /// 「该查询还算不算当前局面的应手依据」——对弈中难度可被用户随时
    /// 改变，改难度后旧口径的终态报告不得再驱动应手。
    pub(super) sig: Vec<MoveRecord>,
}

/// 引擎「顶层未知字段」警告的一条用户可见记录（按字段名去重，见
/// [`AnalysisState::engine_warnings`]）。
#[derive(Clone, Debug)]
pub struct EngineWarning {
    /// 引擎不认识的顶层字段名（`None` = 报文未带，按原文整体去重）。
    pub field: Option<String>,
    /// 展示文本（已拼入引擎原文与处置说明）。
    pub text: String,
}

/// 终局评估任务的状态（见 [`AnalysisState::terminal_eval`]）。
pub(super) struct TerminalEval {
    /// 终局面的签名（局面变了任务即失效，防旧局面评估串到新盘面）。
    pub(super) sig: Vec<MoveRecord>,
    /// 目标 visits（加深线，建议 800–1000；取 800 与默认展示档的
    /// 4 倍量级，收敛噪声远低于半目）。
    pub(super) target_visits: u32,
    /// 迄今收到的最深终态评估（黑方视角 scoreLead，visits 更高者胜出）。
    pub(super) best: Option<(u64, f64)>,
}

impl AnalysisState {
    /// 引擎「顶层未知字段」警告（**引擎重启时清除**，按字段名去重）。
    /// 「消息」卡片读取；无警告返回空切片。
    pub fn engine_warnings(&self) -> &[EngineWarning] {
        &self.engine_warnings
    }

    /// 最近一次实际发出的查询原文（含真实 id；headless 验证工具取证用，
    /// 见 [`crate::engine::Engine::last_request`]）。引擎未启动时 `None`。
    pub fn last_query(&self) -> Option<&str> {
        self.handle.as_ref().and_then(|handle| handle.last_request())
    }

    /// 引擎 stderr 最近几行（`LOG_TAIL_LINES` 条以内；诊断缓冲，当前
    /// 侧栏只显示最后一行，此切片供错误态展开展示）。
    pub fn log_tail_slice(&self) -> impl Iterator<Item = &str> {
        self.log_tail.iter().map(String::as_str)
    }

    // ---- 规则 / 目数视角（口径变更：重发 + 清历史）----

    /// 当前期望的分析规则设置（`None` = 自动跟随棋谱；侧栏引擎卡片显示用）。
    pub fn want_rules(&self) -> Option<&str> {
        self.want_rules.as_deref()
    }

    /// 设置规则偏好（App 随设置面板保存写入）。**只记偏好**：不清历史、
    /// 不作废快照——若解析后的生效规则没变（如自动跟随下换谱、或设置值
    /// 与解析结果本就相同），既有数据仍然同口径可用；生效规则真的变了
    /// 时，`request` 时刻的比对会清空历史并出提示（见 `data_rules`）。
    pub fn set_want_rules(&mut self, rules: Option<String>) {
        self.want_rules = rules;
    }

    /// 目数视角（显示口径）。
    pub fn display_view(&self) -> DisplayView {
        self.want_view
    }

    /// 取走分析偏置（激进度 PDA）口径变化的提示（「历史数据已清空」；
    /// App 转入消息区）。机制与 [`Self::take_rules_notice`] 同款。
    pub fn take_bias_notice(&mut self) -> Option<String> {
        self.bias_notice.take()
    }

    /// 设置目数视角（App 随设置面板 / 侧栏切换写入）。变化即作废快照，
    /// `sync` 比对 `want/sent` 后重发查询；历史数据不清空——存储恒黑
    /// 视角（SIDETOMOVE 报文在引擎桥接层已归一化），同一份数据两种
    /// 显示视角两用。
    pub fn set_display_view(&mut self, view: DisplayView) {
        if self.want_view == view {
            return;
        }
        self.want_view = view;
        // 旧快照的显示数字按旧视角读出，作废后由重发查询刷新
        // （与 set_want_policy 同一机制）。
        self.snapshot = None;
    }

    /// 取走规则解析提示（「未识别规则串」「规则已切换，历史数据已清空」
    /// 等；App 转入消息区显示）。先收 `effective_rules` / `clear_rule_history`
    /// 经 slot 落下的新提示，再整体取走。
    pub fn take_rules_notice(&mut self) -> Option<String> {
        if let Some(pending) = self.rules_notice_slot.take() {
            self.rules_notice = Some(pending);
        }
        self.rules_notice.take()
    }

    /// 规则变化 / 版本递增时的历史清空：逐手历史 + 候选表 + 当前快照
    /// 一并作废（快照的数字也是按旧规则算的）。只在**实际生效规则**
    /// 变化时调用（见 `data_rules` 与 `request` 内比对）。
    fn clear_rule_history(&mut self) {
        self.history.clear();
        self.candidates.clear();
        self.snapshot = None;
        self.rules_notice_slot
            .set(Some("规则已切换，历史数据已清空。".to_owned()));
    }

    /// 解析本帧生效的规则（设置显式指定 > 棋谱 `RU[]` 宽容映射 > 默认），
    /// 并把「未识别」提示落进 `rules_notice`（已有提示时保留，等 App 取走
    /// 后再落新的）。返回发给引擎的规范名。
    ///
    /// 借用设计：提示槽走 `Cell`（`&self` 内部可变），让本函数能在
    /// `request()` 已持有引擎句柄可变借用（`self.handle.as_mut()`）时
    /// 被调用——规则串必须在查询组装点解析，而句柄借用要活到
    /// `handle.analyze(query)`。AnalysisState 只在 UI 线程使用，无并发。
    pub(super) fn effective_rules(&self, game_rules: Option<&str>) -> String {
        let resolution = resolve_rules(self.want_rules.as_deref(), game_rules);
        if resolution.notice.is_some() {
            self.rules_notice_slot.set(resolution.notice);
        }
        resolution.rules
    }
    /// 用当前配置（重）启动引擎；设置面板「应用并重启」与错误重试共用。
    /// 任何失败都进入可显示的状态，不 panic。
    pub fn start_engine(&mut self, cfg: &EngineConfig, waker: &Waker) {
        if let Some(old) = self.handle.take() {
            old.shutdown();
        }
        self.inflight = None;
        self.analyzed_sig = None;
        self.play_sig = None;
        self.snapshot = None;
        self.transient_error = None;
        // 引擎重启即清除历史警告：警告对应的是**旧引擎进程**看到的查询
        // 环境（字段拼写 / 引擎版本），重启后旧提示不再可信（保留策略
        // 见 `on_warning` 文档）。
        self.engine_warnings.clear();
        self.engine = if cfg.model_path.is_some() {
            match Engine::spawn_with_waker(cfg, Some(waker.clone())) {
                Ok(handle) => {
                    self.handle = Some(handle);
                    EngineStatus::Starting
                }
                // 权重路径失效（文件被删）等启动前错误同样进可显示状态，
                // 不区分于进程启动失败。
                Err(err) => EngineStatus::Failed(err.to_string()),
            }
        } else {
            EngineStatus::Unconfigured
        };
    }

    /// 手动重试当前局面的查询（消息区「重试分析」按钮）：作废已发送
    /// 口径（签名 / 规则 / 贴目 / 限制版本），下一帧 `sync` 比对不一致
    /// 即重发。引擎进程保持运行——瞬时错误（超时 / 被拒）不影响进程，
    /// 无需重启引擎。清掉瞬时错误避免重试前重复显示。
    pub fn retry_query(&mut self) {
        self.analyzed_sig = None;
        self.sent_rules = None;
        self.sent_komi = None;
        self.transient_error = None;
    }

    /// 是否有在飞查询（就绪 + 在飞 = 界面显示「分析中」）。
    pub fn analyzing(&self) -> bool {
        self.inflight.is_some()
    }

    // ---- 内部 ----

    pub(super) fn on_event(&mut self, event: EngineEvent, board: &Board, cfg: &EngineConfig, komi: f64, game_rules: Option<&str>) {
        match event {
            EngineEvent::Ready => {
                self.engine = EngineStatus::Ready;
                self.transient_error = None;
                self.request(board, cfg, Stage::Analysis, komi, game_rules, None);
            }
            EngineEvent::Report {
                id,
                report,
                is_final,
            } => {
                // 批量在飞时报告先按批量 id 归属：批量查询与常规查询的 id
                // 空间互斥（引擎按 id 回传），命中批量即只回填历史不落快照。
                // 参照打分（refscan，阶段 22.3）是第三个独立 id 归属：命中
                // 即回填打分结果表，同样不落展示快照。
                if self
                    .ref_scan
                    .as_ref()
                    .is_some_and(|job| job.active.as_ref().is_some_and(|chunk| chunk.id == id))
                {
                    let Some(mut job) = self.ref_scan.take() else {
                        return;
                    };
                    let chunk_done = Self::on_ref_scan_report(self, &mut job, id, report);
                    self.ref_scan = Some(job);
                    if chunk_done {
                        // 块收齐：关闭在飞并立即派发下一块（sync 每帧的
                        // 派发与完成判定兜底推进，这里先派少空转一帧）。
                        if let Some(job) = self.ref_scan.as_mut() {
                            job.active = None;
                        }
                        self.dispatch_ref_scan_chunk();
                    }
                    return;
                }
                if self
                    .batch
                    .as_ref()
                    .is_some_and(|job| job.active.as_ref().is_some_and(|chunk| chunk.id == id))
                {
                    // 先把任务从 self 里取出来再回填，规避 self 双重可变借用
                    //（回填要访问 self.record_history / self.history）。
                    let Some(mut job) = self.batch.take() else {
                        return;
                    };
                    let chunk_done = Self::on_batch_report(self, &mut job, id, report);
                    if chunk_done {
                        // 块收齐：关闭在飞。主扫描最后一个块收齐时转入加深
                        // 阶段——按「胜率损失降序」取前 N 手差异手（与侧栏
                        // 局后统计完全同一口径：排序复用 game_summary 的
                        // diffWinrate 排序、损失取自 [`loss_from_points`]），
                        // 用更高的 visits 重扫那些局面的走子前后两端；历史
                        // 回填「visits 更高才覆盖」自动完成替换，无需新机制。
                        // 无可加深（未开加深 / 无损失可算的手）时直接收尾。
                        job.active = None;
                        if !job.deepening
                            && let Some(turns) = self.plan_deepen(&job, board)
                        {
                            job.deepen_turns = turns;
                            job.deepening = true;
                        }
                        self.batch = Some(job);
                        self.dispatch_batch_chunk();
                        // 主扫描完成（未转加深）时立即收尾；转了加深的完成
                        // 判定由 sync 每帧复查（加深派发完毕且收齐）。
                        if let Some(job) = self.batch.as_ref()
                            && !job.deepening
                            && job.group >= job.plan.groups.len()
                            && job.done >= job.plan.positions
                        {
                            self.finish_batch(true);
                        }
                    } else {
                        self.batch = Some(job);
                    }
                    return;
                }
                self.on_report(id, report, is_final, board, cfg, game_rules);
            }
            EngineEvent::Log(line) => {
                // 尾行环形缓冲 + 单行显示（诊断可见性：崩溃时至少一行原因）。
                if self.log_tail.len() == LOG_TAIL_LINES {
                    self.log_tail.pop_front();
                }
                self.log_tail.push_back(line.clone());
                self.last_log = Some(line);
            }
            EngineEvent::Warning { id, field, message } => {
                self.on_warning(id, field, message);
            }
            EngineEvent::Failed(err) => self.on_failed(err),
            EngineEvent::Exited(status, stderr_tail) => {
                self.inflight = None;
                // 引擎退出批量随之失效（错误状态已可见，不再另发提示）。
                self.batch = None;
                // 临终 stderr 尾行并入错误文案：崩溃原因（OpenCL 报错 /
                // 配置非法 / panic）几乎总在 stderr 尾部，只报退出码用户
                // 无从查起。行数由 engine 层的 STDERR_TAIL_LINES（8 行）定。
                let mut message = format!("引擎进程已退出（{status}）");
                for line in &stderr_tail {
                    message.push_str("\n　");
                    message.push_str(line);
                }
                self.engine = EngineStatus::Failed(message);
            }
        }
    }

    /// 查询级失败不影响引擎进程；其余（启动超时等）进入可重试错误状态。
    fn on_failed(&mut self, err: EngineError) {
        self.inflight = None;
        match err {
            EngineError::QueryTimeout { .. } | EngineError::QueryRejected { .. } => {
                self.transient_error = Some(err.to_string());
            }
            other => self.engine = EngineStatus::Failed(other.to_string()),
        }
    }

    /// 引擎「顶层未知字段」警告：**非破坏性**——实测引擎发完警告后照常
    /// 分析该查询（全部报告正常到达），这里只落用户可见提示，绝不能走
    /// [`Self::on_failed`]（那会清 `inflight`，把仍在工作的查询打死）。
    /// 按字段名去重：流式查询每个中间报告都可能触发一次警告，
    /// 同一字段只保留首条，避免消息卡片刷屏。
    ///
    /// **保留策略**：会话内保留、**引擎重启时清除**（见 `start_engine`）。
    /// 理由：警告源于「查询字段与引擎能力不匹配」，重启意味着用户改了
    /// 配置（设置面板「应用并重启」）或换了一次引擎进程——旧警告对应
    /// 的查询环境已不存在，保留只会误导；同一会话内不改配置则保留，
    /// 用户始终能看到「配置可能拼错了」的提醒。
    fn on_warning(&mut self, id: Option<QueryId>, field: Option<String>, message: String) {
        let _ = id; // 警告不改变任何查询状态，id 仅用于展示定位
        if self.engine_warnings.iter().any(|w| w.field == field) {
            return;
        }
        let name = field.as_deref().unwrap_or("（未知字段）");
        self.engine_warnings.push(EngineWarning {
            field: field.clone(),
            text: format!(
                "引擎不认识查询里的字段「{name}」（可能拼错了）。\
                 本次分析仍会照常进行，但该字段不会生效。引擎原文：{message}"
            ),
        });
    }

    /// 接收报告：按 id 丢弃过期补发，落快照；流式中间报告实时刷新展示，
    /// 终态关闭在飞并转存逐手历史。
    fn on_report(
        &mut self,
        id: QueryId,
        report: AnalysisReport,
        is_final: bool,
        board: &Board,
        cfg: &EngineConfig,
        game_rules: Option<&str>,
    ) {
        let Some(inflight) = self.inflight.as_ref() else {
            return;
        };
        if inflight.id != id {
            return; // 旧查询被 terminate 后的补发终态：局面已变，丢弃
        }
        let (stage, turn, started) = (inflight.stage, inflight.turn, inflight.started);
        let moves = report.move_infos;
        let root = report.root_info;
        // 候选表快照在 moves 被 move 进快照前提取（终态才用得到，
        // 但提前提取成本可忽略：表长 ≤ 候选数）。
        let (candidates, candidates_total, candidates_best) = Self::candidates_from(&moves);
        // 思考过程记录：**只记中间报告**（终态照旧写快照 / 逐手历史——
        // 记录只描述过程，终态是结果），且只在展示口径（走子不开流式，
        // 本就无中间报告；条件冗余但语义显式）。放在 moves 被 move 进
        // 快照之前，借引用蒸馏，O(1) 分配 / 条，不拖慢报告路径。
        if stage == Stage::Analysis && !is_final {
            let entry = thinking::ThinkingEntry::from_report(
                started.elapsed(),
                root.as_ref().map_or(0, |root| root.visits),
                &moves,
            );
            self.thinking.record(entry);
        }
        let snapshot = Snapshot {
            turn,
            size: board.size(),
            visits_cap: stage.cap(cfg),
            is_final,
            elapsed: started.elapsed(),
            root: root.clone(),
            moves,
            ownership: report.ownership,
            policy: report.policy,
        };
        // 走子口径落位：只认终态（中间报告对决策无意义，守卫见
        // `play::engine_move_decision` 的 is_final 判断）。局面签名随查询
        // 记录，难度变更由 `sync` 的 want_play 比对触发重新查询。
        if stage == Stage::Play && is_final {
            self.play_snapshot = Some(snapshot.clone());
            self.play_sig = Some((inflight.sig.clone(), cfg.play_difficulty));
        }
        // 展示快照实时更新（含流式中间报告）：胜率 / 目差 / 候选点 / 热度图
        // 随搜索推进刷新。走子口径的查询不覆盖展示快照，侧栏不随难度值变浅。
        if stage == Stage::Analysis {
            self.snapshot = Some(snapshot);
        }
        // 仅终态转存进逐手历史：局面未变时 visits 不降者胜。
        // 曲线与失误分析不被流式中间值反复改写。
        if is_final && let Some(root) = &root {
            // 在飞报告的 turn 恒等于发起查询时的游标，局面未变即当前线的全部着法。
            let sig = position_sig(board.records());
            // 候选表随 root 同一次写入（口径一致，见 record_history 文档）；
            // moves 已按 order 排序，快照顺序即引擎序。
            self.record_history(
                turn,
                root,
                sig,
                &candidates,
                candidates_total,
                candidates_best,
                game_rules,
            );
            // 终局评估回填：任务在、局面匹配、visits 不低于已记录值才
            // 覆盖（与 history 同一覆盖规则；评估任务是纯读方，不改
            // 展示 / 走子口径的任何状态）。
            if let Some(job) = self.terminal_eval.as_mut()
                && job.sig.eq(board.records())
                && job.best.as_ref().is_none_or(|(visits, _)| root.visits >= *visits)
            {
                job.best = Some((root.visits, root.score_lead));
            }
        }
        if !report.no_results {
            self.transient_error = None;
        }
        if is_final {
            self.inflight = None;
        }
    }

    /// 引擎当前是否应该应手、走子依据是否已就绪（`App::logic` 转接用）。
    ///
    /// 返回「该局面按当前难度完整搜索」的终态报告；`None` = 尚未就绪
    /// （查询在飞 / 未发起 / 难度刚变 / 局面已变）。**决策必须取自这里
    /// 而非展示快照**：展示口径可以是流式中间报告，拿来走子会让难度设置
    /// 形同虚设（且中间结果远未收敛）。走子口径查询不开流式，
    /// `Snapshot.is_final` 恒为真，终态守卫仍由决策函数保留兜底。
    pub fn play_snapshot(&self, board: &Board, difficulty: Difficulty) -> Option<&Snapshot> {
        let (sig, d) = self.play_sig.as_ref()?;
        if *d != difficulty || !sig.eq(board.records()) {
            return None;
        }
        self.play_snapshot.as_ref()
    }

    /// 走子口径的查询是否在飞（侧栏「引擎思考中」的判定依据之一：
    /// 展示快照已齐但走子查询未回时，引擎实际仍在为应手思考）。
    pub fn play_pending(&self, board: &Board, difficulty: Difficulty) -> bool {
        self.play_snapshot(board, difficulty).is_none()
            && self.inflight.as_ref().is_some_and(|inflight| {
                inflight.stage == Stage::Play && inflight.sig.eq(board.records())
            })
    }

    /// 对当前局面发起查询（就绪且无在飞时才生效）。`komi` 与解析后的
    /// 规则随查询发给引擎；视角按当前显示口径随 `overrideSettings`
    /// 显式发送（报告在引擎桥接层归一化回黑视角入库）。`max_time` 为
    /// 给引擎的时间预算（秒；`None` = 不限时）：**只用于走子口径**——
    /// 对局制式下的 AI 应手截止线（`overrideSettings.maxTime`，与难度
    /// 档 maxVisits 先到为准）；展示 / 批量查询绝不携带，时限概念不
    /// 泄漏进复盘分析。
    pub(super) fn request(
        &mut self,
        board: &Board,
        cfg: &EngineConfig,
        stage: Stage,
        komi: f64,
        game_rules: Option<&str>,
        max_time: Option<f64>,
    ) {
        if self.inflight.is_some() {
            return;
        }
        // 规则先在取句柄之前解析（借用分离）：`effective_rules` 只读
        // `want_rules`，与引擎句柄的可变借用互斥。
        let rules = self.effective_rules(game_rules);
        // 分析偏置（PDA）的数据口径判定：PDA 直接扭曲胜率 / 目差（实测
        // ±2 → 目差 ±8–10 目），不同 PDA 的历史点不可混在一条曲线上——
        // 与规则口径（`data_rules`）同一纪律。首次产生口径（程序刚启动 /
        // 载谱后首查）不算「切换」。WRN 不参与（评估漂移在噪声内）。
        if self.data_bias != Some(cfg.playout_doubling_advantage) {
            if self.data_bias.is_some() {
                self.history.clear();
                self.candidates.clear();
                self.snapshot = None;
                self.bias_notice =
                    Some("分析偏置（激进度）已变化，历史数据已清空。".to_owned());
            }
            self.data_bias = Some(cfg.playout_doubling_advantage);
        }
        // 生效规则相对**数据口径**变化 ⇒ 旧历史（曲线 / 候选表）是别的
        // 规则算的，必须清空并提示。比对放这里（查询组装点）而非设置
        // 写入点：设置变化不必然改变生效规则（自动跟随下换谱同理）。
        if self.data_rules.as_deref() != Some(rules.as_str()) {
            if self.data_rules.is_some() {
                // 首次产生数据口径（程序刚启动 / 载谱后首查）不算「切换」。
                self.clear_rule_history();
            }
            self.data_rules = Some(rules.clone());
        }
        let view = match self.want_view {
            DisplayView::Black => WinrateView::Black,
            DisplayView::Alternating => WinrateView::SideToMove,
        };
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        let moves: Vec<(Stone, Action)> = board
            .records()
            .iter()
            .map(|record| (record.player, record.action))
            .collect();
        let mut query = AnalysisQuery::new(board.size(), moves);
        // 预设摆子（让子 / 摆子谱的 AB/AW）随查询发给引擎：`records()` 按
        // 设计只含预设之后的真实着法，摆子不在 moves 里，漏发它引擎看到的
        // 就是另一个（少子、先后手错位）局面。从根盘面反推（同一事实来源，
        // 见 `Board::root_setup`）；空盘局为空、线上省略字段。
        let (setup_black, setup_white) = board.root_setup();
        query.initial_stones = setup_black
            .into_iter()
            .map(|at| (Stone::Black, at))
            .chain(setup_white.into_iter().map(|at| (Stone::White, at)))
            .collect();
        query.komi = komi;
        // 规则：设置显式指定 > 棋谱 RU 宽容映射 > 默认 chinese。解析出的
        // 恒为规范名（绝不把 SGF 原始串发给引擎）；解析提示已由
        // `effective_rules` 落进消息区。
        query.rules = rules;
        // 视角：显示口径（黑视角时显式发 BLACK，钉死「一律黑视角入库」
        // 的解析前提，不依赖用户 cfg 里的值）。
        query.view = view;
        query.max_visits = Some(
            // 终局评估查询的 visits 覆盖：任务在、局面匹配、目标高于
            // 展示配置时按目标发（目标必 ≥800，展示档默认 300；不达标
            // 的任务在 sync 派发处已被 visits 判定拦住，这里是执行点）。
            self.terminal_eval
                .as_ref()
                .filter(|job| job.sig.eq(board.records()) && stage == Stage::Analysis)
                .map(|job| job.target_visits.max(stage.cap(cfg)))
                .unwrap_or_else(|| stage.cap(cfg)),
        );
        // 热度图需要 ownership（opt-in，引擎缺省不返回该字段）：协议实测
        // 中间报告同样携带（每条约 7–9 KB，0.5s 键下 300 visits 共约 11 条，
        // 见 /tmp/stream-notes.md），开销可忽略，热度图因此也能边搜边显示。
        query.include_ownership = true;
        // policy 只在策略热度图层开启时才请求（opt-in）：362 个浮点使每条
        // 流式报告增约 5 KB（300 visits 整次查询约 +50 KB，实测见
        // /tmp/policy-notes.md），关闭时必须零开销。开关切换由 `sync` 的
        // want/sent 比对触发重发（局面未变也重发）。
        query.include_policy = self.want_policy;
        // 候选点级 ownership 同理（opt-in 之最重）：每个候选点 × 361 float
        // 使单条报告 3.6 KB → 34.2 KB（约 +3.4 KB/候选，实测见模块文档），
        // 只在「候选点领地」图层开启时才请求；关闭时零开销。
        query.include_moves_ownership = self.want_moves_ownership;
        // PV 每手计算量（opt-in）：紧凑整型数组（每项 1–3 字节 × 候选数 ×
        // PV 长度），增量远小于 ownership/policy；仍按 opt-in 纪律只在
        // 「候选点悬停显示 PV visits」开启时请求（开关入口在
        // 「分析 → 叠加层」），关闭时省略字段、零开销。实测与缺失语义见
        // `engine::protocol` 模块文档「PV 每手计算量」条目。
        query.include_pv_visits = self.want_pv_visits;
        // 展示口径开启流式中间报告（边搜边刷新界面）；走子口径只认终态。
        if stage.streaming() {
            query.report_during_search_every = Some(REPORT_EVERY_SECS);
            // 新的展示查询开始：思考记录清空重记（它描述的是「这一次
            // 搜索」，见 `thinking` 模块文档）。放在 `stage.streaming()`
            // 判断内 = 只有开流式的查询重置记录；走子口径与批量查询
            // （都不经此分支）绝不误清——对弈时「先 Play 后补展示」，
            // 若 Play 也清，展示查询的记录会在落位前被走子查询抹掉。
            self.thinking.begin_search();
        }
        // 选点限制（限定区域 / 排除选点）随查询发给引擎，展示与走子口径
        // 都受限（区域模式研究局部时，引擎应手也应在局部走才自然）。
        query.move_rules = move_rules_of(&self.limits, board);
        // 交互查询带高优先级：整谱快扫占满引擎队列时，实测不带 priority 的
        // 交互查询会被无限期阻塞（90 秒无响应），带 10 可数秒内插队返回
        // （/tmp/batch-notes.md §3/§7）。
        query.priority = INTERACTIVE_PRIORITY;
        // 时间预算只进走子口径（对局制式下的 AI 应手截止线）：与视角同在
        // 唯一的 overrideSettings 对象里发送（见 protocol 模块文档）。
        if stage == Stage::Play {
            query.max_time = max_time;
        }
        // 分析偏置（激进度 / 分析广度）：恒随查询显式发送（0 = 引擎默认），
        // 值来自设置（cfg），变化由 sync 的 sent_bias 比对触发重发。
        query.playout_doubling_advantage = cfg.playout_doubling_advantage;
        query.wide_root_noise = cfg.wide_root_noise;
        let query_rules_after = query.rules.clone();
        let id = handle.analyze(query);
        self.analyzed_sig = Some(board.records().to_vec());
        self.sent_epoch = self.limits_epoch;
        self.sent_policy = self.want_policy;
        self.sent_moves_ownership = self.want_moves_ownership;
        self.sent_pv_visits = self.want_pv_visits;
        self.sent_bias = Some((cfg.playout_doubling_advantage, cfg.wide_root_noise));
        // 规则 / 视角 / 贴目随查询定格：want/sent 比对的基准（规则变化在下次
        // sync 比对中触发重发；视角与贴目同理）。
        self.sent_rules = Some(query_rules_after);
        self.sent_view = self.want_view;
        self.sent_komi = Some(komi);
        self.inflight = Some(Inflight {
            id,
            turn: board.cursor(),
            stage,
            started: Instant::now(),
            sig: board.records().to_vec(),
        });
    }
}
