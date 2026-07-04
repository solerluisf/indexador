use tantivy::schema::Value;
use tantivy::TantivyDocument;
use tantivy::DocAddress;
use crate::search::traits::{QueryBuilder, SearchEngine};
use crate::search::types::*;
use crate::search::errors::SearchError;
use crate::search::enrichers::EnricherCoordinator;
use crate::search::response::{SearchResponse, JsonResponseBuilder};

pub struct SearchPipeline {
    ctx: SearchContext,
    builder: Box<dyn QueryBuilder>,
    engine: Box<dyn SearchEngine>,
    enrichers: EnricherCoordinator,
}

impl SearchPipeline {
    pub fn new(
        ctx: SearchContext,
        builder: Box<dyn QueryBuilder>,
        engine: Box<dyn SearchEngine>,
        enrichers: EnricherCoordinator,
    ) -> Self {
        Self { ctx, builder, engine, enrichers }
    }

    pub fn ctx(&self) -> &SearchContext {
        &self.ctx
    }

    pub fn execute_raw(
        &self,
        input: &SearchInput,
    ) -> Result<Vec<(f32, TantivyDocument)>, SearchError> {
        let query = self.builder.build(&self.ctx, input)?;
        let results = self.engine.search(&self.ctx, &*query, input)?;
        Ok(results.into_iter().map(|(s, d, _)| (s, d)).collect())
    }

    pub fn execute(
        &self,
        input: &SearchInput,
    ) -> Result<Vec<RichResult>, SearchError> {
        let query = self.builder.build(&self.ctx, input)?;
        let raw = self.engine.search(&self.ctx, &*query, input)?;
        let mut rich = raw_to_rich(raw, &self.ctx);
        self.enrichers.enrich_all(&self.ctx, input, &*query, &mut rich)?;
        Ok(rich)
    }

    pub fn execute_to_response(
        &self,
        input: &SearchInput,
    ) -> Result<SearchResponse, SearchError> {
        let query = self.builder.build(&self.ctx, input)?;
        let total_count = self.engine.count(&self.ctx, &*query, input)?;
        let raw = self.engine.search(&self.ctx, &*query, input)?;
        let mut rich = raw_to_rich(raw, &self.ctx);
        self.enrichers.enrich_all(&self.ctx, input, &*query, &mut rich)?;
        Ok(JsonResponseBuilder::build(rich, total_count, input, self.builder.name()))
    }
}

pub fn raw_to_rich(
    raw: Vec<(f32, TantivyDocument, DocAddress)>,
    ctx: &SearchContext,
) -> Vec<RichResult> {
    raw.into_iter().map(|(score, doc, addr)| {
        let path = doc
            .get_first(ctx.path_field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let doc_id = doc
            .get_first(ctx.id_field)
            .and_then(|v| v.as_u64());
        RichResult {
            score,
            path,
            snippet: None,
            positions: Vec::new(),
            doc_id: doc_id.map(|id| id as i64),
            doc_address: Some(addr),
            matched_terms: Vec::new(),
            phrase_groups: Vec::new(),
        }
    }).collect()
}

pub fn default_enrichers() -> EnricherCoordinator {
    EnricherCoordinator::new(vec![
        Box::new(crate::search::enrichers::term_collector::TermCollectorEnricher),
        Box::new(crate::search::enrichers::snippet::SnippetEnricher),
    ]).unwrap()
}
