//! 棋盘谱树 → SGF 文本与落盘（「另存为」的数据层，与 [`load_from_bytes`] 互补）。
//!
//! 序列化策略（与载入端的惯例一一对应）：
//!
//! - [`Board`] 的谱树是「节点 + 子分支列表」结构，[`GameTree`] 是「节点
//!   序列 + 子树列表」结构：本层主链沿 `children[0]` 下行，**分支点处
//!   截断**——全部子分支（含第 0 个）都递归成子树（SGF 的子树只能从
//!   本层末节点分出）。Board 的子分支按创建顺序排列（新分支总是追加在
//!   末尾），写出后重新载入仍以原先的第一个子变为主变，往返稳定。
//! - 逐手注释以**局面签名**（根到该节点着法路径的 FNV-1a）为键，与
//!   载入端 [`GameMeta::comment_by_sig`] 共用同一哈希实现；保存按完整树
//!   遍历逐节点增量混入各手，无需对每个节点重算整条路径。
//! - 值内容（坐标、注释、对局信息）全部进 [`Node`] / [`Property`]，
//!   最终文本经 [`GameTree::write`] 转义写出，本模块不自行拼接值，
//!   保证「写出 → 重解析」往返一致。
//!
//! # 已知近似（无法无损还原的信息）
//!
//! - `AE`（清除摆子）在盘面上不留痕迹（清除后即空点），写不出；
//!   载入端对空 `AE` 无感，仅「全清摆子」这类罕见谱会丢摆子信息；
//! - 当前选中分支（`Board::selected_child`）在 SGF 标准里没有对应
//!   属性，不保存；重新载入后按惯例定位主变。

use std::path::Path;

use super::load::{SIG_INIT, sig_with_record};
use super::tree::{GameInfo, GameTree, Node as SgfNode, Property};
use super::GameMeta;
use crate::board::{Action, Board, Coord, MoveRecord, Stone};

/// 「另存为」失败原因（可直接 `Display` 给用户看）。
#[derive(Debug)]
pub enum SaveError {
    /// 写入文件失败（磁盘只读、目录不存在、权限不足、空间不足等）。
    Io(std::io::Error),
}

impl SaveError {
    /// 常见 I/O 错误的中文说明；未覆盖的错误码退回系统原始描述。
    fn describe(err: &std::io::Error) -> String {
        match err.raw_os_error() {
            Some(30) => "目标位置所在磁盘为只读".to_owned(),
            Some(2) => "目标目录不存在".to_owned(),
            Some(13) => "没有写入权限".to_owned(),
            Some(28) => "磁盘空间不足".to_owned(),
            _ => err.to_string(),
        }
    }
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "写入文件失败：{}", Self::describe(err)),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
        }
    }
}

/// 把当前棋盘（**含用户新建的变着**）序列化为 SGF 文本。
///
/// `meta` 为已载入棋谱的元信息（对局信息 + 逐手注释），可 `None`：
/// 空盘或未载入棋谱时也能存出合法的 SGF，此时只写通用属性与根摆子。
pub fn board_to_sgf(board: &Board, meta: Option<&GameMeta>) -> String {
    let info = meta.map(|m| &m.info);
    let mut tree = GameTree::default();
    tree.nodes.push(build_root(board, info));
    extend_tree(board, 0, SIG_INIT, meta, &mut tree);
    tree.write()
}

/// 序列化并写入文件（UTF-8 无 BOM；文本由 [`GameTree::write`] 生成）。
/// 落盘为原子写（见 [`write_atomically`]），失败时已有文件不受影响。
pub fn save_to_file(
    path: &Path,
    board: &Board,
    meta: Option<&GameMeta>,
) -> Result<(), SaveError> {
    let text = board_to_sgf(board, meta);
    write_atomically(path, &text)
}

/// 原子落盘：先写**同目录**的临时文件（`原名.tmp`），再 `rename` 覆盖
/// 目标——写到一半崩溃 / 断电只损失临时文件，已有棋谱不会被截断。
/// 临时文件与目标同目录是 `rename` 原子性的前提（跨文件系统会退化为
/// 拷贝）；写入或换名失败都删除临时文件，不留垃圾。
///
/// 不复用 `engine::config::write_atomically`：那是配置写入的模块私有
/// 实现，语义不合——它会静默创建目标目录、失败时不清理临时文件；
/// 「另存为」面对用户选定路径的目录缺失应当报错而非替用户建目录。
fn write_atomically(path: &Path, text: &str) -> Result<(), SaveError> {
    let name = path.file_name().ok_or_else(|| {
        SaveError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "目标路径没有文件名",
        ))
    })?;
    let tmp = path.with_file_name(format!("{}.tmp", name.to_string_lossy()));
    let result = std::fs::write(&tmp, text)
        .map_err(SaveError::Io)
        .and_then(|()| std::fs::rename(&tmp, path).map_err(SaveError::Io));
    if result.is_err() {
        // 临时文件可能尚未建出，清理失败（已不存在）无碍，静默即可。
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 从「当前链起点」`start` 起填充一棵游戏树：`start`（根或分支首手）的
/// 着法进 `tree.nodes`，之后沿 `children[0]` 继续本层主链；**遇到分支点
/// 即截断本层主链**——该节点的全部子分支（含第 0 个）都递归成
/// `tree.children` 的子树。这与 SGF「子树从本层末节点分出」的语义一致：
/// 若第 0 个子分支留在链上而其余挂子树，重载会把变着错误接到主链末端。
fn extend_tree(
    board: &Board,
    start: usize,
    incoming_sig: u64,
    meta: Option<&GameMeta>,
    tree: &mut GameTree,
) {
    let (mut node_id, mut sig) = (start, incoming_sig);
    loop {
        let node = &board.nodes()[node_id];
        // 分支首手（start != 0）的着法进本层链；分支点处不再下行。
        if node_id != 0 {
            let record = node.record().expect("非根节点必有着法记录");
            sig_with_record(&mut sig, record);
            tree.nodes.push(record_node(record, sig, meta));
        }
        if node.children().len() > 1 {
            let children = node.children().to_vec();
            for &branch in &children {
                let mut child_tree = GameTree::default();
                extend_tree(board, branch, sig, meta, &mut child_tree);
                tree.children.push(child_tree);
            }
            return;
        }
        let Some(&child) = node.children().first() else {
            return; // 叶子：本层链到此结束。
        };
        node_id = child;
    }
}

/// 一手棋 → SGF 节点：`B[xx]` / `W[xx]`（弃着写 `B[]`），有注释带 `C[..]`。
/// 注释按 `sig`（根到该手着法路径的签名）查 [`GameMeta`]。
fn record_node(record: &MoveRecord, sig: u64, meta: Option<&GameMeta>) -> SgfNode {
    let mut node = SgfNode::default();
    let ident = match record.player {
        Stone::Black => "B",
        Stone::White => "W",
    };
    let value = match record.action {
        Action::Place(at) => at.to_sgf(),
        Action::Pass => String::new(),
    };
    node.props.push(prop(ident, &value));
    if let Some(text) = meta
        .and_then(|m| m.comment_by_sig(sig))
        .filter(|text| !text.is_empty())
    {
        node.props.push(prop("C", text));
    }
    node
}

/// 由棋盘预设局面与对局信息构造根节点属性。
fn build_root(board: &Board, info: Option<&GameInfo>) -> SgfNode {
    let size = board.size();
    let (setup_black, setup_white) = root_setup(board);
    let has_setup = !setup_black.is_empty() || !setup_white.is_empty();

    // 首着方：树上有手时以主变首手的行棋方为准（比 GameInfo 更真实），
    // 树空时退回 info 的 PL 声明，再退回载入端的惯例推定——保证与
    // 「载入 → 不动 → 保存」的往返里 PL 语义不漂移。
    let first_player = board.nodes()[0]
        .children()
        .first()
        .and_then(|&id| board.nodes()[id].record())
        .map(|record| record.player)
        .or_else(|| info.and_then(|i| i.initial_player))
        .unwrap_or(if has_setup { Stone::White } else { Stone::Black });

    let mut node = SgfNode::default();
    // 通用属性：对局类型 / SGF 版本 / 字符集 / 产生程序 / 棋盘尺寸。
    node.props.push(prop("GM", "1"));
    node.props.push(prop("FF", "4"));
    node.props.push(prop("CA", "UTF-8"));
    node.props.push(prop("AP", concat!("Guanqi:", env!("CARGO_PKG_VERSION"))));
    node.props.push(prop("SZ", &size.n().to_string()));
    if let Some(info) = info {
        // KM / HA / PL 只在语义非缺省时写：贴目 0、无让子、以及与
        // FF4 惯例（无摆子黑先 / 有摆子白先）一致的首着方都可由
        // 载入端自行推定，写出只会让文件变啰嗦。
        if let Some(komi) = info.komi {
            node.props.push(prop("KM", &format_komi(komi)));
        }
        if info.handicap > 1 {
            node.props.push(prop("HA", &info.handicap.to_string()));
        }
        let conventional = if has_setup { Stone::White } else { Stone::Black };
        if first_player != conventional {
            let letter = match first_player {
                Stone::Black => "B",
                Stone::White => "W",
            };
            node.props.push(prop("PL", letter));
        }
        // 文本类对局信息原样写回（有则写），保存不丢已载入的数据。
        let texts = [
            ("PB", &info.player_black),
            ("PW", &info.player_white),
            ("BR", &info.rank_black),
            ("WR", &info.rank_white),
            ("BT", &info.team_black),
            ("WT", &info.team_white),
            ("RE", &info.result),
            ("DT", &info.date),
            ("EV", &info.event),
            ("RO", &info.round),
            ("PC", &info.place),
            ("GN", &info.game_name),
            ("RU", &info.rules),
            ("TM", &info.time_limit),
            ("OT", &info.overtime),
        ];
        for (ident, value) in texts {
            if let Some(text) = value
                && !text.is_empty()
            {
                node.props.push(prop(ident, text));
            }
        }
        if let Some(comment) = &info.root_comment
            && !comment.is_empty()
        {
            node.props.push(prop("C", comment));
        }
    }
    // 预设摆子：从根盘面读出（该盘面即 from_setup 的结果）。
    // AB 摆在 AW 之前与解析期展开顺序一致；AE 无法还原，见模块文档。
    if !setup_black.is_empty() {
        node.props.push(coords_prop("AB", &setup_black));
    }
    if !setup_white.is_empty() {
        node.props.push(coords_prop("AW", &setup_white));
    }
    node
}

/// 根节点的预设摆子：`position_at(0)` 即根节点盘面（无论游标在哪），
/// 黑子进 AB、白子进 AW，按索引序（行优先）排列。
fn root_setup(board: &Board) -> (Vec<Coord>, Vec<Coord>) {
    let size = board.size();
    let mut black = Vec::new();
    let mut white = Vec::new();
    if let Some(grid) = board.position_at(0) {
        for (index, stone) in grid.iter().enumerate() {
            let Some(stone) = stone else { continue };
            let Some(at) = Coord::from_index(size, index) else {
                continue;
            };
            match stone {
                Stone::Black => black.push(at),
                Stone::White => white.push(at),
            }
        }
    }
    (black, white)
}

/// 贴目格式化：整数值不带小数点（`6.0` → `6`），其余按最短表示；
/// 两种写法 `parse::<f64>` 均可还原。
fn format_komi(komi: f64) -> String {
    if komi == komi.trunc() && komi.abs() < 1e9 {
        format!("{}", komi as i64)
    } else {
        format!("{komi}")
    }
}

/// 单值文本属性。
fn prop(ident: &str, value: &str) -> Property {
    Property {
        ident: ident.to_owned(),
        values: vec![value.to_owned()],
    }
}

/// 多坐标属性（AB/AW），每个坐标一个值（展开后的逐点写法）。
fn coords_prop(ident: &str, coords: &[Coord]) -> Property {
    Property {
        ident: ident.to_owned(),
        values: coords.iter().map(|at| at.to_sgf()).collect(),
    }
}
