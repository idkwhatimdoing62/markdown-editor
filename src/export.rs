//! 导出渲染结果：HTML 与 PDF。
//!
//! 导出与浏览器预览共享主题 CSS、Markdown 解析规则、字号覆盖和应用字体。
//! HTML 会内嵌本地图片与字体；PDF 从同一份主题化 DOM 生成。

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use pulldown_cmark::{CowStr, Event, Tag, html};

use crate::markdown::ParsedDocument;

/// 一次导出所需的预览状态。调用方必须传入当前主题，而非使用导出模块默认样式。
#[derive(Clone, Copy)]
pub struct ExportOptions<'a> {
    pub title: &'a str,
    pub theme_css: &'a str,
    pub base_directory: Option<&'a Path>,
    pub body_font_size: Option<f32>,
}

#[derive(Clone, Copy)]
enum ImageMode {
    StandaloneHtml,
    Pdf,
}

pub fn render_html(document: &ParsedDocument) -> String {
    let mut out = String::new();
    html::push_html(
        &mut out,
        document.events().iter().map(|item| item.event.clone()),
    );
    out
}

pub fn export_html(
    path: &Path,
    document: &ParsedDocument,
    options: ExportOptions<'_>,
) -> Result<(), String> {
    let doc = render_styled_html(document, options);
    std::fs::write(path, doc).map_err(|e| e.to_string())
}

pub fn render_styled_html(document: &ParsedDocument, options: ExportOptions<'_>) -> String {
    let (body, _) = render_export_body(document, options.base_directory, ImageMode::StandaloneHtml);
    styled_document(&body, options, true)
}

pub fn export_pdf(
    path: &Path,
    document: &ParsedDocument,
    options: ExportOptions<'_>,
) -> Result<(), String> {
    let (body, images) = render_export_body(document, options.base_directory, ImageMode::Pdf);
    let html_doc = styled_document(&body, options, false);

    let mut fonts = BTreeMap::new();
    fonts.insert(
        "Markdown Editor Mono".to_string(),
        printpdf::Base64OrRaw::Raw(jetbrains_mono_regular_bytes().to_vec()),
    );
    fonts.insert(
        "Markdown Editor Mono Bold".to_string(),
        printpdf::Base64OrRaw::Raw(jetbrains_mono_bold_bytes().to_vec()),
    );
    fonts.insert(
        "LXGW WenKai Lite".to_string(),
        printpdf::Base64OrRaw::Raw(lxgw_wenkai_regular_bytes().to_vec()),
    );
    fonts.insert(
        "LXGW WenKai Lite Medium".to_string(),
        printpdf::Base64OrRaw::Raw(lxgw_wenkai_medium_bytes().to_vec()),
    );
    let options = printpdf::GeneratePdfOptions {
        page_width: Some(210.0),
        page_height: Some(297.0),
        // 主题本身控制 body 的留白。这里只保留防止内容贴边的安全边距。
        margin_top: Some(8.0),
        margin_right: Some(8.0),
        margin_bottom: Some(8.0),
        margin_left: Some(8.0),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    let doc = printpdf::PdfDocument::from_html(&html_doc, &images, &fonts, &options, &mut warnings)
        .map_err(|e| e)?;
    let bytes = doc.save(&printpdf::PdfSaveOptions::default(), &mut warnings);
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

fn styled_document(body: &str, options: ExportOptions<'_>, include_mermaid: bool) -> String {
    let font_size = options
        .body_font_size
        .map(|size| crate::theme::font_size_override_css(options.theme_css, size))
        .unwrap_or_default();
    let mermaid = if include_mermaid && body.contains("language-mermaid") {
        format!(
            "<script>{}</script><script>{}</script>",
            include_str!("../assets/mermaid-11.16.0.min.js"),
            MERMAID_BOOTSTRAP
        )
    } else {
        String::new()
    };
    let font_css = if include_mermaid {
        embedded_font_css()
    } else {
        PDF_FONT_CSS.to_string()
    };
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{STRUCTURAL_FALLBACK}</style><style>{}</style><style>{}{}{font_size}</style>{mermaid}</head><body>{body}</body></html>",
        escape_html(options.title),
        options.theme_css,
        font_css,
        MARKDOWN_DOM_COMPATIBILITY,
    )
}

fn render_export_body(
    document: &ParsedDocument,
    base_directory: Option<&Path>,
    image_mode: ImageMode,
) -> (String, BTreeMap<String, printpdf::Base64OrRaw>) {
    let mut images = BTreeMap::new();
    let mut image_index = 0usize;
    let events = document.events().iter().map(|item| {
        rewrite_export_image_event(
            item.event.clone(),
            base_directory,
            image_mode,
            &mut images,
            &mut image_index,
        )
    });
    let mut body = String::new();
    html::push_html(&mut body, events);
    annotate_code_languages(&mut body);
    normalize_footnote_dom(&mut body);
    (body, images)
}

fn rewrite_export_image_event<'a>(
    event: Event<'a>,
    base_directory: Option<&Path>,
    mode: ImageMode,
    images: &mut BTreeMap<String, printpdf::Base64OrRaw>,
    image_index: &mut usize,
) -> Event<'a> {
    match event {
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let destination = export_image_destination(
                dest_url.as_ref(),
                base_directory,
                mode,
                images,
                image_index,
            )
            .map(CowStr::from)
            .unwrap_or(dest_url);
            Event::Start(Tag::Image {
                link_type,
                dest_url: destination,
                title,
                id,
            })
        }
        Event::Html(fragment) => Event::Html(
            crate::html_image::rewrite_sources(fragment.as_ref(), |destination| {
                export_image_destination(destination, base_directory, mode, images, image_index)
            })
            .into(),
        ),
        Event::InlineHtml(fragment) => Event::InlineHtml(
            crate::html_image::rewrite_sources(fragment.as_ref(), |destination| {
                export_image_destination(destination, base_directory, mode, images, image_index)
            })
            .into(),
        ),
        event => event,
    }
}

fn export_image_destination(
    destination: &str,
    base_directory: Option<&Path>,
    mode: ImageMode,
    images: &mut BTreeMap<String, printpdf::Base64OrRaw>,
    image_index: &mut usize,
) -> Option<String> {
    let path = local_image_path(destination, base_directory)?;
    let content_type = image_content_type(&path)?;
    let bytes = std::fs::read(path).ok()?;
    Some(match mode {
        ImageMode::StandaloneHtml => {
            format!("data:{content_type};base64,{}", BASE64.encode(bytes))
        }
        ImageMode::Pdf => {
            let key = format!("md-export-image-{image_index}");
            *image_index += 1;
            images.insert(key.clone(), printpdf::Base64OrRaw::Raw(bytes));
            key
        }
    })
}

fn local_image_path(
    destination: &str,
    base_directory: Option<&Path>,
) -> Option<std::path::PathBuf> {
    if destination.is_empty() || destination.starts_with('#') {
        return None;
    }
    let path = Path::new(destination);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    if let Ok(url) = url::Url::parse(destination) {
        return (url.scheme() == "file")
            .then(|| url.to_file_path().ok())
            .flatten();
    }
    Some(base_directory?.join(path))
}

fn image_content_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

fn embedded_font_css() -> String {
    let face = |family: &str, weight: u16, bytes: &[u8]| {
        format!(
            "@font-face{{font-family:'{family}';src:url('data:font/ttf;base64,{}') format('truetype');font-style:normal;font-weight:{weight};font-display:block;}}",
            BASE64.encode(bytes)
        )
    };
    format!(
        "{}{}{}{}body,pre,code,blockquote::before,blockquote::after{{font-family:'Markdown Editor Mono','LXGW WenKai Lite',monospace!important;font-synthesis:weight;}}strong,b{{font-family:'Markdown Editor Mono Bold','LXGW WenKai Lite Medium','Markdown Editor Mono','LXGW WenKai Lite',monospace!important;font-weight:700!important;}}",
        face("Markdown Editor Mono", 400, jetbrains_mono_regular_bytes()),
        face(
            "Markdown Editor Mono Bold",
            700,
            jetbrains_mono_bold_bytes()
        ),
        face("LXGW WenKai Lite", 400, lxgw_wenkai_regular_bytes()),
        face("LXGW WenKai Lite Medium", 700, lxgw_wenkai_medium_bytes()),
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn annotate_code_languages(body: &mut String) {
    const OPEN: &str = "<pre><code class=\"language-";
    let mut search_from = 0;
    while let Some(relative_start) = body[search_from..].find(OPEN) {
        let pre_start = search_from + relative_start;
        let language_start = pre_start + OPEN.len();
        let Some(language_end_relative) = body[language_start..].find('"') else {
            break;
        };
        let language_end = language_start + language_end_relative;
        let language = body[language_start..language_end].trim().to_string();
        if !language.is_empty() {
            let attribute = format!(" data-language=\"{language}\"");
            body.insert_str(pre_start + "<pre".len(), &attribute);
            search_from = language_end + attribute.len() + 1;
        } else {
            search_from = language_end + 1;
        }
    }
}

fn normalize_footnote_dom(body: &mut String) {
    const OPEN: &str = "<div class=\"footnote-definition\" id=\"";
    const LABEL: &str = "<sup class=\"footnote-definition-label\">";
    let mut items = Vec::new();
    while let Some(start) = body.find(OPEN) {
        let id_start = start + OPEN.len();
        let Some(id_end_rel) = body[id_start..].find("\">") else {
            break;
        };
        let id_end = id_start + id_end_rel;
        let Some(end_rel) = body[id_end + 2..].find("</div>") else {
            break;
        };
        let end = id_end + 2 + end_rel + "</div>".len();
        let mut content = body[id_end + 2..end - "</div>".len()].to_string();
        if let Some(label_start) = content.find(LABEL)
            && let Some(label_end_rel) = content[label_start..].find("</sup>")
        {
            let label_end = label_start + label_end_rel + "</sup>".len();
            content.replace_range(label_start..label_end, "");
        }
        items.push(format!(
            "<li id=\"{}\">{}</li>",
            &body[id_start..id_end],
            content
        ));
        body.replace_range(start..end, "");
    }
    if !items.is_empty() {
        body.push_str("<ol id=\"footnotes\">");
        body.push_str(&items.join(""));
        body.push_str("</ol>");
    }
}

const STRUCTURAL_FALLBACK: &str = r#"
table { width: 100%; border-collapse: collapse; border-spacing: 0; margin: 0 0 20px; }
th, td { padding: 8px 12px; border: 1px solid rgba(127, 127, 127, .22); text-align: left; }
th { font-weight: 700; }
tbody tr:nth-child(even) { background: rgba(127, 127, 127, .055); }
"#;

const MARKDOWN_DOM_COMPATIBILITY: &str = r#"
pre > code { color: inherit; background: transparent; border-radius: 0; font-family: inherit; padding: 0; font-size: inherit; }
pre[data-language] { position: relative; }
pre[data-language] > code { display: block; padding-right: 5.5em; }
pre[data-language]::before { content: attr(data-language); position: absolute; top: 8px; right: 12px; color: #aaa; font-size: 10px; font-weight: 400; line-height: 1; letter-spacing: .08em; text-transform: uppercase; }
.mermaid-diagram { display: flex; justify-content: center; width: 100%; margin: 1.5em 0; overflow-x: auto; }
.mermaid-diagram svg { display: block; max-width: 100%; height: auto; }
ol:not(#footnotes), ul { padding-inline-start: clamp(1.5em, 3vw, 2.25em) !important; }
ol:not(#footnotes) > li::marker { font-variant-numeric: tabular-nums; }
@media print { html { print-color-adjust: exact; -webkit-print-color-adjust: exact; } body { box-sizing: border-box; } img, pre, table, blockquote { break-inside: avoid; } }
"#;

const PDF_FONT_CSS: &str = r#"
body,pre,code,blockquote::before,blockquote::after { font-family: 'Markdown Editor Mono','LXGW WenKai Lite',monospace !important; }
strong,b { font-family: 'Markdown Editor Mono Bold','LXGW WenKai Lite Medium','Markdown Editor Mono','LXGW WenKai Lite',monospace !important; font-weight: 700 !important; }
"#;

const MERMAID_BOOTSTRAP: &str = r#"
(async () => {
    const blocks = Array.from(document.querySelectorAll('pre > code.language-mermaid'));
    if (blocks.length === 0) return;
    mermaid.initialize({ startOnLoad: false, securityLevel: 'strict', suppressErrorRendering: true, fontFamily: "'Markdown Editor Mono', 'LXGW WenKai Lite', monospace" });
    for (const [index, code] of blocks.entries()) {
        const source = code.textContent || '';
        const pre = code.parentElement;
        const diagram = document.createElement('div');
        diagram.className = 'mermaid-diagram';
        diagram.setAttribute('role', 'img');
        diagram.setAttribute('aria-label', 'Mermaid diagram');
        try {
            const rendered = await mermaid.render(`markdown-editor-mermaid-${index}`, source);
            diagram.innerHTML = rendered.svg;
            pre.replaceWith(diagram);
            if (rendered.bindFunctions) rendered.bindFunctions(diagram);
        } catch (error) {
            pre.setAttribute('data-language', 'Mermaid error');
        }
    }
})();
"#;

#[cfg(test)]
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
        egui::FontData::from_static(JB_MONO_REGULAR).into(),
    );
    fonts.font_data.insert(
        "jb_mono_bold".to_string(),
        egui::FontData::from_static(JB_MONO_BOLD).into(),
    );
    fonts.font_data.insert(
        "lxgw_wenkai".to_string(),
        egui::FontData::from_static(LXGW_WENKAI_REGULAR).into(),
    );
    fonts.font_data.insert(
        "lxgw_wenkai_medium".to_string(),
        egui::FontData::from_static(LXGW_WENKAI_MEDIUM).into(),
    );

    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let family_fonts = fonts.families.entry(family).or_default();
        family_fonts.insert(0, "lxgw_wenkai".to_string());
        family_fonts.insert(0, "jb_mono".to_string());
    }

    let bold_family = vec!["jb_mono_bold".to_string(), "lxgw_wenkai_medium".to_string()];

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

    fn parsed(markdown: &str) -> crate::markdown::ParsedDocument {
        crate::markdown::parse_document(markdown)
    }

    #[test]
    fn compatible_strong_markup_is_exported_as_strong_html() {
        let document = parsed("1. **结构层： **训练一个统一的纹样 LoRA。");
        let html = render_html(&document);
        assert!(html.contains("<strong>结构层：</strong> 训练一个统一的纹样 LoRA。"));
        assert!(!html.contains("**结构层"));
    }

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
        let document = parsed(md);
        let html_doc = render_html(&document);
        assert!(html_doc.contains("<h1>会议记录</h1>"));
        assert!(html_doc.contains("<ul>"));
        assert!(html_doc.contains("<li>本周发布 v1.2</li>"));
        assert!(html_doc.contains("<a href=\"https://example.com\">接口文档</a>"));
    }

    #[test]
    fn 代码块内特殊字符不转成标题() {
        let md = "```\n# 这不是标题\n**这不是粗体**\n```\n";
        let document = parsed(md);
        let html_doc = render_html(&document);
        assert!(html_doc.contains("<pre><code>"));
        assert!(!html_doc.contains("<h1>这不是标题</h1>"));
    }

    fn test_options<'a>(base_directory: Option<&'a Path>) -> ExportOptions<'a> {
        ExportOptions {
            title: "测试文档",
            theme_css: "body { color: #123456; } h2 { border-left: 6px solid #ff7e79; }",
            base_directory,
            body_font_size: Some(17.0),
        }
    }

    #[test]
    fn html导出复用主题字号字体和本地图片() {
        let dir = std::env::temp_dir().join(format!("md_editor_html_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("图标.png"),
            include_bytes!("../assets/app-icon-256.png"),
        )
        .unwrap();
        let document = parsed("## 小结\n\n![图标](图标.png)\n");
        let html = render_styled_html(&document, test_options(Some(&dir)));
        assert!(html.contains("body { color: #123456; }"));
        assert!(html.contains("font-size: 17.00px !important"));
        assert!(html.contains("font-family:'Markdown Editor Mono'"));
        assert!(html.contains("src=\"data:image/png;base64,"));
        assert!(!html.contains("src=\"图标.png\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn html导出内嵌原生img标签的相对本地图片() {
        let dir = std::env::temp_dir().join(format!(
            "md_editor_raw_html_image_test_{}",
            std::process::id()
        ));
        let assets = dir.join("无人机动物检测讲解_assets");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(
            assets.join("image7.png"),
            include_bytes!("../assets/app-icon-256.png"),
        )
        .unwrap();
        let document = parsed(
            r#"<img src="./无人机动物检测讲解_assets/image7.png" alt="羊群正样本" width="720">"#,
        );

        let html = render_styled_html(&document, test_options(Some(&dir)));

        assert!(html.contains("src=\"data:image/png;base64,"));
        assert!(html.contains("alt=\"羊群正样本\""));
        assert!(html.contains("width=\"720\""));
        assert!(!html.contains("./无人机动物检测讲解_assets/image7.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn html导出保留mermaid渲染能力() {
        let document = parsed("```mermaid\ngraph TD; A-->B\n```");
        let html = render_styled_html(&document, test_options(None));
        assert!(html.contains("language-mermaid"));
        assert!(html.contains("mermaid.initialize"));
        assert!(html.contains("mermaid-diagram"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows绝对图片路径不会被误判为url() {
        assert_eq!(
            local_image_path(r"C:\纹样\莲花.png", None),
            Some(std::path::PathBuf::from(r"C:\纹样\莲花.png"))
        );
    }

    #[test]
    fn 导出pdf生成非空文件() {
        let dir = std::env::temp_dir().join(format!("md_editor_pdf_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("out.pdf");
        let md = "# 标题\n\n中文段落内容。\n\n- 条目一\n- 条目二\n";
        let document = parsed(md);
        match export_pdf(&p, &document, test_options(None)) {
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
