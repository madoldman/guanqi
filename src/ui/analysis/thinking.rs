//! 引擎思考过程记录（阶段 21 #2）：流式中间报告的**蒸馏**时间线、容量
//! 上限与结论稳定性读数。
//!
//! 常规展示分析开启流式后，引擎每 [`super::REPORT_EVERY_SECS`] 秒推一条
//! 中间报告，此前只被用来刷新 [`super::Snapshot`] 就被下一条覆盖——
//! 「首选点在搜索中如何翻转、第几秒稳定下来」的过程信息随之丢失。本模块
//! 把每条中间报告**蒸馏**成一条小记录（耗时 / visits / 当时首选 + 前几名
//! 概要），形成「这一次搜索」的时间线，供侧栏回放。
//!
//! 取舍（任务口径）：**不保存完整 `moveInfos`**——一条可达上百候选，
//! 存原始结构既大又没必要；蒸馏字段足以回答「结论怎么形成的」。
//!
//! 生命周期：**只描述一次搜索**。新的展示查询发出时清空重记（见
//! [`ThinkingLog::begin_search`]）；换谱 / 新对局随 `AnalysisState::reset`
//! 一并清空。走子口径（Play）不开流式、批量查询（analyzeTurns）走独立
//! 口径，二者都不经过本模块。

use std::time::Duration;

use crate::board::Coord;

/// 单条思考记录的容量上限（条）。流式间隔 0.5s（[`super::REPORT_EVERY_SECS`]）
/// 下约等于 64 秒搜索过程；展示口径常用 300 visits ≈ 5–6 秒，即使最深
/// 2000 visits（约半分钟）也留有一倍余量。
///
/// 超限**丢弃最早**（`VecDeque::pop_front`）：时间轴回放看的是连续序列，
/// 保尾弃头对「回看刚才这次搜索」影响最小；稳定性读数（翻转次数 / 稳定
/// 时刻）在记录时**增量维护**、不受截断影响（若从保留序列重算，被丢掉的
/// 早期翻转就丢了，读数会漂移——这也是读数必须存而非现算的原因）。
pub(super) const THINKING_CAP: usize = 128;

/// 回放概要里保留的**前几名候选数**：展示层一行能读下的量，也足以看出
/// 「第二名何时反超」这类翻转戏码。
const TOP_LEN: usize = 3;

/// 一条思考记录：某条流式中间报告的蒸馏。
///
/// 所有胜率均为**黑方视角**（与 [`super::Snapshot`] 同一口径，不翻转）；
/// 首选与概要里的 visits 为该候选分支的搜索量。
#[derive(Clone, Debug)]
pub(crate) struct ThinkingEntry {
    /// 距本次查询发出的耗时（回放时间轴的横轴）。
    pub(crate) elapsed: Duration,
    /// 该时刻的根节点搜索量（全树 visits，随搜索单调递增）。
    pub(crate) visits: u64,
    /// 当时的**首选**候选（引擎 `order` 最小）：落点与黑方视角胜率。
    /// 首选是弃着或报告无候选时为 `None`（不臆造）。
    pub(crate) best: Option<(Coord, f64)>,
    /// 前几名概要（按引擎序取前 [`TOP_LEN`] 个）：落点、黑方视角胜率、
    /// 该分支 visits。弃着不进概要（无处展示）。不足 [`TOP_LEN`] 个时
    /// 尾部为 `None`——展示层取到第一个 `None` 或 `top_len` 为止。
    pub(crate) top: [Option<(Coord, f64, u64)>; TOP_LEN],
    /// [`Self::top`] 的有效长度。
    pub(crate) top_len: usize,
}

impl ThinkingEntry {
    /// 从一条报告的候选列表蒸馏。`infos` 按引擎到达序（`order` 最小者
    /// 不保证在前，独立取 `order` 最小为首选，与 `candidates_from` 同口径）。
    /// O(候选数) 一次遍历——由调用方保证只在中间报告路径上调用，
    /// 每条一次，不构成热路径（0.5s 一条）。
    pub(crate) fn from_report(elapsed: Duration, visits: u64, infos: &[crate::engine::MoveInfo]) -> Self {
        let best = infos
            .iter()
            .min_by_key(|info| info.order)
            .and_then(|info| info.mv.map(|at| (at, info.winrate)));
        let mut top: [Option<(Coord, f64, u64)>; TOP_LEN] = [None; TOP_LEN];
        let mut top_len = 0;
        for info in infos {
            let Some(at) = info.mv else { continue }; // 弃着不进概要
            top[top_len] = Some((at, info.winrate, info.visits));
            top_len += 1;
            if top_len == TOP_LEN {
                break;
            }
        }
        Self { elapsed, visits, best, top, top_len }
    }

    /// 首选落点（翻转判定用；`None` = 首选是弃着或无候选）。
    /// 翻转只看**落点**：胜率小幅波动与 visits 增长不算翻转，
    /// 否则每条记录都「翻转」，读数失去意义。
    fn best_at(&self) -> Option<Coord> {
        self.best.map(|(at, _)| at)
    }
}

/// 结论稳定性读数：首选翻转次数与最后一次翻转的耗时。
///
/// 从未翻转（或只有一条记录）时「结论自始稳定」；`last_flip` 为
/// `None` 即该情形（沿用项目「未知与 0 严格区分」的口径，不显示 0 秒）。
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Stability {
    /// 首选落点翻转次数（相邻记录首选落点不同即 +1）。
    pub flips: u32,
    /// 最后一次翻转发生时的耗时（自查询发出起）。
    pub last_flip: Option<Duration>,
}

impl Stability {
    /// 侧栏一行文案：翻转过给「首选翻转 N 次，X.X 秒后稳定」，从未翻转
    /// 给「结论自始稳定」。只有一条记录（无从谈翻转）与从未翻转同文案。
    pub fn text(&self) -> String {
        match self.last_flip {
            Some(at) => format!("首选翻转 {} 次，{:.1} 秒后稳定", self.flips, at.as_secs_f32()),
            None => "结论自始稳定".to_owned(),
        }
    }
}

/// 思考记录的**只读视图**（侧栏与 dump 的取数面）。借用于
/// [`ThinkingLog`] 内部条目，不复制概要数组；公开字段与
/// [`ThinkingEntry`] 一一对应。在 [`super::AnalysisState::thinking_entry`]
/// 处产出。
#[derive(Clone, Copy, Debug)]
pub struct ThinkingEntryRef<'a> {
    /// 距本次查询发出的耗时。
    pub elapsed: Duration,
    /// 该时刻的根节点搜索量。
    pub visits: u64,
    /// 当时的首选（落点 + 黑方视角胜率）；`None` = 首选弃着或无候选。
    pub best: Option<(Coord, f64)>,
    /// 前几名概要（有效段 = 前 `top_len` 个非 `None` 项）。
    pub top: &'a [Option<(Coord, f64, u64)>; 3],
    /// 概要有效长度。
    pub top_len: usize,
}

/// 一次搜索的思考记录（时间线 + 增量维护的稳定性读数）。
///
/// 挂在 [`super::AnalysisState`] 下；全部操作 O(1)/条（`push_back` +
/// 落点比较），不拖慢报告路径。`clear` / `begin_search` 之外无批量操作。
#[derive(Debug, Default)]
pub(crate) struct ThinkingLog {
    /// 时间线（按报告到达序，visits 单调递增）。
    entries: std::collections::VecDeque<ThinkingEntry>,
    /// 稳定性读数（增量维护：翻转次数 / 最后翻转耗时）。随 entries
    /// 同生共死（`clear` 一并清零）。
    stability: Stability,
    /// 上一条记录的首选落点（翻转判定的比较基准）。
    last_best: Option<Coord>,
}

impl ThinkingLog {
    /// 新的展示查询发出：清空重记（本模块描述的是「这一次搜索」）。
    pub(super) fn begin_search(&mut self) {
        self.clear();
    }

    /// 清空（换谱 / 新对局随 `AnalysisState::reset` 调用；`begin_search`
    /// 的实现体）。
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.stability = Stability { flips: 0, last_flip: None };
        self.last_best = None;
    }

    /// 记录一条中间报告的蒸馏。**稳定性读数在此增量维护**：与上一条的
    /// 首选落点比较，落点变了即翻转 +1（理由见 [`THINKING_CAP`] 文档：
    /// 截断后从保留序列重算会失真，必须边记边算）。
    pub(super) fn record(&mut self, entry: ThinkingEntry) {
        let best_at = entry.best_at();
        if self.last_best.is_some() && self.last_best != best_at {
            self.stability.flips += 1;
            self.stability.last_flip = Some(entry.elapsed);
        }
        self.last_best = best_at;
        if self.entries.len() >= THINKING_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// 是否没有任何记录（UI「本次无思考过程」的判定；侧栏在
    /// `thinking_len() == 0` 时用同一口径，此处为记录内部的对偶读数）。
    #[allow(dead_code)]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 稳定性读数（侧栏一行文案的数据源）。
    pub(super) fn stability(&self) -> Stability {
        self.stability
    }

    /// 回放游标取数：`i` 为时间线内下标；`None` = 越界。返回（条目，
    /// 是否最新一条）。滑杆回放与「最新」判定共用。
    pub(super) fn get(&self, index: usize) -> Option<&ThinkingEntry> {
        self.entries.get(index)
    }

    /// 记录条数（滑杆量程与 dump 取证用）。
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}
