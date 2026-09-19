//! 棋谱载入：SGF 游戏树 → [`Board`]（「打开棋谱」的数据层）。
//!
//! 职责边界与容错策略：
//!
//! - 只走主变着 [`GameTree::mainline`]；变着分支的切换留待棋谱树控件；
//! - 根节点 `AB` / `AW` / `AE` 构成预设局面，经 [`Board::from_setup`]
//!   进入棋盘（不计手数、不参与劫争，见 `board` 模块文档）；`PL` 决定
//!   首着方，缺省时按 FF4 惯例：有摆子则白先，否则黑先；
//! - 其后逐节点取 `B` / `W` 着点（空值与 `tt` 为弃着），用
//!   [`Board::play`] / [`Board::pass`] 重放；
//! - **非法着法不整体失败**：真实棋谱可能含规则层不接受的着法（超级劫、
//!   记谱错误、行棋方颠倒）。遇到即停止重放，保留已载入部分，把
//!   「第 N 手无法载入：原因」写入 [`LoadedGame::warning`]，载入结果仍为
//!   成功；只有解析期错误（编码 / 结构 / 根属性非法 / 预设摆子非法）
//!   才整体失败（返回 [`LoadError`]）；
//! - 中途节点的 `AB` / `AW` / `AE`（对局中途摆子）本期不支持，静默忽略；
//! - 逐手注释取自**产生该局面的节点**上的 `C`；独立注释节点（无着法的
//!   纯注释节点）的注释本期丢弃。下标 = 手数（0 = 根节点注释）。

use std::path::{Path, PathBuf};

use super::tree::GameInfo;
use super::{SgfError, parse_bytes};
use crate::board::{Action, Board, SetupError, Stone};

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
pub struct GameMeta {
    /// 来源路径（原样回显，供显示与将来「另存」复用）。
    pub source: PathBuf,
    /// 根节点对局信息（`PB` / `PW` / `RE` / `KM` / `DT` / `HA` 等）。
    pub info: GameInfo,
    /// 逐手注释：下标 = 手数（0 = 根节点注释），长度 = 已载入手数 + 1。
    pub comments: Vec<Option<String>>,
}

impl GameMeta {
    /// 第 `turn` 手局面（0 = 开局）的注释；无注释或整理后为空串时不显示。
    pub fn comment_at(&self, turn: usize) -> Option<&str> {
        self.comments
            .get(turn)
            .and_then(Option::as_deref)
            .filter(|text| !text.is_empty())
    }
}

/// 一次成功（可能是部分）的载入结果。棋盘游标已定位于开局第 0 手。
pub struct LoadedGame {
    /// 棋谱元信息。
    pub meta: GameMeta,
    /// 载入结果棋盘：`records()` 即已载入的着法（`move_count()` = 手数）。
    pub board: Board,
    /// 部分载入提示（非法着法停止重放时给出「第 N 手无法载入：原因」）。
    pub warning: Option<String>,
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

    // 逐手重放主变着。comments.len() 始终 = 已载入手数 + 1，
    // 且 comments[0] 已是根注释，故下一手编号即 comments.len()。
    let mut comments = vec![info.root_comment.clone()];
    let mut warning: Option<String> = None;
    for node in tree.mainline() {
        let action = match node.move_action(size) {
            Ok(Some((stone, action))) => {
                if stone != board.to_play() {
                    warning = Some(format!(
                        "第 {} 手无法载入：谱中行棋方为{}，实际轮到{}",
                        comments.len(),
                        stone.name(),
                        board.to_play().name(),
                    ));
                    break;
                }
                action
            }
            // 无行棋属性的节点（注释 / 空节点 / 中途摆子等）跳过。
            Ok(None) => continue,
            Err(err) => {
                warning = Some(format!("第 {} 手无法载入：{err}", comments.len()));
                break;
            }
        };
        let placed = match action {
            Action::Place(at) => board.play(at).map_err(|reason| reason.to_string()),
            Action::Pass => {
                board.pass();
                Ok(())
            }
        };
        if let Err(reason) = placed {
            warning = Some(format!("第 {} 手无法载入：{reason}", comments.len()));
            break;
        }
        comments.push(node.comment().map(str::to_owned));
    }

    // 定位到开局第 0 手；载入只读谱，不落在末端。
    board.go_to(0);
    Ok(LoadedGame {
        meta: GameMeta {
            source: source.to_path_buf(),
            info,
            comments,
        },
        board,
        warning,
    })
}
