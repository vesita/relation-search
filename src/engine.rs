//! 引擎：schema、建索引、BM25 检索、snippet、可选 ast-grep 结构召回、输出结构。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::merge_policy::LogMergePolicy;
use tantivy::query::{BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, Term, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{DocId, Index, IndexWriter, Score, SegmentReader, TantivyDocument, doc};

use ast_grep_core::Pattern;
use ast_grep_language::{Language, LanguageExt, SupportLang};

use crate::chunk::{chunk_text, for_each_text_file};
use crate::tokenizer::{TOKENIZER_NAME, build_analyzer, tokenize_to_strings};

const PATH_FIELD: &str = "path";
const START_FIELD: &str = "start_line";
const END_FIELD: &str = "end_line";
const CONTENT_FIELD: &str = "content";
/// 文件类型编号（code/test/doc/config/generated/other → 0..5），用于查询期 kind 过滤与打分加权。
const KIND_ID_FIELD: &str = "kind_id";
/// 文件相对路径（`STRING` 原样索引、不存储）：增量更新时按 file 精确 `delete_term` 删掉该文件旧块。
const FILE_FIELD: &str = "file";

/// 建索引时 writer 的内存预算（字节）。
const WRITER_MEMORY_BUDGET: usize = 64 * 1024 * 1024;

/// 同一文件在一个结果集里最多保留的块数（防止单个大文件把候选位占满）。
pub const DEFAULT_PER_FILE_CAP: usize = 2;

/// 检索时先多取一些候选，再做「文件类型权重 + 每文件上限」重排的池大小系数/上下限。
const POOL_FACTOR: usize = 6;
const POOL_MIN: usize = 40;
const POOL_MAX: usize = 200;

/// 索引格式版本：schema / 分词器语义发生不兼容变化时 +1。
/// 它参与默认索引目录的哈希输入，所以升级后旧缓存会“自然失效”为新目录，
/// 不会被 `Index::open_in_dir` 用不匹配的 schema 打开而运行时报错。
const INDEX_FORMAT_VERSION: u32 = 4;

/// schema：path/start_line/end_line/content 四个字段。
/// content 用自定义 "code" 分词器，且要 WithFreqsAndPositions（BM25 需要词频）。
/// path / start_line / end_line 仅用于结果展示（从 stored doc 读取），不需要倒排或 fast field。
pub fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field(PATH_FIELD, STORED);
    b.add_u64_field(START_FIELD, STORED);
    b.add_u64_field(END_FIELD, STORED);
    // kind_id：INDEXED 供查询期 kind 过滤（TermQuery），FAST 供收集阶段读取并乘类型权重。
    b.add_u64_field(KIND_ID_FIELD, INDEXED | FAST);
    // file：仅索引（不存储），用于增量更新时按 file 精确删除该文件的旧块。
    b.add_text_field(FILE_FIELD, STRING);
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

fn field_ids(schema: &Schema) -> Result<(Field, Field, Field, Field, Field, Field)> {
    Ok((
        schema.get_field(PATH_FIELD)?,
        schema.get_field(START_FIELD)?,
        schema.get_field(END_FIELD)?,
        schema.get_field(CONTENT_FIELD)?,
        schema.get_field(KIND_ID_FIELD)?,
        schema.get_field(FILE_FIELD)?,
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
    // 把索引格式版本拼进哈希输入：schema / 分词器升级后旧目录不再被复用（自动重建）。
    let mut key = format!("v{INDEX_FORMAT_VERSION}:");
    key.push_str(&canonical.to_string_lossy());
    let hash = fnv1a64(key.as_bytes());
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

/// 索引模式（`index --rebuild` 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    /// 默认：索引目录里有匹配当前格式版本的清单时，只处理新增 / 修改 / 删除的文件。
    Incremental,
    /// 强制全量重建：忽略已有清单，安全删除索引目录后从头建索引。
    Rebuild,
}

/// 索引清单：记录每个文件的内容哈希与块数，用于增量更新（无需每次整仓重建）。
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    files: HashMap<String, FileEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            format_version: INDEX_FORMAT_VERSION,
            files: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    /// 文件字节数（快速判断是否可能变化）。
    size: u64,
    /// 文件内容的 FNV-1a 64 位哈希（跨进程稳定，用于精确判断是否需要重索引）。
    hash: u64,
    /// 该文件切出的块数（用于汇总 docs 计数，无需读索引）。
    chunks: usize,
}

const MANIFEST_FILE: &str = "manifest.json";

/// 索引目录所有权标记文件（内容为 `{"formatVersion":N}`）：`index` 成功后写入。
/// `--rebuild` 清理目录前用它确认「这是本工具生成的索引目录」；没有标记的目录还会
/// 退一步检查「目录内是否全是 tantivy 索引产物」，只要混入非索引文件就拒绝删除，
/// 避免 `--index-dir` 误指用户目录时删掉用户数据。
const INDEX_MARKER_FILE: &str = ".relation-search-index";

fn read_manifest(index_dir: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(index_dir.join(MANIFEST_FILE)).ok()?;
    serde_json::from_str::<Manifest>(&text).ok()
}

fn write_manifest(index_dir: &Path, manifest: &Manifest) -> Result<()> {
    let text = serde_json::to_string(manifest)?;
    std::fs::write(index_dir.join(MANIFEST_FILE), text)
        .with_context(|| format!("无法写入索引清单: {}", index_dir.display()))?;
    Ok(())
}

/// 递归扫描 root、切块、写 tantivy 索引（默认增量模式）。
///
/// 保留旧签名以兼容既有调用方；等价于 [`IndexMode::Incremental`]。
pub fn index_root(root: &Path, index_dir: &Path) -> Result<IndexSummary> {
    index_root_with_mode(root, index_dir, IndexMode::Incremental)
}

/// 按指定模式建索引。
///
/// **增量**（默认）：索引目录里有匹配当前 `INDEX_FORMAT_VERSION` 的 `manifest.json` 时，
/// 只对「新增 / 修改 / 删除」的文件做 `delete_term + add_document`，不再整目录重建；
/// 否则（首次、或格式版本不符）全量重建。无论哪种，重跑后索引都与当前源码保持一致。
///
/// **重建**（`index --rebuild`）：忽略已有清单，改走「安全删除旧索引目录 + 从头建」，
/// 用于索引损坏 / manifest 与实际不一致时强制自愈；删除前的安全校验见
/// [`remove_index_dir`]，绝不删用户数据。
pub fn index_root_with_mode(
    root: &Path,
    index_dir: &Path,
    mode: IndexMode,
) -> Result<IndexSummary> {
    let root = root
        .canonicalize()
        .with_context(|| format!("无法规范化 root 路径: {}", root.display()))?;
    // 索引目录不能等于 root、也不能是 root 的祖先（否则重建会清掉仓库/用户数据）。
    ensure_index_dir_is_usable(&root, index_dir)?;

    // 1) 判断能否增量：已有清单且格式版本匹配当前 INDEX_FORMAT_VERSION。
    //    `Rebuild` 一律不走增量，强制全量重建。
    let old_manifest = match mode {
        IndexMode::Incremental => {
            read_manifest(index_dir).filter(|m| m.format_version == INDEX_FORMAT_VERSION)
        }
        IndexMode::Rebuild => None,
    };
    let incremental = old_manifest.is_some() && index_dir.join("meta.json").exists();

    if !incremental && index_dir.exists() {
        // 全量重建：安全清理旧目录（首次 / 格式不兼容 / 显式 --rebuild）。
        remove_index_dir(&root, index_dir)?;
    }
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("无法创建索引目录: {}", index_dir.display()))?;
    let index_dir_abs = index_dir
        .canonicalize()
        .with_context(|| format!("无法规范化索引目录: {}", index_dir.display()))?;

    // 2) 打开或新建索引。
    let index = if incremental {
        Index::open_in_dir(&index_dir_abs)?
    } else {
        Index::create_in_dir(&index_dir_abs, build_schema())?
    };
    // 关键坑：索引与检索都必须注册自定义分词器。
    index
        .tokenizers()
        .register(TOKENIZER_NAME, build_analyzer());

    let (path_f, start_f, end_f, content_f, kind_f, file_f) = field_ids(&index.schema())?;
    let mut writer: IndexWriter<TantivyDocument> = index.writer(WRITER_MEMORY_BUDGET)?;
    // 增量会不断产生删除墓碑；默认 `del_docs_ratio_before_merge=1.0` 使墓碑永不被 merge 回收，
    // 调小该比例让 merge 真正清理旧块（配合结尾的 wait_merging_threads）。
    let mut merge_policy = LogMergePolicy::default();
    merge_policy.set_del_docs_ratio_before_merge(0.3);
    writer.set_merge_policy(Box::new(merge_policy));

    let old_files = old_manifest.map(|m| m.files).unwrap_or_default();
    let mut new_manifest = Manifest::default();
    let mut seen: HashSet<String> = HashSet::new();

    // 3) 扫描当前文件：未变的沿用旧记录；新增/修改的先按 file 删旧块再写新块。
    for_each_text_file(&root, |path, content| {
        // `--index-dir` 落在 root 内时，绝不把索引自身产物（meta.json / 段文件 / 标记文件）
        // 当作源码再索引一遍。
        if path.starts_with(&index_dir_abs) {
            return Ok(());
        }
        let rel = match path.strip_prefix(&root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => return Ok(()),
        };
        seen.insert(rel.clone());
        let size = content.len() as u64;
        let hash = fnv1a64(content.as_bytes());
        if let Some(old) = old_files.get(&rel)
            && old.size == size
            && old.hash == hash
        {
            // 未变化：保留旧记录（其块仍在索引里）。
            new_manifest.files.insert(rel, old.clone());
            return Ok(());
        }
        // 新增或内容变化：先删掉该文件所有旧块（此前不存在时为空操作），再写入新块。
        writer.delete_term(Term::from_field_text(file_f, &rel));
        let kid = kind_id(classify_kind(&rel));
        let chunks = chunk_text(&rel, content);
        let n = chunks.len();
        for ch in chunks {
            let document = doc! {
                path_f => ch.rel_path,
                start_f => ch.start_line,
                end_f => ch.end_line,
                content_f => ch.content,
                kind_f => kid,
                file_f => rel.clone(),
            };
            writer.add_document(document)?;
        }
        new_manifest.files.insert(
            rel,
            FileEntry {
                size,
                hash,
                chunks: n,
            },
        );
        Ok(())
    })?;

    // 4) 处理已删除的文件：在旧清单里但磁盘上已不存在 → 删掉它的块。
    for old_path in old_files.keys() {
        if !seen.contains(old_path) {
            writer.delete_term(Term::from_field_text(file_f, old_path));
        }
    }

    writer.commit()?;
    // 等待后台 merge 完成再释放 writer：`drop(writer)` 会 kill 掉尚未完成的 merge。
    writer.wait_merging_threads()?;

    let documents = new_manifest.files.values().map(|e| e.chunks).sum();
    write_manifest(&index_dir_abs, &new_manifest)?;
    // 写所有权标记：供后续 `--rebuild` 安全识别「这是索引目录」，而不是用户数据目录。
    write_index_marker(&index_dir_abs)?;

    Ok(IndexSummary {
        documents,
        index_dir: index_dir_abs,
    })
}

/// 写索引目录所有权标记（`formatVersion` 便于以后排查是哪个索引格式版本留下的目录）。
fn write_index_marker(index_dir: &Path) -> Result<()> {
    let marker = serde_json::json!({ "formatVersion": INDEX_FORMAT_VERSION });
    std::fs::write(
        index_dir.join(INDEX_MARKER_FILE),
        serde_json::to_vec(&marker)?,
    )
    .with_context(|| format!("无法写入索引标记文件: {}", index_dir.display()))?;
    Ok(())
}

/// 索引目录的基本合法性：存在时不能与 root 相同、也不能是 root 的祖先。
/// （这两种情况下删索引或写索引都会直接波及仓库/用户数据。）
fn ensure_index_dir_is_usable(root: &Path, index_dir: &Path) -> Result<()> {
    if !index_dir.exists() {
        return Ok(());
    }
    let index_abs = index_dir
        .canonicalize()
        .with_context(|| format!("无法规范化索引目录: {}", index_dir.display()))?;
    if index_abs == root {
        anyhow::bail!(
            "索引目录不能与 root 相同（{}）：请换一个 --index-dir，否则会污染/清空仓库",
            index_dir.display()
        );
    }
    if root.starts_with(&index_abs) {
        anyhow::bail!(
            "索引目录不能是 root 的祖先目录（{} 包含 {}）：删除索引会波及仓库/用户数据",
            index_dir.display(),
            root.display()
        );
    }
    Ok(())
}

/// 全量重建前安全删除索引目录：只删索引目录本身（不向上递归），且仅当它确实是
/// 本工具管理的索引目录（空目录 / 带所有权标记 / 目录内全是 tantivy 索引产物）。
/// 只要发现一个非索引文件就拒绝删除，避免 `--index-dir` 误指用户目录时造成数据丢失。
fn remove_index_dir(root: &Path, index_dir: &Path) -> Result<()> {
    let index_abs = index_dir
        .canonicalize()
        .with_context(|| format!("无法规范化索引目录: {}", index_dir.display()))?;
    ensure_index_dir_is_usable(root, &index_abs)?;
    if !is_managed_index_dir(&index_abs) {
        anyhow::bail!(
            "拒绝删除索引目录 {}：它不是 relation-search 的索引目录（缺少 {} 标记，或含非索引文件）。\
             为避免误删用户数据，请确认 --index-dir 指向别处，或手动清理该目录后重试",
            index_abs.display(),
            INDEX_MARKER_FILE
        );
    }
    std::fs::remove_dir_all(&index_abs)
        .with_context(|| format!("无法清理旧索引目录: {}", index_abs.display()))?;
    Ok(())
}

/// 目录是否可安全删除：空目录、带所有权标记，或「目录内全是 tantivy 索引产物且至少
/// 含 meta.json / manifest.json」（兼容升级前没有标记文件的旧索引目录）。
fn is_managed_index_dir(dir: &Path) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    let mut any = false;
    let mut saw_index_file = false;
    for entry in entries.flatten() {
        any = true;
        let name = entry.file_name().to_string_lossy().to_string();
        // 有本工具的所有权标记 → 无条件信任（创建这个目录的就是索引器）。
        if name == INDEX_MARKER_FILE {
            return true;
        }
        if !is_index_artifact_name(&name) {
            // 混入任何非索引文件 → 视为用户数据，不删。
            return false;
        }
        if name == "meta.json" || name == MANIFEST_FILE {
            saw_index_file = true;
        }
    }
    // 空目录没有用户数据；否则必须是可识别的索引目录。
    !any || saw_index_file
}

/// tantivy 索引目录里会出现的文件名（meta / 锁 / 清单 / 段文件）。
fn is_index_artifact_name(name: &str) -> bool {
    if name == "meta.json"
        || name == MANIFEST_FILE
        || name == ".managed.json"
        || name == ".tantivy-meta.lock"
        || name == ".tantivy-writer.lock"
    {
        return true;
    }
    // 段文件：`<segment-uuid>.<ext>`。
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext,
        "term" | "idx" | "pos" | "store" | "fast" | "fieldnorm" | "del"
    )
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

/// 检索选项：默认值等价于 0.2.10 的行为（8 条、代码优先、每文件最多 2 块）。
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// 查询串。
    pub query: String,
    /// 返回条数上限。
    pub limit: usize,
    /// ast-grep 结构模式。
    pub ast_pattern: Option<String>,
    /// 硬过滤（kinds / include / exclude）。
    pub filter: Filter,
    /// 检索前先跑一次增量 `index`（`search --refresh`），保证结果反映当前磁盘内容。
    pub refresh: bool,
    /// 打分阶段是否乘 kind 权重；`false` 时 `score` 就是原始 BM25（`--no-kind-weight`）。
    pub kind_weight: bool,
    /// 每文件最多保留的块数；`None` = 不限制（`--no-per-file-cap`）。
    pub per_file_cap: Option<usize>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 8,
            ast_pattern: None,
            filter: Filter::default(),
            refresh: false,
            kind_weight: true,
            per_file_cap: Some(DEFAULT_PER_FILE_CAP),
        }
    }
}

/// 使用 BM25 检索；索引不存在则先自动建索引；可选合并 ast-grep 命中。
///
/// 保留旧签名以兼容既有调用方；`--refresh` / `--no-kind-weight` / `--no-per-file-cap`
/// 等开关见 [`SearchOptions`] 与 [`search_root_with_options`]。
pub fn search_root(
    root: &Path,
    index_dir: &Path,
    query: &str,
    limit: usize,
    ast_pattern: Option<&str>,
    filter: &Filter,
) -> Result<SearchJson> {
    let options = SearchOptions {
        query: query.to_string(),
        limit,
        ast_pattern: ast_pattern.map(str::to_string),
        filter: filter.clone(),
        ..SearchOptions::default()
    };
    search_root_with_options(root, index_dir, &options)
}

/// 使用 BM25 检索；索引不存在（或 `refresh`）时先建/更新索引；可选合并 ast-grep 命中。
pub fn search_root_with_options(
    root: &Path,
    index_dir: &Path,
    options: &SearchOptions,
) -> Result<SearchJson> {
    let root_abs = root
        .canonicalize()
        .with_context(|| format!("无法规范化 root 路径: {}", root.display()))?;
    // tantivy 的 TopDocs::with_limit 断言 limit>0；这里兜底成至少 1，避免 --limit 0 panic。
    let limit = options.limit.max(1);

    // 索引不存在（或不是有效 tantivy 索引）→ 自动建，方便插件即插即用；
    // `--refresh` 则不管索引是否已存在，都先跑一次增量索引，把新增/修改/删除同步进来。
    if options.refresh || !index_dir.join("meta.json").exists() {
        index_root(&root_abs, index_dir)?;
    }

    let mut index = Index::open_in_dir(index_dir)
        .with_context(|| format!("无法打开索引目录: {}", index_dir.display()))?;
    // 多 segment 时按 segment 并行检索；初始化失败不应让检索本身失败，退回单线程。
    index.set_default_multithread_executor().ok();
    // 关键坑：open_in_dir 之后 TokenizerManager 不保留，必须重新注册，否则 QueryParser 报
    // "Error getting tokenizer for field"。
    index
        .tokenizers()
        .register(TOKENIZER_NAME, build_analyzer());

    let (path_f, start_f, end_f, content_f, kind_f, _file_f) = field_ids(&index.schema())?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    // 1) 内容查询（代码片段安全：分词后按词项 OR 召回，不用 QueryParser）。
    let content_query = build_query(content_f, &options.query);
    // 2) kind 硬过滤下推到查询期：只对目标 kind 的子集做 BM25，避免「先取小池再后过滤」丢候选。
    let query_ast: Box<dyn Query> = if options.filter.kinds.is_empty() {
        content_query
    } else {
        let mut kind_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for k in &options.filter.kinds {
            let term = Term::from_field_u64(kind_f, kind_id(k));
            kind_clauses.push((
                Occur::Should,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>,
            ));
        }
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, content_query),
            (
                Occur::Must,
                Box::new(BooleanQuery::new(kind_clauses)) as Box<dyn Query>,
            ),
        ]))
    };
    // 3) 摘要生成器用最终查询（内部只收集 content 字段的词项）。
    let mut snippet_gen = SnippetGenerator::create(&searcher, query_ast.as_ref(), content_f)?;
    snippet_gen.set_max_num_chars(240);
    let compiled = CompiledFilter::compile(&options.filter)?;
    // kind 已在查询期过滤；只有 include/exclude 这类路径后过滤需要放大候选池补偿。
    let path_filter_active =
        !options.filter.include.is_empty() || !options.filter.exclude.is_empty();
    let pool = if path_filter_active {
        limit.saturating_mul(20).clamp(200, 2000)
    } else {
        limit.saturating_mul(POOL_FACTOR).clamp(POOL_MIN, POOL_MAX)
    };
    // 4) 收集阶段按 kind 权重加权（tweak_score 从 FAST 字段读 kind_id），
    //    使「代码优先」在加权分数上选 top-pool，而不是先按原始 BM25 取池再重排。
    //    `--no-kind-weight` 时跳过加权：score 即原始 BM25，文件类型不再影响排序。
    let top = if options.kind_weight {
        let collector =
            TopDocs::with_limit(pool).tweak_score(move |segment_reader: &SegmentReader| {
                let kind_col = segment_reader
                    .fast_fields()
                    .u64(KIND_ID_FIELD)
                    .ok()
                    .map(|c| c.first_or_default_col(0));
                move |doc: DocId, original_score: Score| -> f64 {
                    let weight = kind_col
                        .as_ref()
                        .map(|c| kind_weight_from_id(c.get_val(doc)))
                        .unwrap_or(1.0);
                    f64::from(original_score) * weight
                }
            });
        searcher.search(query_ast.as_ref(), &collector)?
    } else {
        searcher
            .search(
                query_ast.as_ref(),
                &TopDocs::with_limit(pool).order_by_score(),
            )?
            .into_iter()
            // `order_by_score` 的 Fruit 用 f32；统一成 f64，与加权分支的最终分类型一致。
            .map(|(score, addr)| (f64::from(score), addr))
            .collect()
    };

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
            // 默认 raw_score 是 tweak_score 加权后的最终分（含 kind 权重）；
            // `--no-kind-weight` 时是原始 BM25。
            score: raw_score,
            kind: kind.to_string(),
            symbol: extract_symbol(content),
            path,
            start_line,
            end_line,
            snippet: snippet_text(&snippet_gen, content),
            source: "bm25".to_string(),
        });
    }
    // 权重已并入 score；先按 score 降序（BM25 结果）。
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));

    // 可选：ast-grep 结构召回。与 BM25 做「去重 + 融合」而不是无脑 append：
    // - 同一文件里与已有 BM25 候选行区间重叠的 AST 命中跳过（避免重复展示同一段代码）；
    // - 其余 AST 命中给一个「略低于最弱 BM25 候选」的基础分（BM25 为空时给 0），
    //   使其能进榜且排序稳定，但不会压过内容命中；最后统一 cap_per_file + 截断到 limit。
    if let Some(pattern) = options.ast_pattern.as_deref() {
        let ast = ast_candidates(&root_abs, pattern, limit, &compiled);
        let base = candidates
            .last()
            .map(|c| c.score)
            .filter(|s| *s > 0.0)
            .map(|s| s * 0.5)
            .unwrap_or(0.0);
        let mut added = 0usize;
        for mut a in ast {
            let overlaps = candidates.iter().any(|c| {
                c.path == a.path
                    && ranges_overlap(c.start_line, c.end_line, a.start_line, a.end_line)
            });
            if overlaps {
                continue;
            }
            a.score = (base - (added as f64) * 1e-6).max(0.0);
            candidates.push(a);
            added += 1;
        }
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    }

    let candidates = match options.per_file_cap {
        Some(per_file) => cap_per_file(candidates, per_file, limit),
        // `--no-per-file-cap`：只按 limit 截断，不再压每文件块数。
        None => candidates.into_iter().take(limit).collect(),
    };

    Ok(SearchJson {
        query: options.query.clone(),
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
/// （`index-writing` 反而搜不到）。这里复用建索引时的同一个分析管线（`build_analyzer`）把查询切成词项：
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

/// 用 tantivy 的 SnippetGenerator 生成摘要片段；取不到片段时退化为「取前 240 字符」。
fn snippet_text(generator: &SnippetGenerator, content: &str) -> String {
    let snippet = generator.snippet(content);
    let fragment = snippet.fragment();
    if fragment.is_empty() {
        truncate_chars(content, 240)
    } else {
        fragment.to_string()
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
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

/// kind 名称 → 稳定编号（写入 kind_id 字段；已有编号不要改，否则旧语义漂移）。
fn kind_id(kind: &str) -> u64 {
    match kind {
        "code" => 0,
        "test" => 1,
        "doc" => 2,
        "config" => 3,
        "generated" => 4,
        _ => 5,
    }
}

/// 编号 → kind 名称（与 kind_id 对应）。
fn kind_from_id(id: u64) -> &'static str {
    match id {
        0 => "code",
        1 => "test",
        2 => "doc",
        3 => "config",
        4 => "generated",
        _ => "other",
    }
}

/// 编号对应的排序权重（复用 kind_weight，保证两处一致）。
fn kind_weight_from_id(id: u64) -> f64 {
    kind_weight(kind_from_id(id))
}

/// 两个 1 基闭区间 [a1,a2] 与 [b1,b2] 是否重叠。
fn ranges_overlap(a1: u64, a2: u64, b1: u64, b2: u64) -> bool {
    a1 <= b2 && b1 <= a2
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
    fn fnv1a64_matches_known_vectors() {
        // 锁定常量：换实现导致索引目录哈希漂移时这条会变红。
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"hello"), 0xa430_d846_80aa_bd0b);
    }
}
