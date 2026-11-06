# dsh-code-reuse-search（插件）

把 `code-reuse-search` Rust 引擎包成 dsh 工具 `code_reuse_search`。

- 工具参数：`query`（必填）、`root`（默认会话工作目录）、`limit`（默认 8）、`astPattern`（可选 ast-grep 模式）。
- 首次对一个 root 检索会自动 `index`，之后复用缓存索引。
- 二进制解析顺序：`CRS_BIN` → 包内 `bin/code-reuse-search` → 仓库 `../../target/release/code-reuse-search` → PATH。

## 构建与安装

```bash
# 1) 先构建引擎（仓库根）
cargo build --release

# 2) 打包插件（prepack 会 tsc 到 lib/，并把引擎二进制放进 bin/）
mkdir -p bin && cp ../target/release/code-reuse-search bin/code-reuse-search
pnpm install
pnpm pack

# 3) 装进 profile（把 <profile> 换成正在用的，如 web）
dsh plugin --profile <profile> add "file:$PWD/dsh-code-reuse-search-0.1.0.tgz"
# 重启 dsh 后该 profile 的 agent 即可调用 code_reuse_search
```

> 若 profile 的 `package.json` 的 `dsh.profile.bundles` 里没有 `dsh-code-reuse-search`，需补一行再重启（参照已装插件的登记方式）。
