//! 代码感知分词器。
//!
//! 设计要点（为什么不能直接用 tantivy 的 default 分词器）：
//! - 代码里的标识符 `UserProfileCache2` / `load_user_profile` 是一个整体 token，
//!   default 分词器只会按标点切，导致搜 `user profile` 命中不了。这里要先按
//!   「非字母数字 / 非 CJK」切成基元，再对 ASCII 标识符拆 camelCase、letter↔digit 边界。
//! - 中文注释 `// 用户资料缓存` 必须能被 `用户 资料 缓存` 查到，所以 CJK 连续串交给
//!   jieba-rs 切词（`Jieba::cut(text, false)`，false 表示不做 HMM 新词发现）。
//!
//! tantivy 0.27 API 注意点：
//! - `Tokenizer` / `TokenStream` trait 来自 `tantivy-tokenizer-api`，经
//!   `tantivy::tokenizer` 再导出。`TokenStream` 只有 advance/token/token_mut，没有 reset，
//!   所以这里用「一次性把整段 text 切成 Vec<Token> 再逐个吐出」的简单实现。
//! - 复用一个全局 `Jieba`（构造要加载词典，很贵），用 `OnceLock` 惰性初始化。

use std::sync::OnceLock;

use tantivy::tokenizer::{
    AsciiFoldingFilter, RemoveLongFilter, TextAnalyzer, Token, TokenStream, Tokenizer,
};

/// 注册到 tantivy `TokenizerManager` 的名字，索引与检索两处必须一致。
pub const TOKENIZER_NAME: &str = "code";

/// 全局复用的 jieba 实例，构造代价高，只初始化一次。
fn jieba() -> &'static jieba_rs::Jieba {
    static JIEBA: OnceLock<jieba_rs::Jieba> = OnceLock::new();
    JIEBA.get_or_init(jieba_rs::Jieba::new)
}

/// 是否 CJK 表意文字（中文/日文汉字、扩展区、兼容区）。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF   // Ext A
        | 0x4E00..=0x9FFF // 基本区
        | 0xF900..=0xFAFF // 兼容区
        | 0x20000..=0x2A6DF // Ext B
        | 0x2A700..=0x2EBEF // Ext C/D
        | 0x30000..=0x3134F // Ext G
    )
}

/// 把一段原文切成 token 列表（位置 position 全局递增，offset 用真实字节偏移）。
fn tokenize_code(text: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut position = 0usize;

    let push = |tokens: &mut Vec<Token>, text: String, from: usize, to: usize, pos: &mut usize| {
        if text.is_empty() {
            return;
        }
        tokens.push(Token {
            offset_from: from,
            offset_to: to,
            position: *pos,
            text,
            position_length: 1,
        });
        *pos += 1;
    };

    // 按字符类别把原文切成连续 run：CJK / 字母数字（ASCII 与其它 Unicode 合并，便于后续 ASCII 折叠）/ 分隔符。
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        let c = chars[i].1;
        if is_cjk(c) {
            // 收集 CJK 连续串，交给 jieba。
            let run_start = chars[i].0;
            let mut j = i;
            while j < n && is_cjk(chars[j].1) {
                j += 1;
            }
            let run_end = if j < n { chars[j].0 } else { text.len() };
            let run = &text[run_start..run_end];
            for tok in jieba().cut(run, false) {
                push(
                    &mut tokens,
                    tok.word.to_string(),
                    run_start + tok.byte_start,
                    run_start + tok.byte_end,
                    &mut position,
                );
            }
            i = j;
        } else if c.is_alphanumeric() {
            // 收集「字母数字（非 CJK）」run：ASCII 与带重音的拉丁字母合并在一起，
            // 这样 café 是一个整词（后续折叠成 cafe），而不是被拆成 caf + e。
            let run_start = chars[i].0;
            let mut j = i;
            while j < n && chars[j].1.is_alphanumeric() && !is_cjk(chars[j].1) {
                j += 1;
            }
            let run_end = if j < n { chars[j].0 } else { text.len() };
            let run = &text[run_start..run_end];
            for (from, to) in split_ascii_ident(run) {
                push(
                    &mut tokens,
                    run[from..to].to_lowercase(),
                    run_start + from,
                    run_start + to,
                    &mut position,
                );
            }
            i = j;
        } else {
            // 分隔符，跳过。
            i += 1;
        }
    }
    tokens
}

/// 把一段纯 ASCII 标识符拆成子片段的字节区间：
/// - camelCase：小写→大写处断开；
/// - 连续大写缩写后接小写：在最后一个大写前断开（`HTTPServer` → `HTTP` + `Server`）；
/// - letter↔digit：数字与字母交界处断开（`Cache2` → `Cache` + `2`）。
fn split_ascii_ident(ident: &str) -> Vec<(usize, usize)> {
    let ci: Vec<(usize, char)> = ident.char_indices().collect();
    let n = ci.len();
    if n == 0 {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut start = 0usize; // 指向 ci 的下标
    for i in 1..n {
        let prev = ci[i - 1].1;
        let cur = ci[i].1;
        let next = if i + 1 < n { Some(ci[i + 1].1) } else { None };
        let boundary = (cur.is_ascii_digit() != prev.is_ascii_digit())
            || (cur.is_uppercase() && prev.is_lowercase())
            || (cur.is_uppercase()
                && prev.is_uppercase()
                && matches!(next, Some(c) if c.is_lowercase()));
        if boundary {
            spans.push((ci[start].0, ci[i].0));
            start = i;
        }
    }
    spans.push((ci[start].0, ident.len()));
    spans
}

/// `CodeTokenizer` 是无状态的（jieba 走全局单例），所以只用一个 unit struct。
#[derive(Clone, Default)]
pub struct CodeTokenizer;

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecTokenStream {
        VecTokenStream {
            inner: tokenize_code(text).into_iter(),
            token: Token::default(),
        }
    }
}

/// 拥有 token 列表的简单 TokenStream。
pub struct VecTokenStream {
    inner: std::vec::IntoIter<Token>,
    token: Token,
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        match self.inner.next() {
            Some(t) => {
                self.token = t;
                true
            }
            None => false,
        }
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

/// 构建索引与查询**共用**的文本分析管线：代码分词 + 超长 token 过滤 + ASCII 折叠。
///
/// - `RemoveLong(64)`：丢弃 minified / 压缩产物里的超长串，避免膨胀词典。
/// - `AsciiFolding`：把带重音的拉丁字母折叠成 ASCII（`café` → `cafe`），让注释里的重音词可被普通拼写命中。
///
/// 索引与查询必须走同一管线，否则两侧词项对不上（折叠后尤其明显）。
pub fn build_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(CodeTokenizer)
        .filter(RemoveLongFilter::limit(64))
        .filter(AsciiFoldingFilter)
        .build()
}

/// 把任意文本切成小写 token 字符串列表（走与索引相同的分析管线，保证查询词项对齐）。
pub fn tokenize_to_strings(text: &str) -> Vec<String> {
    let mut analyzer = build_analyzer();
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        tokenize_to_strings(s)
    }

    #[test]
    fn camel_case_and_digit() {
        assert_eq!(
            toks("UserProfileCache2"),
            vec!["user", "profile", "cache", "2"]
        );
    }

    #[test]
    fn snake_case() {
        assert_eq!(toks("load_user_profile"), vec!["load", "user", "profile"]);
        assert_eq!(
            toks("fn user_profile_cache()"),
            vec!["fn", "user", "profile", "cache"]
        );
    }

    #[test]
    fn acronym_split() {
        assert_eq!(toks("HTTPServer"), vec!["http", "server"]);
    }

    #[test]
    fn chinese_comment() {
        let t = toks("用户资料缓存");
        assert!(t.contains(&"用户".to_string()));
        assert!(t.contains(&"缓存".to_string()));
    }

    #[test]
    fn mixed() {
        let t = toks("// 用户资料缓存 UserProfileCache");
        assert!(t.contains(&"用户".to_string()));
        assert!(t.contains(&"user".to_string()));
        assert!(t.contains(&"cache".to_string()));
    }

    #[test]
    fn positions_increase() {
        let tokens = tokenize_code("aa bb cc");
        let positions: Vec<usize> = tokens.iter().map(|t| t.position).collect();
        assert_eq!(positions, vec![0, 1, 2]);
    }

    #[test]
    fn ascii_folding_and_remove_long() {
        // 重音折叠：café → cafe（索引与查询共用同一管线，故两侧都能命中）。
        assert_eq!(toks("café"), vec!["cafe"]);
        // 重音 + 驼峰混合仍应在驼峰边界拆词：caféTest → cafe + test。
        assert_eq!(toks("caféTest"), vec!["cafe", "test"]);
        // 超长 token（>64）被 RemoveLongFilter 丢弃，避免 minified 串膨胀词典。
        let too_long = "a".repeat(100);
        assert!(toks(&too_long).is_empty(), ">64 的 token 应被过滤");
        let ok = "a".repeat(60);
        assert_eq!(toks(&ok), vec![ok]);
    }
}
