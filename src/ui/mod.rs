//! 界面组件：CJK 字体加载、棋盘视图、分析侧栏、胜率曲线与设置面板。

pub mod analysis;
pub mod analysis_panel;
pub mod board_view;
pub mod curve;
pub mod overlay;
pub mod settings;

pub use board_view::show;

use std::path::Path;
use std::sync::Arc;

use egui::{Context, FontData, FontDefinitions, FontFamily};

/// CJK 字体候选路径，按优先级排列。
///
/// 项目不内嵌字体文件（避免仓库膨胀与字体授权问题），
/// 运行时按序探测第一个存在的字体并注册；
/// 发行版打包时可将对应字体包声明为可选运行时依赖。
const CJK_FONT_CANDIDATES: &[&str] = &[
    // 思源黑体（单字体 OTF，首选）。
    "/usr/share/fonts/adobe-source-han-sans/SourceHanSansCN-Regular.otf",
    // Noto CJK（TrueType 字体集合，需指定子字体索引）。
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    // Droid Sans Fallback（兜底）。
    "/usr/share/fonts/droid/DroidSansFallback.ttf",
];

/// Noto CJK TTC 集合中「Noto Sans CJK SC（简体中文）」的子字体索引。
///
/// 实测 Arch `noto-fonts-cjk` 包内顺序：
/// 0 JP / 1 KR / 2 SC / 3 TC / 4 HK / 5-9 对应 Mono 变体。
const NOTO_CJK_SC_INDEX: u32 = 2;

/// 注册的字体在 `FontDefinitions::font_data` 表中的键名。
const CJK_FONT_KEY: &str = "guanqi-cjk";

/// 将第一个存在的 CJK 候选字体追加到 egui 的 Proportional 与
/// Monospace 字族末尾（作为缺字回退，不覆盖拉丁字形）。
///
/// 返回 `true` 表示字体加载成功；全部候选都不可用时返回 `false`
/// 并通过 `eprintln!` 输出日志——绝不 panic，界面中文会退化为方框。
pub fn install_cjk_fonts(ctx: &Context) -> bool {
    for path in CJK_FONT_CANDIDATES {
        if !Path::new(path).is_file() {
            continue;
        }

        match std::fs::read(path) {
            Ok(data) => {
                let mut font_data = FontData::from_owned(data);
                if path.ends_with(".ttc") {
                    // TTC 为多字体集合，必须显式选择子字体。
                    font_data.index = NOTO_CJK_SC_INDEX;
                }

                // egui 0.36 起 font_data 表的值类型为 Arc<FontData>。
                let mut fonts = FontDefinitions::default();
                fonts
                    .font_data
                    .insert(CJK_FONT_KEY.to_owned(), Arc::new(font_data));
                for family in [FontFamily::Proportional, FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .push(CJK_FONT_KEY.to_owned());
                }
                ctx.set_fonts(fonts);
                return true;
            }
            Err(err) => {
                eprintln!("观棋：读取字体 {path} 失败（{err}），尝试下一个候选。");
            }
        }
    }

    eprintln!(
        "观棋：未找到任何中文字体，界面中文将显示为方框。\n\
         候选路径：{}\n\
         请安装 adobe-source-han-sans-cn 或 noto-fonts-cjk。",
        CJK_FONT_CANDIDATES.join("、")
    );
    false
}
