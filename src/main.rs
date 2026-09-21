//! 观棋 (Guanqi) —— KataGo 围棋 AI 引擎图形前端。
//!
//! 程序入口（薄壳）：配置原生窗口选项并启动 eframe 运行时。
//! 全部功能在库目标中（见 `lib.rs`）。

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("观棋")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };

    // app_id 用作 Wayland 窗口标识；可见标题由 ViewportBuilder 提供。
    eframe::run_native(
        "guanqi",
        options,
        Box::new(|cc| Ok(Box::new(guanqi::app::GuanqiApp::new(cc)))),
    )
}
