//! 棋谱载入：SGF 游戏树 → [`Board`]（「打开棋谱」的数据层）。
//!
//! 职责边界与容错策略：
//!
//! - 递归载入**整棵游戏树**：按 SGF 惯例，每层的第一个子树是主变，其余
//!   子树按序挂成变着分支（[`Board::play`] 在非叶节点落子即建分支，已存在
//!   的着法直接切换）；载入完成后定位到开局第 0 手，默认当前线即主变；
//! - 根节点 `AB` / `AW` / `AE` 构成预设局面，经 [`Board::from_setup`]
//!   进入棋盘（不计手数、不参与劫争，见 `board` 模块文档）；`PL` 决定
//!   首着方，缺省时按 FF4 惯例：有摆子则白先，否则黑先；
//! - 其后逐节点取 `B` / `W` 着点（空值与 `tt` 为弃着），用
//!   [`Board::play`] / [`Board::pass`] 重放；中途节点的摆子属性不支持，
//!   忽略并在提示中说明（不静默丢数据）；无行棋属性的纯注释节点跳过，
//!   其 `C` 注释随节点丢弃——**丢弃条数计入载入提示**（不静默丢，见
//!   [`LoadedGame::dropped_comments`]）；
//! - **非法着法不整体失败**：主变上遇到即停止重放该线，保留已载入部分
//!   （[`LoadedGame::partial`] = true）；变着分支上遇到只放弃该分支，
//!   其它分支与主变不受影响；两类情况连同中途摆子一并汇入
//!   [`LoadedGame::warning`]（「第 N 手无法载入：原因」「第 N 手（第 k 个
//!   变着内）无法载入：原因」）；只有解析期错误（编码 / 结构 / 根属性非法 /
//!   预设摆子非法）才整体失败（返回 [`LoadError`]）；
//! - 逐手注释以**局面签名**（根到该节点着法路径的 FNV-1a，见
//!   `position_sig`）为键存于 [`GameMeta`]：树形谱中不同分支的同一手数
//!   是不同局面，按手数索引不再成立；局面签名保证「同一局面无论经主变
//!   还是变着到达，注释都对应得上」。查询经 [`GameMeta::comment_at`]，
//!   UI 无需关心键结构。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::tree::{GameInfo, GameTree, Node};
use super::{SgfError, parse_bytes};
use crate::board::{Action, Board, SetupError, Size, Stone};
/// 载入整体失败（此时**不应替换**用户当前棋盘）。
#[derive(Debug)]
pub enum LoadError {
    /// SGF 解析或根属性错误（[`SgfError`] 的 Display 已含中文与行列）。
    Parse(SgfError),
    /// 预设摆子非法。解析期已拦下越界坐标，实际仅黑白冲突可达，罕见。
    Setup(SetupError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(err) => write!(f, "{err}"),
            Self::Setup(err) => write!(f, "预设摆子非法：{err}"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(err) => Some(err),
            Self::Setup(err) => Some(err),
        }
    }
}

/// 载入后留在应用里的棋谱元信息（棋盘本体由 App 单独持有）。
///
/// `Clone` 供「研究副本」复制元信息：副本与原谱共享同一批局面签名
/// （副本由原谱当前线前缀重放而来），逐手注释因此自动跟随；本类型
/// 无外部可变状态，克隆是纯数据拷贝。
#[derive(Clone)]
pub struct GameMeta {
    /// 来源路径（原样回显，供显示与将来「另存」复用）。
    pub source: PathBuf,
    /// 根节点对局信息（`PB` / `PW` / `RE` / `KM` / `DT` / `HA` 等）。
    pub info: GameInfo,
    /// 逐手注释：键 = 局面签名（根到该节点着法路径的 FNV-1a，见
    /// `position_sig`）。不按手数索引的理由见模块文档。
    comments: HashMap<u64, String>,
    /// **未识别属性**的保全存储：键与 [`Self::comments`] 同一套局面签名，
    /// 值为该节点上我们不写回的属性原样列表（另存时原样追加，另存不丢
    /// 标记 / 死子标注 / 第三方扩展属性）。收集口径见 [`collect_extras`]；
    /// 属性顺序、同名多值、值内容（转义已解码）全部原样保留。
    extras: HashMap<u64, Vec<super::tree::Property>>,
}

impl GameMeta {
    /// 空棋谱元信息（空盘「另存为」后记录档案路径用）：无对局信息、
    /// 无注释，尺寸按当前棋盘。
    pub fn for_path(source: &Path, size: Size) -> Self {
        Self {
            source: source.to_path_buf(),
            info: GameInfo {
                size,
                komi: None,
                handicap: 0,
                initial_player: None,
                player_black: None,
                player_white: None,
                rank_black: None,
                rank_white: None,
                team_black: None,
                team_white: None,
                result: None,
                date: None,
                event: None,
                round: None,
                place: None,
                game_name: None,
                rules: None,
                time_limit: None,
                overtime: None,
                application: None,
                charset: None,
                format: None,
                root_comment: None,
            result_block: None,
            },
            comments: HashMap::new(),
            // 新建（非载入）的棋谱没有历史属性可保全：空表 = 无第三方属性，
            // 另存路径零开销。
            extras: HashMap::new(),
        }
    }

    /// 当前局面（`board` 游标处）的注释；无注释返回 `None`。
    ///
    /// 键为「根到游标节点的着法序列」的签名：主变与变着各自的手数可能
    /// 相同而局面不同，且 `Board::undo` 会重排节点 id，二者都不可作键。
    pub fn comment_at(&self, board: &Board) -> Option<&str> {
        self.comments
            .get(&position_sig(board.records()))
            .map(String::as_str)
    }

    /// 按局面签名取注释（「另存为」还原逐手注释用）。
    pub fn comment_by_sig(&self, sig: u64) -> Option<&str> {
        self.comments.get(&sig).map(String::as_str)
    }

    /// 按局面签名取该节点保全的未识别属性（「另存为」原样写回用）。
    /// 无保全返回空切片。
    pub fn extras_by_sig(&self, sig: u64) -> &[super::tree::Property] {
        self.extras.get(&sig).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 有多少条未识别属性被保全（报告与验证用：0 = 无第三方属性）。
    pub fn extras_count(&self) -> usize {
        self.extras.values().map(Vec::len).sum()
    }
}

/// 我们「已经专门写」的属性名单（另存路径会自行生成这些属性）：未识别
/// 属性保全必须**排除**它们，否则同一属性在输出节点上出现两份——
/// 部分解析器（含本项目的 [`super::tree::Node::get`]）只取第一个，
/// 用户改了第二份也不会生效，属制造数据混乱。名单与 `save.rs` 的
/// 写出集合一一对应，两处改动需同步。
fn is_owned_by_save(ident: &str) -> bool {
    matches!(
        ident,
        // 行棋 / 注释：逐手节点写出（C 仅在有注释时写，但保全侧统一
        // 排除——根注释与逐手注释另有原样回写通道）。
        "B" | "W" | "C"
        // 对局信息：build_root 按 info 写出。
        | "KM" | "HA" | "PL" | "PB" | "PW" | "BR" | "WR" | "BT" | "WT" | "RE" | "DT" | "EV"
        | "RO" | "PC" | "GN" | "RU" | "TM" | "OT"
        // 通用属性：build_root 恒写（GM/FF/CA/AP/SZ）。
        | "GM" | "FF" | "CA" | "AP" | "SZ"
        // 摆子：build_root 从根盘面重建（压缩矩形已展开，原值不再复用）。
        | "AB" | "AW" | "AE"
    )
}

/// 收集一个节点上需要保全的未识别属性：既不是程序写回集合（见
/// [`is_owned_by_save`]），也不是已识别进 `GameInfo` 的常见文本信息。
/// 判定按属性名，值原样克隆（解码后的语义值，写出时经词法层重新转义，
/// 与全仓「树内不存转义」的表示约定一致）。
fn collect_extras(node: &Node) -> Vec<super::tree::Property> {
    node.props
        .iter()
        .filter(|prop| !is_owned_by_save(&prop.ident))
        .cloned()
        .collect()
}

/// 一次成功（可能是部分）的载入结果。棋盘游标已定位于开局第 0 手。
pub struct LoadedGame {
    /// 棋谱元信息。
    pub meta: GameMeta,
    /// 载入结果棋盘：含整棵谱树（`move_count()` = 全树着法数，
    /// `line_len()` = 主变手数），游标在开局第 0 手。
    pub board: Board,
    /// 载入提示汇总（主变停止 / 变着舍弃 / 中途摆子忽略），无提示为 `None`。
    pub warning: Option<String>,
    /// 主变是否中途停止（非法着法 / 行棋方不符），即「部分载入」；
    /// 仅变着分支被舍弃时为 `false`。
    pub partial: bool,
    /// 无法对应到任何节点的未识别属性条数（挂在被载入过程丢弃的节点上，
    /// 例如非法着法的节点、纯注释节点）：另存写不出它们，App 据此提示。
    pub unplaced_props: usize,
    /// 因所在节点没有着法（纯注释 / 空节点）而无法挂载的**注释条数**：
    /// 逐手注释按局面签名索引，而这类节点不产生棋盘节点、拿不到签名，
    /// 注释只能丢弃。与 [`Self::unplaced_props`] 同为「载入丢东西」的
    /// 计数，两者合并进同一条提示（App 侧），不静默丢。
    pub dropped_comments: usize,
}

impl LoadedGame {
    /// 拆出棋盘与元信息（App 分别持有二者）。
    pub fn into_parts(self) -> (Board, GameMeta) {
        (self.board, self.meta)
    }
}

/// 解析并载入 SGF 字节流（来源路径原样带入结果）。
///
/// 手数编号按 1 起计（与界面「第 N 手」一致）；预设摆子不计入手数。
pub fn load_from_bytes(source: &Path, bytes: &[u8]) -> Result<LoadedGame, LoadError> {
    let tree = parse_bytes(bytes).map_err(LoadError::Parse)?;
    let info = GameInfo::from_tree(&tree).map_err(LoadError::Parse)?;
    let size = info.size;
    let root = tree.root().expect("from_tree 成功则根节点必然存在");

    // 预设局面：坐标合法性已由解析期的 from_sgf 保证（越界即 BadCoord），
    // from_setup 实际只剩黑白冲突一种失败。
    let black = root.coords("AB", size).map_err(LoadError::Parse)?;
    let white = root.coords("AW", size).map_err(LoadError::Parse)?;
    let removed = root.coords("AE", size).map_err(LoadError::Parse)?;
    let to_play = info.initial_player.unwrap_or(if black.is_empty() && white.is_empty() {
        // FF4 惯例：普通局黑先。
        Stone::Black
    } else {
        // 让子 / 摆子局白先。
        Stone::White
    });
    let mut board =
        Board::from_setup(size, &black, &white, &removed, to_play).map_err(LoadError::Setup)?;

    // 根注释的路径为空序列；根节点的摆子属性已在预设局面处理，
    // walk 从本层第二个节点起（skip_first）。
    let mut comments = HashMap::new();
    if let Some(text) = &info.root_comment {
        comments.insert(position_sig(&[]), text.clone());
    }
    // 未识别属性保全：根节点先收（签名 = 空路径），其余随 walk 逐节点收。
    // 挂不上任何节点的条数（被丢弃节点上的属性）单独累计，随结果带出。
    let mut extras = HashMap::new();
    let mut orphan_extras = 0usize;
    // 无行棋属性节点上的注释无处可挂（没有对应局面签名），累计后提示。
    let mut dropped_comments = 0usize;
    {
        let root = tree.root().expect("from_tree 成功则根节点必然存在");
        let root_props = collect_extras(root);
        if !root_props.is_empty() {
            extras.insert(position_sig(&[]), root_props);
        }
    }
    let mut warnings = Vec::new();
    let mut partial = false;
    walk(
        &mut board,
        &tree,
        &mut comments,
        &mut extras,
        &mut orphan_extras,
        &mut dropped_comments,
        &mut warnings,
        &mut partial,
        None,
        true,
    );

    // 重放一遍主变，把沿途各分支点的选中子分支恢复为主变（载入变着时
    // Board 会把新分支设为选中项）；已存在的着法直接切换，不建新分支。
    // 遇到载入时同款失败即停（同局面同判定，不会建出多余分支）。
    board.go_to(0);
    for node in tree.mainline() {
        if play_node(&mut board, node).is_err() {
            break;
        }
    }
    // 定位到开局第 0 手；载入只读谱，不落在末端。
    board.go_to(0);

    Ok(LoadedGame {
        meta: GameMeta {
            source: source.to_path_buf(),
            info,
            comments,
            extras,
        },
        board,
        warning: (!warnings.is_empty()).then(|| warnings.join("；")),
        partial,
        unplaced_props: orphan_extras,
        dropped_comments,
    })
}

/// 递归挂载一棵 SGF 子树：本层 `nodes` 依次落子（游标即配对的 board
/// 节点），末节点后按序分叉到 `children`——第 0 个是主变（延续当前
/// 线的选中项），其余是变着分支。
///
/// `label` 为所属变着的定位描述（如「第 3 手的第 2 个变着」），主线为
/// `None`；`skip_first` 仅最外层调用为 true（根节点已按预设局面处理）。
/// 任一节点的着法无法载入时放弃**当前分支**并返回：主线同时置
/// `partial`，变着分支不影响已走完的其它分支。
///
/// 未识别属性随节点收集：落子成功的节点按**落子后的局面签名**挂入
/// `extras`（与注释同一套键——保存端逐节点重放路径可还原同一签名）；
/// 被跳过 / 丢弃节点上的条数计入 `orphan_extras`（写不出，需如实提示）。
/// 无行棋属性节点上的注释同样无处可挂，条数计入 `dropped_comments`。
#[allow(clippy::too_many_arguments)]
fn walk(
    board: &mut Board,
    tree: &GameTree,
    comments: &mut HashMap<u64, String>,
    extras: &mut HashMap<u64, Vec<super::tree::Property>>,
    orphan_extras: &mut usize,
    dropped_comments: &mut usize,
    warnings: &mut Vec<String>,
    partial: &mut bool,
    label: Option<&str>,
    skip_first: bool,
) {
    // 提示文案的定位前缀：主线为「第 N 手」，分支追加变着路径。
    let site = |turn: usize| match label {
        None => format!("第 {turn} 手"),
        Some(path) => format!("第 {turn} 手（{path}内）"),
    };
    for node in tree.nodes.iter().skip(if skip_first { 1 } else { 0 }) {
        let turn = board.cursor() + 1;
        // 中途摆子（AB/AW/AE）不支持：忽略但明确提示，不静默丢数据。
        if node.get("AB").is_some() || node.get("AW").is_some() || node.get("AE").is_some() {
            warnings.push(format!("{}含中途摆子（AB/AW/AE），已忽略", site(turn)));
        }
        match play_node(board, node) {
            Ok(true) => {
                // 注释挂在产生该局面的节点上，以落子后的局面签名为键。
                if let Some(text) = node.comment().filter(|text| !text.is_empty()) {
                    comments.insert(position_sig(board.records()), text.to_owned());
                }
                // 未识别属性同键保全（TR/SQ/TB/TW 及一切第三方属性）。
                let props = collect_extras(node);
                if !props.is_empty() {
                    extras.insert(position_sig(board.records()), props);
                }
            }
            // 无行棋属性的节点（纯注释 / 空节点）跳过。其上若挂有未识别
            // 属性，同样写不出（没有 board 节点可对应签名）——计数带出。
            // 它的 `C` 注释同样无处可挂（没有局面签名），一并计数——
            // 这是载入过程真的丢了用户数据，必须在提示里如实告知。
            Ok(false) => {
                *orphan_extras += collect_extras(node).len();
                if node.comment().is_some_and(|text| !text.is_empty()) {
                    *dropped_comments += 1;
                }
            }
            Err(reason) => {
                // 非法着法节点被丢弃：其上未识别属性无处可挂，计数带出。
                *orphan_extras += collect_extras(node).len();
                if label.is_none() {
                    *partial = true;
                    warnings.push(format!("{}无法载入：{reason}", site(turn)));
                } else {
                    warnings.push(format!(
                        "{}无法载入：{reason}，该分支已舍弃",
                        site(turn)
                    ));
                }
                return;
            }
        }
    }
    // 本层走完后的游标即分叉点；各分支都从这里重新出发。
    let fork = board.current_node();
    for (i, child) in tree.children.iter().enumerate() {
        board.go_to_node(fork);
        // 变着的定位描述：主线记「第 N 手的第 i 个变着」（N = 分叉点
        // 手数 + 1 = 分支首着手数），深层变着在路径上继续追加。
        let child_label = if i == 0 {
            label.map(str::to_owned)
        } else {
            let n = board.cursor() + 1;
            Some(match label {
                Some(path) => format!("{path}的第 {i} 个变着"),
                None => format!("第 {n} 手的第 {i} 个变着"),
            })
        };
        walk(
            board,
            child,
            comments,
            extras,
            orphan_extras,
            dropped_comments,
            warnings,
            partial,
            child_label.as_deref(),
            false,
        );
    }
}

/// 重放单个 SGF 节点的行棋属性到 `board`（游标处）。
/// 返回 `Ok(true)` = 已落子 / 弃着；`Ok(false)` = 无行棋属性（跳过）；
/// `Err` = 可显示的中文原因（坐标非法 / 行棋方不符 / 着法非法）。
fn play_node(board: &mut Board, node: &Node) -> Result<bool, String> {
    let (stone, action) = match node.move_action(board.size()) {
        Ok(Some(pair)) => pair,
        Ok(None) => return Ok(false),
        Err(err) => return Err(err.to_string()),
    };
    if stone != board.to_play() {
        return Err(format!(
            "谱中行棋方为{}，实际轮到{}",
            stone.name(),
            board.to_play().name()
        ));
    }
    match action {
        Action::Place(at) => board.play(at).map(|_| true).map_err(|e| e.to_string()),
        Action::Pass => {
            board.pass();
            Ok(true)
        }
    }
}

/// 局面签名：对根到当前节点的着法序列（行棋方 + 着点 / 弃着）做
/// FNV-1a。与 `ui::analysis` 的同名函数同构但更简（不含提子——同一
/// 着法序列必然同一盘面，提子是派生结果）；两处语义独立，改动需同步。
/// 仅作注释索引键，不要求抗碰撞。
///
/// 保存端（`save.rs`）逐节点增量混入各手还原同一签名，哈希实现经
/// [`sig_with_record`] 共享，两侧按键必然一致。
fn position_sig(records: &[crate::board::MoveRecord]) -> u64 {
    let mut h = SIG_INIT;
    for record in records {
        sig_with_record(&mut h, record);
    }
    h
}

/// FNV-1a 初始值（空路径的签名；与 `ui::analysis` 的实现保持一致）。
pub(super) const SIG_INIT: u64 = 0xcbf2_9ce4_8422_2325;

/// 把一手棋混入局面签名（[`position_sig`] 的增量步；保存端共用，
/// 保证两处对同一着法序列算出同一签名）。
pub(super) fn sig_with_record(sig: &mut u64, record: &crate::board::MoveRecord) {
    fn byte(h: &mut u64, b: u8) {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    byte(sig, u8::from(matches!(record.player, Stone::Black)));
    match record.action {
        Action::Place(c) => {
            byte(sig, 1);
            byte(sig, c.x());
            byte(sig, c.y());
        }
        Action::Pass => byte(sig, 0),
    }
}
