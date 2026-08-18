//! 可导入的 JSON 文档主题包。

use std::path::PathBuf;
use std::{fs::File, io::Read, path::Path};

use egui::Color32;
use serde::{Deserialize, Serialize};

use crate::storage;

pub const BUILT_IN_SSPAI_CSS: &str = include_str!("../assets/sspai.css");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HeadingStyle {
    Plain,
    Card,
    Tech,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThemePackage {
    pub name: String,
    #[serde(default)]
    pub author: String,
    /// 原始 CSS。浏览器预览直接执行它，不再转换为 egui 样式。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub css: Option<String>,
    pub light: ThemeColors,
    pub dark: ThemeColors,
    #[serde(default)]
    pub layout: ThemeLayout,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThemeColors {
    pub canvas: String,
    pub editor_canvas: String,
    pub panel: String,
    pub text: String,
    pub muted: String,
    pub heading: String,
    pub accent: String,
    pub border: String,
    pub code_bg: String,
    pub quote_bg: String,
    pub table_alt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ThemeLayout {
    pub heading_style: HeadingStyle,
    pub body_font_size: f32,
    pub content_width: f32,
    pub preview_padding: i8,
    pub block_spacing: f32,
    pub line_height: f32,
    pub list_item_spacing: f32,
    pub code_radius: u8,
    pub code_padding_x: i8,
    pub code_padding_y: i8,
    pub table_spacing_x: f32,
    pub table_spacing_y: f32,
}

impl Default for ThemeLayout {
    fn default() -> Self {
        Self {
            heading_style: HeadingStyle::Plain,
            body_font_size: 15.5,
            content_width: 820.0,
            preview_padding: 40,
            block_spacing: 10.0,
            line_height: 1.55,
            list_item_spacing: 6.0,
            code_radius: 8,
            code_padding_x: 16,
            code_padding_y: 13,
            table_spacing_x: 22.0,
            table_spacing_y: 10.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeSpec {
    pub canvas: Color32,
    pub editor_canvas: Color32,
    pub panel: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub heading: Color32,
    pub accent: Color32,
    pub border: Color32,
    pub code_bg: Color32,
    pub quote_bg: Color32,
    pub table_alt: Color32,
    pub heading_style: HeadingStyle,
    pub content_width: f32,
    pub preview_padding: i8,
    pub block_spacing: f32,
    pub line_height: f32,
    pub list_item_spacing: f32,
    pub code_radius: u8,
    pub code_padding: [i8; 2],
    pub table_spacing: [f32; 2],
}

impl ThemePackage {
    pub fn built_in_sspai() -> Self {
        Self {
            name: "少数派经典".to_string(),
            author: "Built-in".to_string(),
            css: Some(BUILT_IN_SSPAI_CSS.to_string()),
            light: ThemeColors {
                canvas: "#FFFFFF".to_string(),
                editor_canvas: "#FFFFFF".to_string(),
                panel: "#FAFAFA".to_string(),
                text: "#333333".to_string(),
                muted: "#888888".to_string(),
                heading: "#333333".to_string(),
                accent: "#FF7E79".to_string(),
                border: "#EEEEEE".to_string(),
                code_bg: "#F8F8F8".to_string(),
                quote_bg: "#FFFFFF".to_string(),
                table_alt: "#FFF1F0".to_string(),
            },
            dark: ThemeColors {
                canvas: "#1C1D1F".to_string(),
                editor_canvas: "#18191B".to_string(),
                panel: "#18191B".to_string(),
                text: "#E8E9EB".to_string(),
                muted: "#96989D".to_string(),
                heading: "#E8E9EB".to_string(),
                accent: "#FF7E79".to_string(),
                border: "#2F3034".to_string(),
                code_bg: "#151618".to_string(),
                quote_bg: "#232427".to_string(),
                table_alt: "#371F20".to_string(),
            },
            layout: ThemeLayout {
                heading_style: HeadingStyle::Tech,
                body_font_size: 15.0,
                content_width: 820.0,
                preview_padding: 40,
                block_spacing: 20.0,
                line_height: 1.64,
                list_item_spacing: 11.25,
                code_radius: 4,
                code_padding_x: 16,
                code_padding_y: 13,
                table_spacing_x: 22.0,
                table_spacing_y: 10.0,
            },
        }
    }

    pub fn recommended_body_font_size(&self) -> f32 {
        let configured = self.layout.body_font_size.clamp(12.0, 22.0);
        if self.author == "CSS Import" && (configured - 15.5).abs() < f32::EPSILON {
            16.5
        } else {
            configured
        }
    }

    pub fn from_file(path: &Path) -> Result<Self, String> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "json" => {
                let text =
                    std::fs::read_to_string(path).map_err(|e| format!("无法读取主题包：{e}"))?;
                Self::from_json(&text)
            }
            "css" => {
                let css =
                    std::fs::read_to_string(path).map_err(|e| format!("无法读取 CSS：{e}"))?;
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("CSS Theme");
                Self::from_css(name, &css)
            }
            "zip" => Self::from_zip(path),
            _ => Err("仅支持 .json、.css 和包含 CSS/JSON 的 .zip 主题包".to_string()),
        }
    }

    fn from_zip(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("无法打开 ZIP：{e}"))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("ZIP 格式无效：{e}"))?;
        let mut css_candidate: Option<(String, String)> = None;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
            if entry.is_dir() || entry.size() > 2 * 1024 * 1024 {
                continue;
            }
            let name = entry.name().to_string();
            let lower = name.to_ascii_lowercase();
            if !lower.ends_with(".json") && !lower.ends_with(".css") {
                continue;
            }
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| format!("无法读取 ZIP 中的 {name}：{e}"))?;
            if lower.ends_with(".json") {
                if let Ok(package) = Self::from_json(&text) {
                    return Ok(package);
                }
            } else if css_candidate.is_none() {
                css_candidate = Some((name, text));
            }
        }
        let Some((name, css)) = css_candidate else {
            return Err("ZIP 中没有找到可用的 .css 或主题 .json 文件".to_string());
        };
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("CSS Theme");
        Self::from_css(stem, &css)
    }

    pub fn from_css(name: &str, css: &str) -> Result<Self, String> {
        if !css.contains('{') || !css.contains('}') {
            return Err("CSS 中没有找到有效规则".to_string());
        }
        let fallback_light = ThemeSpec::fallback(false);
        let fallback_dark = ThemeSpec::fallback(true);
        let body_bg = css_color(css, "body", "background")
            .or_else(|| css_color(css, "body", "background-color"))
            .unwrap_or(fallback_light.canvas);
        let text = css_color(css, "body", "color").unwrap_or(fallback_light.text);
        let accent = css_color(css, "h2", "border-left")
            .or_else(|| css_color(css, "a", "color"))
            .unwrap_or(fallback_light.accent);
        let heading = css_color(css, "h1", "color")
            .or_else(|| css_color(css, "h2", "color"))
            .unwrap_or(text);
        let muted = css_color(css, "blockquote", "color").unwrap_or(fallback_light.muted);
        let code_bg = css_color(css, "pre", "background")
            .or_else(|| css_color(css, "pre", "background-color"))
            .unwrap_or(fallback_light.code_bg);
        let quote_bg = css_color(css, "blockquote", "background")
            .or_else(|| css_color(css, "blockquote", "background-color"))
            .unwrap_or(body_bg);
        let border = css_color(css, "hr", "border-top")
            .or_else(|| css_color(css, "pre", "border"))
            .unwrap_or(fallback_light.border);
        let code_radius = css_number(css, "pre", "border-radius").unwrap_or(8.0) as u8;
        let body_font_size = css_number(css, "body", "font-size").unwrap_or(15.0);
        let css_line_height = css_number(css, "p", "line-height")
            .or_else(|| css_number(css, "body", "line-height"))
            .unwrap_or(1.55);
        let rendered_body_size = body_font_size.clamp(12.0, 22.0);
        let line_height = css_line_height.clamp(1.2, 1.9);
        let block_spacing = css_margin_side(css, "p", false, body_font_size).unwrap_or(10.0);
        let list_item_spacing = css_margin_side(css, "li", false, body_font_size).unwrap_or(6.0);
        let heading_style = if css_property(css, "h2", "border-left").is_some() {
            HeadingStyle::Tech
        } else {
            HeadingStyle::Plain
        };

        let light = ThemeColors::from_spec_colors(
            body_bg,
            body_bg,
            body_bg,
            text,
            muted,
            heading,
            accent,
            border,
            code_bg,
            quote_bg,
            mix_color(body_bg, accent, 0.07),
        );
        let dark = ThemeColors::from_spec_colors(
            fallback_dark.canvas,
            fallback_dark.editor_canvas,
            fallback_dark.panel,
            fallback_dark.text,
            fallback_dark.muted,
            fallback_dark.heading,
            accent,
            fallback_dark.border,
            fallback_dark.code_bg,
            fallback_dark.quote_bg,
            mix_color(fallback_dark.canvas, accent, 0.12),
        );
        Ok(Self {
            name: name.to_string(),
            author: "CSS Import".to_string(),
            css: Some(css.to_string()),
            light,
            dark,
            layout: ThemeLayout {
                heading_style,
                body_font_size: rendered_body_size,
                code_radius,
                line_height,
                block_spacing,
                list_item_spacing,
                ..Default::default()
            },
        })
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let package: Self =
            serde_json::from_str(text).map_err(|e| format!("主题包 JSON 无效：{e}"))?;
        if package.name.trim().is_empty() {
            return Err("主题名称不能为空".to_string());
        }
        package.spec(false)?;
        package.spec(true)?;
        Ok(package)
    }

    /// 返回浏览器预览使用的完整 CSS。
    ///
    /// 旧版本保存的 sspai 主题没有 `css` 字段，这里自动迁移到内置原版。
    pub fn browser_css(&self) -> Option<&str> {
        self.css.as_deref().or_else(|| {
            let is_legacy_sspai =
                self.name.trim().eq_ignore_ascii_case("sspai") || self.name.trim() == "少数派经典";
            is_legacy_sspai.then_some(BUILT_IN_SSPAI_CSS)
        })
    }

    pub fn spec(&self, dark: bool) -> Result<ThemeSpec, String> {
        let colors = if dark { &self.dark } else { &self.light };
        let color = |name: &str, value: &str| {
            parse_color(value).map_err(|e| format!("颜色 {name} 无效：{e}"))
        };
        Ok(ThemeSpec {
            canvas: color("canvas", &colors.canvas)?,
            editor_canvas: color("editor_canvas", &colors.editor_canvas)?,
            panel: color("panel", &colors.panel)?,
            text: color("text", &colors.text)?,
            muted: color("muted", &colors.muted)?,
            heading: color("heading", &colors.heading)?,
            accent: color("accent", &colors.accent)?,
            border: color("border", &colors.border)?,
            code_bg: color("code_bg", &colors.code_bg)?,
            quote_bg: color("quote_bg", &colors.quote_bg)?,
            table_alt: if self.author == "CSS Import" {
                mix_color(
                    color("canvas", &colors.canvas)?,
                    color("accent", &colors.accent)?,
                    if dark { 0.12 } else { 0.07 },
                )
            } else {
                color("table_alt", &colors.table_alt)?
            },
            heading_style: self.layout.heading_style,
            content_width: self.layout.content_width.clamp(560.0, 1200.0),
            preview_padding: self.layout.preview_padding.clamp(12, 80),
            block_spacing: if self.author == "CSS Import"
                && (self.layout.block_spacing - 10.0).abs() < f32::EPSILON
            {
                18.0
            } else {
                self.layout.block_spacing.clamp(4.0, 32.0)
            },
            line_height: if self.author == "CSS Import"
                && (self.layout.line_height - 1.55).abs() < f32::EPSILON
            {
                1.65
            } else {
                self.layout.line_height.clamp(1.0, 2.2)
            },
            list_item_spacing: if self.author == "CSS Import"
                && (self.layout.list_item_spacing - 6.0).abs() < f32::EPSILON
            {
                10.0
            } else {
                self.layout.list_item_spacing.clamp(0.0, 24.0)
            },
            code_radius: self.layout.code_radius.min(24),
            code_padding: [
                self.layout.code_padding_x.clamp(6, 32),
                self.layout.code_padding_y.clamp(4, 28),
            ],
            table_spacing: [
                self.layout.table_spacing_x.clamp(8.0, 48.0),
                self.layout.table_spacing_y.clamp(4.0, 28.0),
            ],
        })
    }
}

impl ThemeColors {
    #[allow(clippy::too_many_arguments)]
    fn from_spec_colors(
        canvas: Color32,
        editor_canvas: Color32,
        panel: Color32,
        text: Color32,
        muted: Color32,
        heading: Color32,
        accent: Color32,
        border: Color32,
        code_bg: Color32,
        quote_bg: Color32,
        table_alt: Color32,
    ) -> Self {
        Self {
            canvas: color_hex(canvas),
            editor_canvas: color_hex(editor_canvas),
            panel: color_hex(panel),
            text: color_hex(text),
            muted: color_hex(muted),
            heading: color_hex(heading),
            accent: color_hex(accent),
            border: color_hex(border),
            code_bg: color_hex(code_bg),
            quote_bg: color_hex(quote_bg),
            table_alt: color_hex(table_alt),
        }
    }
}

impl ThemeSpec {
    pub fn fallback(dark: bool) -> Self {
        let (canvas, editor, panel, text, muted, accent, border, code, quote, table) = if dark {
            (
                Color32::from_rgb(28, 29, 31),
                Color32::from_rgb(24, 25, 27),
                Color32::from_rgb(24, 25, 27),
                Color32::from_rgb(232, 233, 235),
                Color32::from_rgb(150, 152, 157),
                Color32::from_rgb(99, 164, 230),
                Color32::from_rgb(47, 48, 52),
                Color32::from_rgb(21, 22, 24),
                Color32::from_rgb(35, 36, 39),
                Color32::from_rgb(38, 42, 46),
            )
        } else {
            (
                Color32::from_rgb(250, 250, 249),
                Color32::from_rgb(247, 247, 245),
                Color32::from_rgb(247, 247, 245),
                Color32::from_rgb(32, 33, 35),
                Color32::from_rgb(112, 114, 118),
                Color32::from_rgb(38, 116, 181),
                Color32::from_rgb(228, 228, 224),
                Color32::from_rgb(243, 244, 246),
                Color32::from_rgb(247, 247, 245),
                Color32::from_rgb(238, 242, 246),
            )
        };
        Self {
            canvas,
            editor_canvas: editor,
            panel,
            text,
            muted,
            heading: text,
            accent,
            border,
            code_bg: code,
            quote_bg: quote,
            table_alt: table,
            heading_style: HeadingStyle::Plain,
            content_width: 820.0,
            preview_padding: 40,
            block_spacing: 10.0,
            line_height: 1.55,
            list_item_spacing: 6.0,
            code_radius: 8,
            code_padding: [16, 13],
            table_spacing: [22.0, 10.0],
        }
    }
}

const THEME_STATE_LIMIT: u64 = 20 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct SavedThemeEnvelope {
    schema_version: u32,
    saved_at_unix: u64,
    package: ThemePackage,
}

pub fn saved_theme_path() -> PathBuf {
    storage::config_dir().join("themes").join("current.json")
}

fn legacy_theme_path() -> PathBuf {
    std::env::temp_dir().join("markdown-editor-theme.json")
}

fn validate_saved_package(package: ThemePackage) -> Option<ThemePackage> {
    if package.name.trim().is_empty() || package.spec(false).is_err() || package.spec(true).is_err()
    {
        None
    } else {
        Some(package)
    }
}

fn load_saved_at(path: &Path) -> Option<ThemePackage> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() > THEME_STATE_LIMIT {
        storage::quarantine_corrupt(path);
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let envelope: SavedThemeEnvelope = match serde_json::from_slice(&bytes) {
        Ok(envelope) => envelope,
        Err(_) => {
            storage::quarantine_corrupt(path);
            return None;
        }
    };
    let invalid_version = envelope.schema_version != storage::STORAGE_SCHEMA_VERSION;
    let invalid_time =
        envelope.saved_at_unix > storage::unix_timestamp().saturating_add(24 * 60 * 60);
    if invalid_version || invalid_time {
        storage::quarantine_corrupt(path);
        return None;
    }
    match validate_saved_package(envelope.package) {
        Some(package) => Some(package),
        None => {
            storage::quarantine_corrupt(path);
            None
        }
    }
}

fn save_imported_at(path: &Path, package: &ThemePackage) -> Result<(), String> {
    validate_saved_package(package.clone()).ok_or_else(|| "主题包内容无效".to_string())?;
    let envelope = SavedThemeEnvelope {
        schema_version: storage::STORAGE_SCHEMA_VERSION,
        saved_at_unix: storage::unix_timestamp(),
        package: package.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(|error| error.to_string())?;
    storage::write_atomic(path, &bytes).map_err(|error| error.to_string())
}

pub fn load_saved() -> Option<ThemePackage> {
    let path = saved_theme_path();
    if let Some(parent) = path.parent() {
        storage::cleanup_sidecars(parent);
    }
    if path.exists() {
        return load_saved_at(&path);
    }

    // One-time migration from releases that stored the raw package in the temp directory.
    let legacy = legacy_theme_path();
    let text = std::fs::read_to_string(&legacy).ok()?;
    let package = match ThemePackage::from_json(&text) {
        Ok(package) => package,
        Err(_) => {
            let _ = std::fs::remove_file(legacy);
            return None;
        }
    };
    if save_imported(&package).is_ok() {
        let _ = std::fs::remove_file(legacy);
    }
    Some(package)
}

pub fn save_imported(package: &ThemePackage) -> Result<(), String> {
    save_imported_at(&saved_theme_path(), package)
}

pub fn clear_saved() {
    let _ = std::fs::remove_file(saved_theme_path());
    let _ = std::fs::remove_file(legacy_theme_path());
}

fn parse_color(value: &str) -> Result<Color32, String> {
    let hex = value.trim().trim_start_matches('#');
    let expanded;
    let hex = if hex.len() == 3 {
        expanded = hex.chars().flat_map(|c| [c, c]).collect::<String>();
        expanded.as_str()
    } else {
        hex
    };
    if hex.len() != 6 {
        return Err("必须使用 #RRGGBB 格式".to_string());
    }
    let n = u32::from_str_radix(hex, 16).map_err(|_| "必须使用十六进制颜色".to_string())?;
    Ok(Color32::from_rgb(
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    ))
}

fn color_hex(color: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
}

fn mix_color(background: Color32, foreground: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |bg: u8, fg: u8| (bg as f32 * (1.0 - amount) + fg as f32 * amount).round() as u8;
    Color32::from_rgb(
        mix(background.r(), foreground.r()),
        mix(background.g(), foreground.g()),
        mix(background.b(), foreground.b()),
    )
}

fn css_property(css: &str, selector: &str, property: &str) -> Option<String> {
    for block in css.split('}') {
        let Some((selectors, declarations)) = block.rsplit_once('{') else {
            continue;
        };
        let matches = selectors.split(',').any(|item| {
            item.split_whitespace()
                .last()
                .is_some_and(|last| last.eq_ignore_ascii_case(selector))
        });
        if !matches {
            continue;
        }
        for declaration in declarations.split(';') {
            let Some((name, value)) = declaration.split_once(':') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case(property) {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Builds the final text-size layer for browser preview and export.
///
/// A theme can use relative sizes (`em`, `%`) for most text while keeping a few
/// elements, commonly fenced code blocks, at an absolute `px` size. Changing
/// only `body` would leave those elements behind. Re-emit absolute font sizes
/// with the same theme ratio, then set the requested body size last.
pub fn font_size_override_css(css: &str, target_body_size: f32) -> String {
    let base_body_size = css_property(css, "body", "font-size")
        .as_deref()
        .and_then(absolute_px)
        .unwrap_or(15.0);
    let scale = target_body_size / base_body_size.max(1.0);
    let mut scaled_rules = String::new();
    append_scaled_font_rules(css, scale, &mut scaled_rules);
    scaled_rules.push_str(&format!(
        "body {{ font-size: {target_body_size:.2}px !important; }}"
    ));
    scaled_rules
}

fn append_scaled_font_rules(css: &str, scale: f32, output: &mut String) {
    let mut cursor = 0usize;
    while let Some(relative_open) = css[cursor..].find('{') {
        let open = cursor + relative_open;
        let raw_prelude = css[cursor..open].trim();
        let prelude = raw_prelude
            .rsplit_once(';')
            .map_or(raw_prelude, |(_, tail)| tail)
            .trim();
        let Some(close) = matching_brace(css, open) else {
            break;
        };
        let declarations = &css[open + 1..close];

        if prelude.starts_with("@media")
            || prelude.starts_with("@supports")
            || prelude.starts_with("@container")
            || prelude.starts_with("@layer")
        {
            let mut nested = String::new();
            append_scaled_font_rules(declarations, scale, &mut nested);
            if !nested.is_empty() {
                output.push_str(prelude);
                output.push('{');
                output.push_str(&nested);
                output.push('}');
            }
        } else if !prelude.is_empty()
            && !prelude.starts_with('@')
            && let Some(size) = declaration_value(declarations, "font-size")
                .as_deref()
                .and_then(absolute_px)
        {
            output.push_str(prelude);
            output.push_str(" { font-size: ");
            output.push_str(&format!("{:.2}px", size * scale));
            output.push_str(" !important; }");
        }

        cursor = close + 1;
    }
}

fn matching_brace(css: &str, open: usize) -> Option<usize> {
    let bytes = css.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate().skip(open) {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if byte == active_quote {
                quote = None;
            }
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            continue;
        }
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn declaration_value(declarations: &str, property: &str) -> Option<String> {
    declarations.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(property)
            .then(|| value.trim().to_string())
    })
}

fn absolute_px(value: &str) -> Option<f32> {
    let value = value.trim();
    let value = value.strip_suffix("!important").unwrap_or(value).trim();
    if value.len() < 3 || !value[value.len() - 2..].eq_ignore_ascii_case("px") {
        return None;
    }
    value[..value.len() - 2].trim().parse().ok()
}

fn css_color(css: &str, selector: &str, property: &str) -> Option<Color32> {
    let value = css_property(css, selector, property)?;
    if let Some(start) = value.find('#') {
        let hex: String = value[start + 1..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        return parse_color(&hex).ok();
    }
    match value.trim().to_ascii_lowercase().as_str() {
        "white" => Some(Color32::WHITE),
        "black" => Some(Color32::BLACK),
        _ => None,
    }
}

fn css_number(css: &str, selector: &str, property: &str) -> Option<f32> {
    let value = css_property(css, selector, property)?;
    let number: String = value
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '.')
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    number.parse().ok()
}

fn css_margin_side(css: &str, selector: &str, top: bool, font_size: f32) -> Option<f32> {
    let direct = if top { "margin-top" } else { "margin-bottom" };
    if let Some(value) = css_property(css, selector, direct) {
        return css_length_px(&value, font_size);
    }
    let margin = css_property(css, selector, "margin")?;
    let values: Vec<&str> = margin.split_whitespace().collect();
    let index = match (values.len(), top) {
        (1, _) => 0,
        (2, _) => 0,
        (3, true) | (4, true) => 0,
        (3, false) | (4, false) => 2,
        _ => return None,
    };
    css_length_px(values[index], font_size)
}

fn css_length_px(value: &str, font_size: f32) -> Option<f32> {
    let number: String = value
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '.')
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let value_number: f32 = number.parse().ok()?;
    if value.trim().to_ascii_lowercase().ends_with("em") {
        Some(value_number * font_size)
    } else {
        Some(value_number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析十六进制颜色() {
        assert_eq!(parse_color("#07C160"), Ok(Color32::from_rgb(7, 193, 96)));
        assert!(parse_color("red").is_err());
    }

    #[test]
    fn 示例主题包可以导入() {
        let package = ThemePackage::from_json(include_str!("../theme-package.example.json"))
            .expect("示例主题包应有效");
        assert_eq!(package.name, "My Theme");
        assert!(package.spec(false).is_ok());
        assert!(package.spec(true).is_ok());
    }

    #[test]
    fn typora_css可以转换为主题包() {
        let css = "body { color:#333; background:#fff; font-size:15px; } p { margin:0 0 20px; line-height:1.8; } li { margin-bottom:.75em; } h2 { border-left:6px solid #ff7e79; } pre { background:#f8f8f8; border-radius:4px; }";
        let package = ThemePackage::from_css("sspai", css).expect("CSS 应可转换");
        let light = package.spec(false).unwrap();
        assert_eq!(package.name, "sspai");
        assert_eq!(light.canvas, Color32::WHITE);
        assert_eq!(light.accent, Color32::from_rgb(255, 126, 121));
        assert_eq!(light.table_alt, Color32::from_rgb(255, 246, 246));
        assert_eq!(light.heading_style, HeadingStyle::Tech);
        assert_eq!(light.code_radius, 4);
        assert_eq!(light.block_spacing, 20.0);
        assert!((light.line_height - 1.8).abs() < 0.001);
        assert_eq!(light.list_item_spacing, 11.25);
        assert_eq!(package.recommended_body_font_size(), 15.0);
        assert_eq!(package.browser_css(), Some(css));
    }

    #[test]
    fn 内置少数派主题参数有效() {
        let package = ThemePackage::built_in_sspai();
        let light = package.spec(false).unwrap();
        let dark = package.spec(true).unwrap();
        assert_eq!(package.name, "少数派经典");
        assert_eq!(light.canvas, Color32::WHITE);
        assert_eq!(light.accent, Color32::from_rgb(255, 126, 121));
        assert_eq!(light.heading_style, HeadingStyle::Tech);
        assert_eq!(package.recommended_body_font_size(), 15.0);
        assert!(package.browser_css().unwrap().contains("padding: 10%"));
        assert_ne!(light.canvas, dark.canvas);
    }

    #[test]
    fn 保存主题携带版本并可恢复() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-theme-state-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("current.json");
        let package = ThemePackage::built_in_sspai();
        save_imported_at(&path, &package).unwrap();
        let loaded = load_saved_at(&path).expect("版本有效的主题应可恢复");
        assert_eq!(loaded.name, package.name);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"schema_version\": 1"));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn 损坏主题被隔离并降级到内置主题() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-theme-corrupt-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("current.json");
        std::fs::write(&path, b"not-json").unwrap();
        assert!(load_saved_at(&path).is_none());
        assert!(!path.exists());
        assert!(std::fs::read_dir(&directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("current.json.corrupt-")
        }));
        let _ = std::fs::remove_dir_all(directory);
    }
}
