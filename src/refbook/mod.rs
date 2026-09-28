//! 外部参照 SGF（「参照文件」基础层，阶段 22）。
//!
//! 一份参照 = 用户提供的一张外部棋谱文件（定式库 / 死活题谱 / 其它参考
//! 谱）。本模块负责把它加载为内存里的「参照」：解析完整游戏树、沿树重放
//! 每个节点得到该节点的实际盘面，并**为每个有着点的节点建一个局面键**
//! （见 [`PositionKey`]）索引。查询侧（[`Reference::lookup`]）把当前盘面
//! 算成同口径的键即可在常数级命中，从而给出库内该局面的续法与注释。
//!
//! # 定位与边界
//!
//! - **不内置任何棋谱数据**：参照内容完全来自用户选择的文件；
//! - 不做题库管理 / 索引检索 / 难度分级 / 刷题进度（下一批的「解题模式」
//!   在本模块的匹配 API 之上实现）；
//! - 参照与「当前对局 / 当前棋谱」完全独立：加载参照绝不替换棋盘。
//!
//! # 匹配口径（本批核心，索引侧与查询侧共用 [`match::key_of_cluster`]）
//!
//! **按局面匹配，不按着法序列**——同一局部形状可由不同次序达成（转置）、
//! 也可在棋盘任意位置出现（平移），都要命中：
//!
//! 1. 把盘面上的棋子按**切比雪夫距离 ≤ 2×MARGIN** 聚成若干「簇」；
//! 2. 每簇取「簇包围盒向四周各扩 MARGIN（截断到盘内）」为**比较区域**；
//! 3. 键 = (区域宽, 区域高, 区域内以左上角为原点的棋子分布, 行棋方)。
//!
//! 于是：区域外的棋子（实战其它角的落子）不参与键 → 不干扰命中；分布记
//! **簇内相对坐标** → 同形状平移后键不变；键即最终分布 → 转置命中。
//!
//! **键不含棋盘尺寸 / 规则**：尺寸在查询前单独判定（尺寸不符直接不命中，
//! [`Reference::lookup`] 返回 [`LookupOutcome::SizeMismatch`]，UI 另有提示）；
//! 规则本批不区分（定式库与规则弱相关，留作可扩展项）。
//!
//! **对称归一化（8 种对称映射到一键）本批不做**：索引与查询两侧都要做
//! 8 倍展开，命中后还需反算对称方向才能正确展示续法，复杂度高；而定式库
//! 通常已按四向角收录条目，收益有限。列为本模块可扩展项（扩展点集中在
//! [`match::key_of_cluster`] 一处，两侧同步换）。
//!
//! # 重放口径
//!
//! 沿树重放用 [`rules::apply_move`]：**无劫争判定**——定式库中的劫争循环
//! 着法若走 [`Board::play`] 会被简单劫拒绝、整分支丢失。占位 / 自杀仍拒绝：
//! 该分支放弃并计入 [`Reference::warnings`]（不整体失败，宽容真实文件）。
//! 提子照实结算（键按**实际盘面**计算，提子不同即不同局面，与
//! `sgf::load` 的局面签名口径一致）。

mod entry;
mod match_;
mod walk;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::board::{Size, Stone};
use crate::sgf::GameTree;

pub use entry::{RefEntry, TAIL_LEN};
pub use match_::{Continuation, LookupOutcome, MARGIN};
pub use walk::{PositionKey, Rect};

// ---- 参照加载结果 ----

/// 参照加载整体失败（此时**不应替换**已加载的旧参照）。文案面向用户。
#[derive(Debug)]
pub enum LoadError {
    /// 读文件失败（不存在 / 无权限等）。
    Io(std::io::Error),
    /// SGF 解析或根属性失败（[`crate::sgf::SgfError`] 的 Display 已含行列）。
    Parse(crate::sgf::SgfError),
    /// 参照里没有任何有着点的节点（空谱 / 纯摆子谱），匹配无从谈起。
    Empty,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "读取参照文件失败：{err}"),
            Self::Parse(err) => write!(f, "参照文件解析失败：{err}"),
            Self::Empty => write!(f, "参照文件里没有任何着法，无法用于局面匹配。"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Parse(err) => Some(err),
            Self::Empty => None,
        }
    }
}

/// 加载期汇总的非致命问题（个别分支无法重放等），整份加载仍成功。
#[derive(Debug, Default)]
pub struct LoadWarnings(Vec<String>);

impl LoadWarnings {
    /// 是否有任何警告。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 逐条只读访问。
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

/// 参照：一份外部 SGF 棋谱 + 局面键索引。同一时刻 App 只保留一份
/// （再加载即整体替换），无任何后台状态。
#[derive(Debug)]
pub struct Reference {
    /// 来源路径（原样回显）。
    source: PathBuf,
    /// 完整 SGF 游戏树（含变着分支；下一批「跳进参照树浏览」直接用）。
    tree: GameTree,
    /// 棋盘尺寸（取自根节点 `SZ`，缺省 19 路）。
    size: Size,
    /// 局面键 → 索引条目。同键多节点时**首个**胜出（深搜先到主变）：
    /// 同键即同局面，续法取哪条都是库内有效信息，取首个展示稳定。
    index: HashMap<PositionKey, RefEntry>,
    /// 加载期非致命警告。
    warnings: LoadWarnings,
    /// 建索引（重放 + 键计算）耗时；加载提示与性能取证用。
    index_elapsed: Duration,
    /// 全树有着点的节点数（含被同键合并 / 非法跳过的）。
    move_nodes: usize,
}

impl Reference {
    /// 来源路径。
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// 来源文件名（无文件名时回退完整路径的展示串）。
    pub fn file_name(&self) -> String {
        self.source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.source.display().to_string())
    }

    /// 参照的棋盘尺寸。
    pub fn size(&self) -> Size {
        self.size
    }

    /// 完整游戏树（只读；下一批解题模式浏览参照树用）。
    pub fn tree(&self) -> &GameTree {
        &self.tree
    }

    /// 索引条目数（= 不同局面键数）。
    pub fn entry_count(&self) -> usize {
        self.index.len()
    }

    /// 全树有着点的节点数。
    pub fn move_node_count(&self) -> usize {
        self.move_nodes
    }

    /// 加载期警告。
    pub fn warnings(&self) -> &LoadWarnings {
        &self.warnings
    }

    /// 建索引用时（读文件 / 解析之外的重放 + 键计算部分）。
    pub fn index_elapsed(&self) -> Duration {
        self.index_elapsed
    }

    /// 取证用：全部条目的只读摘要（headless 验证工具核对索引内容用；
    /// 应用路径不调用）。
    #[doc(hidden)]
    pub fn debug_entries(&self) -> Vec<DebugEntryInfo> {
        self.index
            .values()
            .map(|entry| DebugEntryInfo {
                move_no: entry.move_no,
                path: entry.path.clone(),
                region: entry.region,
                stones: entry.stones.clone(),
                continuations: entry
                    .continuations
                    .iter()
                    .map(Continuation::text)
                    .collect(),
                comment: entry.comment.clone(),
                to_play_black: entry
                    .continuations
                    .first()
                    .map(|c| c.player == Stone::Black)
                    .unwrap_or(true),
                key_len: entry.stones.len(),
            })
            .collect()
    }
}

/// 取证用条目摘要（[`Reference::debug_entries`] 的元素）。
#[doc(hidden)]
#[derive(Debug)]
pub struct DebugEntryInfo {
    /// 库内手数。
    pub move_no: usize,
    /// 树内路径。
    pub path: Vec<usize>,
    /// 比较区域（内部坐标）。
    pub region: walk::Rect,
    /// 区域内棋子。
    pub stones: Vec<(u8, u8, Stone)>,
    /// 续法展示串。
    pub continuations: Vec<String>,
    /// 注释。
    pub comment: Option<String>,
    /// 行棋方提示（粗略：首续法是否黑方）。
    pub to_play_black: bool,
    /// 簇内子数（键长度量级参考）。
    pub key_len: usize,
}

// ---- 加载与建索引 ----

/// 读取并解析参照文件，建局面键索引。
///
/// 失败（文件不可读 / 解析失败 / 无着法节点）返回 [`LoadError`]，
/// **不产生半成品**；成功后个别分支无法重放只记警告。建索引为
/// O(节点数) 一次深搜，每节点键计算 O(区域面积)（≤ 全盘、实际 ≤ 10×10）。
pub fn load_reference(path: &Path) -> Result<Reference, LoadError> {
    let bytes = std::fs::read(path).map_err(LoadError::Io)?;
    let tree = crate::sgf::parse_bytes(&bytes).map_err(LoadError::Parse)?;
    let size = crate::sgf::GameInfo::from_tree(&tree)
        .map_err(LoadError::Parse)?
        .size;
    let reference = build_index(path.to_path_buf(), tree, size);
    if reference.move_nodes == 0 {
        return Err(LoadError::Empty);
    }
    Ok(reference)
}

/// 由已解析的树构建参照（headless 性能实测与「内存注入」路径共用）。
/// 尺寸由调用方从根节点解析后传入；个别节点无法重放按警告跳过。
pub fn build_index(source: PathBuf, tree: GameTree, size: Size) -> Reference {
    let started = std::time::Instant::now();
    let (index, warnings, move_nodes) = walk::build(&tree, size);
    Reference {
        source,
        tree,
        size,
        index,
        warnings: LoadWarnings(warnings),
        index_elapsed: started.elapsed(),
        move_nodes,
    }
}
