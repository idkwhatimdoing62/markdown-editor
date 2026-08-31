//! 把块模型渲染到 egui 预览区。

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId, Stroke};

use crate::markdown::{Block, Inline};
use crate::theme::{HeadingStyle, ThemeSpec};

/// 加粗使用独立字体族。egui 0.35 没有字重概念，粗体必须换字体族实现。
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

#[cfg(test)]
pub fn show_preview(ui: &mut egui::Ui, blocks: &[Block]) {
    show_preview_with_theme(ui, blocks, 15.5, &ThemeSpec::fallback(false));
}

pub fn show_preview_with_theme(
    ui: &mut egui::Ui,
    blocks: &[Block],
    body_size: f32,
    theme: &ThemeSpec,
) {
    if blocks.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new("无内容").weak().size(18.0));
        });
        return;
    }

    let mut table_id = 0usize;
    for block in blocks {
        show_block(ui, block, &mut table_id, body_size, theme);
        ui.add_space(theme.block_spacing);
    }
}

fn show_block(
    ui: &mut egui::Ui,
    block: &Block,
    table_id: &mut usize,
    body_size: f32,
    theme: &ThemeSpec,
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
            show_heading(ui, inlines, size, *level, theme);
        }
        Block::Paragraph(inlines) => {
            show_inlines_block(ui, inlines, body_size, false, true, theme.line_height)
        }
        Block::List {
            ordered,
            start,
            items,
        } => {
            let mut idx = *start;
            for item in items {
                let (task, blocks) = strip_task_marker(item);
                ui.horizontal_top(|ui| {
                    let marker = if *ordered {
                        format!("{}.", idx)
                    } else if let Some(checked) = task {
                        if checked { "[x]" } else { "[ ]" }.to_string()
                    } else {
                        "•".to_string()
                    };
                    ui.label(egui::RichText::new(marker).strong());
                    ui.vertical(|ui| {
                        for b in &blocks {
                            show_block(ui, b, table_id, body_size, theme);
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
                    ui.set_min_width((ui.available_width() - 34.0).max(120.0));
                    if !lang.is_empty() {
                        ui.label(
                            egui::RichText::new(lang.to_uppercase())
                                .color(theme.muted)
                                .size(10.0),
                        );
                        ui.add_space(5.0);
                    }
                    ui.label(
                        egui::RichText::new(text)
                            .monospace()
                            .size((body_size - 2.0).max(11.0)),
                    );
                });
        }
        Block::Quote(blocks) => {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(10, 4))
                .fill(theme.quote_bg)
                .stroke(Stroke::new(3.0, theme.accent.gamma_multiply(0.7)))
                .corner_radius(match theme.heading_style {
                    HeadingStyle::Plain => 5,
                    HeadingStyle::Card => 12,
                    HeadingStyle::Tech => 2,
                })
                .show(ui, |ui| {
                    for b in blocks {
                        show_block(ui, b, table_id, body_size, theme);
                    }
                });
        }
        Block::Table { headers, rows } => {
            let cols = headers
                .len()
                .max(rows.iter().map(|r| r.len()).max().unwrap_or(0))
                .max(1);
            ui.scope(|ui| {
                ui.visuals_mut().faint_bg_color = theme.table_alt;
                egui::Grid::new(ui.id().with(("md_table", *table_id)))
                    .num_columns(cols)
                    .striped(true)
                    .min_col_width(90.0)
                    .spacing(theme.table_spacing)
                    .show(ui, |ui| {
                        for header in headers {
                            show_inlines_block(ui, header, body_size - 1.0, true, false, 1.5);
                        }
                        ui.end_row();
                        for row in rows {
                            for cell in row {
                                show_inlines_block(ui, cell, body_size - 1.0, false, false, 1.5);
                            }
                            ui.end_row();
                        }
                    });
            });
            *table_id += 1;
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

fn show_heading(ui: &mut egui::Ui, inlines: &[Inline], size: f32, level: u8, theme: &ThemeSpec) {
    match theme.heading_style {
        HeadingStyle::Plain => {
            ui.scope(|ui| {
                ui.visuals_mut().override_text_color = Some(theme.heading);
                show_inlines_block(ui, inlines, size, true, true, 1.25);
            });
            if level == 2 {
                ui.add_space(3.0);
                ui.separator();
            }
        }
        HeadingStyle::Card if level <= 2 => {
            egui::Frame::new()
                .fill(theme.quote_bg)
                .inner_margin(egui::Margin::symmetric(16, if level == 1 { 13 } else { 9 }))
                .corner_radius(if level == 1 { 12 } else { 8 })
                .stroke(Stroke::new(1.0, theme.border))
                .show(ui, |ui| {
                    ui.set_min_width((ui.available_width() - 34.0).max(120.0));
                    ui.visuals_mut().override_text_color = Some(theme.heading);
                    show_inlines_block(ui, inlines, size, true, true, 1.25);
                });
        }
        HeadingStyle::Card => {
            ui.horizontal(|ui| {
                ui.colored_label(theme.accent, "●");
                ui.visuals_mut().override_text_color = Some(theme.heading);
                show_inlines_block(ui, inlines, size, true, true, 1.3);
            });
        }
        HeadingStyle::Tech => {
            egui::Frame::new()
                .fill(if level <= 2 {
                    theme.accent.gamma_multiply(0.08)
                } else {
                    Color32::TRANSPARENT
                })
                .inner_margin(egui::Margin::symmetric(if level <= 2 { 12 } else { 0 }, 7))
                .corner_radius(2)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(theme.accent, if level == 1 { "▌" } else { "›" });
                        ui.visuals_mut().override_text_color = Some(if level <= 2 {
                            theme.accent
                        } else {
                            theme.heading
                        });
                        show_inlines_block(ui, inlines, size, true, true, 1.25);
                    });
                });
        }
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
    let mut links = Vec::new();
    let job = inlines_to_job(inlines, &base, ui.visuals().hyperlink_color, &mut links);
    let mut label = egui::Label::new(job).wrap();
    if clickable {
        label = label.sense(egui::Sense::click());
    }
    let resp = ui.add(label);
    if resp.clicked()
        && let Some(url) = links.first()
    {
        ui.ctx().open_url(egui::OpenUrl {
            url: url.clone(),
            new_tab: true,
        });
    }
}

fn inlines_to_job(
    inlines: &[Inline],
    base: &TextFormat,
    link_color: Color32,
    links: &mut Vec<String>,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    push_inlines(&mut job, inlines, base, link_color, links);
    job
}

fn push_inlines(
    job: &mut LayoutJob,
    inlines: &[Inline],
    base: &TextFormat,
    link_color: Color32,
    links: &mut Vec<String>,
) {
    for inline in inlines {
        match inline {
            Inline::Text(t) => job.append(t, 0.0, base.clone()),
            Inline::Emphasis(children) => {
                let mut f = base.clone();
                f.italics = true;
                push_inlines(job, children, &f, link_color, links);
            }
            Inline::Strong(children) => {
                let mut f = base.clone();
                f.font_id.family = bold_family();
                push_inlines(job, children, &f, link_color, links);
            }
            Inline::Strikethrough(children) => {
                let mut f = base.clone();
                f.strikethrough = Stroke::new(1.0, f.color);
                push_inlines(job, children, &f, link_color, links);
            }
            Inline::Code(c) => {
                let mut f = base.clone();
                f.font_id.family = FontFamily::Monospace;
                job.append(c, 0.0, f);
            }
            Inline::Link { url, children, .. } => {
                links.push(url.clone());
                let mut f = base.clone();
                f.underline = Stroke::new(1.0, f.color);
                f.color = link_color;
                push_inlines(job, children, &f, link_color, links);
            }
            Inline::Image { alt, .. } => {
                let mut f = base.clone();
                f.color = f.color.gamma_multiply(0.6);
                job.append(&format!("[图片] {alt}"), 0.0, f);
            }
            Inline::SoftBreak => job.append(" ", 0.0, base.clone()),
            Inline::HardBreak => job.append("\n", 0.0, base.clone()),
        }
    }
}

/// 任务列表项：从第一段去掉 `[x] ` / `[ ] ` 前缀，返回勾选状态和剩余块。
fn strip_task_marker(item: &[Block]) -> (Option<bool>, Vec<Block>) {
    let mut blocks = item.to_vec();
    if let Some(Block::Paragraph(inlines)) = blocks.first_mut()
        && let Some(Inline::Text(t)) = inlines.first_mut()
    {
        let state = if let Some(rest) = t.strip_prefix("[x] ") {
            *t = rest.to_string();
            Some(true)
        } else if let Some(rest) = t.strip_prefix("[ ] ") {
            *t = rest.to_string();
            Some(false)
        } else {
            None
        };
        if state.is_some() && t.is_empty() {
            inlines.remove(0);
        }
        if let Some(s) = state {
            return (Some(s), blocks);
        }
    }
    (None, blocks)
}

#[cfg(test)]
mod tests {
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

        assert!(get("[x]").x < get("checked").x);
        assert!(get("[ ]").x < get("unchecked").x);
        assert!(
            !runs.iter().any(|r| r.text.trim_start().starts_with("• [")),
            "任务标记不应跟在圆点后面"
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
}
