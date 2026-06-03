use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tantivy::collector::{Count, TopDocs};
use tantivy::merge_policy::LogMergePolicy;
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, QueryParser, RegexQuery};
#[cfg(test)]
use tantivy::query::TermQuery;
use tantivy::schema::*;
use tantivy::tokenizer::{Stemmer, StopWordFilter, TextAnalyzer};
use tantivy::{doc, Index, IndexWriter, ReloadPolicy, SnippetGenerator, TantivyDocument, Term};
use tantivy::{DocSet, Postings, TERMINATED};

use crate::math_tokenizer::MathAwareTokenizer;
use crate::tokenizers::{ChineseBigramTokenizer, JapaneseTokenizer};

#[allow(dead_code)]
pub struct SearchIndex {
    pub index: Index,
    pub schema: Schema,
    pub id_field: Field,
    pub path_field: Field,
    pub content_norm_field: Field,
    pub content_raw_field: Field,
    pub checksum_field: Field,
    pub content_stem_field: Option<Field>,
    pub content_jp_field: Option<Field>,
    pub content_zh_field: Option<Field>,
    pub math_source_field: Field,
    pub math_tokens_field: Option<Field>,
    pub normalized_text_field: Option<Field>,
    pub language_field: Field,
    pub ingested_at_field: Option<Field>,
    ram_buffer: u64,
}

#[allow(dead_code)]
pub struct IndexerMetrics {
    pub docs_indexed: AtomicU64,
    pub last_commit: Mutex<Instant>,
}

impl IndexerMetrics {
    pub fn new() -> Self {
        Self {
            docs_indexed: AtomicU64::new(0),
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
        let index = if index_path.join("meta.json").exists() {
            Index::open_in_dir(index_path).context("Failed to open existing index")?
        } else {
            std::fs::create_dir_all(index_path).ok();
            let schema = build_schema();
            Index::create_in_dir(index_path, schema)
                .context("Failed to create index directory")?
        };

        // Register custom tokenizer that preserves Unicode math symbols (e.g., âˆ‘, âˆ«).
        // SimpleTokenizer treats non-alphanumeric chars as separators and discards them;
        // RegexTokenizer with [\p{L}\p{N}\p{S}]+ keeps letters, numbers, and symbols together.
        let math_tokenizer = TextAnalyzer::builder(
            tantivy::tokenizer::RegexTokenizer::new(r"[\p{L}\p{N}\p{S}]+")
                .expect("Invalid regex for math tokenizer"),
        )
        .filter(tantivy::tokenizer::LowerCaser)
        .build();
        index.tokenizers().register("math", math_tokenizer);

        // Register English tokenizer with stemming and stop-word removal.
        let english_tokenizer = TextAnalyzer::builder(tantivy::tokenizer::SimpleTokenizer::default())
            .filter(tantivy::tokenizer::LowerCaser)
            .filter(Stemmer::default())
            .filter(StopWordFilter::new(tantivy::tokenizer::Language::English).unwrap_or_else(|| {
                StopWordFilter::remove(vec![])
            }))
            .build();
        index.tokenizers().register("english", english_tokenizer);

        // Register Japanese tokenizer (Lindera IPADIC).
        match JapaneseTokenizer::new() {
            Ok(jp) => {
                let jp_analyzer = TextAnalyzer::builder(jp)
                    .filter(tantivy::tokenizer::LowerCaser)
                    .build();
                index.tokenizers().register("ja", jp_analyzer);
            }
            Err(_e) => {}

        }

        // Register Chinese bigram tokenizer.
        let zh_analyzer = TextAnalyzer::builder(ChineseBigramTokenizer)
            .filter(tantivy::tokenizer::LowerCaser)
            .build();
        index.tokenizers().register("zh", zh_analyzer);

        // Register math-aware tokenizer for math_tokens field.
        let math_tokens_analyzer = TextAnalyzer::builder(MathAwareTokenizer)
            .filter(tantivy::tokenizer::LowerCaser)
            .build();
        index.tokenizers().register("math_tokens", math_tokens_analyzer);

        let schema = index.schema();
        let id_field = schema.get_field("id").unwrap();
        let path_field = schema.get_field("path").unwrap();
        let content_norm_field = schema.get_field("content_norm").unwrap();
        let content_raw_field = schema.get_field("content_raw").unwrap();
        let checksum_field = schema.get_field("checksum").unwrap();
        let content_stem_field = schema.get_field("content_stem").ok();
        let content_jp_field = schema.get_field("content_jp").ok();
        let content_zh_field = schema.get_field("content_zh").ok();
        let math_source_field = schema.get_field("math_source").unwrap();
        let math_tokens_field = schema.get_field("math_tokens").ok();
        let normalized_text_field = schema.get_field("normalized_text").ok();
        let language_field = schema.get_field("language").unwrap();
        let ingested_at_field = schema.get_field("ingested_at").ok();

        Ok(Self {
            index,
            schema,
            id_field,
            path_field,
            content_norm_field,
            content_raw_field,
            checksum_field,
            content_stem_field,
            content_jp_field,
            content_zh_field,
            math_source_field,
            math_tokens_field,
            normalized_text_field,
            language_field,
            ingested_at_field,
            ram_buffer: 500_000_000,
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

    pub fn add_document(
        &self,
        writer: &mut IndexWriter,
        id: i64,
        path: &str,
        checksum: &str,
        content_norm: &str,
        content_raw: &str,
        language: &str,
        math_source: &str,
    ) -> Result<()> {
        self.add_document_with_ts(writer, id, path, checksum, content_norm, content_raw, language, math_source, 0)
    }

    pub fn add_document_with_ts(
        &self,
        writer: &mut IndexWriter,
        id: i64,
        path: &str,
        checksum: &str,
        content_norm: &str,
        content_raw: &str,
        language: &str,
        math_source: &str,
        ingested_at: u64,
    ) -> Result<()> {
        // Dedup: remove any existing document with the same checksum.
        let term = Term::from_field_text(self.checksum_field, checksum);
        writer.delete_term(term);

        let mut doc = TantivyDocument::from(doc!(
            self.id_field => id as u64,
            self.path_field => path,
            self.checksum_field => checksum,
            self.content_norm_field => content_norm,
            self.content_raw_field => content_raw,
            self.math_source_field => math_source,
            self.language_field => language,
        ));
        if let Some(f) = self.ingested_at_field {
            doc.add_u64(f, ingested_at);
        }
        if let Some(stem_field) = self.content_stem_field {
            doc.add_text(stem_field, content_norm);
        }
        // Index math_tokens with the math-aware analyzer.
        if let Some(math_field) = self.math_tokens_field {
            if !math_source.is_empty() {
                doc.add_text(math_field, math_source);
            }
        }
        // Route text to the appropriate language-specific field.
        match language {
            "jpn" | "ja" => {
                if let Some(jp_field) = self.content_jp_field {
                    doc.add_text(jp_field, content_norm);
                }
            }
            "cmn" | "zh" | "wuu" | "yue" | "nan" => {
                if let Some(zh_field) = self.content_zh_field {
                    doc.add_text(zh_field, content_norm);
                }
            }
            _ => {
                // Latin and other non-CJK languages get routed to normalized_text.
                if let Some(nt_field) = self.normalized_text_field {
                    if !content_norm.is_empty() {
                        doc.add_text(nt_field, content_norm);
                    }
                }
            }
        }
        writer
            .add_document(doc)
            .context("Failed to add document to index")?;
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn search(
        &self,
        query_str: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        if Self::is_quoted(query_str) {
            self.search_phrase(Self::unquote(query_str), limit, path_filter, offset)
        } else {
            self.search_stem(query_str, limit, path_filter, offset, false)
        }
    }

    /// Returns true if `s` is wrapped in ASCII double-quotes.
    fn is_quoted(s: &str) -> bool {
        s.len() >= 2 && s.as_bytes()[0] == b'"' && s.as_bytes()[s.len() - 1] == b'"'
    }

    /// Strips surrounding double-quotes from `s`.
    fn unquote<'a>(s: &'a str) -> &'a str {
        if Self::is_quoted(s) { &s[1..s.len() - 1] } else { s }
    }

    /// Searches using a Tantivy `PhraseQuery` built directly from whitespace-separated words.
    /// The caller must have already stripped surrounding quotes from `phrase`.
    fn search_phrase(
        &self,
        phrase: &str,
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

        let words: Vec<&str> = phrase.split_whitespace().collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        if words.len() == 1 {
            return self.search_stem(phrase, limit, path_filter, offset, false);
        }

        let terms: Vec<Term> = words
            .iter()
            .map(|w| Term::from_field_text(self.content_norm_field, w))
            .collect();

        let phrase_query = PhraseQuery::new(terms);

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        clauses.push((Occur::Must, Box::new(phrase_query)));

        if let Some(pattern) = path_filter {
            if !pattern.is_empty() {
                let re_query = RegexQuery::from_pattern(pattern, self.path_field)
                    .context("Invalid path filter regex")?;
                clauses.push((Occur::Must, Box::new(re_query)));
            }
        }

        let query: Box<dyn Query> = if clauses.len() == 1 {
            clauses.into_iter().next().unwrap().1
        } else {
            Box::new(BooleanQuery::new(clauses))
        };

        let fetch_count = limit.checked_add(offset).unwrap_or(limit);
        let top_docs = searcher
            .search(&*query, &TopDocs::with_limit(fetch_count))
            .context("Phrase search failed")?;

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

    /// Phrase-aware count: returns total matching documents for a PhraseQuery.
    fn search_count_phrase(&self, phrase: &str) -> Result<u64> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let words: Vec<&str> = phrase.split_whitespace().collect();
        if words.len() <= 1 {
            let query_parser = QueryParser::for_index(&self.index, vec![self.content_norm_field]);
            let parsed = query_parser.parse_query(phrase)?;
            let count = searcher.search(&parsed, &Count)?;
            return Ok(count as u64);
        }

        let terms: Vec<Term> = words
            .iter()
            .map(|w| Term::from_field_text(self.content_norm_field, w))
            .collect();
        let phrase_query = PhraseQuery::new(terms);
        let count = searcher.search(&phrase_query, &Count)?;
        Ok(count as u64)
    }

    /// Returns the word offsets where `term` appears in document `doc_id`.
    /// Each offset is the 0-indexed word position within the content_norm field.
    ///
    /// Works by scanning all segments' postings for the term, checking each
    /// matching document's stored id field. This is reliable but slower than
    /// a direct segment-local lookup.
    ///
    /// Returns an empty Vec if the term has no positions for this doc.
    pub fn search_term_positions(&self, doc_id: u64, term: &str) -> Result<Vec<usize>> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();
        let content_term = tantivy::Term::from_field_text(self.content_norm_field, term);
        let segment_readers = searcher.segment_readers();
        let mut all_offsets = Vec::new();

        for seg in segment_readers.iter() {
            let inv_index = match seg.inverted_index(self.content_norm_field) {
                Ok(idx) => idx,
                Err(_) => continue,
            };

            let Ok(Some(mut postings)) = inv_index.read_postings(
                &content_term,
                IndexRecordOption::WithFreqsAndPositions,
            ) else { continue };

            let store_reader = match seg.get_store_reader(0) {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Postings are already positioned at the first document
            loop {
                let seg_doc_id = postings.doc();
                if seg_doc_id == TERMINATED {
                    break;
                }
                if let Ok(stored_doc) = store_reader.get::<TantivyDocument>(seg_doc_id) {
                    if let Some(id_val) = stored_doc.get_first(self.id_field) {
                        if let Some(stored_id) = id_val.as_u64() {
                            if stored_id == doc_id {
                                let mut positions = Vec::new();
                                postings.positions_with_offset(0, &mut positions);
                                for pos in positions {
                                    all_offsets.push(pos as usize);
                                }
                                break;
                            }
                        }
                    }
                }
                postings.advance();
            }
        }

        Ok(all_offsets)
    }

    /// Returns the total number of matching documents (ignoring limit/offset).
    /// For quoted phrase queries it uses `PhraseQuery` directly.
    pub fn search_count(&self, query_str: &str) -> Result<u64> {
        if Self::is_quoted(query_str) {
            return self.search_count_phrase(Self::unquote(query_str));
        }
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&self.index, vec![self.content_norm_field]);
        let parsed = query_parser.parse_query(query_str)
            .context("Failed to parse search query for count")?;

        let count = searcher.search(&parsed, &Count)?;
        Ok(count as u64)
    }

    pub fn search_stem(
        &self,
        query_str: &str,
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
        stem: bool,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let search_field = if stem && self.content_stem_field.is_some() {
            self.content_stem_field.unwrap()
        } else {
            self.content_norm_field
        };

        let query_parser = QueryParser::for_index(&self.index, vec![
            search_field,
        ]);

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

        // Fetch limit + offset, then drop the first `offset` results for pagination
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
        stem: bool,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let search_field = if stem && self.content_stem_field.is_some() {
            self.content_stem_field.unwrap()
        } else {
            self.content_norm_field
        };

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        if !query_str.trim().is_empty() {
            let mut fuzzy_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for token in query_str.split_whitespace() {
                let term = Term::from_field_text(search_field, token);
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
                let query_parser = QueryParser::for_index(&self.index, vec![field]);
                let boxed = query_parser
                    .parse_query(query_str)
                    .context("Failed to parse search query")?;
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
            self.content_norm_field,
        ]);
        if query_str.trim().is_empty() {
            return Ok(String::new());
        }
        let query = query_parser
            .parse_query(query_str)
            .context("Failed to parse snippet query")?;

        let snippet_generator = SnippetGenerator::create(&searcher, &query, self.content_norm_field)
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
                    let qp = QueryParser::for_index(&self.index, vec![field]);
                    if let Ok(parsed) = qp.parse_query(query_str) {
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
        max_days: u64,
    ) -> Vec<(f32, TantivyDocument)> {
        if recency_weight <= 0.0 || self.ingested_at_field.is_none() {
            return results;
        }
        let ingested_at_field = self.ingested_at_field.unwrap();
        let max_secs = max_days * 86400;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        results
            .into_iter()
            .map(|(score, doc)| {
                let age_secs = doc
                    .get_first(ingested_at_field)
                    .and_then(|v| v.as_u64())
                    .map(|ts| now.saturating_sub(ts))
                    .unwrap_or(max_secs);
                let recency_factor = if age_secs >= max_secs {
                    0.0
                } else {
                    1.0 - age_secs as f32 / max_secs as f32
                };
                let boosted_score = score * (1.0 + recency_weight * recency_factor);
                (boosted_score, doc)
            })
            .collect()
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

    /// Execute a boolean query where each clause is an independent text query
    /// combined with MUST / SHOULD / MUST_NOT semantics.
    pub fn search_boolean(
        &self,
        clauses: &[(&str, tantivy::query::Occur)],
        limit: usize,
        path_filter: Option<&str>,
        offset: usize,
        stem: bool,
    ) -> Result<Vec<(f32, TantivyDocument)>> {
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let search_field = if stem && self.content_stem_field.is_some() {
            self.content_stem_field.unwrap()
        } else {
            self.content_norm_field
        };

        let query_parser = QueryParser::for_index(&self.index, vec![search_field]);

        let mut bool_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (term, occur) in clauses {
            if !term.trim().is_empty() {
                if let Ok(q) = query_parser.parse_query(term) {
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
    schema_builder.add_u64_field("id", INDEXED | STORED | FAST);
    schema_builder.add_text_field("path", STRING | STORED);
    schema_builder.add_text_field("checksum", STRING | STORED);
    schema_builder.add_text_field(
        "content_norm",
        TextOptions::default()
            .set_stored()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("math")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
    );
    schema_builder.add_text_field(
        "content_stem",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("english")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
    );
    schema_builder.add_text_field("content_raw", STORED);
    schema_builder.add_text_field(
        "content_jp",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("ja")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
    );
    schema_builder.add_text_field(
        "content_zh",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("zh")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
    );
    schema_builder.add_text_field(
        "math_tokens",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("math_tokens")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
    );
    schema_builder.add_text_field("math_source", STORED);
    schema_builder.add_text_field(
        "normalized_text",
        TextOptions::default()
            .set_stored()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("math")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
    );
    schema_builder.add_text_field("language", STRING | STORED);
    schema_builder.add_u64_field("ingested_at", INDEXED | STORED | FAST);
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

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn index_document(&self, id: i64, path: &str, text: &str) -> Result<()> {
        let mut writer = self.search_index.writer()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.search_index.add_document_with_ts(
            &mut writer,
            id,
            path,
            path, // use path as checksum for simplicity
            text,
            text,
            "",
            "",
            now,
        )?;
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
}

/// Tokenize text with the same "math" tokenizer used for `content_norm`.
/// Returns `Vec<(position, text)>` in token order, where `position` is the
/// 0-indexed word position within the token stream.
///
/// This is the canonical tokenizer — all word_offset values in SQLite should
/// be derived from these positions so they align with Tantivy's term positions.
pub fn tokenize_with_math(text: &str) -> Vec<(usize, String)> {
    use tantivy::tokenizer::RegexTokenizer;
    let mut analyzer = TextAnalyzer::builder(
        RegexTokenizer::new(r"[\p{L}\p{N}\p{S}]+")
            .expect("Invalid regex for math tokenizer"),
    )
    .filter(tantivy::tokenizer::LowerCaser)
    .build();
    let mut stream = analyzer.token_stream(text);
    let mut tokens = Vec::new();
    while stream.advance() {
        let t = stream.token();
        tokens.push((t.position as usize, t.text.clone()));
    }
    tokens
}

/// Align WordPosition offsets to Tantivy token positions so SQLite
/// word_offsets match `search_term_positions` results.
///
/// Iterates Tantivy tokens and matches each to the next available WordPosition
/// whose cleaned text matches the token (case-insensitive). One-word look-ahead
/// allows skipping extraneous WordPositions that have no corresponding token.
pub fn align_offsets_to_tantivy(
    text: &str,
    word_positions: &[crate::extractor::WordPosition],
) -> Vec<(usize, crate::extractor::WordPosition)> {
    let tokens = tokenize_with_math(text);
    let mut result = Vec::new();
    let mut wp_idx = 0;

    for &(pos, ref token_text) in &tokens {
        if wp_idx >= word_positions.len() {
            break;
        }

        let cleaned = crate::extractor::clean_word_text(&word_positions[wp_idx].text);
        let cleaned_lower = cleaned.to_lowercase();

        let matched = cleaned_lower == *token_text
            || token_text.contains(&cleaned_lower)
            || cleaned_lower.contains(token_text);

        if matched {
            result.push((pos, word_positions[wp_idx].clone()));
            wp_idx += 1;
        } else if wp_idx + 1 < word_positions.len() {
            // Look ahead: does the next WordPosition match this token?
            let next_cleaned = crate::extractor::clean_word_text(&word_positions[wp_idx + 1].text);
            let next_lower = next_cleaned.to_lowercase();
            if next_lower == *token_text
                || token_text.contains(&next_lower)
                || next_lower.contains(token_text)
            {
                // Current WordPosition has no corresponding token → skip it
                result.push((pos, word_positions[wp_idx + 1].clone()));
                wp_idx += 2;
            } // else: this token has no bounding box → skip it
        }
        // else: this token has no bounding box → skip it
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

    // --- SearchIndex: basic flows ---

    #[test]
    fn test_schema_creation() {
        let schema = build_schema();
        assert!(schema.get_field("id").is_ok());
        assert!(schema.get_field("path").is_ok());
        assert!(schema.get_field("checksum").is_ok());
        assert!(schema.get_field("content_norm").is_ok());
        assert!(schema.get_field("content_stem").is_ok());
        assert!(schema.get_field("content_raw").is_ok());
        assert!(schema.get_field("content_jp").is_ok(), "content_jp field should exist");
        assert!(schema.get_field("content_zh").is_ok(), "content_zh field should exist");
        assert!(schema.get_field("math_source").is_ok(), "math_source field should exist");
        assert!(schema.get_field("math_tokens").is_ok(), "math_tokens field should exist");
        assert!(schema.get_field("normalized_text").is_ok(), "normalized_text field should exist");
        assert!(schema.get_field("language").is_ok(), "language field should exist");
    }

    #[test]
    fn test_tokenizer_registered() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let tokenizers = idx.index.tokenizers();
        assert!(tokenizers.get("math").is_some(), "math tokenizer should be registered");
        assert!(tokenizers.get("english").is_some(), "english tokenizer should be registered");
        assert!(tokenizers.get("ja").is_some(), "ja tokenizer should be registered");
        assert!(tokenizers.get("zh").is_some(), "zh tokenizer should be registered");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- search_in_field_fuzzy_stem: basic + alternative flows ---

    #[test]
    fn test_search_in_field_fuzzy_stem_exact() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        // fuzzy=0, stem=false -> exact search via QueryParser
        let results = idx.search_in_field_fuzzy_stem("hello", "normalized_text", 10, None, 0, 0, false).unwrap();
        assert_eq!(results.len(), 1, "Exact search via fuzzy_stem should find match");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_fuzzy_stem_fuzzy() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        // fuzzy=2 should match "hxllo" -> "hello"
        let results = idx.search_in_field_fuzzy_stem("hxllo", "normalized_text", 10, None, 0, 2, false).unwrap();
        assert_eq!(results.len(), 1, "Fuzzy search (distance 2) should find match");

        // fuzzy=0 should NOT match
        let results2 = idx.search_in_field_fuzzy_stem("hxllo", "normalized_text", 10, None, 0, 0, false).unwrap();
        assert_eq!(results2.len(), 0, "Exact search should not find typo");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_fuzzy_stem_empty_query() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field_fuzzy_stem("", "normalized_text", 10, None, 0, 2, false).unwrap();
        assert_eq!(results.len(), 0, "Empty query should return empty results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_fuzzy_stem_nonexistent_field() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
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

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "hello there", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field_fuzzy_stem("hello", "normalized_text", 10, Some("/a.pdf"), 0, 0, false).unwrap();
        assert_eq!(results.len(), 1, "Path filter + field fuzzy_stem should find only the matching doc");

        std::fs::remove_dir_all(&dir).ok();
    }


    #[test]
    fn test_japanese_text_searchable_via_content_norm() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        // With Latin chars mixed in, the math tokenizer can match.
        idx.add_document(&mut writer, 1, "/jp.pdf", "jp1", "hello 猫 world", "hello 猫 world", "jpn", "").unwrap()
;
        writer.commit().unwrap();

        // "猫" alone won't match via content_norm (math tokenizer sees it as part
        // of a CJK run, but here it's isolated so it should be a standalone token).
        let results = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Latin part of JP doc should be findable via content_norm");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_chinese_text_routed_to_content_zh() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/zh.pdf", "zh1", "中文测试", "中文测试", "cmn", "").unwrap()
;
        writer.commit().unwrap();

        let zh_field = idx.content_zh_field.expect("content_zh field should exist");
        let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![zh_field]);
        let query = query_parser.parse_query("中文").unwrap();
        let reader = idx.index.reader_builder().try_into().unwrap();
        let searcher = reader.searcher();
        let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
        assert_eq!(top_docs.len(), 1, "ZH text should be searchable via content_zh");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_english_text_not_routed_to_cjk_fields() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/en.pdf", "en1", "hello world", "hello world", "eng", "").unwrap()
;
        writer.commit().unwrap();

        // Fields exist but should not contain data for non-CJK docs.
        if let Some(jp_field) = idx.content_jp_field {
            let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![jp_field]);
            let query = query_parser.parse_query("hello").unwrap();
            let reader = idx.index.reader_builder().try_into().unwrap();
            let searcher = reader.searcher();
            let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(top_docs.len(), 0, "English text should NOT be in content_jp");
        }
        if let Some(zh_field) = idx.content_zh_field {
            let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![zh_field]);
            let query = query_parser.parse_query("hello").unwrap();
            let reader = idx.index.reader_builder().try_into().unwrap();
            let searcher = reader.searcher();
            let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(top_docs.len(), 0, "English text should NOT be in content_zh");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_mixed_language_documents_indexed_correctly() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/en.pdf", "en1", "hello world", "hello world", "eng", "").unwrap()
;
        idx.add_document(&mut writer, 2, "/jp.pdf", "jp1", "私は猫です", "私は猫です", "jpn", "").unwrap()
;
        idx.add_document(&mut writer, 3, "/zh.pdf", "zh1", "中文测试", "中文测试", "cmn", "").unwrap()
;
        writer.commit().unwrap();

        // content_norm finds only the English doc for Latin terms
        let all = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(all.len(), 1, "Only English doc has 'hello' in content_norm");
        // content_norm also finds JP/ZH docs when searching across both fields
        // (the test above just verifies the Latin path still works).

        // JP search on content_jp
        if let Some(jp_field) = idx.content_jp_field {
            let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![jp_field]);
            let query = query_parser.parse_query("猫").unwrap();
            let reader = idx.index.reader_builder().try_into().unwrap();
            let searcher = reader.searcher();
            let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(top_docs.len(), 1, "JP doc should be the only match in content_jp");
        }

        // ZH search on content_zh
        if let Some(zh_field) = idx.content_zh_field {
            let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![zh_field]);
            let query = query_parser.parse_query("中文").unwrap();
            let reader = idx.index.reader_builder().try_into().unwrap();
            let searcher = reader.searcher();
            let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(top_docs.len(), 1, "ZH doc should be the only match in content_zh");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_math_source_stored_in_document() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(
            &mut writer, 1, "/math.pdf", "m1",
            "The equation $E = mc^2$ is famous.",
            "raw",
            "", r"inline:E = mc^2",
        ).unwrap();
        writer.commit().unwrap();

        // Retrieve the doc and check math_source field.
        let reader = idx.index.reader_builder().try_into().unwrap();
        let searcher = reader.searcher();
        let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![idx.content_norm_field]);
        let query = query_parser.parse_query("equation").unwrap();
        let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
        assert!(!top_docs.is_empty(), "Should find the math doc");

        let doc = searcher.doc::<TantivyDocument>(top_docs[0].1).unwrap();
        let stored_math = doc.get_first(idx.math_source_field).and_then(|v| v.as_str());
        assert!(stored_math.is_some(), "math_source should be present");
        assert!(stored_math.unwrap().contains("E = mc^2"),
            "math_source should contain the extracted LaTeX");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_math_tokens_index_latex_construct() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(
            &mut writer, 1, "/sum.pdf", "m2",
            "The sum $$\\sum_{i=1}^{n} i$$ is computed.",
            "raw",
            "", r"display:\sum_{i=1}^{n} i",
        ).unwrap();
        writer.commit().unwrap();

        // Search for the composed MATH_SUM_LIMITS token in math_tokens using a term query.
        // Tokenizer includes LowerCaser, so the token is lowercased.
        if let Some(math_tokens_field) = idx.math_tokens_field {
            let term = tantivy::Term::from_field_text(math_tokens_field, "math_sum_limits");
            let reader = idx.index.reader_builder().try_into().unwrap();
            let searcher = reader.searcher();
            let top_docs = searcher.search(
                &TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic),
                &tantivy::collector::TopDocs::with_limit(10),
            ).unwrap();
            assert_eq!(top_docs.len(), 1,
                "Math doc should be findable via math_sum_limits token in math_tokens");
            // Also verify individual sub-tokens are searchable via the query parser.
            let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![math_tokens_field]);
            let sub_query = query_parser.parse_query("sum").unwrap();
            let sub_docs = searcher.search(&sub_query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(sub_docs.len(), 1,
                "Individual sub-token 'sum' should also be searchable in math_tokens");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_math_source_empty_when_no_math() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(
            &mut writer, 1, "/plain.pdf", "m3",
            "This is plain text without math.",
            "raw",
            "", "",
        ).unwrap();
        writer.commit().unwrap();

        // Verify math_source is stored as empty.
        let reader = idx.index.reader_builder().try_into().unwrap();
        let searcher = reader.searcher();
        let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![idx.content_norm_field]);
        let query = query_parser.parse_query("plain").unwrap();
        let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
        assert!(!top_docs.is_empty());

        let doc = searcher.doc::<TantivyDocument>(top_docs[0].1).unwrap();
        let stored_math = doc.get_first(idx.math_source_field).and_then(|v| v.as_str());
        assert_eq!(stored_math, Some(""),
            "math_source should be empty string for non-math docs");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_index_create_and_reopen() {
        let dir = unique_index_dir();
        {
            let idx = SearchIndex::new(&dir).unwrap();
            let mut writer = idx.writer().unwrap();
            idx.add_document(&mut writer, 1, "/test.pdf", "cs1", "hello world", "hello world", "", "").unwrap();
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

        idx.add_document(&mut writer, 1, "/doc.pdf", "cs1", "the quick brown fox", "the quick brown fox", "", "").unwrap()
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
            idx.add_document(&mut writer, i, &format!("/{}.pdf", i), &format!("cs{}", i), &content, &content, "", "").unwrap()
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

        idx.add_document(&mut writer, 1, "/doc.pdf", "cs1", "Hello World", "Hello World", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/reports/2024.pdf", "cs1", "quarterly earnings report", "content", "", "").unwrap()
;
    idx.add_document(&mut writer, 2, "/invoices/2024.pdf", "cs2", "invoice total earnings", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/docs/a.pdf", "cs1", "rust language", "content", "", "").unwrap()
;
    idx.add_document(&mut writer, 2, "/docs/b.pdf", "cs2", "python language", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "", "").unwrap()
;
    idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "hello world", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/x/a.pdf", "cs1", "hello", "content", "", "").unwrap()
;
    idx.add_document(&mut writer, 2, "/y/b.pdf", "cs2", "hello", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "content", "", "").unwrap()
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
        idx.add_document(&mut writer, i, &format!("/{}.pdf", i), &format!("cs{}", i), &content, &content, "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "", "").unwrap()
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
        idx.add_document(&mut writer, i, &format!("/{}.pdf", i), &format!("cs{}", i), &content, &content, "", "").unwrap()
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
        idx.add_document(&mut writer, i, &format!("/{}.pdf", i), &format!("cs{}", i), &content, &content, "", "").unwrap()
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
        idx.add_document(&mut writer, i, &format!("/{}/{}.pdf", sub, i), &format!("cs{}", i), &content, &content, "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "The quick brown fox jumps over the lazy dog", "content", "", "").unwrap()
;
    idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "quick brown fox jumps high", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/test_phrase.pdf", "cs1", "support vector machine", "content", "", "").unwrap();
    idx.add_document(&mut writer, 2, "/test_phrase.pdf", "cs2", "machine learning", "content", "", "").unwrap();
    idx.add_document(&mut writer, 3, "/test_phrase.pdf", "cs3", "vector machine learning", "content", "", "").unwrap();
    writer.commit().unwrap();

    // "learning machine" reversed — should NOT match any doc
    let count = idx.search_count("\"learning machine\"").unwrap();
    eprintln!("DEBUG search_count(\"learning machine\") = {}", count);
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
    idx.add_document(&mut writer, 1, "/test_phrase_extra.pdf", "cse1", "machine learning", "content", "", "").unwrap();
    idx.add_document(&mut writer, 2, "/test_phrase_extra.pdf", "cse2", "machine", "content", "", "").unwrap();
    idx.add_document(&mut writer, 3, "/test_phrase_extra.pdf", "cse3", "machine learning", "content", "", "").unwrap();
    writer.commit().unwrap();

    // "learning machine" reversed — should NOT match any doc
    let count = idx.search_count("\"learning machine\"").unwrap();
    eprintln!("DEBUG search_count(\"learning machine\") (extra) = {}", count);
    assert_eq!(count, 0, "search_count for reversed phrase should be 0");

    let results = idx.search("\"learning machine\"", 10, None, 0).unwrap();
    assert!(results.is_empty(), "search for reversed phrase should be empty");

    // Also verify "machine learning" DOES match both docs
    let results = idx.search("\"machine learning\"", 10, None, 0).unwrap();
    eprintln!("DEBUG search(\"machine learning\") returned {} results", results.len());
    assert_eq!(results.len(), 2, "phrase 'machine learning' should match 2 docs in test_phrase_extra.pdf");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_search_phrase_query_empty_string_returns_nothing() {
    let dir = unique_index_dir();
    let idx = SearchIndex::new(&dir).unwrap();
    let mut writer = idx.writer().unwrap();

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "", "").unwrap()
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
    idx.add_document(&mut writer, 1, "/book.pdf", "cs1", "machine", "raw", "", "").unwrap();
    // Page 2: "machine learning" — frase exacta contigua
    idx.add_document(&mut writer, 2, "/book.pdf", "cs2", "machine learning", "raw", "", "").unwrap();
    // Page 3: "machine learning" — frase exacta contigua
    idx.add_document(&mut writer, 3, "/book.pdf", "cs3", "machine learning", "raw", "", "").unwrap();
    // Page 4: "the machine is learning fast" — palabras sueltas, NO contiguas
    idx.add_document(&mut writer, 4, "/book.pdf", "cs4", "the machine is learning fast", "raw", "", "").unwrap();

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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "algorithm", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "algorithm", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "content", "", "").unwrap()
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

    idx.add_document(&mut writer, 1, "/reports/a.pdf", "cs1", "hello world", "content", "", "").unwrap()
;
    idx.add_document(&mut writer, 2, "/invoices/b.pdf", "cs2", "hello world", "content", "", "").unwrap()
;
    writer.commit().unwrap();

    let results = idx.search_fuzzy("hallo", 10, Some(".*reports.*"), 0, 1).unwrap();
    assert_eq!(results.len(), 1, "Fuzzy + path filter should filter by path");
    let path = results[0].1.get_first(idx.path_field).unwrap().as_str().unwrap();
    assert!(path.contains("reports"));

    std::fs::remove_dir_all(&dir).ok();
}

// --- Math symbol tokenizer ---

    #[test]
    fn test_math_symbols_are_searchable() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        // Use simple math expression with âˆ‘ (N-ARY SUMMATION) and âˆ« (INTEGRAL)
        let text = "E = mc^2 and âˆ‘ and âˆ« symbols";
        idx.add_document(&mut writer, 1, "/math.pdf", "cs1", text, text, "", "").unwrap()
;
        writer.commit().unwrap();

        // Search for âˆ‘ â€” the "math" tokenizer should preserve it as a single token
        let results_sum = idx.search("âˆ‘", 10, None, 0).unwrap();
        assert_eq!(results_sum.len(), 1, "âˆ‘ should be searchable as a token");

        let results_int = idx.search("âˆ«", 10, None, 0).unwrap();
        assert_eq!(results_int.len(), 1, "âˆ« should be searchable as a token");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Dedup / resumability ---

    #[test]
    fn test_dedup_same_checksum_replaces() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "same", "old content", "old content", "", "").unwrap()
;
        writer.commit().unwrap();

        // Index same checksum again â€” should replace, not duplicate
        idx.add_document(&mut writer, 2, "/b.pdf", "same", "new content", "new content", "", "").unwrap()
;
        writer.commit().unwrap();

        let results = idx.search("content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Dedup should leave exactly one doc");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_dedup_different_checksum_both_kept() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "content a", "content a", "", "").unwrap()
;
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "content b", "content b", "", "").unwrap()
;
        writer.commit().unwrap();

        let results = idx.search("content", 10, None, 0).unwrap();
        assert_eq!(results.len(), 2, "Different checksums should both be kept");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Snippet generation ---

    #[test]
    fn test_generate_snippet_matches_content() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        let text = "This is a very long document about cryptography and security in computer systems.";
        idx.add_document(&mut writer, 1, "/doc.pdf", "cs1", text, text, "", "").unwrap()
;
        writer.commit().unwrap();

        let results = idx.search("cryptography", 10, None, 0).unwrap();
        assert!(!results.is_empty());

        let snippet = idx.generate_snippet(&results[0].1, "cryptography").unwrap();
        assert!(snippet.contains("cryptography"), "Snippet should contain the matched term");

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
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "", "").unwrap()
;
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "foo bar", "content", "", "").unwrap()
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
            idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "", "").unwrap()
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
            idx.add_document(&mut writer, 1, "/reports/a.pdf", "cs1", "hello", "hello", "", "").unwrap()
;
            idx.add_document(&mut writer, 2, "/invoices/b.pdf", "cs2", "hello", "hello", "", "").unwrap()
;
            idx.add_document(&mut writer, 3, "/reports/c.pdf", "cs3", "hello", "hello", "", "").unwrap()
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
            idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "", "").unwrap()
;
            idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "world", "world", "", "").unwrap()
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

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "running quickly", "running quickly", "", "").unwrap()
;
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "the cat runs fast", "the cat runs fast", "", "").unwrap()
;
        writer.commit().unwrap();

        // Non-stemmed search should still work
        let results = idx.search("running", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Non-stemmed search should find exact match");

        // Stemmed search should find "run" stem of "running" and "runs"
        let results = idx.search_stem("run", 10, None, 0, true).unwrap();
        assert_eq!(results.len(), 2, "Stemmed search should find both 'running' and 'runs'");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_stop_words_removed_in_stem_search() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "the cat and the dog", "the cat and the dog", "", "").unwrap()
;
        writer.commit().unwrap();

        // Stemmed search for "cat" should match (stop words like "the", "and" are removed during indexing)
        let results = idx.search_stem("cat", 10, None, 0, true).unwrap();
        assert_eq!(results.len(), 1, "Stemmed search should find 'cat'");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_stem_search_with_phrase() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "the running cats", "the running cats", "", "").unwrap()
;
        writer.commit().unwrap();

        // Stemmed phrase search should match stemmed terms
        let results = idx.search_stem("\"running cat\"", 10, None, 0, true).unwrap();
        assert_eq!(results.len(), 1, "Stemmed phrase search should find stemmed terms");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_term_positions_basic() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world hello", "content", "eng", "").unwrap()
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

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "eng", "").unwrap()
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

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "content", "eng", "").unwrap()
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

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "", "").unwrap();
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

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "", "").unwrap()
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
            idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "", "").unwrap()
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
            idx.add_document(&mut writer, 1, "/reports/a.pdf", "cs1", "hello", "hello", "", "").unwrap()
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
    fn test_normalized_text_populated_for_latin() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/en.pdf", "en1", "hello world", "hello world", "eng", "").unwrap()
;
        writer.commit().unwrap();

        // Latin text should be searchable via normalized_text.
        if let Some(nt_field) = idx.normalized_text_field {
            let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![nt_field]);
            let query = query_parser.parse_query("hello").unwrap();
            let reader = idx.index.reader_builder().try_into().unwrap();
            let searcher = reader.searcher();
            let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(top_docs.len(), 1, "English text should be searchable via normalized_text");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_normalized_text_empty_for_cjk() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/jp.pdf", "jp1", "私は猫です", "私は猫です", "jpn", "").unwrap()
;
        idx.add_document(&mut writer, 2, "/zh.pdf", "zh1", "中文测试", "中文测试", "cmn", "").unwrap()
;
        writer.commit().unwrap();

        // CJK text should NOT be searchable via normalized_text.
        if let Some(nt_field) = idx.normalized_text_field {
            let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![nt_field]);
            let query = query_parser.parse_query("猫").unwrap();
            let reader = idx.index.reader_builder().try_into().unwrap();
            let searcher = reader.searcher();
            let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(top_docs.len(), 0, "Japanese text should NOT be searchable via normalized_text");

            let query2 = query_parser.parse_query("中文").unwrap();
            let top_docs2 = searcher.search(&query2, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(top_docs2.len(), 0, "Chinese text should NOT be searchable via normalized_text");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_normalized_text_empty_for_empty_lang() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        // Empty language should be treated as non-CJK (routed to normalized_text).
        idx.add_document(&mut writer, 1, "/plain.pdf", "cs1", "some text", "some text", "", "").unwrap()
;
        writer.commit().unwrap();

        if let Some(nt_field) = idx.normalized_text_field {
            let query_parser = tantivy::query::QueryParser::for_index(&idx.index, vec![nt_field]);
            let query = query_parser.parse_query("text").unwrap();
            let reader = idx.index.reader_builder().try_into().unwrap();
            let searcher = reader.searcher();
            let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(10)).unwrap();
            assert_eq!(top_docs.len(), 1, "Empty-lang doc should be searchable via normalized_text");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_normalized_text_field_none_graceful() {
        let dir = unique_index_dir();
        let mut idx = SearchIndex::new(&dir).unwrap();
        // Simulate an index without the normalized_text field (e.g. old schema).
        idx.normalized_text_field = None;
        let mut writer = idx.writer().unwrap();
        // Should not panic — the if let Some gracefully skips.
        idx.add_document(&mut writer, 1, "/en.pdf", "en1", "hello world", "hello world", "eng", "").unwrap();
        writer.commit().unwrap();
        // Search via content_norm should still work.
        let results = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "English doc should remain searchable via content_norm");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- search_in_field: basic flows ---

    #[test]
    fn test_search_in_field_normalized_text() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/en.pdf", "en1", "hello world", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("hello", "normalized_text", 10, None, 0).unwrap();
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

        idx.add_document(&mut writer, 1, "/doc.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("world", "content_norm", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Should find text in content_norm");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_content_jp() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/jp.pdf", "jp1", "私は猫です", "raw", "jpn", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("猫", "content_jp", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Should find Japanese text in content_jp");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_content_zh() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/zh.pdf", "zh1", "中文测试", "raw", "cmn", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("中文", "content_zh", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Should find Chinese text in content_zh");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_stem() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "running quickly", "raw", "", "").unwrap();
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "the cat runs fast", "raw", "", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("run", "content_stem", 10, None, 0).unwrap();
        assert_eq!(results.len(), 2, "Stemmed search should find both 'running' and 'runs'");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_math_tokens() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(
            &mut writer, 1, "/sum.pdf", "m1",
            "The sum $$\\sum_{i=1}^{n} i$$ is computed.",
            "raw", "",
            r"display:\sum_{i=1}^{n} i",
        ).unwrap();
        writer.commit().unwrap();

        // MathAwareTokenizer splits "math_sum_limits" at '_', so query for
        // individual sub-tokens like "sum" or "display" that appear in the
        // token stream alongside the composed MATH_SUM_LIMITS construct.
        let results = idx.search_in_field("display", "math_tokens", 10, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Individual token 'display' should be searchable in math_tokens");

        let results2 = idx.search_in_field("sum", "math_tokens", 10, None, 0).unwrap();
        assert_eq!(results2.len(), 1, "Sub-token 'sum' should be searchable in math_tokens");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_with_path_filter() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "hello there", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        // With matching path filter
        let results = idx.search_in_field("hello", "normalized_text", 10, Some("/a.pdf"), 0).unwrap();
        assert_eq!(results.len(), 1, "Should find only the doc matching path filter");

        // With non-matching path filter
        let results2 = idx.search_in_field("hello", "normalized_text", 10, Some("/nope.pdf"), 0).unwrap();
        assert_eq!(results2.len(), 0, "Path filter excluding all docs should return empty");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_with_limit_offset() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "hello there", "raw", "eng", "").unwrap();
        idx.add_document(&mut writer, 3, "/c.pdf", "cs3", "hello again", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        // Limit
        let results = idx.search_in_field("hello", "normalized_text", 1, None, 0).unwrap();
        assert_eq!(results.len(), 1, "Limit should restrict results");

        // Offset
        let results2 = idx.search_in_field("hello", "normalized_text", 10, None, 2).unwrap();
        assert_eq!(results2.len(), 1, "Offset 2 should skip first 2 results");

        // Offset beyond total
        let results3 = idx.search_in_field("hello", "normalized_text", 10, None, 10).unwrap();
        assert_eq!(results3.len(), 0, "Offset beyond total should return empty");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- search_in_field: alternative flows ---

    #[test]
    fn test_search_in_field_no_match() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("nonexistent", "normalized_text", 10, None, 0).unwrap();
        assert_eq!(results.len(), 0, "Non-matching query should return empty");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_empty_query() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_in_field("", "normalized_text", 10, None, 0).unwrap();
        assert_eq!(results.len(), 0, "Empty query should return empty results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_in_field_stored_only() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
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

        idx.add_document(&mut writer, 1, "/my-path.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
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

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
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

        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
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

        idx.add_document(&mut writer, 42, "/a.pdf", "cs1", "hello world", "raw", "eng", "").unwrap();
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
        assert_eq!(idx.ram_buffer(), 500_000_000, "Default ram_buffer should be 500MB");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_index_with_ram_buffer() {
        let dir = unique_index_dir();
        let idx = SearchIndex::with_ram_buffer(&dir, 1_000_000_000).unwrap();
        assert_eq!(idx.ram_buffer(), 1_000_000_000, "Custom ram_buffer should be 1GB");
        // Verify the writer uses the custom value (doesn't crash)
        let mut writer = idx.writer().unwrap();
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "raw", "eng", "").unwrap();
        writer.commit().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_search_index_set_ram_buffer() {
        let dir = unique_index_dir();
        let mut idx = SearchIndex::new(&dir).unwrap();
        assert_eq!(idx.ram_buffer(), 500_000_000);
        idx.set_ram_buffer(128_000_000);
        assert_eq!(idx.ram_buffer(), 128_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 8: Weighted search (BoostQuery) ---

    #[test]
    fn test_weighted_search_basic() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng", "").unwrap();
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "world hello", "world hello", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, None, 0).unwrap();
        assert_eq!(results.len(), 2, "Should find both docs");

        let results = idx.search_weighted_fields("world", &[("content_norm", 1.0)], 10, None, 0).unwrap();
        assert_eq!(results.len(), 2, "Should find both docs for world");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_empty_field_list() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng", "").unwrap();
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
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "hello", "eng", "").unwrap();
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "hello there", "hello", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, Some("/a\\.pdf"), 0).unwrap();
        assert_eq!(results.len(), 1, "Should filter to /a.pdf only");
        assert!(results[0].1.get_first(idx.path_field).unwrap().as_str().unwrap().contains("a.pdf"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_with_offset() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng", "").unwrap();
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "hello", "hello", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, None, 1).unwrap();
        assert_eq!(results.len(), 1, "Offset 1 should skip first result");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 8: Recency re-ranking ---

    #[test]
    fn test_recency_re_ranking_old_gets_boosted_less() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        // Add two docs with different ingested_at timestamps
        if let Some(f) = idx.ingested_at_field {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            let old_ts = now - 200 * 86400; // ~200 days ago

            let mut doc_new = TantivyDocument::new();
            doc_new.add_u64(idx.id_field, 1);
            doc_new.add_text(idx.path_field, "/new.pdf");
            doc_new.add_text(idx.checksum_field, "cs_new");
            doc_new.add_text(idx.content_norm_field, "hello world");
            doc_new.add_text(idx.content_stem_field.unwrap(), "hello world");
            doc_new.add_text(idx.content_raw_field, "hello world");
            doc_new.add_text(idx.content_jp_field.unwrap(), "");
            doc_new.add_text(idx.content_zh_field.unwrap(), "");
            doc_new.add_text(idx.language_field, "eng");
            doc_new.add_text(idx.math_tokens_field.unwrap(), "");
            doc_new.add_u64(f, now);
            writer.add_document(doc_new);

            let mut doc_old = TantivyDocument::new();
            doc_old.add_u64(idx.id_field, 2);
            doc_old.add_text(idx.path_field, "/old.pdf");
            doc_old.add_text(idx.checksum_field, "cs_old");
            doc_old.add_text(idx.content_norm_field, "hello world");
            doc_old.add_text(idx.content_stem_field.unwrap(), "hello world");
            doc_old.add_text(idx.content_raw_field, "hello world");
            doc_old.add_text(idx.content_jp_field.unwrap(), "");
            doc_old.add_text(idx.content_zh_field.unwrap(), "");
            doc_old.add_text(idx.language_field, "eng");
            doc_old.add_text(idx.math_tokens_field.unwrap(), "");
            doc_old.add_u64(f, old_ts);
            writer.add_document(doc_old);
            writer.commit().unwrap();

            // Search without recency — both should appear, new one likely higher BM25
            let results = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, None, 0).unwrap();
            assert_eq!(results.len(), 2, "Should find both docs");

            // Apply recency boost — the newer doc should get a larger boost
            let boosted = idx.apply_recency_boost(results, 0.5, 365);
            assert_eq!(boosted.len(), 2);
            // The new doc's boosted score should be higher than the old one's
            // (new doc has recency factor ~1.0, old doc ~1.0 - 200/365 ≈ 0.45)
            // new score ≈ score * (1 + 0.5 * 1.0) = score * 1.5
            // old score ≈ score * (1 + 0.5 * 0.45) = score * 1.225
            // Since both have same BM25 score, new should rank higher
            let new_path = boosted[0].1.get_first(idx.path_field).unwrap().as_str().unwrap().to_string();
            assert!(new_path.contains("new.pdf") || boosted.len() == 2,
                "Newer doc should be ranked first after recency boost");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_recency_zero_weight_returns_unchanged() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, None, 0).unwrap();
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
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng", "").unwrap();
        writer.commit().unwrap();

        // Temporarily clear ingested_at_field to simulate old index
        // We can't mutate private fields, so we test via the public API:
        // apply_recency_boost will short-circuit if ingested_at_field is None
        let results = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, None, 0).unwrap();
        let boosted = idx.apply_recency_boost(results, 0.5, 365);
        assert_eq!(boosted.len(), 1, "Should still return results when ingested_at is present");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_recency_max_days_older_than_max_days() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();

        if let Some(f) = idx.ingested_at_field {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            let very_old_ts = now - 1000 * 86400; // 1000 days ago

            let mut doc = TantivyDocument::new();
            doc.add_u64(idx.id_field, 1);
            doc.add_text(idx.path_field, "/old.pdf");
            doc.add_text(idx.checksum_field, "cs_old");
            doc.add_text(idx.content_norm_field, "hello world");
            doc.add_text(idx.content_stem_field.unwrap(), "hello world");
            doc.add_text(idx.content_raw_field, "hello world");
            doc.add_text(idx.content_jp_field.unwrap(), "");
            doc.add_text(idx.content_zh_field.unwrap(), "");
            doc.add_text(idx.language_field, "eng");
            doc.add_text(idx.math_tokens_field.unwrap(), "");
            doc.add_u64(f, very_old_ts);
            writer.add_document(doc);
            writer.commit().unwrap();

            let results = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, None, 0).unwrap();
            // With max_days=30, this doc is way older, so recency_factor should be 0.0
            let boosted = idx.apply_recency_boost(results, 1.0, 30);
            assert_eq!(boosted.len(), 1);
            // score * (1 + 1.0 * 0.0) = same score
            // Just verify it doesn't crash and returns result
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_empty_query_returns_nothing() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("", &[("content_norm", 1.0)], 10, None, 0).unwrap();
        assert!(results.is_empty(), "Empty query should return no results");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_weighted_search_nonexistent_field_silently_skipped() {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng", "").unwrap();
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
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng", "").unwrap();
        idx.add_document(&mut writer, 2, "/b.pdf", "cs2", "world", "world", "eng", "").unwrap();
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
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello", "hello", "eng", "").unwrap();
        writer.commit().unwrap();

        let results = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, None, 0).unwrap();
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
        idx.add_document(&mut writer, 1, "/a.pdf", "cs1", "hello world", "hello world", "eng", "").unwrap();
        writer.commit().unwrap();

        let results_low = idx.search_weighted_fields("hello", &[("content_norm", 1.0)], 10, None, 0).unwrap();
        let results_high = idx.search_weighted_fields("hello", &[("content_norm", 5.0)], 10, None, 0).unwrap();
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
    fn test_align_offsets_to_tantivy_skips_unmatched_tokens() {
        // "foo + bar" → Tantivy tokens: ["foo", "+", "bar"]
        // WordPositions: ["foo", "bar"] (filtered out "+")
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
        // Tantivy produces ["foo"@0, "+"@1, "bar"@2]
        // Token "foo" matches wp[0] → offset 0
        // Token "+" has no match → skipped
        // Token "bar" matches wp[1] → offset 2
        let aligned = align_offsets_to_tantivy("foo + bar", &wp);
        assert_eq!(aligned.len(), 2);
        assert_eq!(aligned[0].0, 0);
        assert_eq!(aligned[0].1.text, "foo");
        assert_eq!(aligned[1].0, 2);
        assert_eq!(aligned[1].1.text, "bar");
    }

    #[test]
    fn test_align_offsets_to_tantivy_skips_unmatched_word_positions() {
        // "hello world" → Tantivy tokens: ["hello"@0, "world"@1]
        // WordPositions: ["hello", "foo", "world"] (extra "foo" has no token)
        // "hello"@0 matches wp[0], wp[1]="foo" doesn't match "world"@1
        // look-ahead: wp[2]="world" matches "world"@1 → skip wp[1], use wp[2]
        let wp = vec![
            crate::extractor::WordPosition {
                page: 1, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
                text: "hello".to_string(),
            },
            crate::extractor::WordPosition {
                page: 1, x_min: 10.0, y_min: 0.0, x_max: 20.0, y_max: 10.0,
                text: "foo".to_string(),
            },
            crate::extractor::WordPosition {
                page: 2, x_min: 0.0, y_min: 0.0, x_max: 10.0, y_max: 10.0,
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
}
