//! 文件扫描与切块。
//!
//! 策略：
//! - 用 `walkdir` 递归，跳过常见重型/无关目录；不跟随符号链接。
//! - 只索引文本文件：> 1 MiB 跳过，含 NUL 字节视为二进制跳过，非 UTF-8 跳过。
//! - 按空行分成段（paragraph），再把相邻小段合并到「约 ≤60 行 / ≤1600 字符」一块。
//!   合并是贪心的：下一个段塞得下就并，塞不下就封口。单个超长段自成一块（不强行切碎，
//!   因为强行切断函数体反而降低语义完整性；snippet 会在展示时截断）。
//! - 每块记录相对路径、1 基起止行、内容。逐文件处理并立即交给 sink，避免把整仓读进内存。

use std::path::Path;

use anyhow::{Context, Result};
use walkdir::WalkDir;

/// 跳过的目录名（任意层级）。
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "__pycache__",
    ".venv",
];

/// 单文件大小上限 1 MiB。
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// 合并后一块的行数 / 字符数上限（软上限）。
const MAX_CHUNK_LINES: usize = 60;
const MAX_CHUNK_CHARS: usize = 1600;

/// 一个待索引的代码块。
#[derive(Debug, Clone)]
pub struct Chunk {
    /// 相对 root 的路径（统一用 `/` 分隔的字符串）。
    pub rel_path: String,
    /// 1 基起始行。
    pub start_line: u64,
    /// 1 基结束行（含）。
    pub end_line: u64,
    /// 块内容（原样保留换行）。
    pub content: String,
}

/// 遍历 root 下所有可索引文本文件，对每个 (绝对路径, 内容) 调用 `sink`。
/// 跳过规则与大小/编码判断集中在这里，供切块（BM25）与 ast 结构搜索共用。
pub fn for_each_text_file(
    root: &Path,
    mut sink: impl FnMut(&Path, &str) -> Result<()>,
) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("无法规范化 root 路径: {}", root.display()))?;

    let walker = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_skipped_dir(e));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // 读不了的目录/文件静默跳过，不让单点错误中断整仓
        };
        if !entry.file_type().is_file() {
            continue;
        }
        // 大小预检，避免读取超大文件。
        match entry.metadata() {
            Ok(m) if m.len() > MAX_FILE_BYTES => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // 含 NUL 字节 → 疑似二进制。
        if bytes.contains(&0u8) {
            continue;
        }
        let content = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => continue, // 非 UTF-8 文本跳过
        };
        sink(entry.path(), &content)?;
    }
    Ok(())
}

/// 遍历 root、切块，对每块调用 `sink`。返回块总数。
pub fn for_each_chunk(root: &Path, mut sink: impl FnMut(Chunk) -> Result<()>) -> Result<usize> {
    let root = root
        .canonicalize()
        .with_context(|| format!("无法规范化 root 路径: {}", root.display()))?;
    let mut total = 0usize;
    for_each_text_file(&root, |path, content| {
        let rel = match path.strip_prefix(&root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => return Ok(()),
        };
        for chunk in chunk_text(&rel, content) {
            sink(chunk)?;
            total += 1;
        }
        Ok(())
    })?;
    Ok(total)
}

/// 是否是应当跳过的目录（root 自身不跳过）。
fn is_skipped_dir(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .map(|name| SKIP_DIRS.contains(&name))
            .unwrap_or(false)
}

/// 把文件内容按空行切段并贪心合并成块。
fn chunk_text(rel_path: &str, content: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.split('\n').collect();

    // 找出所有非空段（连续非空行）的 [start, end) 零基行区间。
    let mut paragraphs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && !lines[i].trim().is_empty() {
            i += 1;
        }
        paragraphs.push((start, i));
    }

    let mut chunks = Vec::new();
    let mut current: Option<(usize, usize)> = None;
    for (ps, pe) in paragraphs {
        match current {
            None => current = Some((ps, pe)),
            Some((cs, _ce)) => {
                // 尝试把 [cs, pe) 作为一块，看是否仍在软上限内。
                let merged_lines = pe - cs;
                let merged_chars: usize = lines[cs..pe].iter().map(|l| l.chars().count() + 1).sum();
                if merged_lines <= MAX_CHUNK_LINES && merged_chars <= MAX_CHUNK_CHARS {
                    current = Some((cs, pe));
                } else {
                    let (cs0, ce0) = current.take().unwrap();
                    chunks.push(make_chunk(rel_path, &lines, cs0, ce0));
                    current = Some((ps, pe));
                }
            }
        }
    }
    if let Some((cs, ce)) = current {
        chunks.push(make_chunk(rel_path, &lines, cs, ce));
    }
    chunks
}

/// 由零基行区间 [start, end) 生成 Chunk（行号转 1 基，end 含）。
fn make_chunk(rel_path: &str, lines: &[&str], start: usize, end: usize) -> Chunk {
    Chunk {
        rel_path: rel_path.to_string(),
        start_line: start as u64 + 1,
        end_line: end as u64, // end 是零基排他下标，正好等于最后一行的 1 基行号
        content: lines[start..end].join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_blank_lines_and_merges() {
        let content = "fn a() {}\n\nfn b() {}\n";
        let chunks = chunk_text("x.rs", &content);
        // 两段都小，会被合并成一块。
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
        assert!(chunks[0].content.contains("fn a()"));
        assert!(chunks[0].content.contains("fn b()"));
    }

    #[test]
    fn big_paragraph_forces_split() {
        // 制造 70 行的段 + 另一个段，超过 60 行上限，应切成两块。
        let mut content = String::new();
        for i in 0..70 {
            content.push_str(&format!("let x{i} = 1;\n"));
        }
        content.push('\n');
        content.push_str("fn tail() {}\n");
        let chunks = chunk_text("x.rs", &content);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[1].end_line, 72);
    }
}
