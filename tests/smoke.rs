//! 端到端冒烟测试：调用真实 release/debug 二进制跑 index → search。
//!
//! `env!("CARGO_BIN_EXE_<name>")` 由 cargo 在集成测试里注入，指向本次构建的二进制，
//! 因此 `cargo test` 就能复现整条 CLI 契约，不依赖已安装的路径。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_relation-search")
}

/// 造一个独一无二的临时目录。
fn unique_tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("crs-test-{tag}-{}-{nanos}", std::process::id()))
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

fn parse_single_json(stdout: &str) -> Value {
    let line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .expect("stdout 无输出");
    serde_json::from_str(line).expect("stdout 不是合法 JSON")
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

#[test]
fn end_to_end_index_and_search() {
    let fixture = unique_tmp("fixture");
    let index_dir = unique_tmp("index");
    let fixture_s = fixture.to_string_lossy().to_string();
    let index_s = index_dir.to_string_lossy().to_string();

    write(
        &fixture.join("src/lib.rs"),
        "// 用户资料缓存的实现\npub struct UserProfileCache2 {}\n\n// 按用户 ID 构建资料缓存\npub fn user_profile_cache(user_id: u64) -> u64 {\n    user_id\n}\n",
    );
    write(
        &fixture.join("py/loader.py"),
        "def load_user_profile(user_id):\n    \"\"\"加载用户资料。\"\"\"\n    return {\"id\": user_id}\n",
    );
    // 应被跳过：node_modules 目录 + 含 NUL 的二进制。
    write(
        &fixture.join("node_modules/skip.js"),
        "function userProfileCacheShouldNotBeIndexed() {}\n",
    );
    std::fs::write(
        fixture.join("blob.bin"),
        b"user_profile_cache\x00\x01binary",
    )
    .unwrap();

    // 1) index
    let (code, stdout, stderr) = run(&["index", "--root", &fixture_s, "--index-dir", &index_s]);
    assert_eq!(code, 0, "index 退出码非 0, stderr={stderr}");
    let j = parse_single_json(&stdout);
    assert_eq!(j["ok"], Value::Bool(true));
    assert_eq!(j["documents"], Value::from(2u64), "应只索引两个文本文件");
    assert!(j["indexDir"].as_str().unwrap().starts_with('/'));

    // 2) 中文 + 英文混合查询命中 .rs
    let (code, stdout, stderr) = run(&[
        "search",
        "--root",
        &fixture_s,
        "--index-dir",
        &index_s,
        "--query",
        "用户资料缓存 user cache",
    ]);
    assert_eq!(code, 0, "search 退出码非 0, stderr={stderr}");
    let j = parse_single_json(&stdout);
    assert_eq!(j["query"], Value::from("用户资料缓存 user cache"));
    assert!(j["count"].as_u64().unwrap() > 0, "count 应 > 0");
    let cands = j["candidates"].as_array().unwrap();
    let rs = cands
        .iter()
        .find(|c| c["path"].as_str().unwrap_or("").ends_with("lib.rs"))
        .expect("应命中 src/lib.rs");
    assert!(rs["startLine"].as_u64().unwrap() >= 1);
    assert!(rs["endLine"].as_u64().unwrap() >= rs["startLine"].as_u64().unwrap());
    assert_eq!(rs["source"], Value::from("bm25"));

    // 3) 英文标识符拆分命中 .py
    let (code, stdout, _) = run(&[
        "search",
        "--root",
        &fixture_s,
        "--index-dir",
        &index_s,
        "--query",
        "load user profile",
    ]);
    assert_eq!(code, 0);
    let j = parse_single_json(&stdout);
    let cands = j["candidates"].as_array().unwrap();
    assert!(
        cands
            .iter()
            .any(|c| c["path"].as_str().unwrap_or("").ends_with("loader.py")),
        "应命中 py/loader.py"
    );

    // 4) 无命中 → count 0、退出码 0
    let (code, stdout, _) = run(&[
        "search",
        "--root",
        &fixture_s,
        "--index-dir",
        &index_s,
        "--query",
        "zzz_nonexistent_token",
    ]);
    assert_eq!(code, 0);
    let j = parse_single_json(&stdout);
    assert_eq!(j["count"], Value::from(0u64));
    assert_eq!(j["candidates"].as_array().unwrap().len(), 0);

    // 5) 索引目录不存在时 search 自动建索引
    let auto_index = unique_tmp("autoindex");
    let auto_s = auto_index.to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&[
        "search",
        "--root",
        &fixture_s,
        "--index-dir",
        &auto_s,
        "--query",
        "user_profile_cache",
    ]);
    assert_eq!(code, 0, "自动建索引失败, stderr={stderr}");
    assert!(parse_single_json(&stdout)["count"].as_u64().unwrap() > 0);

    // 6) 错误路径：root 不存在 → 非 0 退出、stderr 有原因
    let (code, stdout, stderr) = run(&["index", "--root", "/tmp/crs-definitely-missing-root-xyz"]);
    assert_ne!(code, 0);
    assert!(stdout.trim().is_empty(), "出错时 stdout 应为空");
    assert!(!stderr.trim().is_empty(), "出错时 stderr 应有原因");

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&index_dir);
    let _ = std::fs::remove_dir_all(&auto_index);
}

/// 回归：代码片段式查询（含 `:: () [] : " /regex/` 等）以前会被 tantivy QueryParser
/// 当成查询语法，导致整个 search 退出码 1；应改为分词后按词项 OR 召回、始终成功。
#[test]
fn code_like_queries_do_not_error_and_still_match() {
    let fixture = unique_tmp("punct");
    let index_dir = unique_tmp("punct-idx");
    let fs = fixture.to_string_lossy().to_string();
    let is = index_dir.to_string_lossy().to_string();

    write(
        &fixture.join("src/engine.rs"),
        "pub struct IndexWriter;\nimpl IndexWriter {\n    pub fn commit(&mut self) {}\n}\nfn check(x: Option<u8>) -> bool {\n    matches!(x, Some(_))\n}\n",
    );
    let (code, _, stderr) = run(&["index", "--root", &fs, "--index-dir", &is]);
    assert_eq!(code, 0, "index 失败: {stderr}");

    // 以前这些都会 "error: Syntax Error / Field does not exist / Unsupported query"。
    let queries = [
        "IndexWriter::commit",
        "fn(check)",
        "matches!(x, Some(_))",
        "user:foo",
        "\"unbalanced",
        "[index]",
        "/regex/",
        "index-writing",
        "-verbose",
        "--foo",
    ];
    for q in queries {
        let (code, stdout, stderr) =
            run(&["search", "--root", &fs, "--index-dir", &is, "--query", q]);
        assert_eq!(code, 0, "查询应成功而非报错: {q:?}, stderr={stderr}");
        let j = parse_single_json(&stdout);
        assert!(j["count"].is_number(), "应有 count 字段: {q:?}");
    }

    // 代码片段应真正命中（查询分词与索引分词对齐）。
    let (code, stdout, _) = run(&[
        "search",
        "--root",
        &fs,
        "--index-dir",
        &is,
        "--query",
        "matches!(x, Some(_))",
    ]);
    assert_eq!(code, 0);
    let j = parse_single_json(&stdout);
    assert!(
        j["count"].as_u64().unwrap() > 0,
        "matches! 查询应命中 src/engine.rs，实际 count=0"
    );

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&index_dir);
}

/// 增量索引：同一个 index-dir 重复 index 时，应正确处理「未变 / 修改 / 新增 / 删除」，
/// 保证索引与当前源码一致，而不是依赖整目录重建。
#[test]
fn incremental_index_reflects_changes() {
    let fixture = unique_tmp("inc-fixture");
    let index_dir = unique_tmp("inc-idx");
    let fs = fixture.to_string_lossy().to_string();
    let is = index_dir.to_string_lossy().to_string();

    let a = fixture.join("a.rs");
    let b = fixture.join("b.rs");
    // 三个互不共享词项的标识符（无 camel 边界，整体一个 token），便于精确判断命中变化。
    write(&a, "fn aardvarktoken() {}\n");

    let index_docs = |expect: u64| {
        let (code, stdout, stderr) = run(&["index", "--root", &fs, "--index-dir", &is]);
        assert_eq!(code, 0, "index 失败: {stderr}");
        let j = parse_single_json(&stdout);
        assert_eq!(
            j["documents"],
            Value::from(expect),
            "documents 不符: stdout={stdout}"
        );
    };
    let count_for = |q: &str| -> u64 {
        let (code, stdout, _) = run(&["search", "--root", &fs, "--index-dir", &is, "--query", q]);
        assert_eq!(code, 0);
        parse_single_json(&stdout)["count"].as_u64().unwrap()
    };

    // 首次：1 个块。
    index_docs(1);
    assert!(count_for("aardvarktoken") > 0, "初始应命中 a.rs");

    // 修改 a.rs 内容：重跑应替换旧块（旧词消失、新词命中），docs 仍为 1。
    write(&a, "fn buffalotoken() {}\n");
    index_docs(1);
    assert_eq!(count_for("aardvarktoken"), 0, "修改后旧词应不再命中");
    assert!(count_for("buffalotoken") > 0, "修改后新词应命中");

    // 新增 b.rs：docs 变 2。
    write(&b, "fn cheetahtoken() {}\n");
    index_docs(2);
    assert!(count_for("cheetahtoken") > 0, "新增文件应命中");

    // 未变再跑：docs 仍 2。
    index_docs(2);

    // 删除 b.rs：docs 回 1，其词不再命中。
    std::fs::remove_file(&b).unwrap();
    index_docs(1);
    assert_eq!(count_for("cheetahtoken"), 0, "删除文件应不再命中");
    assert!(count_for("buffalotoken") > 0, "保留文件应仍命中");

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&index_dir);
}

/// 回归：ast-grep 模式按语言缓存后，结构召回仍应命中。
#[test]
fn ast_pattern_still_matches() {
    let fixture = unique_tmp("ast");
    let index_dir = unique_tmp("ast-idx");
    let fs = fixture.to_string_lossy().to_string();
    let is = index_dir.to_string_lossy().to_string();

    write(
        &fixture.join("src/lib.rs"),
        "fn alpha() {\n    let x = 1;\n}\n\nfn beta() {\n    let y = 2;\n}\n",
    );
    let (code, _, stderr) = run(&["index", "--root", &fs, "--index-dir", &is]);
    assert_eq!(code, 0, "index 失败: {stderr}");

    let (code, stdout, stderr) = run(&[
        "search",
        "--root",
        &fs,
        "--index-dir",
        &is,
        "--query",
        "zzz_nomatch",
        "--ast-pattern",
        "fn $F($$$) { $$$ }",
    ]);
    assert_eq!(code, 0, "ast 检索失败: {stderr}");
    let j = parse_single_json(&stdout);
    let cands = j["candidates"].as_array().unwrap();
    assert!(
        cands
            .iter()
            .any(|c| c["source"] == "ast" && c["path"].as_str().unwrap_or("").ends_with("lib.rs")),
        "ast 结构召回应命中 lib.rs"
    );
    // 融合后不应超过 limit（默认 8），也不会把 AST 结果无脑追加到 limit 之外。
    assert!(
        cands.len() <= 8,
        "结果数不应超过 limit，实际 {}",
        cands.len()
    );

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&index_dir);
}
