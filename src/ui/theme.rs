//! 全局主题：启动时一次性应用到 `egui::Context`（`GuanqiApp::new`），
//! 不逐帧重设。
//!
//! 设计目标「去 TUI 化」：
//! - 深色背景三级分层：窗口底 < 面板底 < 卡片底（见 [`colors`]），
//!   让侧栏分区卡片、弹窗与底层面板一眼可分；
//! - 强调色取棋盘木色系的琥珀（与分支选择器 / 曲线当前手指示同源），
//!   按钮边框、选中态、分段选择器都向它靠拢；
//! - 按钮三态（normal / hover / active）用「底色渐亮 + 琥珀描边浮现」
//!   表达，与 egui 默认的灰阶三态区分明显；
//! - 圆角与内边距统一放大（按钮 5px 圆角 + (10,5) 内边距），使默认
//!   `ui.button` 出来就是「看得清的按钮」，无需每个调用点手动包样式。

use egui::{
    Color32, CursorIcon, FontFamily, Margin, Stroke, TextStyle, Vec2, Visuals, style::Selection,
};

/// 主题色板与常用色（供各视图取用，避免散落 magic number）。
pub mod colors {
    use egui::Color32;

    /// 深琥珀（强调色的深化形态：活动边框、文档活动项左侧强调条）。
    pub const ACCENT_DEEP: Color32 = Color32::from_rgb(200, 128, 24);
    /// 深琥珀的暗底（文档活动项填充、选中背景）。
    pub const ACCENT_DIM: Color32 = Color32::from_rgb(86, 66, 26);
    /// 文档活动项左侧强调条（琥珀，与棋盘分支选择器同源）。
    pub const ACCENT_BAR: Color32 = Color32::from_rgb(255, 170, 40);
    /// 成功 / 状态绿色。
    pub const OK: Color32 = Color32::from_rgb(140, 220, 140);
    /// 留意 / 警示橙色。
    pub const WARN: Color32 = Color32::from_rgb(255, 190, 90);
    /// 错误红色。
    pub const ERROR: Color32 = Color32::from_rgb(255, 120, 110);
    /// 状态栏底色（棋盘下方分段状态栏）。
    pub const STATUS_BG: Color32 = Color32::from_rgb(28, 31, 39);
    /// 状态栏分段底色。
    pub const STATUS_SEG: Color32 = Color32::from_rgb(38, 42, 52);

    /// 正文浅灰（自绘行 / 段落文字的默认色，与 widgets.inactive.fg_stroke 同值）。
    pub const TEXT: Color32 = Color32::from_rgb(214, 218, 226);
    /// 弱化灰（辅助小字 / visits 列 / 图例说明；等同 weak 文本观感）。
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(150, 156, 166);
    /// 自绘行常态底色（候选行 / 排行行 / 文档行的「卡片内凹底」）。
    pub const ROW_REST: Color32 = Color32::from_rgb(39, 43, 53);
    /// 自绘行悬停底色（与控件 hover 亮底同档）。
    pub const ROW_HOVER: Color32 = Color32::from_rgb(52, 57, 70);
    /// 琥珀底上的强调文字（选中态行文本、主按钮文字）。
    pub const TEXT_ON_ACCENT: Color32 = Color32::from_rgb(255, 214, 140);
}

/// 侧栏分区卡片的 Frame：卡片底色 + 圆角 + 内边距。
///
/// **无描边**：卡片观感靠「比面板底亮一档的浮起」表达（ZCode 设置式），
/// 描边会把卡片拉回「线条分隔的 TUI 感」。各调用点内容不同，间距由
/// 调用方用 `ui.add_space` 控制。
pub fn card_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(Color32::from_rgb(41, 45, 56))
        .corner_radius(8.0)
        .inner_margin(Margin::same(12))
}

/// 侧栏分区标题（卡片内的第一行）：字号更大、用强调色、加粗。
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(15.0)
            .strong()
            .color(Color32::from_rgb(255, 196, 96)),
    );
    ui.add_space(2.0);
}

/// 表单控件行的标准高度（= 主题 `interact_size.y`，22）。
///
/// ⚠️ egui 的 `add_sized([w, 0.0], …)` 用 0 高期望区域配合居中对齐，
/// 控件会上下各溢出半个行高（Grid 行高收集不到 → 重叠；horizontal 里
/// 逐个错位/阶梯）。**凡是 add_sized / 手动 allocate 定宽定高，高度一律
/// 给本常量**，绝不用 0。
pub fn row_height(ui: &egui::Ui) -> f32 {
    ui.spacing().interact_size.y
}

/// DragValue 的实际渲染高度（含边框，实测 26 = interact 22 + 边框余量）。
/// [`param_row`] 的固定行高按它取值——行内最高的就是 DragValue。
pub const PARAM_ROW_HEIGHT: f32 = 26.0;

/// 固定行高的水平参数行（「标签 + 输入框 + 提示」混排行的统一容器）：
/// 行内所有控件与文字**垂直居中于同一行高**。
///
/// egui 裸 `horizontal` 的行 y 范围随前序元素逐个扩张（16 高文字后面放
/// 26 高 DragValue，后续元素以扩张后的范围居中），「一个框比一个低」的
/// 阶梯就是这么来的（实测 10 分/30 秒/3 次中心 621→623→624）。本容器
/// 把行高一次性钉死，全部元素以同一范围居中，零漂移。
pub fn param_row(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        egui::Vec2::new(ui.available_width(), PARAM_ROW_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        add_contents,
    );
}

/// 表单**设置卡片**的帧（ZCode 设置风格的观感基准）：比窗口底亮一档的
/// 浮起底、8px 圆角、无描边、内边距 14——每个设置项一个块。
pub fn setting_card_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(Color32::from_rgb(41, 45, 56))
        .corner_radius(8.0)
        .inner_margin(Margin::same(14))
}

/// 设置卡片（**简单控件右置**形态）：标题与说明在左，控件在卡片右侧
/// 垂直居中——开关 / 下拉 / 数值框这类「标题一句话 + 控件一颗」的行。
///
/// ```text
/// ┌──────────────────────────────────────┐
/// │ 思考量                    [300 visits] │
/// │ 访问次数，越大越强越慢                    │
/// └──────────────────────────────────────┘
/// ```
pub fn setting_card(
    ui: &mut egui::Ui,
    title: &str,
    tip: Option<&str>,
    control: impl FnOnce(&mut egui::Ui),
) {
    setting_card_frame().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(title).strong().size(13.5));
                if let Some(tip) = tip {
                    ui.weak(tip);
                }
            });
            // 控件最右、垂直居中于左列（right_to_left 占据剩余宽度）。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                control(ui);
            });
        });
    });
    ui.add_space(12.0);
}

/// 设置卡片（**纵向内容**形态）：标题与说明在卡片顶部，复杂内容
/// （路径输入行 / 分段选择 / 多参数组）在下方自由排布。
pub fn setting_card_free(
    ui: &mut egui::Ui,
    title: &str,
    tip: Option<&str>,
    contents: impl FnOnce(&mut egui::Ui),
) {
    setting_card_frame().show(ui, |ui| {
        ui.label(egui::RichText::new(title).strong().size(13.5));
        if let Some(tip) = tip {
            ui.weak(tip);
        }
        ui.add_space(4.0);
        contents(ui);
    });
    ui.add_space(12.0);
}

/// 等宽铺满所在容器的按钮（侧栏与对话框统一的「主行动」按钮形态）。
/// 原为 analysis_panel / batch_scan / ref_scan 三份相同实现，收归主题层。
///
/// 文字**水平居中**：`add_sized` 内部是居中布局，egui Button 的文字对齐
/// 跟随外层容器的 cross_align（`AtomLayout` 的 align2 回退取
/// `ui.layout()` 的水平对齐），这里恰好是 Center。高度给实值
/// （[`row_height`]）避免 0 高溢出（见 [`field_group`] 文档）。
pub fn wide_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), row_height(ui)],
        egui::Button::new(text),
    )
}

/// **主按钮**的唯一样式源（对话框首要动作：开始新对局、主操作确认等）：
/// 琥珀暗底 + 琥珀描边 + 强调文字色。文字颜色用 [`colors::TEXT_ON_ACCENT`]
/// 在琥珀底上保持对比。
///
/// ⚠️ egui Button 的文字对齐**跟随外层容器**（左对齐容器里把按钮拉宽，
/// 文字就会贴左——「开始新对局」曾因此极丑）。全宽使用请走
/// [`primary_wide`]（内含居中布局），不要自行 `min_size` 拉宽。
pub fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(text)
            .strong()
            .color(colors::TEXT_ON_ACCENT),
    )
    .fill(colors::ACCENT_DIM)
    .stroke(Stroke::new(1.0, colors::ACCENT_BAR))
}

/// 全宽主按钮：铺满所在容器宽、**文字水平居中**（`top_down_justified`
/// 的 cross_align=Center 提供文字居中，cross_justify 提供拉满），禁用态
/// 由 `enabled` 控制（悬停原因由调用方挂在返回的 Response 上）。
/// 高度给实值（[`row_height`]）避免 0 高溢出。
pub fn primary_wide(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let height = row_height(ui);
    ui.with_layout(
        egui::Layout::top_down_justified(egui::Align::Center),
        |ui| {
            ui.add_enabled(
                enabled,
                primary_button(text).min_size(egui::Vec2::new(ui.available_width(), height)),
            )
        },
    )
    .inner
}

/// 对话框底部动作条的一枚按钮的描述。
pub struct Action {
    /// 按钮文字。
    pub label: &'static str,
    /// 主操作：琥珀描边强调（每条动作条至多一枚，多条时靠前的优先视觉）。
    pub primary: bool,
    /// 禁用（禁用原因由调用方在返回后按需补 `on_disabled_hover_text`）。
    pub enabled: bool,
}

/// 对话框 / 确认框底部动作条的统一实现：按钮**自然宽度**（文字多长
/// 按钮多长，不再出现「取消被撑到 80px」这类失衡），整行**右对齐**，
/// 主操作琥珀描边；返回被点击的下标（`None` = 本帧无点击）。
///
/// 此前三个对话框各写一套（快扫自然宽度、参照打分拉伸 + 固定 80px、
/// 设置对半分），观感参差；确认框与未保存条又各是一套。收拢后新加
/// 对话框直接复用，风格不再漂移。
pub fn action_row(ui: &mut egui::Ui, actions: &[Action]) -> Option<usize> {
    let mut clicked = None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        // right_to_left 布局按声明顺序从右往左落位：首个动作落在最右，
        // 即「主操作在最右」的桌面惯例（macOS 例外，但本项目界面
        // 语境按 GTK/KDE 惯例走）。
        for (index, action) in actions.iter().enumerate() {
            let mut button = egui::Button::new(action.label);
            if action.primary {
                button = button.stroke(Stroke::new(1.0, colors::ACCENT_BAR));
            }
            let response = ui.add_enabled(action.enabled, button);
            if response.clicked() {
                clicked = Some(index);
            }
        }
    });
    clicked
}

/// 构建观棋的统一视觉样式（深色 + 琥珀强调）。
pub fn visuals() -> Visuals {
    // 以 egui 内建深色主题为基线，只覆写需要统一的项。
    let mut visuals = egui::Style::default().visuals;

    // 分层背景：窗口（弹窗 / CentralPanel 兜底）< 面板 < 输入框底。
    // 窗口与面板压到同一档深底，让卡片（+15 级亮）从底上明显浮起——
    // 「文本界面的图形化」观感的最主要来源就是层次差太弱（作者
    // 2026-11-01：「生硬的把 TUI 做成了 GUI」）。
    visuals.window_fill = Color32::from_rgb(24, 27, 34);
    visuals.panel_fill = Color32::from_rgb(26, 29, 36);
    visuals.extreme_bg_color = Color32::from_rgb(18, 20, 26);

    // 窗口与菜单的圆角（弹窗 / 菜单不再直角生硬）。
    visuals.window_corner_radius = 8.0.into();
    visuals.menu_corner_radius = 6.0.into();
    visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(62, 68, 82));
    visuals.window_shadow = egui::Shadow::NONE;
    visuals.popup_shadow = egui::Shadow::NONE;

    // 文本：主文本提高亮度；弱文本（weak）不要太灰以免看不清。
    visuals.override_text_color = None;
    visuals.weak_text_alpha = 0.65;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(198, 203, 213));

    // 选中态（selectable_label / 文本选择）：琥珀薄底 + 浅琥珀文字。
    //
    // ⚠️ 底色必须用 `from_rgba_unmultiplied`：早先写成
    // `from_rgba_premultiplied(255, 170, 40, 60)` —— 该 API 要求 RGB **已按 alpha 预乘**，
    // 这里却传了未预乘的琥珀值，合成时亮度被再放大一遍 ⇒ 渲染成**高饱和亮黄底**，
    // 再叠琥珀色选中文字就是「黄底黄字」。egui 的输入框聚焦即全选内容，
    // 于是表现为「填数字时整格看不清」（作者 2026-09-23 报的 bug）。
    // 文字同时改浅琥珀，保证在琥珀薄底上依然清晰。
    visuals.selection = Selection {
        bg_fill: Color32::from_rgba_unmultiplied(255, 170, 40, 70),
        stroke: Stroke::new(1.0, Color32::from_rgb(255, 236, 200)),
    };

    // 控件三态：底色渐亮 + 琥珀描边浮现（hover 起）。按钮底色比卡片
    // (41,45,56) 亮一档，控件在卡片上可辨。
    let widgets = &mut visuals.widgets;
    widgets.inactive.bg_fill = Color32::from_rgb(52, 57, 70);
    widgets.inactive.weak_bg_fill = Color32::from_rgb(52, 57, 70);
    widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(58, 63, 76));
    widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(216, 219, 226));
    widgets.inactive.corner_radius = 5.0.into();

    widgets.hovered.bg_fill = Color32::from_rgb(62, 68, 82);
    widgets.hovered.weak_bg_fill = Color32::from_rgb(62, 68, 82);
    widgets.hovered.bg_stroke =
        Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 170, 40, 120));
    widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::from_rgb(236, 239, 246));
    widgets.hovered.corner_radius = 5.0.into();
    widgets.hovered.expansion = 0.0;

    widgets.active.bg_fill = Color32::from_rgb(72, 79, 94);
    widgets.active.weak_bg_fill = Color32::from_rgb(72, 79, 94);
    widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(255, 170, 40));
    widgets.active.fg_stroke = Stroke::new(1.0, Color32::from_rgb(255, 244, 224));
    widgets.active.corner_radius = 5.0.into();
    widgets.active.expansion = 0.0;

    // 非交互控件（文本框底、分隔线所在的底）同样统一圆角。
    widgets.noninteractive.bg_fill = Color32::from_rgb(38, 41, 50);
    widgets.noninteractive.weak_bg_fill = Color32::from_rgb(38, 41, 50);
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(52, 57, 70));
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(198, 203, 213));
    widgets.noninteractive.corner_radius = 4.0.into();

    // 悬停可点控件显示手型光标（浏览器式反馈）。
    visuals.interact_cursor = Some(CursorIcon::PointingHand);
    // 禁用态统一变淡。
    visuals.disabled_alpha = 0.45;

    visuals
}

/// 统一字号与间距（布局参数）：按钮更大更好点、分区之间留白。
///
/// 依赖启动时先 [`crate::ui::install_cjk_fonts`]（字号表在字体之后设置）。
pub fn apply_spacing(ctx: &egui::Context) {
    ctx.global_style_mut(|style| {
        // 字号层次：标题 17 / 正文 14 / 等宽 13 / 小字 11.5。
        style.text_styles = [
            (
                TextStyle::Heading,
                egui::FontId::new(17.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Body,
                egui::FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Button,
                egui::FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                egui::FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                egui::FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into_iter()
        .collect();

        let spacing = &mut style.spacing;
        spacing.item_spacing = Vec2::new(10.0, 8.0);
        spacing.button_padding = Vec2::new(10.0, 5.0);
        spacing.icon_width_inner = 12.0;
        spacing.icon_width = 16.0;
        // 控件行高（DragValue / 滑杆 / 按钮最小高度）：默认 18px 在 14px
        // 正文字号下显得局促，抬到 22 与按钮自然高度一致。
        spacing.interact_size = Vec2::new(spacing.interact_size.x, 22.0);
        // 弹窗与菜单的内边距：默认 8px 让内容贴边，统一放大后对话框
        // 内容有呼吸感（内边距只影响窗框内留白，不撑大窗口）。
        spacing.window_margin = Margin::same(14);
        spacing.menu_margin = Margin::same(8);
        // 滚动条更细、贴边，滚动手感一致。
        spacing.scroll.bar_width = 8.0;
        spacing.scroll.bar_inner_margin = 2.0;
        spacing.scroll.bar_outer_margin = 2.0;
    });
}
