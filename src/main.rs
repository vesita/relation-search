//! CLI 入口：index / search 两个子命令，输出单行 JSON。

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use relation_search::engine::{
    DEFAULT_PER_FILE_CAP, Filter, IndexMode, SearchOptions, default_index_dir, index_dir_busy,
    index_root_with_mode, search_root_with_options,
};

#[derive(Parser)]
#[command(
    name = "relation-search",
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
        /// 强制全量重建：安全删除本工具管理的索引目录后从头建索引（不删用户数据）
        #[arg(long)]
        rebuild: bool,
    },
    /// BM25 检索（可选 ast-grep 结构召回）
    Search {
        /// 要检索的仓库根目录
        #[arg(long)]
        root: PathBuf,
        /// 自定义索引目录（默认与 index 相同）
        #[arg(long)]
        index_dir: Option<PathBuf>,
        /// 查询串（允许以 `-` 开头，如搜索 `-verbose` 这类代码片段）
        #[arg(long, allow_hyphen_values = true)]
        query: String,
        /// 返回条数上限（默认 8）
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// ast-grep 结构模式（已编入二进制，无需 PATH 中的 sg / ast-grep）
        #[arg(long, allow_hyphen_values = true)]
        ast_pattern: Option<String>,
        /// 只保留这些文件类型 kind（逗号分隔：code/test/doc/config/generated/other）
        #[arg(long, value_delimiter = ',')]
        kinds: Option<Vec<String>>,
        /// 路径 glob：只保留命中任一者（逗号分隔；`*` 可跨目录）
        #[arg(long, value_delimiter = ',')]
        include: Option<Vec<String>>,
        /// 路径 glob：命中任一即排除（逗号分隔）
        #[arg(long, value_delimiter = ',')]
        exclude: Option<Vec<String>>,
        /// 检索前先跑一次增量索引（只处理新增/修改/删除的文件）
        #[arg(long)]
        refresh: bool,
        /// 关闭 kind 权重加权（score 变为原始 BM25；通用文本/记忆检索场景更合适）
        #[arg(long)]
        no_kind_weight: bool,
        /// 关闭「同一文件最多保留 2 块」的截断
        #[arg(long)]
        no_per_file_cap: bool,
        /// 只回扫描摘要：candidates 不带 snippet（先扫一遍再决定细看哪条）
        #[arg(long)]
        summary: bool,
    },
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index {
            root,
            index_dir,
            rebuild,
        } => {
            let index_dir = match index_dir {
                Some(d) => d,
                None => default_index_dir(&root)?,
            };
            let mode = if rebuild {
                IndexMode::Rebuild
            } else {
                IndexMode::Incremental
            };
            let summary = index_root_with_mode(&root, &index_dir, mode).map_err(|error| {
                if index_dir_busy(&error) {
                    anyhow::anyhow!(
                        "另一个进程正在写索引目录 {}：请等它结束再试（`search --refresh` 会自动沿用旧索引）",
                        index_dir.display()
                    )
                } else {
                    error
                }
            })?;
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
            kinds,
            include,
            exclude,
            refresh,
            no_kind_weight,
            no_per_file_cap,
            summary,
        } => {
            let index_dir = match index_dir {
                Some(d) => d,
                None => default_index_dir(&root)?,
            };
            let options = SearchOptions {
                query,
                limit,
                ast_pattern,
                filter: Filter {
                    kinds: kinds.unwrap_or_default(),
                    include: include.unwrap_or_default(),
                    exclude: exclude.unwrap_or_default(),
                },
                refresh,
                kind_weight: !no_kind_weight,
                // 关闭时 None（不压每文件块数）；默认沿用引擎的「每文件最多 2 块」。
                per_file_cap: if no_per_file_cap {
                    None
                } else {
                    Some(DEFAULT_PER_FILE_CAP)
                },
                summary,
            };
            let out = search_root_with_options(&root, &index_dir, &options)?;
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
