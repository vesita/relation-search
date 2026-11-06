//! CLI 入口：index / search 两个子命令，输出单行 JSON。

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use code_search::engine::{default_index_dir, index_root, search_root};

#[derive(Parser)]
#[command(
    name = "code-search",
    version,
    about = "库内旧实现检索：tantivy BM25 + 代码感知分词 + 可选 ast-grep 结构召回"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 递归扫描 root，切块并建立 tantivy 索引
    Index {
        /// 要索引的仓库根目录
        #[arg(long)]
        root: PathBuf,
        /// 自定义索引目录（默认按 root 稳定哈希落到缓存目录）
        #[arg(long)]
        index_dir: Option<PathBuf>,
    },
    /// BM25 检索（可选 ast-grep 结构召回）
    Search {
        /// 要检索的仓库根目录
        #[arg(long)]
        root: PathBuf,
        /// 自定义索引目录（默认与 index 相同）
        #[arg(long)]
        index_dir: Option<PathBuf>,
        /// 查询串
        #[arg(long)]
        query: String,
        /// 返回条数上限（默认 8）
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// ast-grep 结构模式（需 PATH 中有 sg / ast-grep，否则静默跳过）
        #[arg(long)]
        ast_pattern: Option<String>,
    },
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { root, index_dir } => {
            let index_dir = match index_dir {
                Some(d) => d,
                None => default_index_dir(&root)?,
            };
            let summary = index_root(&root, &index_dir)?;
            // 成功：stdout 单行 JSON，字段名与插件契约一致。
            let json = serde_json::json!({
                "ok": true,
                "documents": summary.documents,
                "indexDir": summary.index_dir.to_string_lossy(),
            });
            println!("{}", serde_json::to_string(&json)?);
        }
        Command::Search {
            root,
            index_dir,
            query,
            limit,
            ast_pattern,
        } => {
            let index_dir = match index_dir {
                Some(d) => d,
                None => default_index_dir(&root)?,
            };
            let out = search_root(&root, &index_dir, &query, limit, ast_pattern.as_deref())?;
            println!("{}", serde_json::to_string(&out)?);
        }
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        // 出错：stderr 打印原因、非 0 退出码。
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
