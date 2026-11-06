# relation-search

开发新功能前的**库内旧实现检索**工具：给 AI 一个"这个是不是已经有人写过"的硬机制。
配套 skill：`search-before-implement`（策略层）+ 本插件（能力层）。

## 组成

- **Rust 引擎**（`Cargo.toml` / `src/`）：tantivy 全文索引 + BM25 排序，代码感知分词（camelCase/snake_case 拆词 + 重音折叠 + 超长 token 过滤）+ jieba-rs 中文分词；内嵌 ast-grep 结构召回（无需外部工具）。
- **dsh 插件**（`plugin/`）：把引擎包成一个 agent 可调用的 tool `relation_search`。

## CLI 契约（插件依赖，勿随意改）

```
relation-search index  --root <path> [--index-dir <dir>]
relation-search search --root <path> [--index-dir <dir>] --query <q> [--limit <n>] [--ast-pattern <p>] [--kinds <k1,k2>] [--include <g1,g2>] [--exclude <g1,g2>]
```

- `index` 成功向 stdout 打印 JSON：`{"ok":true,"documents":<n>,"indexDir":"<abs>"}`，退出码 0。其中 `documents` 是**当前索引内的块总数**（增量更新后为更新后的总数，而非本次新写入的块数）。
- `search` 成功向 stdout 打印 JSON：
  ```json
  {"query":"...","root":"...","count":2,"candidates":[
    {"path":"rel/file.rs","startLine":10,"endLine":28,"score":12.3,"snippet":"...","source":"bm25","kind":"code","symbol":"fn user_profile_cache"}
  ]}
  ```
  `source` 取 `bm25` 或 `ast`；无命中时 `count:0,"candidates":[]`，退出码 0。
  `kind` 取 `code` / `test` / `doc` / `config` / `generated` / `other`；`symbol` 是块内首个定义签名（`fn foo` / `struct Bar` …，尽力而为，取不到为空串）。
- `search` 默认**代码优先**：文件类型权重（`code` 1.0 / `test` 0.85 / `doc` 0.30 / `config` 0.35 / `generated` 0.15 / 其它 0.60）在 BM25 **收集阶段**并入打分（`TopDocs::tweak_score`），并对**同一文件最多保留 2 块**——避免文档/配置或单个大文件把候选位占满。返回的 `score` 是加权后的最终排序分。
- **硬过滤（给了才生效，与上一条的"降权"不同：不满足即排除）**：`--kinds`（逗号分隔，只保留指定 kind：`code`/`test`/`doc`/`config`/`generated`/`other`）、`--include` / `--exclude`（路径 glob，逗号分隔，`*` 可跨目录，如 `*.rs` 命中所有 rust 文件）。未知 kind 直接报错。不给这些参数时行为与上一条完全一致。
- `--query` 与建索引共用同一个代码分词器（camelCase / snake_case / CJK 切词，并叠加 `RemoveLong(64)` 超长过滤与 `AsciiFolding` 重音折叠）：切出的词项按 OR 召回、BM25 排序；词项整串相邻出现时（如完整标识符 `user_profile_cache`）再加一条短语子句加分，把精确命中排到更前。`:`、`(`、`[`、引号、开头的 `-` 等标点/符号**不当作查询语法**，`IndexWriter::commit`、`fn(x)`、`matches!(x, Some(_))`、`-verbose` 这类代码片段可直接搜、不会因语法错误整条失败。
- `--ast-pattern <p>` 的 ast-grep 结构召回**已编入二进制**（`ast-grep-core` + `ast-grep-language`，tree-sitter 语法随包），无需另装 `sg`/`ast-grep`。
- 出错向 stderr 打印原因、退出码非 0。
- 默认索引目录：`${RELATION_SEARCH_CACHE_DIR:-$HOME/.cache/relation-search}/<对 root 稳定哈希>`；哈希输入包含**索引格式版本**，schema / 分词升级后旧目录会自动失效为新目录。
- **增量索引**：同一 `--index-dir` 再次 `index` 时，只处理「新增 / 修改 / 删除」的文件（`manifest.json` 记录每个文件的内容哈希），不再整仓重建；删除用 tantivy `delete_term` 打墓碑并靠 merge 回收。首次或格式版本不符时自动全量重建。

## 构建

```bash
cargo build --release            # 产物 target/release/relation-search
cd plugin && pnpm install && pnpm build
```

（本仓文档与注释跟随代码；插件安装见 `plugin/README.md`。）

## 随包 skill

`skills/search-before-implement/` 是本仓内置的项目 skill（策略层：开发新功能前先检索旧实现再决定新写），与插件（能力层）配套使用。

## License

MIT，见 [LICENSE](LICENSE)。
