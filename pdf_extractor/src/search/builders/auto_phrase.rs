use tantivy::query::Query;
use tantivy::schema::Field;
use tantivy::query::QueryParser;
use anyhow::Context;
use crate::search::traits::QueryBuilder;
use crate::search::types::*;
use crate::search::errors::SearchError;

pub struct AutoPhraseQueryBuilder;

impl QueryBuilder for AutoPhraseQueryBuilder {
    fn build(&self, ctx: &SearchContext, input: &SearchInput) -> Result<Box<dyn Query>, SearchError> {
        parse_query_auto_phrase(&ctx.index, &input.query_str, ctx.content_field)
            .map_err(SearchError::from)
    }

    fn name(&self) -> &'static str {
        "auto_phrase"
    }
}

fn is_bare_multiword(s: &str) -> bool {
    let t = s.trim();
    t.contains(' ') && !t.contains('"')
}

pub fn parse_query_auto_phrase(
    index: &tantivy::Index,
    query_str: &str,
    field: Field,
) -> Result<Box<dyn Query>, anyhow::Error> {
    let mut qp = QueryParser::for_index(index, vec![field]);
    qp.set_conjunction_by_default();
    let adjusted = if is_bare_multiword(query_str) {
        format!("\"{}\"", query_str.trim())
    } else {
        query_str.to_string()
    };
    qp.parse_query(&adjusted).context("Failed to parse search query")
}
