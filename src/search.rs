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
        let byte_ranges = case_insensitive_byte_ranges(text, query);
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

/// ASCII 大小写不敏感的字节级子串搜索（Horspool 坏字符跳跃 + 逐字节验证）。
///
/// 非 ASCII 字节按原值精确比较：完整的 Unicode 大小写折叠会改变字节长度、
/// 无法保持偏移稳定，所以 `café` 匹配不到 `CAFÉ`（重音字符仍区分大小写），
/// 但 ASCII 字母部分一律忽略大小写。UTF-8 下字节级精确匹配天然落在字符
/// 边界上：needle 的非 ASCII 字节序列只可能命中相同的合法序列。
fn case_insensitive_byte_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    let haystack = text.as_bytes();
    let needle = query.as_bytes();
    let mut ranges = Vec::new();
    let m = needle.len();
    if m == 0 || m > haystack.len() {
        return ranges;
    }
    let fold = |byte: u8| byte.to_ascii_lowercase();
    // 坏字符表：窗口末字节（折叠后）在 needle 中最后一次出现的相对位置
    // 决定下次窗口起点，天然文本上通常亚线性。
    let mut skip = [m; 256];
    for (index, byte) in needle[..m - 1].iter().enumerate() {
        skip[fold(*byte) as usize] = m - 1 - index;
    }
    let mut position = 0usize;
    while position + m <= haystack.len() {
        if haystack[position..position + m].eq_ignore_ascii_case(needle) {
            ranges.push(position..position + m);
            position += m; // 匹配互不重叠
        } else {
            position += skip[fold(haystack[position + m - 1]) as usize];
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

    #[test]
    fn 混合大小写查询同样忽略ascii大小写() {
        let results = SearchResults::new("Rust rust RUST", "RuSt");

        assert_eq!(results.ranges(), &[0..4, 5..9, 10..14]);
    }

    #[test]
    fn 非ascii查询走同一条匹配路径() {
        let results = SearchResults::new("甲搜索乙，搜索丙", "搜索");

        assert_eq!(results.ranges(), &[1..3, 5..7]);
    }

    #[test]
    fn utf8续字节不会被误认成匹配() {
        // "é" = 0xC3 0xA9；任何多字节字符内部都不会被 ASCII 查询命中。
        let results = SearchResults::new("café café", "caf");

        assert_eq!(results.ranges(), &[0..3, 5..8]);
    }
}
