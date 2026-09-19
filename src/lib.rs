//! 观棋的库目标：注册 portal 等「主二进制之外、待 UI 接线」的模块。
//!
//! `main.rs` 保持纯二进制入口不动；接线时在 UI 侧 `use guanqi::portal::...` 即可。

pub mod portal;
