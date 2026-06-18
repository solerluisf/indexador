use serde::Serialize;
use crate::search::types::*;
use std::collections::HashMap;

#[derive(Serialize)]
pub struct SearchResponse {
    pub success: bool,
    pub total_count: u64,
    pub page: usize,
    pub page_size: usize,
    pub query: String,
    pub strategy: String,
    pub results: Vec<SearchResult>,
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub score: f32,
    pub path: String,
    pub snippet: Option<String>,
    pub positions: Vec<PagePosition>,
    pub doc_id: Option<i64>,
}

pub struct JsonResponseBuilder;

impl JsonResponseBuilder {
    pub fn build(
        rich: Vec<RichResult>,
        total_count: u64,
        input: &SearchInput,
        strategy: &str,
    ) -> SearchResponse {
        let page = if input.limit > 0 {
            (input.offset / input.limit) + 1
        } else {
            1
        };

        let results: Vec<SearchResult> = rich.into_iter().map(|r| SearchResult {
            score: r.score,
            path: r.path,
            snippet: r.snippet,
            positions: r.positions,
            doc_id: r.doc_id,
        }).collect();

        SearchResponse {
            success: true,
            total_count,
            page,
            page_size: input.limit,
            query: input.query_str.clone(),
            strategy: strategy.to_string(),
            results,
            metadata: HashMap::new(),
        }
    }
}
