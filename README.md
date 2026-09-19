# 观棋 (Guanqi)

**围棋 AI 引擎图形前端** · Go AI Engine Frontend

---

## 项目简介

观棋是一个为围棋 AI 引擎（KataGo）提供图形界面的 Linux 桌面程序，
核心职能是**实时呈现引擎的分析结果**：胜率、目差、候选点、局面热度。

棋由引擎计算，人通过界面**观看**引擎的思考 —— 此即「观棋」之意；
取名自中国围棋典故「观棋烂柯」。

## 技术栈

| 组件 | 选型 |
|---|---|
| 语言 | Rust |
| GUI | egui + eframe（纯 Rust 自绘） |
| 引擎 | KataGo（analysis JSON 行协议） |
| 平台 | Linux |

## 开发

```bash
cargo run               # 运行
cargo build --release   # 发布构建
```

当前进度：M2 已完成（棋盘可交互），引擎桥接层开发中。
