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
//! - v1.18.2 实测 `isDuringSearch` 恒为 `false`（渐进报告只在 GTP `kata-analyze`
//!   存在；analysis 引擎按 turn 逐个搜索、逐个输出终态）。解析层仍保留该字段，
//!   若未来版本输出 `true`，[`AnalysisReport::is_final`] 语义自动成立。
//! - 引擎的搜索树缓存跨查询存活：同一局面再次查询（即使更小 `maxVisits`）
//!   会立刻返回（实测 <0.1s），上层可用「同局面分段加深」实现渐进显示。
//! - `ownership` 为 opt-in；排列与 [`crate::board::Coord::index`] 一致
//!   （`y*size + x`，`y=0` 为顶行），正值 = 黑势。

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
    /// 显式指定要分析的 turn 列表（`None` = 只分析 `moves` 结束后的最终局面）。
    /// 实测引擎对每个 turn 独立搜索并逐个输出终态报告。
    pub analyze_turns: Option<Vec<usize>>,
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
            analyze_turns: None,
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
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
    #[serde(skip_serializing_if = "Option::is_none")]
    analyze_turns: Option<&'a [usize]>,
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
            analyze_turns: self.analyze_turns.as_deref(),
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
}

/// 一次分析报告。
///
/// `no_results = true` 为引擎的空报告（实测：`analyzeTurns` 查询被 terminate
/// 打断时，未搜到的 turn 会输出 `{"noResults":true,...}`，无 rootInfo/moveInfos）。
#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisReport {
    /// 被分析的 turn 序号（0 = 初始盘面；缺省视为 `moves` 末尾）。
    pub turn_number: usize,
    /// 搜索是否仍在进行（v1.18.2 analysis 引擎实测恒为 `false`，见模块文档）。
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
}

impl AnalysisReport {
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
                })
                .collect(),
            ownership: self.ownership,
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
}

/// 一行 stdout 解码结果（坐标保持 GTP 原文，尺寸后补，见 [`RawReport::decode`]）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Incoming {
    /// 分析报告（按 id 关联）。
    Report { id: Option<QueryId>, report: RawReport },
    /// 引擎报告的错误（含请求格式错误；实测此时 `id` 可能为空）。
    Error { id: Option<QueryId>, message: String, field: Option<String> },
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
                })
                .collect(),
            ownership: self.ownership,
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

