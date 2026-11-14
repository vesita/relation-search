//! 查询构造：按与索引相同的分词管线切词，词项 OR 召回 + 整串短语加分 + PriorSeek 代码概念别名与双语映射扩展。

use std::collections::HashSet;
use std::sync::OnceLock;

use sl_engine::BilingualLexicon;
use tantivy::query::{BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Term};

use crate::tokenizer::tokenize_to_strings;

/// 全局懒初始化的 PriorSeek 核心双语映射枢纽。
fn bilingual_hub() -> &'static BilingualLexicon {
    static HUB: OnceLock<BilingualLexicon> = OnceLock::new();
    HUB.get_or_init(BilingualLexicon::core)
}

/// 代码概念同义词硬映射表（双向规范化互查）：
/// 让常见缩写与全称互相召回，消解程序员在命名上的缩写鸿沟。
const CODE_ALIASES: &[(&str, &[&str])] = &[
    ("config", &["cfg", "conf", "configuration"]),
    ("cfg", &["config", "conf", "configuration"]),
    ("conf", &["config", "cfg"]),
    ("request", &["req"]),
    ("req", &["request"]),
    ("response", &["resp", "res"]),
    ("resp", &["response", "res"]),
    ("res", &["response", "resp"]),
    ("context", &["ctx"]),
    ("ctx", &["context"]),
    ("error", &["err"]),
    ("err", &["error"]),
    ("function", &["fn", "func"]),
    ("fn", &["function", "func"]),
    ("func", &["function", "fn"]),
    ("database", &["db"]),
    ("db", &["database"]),
    ("initialize", &["init"]),
    ("init", &["initialize", "initialization"]),
    ("parameter", &["param", "arg", "argument"]),
    ("param", &["parameter", "arg", "argument"]),
    ("argument", &["arg", "param", "parameter"]),
    ("arg", &["argument", "param", "parameter"]),
    ("message", &["msg"]),
    ("msg", &["message"]),
    ("index", &["idx"]),
    ("idx", &["index"]),
    ("auth", &["authentication", "authorize"]),
    ("authentication", &["auth"]),
    ("impl", &["implementation"]),
    ("implementation", &["impl"]),
    ("doc", &["document", "documentation"]),
    ("document", &["doc", "documentation"]),
    ("str", &["string"]),
    ("string", &["str"]),
    ("num", &["number"]),
    ("number", &["num"]),
];

fn get_aliases(token: &str) -> Option<&'static [&'static str]> {
    CODE_ALIASES
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(token))
        .map(|(_, aliases)| *aliases)
}

/// 把查询串构成 BM25 查询。
///
/// 关键：**不走 tantivy 的 `QueryParser`**。用户给的多半是代码片段（`IndexWriter::commit`、
/// `fn(user_id)`、`foo[0]`、`"xxx`），`QueryParser` 会把 `:`/`[`/`(`/引号当成查询语法而直接
/// 报语法错（整个 search 退出码非 0），也会把 `-`/`|`/`.` 拆成的多段强绑成「相邻短语」
/// （`index-writing` 反而搜不到）。这里复用建索引时的同一个分析管线（`build_analyzer`）把查询切成词项：
/// - 每个词项一条 `TermQuery`（`Should`/OR）：任一命中即可召回，标点不再是语法；
/// - 概念别名扩展项（`Should`/OR）：缩写与全称互相激活召回；
/// - 词项多于一个时再加一条整串 `PhraseQuery`（`Should`）：词项**相邻**出现（如完整标识符
///   `user_profile_cache`）时额外贡献 BM25 分，把「精确命中该符号」的块排到更前。
///
/// 于是既保住代码片段的召回，又保住标识符检索的排序精度。空查询返回匹配空集。
pub(super) fn build_query(content_f: Field, query: &str) -> Box<dyn Query> {
    let raw = tokenize_to_strings(query);
    let mut seen: HashSet<String> = HashSet::new();
    let mut terms: Vec<Term> = Vec::with_capacity(raw.len());
    let mut alias_terms: Vec<Term> = Vec::new();

    for token in &raw {
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if seen.insert(lower.clone()) {
            terms.push(Term::from_field_text(content_f, token));
        }

        // 1. 引入 PriorSeek 概念硬对齐先验：针对常见代码缩写进行同义召回扩展
        if let Some(aliases) = get_aliases(&lower) {
            for &alias in aliases {
                if seen.insert(alias.to_string()) {
                    alias_terms.push(Term::from_field_text(content_f, alias));
                }
            }
        }

        // 2. 引入 PriorSeek 中英双语映射枢纽：支持中文搜英文代码、英文搜中文注释
        for mapped in bilingual_hub().map_auto(token) {
            let m_lower = mapped.to_ascii_lowercase();
            if seen.insert(m_lower.clone()) {
                alias_terms.push(Term::from_field_text(content_f, &mapped));
            }
            // 联动：对映射出的英文词进一步触发缩写别名扩展
            if let Some(aliases) = get_aliases(&m_lower) {
                for &alias in aliases {
                    if seen.insert(alias.to_string()) {
                        alias_terms.push(Term::from_field_text(content_f, alias));
                    }
                }
            }
        }
    }
    if terms.is_empty() && alias_terms.is_empty() {
        return Box::new(EmptyQuery);
    }

    let mut clauses: Vec<(Occur, Box<dyn Query>)> =
        Vec::with_capacity(terms.len() + alias_terms.len() + 1);

    // 1. 原始查询词项作为主要召回项
    for t in &terms {
        clauses.push((
            Occur::Should,
            Box::new(TermQuery::new(t.clone(), IndexRecordOption::WithFreqs)),
        ));
    }

    // 2. 概念同义词作为候选扩展召回项
    for at in alias_terms {
        clauses.push((
            Occur::Should,
            Box::new(TermQuery::new(at, IndexRecordOption::WithFreqs)),
        ));
    }

    // 3. 原始词项整串 PhraseQuery 加分（完全匹配精确置顶）
    if terms.len() > 1 {
        clauses.push((Occur::Should, Box::new(PhraseQuery::new(terms))));
    }
    Box::new(BooleanQuery::new(clauses))
}
