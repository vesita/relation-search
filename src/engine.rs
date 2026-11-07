//! 引擎：schema、建索引、BM25 检索、snippet、可选 ast-grep 结构召回、输出结构。
//!
//! 模块边界：`facet`（kind/过滤）、`snippet`、`query`、`ast` 各自独立；建索引与检索
//! 共享同一套字段定义与分词器注册。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::merge_policy::LogMergePolicy;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, Term, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{
    DocId, Index, IndexWriter, Score, SegmentReader, TantivyDocument, TantivyError, doc,
};

use crate::chunk::{chunk_text, for_each_text_file};
use crate::tokenizer::{TOKENIZER_NAME, build_analyzer, has_cjk};

mod ast;
mod facet;
mod query;
mod snippet;

use ast::ast_candidates;
pub use facet::Filter;
use facet::{CompiledFilter, classify_kind, kind_id, kind_weight_from_id};
use query::build_query;
use snippet::{cheap_snippet, snippet_text};

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

/// schema 里用到的字段句柄。建索引与检索共用同一套（避免两处各取一遍、各注册一次分词器）。
#[derive(Clone, Copy)]
struct Fields {
    path: Field,
    start_line: Field,
    end_line: Field,
    content: Field,
    kind: Field,
    file: Field,
}

fn field_ids(schema: &Schema) -> Result<Fields> {
    Ok(Fields {
        path: schema.get_field(PATH_FIELD)?,
        start_line: schema.get_field(START_FIELD)?,
        end_line: schema.get_field(END_FIELD)?,
        content: schema.get_field(CONTENT_FIELD)?,
        kind: schema.get_field(KIND_ID_FIELD)?,
        file: schema.get_field(FILE_FIELD)?,
    })
}

/// 打开已有索引并注册自定义分词器。
///
/// tantivy 的 `TokenizerManager` 不随 `open_in_dir` 保留，检索前必须重新注册；
/// 建索引/检索/测试都走这个入口，避免「某处忘了注册」只在运行时才暴露。
fn open_index(index_dir: &Path) -> Result<(Index, Fields)> {
    let index = Index::open_in_dir(index_dir)
        .with_context(|| format!("无法打开索引目录: {}", index_dir.display()))?;
    index
        .tokenizers()
        .register(TOKENIZER_NAME, build_analyzer());
    let fields = field_ids(&index.schema())?;
    Ok((index, fields))
}

/// 新建索引并注册自定义分词器（与 `open_index` 同一套字段/分词器）。
fn create_index(index_dir: &Path) -> Result<(Index, Fields)> {
    let index = Index::create_in_dir(index_dir, build_schema())?;
    index
        .tokenizers()
        .register(TOKENIZER_NAME, build_analyzer());
    let fields = field_ids(&index.schema())?;
    Ok((index, fields))
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

    // 2) 打开或新建索引（统一走 open_index/create_index：字段与分词器注册只有一处）。
    let (index, fields) = if incremental {
        open_index(&index_dir_abs)?
    } else {
        create_index(&index_dir_abs)?
    };
    let Fields {
        path: path_f,
        start_line: start_f,
        end_line: end_f,
        content: content_f,
        kind: kind_f,
        file: file_f,
    } = fields;
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

/// 错误链里是否有 tantivy 的 writer 锁冲突（另一个进程正在写同一个索引目录）。
pub fn index_dir_busy(error: &anyhow::Error) -> bool {
    matches!(
        error.root_cause().downcast_ref::<TantivyError>(),
        Some(TantivyError::LockFailure(..))
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
    /// 代码片段；`--summary` 模式不生成，序列化时省略空串。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub snippet: String,
    pub source: String, // "bm25" 或 "ast"
    pub kind: String,   // "code" | "test" | "doc" | "config" | "generated" | "other"
    pub symbol: String, // 所在一处定义的签名（尽力而为，可能为空）
}

/// 候选池里的一项：`snippet` 延迟到「进入最终结果」时再生。
///
/// tantivy 的 `SnippetGenerator` 会重新分词整块正文；中文正文会触发 jieba 词典加载
/// （约 60ms）并按篇切词。候选池默认 ≥40 篇，若在池阶段逐篇生成，纯 ASCII 查询也会
/// 被拖进 jieba，实测 4ms → 70ms。故池阶段只保留正文，最终 top-limit 时才生成。
struct PendingCandidate {
    candidate: Candidate,
    /// 待补 snippet 的块正文；AST 命中自带 snippet，这里为 None。
    content: Option<String>,
}

/// search 命令的 stdout JSON 结构。
#[derive(Debug, Serialize)]
pub struct SearchJson {
    pub query: String,
    pub root: String,
    /// 索引里当前的块总数（扫描面），用于摘要里说明「在多少块里命中多少」。
    pub documents: u64,
    pub count: usize,
    pub candidates: Vec<Candidate>,
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
    /// 只回扫描摘要：`candidates` 不带 snippet（省上下文，也省 snippet 生成成本）。
    pub summary: bool,
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
            summary: false,
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
        match index_root(&root_abs, index_dir) {
            Ok(_) => {}
            // 同一索引目录的 tantivy writer 是排他锁：插件每次检索前都会 refresh，
            // 多个会话/多个 CLI 并发时必然有人拿不到锁。这时**沿用现有索引继续检索**，
            // 而不是让整个检索失败（下一次 refresh 会补上这次的改动）。
            Err(error) if index_dir_busy(&error) && index_dir.join("meta.json").exists() => {
                eprintln!("索引正被其它进程更新，本次检索沿用现有索引（改动下次 refresh 生效）");
            }
            Err(error) => return Err(error),
        }
    }

    let (mut index, fields) = open_index(index_dir)?;
    // 多 segment 时按 segment 并行检索；初始化失败不应让检索本身失败，退回单线程。
    index.set_default_multithread_executor().ok();
    let Fields {
        path: path_f,
        start_line: start_f,
        end_line: end_f,
        content: content_f,
        kind: kind_f,
        file: _file_f,
    } = fields;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let documents = searcher.num_docs();
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

    let mut candidates: Vec<PendingCandidate> = Vec::with_capacity(top.len());
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
        candidates.push(PendingCandidate {
            candidate: Candidate {
                // 默认 raw_score 是 tweak_score 加权后的最终分（含 kind 权重）；
                // `--no-kind-weight` 时是原始 BM25。
                score: raw_score,
                kind: kind.to_string(),
                symbol: extract_symbol(content),
                path,
                start_line,
                end_line,
                snippet: String::new(), // 池阶段不生成，最终候选才生成
                source: "bm25".to_string(),
            },
            content: Some(content.to_string()),
        });
    }
    // 权重已并入 score；先按 score 降序（BM25 结果）。
    candidates.sort_by(|a, b| b.candidate.score.total_cmp(&a.candidate.score));

    // 可选：ast-grep 结构召回。与 BM25 做「去重 + 融合」而不是无脑 append：
    // - 同一文件里与已有 BM25 候选行区间重叠的 AST 命中跳过（避免重复展示同一段代码）；
    // - 其余 AST 命中给一个「略低于最弱 BM25 候选」的基础分（BM25 为空时给 0），
    //   使其能进榜且排序稳定，但不会压过内容命中；最后统一 cap_per_file + 截断到 limit。
    if let Some(pattern) = options.ast_pattern.as_deref() {
        let ast = ast_candidates(&root_abs, pattern, limit, &compiled);
        let base = candidates
            .last()
            .map(|c| c.candidate.score)
            .filter(|s| *s > 0.0)
            .map(|s| s * 0.5)
            .unwrap_or(0.0);
        let mut added = 0usize;
        for mut a in ast {
            let overlaps = candidates.iter().any(|c| {
                c.candidate.path == a.path
                    && ranges_overlap(
                        c.candidate.start_line,
                        c.candidate.end_line,
                        a.start_line,
                        a.end_line,
                    )
            });
            if overlaps {
                continue;
            }
            a.score = (base - (added as f64) * 1e-6).max(0.0);
            candidates.push(PendingCandidate {
                candidate: a,
                content: None,
            });
            added += 1;
        }
        candidates.sort_by(|a, b| b.candidate.score.total_cmp(&a.candidate.score));
    }

    let mut candidates = match options.per_file_cap {
        Some(per_file) => cap_per_file(candidates, per_file, limit, |c| c.candidate.path.as_str()),
        // `--no-per-file-cap`：只按 limit 截断，不再压每文件块数。
        None => candidates.into_iter().take(limit).collect(),
    };

    // snippet 只为最终候选生成（见 PendingCandidate 注释）。
    // 另：纯 ASCII 查询 + CJK 正文时，改用「定位查询词窗口」的廉价实现——此时
    // SnippetGenerator 的唯一作用也是找同一个词，却会把 jieba 词典加载进来。
    // `--summary`：整段跳过生成，只回路径/行号/kind/symbol/分数。
    let query_has_cjk = has_cjk(options.query.as_str());
    let candidates: Vec<Candidate> = candidates
        .drain(..)
        .map(|mut pending| {
            if options.summary {
                pending.candidate.snippet.clear();
            } else if let Some(content) = pending.content.take() {
                pending.candidate.snippet = if !query_has_cjk && has_cjk(&content) {
                    cheap_snippet(&content, &options.query)
                } else {
                    snippet_text(&snippet_gen, &content)
                };
            }
            pending.candidate
        })
        .collect();

    Ok(SearchJson {
        query: options.query.clone(),
        root: root_abs.to_string_lossy().to_string(),
        documents,
        count: candidates.len(),
        candidates,
    })
}

/// 两个 1 基闭区间 [a1,a2] 与 [b1,b2] 是否重叠。
fn ranges_overlap(a1: u64, a2: u64, b1: u64, b2: u64) -> bool {
    a1 <= b2 && b1 <= a2
}

/// 按「每文件最多 per_file 块」过滤并截断到 limit 条；在已按 score 排序的前提下使用。
fn cap_per_file<T, F>(items: Vec<T>, per_file: usize, limit: usize, path_of: F) -> Vec<T>
where
    F: for<'a> Fn(&'a T) -> &'a str,
{
    let mut per: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(limit.min(items.len()));
    for item in items {
        if out.len() >= limit {
            break;
        }
        let seen = per.entry(path_of(&item).to_string()).or_insert(0);
        if *seen < per_file {
            *seen += 1;
            out.push(item);
        }
    }
    out
}

/// 尽力从块内容里取「所在定义」的签名（`fn foo` / `struct Bar` / `class Baz` …），取不到返回空串。
/// 只做启发式：扫块内首个看起来像定义的行（切块按空行分段，块通常以定义起始）。
pub(super) fn extract_symbol(content: &str) -> String {
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
        let out = cap_per_file(items, 2, 10, |c| c.path.as_str());
        assert_eq!(out.len(), 3);
        assert_eq!(out.iter().filter(|c| c.path == "a.rs").count(), 2);
    }

    #[test]
    fn cheap_snippet_centers_on_ascii_token() {
        let mut content = String::from("中文注释行\n");
        content.push_str(&"填充文本。".repeat(80));
        content.push_str("\nfn user_profile_cache() {}\n");
        let snip = cheap_snippet(&content, "user_profile_cache");
        assert!(snip.to_lowercase().contains("user_profile_cache"));
        assert!(snip.chars().count() <= 240);
    }

    #[test]
    fn cheap_snippet_falls_back_to_head() {
        assert_eq!(cheap_snippet("hello world", "zzz_nomatch"), "hello world");
    }

    /// 索引目录被别的进程占用（tantivy writer 排他锁）时，`--refresh` 不能把检索弄死：
    /// 必须沿用现有索引返回结果，等下一次 refresh 再同步改动。
    #[test]
    fn 索引被占用时刷新不失败_沿用现有索引() {
        let base = std::env::temp_dir().join(format!(
            "relation-search-busy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let root = base.join("repo");
        let index_dir = base.join("index");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("busy_probe.rs"),
            "pub fn busy_probe_marker() -> u32 { 42 }\n",
        )
        .unwrap();
        index_root(&root, &index_dir).expect("首次建索引");

        // 模拟「另一个进程正在写同一个索引目录」：拿到 writer 就等于占住锁。
        let index = Index::open_in_dir(&index_dir).unwrap();
        let writer: IndexWriter<TantivyDocument> =
            index.writer(WRITER_MEMORY_BUDGET).expect("拿到 writer");
        assert!(
            index
                .writer::<TantivyDocument>(WRITER_MEMORY_BUDGET)
                .is_err(),
            "同一索引目录的 writer 必须是排他锁（否则本用例没测到真冲突）"
        );

        let options = SearchOptions {
            query: "busy_probe_marker".to_string(),
            limit: 5,
            refresh: true,
            ..SearchOptions::default()
        };
        let out = search_root_with_options(&root, &index_dir, &options)
            .expect("索引被占用也不该让检索失败");
        assert!(out.count >= 1, "沿用现有索引应能检索到内容：{out:?}");

        drop(writer);
        let refreshed =
            search_root_with_options(&root, &index_dir, &options).expect("锁释放后 refresh 正常");
        assert!(refreshed.count >= 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn fnv1a64_matches_known_vectors() {
        // 锁定常量：换实现导致索引目录哈希漂移时这条会变红。
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"hello"), 0xa430_d846_80aa_bd0b);
    }
}
