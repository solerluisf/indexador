use crate::search::traits::ResultEnricher;
use crate::search::types::*;
use crate::search::errors::SearchError;

pub struct PositionEnricher;

impl ResultEnricher for PositionEnricher {
    fn enrich(
        &self,
        ctx: &SearchContext,
        input: &SearchInput,
        _query: &dyn tantivy::query::Query,
        results: &mut Vec<RichResult>,
    ) -> Result<(), SearchError> {
        let Some(ref store) = ctx.position_store else {
            return Ok(());
        };

        // Tokenizar query string en terminos individuales unicos
        let terms: Vec<&str> = input.query_str
            .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
            .filter(|t| !t.is_empty())
            .collect();
        let mut seen = std::collections::HashSet::new();
        let unique_terms: Vec<&str> = terms.into_iter()
            .filter(|t| seen.insert(t.to_lowercase()))
            .collect();

        if unique_terms.is_empty() {
            return Ok(());
        }

        for r in results.iter_mut() {
            let Some(doc_id) = r.doc_id else {
                continue;
            };
            let mut all = Vec::new();
            for term in &unique_terms {
                if let Ok(positions) = store.get_positions_by_term(doc_id, term) {
                    all.extend(positions.into_iter().map(|sp| PagePosition {
                        page: sp.page,
                        x: sp.x_min,
                        y: sp.y_min,
                        width: sp.x_max - sp.x_min,
                        height: sp.y_max - sp.y_min,
                    }));
                }
            }
            r.positions = all;
        }
        Ok(())
    }
}
