//! 把 Markdown 解析为可渲染的块模型。

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

pub fn parse_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
}

/// Return whether a link destination is safe to pass to the operating system
/// or an HTML renderer. Relative paths and ordinary web/mail protocols are
/// allowed; executable URL schemes are rejected.
pub fn is_safe_link_destination(destination: &str) -> bool {
    // Browsers strip leading/trailing C0 control characters and spaces and
    // remove every tab/CR/LF before resolving a URL, so `"  javascript:…"`
    // and `"&#x0A;javascript:…"` still execute. Classify the same cleaned
    // form the browser would see.
    let destination = normalize_url_for_scheme_check(destination);
    // `//host/share` is a protocol-relative URL and `\\host\share` is a
    // Windows UNC path. Both resolve to a network location when handed to the
    // operating system, which can leak credentials or block the UI while a
    // share is probed, so neither is treated as an ordinary relative path.
    if destination.starts_with("//") || destination.starts_with(r"\\") {
        return false;
    }
    // A Windows absolute path starts with a drive letter, which is not a URL
    // scheme even though it contains a colon.
    if destination.len() >= 3
        && destination.as_bytes()[1] == b':'
        && matches!(destination.as_bytes()[2], b'\\' | b'/')
    {
        return true;
    }
    let scheme_end = destination.find(':').filter(|index| {
        destination[..*index]
            .chars()
            .all(|ch| ch.is_ascii_alphabetic())
    });
    match scheme_end.map(|index| destination[..index].to_ascii_lowercase()) {
        Some(scheme) => matches!(scheme.as_str(), "http" | "https" | "mailto" | "ftp"),
        None => true,
    }
}

/// Strip the characters browsers ignore when resolving a URL: leading and
/// trailing C0-control/space characters, plus any embedded tab/CR/LF.
pub fn normalize_url_for_scheme_check(destination: &str) -> std::borrow::Cow<'_, str> {
    let needs_trim = destination.chars().next().is_some_and(|ch| ch <= ' ')
        || destination.chars().next_back().is_some_and(|ch| ch <= ' ');
    let has_embedded = destination.contains(['\t', '\n', '\r']);
    if !needs_trim && !has_embedded {
        return std::borrow::Cow::Borrowed(destination);
    }
    let trimmed = destination.trim_matches(|ch: char| ch <= ' ');
    if !has_embedded {
        std::borrow::Cow::Owned(trimmed.to_string())
    } else {
        std::borrow::Cow::Owned(
            trimmed
                .chars()
                .filter(|ch| *ch != '\t' && *ch != '\n' && *ch != '\r')
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
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
/// 原生预览、目录、搜索和导出读取同一次解析产生的 `blocks`/`events`。
/// 任何消费者都不得再次从源码创建 `pulldown_cmark::Parser`。
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    source: String,
    blocks: Vec<Block>,
    events: Vec<SpannedEvent>,
    block_ranges: Vec<Range<usize>>,
    headings: Vec<HeadingInfo>,
}

#[derive(Debug, Clone)]
pub struct SpannedEvent {
    pub event: Event<'static>,
    pub range: Range<usize>,
}

/// Stable, renderer-neutral metadata derived from a heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingInfo {
    pub level: u8,
    pub text: String,
    pub id: String,
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

    /// Source spans for top-level blocks. Ranges are always measured against
    /// the original source because parsing is strict and never rewrites it.
    pub fn block_ranges(&self) -> &[Range<usize>] {
        &self.block_ranges
    }

    pub fn headings(&self) -> &[HeadingInfo] {
        &self.headings
    }
}

impl Default for ParsedDocument {
    fn default() -> Self {
        parse_document("")
    }
}

pub fn parse_document(markdown: &str) -> ParsedDocument {
    let events = Parser::new_ext(markdown, parse_options())
        .into_offset_iter()
        .map(|(event, range)| SpannedEvent {
            event: event.into_static(),
            range,
        })
        .collect::<Vec<_>>();
    let mut builder = Builder::default();
    // 区间收集与建块走同一个事件循环：每个顶层块记录一条源码区间。脚注
    // 定义由 Builder 展开成"合成 `[label]` 段落 + N 个正文块"，全部块共享
    // 该定义的源码区间，从而保持 blocks()/block_ranges() 一一对应——此前
    // 固定压入两条区间，多块脚注会让后续所有块错位、整篇退化为全文源码
    // 编辑。
    let mut block_ranges = Vec::new();
    let mut depth = 0usize;
    let mut container_start = 0usize;
    let mut footnote_first_block: Option<usize> = None;
    for item in &events {
        if let Event::Start(tag) = &item.event {
            if depth == 0 {
                container_start = item.range.start;
                footnote_first_block =
                    matches!(tag, Tag::FootnoteDefinition(_)).then(|| builder.root.len());
            }
            depth += 1;
        }
        builder.push(&item.event);
        match &item.event {
            Event::Start(_) => {}
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let range = container_start..item.range.end;
                    match footnote_first_block.take() {
                        Some(first) => {
                            for _ in first..builder.root.len() {
                                block_ranges.push(range.clone());
                            }
                        }
                        None => block_ranges.push(range),
                    }
                }
            }
            Event::Rule if depth == 0 => block_ranges.push(item.range.clone()),
            _ => {}
        }
    }
    let blocks = builder.finish();
    ParsedDocument {
        source: markdown.to_string(),
        headings: collect_headings(&blocks),
        blocks,
        events,
        block_ranges,
    }
}

fn collect_headings(blocks: &[Block]) -> Vec<HeadingInfo> {
    fn visit(
        blocks: &[Block],
        result: &mut Vec<HeadingInfo>,
        used: &mut std::collections::HashMap<String, usize>,
    ) {
        for block in blocks {
            match block {
                Block::Heading { level, inlines } => {
                    let text = plain_of_inlines(inlines);
                    let base = heading_slug(&text);
                    let count = used.entry(base.clone()).or_insert(0);
                    *count += 1;
                    let id = if *count == 1 {
                        base
                    } else {
                        format!("{}-{}", base, count)
                    };
                    result.push(HeadingInfo {
                        level: *level,
                        text,
                        id,
                    });
                }
                Block::List { items, .. } => {
                    for item in items {
                        visit(item, result, used);
                    }
                }
                Block::Quote(children) => visit(children, result, used),
                _ => {}
            }
        }
    }

    let mut result = Vec::new();
    visit(blocks, &mut result, &mut std::collections::HashMap::new());
    result
}

fn heading_slug(text: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in text.trim().chars() {
        if ch.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.extend(ch.to_lowercase());
            pending_dash = false;
        } else {
            // Treat punctuation as a word boundary as well. This keeps ids
            // readable for Chinese headings such as “你好，世界” and makes
            // duplicate headings deterministic without preserving symbols.
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        "heading".to_string()
    } else {
        slug
    }
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
        ordered: bool,
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
    /// 借用事件即可：只有真正要保留的字符串才克隆，长文档每次按键不再
    /// 为整个事件流付一次额外克隆的开销。
    fn push(&mut self, event: &Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag_end) => self.end(tag_end),
            Event::Text(t) => self.text(t.to_string()),
            Event::Code(c) => self.inline(Inline::Code(c.to_string())),
            Event::SoftBreak => self.inline(Inline::SoftBreak),
            Event::HardBreak => self.inline(Inline::HardBreak),
            Event::Rule => self.block(Block::Rule),
            Event::TaskListMarker(checked) => {
                self.text(if *checked { "[x] " } else { "[ ] " }.to_string())
            }
            Event::Html(h) => match self.stack.last_mut() {
                Some(Frame::Raw { text }) | Some(Frame::Code { text, .. }) => text.push_str(h),
                _ => self.text(h.to_string()),
            },
            Event::FootnoteReference(name) => self.inline(Inline::Text(format!("[{name}]"))),
            _ => {}
        }
    }

    fn start(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Paragraph => self.stack.push(Frame::Paragraph {
                inlines: Vec::new(),
            }),
            Tag::Heading { level, .. } => self.stack.push(Frame::Heading {
                level: *level as u8,
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
                ordered: start.is_some(),
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
            } => self.push_inline_frame(InlineKind::Link, dest_url, title),
            Tag::Image {
                dest_url, title, ..
            } => self.push_inline_frame(InlineKind::Image, dest_url, title),
            Tag::FootnoteDefinition(name) => {
                self.block(Block::Paragraph(vec![Inline::Text(format!("[{name}]"))]))
            }
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

    fn end(&mut self, tag_end: &TagEnd) {
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
            TagEnd::HtmlBlock => {
                if let Some(Frame::Raw { text }) = self.stack.pop() {
                    self.block(Block::Raw(text));
                }
            }
            TagEnd::FootnoteDefinition => {}
            TagEnd::List(_) => {
                if let Some(Frame::List {
                    ordered,
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
                    ordered,
                    start,
                    mut items,
                    cur,
                } => {
                    if !cur.is_empty() {
                        items.push(cur);
                    }
                    self.root.push(Block::List {
                        ordered,
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
    fn strict_parser_keeps_source_and_block_ranges_for_adjacent_strong_text() {
        let source = "- **识别与生成：**区分植物";
        let document = parse_document(source);
        assert_eq!(document.source(), source);
        assert_eq!(document.block_ranges().len(), document.blocks().len());
        assert_eq!(plain_text(document.blocks()), "**识别与生成：**区分植物");
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
    fn 链接协议安全边界明确() {
        assert!(is_safe_link_destination("https://example.com"));
        assert!(is_safe_link_destination("notes/next.md"));
        assert!(is_safe_link_destination(r"C:\notes\next.md"));
        assert!(is_safe_link_destination("mailto:team@example.com"));
        assert!(!is_safe_link_destination("javascript:alert(1)"));
        assert!(!is_safe_link_destination("vbscript:msgbox(1)"));
        assert!(!is_safe_link_destination("data:text/html,<script>"));
    }

    #[test]
    fn 网络位置链接不会被当成相对路径() {
        // 协议相对 URL 与 UNC 路径都会交给系统打开网络位置。
        assert!(!is_safe_link_destination("//evil.example/share"));
        assert!(!is_safe_link_destination(r"\\evil\share"));
        assert!(!is_safe_link_destination("  //evil.example/share"));
        assert!(is_safe_link_destination("notes/next.md"));
        assert!(is_safe_link_destination(r"C:\notes\next.md"));
    }

    #[test]
    fn 前导空白与控制字符不能伪装成相对路径() {
        // Browsers strip leading/trailing C0+space and remove tabs/newlines
        // before resolving, so these must not pass as relative paths.
        assert!(!is_safe_link_destination(" javascript:alert(1)"));
        assert!(!is_safe_link_destination("\tjavascript:alert(1)"));
        assert!(!is_safe_link_destination("\njavascript:alert(1)"));
        assert!(!is_safe_link_destination("java\nscript:alert(1)"));
        assert!(!is_safe_link_destination("\u{1}javascript:alert(1)"));
        assert!(!is_safe_link_destination("  data:text/html,<script>  "));
        assert!(is_safe_link_destination("  https://example.com  "));
        assert!(is_safe_link_destination("notes/next.md"));
    }

    #[test]
    fn 脚注在桌面块模型中保留引用和正文() {
        let blocks = parse("正文[^1]\n\n[^1]: 说明文字\n");
        assert!(blocks.iter().all(|block| !matches!(block, Block::Raw(_))));
        assert_eq!(plain_text(&blocks), "正文[1]\n[1]\n说明文字");
    }

    #[test]
    fn 文档元数据提供稳定标题锚点() {
        let document =
            parse_document("# 你好，世界\n\n# 你好，世界\n\n```Rust,ignore\nlet x = 1;\n```\n");
        assert_eq!(
            document.headings(),
            &[
                HeadingInfo {
                    level: 1,
                    text: "你好，世界".into(),
                    id: "你好-世界".into(),
                },
                HeadingInfo {
                    level: 1,
                    text: "你好，世界".into(),
                    id: "你好-世界-2".into(),
                },
            ]
        );
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

    // A compact renderer-neutral fixture keeps the supported syntax profile explicit.
    const SYNTAX_PROFILE_FIXTURE: &str = r#"# 主题

段落含有 *强调*、**重点**、~~删除~~、`代码`、[链接](https://example.com "示例") 和 ![图](images/picture.png)。

> 引用内容

- [ ] 待办
- [x] 已完成

1. 第一项
2. 第二项

| 名称 | 数量 |
| :--- | ---: |
| 苹果 | 3 |

```rust
fn main() {}
```

脚注引用[^note]

[^note]: 脚注正文
"#;

    #[test]
    fn syntax_profile_fixture_preserves_core_ast_shape() {
        let document = parse_document(SYNTAX_PROFILE_FIXTURE);
        let blocks = document.blocks();
        assert!(matches!(
            blocks.first(),
            Some(Block::Heading { level: 1, inlines }) if plain_of_inlines(inlines) == "主题"
        ));
        assert!(matches!(
            blocks.get(1),
            Some(Block::Paragraph(inlines))
                if inlines.iter().any(|inline| matches!(inline, Inline::Emphasis(_)))
                    && inlines.iter().any(|inline| matches!(inline, Inline::Strong(_)))
                    && inlines.iter().any(|inline| matches!(inline, Inline::Strikethrough(_)))
                    && inlines.iter().any(|inline| matches!(inline, Inline::Code(_)))
                    && inlines.iter().any(|inline| matches!(inline, Inline::Link { url, title, .. }
                        if url == "https://example.com" && title == "示例"))
                    && inlines.iter().any(|inline| matches!(inline, Inline::Image { url, alt }
                        if url == "images/picture.png" && alt == "图"))
        ));
        assert!(matches!(
            blocks.get(2),
            Some(Block::Quote(children)) if children.len() == 1
                && plain_text(children) == "引用内容"
        ));
        assert!(matches!(
            blocks.get(3),
            Some(Block::List { ordered: false, start: 1, items }) if items.len() == 2
        ));
        assert!(matches!(
            blocks.get(4),
            Some(Block::List { ordered: true, start: 1, items }) if items.len() == 2
        ));
        assert!(matches!(
            blocks.get(5),
            Some(Block::Table { headers, rows }) if headers.len() == 2
                && rows.len() == 1
                && plain_of_inlines(&rows[0][0]) == "苹果"
                && plain_of_inlines(&rows[0][1]) == "3"
        ));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Code { lang, text } if lang == "rust" && text.contains("fn main()")
        )));
        assert!(plain_text(blocks).contains("脚注引用"));
        assert_eq!(
            document.headings(),
            &[HeadingInfo {
                level: 1,
                text: "主题".to_string(),
                id: "主题".to_string(),
            }]
        );
        assert!(
            document
                .events()
                .iter()
                .all(|event| event.range.start <= event.range.end)
        );
    }

    #[test]
    fn heading_after_image_keeps_heading_semantics() {
        let document = parse_document(
            "![图](images/picture.png)\n\n## 冻结事实，避免处理中途换数据\n\n正文\n",
        );
        assert!(matches!(
            document.blocks().get(1),
            Some(Block::Heading { level: 2, inlines })
                if plain_of_inlines(inlines) == "冻结事实，避免处理中途换数据"
        ));
    }

    #[test]
    fn syntax_profile_fixture_is_deterministic_for_ast_and_events() {
        let first = parse_document(SYNTAX_PROFILE_FIXTURE);
        let second = parse_document(SYNTAX_PROFILE_FIXTURE);
        assert_eq!(first.blocks(), second.blocks());
        assert_eq!(
            first
                .events()
                .iter()
                .map(|item| item.range.clone())
                .collect::<Vec<_>>(),
            second
                .events()
                .iter()
                .map(|item| item.range.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(first.source(), SYNTAX_PROFILE_FIXTURE);
        assert_eq!(first.source(), second.source());
    }

    #[test]
    fn top_level_block_ranges_cover_original_source() {
        let source = "# 标题\n\n第一段。\n\n第二段。\n";
        let document = parse_document(source);
        let ranges = document.block_ranges();
        assert_eq!(ranges.len(), document.blocks().len());
        assert_eq!(&source[ranges[0].clone()], "# 标题\n");
        assert_eq!(&source[ranges[1].clone()], "第一段。\n");
        assert_eq!(&source[ranges[2].clone()], "第二段。\n");
    }

    #[test]
    fn top_level_block_ranges_include_rules() {
        let document = parse_document("前文\n\n---\n\n后文\n");
        assert_eq!(document.block_ranges().len(), document.blocks().len());
    }

    #[test]
    fn footnote_block_ranges_cover_synthetic_marker_and_body() {
        let source = "正文[^1]\n\n[^1]: 说明文字\n";
        let document = parse_document(source);
        assert_eq!(document.block_ranges().len(), document.blocks().len());
        assert_eq!(document.blocks().len(), 3);
        assert_eq!(
            &source[document.block_ranges()[1].clone()],
            "[^1]: 说明文字\n"
        );
        assert_eq!(
            &source[document.block_ranges()[2].clone()],
            "[^1]: 说明文字\n"
        );
    }

    #[test]
    fn 多块脚注保持块与源码区间一一对应() {
        let source = "正文[^1]\n\n[^1]: 第一段\n\n    第二段缩进\n\n后续段落\n";
        let document = parse_document(source);
        // 之前：脚注区间固定只压两条，后续段落的区间错位，预览整体退化为
        // 全文源码编辑模式。
        assert_eq!(document.block_ranges().len(), document.blocks().len());
        assert_eq!(document.blocks().len(), 5);
        let ranges = document.block_ranges();
        let after = source.find("后续段落").expect("源码应包含后续段落");
        assert_eq!(ranges[4].start, after, "最后一块的区间必须指向它自己");
        assert!(
            ranges[1..4]
                .iter()
                .all(|range| range.start < after && range.end <= after),
            "脚注的 3 个块（标记 + 两段正文）共享同一定义区间"
        );
    }

    #[test]
    fn empty_footnote_does_not_create_a_spurious_range() {
        let source = "正文[^1]\n\n[^1]:\n";
        let document = parse_document(source);
        assert_eq!(document.block_ranges().len(), document.blocks().len());
    }

    #[test]
    fn nested_rules_do_not_create_extra_top_level_ranges() {
        let document = parse_document("> 前文\n>\n> ---\n\n- 项目\n\n  ---\n");
        assert_eq!(document.block_ranges().len(), document.blocks().len());
    }

    #[test]
    fn malformed_markdown_remains_bounded_and_repeatable() {
        let source = "# 未闭合 **强调\n\n- [x] 未结束列表\n\n```rust\n<&>\n";
        let first = parse_document(source);
        let second = parse_document(source);
        assert_eq!(first.blocks(), second.blocks());
        assert_eq!(first.events().len(), second.events().len());
        assert!(
            first
                .events()
                .iter()
                .all(|event| event.range.end <= source.len())
        );
    }
}
