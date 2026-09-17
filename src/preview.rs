//! 把块模型渲染到 egui 预览区。
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId, Stroke};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::markdown::{self, Block, Inline};
use crate::theme::{HeadingStyle, ThemeSpec};

/// 加粗使用独立字体族。egui 0.35 没有字重概念，粗体必须换字体族实现。
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// 斜体同理。egui 的 `TextFormat::italics` 目前只是元数据，不会选择或合成
/// 斜体字形，所以 `*强调*` 必须切换到内嵌的 JetBrains Mono Italic 字体族。
pub fn italic_family() -> FontFamily {
    FontFamily::Name("italic".into())
}

/// 粗斜体：`***粗斜体***` 或斜体环境中的 `**加粗**`。
pub fn bold_italic_family() -> FontFamily {
    FontFamily::Name("bold_italic".into())
}

/// 依据当前字体族推断套用斜体后应使用的字体族（保持粗斜体组合正确）。
fn italic_family_for(family: &FontFamily) -> FontFamily {
    match family {
        f if f == &bold_italic_family() => bold_italic_family(),
        f if f == &bold_family() => bold_italic_family(),
        _ => italic_family(),
    }
}

/// 依据当前字体族推断套用加粗后应使用的字体族。
fn bold_family_for(family: &FontFamily) -> FontFamily {
    match family {
        f if f == &bold_italic_family() => bold_italic_family(),
        f if f == &italic_family() => bold_italic_family(),
        _ => bold_family(),
    }
}

/// 中文标点专用字体族（霞鹜文楷优先，见 `app_font_definitions`）。
fn cjk_family_for(family: &FontFamily) -> FontFamily {
    match family {
        f if f == &bold_italic_family() => FontFamily::Name("cjk_bold_italic".into()),
        f if f == &bold_family() => FontFamily::Name("cjk_bold".into()),
        f if f == &italic_family() => FontFamily::Name("cjk_italic".into()),
        _ => FontFamily::Name("cjk".into()),
    }
}

/// JetBrains Mono 自带但中文排版需要全角呈现的标点。全角句读
/// （。，：等）JetBrains Mono 本来就没有，会自然回退霞鹜文楷，无需处理。
fn is_cjk_punct(ch: char) -> bool {
    matches!(
        ch,
        '\u{00B7}' | // · 间隔号
        '\u{2014}' | // — 破折号
        '\u{2026}' | // … 省略号
        '\u{2018}' | '\u{2019}' | // '' 单引号
        '\u{201C}' | '\u{201D}' // "" 双引号
    )
}

/// 把文本按"是否中文标点"切段后逐段回调，零分配。
fn for_each_script_run(text: &str, mut emit: impl FnMut(&str, bool)) {
    let mut start = 0usize;
    let mut cursor = 0usize;
    let mut run_cjk: Option<bool> = None;
    for ch in text.chars() {
        let cjk = is_cjk_punct(ch);
        match run_cjk {
            None => run_cjk = Some(cjk),
            Some(previous) if previous != cjk => {
                emit(&text[start..cursor], previous);
                start = cursor;
                run_cjk = Some(cjk);
            }
            _ => {}
        }
        cursor += ch.len_utf8();
    }
    if let Some(cjk) = run_cjk {
        emit(&text[start..], cjk);
    }
}

/// 追加一段文本到布局任务，中文标点切换到 CJK 字体族。
fn append_text_with_cjk_punct(job: &mut LayoutJob, text: &str, format: &TextFormat) {
    for_each_script_run(text, |run, cjk| {
        let mut format = format.clone();
        if cjk {
            format.font_id.family = cjk_family_for(&format.font_id.family);
        }
        job.append(run, 0.0, format);
    });
}

/// Small per-application cache for local preview images. Failed loads are
/// cached briefly to avoid disk I/O on every frame, then retried so an image
/// that is still being copied or generated can appear without reopening the tab.
#[derive(Default)]
pub struct ImageCache {
    entries: HashMap<PathBuf, CachedImage>,
    pending: HashSet<PathBuf>,
    decoded: Option<ImageChannel>,
    generation: u64,
}

struct CachedImage {
    texture: Option<egui::TextureHandle>,
    failed_at: Option<Instant>,
}

const IMAGE_RETRY_DELAY: Duration = Duration::from_secs(1);

// Keep malformed or unusually large local images from allocating an
// unbounded amount of memory during a preview frame. PNG is the only image
// format currently enabled by the application, so a modest file cap is enough
// to cover normal notes while rejecting accidental dump files.
const MAX_IMAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 4096;
const MAX_PENDING_IMAGE_DECODES: usize = 4;
type DecodedImage = (u64, PathBuf, Option<egui::ColorImage>);
type ImageChannel = (Sender<DecodedImage>, Receiver<DecodedImage>);

pub struct PreviewImages<'a> {
    pub base_directory: Option<&'a Path>,
    pub cache: &'a mut ImageCache,
}

#[derive(Default)]
pub struct BlockEditorOutput {
    pub clicked_block: Option<usize>,
    /// Character offset inside the clicked block, used to place the first caret.
    pub clicked_cursor: Option<usize>,
    pub changed: bool,
    /// The byte range currently owned by the active inline editor.
    pub edited_range: Option<std::ops::Range<usize>>,
    /// True when a culled block's real height differed from its cached
    /// estimate this frame; the caller repaints so spacers converge.
    pub viewport_unsettled: bool,
}

/// Documents above this block count render only blocks near the viewport.
pub const VIRTUALIZE_MIN_BLOCKS: usize = 200;

/// Per-frame viewport-culling state for long documents.
pub struct PreviewViewport<'a> {
    /// Visible band in y coordinates relative to `origin` (captured from
    /// `Ui::cursor` at the top of the scroll content).
    pub band: Range<f32>,
    /// Rendered height per top-level block, including its trailing spacing.
    /// `0.0` means never measured.
    pub heights: &'a mut Vec<f32>,
    /// Extra band rendered above and below `band`.
    pub margin: f32,
}

/// Count headings a block contributes to `RenderPosition`, in render order.
/// Used to keep heading indices (and the TOC jump target) correct while the
/// block is skipped.
fn heading_count(block: &Block) -> usize {
    match block {
        Block::Heading { .. } => 1,
        Block::List { items, .. } => items
            .iter()
            .flat_map(|item| item.iter())
            .map(heading_count)
            .sum(),
        Block::Quote(children) => children.iter().map(heading_count).sum(),
        _ => 0,
    }
}

/// Count tables a block contributes to `RenderPosition`, in render order.
fn table_count(block: &Block) -> usize {
    match block {
        Block::Table { .. } => 1,
        Block::List { items, .. } => items
            .iter()
            .flat_map(|item| item.iter())
            .map(table_count)
            .sum(),
        Block::Quote(children) => children.iter().map(table_count).sum(),
        _ => 0,
    }
}

/// Top-level block index whose render range contains the `target`-th heading
/// (document order), or `None`.
pub fn block_index_for_heading(blocks: &[Block], target: usize) -> Option<usize> {
    let mut seen = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        let count = heading_count(block);
        if target < seen + count {
            return Some(index);
        }
        seen += count;
    }
    None
}

/// Cheap height guess for a never-measured block: source line count times the
/// configured line height. Overestimates (list nesting, tables) converge once
/// the block scrolls into view.
fn estimate_block_height(source: &str, range: &Range<usize>, line_height_px: f32) -> f32 {
    let text = source.get(range.clone()).unwrap_or("");
    let lines = text
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    (lines as f32 * line_height_px + 12.0).clamp(24.0, 3000.0)
}

/// Return the minimum number of rows an editor needs for its current buffer.
///
/// `TextEdit::multiline` grows to fit wrapped content, so this only needs to
/// account for explicit newlines. Keeping the minimum dynamic avoids the
/// large blank area caused by a fixed row count on short paragraphs.
fn editor_desired_rows(text: &str) -> usize {
    text.as_bytes()
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count()
        .saturating_add(1)
}

/// Typora 式列表续写：识别一行开头的列表标记。
///
/// 返回 `(下一行应续写的标记, 本行是否只有标记)`。有序列表自动递增序号，
/// 任务列表续写为未勾选状态，缩进原样保留。
fn next_list_marker(line: &str) -> Option<(String, bool)> {
    let indent_len = line.len() - line.trim_start_matches(' ').len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];
    for token in ["- [ ] ", "- [x] ", "- [X] ", "- ", "* ", "+ "] {
        if let Some(content) = rest.strip_prefix(token) {
            let marker = if token.len() >= 5 && token.starts_with("- [") {
                format!("{indent}- [ ] ")
            } else {
                format!("{indent}{token}")
            };
            return Some((marker, content.trim().is_empty()));
        }
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0
        && digits < rest.len()
        && rest.as_bytes()[digits] == b'.'
        && rest[digits..].len() > 1
        && rest.as_bytes()[digits + 1] == b' '
        && let Ok(number) = rest[..digits].parse::<u64>()
    {
        let content = &rest[digits + 2..];
        return Some((
            format!("{indent}{}. ", number + 1),
            content.trim().is_empty(),
        ));
    }
    None
}

/// 段落编辑器中按下 Enter 后的列表续写。
///
/// egui 的 TextEdit 只会插入一个裸换行；这里在变更落盘前检测"焦点在编辑器
/// 内 + 本帧按过 Enter + 内容有变化"，为新的空行补上与上一行一致的列表
/// 标记（有序列表序号 +1），并让光标停在标记之后。再次 Enter 退出列表：
/// 仅含标记的行会被清空，交还原生段落。
fn maybe_continue_list(
    ui: &egui::Ui,
    buffer: &mut String,
    response: &egui::Response,
    changed: bool,
) {
    let enter_pressed =
        ui.input(|input| input.key_pressed(egui::Key::Enter) && !input.modifiers.shift);
    if !(changed && response.has_focus() && enter_pressed) {
        return;
    }
    let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), response.id) else {
        return;
    };
    let Some(cursor_range) = state.cursor.char_range() else {
        return;
    };
    let caret_char = usize::from(cursor_range.primary.index);
    let caret_byte = char_to_byte_index(buffer, caret_char);
    // 光标必须停在一行的开头（正是刚插入换行后的位置），才谈得上"新起一项"。
    let line_start = buffer[..caret_byte]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    if line_start != caret_byte || line_start == 0 {
        return;
    }
    let newline = line_start - 1;
    let previous_start = buffer[..newline]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let previous_line = &buffer[previous_start..newline];
    let previous_line_len = previous_line.chars().count();
    let Some((marker, marker_only)) = next_list_marker(previous_line) else {
        return;
    };
    let mut new_caret = caret_char;
    if marker_only {
        // 空列表项上再按一次 Enter：清掉整行标记，退出列表。
        buffer.replace_range(previous_start..line_start, "");
        new_caret -= previous_line_len + 1;
    } else {
        buffer.replace_range(caret_byte..caret_byte, &marker);
        new_caret += marker.chars().count();
    }
    let range = egui::text::CCursorRange::two(
        egui::text::CCursor::new(new_caret),
        egui::text::CCursor::new(new_caret),
    );
    state.cursor.set_char_range(Some(range));
    state.store(ui.ctx(), response.id);
}

/// 把字符索引转换为 UTF-8 字节索引（越界时返回缓冲区末尾）。
fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// Find the top-level block containing a byte-indexed search match.
///
/// Search results use character offsets while parser ranges use bytes; the
/// caller converts once per hit with [`byte_range_for_chars`] so this lookup
/// stays a plain interval scan.
pub fn block_index_for_search(
    block_ranges: &[std::ops::Range<usize>],
    search_range: &std::ops::Range<usize>,
) -> Option<usize> {
    block_ranges
        .iter()
        .position(|range| search_range.start < range.end && search_range.end > range.start)
}

/// Convert a character range into the byte offsets used by the parser.
///
/// The editor and `egui::CCursor` count characters while `ParsedDocument`
/// ranges count bytes; this is the single conversion point for search hits.
pub fn byte_range_for_chars(
    source: &str,
    chars: &std::ops::Range<usize>,
) -> std::ops::Range<usize> {
    let byte_at = |char_index: usize| {
        source
            .char_indices()
            .nth(char_index)
            .map_or(source.len(), |(byte, _)| byte)
    };
    byte_at(chars.start)..byte_at(chars.end)
}

/// Convert a byte range back into the character range `egui::CCursor` expects.
pub fn char_range_for_bytes(
    source: &str,
    bytes: &std::ops::Range<usize>,
) -> std::ops::Range<usize> {
    let start_byte = bytes.start.min(source.len());
    let end_byte = bytes.end.clamp(start_byte, source.len());
    let mut start = None;
    let mut end = None;
    for (char_index, (byte_index, _)) in source.char_indices().enumerate() {
        if start.is_none() && byte_index >= start_byte {
            start = Some(char_index);
        }
        if end.is_none() && byte_index >= end_byte {
            end = Some(char_index);
        }
        if start.is_some() && end.is_some() {
            break;
        }
    }
    let total = source.chars().count();
    start.unwrap_or(total)..end.unwrap_or(total)
}

/// Byte ranges of the paired inline delimiters (`**`, `*`, `_`, `~~`, `` ` ``)
/// that are dimmed while a block is edited, exactly like the block prefixes:
/// editing shows the markdown source, so delimiters stay readable. The bytes
/// remain in the layout, so cursor positions and search ranges continue to use
/// the original source offsets.
fn hidden_syntax_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut hidden = vec![false; text.len()];
    let mut mark = |start: usize, end: usize| {
        if start < end && end <= hidden.len() {
            hidden[start..end].fill(true);
        }
    };

    // Only PAIRED delimiters are dimmed. Unpaired punctuation keeps its normal
    // color so a malformed document remains easy to diagnose while it is being
    // edited. This covers strong/emphasis, strike-through and inline code.
    // Backtick positions are collected once so the code-span checks below are
    // binary searches instead of re-scanning the whole block per delimiter.
    let backticks = backtick_positions(text);
    for token in ["**", "__", "~~", "`", "*", "_"] {
        let mut cursor = 0;
        while let Some(relative) = text[cursor..].find(token) {
            let start = cursor + relative;
            let next_start = start + token.len();
            // Delimiters inside inline/fenced code are literal content. Keep
            // them visible even when they happen to form a pair themselves.
            if token != "`" && inside_code_span(&backticks, start) {
                cursor = next_start;
                continue;
            }
            // CommonMark does not treat underscores surrounded by letters as
            // emphasis delimiters (`foo_bar_baz`). Keep those identifiers
            // fully visible while still hiding `_斜体_` and similar forms.
            if token == "_"
                && text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|ch| ch.is_alphanumeric())
                && text[next_start..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_alphanumeric())
            {
                cursor = next_start;
                continue;
            }
            let mut candidate_cursor = next_start;
            let end = loop {
                let relative_end = match text[candidate_cursor..].find(token) {
                    Some(relative_end) => relative_end,
                    None => break None,
                };
                let end = candidate_cursor + relative_end;
                if token != "`" && inside_code_span(&backticks, end) {
                    candidate_cursor = end + token.len();
                    continue;
                }
                // An underscore between two word characters is part of an
                // identifier, not the closing delimiter of emphasis.
                if token == "_"
                    && text[..end]
                        .chars()
                        .next_back()
                        .is_some_and(|ch| ch.is_alphanumeric())
                    && text[end + token.len()..]
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_alphanumeric())
                {
                    candidate_cursor = end + token.len();
                    continue;
                }
                break Some(end);
            };
            let Some(end) = end else {
                break;
            };
            mark(start, next_start);
            mark(end, end + token.len());
            cursor = end + token.len();
        }
    }

    collect_ranges(&hidden)
}

/// Byte ranges of the block prefixes that stay readable while a block is being
/// edited: `#` heading markers, `-`/`*`/`+` and `1.` list markers, task
/// checkboxes, and `>` quote markers.
///
/// These are only dimmed rather than hidden, so the paragraph being written
/// still shows which marker it belongs to (the Typora behaviour). Rendered,
/// non-active blocks never reach this function, so the marker disappears there
/// together with the rest of the source syntax.
fn muted_syntax_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut muted = vec![false; text.len()];
    let mut mark = |start: usize, end: usize| {
        if start < end && end <= muted.len() {
            muted[start..end].fill(true);
        }
    };

    // Block prefixes are only syntax at the beginning of a line. Keep the
    // indentation visible so nested list structure remains apparent.
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let content_end = line_start + line.trim_end_matches('\n').len();
        let mut cursor = line_start;
        while cursor < content_end && text.as_bytes()[cursor] == b' ' {
            cursor += 1;
        }
        let bytes = text.as_bytes();
        if cursor < content_end && bytes[cursor] == b'>' {
            // 引用记号可以嵌套（`> > `），逐个弱化并让出记号后的空格。
            while cursor < content_end && bytes[cursor] == b'>' {
                let mut end = cursor + 1;
                if end < content_end && bytes[end] == b' ' {
                    end += 1;
                }
                mark(cursor, end);
                cursor = end;
                if cursor < content_end && bytes[cursor] == b' ' {
                    cursor += 1;
                }
            }
        } else if cursor < content_end && bytes[cursor] == b'#' {
            let mut end = cursor;
            while end < content_end && bytes[end] == b'#' {
                end += 1;
            }
            if end < content_end && bytes[end].is_ascii_whitespace() {
                mark(cursor, end + 1);
            }
        } else if cursor < content_end && matches!(bytes[cursor], b'-' | b'+' | b'*') {
            if cursor + 1 < content_end && bytes[cursor + 1].is_ascii_whitespace() {
                let mut end = cursor + 2;
                if end + 2 < content_end
                    && bytes[end] == b'['
                    && matches!(bytes[end + 1], b' ' | b'x' | b'X')
                    && bytes[end + 2] == b']'
                    && end + 3 < content_end
                    && bytes[end + 3].is_ascii_whitespace()
                {
                    end += 4;
                }
                mark(cursor, end);
            }
        } else {
            let mut end = cursor;
            while end < content_end && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > cursor
                && end + 1 < content_end
                && bytes[end] == b'.'
                && bytes[end + 1].is_ascii_whitespace()
            {
                mark(cursor, end + 2);
            }
        }
        line_start += line.len();
    }

    collect_ranges(&muted)
}

/// Merge a per-byte flag map into sorted, non-overlapping byte ranges.
fn collect_ranges(flags: &[bool]) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut range_start = None;
    for (index, flagged) in flags.iter().copied().enumerate() {
        match (range_start, flagged) {
            (None, true) => range_start = Some(index),
            (Some(start), false) => {
                ranges.push(start..index);
                range_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = range_start {
        ranges.push(start..flags.len());
    }
    ranges
}

/// Byte positions of every backtick in the block. The inline-delimiter pass
/// below decides "is this marker literal code?" by asking how many backticks
/// occur before it: an odd count means an unclosed `` ` `` opened a code span.
/// Collecting positions once turns that per-marker O(n) scan into one
/// `partition_point`, so hiding syntax stays linear in the block size.
fn backtick_positions(text: &str) -> Vec<usize> {
    text.bytes()
        .enumerate()
        .filter_map(|(index, byte)| (byte == b'`').then_some(index))
        .collect()
}

fn inside_code_span(backticks: &[usize], byte_index: usize) -> bool {
    backticks.partition_point(|position| *position < byte_index) % 2 == 1
}

/// 编辑态的语法排版：所有 markdown 语法（成对行内分隔符、块级记号）都以
/// 弱化色照常显示——编辑态所见即源码，渲染效果只属于阅读态的非活动块。
fn layout_source_with_quiet_syntax(
    text: &str,
    font_id: FontId,
    color: Color32,
    wrap_width: f32,
) -> LayoutJob {
    let mut quiet = hidden_syntax_ranges(text)
        .into_iter()
        .chain(muted_syntax_ranges(text))
        .collect::<Vec<_>>();
    quiet.sort_by_key(|range| range.start);
    let muted_color = color.gamma_multiply(0.45);
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    job.keep_trailing_whitespace = true;
    let mut cursor = 0;
    for range in quiet {
        // Overlapping markers keep the style of the range that starts first.
        if range.start < cursor || range.end > text.len() {
            continue;
        }
        if cursor < range.start {
            append_text_with_cjk_punct(
                &mut job,
                &text[cursor..range.start],
                &TextFormat {
                    font_id: font_id.clone(),
                    color,
                    ..Default::default()
                },
            );
        }
        job.append(
            &text[range.clone()],
            0.0,
            TextFormat {
                font_id: font_id.clone(),
                color: muted_color,
                ..Default::default()
            },
        );
        cursor = range.end;
    }
    if cursor < text.len() {
        append_text_with_cjk_punct(
            &mut job,
            &text[cursor..],
            &TextFormat {
                font_id,
                color,
                ..Default::default()
            },
        );
    }
    job
}

/// 编辑中的围栏代码块在源码缓冲区里的分段。
///
/// 所有分段按字节顺序首尾相接、恰好覆盖整个缓冲区：编辑器与点击落点映射
/// 都按"字节顺序拼接"还原光标坐标，围栏行在源码中的偏移保持原位。
struct CodeFenceLayout {
    /// 开头围栏整行（缩进、```` ``` ```` 记号与信息串）。
    open_line: Range<usize>,
    /// 两道围栏之间的内容（含首尾换行），正常行文。
    content: Range<usize>,
    /// 结尾围栏整行（直至缓冲区末尾），未闭合时为空。
    close_line: Option<Range<usize>>,
}

/// 识别围栏代码块缓冲区的围栏行。
///
/// 首行须以 3 个及以上的 ```` ` ```` 或 `~` 开头（允许行首缩进），其后为
/// 信息串；末行须为同一记号、不短于开头记号、其后只有空白的围栏行。
/// 不满足时返回 `None`（例如缩进代码块或尚未敲完的开头围栏）。
fn code_fence_layout(text: &str) -> Option<CodeFenceLayout> {
    let first_end = text.find('\n').unwrap_or(text.len());
    let first_line = &text[..first_end];
    let indent = first_line.len() - first_line.trim_start_matches(' ').len();
    let bytes = first_line.as_bytes();
    let marker = *bytes.get(indent)?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let mut run_end = indent;
    while run_end < first_line.len() && bytes[run_end] == marker {
        run_end += 1;
    }
    if run_end - indent < 3 {
        return None;
    }
    let stripped = text.strip_suffix('\n').unwrap_or(text);
    let stripped = stripped.strip_suffix('\r').unwrap_or(stripped);
    let close_line = if stripped.len() > first_end {
        let line_start = stripped.rfind('\n').map_or(0, |position| position + 1);
        let body = stripped[line_start..].trim_matches([' ', '\t']);
        (body.len() >= run_end - indent && body.bytes().all(|byte| byte == marker))
            .then_some(line_start..text.len())
    } else {
        None
    };
    Some(CodeFenceLayout {
        open_line: 0..first_end,
        content: first_end..close_line.as_ref().map_or(text.len(), |range| range.start),
        close_line,
    })
}

/// 编辑中的代码块的排版：围栏行（```` ``` ```` 记号与语言名）与块级标记
/// 一样以弱化色照常显示——编辑时语法可见；代码内容按字面文本渲染，不再
/// 套用行内标记的隐藏规则（代码里的 `#`、`- `、`**` 都是字面内容）。
/// 缓冲区字节原样按序进入布局，光标与搜索偏移仍是源码坐标。
fn layout_code_block_source(
    text: &str,
    font_id: FontId,
    color: Color32,
    wrap_width: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    job.keep_trailing_whitespace = true;
    let content_format = TextFormat {
        font_id: font_id.clone(),
        color,
        ..Default::default()
    };
    let Some(fence) = code_fence_layout(text) else {
        // 缩进代码块等无围栏形态：整段按字面内容排版。
        append_text_with_cjk_punct(&mut job, text, &content_format);
        return job;
    };
    let fence_format = TextFormat {
        font_id,
        color: color.gamma_multiply(0.45),
        ..Default::default()
    };
    for (range, format) in [
        (Some(fence.open_line), &fence_format),
        (Some(fence.content), &content_format),
        (fence.close_line, &fence_format),
    ] {
        if let Some(range) = range.filter(|range| !range.is_empty()) {
            append_text_with_cjk_punct(&mut job, &text[range], format);
        }
    }
    job
}

fn cursor_index_for_click(
    ui: &egui::Ui,
    source: &str,
    range: &std::ops::Range<usize>,
    block: &Block,
    body_size: f32,
    rect: egui::Rect,
    pointer: egui::Pos2,
) -> Option<usize> {
    let text = source.get(range.clone())?;
    let font = editor_font_for_block(block, body_size);
    let job = match block {
        Block::Code { .. } => layout_code_block_source(
            text,
            font,
            ui.visuals().widgets.inactive.text_color(),
            rect.width().max(1.0),
        ),
        _ => layout_source_with_quiet_syntax(
            text,
            font,
            ui.visuals().widgets.inactive.text_color(),
            rect.width().max(1.0),
        ),
    };
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let local = pointer - rect.min;
    Some(
        galley
            .cursor_from_pos(local)
            .index
            .0
            .min(text.chars().count()),
    )
}

struct RenderPosition {
    table_id: usize,
    heading_index: usize,
    heading_target: Option<usize>,
}

impl ImageCache {
    fn channel(&mut self) -> (&Sender<DecodedImage>, &Receiver<DecodedImage>) {
        if self.decoded.is_none() {
            let channel = mpsc::channel();
            self.decoded = Some(channel);
        }
        let (sender, receiver) = self.decoded.as_ref().expect("image channel initialized");
        (sender, receiver)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending.clear();
        self.generation = self.generation.wrapping_add(1);
        if let Some((_, receiver)) = self.decoded.as_ref() {
            while receiver.try_recv().is_ok() {}
        }
    }

    fn load(&mut self, ui: &egui::Ui, path: &Path) -> Option<egui::TextureHandle> {
        self.poll_decoded(ui);
        if let Some(entry) = self.entries.get(path) {
            if let Some(texture) = &entry.texture {
                return Some(texture.clone());
            }
            // 守卫已经证明 `failed_at` 是 `Some`，因此这里读一次即可：原来的
            // `.map(..).unwrap_or(IMAGE_RETRY_DELAY)` 永远不会执行。
            if let Some(failed_at) = entry.failed_at
                && failed_at.elapsed() < IMAGE_RETRY_DELAY
            {
                let remaining = IMAGE_RETRY_DELAY.saturating_sub(failed_at.elapsed());
                ui.ctx()
                    .request_repaint_after(remaining.min(Duration::from_millis(250)));
                return None;
            }
            self.entries.remove(path);
        }
        let path = path.to_path_buf();
        if self.pending.len() < MAX_PENDING_IMAGE_DECODES && self.pending.insert(path.clone()) {
            let sender = self.channel().0.clone();
            let generation = self.generation;
            thread::spawn(move || {
                let decoded = decode_image(&path);
                let _ = sender.send((generation, path, decoded));
            });
        }
        ui.ctx().request_repaint_after(Duration::from_millis(32));
        None
    }

    fn poll_decoded(&mut self, ui: &egui::Ui) {
        let Some((_, receiver)) = self.decoded.as_ref() else {
            return;
        };
        let mut completed = Vec::new();
        while let Ok((generation, path, image)) = receiver.try_recv() {
            if generation != self.generation {
                continue;
            }
            completed.push((path, image));
        }
        for (path, image) in completed {
            self.pending.remove(&path);
            let texture = image.map(|color| {
                ui.ctx().load_texture(
                    format!("markdown-image:{}", path.display()),
                    color,
                    egui::TextureOptions::LINEAR,
                )
            });
            self.entries.insert(
                path,
                CachedImage {
                    failed_at: texture.is_none().then_some(Instant::now()),
                    texture,
                },
            );
        }
    }
}

fn decode_image(path: &Path) -> Option<egui::ColorImage> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_IMAGE_FILE_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let image = image
        .thumbnail(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION)
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        image.as_raw(),
    ))
}

#[cfg(test)]
pub fn show_preview(ui: &mut egui::Ui, blocks: &[Block]) {
    show_preview_with_theme(ui, blocks, 15.5, &ThemeSpec::fallback(false));
}

#[cfg(test)]
pub fn show_preview_with_theme(
    ui: &mut egui::Ui,
    blocks: &[Block],
    body_size: f32,
    theme: &ThemeSpec,
) {
    let mut cache = ImageCache::default();
    show_preview_with_heading_target_and_images(
        ui,
        blocks,
        body_size,
        theme,
        None,
        &mut PreviewImages {
            base_directory: None,
            cache: &mut cache,
        },
    );
}

#[cfg(test)]
pub fn show_preview_with_heading_target_and_images(
    ui: &mut egui::Ui,
    blocks: &[Block],
    body_size: f32,
    theme: &ThemeSpec,
    heading_target: Option<usize>,
    images: &mut PreviewImages<'_>,
) -> Option<usize> {
    if blocks.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new("无内容").weak().size(18.0));
        });
        return None;
    }

    let mut position = RenderPosition {
        table_id: 0,
        heading_index: 0,
        heading_target,
    };
    let mut clicked_block = None;
    for (index, block) in blocks.iter().enumerate() {
        let rendered = ui.scope(|ui| {
            show_block(ui, block, body_size, theme, &mut position, images);
        });
        if ui
            .interact(
                rendered.response.rect,
                ui.make_persistent_id(("preview_block", index)),
                egui::Sense::click(),
            )
            .clicked()
        {
            clicked_block = Some(index);
        }
        ui.add_space(theme.block_spacing);
    }
    clicked_block
}

/// Render the document while replacing the block owned by the active editor
/// with source editing. The rest of the document keeps the reading layout.
///
/// `search_range` is a byte range (the same unit as `block_ranges`); it is
/// converted to the editor-local character range only for the block it hits.
/// `stale_blocks` marks a call whose `blocks`/`block_ranges` come from an older
/// parse than `source`: the source slice owned by the editor is then identified
/// by `active_block` instead of the (already grown) source range, so blocks
/// after the edited one are not hidden by stale offsets.
///
/// `viewport` additionally enables per-block height caching and culling for
/// long documents; without it every top-level block is laid out each frame.
#[allow(clippy::too_many_arguments)]
pub fn show_preview_with_block_editor_and_search(
    ui: &mut egui::Ui,
    blocks: &[Block],
    block_ranges: &[std::ops::Range<usize>],
    source: &mut String,
    body_size: f32,
    theme: &ThemeSpec,
    active_block: Option<usize>,
    active_range: Option<std::ops::Range<usize>>,
    initial_cursor: Option<usize>,
    request_focus: bool,
    search_range: Option<std::ops::Range<usize>>,
    scroll_to_search: bool,
    typewriter_mode: bool,
    dim_inactive: bool,
    stale_blocks: bool,
    heading_target: Option<usize>,
    images: &mut PreviewImages<'_>,
    mut viewport: Option<&mut PreviewViewport<'_>>,
) -> BlockEditorOutput {
    let mut output = BlockEditorOutput::default();
    let mut position = RenderPosition {
        table_id: 0,
        heading_index: 0,
        heading_target,
    };
    // 专注模式：除当前编辑块外全部向弱化色靠拢，视线自然锚定在写作位置。
    // 两个系数与 4.5:1 对比度约束绑定，见 `theme::dimmed_text_color`。
    let dim_colors = (dim_inactive && active_block.is_some()).then(|| {
        (
            crate::theme::dimmed_text_color(theme),
            crate::theme::dimmed_heading_color(theme),
        )
    });
    let mut dim_block = |ui: &mut egui::Ui,
                         index: usize,
                         block: &Block,
                         position: &mut RenderPosition| {
        if let Some((dim_text, dim_heading)) = dim_colors.filter(|_| Some(index) != active_block) {
            ui.visuals_mut().override_text_color = Some(dim_text);
            ui.visuals_mut().widgets.active.fg_stroke.color = dim_heading;
        }
        show_block(ui, block, body_size, theme, position, images);
    };
    let can_edit = block_ranges.len() == blocks.len();
    if !can_edit {
        // The fallback edits the whole source, so the search hit has to be
        // expressed in the editor's character coordinates rather than bytes.
        let search_chars = search_range
            .clone()
            .map(|range| char_range_for_bytes(source, &range));
        let desired_rows = editor_desired_rows(source);
        let edit = egui::TextEdit::multiline(source)
            .id(ui.make_persistent_id("document_source_fallback"))
            .font(egui::FontId::new(body_size, egui::FontFamily::Monospace))
            .frame(egui::Frame::NONE)
            .desired_width(f32::INFINITY)
            // Keep the fallback editor to the document's actual line count.
            // A fixed 40-row minimum made short documents look empty while
            // still not helping long documents (the surrounding scroll area
            // already handles overflow).
            .desired_rows(desired_rows);
        let editor_output = if let Some(range) = search_chars.clone() {
            let font_id = egui::FontId::new(body_size, egui::FontFamily::Monospace);
            let mut layouter =
                move |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
                    let job = layout_source_with_quiet_syntax(
                        buffer.as_str(),
                        font_id.clone(),
                        ui.visuals().widgets.inactive.text_color(),
                        wrap_width,
                    );
                    let mut galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                    egui::text_selection::visuals::paint_text_selection(
                        &mut galley,
                        ui.visuals(),
                        &egui::text::CCursorRange::two(
                            egui::text::CCursor::new(range.start),
                            egui::text::CCursor::new(range.end),
                        ),
                        None,
                    );
                    galley
                };
            edit.layouter(&mut layouter).show(ui)
        } else {
            edit.show(ui)
        };
        let response = editor_output.response;
        if request_focus {
            response.request_focus();
        }
        if scroll_to_search {
            if let Some(range) = search_chars {
                let local = editor_output
                    .galley
                    .pos_from_cursor(egui::text::CCursor::new(range.start));
                let screen = local.translate(
                    editor_output.galley_pos.to_vec2()
                        - egui::vec2(editor_output.galley.rect.left(), 0.0),
                );
                ui.scroll_to_rect(
                    screen.expand2(egui::vec2(24.0, 16.0)),
                    Some(egui::Align::Center),
                );
            } else {
                response.scroll_to_me(Some(egui::Align::Center));
            }
        }
        if typewriter_mode {
            response.scroll_to_me(Some(egui::Align::Center));
        }
        output.changed = response.changed();
        return output;
    }

    // Keep the edited source slice stable while the parser catches up. This
    // lets Enter create extra lines (or delete a block entirely) without
    // moving the editor to a different parsed block on the next frame.
    let had_stable_range = active_range.is_some();
    let mut edit_range =
        active_range.or_else(|| active_block.and_then(|index| block_ranges.get(index).cloned()));
    if edit_range.is_none() && blocks.is_empty() {
        edit_range = Some(0..source.len());
    }
    if let Some(range) = &mut edit_range {
        range.start = range.start.min(source.len());
        range.end = range.end.min(source.len()).max(range.start);
        if !had_stable_range {
            // Keep one boundary newline in the first edit session so the user
            // can delete it to merge blocks or replace it with a new break.
            while range.end.saturating_sub(range.start) > 1
                && source.as_bytes().get(range.end - 1) == Some(&b'\n')
                && source.as_bytes().get(range.end - 2) == Some(&b'\n')
            {
                range.end -= 1;
            }
        }
        if !source.is_char_boundary(range.start) || !source.is_char_boundary(range.end) {
            edit_range = None;
        }
    }
    let Some(edit_range) = edit_range else {
        // Invalid parser offsets are safer as read-only content than as a
        // potentially corrupting source replacement.
        // Browsing mode must virtualize too — it is the common path.
        let virtualize = viewport.is_some() && blocks.len() >= VIRTUALIZE_MIN_BLOCKS;
        let line_height_px = body_size * theme.line_height;
        let band = viewport
            .as_deref()
            .map(|vp| (vp.band.start - vp.margin, vp.band.end + vp.margin));
        let (band_start, band_end) = band.unwrap_or((f32::MIN, f32::MAX));
        // Force-render the block holding the TOC jump target so its
        // `scroll_to_me` fires even when it starts outside the band.
        let heading_anchor = if virtualize {
            position
                .heading_target
                .and_then(|target| block_index_for_heading(blocks, target))
        } else {
            None
        };
        for (index, block) in blocks.iter().enumerate() {
            let block_top = ui.cursor().min.y;
            let block_start = ui.cursor().min;
            let mut reserved = 0.0_f32;
            let skip = virtualize
                && Some(index) != heading_anchor
                && block_ranges.get(index).is_some()
                && {
                    // Measured height first: estimating rescans the block text
                    // on every frame it stays outside the band.
                    reserved = viewport
                        .as_deref()
                        .and_then(|vp| vp.heights.get(index).copied())
                        .filter(|height| *height > 0.0)
                        .unwrap_or_else(|| {
                            estimate_block_height(source, &block_ranges[index], line_height_px)
                                + theme.block_spacing
                        });
                    block_top + reserved < band_start || block_top > band_end
                };
            if skip {
                ui.add_space(reserved);
                if let Some(vp) = viewport.as_deref_mut()
                    && let Some(slot) = vp.heights.get_mut(index)
                {
                    *slot = reserved;
                }
                position.heading_index += heading_count(block);
                position.table_id += table_count(block);
                continue;
            }
            let rendered = ui.scope(|ui| {
                dim_block(ui, index, block, &mut position);
            });
            let block_rect = egui::Rect::from_min_max(block_start, ui.min_rect().max);
            let response = ui.interact(
                block_rect,
                ui.make_persistent_id(("preview_block", index)),
                egui::Sense::click(),
            );
            if response.clicked() {
                output.clicked_block = Some(index);
                output.clicked_cursor = response.interact_pointer_pos().and_then(|pointer| {
                    cursor_index_for_click(
                        ui,
                        source,
                        block_ranges.get(index)?,
                        block,
                        body_size,
                        rendered.response.rect,
                        pointer,
                    )
                });
            }
            ui.add_space(theme.block_spacing);
            if virtualize {
                let used = ui.cursor().min.y - block_top;
                let vp = viewport
                    .as_deref_mut()
                    .expect("virtualize implies viewport");
                if let Some(slot) = vp.heights.get_mut(index) {
                    if (used - *slot).abs() > 0.5 {
                        output.viewport_unsettled = true;
                    }
                    *slot = used;
                }
            }
        }
        return output;
    };
    output.edited_range = Some(edit_range.clone());

    let first_edit_index = blocks.iter().enumerate().find_map(|(index, _)| {
        let block_range = block_ranges.get(index)?;
        let overlaps = if edit_range.start == edit_range.end {
            block_range.end > edit_range.start
        } else {
            edit_range.start < block_range.end && edit_range.end > block_range.start
        };
        overlaps.then_some(index)
    });
    let mut editor_shown = false;

    let search_local_range = search_range.and_then(|range| {
        let start = range.start.max(edit_range.start);
        let end = range.end.min(edit_range.end);
        (start < end).then(|| {
            // Only the slice inside the edited block is measured, so the
            // conversion stays proportional to the block instead of the
            // document prefix.
            let local_start = source[edit_range.start..start].chars().count();
            let local_end = local_start + source[start..end].chars().count();
            local_start..local_end
        })
    });
    let editor_font = active_block
        .and_then(|index| blocks.get(index))
        .map(|block| editor_font_for_block(block, body_size))
        .unwrap_or_else(|| FontId::new(body_size, FontFamily::Proportional));
    // 只有代码块保留代码盒外框（它是编辑器载体，Typora 同样如此）；
    // 引用块等其余块编辑时回到朴素源码，渲染装饰只属于阅读态。
    let code_block_editor = matches!(
        active_block.and_then(|index| blocks.get(index)),
        Some(Block::Code { .. })
    );
    let show_editor = |ui: &mut egui::Ui, source: &mut String, output: &mut BlockEditorOutput| {
        let mut buffer = source[edit_range.clone()].to_string();
        let desired_rows = editor_desired_rows(&buffer);
        let edit = egui::TextEdit::multiline(&mut buffer)
            .id(ui.make_persistent_id(("block_editor", active_block.unwrap_or(usize::MAX))))
            .font(editor_font.clone())
            .frame(egui::Frame::NONE)
            .desired_width(f32::INFINITY);
        // Use the current buffer height as the minimum. This preserves
        // wrapped/multiline edits without reserving extra rows for every
        // one-line paragraph.
        let edit = edit.desired_rows(desired_rows);
        let font_id = editor_font.clone();
        let local_search_range = search_local_range.clone();
        let mut layouter = move |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
            let job = if code_block_editor {
                layout_code_block_source(
                    buffer.as_str(),
                    font_id.clone(),
                    ui.visuals().widgets.inactive.text_color(),
                    wrap_width,
                )
            } else {
                layout_source_with_quiet_syntax(
                    buffer.as_str(),
                    font_id.clone(),
                    ui.visuals().widgets.inactive.text_color(),
                    wrap_width,
                )
            };
            let mut galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
            if let Some(range) = local_search_range.clone() {
                egui::text_selection::visuals::paint_text_selection(
                    &mut galley,
                    ui.visuals(),
                    &egui::text::CCursorRange::two(
                        egui::text::CCursor::new(range.start),
                        egui::text::CCursor::new(range.end),
                    ),
                    None,
                );
            }
            galley
        };
        let editor_output = if code_block_editor {
            let frame = egui::Frame::new()
                .fill(theme.code_bg)
                .inner_margin(egui::Margin::symmetric(
                    theme.code_padding[0],
                    theme.code_padding[1],
                ))
                .corner_radius(theme.code_radius)
                .stroke(Stroke::new(1.0, theme.border));
            frame
                .show(ui, |ui| edit.layouter(&mut layouter).show(ui))
                .inner
        } else {
            edit.layouter(&mut layouter).show(ui)
        };
        let response = editor_output.response;
        let changed = response.changed();
        if request_focus {
            response.request_focus();
            if let Some(cursor) = initial_cursor
                && let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), response.id)
            {
                let cursor = egui::text::CCursor::new(cursor.min(buffer.chars().count()));
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(cursor, cursor)));
                state.store(ui.ctx(), response.id);
            }
        }
        // 代码内容里的 `- ` / `1. ` 是字面文本，Enter 不得续写列表标记。
        if !code_block_editor {
            maybe_continue_list(ui, &mut buffer, &response, changed);
        }
        if scroll_to_search {
            if let Some(range) = search_local_range.as_ref() {
                let local = editor_output
                    .galley
                    .pos_from_cursor(egui::text::CCursor::new(range.start));
                let screen = local.translate(
                    editor_output.galley_pos.to_vec2()
                        - egui::vec2(editor_output.galley.rect.left(), 0.0),
                );
                ui.scroll_to_rect(
                    screen.expand2(egui::vec2(24.0, 16.0)),
                    Some(egui::Align::Center),
                );
            } else {
                response.scroll_to_me(Some(egui::Align::Center));
            }
        }
        if typewriter_mode {
            response.scroll_to_me(Some(egui::Align::Center));
        }
        if changed {
            let start = edit_range.start;
            let mut replacement = edit_range.clone();
            if buffer.is_empty() {
                // Removing a whole block should also remove its separating
                // blank lines, so an empty paragraph never leaves ghost space.
                while replacement.end < source.len() {
                    let bytes = source.as_bytes();
                    if bytes[replacement.end] == b'\n' {
                        replacement.end += 1;
                    } else if bytes[replacement.end] == b'\r'
                        && bytes.get(replacement.end + 1) == Some(&b'\n')
                    {
                        replacement.end += 2;
                    } else {
                        break;
                    }
                }
            }
            source.replace_range(replacement, &buffer);
            output.changed = true;
            output.edited_range = Some(start..start + buffer.len());
        }
    };

    // Long documents lay out one viewport slice per frame. Skipped blocks
    // reserve their last measured height (or a cheap estimate) as pure space,
    // so the scrollbar and click mapping stay stable while widget cost drops
    // from O(blocks) to O(visible).
    let virtualize = viewport.is_some() && blocks.len() >= VIRTUALIZE_MIN_BLOCKS;
    let line_height_px = body_size * theme.line_height;
    let band = viewport
        .as_deref()
        .map(|vp| (vp.band.start - vp.margin, vp.band.end + vp.margin));
    let (band_start, band_end) = band.unwrap_or((f32::MIN, f32::MAX));
    // A TOC jump may point at a block that is currently off the band; force
    // its block to render so the existing `scroll_to_me` fires.
    let heading_anchor = if virtualize {
        position
            .heading_target
            .and_then(|target| block_index_for_heading(blocks, target))
    } else {
        None
    };

    for (index, block) in blocks.iter().enumerate() {
        // With a stale parse the ranges describe the previous source, so the
        // grown editor range must not claim the blocks that follow it: only the
        // block being edited is replaced by the source editor.
        let in_edit_range = if stale_blocks {
            Some(index) == active_block
        } else {
            block_ranges[index].start < edit_range.end && block_ranges[index].end > edit_range.start
        };
        let is_editor_anchor = !editor_shown && first_edit_index == Some(index);
        let block_top = ui.cursor().min.y;
        let mut reserved = 0.0_f32;
        let skip = virtualize && !is_editor_anchor && Some(index) != heading_anchor && {
            // A measured height is authoritative; only fall back to the
            // source estimate when the block was never laid out. Estimating
            // first would rescan every skipped block each frame.
            reserved = viewport
                .as_deref()
                .and_then(|vp| vp.heights.get(index).copied())
                .filter(|height| *height > 0.0)
                .unwrap_or_else(|| {
                    estimate_block_height(source, &block_ranges[index], line_height_px)
                        + theme.block_spacing
                });
            block_top + reserved < band_start || block_top > band_end
        };
        if skip {
            ui.add_space(reserved);
            if let Some(vp) = viewport.as_deref_mut()
                && let Some(slot) = vp.heights.get_mut(index)
            {
                *slot = reserved;
            }
            // Keep render-order counters correct across skipped blocks: the
            // table ids and heading indices (TOC target) must not shift.
            position.heading_index += heading_count(block);
            position.table_id += table_count(block);
        } else {
            if is_editor_anchor {
                show_editor(ui, source, &mut output);
                editor_shown = true;
            } else if in_edit_range {
                // The stable range owns every parsed block it covers. Render one
                // editor only, avoiding duplicate content after a multiline edit.
            } else {
                let rendered = ui.scope(|ui| {
                    dim_block(ui, index, block, &mut position);
                });
                let block_rect = rendered.response.rect;
                let response = ui.interact(
                    block_rect,
                    ui.make_persistent_id(("preview_block", index)),
                    egui::Sense::click(),
                );
                if response.clicked() {
                    output.clicked_block = Some(index);
                    output.clicked_cursor = response.interact_pointer_pos().and_then(|pointer| {
                        cursor_index_for_click(
                            ui,
                            source,
                            block_ranges.get(index)?,
                            block,
                            body_size,
                            block_rect,
                            pointer,
                        )
                    });
                }
            }
            ui.add_space(theme.block_spacing);
            if virtualize {
                let used = ui.cursor().min.y - block_top;
                let vp = viewport
                    .as_deref_mut()
                    .expect("virtualize implies viewport");
                if let Some(slot) = vp.heights.get_mut(index) {
                    if (used - *slot).abs() > 0.5 {
                        output.viewport_unsettled = true;
                    }
                    *slot = used;
                }
            }
        }
    }
    if !editor_shown {
        // Empty ranges at EOF have no block to anchor to.
        show_editor(ui, source, &mut output);
    }
    output
}

fn show_block(
    ui: &mut egui::Ui,
    block: &Block,
    body_size: f32,
    theme: &ThemeSpec,
    position: &mut RenderPosition,
    images: &mut PreviewImages<'_>,
) {
    match block {
        Block::Heading { level, inlines } => {
            let size = match level {
                1 => body_size * 1.94,
                2 => body_size * 1.48,
                3 => body_size * 1.23,
                4 => body_size * 1.10,
                _ => body_size,
            };
            ui.add_space(if *level == 1 { 10.0 } else { 16.0 });
            let response = ui.scope(|ui| show_heading(ui, inlines, size, *level, theme, images));
            if position.heading_target == Some(position.heading_index) {
                response.response.scroll_to_me(Some(egui::Align::Min));
            }
            position.heading_index += 1;
        }
        Block::Paragraph(inlines) => show_inlines_block(
            ui,
            inlines,
            body_size,
            false,
            true,
            theme.line_height,
            images,
        ),
        Block::List {
            ordered,
            start,
            items,
        } => {
            let mut idx = *start;
            for item in items {
                let task = task_marker(item);
                ui.horizontal_top(|ui| {
                    let marker = if *ordered {
                        format!("{}.", idx)
                    } else if let Some((checked, _)) = task {
                        // 任务项已经在块模型中被识别为语义状态，不再把 Markdown
                        // 标记原样带回阅读视图。轻量符号也比方括号更接近原生阅读体验。
                        if checked { "☑" } else { "☐" }.to_string()
                    } else {
                        "•".to_string()
                    };
                    ui.label(egui::RichText::new(marker).strong());
                    ui.vertical(|ui| {
                        if let Some((_, prefix)) = task {
                            if let Some(Block::Paragraph(inlines)) = item.first() {
                                show_inlines_block_without_prefix(
                                    ui,
                                    inlines,
                                    prefix,
                                    body_size,
                                    false,
                                    true,
                                    theme.line_height,
                                    images,
                                );
                                for block in item.iter().skip(1) {
                                    show_block(ui, block, body_size, theme, position, images);
                                }
                            } else {
                                for block in item {
                                    show_block(ui, block, body_size, theme, position, images);
                                }
                            }
                        } else {
                            for block in item {
                                show_block(ui, block, body_size, theme, position, images);
                            }
                        }
                    });
                });
                if *ordered {
                    idx += 1;
                }
                ui.add_space(theme.list_item_spacing);
            }
        }
        Block::Code { lang, text } => {
            egui::Frame::new()
                .fill(theme.code_bg)
                .inner_margin(egui::Margin::symmetric(
                    theme.code_padding[0],
                    theme.code_padding[1],
                ))
                .corner_radius(theme.code_radius)
                .stroke(Stroke::new(1.0, theme.border))
                .show(ui, |ui| {
                    ui.set_min_width((ui.available_width() - 32.0).max(120.0));
                    if !lang.is_empty() {
                        // 语言名是等宽标签：字号取 mono 档下限，颜色必须在
                        // code_bg 上达到 4.5:1（直接用 muted 只有 4.40:1）。
                        ui.label(
                            egui::RichText::new(lang.to_uppercase())
                                .color(crate::theme::code_block_label_color(theme))
                                .size(crate::theme::MONO_LABEL_SIZE),
                        );
                        ui.add_space(8.0);
                    }
                    ui.label(
                        egui::RichText::new(text)
                            .monospace()
                            .size((body_size - 2.0).max(11.0)),
                    );
                });
        }
        Block::Quote(blocks) => {
            let response = egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(12, 4))
                .fill(theme.quote_bg)
                .stroke(Stroke::NONE)
                .corner_radius(match theme.heading_style {
                    HeadingStyle::Plain => 5,
                    HeadingStyle::Card => 12,
                    HeadingStyle::Tech => 2,
                })
                .show(ui, |ui| {
                    for b in blocks {
                        show_block(ui, b, body_size, theme, position, images);
                    }
                });
            let rect = response.response.rect;
            let rule = egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.top()),
                egui::pos2(rect.left() + 3.0, rect.bottom()),
            );
            ui.painter().rect_filled(
                rule,
                egui::CornerRadius::same(1),
                theme.accent.gamma_multiply(0.7),
            );
        }
        Block::Table { headers, rows } => {
            let cols = headers
                .len()
                .max(rows.iter().map(|r| r.len()).max().unwrap_or(0))
                .max(1);
            ui.scope(|ui| {
                ui.visuals_mut().faint_bg_color = theme.table_alt;
                egui::Grid::new(ui.id().with(("md_table", position.table_id)))
                    .num_columns(cols)
                    .striped(true)
                    .min_col_width(90.0)
                    .spacing(theme.table_spacing)
                    .show(ui, |ui| {
                        for header in headers {
                            show_inlines_block(
                                ui,
                                header,
                                body_size - 1.0,
                                true,
                                false,
                                1.5,
                                images,
                            );
                        }
                        ui.end_row();
                        for row in rows {
                            for cell in row {
                                show_inlines_block(
                                    ui,
                                    cell,
                                    body_size - 1.0,
                                    false,
                                    false,
                                    1.5,
                                    images,
                                );
                            }
                            ui.end_row();
                        }
                    });
            });
            position.table_id += 1;
        }
        Block::Rule => {
            ui.separator();
        }
        Block::Raw(t) => {
            egui::Frame::new()
                .fill(theme.code_bg)
                .inner_margin(egui::Margin::symmetric(
                    theme.code_padding[0],
                    theme.code_padding[1],
                ))
                .corner_radius(theme.code_radius)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(t)
                            .monospace()
                            .size((body_size - 2.0).max(11.0)),
                    );
                });
        }
    }
}

fn show_heading(
    ui: &mut egui::Ui,
    inlines: &[Inline],
    size: f32,
    level: u8,
    theme: &ThemeSpec,
    images: &mut PreviewImages<'_>,
) {
    // Heading glyphs are shaped through `strong_text_color()`（读
    // `widgets.active.fg_stroke`），`override_text_color` 对它无效——所以标题
    // 颜色必须同时写入该槽位。从 `override_text_color` 出发还能让外层专注
    // 模式的置灰作用域自然传导到标题。
    let heading_color = ui.visuals().override_text_color.unwrap_or(theme.heading);
    ui.visuals_mut().widgets.active.fg_stroke.color = heading_color;
    match theme.heading_style {
        HeadingStyle::Plain => {
            ui.scope(|ui| {
                ui.visuals_mut().override_text_color = Some(theme.heading);
                show_inlines_block(ui, inlines, size, true, true, 1.25, images);
            });
            if level == 2 {
                ui.add_space(4.0);
                ui.separator();
            }
        }
        HeadingStyle::Card if level <= 2 => {
            egui::Frame::new()
                .fill(theme.quote_bg)
                .inner_margin(egui::Margin::symmetric(16, if level == 1 { 12 } else { 8 }))
                .corner_radius(if level == 1 { 12 } else { 8 })
                .stroke(Stroke::new(1.0, theme.border))
                .show(ui, |ui| {
                    ui.set_min_width((ui.available_width() - 32.0).max(120.0));
                    ui.visuals_mut().override_text_color = Some(theme.heading);
                    show_inlines_block(ui, inlines, size, true, true, 1.25, images);
                });
        }
        HeadingStyle::Card => {
            ui.horizontal(|ui| {
                ui.colored_label(theme.accent, "●");
                ui.visuals_mut().override_text_color = Some(theme.heading);
                show_inlines_block(ui, inlines, size, true, true, 1.3, images);
            });
        }
        HeadingStyle::Tech => {
            // Imported CSS themes often encode a technical heading as a
            // filled card with a marker. Native reading mode keeps the same
            // hierarchy while dropping the decorative box and arrow.
            ui.scope(|ui| {
                ui.visuals_mut().override_text_color = Some(if level <= 2 {
                    theme.heading
                } else {
                    theme.text
                });
                show_inlines_block(ui, inlines, size, true, true, 1.25, images);
            });
        }
    }
}

fn editor_font_for_block(block: &Block, body_size: f32) -> FontId {
    match block {
        Block::Heading { level, .. } => {
            let size = match level {
                1 => body_size * 1.94,
                2 => body_size * 1.48,
                3 => body_size * 1.23,
                4 => body_size * 1.10,
                _ => body_size,
            };
            FontId::new(size, bold_family())
        }
        Block::Code { .. } | Block::Raw(_) => FontId::new(body_size, FontFamily::Monospace),
        _ => FontId::new(body_size, FontFamily::Proportional),
    }
}

/// 记录行内链接的字符区间，用于精确的点击命中。
///
/// Links in a paragraph must open by click position, not "the first link in
/// the paragraph". A plain `Label` cannot report which part of its galley was
/// clicked, so the job builder records each link's character range while it
/// appends text; the click handler then re-lays-out the same job and maps the
/// pointer to a cursor position with `cursor_from_pos`.
#[derive(Default)]
struct InlineJobBuilder {
    char_pos: usize,
    links: Vec<(Range<usize>, String)>,
}

impl InlineJobBuilder {
    fn append(&mut self, job: &mut LayoutJob, text: &str, format: TextFormat) {
        self.char_pos += text.chars().count();
        job.append(text, 0.0, format);
    }

    fn take_links(&mut self) -> Vec<(Range<usize>, String)> {
        std::mem::take(&mut self.links)
    }
}

fn open_link_for_click(
    ui: &egui::Ui,
    job: &LayoutJob,
    links: &[(Range<usize>, String)],
    rect: egui::Rect,
    pointer: egui::Pos2,
    wrap_width: f32,
) {
    // Re-layout the identical job with the same wrap width the `Label` used
    // (`TextWrapMode::Wrap` lays out at `available_width`), so the cursor
    // mapping matches the rendered lines.
    let mut job = job.clone();
    job.wrap.max_width = wrap_width.max(1.0);
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let cursor = galley.cursor_from_pos(pointer - rect.min).index.0;
    if let Some((_, url)) = links.iter().find(|(range, _)| range.contains(&cursor))
        && markdown::is_safe_link_destination(url)
    {
        ui.ctx().open_url(egui::OpenUrl {
            url: url.clone(),
            new_tab: true,
        });
    }
}

/// 把一段行内内容渲染为一个可换行的标签。
fn show_inlines_block(
    ui: &mut egui::Ui,
    inlines: &[Inline],
    size: f32,
    strong: bool,
    clickable: bool,
    line_height: f32,
    images: &mut PreviewImages<'_>,
) {
    let family = if strong {
        bold_family()
    } else {
        FontFamily::Proportional
    };
    let base = TextFormat {
        font_id: FontId::new(size, family),
        color: if strong {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().text_color()
        },
        line_height: Some(size * line_height),
        ..Default::default()
    };
    if inlines
        .iter()
        .any(|inline| matches!(inline, Inline::Image { .. }))
    {
        show_inlines_with_images(ui, inlines, size, strong, clickable, line_height, images);
        return;
    }
    let mut builder = InlineJobBuilder::default();
    let code_bg = ui.visuals().extreme_bg_color;
    let job = inlines_to_job(
        inlines,
        &base,
        ui.visuals().hyperlink_color,
        code_bg,
        &mut builder,
    );
    let links = builder.take_links();
    let mut label = egui::Label::new(job.clone()).wrap();
    if clickable && !links.is_empty() {
        label = label.sense(egui::Sense::click());
    }
    let wrap_width = ui.available_width();
    let resp = ui.add(label);
    if clickable && !links.is_empty() && resp.hovered() {
        resp.clone().on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    if clickable
        && resp.clicked()
        && let Some(pointer) = resp.interact_pointer_pos()
    {
        open_link_for_click(ui, &job, &links, resp.rect, pointer, wrap_width);
    }
}

/// Render a task paragraph while hiding its Markdown checkbox prefix. The
/// source AST remains borrowed; only the layout job is built for this frame.
#[allow(clippy::too_many_arguments)]
fn show_inlines_block_without_prefix(
    ui: &mut egui::Ui,
    inlines: &[Inline],
    prefix: &str,
    size: f32,
    strong: bool,
    clickable: bool,
    line_height: f32,
    images: &mut PreviewImages<'_>,
) {
    if inlines
        .iter()
        .any(|inline| matches!(inline, Inline::Image { .. }))
    {
        show_inlines_with_images_without_prefix(
            ui,
            inlines,
            prefix,
            size,
            strong,
            clickable,
            line_height,
            images,
        );
        return;
    }
    let family = if strong {
        bold_family()
    } else {
        FontFamily::Proportional
    };
    let base = TextFormat {
        font_id: FontId::new(size, family),
        color: if strong {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().text_color()
        },
        line_height: Some(size * line_height),
        ..Default::default()
    };
    let mut builder = InlineJobBuilder::default();
    let mut job = LayoutJob::default();
    let mut prefix_pending = true;
    let code_bg = ui.visuals().extreme_bg_color;
    push_inlines_without_prefix(
        &mut job,
        inlines,
        &base,
        ui.visuals().hyperlink_color,
        code_bg,
        &mut builder,
        prefix,
        &mut prefix_pending,
    );
    let links = builder.take_links();
    let mut label = egui::Label::new(job.clone()).wrap();
    if clickable && !links.is_empty() {
        label = label.sense(egui::Sense::click());
    }
    let wrap_width = ui.available_width();
    let response = ui.add(label);
    if clickable && !links.is_empty() && response.hovered() {
        response
            .clone()
            .on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    if clickable
        && response.clicked()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        open_link_for_click(ui, &job, &links, response.rect, pointer, wrap_width);
    }
}

#[allow(clippy::too_many_arguments)]
fn show_inlines_with_images_without_prefix(
    ui: &mut egui::Ui,
    inlines: &[Inline],
    prefix: &str,
    size: f32,
    strong: bool,
    clickable: bool,
    line_height: f32,
    images: &mut PreviewImages<'_>,
) {
    ui.horizontal_wrapped(|ui| {
        let mut text_start = 0;
        for (index, inline) in inlines.iter().enumerate() {
            if let Inline::Image { url, alt } = inline {
                if text_start < index {
                    show_inlines_block_without_prefix(
                        ui,
                        &inlines[text_start..index],
                        prefix,
                        size,
                        strong,
                        clickable,
                        line_height,
                        images,
                    );
                }
                let path = resolve_image_path(url, images.base_directory);
                if let Some(path) = path
                    && let Some(texture) = images.cache.load(ui, &path)
                {
                    let width = texture.size_vec2().x.min(ui.available_width().max(80.0));
                    let height = width * texture.size_vec2().y / texture.size_vec2().x.max(1.0);
                    ui.add(
                        egui::Image::from_texture(&texture)
                            .fit_to_exact_size(egui::vec2(width, height))
                            .alt_text(alt),
                    );
                } else {
                    let mut f = egui::RichText::new(format!("[图片] {alt}"))
                        .size(size)
                        .weak();
                    if strong {
                        f = f.strong();
                    }
                    ui.label(f);
                }
                text_start = index + 1;
            }
        }
        if text_start < inlines.len() {
            show_inlines_block_without_prefix(
                ui,
                &inlines[text_start..],
                prefix,
                size,
                strong,
                clickable,
                line_height,
                images,
            );
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn push_inlines_without_prefix(
    job: &mut LayoutJob,
    inlines: &[Inline],
    base: &TextFormat,
    link_color: Color32,
    code_bg: Color32,
    builder: &mut InlineJobBuilder,
    prefix: &str,
    prefix_pending: &mut bool,
) {
    for inline in inlines {
        match inline {
            Inline::Text(text) => {
                let visible = if *prefix_pending {
                    *prefix_pending = false;
                    text.strip_prefix(prefix).unwrap_or(text)
                } else {
                    text
                };
                for_each_script_run(visible, |run, cjk| {
                    let mut format = base.clone();
                    if cjk {
                        format.font_id.family = cjk_family_for(&format.font_id.family);
                    }
                    builder.append(job, run, format);
                });
            }
            Inline::Emphasis(children) => {
                let mut format = base.clone();
                format.italics = true;
                format.font_id.family = italic_family_for(&format.font_id.family);
                push_inlines_without_prefix(
                    job,
                    children,
                    &format,
                    link_color,
                    code_bg,
                    builder,
                    prefix,
                    prefix_pending,
                );
            }
            Inline::Strong(children) => {
                let mut format = base.clone();
                format.font_id.family = bold_family_for(&format.font_id.family);
                push_inlines_without_prefix(
                    job,
                    children,
                    &format,
                    link_color,
                    code_bg,
                    builder,
                    prefix,
                    prefix_pending,
                );
            }
            Inline::Strikethrough(children) => {
                let mut format = base.clone();
                format.strikethrough = Stroke::new(1.0, format.color);
                push_inlines_without_prefix(
                    job,
                    children,
                    &format,
                    link_color,
                    code_bg,
                    builder,
                    prefix,
                    prefix_pending,
                );
            }
            Inline::Code(code) => {
                let mut format = base.clone();
                format.font_id.family = FontFamily::Monospace;
                format.background = code_bg;
                builder.append(job, code, format);
            }
            Inline::Link { url, children, .. } => {
                let link_start = builder.char_pos;
                let mut format = base.clone();
                format.underline = Stroke::new(1.0, format.color);
                format.color = link_color;
                push_inlines_without_prefix(
                    job,
                    children,
                    &format,
                    link_color,
                    code_bg,
                    builder,
                    prefix,
                    prefix_pending,
                );
                builder
                    .links
                    .push((link_start..builder.char_pos, url.clone()));
            }
            Inline::Image { alt, .. } => {
                let mut format = base.clone();
                format.color = format.color.gamma_multiply(0.6);
                builder.append(job, &format!("[图片] {alt}"), format);
            }
            Inline::SoftBreak => builder.append(job, " ", base.clone()),
            Inline::HardBreak => builder.append(job, "\n", base.clone()),
        }
    }
}

fn show_inlines_with_images(
    ui: &mut egui::Ui,
    inlines: &[Inline],
    size: f32,
    strong: bool,
    clickable: bool,
    line_height: f32,
    images: &mut PreviewImages<'_>,
) {
    ui.horizontal_wrapped(|ui| {
        // Render text as borrowed slices around images. The old implementation
        // cloned every non-image Inline into a temporary Vec on each frame.
        let mut text_start = 0;
        for (index, inline) in inlines.iter().enumerate() {
            if let Inline::Image { url, alt } = inline {
                if text_start < index {
                    show_inlines_block(
                        ui,
                        &inlines[text_start..index],
                        size,
                        strong,
                        clickable,
                        line_height,
                        images,
                    );
                }
                let path = resolve_image_path(url, images.base_directory);
                if let Some(path) = path
                    && let Some(texture) = images.cache.load(ui, &path)
                {
                    let width = texture.size_vec2().x.min(ui.available_width().max(80.0));
                    let height = width * texture.size_vec2().y / texture.size_vec2().x.max(1.0);
                    ui.add(
                        egui::Image::from_texture(&texture)
                            .fit_to_exact_size(egui::vec2(width, height))
                            .alt_text(alt),
                    );
                } else {
                    let mut f = egui::RichText::new(format!("[图片] {alt}"))
                        .size(size)
                        .weak();
                    if strong {
                        f = f.strong();
                    }
                    ui.label(f);
                }
                text_start = index + 1;
            }
        }
        if text_start < inlines.len() {
            show_inlines_block(
                ui,
                &inlines[text_start..],
                size,
                strong,
                clickable,
                line_height,
                images,
            );
        }
    });
}

fn resolve_image_path(url: &str, base: Option<&Path>) -> Option<PathBuf> {
    if url.is_empty() || url.starts_with('#') || url.contains("://") || url.starts_with("data:") {
        return None;
    }
    let clean = percent_decode(url.split(['#', '?']).next().unwrap_or(url));
    let path = Path::new(&clean);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base?.join(path)
    };
    Some(resolved)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            output.push(high * 16 + low);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn inlines_to_job(
    inlines: &[Inline],
    base: &TextFormat,
    link_color: Color32,
    code_bg: Color32,
    builder: &mut InlineJobBuilder,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    push_inlines(&mut job, inlines, base, link_color, code_bg, builder);
    job
}

#[allow(clippy::too_many_arguments)]
fn push_inlines(
    job: &mut LayoutJob,
    inlines: &[Inline],
    base: &TextFormat,
    link_color: Color32,
    code_bg: Color32,
    builder: &mut InlineJobBuilder,
) {
    for inline in inlines {
        match inline {
            Inline::Text(t) => {
                for_each_script_run(t, |run, cjk| {
                    let mut format = base.clone();
                    if cjk {
                        format.font_id.family = cjk_family_for(&format.font_id.family);
                    }
                    builder.append(job, run, format);
                });
            }
            Inline::Emphasis(children) => {
                let mut f = base.clone();
                f.italics = true;
                f.font_id.family = italic_family_for(&f.font_id.family);
                push_inlines(job, children, &f, link_color, code_bg, builder);
            }
            Inline::Strong(children) => {
                let mut f = base.clone();
                f.font_id.family = bold_family_for(&f.font_id.family);
                push_inlines(job, children, &f, link_color, code_bg, builder);
            }
            Inline::Strikethrough(children) => {
                let mut f = base.clone();
                f.strikethrough = Stroke::new(1.0, f.color);
                push_inlines(job, children, &f, link_color, code_bg, builder);
            }
            Inline::Code(c) => {
                let mut f = base.clone();
                f.font_id.family = FontFamily::Monospace;
                // 行内代码加一块底色（入口处取自 extreme_bg_color，即主题的
                // code_bg），与围栏代码块视觉呼应。
                f.background = code_bg;
                builder.append(job, c, f);
            }
            Inline::Link { url, children, .. } => {
                let link_start = builder.char_pos;
                let mut f = base.clone();
                f.underline = Stroke::new(1.0, f.color);
                f.color = link_color;
                push_inlines(job, children, &f, link_color, code_bg, builder);
                builder
                    .links
                    .push((link_start..builder.char_pos, url.clone()));
            }
            Inline::Image { alt, .. } => {
                let mut f = base.clone();
                f.color = f.color.gamma_multiply(0.6);
                builder.append(job, &format!("[图片] {alt}"), f);
            }
            Inline::SoftBreak => builder.append(job, " ", base.clone()),
            Inline::HardBreak => builder.append(job, "\n", base.clone()),
        }
    }
}

/// 任务列表项的标记。只返回前缀元数据，渲染时借用原始块，避免每帧复制整个列表项 AST。
fn task_marker(item: &[Block]) -> Option<(bool, &'static str)> {
    let Block::Paragraph(inlines) = item.first()? else {
        return None;
    };
    let Inline::Text(text) = inlines.first()? else {
        return None;
    };
    if text.starts_with("[x] ") || text.starts_with("[X] ") {
        Some((true, "[x] "))
    } else if text.starts_with("[ ] ") {
        Some((false, "[ ] "))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Color32, FontFamily, FontId, TextFormat};
    use crate::markdown::parse;
    use crate::preview::bold_family;

    struct TextRun {
        x: f32,
        y: f32,
        text: String,
    }

    fn render_runs(markdown: &str) -> Vec<TextRun> {
        let blocks = parse(markdown);
        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(620.0, 1000.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                crate::preview::show_preview(ui, &blocks);
            });
        });
        let mut runs = Vec::new();
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(t) = &shape.shape {
                for placed in &t.galley.rows {
                    let text: String = placed.row.glyphs.iter().map(|g| g.chr).collect();
                    runs.push(TextRun {
                        x: t.pos.x + placed.pos.x,
                        y: t.pos.y + placed.pos.y,
                        text,
                    });
                }
            }
        }
        runs
    }

    #[test]
    fn 列表渲染布局正确() {
        let runs = render_runs(
            "- item one\n- item two\n\n1. first ordered\n2. second ordered\n\n- level one\n  - level two nested\n\n- [ ] unchecked\n- [x] checked\n",
        );

        let get = |t: &str| runs.iter().find(|r| r.text.trim() == t).expect(t);

        let bullet = get("•");
        let item = get("item one");
        assert!(item.x > bullet.x);
        assert!((item.y - bullet.y).abs() < 2.0, "圆点应与正文同行");

        let n1 = get("1.");
        let first = get("first ordered");
        assert!(first.x > n1.x);

        let outer = get("level one");
        let inner = get("level two nested");
        assert!(inner.x > outer.x, "嵌套列表应缩进");

        assert!(get("☑").x < get("checked").x);
        assert!(get("☐").x < get("unchecked").x);
        assert!(
            !runs.iter().any(|r| r.text.trim_start().starts_with("• [")),
            "任务标记不应跟在圆点后面"
        );
    }

    #[test]
    fn 大写任务标记使用语义符号() {
        let runs = render_runs("- [X] done\n- [ ] todo\n");
        let text: Vec<&str> = runs.iter().map(|run| run.text.trim()).collect();
        assert!(text.contains(&"☑"));
        assert!(text.contains(&"☐"));
        assert!(text.contains(&"done"));
        assert!(text.contains(&"todo"));
        assert!(!text.contains(&"[X]"));
    }

    #[test]
    fn 含图片的任务项隐藏复选框语法() {
        let runs = render_runs("- [ ] ![截图](missing-task-image.png)\n");
        let joined: String = runs.iter().map(|run| run.text.as_str()).collect();
        assert!(joined.contains("☐"), "任务应显示语义复选框");
        assert!(joined.contains("[图片] 截图"), "图片占位文本应保留替代文字");
        assert!(
            !joined.contains("[ ]"),
            "图片任务不应显示 Markdown 复选框前缀"
        );
    }

    #[test]
    fn 表格表头渲染在同一行() {
        let runs = render_runs("| 功能 | 状态 |\n| --- | --- |\n| 编辑 | 可用 |\n");
        let cell = |t: &str| runs.iter().find(|r| r.text.trim() == t).expect(t);
        let h1 = cell("功能");
        let h2 = cell("状态");
        assert!(h1.x < h2.x, "表头单元格应横向排列");
        assert!((h1.y - h2.y).abs() < 2.0, "表头单元格应在同一行");
        let row1 = cell("编辑");
        assert!((row1.y - h1.y).abs() > 10.0, "数据行应在表头下方");
    }

    #[test]
    fn 段落内多个行内片段不重叠() {
        let runs = render_runs("这是**加粗**文本。");
        // 整段是一个 galley：行内片段拼进同一行文本，而不是各自叠在原点
        let para = runs
            .iter()
            .find(|r| r.text.contains("加粗"))
            .expect("应有段落文本");
        assert_eq!(para.text, "这是加粗文本。", "片段应拼接为一段");
    }

    #[test]
    fn 加粗文本使用粗体字体() {
        if crate::export::bold_latin_font_bytes().is_none() {
            eprintln!("跳过：未找到粗体拉丁字体");
            return;
        }
        let blocks = parse("plain b **strong b** plain b");
        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(620.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                crate::preview::show_preview(ui, &blocks);
            });
        });
        let mut advances = Vec::new();
        let mut has_bold_family = false;
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(t) = &shape.shape {
                has_bold_family |= t
                    .galley
                    .job
                    .sections
                    .iter()
                    .any(|section| section.format.font_id.family == bold_family());
                for placed in &t.galley.rows {
                    for g in &placed.row.glyphs {
                        if g.chr == 'b' {
                            advances.push(g.advance_width);
                        }
                    }
                }
            }
        }
        assert_eq!(advances.len(), 3, "应有 3 个 b 字形");
        assert!(has_bold_family, "加粗片段应使用独立的粗体字体族");
    }

    #[test]
    fn 列表项开头的加粗文字完整显示() {
        let md = "1. **目标**：系统要帮谁省掉什么麻烦；\n2. **范围**：当前范围包括哪些事。\n";
        let runs = render_runs(md);
        let joined: String = runs.iter().map(|r| r.text.clone()).collect();
        assert!(
            joined.contains("目标"),
            "目标应出现在预览中，实际 {joined:?}"
        );
        assert!(
            joined.contains("范围"),
            "范围应出现在预览中，实际 {joined:?}"
        );
        assert!(
            joined.contains("目标：系统要帮谁省掉什么麻烦；"),
            "列表项文字应完整，实际 {joined:?}"
        );
    }

    #[test]
    fn 图片路径支持相对目录和百分号空格() {
        let base = std::path::Path::new(r"C:\notes");
        assert_eq!(
            super::resolve_image_path("assets/my%20image.png", Some(base)),
            Some(std::path::PathBuf::from(r"C:\notes\assets\my image.png"))
        );
    }

    #[test]
    fn 搜索字节区间映射到包含它的顶层块() {
        let source = "标题\n\n正文搜索\n";
        let ranges = [0..8, 8..23];
        let bytes = super::byte_range_for_chars(source, &(4..8));
        assert_eq!(super::block_index_for_search(&ranges, &bytes), Some(1));
    }

    #[test]
    fn 字符与字节区间可以互相换算() {
        let source = "标题\n\n正文搜索\n";
        let bytes = super::byte_range_for_chars(source, &(4..8));
        assert_eq!(&source[bytes.clone()], "正文搜索");
        assert_eq!(super::char_range_for_bytes(source, &bytes), 4..8);
    }

    #[test]
    fn 编辑时隐藏单星号和下划线强调标记() {
        let source = "普通 *强调* 与 _斜体_，以及 * 列表项";
        let hidden = super::hidden_syntax_ranges(source);
        let emphasis_start = source.find("*强调*").unwrap();
        let emphasis_end = emphasis_start + "*强调".len();
        assert!(
            hidden
                .iter()
                .any(|range| range.start <= emphasis_start && range.end > emphasis_start)
        );
        assert!(
            hidden
                .iter()
                .any(|range| range.start <= emphasis_end && range.end > emphasis_end)
        );
        let italic_start = source.find("_斜体_").unwrap();
        let italic_end = italic_start + "_斜体".len();
        assert!(
            hidden
                .iter()
                .any(|range| range.start <= italic_start && range.end > italic_start)
        );
        assert!(
            hidden
                .iter()
                .any(|range| range.start <= italic_end && range.end > italic_end)
        );
        // Block markers are no longer part of the hidden set: they stay visible
        // while the paragraph is edited, so only the body must never be hidden.
        assert!(!hidden.iter().any(|range| &source[range.clone()] == "列表"));
    }

    /// 判断给定字节区间集合是否覆盖源码中的某个字面量。
    fn 覆盖(ranges: &[std::ops::Range<usize>], source: &str, needle: &str) -> bool {
        let at = source.find(needle).expect("字面量应存在于源码");
        ranges
            .iter()
            .any(|range| range.start <= at && range.end > at)
    }

    #[test]
    fn 编辑时块级标记保留可见() {
        let source = "# 标题\n\n- 列表项\n\n1. 有序项\n\n- [ ] 任务\n";
        let muted = super::muted_syntax_ranges(source);
        assert!(
            覆盖(&muted, source, "# 标题"),
            "标题井号应弱化显示而不是隐藏"
        );
        assert!(覆盖(&muted, source, "- 列表项"));
        assert!(覆盖(&muted, source, "1. 有序项"));
        assert!(覆盖(&muted, source, "- [ ] 任务"));

        // 块级标记不再进入隐藏集合，编辑时看不到的只有成对的行内标记。
        assert!(super::hidden_syntax_ranges(source).is_empty());
    }

    #[test]
    fn 行内标记成对识别并弱化显示() {
        let source = "普通 **加粗** 与 `代码`";
        let delimiters = super::hidden_syntax_ranges(source);
        assert!(覆盖(&delimiters, source, "**加粗**"));
        assert!(覆盖(&delimiters, source, "`代码`"));
        assert!(super::muted_syntax_ranges(source).is_empty());
        // 弱化而不是隐形：编辑态所见即源码。
        let job = super::layout_source_with_quiet_syntax(
            source,
            FontId::new(16.0, FontFamily::Proportional),
            Color32::from_rgb(40, 40, 40),
            500.0,
        );
        let covering = |at: usize| -> TextFormat {
            job.sections
                .iter()
                .find(|section| {
                    usize::from(section.byte_range.start) <= at
                        && at < usize::from(section.byte_range.end)
                })
                .expect("字节必须落在某个分段内")
                .format
                .clone()
        };
        assert_eq!(job.text, source);
        let star_at = source.find('*').unwrap();
        assert_eq!(
            covering(star_at).color,
            Color32::from_rgb(40, 40, 40).gamma_multiply(0.45)
        );
        let text_at = source.find("加粗").unwrap();
        assert_eq!(covering(text_at).color, Color32::from_rgb(40, 40, 40));
    }

    #[test]
    fn 编辑时引用记号弱化显示() {
        let source = "> 引用行\n> > 嵌套引用\n";
        let muted = super::muted_syntax_ranges(source);
        assert!(覆盖(&muted, source, "> 引用行"), "行首引用记号应弱化");
        let inner_quote = source.find("> > 嵌套引用").unwrap() + 2;
        assert!(
            muted
                .iter()
                .any(|range| range.start <= inner_quote && range.end > inner_quote),
            "嵌套的 `>` 也应弱化"
        );
        assert!(!覆盖(&muted, source, "嵌套引用"), "引用正文不得被弱化");
    }

    #[test]
    fn 围栏布局识别围栏行与信息串() {
        let source = "```powershell\nwsl -d Ubuntu-24.04\n```\n";
        let fence = super::code_fence_layout(source).expect("应识别为围栏代码块");
        assert_eq!(&source[fence.open_line.clone()], "```powershell");
        assert_eq!(&source[fence.content.clone()], "\nwsl -d Ubuntu-24.04\n");
        assert_eq!(
            &source[fence.close_line.clone().expect("应识别结尾围栏")],
            "```\n"
        );

        // 波浪线围栏、行首缩进与不含结尾换行的结尾围栏
        let source = "  ~~~rust\nfn main() {}\n  ~~~";
        let fence = super::code_fence_layout(source).expect("波浪线围栏同样成立");
        assert_eq!(&source[fence.open_line.clone()], "  ~~~rust");
        assert_eq!(
            &source[fence.close_line.clone().expect("应识别结尾围栏")],
            "  ~~~"
        );

        // 未闭合的围栏：结尾行为空，内容保持原样
        let source = "```python\nprint(1)";
        let fence = super::code_fence_layout(source).expect("开头围栏应成立");
        assert!(fence.close_line.is_none());
        assert_eq!(&source[fence.content.clone()], "\nprint(1)");

        // 结尾围栏必须与开头同记号且不短于开头（CommonMark）
        let source = "```text\n~~~\n";
        let fence = super::code_fence_layout(source).expect("开头围栏应成立");
        assert!(fence.close_line.is_none(), "波浪线不能闭合反引号围栏");
        let source = "````text\n```\n";
        let fence = super::code_fence_layout(source).expect("开头围栏应成立");
        assert!(fence.close_line.is_none(), "短于开头记号的行不能闭合围栏");

        // 缩进代码块与不足 3 个记号的行没有围栏可识别
        assert!(super::code_fence_layout("    缩进代码\n").is_none());
        assert!(super::code_fence_layout("`` 两点不是围栏\n").is_none());

        // CRLF 行尾同样成立
        let source = "```py\r\nx = 1\r\n```\r\n";
        let fence = super::code_fence_layout(source).expect("CRLF 围栏应成立");
        assert_eq!(&source[fence.open_line.clone()], "```py\r");
        assert_eq!(&source[fence.content.clone()], "\nx = 1\r\n");
        assert_eq!(
            &source[fence.close_line.clone().expect("应识别结尾围栏")],
            "```\r\n"
        );
    }

    #[test]
    fn 编辑态代码块弱化围栏并保留内容() {
        let source = "```powershell\nwsl -d Ubuntu-24.04\n```\n";
        let content_color = Color32::from_rgb(40, 40, 40);
        let job = super::layout_code_block_source(
            source,
            FontId::new(16.0, FontFamily::Monospace),
            content_color,
            500.0,
        );
        // 分段必须恰好覆盖缓冲区，光标与搜索映射才不会漂移。
        assert_eq!(job.text, source);
        let covering = |at: usize| -> TextFormat {
            job.sections
                .iter()
                .find(|section| {
                    usize::from(section.byte_range.start) <= at
                        && at < usize::from(section.byte_range.end)
                })
                .expect("字节必须落在某个分段内")
                .format
                .clone()
        };
        let fence_color = content_color.gamma_multiply(0.45);
        // 围栏记号与语言名弱化显示，但保持正常字号（编辑时语法可见）
        let fence_format = covering(0);
        assert_eq!(fence_format.font_id.size, 16.0);
        assert_eq!(fence_format.color, fence_color);
        assert_eq!(
            covering(source.find("powershell").unwrap()).color,
            fence_color
        );
        let body_format = covering(source.find("wsl").unwrap());
        assert_eq!(body_format.font_id.size, 16.0);
        assert_eq!(body_format.color, content_color);
        assert_eq!(covering(source.rfind("```").unwrap()).color, fence_color);
    }

    #[test]
    fn 编辑态代码块内容不再套用行内语法规则() {
        // 代码内容里的 `#`、`- `、`**` 都是字面文本：既不隐藏也不弱化，
        // 否则 shell 注释、普通减号行会在编辑时莫名变淡或消失。
        let source = "```text\n# 注释行\n- 减号行\n**加粗样式**\n```\n";
        let content_color = Color32::from_rgb(40, 40, 40);
        let job = super::layout_code_block_source(
            source,
            FontId::new(16.0, FontFamily::Monospace),
            content_color,
            500.0,
        );
        for needle in ["# 注释行", "- 减号行", "**加粗样式**"] {
            let at = source.find(needle).unwrap();
            let format = job
                .sections
                .iter()
                .find(|section| {
                    usize::from(section.byte_range.start) <= at
                        && at < usize::from(section.byte_range.end)
                })
                .expect("内容字节必须落在某个分段内")
                .format
                .clone();
            assert_eq!(format.color, content_color, "{needle} 应按字面内容渲染");
            assert_eq!(format.font_id.size, 16.0, "{needle} 不应套用围栏样式");
        }
    }

    #[test]
    fn 活动代码块以代码框形态编辑() {
        // 编辑中的代码块沿用阅读态的盒子外观，而不是整块退化为裸源码。
        let theme = crate::theme::ThemeSpec::fallback(false);
        let output = render_active_frame(
            "正文段落\n\n```powershell\nwsl -d Ubuntu-24.04\n```\n",
            Some(1),
            &theme,
        );
        let code_box = output.shapes.iter().any(|clipped| {
            matches!(&clipped.shape, egui::epaint::Shape::Rect(rect) if rect.fill == theme.code_bg)
        });
        assert!(code_box, "活动代码块应绘制阅读态的代码框底色");
    }

    #[test]
    fn 活动引用块编辑时不套用渲染外框() {
        // 渲染装饰（引用底色、左侧强调条）只属于阅读态的非活动块；
        // 编辑中的引用块回到朴素源码，否则编辑器里会漏进预览效果。
        let theme = crate::theme::ThemeSpec::fallback(false);
        let output = render_active_frame("> 引用正文\n", Some(0), &theme);
        let quote_box = output.shapes.iter().any(|clipped| {
            matches!(&clipped.shape, egui::epaint::Shape::Rect(rect) if rect.fill == theme.quote_bg)
        });
        assert!(!quote_box, "活动引用块不得绘制阅读态的引用框底色");
        // 源码本身照常可见：`>` 记号与正文都在编辑器文本里。
        let texts: Vec<&str> = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|text| text.contains("> 引用正文")),
            "编辑态应显示引用源码：{texts:?}"
        );
    }

    #[test]
    fn 编辑时代码内容中的标点保持可见() {
        let source = "`**literal**`";
        let hidden = super::hidden_syntax_ranges(source);
        let stars_start = source.find("**literal**").unwrap();
        assert!(
            !hidden
                .iter()
                .any(|range| range.start <= stars_start && range.end > stars_start)
        );
    }

    #[test]
    fn 编辑器最小高度随显式换行变化() {
        assert_eq!(super::editor_desired_rows(""), 1);
        assert_eq!(super::editor_desired_rows("单行段落"), 1);
        assert_eq!(super::editor_desired_rows("第一行\n第二行"), 2);
        assert_eq!(super::editor_desired_rows("第一行\n第二行\n"), 3);
    }

    #[test]
    fn 列表续写标记识别与递增() {
        let get = |line: &str| super::next_list_marker(line).expect("应识别为列表行");
        // 常规列表：续写同类标记
        let (marker, marker_only) = get("- item");
        assert_eq!(marker, "- ");
        assert!(!marker_only);
        let (marker, _) = get("* 星号项");
        assert_eq!(marker, "* ");
        let (marker, _) = get("+ 加号项");
        assert_eq!(marker, "+ ");
        // 缩进保留
        let (marker, _) = get("  - 嵌套项");
        assert_eq!(marker, "  - ");
        // 任务列表续写为未勾选
        let (marker, _) = get("- [x] 已完成");
        assert_eq!(marker, "- [ ] ");
        // 有序列表序号递增
        let (marker, _) = get("3. 第三项");
        assert_eq!(marker, "4. ");
        let (marker, _) = get("  10. 深层有序");
        assert_eq!(marker, "  11. ");
        // 仅含标记的行：续写方应退出列表
        assert!(get("- ").1);
        assert!(get("7. ").1);
        // 非列表行不续写
        assert_eq!(super::next_list_marker("普通段落"), None);
        assert_eq!(super::next_list_marker("# 标题"), None);
        assert_eq!(super::next_list_marker("-没有空格的减号"), None);
    }

    #[test]
    fn 强调文本使用斜体字体族() {
        let blocks = parse("plain e *em e* plain e");
        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(620.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                crate::preview::show_preview(ui, &blocks);
            });
        });
        let has_italic_family = output.shapes.iter().any(|shape| {
            let egui::epaint::Shape::Text(text) = &shape.shape else {
                return false;
            };
            text.galley
                .job
                .sections
                .iter()
                .any(|section| section.format.font_id.family == super::italic_family())
        });
        assert!(has_italic_family, "强调片段应使用独立的斜体字体族");
    }

    #[test]
    fn 中文标点采用全角宽度渲染() {
        let blocks = parse("……——");
        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(620.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                crate::preview::show_preview(ui, &blocks);
            });
        });
        let mut advances = Vec::new();
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                for placed in &text.galley.rows {
                    for glyph in &placed.row.glyphs {
                        if glyph.chr == '…' || glyph.chr == '—' {
                            advances.push((glyph.chr, glyph.advance_width));
                        }
                    }
                }
            }
        }
        assert_eq!(advances.len(), 4, "应有 2 个省略号 + 2 个破折号字形");
        // JetBrains Mono 自带的半宽字形约 0.6em（≈9.3px@15.5px），霞鹜文楷
        // 全角为 1em；只有正确回退到霞鹜文楷才会超过 15px。
        for (chr, advance) in &advances {
            assert!(*advance > 15.0, "{chr} 宽度 {advance:.1} 应为全角");
        }
    }

    #[test]
    fn 标题锚点按渲染顺序映射到顶层块() {
        // Headings nested inside list items and quotes still count, in order.
        let document = crate::markdown::parse_document("# A\n\n> ## B\n\n正文\n\n- ## C\n\n## D\n");
        let blocks = document.blocks();
        assert_eq!(super::block_index_for_heading(blocks, 0), Some(0)); // # A
        assert_eq!(super::block_index_for_heading(blocks, 1), Some(1)); // > ## B
        assert_eq!(super::block_index_for_heading(blocks, 2), Some(3)); // - ## C
        assert_eq!(super::block_index_for_heading(blocks, 3), Some(4)); // ## D
        assert_eq!(super::block_index_for_heading(blocks, 4), None);
    }

    #[test]
    fn 高度估算随行数增长并封顶() {
        let source = "一\n二\n三\n";
        let one = super::estimate_block_height(source, &(0..2), 20.0);
        let all = super::estimate_block_height(source, &(0..source.len()), 20.0);
        assert!(all > one, "更多行应估算得更高");
        let huge = "x\n".repeat(100_000);
        let capped = super::estimate_block_height(&huge, &(0..huge.len()), 20.0);
        assert_eq!(capped, 3000.0, "极端块必须封顶以免撑爆滚动条");
    }

    #[allow(clippy::too_many_arguments)]
    fn render_virtualized_frame(
        ctx: &egui::Context,
        input: &egui::RawInput,
        blocks: &[crate::markdown::Block],
        block_ranges: &[std::ops::Range<usize>],
        theme: &crate::theme::ThemeSpec,
        text: &mut String,
        heights: &mut Vec<f32>,
        cache: &mut super::ImageCache,
        active_block: Option<usize>,
        active_range: Option<std::ops::Range<usize>>,
        stale_blocks: bool,
    ) -> (super::BlockEditorOutput, Vec<(String, egui::Rect)>) {
        let mut captured: Option<super::BlockEditorOutput> = None;
        let output = ctx.run_ui(input.clone(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let mut viewport = super::PreviewViewport {
                    band: ui.cursor().min.y..(ui.cursor().min.y + 120.0),
                    heights,
                    margin: 60.0,
                };
                captured = Some(super::show_preview_with_block_editor_and_search(
                    ui,
                    blocks,
                    block_ranges,
                    text,
                    15.5,
                    theme,
                    active_block,
                    active_range.clone(),
                    None,
                    false,
                    None,
                    false,
                    false,
                    false,
                    stale_blocks,
                    None,
                    &mut super::PreviewImages {
                        base_directory: None,
                        cache,
                    },
                    Some(&mut viewport),
                ));
            });
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            if let egui::epaint::Shape::Text(text_shape) = &clipped.shape {
                texts.push((
                    text_shape.galley.text().to_owned(),
                    text_shape.galley.rect.translate(text_shape.pos.to_vec2()),
                ));
            }
        }
        (captured.expect("frame rendered"), texts)
    }

    /// 以指定活动块渲染一帧混合编辑视图，返回完整绘制输出，
    /// 供断言编辑器外框形态的测试使用。
    fn render_active_frame(
        source: &str,
        active_block: Option<usize>,
        theme: &crate::theme::ThemeSpec,
    ) -> egui::FullOutput {
        let document = crate::markdown::parse_document(source);
        let blocks = document.blocks().to_vec();
        let block_ranges = document.block_ranges().to_vec();
        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let input = frame_input(Vec::new(), None);
        let mut text = source.to_string();
        let mut cache = super::ImageCache::default();
        ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                super::show_preview_with_block_editor_and_search(
                    ui,
                    &blocks,
                    &block_ranges,
                    &mut text,
                    15.5,
                    theme,
                    active_block,
                    None,
                    None,
                    false,
                    None,
                    false,
                    false,
                    false,
                    false,
                    None,
                    &mut super::PreviewImages {
                        base_directory: None,
                        cache: &mut cache,
                    },
                    None,
                );
            });
        })
    }

    /// 构造“解析落后”场景：三段文档，用户在第二段继续输入使源码与编辑区间
    /// 增长，而 `blocks`/`block_ranges` 仍是上一版解析的产物。
    fn stale_parse_setup() -> (
        Vec<crate::markdown::Block>,
        Vec<std::ops::Range<usize>>,
        String,
        std::ops::Range<usize>,
    ) {
        let source = "第一段\n\n第二段\n\n第三段\n";
        let parsed = crate::markdown::parse_document(source);
        let blocks = parsed.blocks().to_vec();
        let block_ranges = parsed.block_ranges().to_vec();
        assert_eq!(blocks.len(), 3);
        let grown_block = "第二段第二段第二段第二段";
        let live_source = format!(
            "{}{}{}",
            &source[..block_ranges[1].start],
            grown_block,
            &source[block_ranges[1].end..]
        );
        let grown_range = block_ranges[1].start..block_ranges[1].start + grown_block.len();
        (blocks, block_ranges, live_source, grown_range)
    }

    fn frame_input(events: Vec<egui::Event>, time: Option<f64>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 400.0),
            )),
            time,
            events,
            ..Default::default()
        }
    }

    #[test]
    fn 视口剔除渲染前部块并记录高度() {
        // Build a document well above the virtualization threshold, then render
        // a viewport band that only covers the very top. Only early blocks may
        // paint, and their measured heights must be recorded for later frames.
        let mut source = String::new();
        for index in 0..(super::VIRTUALIZE_MIN_BLOCKS + 50) {
            source.push_str(&format!("第 {index} 段内容。\n\n"));
        }
        let document = crate::markdown::parse_document(&source);
        let blocks = document.blocks().to_vec();
        let block_ranges = document.block_ranges().to_vec();
        assert!(blocks.len() > super::VIRTUALIZE_MIN_BLOCKS);

        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let theme = crate::theme::ThemeSpec::fallback(false);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 400.0),
            )),
            ..Default::default()
        };
        let mut text = source.clone();
        let mut heights = vec![0.0f32; blocks.len()];
        let mut cache = super::ImageCache::default();
        let (first, _) = render_virtualized_frame(
            &ctx,
            &input,
            &blocks,
            &block_ranges,
            &theme,
            &mut text,
            &mut heights,
            &mut cache,
            None,
            None,
            false,
        );
        assert!(first.viewport_unsettled, "首帧用估算高度，应与实测不符");
        assert!(heights[0] > 0.0, "视口内首个块必须量得高度");
        // Skipped blocks cache an estimate, so they are non-zero but never
        // measured. The key invariant is that the frame settled the top blocks
        // and did not lay out the whole document.
        let (second, _) = render_virtualized_frame(
            &ctx,
            &input,
            &blocks,
            &block_ranges,
            &theme,
            &mut text,
            &mut heights,
            &mut cache,
            None,
            None,
            false,
        );
        assert!(!second.viewport_unsettled, "第二帧命中已测高度，视口应稳定");
    }

    #[test]
    fn 空文档渲染无内容占位() {
        // The placeholder was hand-rebuilt after an encoding accident and has
        // no test coverage elsewhere; lock the exact glyphs in. Note the
        // current app UI never reaches this branch (the hybrid editor edits
        // empty documents as whole-source), so this harness frame is the only
        // place it paints.
        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                crate::preview::show_preview(ui, &[]);
            });
        });
        let texts: Vec<&str> = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|text| text.contains("无内容")),
            "空文档应渲染 无内容 占位标签，实际文本：{texts:?}"
        );
    }

    #[test]
    fn 解析落后时按上一版排版且陈旧索引不吞块() {
        // Regression for the stale-parse contract (ADR-0004 decision 3):
        // while the parse lags behind the source, the previous parse keeps
        // driving the layout and only the block owned by the editor switches
        // to source editing. The grown edit range must not claim the blocks
        // that follow it.
        let (blocks, block_ranges, live_source, grown_range) = stale_parse_setup();
        let count = |texts: &[(String, egui::Rect)], needle: &str| {
            texts
                .iter()
                .filter(|(body, _)| body.contains(needle))
                .count()
        };

        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let theme = crate::theme::ThemeSpec::fallback(false);
        let input = frame_input(Vec::new(), None);

        let mut text = live_source.clone();
        let mut heights = vec![0.0f32; blocks.len()];
        let mut cache = super::ImageCache::default();
        let (output, texts) = render_virtualized_frame(
            &ctx,
            &input,
            &blocks,
            &block_ranges,
            &theme,
            &mut text,
            &mut heights,
            &mut cache,
            Some(1),
            Some(grown_range.clone()),
            true,
        );
        assert_eq!(count(&texts, "第一段"), 1, "活动块之前的块照常渲染");
        assert_eq!(count(&texts, "第三段"), 1, "陈旧区间不得吞掉活动块之后的块");
        assert_eq!(count(&texts, "第二段"), 1, "活动块只以编辑器形态出现一次");
        assert!(!output.changed, "纯渲染帧不得改写源码");

        // Without the stale fallback the grown range swallows block 2; this is
        // the failure mode `stale_blocks` exists to prevent.
        let mut text = live_source;
        let mut heights = vec![0.0f32; blocks.len()];
        let mut cache = super::ImageCache::default();
        let (_, texts) = render_virtualized_frame(
            &ctx,
            &input,
            &blocks,
            &block_ranges,
            &theme,
            &mut text,
            &mut heights,
            &mut cache,
            Some(1),
            Some(grown_range),
            false,
        );
        assert_eq!(
            count(&texts, "第三段"),
            0,
            "对照组：沿用陈旧区间会把活动块之后的块吞进编辑器"
        );
    }

    #[test]
    fn 解析落后时点击非活动块被门控丢弃() {
        // The frame keeps laying out from the previous parse, so a click on a
        // non-active block addresses that parse's indices. preview reports the
        // click to the caller; main.rs accepts it only while the parse is
        // current, so a stale click must be discarded instead of moving the
        // editor or its caret.
        let (blocks, block_ranges, live_source, grown_range) = stale_parse_setup();

        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let theme = crate::theme::ThemeSpec::fallback(false);
        let input = frame_input(Vec::new(), None);

        let mut text = live_source.clone();
        let mut heights = vec![0.0f32; blocks.len()];
        let mut cache = super::ImageCache::default();
        let (_, texts) = render_virtualized_frame(
            &ctx,
            &input,
            &blocks,
            &block_ranges,
            &theme,
            &mut text,
            &mut heights,
            &mut cache,
            Some(1),
            Some(grown_range.clone()),
            true,
        );
        let rect = texts
            .iter()
            .find(|(body, _)| body.contains("第三段"))
            .map(|(_, rect)| *rect)
            .expect("非活动块应渲染出可点击的文本");
        let center = rect.center();
        let press = frame_input(
            vec![
                egui::Event::PointerMoved(center),
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            Some(1.0),
        );
        let release = frame_input(
            vec![egui::Event::PointerButton {
                pos: center,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
            Some(1.1),
        );

        let mut text = live_source;
        let mut heights = vec![0.0f32; blocks.len()];
        let mut cache = super::ImageCache::default();
        let _ = render_virtualized_frame(
            &ctx,
            &press,
            &blocks,
            &block_ranges,
            &theme,
            &mut text,
            &mut heights,
            &mut cache,
            Some(1),
            Some(grown_range.clone()),
            true,
        );
        let (output, _) = render_virtualized_frame(
            &ctx,
            &release,
            &blocks,
            &block_ranges,
            &theme,
            &mut text,
            &mut heights,
            &mut cache,
            Some(1),
            Some(grown_range.clone()),
            true,
        );
        assert_eq!(
            output.clicked_block,
            Some(2),
            "preview 层把陈旧索引上的点击上报给调用方"
        );
        assert_eq!(
            crate::clicked_block_accepted(false, output.clicked_block),
            None,
            "解析落后时调用方门控必须丢弃陈旧索引上的点击"
        );
    }

    #[test]
    #[ignore = "长文档性能基准，手动运行：cargo test -- --ignored --nocapture"]
    fn 长文档全帧渲染耗时() {
        use std::time::Instant;
        let unit = "## 小节标题\n\n这是一段用于压测的中文段落，包含 **加粗**、`代码` 与[链接](https://example.com)等常见行内元素。English words mixed in for spacing.\n\n- 列表项甲\n- 列表项乙\n\n";
        let target_kb: usize = std::env::var("MD_BENCH_KB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2048);
        let mut source = String::new();
        while source.len() < target_kb * 1024 {
            source.push_str(unit);
        }
        let t0 = Instant::now();
        let document = crate::markdown::parse_document(&source);
        let parse_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let ctx = egui::Context::default();
        crate::export::install_app_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 1000.0),
            )),
            ..Default::default()
        };
        let mut best = f64::MAX;
        let mut worst = 0.0_f64;
        for frame in 0..5 {
            let blocks = document.blocks();
            let t1 = Instant::now();
            let _output = ctx.run_ui(input.clone(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    crate::preview::show_preview(ui, blocks);
                });
            });
            let ms = t1.elapsed().as_secs_f64() * 1000.0;
            best = best.min(ms);
            worst = worst.max(ms);
            eprintln!("frame {frame}: {ms:.0}ms");
        }
        eprintln!(
            "size={}MB blocks={} parse={parse_ms:.0}ms FULL best={best:.0}ms worst={worst:.0}ms",
            source.len() / (1024 * 1024),
            document.blocks().len(),
        );

        // Virtualized path: same document, only the top band visible.
        let mut text = source.clone();
        let mut heights = vec![0.0f32; document.blocks().len()];
        let mut cache = super::ImageCache::default();
        let theme = crate::theme::ThemeSpec::fallback(false);
        let ranges = document.block_ranges().to_vec();
        let blocks = document.blocks().to_vec();
        // Warm the cache with a few settled frames, then time.
        let mut vbest = f64::MAX;
        let mut vworst = 0.0_f64;
        for frame in 0..5 {
            let t1 = Instant::now();
            let _ = ctx.run_ui(input.clone(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let mut viewport = super::PreviewViewport {
                        band: ui.cursor().min.y..(ui.cursor().min.y + 900.0),
                        heights: &mut heights,
                        margin: 600.0,
                    };
                    super::show_preview_with_block_editor_and_search(
                        ui,
                        &blocks,
                        &ranges,
                        &mut text,
                        15.5,
                        &theme,
                        None,
                        None,
                        None,
                        false,
                        None,
                        false,
                        false,
                        false,
                        false,
                        None,
                        &mut super::PreviewImages {
                            base_directory: None,
                            cache: &mut cache,
                        },
                        Some(&mut viewport),
                    )
                });
            });
            let ms = t1.elapsed().as_secs_f64() * 1000.0;
            vbest = vbest.min(ms);
            vworst = vworst.max(ms);
            eprintln!("virt frame {frame}: {ms:.0}ms");
        }
        eprintln!("VIRTUAL best={vbest:.0}ms worst={vworst:.0}ms");
    }
}
