use tantivy::query::Query;
use tantivy::query::QueryParser;
use crate::search::traits::QueryBuilder;
use crate::search::types::*;
use crate::search::errors::SearchError;

pub struct BooleanPhraseQueryBuilder;

impl QueryBuilder for BooleanPhraseQueryBuilder {
    fn build(&self, ctx: &SearchContext, input: &SearchInput) -> Result<Box<dyn Query>, SearchError> {
        check_bare_operator(&input.query_str)?;
        let mut qp = QueryParser::for_index(&ctx.index, vec![ctx.content_field]);
        qp.set_conjunction_by_default();
        let adjusted = if is_bare_multiword(&input.query_str) {
            format!("\"{}\"", input.query_str.trim())
        } else {
            input.query_str.clone()
        };
        qp.parse_query(&adjusted).map_err(SearchError::from)
    }

    fn name(&self) -> &'static str {
        "boolean_phrase"
    }
}

/// Retorna true si el string es multi-word SIN operadores booleanos ni sintaxis especial.
/// En ese caso se aplica auto-phrase (comillas automaticas).
/// Si contiene operadores, se pasa directo al QueryParser de Tantivy.
fn is_bare_multiword(s: &str) -> bool {
    let t = s.trim();
    if !t.contains(' ') {
        return false;
    }
    if t.contains('"') || t.contains('(') || t.contains(')')
        || t.contains('+') || t.contains('-')
    {
        return false;
    }
    !t.split_whitespace().any(|w| {
        w.eq_ignore_ascii_case("AND")
            || w.eq_ignore_ascii_case("OR")
            || w.eq_ignore_ascii_case("NOT")
    })
}

fn check_bare_operator(query: &str) -> Result<(), SearchError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(SearchError::ParseError("Query is empty".to_string()));
    }
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        "+" | "-" => {
            return Err(SearchError::ParseError(format!(
                "'{}' operator requires a term to perform the search",
                lower
            )));
        }
        _ => {}
    }
    Ok(())
}
