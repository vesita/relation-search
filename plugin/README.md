# dsh-relation-search（插件）

把 `relation-search` Rust 引擎包成 dsh 工具 `relation_search`。

- 工具参数：`query`（必填）、`root`（默认会话工作目录）、`limit`（默认 8）、`astPattern`（可选 ast-grep 模式；ast-grep 已编入引擎二进制，无需外部 `sg`）。
- 每次检索前都会先跑一次**增量 `index`**（只处理新增/修改/删除的文件，未变文件几乎瞬时），保证结果反映当前源码。
- 二进制解析顺序：`RELATION_SEARCH_BIN` → 包内 `bin/relation-search` → 仓库 `../../target/release/relation-search` → PATH。

## 构建与安装

```bash
# 1) 先构建引擎（仓库根）
cargo build --release

# 2) 打包插件（prepack 会 tsc 到 lib/；先把引擎二进制放进 bin/）
mkdir -p bin && cp ../target/release/relation-search bin/relation-search
pnpm install
pnpm run dist        # 产物：../dist/dsh-relation-search-<version>.tgz

# 3) 装进 profile（把 <profile> 换成正在用的，如 web）
dsh plugin --profile <profile> add "file:$PWD/../dist/dsh-relation-search-<version>.tgz"
# 重启 dsh 后该 profile 的 agent 即可调用 relation_search
```

> 打包产物统一输出到仓库根的 `dist/`（已进 `.gitignore`）；直接跑 `pnpm pack` 会落在 `plugin/` 下，别那么用。

> 若 profile 的 `package.json` 的 `dsh.profile.bundles` 里没有 `dsh-relation-search`，需补一行再重启（参照已装插件的登记方式）。
>
> **`pnpm test`（selfcheck）需要在宿主环境里跑**：`@deepseek-ai/dsh-tools` 由宿主提供、不在插件依赖里，脱离 profile 直接跑会 `ERR_MODULE_NOT_FOUND`。可在 profile 的 `node_modules` 目录旁运行，或临时把宿主的 `@deepseek-ai/dsh-tools` 链进 `plugin/node_modules`。
