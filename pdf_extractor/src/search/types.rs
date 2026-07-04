use tantivy::Index;
use tantivy::schema::Field;
use serde::Serialize;

/// Contexto compartido disponible para toda la pipeline.
/// Se resuelve una vez al inicializar el pipeline (OnceLock).
pub struct SearchContext {
    pub index: Index,
    pub id_field: Field,
    pub content_field: Field,
    pub path_field: Field,
    pub position_store: Option<crate::positions::PositionStore>,
}

/// Input normalizado de una busqueda.
pub struct SearchInput {
    pub query_str: String,
    pub field: Option<String>,
    pub limit: usize,
    pub offset: usize,
    pub path_filter: Option<String>,
    pub strategy: SearchStrategy,
}

impl Default for SearchInput {
    fn default() -> Self {
        Self {
            query_str: String::new(),
            field: None,
            limit: 50,
            offset: 0,
            path_filter: None,
            strategy: SearchStrategy::AutoPhrase,
        }
    }
}

/// Que estrategia de parsing usar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SearchStrategy {
    AutoPhrase,
    BooleanPhrase,
}

/// Resultado crudo de la ejecucion (antes de enrichment).
pub type RawResult = (f32, tantivy::TantivyDocument, tantivy::DocAddress);

/// Resultado despues de enrichment (con snippet, positions, etc).
pub struct RichResult {
    pub score: f32,
    pub path: String,
    pub snippet: Option<String>,
    pub positions: Vec<PagePosition>,
    pub doc_id: Option<i64>,
    pub doc_address: Option<tantivy::DocAddress>,
    pub matched_terms: Vec<String>,
    pub phrase_groups: Vec<Vec<String>>,
}

#[derive(Serialize)]
pub struct PagePosition {
    pub page: u32,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
