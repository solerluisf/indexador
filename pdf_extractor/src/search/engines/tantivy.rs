use tantivy::query::Query;
use tantivy::query::Occur;
use tantivy::query::BooleanQuery;
use tantivy::query::RegexQuery;
use tantivy::ReloadPolicy;
use tantivy::TantivyDocument;
use tantivy::collector::TopDocs;
use crate::search::traits::SearchEngine;
use crate::search::types::*;
use crate::search::errors::SearchError;

pub struct TantivyEngine;

impl SearchEngine for TantivyEngine {
    fn search(
        &self,
        ctx: &SearchContext,
        query: &dyn Query,
        input: &SearchInput,
    ) -> Result<Vec<(f32, TantivyDocument)>, SearchError> {
        let reader = ctx
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        if !input.query_str.trim().is_empty() {
            clauses.push((Occur::Must, query.box_clone()));
        }

        if let Some(ref pattern) = input.path_filter {
            if !pattern.is_empty() {
                let re_query = RegexQuery::from_pattern(pattern, ctx.path_field)
                    .map_err(|e| SearchError::ExecutionError(e.to_string()))?;
                clauses.push((Occur::Must, Box::new(re_query)));
            }
        }

        if clauses.is_empty() {
            return Ok(Vec::new());
        }

        let final_query: Box<dyn Query> = if clauses.len() == 1 {
            clauses.into_iter().next().unwrap().1
        } else {
            Box::new(BooleanQuery::new(clauses))
        };

        let fetch_count = input.limit.checked_add(input.offset).unwrap_or(input.limit);
        let top_docs = searcher
            .search(&*final_query, &TopDocs::with_limit(fetch_count))?;

        let mut results = Vec::new();
        for (score, doc_addr) in top_docs.iter().skip(input.offset) {
            let doc = searcher.doc::<TantivyDocument>(*doc_addr)?;
            results.push((*score, doc));
            if results.len() >= input.limit {
                break;
            }
        }
        Ok(results)
    }
}
