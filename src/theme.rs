//! 文档主题包：内置“专注写作”与用户配置目录中保存的主题。

use std::path::{Path, PathBuf};

use egui::Color32;
use serde::{Deserialize, Serialize};

use crate::storage;

/// 内置“专注写作”主题：素净配色、无装饰性色块，仅服务阅读与写作。
pub const BUILT_IN_FOCUS_CSS: &str = include_str!("../assets/focus.css");
// 上限只把关 from_json（导入入口移除后为 test-only）。
#[cfg(test)]
const MAX_THEME_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// 间距网格步长（design-system-ref 第 3 节：4/8/12/16/24/32/48/64）。
///
/// 主题包是外部输入，用户手填的 `block_spacing: 10` 这类网格外数值同样要收敛，
/// 否则换一个主题规范就失效。统一在 [`ThemePackage::spec`] 出口做一次吸附，
/// 比逐个字段加校验更难绕过，也让内置主题与第三方主题走同一条规则。
pub const SPACING_GRID: f32 = 4.0;

/// 把浮点间距吸附到最近的 4px 网格值。
pub(crate) fn snap_to_grid(value: f32) -> f32 {
    (value / SPACING_GRID).round() * SPACING_GRID
}

/// 整数间距（圆角、内边距）的 4px 吸附。
pub(crate) fn snap_to_grid_int(value: i32) -> i32 {
    let step = SPACING_GRID as i32;
    ((value as f32 / SPACING_GRID).round() as i32) * step
}

/// 正文一栏的目标字符宽上限（design-system-ref 第 4 节）。
///
/// 正文用等宽字族，JetBrains Mono 的字身宽恰好是 0.6em，因此可用字符数
/// 完全由 `content_width / (0.6 * body_font_size)` 决定；不再靠肉眼挑一个
/// 好看的像素值。
pub const MAX_BODY_CHARS: f32 = 70.0;

/// 等宽字族的字身宽（em）；JetBrains Mono 为 600/1000。
pub const MONO_ADVANCE_EM: f32 = 0.6;

/// 按目标字符宽与正文字号反推一栏的像素宽度。
pub fn content_width_for_chars(body_font_size: f32, chars: f32) -> f32 {
    chars * MONO_ADVANCE_EM * body_font_size
}

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
        let body_font_size = 16.0;
        Self {
            heading_style: HeadingStyle::Plain,
            body_font_size,
            // 70 字符 / 16px 正文反推得出，而不是随手挑一个像素值。
            content_width: content_width_for_chars(body_font_size, MAX_BODY_CHARS),
            preview_padding: 40,
            block_spacing: 12.0,
            line_height: 1.6,
            list_item_spacing: 8.0,
            code_radius: 8,
            code_padding_x: 16,
            code_padding_y: 12,
            table_spacing_x: 24.0,
            table_spacing_y: 12.0,
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
                // 对比度硬约束：在 #FCFCFA 画布上必须 ≥ 4.5:1。
                // 改为设计系统中性阶 L≈0.55 后实测 4.72:1；旧的 #8B8B88 只有 3.33:1。
                muted: "#6F7274".to_string(),
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
                body_font_size: 16.0,
                content_width: content_width_for_chars(16.0, MAX_BODY_CHARS),
                preview_padding: 40,
                block_spacing: 16.0,
                line_height: 1.6,
                list_item_spacing: 8.0,
                code_radius: 8,
                code_padding_x: 16,
                code_padding_y: 12,
                table_spacing_x: 20.0,
                table_spacing_y: 12.0,
            },
        }
    }

    pub fn recommended_body_font_size(&self) -> f32 {
        // 旧实现在作者为 “CSS Import” 且字号恰好等于默认值时把它微调到 16.5，
        // 用来挽救导入 CSS 偏紧的排版。默认正文现在就是规范值 16，这个基于
        // 浮点相等比较的哨兵已无意义，还会把字号顶到规范之外，故移除。
        self.layout.body_font_size.clamp(12.0, 22.0)
    }

    // 导入入口已移除：release 构建不再解析外部主题包 JSON，
    // 此校验入口仅供回归测试验证格式与大小上限。
    #[cfg(test)]
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
            // 间距一律吸附到 4px 网格；第三方主题包里的网格外数值也在这里收敛。
            // 原先按“是否等于默认值”判断作者未指定的哨兵已移除：默认值本身就
            // 在网格上且等于规范值，哨兵只会在换默认值时静默失效。
            block_spacing: snap_to_grid(self.layout.block_spacing.clamp(4.0, 32.0)),
            line_height: self.layout.line_height.clamp(1.0, 2.2),
            list_item_spacing: snap_to_grid(self.layout.list_item_spacing.clamp(0.0, 24.0)),
            code_radius: snap_to_grid_int(i32::from(self.layout.code_radius)).clamp(0, 24) as u8,
            code_padding: [
                snap_to_grid_int(i32::from(self.layout.code_padding_x)).clamp(4, 32) as i8,
                snap_to_grid_int(i32::from(self.layout.code_padding_y)).clamp(4, 28) as i8,
            ],
            table_spacing: [
                snap_to_grid(self.layout.table_spacing_x.clamp(8.0, 48.0)),
                snap_to_grid(self.layout.table_spacing_y.clamp(4.0, 28.0)),
            ],
        })
    }
}

impl ThemeSpec {
    /// 主题解析失败时的最后兜底：直接复用内置「专注写作」的 spec，
    /// 不再手抄一份色板（旧实现是内置配方的第二份真相，改主题必须两边同步）。
    pub fn fallback(dark: bool) -> Self {
        ThemePackage::built_in_focused()
            .spec(dark)
            .expect("内置主题的 spec 应始终有效")
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

// 导入入口已移除，保存路径仅供回归测试验证信封格式。
#[cfg(test)]
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
    load_saved_at(&path)
}

fn parse_color(value: &str) -> Result<Color32, String> {
    let value = value.trim();
    if let Some(arguments) = function_arguments(value, "oklch") {
        return parse_oklch(arguments);
    }
    let hex = value.trim_start_matches('#');
    let expanded;
    let hex = if hex.len() == 3 {
        expanded = hex.chars().flat_map(|c| [c, c]).collect::<String>();
        expanded.as_str()
    } else {
        hex
    };
    if hex.len() != 6 {
        return Err("颜色必须是 #RRGGBB 或 oklch(L C H)".to_string());
    }
    let n = u32::from_str_radix(hex, 16)
        .map_err(|_| "颜色必须是 #RRGGBB 或 oklch(L C H)".to_string())?;
    Ok(Color32::from_rgb(
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    ))
}

/// 取出 `name(...)` 形式的函数实参；函数名按 CSS 规则忽略大小写。
///
/// 用 `str::get` 而不是直接切片：`value[..name.len()]` 在多字节字符跨越该字节
/// 偏移时会 panic，而颜色值是用户可控输入。
fn function_arguments<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    let head = value.get(..name.len())?;
    if !head.eq_ignore_ascii_case(name) {
        return None;
    }
    value[name.len()..]
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')
}

fn invalid_oklch() -> String {
    "oklch 需要 oklch(亮度 彩度 色相)，例如 oklch(0.55 0.005 250)".to_string()
}

/// 解析 CSS `oklch(L C H)`。
///
/// 设计系统要求 token 用 OKLCH 定义，因此主题包格式必须能承载它；只支持
/// `#RRGGBB` 会让规范无法落地。透明度不在主题颜色模型里（`ThemeSpec` 全是
/// 不透明色），带 `/ alpha` 的写法直接报错，而不是静默丢掉 alpha。
fn parse_oklch(arguments: &str) -> Result<Color32, String> {
    if arguments.contains('/') {
        return Err("oklch 不支持透明度：主题颜色必须不透明".to_string());
    }
    let mut parts = arguments.split_whitespace();
    let lightness = parts.next().and_then(parse_oklch_lightness);
    let chroma = parts.next().and_then(|text| text.parse::<f32>().ok());
    let hue = parts.next().and_then(|text| text.parse::<f32>().ok());
    if parts.next().is_some() {
        return Err(invalid_oklch());
    }
    let (Some(lightness), Some(chroma), Some(hue)) = (lightness, chroma, hue) else {
        return Err(invalid_oklch());
    };
    if !(0.0..=1.0).contains(&lightness) || !(0.0..=1.0).contains(&chroma) {
        return Err("oklch 亮度与彩度必须在 0 到 1 之间".to_string());
    }
    Ok(oklch_to_rgb(lightness, chroma, hue))
}

/// CSS 允许亮度写成百分比（`55%`），也允许 `0.55`。
fn parse_oklch_lightness(text: &str) -> Option<f32> {
    let (digits, scale) = match text.strip_suffix('%') {
        Some(percent) => (percent, 0.01),
        None => (text, 1.0),
    };
    digits.trim().parse::<f32>().ok().map(|value| value * scale)
}

/// OKLCH → sRGB，采用 Björn Ottosson 的 OKLab 矩阵；超出色域的通道按 CSS 规则裁剪。
fn oklch_to_rgb(lightness: f32, chroma: f32, hue_degrees: f32) -> Color32 {
    let hue = hue_degrees.to_radians();
    let a = chroma * hue.cos();
    let b = chroma * hue.sin();

    let l_ = lightness + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = lightness - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = lightness - 0.089_484_18 * a - 1.291_485_5 * b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    let linear = [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ];
    let encode = |channel: f32| {
        let clamped = channel.clamp(0.0, 1.0);
        let gamma = if clamped <= 0.003_130_8 {
            12.92 * clamped
        } else {
            1.055 * clamped.powf(1.0 / 2.4) - 0.055
        };
        (gamma * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(encode(linear[0]), encode(linear[1]), encode(linear[2]))
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

/// 等宽标签（代码块语言名等）的最小字号，取自设计系统字阶的 mono 档。
pub const MONO_LABEL_SIZE: f32 = 13.0;

/// 代码块语言标签的前景色。
///
/// `muted` 直接画在 `code_bg` 上达不到正文 4.5:1 的硬约束（内置浅色主题实测
/// 4.40:1），因此向正文色靠拢一档。对任意主题包都只需两个已有 token，不必新增
/// 颜色字段，也就不会破坏主题包格式。
pub fn code_block_label_color(spec: &ThemeSpec) -> Color32 {
    mix_color(spec.muted, spec.text, 0.25)
}

/// 专注模式下非当前段落的正文字色。
///
/// 弱化不等于可以牺牲可读性：它仍是正文，必须保持 4.5:1。内置主题实测浅色
/// 5.52:1、深色 7.47:1，因此这组系数不能随手调大。
pub fn dimmed_text_color(spec: &ThemeSpec) -> Color32 {
    mix_color(spec.text, spec.muted, 0.62)
}

/// 专注模式下非当前段落的标题色，靠拢幅度比正文略小。
pub fn dimmed_heading_color(spec: &ThemeSpec) -> Color32 {
    mix_color(spec.heading, spec.muted, 0.55)
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
        // 主题 CSS 没写 body 字号时按规范正文 16px 作为缩放基准。
        .unwrap_or(16.0);
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
        // 示例是第三方作者直接抄写的对象，它自己必须先满足硬约束，
        // 否则示例一发布就把网格外间距和低对比度配色扩散出去。
        for dark in [false, true] {
            let spec = package.spec(dark).expect("示例主题应可解析");
            for (name, foreground) in [
                ("text", spec.text),
                ("muted", spec.muted),
                ("accent", spec.accent),
            ] {
                let ratio = contrast_ratio(foreground, spec.canvas);
                assert!(
                    ratio >= 4.5,
                    "示例主题的 {name} 对比度 {ratio:.2} 低于 4.5（dark={dark}）"
                );
            }
            for value in spacing_values(&spec) {
                assert_on_grid(value);
            }
        }
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
        assert_eq!(package.recommended_body_font_size(), 16.0);
        // 栏宽必须与 content_width 一致，否则编辑器与导出的行宽会分叉。
        let expected_width = content_width_for_chars(16.0, MAX_BODY_CHARS);
        assert!(
            package
                .browser_css()
                .unwrap()
                .contains(&format!("max-width: {expected_width:.0}px")),
            "内置主题 CSS 的 max-width 应等于 content_width"
        );
        assert_ne!(light.canvas, dark.canvas);
    }

    #[test]
    fn 内置主题的正文行宽不超过规范上限() {
        let package = ThemePackage::built_in_focused();
        let spec = package.spec(false).unwrap();
        let chars = spec.content_width / (MONO_ADVANCE_EM * package.recommended_body_font_size());
        assert!(
            chars <= MAX_BODY_CHARS + 0.5,
            "正文一行 {chars:.1} 字符，超过规范上限 {MAX_BODY_CHARS}"
        );
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

    #[test]
    fn 解析oklch颜色并保持中性阶() {
        // 设计系统中性阶 L≈0.55、色相 250 的浅色档位。
        let muted = parse_color("oklch(0.55 0.005 250)").expect("oklch 应可解析");
        for (actual, expected) in [(muted.r(), 0x6F), (muted.g(), 0x72), (muted.b(), 0x74)] {
            assert!(
                actual.abs_diff(expected) <= 2,
                "oklch(0.55 0.005 250) 的通道期望 ≈{expected:02X}，实际 {actual:02X}"
            );
        }
        // 亮度 1、彩度 0 就是白色；百分比亮度与大小写函数名都是合法 CSS。
        assert_eq!(
            parse_color("oklch(1 0 0)").unwrap(),
            Color32::from_rgb(255, 255, 255)
        );
        assert!(parse_color("OKLCH(55% 0.005 250)").is_ok());
        assert!(parse_color(" oklch(0.98 0 0) ").is_ok());
    }

    #[test]
    fn oklch的非法写法必须报错而不是静默降级() {
        assert!(parse_color("oklch(0.5 0.1)").is_err(), "缺少色相");
        assert!(parse_color("oklch(1.5 0.01 250)").is_err(), "亮度越界");
        assert!(
            parse_color("oklch(0.5 0.01 250 / 0.5)").is_err(),
            "主题颜色必须不透明"
        );
        assert!(parse_color("oklch()").is_err());
        assert!(parse_color("oklch").is_err());
    }

    #[test]
    fn 非ascii颜色值不会panic() {
        // `value[..name.len()]` 直接切片会在多字节字符中间 panic。
        assert!(parse_color("霞鹜文楷色").is_err());
        assert!(parse_color("oklch霞").is_err());
    }

    #[test]
    fn 主题包可以用oklch定义颜色() {
        let json = r##"{
            "name": "OKLCH 主题",
            "light": {
                "canvas": "oklch(0.98 0.002 250)",
                "editor_canvas": "#FCFCFA",
                "panel": "#F6F6F4",
                "text": "oklch(0.25 0.005 250)",
                "muted": "oklch(0.55 0.005 250)",
                "heading": "#1F1F1F",
                "accent": "#4A6FA5",
                "border": "#E4E4E1",
                "code_bg": "#F4F4F1",
                "quote_bg": "#FAFAF8",
                "table_alt": "#F7F7F5"
            },
            "dark": {
                "canvas": "#191A1C",
                "editor_canvas": "#161718",
                "panel": "#161718",
                "text": "#D8D9D6",
                "muted": "#8B8D90",
                "heading": "#E6E6E3",
                "accent": "#86A8CC",
                "border": "#2B2D2F",
                "code_bg": "#1F2123",
                "quote_bg": "#1C1D1F",
                "table_alt": "#1D1F21"
            }
        }"##;
        let package = ThemePackage::from_json(json).expect("oklch 主题包应有效");
        let light = package.spec(false).unwrap();
        // OKLCH 与十六进制可以在同一个包里混用。
        assert_eq!(light.accent, Color32::from_rgb(0x4A, 0x6F, 0xA5));
        assert!(light.canvas.r() > 0xF0, "画布接近中性白");
        assert!(light.muted.b() >= light.muted.r(), "中性阶色相偏蓝");
    }

    #[test]
    fn 主题包里的网格外间距会被吸附到网格上() {
        let mut package = ThemePackage::built_in_focused();
        // 模拟第三方主题包手填的网格外数值。
        package.author = "第三方".to_string();
        package.layout.block_spacing = 10.0;
        package.layout.list_item_spacing = 6.0;
        package.layout.code_padding_x = 15;
        package.layout.code_padding_y = 13;
        package.layout.code_radius = 6;
        package.layout.table_spacing_x = 22.0;
        package.layout.table_spacing_y = 10.0;

        let spec = package.spec(false).expect("网格外数值应收敛而不是报错");
        for value in spacing_values(&spec) {
            assert_on_grid(value);
        }
        // 吸附取最近的网格点，方向可预期。
        assert_eq!(spec.block_spacing, 12.0);
        assert_eq!(spec.list_item_spacing, 8.0);
        assert_eq!(spec.code_padding, [16, 12]);
        assert_eq!(spec.code_radius, 8);
        assert_eq!(spec.table_spacing, [24.0, 12.0]);
    }

    #[test]
    fn 内置主题的间距落在四像素网格上() {
        for dark in [false, true] {
            let spec = ThemePackage::built_in_focused().spec(dark).unwrap();
            for value in spacing_values(&spec) {
                assert_on_grid(value);
            }
        }
    }

    #[test]
    fn 内置主题的文字对比度满足硬约束() {
        for dark in [false, true] {
            let spec = ThemePackage::built_in_focused().spec(dark).unwrap();
            for (name, foreground) in [
                ("text", spec.text),
                ("muted", spec.muted),
                ("heading", spec.heading),
                ("accent", spec.accent),
            ] {
                let ratio = contrast_ratio(foreground, spec.canvas);
                assert!(
                    ratio >= 4.5,
                    "{name} 在画布上的对比度 {ratio:.2} 低于 AA 的 4.5（dark={dark}）"
                );
            }
            // 旧的 muted 直接画在 code_bg 上只有 4.40:1，必须走加深后的标签色。
            let label_ratio = contrast_ratio(code_block_label_color(&spec), spec.code_bg);
            assert!(
                label_ratio >= 4.5,
                "代码块语言标签对比度 {label_ratio:.2} 低于 4.5（dark={dark}）"
            );
            // 专注模式弱化的是正文，不是装饰。
            for (name, foreground) in [
                ("专注模式正文", dimmed_text_color(&spec)),
                ("专注模式标题", dimmed_heading_color(&spec)),
            ] {
                let ratio = contrast_ratio(foreground, spec.canvas);
                assert!(
                    ratio >= 4.5,
                    "{name} 弱化后对比度 {ratio:.2} 低于 4.5（dark={dark}）"
                );
            }
        }
    }

    fn spacing_values(spec: &ThemeSpec) -> [f32; 7] {
        [
            spec.block_spacing,
            spec.list_item_spacing,
            f32::from(spec.code_padding[0]),
            f32::from(spec.code_padding[1]),
            f32::from(spec.code_radius),
            spec.table_spacing[0],
            spec.table_spacing[1],
        ]
    }

    /// 4px 网格断言：浮点比较留一个 epsilon 余量。
    fn assert_on_grid(value: f32) {
        let steps = value / SPACING_GRID;
        assert!(
            (steps - steps.round()).abs() < 1e-4,
            "{value} 不在 {SPACING_GRID}px 网格上"
        );
    }

    /// WCAG 2.x 相对亮度。
    fn relative_luminance(color: Color32) -> f32 {
        let channel = |value: u8| {
            let value = f32::from(value) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    fn contrast_ratio(first: Color32, second: Color32) -> f32 {
        let first = relative_luminance(first);
        let second = relative_luminance(second);
        let (lighter, darker) = if first > second {
            (first, second)
        } else {
            (second, first)
        };
        (lighter + 0.05) / (darker + 0.05)
    }
}
