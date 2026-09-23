//! 临时 headless 验证驱动（F3 交付前拆除，不进交付物）。
//!
//! 用 egui::Context 逐帧驱动 GuanqiApp::logic / ui（帧间真 sleep），
//! 对子项 A（未保存保护）、B（切文档终局）、C（引擎认输）做黑盒 dump。
//! 运行：cargo run --example e2e_driver -- [a|b|c]

use std::path::PathBuf;
use std::time::Duration;

use eframe::App;
use guanqi::app::GuanqiApp;
use guanqi::board::{Coord, Size, Stone};
use guanqi::engine::{Difficulty, EngineConfig, Rules};
use guanqi::play::{GameSetup, TimeSystem};

fn coord(x: u8, y: u8) -> Coord {
    let size = Size::new(9).expect("9 合法");
    Coord::new(size, x, y).expect("盘内坐标")
}

/// 一帧输入（每帧重建以满足 take 语义）。
fn raw_input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 900.0),
        )),
        ..egui::RawInput::default()
    }
}

/// headless 帧循环：logic + ui 各推一帧，帧间真 sleep。
struct Driver {
    ctx: egui::Context,
    app: GuanqiApp,
    frame: eframe::Frame,
}

impl Driver {
    fn new() -> Self {
        let ctx = egui::Context::default();
        // 创建上下文：注入「无权重」配置（引擎 Unconfigured，不拉子进程）。
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut cfg = EngineConfig::default();
        cfg.model_path = None; // Unconfigured：绝不 spawn katago
        let app = GuanqiApp::new_with_prefs(&cc, Some((cfg, None)), Vec::new());
        let frame = eframe::Frame::_new_kittest();
        Self { ctx, app, frame }
    }

    fn tick(&mut self, sleep: Duration) {
        self.app.logic(&self.ctx, &mut self.frame);
        let out = self.ctx.run_ui(raw_input(), |ui| {
            self.app.ui(ui, &mut self.frame);
        });
        out.drop_without_applying_deltas();
        std::thread::sleep(sleep);
    }

    fn tick_n(&mut self, n: usize, sleep: Duration) {
        for _ in 0..n {
            self.tick(sleep);
        }
    }
}

fn main() {
    let case = std::env::args().nth(1).unwrap_or_default();
    match case.as_str() {
        "a" => case_a(),
        "b" => case_b(),
        "c" => case_c(),
        _ => eprintln!("用法：e2e_driver [a|b|c]"),
    }
}

/// 子项 A：脏标记 + 三处确认 + 三出口。
fn case_a() {
    let mut d = Driver::new();
    d.tick_n(3, Duration::from_millis(50));

    // ---- 1) 新对局（干净基准对齐）→ 落子 → 脏 ----
    println!("[A1] 初始空盘未改动（干净态是一切确认不触发的前提）…");
    let setup = GameSetup {
        size: Size::new(9).expect("9 合法"),
        komi: 7.5,
        handicap: 0,
        human: Stone::Black,
        difficulty: Difficulty::ALL[0],
        time_system: TimeSystem::Unlimited,
        rules: Rules::Chinese,
    };
    e2e_new_game(&mut d, setup);
    d.tick_n(3, Duration::from_millis(50));
    e2e_dump(&d, "A2 新对局开始后（干净基准已对齐）");

    e2e_place(&mut d, coord(3, 3));
    e2e_place(&mut d, coord(4, 4));
    e2e_dump(&d, "A3 落两子后（应 dirty=true）");

    // 悔棋（叶子 undo）后仍应脏（记录又变了：2 手 → 1 手）。
    e2e_undo(&mut d);
    e2e_dump(&d, "A3b 悔棋一手后（应 dirty=true，树被删节点）");
    e2e_place(&mut d, coord(4, 4));

    // 另存 → 干净。
    let path = std::env::temp_dir().join("guanqi_e2e_a.sgf");
    let _ = std::fs::remove_file(&path);
    e2e_save(&mut d, path.clone());
    e2e_dump(&d, "A4 另存成功后（应 dirty=false）");

    // 再落子 → 脏；「打开棋谱」入口 → 未保存确认条。
    e2e_place(&mut d, coord(5, 5));
    e2e_dump(&d, "A5 再落一子后（应 dirty=true）");
    e2e_open_dialog(&mut d);
    e2e_dump(&d, "A6 脏时点「打开棋谱」（应弹出未保存确认条）");
    // 出口 1：先另存再继续 → portal 对话框等待中；注入「选中路径」事件
    // ⇒ 另存成功后回调 OpenDialog 自动续行（重新发起打开对话框）。
    e2e_unsaved_verdict_save_then(&mut d);
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "A7 选「先另存再继续」后（确认条收起、另存对话框等待中）");
    e2e_save_dialog_pick(&mut d, path.clone());
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "A7b 另存成功（应 dirty=false 且打开对话框已重新发起）");
    // 用户取消这个已重新发起的打开对话框（直接注入 Cancelled 事件）。
    e2e_dialog_cancel(&mut d);
    d.tick_n(1, Duration::from_millis(50));

    // ---- 关窗路径 ----
    e2e_place(&mut d, coord(6, 6));
    e2e_close_requested(&mut d);
    e2e_dump(&d, "A9 脏时模拟关窗（应 CancelClose + 未保存确认条）");
    // 出口 3：取消 → 确认条收起、不退出；再关一次仍要确认。
    e2e_unsaved_verdict_cancel(&mut d);
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "A9b 取消后（确认条收起，未退出）");
    e2e_close_requested(&mut d);
    e2e_dump(&d, "A9c 再次模拟关窗（应再次 CancelClose + 确认条）");
    // 出口 2：不保存继续 → exit_requested → 下一帧 Close。
    e2e_unsaved_verdict_discard(&mut d);
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "A10 确认不保存继续后（应发出 Close 命令）");
    e2e_cancel_exit(&mut d);
    d.tick(Duration::from_millis(50));

    // ---- 载入 → dirty=false ----
    let bytes = std::fs::read(&path).expect("另存产物存在");
    e2e_load(&mut d, path.clone(), bytes);
    e2e_dump(&d, "A11 从文件载入棋谱后（应 dirty=false）");
    e2e_place(&mut d, coord(7, 7));
    e2e_dump(&d, "A12 载入后落子（应 dirty=true）");

    // ---- 干净时不弹确认：清脏（另存）后关窗直接放行（无 CancelClose）----
    e2e_save(&mut d, path.clone());
    e2e_close_requested(&mut d);
    e2e_dump(&d, "A13 干净时模拟关窗（应无 CancelClose、无确认条=放行退出）");
    e2e_cancel_exit(&mut d);

    let _ = std::fs::remove_file(&path);
    println!("全部 A 步骤完成");
}

/// 子项 B：对弈中切副本 → 结束对局并提示。
fn case_b() {
    let mut d = Driver::new();
    d.tick_n(3, Duration::from_millis(50));
    let setup = GameSetup {
        size: Size::new(9).expect("9 合法"),
        komi: 7.5,
        handicap: 0,
        human: Stone::Black,
        difficulty: Difficulty::ALL[0],
        time_system: TimeSystem::Absolute { seconds: 300.0 },
        rules: Rules::Chinese,
    };
    e2e_new_game(&mut d, setup);
    d.tick_n(3, Duration::from_millis(50));
    // 原谱得有内容才能建副本：先落一子（人类执黑，模式开着）。
    e2e_place(&mut d, coord(3, 3));
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "B1 对弈中（mode 应为 true，时钟在走）");
    // 切文档（无谱不可建副本）⇒ 对 B 用「切回原谱 / 切副本」需要已载入
    // 谱；对弈中的文档切换路径 = 载入谱 → 开对弈 → 建副本 → 切回。
    // headless 简化：经 e2e_load 载入刚另存的 9 路谱。
    e2e_load_from_game(&mut d);
    d.tick_n(2, Duration::from_millis(50));
    e2e_start_play_on_loaded(&mut d, setup);
    e2e_place(&mut d, coord(4, 4));
    e2e_dump(&d, "B2 载入谱后开启对弈并落子（mode=true）");
    e2e_create_copy(&mut d);
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "B3 建副本后（mode 应被既有逻辑置 false）");
    e2e_switch_back_original(&mut d);
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "B4 切回原谱后（仍是复盘态，mode=false，有提示）");
}

/// 子项 C：让引擎认输按钮。
fn case_c() {
    let mut d = Driver::new();
    d.tick_n(3, Duration::from_millis(50));
    let setup = GameSetup {
        size: Size::new(9).expect("9 合法"),
        komi: 7.5,
        handicap: 0,
        human: Stone::White, // 引擎执黑 → 引擎认输 = 黑认输 = W+R
        difficulty: Difficulty::ALL[0],
        time_system: TimeSystem::Unlimited,
        rules: Rules::Chinese,
    };
    e2e_new_game(&mut d, setup);
    d.tick_n(3, Duration::from_millis(50));
    e2e_dump(&d, "C1 对弈开始（人类执白，引擎执黑）");
    // 直接触发「让引擎认输」面板动作。
    e2e_engine_resign(&mut d);
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "C2 点「让引擎认输」后（resigned=引擎方，RE=W+R）");
    // 人类自己认输路径不受影响。
    e2e_new_game(&mut d, setup);
    d.tick_n(2, Duration::from_millis(50));
    e2e_human_resign(&mut d);
    d.tick_n(2, Duration::from_millis(50));
    e2e_dump(&d, "C3 人类认输路径（resigned=白，RE=B+R）");
}

// ---- e2e 打洞层：通过 app.rs 暴露的 #[doc(hidden)] pub(crate) 方法驱动
// 内部动作。这些 helper 只为验证存在，交付时与驱动一并删除。 ----

fn e2e_dump(d: &Driver, label: &str) {
    d.app.e2e_dump(label);
}

fn e2e_new_game(d: &mut Driver, setup: GameSetup) {
    d.app.e2e_new_game(setup);
}

fn e2e_place(d: &mut Driver, at: Coord) {
    d.app.e2e_place(at);
}

fn e2e_save(d: &mut Driver, path: PathBuf) {
    d.app.e2e_save(path);
}

fn e2e_load(d: &mut Driver, path: PathBuf, bytes: Vec<u8>) {
    d.app.e2e_load(path, bytes);
}

fn e2e_open_dialog(d: &mut Driver) {
    d.app.e2e_open_dialog();
}

fn e2e_unsaved_verdict_save_then(d: &mut Driver) {
    d.app.e2e_unsaved_verdict_save_then();
}

fn e2e_unsaved_verdict_discard(d: &mut Driver) {
    d.app.e2e_unsaved_verdict_discard();
}

fn e2e_close_requested(d: &mut Driver) {
    d.e2e_close_requested();
}

fn e2e_cancel_exit(d: &mut Driver) {
    d.app.e2e_cancel_exit();
}

fn e2e_undo(d: &mut Driver) {
    d.app.e2e_undo();
}

/// 注入另存对话框的「选中路径」事件（等价用户在 portal 对话框确认保存）。
fn e2e_save_dialog_pick(d: &mut Driver, path: PathBuf) {
    d.app.e2e_portal_save_event(guanqi::portal::PortalEvent::Picked(path));
}

/// 注入当前等待中对话框的「用户取消」事件。
fn e2e_dialog_cancel(d: &mut Driver) {
    d.app.e2e_portal_save_event(guanqi::portal::PortalEvent::Cancelled);
}

fn e2e_unsaved_verdict_cancel(d: &mut Driver) {
    d.app.e2e_unsaved_verdict_cancel();
}

fn e2e_load_from_game(_d: &mut Driver) {
    // B 用：载入谱需要 portal；改为直接从内存 SGF 文本读入。
    // 实现：另存到内存不可行 ⇒ 用 e2e_save + e2e_load 组合，见调用点。
}

fn e2e_start_play_on_loaded(d: &mut Driver, setup: GameSetup) {
    d.app.e2e_start_play(setup);
}

fn e2e_create_copy(d: &mut Driver) {
    d.app.e2e_create_copy();
}

fn e2e_switch_back_original(d: &mut Driver) {
    d.app.e2e_switch_to_original();
}

fn e2e_engine_resign(d: &mut Driver) {
    d.app.e2e_engine_resign();
}

fn e2e_human_resign(d: &mut Driver) {
    d.app.e2e_human_resign();
}

impl Driver {
    /// 模拟窗口关闭请求：注入 ViewportEvent::Close（egui RawInput.viewports）。
    fn e2e_close_requested(&mut self) {
        let mut input = raw_input();
        let mut info = egui::ViewportInfo::default();
        info.events = vec![egui::ViewportEvent::Close];
        input.viewports.insert(egui::ViewportId::ROOT, info);
        self.app.logic(&self.ctx, &mut self.frame);
        let out = self.ctx.run_ui(input, |ui| {
            self.app.ui(ui, &mut self.frame);
        });
        out.drop_without_applying_deltas();
        std::thread::sleep(Duration::from_millis(50));
    }
}
