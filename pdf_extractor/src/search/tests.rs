#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::env::temp_dir;
    use tantivy::IndexWriter;
    use crate::indexer::{SearchIndex, Indexer};
    use crate::search::builders::{AutoPhraseQueryBuilder, BooleanPhraseQueryBuilder};
    use crate::search::engines::TantivyEngine;
    use crate::search::enrichers::snippet::SnippetEnricher;
    use crate::search::enrichers::EnricherCoordinator;
    use crate::search::types::*;
    use crate::search::pipeline::{SearchPipeline, raw_to_rich, default_enrichers};
    use crate::search::traits::{QueryBuilder, SearchEngine};

    fn unique_index_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = temp_dir().join(format!("pdf_extractor_search_test_{}", id));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn add_doc(idx: &SearchIndex, writer: &mut IndexWriter, id: i64, path: &str, text: &str) {
        idx.add_document(writer, id, path, text, None).unwrap();
    }

    /// Creates an index with 5 documents:
    ///   doc1  /doc1.pdf   "hello world"
    ///   doc2  /doc2.pdf   "hello there"
    ///   doc3  /other.pdf  "rust programming language"
    ///   doc4  /doc4.pdf   "hello AND world boolean test"
    ///   doc5  /doc5.pdf   "hello OR world"
    fn setup_index() -> (SearchIndex, PathBuf) {
        let dir = unique_index_dir();
        let idx = SearchIndex::new(&dir).unwrap();
        let mut writer = idx.writer().unwrap();
        add_doc(&idx, &mut writer, 1, "/doc1.pdf", "hello world");
        add_doc(&idx, &mut writer, 2, "/doc2.pdf", "hello there");
        add_doc(&idx, &mut writer, 3, "/other.pdf", "rust programming language");
        add_doc(&idx, &mut writer, 4, "/doc4.pdf", "hello AND world boolean test");
        add_doc(&idx, &mut writer, 5, "/doc5.pdf", "hello OR world");
        writer.commit().unwrap();
        (idx, dir)
    }

    fn make_ctx(idx: &SearchIndex) -> SearchContext {
        SearchContext {
            index: idx.index.clone(),
            id_field: idx.id_field,
            content_field: idx.content_field,
            path_field: idx.path_field,
            position_store: None,
        }
    }

    // --- raw_to_rich ---

    #[test]
    fn test_raw_to_rich_converts_path_and_doc_id() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let builder = AutoPhraseQueryBuilder;
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let query = builder.build(&ctx, &input).unwrap();
        let engine = TantivyEngine;
        let raw = engine.search(&ctx, &*query, &input).unwrap();
        let rich = raw_to_rich(raw, &ctx);

        assert!(!rich.is_empty(), "Should have results");
        assert!(rich.iter().all(|r| !r.path.is_empty()), "All results should have path");
        assert!(rich.iter().all(|r| r.doc_id.is_some()), "All results should have doc_id");
        assert!(rich.iter().all(|r| r.snippet.is_none()), "Snippet should be None before enrichment");
        assert!(rich.iter().all(|r| r.positions.is_empty()), "Positions should be empty before enrichment");
        assert!(rich.iter().all(|r| r.doc_address.is_some()), "All results should have doc_address");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_raw_to_rich_extracts_correct_values() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let builder = AutoPhraseQueryBuilder;
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let query = builder.build(&ctx, &input).unwrap();
        let engine = TantivyEngine;
        let raw = engine.search(&ctx, &*query, &input).unwrap();

        // "hello" matches doc1, doc2, doc4, doc5 (4 docs — not doc3)
        assert_eq!(raw.len(), 4, "hello matches 4 docs");
        let rich = raw_to_rich(raw, &ctx);

        let paths: Vec<&str> = rich.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/doc1.pdf"));
        assert!(paths.contains(&"/doc2.pdf"));
        assert!(paths.contains(&"/doc4.pdf"));
        assert!(paths.contains(&"/doc5.pdf"));
        assert!(!paths.contains(&"/other.pdf"));

        let ids: Vec<i64> = rich.iter().filter_map(|r| r.doc_id).collect();
        assert_eq!(ids.len(), 4);
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&4));
        assert!(ids.contains(&5));

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- SearchPipeline::execute_raw ---

    #[test]
    fn test_pipeline_auto_phrase_single_word() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        // "hello" matches doc1, doc2, doc4, doc5
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_auto_phrase_multiword_bare() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // "hello world" auto-phrased -> phrase query -> only doc1 has consecutive "hello world"
        let input = SearchInput {
            query_str: "hello world".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 1, "Phrase 'hello world' matches doc1 only");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_boolean_phrase_and() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // "hello AND world" -> docs with BOTH words: doc1, doc4, doc5
        let input = SearchInput {
            query_str: "hello AND world".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 3, "AND matches doc1, doc4, doc5");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_boolean_phrase_or() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // "rust OR hello" -> doc3 OR doc1,doc2,doc4,doc5 = 5 docs
        let input = SearchInput {
            query_str: "rust OR hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_boolean_phrase_not() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // "hello NOT world" -> doc2 only (hello without world)
        let input = SearchInput {
            query_str: "hello NOT world".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_boolean_phrase_quoted() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "\"hello world\"".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 1, "Exact phrase matches doc1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_boolean_phrase_bare_multiword() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // Bare multiword without operators -> auto-phrased -> doc3
        let input = SearchInput {
            query_str: "programming language".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_boolean_phrase_plus_minus() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "+hello -world".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 1, "+hello -world matches doc2 only");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_auto_phrase_quoted() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // Explicitly quoted phrase
        let input = SearchInput {
            query_str: "\"hello world\"".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 1, "Phrase 'hello world' matches doc1 only");

        // Mixed: unquoted + quoted
        let input2 = SearchInput {
            query_str: "hello \"programming language\"".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results2 = pipeline.execute_raw(&input2).unwrap();
        // doc3 has "rust programming language" — contains "programming language"
        // doc1, doc2, doc4, doc5 have "hello" but not "programming language"
        // Only doc3 has both "hello" (no) and "programming language" (yes)
        // Actually: "hello" in doc1,2,4,5; "programming language" in doc3 only
        // No single doc has both → 0
        assert_eq!(results2.len(), 0, "No document has both 'hello' AND 'programming language'");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- TantivyEngine unit tests ---

    #[test]
    fn test_engine_search_returns_matches() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let engine = TantivyEngine;
        let builder = AutoPhraseQueryBuilder;
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let query = builder.build(&ctx, &input).unwrap();
        let results = engine.search(&ctx, &*query, &input).unwrap();
        assert_eq!(results.len(), 4, "hello matches 4 docs");
        assert!(results.iter().all(|(s, _, _)| *s > 0.0), "All scores should be positive");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_engine_search_with_path_filter() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let engine = TantivyEngine;
        let builder = AutoPhraseQueryBuilder;
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            path_filter: Some(r"/doc\d+\.pdf".into()),
            ..Default::default()
        };
        let query = builder.build(&ctx, &input).unwrap();
        let results = engine.search(&ctx, &*query, &input).unwrap();
        // doc1, doc2, doc4, doc5 all match /doc\d+\.pdf, doc3 doesn't
        // But hello is only in doc1,2,4,5 so still 4
        assert_eq!(results.len(), 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_engine_search_empty_query() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let engine = TantivyEngine;
        let builder = AutoPhraseQueryBuilder;
        let input = SearchInput {
            query_str: "".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let query = builder.build(&ctx, &input).unwrap();
        let results = engine.search(&ctx, &*query, &input).unwrap();
        assert!(results.is_empty(), "Empty query should return no results");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_engine_count_matches() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let engine = TantivyEngine;
        let builder = AutoPhraseQueryBuilder;
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let query = builder.build(&ctx, &input).unwrap();
        let count = engine.count(&ctx, &*query, &input).unwrap();
        assert_eq!(count, 4, "hello should match 4 docs");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_engine_count_with_path_filter() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let engine = TantivyEngine;
        let builder = AutoPhraseQueryBuilder;
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            path_filter: Some(r"/doc\d+\.pdf".into()),
            ..Default::default()
        };
        let query = builder.build(&ctx, &input).unwrap();
        let count = engine.count(&ctx, &*query, &input).unwrap();
        // hello is in doc1,2,4,5 — all match /doc\d+\.pdf
        assert_eq!(count, 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_engine_count_empty_query() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let engine = TantivyEngine;
        let builder = AutoPhraseQueryBuilder;
        let input = SearchInput {
            query_str: "".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let query = builder.build(&ctx, &input).unwrap();
        let count = engine.count(&ctx, &*query, &input).unwrap();
        assert_eq!(count, 0, "Empty query should return 0 count");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- path_filter ---

    #[test]
    fn test_pipeline_path_filter() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // paths are "/doc1.pdf", "/doc2.pdf", "/doc4.pdf", "/doc5.pdf", "/other.pdf"
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            path_filter: Some(r".*/doc\d+\.pdf".into()),
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        // "hello" in doc1, doc2, doc4, doc5 — all match .*/doc\d+\.pdf -> 4
        assert_eq!(results.len(), 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_path_filter_no_match() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            path_filter: Some(r"nonexistent".into()),
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert!(results.is_empty(), "No match with nonexistent path filter");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- limit / offset ---

    #[test]
    fn test_pipeline_limit() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 2,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 2, "Limit 2 should return 2 results");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_offset() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 1,
            ..Default::default()
        };
        // 4 results total, offset 1 -> 3 remaining
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 3, "Offset 1 on 4 results -> 3");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_limit_with_offset() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 2,
            offset: 1,
            ..Default::default()
        };
        // 4 total, skip 1, take 2 -> 2
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- empty query ---

    #[test]
    fn test_pipeline_empty_query_returns_empty() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert!(results.is_empty(), "Empty query should return no results");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- execute with enrichers ---

    #[test]
    fn test_pipeline_execute_with_snippet_enricher() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let enrichers = EnricherCoordinator::new(vec![
            Box::new(SnippetEnricher),
        ]).unwrap();
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            enrichers,
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute(&input).unwrap();
        assert!(!results.is_empty(), "Should have results");
        // At least one result should have a snippet since doc_address is now populated
        assert!(results.iter().any(|r| r.snippet.is_some()),
            "At least one result should have snippet (doc_address is populated)");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_default_enrichers_is_empty() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let raw = pipeline.execute_raw(&input).unwrap();
        let rich = pipeline.execute(&input).unwrap();
        assert_eq!(raw.len(), rich.len(), "Empty enrichers should not change count");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- BooleanPhrase edge cases ---

    #[test]
    fn test_pipeline_boolean_parentheses() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // "(hello OR rust) AND world" -> docs with world + (hello OR rust):
        // doc1 (hello+world), doc4 (hello+world), doc5 (hello+world) = 3
        let input = SearchInput {
            query_str: "(hello OR rust) AND world".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 3, "(hello OR rust) AND world -> doc1, doc4, doc5");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_boolean_mixed_phrase_and_term() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        // doc4: "hello AND world boolean test" — "hello world" is NOT consecutive here
        let input = SearchInput {
            query_str: "\"hello world\" AND boolean".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 0, "No doc has consecutive 'hello world' AND 'boolean'");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_boolean_single_word() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(BooleanPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "rust".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let results = pipeline.execute_raw(&input).unwrap();
        assert_eq!(results.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- integration: pipeline matches indexer.search() ---

    #[test]
    fn test_pipeline_matches_legacy_search() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            ..Default::default()
        };
        let pipeline_results = pipeline.execute_raw(&input).unwrap();
        let legacy_results = idx.search("hello", 10, None, 0).unwrap();
        assert_eq!(pipeline_results.len(), legacy_results.len(),
            "Pipeline and legacy search should return same count");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_with_path_filter_matches_legacy() {
        let (idx, dir) = setup_index();
        let ctx = make_ctx(&idx);
        let pipeline = SearchPipeline::new(
            ctx,
            Box::new(AutoPhraseQueryBuilder),
            Box::new(TantivyEngine),
            default_enrichers(),
        );
        let input = SearchInput {
            query_str: "hello".into(),
            limit: 10,
            offset: 0,
            path_filter: Some(r".*/doc\d+\.pdf".into()),
            ..Default::default()
        };
        let pipeline_results = pipeline.execute_raw(&input).unwrap();
        let legacy_results = idx.search("hello", 10, Some(r".*/doc\d+\.pdf"), 0).unwrap();
        assert_eq!(pipeline_results.len(), legacy_results.len(),
            "Pipeline and legacy search should match with path_filter");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pipeline_delegates_to_search_parsed() {
        let (idx, dir) = setup_index();
        let indexer = Indexer::new(&dir).unwrap();
        let results = indexer.search_index().search("hello", 10, None, 0).unwrap();
        // search() delegates to search_parsed which uses pipeline with AutoPhrase
        assert_eq!(results.len(), 4, "hello matches 4 docs via pipeline");
        std::fs::remove_dir_all(&dir).ok();
    }
}
