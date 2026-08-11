//! 把 Markdown 解析为可渲染的块模型。

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub fn parse_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
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

pub fn parse(markdown: &str) -> Vec<Block> {
    let parser = Parser::new_ext(markdown, parse_options());
    let mut builder = Builder::default();
    for event in parser {
        builder.push(event);
    }
    builder.finish()
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
                if let Some(Frame::Paragraph { inlines }) = self.stack.pop() {
                    if !inlines.is_empty() {
                        self.block(Block::Paragraph(inlines));
                    }
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
                if let Some(Frame::List { cur, items, .. }) = self.stack.last_mut() {
                    if !cur.is_empty() {
                        items.push(std::mem::take(cur));
                    }
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
}
