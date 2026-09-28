//! 复盘报告的**棋盘图绘制**（阶段 21 #6）：把一个局面画进 RGBA 像素
//! 缓冲，再经 `png` crate 编码为 PNG 文件。
//!
//! 与 [`super::board_view`]（egui `Painter` 的实时渲染）刻意不耦合：
//! 那边是 GPU 图形管线的矢量绘制，这边是给 Markdown 报告用的**静态
//! 位图**——图是给读者看的，清晰第一（无抗锯齿硬边、无立体感装饰），
//! 也不参与每帧渲染，画错一次比画慢更糟。两边的样式（木底 / 星位 /
//! 最后一手标记 / 失误标记色）各自维护，保持语义一致即可，不做代码级
//! 复用——复用会把实时渲染的性能约束强加给导出路径。
//!
//! # 文字水印（中文字形，零新依赖）
//!
//! 中文字符串直接用 egui 已加载的中文字体光栅化：[`egui::Context::fonts`]
//! 布局出 [`epaint::Galley`]（字形按需光栅化进字体图集），再取图集像素
//! 与 galley 各行网格（`row.visuals.mesh` 顶点：屏幕位置 + 归一化 UV +
//! 颜色）在目标缓冲上采样合成。混合语义与 egui_glow 后端一致：gamma
//! 空间、premultiplied alpha（`blendFunc(ONE, ONE_MINUS_SRC_ALPHA)`），
//! 片元色 = 顶点色 × 贴图色。水印用不透明黑字，在木底上的合成结果即
//! 「底色按 (1 − 图集α) 变暗」。
//!
//! # 图内标注口径（与棋盘叠加层同色系）
//!
//! - 网格线深棕、星位实心点、黑白子圆盘（白子带描边以在木底上可辨）；
//! - 最后一手：实心小方点（与实时渲染的「最后一手」标记同语义）；
//! - **该手失误标记**：按 [`Severity`] 着色的实心小色点，叠在实战点
//!   棋子中心（疑问手黄 / 失误橙 / 恶手红，与叠加层 `severity_color`
//!   同一套 RGB）；
//! - **实战点**：橙色方框框住该子；**引擎首选**：蓝色圆圈。框 / 圈
//!   的图例写进报告正文（报告端负责）。
//!
//! 纯绘制函数（无全局状态）；I/O 只在 `encode_png` 的调用方。

use crate::board::{Coord, Size, Stone};
use crate::ui::analysis::Severity;
use egui::epaint::{Color32, Mesh, Vertex};

/// 棋盘图配色（木底 / 线 / 子）。与实时渲染的观感一致即可，数值独立。
mod palette {
    use egui::epaint::Color32;

    /// 木色底（棋盘本体）。
    pub(crate) const WOOD: Color32 = Color32::from_rgb(220, 179, 122);
    /// 边距底色（图边框区域，深一档，与实时渲染的深色画布同语义）。
    pub(crate) const MARGIN: Color32 = Color32::from_rgb(52, 45, 38);
    /// 网格线（深棕）。
    pub(crate) const LINE: Color32 = Color32::from_rgb(92, 66, 40);
    /// 黑子。
    pub(crate) const BLACK: Color32 = Color32::from_rgb(28, 28, 30);
    /// 白子本体。
    pub(crate) const WHITE: Color32 = Color32::from_rgb(250, 250, 246);
    /// 白子描边（木底上白子需要轮廓才可辨）。
    pub(crate) const WHITE_EDGE: Color32 = Color32::from_rgb(110, 100, 88);
    /// 星位。
    pub(crate) const STAR: Color32 = Color32::from_rgb(70, 50, 30);
    /// 最后一手标记（琥珀点，与实时渲染同语义）。
    pub(crate) const LAST_MOVE: Color32 = Color32::from_rgb(255, 170, 40);
    /// 实战点方框（橙，与实时渲染的失误标注橙同色系）。
    pub(crate) const ACTUAL_BOX: Color32 = Color32::from_rgb(255, 152, 82);
    /// 引擎首选圆圈（蓝，与实时渲染的候选点首选同语义）。
    pub(crate) const BEST_CIRCLE: Color32 = Color32::from_rgb(56, 118, 235);
}

/// 星位坐标（按路数）。与实时渲染的星位一致（19 路 3-3 / 天元等）。
fn star_points(size: Size) -> Vec<Coord> {
    let n = size.n();
    let edge = 3; // 距边 3 路（19 路惯例）
    let far = n - 1 - edge;
    let mid = n / 2;
    let mut pts = Vec::new();
    for &x in &[edge, mid, far] {
        for &y in &[edge, mid, far] {
            // 13 路只取角 + 天元（4 星 + 天元），19 路 9 星。
            if n == 13 && x != mid && y != mid {
                continue;
            }
            if let Some(c) = Coord::new(size, x, y) {
                pts.push(c);
            }
        }
    }
    pts
}

/// 严重程度 → 图内失误标记色（与叠加层 `severity_color` 同一套 RGB）。
fn severity_color(severity: Severity) -> Color32 {
    match severity {
        Severity::Good | Severity::Fine => Color32::from_rgb(140, 140, 148),
        Severity::Questionable => Color32::from_rgb(248, 196, 42),
        Severity::Mistake => Color32::from_rgb(255, 152, 82),
        Severity::Blunder => Color32::from_rgb(235, 77, 61),
    }
}

/// 画布布局：按路数适配的几何量（像素）。
struct Layout {
    /// 画布边长（正方形，含边距）。
    side: usize,
    /// 棋盘区左上角（含边距偏移）。
    origin: (i32, i32),
    /// 相邻交叉点间距。
    cell: i32,
    /// 线宽。
    line_w: i32,
    /// 棋子半径。
    stone_r: i32,
}

impl Layout {
    fn new(size: Size) -> Self {
        let n = size.n() as i32;
        // 19 路 ≈ 760 + 边距；间距随路数反比缩放（9 路点距更大）。
        let cell = match n {
            19 => 40,
            13 => 58,
            _ => 84,
        };
        let pad = cell * 3 / 2 + 12;
        let side = (cell * (n - 1) + 2 * pad) as usize;
        let line_w = (cell / 16).max(2);
        let stone_r = cell / 2 - 2;
        Self {
            side,
            origin: (pad, pad),
            cell,
            line_w,
            stone_r,
        }
    }

    /// 交叉点的像素中心（内部坐标 → 画布坐标）。
    fn center(&self, at: Coord) -> (i32, i32) {
        (
            self.origin.0 + at.x() as i32 * self.cell,
            self.origin.1 + at.y() as i32 * self.cell,
        )
    }
}

/// 一块可绘制的 RGBA 画布（行优先，sRGB，不透明）。
struct Canvas {
    width: usize,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: usize, height: usize, color: Color32) -> Self {
        let mut pixels = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            pixels.extend_from_slice(&[color.r(), color.g(), color.b(), color.a()]);
        }
        Self { width, pixels }
    }

    /// 不透明色填充矩形（越界裁剪，坐标可为负）。
    fn fill_rect(&mut self, x0: i32, y0: i32, w: i32, h: i32, color: Color32) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                self.set(x, y, color);
            }
        }
    }

    /// 写单像素（越界静默忽略；圆 / 线绘制循环直接用画布坐标，无需判界）。
    fn set(&mut self, x: i32, y: i32, color: Color32) {
        if x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.pixels.len() / 4 / self.width {
            return;
        }
        let o = (y * self.width + x) * 4;
        self.pixels[o..o + 3].copy_from_slice(&[color.r(), color.g(), color.b()]);
        self.pixels[o + 3] = 255;
    }

    /// 布尔形状掩码填充（`true` 的位置画 `color`）：圆 / 环 / 框共用。
    fn fill_mask<F: Fn(i32, i32) -> bool>(
        &mut self,
        cx: i32,
        cy: i32,
        r: i32,
        color: Color32,
        inside: F,
    ) {
        for y in cy - r..=cy + r {
            for x in cx - r..=cx + r {
                if inside(x, y) {
                    self.set(x, y, color);
                }
            }
        }
    }

    /// 实心圆盘。
    fn disc(&mut self, cx: i32, cy: i32, radius: i32, color: Color32) {
        let r2 = radius * radius;
        self.fill_mask(cx, cy, radius, color, |x, y| {
            let dx = x - cx;
            let dy = y - cy;
            dx * dx + dy * dy <= r2
        });
    }

    /// 圆环（线宽 `w`）。
    fn ring(&mut self, cx: i32, cy: i32, radius: i32, w: i32, color: Color32) {
        let r_out = radius + w / 2;
        let r_in = radius - w / 2 - (w % 2);
        let r_out2 = r_out * r_out;
        let r_in2 = r_in * r_in;
        self.fill_mask(cx, cy, r_out, color, |x, y| {
            let d2 = (x - cx) * (x - cx) + (y - cy) * (y - cy);
            d2 <= r_out2 && d2 > r_in2
        });
    }

    /// 正方形描边框（线宽 `w`），`r` 为半边长。
    fn box_outline(&mut self, cx: i32, cy: i32, r: i32, w: i32, color: Color32) {
        let lo = cx - r..=cx + r;
        // 上下两边与左右两边；直接按行 / 列带填充，避免逐点判定。
        self.fill_rect(cx - r, cy - r - w / 2, 2 * r + 1, w, color);
        self.fill_rect(cx - r, cy + r - w / 2 - (w - 1) / 2, 2 * r + 1, w, color);
        self.fill_rect(cx - r - w / 2, cy - r, w, 2 * r + 1, color);
        self.fill_rect(cx + r - w / 2 - (w - 1) / 2, cy - r, w, 2 * r + 1, color);
        let _ = lo;
    }

    /// 水平线段（含端点）。
    fn hline(&mut self, x0: i32, x1: i32, y: i32, w: i32, color: Color32) {
        self.fill_rect(x0, y - w / 2, x1 - x0 + 1, w, color);
    }

    /// 垂直线段（含端点）。
    fn vline(&mut self, x: i32, y0: i32, y1: i32, w: i32, color: Color32) {
        self.fill_rect(x - w / 2, y0, w, y1 - y0 + 1, color);
    }
}

/// 把 egui 字体图集字形合进画布（水印渲染核心，见模块文档的混合语义）。
struct TextBlitter<'a> {
    /// 字体图集像素（sRGB，含 α；字形为白色 × α）。
    atlas: &'a [Color32],
    atlas_w: usize,
    atlas_h: usize,
}

impl<'a> TextBlitter<'a> {
    /// 取图集一个 texel（越界 = 全透明）。
    fn texel(&self, u: i32, v: i32) -> [u8; 4] {
        if u < 0 || v < 0 {
            return [0, 0, 0, 0];
        }
        let (u, v) = (u as usize, v as usize);
        if u >= self.atlas_w || v >= self.atlas_h {
            return [0, 0, 0, 0];
        }
        let c = self.atlas[v * self.atlas_w + u];
        [c.r(), c.g(), c.b(), c.a()]
    }

    /// 双线性采样图集（**texel 坐标**：galley 网格顶点的 `uv` 是布局期的
    /// texel 索引，归一化发生在 tessellate 阶段——本模块不经 tessellator
    /// 直接采样，故自行保持 texel 语义；运行时探针实证
    /// `uv=(145.0,0.0)`、图集 2048×32），返回 [r, g, b, a]（premultiplied）。
    fn sample(&self, u: f32, v: f32) -> [u8; 4] {
        let x = u - 0.5;
        let y = v - 0.5;
        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let mut acc = [0.0f32; 4];
        for (du, wu) in [(0, 1.0 - fx), (1, fx)] {
            for (dv, wv) in [(0, 1.0 - fy), (1, fy)] {
                let t = self.texel(x0 + du, y0 + dv);
                let w = wu * wv;
                for i in 0..4 {
                    acc[i] += t[i] as f32 * w;
                }
            }
        }
        acc.map(|f| f.round().clamp(0.0, 255.0) as u8)
    }

    /// 把一个 galley 的网格合到画布上：顶点色 × 贴图色（gamma 空间），
    /// 目标不透明 → 混合 = `dst*(1-α) + src.rgb*α`（src 已预乘，src.rgb=0
    /// 时的黑字即把底色压暗；带背景色的 galley 同样正确）。
    fn blit_galley(&self, canvas: &mut Canvas, mesh: &Mesh, dx: i32, dy: i32) {
        // 网格均为三角形列表；逐三角形逐像素仿射采样（水印就几行字，
        // 面积小，不需要光栅化器级别的优化）。
        for tri in mesh.indices.as_chunks::<3>().0 {
            let v = [
                mesh.vertices[tri[0] as usize],
                mesh.vertices[tri[1] as usize],
                mesh.vertices[tri[2] as usize],
            ];
            self.rasterize_triangle(canvas, &v, dx, dy);
        }
    }

    /// 三角形光栅化：包围盒 + 重心坐标（含 1/2 像素外扩抗缝）。
    fn rasterize_triangle(&self, canvas: &mut Canvas, v: &[Vertex; 3], dx: i32, dy: i32) {
        // 顶点屏幕坐标（points 即像素：水印按 pixels_per_point=1 布局）。
        let p = [0usize, 1, 2].map(|i| (v[i].pos.x + dx as f32, v[i].pos.y + dy as f32));
        let min_x = p[0].0.min(p[1].0).min(p[2].0).floor() as i32;
        let max_x = p[0].0.max(p[1].0).max(p[2].0).ceil() as i32;
        let min_y = p[0].1.min(p[1].1).min(p[2].1).floor() as i32;
        let max_y = p[0].1.max(p[1].1).max(p[2].1).ceil() as i32;
        // 有向面积的 2 倍（退化为零时跳过）。
        let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1)
            - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
        if area.abs() < 1e-9 {
            return;
        }
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                // 像素中心采样。
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                let w0 = ((p[1].0 - p[0].0) * (fy - p[0].1) - (fx - p[0].0) * (p[1].1 - p[0].1))
                    / area;
                let w1 = ((fx - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (fy - p[0].1))
                    / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                // UV 与颜色按重心坐标插值。
                let u = w0 * v[0].uv.x + w1 * v[1].uv.x + w2 * v[2].uv.x;
                let vv = w0 * v[0].uv.y + w1 * v[1].uv.y + w2 * v[2].uv.y;
                let c = self.sample(u, vv);
                let col = [0usize, 1, 2].map(|i| {
                    let g = w0 * v[0].color[i] as f32
                        + w1 * v[1].color[i] as f32
                        + w2 * v[2].color[i] as f32;
                    g.round().clamp(0.0, 255.0) as u8
                });
                let a = c[3] as f32 / 255.0;
                if a <= 0.0 {
                    continue;
                }
                // src（预乘）与 dst（不透明）在 gamma 空间按「dst*(1-α)+src」混合。
                let o = ((py as usize) * canvas.width + px as usize) * 4;
                if o + 3 >= canvas.pixels.len() {
                    continue;
                }
                for i in 0..3 {
                    let src = col[i] as f32 * (c[i] as f32 / 255.0);
                    let dst = canvas.pixels[o + i] as f32;
                    canvas.pixels[o + i] = (dst * (1.0 - a) + src).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

/// 画水印文字：`text` 用 egui 已装中文字体布局并合到画布 `(x, y)`
/// （左上角对齐）。返回实际行高（供多行水印排版），布局失败（理论
/// 不可达：字体已在 UI 启动时装好）返回 0。
fn draw_watermark(canvas: &mut Canvas, ctx: &egui::Context, text: &str, x: i32, y: i32) {
    let (meshes, atlas): (Vec<Mesh>, egui::epaint::ColorImage) = ctx.fonts_mut(|fonts| {
        let galley = fonts.layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(22.0),
            Color32::BLACK,
        );
        // 各行网格平移到画布坐标（PlacedRow.pos 相对 galley 原点；
        // vertex.pos 相对行原点，见 tessellate_text 的合成方式）。
        let meshes = galley
            .rows
            .iter()
            .filter(|row| !row.visuals.mesh.is_empty())
            .map(|row| {
                let mut mesh = row.visuals.mesh.clone();
                for v in &mut mesh.vertices {
                    v.pos += row.pos.to_vec2();
                }
                mesh
            })
            .collect();
        (meshes, fonts.image())
    });
    let blitter = TextBlitter {
        atlas: &atlas.pixels,
        atlas_w: atlas.width(),
        atlas_h: atlas.height(),
    };
    for mesh in meshes {
        blitter.blit_galley(canvas, &mesh, x, y);
    }
}

/// 单个局面图的数据：走子后的盘面 + 该手的标注信息。
pub struct BoardDiagram<'a> {
    /// 棋盘尺寸。
    pub size: Size,
    /// 走子后的盘面（行优先，下标 = [`Coord::index`]）。
    pub grid: &'a [Option<Stone>],
    /// 最后一手（`None` = 空盘或该手为弃着——弃着不改变盘面，无标记）。
    pub last_move: Option<Coord>,
    /// 该手的失误标记色对应的分级（叠加在实战点棋子上）。
    pub severity: Severity,
    /// 实战落点（弃着为 `None`：无框可画，报告正文如实写「弃着」）。
    pub actual: Option<Coord>,
    /// 引擎首选（候选表缺失 / 首选为弃着时为 `None`，不画圈）。
    pub best: Option<Coord>,
}

/// 画一张棋盘图并编码为 PNG 字节（含「手数 · 行棋方 · 结论」水印）。
///
/// `caption` 为图顶水印文字（如「第 10 手 · 白损失 6.0 目（恶手）」），
/// 用 egui 已装中文字体渲染——调用方须保证 `ctx` 已跑过至少一帧 UI
/// （字体已加载；headless 与真实运行均成立）。
pub fn render_board_png(
    ctx: &egui::Context,
    diagram: &BoardDiagram<'_>,
    caption: &str,
) -> Vec<u8> {
    let size = diagram.size;
    let layout = Layout::new(size);
    let side = layout.side;
    let mut canvas = Canvas::new(side, side, palette::MARGIN);
    // 木色底（棋盘本体区域）。
    let board_px = layout.cell * (size.n() as i32 - 1) + layout.cell;
    canvas.fill_rect(
        layout.origin.0 - layout.cell / 2,
        layout.origin.1 - layout.cell / 2,
        board_px,
        board_px,
        palette::WOOD,
    );
    // 网格：外框画粗一圈（与实时渲染一致）。
    let n = size.n() as i32;
    let last = layout.cell * (n - 1);
    let (ox, oy) = layout.origin;
    for i in 0..n {
        let d = layout.cell * i;
        let w = if i == 0 || i == n - 1 {
            layout.line_w * 2
        } else {
            layout.line_w
        };
        canvas.hline(ox, ox + last, oy + d, w, palette::LINE);
        canvas.vline(ox + d, oy, oy + last, w, palette::LINE);
    }
    // 星位。
    for at in star_points(size) {
        let (cx, cy) = layout.center(at);
        let r = layout.cell / 6;
        canvas.disc(cx, cy, r, palette::STAR);
    }
    // 棋子。
    for (index, stone) in diagram.grid.iter().enumerate() {
        let Some(stone) = stone else { continue };
        let Some(at) = Coord::from_index(size, index) else {
            continue;
        };
        let (cx, cy) = layout.center(at);
        match stone {
            Stone::Black => {
                canvas.disc(cx, cy, layout.stone_r, palette::BLACK);
            }
            Stone::White => {
                canvas.disc(cx, cy, layout.stone_r, palette::WHITE);
                // 白子描边：先画一圈深色环再补内盘（环直径略大）。
                canvas.ring(cx, cy, layout.stone_r, 2, palette::WHITE_EDGE);
            }
        }
    }
    // 最后一手标记：实心小方点（琥珀，与实时渲染同语义）。
    if let Some(at) = diagram.last_move {
        let (cx, cy) = layout.center(at);
        let r = layout.cell / 8;
        canvas.fill_rect(cx - r, cy - r, 2 * r + 1, 2 * r + 1, palette::LAST_MOVE);
    }
    // 引擎首选：蓝圆圈（画在棋子之上、子为空时也在交点上可见）。
    if let Some(at) = diagram.best {
        let (cx, cy) = layout.center(at);
        canvas.ring(
            cx,
            cy,
            layout.stone_r + 3,
            (layout.line_w * 2).max(3),
            palette::BEST_CIRCLE,
        );
    }
    // 实战点方框（画在首选圈之后：两标注重叠时以实战框为显眼）。
    if let Some(at) = diagram.actual {
        let (cx, cy) = layout.center(at);
        canvas.box_outline(
            cx,
            cy,
            layout.stone_r + 6,
            (layout.line_w * 2).max(3),
            palette::ACTUAL_BOX,
        );
    }
    // 失误标记：按分级着色的实心小色点，叠在实战子中心（画在最后，
    // 不被框 / 圈遮盖；与叠加层「子中心小色点」同语义）。
    if let Some(at) = diagram.actual {
        let (cx, cy) = layout.center(at);
        let r = layout.cell / 5;
        canvas.disc(cx, cy, r, severity_color(diagram.severity));
    }
    // 水印：顶部标题行（图中自带结论，单独保存的 PNG 自足可读）。
    draw_watermark(&mut canvas, ctx, caption, 10, 6);
    encode_png(canvas.width, side, canvas.pixels)
}

/// 把 RGBA 缓冲编码为 PNG（`png` crate；8-bit RGBA，无隔行）。
/// 编码失败（缓冲长度不符等编程性错误）返回错误文本，由调用方提示。
pub fn encode_png(width: usize, height: usize, rgba: Vec<u8>) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("PNG 头写入（内存目标，不会失败）");
    writer.write_image_data(&rgba).expect("PNG 数据写入（缓冲自洽）");
    writer.finish().expect("PNG 收尾");
    out.into_inner()
}
