//! 观棋 (Guanqi) —— KataGo 围棋 AI 引擎图形前端（库目标）。
//!
//! 全部功能模块都在这里注册，二进制入口（`main.rs`）保持薄壳，只负责
//! 配置原生窗口选项并启动 eframe 运行时。模块一览：
//!
//! - [`app`]：[`eframe::App`] 实现与整体接线；
//! - [`board`]：坐标、规则与带历史的棋盘状态；
//! - [`sgf`]：SGF 解析 / 序列化与棋谱载入；
//! - [`engine`]：KataGo `analysis` 子进程桥接；
//! - [`portal`]：零依赖调用系统原生文件对话框（XDG Desktop Portal）；
//! - [`ui`]：棋盘视图、分析侧栏、曲线与设置。
//!
//! `examples/portal_probe.rs` 等 example / 集成代码经 `guanqi::` 直接
//! 复用这里的模块。

pub mod app;
pub mod board;
pub mod engine;
pub mod portal;
pub mod sgf;
pub mod ui;
