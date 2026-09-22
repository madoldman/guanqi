//! KataGo analysis JSON 行协议：查询构造与响应解析（阶段 3.2）。
//!
//! 所有协议事实均来自 v1.18.2 引擎实测（见下方各条）：
//!
//! - 每个请求（含 terminate）都必须带字符串 `id`；响应按 `id` 回填。
//! - 没有 `quit` action；优雅关闭 = 关闭 stdin。
//! - `winrate` / `scoreLead` 一律为黑方视角（cfg `reportAnalysisWinratesAs = BLACK`），
//!   上层**不要**按行棋方翻转符号。
//! - 响应字段不封闭（含 `isSymmetryOf` / `edgeVisits` / `edgeWeight` 等）：
//!   反序列化一律宽容——未知字段忽略，可缺字段用 `Option`。
//! - `reportDuringSearchEvery` 为**查询级 JSON 字段**（v1.18.2 实测：作为
//!   cfg 配置键无效，作为查询字段有效）：查询里带上后，搜索期间约每 N 秒
//!   输出一条 `isDuringSearch: true` 的中间报告，**最后一条恒为
//!   `isDuringSearch: false` 的终态**（不带该字段则只有终态一条）。
//!   中间报告的 `rootInfo` / `moveInfos` 同样有效（visits 单调递增），
//!   `ownership` 等全部字段与终态同构。
//! - 引擎的搜索树缓存跨查询存活：同一局面再次查询（即使更小 `maxVisits`）
//!   会立刻返回（实测 <0.1s），上层据此避免重复查询。
//! - `ownership` 为 opt-in；排列与 [`crate::board::Coord::index`] 一致
//!   （`y*size + x`，`y=0` 为顶行），正值 = 黑势。
//! - `policy` 为 opt-in（`includePolicy: true`）：长度 = `size² + 1`，
//!   前 `size²` 项排列与 [`crate::board::Coord::index`] 一致（v1.18.2 b18
//!   下标标定实测：空盘 4 重对称 + 提子/布局局面的 argmax 与 moveInfos
//!   首选交叉验证 + 镜像组，`y*19+x` 映射唯一同时解释全部证据），
//!   **末位推定为弃着**（未确证，渲染层忽略）。概率全盘求和约 1，
//!   单点常在 1e-4～1e-1，渲染必须做相对刻度归一化才可见。
//! - `moveInfos[].ownership` 为 opt-in（`includeMovesOwnership`，复数
//!   Moves；字段名拼错时引擎会发**顶层未知字段警告**并照常分析该查询，
//!   只是字段不生效、没有数据，见下方 warning 条目）：
//!   v1.18.2 实测开启后每条报告（**含流式中间报告**）的每个候选点各带
//!   一份 361 float（19 路）数组 = 「走这一手之后」的领地图（正值黑势，
//!   与根 `ownership` 同一下标口径），搜索早期即可显示、无需等终态。
//!   代价：单条报告 3.6 KB → 34.2 KB（约 +3.4 KB/候选），300 visits
//!   流式一次约 340 KB（基线 9.4 倍）⇒ 必须只在该图层开启时 opt-in。
//! - 顶层未知字段的警告：引擎会发一条
//!   `{"field":"<名>","id":"<id>","warning":"Unexpected or unused field, …"}`
//!   并**随后照常分析该查询**（实测 warning 后仍收到全部正常报告）。
//!   宽容解析必须把它单独分流（[`Incoming::Warning`]），不能落入报告
//!   形态——否则缺省 `turn_number` / `is_during_search` 会被判成终态
//!   空报告，把仍在正常分析的查询静默打死。
//!   **两级行为要分清**（v1.18.2 实测）：顶层拼错 ⇒ 有 warning 可检测；
//!   **嵌套**拼错（如 move rule 里的 `until_depth`）⇒ **完全静默**，
//!   既不报错也不提示，只能靠「效果是否符合预期」发现（本项目踩过）。

use crate::board::{Action, Coord, Size, Stone};
use serde::{Deserialize, Serialize};

/// 一次分析查询的 id（引擎侧原样回传）。
///
/// 由桥接层单调递增分配，上层仅用于事件匹配，不能自行构造。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct QueryId(u64);

impl QueryId {
    pub(crate) fn new(n: u64) -> Self {
        Self(n)
    }

    pub(crate) fn raw(self) -> u64 {
        self.0
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        s.parse().ok().map(Self)
    }
}

impl std::fmt::Display for QueryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// 一次分析请求。
///
/// `moves` 为从初始盘面（恒空盘）到被分析局面的完整手数记录；
/// 弃着用 [`Action::Pass`] 表示（序列化为 GTP 的 `"pass"`）。
#[derive(Clone, Debug)]
pub struct AnalysisQuery {
    /// 棋盘尺寸。
    pub board_size: Size,
    /// 贴目。
    pub komi: f64,
    /// KataGo 规则串（如 `chinese` / `japanese` / `korean`），默认 `chinese`。
    pub rules: String,
    /// 完整手数（行棋方 + 着法），按时间顺序。
    pub moves: Vec<(Stone, Action)>,
    /// 查询级思考量上限；`None` 则省略字段、沿用 cfg 中的 `maxVisits`。
    /// 实测可覆盖 cfg 值。
    pub max_visits: Option<u32>,
    /// 是否返回 ownership（opt-in，缺省引擎不返回该字段）。
    pub include_ownership: bool,
    /// 是否返回 policy（策略网络先验，opt-in）。开启后每条报告（含流式
    /// 中间报告）增约 5 KB（362 个浮点的 JSON 文本，实测见模块文档），
    /// 必须只在策略热度图层开启时才请求。
    pub include_policy: bool,
    /// 是否返回**候选点级** ownership（opt-in，字段名 `includeMovesOwnership`
    /// 复数 Moves，实测见模块文档）。开启后每个候选点各带一份
    /// 「走这一手之后」的领地图（`MoveInfo::ownership`），供聚焦候选点时
    /// 显示后续领地。体积是 ownership/policy 之最（约 +3.4 KB/候选/报告），
    /// 必须只在对应图层开启时才请求；关闭时不得发送该字段（零开销）。
    pub include_moves_ownership: bool,
    /// 流式中间报告的输出间隔（秒）；`None` = 不开启，只回终态。
    /// 开启后搜索期间约每 N 秒一条 `isDuringSearch: true` 的中间报告，
    /// 最后仍有一条 `false` 终态（v1.18.2 实测，见模块文档）。
    pub report_during_search_every: Option<f32>,
    /// 显式指定要分析的 turn 列表（`None` = 只分析 `moves` 结束后的最终局面）。
    /// 实测引擎对每个 turn 独立搜索并逐个输出终态报告。
    pub analyze_turns: Option<Vec<usize>>,
    /// 查询优先级（整数，越大越优先被引擎调度）。v1.18.2 实测有效：
    /// 61-turn 批量查询占满队列时，带 `priority: 10` 的单 turn 查询可在
    /// 数秒内插队返回；不带则被完全阻塞（90 秒无响应，threads=1 与 4 一致）。
    /// 交互查询应高于批量查询（KaTrain 同款用法：base_priority + priority）。
    pub priority: i32,
    /// 选点限制（限定区域 = allowMoves / 排除选点 = avoidMoves）。
    /// 实测二者同时给出会被引擎拒绝（`Cannot specify both allowMoves and
    /// avoidMoves`），上层必须互斥。
    pub move_rules: Option<MoveRules>,
}

/// 一组选点限制。`allow` 与 `avoid` 不可同时非空（引擎实测显式报错）。
///
/// 语义（v1.18.2 实测）：
/// - `allow`：**只允许**这些点作为行棋方 [`Stone`] 的下一手；限制只作用于
///   「当前局面的下一手」时 `until_depth` 发 1。
/// - `avoid`：把这些点从该行棋方的候选中剔除（visits 全部分给其余点）。
/// - `moves` 为空时：allow 得到**空 moveInfos 的终态**（等于不允许任何点，
///   上层必须拦下），avoid 等于不限制；二者都不报错。
/// - 含已有棋子 / 自杀点：静默剔除，不报错。
#[derive(Clone, Debug, PartialEq)]
pub struct MoveRules {
    /// 限定区域（allowMoves）：`Some` 时 `avoid` 必须为空。
    pub allow: Option<MoveRule>,
    /// 排除选点（avoidMoves）：`Some` 时 `allow` 必须为空。
    pub avoid: Option<MoveRule>,
}

impl MoveRules {
    /// 「限定区域」规则（只允许 `moves` 中的点）。
    pub fn allow(player: Stone, moves: Vec<String>) -> Self {
        Self { allow: Some(MoveRule::new(player, moves)), avoid: None }
    }

    /// 「排除选点」规则（把 `moves` 从该行棋方的候选剔除）。
    pub fn avoid(player: Stone, moves: Vec<String>) -> Self {
        Self { allow: None, avoid: Some(MoveRule::new(player, moves)) }
    }
}

/// 单条 allowMoves / avoidMoves 规则的线上形态。
#[derive(Clone, Debug, PartialEq)]
pub struct MoveRule {
    /// 受限（或被排除）的行棋方。
    pub player: Stone,
    /// GTP 坐标串（[`Coord::to_gtp`] 口径；空数组语义见 [`MoveRules`]）。
    pub moves: Vec<String>,
    /// 限制持续的手数（ply，双方合计）：`untilDepth: 1` = 仅当前局面的
    /// 下一手。实测按全局 ply 计数而非该 player 自己的手数。
    pub until_depth: u32,
}

impl MoveRule {
    fn new(player: Stone, moves: Vec<String>) -> Self {
        Self { player, moves, until_depth: 1 }
    }
}

impl AnalysisQuery {
    /// 构造查询：默认贴目 7.5、中国规则、只分析最终局面。
    pub fn new(board_size: Size, moves: Vec<(Stone, Action)>) -> Self {
        Self {
            board_size,
            komi: 7.5,
            rules: "chinese".to_owned(),
            moves,
            max_visits: None,
            include_ownership: false,
            include_policy: false,
            include_moves_ownership: false,
            report_during_search_every: None,
            analyze_turns: None,
            priority: 0,
            move_rules: None,
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

/// 序列化用的线上结构（字段名对齐协议的 camelCase）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireQuery<'a> {
    id: String,
    /// 本项目初始恒为空盘；字段保留以符合协议形态。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    initial_stones: Vec<(String, String)>,
    moves: Vec<(&'a str, &'a str)>,
    rules: &'a str,
    komi: f64,
    board_x_size: u8,
    board_y_size: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_visits: Option<u32>,
    #[serde(skip_serializing_if = "is_false")]
    include_ownership: bool,
    #[serde(skip_serializing_if = "is_false")]
    include_policy: bool,
    /// 候选点级 ownership（opt-in）：字段名复数 Moves（实测，见模块文档）；
    /// 拼错会被引擎当作**顶层未知字段** ⇒ 发 warning 且照常分析该查询，
    /// 字段不生效、候选不带 ownership（该警告已由 [`Incoming::Warning`]
    /// 分流到界面提示，不会再静默）。
    #[serde(skip_serializing_if = "is_false")]
    include_moves_ownership: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    report_during_search_every: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    analyze_turns: Option<&'a [usize]>,
    /// 查询优先级（实测有效，见 [`AnalysisQuery::priority`]）；0 = 缺省不发送。
    #[serde(skip_serializing_if = "is_zero_i32")]
    priority: i32,
    /// allowMoves 与 avoidMoves 引擎实测互斥，至多出现其一。
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_moves: Option<Vec<WireMoveRule<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avoid_moves: Option<Vec<WireMoveRule<'a>>>,
}

/// 线上规则条目：坐标串借自 [`AnalysisQuery::move_rules`] 的字符串。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireMoveRule<'a> {
    player: &'a str,
    moves: &'a [String],
    until_depth: u32,
}

/// 组装 allowMoves / avoidMoves（规则组实测互斥，输出至多一个字段）。
fn wire_move_rules(
    rules: Option<&MoveRules>,
) -> (Option<Vec<WireMoveRule<'_>>>, Option<Vec<WireMoveRule<'_>>>) {
    match rules {
        Some(MoveRules { allow: Some(rule), .. }) => {
            (Some(vec![wire_move_rule(rule)]), None)
        }
        Some(MoveRules { avoid: Some(rule), .. }) => {
            (None, Some(vec![wire_move_rule(rule)]))
        }
        _ => (None, None),
    }
}

fn wire_move_rule(rule: &MoveRule) -> WireMoveRule<'_> {
    WireMoveRule {
        player: stone_tag(rule.player),
        moves: &rule.moves,
        until_depth: rule.until_depth,
    }
}

fn stone_tag(stone: Stone) -> &'static str {
    match stone {
        Stone::Black => "B",
        Stone::White => "W",
    }
}

/// 弃着 → `"pass"`，其余复用 [`Coord::to_gtp`]（GTP 列字母跳 `I`）。
fn action_gtp(action: Action, size: Size) -> String {
    match action {
        Action::Place(c) => c.to_gtp(size),
        Action::Pass => "pass".to_owned(),
    }
}

impl AnalysisQuery {
    /// 临时验证入口（examples/tmp_encode_check.rs 用，验证后随示例一并删除）。
    pub fn debug_encode(&self) -> String {
        self.encode(QueryId::new(0))
    }

    pub(crate) fn encode(&self, id: QueryId) -> String {
        // 先落所有 GTP 串，再组元组，避免借用临时值。
        let coord_strs: Vec<String> = self
            .moves
            .iter()
            .map(|(_, action)| action_gtp(*action, self.board_size))
            .collect();
        let moves: Vec<(&str, &str)> = self
            .moves
            .iter()
            .zip(&coord_strs)
            .map(|((stone, _), gtp)| (stone_tag(*stone), gtp.as_str()))
            .collect();
        let (allow_moves, avoid_moves) = wire_move_rules(self.move_rules.as_ref());
        let wire = WireQuery {
            id: id.raw().to_string(),
            initial_stones: Vec::new(),
            moves,
            rules: &self.rules,
            komi: self.komi,
            board_x_size: self.board_size.n(),
            board_y_size: self.board_size.n(),
            max_visits: self.max_visits,
            include_ownership: self.include_ownership,
            include_policy: self.include_policy,
            include_moves_ownership: self.include_moves_ownership,
            report_during_search_every: self.report_during_search_every,
            analyze_turns: self.analyze_turns.as_deref(),
            priority: self.priority,
            allow_moves,
            avoid_moves,
        };
        serde_json::to_string(&wire).unwrap_or_else(|_| {
            // 全部字段均为可序列化类型，理论上不可达；兜底避免 panic。
            "{\"id\":\"invalid\",\"error\":\"internal encode failure\"}".to_owned()
        })
    }
}

/// 附加请求（terminate / terminate_all / query_version）的统一编码。
pub(crate) enum ControlRequest {
    Terminate(QueryId),
    TerminateAll,
    /// `query_version` 诊断命令：协议完整实现的一部分，当前接线未使用。
    #[allow(dead_code)]
    QueryVersion,
}

impl ControlRequest {
    /// 协议要求所有请求（含控制类）都带字符串 `id`（实测缺失会被拒绝）。
    pub(crate) fn encode(&self, id: QueryId) -> String {
        let mut wire = serde_json::json!({ "id": id.raw().to_string() });
        match self {
            Self::Terminate(target) => {
                wire["action"] = "terminate".into();
                wire["terminateId"] = target.raw().to_string().into();
            }
            Self::TerminateAll => {
                wire["action"] = "terminate_all".into();
            }
            Self::QueryVersion => {
                wire["action"] = "query_version".into();
            }
        }
        wire.to_string()
    }
}

// ---- 响应解析（宽容：未知字段忽略，可缺字段 Option）----

/// 线上消息结构：一行 stdout 既可能是报告，也可能是错误或控制回显，
/// 因此所有字段都可缺省。
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireMessage {
    id: Option<String>,
    error: Option<String>,
    /// 顶层未知字段的警告报文正文（`"Unexpected or unused field, …"`），
    /// 与 `field`（被警告的字段名）成对出现；实测**警告后引擎照常分析**，
    /// 必须分流为非破坏性提示（见 [`Incoming::Warning`]）。
    warning: Option<String>,
    field: Option<String>,
    action: Option<String>,
    terminate_id: Option<String>,
    version: Option<String>,
    turn_number: Option<u64>,
    is_during_search: Option<bool>,
    no_results: Option<bool>,
    move_infos: Option<Vec<WireMoveInfo>>,
    root_info: Option<WireRootInfo>,
    ownership: Option<Vec<f32>>,
    policy: Option<Vec<f32>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireMoveInfo {
    r#move: Option<String>,
    pv: Option<Vec<String>>,
    winrate: Option<f64>,
    score_lead: Option<f64>,
    visits: Option<u64>,
    prior: Option<f32>,
    order: Option<u32>,
    lcb: Option<f32>,
    /// 「走这一手之后」的领地图（opt-in `includeMovesOwnership` 才有；
    /// 长度 = size²，下标与根 `ownership` 同口径）。缺省 `None` =
    /// 查询未开启该字段或引擎版本过旧，上层按无数据回落处理。
    ownership: Option<Vec<f32>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireRootInfo {
    current_player: Option<String>,
    visits: Option<u64>,
    winrate: Option<f64>,
    score_lead: Option<f64>,
    score_stdev: Option<f64>,
}

// ---- 上层使用的解析后类型 ----

/// 根节点统计。**`winrate` / `score_lead` 一律为黑方视角**（cfg
/// `reportAnalysisWinratesAs = BLACK` 已实测对 `rootInfo` 与 `moveInfos` 同时生效）。
#[derive(Clone, Debug, PartialEq)]
pub struct RootInfo {
    /// 被分析局面的行棋方。
    pub current_player: Stone,
    /// 搜索量。
    pub visits: u64,
    /// 黑方胜率 [0,1]。
    pub winrate: f64,
    /// 黑方目差（正 = 黑领先）。
    pub score_lead: f64,
    /// 目数波动估计（终局目差的标准差）。
    pub score_stdev: f64,
}

/// 一条候选点信息。`winrate` / `score_lead` 同样为黑方视角。
#[derive(Clone, Debug, PartialEq)]
pub struct MoveInfo {
    /// 候选点；`None` 为弃着。
    pub mv: Option<Coord>,
    /// 主变着序列；`None` 项为弃着。
    pub pv: Vec<Option<Coord>>,
    /// 走此手后的黑方胜率。
    pub winrate: f64,
    /// 走此手后的黑方目差。
    pub score_lead: f64,
    /// 该分支搜索量。
    pub visits: u64,
    /// 引擎先验。
    pub prior: f32,
    /// 引擎排序（0 = 引擎认为最优）。
    pub order: u32,
    /// 置信下界。
    pub lcb: f32,
    /// 「走这一手之后」的领地图（opt-in [`AnalysisQuery::include_moves_ownership`]
    /// 才有）：长度 = size²，下标与 [`AnalysisReport::ownership`] 一致，
    /// 正值 = 黑势。`None` = 查询未开启该字段（宽容解析缺省），上层必须
    /// 回落根局面数据而非报错。
    pub ownership: Option<Vec<f32>>,
}

/// 一次分析报告。
///
/// `no_results = true` 为引擎的空报告（实测：`analyzeTurns` 查询被 terminate
/// 打断时，未搜到的 turn 会输出 `{"noResults":true,...}`，无 rootInfo/moveInfos）。
#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisReport {
    /// 被分析的 turn 序号（0 = 初始盘面；缺省视为 `moves` 末尾）。
    pub turn_number: usize,
    /// 搜索是否仍在进行：查询开启 `reportDuringSearchEvery` 时中间报告为
    /// `true`，最后一条终态为 `false`（v1.18.2 实测，见模块文档）。
    pub is_during_search: bool,
    /// 空报告标记（无任何数据）。
    pub no_results: bool,
    /// 根节点统计；空报告或字段缺失时为 `None`。
    pub root_info: Option<RootInfo>,
    /// 候选点列表（已按引擎顺序，`order` 最小者在前不保证，见 `order` 字段）。
    pub move_infos: Vec<MoveInfo>,
    /// 各点局势值（opt-in 才有）：长度 = size²，下标与 [`crate::board::Coord::index`]
    /// 一致，正值 = 黑势，单位约为目。
    pub ownership: Option<Vec<f32>>,
    /// 策略网络先验（opt-in 才有）：长度 = size²+1，前 size² 项下标与
    /// [`crate::board::Coord::index`] 一致，末位推定为弃着（未确证，渲染忽略）。
    /// 值为归一化前的原始概率（全盘和约 1）。
    pub policy: Option<Vec<f32>>,
}

impl AnalysisReport {
    /// 终态判定辅助：与 [`super::EngineEvent::Report`] 的 `is_final` 同一
    /// 口径，供诊断 / 测试使用，当前主流程未调用。
    #[allow(dead_code)]
    pub(crate) fn is_final(&self) -> bool {
        !self.is_during_search
    }
}

/// 未解码坐标的报告（读线程产出；此时还不知道对应查询的棋盘尺寸）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawReport {
    pub turn_number: usize,
    pub is_during_search: bool,
    pub no_results: bool,
    pub root_info: Option<RootInfo>,
    pub move_infos: Vec<RawMoveInfo>,
    pub ownership: Option<Vec<f32>>,
    pub policy: Option<Vec<f32>>,
}

impl RawReport {
    pub(crate) fn is_final(&self) -> bool {
        !self.is_during_search
    }

    /// 按查询时的棋盘尺寸解码 GTP 坐标。
    pub(crate) fn decode(self, size: Size) -> AnalysisReport {
        AnalysisReport {
            turn_number: self.turn_number,
            is_during_search: self.is_during_search,
            no_results: self.no_results,
            root_info: self.root_info,
            move_infos: self
                .move_infos
                .into_iter()
                .map(|info| MoveInfo {
                    mv: info.mv.as_deref().and_then(|s| coord_of(size, s)),
                    pv: info.pv.iter().map(|s| coord_of(size, s)).collect(),
                    winrate: info.winrate,
                    score_lead: info.score_lead,
                    visits: info.visits,
                    prior: info.prior,
                    order: info.order,
                    lcb: info.lcb,
                    ownership: info.ownership,
                })
                .collect(),
            ownership: self.ownership,
            policy: self.policy,
        }
    }
}

/// [`RawReport::move_infos`] 的条目：坐标为 GTP 原文。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawMoveInfo {
    pub mv: Option<String>,
    pub pv: Vec<String>,
    pub winrate: f64,
    pub score_lead: f64,
    pub visits: u64,
    pub prior: f32,
    pub order: u32,
    pub lcb: f32,
    /// 候选点级领地图原文（opt-in 才有，[`RawReport::decode`] 原样透传）。
    pub ownership: Option<Vec<f32>>,
}

/// 一行 stdout 解码结果（坐标保持 GTP 原文，尺寸后补，见 [`RawReport::decode`]）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Incoming {
    /// 分析报告（按 id 关联）。
    Report { id: Option<QueryId>, report: RawReport },
    /// 引擎报告的错误（含请求格式错误；实测此时 `id` 可能为空）。
    Error { id: Option<QueryId>, message: String, field: Option<String> },
    /// 顶层未知字段的警告（实测警告后引擎**照常分析该查询**）：必须与
    /// [`Incoming::Error`] 严格分流——Error 会移除 pending 并打断在飞查询，
    /// 而警告只是「字段名可能拼错了，检查一下」，查询本身还在正常出报告，
    /// 按 Error 处理等于把仍在工作的查询静默打死。
    Warning { id: Option<QueryId>, field: Option<String>, message: String },
    /// terminate / terminate_all 的回显。
    TerminateEcho { id: Option<QueryId>, terminate_id: Option<String> },
    /// query_version 的应答。
    Version { id: Option<QueryId>, version: String },
}

fn stone_of(s: &str) -> Option<Stone> {
    match s {
        "B" | "b" => Some(Stone::Black),
        "W" | "w" => Some(Stone::White),
        _ => None,
    }
}

/// GTP 坐标串 → [`Coord`]（复用 board 层实现；`"pass"` 或非法串得 `None`）。
fn coord_of(size: Size, s: &str) -> Option<Coord> {
    Coord::from_gtp(size, s).ok()
}

impl WireMessage {
    fn into_incoming(self) -> Incoming {
        let id = self.id.as_deref().and_then(QueryId::parse);
        if let Some(message) = self.error {
            return Incoming::Error { id, message, field: self.field };
        }
        if let Some(action) = self.action {
            return match action.as_str() {
                "query_version" => Incoming::Version {
                    id,
                    version: self.version.unwrap_or_default(),
                },
                _ => Incoming::TerminateEcho { id, terminate_id: self.terminate_id },
            };
        }
        // warning 判定必须在构造报告**之前**：warning 报文缺省
        // `turn_number` / `is_during_search`，若落入报告形态会被
        // `RawReport::is_final` 判成终态空报告（is_during_search 缺省
        // false），把引擎仍在正常分析的查询当终态提前收掉（实测：
        // 警告后 10 条正常报告全部到达，一条都不能丢）。
        if let Some(message) = self.warning {
            return Incoming::Warning { id, field: self.field, message };
        }
        let report = RawReport {
            turn_number: self
                .turn_number
                .map_or(usize::MAX, |n| n.min(usize::MAX as u64) as usize),
            is_during_search: self.is_during_search.unwrap_or(false),
            no_results: self.no_results.unwrap_or(false),
            root_info: self.root_info.and_then(|root| {
                Some(RootInfo {
                    current_player: stone_of(root.current_player.as_deref()?)?,
                    visits: root.visits.unwrap_or(0),
                    winrate: root.winrate.unwrap_or(0.5),
                    score_lead: root.score_lead.unwrap_or(0.0),
                    score_stdev: root.score_stdev.unwrap_or(0.0),
                })
            }),
            move_infos: self
                .move_infos
                .unwrap_or_default()
                .into_iter()
                .map(|info| RawMoveInfo {
                    mv: info.r#move,
                    pv: info.pv.unwrap_or_default(),
                    winrate: info.winrate.unwrap_or(0.5),
                    score_lead: info.score_lead.unwrap_or(0.0),
                    visits: info.visits.unwrap_or(0),
                    prior: info.prior.unwrap_or(0.0),
                    order: info.order.unwrap_or(u32::MAX),
                    lcb: info.lcb.unwrap_or(0.0),
                    ownership: info.ownership,
                })
                .collect(),
            ownership: self.ownership,
            policy: self.policy,
        };
        Incoming::Report { id, report }
    }
}

/// 解析一行引擎 stdout（坐标未解码）。JSON 不合法时返回 `Err`
/// （上层降级为日志事件，不致命）。
pub(crate) fn parse_line(line: &str) -> Result<Incoming, String> {
    let wire: WireMessage =
        serde_json::from_str(line).map_err(|e| format!("无法解析引擎输出行：{e}; 原文: {line}"))?;
    Ok(wire.into_incoming())
}

