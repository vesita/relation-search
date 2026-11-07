//! 查询构造：按与索引相同的分词管线切词，词项 OR 召回 + 整串短语加分。

use std::collections::HashSet;

use tantivy::query::{BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Term};

use crate::tokenizer::tokenize_to_strings;

/// 把查询串构成 BM25 查询。
///
/// 关键：**不走 tantivy 的 `QueryParser`**。用户给的多半是代码片段（`IndexWriter::commit`、
/// `fn(user_id)`、`foo[0]`、`"xxx`），`QueryParser` 会把 `:`/`[`/`(`/引号当成查询语法而直接
/// 报语法错（整个 search 退出码非 0），也会把 `-`/`|`/`.` 拆成的多段强绑成「相邻短语」
/// （`index-writing` 反而搜不到）。这里复用建索引时的同一个分析管线（`build_analyzer`）把查询切成词项：
/// - 每个词项一条 `TermQuery`（`Should`/OR）：任一命中即可召回，标点不再是语法；
/// - 词项多于一个时再加一条整串 `PhraseQuery`（`Should`）：词项**相邻**出现（如完整标识符
///   `user_profile_cache`）时额外贡献 BM25 分，把「精确命中该符号」的块排到更前。
///
/// 于是既保住代码片段的召回，又保住标识符检索的排序精度。空查询返回匹配空集。
pub(super) fn build_query(content_f: Field, query: &str) -> Box<dyn Query> {
    let raw = tokenize_to_strings(query);
    let mut seen: HashSet<&str> = HashSet::new();
    let mut terms: Vec<Term> = Vec::with_capacity(raw.len());
    for token in &raw {
        if token.is_empty() || !seen.insert(token.as_str()) {
            continue;
        }
        terms.push(Term::from_field_text(content_f, token));
    }
    if terms.is_empty() {
        return Box::new(EmptyQuery);
    }
    // WithFreqs：BM25 需要词频打分；content 字段本就用 WithFreqsAndPositions 建索引。
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = terms
        .iter()
        .map(|t| {
            (
                Occur::Should,
                Box::new(TermQuery::new(t.clone(), IndexRecordOption::WithFreqs)) as Box<dyn Query>,
            )
        })
        .collect();
    if terms.len() > 1 {
        clauses.push((Occur::Should, Box::new(PhraseQuery::new(terms))));
    }
    Box::new(BooleanQuery::new(clauses))
}
