//! 小棋盘 PV 回放面板（第 6 项）：把某个候选点的**主变（PV）逐手摆上**
//! 独立小盘，按可设间隔自动回放，看「这手之后会变成什么样」。
//!
//! 纪律与定位：
//!
//! - **绝不改棋谱树**：回放是纯预览——从当前盘面网格出发，在**本地合成
//!   网格**上用 [`crate::board::rules::apply_move`]（纯函数，含提子演算）
//!   逐手叠加 PV 着法，不触碰 [`Board`]、不落子、不建分支、不写 SGF、
//!   不动游标。与「沿主变前进」的纪律一致（那次刻意只走已有分支），
//!   这里更进一步：连谱都不走，只在像素上预演。
//! - **复用主棋盘绘制**：网格 / 星位 / 坐标 / 棋子全部调用
//!   `board_view` 提升为 `pub(crate)` 的同一批函数（`Layout::fit` 给出
//!   同一套几何），小盘不是第二套画法——主棋盘渲染改动自动跟随。
//! - **数据源**：优先侧栏聚焦的候选点（`overlay.focus`），未聚焦时回落
//!   引擎首选（`snapshot.moves[0]`，避免面板空着）；面板标题行写明当前
//!   显示的是哪个候选点、来自聚焦还是首选。
//! - **与主棋盘幽灵子的关系**：主棋盘上的 PV 幽灵子（聚焦时）与侧栏
//!   PV 文本保持原样——幽灵子是「全程淡淡叠在主盘上」的静态预览，
//!   回放盘是「同一条 PV 按节奏逐手放大看」，信息同源（同一 `info.pv`）
//!   不打架；回放盘额外能看到 PV 全长（不截 [`PV_LIMIT`] 手）。

use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Ui, Vec2};

use crate::board::{rules, Coord, Size, Stone};
use crate::ui::analysis::Snapshot;
use crate::ui::board_view::{draw_board, draw_coordinates, draw_grid, draw_stars, draw_stone, Layout};
use crate::ui::overlay::Overlay;

/// 回放默认间隔（秒 / 手）。
pub(crate) const DEFAULT_INTERVAL_SECS: f64 = 1.0;
/// 间隔可调范围（秒）。
const INTERVAL_RANGE: std::ops::RangeInclusive<f64> = 0.25..=3.0;
/// 回放盘的边长（逻辑像素）：240–360 建议区间取中，不挤占主棋盘。
pub(crate) const MINI_SIDE: f32 = 300.0;
/// 小盘标题与状态文字色。
const LABEL: Color32 = Color32::from_rgb(160, 160, 168);
/// 回放进度文字强调色（琥珀系，与全局强调一致）。
const ACCENT: Color32 = Color32::from_rgb(255, 170, 40);

/// `compose` 的返回：`(合成网格, 实际摆上的 (PV 序号, 落点, 颜色) 列表)`。
pub(crate) type Composed = (Vec<Option<Stone>>, Vec<(usize, Coord, Stone)>);

/// 小棋盘回放状态机（App 持有，跨帧保持）。
/// 手写 `Default`（`last_advance` 需真实时刻，无法派生）。
#[derive(Debug)]
pub struct MiniBoard {
    /// 已摆到 PV 的第几手（0 = 只显示当前盘面；最大 = PV 长度）。
    step: usize,
    /// 是否正在自动回放。
    playing: bool,
    /// 上次自动前进的时刻。
    last_advance: Instant,
    /// 每手间隔（秒）。
    interval: f64,
    /// 当前回放针对的候选落点（切换检测依据：变了就从头开始）。
    current: Option<Coord>,
}

impl Default for MiniBoard {
    fn default() -> Self {
        Self {
            step: 0,
            playing: true,
            last_advance: Instant::now(),
            interval: DEFAULT_INTERVAL_SECS,
            current: None,
        }
    }
}

/// 回放面板的一帧结果（预留）：本模块不改任何棋盘 / 分析状态，
/// 只有自身的播放状态可变；聚焦定位仍由侧栏候选列表负责。
#[derive(Default, Debug)]
pub struct MiniAction {}

impl MiniBoard {
    /// 回放盘的落子坐标序列（验证与绘制共用）：从**当前盘面**出发，
    /// 把 PV 前 `step` 手叠加在本地合成网格上（含提子演算），返回合成
    /// 网格 + 实际摆上的 (序号, 坐标, 颜色) 列表。PV 中断着（盘外 /
    /// 占用，理论不出现）跳过并如实截断。
    ///
    /// 纯函数：输入网格克隆，不改任何共享状态。
    pub fn compose(
        size: Size,
        base: &[Option<Stone>],
        to_play: Stone,
        pv: &[Option<Coord>],
        step: usize,
    ) -> Composed {
        let mut grid = base.to_vec();
        let mut placed = Vec::new();
        let mut player = to_play;
        for (i, mv) in pv.iter().take(step).enumerate() {
            let Some(at) = *mv else { continue };
            // PV 着法理论上恒合法（引擎自洽）；占用 / 盘外等意外一律跳过，
            // 绝不 panic，回放停在最近合法处。
            if rules::apply_move(size, &mut grid, player, at).is_ok() {
                placed.push((i, at, player));
            }
            player = player.opposite();
        }
        (grid, placed)
    }

    /// 当前回放目标：聚焦候选点优先，回落引擎首选。返回
    /// `(落点, PV, 来源标注)`；无快照 / 无候选返回 `None`（面板空置）。
    /// PV 不截短——回放盘的存在意义就是看比幽灵子（截 8 手）更远的未来。
    pub fn target<'a>(
        snapshot: &'a Snapshot,
        overlay: &Overlay,
    ) -> Option<(Coord, &'a [Option<Coord>], &'static str)> {
        if let Some(focus) = &overlay.focus
            && let Some(info) = snapshot.moves.iter().find(|info| Some(focus.at) == info.mv)
        {
            return Some((focus.at, &info.pv, "聚焦"));
            // 聚焦的候选刚被引擎排序挤出当前表（流式刷新中可能发生）：
            // 不硬凑，回落首选并如实标注，避免 PV 与落点错配。
        }
        let best = snapshot.moves.first()?;
        let at = best.mv?;
        Some((at, &best.pv, "引擎首选"))
    }

    /// 验证 / 驱动用访问器（字段私有，状态机只经这组方法读写）。
    pub fn step_value(&self) -> usize {
        self.step
    }

    /// 是否正在自动播放。
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// 设置每手间隔（秒；面板 DragValue 也写入这里，统一钳制）。
    pub fn set_interval(&mut self, secs: f64) {
        self.interval = secs.clamp(*INTERVAL_RANGE.start(), *INTERVAL_RANGE.end());
        self.last_advance = Instant::now();
    }

    /// 当前步数。
    pub fn step(&self) -> usize {
        self.step
    }

    /// 自动推进一步（`show` 的推进块即调用本函数）：播放中且距上次推进
    /// 已到间隔才前进；到 PV 末尾自动暂停（不循环）。单独成函数：
    /// headless 驱动与面板绘制共用同一份推进逻辑，验证不会测到第二套实现。
    pub fn tick(&mut self, snapshot: Option<&Snapshot>, overlay: &Overlay) {
        if self.playing
            && self.last_advance.elapsed() >= Duration::from_secs_f64(self.interval)
            && snapshot.is_some_and(|snapshot| {
                Self::target(snapshot, overlay).is_some_and(|(_, pv, _)| self.step < pv.len())
            })
        {
            self.step += 1;
            self.last_advance = Instant::now();
            // 到末尾：停住（不循环）。用户点播放重新开始前保持静止。
            if snapshot.is_some_and(|snapshot| {
                Self::target(snapshot, overlay).is_some_and(|(_, pv, _)| self.step >= pv.len())
            }) {
                self.playing = false;
            }
        }
    }

    /// 绘制回放面板（底部面板区，与曲线面板并列）。`ui` 为面板内容区。
    ///
    /// `base` / `to_play` 取自当前棋盘（只读）；快照未就绪或无候选时
    /// 显示占位说明。
    pub fn show(
        &mut self,
        ui: &mut Ui,
        board_grid: &[Option<Stone>],
        size: Size,
        to_play: Stone,
        snapshot: Option<&Snapshot>,
        overlay: &Overlay,
    ) {
        let action = MiniAction::default();
        // 顶部一行：候选点标注 + 播放控制 + 间隔调节。
        ui.horizontal(|ui| {
            ui.label(RichText::new("小棋盘回放").strong());
            let Some(snapshot) = snapshot else {
                ui.weak("（等待引擎分析当前局面…）");
                return;
            };
            let Some((at, pv, source)) = Self::target(snapshot, overlay) else {
                ui.weak("（当前局面无候选点）");
                return;
            };
            // 切换候选点检测：目标变了 → 步数归零、计时重置（按 playing
            // 状态重新起算，暂停时保持暂停）。
            if self.current != Some(at) {
                self.current = Some(at);
                self.step = 0;
                self.last_advance = Instant::now();
            }

            ui.label(
                RichText::new(format!("候选 {}（{source}）", at.to_gtp(size)))
                    .color(ACCENT)
                    .strong(),
            );
            ui.separator();
            if ui.button(if self.playing { "⏸ 暂停" } else { "▶ 播放" }).clicked() {
                self.playing = !self.playing;
                self.last_advance = Instant::now();
            }
            if ui.button("⏭ 下一手").clicked() {
                self.playing = false;
                self.step = (self.step + 1).min(pv.len());
                self.last_advance = Instant::now();
            }
            if ui.button("⏮ 重来").clicked() {
                self.step = 0;
                self.last_advance = Instant::now();
            }
            ui.weak("间隔");
            if ui
                .add(
                    egui::DragValue::new(&mut self.interval)
                        .speed(0.1)
                        .range(INTERVAL_RANGE)
                        .suffix(" 秒"),
                )
                .changed()
            {
                self.last_advance = Instant::now();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "{}/{} 手{}",
                        self.step,
                        pv.len(),
                        if pv.len() > super::analysis_panel::PV_LIMIT { "（幽灵子只叠 8 手）" } else { "" }
                    ))
                    .color(LABEL)
                    .size(11.5),
                );
            });
        });

        // 自动推进：播放中且已到间隔才前进一手（到 PV 末尾自动暂停）。
        // 检查放在绘制前，本帧即反映最新步数；真实时间由帧间 sleep 保证。
        self.tick(snapshot, overlay);

        // 小盘绘制：复用主棋盘的网格 / 星位 / 坐标 / 棋子画法。
        let avail = ui.available_rect_before_wrap();
        let side = MINI_SIDE.min(avail.height().max(120.0)).min(avail.width());
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), side), Sense::hover());
        let Some(layout) = Layout::fit(Rect::from_min_size(rect.min, Vec2::splat(side)), size) else {
            let _ = action;
            return;
        };
        let painter = ui.painter_at(layout.rect.expand(2.0));
        draw_board(&painter, &layout);
        draw_grid(&painter, &layout, size);
        draw_stars(&painter, &layout, size);
        draw_coordinates(&painter, &layout, size);

        if let Some(snapshot) = snapshot
            && let Some((_, pv, _)) = Self::target(snapshot, overlay)
        {
            let (_grid, placed) = Self::compose(size, board_grid, to_play, pv, self.step);
            let radius = layout.spacing * 0.47;
            for (i, at, player) in &placed {
                draw_stone(&painter, layout.point(*at), radius, *player);
                // 回放序号小标注：一眼看出这是 PV 第几手（1 基显示）。
                painter.text(
                    layout.point(*at) + Vec2::new(0.0, 1.0),
                    Align2::CENTER_CENTER,
                    (i + 1).to_string(),
                    FontId::proportional((layout.spacing * 0.3).clamp(8.0, 12.0)),
                    match player {
                        Stone::Black => Color32::from_rgba_unmultiplied(255, 255, 255, 210),
                        Stone::White => Color32::from_rgba_unmultiplied(20, 20, 22, 210),
                    },
                );
            }
            // 回放未开始时给一行提示（网格上没东西，别让用户以为坏了）。
            if placed.is_empty() && self.step == 0 {
                painter.text(
                    layout.rect.center(),
                    Align2::CENTER_CENTER,
                    "按「下一手」或播放开始逐手回放",
                    FontId::proportional(12.0),
                    LABEL,
                );
            }
        }
        let _ = action;
    }
}

