//! 引擎：schema、建索引、BM25 检索、snippet、可选 ast-grep 结构召回、输出结构。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, IndexRecordOption, STORED, STRING, Schema, Term, TextFieldIndexing, TextOptions,
    Value,
};
use tantivy::{Index, IndexWriter, TantivyDocument, doc};

use ast_grep_core::Pattern;
use ast_grep_language::{Language, LanguageExt, SupportLang};

use crate::chunk::{for_each_chunk, for_each_text_file};
use crate::tokenizer::{CodeTokenizer, TOKENIZER_NAME, tokenize_to_strings};

const PATH_FIELD: &str = "path";
const START_FIELD: &str = "start_line";
const END_FIELD: &str = "end_line";
const CONTENT_FIELD: &str = "content";

/// 建索引时 writer 的内存预算（字节）。
const WRITER_MEMORY_BUDGET: usize = 64 * 1024 * 1024;

/// 同一文件在一个结果集里最多保留的块数（防止单个大文件把候选位占满）。
const PER_FILE_CAP: usize = 2;

/// 检索时先多取一些候选，再做「文件类型权重 + 每文件上限」重排的池大小系数/上下限。
const POOL_FACTOR: usize = 6;
const POOL_MIN: usize = 40;
const POOL_MAX: usize = 200;

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

/// 默认索引目录：`${RELATION_SEARCH_CACHE_DIR:-$HOME/.cache/relation-search}/<canonical root 的稳定哈希>`。
/// 哈希必须跨进程、跨 Rust 版本稳定（缓存目录语义）：`DefaultHasher` 的文档明确不保证这一点，
/// 一旦算法随版本变化，旧缓存就会变成永远命中不了的孤儿目录，所以这里用固定常量的 FNV-1a。
pub fn default_index_dir(root: &Path) -> Result<PathBuf> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("无法规范化 root 路径: {}", root.display()))?;
    let cache = match std::env::var_os("RELATION_SEARCH_CACHE_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").context(
                "HOME 环境变量未设置，无法确定默认缓存目录（可设 RELATION_SEARCH_CACHE_DIR 覆盖）",
            )?;
            PathBuf::from(home).join(".cache").join("relation-search")
        }
    };
    let hash = fnv1a64(canonical.to_string_lossy().as_bytes());
    Ok(cache.join(format!("{hash:x}")))
}

/// 64 位 FNV-1a：常量固定，因此同一输入在任何 Rust 版本/平台上都得到同一哈希。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
    pub kind: String,   // "code" | "test" | "doc" | "config" | "generated" | "other"
    pub symbol: String, // 所在一处定义的签名（尽力而为，可能为空）
}

/// search 命令的 stdout JSON 结构。
#[derive(Debug, Serialize)]
pub struct SearchJson {
    pub query: String,
    pub root: String,
    pub count: usize,
    pub candidates: Vec<Candidate>,
}

/// 检索的硬过滤（与排序权重不同：不满足即排除，而非降权）。
#[derive(Debug, Default, Clone)]
pub struct Filter {
    /// 只保留这些 kind（空 = 不按 kind 过滤）。
    pub kinds: Vec<String>,
    /// 路径 glob：给定时必须至少命中一个（空 = 不过滤）。
    pub include: Vec<String>,
    /// 路径 glob：命中任一即排除（空 = 不排除）。
    pub exclude: Vec<String>,
}

impl Filter {
    fn is_active(&self) -> bool {
        !self.kinds.is_empty() || !self.include.is_empty() || !self.exclude.is_empty()
    }
}

/// 编译后的过滤器（glob 只编译一次）。
struct CompiledFilter {
    kinds: Vec<String>,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl CompiledFilter {
    fn compile(filter: &Filter) -> Result<Self> {
        const KNOWN_KINDS: &[&str] = &["code", "test", "doc", "config", "generated", "other"];
        for kind in &filter.kinds {
            if !KNOWN_KINDS.contains(&kind.as_str()) {
                anyhow::bail!("未知的 kind: {kind}（可选：{}）", KNOWN_KINDS.join(" / "));
            }
        }
        Ok(Self {
            kinds: filter.kinds.clone(),
            include: build_globset(&filter.include, "include")?,
            exclude: build_globset(&filter.exclude, "exclude")?,
        })
    }

    fn passes(&self, path: &str) -> bool {
        if !self.kinds.is_empty() && !self.kinds.iter().any(|k| k == classify_kind(path)) {
            return false;
        }
        if let Some(include) = &self.include
            && !include.is_match(path)
        {
            return false;
        }
        if let Some(exclude) = &self.exclude
            && exclude.is_match(path)
        {
            return false;
        }
        true
    }
}

fn build_globset(patterns: &[String], what: &str) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        // literal_separator(false)：`*` 可跨目录，符合「`*.rs` = 所有 rust 文件」的直觉。
        let glob = GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .with_context(|| format!("无效的 {what} glob 模式: {pattern}"))?;
        builder.add(glob);
    }
    Ok(Some(builder.build().context("构建 glob 过滤器失败")?))
}

/// 使用 BM25 检索；索引不存在则先自动建索引；可选合并 ast-grep 命中。
pub fn search_root(
    root: &Path,
    index_dir: &Path,
    query: &str,
    limit: usize,
    ast_pattern: Option<&str>,
    filter: &Filter,
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
    let query_ast = build_query(content_f, query);
    // 有「硬过滤」时放宽候选池（过滤会丢很多），否则维持原池再做权重/每文件上限重排。
    let compiled = CompiledFilter::compile(filter)?;
    let pool = if filter.is_active() {
        limit.saturating_mul(20).clamp(200, 2000)
    } else {
        limit.saturating_mul(POOL_FACTOR).clamp(POOL_MIN, POOL_MAX)
    };
    let top = searcher.search(
        query_ast.as_ref(),
        &TopDocs::with_limit(pool).order_by_score(),
    )?;

    let mut candidates: Vec<Candidate> = Vec::with_capacity(top.len());
    for (raw_score, addr) in top {
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
        let kind = classify_kind(&path);
        if !compiled.passes(&path) {
            continue;
        }
        candidates.push(Candidate {
            score: f64::from(raw_score) * kind_weight(kind),
            kind: kind.to_string(),
            symbol: extract_symbol(content),
            path,
            start_line,
            end_line,
            snippet: make_snippet(content, query),
            source: "bm25".to_string(),
        });
    }
    // 权重已并入 score；按 score 降序、每文件最多 PER_FILE_CAP 块，取前 limit。
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut candidates = cap_per_file(candidates, PER_FILE_CAP, limit);

    if let Some(pattern) = ast_pattern {
        candidates.extend(ast_candidates(&root_abs, pattern, limit, &compiled));
    }

    Ok(SearchJson {
        query: query.to_string(),
        root: root_abs.to_string_lossy().to_string(),
        count: candidates.len(),
        candidates,
    })
}

/// 把查询串构成 BM25 查询。
///
/// 关键：**不走 tantivy 的 `QueryParser`**。用户给的多半是代码片段（`IndexWriter::commit`、
/// `fn(user_id)`、`foo[0]`、`"xxx`），`QueryParser` 会把 `:`/`[`/`(`/引号当成查询语法而直接
/// 报语法错（整个 search 退出码非 0），也会把 `-`/`|`/`.` 拆成的多段强绑成「相邻短语」
/// （`index-writing` 反而搜不到）。这里复用建索引时的同一个 `CodeTokenizer` 把查询切成词项：
/// - 每个词项一条 `TermQuery`（`Should`/OR）：任一命中即可召回，标点不再是语法；
/// - 词项多于一个时再加一条整串 `PhraseQuery`（`Should`）：词项**相邻**出现（如完整标识符
///   `user_profile_cache`）时额外贡献 BM25 分，把「精确命中该符号」的块排到更前。
///
/// 于是既保住代码片段的召回，又保住标识符检索的排序精度。空查询返回匹配空集。
fn build_query(content_f: Field, query: &str) -> Box<dyn Query> {
    let raw = tokenize_to_strings(query);
    let mut seen: HashSet<&str> = HashSet::new();
    let mut terms: Vec<Term> = Vec::with_capacity(raw.len());
    for token in &raw {
        if token.is_empty() || !seen.insert(token.as_str()) {
            continue;
        }
        terms.push(Term::from_field_text(content_f, token));
    }
    if terms.is_empty() {
        return Box::new(EmptyQuery);
    }
    // WithFreqs：BM25 需要词频打分；content 字段本就用 WithFreqsAndPositions 建索引。
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = terms
        .iter()
        .map(|t| {
            (
                Occur::Should,
                Box::new(TermQuery::new(t.clone(), IndexRecordOption::WithFreqs)) as Box<dyn Query>,
            )
        })
        .collect();
    if terms.len() > 1 {
        clauses.push((Occur::Should, Box::new(PhraseQuery::new(terms))));
    }
    Box::new(BooleanQuery::new(clauses))
}

/// 用内嵌 ast-grep（tree-sitter 语法已编入二进制）做结构搜索，无需外部 `sg`。
/// 按文件扩展名判定语言；该语言不支持或 pattern 解析失败的文件跳过。
fn ast_candidates(
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

/// snippet：在所有查询词项的出现位置里，选一个「覆盖最多不同词项」的 ~240 字节窗口；
/// 只按首个 token 定位会漏掉「多个词分散出现」的块。一个都没找到就取前 240 字符。
fn make_snippet(content: &str, query: &str) -> String {
    let mut seen: HashSet<String> = HashSet::new();
    let tokens: Vec<String> = tokenize_to_strings(query)
        .into_iter()
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect();
    if tokens.is_empty() {
        return truncate_chars(content, 240);
    }
    let mut best: Option<(usize, String)> = None;
    for token in &tokens {
        let Some(pos) = find_ci(content, token) else {
            continue;
        };
        let window = window_around(content, pos, 240, 80);
        let window_lower = window.to_lowercase();
        let distinct = tokens
            .iter()
            .filter(|t| window_lower.contains(&t.to_lowercase()))
            .count();
        if best.as_ref().is_none_or(|(b, _)| distinct > *b) {
            best = Some((distinct, window));
        }
    }
    best.map(|(_, w)| w)
        .unwrap_or_else(|| truncate_chars(content, 240))
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

/// 判断文件类型：代码 / 测试 / 文档 / 配置 / 生成物 / 其它。
/// 目的：检索默认「代码优先」——文档、配置、编译产物在实现检索里是噪声，应排在源码之后。
fn classify_kind(path: &str) -> &'static str {
    if is_generated(path) {
        return "generated";
    }
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let ext = file.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let is_code = matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "hh"
            | "cs"
            | "rb"
            | "php"
            | "swift"
            | "m"
            | "mm"
            | "scala"
            | "lua"
            | "sh"
            | "bash"
            | "zsh"
            | "sql"
            | "r"
            | "jl"
            | "dart"
            | "ex"
            | "exs"
            | "erl"
            | "hs"
            | "clj"
            | "vue"
            | "svelte"
            | "astro"
    );
    if is_code {
        let in_tests = lower.contains("/tests/")
            || lower.starts_with("tests/")
            || lower.contains("/test/")
            || lower.contains("/__tests__/");
        let name_test = file.contains("_test.")
            || file.starts_with("test_")
            || file.contains(".test.")
            || file.contains(".spec.");
        return if in_tests || name_test {
            "test"
        } else {
            "code"
        };
    }
    match ext {
        "md" | "markdown" | "mdx" | "rst" | "txt" | "adoc" => "doc",
        "toml" | "yaml" | "yml" | "json" | "jsonc" | "ini" | "cfg" | "conf" | "lock" | "env"
        | "properties" => "config",
        _ => "other",
    }
}

/// 编译/打包产物（不在 SKIP_DIRS 覆盖范围内的那些）：min/bundle/generated 后缀，
/// 或 `/lib/` 目录下的 .js（插件 `tsc` 到 `lib/` 的产物）。它们是噪声，降权。
fn is_generated(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let name_generated = file.ends_with(".min.js")
        || file.ends_with(".bundle.js")
        || file.ends_with(".generated.js")
        || file.ends_with(".generated.ts")
        || file.contains(".generated.");
    let lib_js = lower.contains("/lib/")
        && (file.ends_with(".js") || file.ends_with(".cjs") || file.ends_with(".mjs"));
    name_generated || lib_js
}

/// 文件类型对最终排序的权重：实现代码满分；文档/配置/生成物显著降权（仍可被召回，只是排在后面）。
fn kind_weight(kind: &str) -> f64 {
    match kind {
        "code" => 1.0,
        "test" => 0.85,
        "doc" => 0.30,
        "config" => 0.35,
        "generated" => 0.15,
        _ => 0.60,
    }
}

/// 按「每文件最多 per_file 块」过滤并截断到 limit 条；在已按 score 排序的前提下使用。
fn cap_per_file(items: Vec<Candidate>, per_file: usize, limit: usize) -> Vec<Candidate> {
    let mut per: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(limit.min(items.len()));
    for item in items {
        if out.len() >= limit {
            break;
        }
        let seen = per.entry(item.path.clone()).or_insert(0);
        if *seen < per_file {
            *seen += 1;
            out.push(item);
        }
    }
    out
}

/// 尽力从块内容里取「所在定义」的签名（`fn foo` / `struct Bar` / `class Baz` …），取不到返回空串。
/// 只做启发式：扫块内首个看起来像定义的行（切块按空行分段，块通常以定义起始）。
fn extract_symbol(content: &str) -> String {
    const KWS: &[&str] = &[
        "fn",
        "func",
        "def",
        "class",
        "struct",
        "enum",
        "trait",
        "interface",
        "impl",
        "type",
        "module",
        "object",
    ];
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty()
            || line.starts_with("//")
            || line.starts_with('#')
            || line.starts_with("/*")
            || line.starts_with('*')
        {
            continue;
        }
        for kw in KWS {
            if let Some(pos) = find_word(line, kw) {
                let name: String = line[pos + kw.len()..]
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                    .collect();
                let name = name.trim_matches(':');
                if !name.is_empty() {
                    return format!("{kw} {name}");
                }
            }
        }
    }
    String::new()
}

/// 在 line 里查找独立出现的词 kw（左右边界都不是标识符字符），返回字节位置。
fn find_word(line: &str, kw: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = line[from..].find(kw) {
        let pos = from + rel;
        let before_ok = pos == 0 || !is_ident_byte(bytes[pos - 1]);
        let after = pos + kw.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return Some(pos);
        }
        from = pos + kw.len();
    }
    None
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(path: &str, score: f64) -> Candidate {
        Candidate {
            path: path.to_string(),
            start_line: 1,
            end_line: 2,
            score,
            snippet: String::new(),
            source: "bm25".to_string(),
            kind: "code".to_string(),
            symbol: String::new(),
        }
    }

    #[test]
    fn filter_kinds_and_globs() {
        let by_kind = CompiledFilter::compile(&Filter {
            kinds: vec!["code".to_string()],
            include: vec![],
            exclude: vec![],
        })
        .expect("compile");
        assert!(by_kind.passes("src/a.rs"));
        assert!(!by_kind.passes("docs/x.md"));

        let by_glob = CompiledFilter::compile(&Filter {
            kinds: vec![],
            include: vec!["*.rs".to_string()],
            exclude: vec!["tests/*".to_string()],
        })
        .expect("compile");
        assert!(by_glob.passes("src/a.rs"), "*.rs 应跨目录匹配");
        assert!(!by_glob.passes("src/a.py"));
        assert!(!by_glob.passes("tests/a.rs"), "tests/* 应排除");

        assert!(
            CompiledFilter::compile(&Filter {
                kinds: vec!["bogus".to_string()],
                ..Default::default()
            })
            .is_err(),
            "未知 kind 应报错"
        );
    }

    #[test]
    fn classifies_kinds() {
        assert_eq!(classify_kind("src/engine.rs"), "code");
        assert_eq!(classify_kind("crates/x/tests/pg.rs"), "test");
        assert_eq!(classify_kind("tests/smoke.rs"), "test");
        assert_eq!(classify_kind("src/foo_test.rs"), "test");
        assert_eq!(classify_kind("docs/BACKLOG.md"), "doc");
        assert_eq!(classify_kind("Cargo.toml"), "config");
        assert_eq!(classify_kind("README.md"), "doc");
        assert_eq!(classify_kind("plugins/dsh-pod/lib/client.js"), "generated");
        assert_eq!(classify_kind("web/app.min.js"), "generated");
    }

    #[test]
    fn extracts_symbols() {
        assert_eq!(
            extract_symbol("pub fn user_profile_cache(x: u64) -> u64 {\n  x\n}"),
            "fn user_profile_cache"
        );
        assert_eq!(
            extract_symbol("// comment\npub struct UserProfileCache2 {}"),
            "struct UserProfileCache2"
        );
        assert_eq!(
            extract_symbol("def load_user_profile(user_id):\n    ..."),
            "def load_user_profile"
        );
        assert_eq!(extract_symbol("hello world"), "");
    }

    #[test]
    fn cap_per_file_limits_per_file() {
        let items = vec![
            cand("a.rs", 9.0),
            cand("a.rs", 8.0),
            cand("a.rs", 7.0),
            cand("b.rs", 6.0),
        ];
        let out = cap_per_file(items, 2, 10);
        assert_eq!(out.len(), 3);
        assert_eq!(out.iter().filter(|c| c.path == "a.rs").count(), 2);
    }

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

    #[test]
    fn snippet_prefers_window_covering_more_tokens() {
        // 首词后隔了很远才出现后两个词：只按首词定位的窗口只能覆盖 1 个词，
        // 应改为选中覆盖 2 个词的尾部窗口。
        let filler = "x".repeat(400);
        let content = format!("alpha {filler} beta gamma");
        let snip = make_snippet(&content, "alpha beta gamma");
        assert!(
            snip.contains("beta") && snip.contains("gamma"),
            "snippet={snip}"
        );
        assert!(snip.chars().count() <= 240);
    }

    #[test]
    fn fnv1a64_matches_known_vectors() {
        // 锁定常量：换实现导致索引目录哈希漂移时这条会变红。
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"hello"), 0xa430_d846_80aa_bd0b);
    }
}
