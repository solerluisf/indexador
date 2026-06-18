use tantivy::query::Query;
use crate::search::types::{SearchContext, SearchInput, RichResult};
use crate::search::errors::SearchError;

/// Estrategia de construccion de queries.
/// Cada implementacion encapsula UNA forma de parsear el input del usuario.
pub trait QueryBuilder: Send + Sync {
    /// Construye un tantivy::query::Query a partir del input del usuario.
    fn build(&self, ctx: &SearchContext, input: &SearchInput) -> Result<Box<dyn Query>, SearchError>;

    /// Identificador unico de la estrategia (para logging, debug, UI).
    fn name(&self) -> &'static str;
}

/// Motor de ejecucion de busqueda.
/// Recibe un query ya construido, lo ejecuta contra el indice.
/// Retorna (score, documento, doc_address) por cada resultado.
pub trait SearchEngine: Send + Sync {
    fn search(
        &self,
        ctx: &SearchContext,
        query: &dyn Query,
        input: &SearchInput,
    ) -> Result<Vec<(f32, tantivy::TantivyDocument, tantivy::DocAddress)>, SearchError>;

    /// Retorna el total de documentos que matchean el query (sin paginacion).
    fn count(
        &self,
        ctx: &SearchContext,
        query: &dyn Query,
        input: &SearchInput,
    ) -> Result<u64, SearchError>;
}

/// Enriquecimiento de resultados post-ejecucion.
pub trait ResultEnricher: Send + Sync {
    fn enrich(
        &self,
        ctx: &SearchContext,
        input: &SearchInput,
        query: &dyn Query,
        results: &mut Vec<RichResult>,
    ) -> Result<(), SearchError>;

    /// Enrichers de los que depende este (para orden topologico).
    /// Retorna vacio por defecto (sin dependencias).
    fn depends_on(&self) -> Vec<std::any::TypeId> {
        Vec::new()
    }

    fn type_id(&self) -> std::any::TypeId where Self: 'static {
        std::any::TypeId::of::<Self>()
    }
}
