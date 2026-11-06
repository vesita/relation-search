# code-reuse-search

开发新功能前的**库内旧实现检索**工具：给 AI 一个"这个是不是已经有人写过"的硬机制。
配套 skill：`search-before-implement`（策略层）+ 本插件（能力层）。

## 组成

- **Rust 引擎**（`Cargo.toml` / `src/`）：tantivy 全文索引 + BM25 排序，代码感知分词（camelCase/snake_case 拆词）+ jieba-rs 中文分词；可选 ast-grep 结构召回。
- **dsh 插件**（`plugin/`）：把引擎包成一个 agent 可调用的 tool `code_reuse_search`。

## CLI 契约（插件依赖，勿随意改）

```
code-reuse-search index  --root <path> [--index-dir <dir>]
code-reuse-search search --root <path> [--index-dir <dir>] --query <q> [--limit <n>] [--ast-pattern <p>]
```

- `index` 成功向 stdout 打印 JSON：`{"ok":true,"documents":<n>,"indexDir":"<abs>"}`，退出码 0。
- `search` 成功向 stdout 打印 JSON：
  ```json
  {"query":"...","root":"...","count":2,"candidates":[
    {"path":"rel/file.rs","startLine":10,"endLine":28,"score":12.3,"snippet":"...","source":"bm25"}
  ]}
  ```
  `source` 取 `bm25` 或 `ast`；无命中时 `count:0,"candidates":[]`，退出码 0。
- `--ast-pattern <p>` 的 ast-grep 结构召回**已编入二进制**（`ast-grep-core` + `ast-grep-language`，tree-sitter 语法随包），无需另装 `sg`/`ast-grep`。
- 出错向 stderr 打印原因、退出码非 0。
- 默认索引目录：`${CRS_CACHE_DIR:-$HOME/.cache/code-reuse-search}/<对 root 稳定哈希>`。

## 构建

```bash
cargo build --release            # 产物 target/release/code-reuse-search
cd plugin && pnpm install && pnpm build
```

（本仓文档与注释跟随代码；插件安装见 `plugin/README.md`。）
