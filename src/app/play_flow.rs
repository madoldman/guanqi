//! 人机对弈流程：新对局建立、难度切换、认输 / 超时 / 双弃着的终局
//! 判定与结果写回。
//!
//! 全部从原 `app.rs` 单体拆出，逐字节照搬：对局结果串、提示文案与
//! 终局评估加深（TERMINAL_EVAL_TARGET_VISITS）逻辑都不变。

use std::path::Path;

use super::{GuanqiApp, PendingConfirm, TERMINAL_EVAL_TARGET_VISITS};
use crate::board::Board;
use crate::sgf::GameMeta;
use crate::board::Stone;
use crate::engine::{Difficulty, save_settings};
use crate::play::{self, GameSetup, PlayState};
use crate::ui::analysis_panel::{self, LoadNotice};

impl GuanqiApp {
/// 开始新对局（新对局窗口「开始」按钮）：
/// 让子按标准星位预摆（白先），整体替换棋盘；清空分析快照与胜率
/// 历史（新对局不混旧曲线）；进入对弈模式并记录贴目。
/// 副本有研究成果时用户已在确认框里同意（见「新对局」入口）。
pub(super) fn start_new_game(&mut self, setup: GameSetup) {
    let handicap = setup.effective_handicap();
    let stones = play::handicap_stones(setup.size, handicap);
    // 让子 > 1 时白先（标准让子棋惯例）；摆子只摆黑方星位。
    let (black, white, to_play) = if handicap > 0 {
        (stones, Vec::new(), Stone::White)
    } else {
        (Vec::new(), Vec::new(), Stone::Black)
    };
    let board = Board::from_setup(setup.size, &black, &white, &[], to_play)
        .expect("新对局的尺寸与星位坐标均合法，构造必然成功");
    self.analysis.reset();
    self.analysis.clear_limits();
    self.clear_board_derived();
    // 副本同样随原谱离开（研究成果已在「新对局」入口确认过）。
    let kept_research = self.research_moves();
    self.others.clear();
    self.active_from_move = None;
    self.active_number = 0;
    self.pending_confirm = PendingConfirm::None;
    self.unsaved_confirm = None;
    self.board = board;
    // 棋盘整体替换：树布局指纹换代（与载谱同理）。
    self.tree_epoch = self.tree_epoch.wrapping_add(1);
    // 新对局自身是干净的空白内容：对齐基准，脏标记复位。
    self.mark_saved();
    // 元信息同样整体替换：旧棋谱的对局信息（含上一盘的结果 RE[] /
    // 双方 PB[]/PW[]）与逐手注释不能混进新对局的另存文件；让子数
    // 记入 info，「另存」才能写出 HA[n]（普通对局为 0，不写 HA）。
    // 对局双方按执子写入（任务 B：人机对弈的 SGF 缺 PB/PW 的补口）；
    // 时限写 TM/OT（TM = 包干或读秒主时间，OT = 读秒描述）。
    let mut meta = GameMeta::for_path(Path::new("guanqi.sgf"), setup.size);
    meta.info.komi = Some(setup.komi);        meta.info.handicap = handicap.min(9) as u8;
    let (human_name, engine_name) = match setup.human {
        Stone::Black => (&mut meta.info.player_black, &mut meta.info.player_white),
        Stone::White => (&mut meta.info.player_white, &mut meta.info.player_black),
    };
    *human_name = Some("人类".to_owned());
    *engine_name = Some("观棋(KataGo)".to_owned());
    let (time_limit, overtime) = time_meta_text(setup.time_system);
    meta.info.time_limit = time_limit;
    meta.info.overtime = overtime;
    // 规则写进 RU（规范名）：打完的棋谱不再丢规则。本局规则在对局
    // 开始时确定（与时限同一精神）——设置面板的规则偏好只作用于
    // 载入的棋谱，绝不覆盖本局查询口径。
    let rules_wire = setup.rules.wire().to_owned();
    meta.info.rules = Some(rules_wire.clone());
    self.loaded = Some(meta);
    self.komi = setup.komi;
    // 本局生效规则：新对局直接取对话框所选（new_game_rules），设置
    // 面板的 rules（None = 自动 / Some = 强制）不参与本局解析。
    self.active_rules = Some(rules_wire);
    // 时限制式与新对局规则是新对局设置的一部分：落位到引擎配置并
    // 持久化（与难度同一落点），下次新对局默认带出。
    if self.engine_cfg.time_system != setup.time_system
        || self.engine_cfg.new_game_rules != setup.rules
    {
        self.engine_cfg.time_system = setup.time_system;
        self.engine_cfg.new_game_rules = setup.rules;
        if let Err(text) = save_settings(&self.engine_cfg) {
            self.persist_notice = Some(text);
        } else {
            self.persist_notice = None;
        }
    }
    // 难度是新对局设置的一部分：落位到引擎配置并持久化（与侧栏切换
    // 同一落点），下一手应手即按该档搜索。
    self.set_difficulty(setup.difficulty);
    self.play = PlayState::new_game(&setup);
    self.last_frame = None;
    let size = setup.size;
    let desc = if handicap > 0 {
        format!("{size} 让{handicap}子")
    } else {
        size.to_string()
    };
    self.notices.push(LoadNotice::Ok(format!(
        "新对局已开始：{desc}，你执{}，难度{}（{} visits），时限{}。{}",
        setup.human.name(),
        setup.difficulty.name(),
        setup.difficulty.visits(),
        setup.time_system.name(),
        if kept_research > 0 {
            format!("（研究副本连同 {kept_research} 手研究成果已丢弃。）")
        } else {
            String::new()
        }
    )));
}

/// 切换人机对弈难度：写回引擎配置并持久化。引擎的走子查询按当前
/// 难度发起（`AnalysisState::sync` 逐帧比对），**下一手应手即生效**，
/// 无需重开对局。持久化失败不影响本运行内的生效值，只提示。
pub(super) fn set_difficulty(&mut self, difficulty: Difficulty) {
    if self.engine_cfg.play_difficulty == difficulty {
        return;
    }
    self.engine_cfg.play_difficulty = difficulty;
    if let Err(text) = save_settings(&self.engine_cfg) {
        self.persist_notice = Some(text);
    } else {
        self.persist_notice = None;
    }
}

/// 结束对局：写回对局结果（任务 B 的 RE[] 补口）并提示。
/// `result` 为 SGF 标准结果串（`B+R` / `W+T` / `B+3.5` / `?`…）。
/// 结果写进 `loaded.info.result`（新对局的元信息由 `start_new_game`
/// 整体重建，串不进下一盘；载入棋谱的对局结果原值被本盘结果覆盖
/// ——本盘确实结束了，覆盖是事实修正）。
/// 弃着终局的引擎判定（`Some` = 最深终态评估的 (visits, 黑方视角
/// scoreLead)；`None` = 无可用评估）同时写进根注释界标块
/// 「【观棋终局】」（幂等，见 `save::merge_delimited_block`）——存
/// meta 的 root_comment，另存路径把它合入 C[]。评估**就是**本盘
/// 结果（口径见 `terminal_result_string`），但明确标注非点目。
pub(super) fn finish_game(
    &mut self,
    result: String,
    text: String,
    evaluation: Option<(u64, f64)>,
) {
    // 规则名先取（借用于 loaded 之外），再进 meta 的可变借用。
    let rules_name = self.current_rules_name();
    if let Some(meta) = self.loaded.as_mut() {
        meta.info.result = Some(result);
        if let Some((_, lead)) = evaluation {
            // 写进**独立字段**而不是 root_comment：root_comment 是棋谱
            // 自带内容（载入的谱里可能有用户写的文字），另存时原样写回。
            meta.info.result_block =
                Some(crate::ui::analysis::result_block_text(lead, rules_name.as_deref()));
        }
    }
    self.notices.push(analysis_panel::LoadNotice::Ok(text));
}

/// 引擎认输（无望提示区「让引擎认输」按钮的执行体；认输方恒为
/// 引擎一方）：与人类认输共用同一条结束语义（`finish_game` 写 RE /
/// 提示，自动应手因 `resigned` 置位自然停止），不新写结束逻辑。
/// 结果串按胜方换算：引擎执黑认输 ⇒ `W+R`（人类执白中盘胜）。
pub(super) fn engine_resign(&mut self) {
    let loser = self.play.human.opposite();
    self.play.resigned = Some(loser);
    let winner = loser.opposite();
    let result = format!("{}+R", result_letter(winner));
    let text = format!(
        "{}认输：{}（{}）。对局已结束，可继续复盘浏览。",
        loser.name(),
        play::resign_text(loser),
        result
    );
    self.finish_game(result, text, None);
}

/// 当前生效规则的中文名（终局判定块的口径标注；取本局锁定规则或
/// 载入棋谱的解析规则）。
fn current_rules_name(&self) -> Option<String> {
    let wire = self.active_rules.clone().or_else(|| {
        self.loaded
            .as_ref()
            .and_then(|meta| meta.info.rules.as_deref())
            .map(|raw| crate::engine::resolve_rules(None, Some(raw)).rules)
    })?;
    crate::engine::Rules::from_wire(&wire).map(|r| r.name().to_owned())
}

/// 弃着终局的收尾：取当时可用的最深终局评估定 RE（无评估写 `?`），
/// 评估不足 [`TERMINAL_EVAL_TARGET_VISITS`] 时自动发起加深评估
/// （复用既有查询管线；到达后 [`refresh_terminal_result`] 更新）。
/// 界面提示语按有无评估分两态（评估中 / 终局判定）。
pub(super) fn finish_two_passes(&mut self) {
    // 手上评估已够深：直接定结果。
    let deepest = self.analysis.deepest_terminal_eval(&self.board);
    if let Some((visits, lead)) = deepest
        && visits >= u64::from(TERMINAL_EVAL_TARGET_VISITS)
    {
        let result = crate::ui::analysis::terminal_result_string(lead);
        let rules = self.current_rules_name().unwrap_or_else(|| "未知".to_owned());
        let side = if result == "0" {
            "和棋".to_owned()
        } else if result.starts_with('B') {
            "黑胜".to_owned()
        } else {
            "白胜".to_owned()
        };
        let text = format!(
            "对局结束：双方连续弃着。终局判定（引擎）：{side}，{result} · \
             按规则「{rules}」评估（{visits} visits），非点目结果。"
        );
        self.finish_game(result, text, deepest);
        return;
    }
    // 评估缺失 / 不够深：先按现状写（有浅评估也先给出暂定结果，
    // 无评估写 `?`），同时发起加深评估——到达后 refresh 更新 RE。
    let result = deepest
        .as_ref()
        .map(|(_, lead)| crate::ui::analysis::terminal_result_string(*lead))
        .unwrap_or_else(|| "?".to_owned());
    let text = if deepest.is_some() {
        "对局结束：双方连续弃着。终局评估中…（评估到达后更新结果）。".to_owned()
    } else {
        "对局结束：双方连续弃着。暂无引擎评估，SGF 记 RE[?]（评估中…）。".to_owned()
    };
    self.analysis
        .request_terminal_eval(&self.board, TERMINAL_EVAL_TARGET_VISITS);
    self.finish_game(result, text, deepest);
}

/// 加深评估到达后刷新弃着终局的结果（`logic` 每帧检查）：最深
/// 评估达标时更新 RE 与根注释块，并给出终局判定提示。
pub(super) fn refresh_terminal_result(&mut self) {
    if !self.play.finished(&self.board) || !play::two_passes(&self.board) {
        return;
    }
    let Some((visits, lead)) = self.analysis.deepest_terminal_eval(&self.board) else {
        return;
    };
    if visits < u64::from(TERMINAL_EVAL_TARGET_VISITS) {
        return;
    }
    let result = crate::ui::analysis::terminal_result_string(lead);
    let already = self
        .loaded
        .as_ref()
        .and_then(|meta| meta.info.result.as_deref())
        .is_some_and(|r| r == result);
    if already {
        return;
    }
    let rules = self.current_rules_name().unwrap_or_else(|| "未知".to_owned());
    let side = if result == "0" {
        "和棋".to_owned()
    } else if result.starts_with('B') {
        "黑胜".to_owned()
    } else {
        "白胜".to_owned()
    };
    let text = format!(
        "终局判定（引擎）：{side}，{result} · 按棋谱规则「{rules}」评估 \
         （{visits} visits），非点目结果。"
    );
    self.finish_game(result, text, Some((visits, lead)));
}

/// 超时判负的结束路径：与认输同一条结束语义（自动应手停止、进入
/// 复盘浏览），SGF 结果串用 `B+T` / `W+T`（时间超时）。
pub(super) fn finish_timeout(&mut self, loser: Stone) {
    self.play.timeout_loss = Some(loser);
    let winner = loser.opposite();
    let result = format!("{}+T", match winner {
        Stone::Black => "B",
        Stone::White => "W",
    });
    let text = format!(
        "{}方超时，{}方胜（{}）。对局已结束，可继续复盘浏览。",
        loser.name(),
        winner.name(),
        result
    );
    self.finish_game(result, text, None);
}
}

/// 时限制式的 SGF 元信息文本（`TM` / `OT`）：包干与读秒的主时间都进
/// `TM`；读秒描述（每手秒数 × 次数）进 `OT`。无限制不写（`None`，
/// 与 SGF 惯例一致——未配置时限的谱不落 TM/OT）。
fn time_meta_text(system: play::TimeSystem) -> (Option<String>, Option<String>) {    match system {
        play::TimeSystem::Unlimited => (None, None),
        play::TimeSystem::Absolute { seconds } => {
            (Some(format!("{} 分钟", (seconds / 60.0).round() as u64)), None)
        }
        play::TimeSystem::Byoyomi { main_seconds, period_seconds, periods } => (
            Some(format!("{} 分钟", (main_seconds / 60.0).round() as u64)),
            Some(format!("{} 次 × {} 秒读秒", periods, period_seconds.round() as u64)),
        ),
    }
}

/// 胜方的 SGF 结果串字母（`B+R` / `W+T` 等的 `B` / `W` 部分）。
pub(super) fn result_letter(winner: Stone) -> &'static str {
    match winner {
        Stone::Black => "B",
        Stone::White => "W",
    }
}
