//! Small, dependency-free rewriting for image sources inside raw HTML events.

use std::borrow::Cow;
use std::ops::Range;

pub fn rewrite_sources(html: &str, mut rewrite: impl FnMut(&str) -> Option<String>) -> String {
    let mut replacements = Vec::<(Range<usize>, String)>::new();
    let bytes = html.as_bytes();
    let mut cursor = 0usize;

    while let Some(relative) = html[cursor..].find('<') {
        let tag_start = cursor + relative;
        if html[tag_start..].starts_with("<!--") {
            cursor = html[tag_start + 4..]
                .find("-->")
                .map(|end| tag_start + 4 + end + 3)
                .unwrap_or(html.len());
            continue;
        }

        let name_start = tag_start + 1;
        if name_start + 3 > bytes.len()
            || !html[name_start..name_start + 3].eq_ignore_ascii_case("img")
            || bytes
                .get(name_start + 3)
                .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'/' && *byte != b'>')
        {
            cursor = name_start;
            continue;
        }

        let Some(tag_end) = html_tag_end(html, name_start + 3) else {
            break;
        };
        if let Some(attribute) = attribute_value(html, name_start + 3, tag_end, "src")
            && let Some(destination) = rewrite(&decode_attribute(&html[attribute.value.clone()]))
        {
            let replacement = match attribute.quote {
                Some(quote) => escape_for_quote(&destination, quote),
                None => format!("\"{}\"", escape_for_quote(&destination, b'\"')),
            };
            replacements.push((attribute.value, replacement));
        }
        if let Some(width) =
            attribute_value(html, name_start + 3, tag_end, "width").and_then(|attribute| {
                decode_attribute(&html[attribute.value])
                    .trim()
                    .parse::<u32>()
                    .ok()
            })
        {
            let width_declaration = format!("width:{width}px");
            if let Some(style) = attribute_value(html, name_start + 3, tag_end, "style") {
                if !style_declares_width(&decode_attribute(&html[style.value.clone()])) {
                    replacements.push((
                        style.value.end..style.value.end,
                        format!(";{width_declaration}"),
                    ));
                }
            } else {
                let insertion = attribute_insertion_position(html, tag_end);
                let separator =
                    if insertion > 0 && html.as_bytes()[insertion - 1].is_ascii_whitespace() {
                        ""
                    } else {
                        " "
                    };
                replacements.push((
                    insertion..insertion,
                    format!(r#"{separator}style="{width_declaration};max-width:100%""#),
                ));
            }
        }
        cursor = tag_end + 1;
    }

    if replacements.is_empty() {
        return html.to_string();
    }
    replacements.sort_by_key(|(range, _)| range.start);
    let additional = replacements
        .iter()
        .map(|(range, value)| value.len().saturating_sub(range.len()))
        .sum::<usize>();
    let mut output = String::with_capacity(html.len() + additional);
    let mut copied = 0usize;
    for (range, value) in replacements {
        output.push_str(&html[copied..range.start]);
        output.push_str(&value);
        copied = range.end;
    }
    output.push_str(&html[copied..]);
    output
}

struct AttributeValue {
    value: Range<usize>,
    quote: Option<u8>,
}

fn html_tag_end(html: &str, mut cursor: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut quote = None;
    while cursor < bytes.len() {
        match (quote, bytes[cursor]) {
            (None, b'\'' | b'\"') => quote = Some(bytes[cursor]),
            (Some(open), current) if open == current => quote = None,
            (None, b'>') => return Some(cursor),
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn attribute_value(
    html: &str,
    mut cursor: usize,
    tag_end: usize,
    wanted_name: &str,
) -> Option<AttributeValue> {
    let bytes = html.as_bytes();
    while cursor < tag_end {
        while cursor < tag_end && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b'/') {
            cursor += 1;
        }
        let name_start = cursor;
        while cursor < tag_end && is_attribute_name_byte(bytes[cursor]) {
            cursor += 1;
        }
        if cursor == name_start {
            cursor += 1;
            continue;
        }
        let is_wanted = html[name_start..cursor].eq_ignore_ascii_case(wanted_name);
        while cursor < tag_end && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= tag_end || bytes[cursor] != b'=' {
            continue;
        }
        cursor += 1;
        while cursor < tag_end && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= tag_end {
            return None;
        }
        let quote = matches!(bytes[cursor], b'\'' | b'\"').then_some(bytes[cursor]);
        if quote.is_some() {
            cursor += 1;
        }
        let value_start = cursor;
        if let Some(quote) = quote {
            while cursor < tag_end && bytes[cursor] != quote {
                cursor += 1;
            }
        } else {
            while cursor < tag_end && !bytes[cursor].is_ascii_whitespace() && bytes[cursor] != b'>'
            {
                cursor += 1;
            }
        }
        let value = value_start..cursor;
        if is_wanted {
            return Some(AttributeValue { value, quote });
        }
        if quote.is_some() && cursor < tag_end {
            cursor += 1;
        }
    }
    None
}

fn style_declares_width(style: &str) -> bool {
    style.split(';').any(|declaration| {
        declaration
            .split_once(':')
            .is_some_and(|(property, _)| property.trim().eq_ignore_ascii_case("width"))
    })
}

fn attribute_insertion_position(html: &str, tag_end: usize) -> usize {
    let bytes = html.as_bytes();
    let mut insertion = tag_end;
    while insertion > 0 && bytes[insertion - 1].is_ascii_whitespace() {
        insertion -= 1;
    }
    if insertion > 0 && bytes[insertion - 1] == b'/' {
        insertion -= 1;
    }
    insertion
}

fn is_attribute_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
}

fn decode_attribute(value: &str) -> Cow<'_, str> {
    if !value.contains('&') {
        return Cow::Borrowed(value);
    }
    let entities = [
        ("&amp;", "&"),
        ("&#38;", "&"),
        ("&#x26;", "&"),
        ("&quot;", "\""),
        ("&#34;", "\""),
        ("&#x22;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&#x27;", "'"),
    ];
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0usize;
    while let Some(relative) = value[cursor..].find('&') {
        let entity_start = cursor + relative;
        output.push_str(&value[cursor..entity_start]);
        if let Some((entity, decoded)) = entities
            .iter()
            .find(|(entity, _)| value[entity_start..].starts_with(entity))
        {
            output.push_str(decoded);
            cursor = entity_start + entity.len();
        } else {
            output.push('&');
            cursor = entity_start + 1;
        }
    }
    output.push_str(&value[cursor..]);
    Cow::Owned(output)
}

fn escape_for_quote(value: &str, quote: u8) -> String {
    let escaped = value.replace('&', "&amp;");
    if quote == b'\"' {
        escaped.replace('\"', "&quot;")
    } else {
        escaped.replace('\'', "&#39;")
    }
}

#[cfg(test)]
mod tests {
    use super::rewrite_sources;

    #[test]
    fn rewrites_only_img_src_and_preserves_other_attributes() {
        let input = r#"<img class="hero" src='./图 1.png' alt="示例" width="720">"#;
        let output = rewrite_sources(input, |source| Some(format!("local:{source}")));
        assert_eq!(
            output,
            r#"<img class="hero" src='local:./图 1.png' alt="示例" width="720" style="width:720px;max-width:100%">"#
        );
    }

    #[test]
    fn supports_uppercase_and_unquoted_src() {
        let output = rewrite_sources("<IMG SRC=pic.png>", |source| {
            Some(format!("local:{source}"))
        });
        assert_eq!(output, r#"<IMG SRC="local:pic.png">"#);
    }

    #[test]
    fn leaves_comments_and_rejected_urls_unchanged() {
        let input = r#"<!-- <img src="hidden.png"> --><img src="https://example.com/a.png">"#;
        let output = rewrite_sources(input, |_| None);
        assert_eq!(output, input);
    }

    #[test]
    fn decodes_html_entities_once_before_resolving_the_path() {
        let output = rewrite_sources(r#"<img src="a&amp;b&amp;quot;.png">"#, |source| {
            Some(format!("local:{source}"))
        });
        assert_eq!(output, r#"<img src="local:a&amp;b&amp;quot;.png">"#);
    }

    #[test]
    fn existing_inline_width_takes_precedence_over_width_attribute() {
        let input = r#"<img style="display:block;width:50%" src="a.png" width="720">"#;
        let output = rewrite_sources(input, |_| None);
        assert_eq!(output, input);
    }

    #[test]
    fn appends_width_to_existing_style_without_a_width_declaration() {
        let input = r#"<img style="display:block" src="a.png" width="720">"#;
        let output = rewrite_sources(input, |_| None);
        assert_eq!(
            output,
            r#"<img style="display:block;width:720px" src="a.png" width="720">"#
        );
    }

    #[test]
    fn inserts_style_before_the_self_closing_slash() {
        let output = rewrite_sources(r#"<img src="a.png" width="320" />"#, |_| None);
        assert_eq!(
            output,
            r#"<img src="a.png" width="320" style="width:320px;max-width:100%"/>"#
        );
    }
}
