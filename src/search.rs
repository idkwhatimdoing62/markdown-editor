//! Literal full-document search with character-index ranges for the editor.

use std::ops::Range;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SearchResults {
    ranges: Vec<Range<usize>>,
    current: Option<usize>,
}

impl SearchResults {
    pub fn new(text: &str, query: &str) -> Self {
        if query.is_empty() {
            return Self::default();
        }
        let byte_ranges = if query.is_ascii() {
            ascii_case_insensitive_ranges(text, query)
        } else {
            text.match_indices(query)
                .map(|(start, matched)| start..start + matched.len())
                .collect::<Vec<_>>()
        };
        let ranges = byte_ranges_to_char_ranges(text, &byte_ranges);
        let current = (!ranges.is_empty()).then_some(0);
        Self { ranges, current }
    }

    pub fn ranges(&self) -> &[Range<usize>] {
        &self.ranges
    }

    pub fn current_range(&self) -> Option<Range<usize>> {
        self.current.map(|index| self.ranges[index].clone())
    }

    pub fn position(&self) -> Option<(usize, usize)> {
        self.current.map(|index| (index + 1, self.ranges.len()))
    }

    pub fn next(&mut self) -> Option<Range<usize>> {
        if self.ranges.is_empty() {
            return None;
        }
        self.current = Some((self.current.unwrap_or(0) + 1) % self.ranges.len());
        self.current_range()
    }

    pub fn previous(&mut self) -> Option<Range<usize>> {
        if self.ranges.is_empty() {
            return None;
        }
        let current = self.current.unwrap_or(0);
        self.current = Some((current + self.ranges.len() - 1) % self.ranges.len());
        self.current_range()
    }
}

fn ascii_case_insensitive_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    let haystack = text.as_bytes();
    let needle = query.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start + needle.len() <= haystack.len() {
        if haystack[start..start + needle.len()].eq_ignore_ascii_case(needle) {
            ranges.push(start..start + needle.len());
            start += needle.len();
        } else {
            start += 1;
        }
    }
    ranges
}

fn byte_ranges_to_char_ranges(text: &str, byte_ranges: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut ranges = Vec::with_capacity(byte_ranges.len());
    let mut next = 0usize;
    let mut start_char = None;
    let mut total_chars = 0usize;
    for (char_index, (byte_index, _)) in text.char_indices().enumerate() {
        total_chars = char_index + 1;
        if next >= byte_ranges.len() {
            continue;
        }
        if byte_index == byte_ranges[next].start {
            start_char = Some(char_index);
        }
        if byte_index == byte_ranges[next].end {
            ranges.push(start_char.take().expect("匹配起点应为字符边界")..char_index);
            next += 1;
            if next < byte_ranges.len() && byte_index == byte_ranges[next].start {
                start_char = Some(char_index);
            }
        }
    }
    if next < byte_ranges.len() && byte_ranges[next].end == text.len() {
        ranges.push(start_char.expect("结尾匹配应有起点")..total_chars);
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 全文查找返回所有中文匹配的字符位置() {
        let results = SearchResults::new("甲搜索乙，搜索丙", "搜索");

        assert_eq!(results.ranges(), &[1..3, 5..7]);
        assert_eq!(results.current_range(), Some(1..3));
    }

    #[test]
    fn 下一项在末尾循环回第一项() {
        let mut results = SearchResults::new("one two one", "one");

        assert_eq!(results.next(), Some(8..11));
        assert_eq!(results.next(), Some(0..3));
    }

    #[test]
    fn 上一项在开头循环到最后一项() {
        let mut results = SearchResults::new("one two one", "one");

        assert_eq!(results.previous(), Some(8..11));
        assert_eq!(results.previous(), Some(0..3));
    }

    #[test]
    fn 当前位置使用面向用户的一基序号() {
        let mut results = SearchResults::new("甲甲甲", "甲");

        assert_eq!(results.position(), Some((1, 3)));
        results.next();
        assert_eq!(results.position(), Some((2, 3)));
    }

    #[test]
    fn 英文查找忽略ascii大小写() {
        let results = SearchResults::new("Rust rust RUST", "rust");

        assert_eq!(results.ranges(), &[0..4, 5..9, 10..14]);
    }
}
