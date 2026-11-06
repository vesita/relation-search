//! 引擎：schema、建索引、BM25 检索、snippet、可选 ast-grep 结构召回、输出结构。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED, STRING,
};
use tantivy::{doc, Index, IndexWriter, TantivyDocument};

use ast_grep_core::Pattern;
use ast_grep_language::{Language, LanguageExt, SupportLang};

use crate::chunk::{for_each_chunk, for_each_text_file};
use crate::tokenizer::{tokenize_to_strings, CodeTokenizer, TOKENIZER_NAME};

const PATH_FIELD: &str = "path";
const START_FIELD: &str = "start_line";
const END_FIELD: &str = "end_line";
const CONTENT_FIELD: &str = "content";

/// 建索引时 writer 的内存预算（字节）。
const WRITER_MEMORY_BUDGET: usize = 64 * 1024 * 1024;

/// schema：path/start_line/end_line/content 四个字段。
/// content 用自定义 "code" 分词器，且要 WithFreqsAndPositions（BM25 需要词频）。
pub fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field(PATH_FIELD, STRING | STORED);
    b.add_u64_field(START_FIELD, FAST | STORED);
    b.add_u64_field(END_FIELD, FAST | STORED);
    b.add_text_field(
        CONTENT_FIELD,
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(TOKENIZER_NAME)
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored(),
    );
    b.build()
}

fn field_ids(schema: &Schema) -> Result<(Field, Field, Field, Field)> {
    Ok((
        schema.get_field(PATH_FIELD)?,
        schema.get_field(START_FIELD)?,
        schema.get_field(END_FIELD)?,
        schema.get_field(CONTENT_FIELD)?,
    ))
}

/// 默认索引目录：`${CRS_CACHE_DIR:-$HOME/.cache/code-reuse-search}/<canonical root 的稳定哈希>`。
/// 用 DefaultHasher 十六进制；同一 root 每次得到同一目录。
pub fn default_index_dir(root: &Path) -> Result<PathBuf> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("无法规范化 root 路径: {}", root.display()))?;
    let cache = match std::env::var_os("CRS_CACHE_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME")
                .context("HOME 环境变量未设置，无法确定默认缓存目录（可设 CRS_CACHE_DIR 覆盖）")?;
            PathBuf::from(home).join(".cache").join("code-reuse-search")
        }
    };
    let mut hasher = DefaultHasher::new();
    canonical.to_string_lossy().hash(&mut hasher);
    Ok(cache.join(format!("{:x}", hasher.finish())))
}

/// index 命令的摘要。
pub struct IndexSummary {
    pub documents: usize,
    pub index_dir: PathBuf,
}

/// 递归扫描 root、切块、写 tantivy 索引（整体重建）。
///
/// 注意：为了「重跑即最新」，若 index_dir 已存在会先删除重建（它是缓存目录语义）。
pub fn index_root(root: &Path, index_dir: &Path) -> Result<IndexSummary> {
    let root = root
        .canonicalize()
        .with_context(|| format!("无法规范化 root 路径: {}", root.display()))?;

    if index_dir.exists() {
        std::fs::remove_dir_all(index_dir)
            .with_context(|| format!("无法清理旧索引目录: {}", index_dir.display()))?;
    }
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("无法创建索引目录: {}", index_dir.display()))?;
    let index_dir_abs = index_dir
        .canonicalize()
        .with_context(|| format!("无法规范化索引目录: {}", index_dir.display()))?;

    // tantivy 0.27：create_in_dir 要求目录已存在（MmapDirectory::open 不建目录），上面已建。
    let index = Index::create_in_dir(&index_dir_abs, build_schema())?;
    // 关键坑：索引与检索都必须注册自定义分词器；index 刚创建时的 TokenizerManager 是默认那套。
    index.tokenizers().register(TOKENIZER_NAME, CodeTokenizer);

    let (path_f, start_f, end_f, content_f) = field_ids(&index.schema())?;
    let mut writer: IndexWriter<TantivyDocument> = index.writer(WRITER_MEMORY_BUDGET)?;

    let mut count = 0usize;
    for_each_chunk(&root, |ch| {
        let document = doc! {
            path_f => ch.rel_path,
            start_f => ch.start_line,
            end_f => ch.end_line,
            content_f => ch.content,
        };
        writer.add_document(document)?;
        count += 1;
        Ok(())
    })?;

    writer.commit()?;
    drop(writer); // 释放索引锁

    Ok(IndexSummary {
        documents: count,
        index_dir: index_dir_abs,
    })
}

/// 一条检索候选。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub score: f64,
    pub snippet: String,
    pub source: String, // "bm25" 或 "ast"
}

/// search 命令的 stdout JSON 结构。
#[derive(Debug, Serialize)]
pub struct SearchJson {
    pub query: String,
    pub root: String,
    pub count: usize,
    pub candidates: Vec<Candidate>,
}

/// 使用 BM25 检索；索引不存在则先自动建索引；可选合并 ast-grep 命中。
pub fn search_root(
    root: &Path,
    index_dir: &Path,
    query: &str,
    limit: usize,
    ast_pattern: Option<&str>,
) -> Result<SearchJson> {
    let root_abs = root
        .canonicalize()
        .with_context(|| format!("无法规范化 root 路径: {}", root.display()))?;
    // tantivy 的 TopDocs::with_limit 断言 limit>0；这里兜底成至少 1，避免 --limit 0 panic。
    let limit = limit.max(1);

    // 索引不存在（或不是有效 tantivy 索引）→ 自动建，方便插件即插即用。
    if !index_dir.join("meta.json").exists() {
        index_root(&root_abs, index_dir)?;
    }

    let index = Index::open_in_dir(index_dir)
        .with_context(|| format!("无法打开索引目录: {}", index_dir.display()))?;
    // 关键坑：open_in_dir 之后 TokenizerManager 不保留，必须重新注册，否则 QueryParser 报
    // "Error getting tokenizer for field"。
    index.tokenizers().register(TOKENIZER_NAME, CodeTokenizer);

    let (path_f, start_f, end_f, content_f) = field_ids(&index.schema())?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(&index, vec![content_f]);
    let parsed = parser.parse_query(query)?;
    let top = searcher.search(&parsed, &TopDocs::with_limit(limit).order_by_score())?;

    let mut candidates = Vec::with_capacity(top.len());
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let path = doc
            .get_first(path_f)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let start_line = doc.get_first(start_f).and_then(|v| v.as_u64()).unwrap_or(0);
        let end_line = doc.get_first(end_f).and_then(|v| v.as_u64()).unwrap_or(0);
        let content = doc
            .get_first(content_f)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        candidates.push(Candidate {
            path,
            start_line,
            end_line,
            score: f64::from(score),
            snippet: make_snippet(content, query),
            source: "bm25".to_string(),
        });
    }

    if let Some(pattern) = ast_pattern {
        candidates.extend(ast_candidates(&root_abs, pattern, limit));
    }

    Ok(SearchJson {
        query: query.to_string(),
        root: root_abs.to_string_lossy().to_string(),
        count: candidates.len(),
        candidates,
    })
}

/// 用内嵌 ast-grep（tree-sitter 语法已编入二进制）做结构搜索，无需外部 `sg`。
/// 按文件扩展名判定语言；该语言不支持或 pattern 解析失败的文件跳过。
fn ast_candidates(root: &Path, pattern_src: &str, limit: usize) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let root = match root.canonicalize() {
        Ok(r) => r,
        Err(_) => return out,
    };
    let _ = for_each_text_file(&root, |path, content| {
        if out.len() >= limit {
            return Ok(());
        }
        let Some(lang) = SupportLang::from_path(path) else {
            return Ok(());
        };
        let pattern = match Pattern::try_new(pattern_src, lang) {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };
        let source = lang.ast_grep(content);
        let root_node = source.root();
        let rel = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        for m in root_node.find_all(pattern) {
            if out.len() >= limit {
                break;
            }
            out.push(Candidate {
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

/// snippet：围绕 query 首个 token 的位置截 ~240 字符，找不到就取前 240 字符。
fn make_snippet(content: &str, query: &str) -> String {
    let tokens = tokenize_to_strings(query);
    if let Some(first) = tokens.first() {
        if let Some(pos) = find_ci(content, first) {
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

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_centers_on_token() {
        let content = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa用户资料缓存就是这里";
        let snip = make_snippet(content, "用户资料缓存");
        assert!(snip.contains("用户资料缓存"));
        assert!(snip.chars().count() <= 240);
    }

    #[test]
    fn snippet_falls_back_to_head() {
        let content = "hello world";
        assert_eq!(make_snippet(content, "zzz"), "hello world");
    }
}
