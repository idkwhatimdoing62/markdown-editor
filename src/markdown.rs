//! 把 Markdown 解析为可渲染的块模型。

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub fn parse_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub enum Inline {
    Text(String),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Code(String),
    Link {
        url: String,
        title: String,
        children: Vec<Inline>,
    },
    Image {
        url: String,
        alt: String,
    },
    SoftBreak,
    HardBreak,
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub enum Block {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    List {
        ordered: bool,
        start: u64,
        items: Vec<Vec<Block>>,
    },
    Code {
        lang: String,
        text: String,
    },
    Quote(Vec<Block>),
    Table {
        headers: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
    Raw(String),
}

/// Markdown 的单一解析产物。
///
/// 内部预览读取 `blocks`；浏览器预览和导出读取同一次解析产生的
/// `events`。任何消费者都不得再次从源码创建 `pulldown_cmark::Parser`。
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    source: String,
    blocks: Vec<Block>,
    events: Vec<SpannedEvent>,
    block_index: Vec<BlockIndexEntry>,
}

/// Shared metadata for one top-level Markdown block.
///
/// The parser owns the source range and stable identity.  Preview code fills
/// `rendered_height` when it has measured or estimated the corresponding DOM
/// block; the native editor uses the same entry to map a scroll position back
/// to a source block.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockIndexEntry {
    pub id: u64,
    pub source_range: Range<usize>,
    pub rendered_height: f32,
    pub heading_level: Option<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpannedEvent {
    pub event: Event<'static>,
    pub range: Range<usize>,
}

impl ParsedDocument {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn events(&self) -> &[SpannedEvent] {
        &self.events
    }

    pub fn block_index(&self) -> &[BlockIndexEntry] {
        &self.block_index
    }

    pub fn has_mermaid(&self) -> bool {
        self.events.iter().any(|item| {
            matches!(
                &item.event,
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                    if info.split_whitespace().next().is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
            )
        })
    }
}

impl Default for ParsedDocument {
    fn default() -> Self {
        parse_document("")
    }
}

pub fn parse_document(markdown: &str) -> ParsedDocument {
    parse_source_document(markdown)
}

fn parse_source_document(markdown: &str) -> ParsedDocument {
    let events = Parser::new_ext(markdown, parse_options())
        .into_offset_iter()
        .map(|(event, range)| SpannedEvent {
            event: event.into_static(),
            range,
        })
        .collect::<Vec<_>>();
    let mut builder = Builder::default();
    for item in &events {
        builder.push(item.event.clone());
    }
    let blocks = builder.finish();
    let block_index = build_block_index(&blocks, &events);
    ParsedDocument {
        source: markdown.to_string(),
        blocks,
        events,
        block_index,
    }
}

fn build_block_index(blocks: &[Block], events: &[SpannedEvent]) -> Vec<BlockIndexEntry> {
    let ranges = top_level_block_ranges_from_events(events).unwrap_or_default();
    let mut occurrences = HashMap::<u64, usize>::new();
    blocks
        .iter()
        .zip(ranges)
        .map(|(block, source_range)| {
            let mut hasher = DefaultHasher::new();
            "markdown-preview-block".hash(&mut hasher);
            block.hash(&mut hasher);
            let content_hash = hasher.finish();
            let occurrence = occurrences.entry(content_hash).or_default();
            let id = stable_block_id(content_hash, *occurrence);
            *occurrence += 1;
            BlockIndexEntry {
                id,
                source_range,
                rendered_height: 0.0,
                heading_level: match block {
                    Block::Heading { level, .. } => Some(*level),
                    _ => None,
                },
            }
        })
        .collect()
}

fn stable_block_id(content_hash: u64, occurrence: usize) -> u64 {
    let mut hasher = DefaultHasher::new();
    format!("markdown-preview-block:{content_hash}:{occurrence}").hash(&mut hasher);
    hasher.finish()
}

fn top_level_block_ranges_from_events(events: &[SpannedEvent]) -> Option<Vec<Range<usize>>> {
    let mut depth = 0usize;
    let mut start = None;
    let mut ranges = Vec::new();
    for item in events {
        match &item.event {
            Event::Start(tag) if is_block_tag(tag) => {
                if depth == 0 {
                    start = Some(item.range.start);
                }
                depth += 1;
            }
            Event::End(tag_end) if is_block_tag_end(*tag_end) => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    ranges.push(start.take()?..item.range.end);
                }
            }
            Event::Rule if depth == 0 => ranges.push(item.range.clone()),
            _ => {}
        }
    }
    (depth == 0 && start.is_none()).then_some(ranges)
}

/// Preserve logical block IDs across edits. Matching requires content, source
/// overlap, or an unchanged neighbouring block plus the same block kind. A
/// position by itself is deliberately not sufficient.
fn reconcile_block_ids(previous: &ParsedDocument, next: &mut ParsedDocument) {
    if previous.block_index.is_empty() || next.block_index.is_empty() {
        return;
    }
    let mut candidates = Vec::new();
    for (new_index, new_block) in next.blocks.iter().enumerate() {
        for (old_index, old_block) in previous.blocks.iter().enumerate() {
            if std::mem::discriminant(old_block) != std::mem::discriminant(new_block) {
                continue;
            }
            let old_range = &previous.block_index[old_index].source_range;
            let new_range = &next.block_index[new_index].source_range;
            let overlap = old_range.start.max(new_range.start) < old_range.end.min(new_range.end);
            let exact = old_block == new_block;
            let previous_neighbour = new_index > 0
                && old_index > 0
                && previous.blocks[old_index - 1] == next.blocks[new_index - 1];
            let next_neighbour = old_index + 1 < previous.blocks.len()
                && new_index + 1 < next.blocks.len()
                && previous.blocks[old_index + 1] == next.blocks[new_index + 1];
            if !(exact || overlap || previous_neighbour || next_neighbour) {
                continue;
            }
            let score = (exact as i64) * 1_000_000
                + (overlap as i64) * 10_000
                + (previous_neighbour as i64) * 2_000
                + (next_neighbour as i64) * 2_000
                - (old_index.abs_diff(new_index) as i64);
            candidates.push((score, old_index, new_index));
        }
    }
    candidates.sort_unstable_by(|left, right| right.cmp(left));
    let mut old_used = vec![false; previous.blocks.len()];
    let mut new_used = vec![false; next.blocks.len()];
    for (_, old_index, new_index) in candidates {
        if old_used[old_index] || new_used[new_index] {
            continue;
        }
        old_used[old_index] = true;
        new_used[new_index] = true;
        next.block_index[new_index].id = previous.block_index[old_index].id;
        next.block_index[new_index].rendered_height =
            previous.block_index[old_index].rendered_height;
    }
}

pub fn parse_document_with_previous(
    previous: Option<&ParsedDocument>,
    markdown: &str,
) -> ParsedDocument {
    let mut next = parse_document(markdown);
    if let Some(previous) = previous {
        reconcile_block_ids(previous, &mut next);
    }
    next
}

/// Reuse a parsed document for edits whose Markdown context is provably local.
/// Line-only edits reuse all blocks; textual edits reparse the edited block and
/// its immediate safe neighbors. A list, quote, or table is parsed as one
/// top-level block, so edits inside it stay local to that block.
pub fn parse_document_incremental(
    previous: &ParsedDocument,
    markdown: &str,
) -> Option<ParsedDocument> {
    if previous.source == markdown {
        return Some(previous.clone());
    }
    let next_source = markdown;
    let previous_source = previous.source();
    if previous_source == next_source {
        let mut next = previous.clone();
        next.source = markdown.to_string();
        return Some(next);
    }
    let old = previous_source.as_bytes();
    let new = next_source.as_bytes();
    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut old_end = old.len();
    let mut new_end = new.len();
    while old_end > prefix && new_end > prefix && old[old_end - 1] == new[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    let old_mid = &old[prefix..old_end];
    let new_mid = &new[prefix..new_end];
    let line_only =
        |slice: &[u8]| !slice.is_empty() && slice.iter().all(|byte| matches!(byte, b'\r' | b'\n'));
    let old_mid_len = old_end - prefix;
    let new_mid_len = new_end - prefix;
    let delta = new_mid_len as isize - old_mid_len as isize;

    if line_only(old_mid) || line_only(new_mid) {
        if !safe_line_edit(old, prefix, old_end) {
            return None;
        }
        let ranges = top_level_block_ranges(previous)?;
        let structural_line_edit = ranges.len() != previous.blocks.len()
            || ranges.iter().any(|range| {
                (range.start < prefix && prefix < range.end)
                    || (range.start < old_end && old_end < range.end)
            });
        if structural_line_edit {
            // A newline inside a list, quote, table, or fenced code block can
            // change the parent block's children. Let the parent-aware path
            // below reparse that complete boundary instead of shifting stale
            // events in place.
        } else {
            let shift = |offset: usize| {
                if offset <= prefix {
                    offset
                } else if offset >= old_end {
                    offset.saturating_add_signed(delta)
                } else {
                    prefix + new_mid_len
                }
            };
            let events: Vec<SpannedEvent> = previous
                .events
                .iter()
                .map(|item| SpannedEvent {
                    event: item.event.clone(),
                    range: shift(item.range.start)..shift(item.range.end),
                })
                .collect();
            let blocks = previous.blocks.clone();
            let mut next = ParsedDocument {
                source: markdown.to_string(),
                block_index: build_block_index(&blocks, &events),
                blocks,
                events,
            };
            reconcile_block_ids(previous, &mut next);
            return Some(next);
        }
    }

    let ranges = top_level_block_ranges(previous)?;
    if ranges.len() != previous.blocks.len() {
        return None;
    }
    if ranges.is_empty() {
        return None;
    }
    // Reparse the blocks touched by the edit together with their immediate
    // neighbors.  The reparsed window may contain a different number of
    // blocks, which covers inserting or deleting a paragraph without forcing
    // a full-document parse.
    let first_touched = ranges
        .iter()
        .position(|range| range.end >= prefix && range.start <= old_end)
        .unwrap_or_else(|| ranges.len().saturating_sub(1));
    let last_touched_exclusive = ranges
        .iter()
        .enumerate()
        .rev()
        .find(|(_, range)| range.start <= old_end && range.end >= prefix)
        .map_or(first_touched + 1, |(index, _)| index + 1);
    let mut window_start = first_touched.saturating_sub(1);
    let mut window_end = (last_touched_exclusive + 1).min(ranges.len());
    // A local parser window can be ambiguous at a boundary (for example when
    // a delimiter changes whether the next paragraph belongs to a list or a
    // quote). Expand both sides until the reparsed structure is valid. This
    // keeps the common path local while avoiding a second full parse merely
    // because the first one-block context was too narrow.
    let (old_window_start, old_window_end, reparsed) = loop {
        let old_window_start = ranges[window_start].start;
        let old_window_end = ranges[window_end - 1].end;
        let old_blocks_safe = ranges[window_start..window_end]
            .iter()
            .enumerate()
            .all(|(offset, _)| is_incremental_block_safe(&previous.blocks[window_start + offset]));
        let Some(new_window_end) = old_window_end.checked_add_signed(delta) else {
            if window_start == 0 && window_end == ranges.len() {
                return None;
            }
            window_start = window_start.saturating_sub(1);
            if window_end < ranges.len() {
                window_end += 1;
            }
            continue;
        };
        let reparsed_source = (old_blocks_safe
            && new_window_end >= old_window_start
            && new_window_end <= next_source.len())
        .then(|| parse_source_document(&next_source[old_window_start..new_window_end]));
        let valid = reparsed_source.as_ref().is_some_and(|reparsed| {
            let source = &next_source[old_window_start..new_window_end];
            (source.is_empty() || !reparsed.blocks.is_empty())
                && reparsed.blocks.iter().all(is_incremental_block_safe)
                && reparsed
                    .events
                    .iter()
                    .all(|item| item.range.end <= new_window_end - old_window_start)
                && local_window_boundary_is_safe(
                    next_source,
                    new_window_end,
                    reparsed,
                    window_end < ranges.len(),
                )
        });
        if valid {
            break (
                old_window_start,
                old_window_end,
                reparsed_source.expect("validated reparsed window"),
            );
        }
        if window_start == 0 && window_end == ranges.len() {
            return None;
        }
        window_start = window_start.saturating_sub(1);
        if window_end < ranges.len() {
            window_end += 1;
        }
    };

    let mut blocks = previous.blocks.clone();
    blocks.splice(window_start..window_end, reparsed.blocks);
    let mut events = Vec::with_capacity(previous.events.len() + reparsed.events.len());
    events.extend(
        previous
            .events
            .iter()
            .filter(|item| item.range.end <= old_window_start)
            .cloned(),
    );
    events.extend(reparsed.events.into_iter().map(|item| SpannedEvent {
        event: item.event,
        range: (old_window_start + item.range.start)..(old_window_start + item.range.end),
    }));
    events.extend(
        previous
            .events
            .iter()
            .filter(|item| item.range.start >= old_window_end)
            .map(|item| SpannedEvent {
                event: item.event.clone(),
                range: item.range.start.saturating_add_signed(delta)
                    ..item.range.end.saturating_add_signed(delta),
            }),
    );
    let block_index = build_block_index(&blocks, &events);
    let mut next = ParsedDocument {
        source: markdown.to_string(),
        blocks,
        events,
        block_index,
    };
    reconcile_block_ids(previous, &mut next);
    Some(next)
}

fn top_level_block_ranges(document: &ParsedDocument) -> Option<Vec<Range<usize>>> {
    top_level_block_ranges_from_events(&document.events)
}

fn is_incremental_block_safe(block: &Block) -> bool {
    matches!(
        block,
        Block::Heading { .. }
            | Block::Paragraph(_)
            | Block::Code { .. }
            | Block::List { .. }
            | Block::Quote(_)
            | Block::Table { .. }
            | Block::Rule
    )
}

pub(crate) fn is_block_tag(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::List(_)
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::Table(_)
            | Tag::MetadataBlock(_)
    )
}

pub(crate) fn is_block_tag_end(tag_end: TagEnd) -> bool {
    matches!(
        tag_end,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::Table
            | TagEnd::MetadataBlock(_)
    )
}

fn safe_line_edit(source: &[u8], start: usize, end: usize) -> bool {
    (start == end && (start == 0 || start == source.len() || source[start - 1] == b'\n'))
        || start == 0
        || end == source.len()
        || (start > 0 && source[start - 1] == b'\n' && end < source.len() && source[end] == b'\n')
}

fn local_window_boundary_is_safe(
    source: &str,
    window_end: usize,
    reparsed: &ParsedDocument,
    has_suffix: bool,
) -> bool {
    let Some(ranges) = top_level_block_ranges(reparsed) else {
        return false;
    };
    if ranges.len() != reparsed.blocks.len() {
        return false;
    }
    if ranges.first().is_some_and(|range| range.start != 0) {
        return false;
    }
    if !has_suffix {
        return true;
    }

    // An unterminated fence can only be closed by text outside the local
    // window. Treat that as ambiguous and let the adaptive loop expand.
    if has_unclosed_fence(reparsed.source()) {
        return false;
    }
    let Some(last_block) = reparsed.blocks.last() else {
        return false;
    };
    let suffix = source.get(window_end..).unwrap_or_default();
    let has_blank_separator = leading_newline_count(suffix) >= 2;
    if matches!(
        last_block,
        Block::List { .. } | Block::Quote(_) | Block::Code { .. } | Block::Raw(_)
    ) {
        return has_blank_separator;
    }
    has_blank_separator || suffix_line_starts_new_block(suffix)
}

fn leading_newline_count(source: &str) -> usize {
    let mut count = 0;
    let mut bytes = source.as_bytes();
    while let Some(first) = bytes.first().copied() {
        match first {
            b'\n' => {
                count += 1;
                bytes = &bytes[1..];
            }
            b'\r' if bytes.get(1) == Some(&b'\n') => {
                count += 1;
                bytes = &bytes[2..];
            }
            _ => break,
        }
    }
    count
}

fn suffix_line_starts_new_block(source: &str) -> bool {
    let line = source
        .trim_start_matches(['\r', '\n'])
        .lines()
        .next()
        .unwrap_or_default();
    let trimmed = line.trim_start();
    trimmed.starts_with('#')
        || trimmed.starts_with('>')
        || trimmed.starts_with("```")
        || trimmed.starts_with("~~~")
        || trimmed == "---"
        || trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || trimmed.split_once('.').is_some_and(|(prefix, rest)| {
            !prefix.is_empty()
                && prefix.chars().all(|ch| ch.is_ascii_digit())
                && rest.starts_with(' ')
        })
}

fn has_unclosed_fence(source: &str) -> bool {
    let mut open: Option<(u8, usize)> = None;
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some((character, length)) = fence_prefix(trimmed) else {
            continue;
        };
        match open {
            Some((open_character, open_length))
                if open_character == character && length >= open_length =>
            {
                open = None
            }
            Some(_) => {}
            None => open = Some((character, length)),
        }
    }
    open.is_some()
}

fn fence_prefix(line: &str) -> Option<(u8, usize)> {
    let bytes = line.as_bytes();
    let character = *bytes.first()?;
    if !matches!(character, b'`' | b'~') {
        return None;
    }
    let length = bytes.iter().take_while(|byte| **byte == character).count();
    (length >= 3).then_some((character, length))
}

#[cfg(test)]
pub fn parse(markdown: &str) -> Vec<Block> {
    parse_document(markdown).blocks
}

pub fn plain_text(blocks: &[Block]) -> String {
    let mut out = String::new();
    for block in blocks {
        block_text(block, &mut out);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn block_text(block: &Block, out: &mut String) {
    match block {
        Block::Heading { inlines, .. } => inline_text(inlines, out),
        Block::Paragraph(inlines) => inline_text(inlines, out),
        Block::List { items, .. } => {
            for item in items {
                for b in item {
                    block_text(b, out);
                }
            }
        }
        Block::Code { text, .. } => out.push_str(text),
        Block::Quote(blocks) => {
            for b in blocks {
                block_text(b, out);
            }
        }
        Block::Table { headers, rows } => {
            for h in headers {
                inline_text(h, out);
                out.push('|');
            }
            out.push('\n');
            for row in rows {
                for cell in row {
                    inline_text(cell, out);
                    out.push('|');
                }
                out.push('\n');
            }
        }
        Block::Rule => out.push_str("---"),
        Block::Raw(t) => out.push_str(t),
    }
}

fn inline_text(inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text(t) | Inline::Code(t) => out.push_str(t),
            Inline::Emphasis(c) | Inline::Strong(c) | Inline::Strikethrough(c) => {
                inline_text(c, out)
            }
            Inline::Link { children, .. } => inline_text(children, out),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
        }
    }
}

#[derive(Default)]
struct Builder {
    root: Vec<Block>,
    stack: Vec<Frame>,
}

#[derive(Clone, Copy, PartialEq)]
enum InlineKind {
    Emphasis,
    Strong,
    Strikethrough,
    Link,
    Image,
}

enum Frame {
    Paragraph {
        inlines: Vec<Inline>,
    },
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    List {
        start: u64,
        items: Vec<Vec<Block>>,
        cur: Vec<Block>,
    },
    Quote {
        blocks: Vec<Block>,
    },
    Code {
        lang: String,
        text: String,
    },
    Raw {
        text: String,
    },
    Table {
        headers: Vec<Vec<Vec<Inline>>>,
        rows: Vec<Vec<Vec<Inline>>>,
        row: Vec<Vec<Inline>>,
        cell: Vec<Inline>,
        in_head: bool,
    },
    Inline {
        kind: InlineKind,
        url: String,
        title: String,
        children: Vec<Inline>,
    },
}

impl Builder {
    fn push(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag_end) => self.end(tag_end),
            Event::Text(t) => self.text(t.to_string()),
            Event::Code(c) => self.inline(Inline::Code(c.to_string())),
            Event::SoftBreak => self.inline(Inline::SoftBreak),
            Event::HardBreak => self.inline(Inline::HardBreak),
            Event::Rule => self.block(Block::Rule),
            Event::TaskListMarker(checked) => {
                self.text(if checked { "[x] " } else { "[ ] " }.to_string())
            }
            Event::Html(h) => match self.stack.last_mut() {
                Some(Frame::Raw { text }) | Some(Frame::Code { text, .. }) => text.push_str(&h),
                _ => self.text(h.to_string()),
            },
            Event::FootnoteReference(name) => self.inline(Inline::Text(format!("[^{}]", name))),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.stack.push(Frame::Paragraph {
                inlines: Vec::new(),
            }),
            Tag::Heading { level, .. } => self.stack.push(Frame::Heading {
                level: level as u8,
                inlines: Vec::new(),
            }),
            Tag::BlockQuote(_) => self.stack.push(Frame::Quote { blocks: Vec::new() }),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.stack.push(Frame::Code {
                    lang,
                    text: String::new(),
                });
            }
            Tag::HtmlBlock => self.stack.push(Frame::Raw {
                text: String::new(),
            }),
            Tag::List(start) => self.stack.push(Frame::List {
                start: start.unwrap_or(1),
                items: Vec::new(),
                cur: Vec::new(),
            }),
            Tag::Item => {}
            Tag::Table(_) => self.stack.push(Frame::Table {
                headers: Vec::new(),
                rows: Vec::new(),
                row: Vec::new(),
                cell: Vec::new(),
                in_head: false,
            }),
            Tag::TableHead => {
                if let Some(Frame::Table { in_head, .. }) = self.stack.last_mut() {
                    *in_head = true;
                }
            }
            Tag::TableRow => {}
            Tag::TableCell => {
                if let Some(Frame::Table { cell, .. }) = self.stack.last_mut() {
                    cell.clear();
                }
            }
            Tag::Emphasis => self.push_inline_frame(InlineKind::Emphasis, "", ""),
            Tag::Strong => self.push_inline_frame(InlineKind::Strong, "", ""),
            Tag::Strikethrough => self.push_inline_frame(InlineKind::Strikethrough, "", ""),
            Tag::Link {
                dest_url, title, ..
            } => self.push_inline_frame(InlineKind::Link, &dest_url, &title),
            Tag::Image {
                dest_url, title, ..
            } => self.push_inline_frame(InlineKind::Image, &dest_url, &title),
            Tag::FootnoteDefinition(_) => self.stack.push(Frame::Raw {
                text: String::new(),
            }),
            _ => self.stack.push(Frame::Raw {
                text: String::new(),
            }),
        }
    }

    fn push_inline_frame(&mut self, kind: InlineKind, url: &str, title: &str) {
        self.stack.push(Frame::Inline {
            kind,
            url: url.to_string(),
            title: title.to_string(),
            children: Vec::new(),
        });
    }

    fn end(&mut self, tag_end: TagEnd) {
        match tag_end {
            TagEnd::Paragraph => {
                if let Some(Frame::Paragraph { inlines }) = self.stack.pop()
                    && !inlines.is_empty()
                {
                    self.block(Block::Paragraph(inlines));
                }
            }
            TagEnd::Heading(_) => {
                if let Some(Frame::Heading { level, inlines }) = self.stack.pop() {
                    self.block(Block::Heading { level, inlines });
                }
            }
            TagEnd::BlockQuote(_) => {
                if let Some(Frame::Quote { blocks }) = self.stack.pop() {
                    self.block(Block::Quote(blocks));
                }
            }
            TagEnd::CodeBlock => {
                if let Some(Frame::Code { lang, text }) = self.stack.pop() {
                    self.block(Block::Code { lang, text });
                }
            }
            TagEnd::HtmlBlock | TagEnd::FootnoteDefinition => {
                if let Some(Frame::Raw { text }) = self.stack.pop() {
                    self.block(Block::Raw(text));
                }
            }
            TagEnd::List(ordered) => {
                if let Some(Frame::List {
                    start,
                    mut items,
                    cur,
                }) = self.stack.pop()
                {
                    if !cur.is_empty() {
                        items.push(cur);
                    }
                    self.block(Block::List {
                        ordered,
                        start,
                        items,
                    });
                }
            }
            TagEnd::Item => {
                self.close_open_paragraph();
                if let Some(Frame::List { cur, items, .. }) = self.stack.last_mut()
                    && !cur.is_empty()
                {
                    items.push(std::mem::take(cur));
                }
            }
            TagEnd::Table => {
                if let Some(Frame::Table { headers, rows, .. }) = self.stack.pop() {
                    let headers = headers.into_iter().next().unwrap_or_default();
                    self.block(Block::Table { headers, rows });
                }
            }
            TagEnd::TableHead => {
                if let Some(Frame::Table {
                    in_head,
                    row,
                    headers,
                    ..
                }) = self.stack.last_mut()
                {
                    *in_head = false;
                    if !row.is_empty() {
                        headers.push(std::mem::take(row));
                    }
                }
            }
            TagEnd::TableRow => {
                if let Some(Frame::Table {
                    in_head,
                    row,
                    rows,
                    headers,
                    ..
                }) = self.stack.last_mut()
                {
                    if *in_head {
                        headers.push(std::mem::take(row));
                    } else {
                        rows.push(std::mem::take(row));
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(Frame::Table { row, cell, .. }) = self.stack.last_mut() {
                    row.push(std::mem::take(cell));
                }
            }
            TagEnd::Emphasis => self.finish_inline(InlineKind::Emphasis),
            TagEnd::Strong => self.finish_inline(InlineKind::Strong),
            TagEnd::Strikethrough => self.finish_inline(InlineKind::Strikethrough),
            TagEnd::Link => self.finish_inline(InlineKind::Link),
            TagEnd::Image => self.finish_inline(InlineKind::Image),
            _ => {}
        }
    }

    fn finish_inline(&mut self, _expected: InlineKind) {
        if let Some(Frame::Inline {
            kind,
            url,
            title,
            children,
        }) = self.stack.pop()
        {
            let inline = match kind {
                InlineKind::Emphasis => Inline::Emphasis(children),
                InlineKind::Strong => Inline::Strong(children),
                InlineKind::Strikethrough => Inline::Strikethrough(children),
                InlineKind::Link => Inline::Link {
                    url,
                    title,
                    children,
                },
                InlineKind::Image => Inline::Image {
                    url,
                    alt: plain_of_inlines(&children),
                },
            };
            self.inline(inline);
        }
    }

    fn close_open_paragraph(&mut self) {
        if let Some(Frame::Paragraph { inlines }) = self.stack.last() {
            if inlines.is_empty() {
                let _ = self.stack.pop();
            } else if let Some(Frame::Paragraph { inlines }) = self.stack.pop() {
                self.block(Block::Paragraph(inlines));
            }
        }
    }

    fn text(&mut self, t: String) {
        match self.stack.last_mut() {
            Some(Frame::Code { text, .. }) | Some(Frame::Raw { text }) => text.push_str(&t),
            _ => {
                self.ensure_paragraph();
                self.inline(Inline::Text(t));
            }
        }
    }

    fn ensure_paragraph(&mut self) {
        match self.stack.last() {
            Some(Frame::Paragraph { .. })
            | Some(Frame::Heading { .. })
            | Some(Frame::Inline { .. })
            | Some(Frame::Table { .. }) => {}
            _ => self.stack.push(Frame::Paragraph {
                inlines: Vec::new(),
            }),
        }
    }

    fn inline(&mut self, inline: Inline) {
        // 紧凑列表等场景下，行内标签直接出现在块容器里而没有段落帧；
        // 找不到容纳行内内容的帧时先补一个段落，避免内容被丢弃。
        let has_target = self.stack.iter_mut().rev().any(|frame| {
            matches!(
                frame,
                Frame::Inline { .. }
                    | Frame::Paragraph { .. }
                    | Frame::Heading { .. }
                    | Frame::Table { .. }
            )
        });
        if !has_target {
            self.ensure_paragraph();
        }
        for frame in self.stack.iter_mut().rev() {
            match frame {
                Frame::Inline { children, .. } => {
                    children.push(inline);
                    return;
                }
                Frame::Paragraph { inlines } => {
                    inlines.push(inline);
                    return;
                }
                Frame::Heading { inlines, .. } => {
                    inlines.push(inline);
                    return;
                }
                Frame::Table { cell, .. } => {
                    cell.push(inline);
                    return;
                }
                _ => {}
            }
        }
    }

    fn block(&mut self, block: Block) {
        for frame in self.stack.iter_mut().rev() {
            match frame {
                Frame::List { cur, .. } => {
                    cur.push(block);
                    return;
                }
                Frame::Quote { blocks } => {
                    blocks.push(block);
                    return;
                }
                _ => {}
            }
        }
        self.root.push(block);
    }

    fn finish(mut self) -> Vec<Block> {
        while let Some(frame) = self.stack.pop() {
            match frame {
                Frame::Paragraph { inlines } => {
                    if !inlines.is_empty() {
                        self.root.push(Block::Paragraph(inlines));
                    }
                }
                Frame::Heading { level, inlines } => {
                    self.root.push(Block::Heading { level, inlines });
                }
                Frame::List {
                    start,
                    mut items,
                    cur,
                } => {
                    if !cur.is_empty() {
                        items.push(cur);
                    }
                    self.root.push(Block::List {
                        ordered: false,
                        start,
                        items,
                    });
                }
                Frame::Quote { blocks } => self.root.push(Block::Quote(blocks)),
                Frame::Code { lang, text } => self.root.push(Block::Code { lang, text }),
                Frame::Raw { text } => self.root.push(Block::Raw(text)),
                Frame::Table { headers, rows, .. } => {
                    let headers = headers.into_iter().next().unwrap_or_default();
                    self.root.push(Block::Table { headers, rows });
                }
                Frame::Inline {
                    kind,
                    url,
                    title,
                    children,
                } => {
                    let inline = match kind {
                        InlineKind::Emphasis => Inline::Emphasis(children),
                        InlineKind::Strong => Inline::Strong(children),
                        InlineKind::Strikethrough => Inline::Strikethrough(children),
                        InlineKind::Link => Inline::Link {
                            url,
                            title,
                            children,
                        },
                        InlineKind::Image => Inline::Image {
                            url,
                            alt: plain_of_inlines(&children),
                        },
                    };
                    self.root.push(Block::Paragraph(vec![inline]));
                }
            }
        }
        self.root
    }
}

fn plain_of_inlines(inlines: &[Inline]) -> String {
    let mut out = String::new();
    inline_text(inlines, &mut out);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_parse_reuses_ast_for_safe_blank_line_insertion() {
        let previous = parse_document("# 标题\n\n正文\n");
        let next = parse_document_incremental(&previous, "\n# 标题\n\n正文\n")
            .expect("leading blank line should preserve markdown structure");

        assert_eq!(next.blocks(), previous.blocks());
        assert_eq!(next.events().len(), previous.events().len());
        assert_eq!(next.source(), "\n# 标题\n\n正文\n");
        assert!(
            next.events()
                .iter()
                .all(|event| event.range.end <= next.source().len())
        );
    }

    #[test]
    fn incremental_parse_falls_back_when_text_content_changes() {
        let previous = parse_document("# 标题\n\n正文\n");
        let next = parse_document_incremental(&previous, "# 标题\n\n修改后的正文\n");
        assert!(
            next.is_some(),
            "a single paragraph should use block-level parsing"
        );
        assert_eq!(next.unwrap().source(), "# 标题\n\n修改后的正文\n");
    }

    #[test]
    fn incremental_parse_reparses_only_the_edited_top_level_block() {
        let previous = parse_document("# 一\n\n甲\n\n# 二\n\n乙\n");
        let next = parse_document_incremental(&previous, "# 一\n\n修改后的甲\n\n# 二\n\n乙\n")
            .expect("single paragraph edit should be incremental");
        let full = parse_document("# 一\n\n修改后的甲\n\n# 二\n\n乙\n");

        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
    }

    #[test]
    fn incremental_parse_reparses_when_a_newline_splits_a_paragraph() {
        let previous = parse_document("甲\n乙\n\n后文\n");
        let next_source = "甲\n\n乙\n\n后文\n";
        let next = parse_document_incremental(&previous, next_source)
            .expect("paragraph split should remain locally reparsable");
        let full = parse_document(next_source);
        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
    }

    #[test]
    fn incremental_parse_supports_inserting_a_top_level_block() {
        let previous = parse_document("# 一\n\n甲\n\n乙\n");
        let next_source = "# 一\n\n甲\n\n新增\n\n乙\n";
        let next = parse_document_incremental(&previous, next_source)
            .expect("inserting a paragraph should stay incremental");

        assert_eq!(next.source(), next_source);
        assert_eq!(next.blocks().len(), 4);
        assert_eq!(next.blocks()[1], previous.blocks()[1]);
        assert_eq!(next.blocks()[3], previous.blocks()[2]);
        let full = parse_document(next_source);
        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
    }

    #[test]
    fn incremental_parse_supports_deleting_a_top_level_block() {
        let previous = parse_document("# 一\n\n甲\n\n删除\n\n乙\n");
        let next_source = "# 一\n\n甲\n\n乙\n";
        let next = parse_document_incremental(&previous, next_source)
            .expect("deleting a paragraph should stay incremental");

        assert_eq!(next.source(), next_source);
        assert_eq!(next.blocks().len(), 3);
        assert_eq!(next.blocks()[1], previous.blocks()[1]);
        assert_eq!(next.blocks()[2], previous.blocks()[3]);
        let full = parse_document(next_source);
        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
    }

    #[test]
    fn incremental_parse忽略不相邻的结构性块() {
        let previous = parse_document("# 一\n\n甲\n\n- 列表项\n\n乙\n");
        let next = parse_document_incremental(&previous, "# 新标题\n\n甲\n\n- 列表项\n\n乙\n")
            .expect("不相邻的列表不应阻止安全标题块的局部解析");
        let full = parse_document("# 新标题\n\n甲\n\n- 列表项\n\n乙\n");
        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
    }

    #[test]
    fn incremental_parse_reparses_heading_and_code_blocks() {
        for (previous_source, next_source) in [
            ("# 标题\n\n正文\n", "# 新标题\n\n正文\n"),
            (
                "正文\n\n```rust\nlet a = 1;\n```\n",
                "正文\n\n```rust\nlet answer = 42;\n```\n",
            ),
        ] {
            let previous = parse_document(previous_source);
            let next = parse_document_incremental(&previous, next_source)
                .expect("simple block edit should be incremental");
            let full = parse_document(next_source);
            assert_eq!(next.blocks(), full.blocks());
            assert_eq!(next.events(), full.events());
        }
    }

    #[test]
    fn incremental_parse_handles_text_edit_at_end_of_document() {
        let previous = parse_document("前文\n\n末尾\n");
        let next = parse_document_incremental(&previous, "前文\n\n末尾追加\n")
            .expect("editing the final paragraph should be incremental");
        let full = parse_document("前文\n\n末尾追加\n");
        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
    }

    #[test]
    fn incremental_parse重解析整个列表块() {
        let previous = parse_document("前文\n\n- 一\n- 二\n\n后文\n");
        let next = parse_document_incremental(&previous, "前文\n\n- 一\n- 修改后的二\n\n后文\n")
            .expect("列表块内部的文字修改应保持局部解析");
        let full = parse_document("前文\n\n- 一\n- 修改后的二\n\n后文\n");
        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
    }

    #[test]
    fn incremental_parse重解析引用和表格块() {
        for (previous_source, next_source) in [
            ("> 说明\n> 原文\n\n后文\n", "> 说明\n> 修改后\n\n后文\n"),
            (
                "| 名称 | 状态 |\n| --- | --- |\n| 编辑 | 可用 |\n\n后文\n",
                "| 名称 | 状态 |\n| --- | --- |\n| 编辑 | 已完成 |\n\n后文\n",
            ),
        ] {
            let previous = parse_document(previous_source);
            let next = parse_document_incremental(&previous, next_source)
                .expect("引用和表格块内部的文字修改应保持局部解析");
            let full = parse_document(next_source);
            assert_eq!(next.blocks(), full.blocks());
            assert_eq!(next.events(), full.events());
        }
    }

    #[test]
    fn incremental_parse_reparses_raw_html_block() {
        let previous = parse_document("<div>旧内容</div>\n\n后文\n");
        let next_source = "<div><span>新内容</span></div>\n\n后文\n";
        assert!(parse_document_incremental(&previous, next_source).is_none());
    }

    #[test]
    fn incremental_parse_uses_source_without_compat_normalization() {
        let previous = parse_document("- **识别与生成：**区分植物\n\n后文\n");
        assert_eq!(previous.source(), "- **识别与生成：**区分植物\n\n后文\n");
        let next_source = "- **识别与生成：**区分植物和动物\n\n后文\n";
        let next = parse_document_incremental(&previous, next_source)
            .expect("ordinary Markdown edits should stay incremental");
        let full = parse_document(next_source);

        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
        assert_eq!(next.source(), full.source());
    }

    #[test]
    fn incremental_parse_supports_deleting_the_last_block() {
        let previous = parse_document("最后一段\n");
        let next = parse_document_incremental(&previous, "")
            .expect("deleting the last block should remain incremental");
        let full = parse_document("");

        assert!(next.blocks().is_empty());
        assert_eq!(next.blocks(), full.blocks());
        assert_eq!(next.events(), full.events());
    }

    #[test]
    fn block_ids_follow_logical_blocks_when_text_changes() {
        let previous = parse_document("# 一\n\n甲\n\n乙\n");
        let next = parse_document_with_previous(Some(&previous), "# 一\n\n修改后的甲\n\n乙\n");

        assert_eq!(next.block_index()[0].id, previous.block_index()[0].id);
        assert_eq!(next.block_index()[1].id, previous.block_index()[1].id);
        assert_eq!(next.block_index()[2].id, previous.block_index()[2].id);
    }

    #[test]
    fn block_ids_do_not_relabel_same_kind_blocks_by_index() {
        let previous = parse_document("甲\n\n乙\n\n丙\n");
        let next = parse_document_with_previous(Some(&previous), "新块\n\n甲\n\n乙\n");

        assert_eq!(next.block_index()[1].id, previous.block_index()[0].id);
        assert_eq!(next.block_index()[2].id, previous.block_index()[1].id);
        assert_ne!(next.block_index()[0].id, previous.block_index()[0].id);
    }

    #[test]
    fn incremental_parse_reparses_blank_line_inside_code_block_boundary() {
        let previous = parse_document("```text\na\n\nb\n```\n");
        let next = "```text\na\n\n\nb\n```\n";

        let next = parse_document_incremental(&previous, next)
            .expect("the fenced code parent should be reparsed as one boundary");
        assert_eq!(
            next.blocks(),
            parse_document("```text\na\n\n\nb\n```\n").blocks()
        );
    }

    #[test]
    fn incremental_parse_keeps_nested_parent_boundaries() {
        let cases = [
            (
                "- 外层\n  - 内层一\n  - 内层二\n\n后文\n",
                "- 外层\n  - 内层一\n  - 修改后的内层二\n\n后文\n",
            ),
            (
                "> 引用\n> 第二行\n\n后文\n",
                "> 引用\n> 修改后的第二行\n\n后文\n",
            ),
            (
                "| 名称 | 状态 |\n| --- | --- |\n| A | 可用 |\n\n后文\n",
                "| 名称 | 状态 |\n| --- | --- |\n| A | 已完成 |\n\n后文\n",
            ),
        ];
        for (previous_source, next_source) in cases {
            let previous = parse_document(previous_source);
            let next = parse_document_incremental(&previous, next_source)
                .expect("nested parent edits should stay within one boundary");
            assert_eq!(next.blocks(), parse_document(next_source).blocks());
        }
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct StructureCounts {
        headings: usize,
        lists: usize,
        list_items: usize,
        code_blocks: usize,
        tables: usize,
        images: usize,
        links: usize,
        strong: usize,
    }

    fn event_structure(document: &ParsedDocument) -> StructureCounts {
        let mut counts = StructureCounts::default();
        for item in document.events() {
            match &item.event {
                Event::Start(Tag::Heading { .. }) => counts.headings += 1,
                Event::Start(Tag::List(_)) => counts.lists += 1,
                Event::Start(Tag::Item) => counts.list_items += 1,
                Event::Start(Tag::CodeBlock(_)) => counts.code_blocks += 1,
                Event::Start(Tag::Table(_)) => counts.tables += 1,
                Event::Start(Tag::Image { .. }) => counts.images += 1,
                Event::Start(Tag::Link { .. }) => counts.links += 1,
                Event::Start(Tag::Strong) => counts.strong += 1,
                _ => {}
            }
        }
        counts
    }

    fn block_structure(blocks: &[Block]) -> StructureCounts {
        fn visit_inlines(inlines: &[Inline], counts: &mut StructureCounts) {
            for inline in inlines {
                match inline {
                    Inline::Emphasis(children) | Inline::Strikethrough(children) => {
                        visit_inlines(children, counts)
                    }
                    Inline::Strong(children) => {
                        counts.strong += 1;
                        visit_inlines(children, counts);
                    }
                    Inline::Link { children, .. } => {
                        counts.links += 1;
                        visit_inlines(children, counts);
                    }
                    Inline::Image { .. } => counts.images += 1,
                    Inline::Text(_) | Inline::Code(_) | Inline::SoftBreak | Inline::HardBreak => {}
                }
            }
        }

        fn visit_blocks(blocks: &[Block], counts: &mut StructureCounts) {
            for block in blocks {
                match block {
                    Block::Heading { inlines, .. } => {
                        counts.headings += 1;
                        visit_inlines(inlines, counts);
                    }
                    Block::Paragraph(inlines) => visit_inlines(inlines, counts),
                    Block::List { items, .. } => {
                        counts.lists += 1;
                        counts.list_items += items.len();
                        for item in items {
                            visit_blocks(item, counts);
                        }
                    }
                    Block::Code { .. } => counts.code_blocks += 1,
                    Block::Quote(children) => visit_blocks(children, counts),
                    Block::Table { headers, rows } => {
                        counts.tables += 1;
                        for cell in headers {
                            visit_inlines(cell, counts);
                        }
                        for row in rows {
                            for cell in row {
                                visit_inlines(cell, counts);
                            }
                        }
                    }
                    Block::Rule | Block::Raw(_) => {}
                }
            }
        }

        let mut counts = StructureCounts::default();
        visit_blocks(blocks, &mut counts);
        counts
    }

    #[test]
    fn 正常笔记解析出标题列表和链接() {
        let md = "# 会议记录\n\n## 结论\n\n- 本周发布 v1.2\n- 下周评审接口\n\n详见[接口文档](https://example.com)\n";
        let blocks = parse(md);
        assert!(matches!(
            &blocks[0],
            Block::Heading { level: 1, inlines } if plain_of_inlines(inlines) == "会议记录"
        ));
        assert!(matches!(
            &blocks[1],
            Block::Heading { level: 2, inlines } if plain_of_inlines(inlines) == "结论"
        ));
        assert!(matches!(&blocks[2], Block::List { items, .. } if items.len() == 2));
        let last = &blocks[3];
        let text = match last {
            Block::Paragraph(inlines) => inlines,
            _ => panic!("应为段落"),
        };
        let has_link = text
            .iter()
            .any(|i| matches!(i, Inline::Link { url, .. } if url == "https://example.com"));
        assert!(has_link, "段落应包含链接");
    }

    #[test]
    fn 空文档解析为空() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn 代码块内特殊字符不解析() {
        let md = "```\n# 这不是标题\n**这不是粗体**\n[这不是链接](https://example.com)\n```\n";
        let blocks = parse(md);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Code { text, .. } => {
                assert!(text.contains("# 这不是标题"));
                assert!(text.contains("**这不是粗体**"));
            }
            other => panic!("应为代码块，实际 {:?}", other),
        }
    }

    #[test]
    fn 表格解析出表头和行() {
        let md = "| 名称 | 数量 |\n| --- | --- |\n| 苹果 | 3 |\n";
        let blocks = parse(md);
        match &blocks[0] {
            Block::Table { headers, rows } => {
                assert_eq!(headers.len(), 2, "表头应有 2 个单元格");
                assert_eq!(plain_of_inlines(&headers[0]), "名称");
                assert_eq!(plain_of_inlines(&headers[1]), "数量");
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].len(), 2);
                assert_eq!(plain_of_inlines(&rows[0][0]), "苹果");
                assert_eq!(plain_of_inlines(&rows[0][1]), "3");
            }
            other => panic!("应为表格，实际 {:?}", other),
        }
    }

    #[test]
    fn 纯文本提取可用() {
        let md = "# 标题\n\n段落**加粗**。\n\n- 条目一\n- 条目二\n";
        let text = plain_text(&parse(md));
        assert!(text.contains("标题"));
        assert!(text.contains("段落加粗。"));
        assert!(text.contains("条目一"));
    }

    #[test]
    fn 单一解析产物的事件流和内部模型结构一致() {
        let source = r#"# 总览

正文包含 **重点**、[链接](https://example.com) 和图片：![莲花](lotus.png)。

- 第一项
- 第二项

| 名称 | 数量 |
| --- | ---: |
| 莲花 | 3 |

```rust
fn main() {}
```
"#;
        let document = parse_document(source);
        assert_eq!(
            event_structure(&document),
            block_structure(document.blocks()),
            "新增 Markdown 语法必须同时进入事件流和内部块模型"
        );

        let mut browser_html = String::new();
        pulldown_cmark::html::push_html(
            &mut browser_html,
            document.events().iter().map(|item| item.event.clone()),
        );
        let expected = event_structure(&document);
        assert_eq!(browser_html.matches("<h1").count(), expected.headings);
        assert_eq!(browser_html.matches("<li>").count(), expected.list_items);
        assert_eq!(browser_html.matches("<table>").count(), expected.tables);
        assert_eq!(
            browser_html.matches("<pre><code").count(),
            expected.code_blocks
        );
        assert_eq!(browser_html.matches("<img ").count(), expected.images);
        assert_eq!(browser_html.matches("<a ").count(), expected.links);
        assert_eq!(browser_html.matches("<strong>").count(), expected.strong);
    }
}
