//! ast-grep 结构召回（tree-sitter 语法编入二进制，无需外部 sg）。

use std::collections::HashMap;
use std::path::Path;

use ast_grep_core::Pattern;
use ast_grep_language::{Language, LanguageExt, SupportLang};

use crate::chunk::for_each_text_file;

use super::Candidate;
use super::extract_symbol;
use super::facet::{CompiledFilter, classify_kind};
use super::snippet::truncate_chars;

/// 用内嵌 ast-grep（tree-sitter 语法已编入二进制）做结构搜索，无需外部 `sg`。
/// 按文件扩展名判定语言；该语言不支持或 pattern 解析失败的文件跳过。
pub(super) fn ast_candidates(
    root: &Path,
    pattern_src: &str,
    limit: usize,
    filter: &CompiledFilter,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let root = match root.canonicalize() {
        Ok(r) => r,
        Err(_) => return out,
    };
    // Pattern::try_new 会把源码解析成 AST 模式树，有实打实的解析成本；旧写法在「每个文件」
    // 里都重新编译一次（N 个文件 = N 次编译）。这里按语言缓存：同一语言只编译一次，
    // 编译失败也缓存 None（该语言下所有文件都跳过）。
    let mut patterns: HashMap<SupportLang, Option<Pattern>> = HashMap::new();
    let _ = for_each_text_file(&root, |path, content| {
        if out.len() >= limit {
            return Ok(());
        }
        let Some(lang) = SupportLang::from_path(path) else {
            return Ok(());
        };
        let pattern = patterns
            .entry(lang)
            .or_insert_with(|| Pattern::try_new(pattern_src, lang).ok());
        let Some(pattern) = pattern.as_ref() else {
            return Ok(());
        };
        let source = lang.ast_grep(content);
        let root_node = source.root();
        let rel = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if !filter.passes(&rel) {
            return Ok(());
        }
        for m in root_node.find_all(pattern) {
            if out.len() >= limit {
                break;
            }
            out.push(Candidate {
                kind: classify_kind(&rel).to_string(),
                symbol: extract_symbol(&m.text()),
                path: rel.clone(),
                start_line: m.start_pos().line() as u64 + 1,
                end_line: m.end_pos().line() as u64 + 1,
                score: 0.0,
                snippet: truncate_chars(&m.text(), 240),
                source: "ast".to_string(),
            });
        }
        Ok(())
    });
    out
}
