//! 导出渲染结果：HTML 与 PDF。

use std::collections::BTreeMap;
use std::path::Path;

use pulldown_cmark::{Parser, html};

use crate::markdown::parse_options;

pub fn render_html(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, parse_options());
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

pub fn export_html(path: &Path, markdown: &str) -> Result<(), String> {
    let body = render_html(markdown);
    let doc = format!(
        "<!DOCTYPE html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n<title>导出文档</title>\n\
<style>body{{font-family:system-ui,'Microsoft YaHei',sans-serif;max-width:800px;margin:40px auto;padding:0 20px;line-height:1.7}}\
pre{{background:#f5f5f5;padding:12px;overflow-x:auto}}code{{background:#f5f5f5;padding:2px 4px}}\
table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:6px 10px}}\
blockquote{{border-left:3px solid #ccc;margin-left:0;padding-left:12px;color:#555}}</style>\n</head>\n<body>\n{body}\n</body>\n</html>"
    );
    std::fs::write(path, doc).map_err(|e| e.to_string())
}

pub fn export_pdf(path: &Path, markdown: &str) -> Result<(), String> {
    let font_bytes =
        cjk_font_bytes().ok_or_else(|| "未找到可用的中文字体（如 simhei.ttf）".to_string())?;
    let body = render_html(markdown);
    let html_doc = format!(
        "<html><head><style>\
body{{font-family:'SimHei',sans-serif;font-size:12px;line-height:1.6;margin:0}}\
pre{{background:#f4f4f4;padding:8px;font-family:'SimHei',monospace;white-space:pre-wrap}}\
code{{font-family:'SimHei',monospace}}\
table{{border-collapse:collapse}}td,th{{border:1px solid #999;padding:4px 8px}}\
blockquote{{border-left:3px solid #999;margin-left:0;padding-left:10px;color:#444}}\
</style></head><body>{body}</body></html>"
    );

    let mut fonts = BTreeMap::new();
    fonts.insert("SimHei".to_string(), printpdf::Base64OrRaw::Raw(font_bytes));
    let options = printpdf::GeneratePdfOptions {
        page_width: Some(210.0),
        page_height: Some(297.0),
        margin_top: Some(20.0),
        margin_right: Some(20.0),
        margin_bottom: Some(20.0),
        margin_left: Some(20.0),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    let doc = printpdf::PdfDocument::from_html(
        &html_doc,
        &BTreeMap::new(),
        &fonts,
        &options,
        &mut warnings,
    )
    .map_err(|e| e)?;
    let bytes = doc.save(&printpdf::PdfSaveOptions::default(), &mut warnings);
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

pub fn cjk_font_bytes() -> Option<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/Deng.ttf",
        "C:/Windows/Fonts/simsun.ttc",
        "C:/Windows/Fonts/msyh.ttc",
    ];
    for candidate in CANDIDATES {
        if let Ok(bytes) = std::fs::read(candidate) {
            return Some(bytes);
        }
    }
    None
}

pub fn bold_latin_font_bytes() -> Option<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/segoeuib.ttf",
        "C:/Windows/Fonts/arialbd.ttf",
        "C:/Windows/Fonts/calibrib.ttf",
        "C:/Windows/Fonts/georgiab.ttf",
        "C:/Windows/Fonts/timesbd.ttf",
    ];
    for candidate in CANDIDATES {
        if let Ok(bytes) = std::fs::read(candidate) {
            return Some(bytes);
        }
    }
    None
}

pub fn bold_cjk_font_bytes() -> Option<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/Dengb.ttf",
        "C:/Windows/Fonts/msyhbd.ttc",
        "C:/Windows/Fonts/simsunb.ttf",
        "C:/Windows/Fonts/simhei.ttf",
    ];
    for candidate in CANDIDATES {
        if let Ok(bytes) = std::fs::read(candidate) {
            return Some(bytes);
        }
    }
    None
}

/// JetBrains Mono 常规字体内置字节（随应用分发，不依赖系统安装）。
const JB_MONO_REGULAR: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");
/// JetBrains Mono 粗体字体内置字节。
const JB_MONO_BOLD: &[u8] = include_bytes!("../fonts/JetBrainsMono-Bold.ttf");
/// 霞鹜文楷轻便版常规字体，仅作为 JetBrains Mono 缺失中文字符的回退。
const LXGW_WENKAI_REGULAR: &[u8] = include_bytes!("../fonts/LXGWWenKaiLite-Regular.ttf");
/// 霞鹜文楷轻便版 Medium 字重，用于中文标题和粗体。
const LXGW_WENKAI_MEDIUM: &[u8] = include_bytes!("../fonts/LXGWWenKaiLite-Medium.ttf");

pub fn jetbrains_mono_regular_bytes() -> &'static [u8] {
    JB_MONO_REGULAR
}

pub fn jetbrains_mono_bold_bytes() -> &'static [u8] {
    JB_MONO_BOLD
}

pub fn lxgw_wenkai_regular_bytes() -> &'static [u8] {
    LXGW_WENKAI_REGULAR
}

pub fn lxgw_wenkai_medium_bytes() -> &'static [u8] {
    LXGW_WENKAI_MEDIUM
}

/// 构造应用字体：英文优先 JetBrains Mono，中文回退到霞鹜文楷。
fn app_font_definitions() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "jb_mono".to_string(),
        egui::FontData::from_owned(JB_MONO_REGULAR.to_vec()).into(),
    );
    fonts.font_data.insert(
        "jb_mono_bold".to_string(),
        egui::FontData::from_owned(JB_MONO_BOLD.to_vec()).into(),
    );
    fonts.font_data.insert(
        "lxgw_wenkai".to_string(),
        egui::FontData::from_owned(LXGW_WENKAI_REGULAR.to_vec()).into(),
    );
    fonts.font_data.insert(
        "lxgw_wenkai_medium".to_string(),
        egui::FontData::from_owned(LXGW_WENKAI_MEDIUM.to_vec()).into(),
    );

    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let family_fonts = fonts.families.entry(family).or_default();
        family_fonts.insert(0, "lxgw_wenkai".to_string());
        family_fonts.insert(0, "jb_mono".to_string());
    }

    let mut bold_family = vec!["jb_mono_bold".to_string(), "lxgw_wenkai_medium".to_string()];

    if let Some(bytes) = bold_latin_font_bytes() {
        fonts.font_data.insert(
            "bold_latin".to_string(),
            egui::FontData::from_owned(bytes).into(),
        );
        bold_family.push("bold_latin".to_string());
    }
    if let Some(bytes) = bold_cjk_font_bytes() {
        fonts.font_data.insert(
            "cjk_bold".to_string(),
            egui::FontData::from_owned(bytes).into(),
        );
        bold_family.push("cjk_bold".to_string());
    }
    if let Some(bytes) = cjk_font_bytes() {
        fonts
            .font_data
            .insert("cjk".to_string(), egui::FontData::from_owned(bytes).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_string());
        }
        bold_family.push("cjk".to_string());
    }
    fonts
        .families
        .insert(egui::FontFamily::Name("bold".into()), bold_family);
    fonts
}

/// 安装应用字体：英文保持 JetBrains Mono，中文使用霞鹜文楷轻便版。
pub fn install_app_fonts(ctx: &egui::Context) {
    ctx.set_fonts(app_font_definitions());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 英文优先jetbrains中文回退霞鹜文楷() {
        let fonts = app_font_definitions();
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let names = &fonts.families[&family];
            assert_eq!(names[0], "jb_mono");
            assert_eq!(names[1], "lxgw_wenkai");
        }
        let bold = &fonts.families[&egui::FontFamily::Name("bold".into())];
        assert_eq!(bold[0], "jb_mono_bold");
        assert_eq!(bold[1], "lxgw_wenkai_medium");
    }

    #[test]
    fn 正常笔记渲染出结构和链接() {
        let md = "# 会议记录\n\n- 本周发布 v1.2\n\n详见[接口文档](https://example.com)\n";
        let html_doc = render_html(md);
        assert!(html_doc.contains("<h1>会议记录</h1>"));
        assert!(html_doc.contains("<ul>"));
        assert!(html_doc.contains("<li>本周发布 v1.2</li>"));
        assert!(html_doc.contains("<a href=\"https://example.com\">接口文档</a>"));
    }

    #[test]
    fn 代码块内特殊字符不转成标题() {
        let md = "```\n# 这不是标题\n**这不是粗体**\n```\n";
        let html_doc = render_html(md);
        assert!(html_doc.contains("<pre><code>"));
        assert!(!html_doc.contains("<h1>这不是标题</h1>"));
    }

    #[test]
    fn 导出pdf生成非空文件() {
        let dir = std::env::temp_dir().join(format!("md_editor_pdf_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("out.pdf");
        let md = "# 标题\n\n中文段落内容。\n\n- 条目一\n- 条目二\n";
        match export_pdf(&p, md) {
            Ok(()) => {
                let bytes = std::fs::read(&p).unwrap();
                assert!(bytes.starts_with(b"%PDF"), "文件应以 %PDF 开头");
                assert!(bytes.len() > 1000, "PDF 不应为空");
            }
            Err(e) => panic!("PDF 导出失败：{}", e),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
