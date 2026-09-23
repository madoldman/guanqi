//! 人机对弈时限制式：包干 / 读秒 / 无限制 的计时状态机。
//!
//! # 计时口径（务必与实现一致，改动前先读）
//!
//! - **人类一方**：从「轮到人类」起算到落子；导航 / 回看 / 浏览**不计时**。
//!   推进条件（调用方每帧判定，四条同时满足才调 [`Clock::tick`]）：
//!   对弈模式开启、对局未结束、轮到该方、游标在活子位置
//!   （`cursor == line_len`）。切到复盘浏览再回来：游标不在线尾时不
//!   推进（浏览不计时）；对局结束后永停。
//! - **AI 一方**：按引擎**实际思考时间**计——从发走子口径查询到收到
//!   终态报告的墙钟时长，由 [`Clock::on_engine_move`] 结算。
//! - **超时判负**：制式本身就是开关——选定包干 / 读秒即按规则判负，
//!   不另设「是否判负」勾选项；无限制不封顶、不判负但显示累计用时。
//! - **给引擎的预算**（[`engine_budget`]）：`max(0.2, 剩余 − 余量)`，
//!   读秒期取「一次读秒时长 − 余量」，绝不发 0 或负数；AI 剩余 ≤ 0
//!   由调用方在出手前判负（不给引擎 0 时间）。**难度档的 maxVisits
//!   仍是上限**：时间限制是截止线，不该偷偷让引擎变强（KataGo 中
//!   `maxTime` 与 `maxVisits` 都满足才停，先到者生效）。
//! - 本模块是**纯状态机**：只做时间记账与超时判定，不触碰棋盘、
//!   引擎与 UI；秒数用 `f64`（读秒需要亚秒分辨率）。

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::board::Stone;

/// 给引擎预算的余量秒数：预算 = 剩余 − 余量，防止引擎贴着时限用满
/// 才回报告（读秒 5s 档下余量占比稍大，属可接受的保守）。
const BUDGET_MARGIN: f64 = 0.8;

/// 预算下限：剩余再少也至少给引擎 0.2s（绝不发 0 或负数；判负在发
/// 查询之前完成，走不到这里）。
pub const MIN_BUDGET: f64 = 0.2;

/// 对局时限制式（新对局设置三选一；制式本身即「是否判负」的开关）。
/// serde 用带字段名的变体序列化进 settings.json（外部 tag 形式）。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
pub enum TimeSystem {
    /// 无限制：不封顶、不判负，但显示双方累计用时（缺省制式，理由见
    /// 下 Default 说明）。
    #[default]
    Unlimited,
    /// 包干制：每方一个总时长（分钟级设置，存秒统一扣减），用尽判负。
    Absolute {
        /// 每方总时长（秒）。
        seconds: f64,
    },
    /// 读秒制：主时间用尽后进入读秒——每手 M 秒内落子即可；某一手
    /// 超过 M 秒则消耗一次读秒；次数用完判负。
    Byoyomi {
        /// 每方主时间（秒）。
        main_seconds: f64,
        /// 每手读秒时长（秒）。
        period_seconds: f64,
        /// 读秒次数。
        periods: u32,
    },
}

// 缺省制式 = 无限制（`#[default]` 标在 [`TimeSystem::Unlimited`] 上）。
// 理由：默认值不能悄悄改变既有行为——老用户的 settings.json 里没有
// time_system 键 ⇒ serde 回退到这里，若回退成会判负的制式（包干 /
// 读秒），用户下一次新对局会毫无预期地进入时限并可能被判负。本应用
// 的定位是复盘 / 研究工具，时限必须由用户在「新对局」窗口**主动选择**
// （选择随 settings.json 持久化，下次带出）；无限制同样满足「显示双方
// 累计用时」的要求。

impl TimeSystem {
    /// 界面显示名（新对局窗口的分段选择器用）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Unlimited => "无限制",
            Self::Absolute { .. } => "包干制",
            Self::Byoyomi { .. } => "读秒制",
        }
    }

    /// 读秒时长（非读秒制返回 `None`）：[`SideClock`] 在读秒期重置
    /// 满值时的取值来源。
    pub fn period_seconds(self) -> Option<f64> {
        match self {
            Self::Byoyomi { period_seconds, .. } => Some(period_seconds),
            _ => None,
        }
    }

    /// 一方在制式下的初始时钟。
    pub fn initial_side(self) -> SideClock {
        match self {
            Self::Unlimited => SideClock { main: f64::INFINITY, period: 0.0, periods_left: 0 },
            Self::Absolute { seconds } => SideClock { main: seconds, period: 0.0, periods_left: 0 },
            Self::Byoyomi { main_seconds, period_seconds, periods } => SideClock {
                main: main_seconds,
                // 开局即读秒期（主时间 0）时当前读秒从满值起算——否则
                // 行动方 usable()=0 会在第一帧被误判超时（headless 驱动实测踩坑）。
                period: if main_seconds <= 0.0 { period_seconds } else { 0.0 },
                periods_left: periods,
            },
        }
    }
}

/// 一方的时钟读数。`main = INFINITY` 表示无限制制式（只显示累计用时、
/// 永不判负）；`period` 仅在读秒期（主时间用尽后）有意义。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SideClock {
    /// 主时间剩余（秒）；无限制制式为 `INFINITY`。
    pub main: f64,
    /// 当前读秒的剩余（秒）；主时间未用尽时为 0。
    pub period: f64,
    /// 剩余读秒次数（包干 / 无限制恒 0）。
    pub periods_left: u32,
}

impl SideClock {
    /// 是否已进入读秒期（主时间用尽且制式带读秒）。
    pub fn in_byoyomi(&self) -> bool {
        self.main.is_finite() && self.main <= 0.0 && self.periods_left > 0
    }

    /// 该方剩余「可用时间」（秒）：包干 = 主时间；读秒期 = 当前读秒
    /// 剩余。预算与超时判定的取数口径。无限制返回 `None`（不封顶）。
    pub fn usable(&self) -> Option<f64> {
        if self.main.is_infinite() {
            return None;
        }
        if self.in_byoyomi() {
            Some(self.period.max(0.0))
        } else {
            Some(self.main.max(0.0))
        }
    }
}

/// 给引擎的时间预算（查询 `overrideSettings.maxTime` 的值与出处，
/// 供验证日志比对）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeBudget {
    /// 预算秒数：`max(0.2, 剩余 − 余量)`；读秒期取「一次读秒 − 余量」。
    pub max_time: f64,
    /// 计算预算时该方的剩余可用时间（无限制为 `None`）。
    pub remaining: Option<f64>,
}

/// 由一方时钟计算给引擎的时间预算：`max(0.2, 剩余可用 − 余量)`，读秒
/// 期口径为「一次读秒时长 − 余量」。`maxVisits`（难度档）仍是上限，
/// 时间只是另一条截止线（见模块文档）。
pub fn engine_budget(clock: &SideClock) -> Option<TimeBudget> {
    let remaining = clock.usable()?;
    let max_time = (remaining - BUDGET_MARGIN).max(MIN_BUDGET);
    Some(TimeBudget { max_time, remaining: Some(remaining) })
}

/// 一局棋的双方时钟（制式对局开始时确定，之后只读）。
pub struct Clock {
    /// 时限制式。
    pub system: TimeSystem,
    /// 双方时钟（黑 0 白 1）。
    pub sides: [SideClock; 2],
    /// 双方累计用时（秒）：无限制制式的「用时」显示来源；包干 / 读秒
    /// 也继续累计（对局信息展示，不影响判定）。
    pub total: [f64; 2],
    /// AI 最近一次应手的实际思考时长（终态到达时结算，侧栏显示用）。
    pub last_engine_think: Option<Duration>,
}

impl Clock {
    /// 新对局的初始时钟。
    pub fn new(system: TimeSystem) -> Self {
        Self {
            system,
            sides: [system.initial_side(), system.initial_side()],
            total: [0.0; 2],
            last_engine_think: None,
        }
    }

    /// 按执子取时钟读数。
    pub fn side(&self, stone: Stone) -> &SideClock {
        &self.sides[side_index(stone)]
    }

    fn side_mut(&mut self, stone: Stone) -> &mut SideClock {
        &mut self.sides[side_index(stone)]
    }

    /// 每帧推进 `stone` 一方的真实流逝 `dt`（推进条件见模块文档）。
    ///
    /// 记账顺序：先累计用时，再按制式扣减。读秒制下「主时间扣穿」的
    /// 溢出量当帧即转入读秒（`period` 满值起扣），不跨帧丢秒。
    ///
    /// 返回 `Some(stone)` = 该方超时判负：包干主时间用尽；或读秒期
    /// 扣穿且次数已用尽。读秒期扣穿但次数尚余时**不判负**——消耗一次
    /// （次数减一）并把读秒重置为满值，该手继续。
    pub fn tick(&mut self, stone: Stone, dt: f64) -> Option<Stone> {
        if dt <= 0.0 {
            return None;
        }
        self.total[side_index(stone)] += dt;
        // 制式是 Copy：先复制 period 秒数再借 clock（borrow checker 要求，
        // side_mut 的 &mut 与 self.system 的读取不能交叠）。
        let period_opt = self.system.period_seconds();
        let clock = self.side_mut(stone);
        if clock.main.is_infinite() {
            return None; // 无限制：只累计，永不判负
        }
        if clock.main > 0.0 {
            clock.main -= dt;
            if clock.main >= 0.0 {
                // 包干制「用尽即判负」：main 恰好到 0 也算用尽（下一帧
                // 再判就要读秒兜底，而包干没有读秒）——本帧立即判。
                // 读秒制：恰好到 0 只是进入读秒期（periods_left > 0），
                // 由下方读秒分支接管，不在此判。
                if clock.main == 0.0 && period_opt.is_none() {
                    return Some(stone);
                }
                return None; // 主时间未用尽
            }
            // 主时间本帧扣穿：溢出量转读秒（读秒制）；包干制直接判负。
            let spill = -clock.main;
            clock.main = 0.0;
            let Some(period_secs) = period_opt else {
                return Some(stone); // 包干制：主时间用尽即判负
            };
            if clock.periods_left == 0 {
                return Some(stone); // 无读秒次数的防御分支（读秒制不该出现）
            }
            clock.period = period_secs;
            // 溢出量从读秒继续扣（本帧内跨段，不丢时间）。
            return tick_byoyomi(stone, clock, spill, period_secs);
        }
        // 主时间已用尽：读秒期计时（包干制 main == 0 且无读秒次数，
        // 走下面的判负兜底——正常时序下第一帧就已判负）。
        let period_secs = period_opt.unwrap_or(0.0);
        tick_byoyomi(stone, clock, dt, period_secs)
    }

    /// 人类落子：读秒期内把当前读秒重置为满值（「每手 M 秒内落子即可」
    /// ——落子后下一手重新数满）；主时间不动。其余制式无操作。
    pub fn on_human_move(&mut self, stone: Stone) {
        let period_opt = self.system.period_seconds();
        let in_byoyomi = self.side(stone).in_byoyomi();
        if let Some(period_secs) = period_opt
            && in_byoyomi
        {
            self.side_mut(stone).period = period_secs;
        }
    }

    /// AI 应手完成：按引擎实际思考时长扣减（从发走子口径查询到收到
    /// 终态的墙钟时长）。读秒期内一手超过 M 秒同样消耗一次读秒；次数
    /// 用尽后剩余清零——调用方在**下一次出手前**按 `usable() == 0`
    /// 判 AI 超时负（本手已完成，不追溯）。
    pub fn on_engine_move(&mut self, stone: Stone, think: Duration) {
        let secs = think.as_secs_f64();
        // 累计用时先记账（无限制制式的「用时」显示来源；其它制式同样
        // 累计——AI 的用时按实际思考时间计，不按帧推进）。
        self.total[side_index(stone)] += secs;
        let period_opt = self.system.period_seconds();
        let clock = self.side_mut(stone);
        if clock.main.is_infinite() {
            return; // 无限制：不扣（累计用时已由 tick 记账）
        }
        if clock.main > 0.0 {
            clock.main = (clock.main - secs).max(0.0);
            // 主时间一手用尽且是读秒制：转入读秒期（下一手从读秒起算）。
            if clock.main == 0.0
                && let Some(period_secs) = period_opt
                && clock.periods_left > 0
            {
                clock.period = period_secs;
            }
        } else if let Some(period_secs) = period_opt {
            // 读秒期：超过 M 秒 = 消耗一次并重置；次数用尽剩余清零。
            if secs > clock.period && clock.periods_left > 0 {
                clock.periods_left -= 1;
            }
            if clock.periods_left > 0 {
                clock.period = period_secs;
            } else {
                clock.period = 0.0;
            }
        }
    }
}

/// 执子 → 数组下标（黑 0 白 1）。
fn side_index(stone: Stone) -> usize {
    match stone {
        Stone::Black => 0,
        Stone::White => 1,
    }
}

/// 时钟的用户可读文本（侧栏对弈卡片）：包干显示剩余主时间；读秒制
/// 显示主时间 + 读秒次数，进入读秒期显示当前读秒剩余与剩余次数；
/// 无限制显示累计用时。
pub fn clock_text(system: TimeSystem, clock: &SideClock, total_used: f64) -> String {
    match system {
        TimeSystem::Unlimited => format!("累计 {} 秒", total_used as u64),
        TimeSystem::Absolute { .. } => format_mmss(clock.main.max(0.0)),
        TimeSystem::Byoyomi { .. } => {
            if clock.in_byoyomi() {
                format!(
                    "读秒 {} × {} 次",
                    format_secs_int(clock.period.max(0.0)),
                    clock.periods_left
                )
            } else {
                format!(
                    "{} + 读秒 {}×{}",
                    format_mmss(clock.main.max(0.0)),
                    format_secs_int(system.period_seconds().unwrap_or(0.0)),
                    clock.periods_left
                )
            }
        }
    }
}

/// mm:ss（时钟显示）。
fn format_mmss(secs: f64) -> String {
    let total = secs.ceil() as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// 整秒（读秒与次数显示）。
fn format_secs_int(secs: f64) -> String {
    format!("{}s", secs.ceil() as u64)
}

/// 读秒期扣减（模块级自由函数：与 [`Clock::tick`] 共享逻辑，避免
/// `&mut self` 与 `&mut SideClock` 的借用交叠）。
fn tick_byoyomi(
    stone: Stone,
    clock: &mut SideClock,
    mut dt: f64,
    period_secs: f64,
) -> Option<Stone> {
    // 同帧内可能连续扣穿多次（dt 极大 / period 极小时），循环兜底。
    loop {
        if clock.period > dt {
            clock.period -= dt;
            return None;
        }
        dt -= clock.period;
        clock.period = 0.0;
        if clock.periods_left == 0 {
            return Some(stone); // 次数用尽：判负
        }
        clock.periods_left -= 1;
        if clock.periods_left == 0 {
            return Some(stone); // 最后一次读秒也超了：判负
        }
        clock.period = period_secs;
        if dt <= 0.0 {
            return None;
        }
    }
}

/// 执子 → 时钟数组下标（`Clock::total` / `Clock::sides` 的公开索引口径）。
pub fn side_index_of(stone: Stone) -> usize {
    side_index(stone)
}
