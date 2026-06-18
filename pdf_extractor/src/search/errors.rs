use std::fmt;
use std::error::Error;

#[derive(Debug)]
pub enum SearchError {
    ParseError(String),
    ExecutionError(String),
    EnricherError(String),
    UnknownStrategy(String),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::ParseError(msg) => write!(f, "Failed to parse query: {}", msg),
            SearchError::ExecutionError(msg) => write!(f, "Search execution failed: {}", msg),
            SearchError::EnricherError(msg) => write!(f, "Enricher failed: {}", msg),
            SearchError::UnknownStrategy(msg) => write!(f, "Unknown search strategy: {}", msg),
        }
    }
}

impl Error for SearchError {}

impl From<anyhow::Error> for SearchError {
    fn from(e: anyhow::Error) -> Self {
        SearchError::ExecutionError(e.to_string())
    }
}

impl From<tantivy::TantivyError> for SearchError {
    fn from(e: tantivy::TantivyError) -> Self {
        SearchError::ExecutionError(e.to_string())
    }
}

impl From<tantivy::query::QueryParserError> for SearchError {
    fn from(e: tantivy::query::QueryParserError) -> Self {
        SearchError::ParseError(e.to_string())
    }
}
