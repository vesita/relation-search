# AGENTS.md — relation-search

开发新功能前的**库内旧实现检索**工具。配套用户级 skill：`search-before-implement`（策略）+ 本插件（能力）。

## 结构
- `Cargo.toml` + `src/`：Rust 引擎（tantivy BM25 + 代码感知分词 + jieba-rs 中文分词 + 内嵌 ast-grep 结构召回，tree-sitter 语法随二进制，无需外部工具）。
- `plugin/`：dsh 插件，`defineTool` 包装引擎，工具名 `relation_search`。
- `fixtures/sample/`：插件端到端自检用的样例仓库。

## 构建与测试
- 引擎：`cargo build --release`（产物 `target/release/relation-search`）。
- 插件：`cd plugin && pnpm install && pnpm run build && pnpm test`（`test` 需要先有引擎二进制；且 `@deepseek-ai/dsh-tools` 由宿主提供，脱离 profile 直接跑会找不到，须在宿主 `node_modules` 环境下运行或临时软链）。
- CLI 契约见 `README.md`；插件与 `plugin/selfcheck.mjs` 依赖其字段名，改契约要同时改这两处。

## 约定
- 文档/注释跟随代码；CLI 契约以 `README.md` 为唯一事实源。
- 插件不把 `@deepseek-ai/*` 写进 `dependencies`（由宿主提供，从 profile 的 `node_modules` 解析）；tsc 用 `plugin/src/dsh-tools.d.ts` 类型垫片。
