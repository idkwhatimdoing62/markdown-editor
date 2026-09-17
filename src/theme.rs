//! 文档主题包：内置“专注写作”与用户配置目录中保存的主题。

use std::path::{Path, PathBuf};

use egui::Color32;
use serde::{Deserialize, Serialize};

use crate::storage;

/// 内置“专注写作”主题：素净配色、无装饰性色块，仅服务阅读与写作。
pub const BUILT_IN_FOCUS_CSS: &str = include_str!("../assets/focus.css");
const MAX_THEME_TEXT_BYTES: usize = 2 * 1024 * 1024;

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
    /// 原始 CSS，供 HTML 导出使用；编辑器混合编辑界面使用结构化 egui 样式。
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
    pub block_spacing: f32,
    pub line_height: f32,
    pub list_item_spacing: f32,
    pub code_radius: u8,
    pub code_padding: [i8; 2],
    pub table_spacing: [f32; 2],
}

impl ThemePackage {
    /// 内置“专注写作”主题：纸白画布、石墨文字、克制的蓝灰强调色，
    /// 标题不加装饰样式，表格与引用只保留最小可辨的层次。
    pub fn built_in_focused() -> Self {
        Self {
            name: "专注写作".to_string(),
            author: "Built-in".to_string(),
            css: Some(BUILT_IN_FOCUS_CSS.to_string()),
            light: ThemeColors {
                canvas: "#FCFCFA".to_string(),
                editor_canvas: "#FCFCFA".to_string(),
                panel: "#F6F6F4".to_string(),
                text: "#2B2B2B".to_string(),
                muted: "#8B8B88".to_string(),
                heading: "#1F1F1F".to_string(),
                accent: "#4A6FA5".to_string(),
                border: "#E4E4E1".to_string(),
                code_bg: "#F4F4F1".to_string(),
                quote_bg: "#FAFAF8".to_string(),
                table_alt: "#F7F7F5".to_string(),
            },
            dark: ThemeColors {
                canvas: "#191A1C".to_string(),
                editor_canvas: "#161718".to_string(),
                panel: "#161718".to_string(),
                text: "#D8D9D6".to_string(),
                muted: "#8B8D90".to_string(),
                heading: "#E6E6E3".to_string(),
                accent: "#86A8CC".to_string(),
                border: "#2B2D2F".to_string(),
                code_bg: "#1F2123".to_string(),
                quote_bg: "#1C1D1F".to_string(),
                table_alt: "#1D1F21".to_string(),
            },
            layout: ThemeLayout {
                heading_style: HeadingStyle::Plain,
                body_font_size: 16.5,
                content_width: 780.0,
                preview_padding: 40,
                block_spacing: 16.0,
                line_height: 1.7,
                list_item_spacing: 8.0,
                code_radius: 6,
                code_padding_x: 16,
                code_padding_y: 13,
                table_spacing_x: 20.0,
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

    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() > MAX_THEME_TEXT_BYTES {
            return Err("主题包 JSON 超过 2 MiB 限制".to_string());
        }
        let package: Self =
            serde_json::from_str(text).map_err(|e| format!("主题包 JSON 无效：{e}"))?;
        if package.name.trim().is_empty() {
            return Err("主题名称不能为空".to_string());
        }
        package.spec(false)?;
        package.spec(true)?;
        Ok(package)
    }

    /// 返回 HTML 导出使用的完整 CSS。没有携带 CSS 的旧包（例如已移除的
    /// sspai 时代主题）返回 `None`，由调用方回退到内置专注写作 CSS。
    pub fn browser_css(&self) -> Option<&str> {
        self.css.as_deref()
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

impl ThemeSpec {
    pub fn fallback(dark: bool) -> Self {
        let (canvas, editor, panel, text, muted, accent, border, code, quote, table) = if dark {
            (
                Color32::from_rgb(25, 26, 28),
                Color32::from_rgb(22, 23, 24),
                Color32::from_rgb(22, 23, 24),
                Color32::from_rgb(216, 217, 214),
                Color32::from_rgb(139, 141, 144),
                Color32::from_rgb(134, 168, 204),
                Color32::from_rgb(43, 45, 47),
                Color32::from_rgb(31, 33, 35),
                Color32::from_rgb(28, 29, 31),
                Color32::from_rgb(29, 31, 33),
            )
        } else {
            (
                Color32::from_rgb(252, 252, 250),
                Color32::from_rgb(252, 252, 250),
                Color32::from_rgb(246, 246, 244),
                Color32::from_rgb(43, 43, 43),
                Color32::from_rgb(139, 139, 136),
                Color32::from_rgb(74, 111, 165),
                Color32::from_rgb(228, 228, 225),
                Color32::from_rgb(244, 244, 241),
                Color32::from_rgb(250, 250, 248),
                Color32::from_rgb(247, 247, 245),
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
            content_width: 780.0,
            block_spacing: 16.0,
            line_height: 1.7,
            list_item_spacing: 8.0,
            code_radius: 6,
            code_padding: [16, 13],
            table_spacing: [20.0, 10.0],
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

pub(crate) fn mix_color(background: Color32, foreground: Color32, amount: f32) -> Color32 {
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
    append_scaled_font_rules_bounded(css, scale, output, 0);
}

/// Hard cap on nested at-rule recursion. Imported themes are attacker-adjacent
/// input (any file the user picks); without a cap a few thousand `@media {`
/// layers overflow the stack and take the app down.
const MAX_CSS_NESTING_DEPTH: usize = 32;

fn append_scaled_font_rules_bounded(css: &str, scale: f32, output: &mut String, depth: usize) {
    if depth >= MAX_CSS_NESTING_DEPTH {
        return;
    }
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
            append_scaled_font_rules_bounded(declarations, scale, &mut nested, depth + 1);
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
    // Compare the suffix byte-wise: a `"px"` check at `len - 2` would panic
    // when imported CSS ends a font-size value with a multi-byte character.
    let bytes = value.as_bytes();
    if bytes.len() < 3
        || !matches!(bytes[bytes.len() - 2], b'p' | b'P')
        || !matches!(bytes[bytes.len() - 1], b'x' | b'X')
    {
        return None;
    }
    value[..value.len() - 2].trim().parse().ok()
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
    fn 内置专注主题保持素净() {
        let package = ThemePackage::built_in_focused();
        let light = package.spec(false).unwrap();
        let dark = package.spec(true).unwrap();
        assert_eq!(package.name, "专注写作");
        assert_eq!(light.canvas, Color32::from_rgb(0xFC, 0xFC, 0xFA));
        assert_eq!(light.accent, Color32::from_rgb(0x4A, 0x6F, 0xA5));
        assert_eq!(light.heading_style, HeadingStyle::Plain);
        assert_eq!(package.recommended_body_font_size(), 16.5);
        assert!(package.browser_css().unwrap().contains("max-width: 780px"));
        assert_ne!(light.canvas, dark.canvas);
    }

    #[test]
    fn 字体像素值以多字节字符结尾不会panic() {
        // A `px` suffix check at `len - 2` used to slice inside a multi-byte
        // character when imported CSS ended a font-size value with it.
        let css = "body { font-size: 15px; } p { font-size: 15汉; }";
        let override_css = font_size_override_css(css, 17.0);
        assert!(override_css.contains("font-size: 17.00px !important;"));
        assert!(!override_css.contains("15汉"));
    }

    #[test]
    fn 深层嵌套媒体查询不会栈溢出() {
        let mut deep = "@media screen { ".repeat(4000);
        deep.push_str("body { font-size: 20px; } ");
        deep.push_str(&"}".repeat(4000));
        let css = format!("body {{ font-size: 15px; color:#333; background:#fff; }} {deep}");
        let override_css = font_size_override_css(&css, 17.0);
        // 超出深度上限的嵌套层被丢弃，但函数必须正常返回；
        // 顶层规则仍然缩放。
        assert!(override_css.contains("font-size: 17.00px !important;"));
        // 浅层嵌套依旧生效
        let shallow = "body { font-size: 15px; } @media print { p { font-size: 12px; } }";
        let scaled = font_size_override_css(shallow, 15.0);
        assert!(scaled.contains("font-size: 12.00px !important;"));
    }

    #[test]
    fn 保存主题携带版本并可恢复() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-theme-state-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("current.json");
        let package = ThemePackage::built_in_focused();
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

    #[test]
    fn 超大主题文本会被拒绝() {
        let oversized = " ".repeat(MAX_THEME_TEXT_BYTES + 1);
        assert!(
            ThemePackage::from_json(&oversized)
                .unwrap_err()
                .contains("超过 2 MiB")
        );
    }
}
