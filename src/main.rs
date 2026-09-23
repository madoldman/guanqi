//! 观棋 (Guanqi) —— KataGo 围棋 AI 引擎图形前端。
//!
//! 程序入口（薄壳）：配置原生窗口选项并启动 eframe 运行时。
//! 全部功能在库目标中（见 `lib.rs`）。

use guanqi::engine::load_settings;

fn main() -> eframe::Result {
    // 窗口几何：用上次的值（子项 2）。尺寸取上次内容区大小；位置
    // Wayland 下通常可取到（winit 估算），取不到时 `None` 交 WM 摆放。
    // 最小尺寸约束恒 960×640（既有行为）。防抖与退出落盘在 `App` 侧。
    let (cfg, _) = load_settings();
    let (width, height) = cfg
        .ui_prefs
        .window_geometry
        .map(|g| (g.width, g.height))
        .unwrap_or((1200.0, 800.0));
    let (width, height) = (
        width.clamp(960.0, 8192.0),
        height.clamp(640.0, 8192.0),
    );
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("观棋")
        .with_inner_size([width, height])
        .with_min_inner_size([960.0, 640.0]);
    if let Some([x, y]) = cfg.ui_prefs.window_geometry.and_then(|g| g.position) {
        viewport = viewport.with_position(egui::Pos2::new(x, y));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    // app_id 用作 Wayland 窗口标识；可见标题由 ViewportBuilder 提供。
    eframe::run_native(
        "guanqi",
        options,
        Box::new(|cc| Ok(Box::new(guanqi::app::GuanqiApp::new(cc)))),
    )
}
