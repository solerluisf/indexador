pub mod snippet;
pub mod positions;

use std::collections::VecDeque;
use std::any::TypeId;
use tantivy::query::Query;
use crate::search::traits::ResultEnricher;
use crate::search::types::*;
use crate::search::errors::SearchError;

pub struct EnricherCoordinator {
    enrichers: Vec<Box<dyn ResultEnricher>>,
    execution_order: Vec<usize>,
}

impl EnricherCoordinator {
    pub fn new(enrichers: Vec<Box<dyn ResultEnricher>>) -> Result<Self, SearchError> {
        let n = enrichers.len();
        let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
        let type_ids: Vec<TypeId> = enrichers.iter()
            .map(|e| e.type_id())
            .collect();

        for (i, e) in enrichers.iter().enumerate() {
            for dep_type in e.depends_on() {
                if let Some(j) = type_ids.iter().position(|tid| *tid == dep_type) {
                    adj[i].push(j);
                }
            }
        }

        let mut in_degree = vec![0usize; n];
        for deps in &adj {
            for &dep in deps {
                in_degree[dep] += 1;
            }
        }

        let mut q: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
        let mut order = Vec::with_capacity(n);
        while let Some(i) = q.pop_front() {
            order.push(i);
            for &j in &adj[i] {
                in_degree[j] -= 1;
                if in_degree[j] == 0 {
                    q.push_back(j);
                }
            }
        }

        if order.len() != n {
            return Err(SearchError::EnricherError(
                "Circular dependency detected in enrichers".into(),
            ));
        }

        Ok(Self { enrichers, execution_order: order })
    }

    pub fn enrich_all(
        &self,
        ctx: &SearchContext,
        input: &SearchInput,
        query: &dyn Query,
        results: &mut Vec<RichResult>,
    ) -> Result<(), SearchError> {
        for &idx in &self.execution_order {
            self.enrichers[idx].enrich(ctx, input, query, results)?;
        }
        Ok(())
    }
}
