//! 使用系统浏览器引擎执行主题 CSS 的 Markdown 预览。

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use pulldown_cmark::{Parser, html};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_TYPE};
use wry::http::{Request, Response};
use wry::{Rect, ScrollBarStyle, WebView, WebViewBuilder, WebViewBuilderExtWindows};

pub struct BrowserPreview {
    webview: Option<WebView>,
    document_hash: u64,
    bounds: Option<[i32; 4]>,
    visible: bool,
}

impl Default for BrowserPreview {
    fn default() -> Self {
        Self {
            webview: None,
            document_hash: 0,
            bounds: None,
            visible: false,
        }
    }
}

impl BrowserPreview {
    pub fn show(
        &mut self,
        frame: &eframe::Frame,
        rect: egui::Rect,
        pixels_per_point: f32,
        document: &str,
    ) -> Result<(), String> {
        let bounds = physical_bounds(rect, pixels_per_point);
        let document_hash = hash(document);

        if self.webview.is_none() {
            let window = frame
                .winit_window()
                .ok_or_else(|| "当前窗口后端不支持浏览器预览".to_string())?;
            let webview = WebViewBuilder::new()
                .with_https_scheme(true)
                .with_custom_protocol("mdfont".into(), |_webview_id, request| {
                    preview_asset_response(request)
                })
                .with_html(document.to_owned())
                .with_bounds(to_wry_rect(bounds))
                .with_visible(true)
                .with_focused(false)
                .with_clipboard(true)
                .with_browser_accelerator_keys(false)
                .with_scroll_bar_style(ScrollBarStyle::FluentOverlay)
                .build_as_child(window)
                .map_err(|error| format!("无法创建 WebView2 预览：{error}"))?;
            self.webview = Some(webview);
            self.document_hash = document_hash;
            self.bounds = Some(bounds);
            self.visible = true;
            return Ok(());
        }

        let webview = self.webview.as_ref().expect("webview 已初始化");
        if self.bounds != Some(bounds) {
            webview
                .set_bounds(to_wry_rect(bounds))
                .map_err(|error| format!("无法调整预览区域：{error}"))?;
            self.bounds = Some(bounds);
        }
        if self.document_hash != document_hash {
            webview
                .load_html(document)
                .map_err(|error| format!("无法刷新浏览器预览：{error}"))?;
            self.document_hash = document_hash;
        }
        if !self.visible {
            webview
                .set_visible(true)
                .map_err(|error| format!("无法显示浏览器预览：{error}"))?;
            self.visible = true;
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        if self.visible {
            if let Some(webview) = &self.webview {
                let _ = webview.set_visible(false);
                let _ = webview.focus_parent();
            }
            self.visible = false;
        }
    }

    pub fn focus_parent(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.focus_parent();
        }
    }
}

pub fn document(
    markdown: &str,
    css: &str,
    base_directory: Option<&Path>,
    font_size_override: Option<f32>,
) -> String {
    let parser = Parser::new_ext(markdown, crate::markdown::parse_options());
    let mut body = String::new();
    html::push_html(&mut body, parser);
    annotate_code_languages(&mut body);
    normalize_footnote_dom(&mut body);

    let base = base_directory
        .and_then(|path| url::Url::from_directory_path(path).ok())
        .map(|url| format!(r#"<base href="{}">"#, escape_attribute(url.as_str())))
        .unwrap_or_default();
    let font_override = font_size_override
        .map(|size| format!("body {{ font-size: {size:.2}px !important; }}"))
        .unwrap_or_default();
    let editor_font = editor_font_css();

    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"script-src https://mdfont.localhost; object-src 'none'; base-uri 'self' file:\">{base}<style>{STRUCTURAL_FALLBACK}</style><style>{css}</style><style>{editor_font}{MARKDOWN_DOM_COMPATIBILITY}{font_override}</style><script defer src=\"https://mdfont.localhost/mermaid.min.js\"></script><script defer src=\"https://mdfont.localhost/mermaid-init.js\"></script></head><body>{body}</body></html>"
    )
}

fn editor_font_css() -> &'static str {
    "@font-face{font-family:'Markdown Editor Mono';src:url('https://mdfont.localhost/jetbrains-regular.ttf') format('truetype');font-style:normal;font-weight:400;font-display:block;}\
     @font-face{font-family:'Markdown Editor Mono';src:url('https://mdfont.localhost/jetbrains-bold.ttf') format('truetype');font-style:normal;font-weight:700;font-display:block;}\
     @font-face{font-family:'LXGW WenKai Lite';src:url('https://mdfont.localhost/lxgw-regular.ttf') format('truetype');font-style:normal;font-weight:400;font-display:block;}\
     @font-face{font-family:'LXGW WenKai Lite';src:url('https://mdfont.localhost/lxgw-medium.ttf') format('truetype');font-style:normal;font-weight:500 900;font-display:block;}\
     body,pre,code,blockquote::before,blockquote::after{font-family:'Markdown Editor Mono','LXGW WenKai Lite','SimHei','DengXian','SimSun','Microsoft YaHei',monospace!important;}"
}

fn preview_asset_response(request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let (bytes, content_type, cache_control): (&'static [u8], &str, &str) =
        match request.uri().path() {
            "/jetbrains-regular.ttf" => (
                crate::export::jetbrains_mono_regular_bytes(),
                "font/ttf",
                "public, max-age=31536000, immutable",
            ),
            "/jetbrains-bold.ttf" => (
                crate::export::jetbrains_mono_bold_bytes(),
                "font/ttf",
                "public, max-age=31536000, immutable",
            ),
            "/lxgw-regular.ttf" => (
                crate::export::lxgw_wenkai_regular_bytes(),
                "font/ttf",
                "public, max-age=31536000, immutable",
            ),
            "/lxgw-medium.ttf" => (
                crate::export::lxgw_wenkai_medium_bytes(),
                "font/ttf",
                "public, max-age=31536000, immutable",
            ),
            "/mermaid.min.js" => (
                include_bytes!("../assets/mermaid-11.16.0.min.js"),
                "text/javascript; charset=utf-8",
                "public, max-age=31536000, immutable",
            ),
            "/mermaid-init.js" => (
                MERMAID_BOOTSTRAP.as_bytes(),
                "text/javascript; charset=utf-8",
                "no-cache",
            ),
            _ => {
                return Response::builder()
                    .status(404)
                    .body(Cow::Borrowed(&[] as &[u8]))
                    .expect("有效的预览资源 404 响应");
            }
        };
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .header("X-Content-Type-Options", "nosniff")
        .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CACHE_CONTROL, cache_control)
        .body(Cow::Borrowed(bytes))
        .expect("有效的预览资源响应")
}

const MERMAID_BOOTSTRAP: &str = r#"
(async () => {
    const blocks = Array.from(document.querySelectorAll('pre > code.language-mermaid'));
    if (blocks.length === 0) return;

    mermaid.initialize({
        startOnLoad: false,
        securityLevel: 'strict',
        suppressErrorRendering: true,
        fontFamily: "'Markdown Editor Mono', 'LXGW WenKai Lite', monospace"
    });

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
            pre.classList.add('mermaid-error');
            pre.setAttribute('data-language', 'Mermaid error');
            const message = document.createElement('div');
            message.className = 'mermaid-error-message';
            message.textContent = `Mermaid 图表语法错误：${error?.message || String(error)}`;
            pre.after(message);
        }
    }
})();
"#;

/// 只为浏览器默认没有可用排版的 Markdown 结构提供底线样式。
/// 放在主题 CSS 之前，所以主题只要声明同一属性就会自然覆盖这里。
const STRUCTURAL_FALLBACK: &str = r#"
table { width: 100%; border-collapse: collapse; border-spacing: 0; margin: 0 0 20px; }
th, td { padding: 8px 12px; border: 1px solid rgba(127, 127, 127, .22); text-align: left; }
th { font-weight: 700; }
tbody tr:nth-child(even) { background: rgba(127, 127, 127, .055); }
"#;

/// pulldown-cmark 用 `<pre><code>` 表示代码块，而这类传统 Web 主题通常按
/// `<pre>` 单层结构编写。这里只消除内部 `code` 包装造成的重复样式。
const MARKDOWN_DOM_COMPATIBILITY: &str = r#"
pre > code { color: inherit; background: transparent; border-radius: 0; font-family: inherit; padding: 0; font-size: inherit; }
pre[data-language] { position: relative; }
pre[data-language] > code { display: block; padding-right: 5.5em; }
pre[data-language]::before {
    content: attr(data-language);
    position: absolute;
    top: 8px;
    right: 12px;
    color: #aaa;
    font-size: 10px;
    font-weight: 400;
    line-height: 1;
    letter-spacing: .08em;
    text-transform: uppercase;
    pointer-events: none;
}
.mermaid-diagram {
    display: flex;
    justify-content: center;
    width: 100%;
    margin: 1.5em 0;
    overflow-x: auto;
}
.mermaid-diagram svg { display: block; max-width: 100%; height: auto; }
pre.mermaid-error { border-color: rgba(220, 70, 70, .45); }
.mermaid-error-message {
    margin: -.75em 0 1.5em;
    color: #b42318;
    font-size: .85em;
    white-space: pre-wrap;
}
ol:not(#footnotes), ul {
    padding-inline-start: clamp(1.5em, 3vw, 2.25em) !important;
}
ol:not(#footnotes) > li::marker {
    font-variant-numeric: tabular-nums;
}
html { scrollbar-width: none; }
::-webkit-scrollbar { width: 0; height: 0; }
@media (max-width: 700px) {
    body > h1:first-child { margin-top: 0; margin-bottom: 10px; }
    body > h1:first-child + p { margin-bottom: 12px; }
    body > h1:first-child + p + ul,
    body > h1:first-child + p + ol { margin-top: 0; }
    li { margin-top: .45em; margin-bottom: .45em; }
}
"#;

fn physical_bounds(rect: egui::Rect, pixels_per_point: f32) -> [i32; 4] {
    let scale = pixels_per_point.max(0.1);
    [
        (rect.min.x * scale).round() as i32,
        (rect.min.y * scale).round() as i32,
        (rect.width() * scale).round().max(1.0) as i32,
        (rect.height() * scale).round().max(1.0) as i32,
    ]
}

fn to_wry_rect(bounds: [i32; 4]) -> Rect {
    Rect {
        position: PhysicalPosition::new(bounds[0], bounds[1]).into(),
        size: PhysicalSize::new(bounds[2] as u32, bounds[3] as u32).into(),
    }
}

fn hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn escape_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

/// 把围栏代码的 `language-*` 类同步到 `<pre data-language>`，供右上角标签显示。
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
        if language.is_empty() {
            search_from = language_end + 1;
            continue;
        }
        let attribute = format!(" data-language=\"{language}\"");
        let insertion = pre_start + "<pre".len();
        body.insert_str(insertion, &attribute);
        search_from = language_end + attribute.len() + 1;
    }
}

/// pulldown-cmark 输出 `.footnote-definition`，而传统 Markdown Web 主题通常
/// 使用 `ol#footnotes > li`。转换结构后，主题原有选择器可以直接生效。
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
        let content_start = id_end + 2;
        let Some(close_rel) = body[content_start..].find("</div>") else {
            break;
        };
        let close_start = content_start + close_rel;
        let close_end = close_start + "</div>".len();

        let id = body[id_start..id_end].to_string();
        let mut content = body[content_start..close_start].to_string();
        if content.starts_with(LABEL)
            && let Some(label_end) = content.find("</sup>")
        {
            content.replace_range(..label_end + "</sup>".len(), "");
            content = content.trim_start_matches(['\r', '\n']).to_string();
        }
        items.push(format!("<li id=\"{id}\">{content}</li>"));
        body.replace_range(start..close_end, "");
    }

    if !items.is_empty() {
        body.push_str("<ol id=\"footnotes\">\n");
        for item in items {
            body.push_str(&item);
            body.push('\n');
        }
        body.push_str("</ol>\n");
    }
}

#[cfg(test)]
mod tests {
    use super::{document, preview_asset_response};
    use wry::http::Request;

    #[test]
    fn markdown与css原样进入浏览器文档() {
        let html = document(
            "# 标题\n\n`代码`",
            "h1 { color: #f00; } code::before { content: '>'; }",
            None,
            None,
        );
        assert!(html.contains("<h1>标题</h1>"));
        assert!(html.contains("<code>代码</code>"));
        assert!(html.contains("code::before { content: '>'; }"));
        assert!(html.contains("pre > code { color: inherit"));
        assert!(html.find("table { width: 100%").unwrap() < html.find("h1 { color").unwrap());
    }

    #[test]
    fn 用户字号覆盖位于主题之后() {
        let html = document("正文", "body { font-size: 15px; }", None, Some(18.0));
        let theme = html.find("font-size: 15px").unwrap();
        let override_rule = html.find("font-size: 18.00px !important").unwrap();
        assert!(override_rule > theme);
    }

    #[test]
    fn 窄分栏使用响应式阅读节奏() {
        let html = document("# 标题\n\n正文\n\n- 一\n- 二", "", None, None);
        assert!(html.contains("@media (max-width: 700px)"));
        assert!(html.contains("body > h1:first-child { margin-top: 0; margin-bottom: 10px; }"));
        assert!(html.contains("li { margin-top: .45em; margin-bottom: .45em; }"));
    }

    #[test]
    fn 普通列表使用稳定缩进且不影响脚注() {
        let html = document(
            "1. 第一项\n2. 第二项",
            crate::theme::BUILT_IN_SSPAI_CSS,
            None,
            None,
        );
        assert!(html.contains(
            "ol:not(#footnotes), ul {\n    padding-inline-start: clamp(1.5em, 3vw, 2.25em) !important;"
        ));
        assert!(html.contains("font-variant-numeric: tabular-nums"));
    }

    #[test]
    fn 预览使用编辑区内置字体() {
        let html = document(
            "# 标题\n\n正文 `代码`",
            "body { font-family: serif; }",
            None,
            None,
        );
        assert!(html.contains("@font-face{font-family:'Markdown Editor Mono'"));
        assert!(html.contains("font-family:'LXGW WenKai Lite'"));
        assert!(html.contains("https://mdfont.localhost/lxgw-regular.ttf"));
        assert!(html.contains(
            "body,pre,code,blockquote::before,blockquote::after{font-family:'Markdown Editor Mono','LXGW WenKai Lite'"
        ));
        assert!(html.find("font-family: serif").unwrap() < html.find("body,pre,code").unwrap());
    }

    #[test]
    fn 本地协议提供内置霞鹜文楷字体() {
        let request = Request::builder()
            .uri("mdfont://localhost/lxgw-regular.ttf")
            .body(Vec::new())
            .unwrap();
        let response = preview_asset_response(request);
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "font/ttf");
        assert_eq!(
            response.body().len(),
            crate::export::lxgw_wenkai_regular_bytes().len()
        );
    }

    #[test]
    fn 围栏代码在右上角标注语言() {
        let html = document("```rust\nfn main() {}\n```", "", None, None);
        assert!(html.contains("<pre data-language=\"rust\"><code class=\"language-rust\">"));
        assert!(html.contains("content: attr(data-language)"));
        assert!(html.contains("text-transform: uppercase"));
    }

    #[test]
    fn mermaid_代码块加载内置渲染器() {
        let html = document(
            "```mermaid\nstateDiagram-v2\n    [*] --> Standby\n```",
            "",
            None,
            None,
        );
        assert!(html.contains("<pre data-language=\"mermaid\"><code class=\"language-mermaid\">"));
        assert!(html.contains("https://mdfont.localhost/mermaid.min.js"));
        assert!(html.contains("https://mdfont.localhost/mermaid-init.js"));
        assert!(html.contains("script-src https://mdfont.localhost"));
        assert!(!html.contains("script-src 'unsafe-inline'"));
    }

    #[test]
    fn 本地协议提供_mermaid_运行库和启动脚本() {
        for path in ["mermaid.min.js", "mermaid-init.js"] {
            let request = Request::builder()
                .uri(format!("mdfont://localhost/{path}"))
                .body(Vec::new())
                .unwrap();
            let response = preview_asset_response(request);
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.headers()["content-type"],
                "text/javascript; charset=utf-8"
            );
            assert!(!response.body().is_empty());
        }
    }

    #[test]
    fn 未声明语言的代码块不显示标签() {
        let html = document("```\nplain\n```", "", None, None);
        assert!(html.contains("<pre><code>plain"));
        assert!(!html.contains("<pre data-language="));
    }

    #[test]
    fn 少数派二级标题保留粉色边线() {
        let html = document("## 小结", crate::theme::BUILT_IN_SSPAI_CSS, None, None);
        assert!(html.contains("<h2>小结</h2>"));
        assert!(html.contains("border-left: 6px solid #ff7e79"));
    }

    #[test]
    fn 审计markdown生成的主题选择器结构() {
        let markdown = "# 一级\n\n## 二级\n\n> 引用\n\n行内 `代码`。\n\n```rust\nfn main() {}\n```\n\n![图片](image.png)\n\n| 功能 | 状态 |\n| --- | --- |\n| 编辑 | 可用 |\n\n脚注[^1]\n\n[^1]: 脚注内容\n";
        let html = document(markdown, crate::theme::BUILT_IN_SSPAI_CSS, None, None);
        assert!(html.contains("<h1>一级</h1>"));
        assert!(html.contains("<h2>二级</h2>"));
        assert!(html.contains("<blockquote>"));
        assert!(html.contains("<pre data-language=\"rust\"><code class=\"language-rust\">"));
        assert!(html.contains("<img src=\"image.png\" alt=\"图片\""));
        assert!(html.contains("<table>"));
        assert!(html.contains("<ol id=\"footnotes\">"));
        assert!(html.contains("<li id=\"1\"><p>脚注内容</p>"));
        assert!(!html.contains("class=\"footnote-definition\""));
    }
}
