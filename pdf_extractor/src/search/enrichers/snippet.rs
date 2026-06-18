use tantivy::query::Query;
use tantivy::ReloadPolicy;
use tantivy::SnippetGenerator;
use tantivy::TantivyDocument;
use crate::search::traits::ResultEnricher;
use crate::search::types::*;
use crate::search::errors::SearchError;

pub struct SnippetEnricher;

impl ResultEnricher for SnippetEnricher {
    fn enrich(
        &self,
        ctx: &SearchContext,
        _input: &SearchInput,
        query: &dyn Query,
        results: &mut Vec<RichResult>,
    ) -> Result<(), SearchError> {
        let reader = ctx
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();
        let Ok(gen) = SnippetGenerator::create(&searcher, query, ctx.content_field) else {
            return Ok(());
        };
        for r in results.iter_mut() {
            if let Some(addr) = r.doc_address {
                if let Ok(doc) = searcher.doc::<TantivyDocument>(addr) {
                    let snippet = gen.snippet_from_doc(&doc);
                    r.snippet = Some(snippet.to_html());
                }
            }
        }
        Ok(())
    }
}
