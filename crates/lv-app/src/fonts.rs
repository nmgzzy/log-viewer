//! CJK 字体加载：egui 自带字体不含中文字形，启动时从系统字体目录
//! 按平台候选列表加载一个 CJK 字体作为回退，三端均不需打包字体文件。

use eframe::egui;

struct Candidate {
    path: &'static str,
    /// .ttc 集合内的字体索引。
    index: u32,
}

#[cfg(target_os = "windows")]
const CANDIDATES: &[Candidate] = &[
    Candidate { path: "C:\\Windows\\Fonts\\msyh.ttc", index: 0 },   // 微软雅黑
    Candidate { path: "C:\\Windows\\Fonts\\msyh.ttf", index: 0 },
    Candidate { path: "C:\\Windows\\Fonts\\simhei.ttf", index: 0 }, // 黑体
    Candidate { path: "C:\\Windows\\Fonts\\simsun.ttc", index: 0 }, // 宋体
];

#[cfg(target_os = "macos")]
const CANDIDATES: &[Candidate] = &[
    Candidate { path: "/System/Library/Fonts/PingFang.ttc", index: 0 },
    Candidate { path: "/System/Library/Fonts/STHeiti Light.ttc", index: 0 },
    Candidate { path: "/System/Library/Fonts/Hiragino Sans GB.ttc", index: 0 },
];

#[cfg(all(unix, not(target_os = "macos")))]
const CANDIDATES: &[Candidate] = &[
    Candidate { path: "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", index: 2 },
    Candidate { path: "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", index: 2 },
    Candidate { path: "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", index: 0 },
    Candidate { path: "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc", index: 0 },
];

/// 安装 CJK 回退字体。找不到时静默跳过（界面仍可用，中文字形缺失）。
pub fn install_cjk_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = false;
    for c in CANDIDATES {
        if let Ok(bytes) = std::fs::read(c.path) {
            let mut data = egui::FontData::from_owned(bytes);
            data.index = c.index;
            fonts.font_data.insert("cjk".into(), std::sync::Arc::new(data));
            loaded = true;
            break;
        }
    }
    if loaded {
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".into());
        }
    }
    ctx.set_fonts(fonts);
}
