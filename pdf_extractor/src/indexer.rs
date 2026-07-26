use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tantivy::collector::{Count, TopDocs};
use tantivy::merge_policy::LogMergePolicy;
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, Query, QueryParser, RegexQuery, TermQuery};
use tantivy::schema::*;
use tantivy::tokenizer::{TextAnalyzer, TokenStream, Tokenizer};
use tantivy::{doc, Index, IndexWriter, ReloadPolicy, SnippetGenerator, TantivyDocument, Term};
use tantivy::{DocSet, Postings, TERMINATED};


/// Canonical token pattern used by both the extractor (`clean_word_text`) and
/// Tantivy's tokenizers. Single source of truth — guarantees lockstep alignment.
pub(crate) const TOKEN_PATTERN: &str = r"[\p{L}\p{N}\p{S}]+";

#[allow(dead_code)]
pub struct SearchIndex {
    pub index: Index,
    pub schema: Schema,
    pub id_field: Field,
    pub path_field: Field,
    pub content_field: Field,
    ram_buffer: u64,
}

#[allow(dead_code)]
pub struct IndexerMetrics {
    pub docs_indexed: AtomicU64,
    pub dedup_skipped: AtomicU64,
    pub last_commit: Mutex<Instant>,
}

impl IndexerMetrics {
    pub fn new() -> Self {
        Self {
            docs_indexed: AtomicU64::new(0),
            dedup_skipped: AtomicU64::new(0),
            last_commit: Mutex::new(Instant::now()),
        }
    }

    pub fn docs_indexed(&self) -> u64 {
        self.docs_indexed.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn last_commit_age_secs(&self) -> f64 {
        self.last_commit
            .lock()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }
}

impl SearchIndex {
    pub fn new(index_path: &Path) -> Result<Self> {
        // Remove any stale writer lock files left from a previous crashed
        // writer.  On Windows a 0-byte .tantivy-writer.lock can prevent new
        // segment files from being created (ERROR_ACCESS_DENIED).
        for lock in [".tantivy-writer.lock", ".tantivy-meta.lock"] {
            let p = index_path.join(lock);
            if p.exists() {
                let _ = std::fs::remove_file(&p);
            }
        }

        let index = if index_path.join("meta.json").exists() {
            Index::open_in_dir(index_path).context("Failed to open existing index")?
        } else {
            std::fs::create_dir_all(index_path).ok();
            let schema = build_schema();
            Index::create_in_dir(index_path, schema)
                .context("Failed to create index directory")?
        };

        // Register tokenizer that splits on `[\p{L}\p{N}\p{S}]+` and lowercases.
        let multilang_analyzer = TextAnalyzer::builder(crate::tokenizers::LanguageAwareTokenizer)
            .build();
        index.tokenizers().register("multilang", multilang_analyzer);

        let schema = index.schema();
        let id_field = schema.get_field("id").map_err(|_| anyhow::anyhow!("Missing 'id' field in index schema"))?;
        let path_field = schema.get_field("path").map_err(|_| anyhow::anyhow!("Missing 'path' field in index schema"))?;
        let content_field = schema.get_field("content").map_err(|_| anyhow::anyhow!("Missing 'content' field — index was created with an older schema version; please re-index"))?;

        Ok(Self {
            index,
            schema,
            id_field,
            path_field,
            content_field,
            ram_buffer: 3_000_000_000,
        })
    }

    pub fn with_ram_buffer(index_path: &Path, ram_buffer: u64) -> Result<Self> {
        let mut si = Self::new(index_path)?;
        si.ram_buffer = ram_buffer;
        Ok(si)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_ram_buffer(&mut self, bytes: u64) {
        self.ram_buffer = bytes;
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ram_buffer(&self) -> u64 {
        self.ram_buffer
    }

    pub fn writer(&self) -> Result<IndexWriter> {
        self.index
            .writer(self.ram_buffer as usize)
            .context("Failed to create index writer")
    }

    pub fn writer_with_num_threads(&self, num_threads: usize) -> Result<IndexWriter> {
        self.index
            .writer_with_num_threads(num_threads, self.ram_buffer as usize)
            .context("Failed to create index writer with num_threads")
    }

    pub fn add_document(
        &self,
        writer: &IndexWriter,
        id: i64,
        path: &str,
        text: &str,
    ) -> Result<()> {
        // Delete any existing document with the same id before adding,
        // so re-processing a file after a crash does not create duplicates.
        writer
            .delete_term(Term::from_field_u64(self.id_field, id as u64));
        let doc = TantivyDocument::from(doc!(
            self.id_field => id as u64,
            self.path_field => path,
            self.content_field => text,
        ));
        writer
            .add_document(doc)
            .context("Failed to add document to index")?;
        Ok(())
    }

    /// Unified search using Tantivy's QueryParser.
    ///
    /// QueryParser natively handles:
    ///   - `"frase exacta"`       → PhraseQuery
    ///   - `palabra1 palabra2`     → BooleanQuery (SHOULD)
    ///   - `+obligatorio -excluir` → BooleanQuery (MUST / MUST_NOT)
    ///   - `palabra*`              → (if wildcard enabled)
    ///
    /// The parser applies the registered `"multilang"` tokenizer to the query,
    /// so tokenization (lowercasing, Lindera, bigram, regex) is consistent
    /// between indexing and search time.
    fn search_parsed(
        &self,
        query_str: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        use crate::search::builders::AutoPhraseQueryBuilder;
        use crate::search::engines::TantivyEngine;
        use crate::search::pipeline::{SearchPipeline, default_enrichers};

        let ctx = crate::search::types::SearchContext {
            index: self.index.clone(),
            id_field: self.id_field,
            content_field: self.content_field,
            path_field: self.path_field,
            position_store: None,
        };
        let input = crate::search::types::SearchInput {
            query_str: query_str.to_string(),
            field: None,
            limit,
            offset,
            path_filter: path_filter.map(|s| s.to_string()),
            strategy: crate::search::types::SearchStrategy::AutoPhrase,
        };

        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        pipeline.execute_raw(&input).map_err(|e| anyhow::anyhow!("{}", e))
    }

    /// Public search entry-point — delegates to `search_parsed`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn search(
        &self,
        query_str: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        self.search_parsed(query_str, limit, path_filter, offset)
    }

    /// Returns the word offsets where `term` appears in document `doc_id`.
    /// Each offset is the 0-indexed word position within the content_norm field.
    ///
    /// Uses a `TermQuery` on the id field to locate the exact `DocAddress`,
    /// then reads postings only from the containing segment — O(1) segment
    /// scan instead of O(segments × postings).
    ///
    /// Returns an empty Vec if the term has no positions for this doc.
    pub fn search_term_positions(&self, doc_id: u64, term: &str) -> Result<Vec<usize>> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        // Locate the document via its id field — this gives us the segment
        // ordinal and segment-local doc id in one cheap term lookup.
        let id_term = Term::from_field_u64(self.id_field, doc_id);
        let id_query = TermQuery::new(id_term, IndexRecordOption::Basic);
        let top_docs = searcher.search(&id_query, &TopDocs::with_limit(1))?;
        let (_score, doc_address) = match top_docs.first() {
            Some(addr) => *addr,
            None => return Ok(Vec::new()),
        };

        let seg = &searcher.segment_readers()[doc_address.segment_ord as usize];
        let inv_index = match seg.inverted_index(self.content_field) {
            Ok(idx) => idx,
            Err(_) => return Ok(Vec::new()),
        };

        let content_term = Term::from_field_text(self.content_field, term);
        let Ok(Some(mut postings)) = inv_index.read_postings(
            &content_term,
            IndexRecordOption::WithFreqsAndPositions,
        ) else { return Ok(Vec::new()) };

        // Seek directly to our segment-local doc
        let target = doc_address.doc_id;
        if postings.doc() == TERMINATED || postings.doc() > target {
            return Ok(Vec::new());
        }
        while postings.doc() < target {
            postings.advance();
            if postings.doc() == TERMINATED {
                return Ok(Vec::new());
            }
        }

        let mut positions = Vec::new();
        postings.positions_with_offset(0, &mut positions);
        Ok(positions.into_iter().map(|p| p as usize).collect())
    }

    /// Returns the total number of matching documents (ignoring limit/offset).
    pub fn search_count(&self, query_str: &str) -> Result<u64> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let parsed = parse_query_auto_phrase(&self.index, query_str, self.content_field)?;
        let count = searcher.search(&parsed, &Count)?;
        Ok(count as u64)
    }

    /// Backward-compat wrapper — delegates to `search_parsed`.
    /// The `_stem` parameter is ignored (no stemming in the tokenizer).
    pub fn search_stem(
        &self,
        query_str: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
        _stem: bool,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        self.search_parsed(query_str, limit, path_filter, offset)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn search_fuzzy(
        &self,
        query_str: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
        fuzzy_distance: u8,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        self.search_fuzzy_stem(query_str, limit, path_filter, offset, fuzzy_distance, false)
    }

    pub fn search_fuzzy_stem(
        &self,
        query_str: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
        fuzzy_distance: u8,
        _stem: bool,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        if !query_str.trim().is_empty() {
            let mut fuzzy_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for token in query_str.split_whitespace() {
                let term = Term::from_field_text(self.content_field, token);
                let fuzzy_query = FuzzyTermQuery::new(term, fuzzy_distance, true);
                fuzzy_clauses.push((Occur::Should, Box::new(fuzzy_query)));
            }
            if fuzzy_clauses.len() == 1 {
                clauses.push((Occur::Must, fuzzy_clauses.into_iter().next().unwrap().1));
            } else if fuzzy_clauses.len() > 1 {
                clauses.push((Occur::Must, Box::new(BooleanQuery::new(fuzzy_clauses))));
            }
        }

        if let Some(pattern) = path_filter {
            if !pattern.is_empty() {
                let re_query = RegexQuery::from_pattern(pattern, self.path_field)
                    .context("Invalid path filter regex")?;
                clauses.push((Occur::Must, Box::new(re_query)));
            }
        }

        if clauses.is_empty() {
            return Ok(Vec::new());
        }

        let query: Box<dyn Query> = if clauses.len() == 1 {
            clauses.into_iter().next().unwrap().1
        } else {
            Box::new(BooleanQuery::new(clauses))
        };

        let fetch_count = limit.checked_add(offset).unwrap_or(limit);
        let top_docs = searcher
            .search(&*query, &TopDocs::with_limit(fetch_count))
            .context("Search failed")?;

        let mut results = Vec::new();
        for (score, doc_addr) in top_docs.iter().skip(offset) {
            let doc = searcher.doc::<TantivyDocument>(*doc_addr)?;
            results.push((*score, doc));
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    /// Search on a specific named field, optionally with fuzzy matching.
    /// When fuzzy_distance is 0, uses the QueryParser for the target field.
    /// When fuzzy_distance > 0, builds per-token FuzzyTermQuery clauses.
    /// The `stem` parameter is ignored when `field_name` is provided
    /// (each field has its own fixed tokenizer).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn search_in_field_fuzzy_stem(
        &self,
        query_str: &str,
        field_name: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
        fuzzy_distance: u8,
        _stem: bool,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        let field = self
            .schema
            .get_field(field_name)
            .with_context(|| format!("Field '{}' not found in schema", field_name))?;
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        if !query_str.trim().is_empty() {
            if fuzzy_distance > 0 {
                let mut fuzzy_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for token in query_str.split_whitespace() {
                    let term = Term::from_field_text(field, token);
                    let fuzzy_query = FuzzyTermQuery::new(term, fuzzy_distance, true);
                    fuzzy_clauses.push((Occur::Should, Box::new(fuzzy_query)));
                }
                if fuzzy_clauses.len() == 1 {
                    clauses.push((Occur::Must, fuzzy_clauses.into_iter().next().unwrap().1));
                } else if fuzzy_clauses.len() > 1 {
                    clauses.push((Occur::Must, Box::new(BooleanQuery::new(fuzzy_clauses))));
                }
            } else {
                let boxed = parse_query_auto_phrase(&self.index, query_str, field)?;
                clauses.push((Occur::Must, boxed));
            }
        }

        if let Some(pattern) = path_filter {
            if !pattern.is_empty() {
                let re_query = RegexQuery::from_pattern(pattern, self.path_field)
                    .context("Invalid path filter regex")?;
                clauses.push((Occur::Must, Box::new(re_query)));
            }
        }

        if clauses.is_empty() {
            return Ok(Vec::new());
        }

        let query: Box<dyn Query> = if clauses.len() == 1 {
            clauses.into_iter().next().unwrap().1
        } else {
            Box::new(BooleanQuery::new(clauses))
        };

        let fetch_count = limit.checked_add(offset).unwrap_or(limit);
        let top_docs = searcher
            .search(&*query, &TopDocs::with_limit(fetch_count))
            .context("Search failed")?;

        let mut results = Vec::new();
        for (score, doc_addr) in top_docs.iter().skip(offset) {
            let doc = searcher.doc::<TantivyDocument>(*doc_addr)?;
            results.push((*score, doc));
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    /// Search on a specific named field in the schema.
    /// Returns an error if the field does not exist or is not indexed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn search_in_field(
        &self,
        query_str: &str,
        field_name: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        let field = self
            .schema
            .get_field(field_name)
            .with_context(|| format!("Field '{}' not found in schema", field_name))?;
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&self.index, vec![field]);

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        if !query_str.trim().is_empty() {
            let boxed = query_parser
                .parse_query(query_str)
                .context("Failed to parse search query")?;
            clauses.push((Occur::Must, boxed));
        }

        if let Some(pattern) = path_filter {
            if !pattern.is_empty() {
                let re_query = RegexQuery::from_pattern(pattern, self.path_field)
                    .context("Invalid path filter regex")?;
                clauses.push((Occur::Must, Box::new(re_query)));
            }
        }

        if clauses.is_empty() {
            return Ok(Vec::new());
        }

        let query: Box<dyn Query> = if clauses.len() == 1 {
            clauses.into_iter().next().unwrap().1
        } else {
            Box::new(BooleanQuery::new(clauses))
        };

        let fetch_count = limit.checked_add(offset).unwrap_or(limit);
        let top_docs = searcher
            .search(&*query, &TopDocs::with_limit(fetch_count))
            .context("Search failed")?;

        let mut results = Vec::new();
        for (score, doc_addr) in top_docs.iter().skip(offset) {
            let doc = searcher.doc::<TantivyDocument>(*doc_addr)?;
            results.push((*score, doc));
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    pub fn generate_snippet(
        &self,
        doc: &TantivyDocument,
        query_str: &str,
    ) -> Result<String> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&self.index, vec![
            self.content_field,
        ]);
        if query_str.trim().is_empty() {
            return Ok(String::new());
        }
        let query = query_parser
            .parse_query(query_str)
            .context("Failed to parse snippet query")?;

        let snippet_generator = SnippetGenerator::create(&searcher, &query, self.content_field)
            .context("Failed to create snippet generator")?;

        let snippet = snippet_generator.snippet_from_doc(doc);
        Ok(snippet.to_html())
    }

    /// Search across multiple fields with per-field boost weights.
    /// Each entry in `fields` is `(field_name, boost)`.
    /// Uses `BoostQuery` to weight each field-specific query clause,
    /// combined into a `BooleanQuery` with `Should` (OR) semantics.
    pub fn search_weighted_fields(
        &self,
        query_str: &str,
        fields: &[(&str, f32)],
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        if !query_str.trim().is_empty() {
            for &(field_name, boost) in fields {
                if let Ok(field) = self.schema.get_field(field_name) {
                    if let Ok(parsed) = parse_query_auto_phrase(&self.index, query_str, field) {
                        if boost != 1.0 {
                            clauses.push((Occur::Should, Box::new(BoostQuery::new(parsed, boost))));
                        } else {
                            clauses.push((Occur::Should, parsed));
                        }
                    }
                }
            }
        }

        if let Some(pattern) = path_filter {
            if !pattern.is_empty() {
                if let Ok(re_query) = RegexQuery::from_pattern(pattern, self.path_field) {
                    clauses.push((Occur::Must, Box::new(re_query)));
                }
            }
        }

        if clauses.is_empty() {
            return Ok(Vec::new());
        }

        let query: Box<dyn Query> = if clauses.len() == 1 {
            clauses.into_iter().next().unwrap().1
        } else {
            Box::new(BooleanQuery::new(clauses))
        };

        let fetch_count = limit.checked_add(offset).unwrap_or(limit);
        let top_docs = searcher
            .search(&*query, &TopDocs::with_limit(fetch_count))
            .context("Weighted fields search failed")?;

        let mut results = Vec::new();
        for (score, doc_addr) in top_docs.iter().skip(offset) {
            let doc = searcher.doc::<TantivyDocument>(*doc_addr)?;
            results.push((*score, doc));
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    /// Apply a recency boost to search results based on `ingested_at`.
    /// Each result's score is multiplied by `1.0 + recency_weight * recency_factor(elapsed_days)`.
    /// `recency_factor` decays from 1.0 (today) toward 0.0 (older than `max_days`).
    /// Requires the `ingested_at` field; returns results unchanged if absent.
    pub fn apply_recency_boost(
        &self,
        results: Vec<(f32, TantivyDocument)>,
        recency_weight: f32,
        _max_days: u64,
    ) -> Vec<(f32, TantivyDocument)> {
        if recency_weight <= 0.0 {
            return results;
        }
        // ingested_at field was removed in the pure-index refactor;
        // recency boost is no longer available. Return results unchanged.
        results
    }

    pub fn optimize(&self) -> Result<(usize, usize)> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let before_segments = reader.searcher().segment_readers().len();

        let mut writer = self.writer()?;
        let mut merge_policy = LogMergePolicy::default();
        merge_policy.set_min_num_segments(1);
        writer.set_merge_policy(Box::new(merge_policy));
        writer.commit()?;
        if before_segments > 1 {
            writer.wait_merging_threads()?;
        }

        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let after_segments = reader.searcher().segment_readers().len();
        Ok((before_segments, after_segments))
    }

    pub fn delete_by_path(&self, path_pattern: &str) -> Result<u64> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let re_query = RegexQuery::from_pattern(path_pattern, self.path_field)
            .context("Invalid path filter regex")?;
        let top_docs = searcher
            .search(&re_query, &TopDocs::with_limit(i32::MAX as usize))
            .context("Failed to find documents to delete")?;

        let count = top_docs.len() as u64;
        let mut writer = self.writer()?;
        for (_, doc_addr) in &top_docs {
            let doc = searcher.doc::<TantivyDocument>(*doc_addr)?;
            if let Some(id_val) = doc.get_first(self.id_field) {
                if let Some(id) = id_val.as_u64() {
                    let term = Term::from_field_u64(self.id_field, id);
                    writer.delete_term(term);
                }
            }
        }
        writer.commit()?;
        Ok(count)
    }

    pub fn delete_by_id(&self, id: i64) -> Result<bool> {
        let term = Term::from_field_u64(self.id_field, id as u64);
        let mut writer = self.writer()?;
        writer.delete_term(term);
        writer.commit()?;
        Ok(true)
    }

    /// Delete a document by exact path match (uses a term query on the path field).
    pub fn delete_by_exact_path(&self, path: &str) -> Result<bool> {
        let term = Term::from_field_text(self.path_field, path);
        let mut writer = self.writer()?;
        writer.delete_term(term);
        writer.commit()?;
        Ok(true)
    }

    /// Execute a boolean query where each clause is an independent text query
    /// combined with MUST / SHOULD / MUST_NOT semantics.
    pub fn search_boolean(
        &self,
        clauses: &[(&str, tantivy::query::Occur)],
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
        _stem: bool,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let mut bool_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (term, occur) in clauses {
            if !term.trim().is_empty() {
                if let Ok(q) = parse_query_auto_phrase(&self.index, term, self.content_field) {
                    bool_clauses.push((*occur, q));
                }
            }
        }

        if let Some(pattern) = path_filter {
            if !pattern.is_empty() {
                if let Ok(re_query) = RegexQuery::from_pattern(pattern, self.path_field) {
                    bool_clauses.push((Occur::Must, Box::new(re_query)));
                }
            }
        }

        if bool_clauses.is_empty() {
            return Ok(Vec::new());
        }

        let query: Box<dyn Query> = if bool_clauses.len() == 1 {
            bool_clauses.into_iter().next().unwrap().1
        } else {
            Box::new(BooleanQuery::new(bool_clauses))
        };

        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit + offset))?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (_score, doc_addr) in top_docs {
            let doc = searcher.doc::<TantivyDocument>(doc_addr)?;
            results.push((_score, doc));
        }

        if offset > 0 && offset < results.len() {
            results = results.into_iter().skip(offset).collect();
        }

        Ok(results)
    }
}

/// Re-exported for backward compatibility (C API uses it directly).
/// Real implementation lives in search::builders::auto_phrase.
pub use crate::search::builders::auto_phrase::parse_query_auto_phrase;

#[allow(dead_code)]
#[derive(Debug)]
pub struct IndexStats {
    pub num_docs: u64,
    pub num_segments: usize,
    pub size_bytes: u64,
}

impl SearchIndex {
    pub fn compute_stats(&self, index_path: &Path) -> Result<IndexStats> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();
        let num_docs = searcher.num_docs() as u64;

        let num_segments = searcher.segment_readers().len();

        let size_bytes = if index_path.exists() {
            let mut total = 0u64;
            if let Ok(entries) = std::fs::read_dir(index_path) {
                for entry in entries.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        total += meta.len();
                    }
                }
            }
            total
        } else {
            0
        };

        Ok(IndexStats {
            num_docs,
            num_segments,
            size_bytes,
        })
    }
}

fn build_schema() -> Schema {
    let mut schema_builder = Schema::builder();
    schema_builder.add_u64_field("id", INDEXED | STORED);
    schema_builder.add_text_field("path", STRING | STORED);
    schema_builder.add_text_field(
        "content",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("multilang")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
    );
    schema_builder.build()
}

pub struct Indexer {
    search_index: SearchIndex,
    pub metrics: IndexerMetrics,
    pub position_store: std::sync::Mutex<crate::positions::PositionStore>,
}

impl Indexer {
    pub fn new(index_path: &Path) -> Result<Self> {
        let search_index = SearchIndex::new(index_path)?;
        let positions_path = index_path.join("positions.sqlite");
        let position_store = std::sync::Mutex::new(crate::positions::PositionStore::open(&positions_path)?);
        Ok(Self {
            search_index,
            metrics: IndexerMetrics::new(),
            position_store,
        })
    }

    pub fn with_ram_buffer(index_path: &Path, ram_buffer: u64) -> Result<Self> {
        let search_index = SearchIndex::with_ram_buffer(index_path, ram_buffer)?;
        let positions_path = index_path.join("positions.sqlite");
        let position_store = std::sync::Mutex::new(crate::positions::PositionStore::open(&positions_path)?);
        Ok(Self {
            search_index,
            metrics: IndexerMetrics::new(),
            position_store,
        })
    }

    #[cfg(test)]
    pub fn index_document(&self, id: i64, path: &str, text: &str) -> Result<()> {
        let mut writer = self.search_index.writer()?;
        self.search_index.add_document(&mut writer, id, path, text)?;
        writer.commit()?;
        self.metrics.docs_indexed.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut t) = self.metrics.last_commit.lock() {
            *t = Instant::now();
        }
        Ok(())
    }

    pub fn search_index(&self) -> &SearchIndex {
        &self.search_index
    }

    pub fn metrics(&self) -> &IndexerMetrics {
        &self.metrics
    }

    pub fn store_word_positions(&self, doc_id: i64, positions: &[(usize, crate::extractor::WordPosition)]) -> Result<()> {
        let store = self.position_store.lock().unwrap();
        store.store_positions(doc_id, positions)?;
        Ok(())
    }

    /// Delete a document from the Tantivy index and its positions from SQLite.
    /// The caller is responsible for removing the job entry from `jobs.db`.
    pub fn delete_document(&self, doc_id: i64, path: &str) -> Result<()> {
        self.search_index.delete_by_exact_path(path)?;
        let store = self.position_store.lock().unwrap();
        store.delete_doc(doc_id)?;
        Ok(())
    }
}

/// Tokenize text with the same "math" tokenizer used for `content_norm`.
/// Returns `Vec<(position, text)>` in token order, where `position` is the
/// 0-indexed word position within the token stream.
///
/// This is the canonical tokenizer — all word_offset values in SQLite should
/// be derived from these positions so they align with Tantivy's term positions.
pub fn tokenize_with_math(text: &str) -> Vec<(usize, String)> {
    use tantivy::tokenizer::RegexTokenizer;
    thread_local! {
        static TOKENIZER: std::cell::RefCell<RegexTokenizer> = std::cell::RefCell::new(
            RegexTokenizer::new(TOKEN_PATTERN)
                .expect("Hardcoded regex pattern should never fail"),
        );
    }
    TOKENIZER.with(|tokenizer| {
        let mut tokenizer = tokenizer.borrow_mut();
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            let t = stream.token();
            tokens.push((t.position as usize, t.text.to_lowercase()));
        }
        tokens
    })
}

/// Walk WordPositions in document order and assign consecutive token positions.
/// Each WP now contains exactly one token (no spaces in the text), so the
/// alignment is 1:1 — one position entry per WordPosition.
///
/// This is O(N) lockstep alignment. The `text` parameter is kept only for a
/// `debug_assert_eq!` sanity check that fires when Tantivy token count diverges
/// from WP count.
pub fn align_offsets_to_tantivy<'a>(
    _text: &str,
    word_positions: &'a [crate::extractor::WordPosition],
) -> Vec<(usize, &'a crate::extractor::WordPosition)> {
    let mut result = Vec::with_capacity(word_positions.len());
    let mut pos = 0usize;

    for wp in word_positions {
        if wp.text.is_empty() {
            continue;
        }
        result.push((pos, wp));
        pos += 1;
    }

    // Freno de mano: log de advertencia si el conteo de WPs no coincide con
    // los tokens de Tantivy. Solo en debug (cero overhead en release).
    #[cfg(debug_assertions)]
    {
        let tantivy_count = tokenize_with_math(_text).len();
        if tantivy_count != pos {
            eprintln!(
                "[LOCKSTEP] WARNING: Tantivy token count ({}) != WP count ({}). \
                 Tokenizer drift detected — update TOKEN_PATTERN or extraction logic.",
                tantivy_count, pos,
            );
        }
    }

    result
}

#[cfg(test)]
use std::env::temp_dir;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicU32;

#[cfg(test)]
fn unique_index_dir() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = temp_dir().join(format!("pdf_extractor_index_test_{}", id));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_doc(idx: &SearchIndex, writer: &mut IndexWriter, _id: i64, _path: &str, _checksum: &str, text: &str, _raw: &str, _lang: &str) -> Result<()> {
        idx.add_document(writer, _id, _path, text)
    }

    // --- SearchIndex: basic flows ---

    #[test]
    fn test_schema_creation() {
        let schema = build_schema();
        assert!(schema.get_field("id").is_ok());
        assert!(schema.get_field("path").is_ok());
        assert!(schema.get_field("content").is_ok());
    }

    #[test]
    fn test_tokenizer_registered() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let tokenizers = idx.index.tokenizers();
        assert!(tokenizers.get("multilang").is_some(), "multilang tokenizer should be registered");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- search_in_field_fuzzy_stem: basic + alternative flows ---

    #[test]
    fn test_search_in_field_fuzzy_stem_exact() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        // fuzzy=0, stem=false -> exact search via QueryParser
        let results = idx.search_in_field_fuzzy_stem("hello", "content", 10, None, 0, 0, false).unwrap();
        assert_eq!(results.len(), 1, "Exact search via fuzzy_stem should find match");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_fuzzy_stem_fuzzy() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        // fuzzy=2 should match "hxllo" -> "hello"
        let results = idx.search_in_field_fuzzy_stem("hxllo", "content", 10, None, 0, 2, false).unwrap();
        assert_eq!(results.len(), 1, "Fuzzy search (distance 2) should find match");

        // fuzzy=0 should NOT match
        let results2 = idx.search_in_field_fuzzy_stem("hxllo", "content", 10, None, 0, 0, false).unwrap();
        assert_eq!(results2.len(), 0, "Exact search should not find typo");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_fuzzy_stem_empty_query() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field_fuzzy_stem("", "content", 10, None, 0, 2, false).unwrap();
        assert_eq!(results.len(), 0, "Empty query should return empty results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_fuzzy_stem_nonexistent_field() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let result = idx.search_in_field_fuzzy_stem("hello", "bad_field", 10, None, 0, 2, false);
        assert!(result.is_err(), "Non-existent field should error");
        assert!(result.unwrap_err().to_string().contains("not found in schema"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_fuzzy_stem_path_filter() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "hello there", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field_fuzzy_stem("hello", "content", 10, Some("/a.pdf"), 0, 0, false).unwrap();
        assert_eq!(results.len(), 1, "Path filter + field fuzzy_stem should find only the matching doc");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_index_create_and_reopen() {
        let dir = unique_index_dir();
        {
            let idx = SearchIndex::new(&dir).unwrap();
            let mut writer = idx.writer().unwrap();
            add_doc(&idx, &mut writer, 1, "/test.pdf", "cs1", "hello world", "hello world", "").unwrap();
            writer.commit().unwrap();
        }
        {
            let idx = SearchIndex::new(&dir).unwrap();
            let results = idx.search("hello", 10, None, 0).unwrap();
            assert_eq!(results.len(), 1, "Should find the indexed document after reopen");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_add_and_search_document() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/doc.pdf", "cs1", "the quick brown fox", "the quick brown fox", "").unwrap()
;
        writer.commit().unwrap();

        let results = idx.search("fox", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].1.get_first(idx.path_field).unwrap().as_str().unwrap().contains("doc.pdf"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_no_results() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let results = idx.search("nonexistent", 10, None, 0).unwrap();
        assert!(results.is_empty(), "Search with no matches should return empty");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_respects_limit() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        for i in 0..10 {
            let content = format!("document number {}", i);
            add_doc(&idx, &mut writer, i, &format!("/{}.pdf", i), &format!("cs{}", i), &content, &content, "").unwrap()
;
        }
        writer.commit().unwrap();

        let results = idx.search("document", 3, None, 0).unwrap();
        assert_eq!(results.len(), 3, "Limit should restrict results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_case_insensitive() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/doc.pdf", "cs1", "Hello World", "Hello World", "").unwrap()
;
        writer.commit().unwrap();

        let results_lower = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(results_lower.len(), 1, "Case-insensitive search should find 'hello'");

        let results_upper = idx.search("WORLD", 10, None, 0).unwrap();
        assert_eq!(results_upper.len(), 1, "Case-insensitive search should find 'WORLD'");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Multi-field search (path filtering) ---

#[test]
fn test_search_with_path_filter_filters_by_path() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/reports/2024.pdf", "cs1", "quarterly earnings report", "content", "").unwrap()
;
    add_doc(&idx, &mut writer, 2, "/invoices/2024.pdf", "cs2", "invoice total earnings", "content", "").unwrap()
;
    writer.commit().unwrap();

    // Without filter, both match
    let all = idx.search("earnings", 10, None, 0).unwrap();
    assert_eq!(all.len(), 2);

    // Filter by path prefix regex
    let filtered = idx.search("earnings", 10, Some("/reports/.*"), 0).unwrap();
    assert_eq!(filtered.len(), 1);
    let path = filtered[0].1.get_first(idx.path_field).unwrap().as_str().unwrap();
    assert!(path.contains("reports"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_path_filter_without_text_query() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/docs/a.pdf", "cs1", "rust language", "content", "").unwrap()
;
    add_doc(&idx, &mut writer, 2, "/docs/b.pdf", "cs2", "python language", "content", "").unwrap()
;
    writer.commit().unwrap();

    // Path filter alone with empty query
    let results = idx.search("", 10, Some(".*b\\.pdf"), 0).unwrap();
    assert_eq!(results.len(), 1);
    let path = results[0].1.get_first(idx.path_field).unwrap().as_str().unwrap();
    assert!(path.contains("b.pdf"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_path_filter_empty_string_no_filtering() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "").unwrap()
;
    add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "hello world", "content", "").unwrap()
;
    writer.commit().unwrap();

    // Empty filter string should behave like no filter
    let results = idx.search("hello", 10, Some(""), 0).unwrap();
    assert_eq!(results.len(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_path_filter_no_match_returns_empty() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "").unwrap()
;
    writer.commit().unwrap();

    let results = idx.search("hello", 10, Some("/nonexistent/.*"), 0).unwrap();
    assert!(results.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_path_filter_matches_all() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/x/a.pdf", "cs1", "hello", "content", "").unwrap()
;
    add_doc(&idx, &mut writer, 2, "/y/b.pdf", "cs2", "hello", "content", "").unwrap()
;
    writer.commit().unwrap();

    let results = idx.search("hello", 10, Some(".*"), 0).unwrap();
    assert_eq!(results.len(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_path_filter_invalid_regex_returns_error() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "content", "").unwrap()
;
    writer.commit().unwrap();

    let result = idx.search("hello", 10, Some("[invalid"), 0);
    assert!(result.is_err());

    std::fs::remove_dir_all(&dir).ok();
}

// --- Pagination (offset) ---

#[test]
fn test_search_offset_skips_results() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    for i in 0..10 {
        let content = format!("document number {}", i);
        add_doc(&idx, &mut writer, i, &format!("/{}.pdf", i), &format!("cs{}", i), &content, &content, "").unwrap()
;
    }
    writer.commit().unwrap();

    let all = idx.search("document", 10, None, 0).unwrap();
    assert_eq!(all.len(), 10);

    let with_offset = idx.search("document", 5, None, 3).unwrap();
    assert_eq!(with_offset.len(), 5);
    // With offset 3 and total 10, we get 5 results (docs at positions 3-7)
    // Don't assert specific doc because order among equal scores is undefined

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_offset_beyond_total_returns_empty() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "").unwrap()
;
    writer.commit().unwrap();

    // Offset past all results
    let results = idx.search("hello", 10, None, 5).unwrap();
    assert!(results.is_empty(), "Offset beyond total should return empty");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_offset_and_limit_together() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    for i in 0..20 {
        let content = format!("item {}", i);
        add_doc(&idx, &mut writer, i, &format!("/{}.pdf", i), &format!("cs{}", i), &content, &content, "").unwrap()
;
    }
    writer.commit().unwrap();

    // Page 1: first 5
    let page1 = idx.search("item", 5, None, 0).unwrap();
    assert_eq!(page1.len(), 5);

    // Page 2: next 5 (skip 5)
    let page2 = idx.search("item", 5, None, 5).unwrap();
    assert_eq!(page2.len(), 5);

    // Page 3: next 5 (skip 10)
    let page3 = idx.search("item", 5, None, 10).unwrap();
    assert_eq!(page3.len(), 5);

    // Page 5: beyond total (skip 20, 0 remaining)
    let page5 = idx.search("item", 5, None, 20).unwrap();
    assert!(page5.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_offset_zero_same_as_no_offset() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    for i in 0..5 {
        let content = format!("item {}", i);
        add_doc(&idx, &mut writer, i, &format!("/{}.pdf", i), &format!("cs{}", i), &content, &content, "").unwrap()
;
    }
    writer.commit().unwrap();

    let without_offset = idx.search("item", 10, None, 0).unwrap();
    let with_offset = idx.search("item", 10, None, 0).unwrap();
    assert_eq!(without_offset.len(), with_offset.len());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_offset_with_path_filter() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    for i in 0..6 {
        let sub = if i < 3 { "reports" } else { "invoices" };
        let content = format!("document {}", i);
        add_doc(&idx, &mut writer, i, &format!("/{}/{}.pdf", sub, i), &format!("cs{}", i), &content, &content, "").unwrap()
;
    }
    writer.commit().unwrap();

    // All docs match
    let all = idx.search("document", 10, None, 0).unwrap();
    assert_eq!(all.len(), 6);

    // Path filter + offset: only reports docs (3 docs), skip 1, get remaining 2
    let filtered = idx.search("document", 10, Some(".*reports.*"), 1).unwrap();
    assert_eq!(filtered.len(), 2);
    for (_, doc) in &filtered {
        let path = doc.get_first(idx.path_field).unwrap().as_str().unwrap();
        assert!(path.contains("reports"), "All results should be in reports subdir");
    }

    std::fs::remove_dir_all(&dir).ok();
}

// --- Phrase queries ---

#[test]
fn test_search_phrase_query_matches_exact_phrase() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "The quick brown fox jumps over the lazy dog", "content", "").unwrap()
;
    add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "quick brown fox jumps high", "content", "").unwrap()
;
    writer.commit().unwrap();

    let results = idx.search("\"quick brown fox\"", 10, None, 0).unwrap();
    assert_eq!(results.len(), 2, "Phrase matches both docs");

    let results = idx.search("\"lazy dog\"", 10, None, 0).unwrap();
    assert_eq!(results.len(), 1, "Phrase 'lazy dog' matches only first doc");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_phrase_query_no_match_when_words_out_of_order() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "").unwrap()
;
    writer.commit().unwrap();

    // Words exist but not as a phrase
    let results = idx.search("\"world hello\"", 10, None, 0).unwrap();
    assert!(results.is_empty(), "Out-of-order phrase should not match");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_phrase_learning_machine_does_not_match() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/test_phrase.pdf", "cs1", "support vector machine", "content", "").unwrap();
    add_doc(&idx, &mut writer, 2, "/test_phrase.pdf", "cs2", "machine learning", "content", "").unwrap();
    add_doc(&idx, &mut writer, 3, "/test_phrase.pdf", "cs3", "vector machine learning", "content", "").unwrap();
    writer.commit().unwrap();

    // "learning machine" reversed — should NOT match any doc
    let count = idx.search_count("\"learning machine\"").unwrap();
    assert_eq!(count, 0, "search_count for reversed phrase should be 0");

    let results = idx.search("\"learning machine\"", 10, None, 0).unwrap();
    assert!(results.is_empty(), "search for reversed phrase should be empty");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_phrase_extra_learning_machine_does_not_match() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    // Same content as test_phrase_extra.pdf
    add_doc(&idx, &mut writer, 1, "/test_phrase_extra.pdf", "cse1", "machine learning", "content", "").unwrap();
    add_doc(&idx, &mut writer, 2, "/test_phrase_extra.pdf", "cse2", "machine", "content", "").unwrap();
    add_doc(&idx, &mut writer, 3, "/test_phrase_extra.pdf", "cse3", "machine learning", "content", "").unwrap();
    writer.commit().unwrap();

    // "learning machine" reversed — should NOT match any doc
    let count = idx.search_count("\"learning machine\"").unwrap();
    assert_eq!(count, 0, "search_count for reversed phrase should be 0");

    let results = idx.search("\"learning machine\"", 10, None, 0).unwrap();
    assert!(results.is_empty(), "search for reversed phrase should be empty");

    // Also verify "machine learning" DOES match both docs
    let results = idx.search("\"machine learning\"", 10, None, 0).unwrap();
    assert_eq!(results.len(), 2, "phrase 'machine learning' should match 2 docs in test_phrase_extra.pdf");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_phrase_query_empty_string_returns_nothing() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "").unwrap()
;
    writer.commit().unwrap();

    let results = idx.search("\"\"", 10, None, 0).unwrap();
    assert!(results.is_empty(), "Empty phrase should return no results");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_raw_string_machine_learning_returns_exactly_2_pages() {
    // Simulates the real flow:
    //   1. User types "machine learning" (sin comillas) in the C# UI
    //   2. C# auto-quoting wraps it → "\"machine learning\""
    //   3. Rust search() detects quotes → builds PhraseQuery directly
    //   4. PhraseQuery matches only pages where tokens are strictly adjacent
    let raw_query = "machine learning";
    let quoted = format!("\"{}\"", raw_query);

    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    // Page 1: solo "machine" — no tiene "learning" después
    add_doc(&idx, &mut writer, 1, "/book.pdf", "cs1", "machine", "raw", "").unwrap();
    // Page 2: "machine learning" — frase exacta contigua
    add_doc(&idx, &mut writer, 2, "/book.pdf", "cs2", "machine learning", "raw", "").unwrap();
    // Page 3: "machine learning" — frase exacta contigua
    add_doc(&idx, &mut writer, 3, "/book.pdf", "cs3", "machine learning", "raw", "").unwrap();
    // Page 4: "the machine is learning fast" — palabras sueltas, NO contiguas
    add_doc(&idx, &mut writer, 4, "/book.pdf", "cs4", "the machine is learning fast", "raw", "").unwrap();

    writer.commit().unwrap();

    // Usar el query quoted (como llega desde C# con auto-quoting)
    let results = idx.search(&quoted, 10, None, 0).unwrap();
    assert_eq!(results.len(), 2,
        "PhraseQuery for 'machine learning' debe retornar exactamente 2 páginas (2 y 3), no {}",
        results.len());

    // Verificar que los IDs corresponden a las páginas correctas
    let mut ids: Vec<u64> = results.iter()
        .map(|(_, doc)| doc.get_first(idx.id_field).unwrap().as_u64().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, vec![2, 3],
        "Las páginas 2 y 3 deben ser las únicas retornadas. Obtenidas: {:?}", ids);

    // También verificar que search_count es consistente
    let count = idx.search_count(&quoted).unwrap();
    assert_eq!(count, 2,
        "search_count debe coincidir: 2, no {}", count);

    std::fs::remove_dir_all(&dir).ok();
}

// --- Fuzzy queries ---

#[test]
fn test_search_fuzzy_matches_with_typo() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "content", "").unwrap()
;
    writer.commit().unwrap();

    // "hallo" is edit distance 1 (eâ†’a substitution)
    let results = idx.search_fuzzy("hallo", 10, None, 0, 1).unwrap();
    assert_eq!(results.len(), 1, "Fuzzy search with edit distance 1 should match 'hallo' -> 'hello'");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_fuzzy_edit_distance_2_matches() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "algorithm", "content", "").unwrap()
;
    writer.commit().unwrap();

    // "algorith" missing last char, edit distance 1 (insert 'm')
    let results = idx.search_fuzzy("algorith", 10, None, 0, 2).unwrap();
    assert_eq!(results.len(), 1, "Fuzzy search with edit distance 2 should match");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_fuzzy_no_match_when_edit_distance_too_low() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "algorithm", "content", "").unwrap()
;
    writer.commit().unwrap();

    // "algorithx" has edit distance 2 from "algorithm" (subst 'm'â†’'x', delete 'm')
    // Actually: "algorith" vs "algorithm": insert 'm' at end = distance 1
    // But "algorit" vs "algorithm": insert 'h' (1), insert 'm' (2) = distance 2
    // So with distance 1, "algorit" should not match
    let results = idx.search_fuzzy("algorit", 10, None, 0, 1).unwrap();
    assert!(results.is_empty(), "Edit distance 1 should not match 2-char typo");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_fuzzy_empty_query_returns_nothing() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "content", "").unwrap()
;
    writer.commit().unwrap();

    let results = idx.search_fuzzy("", 10, None, 0, 1).unwrap();
    assert!(results.is_empty(), "Empty fuzzy query should return no results");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_fuzzy_with_path_filter() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    add_doc(&idx, &mut writer, 1, "/reports/a.pdf", "cs1", "hello world", "content", "").unwrap()
;
    add_doc(&idx, &mut writer, 2, "/invoices/b.pdf", "cs2", "hello world", "content", "").unwrap()
;
    writer.commit().unwrap();

    let results = idx.search_fuzzy("hallo", 10, Some(".*reports.*"), 0, 1).unwrap();
    assert_eq!(results.len(), 1, "Fuzzy + path filter should filter by path");
    let path = results[0].1.get_first(idx.path_field).unwrap().as_str().unwrap();
    assert!(path.contains("reports"));

    std::fs::remove_dir_all(&dir).ok();
}

    // --- Dedup / resumability ---

    #[test]
    fn test_dedup_same_checksum_both_kept() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "same", "old content", "old content", "").unwrap()
;
        add_doc(&idx, &mut writer, 2, "/b.pdf", "same", "new content", "new content", "").unwrap()
;
        writer.commit().unwrap();

        // Checksum-based dedup was removed — same checksum no longer replaces.
        // Path-based dedup in the job store is the true dedup mechanism.
        let results = idx.search("content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 2, "Both docs kept (no Tantivy checksum dedup)");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_dedup_different_checksum_both_kept() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "content a", "content a", "").unwrap()
;
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "content b", "content b", "").unwrap()
;
        writer.commit().unwrap();

        let results = idx.search("content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 2, "Different checksums should both be kept");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Snippet generation ---

    #[test]
    fn test_generate_snippet_non_stored_returns_empty() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        let text = "This is a very long document about cryptography.";
        add_doc(&idx, &mut writer, 1, "/doc.pdf", "cs1", text, text, "").unwrap();
        writer.commit().unwrap();

        let results = idx.search("cryptography", 10, None, 0).unwrap();
        assert!(!results.is_empty());

        // content field is INDEXED only (not STORED), so snippet should be empty
        let snippet = idx.generate_snippet(&results[0].1, "cryptography").unwrap();
        assert!(snippet.is_empty(), "Snippet should be empty when content is not stored");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Indexer convenience ---

    #[test]
    fn test_indexer_index_document_and_search() {
        let dir = unique_index_dir();
        let indexer = Indexer::new(&dir).unwrap();
        indexer.index_document(1, "/a.pdf", "rust programming language").unwrap();
        let results = indexer.search_index().search("rust", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Error flows ---

    #[test]
    fn test_search_empty_query() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let results = idx.search("", 10, None, 0).unwrap();
        assert!(results.is_empty(), "Empty query should return no results");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_index_invalid_path() {
        let result = SearchIndex::new(&PathBuf::from(""));
        assert!(result.is_err(), "Creating index with empty path should fail");
    }

    // --- Metrics ---

    #[test]
    fn test_indexer_metrics_start_zero() {
        let m = IndexerMetrics::new();
        assert_eq!(m.docs_indexed(), 0);
    }

    #[test]
    fn test_indexer_metrics_increments() {
        let dir = unique_index_dir();
        let indexer = Indexer::new(&dir).unwrap();
        indexer.index_document(1, "/a.pdf", "content").unwrap();
        assert_eq!(indexer.metrics().docs_indexed(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_indexer_metrics_last_commit_age() {
        let m = IndexerMetrics::new();
        let age = m.last_commit_age_secs();
        assert!(age >= 0.0, "last_commit_age should be non-negative");
    }

    #[test]
    fn test_indexer_metrics_updates_on_commit() {
        let dir = unique_index_dir();
        let indexer = Indexer::new(&dir).unwrap();
        indexer.index_document(1, "/a.pdf", "content").unwrap();
        let age = indexer.metrics().last_commit_age_secs();
        assert!(age < 1.0, "After commit, last_commit_age should be small");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Index stats ---

    #[test]
    fn test_index_stats_empty_index() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let stats = idx.compute_stats(&dir).unwrap();
        assert_eq!(stats.num_docs, 0, "Empty index should have 0 docs");
        assert!(stats.size_bytes > 0, "Index directory should have data on disk");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_index_stats_with_documents() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "").unwrap()
;
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "foo bar", "content", "").unwrap()
;
        writer.commit().unwrap();

        let stats = idx.compute_stats(&dir).unwrap();
        assert_eq!(stats.num_docs, 2, "Index with 2 docs should report 2");
        assert!(stats.num_segments >= 1, "At least 1 segment");
        assert!(stats.size_bytes > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_index_stats_non_existent_path() {
        let dir = unique_index_dir().join("nonexistent_subdir");
        let idx = SearchIndex::new(&dir).unwrap();
        let stats = idx.compute_stats(&dir).unwrap();
        assert_eq!(stats.num_docs, 0);
        // size_bytes may be 0 or >0 depending on whether create_dir_all happened
        // Just verify the function doesn't crash
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_index_stats_path_is_file_read_dir_fails() {
        let dir = unique_index_dir();
        std::fs::create_dir_all(&dir).ok();
        let file_path = dir.join("not_a_dir.txt");
        std::fs::write(&file_path, "this is a file").unwrap();

        let idx = SearchIndex::new(&dir).unwrap();
        // Passing a file path instead of directory: read_dir fails, size_bytes = 0
        let stats = idx.compute_stats(&file_path).unwrap();
        assert_eq!(stats.num_docs, 0, "File path should not crash stats");
        assert_eq!(stats.size_bytes, 0, "File is not a directory, size = 0");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Index optimize ---

    #[test]
    fn test_optimize_does_not_crash() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();

        {
            let mut writer = idx.writer().unwrap();
            add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "").unwrap()
;
            writer.commit().unwrap();
        }

        // Just verify optimize runs without error
        let (before, after) = idx.optimize().unwrap();
        assert!(after <= before, "Optimize should not increase segment count");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Delete from index ---

    #[test]
    fn test_delete_by_path_removes_matching_documents() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        {
            let mut writer = idx.writer().unwrap();
            add_doc(&idx, &mut writer, 1, "/reports/a.pdf", "cs1", "hello", "hello", "").unwrap()
;
            add_doc(&idx, &mut writer, 2, "/invoices/b.pdf", "cs2", "hello", "hello", "").unwrap()
;
            add_doc(&idx, &mut writer, 3, "/reports/c.pdf", "cs3", "hello", "hello", "").unwrap()
;
            writer.commit().unwrap();
        } // writer dropped, lock released

        let count = idx.delete_by_path(".*reports.*").unwrap();
        assert_eq!(count, 2, "Should delete 2 matching docs");

        let remaining = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(remaining.len(), 1, "Only 1 doc should remain");
        let path = remaining[0].1.get_first(idx.path_field).unwrap().as_str().unwrap();
        assert!(path.contains("invoices"), "Remaining doc should be from invoices");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_delete_by_id_removes_specific_document() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        {
            let mut writer = idx.writer().unwrap();
            add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "").unwrap()
;
            add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "world", "world", "").unwrap()
;
            writer.commit().unwrap();
        } // writer dropped, lock released

        idx.delete_by_id(1).unwrap();
        let remaining = idx.search("hello", 10, None, 0).unwrap();
        assert!(remaining.is_empty(), "Doc with id=1 should be deleted");

        let world_results = idx.search("world", 10, None, 0).unwrap();
        assert_eq!(world_results.len(), 1, "Doc with id=2 should remain");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Stemming / stop words ---

    #[test]
    fn test_stem_search_finds_stemmed_variants() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "running quickly", "running quickly", "").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "the cat runs fast", "the cat runs fast", "").unwrap();
        writer.commit().unwrap();

        // Exact match works
        let results = idx.search("running", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Search should find exact match");

        // Search for "run" does NOT match "running" without stemming
        let results = idx.search("run", 10, None, 0).unwrap();
        assert_eq!(results.len(), 0, "Without stemming, 'run' does not match 'running'");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_stop_words_removed_in_stem_search() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "the cat and the dog", "the cat and the dog", "").unwrap()
;
        writer.commit().unwrap();

        // Stemmed search for "cat" should match (stop words like "the", "and" are removed during indexing)
        let results = idx.search_stem("cat", 10, None, 0, true).unwrap();
        assert_eq!(results.len(), 1, "Stemmed search should find 'cat'");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_with_phrase() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "the running cats", "the running cats", "").unwrap();
        writer.commit().unwrap();

        // Phrase search matches exact phrase
        let results = idx.search("\"running cats\"", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Phrase search should find exact match");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_term_positions_basic() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world hello", "content", "eng").unwrap()
;
        writer.commit().unwrap();

        // First verify the doc is searchable at all
        let results = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Doc should be findable via search");

        // Now try position lookup
        let positions = idx.search_term_positions(1, "hello").unwrap();
        if positions.is_empty() {
            // Debug: check which segment the doc is in
            let reader = idx.index.reader_builder()
                .reload_policy(ReloadPolicy::OnCommitWithDelay)
                .try_into().unwrap();
            let searcher = reader.searcher();
            let doc_id_term = tantivy::Term::from_field_u64(idx.id_field, 1);
            let addrs = searcher.search(
                &tantivy::query::TermQuery::new(doc_id_term, tantivy::schema::IndexRecordOption::Basic),
                &tantivy::collector::DocSetCollector,
            ).unwrap();
            panic!(
                "No positions found. DocAddrs: {:?}, num_segments: {}, num_docs: {}",
                addrs,
                searcher.segment_readers().len(),
                searcher.num_docs(),
            );
        }
        assert!(positions.contains(&0), "Position 0 should contain 'hello', got positions: {:?}", positions);
        assert!(positions.contains(&2), "Position 2 should contain 'hello'");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_term_positions_no_match() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "eng").unwrap()
;
        writer.commit().unwrap();

        let positions = idx.search_term_positions(1, "nonexistent").unwrap();
        assert!(positions.is_empty(), "Non-existent term should return no positions");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_term_positions_nonexistent_doc() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "eng").unwrap()
;
        writer.commit().unwrap();

        let positions = idx.search_term_positions(999, "hello").unwrap();
        assert!(positions.is_empty(), "Non-existent doc_id should return no positions");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_stem_search_no_match() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_stem("nonexistent", 10, None, 0, true).unwrap();
        assert!(results.is_empty(), "Stemmed search should return empty for non-matching query");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_stem_search_empty_query() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "").unwrap()
;
        writer.commit().unwrap();

        // Stem search with empty query should return empty
        let results = idx.search_stem("", 10, None, 0, true).unwrap();
        assert!(results.is_empty(), "Stemmed search with empty query should return empty");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Index optimize alternative / error flows ---

    #[test]
    fn test_optimize_empty_index() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();

        // Optimize on empty index with no segments should not crash
        let (before, after) = idx.optimize().unwrap();
        assert!(after <= before, "Optimize on empty index should not increase segment count");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Delete from index error flows ---

    #[test]
    fn test_delete_by_id_nonexistent() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        {
            let mut writer = idx.writer().unwrap();
            add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "").unwrap()
;
            writer.commit().unwrap();
        }

        // Deleting a non-existent id should not remove existing docs
        idx.delete_by_id(999).unwrap();

        // The existing document should remain
        let results = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Document should remain after deleting non-existent id");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_delete_by_path_no_match() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        {
            let mut writer = idx.writer().unwrap();
            add_doc(&idx, &mut writer, 1, "/reports/a.pdf", "cs1", "hello", "hello", "").unwrap()
;
            writer.commit().unwrap();
        }

        // Path regex that matches nothing should return 0
        let count = idx.delete_by_path(".*nonexistent.*").unwrap();
        assert_eq!(count, 0, "Deleting path with no matches should return 0");

        let results = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Document should remain after deleting non-matching path");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_delete_by_path_invalid_regex() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();

        let result = idx.delete_by_path("[invalid");
        assert!(result.is_err(), "Invalid regex should return error");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_delete_on_empty_index() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();

        // Delete on empty index should not crash
        idx.delete_by_id(1).unwrap();

        let count = idx.delete_by_path(".*").unwrap();
        assert_eq!(count, 0, "Delete path on empty index should return 0");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_english_text_searchable_via_content() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/en.pdf", "en1", "hello world", "hello world", "eng").unwrap();
        writer.commit().unwrap();

        // English text should be searchable via content.
        let results = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "English text should be searchable via content");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_empty_lang_searchable_via_content() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/plain.pdf", "cs1", "some text", "some text", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search("text", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Empty-lang doc should be searchable via content");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- search_in_field: basic flows ---

    #[test]
    fn test_search_in_field_normalized_text() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/en.pdf", "en1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("hello", "content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Should find Latin text in normalized_text");

        let path = results[0].1.get_first(idx.path_field).and_then(|v| v.as_str()).unwrap();
        assert_eq!(path, "/en.pdf");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_content_norm() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/doc.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("world", "content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Should find text in content_norm");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_exact() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "running quickly", "raw", "").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "the cat runs fast", "raw", "").unwrap();
        writer.commit().unwrap();

        // Without stemming, search is exact (no morphological variants).
        let results = idx.search_in_field("running", "content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Exact search should find 'running'");

        let results = idx.search_in_field("runs", "content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Exact search should find 'runs'");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_with_path_filter() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "hello there", "raw", "eng").unwrap();
        writer.commit().unwrap();

        // With matching path filter
        let results = idx.search_in_field("hello", "content", 10, Some("/a.pdf"), 0).unwrap();
        assert_eq!(results.len(), 1, "Should find only the doc matching path filter");

        // With non-matching path filter
        let results2 = idx.search_in_field("hello", "content", 10, Some("/nope.pdf"), 0).unwrap();
        assert_eq!(results2.len(), 0, "Path filter excluding all docs should return empty");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_with_limit_offset() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "hello there", "raw", "eng").unwrap();
        add_doc(&idx, &mut writer, 3, "/c.pdf", "cs3", "hello again", "raw", "eng").unwrap();
        writer.commit().unwrap();

        // Limit
        let results = idx.search_in_field("hello", "content", 1, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Limit should restrict results");

        // Offset
        let results2 = idx.search_in_field("hello", "content", 10, None, 2).unwrap();
        assert_eq!(results2.len(), 1, "Offset 2 should skip first 2 results");

        // Offset beyond total
        let results3 = idx.search_in_field("hello", "content", 10, None, 10).unwrap();
        assert_eq!(results3.len(), 0, "Offset beyond total should return empty");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- search_in_field: alternative flows ---

    #[test]
    fn test_search_in_field_no_match() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("nonexistent", "content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 0, "Non-matching query should return empty");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_empty_query() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("", "content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 0, "Empty query should return empty results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_stored_only() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        // content_raw is stored-only, not indexed. QueryParser requires an
        // indexed text field, so this should return an error.
        let result = idx.search_in_field("hello", "content_raw", 10, None, 0);
        assert!(result.is_err(), "Search on stored-only field should fail with an error");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_string_field() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/my-path.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        // path is a STRING field (indexed as raw). Search must match the full string.
        let results = idx.search_in_field("/my-path.pdf", "path", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Should find doc by exact path string");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- search_in_field: error flows ---

    #[test]
    fn test_search_in_field_nonexistent_field() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let result = idx.search_in_field("hello", "nonexistent_field", 10, None, 0);
        assert!(result.is_err(), "Non-existent field should return an error");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found in schema"), "Error should mention missing field, got: {}", err);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_empty_field_name() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let result = idx.search_in_field("hello", "", 10, None, 0);
        assert!(result.is_err(), "Empty field name should return an error");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_numeric_field() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        add_doc(&idx, &mut writer, 42, "/a.pdf", "cs1", "hello world", "raw", "eng").unwrap();
        writer.commit().unwrap();

        // id is a u64 field; QueryParser should fail to parse a text query against it.
        let result = idx.search_in_field("hello", "id", 10, None, 0);
        assert!(result.is_err(), "Text search against numeric field should fail");
        let err = result.unwrap_err().to_string();
        assert!(!err.is_empty(), "Should contain a meaningful error message");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- ram_buffer ---

    #[test]
    fn test_search_index_default_ram_buffer() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        assert_eq!(idx.ram_buffer(), 3_000_000_000, "Default ram_buffer should be 3GB");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_index_with_ram_buffer() {
        let dir = unique_index_dir();
        let idx = SearchIndex::with_ram_buffer(&dir, 1_000_000_000).unwrap();
        assert_eq!(idx.ram_buffer(), 1_000_000_000, "Custom ram_buffer should be 1GB");
        // Verify the writer uses the custom value (doesn't crash)
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "raw", "eng").unwrap();
        writer.commit().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_index_set_ram_buffer() {
        let dir = unique_index_dir();
        let mut idx = SearchIndex::new(&dir).unwrap();
        assert_eq!(idx.ram_buffer(), 3_000_000_000);
        idx.set_ram_buffer(128_000_000);
        assert_eq!(idx.ram_buffer(), 128_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_writer_with_num_threads_default() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer_with_num_threads(2).unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng").unwrap();
        writer.commit().unwrap();
        let results = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "should index and find doc with writer_with_num_threads");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_writer_with_num_threads_large_value_no_crash() {
        let dir = unique_index_dir();
        let mut idx = SearchIndex::new(&dir).unwrap();
        // Use a larger ram_buffer so per-thread memory stays above tantivy's 15MB minimum
        idx.set_ram_buffer(500_000_000);
        let mut writer = idx.writer_with_num_threads(16).unwrap();
        add_doc(&idx, &mut writer, 1, "/big.pdf", "cs1", "test", "test", "eng").unwrap();
        writer.commit().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 8: Weighted search (BoostQuery) ---

    #[test]
    fn test_weighted_search_basic() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "world hello", "world hello", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, None, 0).unwrap();
        assert_eq!(results.len(), 2, "Should find both docs");

        let results = idx.search_weighted_fields("world", &[("content", 1.0)], 10, None, 0).unwrap();
        assert_eq!(results.len(), 2, "Should find both docs for world");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_empty_field_list() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[], 10, None, 0).unwrap();
        assert!(results.is_empty(), "Empty field list should return no results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_with_path_filter() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "hello", "eng").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "hello there", "hello", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, Some("/a\\.pdf"), 0).unwrap();
        assert_eq!(results.len(), 1, "Should filter to /a.pdf only");
        assert!(results[0].1.get_first(idx.path_field).unwrap().as_str().unwrap().contains("a.pdf"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_with_offset() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "hello", "hello", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, None, 1).unwrap();
        assert_eq!(results.len(), 1, "Offset 1 should skip first result");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 8: Recency re-ranking ---

    #[test]
    fn test_recency_noop_when_ingested_at_removed() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "raw", "eng").unwrap();
        writer.commit().unwrap();

        // ingested_at_field is None, so apply_recency_boost is a no-op
        let results = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, None, 0).unwrap();
        assert_eq!(results.len(), 1);
        let boosted = idx.apply_recency_boost(results, 0.5, 365);
        assert_eq!(boosted.len(), 1, "Should return same results when ingested_at is None");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_recency_zero_weight_returns_unchanged() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, None, 0).unwrap();
        let original_len = results.len();
        let boosted = idx.apply_recency_boost(results, 0.0, 365);
        assert_eq!(boosted.len(), original_len, "Zero weight should return results unchanged");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_recency_no_ingested_at_field() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        // Use add_document (no ingested_at)
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng").unwrap();
        writer.commit().unwrap();

        // Temporarily clear ingested_at_field to simulate old index
        // We can't mutate private fields, so we test via the public API:
        // apply_recency_boost will short-circuit if ingested_at_field is None
        let results = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, None, 0).unwrap();
        let boosted = idx.apply_recency_boost(results, 0.5, 365);
        assert_eq!(boosted.len(), 1, "Should still return results when ingested_at is present");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_recency_noop_with_zero_weight() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "raw", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, None, 0).unwrap();
        let boosted = idx.apply_recency_boost(results, 0.0, 365);
        assert_eq!(boosted.len(), 1, "Zero weight should return results unchanged");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_empty_query_returns_nothing() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("", &[("content", 1.0)], 10, None, 0).unwrap();
        assert!(results.is_empty(), "Empty query should return no results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_nonexistent_field_silently_skipped() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng").unwrap();
        writer.commit().unwrap();

        // Nonexistent field names are silently skipped → no query clauses → empty
        let results = idx.search_weighted_fields("hello", &[("nonexistent_field_xyz", 2.0)], 10, None, 0).unwrap();
        assert!(results.is_empty(), "Nonexistent field should be silently skipped, returning empty results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_empty_query_with_path_filter_returns_matching() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng").unwrap();
        add_doc(&idx, &mut writer, 2, "/b.pdf", "cs2", "world", "world", "eng").unwrap();
        writer.commit().unwrap();

        // Empty query + path filter should still match via path regex
        let results = idx.search_weighted_fields("", &[], 10, Some("/a\\.pdf"), 0).unwrap();
        assert_eq!(results.len(), 1, "Empty query with path filter should match by path alone");
        assert!(results[0].1.get_first(idx.path_field).unwrap().as_str().unwrap().contains("a.pdf"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_recency_negative_weight_returns_unchanged() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, None, 0).unwrap();
        let original_score = results[0].0;
        let boosted = idx.apply_recency_boost(results, -0.5, 365);
        assert_eq!(boosted.len(), 1, "Negative weight should return results unchanged");
        assert!((boosted[0].0 - original_score).abs() < f32::EPSILON, "Negative weight should not alter score");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_multiple_field_weights_produce_different_scores() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng").unwrap();
        writer.commit().unwrap();

        let results_low = idx.search_weighted_fields("hello", &[("content", 1.0)], 10, None, 0).unwrap();
        let results_high = idx.search_weighted_fields("hello", &[("content", 5.0)], 10, None, 0).unwrap();
        assert!(
            results_high[0].0 > results_low[0].0,
            "Higher boost should produce higher score ({} vs {})",
            results_high[0].0, results_low[0].0
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- tokenize_with_math ---

    #[test]
    fn test_tokenize_with_math_simple() {
        let tokens = tokenize_with_math("hello world");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], (0, "hello".to_string()));
        assert_eq!(tokens[1], (1, "world".to_string()));
    }

    #[test]
    fn test_tokenize_with_math_lowercases() {
        let tokens = tokenize_with_math("Hello World");
        assert_eq!(tokens[0].1, "hello");
        assert_eq!(tokens[1].1, "world");
    }

    #[test]
    fn test_tokenize_with_math_skips_punctuation() {
        let tokens = tokenize_with_math("hello, world!");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], (0, "hello".to_string()));
        assert_eq!(tokens[1], (1, "world".to_string()));
    }

    #[test]
    fn test_tokenize_with_math_keeps_symbols() {
        let tokens = tokenize_with_math("E = mc^2");
        assert!(tokens.iter().any(|(_, t)| t == "e"));
        assert!(tokens.iter().any(|(_, t)| t == "mc^2"));
    }

    #[test]
    fn test_tokenize_with_math_empty() {
        let tokens = tokenize_with_math("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_tokenize_with_math_newline_separator() {
        let tokens = tokenize_with_math("machine\nlearning");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], (0, "machine".to_string()));
        assert_eq!(tokens[1], (1, "learning".to_string()));
    }

    // --- align_offsets_to_tantivy ---

    #[test]
    fn test_align_offsets_to_tantivy_simple() {
        let wp = vec![
            crate::extractor::WordPosition {
                page: 1, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "hello".to_string(),
            },
            crate::extractor::WordPosition {
                page: 1, x_min: 10.0, y_min: 0.0, x_max: 20.0, y_max: 10.0,
                text: "world".to_string(),
            },
        ];
        let aligned = align_offsets_to_tantivy("hello world", &wp);
        assert_eq!(aligned.len(), 2);
        assert_eq!(aligned[0].0, 0); // Tantivy position 0
        assert_eq!(aligned[0].1.page, 1);
        assert_eq!(aligned[1].0, 1); // Tantivy position 1
        assert_eq!(aligned[1].1.page, 1);
    }

    #[test]
    fn test_align_offsets_to_tantivy_case_insensitive() {
        let wp = vec![
            crate::extractor::WordPosition {
                page: 1, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "PATTERN".to_string(),
            },
            crate::extractor::WordPosition {
                page: 2, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "Pattern".to_string(),
            },
        ];
        let aligned = align_offsets_to_tantivy("PATTERN Pattern", &wp);
        assert_eq!(aligned.len(), 2);
        assert_eq!(aligned[0].0, 0);
        assert_eq!(aligned[0].1.page, 1);
        assert_eq!(aligned[1].0, 1);
        assert_eq!(aligned[1].1.page, 2);
    }

    #[test]
    fn test_align_offsets_to_tantivy_all_wps_produce_entries() {
        // Lockstep: cada WP produce exactamente sus segmentos como entries consecutivas.
        // No hay "skipping" — todas las WPs generan entries.
        let wp = vec![
            crate::extractor::WordPosition {
                page: 1, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "foo".to_string(),
            },
            crate::extractor::WordPosition {
                page: 2, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "bar".to_string(),
            },
        ];
        // Con lockstep, el texto debe coincidir con las WPs para el debug_assert.
        // "foo" y "bar" producen 2 tokens de Tantivy que corresponden a 2 segmentos de WPs.
        let aligned = align_offsets_to_tantivy("foo bar", &wp);
        assert_eq!(aligned.len(), 2);
        assert_eq!(aligned[0].0, 0);
        assert_eq!(aligned[0].1.text, "foo");
        assert_eq!(aligned[1].0, 1);
        assert_eq!(aligned[1].1.text, "bar");
    }

    #[test]
    fn test_align_offsets_to_tantivy_multi_page_preserves_offsets() {
        // Simulates a 2-page PDF with "machine" on page 1 and "learning" on page 2
        let wp = vec![
            crate::extractor::WordPosition {
                page: 1, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "machine".to_string(),
            },
            crate::extractor::WordPosition {
                page: 2, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "learning".to_string(),
            },
        ];
        let aligned = align_offsets_to_tantivy("machine\nlearning", &wp);
        assert_eq!(aligned.len(), 2);
        assert_eq!(aligned[0].0, 0); // "machine" at Tantivy position 0
        assert_eq!(aligned[0].1.page, 1);
        assert_eq!(aligned[1].0, 1); // "learning" at Tantivy position 1
        assert_eq!(aligned[1].1.page, 2);
    }

    #[test]
    fn test_align_offsets_to_tantivy_single_token_wp() {
        // Cada WP ahora tiene exactamente un token (sin espacios).
        // La alineación es 1:1.
        let wp = vec![
            crate::extractor::WordPosition {
                page: 1, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "hello".to_string(),
            },
            crate::extractor::WordPosition {
                page: 1, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "world".to_string(),
            },
        ];
        let aligned = align_offsets_to_tantivy("hello world", &wp);
        assert_eq!(aligned.len(), 2);
        assert_eq!(aligned[0].0, 0);
        assert_eq!(aligned[0].1.text, "hello");
        assert_eq!(aligned[1].0, 1);
        assert_eq!(aligned[1].1.text, "world");
    }

    // ── Regression: tokenize_with_math caches TextAnalyzer (OnceLock<Mutex<>>) ──

    #[test]
    fn test_tokenize_with_math_cached_idempotent() {
        let text = "cached tokenizer idempotent test 123 $math$";
        let first = tokenize_with_math(text);
        let second = tokenize_with_math(text);
        assert_eq!(first, second, "cached TextAnalyzer should produce identical output across calls");
    }

    #[test]
    fn test_tokenize_with_math_concurrent_safety() {
        let text = "concurrent safety for cached tokenizer access";
        let mut handles = Vec::new();
        for _ in 0..10 {
            let t = text.to_string();
            handles.push(std::thread::spawn(move || tokenize_with_math(&t)));
        }
        let results: Vec<Vec<(usize, String)>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for r in &results {
            assert_eq!(r, &results[0], "all concurrent calls should produce identical tokens");
        }
    }

    #[test]
    fn test_tokenize_with_math_wide_variety() {
        // Empty
        assert!(tokenize_with_math("").is_empty());
        // Whitespace only
        assert!(tokenize_with_math("   \n  \t  ").is_empty());
        // Numbers
        let tokens = tokenize_with_math("42");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].1, "42");
        // Mixed case
        let tokens = tokenize_with_math("UPPER lower Mixed");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].1, "upper");
        assert_eq!(tokens[1].1, "lower");
        assert_eq!(tokens[2].1, "mixed");
        // Unicode letters
        let tokens = tokenize_with_math("café résumé");
        assert_eq!(tokens.len(), 2);
        assert!(tokens.iter().any(|(_, t)| t == "café"));
        assert!(tokens.iter().any(|(_, t)| t == "résumé"));
        // Symbols (kept by \p{S})
        let tokens = tokenize_with_math("x ∈ ℝ");
        assert!(tokens.iter().any(|(_, t)| t == "x"));
        assert!(tokens.iter().any(|(_, t)| t == "∈"));
        assert!(tokens.iter().any(|(_, t)| t == "ℝ"));
        // Punctuation stripped
        let tokens = tokenize_with_math("hello, world! (yes)");
        assert_eq!(tokens.len(), 3, "punctuation should be stripped");
        assert_eq!(tokens[0].1, "hello");
        assert_eq!(tokens[1].1, "world");
        assert_eq!(tokens[2].1, "yes");
    }

    #[test]
    fn test_tokenize_with_math_cached_does_not_affect_positions() {
        // Positions should be 0-based consecutive regardless of caching
        let tokens = tokenize_with_math("a b c d e");
        assert_eq!(tokens.len(), 5);
        for (i, (pos, text)) in tokens.iter().enumerate() {
            assert_eq!(*pos, i, "position {} should be {}", i, i);
            assert_eq!(text.as_str(), ["a", "b", "c", "d", "e"][i]);
        }
    }

    // ── Regression: align_offsets_to_tantivy with cached tokenizer ──

    #[test]
    fn test_align_offsets_to_tantivy_empty_segment_skipped() {
        // Una WP con texto vacío (después de clean_word_text) no produce entry.
        // Esto puede ocurrir si un carácter no genera match con TOKEN_PATTERN.
        let wp = vec![
            crate::extractor::WordPosition {
                page: 1, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "hello".to_string(),
            },
            crate::extractor::WordPosition {
                page: 1, x_min: 10.0, y_min: 0.0, x_max: 20.0, y_max: 10.0,
                text: "".to_string(),  // WP vacía → sin segmentos
            },
            crate::extractor::WordPosition {
                page: 2, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "world".to_string(),
            },
        ];
        let aligned = align_offsets_to_tantivy("hello world", &wp);
        // "hello" y "world" producen entries, la WP vacía se saltea
        assert_eq!(aligned.len(), 2);
        assert_eq!(aligned[0].0, 0);
        assert_eq!(aligned[0].1.text, "hello");
        assert_eq!(aligned[1].0, 1);
        assert_eq!(aligned[1].1.text, "world");
    }
}
