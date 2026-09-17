//! 导出渲染结果：HTML 与 PDF。
//!
//! 导出与编辑器共享 Markdown 解析规则、字号覆盖和应用字体。
//! HTML 会内嵌本地图片与字体；PDF 从同一份主题化 DOM 生成。

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use pulldown_cmark::{CowStr, Event, Tag, html};

use crate::markdown::{self, ParsedDocument};

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
        document
            .events()
            .iter()
            .map(|item| sanitize_export_event(item.event.clone())),
    );
    out
}

pub fn export_html(
    path: &Path,
    document: &ParsedDocument,
    options: ExportOptions<'_>,
) -> Result<(), String> {
    let doc = render_styled_html(document, options);
    crate::storage::write_atomic(path, doc.as_bytes()).map_err(|e| e.to_string())
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
    let doc =
        printpdf::PdfDocument::from_html(&html_doc, &images, &fonts, &options, &mut warnings)?;
    let bytes = doc.save(&printpdf::PdfSaveOptions::default(), &mut warnings);
    crate::storage::write_atomic(path, &bytes).map_err(|e| e.to_string())
}

fn styled_document(body: &str, options: ExportOptions<'_>, include_mermaid: bool) -> String {
    // Theme packages are user-provided CSS. Keep a less-than sign from being
    // interpreted as an HTML end tag inside the surrounding <style> element.
    let theme_css = sanitize_style_text(options.theme_css);
    let font_size = options
        .body_font_size
        .map(|size| crate::theme::font_size_override_css(&theme_css, size))
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
        embedded_font_css(document_needs_cjk_font(body))
    } else {
        pdf_font_css()
    };
    // Mermaid 的引导脚本必须排在 body 之后：内联 script 在解析到它时就同步执行，
    // 放在 <head> 里时文档还没有任何节点，querySelectorAll 必然为空，图表永远
    // 渲染不出来（defer 对内联脚本无效）。
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{STRUCTURAL_FALLBACK}</style><style>{}</style><style>{}{}{font_size}</style></head><body>{body}{mermaid}</body></html>",
        escape_html(options.title),
        theme_css,
        font_css,
        MARKDOWN_DOM_COMPATIBILITY,
    )
}

fn sanitize_style_text(css: &str) -> String {
    css.replace('<', "\\3c ")
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
            sanitize_export_event(item.event.clone()),
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

/// Prevent exported HTML from turning Markdown links into executable URLs.
///
/// Relative links and ordinary web/mail protocols remain intact. URL schemes
/// that can execute script in a browser are replaced with a harmless fragment;
/// this keeps the visible link text while matching the renderer's safe-by-
/// default boundary.
fn sanitize_link_event<'a>(event: Event<'a>) -> Event<'a> {
    let Event::Start(Tag::Link {
        link_type,
        dest_url,
        title,
        id,
    }) = event
    else {
        return event;
    };

    let destination = if markdown::is_safe_link_destination(dest_url.as_ref()) {
        dest_url
    } else {
        CowStr::Borrowed("#")
    };
    Event::Start(Tag::Link {
        link_type,
        dest_url: destination,
        title,
        id,
    })
}

fn sanitize_export_event<'a>(event: Event<'a>) -> Event<'a> {
    match sanitize_link_event(event) {
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: if safe_export_image_url(dest_url.as_ref()) {
                dest_url
            } else {
                CowStr::Borrowed("#")
            },
            title,
            id,
        }),
        Event::Html(fragment) => Event::Html(sanitize_raw_html_fragment(fragment.as_ref()).into()),
        Event::InlineHtml(fragment) => {
            Event::InlineHtml(sanitize_raw_html_fragment(fragment.as_ref()).into())
        }
        event => event,
    }
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
            // Never fall back to an unvalidated Markdown image URL. In
            // particular, `javascript:` and `data:` payloads must not survive
            // an export just because the source file could not be embedded.
            .unwrap_or_else(|| CowStr::Borrowed("#"));
            Event::Start(Tag::Image {
                link_type,
                dest_url: destination,
                title,
                id,
            })
        }
        Event::Html(fragment) => Event::Html(
            sanitize_raw_html_fragment(&crate::html_image::rewrite_sources(
                fragment.as_ref(),
                |destination| {
                    export_image_destination(destination, base_directory, mode, images, image_index)
                },
            ))
            .into(),
        ),
        Event::InlineHtml(fragment) => Event::InlineHtml(
            sanitize_raw_html_fragment(&crate::html_image::rewrite_sources(
                fragment.as_ref(),
                |destination| {
                    export_image_destination(destination, base_directory, mode, images, image_index)
                },
            ))
            .into(),
        ),
        event => event,
    }
}

/// Cap embedded image bytes at the same size the preview loader accepts, so
/// one accidental multi-gigabyte file next to the document cannot exhaust
/// memory during export.
const MAX_EMBEDDED_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

fn export_image_destination(
    destination: &str,
    base_directory: Option<&Path>,
    mode: ImageMode,
    images: &mut BTreeMap<String, printpdf::Base64OrRaw>,
    image_index: &mut usize,
) -> Option<String> {
    let path = local_image_path(destination, base_directory)?;
    let content_type = image_content_type(&path)?;
    if std::fs::metadata(&path).ok()?.len() > MAX_EMBEDDED_IMAGE_BYTES {
        return None;
    }
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

/// Keep raw HTML useful for simple inline markup while making exported output
/// inert. Markdown-generated HTML is unaffected; this only handles explicit
/// `Html`/`InlineHtml` events from the source document.
fn sanitize_raw_html_fragment(fragment: &str) -> String {
    const ALLOWED: &[&str] = &[
        "a",
        "b",
        "blockquote",
        "br",
        "code",
        "del",
        "div",
        "em",
        "hr",
        "i",
        "img",
        "li",
        "ol",
        "p",
        "pre",
        "s",
        "small",
        "span",
        "strong",
        "sub",
        "sup",
        "table",
        "tbody",
        "td",
        "th",
        "thead",
        "tr",
        "ul",
    ];
    const BLOCKED: &[&str] = &[
        "script", "style", "iframe", "object", "embed", "form", "meta", "link", "base", "template",
    ];

    let mut output = String::with_capacity(fragment.len());
    let mut cursor = 0;
    while let Some(relative) = fragment[cursor..].find('<') {
        let start = cursor + relative;
        output.push_str(&fragment[cursor..start]);
        let Some(end_rel) = raw_tag_end(&fragment[start..]) else {
            output.push_str("&lt;");
            output.push_str(&fragment[start + 1..]);
            cursor = fragment.len();
            break;
        };
        let end = start + end_rel;
        let token = &fragment[start..=end];
        if token.starts_with("<!--") {
            cursor = end + 1;
            continue;
        }
        let bytes = token.as_bytes();
        let mut name_start = 1usize;
        let closing = bytes.get(name_start) == Some(&b'/');
        if closing {
            name_start += 1;
        }
        while name_start < bytes.len() && bytes[name_start].is_ascii_whitespace() {
            name_start += 1;
        }
        let name_end = (name_start..bytes.len())
            .find(|index| !bytes[*index].is_ascii_alphanumeric())
            .unwrap_or(bytes.len());
        let name = token[name_start..name_end].to_ascii_lowercase();
        if name.is_empty() || name == "!doctype" || name == "![cdata[" {
            output.push_str("&lt;");
            output.push_str(&fragment[start + 1..=end]);
            cursor = end + 1;
            continue;
        }
        if BLOCKED.contains(&name.as_str()) {
            if !closing {
                let lower = fragment[end + 1..].to_ascii_lowercase();
                if let Some(close_start) = lower.find(&format!("</{name}"))
                    && let Some(close_end_rel) = raw_tag_end(&fragment[end + 1 + close_start..])
                {
                    cursor = end + 1 + close_start + close_end_rel + 1;
                    continue;
                }
                cursor = end + 1;
                continue;
            }
            cursor = end + 1;
            continue;
        }
        if !ALLOWED.contains(&name.as_str()) {
            output.push_str("&lt;");
            output.push_str(&fragment[start + 1..=end]);
            cursor = end + 1;
            continue;
        }
        if closing {
            output.push_str("</");
            output.push_str(&name);
            output.push('>');
            cursor = end + 1;
            continue;
        }
        output.push('<');
        output.push_str(&name);
        for (attr, value) in raw_attributes(&token[name_end..token.len() - 1]) {
            let value = match attr.as_str() {
                "href" if name == "a" => {
                    if markdown::is_safe_link_destination(&value) {
                        value
                    } else {
                        "#".to_string()
                    }
                }
                "src" if name == "img" => {
                    if safe_export_image_url(&value) {
                        value
                    } else {
                        continue;
                    }
                }
                "alt" | "title" | "loading" if name == "img" => value,
                "title" if name == "a" => value,
                "class" if name == "code" || name == "pre" || name == "span" => value,
                "style" if name == "img" => {
                    let Some(safe) = sanitize_img_style(&value) else {
                        continue;
                    };
                    safe
                }
                "width" | "height"
                    if name == "img" && value.chars().all(|ch| ch.is_ascii_digit()) =>
                {
                    value
                }
                _ => continue,
            };
            output.push(' ');
            output.push_str(&attr);
            output.push_str("=\"");
            output.push_str(&escape_html(&value));
            output.push('"');
        }
        if token[..token.len() - 1].trim_end().ends_with('/') {
            output.push_str(" />");
        } else {
            output.push('>');
        }
        cursor = end + 1;
    }
    if cursor < fragment.len() {
        output.push_str(&fragment[cursor..]);
    }
    output
}

/// Keep only the layout declarations `html_image` generates for `<img>`
/// width mapping (`display:block`, `width:<px>`, `max-width:100%`). The
/// sanitizer previously stripped every `style` attribute, which silently
/// dropped the injected sizing; anything beyond this allowlist is rejected
/// as a whole so no declaration can smuggle URLs or expressions.
fn sanitize_img_style(value: &str) -> Option<String> {
    let mut kept = Vec::new();
    for declaration in value.split(';') {
        let (property, size) = declaration.split_once(':')?;
        let property = property.trim().to_ascii_lowercase();
        let size = size.trim().to_ascii_lowercase();
        let dimension_ok = size.ends_with("px") && size[..size.len() - 2].parse::<f64>().is_ok();
        let allowed = match property.as_str() {
            "display" => size == "block",
            "width" => dimension_ok,
            "max-width" => size == "100%",
            _ => false,
        };
        if !allowed {
            return None;
        }
        kept.push(format!("{property}:{size}"));
    }
    (!kept.is_empty()).then(|| kept.join(";"))
}

fn raw_tag_end(fragment: &str) -> Option<usize> {
    let bytes = fragment.as_bytes();
    let mut quote = None;
    for (index, byte) in bytes.iter().copied().enumerate().skip(1) {
        match (quote, byte) {
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(open), current) if open == current => quote = None,
            (None, b'>') => return Some(index),
            _ => {}
        }
    }
    None
}

fn raw_attributes(input: &str) -> Vec<(String, String)> {
    let bytes = input.as_bytes();
    let mut attrs = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        while cursor < bytes.len() && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b'/')
        {
            cursor += 1;
        }
        let start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric()
                || matches!(bytes[cursor], b'-' | b'_' | b':' | b'.'))
        {
            cursor += 1;
        }
        if cursor == start {
            cursor += 1;
            continue;
        }
        let name = input[start..cursor].to_ascii_lowercase();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }
        let quote = matches!(bytes[cursor], b'\'' | b'"').then_some(bytes[cursor]);
        if quote.is_some() {
            cursor += 1;
        }
        let value_start = cursor;
        if let Some(quote) = quote {
            while cursor < bytes.len() && bytes[cursor] != quote {
                cursor += 1;
            }
        } else {
            while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
        }
        attrs.push((name, decode_raw_entities(&input[value_start..cursor])));
        if quote.is_some() && cursor < bytes.len() {
            cursor += 1;
        }
    }
    attrs
}

fn decode_raw_entities(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(relative) = value[cursor..].find('&') {
        let start = cursor + relative;
        output.push_str(&value[cursor..start]);
        let tail = &value[start + 1..];
        if let Some(end) = tail.find(';') {
            let entity = &tail[..end];
            let decoded = match entity.to_ascii_lowercase().as_str() {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                numeric if numeric.strip_prefix('#').is_some() => {
                    let numeric = numeric.strip_prefix('#').unwrap();
                    let value = numeric.strip_prefix('x').map_or_else(
                        || numeric.parse::<u32>().ok(),
                        |hex| u32::from_str_radix(hex, 16).ok(),
                    );
                    value.and_then(char::from_u32)
                }
                _ => None,
            };
            if let Some(decoded) = decoded {
                output.push(decoded);
                cursor = start + end + 2;
                continue;
            }
        }
        output.push('&');
        cursor = start + 1;
    }
    output.push_str(&value[cursor..]);
    output
}

fn safe_export_image_url(value: &str) -> bool {
    if [
        "data:image/png;",
        "data:image/jpeg;",
        "data:image/gif;",
        "data:image/webp;",
        "data:image/bmp;",
    ]
    .iter()
    .any(|prefix| value.to_ascii_lowercase().starts_with(prefix))
    {
        return true;
    }
    // SVG can carry scripts and external references. Export only the raster
    // formats that the image loader can safely decode.
    let path_part = value.split(['?', '#']).next().unwrap_or(value);
    if path_part
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("svg"))
    {
        return false;
    }
    let path = Path::new(value);
    !path.is_absolute()
        && !value.starts_with("//")
        && url::Url::parse(value).is_err()
        && !path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
}

fn local_image_path(
    destination: &str,
    base_directory: Option<&Path>,
) -> Option<std::path::PathBuf> {
    if destination.is_empty() || destination.starts_with('#') {
        return None;
    }
    let base = base_directory?;
    // Export only images below the document directory. Absolute paths, file://
    // URLs, and network URLs are intentionally excluded from the embedding path.
    if Path::new(destination).is_absolute() || url::Url::parse(destination).is_ok() {
        return None;
    }
    // 与预览共用同一套归一规则（解百分号转义、丢掉查询串/片段），否则
    // `图%20一.png` 在应用里可见、在导出产物里却解析不到文件。
    let destination = crate::markdown::local_image_destination(destination);
    let candidate = base.join(Path::new(&destination));
    let canonical_base = std::fs::canonicalize(base).ok()?;
    let canonical_candidate = std::fs::canonicalize(candidate).ok()?;
    canonical_candidate
        .starts_with(&canonical_base)
        .then_some(canonical_candidate)
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
        _ => None,
    }
}

/// Both embedded font families are kept for any document containing
/// non-ASCII content (CJK text, typographic punctuation). For a pure-ASCII
/// document the ~26 MB LXGW WenKai payload contributes no glyphs, so it is
/// skipped and the exported file stays browser-sized.
fn document_needs_cjk_font(body: &str) -> bool {
    !body.is_ascii()
}

/// 导出 HTML 会在陌生人的浏览器里打开，内嵌字族缺失或字形不全时的回退栈。
///
/// 只留一个 `monospace` 关键字时，CJK 会落到浏览器默认等宽字体，度量和字重都
/// 对不上；这里把设计系统的系统字族回退一并写上。
const FONT_FALLBACK_STACK: &str =
    "'霞鹜文楷','PingFang SC','Microsoft YaHei','Noto Serif SC',monospace";

fn embedded_font_css(include_cjk: bool) -> String {
    let face = |family: &str, weight: u16, bytes: &[u8]| {
        format!(
            "@font-face{{font-family:'{family}';src:url('data:font/ttf;base64,{}') format('truetype');font-style:normal;font-weight:{weight};font-display:block;}}",
            BASE64.encode(bytes)
        )
    };
    let mono = format!(
        // No `font-synthesis` restriction: the default `style weight` lets the
        // browser oblique the mono face for `*emphasis*` while `strong` still
        // resolves to the real bold face through the `!important` rules below.
        "{}{}body,pre,code,blockquote::before,blockquote::after{{font-family:'Markdown Editor Mono',{stack}!important;}}strong,b{{font-family:'Markdown Editor Mono Bold',{stack}!important;font-weight:700!important;}}",
        face("Markdown Editor Mono", 400, jetbrains_mono_regular_bytes()),
        face(
            "Markdown Editor Mono Bold",
            700,
            jetbrains_mono_bold_bytes()
        ),
        stack = FONT_FALLBACK_STACK,
    );
    if !include_cjk {
        return mono;
    }
    let cjk = format!(
        "{}{}body,pre,code,blockquote::before,blockquote::after{{font-family:'Markdown Editor Mono','LXGW WenKai Lite',{stack}!important;}}strong,b{{font-family:'Markdown Editor Mono Bold','LXGW WenKai Lite Medium','Markdown Editor Mono','LXGW WenKai Lite',{stack}!important;font-weight:700!important;}}",
        face("LXGW WenKai Lite", 400, lxgw_wenkai_regular_bytes()),
        face("LXGW WenKai Lite Medium", 700, lxgw_wenkai_medium_bytes()),
        stack = FONT_FALLBACK_STACK,
    );
    format!("{mono}{cjk}")
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
    let source = body.as_str();
    let mut output = String::with_capacity(source.len() + source.len() / 64);
    let mut search_from = 0;
    while let Some(relative_start) = source[search_from..].find(OPEN) {
        let pre_start = search_from + relative_start;
        output.push_str(&source[search_from..pre_start]);
        let language_start = pre_start + OPEN.len();
        let Some(language_end_relative) = source[language_start..].find('"') else {
            output.push_str(&source[pre_start..]);
            search_from = source.len();
            break;
        };
        let language_end = language_start + language_end_relative;
        let language = source[language_start..language_end].trim();
        output.push_str("<pre");
        if !language.is_empty() {
            output.push_str(" data-language=\"");
            output.push_str(language);
            output.push('"');
        }
        output.push_str(&source[pre_start + "<pre".len()..language_end + 1]);
        search_from = language_end + 1;
    }
    output.push_str(&source[search_from..]);
    *body = output;
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
img { max-width: 100%; }
"#;

const MARKDOWN_DOM_COMPATIBILITY: &str = r#"
pre > code { color: inherit; background: transparent; border-radius: 0; font-family: inherit; padding: 0; font-size: inherit; }
pre[data-language] { position: relative; }
pre[data-language] > code { display: block; padding-right: 5.5em; }
pre[data-language]::before { content: attr(data-language); position: absolute; top: 8px; right: 12px; color: #5E6062; font-size: 11px; font-weight: 400; line-height: 1; letter-spacing: .08em; text-transform: uppercase; }
.mermaid-diagram { display: flex; justify-content: center; width: 100%; margin: 1.5em 0; overflow-x: auto; }
.mermaid-diagram svg { display: block; max-width: 100%; height: auto; }
ol:not(#footnotes), ul { padding-inline-start: clamp(1.5em, 3vw, 2.25em) !important; }
ol:not(#footnotes) > li::marker { font-variant-numeric: tabular-nums; }
@media print { html { print-color-adjust: exact; -webkit-print-color-adjust: exact; } body { box-sizing: border-box; } img, pre, table, blockquote { break-inside: avoid; } }
"#;

/// PDF 走 printpdf 的字族映射，回退栈只在 PDF 被当作 HTML 解析时有意义；
/// 仍然与 HTML 导出共用同一份字族名与回退，避免两条路径分叉。
fn pdf_font_css() -> String {
    format!(
        "
body,pre,code,blockquote::before,blockquote::after {{ font-family: 'Markdown Editor Mono','LXGW WenKai Lite',{FONT_FALLBACK_STACK} !important; }}
strong,b {{ font-family: 'Markdown Editor Mono Bold','LXGW WenKai Lite Medium','Markdown Editor Mono','LXGW WenKai Lite',{FONT_FALLBACK_STACK} !important; font-weight: 700 !important; }}
"
    )
}

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
/// JetBrains Mono 斜体，用于渲染 `*强调*`（egui 不会合成倾斜字形）。
const JB_MONO_ITALIC: &[u8] = include_bytes!("../fonts/JetBrainsMono-Italic.ttf");
/// JetBrains Mono 粗斜体，用于渲染 `***加粗斜体***`。
const JB_MONO_BOLD_ITALIC: &[u8] = include_bytes!("../fonts/JetBrainsMono-BoldItalic.ttf");
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

/// 构造应用字体：英文使用 JetBrains Mono，中文回退到霞鹜文楷；斜体与粗体
/// 使用各自的真实字重，代码保留等宽字体。
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
        "jb_mono_italic".to_string(),
        egui::FontData::from_static(JB_MONO_ITALIC).into(),
    );
    fonts.font_data.insert(
        "jb_mono_bold_italic".to_string(),
        egui::FontData::from_static(JB_MONO_BOLD_ITALIC).into(),
    );
    fonts.font_data.insert(
        "lxgw_wenkai".to_string(),
        egui::FontData::from_static(LXGW_WENKAI_REGULAR).into(),
    );
    fonts.font_data.insert(
        "lxgw_wenkai_medium".to_string(),
        egui::FontData::from_static(LXGW_WENKAI_MEDIUM).into(),
    );

    // Document and UI text: JetBrains Mono first so Latin glyphs always come
    // from it, 霞鹜文楷 picks up the CJK glyphs it lacks. egui's bundled
    // Ubuntu-Light is dropped from the text slots; only the emoji fallback
    // fonts from the default set are kept at the tail.
    let default_families = fonts.families.clone();
    let keep_fallbacks = |family: &[String]| -> Vec<String> {
        family
            .iter()
            .filter(|name| {
                let lower = name.to_ascii_lowercase();
                lower.contains("emoji") || lower.contains("symbol")
            })
            .cloned()
            .collect()
    };
    {
        let proportional = fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default();
        *proportional = ["jb_mono", "lxgw_wenkai"]
            .into_iter()
            .map(str::to_string)
            .collect();
        proportional.extend(keep_fallbacks(
            &default_families[&egui::FontFamily::Proportional],
        ));
    }
    {
        let monospace = fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default();
        *monospace = ["jb_mono", "lxgw_wenkai"]
            .into_iter()
            .map(str::to_string)
            .collect();
        monospace.extend(keep_fallbacks(
            &default_families[&egui::FontFamily::Monospace],
        ));
    }

    // egui has no font-weight concept, so each style is its own family. CJK
    // falls back to the matching 霞鹜文楷 weight.
    fonts.families.insert(
        egui::FontFamily::Name("bold".into()),
        vec![
            "jb_mono_bold".to_string(),
            "lxgw_wenkai_medium".to_string(),
            "NotoEmoji-Regular".to_string(),
            "emoji-icon-font".to_string(),
        ],
    );
    fonts.families.insert(
        egui::FontFamily::Name("italic".into()),
        vec![
            "jb_mono_italic".to_string(),
            "lxgw_wenkai".to_string(),
            "NotoEmoji-Regular".to_string(),
            "emoji-icon-font".to_string(),
        ],
    );
    fonts.families.insert(
        egui::FontFamily::Name("bold_italic".into()),
        vec![
            "jb_mono_bold_italic".to_string(),
            "lxgw_wenkai_medium".to_string(),
            "NotoEmoji-Regular".to_string(),
            "emoji-icon-font".to_string(),
        ],
    );
    // CJK 标点专用族：JetBrains Mono 自带 `—— … “” ‘’ ·` 的半宽字形，而
    // 中文排版需要全角（霞鹜文楷）。预览层按字符把这些标点切分到本族，
    // 霞鹜文楷优先，其余字形仍回退 JetBrains Mono 保持一致。
    fonts.families.insert(
        egui::FontFamily::Name("cjk".into()),
        vec![
            "lxgw_wenkai".to_string(),
            "jb_mono".to_string(),
            "NotoEmoji-Regular".to_string(),
            "emoji-icon-font".to_string(),
        ],
    );
    fonts.families.insert(
        egui::FontFamily::Name("cjk_bold".into()),
        vec![
            "lxgw_wenkai_medium".to_string(),
            "jb_mono_bold".to_string(),
            "NotoEmoji-Regular".to_string(),
            "emoji-icon-font".to_string(),
        ],
    );
    fonts.families.insert(
        egui::FontFamily::Name("cjk_italic".into()),
        vec![
            "lxgw_wenkai".to_string(),
            "jb_mono_italic".to_string(),
            "NotoEmoji-Regular".to_string(),
            "emoji-icon-font".to_string(),
        ],
    );
    fonts.families.insert(
        egui::FontFamily::Name("cjk_bold_italic".into()),
        vec![
            "lxgw_wenkai_medium".to_string(),
            "jb_mono_bold_italic".to_string(),
            "NotoEmoji-Regular".to_string(),
            "emoji-icon-font".to_string(),
        ],
    );
    fonts
}

/// 安装应用字体：正文使用比例字体，代码使用 JetBrains Mono，中文使用霞鹜文楷轻便版。
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
    fn strict_parser_keeps_malformed_strong_markup_literal() {
        let document = parsed("1. **结构层： **训练一个统一的纹样 LoRA。");
        let html = render_html(&document);
        assert!(html.contains("**结构层： **训练一个统一的纹样 LoRA。"));
        assert!(!html.contains("<strong>结构层：</strong>"));
    }

    #[test]
    fn 正文使用比例字体代码使用等宽字体() {
        let fonts = app_font_definitions();
        // Latin text must come from JetBrains Mono, CJK falls back to 霞鹜文楷,
        // and egui's bundled Ubuntu-Light must not linger in any text slot.
        for family in [
            egui::FontFamily::Proportional,
            egui::FontFamily::Monospace,
            egui::FontFamily::Name("bold".into()),
            egui::FontFamily::Name("italic".into()),
            egui::FontFamily::Name("bold_italic".into()),
            egui::FontFamily::Name("cjk".into()),
            egui::FontFamily::Name("cjk_bold".into()),
            egui::FontFamily::Name("cjk_italic".into()),
            egui::FontFamily::Name("cjk_bold_italic".into()),
        ] {
            assert!(
                !fonts.families[&family]
                    .iter()
                    .any(|name| name.contains("Ubuntu")),
                "族 {family:?} 不应再包含 Ubuntu-Light"
            );
        }
        let proportional = &fonts.families[&egui::FontFamily::Proportional];
        assert_eq!(proportional[0], "jb_mono");
        assert_eq!(proportional[1], "lxgw_wenkai");
        let monospace = &fonts.families[&egui::FontFamily::Monospace];
        assert_eq!(monospace[0], "jb_mono");
        assert_eq!(monospace[1], "lxgw_wenkai");
        let bold = &fonts.families[&egui::FontFamily::Name("bold".into())];
        assert_eq!(bold[0], "jb_mono_bold");
        assert_eq!(bold[1], "lxgw_wenkai_medium");
        let italic = &fonts.families[&egui::FontFamily::Name("italic".into())];
        assert_eq!(italic[0], "jb_mono_italic");
        let bold_italic = &fonts.families[&egui::FontFamily::Name("bold_italic".into())];
        assert_eq!(bold_italic[0], "jb_mono_bold_italic");
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
    fn 导出会屏蔽可执行链接协议() {
        let document = parsed("[危险](javascript:alert(1)) [安全](notes/next.md)\n");
        let html_doc = render_html(&document);
        assert!(html_doc.contains("<a href=\"#\">危险</a>"));
        assert!(html_doc.contains("<a href=\"notes/next.md\">安全</a>"));
        assert!(!html_doc.contains("javascript:"));
        let styled = render_styled_html(&document, test_options(None));
        assert!(!styled.contains("javascript:"));
    }

    #[test]
    fn markdown图片无法验证时不会回退到原始地址() {
        let document = parsed("![危险](javascript:alert(1))\n![网络](https://example.com/a.png)\n");
        let plain_html = render_html(&document);
        assert!(!plain_html.to_ascii_lowercase().contains("javascript:"));
        assert!(!plain_html.contains("https://example.com/a.png"));
        let html_doc = render_styled_html(&document, test_options(None));
        assert!(!html_doc.to_ascii_lowercase().contains("javascript:"));
        assert!(!html_doc.contains("https://example.com/a.png"));
        assert!(html_doc.matches("src=\"#\"").count() >= 2);
    }

    #[test]
    fn svg图片不会被原样内嵌() {
        let dir = std::env::temp_dir().join(format!(
            "md_editor_svg_image_boundary_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("unsafe.svg"),
            br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#,
        )
        .unwrap();
        let document = parsed("![图](unsafe.svg)\n");
        let html_doc = render_styled_html(&document, test_options(Some(&dir)));
        assert!(!html_doc.contains("image/svg+xml"));
        assert!(!html_doc.contains("<script>alert(1)</script>"));
        assert!(html_doc.contains("src=\"#\""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn 主题css不会突破style边界() {
        let document = parsed("安全\n");
        let html_doc = render_styled_html(
            &document,
            ExportOptions {
                title: "测试",
                theme_css: "body { color: red; } </style><script>alert(1)</script>",
                body_font_size: None,
                base_directory: None,
            },
        );
        assert!(!html_doc.contains("</style><script>"));
        assert!(html_doc.contains("\\3c "));
    }

    #[test]
    fn 原始html仅保留安全标签和属性() {
        let document = parsed(
            r#"<script>alert(1)</script><div onclick="alert(2)" style="color:red">安全</div><img src="javascript:alert(3)" onerror="alert(4)" alt="图">"#,
        );
        let html_doc = render_html(&document);
        assert!(!html_doc.contains("<script"));
        assert!(!html_doc.contains("onclick"));
        assert!(!html_doc.contains("onerror"));
        assert!(!html_doc.contains("javascript:"));
        assert!(html_doc.contains("<div>安全</div>"));
        assert!(html_doc.contains("<img alt=\"图\">") || html_doc.contains("<img alt=\"图\" />"));
    }

    #[test]
    fn 前导空白或实体编码的可执行协议在导出中同样被阻断() {
        // Browsers strip leading control characters and entities like &#x0A;
        // before resolving the scheme, so the sanitizer must normalize first.
        let document = parsed(
            r#"<a href=" javascript:alert(1)">危险一</a><a href="&#x09;javascript:alert(2)">危险二</a><a href="&#10;javascript:alert(3)">危险三</a>"#,
        );
        let html_doc = render_html(&document);
        let lower = html_doc.to_ascii_lowercase();
        assert!(!lower.contains("javascript"), "{html_doc}");
        assert_eq!(html_doc.matches("href=\"#\"").count(), 3, "{html_doc}");
    }

    #[test]
    fn 图片style仅保留尺寸白名单其余整体丢弃() {
        let document = parsed(
            r#"<img src="a.png" style="display:block;width:320px;max-width:100%"><img src="b.png" style="position:fixed;background:url(http://evil/x)">"#,
        );
        let html_doc = render_html(&document);
        assert!(
            html_doc.contains("style=\"display:block;width:320px;max-width:100%\""),
            "{html_doc}"
        );
        assert!(!html_doc.contains("position"), "{html_doc}");
        assert!(!html_doc.contains("url("), "{html_doc}");
    }

    #[test]
    fn 本地图片仅允许文档目录内的相对路径() {
        let dir =
            std::env::temp_dir().join(format!("md_editor_image_boundary_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(
            dir.join("assets/a.png"),
            include_bytes!("../assets/app-icon-256.png"),
        )
        .unwrap();
        assert!(local_image_path("assets/a.png", Some(&dir)).is_some());
        assert!(local_image_path("../a.png", Some(&dir)).is_none());
        assert!(local_image_path("file:///C:/secret.png", Some(&dir)).is_none());
        assert!(local_image_path("C:/secret.png", Some(&dir)).is_none());
        // 预览会解百分号转义并丢掉查询串/片段，导出必须走同一套规则。
        std::fs::write(
            dir.join("assets/a b.png"),
            include_bytes!("../assets/app-icon-256.png"),
        )
        .unwrap();
        assert!(local_image_path("assets/a%20b.png", Some(&dir)).is_some());
        assert!(local_image_path("assets/a.png?v=2", Some(&dir)).is_some());
        assert!(local_image_path("assets/a.png#片段", Some(&dir)).is_some());
        // 归一之后再越界同样要被挡下。
        assert!(local_image_path("..%2F..%2Fsecret.png", Some(&dir)).is_none());
        let _ = std::fs::remove_dir_all(dir);
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
    fn html导出内嵌百分号转义的本地图片() {
        // 回归：预览会解 %20 并忽略查询串，导出曾按字面拼路径，
        // 同一张图在应用里能看见、在导出产物里变成 src="#"。
        let dir = std::env::temp_dir().join(format!(
            "md_editor_encoded_image_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("图 一.png"),
            include_bytes!("../assets/app-icon-256.png"),
        )
        .unwrap();
        let document = parsed("![图](图%20一.png)\n");
        let html = render_styled_html(&document, test_options(Some(&dir)));
        assert!(
            html.contains("src=\"data:image/png;base64,"),
            "百分号转义的图片必须被内嵌，实际输出: {html}"
        );
        assert!(!html.contains("src=\"#\""), "不得退化成占位：{html}");
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
        assert!(
            html.contains("style=\"width:720px;max-width:100%\""),
            "重写注入的尺寸样式必须穿过消毒器存活，实际输出: {html}"
        );
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
        // 引导脚本在 <head> 里会先于 body 解析执行，querySelectorAll 拿到空集合，
        // 图表永远渲染不出来；它必须排在 body 之后。定位用引导脚本自己的
        // 渲染 id 前缀：mermaid 压缩包内部也含 `mermaid.initialize` 与
        // `</body>` 字样，按它们取下标会落到错误的位置。
        let body_start = html.find("<body>").expect("导出必须有 body");
        let bootstrap = html
            .find("markdown-editor-mermaid-")
            .expect("必须带引导脚本");
        let body_end = html.rfind("</body>").expect("导出必须有闭合 body");
        assert!(bootstrap > body_start, "引导脚本不能出现在 <head> 里");
        assert!(bootstrap < body_end, "引导脚本必须仍在文档内");
    }

    #[test]
    fn html导出按内容决定是否内嵌中文字体() {
        // A pure-ASCII document must not pay for the ~26 MB LXGW payload,
        // while any CJK or typographic punctuation still embeds both families.
        let ascii = parsed("# Title\n\nEnglish body text only.\n");
        let ascii_html = render_styled_html(&ascii, test_options(None));
        assert!(ascii_html.contains("@font-face"));
        assert!(ascii_html.contains("Markdown Editor Mono"));
        assert!(
            !ascii_html.contains("LXGW WenKai"),
            "纯 ASCII 导出不应内嵌中文字体"
        );

        let cjk = parsed("# 标题\n\n正文内容。\n");
        let cjk_html = render_styled_html(&cjk, test_options(None));
        assert!(
            cjk_html.contains("LXGW WenKai Lite"),
            "含中文导出必须内嵌中文字体"
        );
        assert!(cjk_html.contains("font-family:'Markdown Editor Mono','LXGW WenKai Lite'"));
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
