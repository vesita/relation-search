//! 检索结果摘要（snippet）：优先 tantivy SnippetGenerator；纯 ASCII 查询命中 CJK 正文时
//! 用廉价窗口，避免为摘要触发 jieba 初始化。

use tantivy::snippet::SnippetGenerator;

use crate::tokenizer::tokenize_to_strings;

/// 用 tantivy 的 SnippetGenerator 生成摘要片段；取不到片段时退化为「取前 240 字符」。
pub(super) fn snippet_text(generator: &SnippetGenerator, content: &str) -> String {
    let snippet = generator.snippet(content);
    let fragment = snippet.fragment();
    if fragment.is_empty() {
        truncate_chars(content, 240)
    } else {
        fragment.to_string()
    }
}

/// 廉价摘要：按查询词在正文里的首个出现位置取一个 ~240 字符窗口。
///
/// 用于「纯 ASCII 查询 + CJK 正文」：此时走 `SnippetGenerator` 会为分词 CJK 正文触发
/// jieba 初始化（约 60ms），而它要做的也只是定位同样的查询词，用不着词典。
pub(super) fn cheap_snippet(content: &str, query: &str) -> String {
    for token in tokenize_to_strings(query) {
        if let Some(pos) = find_ci(content, &token) {
            return window_around(content, pos, 240, 80);
        }
    }
    truncate_chars(content, 240)
}

/// 大小写不敏感查找，返回在 hay 里的字节位置。
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    hay.to_lowercase().find(&needle.to_lowercase())
}

/// 以 pos 为中心向前 back 字节起、总长约 len 字节取窗口（对齐 UTF-8 边界）。
fn window_around(content: &str, pos: usize, len: usize, back: usize) -> String {
    let start = floor_char_boundary(content, pos.saturating_sub(back));
    let end = ceil_char_boundary(content, start.saturating_add(len));
    content[start..end].to_string()
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i > s.len() {
        i = s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    let n = s.len();
    if i >= n {
        return n;
    }
    while i < n && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

pub(super) fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
