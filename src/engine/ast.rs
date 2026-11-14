//! ast-grep 结构召回（tree-sitter 语法编入二进制，无需外部 sg）。
//!
//! 忠实于 AST 语法关系：
//! 1. 真实 AST 语法节点类型 (ast_kind)；
//! 2. 真实 AST 语法符号提取 (ast_symbol，优先读取语法树的 name/type 字段而非正则猜词)；
//! 3. 真实 AST 宿主外层作用域层级 (ast_scope，回溯祖先树中的 Class/Impl/Function 包含链)；
//! 4. 真实 AST 模式元变量绑定环境 (ast_env，捕获 $VAR 与 $$$VARS 的具名对应关系)。

use std::collections::HashMap;
use std::path::Path;

use ast_grep_core::{Doc, Language, Node, NodeMatch, Pattern};
use ast_grep_language::{LanguageExt, SupportLang};

use crate::chunk::for_each_text_file;

use super::Candidate;
use super::extract_symbol;
use super::facet::{CompiledFilter, classify_kind};
use super::snippet::truncate_chars;

/// 忠实从 AST 节点提取符号定义名称（函数名/类名/方法名/模块名等）。
pub(super) fn extract_ast_symbol<D: Doc>(node: &Node<D>) -> String {
    // 1. 优先查显式的 AST 字段 "name"（函数/方法/类/trait定义）
    if let Some(name_node) = node.field("name") {
        let text = name_node.text().trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    // 2. 检查 "type"（例如 Rust 中的 `impl MyStruct`）
    if let Some(type_node) = node.field("type") {
        let text = type_node.text().trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    // 3. 检查 "function"（如果是 call_expression）
    if let Some(func_node) = node.field("function") {
        let text = func_node.text().trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    // 4. 检查 "property"（如果是方法调用成员访问）
    if let Some(prop_node) = node.field("property") {
        let text = prop_node.text().trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    // 5. 遍历直接子节点查找常见的标识符节点
    for child in node.children() {
        let k = child.kind();
        if k == "identifier" || k == "type_identifier" || k == "field_identifier" {
            let text = child.text().trim().to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }
    // 6. 兜底回退到正则提取
    extract_symbol(&node.text())
}

/// 忠实沿 AST 祖先树回溯，提取外层宿主作用域层级（如 ClassName::methodName / ImplType）。
pub(super) fn extract_enclosing_scope<D: Doc>(node: &Node<D>) -> Option<String> {
    let mut scopes = Vec::new();
    for ancestor in node.ancestors() {
        let kind = ancestor.kind();
        if kind == "impl_item" {
            if let Some(t) = ancestor.field("type") {
                scopes.push(t.text().trim().to_string());
            }
        } else if kind.contains("class")
            || kind.contains("struct")
            || kind.contains("interface")
            || kind.contains("trait")
        {
            if let Some(n) = ancestor.field("name") {
                scopes.push(n.text().trim().to_string());
            }
        } else if kind.contains("function") || kind.contains("method") {
            // 如果被更外层的函数/方法包含（如内部函数、闭包或嵌套块），记录外层函数
            if let Some(n) = ancestor.field("name") {
                scopes.push(n.text().trim().to_string());
            }
        } else if kind == "mod_item" || kind.contains("module") || kind.contains("namespace") {
            if let Some(n) = ancestor.field("name") {
                scopes.push(n.text().trim().to_string());
            }
        }
    }
    if scopes.is_empty() {
        None
    } else {
        scopes.reverse();
        Some(scopes.join("::"))
    }
}

/// 从 pattern_src 中提取所有声明的元变量（如 $F, $NAME, $$$ARGS 等），
/// 并在命中的节点环境中提取具体的 AST 绑定对应关系。
pub(super) fn extract_meta_env<D: Doc>(
    m: &NodeMatch<D>,
    pattern_src: &str,
) -> Option<HashMap<String, String>> {
    let mut env_map = HashMap::new();
    let env = m.get_env();

    let chars: Vec<char> = pattern_src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            let mut start = i;
            while start < chars.len() && chars[start] == '$' {
                start += 1;
            }
            let dollar_prefix_len = start - i;
            let mut end = start;
            while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            if end > start {
                let var_name: String = chars[start..end].iter().collect();
                let full_var: String = chars[i..end].iter().collect();
                if dollar_prefix_len == 3 {
                    let multies = env.get_multiple_matches(&var_name);
                    if !multies.is_empty() {
                        let texts: Vec<String> =
                            multies.iter().map(|n| n.text().trim().to_string()).collect();
                        env_map.insert(full_var, texts.join(", "));
                    }
                } else if dollar_prefix_len == 1 || dollar_prefix_len == 2 {
                    if let Some(node) = env.get_match(&var_name) {
                        env_map.insert(full_var, node.text().trim().to_string());
                    }
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }

    if env_map.is_empty() {
        None
    } else {
        Some(env_map)
    }
}

/// 用内嵌 ast-grep（tree-sitter 语法已编入二进制）做结构搜索，无需外部 `sg`。
/// 按文件扩展名判定语言；该语言不支持或 pattern 解析失败的文件跳过。
pub(super) fn ast_candidates(
    root: &Path,
    pattern_src: &str,
    limit: usize,
    filter: &CompiledFilter,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let root = match root.canonicalize() {
        Ok(r) => r,
        Err(_) => return out,
    };
    // Pattern::try_new 会把源码解析成 AST 模式树，有实打实的解析成本；按语言缓存：同一语言只编译一次。
    let mut patterns: HashMap<SupportLang, Option<Pattern>> = HashMap::new();
    let _ = for_each_text_file(&root, |path, content| {
        if out.len() >= limit {
            return Ok(());
        }
        let Some(lang) = SupportLang::from_path(path) else {
            return Ok(());
        };
        let pattern = patterns
            .entry(lang)
            .or_insert_with(|| Pattern::try_new(pattern_src, lang).ok());
        let Some(pattern) = pattern.as_ref() else {
            return Ok(());
        };
        let source = lang.ast_grep(content);
        let root_node = source.root();
        let rel = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if !filter.passes(&rel) {
            return Ok(());
        }
        for m in root_node.find_all(pattern) {
            if out.len() >= limit {
                break;
            }
            let ast_symbol = extract_ast_symbol(&m);
            let ast_kind = Some(m.kind().to_string());
            let ast_scope = extract_enclosing_scope(&m);
            let ast_env = extract_meta_env(&m, pattern_src);

            out.push(Candidate {
                kind: classify_kind(&rel).to_string(),
                symbol: ast_symbol,
                path: rel.clone(),
                start_line: m.start_pos().line() as u64 + 1,
                end_line: m.end_pos().line() as u64 + 1,
                score: 0.0,
                snippet: truncate_chars(&m.text(), 240),
                source: "ast".to_string(),
                ast_kind,
                ast_scope,
                ast_env,
            });
        }
        Ok(())
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ast_symbol_and_scope_and_env() {
        let lang = SupportLang::Rust;
        let source = lang.ast_grep(
            "impl MyService {\n    pub fn process_data(&self, count: usize) -> bool {\n        let x = 1;\n        true\n    }\n}\n",
        );
        let pattern = Pattern::try_new("pub fn $F($$$ARGS) -> $RET { $$$BODY }", lang).unwrap();
        let matches: Vec<_> = source.root().find_all(&pattern).collect();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];

        // 1. 忠实于 AST 关系的符号名提取
        let symbol = extract_ast_symbol(m);
        assert_eq!(symbol, "process_data");

        // 2. 忠实于 AST 关系的宿主作用域层级
        let scope = extract_enclosing_scope(m);
        assert_eq!(scope, Some("MyService".to_string()));

        // 3. 忠实于 AST 关系的元变量绑定捕获
        let env = extract_meta_env(m, "pub fn $F($$$ARGS) -> $RET { $$$BODY }").unwrap();
        assert_eq!(env.get("$F").unwrap(), "process_data");
        assert_eq!(env.get("$RET").unwrap(), "bool");
        assert!(env.get("$$$ARGS").unwrap().contains("&self"));
        assert!(env.get("$$$ARGS").unwrap().contains("count: usize"));
    }

    #[test]
    fn test_ast_python_class_scope() {
        let lang = SupportLang::Python;
        let source = lang.ast_grep(
            "class UserManager:\n    def get_user(self, uid):\n        return uid\n",
        );
        let pattern = Pattern::try_new("def $NAME($$$): $$$", lang).unwrap();
        let matches: Vec<_> = source.root().find_all(&pattern).collect();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];

        let symbol = extract_ast_symbol(m);
        assert_eq!(symbol, "get_user");

        let scope = extract_enclosing_scope(m);
        assert_eq!(scope, Some("UserManager".to_string()));

        let env = extract_meta_env(m, "def $NAME($$$): $$$").unwrap();
        assert_eq!(env.get("$NAME").unwrap(), "get_user");
    }
}
