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
}

/// 侧栏分区卡片的 Frame：卡片底色 + 细边框 + 圆角 + 内边距。
///
/// `Frame::default()` 起底（全零）再逐项设置，不跟随全局视觉配置漂移；
/// 各调用点内容不同，间距由调用方用 `ui.add_space` 控制。
pub fn card_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(Color32::from_rgb(33, 36, 45))
        .stroke(Stroke::new(1.0, Color32::from_rgb(52, 57, 70)))
        .corner_radius(8.0)
        .inner_margin(Margin::same(10))
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

/// 构建观棋的统一视觉样式（深色 + 琥珀强调）。
pub fn visuals() -> Visuals {
    // 以 egui 内建深色主题为基线，只覆写需要统一的项。
    let mut visuals = egui::Style::default().visuals;

    // 分层背景：窗口（弹窗 / CentralPanel 兜底）< 面板 < 输入框底。
    visuals.window_fill = Color32::from_rgb(26, 29, 36);
    visuals.panel_fill = Color32::from_rgb(30, 33, 41);
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

    // 选中态（selectable_label / 文本选择）：琥珀半透明底 + 琥珀文字。
    visuals.selection = Selection {
        bg_fill: Color32::from_rgba_premultiplied(255, 170, 40, 60),
        stroke: Stroke::new(1.0, Color32::from_rgb(255, 196, 96)),
    };

    // 控件三态：底色渐亮 + 琥珀描边浮现（hover 起）。
    let widgets = &mut visuals.widgets;
    widgets.inactive.bg_fill = Color32::from_rgb(44, 48, 58);
    widgets.inactive.weak_bg_fill = Color32::from_rgb(44, 48, 58);
    widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(58, 63, 76));
    widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(216, 219, 226));
    widgets.inactive.corner_radius = 5.0.into();

    widgets.hovered.bg_fill = Color32::from_rgb(55, 60, 72);
    widgets.hovered.weak_bg_fill = Color32::from_rgb(55, 60, 72);
    widgets.hovered.bg_stroke =
        Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 170, 40, 120));
    widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::from_rgb(236, 239, 246));
    widgets.hovered.corner_radius = 5.0.into();
    widgets.hovered.expansion = 0.0;

    widgets.active.bg_fill = Color32::from_rgb(66, 72, 86);
    widgets.active.weak_bg_fill = Color32::from_rgb(66, 72, 86);
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
        // 滚动条更细、贴边，滚动手感一致。
        spacing.scroll.bar_width = 8.0;
        spacing.scroll.bar_inner_margin = 2.0;
        spacing.scroll.bar_outer_margin = 2.0;
    });
}
