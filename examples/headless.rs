//! 观棋 headless 验证工具（常驻）。
//!
//! # 用途
//!
//! 用 [`egui::Context`] 逐帧驱动 `GuanqiApp::logic()` 与 `ui()`，不进
//! eframe / winit，不弹真实窗口。引擎走**真实子进程**（按现有
//! [`guanqi::engine::EngineConfig`] 载入路径查找 katago 与权重），因此
//! 引擎状态机、流式分析、整谱快扫等链路与真实运行完全一致；帧间**真
//! sleep**（默认 30ms），批量推进 / 心跳 / 时限都按真实墙钟计算——紧密
//! 循环会伪造时序，这里刻意避免。
//!
//! # 怎么跑
//!
//! ```text
//! cargo run --release --example headless -- [脚本文件|-]
//! ```
//!
//! - 脚本文件为 `-` 时从 stdin 读；缺省跑内置「自证」脚本
//!   （覆盖：载入谱 → 复选框翻转 → F 门控 → 快扫进度，全部 dump 数据证据）；
//! - `--frame-ms N`：帧间隔毫秒（默认 30）；
//! - `--no-engine`：强制权重未配置（不拉 katago 子进程，纯 UI 验证）。
//!
//! # 脚本语法（逐行，`#` 起注释，空行忽略）
//!
//! ```text
//! wait <ms>                 墙钟等待（期间继续逐帧驱动）
//! click <x> <y>             左键单击（先移动后按下抬起，egui points 坐标）
//! click_label <文本>        按控件文本定位后左键单击（见下方「坐标漂移」）
//! rclick <x> <y>            右键单击
//! drag <x1> <y1> <x2> <y2>  按下拖拽再抬起（限定区域框选用）
//! wheel <x> <y> <dy>        滚轮滚动（侧栏 ScrollArea 翻滚用；dy>0 向下）
//! key <名>                  单键（F / ArrowLeft / ArrowRight / …）
//! keys <组合>               组合键，`+` 连接（Ctrl+O / Ctrl+Shift+S / Ctrl+Z）
//! load <sgf>                从文件载入棋谱（等价 portal 对话框选中该文件）
//! save <sgf>                另存到路径（同上）
//! dump [标签]               打印结构化状态快照（单行 JSON）
//! probe [文本过滤]          列出匹配文本的控件矩形与命中点（坐标标定用）
//! viewport <宽> <高>        重设视口尺寸（默认 1180×860，points）
//! quit                      请求退出（走未保存守卫，等价关窗）
//! ```
//!
//! # 视口尺寸与坐标含义
//!
//! 视口尺寸是 egui points（逻辑像素），`RawInput.screen_rect` 直接采用；
//! 点击坐标是**同一点数系**的窗口内坐标（左上原点）。控件真实位置用
//! `probe` 动作查（控件内省），不要靠猜——主题 / 字体变化会让硬编码
//! 坐标漂移。
//!
//! # 坐标漂移与 click_label（重要）
//!
//! 侧栏是滚动容器，且内容高度**随异步引擎结果浮动**（候选点列表、
//! 快扫预估条都会撑高），同一个控件的 y 坐标在不同运行间能差几百
//! points。因此 `click <x> <y>` 的硬编码坐标**只在一次运行内有效**，
//! 换谱 / 换环境就会打空。日常请用 `click_label <文本>`：它取**本帧**
//! 的控件矩形直接点击，不依赖上一次 probe 的滞后值。
//!
//! 即便用 `click_label`，也要先让滚动**停稳**：egui 的滚轮是平滑衰减
//! 的，`wheel` 之后滚动仍会继续若干帧，而控件矩形反映的是上一帧。
//! 判据：连续两次 `probe 目标` 给出同一个 rect 即已停稳。
//!
//! `probe` 输出里的 `可点 / 视口外` 标记就是这个用途：只有矩形**完整**
//! 落在视口内的控件才可能点中，被 ScrollArea 裁掉一半的坐标点了也白点。
//!
//! 同理，`wheel` 的指针坐标必须落在目标滚动区内——egui 按 hover 分派
//! 滚轮事件，在棋盘中间滚是滚不动侧栏的。
//!
//! # 帧节奏要求
//!
//! 每帧 = 一次 `logic()` + 一次 `ui()`，帧间 sleep `--frame-ms`。`wait`
//! 按真实墙钟推进；对弈时钟、引擎查询超时、防抖落盘的判定全部基于
//! 真实时间，因此**不要**把帧间隔设成 0（等于伪造时间流逝）。
//!
//! # 探针实现（probe 动作）
//!
//! 坐标来自 egui 的控件矩形（`Context::viewport` 读取上一帧
//! `prev_pass.widgets` 的 [`egui::WidgetRect`]），文本来自 accesskit
//! 节点树：工具在启动时 `ctx.enable_accesskit()`，egui 每帧在
//! `PlatformOutput.accesskit_update` 里给出全部控件的 label；二者按
//! 控件 `Id` 关联（egui `Id::accesskit_id` 即 accesskit `NodeId`）。
//! 全程公开 API，无反射、无补丁。
//!
//! # 边界说明
//!
//! 本工具不进合成器：不覆盖真实窗口的渲染差异（后端差异、字体光栅化、
//! HiDPI 缩放、Wayland/KWin 合成行为）。**视觉观感类问题（布局裁切、
//! 颜色、刻度重叠等）超出本工具的验证范围**，需真实窗口或截图人工检查。
//!
//! # 配置隔离（铁律）
//!
//! 工具在 main 早期把 `XDG_CONFIG_HOME` 指向 `/tmp` 下**按 pid 独占**的
//! 目录（`config_dir()` 尊重该变量），跑完即删，**绝不**写真实
//! `~/.config/guanqi/`。
//!
//! 之所以要「每次独占」而不是共用一个 `/tmp/guanqi-headless-config`：
//! 退出时的 `on_exit_shim()` 会把偏好落盘，共用目录时上一次改过的开关
//! （如候选门控模式）会变成下一次运行的起点，脚本结果不可复现。每次
//! 全新目录 ⇒ 每次都从默认偏好出发。

use std::collections::HashMap;
use std::io::Read as _;
use std::path::PathBuf;

use eframe::App;
use guanqi::app::GuanqiApp;

/// 帧间真 sleep 的默认值（毫秒）。
const DEFAULT_FRAME_MS: u64 = 30;
/// 视口默认尺寸（points）。
const DEFAULT_VIEWPORT: (f32, f32) = (1180.0, 860.0);

fn main() {
    let mut args = std::env::args().skip(1);
    let mut script: Option<String> = None;
    let mut frame_ms = DEFAULT_FRAME_MS;
    let mut no_engine = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frame-ms" => {
                frame_ms = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_FRAME_MS);
            }
            "--no-engine" => no_engine = true,
            _ if script.is_none() => script = Some(arg),
            _ => eprintln!("忽略多余参数：{arg}"),
        }
    }
    let text = match script.as_deref() {
        Some("-") => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).expect("读 stdin");
            buf
        }
        Some(path) => {
            std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("读脚本 {path} 失败：{e}"))
        }
        None => self_test_script().to_owned(),
    };

    // 配置隔离：headless 一律不碰真实 ~/.config/guanqi。config_dir() 在
    // 构造 app 之前重定向（进程内一次性设定，驱动所有读写路径——含
    // settings.json 读写与 analysis.cfg 生成）。
    //
    // 每次运行用**独占的全新子目录**（进程号后缀）：退出时 settings.json
    // 会落盘偏好，若复用同一目录，上一次运行改过的开关（如门控模式）
    // 会变成下一次的起点，脚本结果不再可复现。全新目录 ⇒ 每次都从
    // 「默认偏好」出发，而默认值本身与实际安装一致（不写死偏好）。
    let sandbox = std::env::temp_dir().join(format!(
        "guanqi-headless-config-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&sandbox).expect("创建隔离配置目录");
    // main 早期、引擎线程尚未拉起，环境变量无并发读者。
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &sandbox) };

    let mut driver = Driver::new(frame_ms, no_engine);
    if let Err(err) = driver.run_script(&text) {
        eprintln!("脚本执行失败：{err}");
        std::process::exit(1);
    }
    driver.app.on_exit_shim();
    let _ = std::fs::remove_dir_all(&sandbox);
}

/// 内置自证脚本：菜单路径下的五项数据证据（无需截图与像素）。
///
/// ## 为什么用 `click_label` 而不是硬编码坐标
///
/// 侧栏内容高度随异步引擎结果浮动，控件 y 坐标在不同运行间能差几百
/// points，硬编码坐标只在单次运行内有效。这里一律用 `click_label`
/// 按文本取**本帧**矩形点击；它会在日志里回显实际取到的点，便于事后
/// 核对。唯一前提是先让滚动**停稳**（`wait` 足够久），详见「坐标漂移」。
/// 菜单条目同理：先点顶层菜单（如「分析」）展开，条目出现在弹层里后
/// 再按文本点击。
///
/// ## 侧栏已瘦身为纯展示
///
/// 叠加层开关 / 门控 / 快扫发起等设置全部走菜单（65941bc 起的自证 2/3/4
/// 原先点侧栏卡片控件，卡片摘除后改走菜单路径，验证目标不变）。
///
/// ## 为什么每项自证都带「对照」
///
/// 单点 dump 只能说明「状态恰好是这个值」，无法排除 dump 本身失效
/// （字段恒为默认值、快照没接上）。每项都配一次反向/相邻操作，用
/// **状态的差分**证明通路真的活着。
fn self_test_script() -> &'static str {
    "\
    # ── 自证 1：载入 /tmp 自造测试谱 → dump「已载入、手数 N」──
    wait 400
    dump 载入前-基线
    load /tmp/guanqi_headless_test.sgf
    wait 1500
    dump 载入后

    # ── 自证 2：经菜单「分析 → 叠加层 ⏵ → 候选点圆圈」翻转开关 ──
    # 注意「叠加层 ⏵」带子菜单后缀，避免误点到棋盘状态栏「限定选点中」等
    # 含相同字样的文本。每次重开菜单前先 ESC 清层：弹层若还开着，点顶层
    # 菜单是「收起」而不是「展开」。
    click_label 分析
    wait 400
    probe 叠加层 ⏵
    click_label 叠加层 ⏵
    wait 500
    click_label 候选点圆圈
    wait 500
    dump 菜单翻转候选圆圈-应false
    # 对照：再翻回 true（先 ESC 收起残余弹层再重开菜单）
    key ESC
    wait 400
    click_label 分析
    wait 400
    click_label 叠加层 ⏵
    wait 500
    click_label 候选点圆圈
    wait 500
    dump 菜单再翻回-应true

    # ── 自证 3：门控切「手动」（分析 → 候选显示 ⏵）+ 按 F ──
    key ESC
    wait 400
    click_label 分析
    wait 400
    click_label 候选显示 ⏵
    wait 500
    click_label 手动
    wait 500
    dump 切手动后-应不可见
    key F
    wait 500
    dump 按F后-应已揭示
    # 对照：切回「立即」应自动清除手动揭示标记
    key ESC
    wait 400
    click_label 分析
    wait 400
    click_label 候选显示 ⏵
    wait 500
    click_label 立即
    wait 500
    dump 切回立即后-应清除手动标记

    # ── 自证 4：非对弈态「对局 → 认输」置灰（点击无效果）──
    click_label 对局
    wait 400
    probe 认输
    click_label 认输
    wait 500
    dump 非对弈态点认输-应无变化

    # ── 自证 5：经「分析 → 整谱快扫…」对话框发起快扫（真实引擎）──
    # 快扫需要引擎已就绪，前面几步的等待已足够；若仍是 starting 则报错回府。
    click_label 分析
    wait 400
    click_label 整谱快扫
    wait 600
    dump 对话框已打开
    click_label 开始快扫
    wait 1500
    dump 快扫-刚发起
    wait 4000
    dump 快扫-5秒后
    # 取消（对话框内「取消快扫」仍在，进度同屏）→ dump 进度应消失
    click_label 取消快扫
    wait 700
    dump 快扫-取消后-进度应消失

    # ── 自证 6：退出守卫（走真实关窗语义）──
    quit
    wait 600
    dump 请求退出后
    "
}

/// 控件探针的一行结果：文本 + 命中矩形。
///
/// 给的是**整矩形**而非中心点：带 label 的部分控件（checkbox / radio /
/// Button）的可点击范围比文字宽得多，只给中心点会让使用者以为「必须
/// 点正中心」，而实际偏离中心照样命中；矩形上界等于控件顶 `(left, top)`，
/// 下界 `(right, bottom)` 是最稳的取点参考。
struct ProbeHit {
    text: String,
    rect: egui::Rect,
}

/// headless 帧驱动：logic + ui 各推一帧，帧间真 sleep。
struct Driver {
    ctx: egui::Context,
    app: GuanqiApp,
    frame: eframe::Frame,
    viewport: (f32, f32),
    frame_ms: u64,
    /// 最近一帧的 accesskit 节点（NodeId → label）。
    /// egui `Id::accesskit_id()` 把控件 Id 换算成 NodeId。
    labels: HashMap<egui::accesskit::NodeId, String>,
}

impl Driver {
    fn new(frame_ms: u64, no_engine: bool) -> Self {
        let ctx = egui::Context::default();
        // 探针文本通路：开启 accesskit 后，每帧 PlatformOutput 带
        // accesskit_update（控件 label），与 widget rects 按 Id 关联。
        ctx.enable_accesskit();
        // 与 eframe 同款创建上下文（kittest 构造器是 eframe 提供的测试入口）。
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut cfg = guanqi::engine::EngineConfig::default();
        if no_engine {
            cfg.model_path = None;
        }
        let app = GuanqiApp::new_with_prefs(&cc, Some((cfg, None)), Vec::new());
        let frame = eframe::Frame::_new_kittest();
        Self {
            ctx,
            app,
            frame,
            viewport: DEFAULT_VIEWPORT,
            frame_ms,
            labels: HashMap::new(),
        }
    }

    /// 推一帧（可选注入额外输入事件），帧间真 sleep。
    fn tick(&mut self, input: egui::RawInput) {
        self.app.logic(&self.ctx, &mut self.frame);
        let mut out = self
            .ctx
            .run_ui(input, |ui| self.app.ui(ui, &mut self.frame));
        // 收集本帧 accesskit 节点文本（下一帧的探针读取源）。
        // 注意：`ui.label` 生成的 Role::Label 节点把文本放在 **value**
        // 属性（egui response.rs 只在非 Label 角色用 label 属性），
        // 因此 label() 为空时回退读 value()——否则侧栏纯文本行全部
        // 探不到（65941bc 后侧栏正是纯文本为主）。
        self.labels.clear();
        if let Some(update) = out.platform_output.accesskit_update.take() {
            for (node_id, node) in update.nodes {
                let text = node
                    .label()
                    .map_or_else(|| node.value().map(|v| v.to_owned()), |l| Some(l.to_owned()));
                if let Some(text) = text {
                    let text = text.trim();
                    if !text.is_empty() {
                        self.labels.insert(node_id, text.to_owned());
                    }
                }
            }
        }
        out.drop_without_applying_deltas();
        std::thread::sleep(std::time::Duration::from_millis(self.frame_ms));
    }

    fn tick_default(&mut self) {
        self.tick(raw_input(self.viewport));
    }

    /// 带事件钩子的一帧。
    fn tick_with(&mut self, push: impl FnOnce(&mut egui::RawInput)) {
        let mut input = raw_input(self.viewport);
        push(&mut input);
        self.tick(input);
    }

    /// 墙钟等待（期间继续逐帧驱动，保持 logic / 时钟 / 引擎轮询推进）。
    fn wait(&mut self, ms: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < deadline {
            self.tick_default();
        }
    }

    /// 枚举上一帧控件：有文本 label 的控件 → 中心坐标。
    ///
    /// `prev_pass.widgets` 是 egui 公开读取的控件矩形表
    /// （`Context::viewport`）；label 来自 accesskit 节点树，按
    /// [`egui::Id::accesskit_id`] 关联。
    fn widget_labels(&self) -> Vec<ProbeHit> {
        self.ctx.viewport(|v| {
            let mut found = Vec::new();
            for (_layer, rects) in v.prev_pass.widgets.layers() {
                for w in rects {
                    let Some(label) = self.labels.get(&w.id.accesskit_id()) else {
                        continue;
                    };
                    let text = label.trim();
                    if text.is_empty() {
                        continue;
                    }
                    found.push(ProbeHit {
                        text: text.to_owned(),
                        rect: w.interact_rect,
                    });
                }
            }
            found
        })
    }

    /// 按文本定位控件，返回其命中点（本帧矩形中心）。
    ///
    /// 要求矩形完整落在视口内：被 ScrollArea 裁掉一部分的控件点了不中，
    /// 宁可报「未找到」也不要拿一个注定打空的坐标去点。
    fn find_widget(&self, needle: &str) -> Option<(String, egui::Pos2)> {
        let screen = raw_input(self.viewport).screen_rect?;
        self.widget_labels()
            .into_iter()
            .find(|h| h.text.contains(needle) && screen.contains_rect(h.rect))
            .map(|h| (h.text.clone(), h.rect.center()))
    }

    /// 定位控件，必要时**自动滚动寻找**。
    ///
    /// 侧栏内容高度随异步引擎结果浮动，脚本没法预知该滚多少；与其让
    /// 使用者硬编码一个必然过期的滚轮量，不如让工具自己翻：每轮向下滚
    /// 一屏，滚到底仍未找到则从顶部再扫一轮（两轮覆盖「初始位置在目标
    /// 之后」的情况，例如刚载入长谱把侧栏撑高）。
    ///
    /// 每轮之间 `wait` 到滚动停稳——egui 滚轮带平滑衰减，滚动未停时
    /// 读到的控件矩形是上一帧的旧值。
    fn find_widget_scrolling(&mut self, needle: &str) -> Option<(String, egui::Pos2)> {
        const STEP: f32 = -700.0;
        const ROUNDS: usize = 12;
        // 指针必须悬停在目标 ScrollArea 内滚动才生效——egui 按 hover
        // 分派滚轮事件。侧栏在窗口右侧，取「右侧 1/6 宽、垂直居中」
        // 的位置；这个点在侧栏宽度范围内，且避开了可能被面板遮挡的
        // 上下边缘。（直接取元组而非闭包：viewport 是 Copy，避免与
        // 后面的 &mut self 借用冲突。）
        let wheel_at = (self.viewport.0 * 0.86, self.viewport.1 * 0.5);
        if let Some(hit) = self.find_widget(needle) {
            return Some(hit);
        }
        for _ in 0..2 {
            for _ in 0..ROUNDS {
                self.wheel(wheel_at.0, wheel_at.1, STEP);
                self.wait(400);
                if let Some(hit) = self.find_widget(needle) {
                    return Some(hit);
                }
            }
            // 回顶：连续反向滚，量取足够大以覆盖任意内容高度。
            for _ in 0..ROUNDS {
                self.wheel(wheel_at.0, wheel_at.1, -STEP);
                self.wait(200);
            }
            self.wait(600);
        }
        None
    }

    /// 执行脚本（逐行）。返回首个动作级错误。
    fn run_script(&mut self, text: &str) -> Result<(), String> {
        for (no, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
            let rest = rest.trim();
            let label = || format!("（第 {} 行：{line}）", no + 1);
            match verb {
                "wait" => {
                    let ms: u64 =
                        rest.parse().map_err(|_| format!("wait 参数非法{}", label()))?;
                    self.wait(ms);
                }
                "click" => {
                    let (x, y) =
                        parse_xy(rest).ok_or_else(|| format!("click 参数非法{}", label()))?;
                    self.click(x, y, egui::PointerButton::Primary);
                }
                "click_label" => {
                    // 自动滚动寻找：目标被 ScrollArea 裁在视口外时先翻进去。
                    let Some((text, p)) = self.find_widget_scrolling(rest) else {
                        return Err(format!("未找到可见控件「{rest}」{}", label()));
                    };
                    println!("[click_label] 「{text}」→ ({:.0},{:.0})", p.x, p.y);
                    self.click(p.x, p.y, egui::PointerButton::Primary);
                }
                "rclick" => {
                    let (x, y) =
                        parse_xy(rest).ok_or_else(|| format!("rclick 参数非法{}", label()))?;
                    self.click(x, y, egui::PointerButton::Secondary);
                }
                "drag" => {
                    let (x1, y1, x2, y2) = parse_xy4(rest)
                        .ok_or_else(|| format!("drag 参数非法{}", label()))?;
                    self.drag(x1, y1, x2, y2);
                }
                "wheel" => {
                    let (x, y, dy) = parse_xy3(rest)
                        .ok_or_else(|| format!("wheel 参数非法{}", label()))?;
                    self.wheel(x, y, dy);
                }
                "key" => {
                    let key = parse_key(rest)
                        .ok_or_else(|| format!("key 名称无法识别：{rest}{}", label()))?;
                    self.press_keys(&[], key);
                }
                "keys" => {
                    let combo = parse_combo(rest)
                        .ok_or_else(|| format!("keys 组合无法识别：{rest}{}", label()))?;
                    self.press_keys(&combo.0, combo.1);
                }
                "load" => self.inject_file_pick(guanqi::app::PendingDialog::Open, rest)
                    .map_err(|e| format!("load 失败：{e}{}", label()))?,
                "save" => self.inject_file_pick(guanqi::app::PendingDialog::Save, rest)
                    .map_err(|e| format!("save 失败：{e}{}", label()))?,
                "dump" => {
                    let snap = self.app.state_snapshot();
                    println!("[dump {rest}] {}", snap.to_json());
                }
                "probe" => {
                    let hits: Vec<ProbeHit> = self
                        .widget_labels()
                        .into_iter()
                        .filter(|h| rest.is_empty() || h.text.contains(&rest.to_owned()))
                        .collect();
                    if hits.is_empty() {
                        println!("[probe] 未命中：{rest}");
                    }
                    for hit in &hits {
                        // 可见性：控件须完整落在当前视口矩形内才可点击
                        // （被 ScrollArea 裁掉一半的坐标点了也没用——egui
                        // 的命中测试过不了裁剪）。探针显式标注，避免使用者
                        // 拿到「看着像坐标、实际打不中」的数字。
                        // 用驱动自己持有的视口矩形判定（与每帧喂进去的
                        // screen_rect 同源，无需回头查 input）。
                        let visible = raw_input(self.viewport)
                            .screen_rect
                            .is_some_and(|r| r.contains_rect(hit.rect));
                        let mark = if visible { "可点" } else { "视口外" };
                        println!(
                            "[probe] 「{}」 rect=({:.0},{:.0})-({:.0},{:.0}) \
                             点=({:.0},{:.0}) {}",
                            hit.text,
                            hit.rect.left(),
                            hit.rect.top(),
                            hit.rect.right(),
                            hit.rect.bottom(),
                            hit.rect.center().x,
                            hit.rect.bottom() - hit.rect.height() / 2.0,
                            mark,
                        );
                    }
                }
                "viewport" => {
                    let (w, h) =
                        parse_xy(rest).ok_or_else(|| format!("viewport 参数非法{}", label()))?;
                    self.viewport = (w, h);
                    println!("[viewport] {w}×{h}");
                    self.tick_default();
                }
                "quit" => {
                    self.app.request_exit();
                    self.tick_default();
                }
                other => return Err(format!("未知动作「{other}」{}", label())),
            }
        }
        Ok(())
    }

    /// 在指定坐标做一次点击（移动 → 按下 → 抬起，中间各推一帧，
    /// 让 hover / press 状态按真实事件序推进）。
    fn click(&mut self, x: f32, y: f32, button: egui::PointerButton) {
        let pos = egui::Pos2::new(x, y);
        self.tick_with(|input| {
            input.events.push(egui::Event::PointerMoved(pos));
        });
        // 多等一帧再按下：菜单弹层（Popup）在展开的下一帧才接受指针事件，
        // 「移动 → 立即按下」时 press 落在弹层尚未注册的帧上，点击会
        // 穿透到下层（表现为「点了菜单项但没反应、菜单反而关闭」）。
        self.tick_default();
        self.tick_with(|input| {
            input.events.push(egui::Event::PointerButton {
                pos,
                button,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            });
        });
        self.tick_with(|input| {
            input.events.push(egui::Event::PointerButton {
                pos,
                button,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
            input.events.push(egui::Event::PointerGone);
        });
    }

    /// 拖拽（限定区域框选）：按下 → 逐段移动 → 抬起。
    /// 与 [`Self::click`] 同理：先 hover 建立并等一帧让目标层稳定，
    /// 再按下（否则 press 帧落在弹层/交互态切换的同一帧上会被忽略）。
    fn drag(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        let (a, b) = (egui::Pos2::new(x1, y1), egui::Pos2::new(x2, y2));
        self.tick_with(|input| {
            input.events.push(egui::Event::PointerMoved(a));
        });
        self.tick_default();
        self.tick_with(|input| {
            input.events.push(egui::Event::PointerButton {
                pos: a,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            });
        });
        // 按住原地两帧：egui 的「明显在拖」判定要求 press 之后的帧才成立
        // （is_decidedly_dragging 在 press 帧恒 false），立刻移动会让
        // potential_drag 的二义决策窗口被跳过，拖拽永远建立不起来。
        self.tick_default();
        self.tick_default();
        let steps = 6;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let pos = a + (b - a) * t;
            self.tick_with(|input| {
                input.events.push(egui::Event::PointerMoved(pos));
            });
        }
        // 释放：先发抬起（此时拖拽终点仍在指针位置上，drag_stopped 在
        // 本帧成立），**下一帧**再让指针离开——同一帧内 release+Gone 会
        // 让 egui 的拖拽判定把「停止」与「消失」合并，drag_stopped 丢失，
        // 棋盘框选就成不了框。
        self.tick_with(|input| {
            input.events.push(egui::Event::PointerButton {
                pos: b,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
        });
        self.tick_with(|input| {
            input.events.push(egui::Event::PointerGone);
        });
    }

    /// 滚轮滚动：先移动指针到目标点，再发若干次 `MouseWheel`（每次分
    /// 量不超过 egui 的「单次滚动」识别阈值，分多次推帧更接近真实滚轮）。
    fn wheel(&mut self, x: f32, y: f32, dy: f32) {
        let pos = egui::Pos2::new(x, y);
        self.tick_with(|input| {
            input.events.push(egui::Event::PointerMoved(pos));
        });
        // 每次 60 points，避免一次给太大被当成「惯性/平滑滚动」丢弃。
        let mut left = dy;
        while left.abs() > 1.0 {
            let step = left.clamp(-60.0, 60.0);
            left -= step;
            self.tick_with(|input| {
                input.events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, step),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::default(),
                });
            });
        }
    }

    /// 组合键（按住修饰键 → 点按主键 → 释放修饰键）。
    fn press_keys(&mut self, mods: &[egui::Modifiers], key: egui::Key) {
        let modifiers = mods
            .iter()
            .copied()
            .fold(egui::Modifiers::default(), |m, f| m | f);
        self.tick_with(|input| {
            for m in mods {
                input.events.push(egui::Event::Key {
                    physical_key: None,
                    key: egui::Key::A,
                    pressed: true,
                    repeat: false,
                    modifiers: *m,
                });
            }
            input.events.push(egui::Event::Key {
                physical_key: None,
                key,
                pressed: true,
                repeat: false,
                modifiers,
            });
            input.events.push(egui::Event::Key {
                physical_key: None,
                key,
                pressed: false,
                repeat: false,
                modifiers,
            });
            for _ in mods {
                input.events.push(egui::Event::Key {
                    physical_key: None,
                    key: egui::Key::A,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::default(),
                });
            }
        });
    }

    /// 注入 portal「用户选中文件」事件（load / save 动作的落点）。
    /// 等价用户在原生对话框确认：先清等待槽再分发（与 logic 取事件
    /// 顺序一致；等待槽为空等价「用户取消后再选」，守卫语义不变）。
    fn inject_file_pick(
        &mut self,
        kind: guanqi::app::PendingDialog,
        path: &str,
    ) -> Result<(), String> {
        let path = unquote(path);
        if path.is_empty() {
            return Err("路径为空".to_owned());
        }
        let path = PathBuf::from(path);
        // 载入要求源文件存在；另存则要求**父目录**存在即可——目标文件
        // 本来就该由应用新建，若照 load 的口径要求它已存在，save 就永远
        // 只能覆盖旧文件，测不到「另存到新路径」。
        match kind {
            guanqi::app::PendingDialog::Open if !path.is_file() => {
                return Err(format!("文件不存在：{}", path.display()));
            }
            guanqi::app::PendingDialog::Save => {
                let Some(parent) = path.parent() else {
                    return Err(format!("路径无父目录：{}", path.display()));
                };
                if !parent.is_dir() {
                    return Err(format!("父目录不存在：{}", parent.display()));
                }
            }
            _ => {}
        }
        self.app
            .inject_portal_event(kind, guanqi::portal::PortalEvent::Picked(path));
        self.tick_default();
        self.tick_default();
        Ok(())
    }
}

/// 一帧输入（每帧重建以满足 take 语义）。
fn raw_input(viewport: (f32, f32)) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(viewport.0, viewport.1),
        )),
        ..egui::RawInput::default()
    }
}

/// 解析 `x y`。
fn parse_xy(s: &str) -> Option<(f32, f32)> {
    let mut it = s.split_whitespace();
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    Some((x, y))
}

/// 解析 `x y z`。
fn parse_xy3(s: &str) -> Option<(f32, f32, f32)> {
    let mut it = s.split_whitespace();
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let z = it.next()?.parse().ok()?;
    Some((x, y, z))
}

/// 解析 `x1 y1 x2 y2`。
fn parse_xy4(s: &str) -> Option<(f32, f32, f32, f32)> {
    let mut it = s.split_whitespace();
    let x1 = it.next()?.parse().ok()?;
    let y1 = it.next()?.parse().ok()?;
    let x2 = it.next()?.parse().ok()?;
    let y2 = it.next()?.parse().ok()?;
    Some((x1, y1, x2, y2))
}

/// 键名 → egui::Key（覆盖导航 / 常用编辑键）。
fn parse_key(name: &str) -> Option<egui::Key> {
    let key = match name.to_ascii_uppercase().as_str() {
        "F" => egui::Key::F,
        "A" => egui::Key::A,
        "Z" => egui::Key::Z,
        "O" => egui::Key::O,
        "S" => egui::Key::S,
        "ARROWLEFT" | "LEFT" => egui::Key::ArrowLeft,
        "ARROWRIGHT" | "RIGHT" => egui::Key::ArrowRight,
        "ARROWUP" | "UP" => egui::Key::ArrowUp,
        "ARROWDOWN" | "DOWN" => egui::Key::ArrowDown,
        "HOME" => egui::Key::Home,
        "END" => egui::Key::End,
        "ENTER" => egui::Key::Enter,
        "ESCAPE" | "ESC" => egui::Key::Escape,
        _ => return None,
    };
    Some(key)
}

/// 组合键 `Ctrl+Shift+S` → (修饰集, 主键)。
fn parse_combo(s: &str) -> Option<(Vec<egui::Modifiers>, egui::Key)> {
    let mut mods = Vec::new();
    let mut key = None;
    for part in s.split('+') {
        let upper = part.to_ascii_uppercase();
        match upper.as_str() {
            "CTRL" | "CONTROL" => mods.push(egui::Modifiers::CTRL),
            "SHIFT" => mods.push(egui::Modifiers::SHIFT),
            "ALT" => mods.push(egui::Modifiers::ALT),
            other => key = parse_key(other),
        }
    }
    let key = key?;
    Some((mods, key))
}

/// 去掉路径参数两侧的引号（脚本里允许 `load "/tmp/a b.sgf"`）。
fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_owned()
}
