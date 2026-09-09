use eframe::egui;

use crate::markdown::{self, Block};

pub const READING_TOC_WIDTH: f32 = 244.0;
const TOC_INDENT: f32 = 14.0;
const TOC_MIN_ROW_WIDTH: f32 = 80.0;

fn heading_title(inlines: &[markdown::Inline]) -> String {
    fn append(inlines: &[markdown::Inline], output: &mut String) {
        for inline in inlines {
            match inline {
                markdown::Inline::Text(text) | markdown::Inline::Code(text) => {
                    output.push_str(text)
                }
                markdown::Inline::Emphasis(children)
                | markdown::Inline::Strong(children)
                | markdown::Inline::Strikethrough(children)
                | markdown::Inline::Link { children, .. } => append(children, output),
                markdown::Inline::Image { alt, .. } => output.push_str(alt),
                markdown::Inline::SoftBreak | markdown::Inline::HardBreak => output.push(' '),
            }
        }
    }
    let mut title = String::new();
    append(inlines, &mut title);
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn reading_headings(blocks: &[Block]) -> Vec<(u8, String)> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Heading { level, inlines } => Some((*level, heading_title(inlines))),
            _ => None,
        })
        .filter(|(_, title)| !title.is_empty())
        .collect()
}

fn heading_indent(level: u8) -> f32 {
    level.saturating_sub(1) as f32 * TOC_INDENT
}

fn reading_toc_rows(headings: &[(u8, String)], available_width: f32) -> Vec<(u8, &str, f32, f32)> {
    headings
        .iter()
        .map(|(level, title)| {
            let indent = heading_indent(*level);
            (
                *level,
                title.as_str(),
                indent,
                (available_width - indent).max(TOC_MIN_ROW_WIDTH),
            )
        })
        .collect()
}

pub(crate) fn reading_toc(ui: &mut egui::Ui, blocks: &[Block]) -> Option<usize> {
    let headings = reading_headings(blocks);
    ui.add_space(18.0);
    ui.label(
        egui::RichText::new("章节目录")
            .size(14.0)
            .strong()
            .color(ui.visuals().strong_text_color()),
    );
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(5.0);
    if headings.is_empty() {
        ui.label(
            egui::RichText::new("当前文档没有标题")
                .size(13.0)
                .color(ui.visuals().weak_text_color()),
        );
        return None;
    }
    let mut target = None;
    let rows = reading_toc_rows(&headings, ui.available_width());
    egui::ScrollArea::vertical()
        .id_salt("reading_toc_scroll")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            for (index, (level, title, indent, row_width)) in rows.iter().enumerate() {
                let text_color = if *level <= 1 {
                    ui.visuals().strong_text_color()
                } else {
                    ui.visuals().weak_text_color()
                };
                ui.horizontal(|ui| {
                    ui.add_space(*indent);
                    let response = ui
                        .add_sized(
                            [*row_width, 28.0],
                            egui::Button::new(())
                                .left_text(egui::RichText::new(*title).size(13.0).color(text_color))
                                .frame(false)
                                .truncate(),
                        )
                        .on_hover_text(*title);
                    if response.clicked() {
                        target = Some(index);
                    }
                    let guide_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
                    let accent_color = ui.visuals().selection.stroke.color;
                    if *level == 1 {
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(
                                egui::pos2(response.rect.left() - 8.0, response.rect.top() + 5.0),
                                egui::vec2(2.0, response.rect.height() - 10.0),
                            ),
                            egui::CornerRadius::same(1),
                            accent_color,
                        );
                    } else {
                        let guide_x = response.rect.left() - 8.0;
                        ui.painter().line_segment(
                            [
                                egui::pos2(guide_x, response.rect.top()),
                                egui::pos2(guide_x, response.rect.bottom()),
                            ],
                            egui::Stroke::new(1.0, guide_color),
                        );
                        ui.painter().circle_filled(
                            egui::pos2(guide_x, response.rect.center().y),
                            if *level == 2 { 2.0 } else { 1.5 },
                            if *level == 2 {
                                accent_color
                            } else {
                                guide_color
                            },
                        );
                    }
                });
            }
        });
    target
}

#[cfg(test)]
mod tests {
    use super::{heading_indent, reading_toc_rows};
    #[test]
    fn toc_layout_uses_level_relationships() {
        assert_eq!(heading_indent(1), 0.0);
        assert_eq!(
            heading_indent(2) - heading_indent(1),
            heading_indent(3) - heading_indent(2)
        );
        let headings = [(2, "Short".into()), (2, "Long".into())];
        let rows = reading_toc_rows(&headings, 1000.0);
        assert_eq!(rows[0].2, rows[1].2);
        assert_eq!(rows[0].3, rows[1].3);
    }
}
