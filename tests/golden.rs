//! 黄金集（golden set）回归：锁定「召回质量」与「kind 硬过滤下推到查询期」这两类
//! 容易在调排序/过滤/分词时被无意破坏的行为。
//!
//! 与 smoke.rs 的区别：smoke 只验证「能跑通、不报错」；这里验证「结果对不对」——
//! 用作后续调 BM25 / 权重 / 分词时的回归基线。
//!
//! - Part A 召回：以本仓 `src/`（纯源码，避免测试/文档自我污染）为语料，标注查询要求
//!   期望文件落在 top-5（recall@5）。
//! - Part B kind 语义：用合成 fixture（code/test/doc/config/generated 各一份）验证
//!   `--kinds` 查询期过滤精确、且默认排序「代码优先」。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_relation-search")
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn unique_tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("crs-golden-{tag}-{}-{nanos}", std::process::id()))
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin())
        .args(args)
        .output()
        .expect("启动二进制失败");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn parse(stdout: &str) -> Value {
    let line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .expect("stdout 无输出");
    serde_json::from_str(line).expect("stdout 不是合法 JSON")
}

fn search(root: &str, index: &str, query: &str, limit: &str, extra: &[&str]) -> Value {
    let mut args = vec![
        "search",
        "--root",
        root,
        "--index-dir",
        index,
        "--query",
        query,
        "--limit",
        limit,
    ];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "search 失败 query={query:?}, stderr={stderr}");
    parse(&stdout)
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// Part A：标注集。路径相对 `src/`。命中 = 出现在 top-5。
const GOLDEN: &[(&str, &str)] = &[
    ("split ascii ident camelCase", "tokenizer.rs"),
    ("classify kind generated doc config", "engine.rs"),
    ("walkdir chunk paragraph merge", "chunk.rs"),
    ("tweak score kind weight pool", "engine.rs"),
    ("clap subcommand args root", "main.rs"),
    ("jieba cjk tokenizer", "tokenizer.rs"),
];

#[test]
fn golden_recall_on_src() {
    let src = crate_root().join("src");
    let src_s = src.to_string_lossy().to_string();
    let idx = unique_tmp("recall-idx");
    let idx_s = idx.to_string_lossy().to_string();

    let (code, _, stderr) = run(&["index", "--root", &src_s, "--index-dir", &idx_s]);
    assert_eq!(code, 0, "index 失败: {stderr}");

    let mut hits = 0usize;
    for (q, want) in GOLDEN {
        let j = search(&src_s, &idx_s, q, "5", &[]);
        let paths: Vec<&str> = j["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["path"].as_str())
            .collect();
        if paths.contains(want) {
            hits += 1;
        } else {
            eprintln!("MISS: query={q:?} 期望 {want}，实际 top5={paths:?}");
        }
    }
    let recall = hits as f64 / GOLDEN.len() as f64;
    assert!(
        recall >= 0.8,
        "recall@5={recall}（{hits}/{}）低于 0.8 阈值",
        GOLDEN.len()
    );

    let _ = std::fs::remove_dir_all(&idx);
}

#[test]
fn kind_filter_and_code_first() {
    let fixture = unique_tmp("kind-fixture");
    let idx = unique_tmp("kind-idx");
    let fx = fixture.to_string_lossy().to_string();
    let is = idx.to_string_lossy().to_string();

    // code / test / doc / config / generated 各一份，都含同一短语。
    write(
        &fixture.join("src/widget.rs"),
        &"alpha widget loader\n".repeat(4),
    );
    write(
        &fixture.join("tests/widget_test.rs"),
        &"alpha widget loader\n".repeat(3),
    );
    write(
        &fixture.join("docs/widget.md"),
        &"alpha widget loader\n".repeat(4),
    );
    write(
        &fixture.join("widget.toml"),
        &"alpha widget loader\n".repeat(3),
    );
    write(
        &fixture.join("static/widget.min.js"),
        &"alpha widget loader\n".repeat(4),
    );

    let (code, _, stderr) = run(&["index", "--root", &fx, "--index-dir", &is]);
    assert_eq!(code, 0, "index 失败: {stderr}");

    // 1) --kinds code：非空且全部 code。
    let j = search(&fx, &is, "alpha widget loader", "10", &["--kinds", "code"]);
    let cands = j["candidates"].as_array().unwrap();
    assert!(!cands.is_empty(), "--kinds code 不应为空");
    assert!(
        cands.iter().all(|c| c["kind"] == "code"),
        "--kinds code 应只返回 code"
    );

    // 2) --kinds doc：非空且全部 doc。
    let j = search(&fx, &is, "alpha widget loader", "10", &["--kinds", "doc"]);
    let cands = j["candidates"].as_array().unwrap();
    assert!(!cands.is_empty(), "--kinds doc 不应为空");
    assert!(
        cands.iter().all(|c| c["kind"] == "doc"),
        "--kinds doc 应只返回 doc"
    );

    // 3) --kinds generated：非空且全部 generated。
    let j = search(
        &fx,
        &is,
        "alpha widget loader",
        "10",
        &["--kinds", "generated"],
    );
    let cands = j["candidates"].as_array().unwrap();
    assert!(!cands.is_empty(), "--kinds generated 不应为空");
    assert!(
        cands.iter().all(|c| c["kind"] == "generated"),
        "--kinds generated 应只返回 generated"
    );

    // 4) 默认排序：高价值类型（code/test）必须排在 doc/config/generated 之前。
    let j = search(&fx, &is, "alpha widget loader", "10", &[]);
    let cands = j["candidates"].as_array().unwrap();
    let first_high = cands
        .iter()
        .position(|c| matches!(c["kind"].as_str(), Some("code") | Some("test")));
    let first_noise = cands.iter().position(|c| {
        matches!(
            c["kind"].as_str(),
            Some("doc") | Some("config") | Some("generated")
        )
    });
    assert!(first_high.is_some(), "top10 应至少有一个 code/test 结果");
    if let (Some(h), Some(n)) = (first_high, first_noise) {
        assert!(
            h < n,
            "默认排序应 code/test 优先于 doc/config/generated（first_high={h}, first_noise={n}）"
        );
    }

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&idx);
}
