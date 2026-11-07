//! 文件类型（kind）分类、排序权重与硬过滤（kinds/include/exclude）。
//! 这些规则同时服务建索引（写 kind_id）与检索（过滤/加权），集中在一处避免两套。

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// 检索的硬过滤（与排序权重不同：不满足即排除，而非降权）。
#[derive(Debug, Default, Clone)]
pub struct Filter {
    /// 只保留这些 kind（空 = 不按 kind 过滤）。
    pub kinds: Vec<String>,
    /// 路径 glob：给定时必须至少命中一个（空 = 不过滤）。
    pub include: Vec<String>,
    /// 路径 glob：命中任一即排除（空 = 不排除）。
    pub exclude: Vec<String>,
}

/// 编译后的过滤器（glob 只编译一次）。
pub(super) struct CompiledFilter {
    kinds: Vec<String>,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl CompiledFilter {
    pub(super) fn compile(filter: &Filter) -> Result<Self> {
        const KNOWN_KINDS: &[&str] = &["code", "test", "doc", "config", "generated", "other"];
        for kind in &filter.kinds {
            if !KNOWN_KINDS.contains(&kind.as_str()) {
                anyhow::bail!("未知的 kind: {kind}（可选：{}）", KNOWN_KINDS.join(" / "));
            }
        }
        Ok(Self {
            kinds: filter.kinds.clone(),
            include: build_globset(&filter.include, "include")?,
            exclude: build_globset(&filter.exclude, "exclude")?,
        })
    }

    pub(super) fn passes(&self, path: &str) -> bool {
        if !self.kinds.is_empty() && !self.kinds.iter().any(|k| k == classify_kind(path)) {
            return false;
        }
        if let Some(include) = &self.include
            && !include.is_match(path)
        {
            return false;
        }
        if let Some(exclude) = &self.exclude
            && exclude.is_match(path)
        {
            return false;
        }
        true
    }
}

fn build_globset(patterns: &[String], what: &str) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        // literal_separator(false)：`*` 可跨目录，符合「`*.rs` = 所有 rust 文件」的直觉。
        let glob = GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .with_context(|| format!("无效的 {what} glob 模式: {pattern}"))?;
        builder.add(glob);
    }
    Ok(Some(builder.build().context("构建 glob 过滤器失败")?))
}

/// 判断文件类型：代码 / 测试 / 文档 / 配置 / 生成物 / 其它。
/// 目的：检索默认「代码优先」——文档、配置、编译产物在实现检索里是噪声，应排在源码之后。
pub(super) fn classify_kind(path: &str) -> &'static str {
    if is_generated(path) {
        return "generated";
    }
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let ext = file.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let is_code = matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "hh"
            | "cs"
            | "rb"
            | "php"
            | "swift"
            | "m"
            | "mm"
            | "scala"
            | "lua"
            | "sh"
            | "bash"
            | "zsh"
            | "sql"
            | "r"
            | "jl"
            | "dart"
            | "ex"
            | "exs"
            | "erl"
            | "hs"
            | "clj"
            | "vue"
            | "svelte"
            | "astro"
    );
    if is_code {
        let in_tests = lower.contains("/tests/")
            || lower.starts_with("tests/")
            || lower.contains("/test/")
            || lower.contains("/__tests__/");
        let name_test = file.contains("_test.")
            || file.starts_with("test_")
            || file.contains(".test.")
            || file.contains(".spec.");
        return if in_tests || name_test {
            "test"
        } else {
            "code"
        };
    }
    match ext {
        "md" | "markdown" | "mdx" | "rst" | "txt" | "adoc" => "doc",
        "toml" | "yaml" | "yml" | "json" | "jsonc" | "ini" | "cfg" | "conf" | "lock" | "env"
        | "properties" => "config",
        _ => "other",
    }
}

/// 编译/打包产物（不在 SKIP_DIRS 覆盖范围内的那些）：min/bundle/generated 后缀，
/// 或 `/lib/` 目录下的 .js（插件 `tsc` 到 `lib/` 的产物）。它们是噪声，降权。
fn is_generated(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let name_generated = file.ends_with(".min.js")
        || file.ends_with(".bundle.js")
        || file.ends_with(".generated.js")
        || file.ends_with(".generated.ts")
        || file.contains(".generated.");
    let lib_js = lower.contains("/lib/")
        && (file.ends_with(".js") || file.ends_with(".cjs") || file.ends_with(".mjs"));
    name_generated || lib_js
}

/// 文件类型对最终排序的权重：实现代码满分；文档/配置/生成物显著降权（仍可被召回，只是排在后面）。
pub(super) fn kind_weight(kind: &str) -> f64 {
    match kind {
        "code" => 1.0,
        "test" => 0.85,
        "doc" => 0.30,
        "config" => 0.35,
        "generated" => 0.15,
        _ => 0.60,
    }
}

/// kind 名称 → 稳定编号（写入 kind_id 字段；已有编号不要改，否则旧语义漂移）。
pub(super) fn kind_id(kind: &str) -> u64 {
    match kind {
        "code" => 0,
        "test" => 1,
        "doc" => 2,
        "config" => 3,
        "generated" => 4,
        _ => 5,
    }
}

/// 编号 → kind 名称（与 kind_id 对应）。
pub(super) fn kind_from_id(id: u64) -> &'static str {
    match id {
        0 => "code",
        1 => "test",
        2 => "doc",
        3 => "config",
        4 => "generated",
        _ => "other",
    }
}

/// 编号对应的排序权重（复用 kind_weight，保证两处一致）。
pub(super) fn kind_weight_from_id(id: u64) -> f64 {
    kind_weight(kind_from_id(id))
}
