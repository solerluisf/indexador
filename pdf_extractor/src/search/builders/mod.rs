pub mod auto_phrase;
pub mod boolean_phrase;

pub use auto_phrase::AutoPhraseQueryBuilder;
pub use auto_phrase::parse_query_auto_phrase;
pub use boolean_phrase::BooleanPhraseQueryBuilder;

use crate::search::traits::QueryBuilder;

/// Retorna la estrategia por defecto (AutoPhrase = comportamiento actual).
pub fn default_strategy() -> Box<dyn QueryBuilder> {
    Box::new(AutoPhraseQueryBuilder)
}
