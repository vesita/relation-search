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
    run_with_env(args, &[])
}

fn run_with_env(args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
    let mut command = Command::new(bin());
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let out = command.output().expect("启动二进制失败");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// 与引擎相同的 FNV-1a 64 位哈希：测试里用来伪造 manifest「文件未变化」的过期状态。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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

/// `search --refresh`：检索前先跑一次增量 index，能搜到「已建索引之后新增的文件」。
/// 反向对照：不加 --refresh 时同一索引不会自动更新，新文件搜不到（证明该标志真的起作用）。
#[test]
fn search_refresh_indexes_new_files_before_search() {
    let fixture = unique_tmp("refresh-fixture");
    let index_dir = unique_tmp("refresh-idx");
    let fs = fixture.to_string_lossy().to_string();
    let is = index_dir.to_string_lossy().to_string();

    let a = fixture.join("a.rs");
    write(&a, "fn aardvarktoken() {}\n");
    let (code, _, stderr) = run(&["index", "--root", &fs, "--index-dir", &is]);
    assert_eq!(code, 0, "index 失败: {stderr}");

    // 建索引之后再新增文件，但不重跑 index。
    write(&fixture.join("b.rs"), "fn buffalotoken() {}\n");

    let count_for = |query: &str, extra: &[&str]| -> u64 {
        let mut args = vec![
            "search",
            "--root",
            fs.as_str(),
            "--index-dir",
            is.as_str(),
            "--query",
            query,
        ];
        args.extend_from_slice(extra);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "search 失败 query={query:?}: {stderr}");
        parse_single_json(&stdout)["count"].as_u64().unwrap()
    };

    // 反向对照：没有 --refresh 时索引不更新 → 新增文件搜不到。
    assert_eq!(
        count_for("buffalotoken", &[]),
        0,
        "未加 --refresh 时不应自动索引新文件"
    );

    // --refresh：检索前先增量 index → 新增文件可检索，旧文件仍在。
    assert!(
        count_for("buffalotoken", &["--refresh"]) > 0,
        "--refresh 应先把新增文件索引进来"
    );
    assert!(
        count_for("aardvarktoken", &["--refresh"]) > 0,
        "--refresh 后旧文件仍应命"
    );

    // --refresh 也同步「修改」：改内容后立刻能搜到新词。
    write(&a, "fn camelotoken() {}\n");
    assert!(
        count_for("camelotoken", &["--refresh"]) > 0,
        "--refresh 应同步被修改的文件内容"
    );

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&index_dir);
}

/// `index --rebuild`：即使 manifest 被伪造/损坏、增量路径误以为「文件未变化」，
/// 也强制丢弃旧索引并从当前源码重建（旧词消失、新词可检索）。
#[test]
fn index_rebuild_forces_full_rebuild() {
    let fixture = unique_tmp("rebuild-fixture");
    let index_dir = unique_tmp("rebuild-idx");
    let fs = fixture.to_string_lossy().to_string();
    let is = index_dir.to_string_lossy().to_string();

    let a = fixture.join("a.rs");
    write(&a, "fn aardvarktoken() {}\n");
    let (code, stdout, stderr) = run(&["index", "--root", &fs, "--index-dir", &is]);
    assert_eq!(code, 0, "首次 index 失败: {stderr}");
    assert_eq!(parse_single_json(&stdout)["documents"], Value::from(1u64));

    // 改源码，并伪造 manifest 里 a.rs 的记录为「与新内容一样」：模拟索引损坏 /
    // manifest 与实际不一致。此时增量 index 会信任 manifest，保持旧索引不动。
    write(&a, "fn buffalotoken() {}\n");
    let new_content = std::fs::read(&a).unwrap();
    let manifest_path = index_dir.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("读不到 manifest"))
            .expect("manifest 不是合法 JSON");
    manifest["files"]["a.rs"]["size"] = Value::from(new_content.len() as u64);
    manifest["files"]["a.rs"]["hash"] = Value::from(fnv1a64(&new_content));
    std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();

    let count_for = |query: &str| -> u64 {
        let (code, stdout, stderr) = run(&[
            "search",
            "--root",
            fs.as_str(),
            "--index-dir",
            is.as_str(),
            "--query",
            query,
        ]);
        assert_eq!(code, 0, "search 失败 query={query:?}: {stderr}");
        parse_single_json(&stdout)["count"].as_u64().unwrap()
    };

    // 反向对照：普通（增量）index 被伪造的 manifest 骗过，索引仍是旧内容。
    let (code, stdout, stderr) = run(&["index", "--root", &fs, "--index-dir", &is]);
    assert_eq!(code, 0, "增量 index 失败: {stderr}");
    assert_eq!(parse_single_json(&stdout)["documents"], Value::from(1u64));
    assert!(
        count_for("aardvarktoken") > 0,
        "反向对照：增量 index 信任 manifest，应暂时仍能搜到旧内容"
    );
    assert_eq!(
        count_for("buffalotoken"),
        0,
        "反向对照：增量 index 信任 manifest，应暂时搜不到新内容"
    );

    // --rebuild：忽略清单，真正全量重建。旧内容必须消失、新内容必须可检索。
    let (code, stdout, stderr) = run(&["index", "--root", &fs, "--index-dir", &is, "--rebuild"]);
    assert_eq!(code, 0, "--rebuild 失败: {stderr}");
    let j = parse_single_json(&stdout);
    assert_eq!(j["ok"], Value::Bool(true));
    assert_eq!(j["documents"], Value::from(1u64), "重建后仍是 1 个块");
    assert_eq!(count_for("aardvarktoken"), 0, "--rebuild 后旧内容应消失");
    assert!(count_for("buffalotoken") > 0, "--rebuild 后新内容应可检索");

    // 再次 --rebuild：索引目录里已有所有权标记，应仍被安全识别并重建成功。
    let (code, stdout, stderr) = run(&["index", "--root", &fs, "--index-dir", &is, "--rebuild"]);
    assert_eq!(code, 0, "第二次 --rebuild 失败: {stderr}");
    assert_eq!(parse_single_json(&stdout)["documents"], Value::from(1u64));

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&index_dir);
}

/// `index --rebuild` 的安全边界：只删本工具管理的索引目录。
/// - `--index-dir` 指向含用户文件的目录 → 拒绝，文件原样保留；
/// - `--index-dir` 等于 root → 拒绝，仓库文件原样保留；
/// - 默认索引目录（哈希规则）下 --rebuild 正常工作，且会写入所有权标记。
#[test]
fn index_rebuild_never_deletes_user_data() {
    let fixture = unique_tmp("safe-fixture");
    let fs = fixture.to_string_lossy().to_string();
    write(&fixture.join("a.rs"), "fn safetoken() {}\n");

    // 1) 用户数据目录：不能删、不能覆盖、必须报错。
    let user_dir = unique_tmp("safe-user");
    let user_s = user_dir.to_string_lossy().to_string();
    let user_file = user_dir.join("notes.txt");
    write(&user_file, "irreplaceable user data\n");
    let (code, stdout, stderr) =
        run(&["index", "--root", &fs, "--index-dir", &user_s, "--rebuild"]);
    assert_ne!(code, 0, "--rebuild 不应接受含用户文件的目录");
    assert!(stdout.trim().is_empty(), "出错时 stdout 应为空");
    assert!(!stderr.trim().is_empty(), "出错时 stderr 应有原因");
    assert!(user_file.exists(), "用户文件绝不能被删除");
    assert_eq!(
        std::fs::read_to_string(&user_file).unwrap(),
        "irreplaceable user data\n",
        "用户文件内容不能被改动"
    );

    // 2) 索引目录 == root：拒绝，不能把仓库当索引删掉。
    let (code, _, stderr) = run(&["index", "--root", &fs, "--index-dir", &fs, "--rebuild"]);
    assert_ne!(code, 0, "--rebuild 不应接受 index-dir == root");
    assert!(!stderr.trim().is_empty());
    assert!(fixture.join("a.rs").exists(), "root 里的源码绝不能被删除");

    // 3) 默认索引目录（`${RELATION_SEARCH_CACHE_DIR}/<root 哈希>`）下 --rebuild 可用。
    let cache = unique_tmp("safe-cache");
    let cache_s = cache.to_string_lossy().to_string();
    let cache_env: &[(&str, &str)] = &[("RELATION_SEARCH_CACHE_DIR", cache_s.as_str())];
    for round in 1..=2 {
        let (code, stdout, stderr) =
            run_with_env(&["index", "--root", &fs, "--rebuild"], cache_env);
        assert_eq!(
            code, 0,
            "第 {round} 次默认索引目录 --rebuild 失败: {stderr}"
        );
        assert_eq!(parse_single_json(&stdout)["documents"], Value::from(1u64));
    }
    // 默认索引目录里应写入所有权标记（供下次 --rebuild 安全识别）。
    let marked = std::fs::read_dir(&cache)
        .expect("缓存目录不存在")
        .flatten()
        .any(|entry| entry.path().join(".relation-search-index").exists());
    assert!(marked, "默认索引目录应写入所有权标记文件");

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&user_dir);
    let _ = std::fs::remove_dir_all(&cache);
}

/// `search --no-kind-weight`：关闭 kind 权重后，同内容块的分数只由 BM25 决定。
/// 正向：默认 doc 被乘 0.30 权重，分数显著低于 code；
/// 反向：--no-kind-weight 时两者分数相等（证明权重确实被关掉）。
#[test]
fn search_no_kind_weight_disables_kind_bias() {
    let fixture = unique_tmp("noweight-fixture");
    let index_dir = unique_tmp("noweight-idx");
    let fs = fixture.to_string_lossy().to_string();
    let is = index_dir.to_string_lossy().to_string();

    // 两份文件内容完全相同 → 原始 BM25 分数相同，差异只可能来自 kind 权重。
    let body = "pub fn rankingtoken() {}\n";
    write(&fixture.join("src/impl.rs"), body);
    write(&fixture.join("docs/notes.md"), body);
    let (code, _, stderr) = run(&["index", "--root", &fs, "--index-dir", &is]);
    assert_eq!(code, 0, "index 失败: {stderr}");

    let search = |extra: &[&str]| -> Value {
        let mut args = vec![
            "search",
            "--root",
            fs.as_str(),
            "--index-dir",
            is.as_str(),
            "--query",
            "rankingtoken",
        ];
        args.extend_from_slice(extra);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "search 失败: {stderr}");
        parse_single_json(&stdout)
    };
    let score_of = |j: &Value, suffix: &str| -> f64 {
        j["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"].as_str().unwrap_or("").ends_with(suffix))
            .unwrap_or_else(|| panic!("结果里应包含 {suffix}: {j}"))["score"]
            .as_f64()
            .unwrap()
    };

    // 默认：code 满分、doc 被降权 → code 分数更高且排第一。
    let weighted = search(&[]);
    assert_eq!(weighted["count"], Value::from(2u64));
    let code_score = score_of(&weighted, "impl.rs");
    let doc_score = score_of(&weighted, "notes.md");
    assert!(
        code_score > doc_score,
        "默认应 code 优先: code={code_score}, doc={doc_score}"
    );
    assert!(
        weighted["candidates"][0]["path"]
            .as_str()
            .unwrap_or("")
            .ends_with("impl.rs"),
        "默认排序 code 应排第一: {weighted}"
    );

    // --no-kind-weight：两份文件原始 BM25 相同 → 分数相等，不再有 doc 降权。
    let unweighted = search(&["--no-kind-weight"]);
    assert_eq!(unweighted["count"], Value::from(2u64));
    let code_score2 = score_of(&unweighted, "impl.rs");
    let doc_score2 = score_of(&unweighted, "notes.md");
    assert!(
        (code_score2 - doc_score2).abs() < 1e-9,
        "--no-kind-weight 后同内容块分数应相等: code={code_score2}, doc={doc_score2}"
    );
    assert!(
        doc_score2 > doc_score,
        "关闭权重后 doc 分数应回到原始 BM25（高于被降权的 {doc_score}）"
    );

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&index_dir);
}

/// `search --no-per-file-cap`：默认同一文件最多保留 2 块；关闭后应能列出更多块。
#[test]
fn search_no_per_file_cap_keeps_more_chunks() {
    let fixture = unique_tmp("nocap-fixture");
    let index_dir = unique_tmp("nocap-idx");
    let fs = fixture.to_string_lossy().to_string();
    let is = index_dir.to_string_lossy().to_string();

    // 4 个段落，每段 62 行（> 60 行软上限）→ 切出 4 个块，每块都含 sharedtoken。
    let mut content = String::new();
    for group in 0..4 {
        for i in 0..61 {
            content.push_str(&format!("let filler{group}_{i} = {i};\n"));
        }
        content.push_str("fn sharedtoken() {}\n");
        content.push('\n');
    }
    write(&fixture.join("src/many.rs"), &content);
    let (code, stdout, stderr) = run(&["index", "--root", &fs, "--index-dir", &is]);
    assert_eq!(code, 0, "index 失败: {stderr}");
    assert_eq!(
        parse_single_json(&stdout)["documents"],
        Value::from(4u64),
        "fixture 应切成 4 块"
    );

    let search = |extra: &[&str]| -> Value {
        let mut args = vec![
            "search",
            "--root",
            fs.as_str(),
            "--index-dir",
            is.as_str(),
            "--query",
            "sharedtoken",
            "--limit",
            "8",
        ];
        args.extend_from_slice(extra);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "search 失败: {stderr}");
        parse_single_json(&stdout)
    };

    let capped = search(&[]);
    assert_eq!(
        capped["count"],
        Value::from(2u64),
        "默认每文件最多 2 块: {capped}"
    );

    let uncapped = search(&["--no-per-file-cap"]);
    assert_eq!(
        uncapped["count"],
        Value::from(4u64),
        "--no-per-file-cap 后 4 块都应返回: {uncapped}"
    );

    let _ = std::fs::remove_dir_all(&fixture);
    let _ = std::fs::remove_dir_all(&index_dir);
}
