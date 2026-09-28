//! 索引条目：一个可匹配参照节点的盘面摘要与展示信息。

use super::match_::Continuation;
use super::walk::Rect;
use crate::board::{Size, Stone};

/// 展示用着法序列尾部长度（含本手）。取 3：定式 / 死活的局部上下文
/// 几手内即可辨认，再多只会刷屏；`walk` 截尾与查询侧 `tail_of` 共用。
pub const TAIL_LEN: usize = 3;

/// 参照文件里一个可匹配节点的索引条目。
///
/// 不存整盘盘面（大库内存考量），只存「复算键所需的区域与棋子」和
/// 「展示所需的续法 / 注释 / 路径」。检索走 `Reference` 内部的键表。
#[derive(Debug)]
pub struct RefEntry {
    /// 在参照树中的路径（各层 `GameTree.nodes` 的下标）。供展示「库内
    /// 位置」，也给下一批「跳进参照树浏览」定位节点用。
    pub path: Vec<usize>,
    /// 该节点的库内手数（根 = 0；预设摆子不计）。
    pub move_no: usize,
    /// 该节点的库内注释（`C`，首尾空白已整理）；无注释为 `None`。
    pub comment: Option<String>,
    /// 该节点的续法：子节点着法（行棋方 + 着点 / 弃着），按参照树顺序。
    pub continuations: Vec<Continuation>,
    /// 展示用着法序列尾部（本手之前的最后 [`TAIL_LEN`] 手 + 本手），
    /// GTP 坐标，「黑 D4 → 白 Q16」形态。
    pub tail: Vec<(Stone, String)>,
    /// 键对应的比较区域（盘上绝对坐标；取证 / debug 展示用）。
    pub region: Rect,
    /// 区域内的棋子（盘上绝对坐标）：查询侧以同构键比对，此处仅留存
    /// 供取证与未来的 u64 压缩键二次复核。
    pub stones: Vec<(u8, u8, Stone)>,
}

impl RefEntry {
    /// 着法尾部的展示串：「黑 D4 → 白 Q16 → …（第 N 手）」。
    /// 弃着显示「弃着」；`size` 供续法 / 尾部坐标换算（坐标串存的是
    /// GTP 文本，此参数预留给下一批按盘面直出坐标的改造）。
    pub fn tail_text(&self, _size: Size) -> String {
        let moves = self
            .tail
            .iter()
            .map(|(stone, at)| format!("{} {at}", stone.name()))
            .collect::<Vec<_>>()
            .join(" → ");
        format!("{moves}（第 {} 手）", self.move_no)
    }
}
