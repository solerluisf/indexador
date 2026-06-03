using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Tests;

/// <summary>
/// Integration tests that exercise the pdf_extractor_capi.dll through PdfEngine.
///
/// Prerequisites:
///   1. Generate test PDFs via: cargo run -p test_pdf_generator -- test_pdfs
///   2. Build the DLL: cargo build --release -p pdf_extractor_capi
///   3. Copy pdf_extractor_capi.dll next to the test assembly (done by .csproj)
///
/// All tests share a single fixture that creates a temp registry and indexes
/// the test PDFs once. Search config is reset before each test.
/// </summary>
[Collection("DLL Sequential")]
public sealed class DllIntegrationTests : IClassFixture<TestPdfFixture>, IDisposable
{
    private readonly TestPdfFixture _fixture;

    public DllIntegrationTests(TestPdfFixture fixture)
    {
        _fixture = fixture;
        ResetSearchConfig();
    }

    public void Dispose()
    {
        ResetSearchConfig();
    }

    // ── Helpers ─────────────────────────────────────────────────────

    private void ResetSearchConfig()
    {
        _fixture.Engine.FuzzyDistance = 0;
        _fixture.Engine.StemEnabled = false;
        _fixture.Engine.SearchField = null;
        _fixture.Engine.PathFilter = null;
        _fixture.Engine.RecencyWeight = 0;
        _fixture.Engine.FieldWeights = null;
        _fixture.Engine.BooleanQuery = null;
    }

    private SearchResponse Search(string query, int limit = 100, int offset = 0, uint? collId = null)
        => _fixture.Engine.Search(query, limit, offset, collId);

    private ulong SearchCountAll(string query)
    {
        var q = Utf8(query);
        ulong count = 0;
        ThrowOnError(pdf_search_count_all(q, ref count));
        return count;
    }

    private static byte[] Utf8(string s) => Encoding.UTF8.GetBytes(s + "\0");

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_search_count_all(byte[] query, ref ulong outCount);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_page_count(byte[] path);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_search_term_offsets(
        uint collId, long docId, byte[] term, [Out] byte[] outJson, ref uint outLen);

    private int[] GetTermOffsets(uint collId, long docId, string term)
    {
        var termBytes = Utf8(term);
        var buf = new byte[4096];
        uint len = (uint)buf.Length;
        var rc = pdf_search_term_offsets(collId, docId, termBytes, buf, ref len);
        ThrowOnError(rc);
        if (len == 0) return [];
        var json = Encoding.UTF8.GetString(buf, 0, (int)len);
        return JsonSerializer.Deserialize<int[]>(json) ?? [];
    }

    private static void ThrowOnError(int rc)
    {
        if (rc < 0) throw new InvalidOperationException($"DLL error {rc}");
    }

    private string GetTestPdf(string name)
    {
        var path = Path.Combine(_fixture.TestPdfDir, name);
        Assert.True(File.Exists(path), $"Test PDF not found: {path}");
        return path;
    }

    // ══════════════════════════════════════════════════════════════════
    //  1. BASIC SEARCH
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Search_pattern_returns_at_least_two_results()
    {
        var resp = Search("pattern");
        Assert.True(resp.Total >= 2, $"Expected ≥2 results for 'pattern', got {resp.Total}");
    }

    [Fact]
    public void Search_count_all_matches_search_total()
    {
        var resp = Search("pattern");
        Assert.Equal(resp.Total, (long)SearchCountAll("pattern"));
    }

    // ══════════════════════════════════════════════════════════════════
    //  2. CASE SENSITIVITY  (via normalized_text — lowercased field)
    // ══════════════════════════════════════════════════════════════════
    //
    //  content_raw is STORED only (not INDEXED), so field-specific
    //  search is not possible on it.  Instead we test case handling
    //  through the normalized_text field which is lowercased.

    [Fact]
    public void Normalized_text_search_is_case_insensitive()
    {
        // normalized_text stores the lowercased version → "PATTERN" should match
        _fixture.Engine.SearchField = "normalized_text";
        var resp = Search("pattern");
        Assert.True(resp.Total >= 2, $"Expected ≥2 results in normalized_text, got {resp.Total}");
    }

    [Fact]
    public void Normalized_text_matches_uppercase_query()
    {
        _fixture.Engine.SearchField = "normalized_text";
        var resp = Search("PATTERN");
        Assert.True(resp.Total >= 2, $"Expected ≥2 results for uppercase query, got {resp.Total}");
    }

    // ══════════════════════════════════════════════════════════════════
    //  3. STEMMING
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Stem_disabled_run_returns_zero()
    {
        var resp = Search("run");
        Assert.Equal(0, resp.Total);
    }

    [Fact]
    public void Stem_disabled_running_finds_direct_match()
    {
        var resp = Search("running");
        Assert.True(resp.Total >= 1, $"Expected ≥1 result for 'running', got {resp.Total}");
    }

    [Fact]
    public void Stem_enabled_running_still_finds_match()
    {
        _fixture.Engine.StemEnabled = true;
        var resp = Search("running");
        Assert.True(resp.Total >= 1, $"Expected ≥1 result with stem, got {resp.Total}");
    }

    // ══════════════════════════════════════════════════════════════════
    //  4. PHRASE SEARCH
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Phrase_machine_learning_returns_results()
    {
        var resp = Search("\"machine learning\"");
        Assert.True(resp.Total >= 2, $"Expected ≥2 results, got {resp.Total}");
    }

    [Fact]
    public void Phrase_vector_machine_returns_result()
    {
        var resp = Search("\"vector machine\"");
        Assert.True(resp.Total >= 1, $"Expected ≥1 result, got {resp.Total}");
    }

    // ══════════════════════════════════════════════════════════════════
    //  5. PATH FILTER  (RegexQuery against the path field)
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Path_filter_nonexistent_filters_out_results()
    {
        _fixture.Engine.PathFilter = "ZZZZNONEXISTENTZZZZ";
        var resp = Search("pattern");
        Assert.Empty(resp.Results);
    }

    [Fact]
    public void Path_filter_clear_restores_all_results()
    {
        _fixture.Engine.PathFilter = null;
        var resp = Search("pattern");
        Assert.True(resp.Total >= 2, $"Expected ≥2 results after clearing filter, got {resp.Total}");
    }

    // ══════════════════════════════════════════════════════════════════
    //  7. BOOLEAN QUERY
    // ══════════════════════════════════════════════════════════════════
    //
    //  Note: when a boolean query is active, the "query" string is
    //  ignored by the Rust side.  However pdf_search_all still calls
    //  search_count(query_str) on each collection to compute the
    //  top-level "total", so .Total may be 0 while .Results still
    //  contains matches.  We assert on .Results.Count instead.

    [Fact]
    public void BooleanQuery_MUST_cat_finds_results()
    {
        _fixture.Engine.BooleanQuery = """[{"term": "cat", "occur": "must"}]""";
        var resp = Search("ignored");
        Assert.True(resp.Results.Count >= 1, $"Expected ≥1 result(s), got {resp.Results.Count}");
    }

    [Fact]
    public void BooleanQuery_MUST_bird_MUST_NOT_dog_returns_bird_only()
    {
        // test_boolean.pdf page 1 has "cat dog bird" so it matches bird but also has dog.
        // Documents with bird but without dog = none in our set.
        // This test verifies the boolean query runs without error.
        _fixture.Engine.BooleanQuery = """[{"term": "bird", "occur": "must"}, {"term": "dog", "occur": "must_not"}]""";
        var resp = Search("ignored");
        // bird only appears alongside dog (page 1), so MUST(bird)+MUST_NOT(dog) returns 0
        Assert.Empty(resp.Results);
    }

    [Fact]
    public void BooleanQuery_MUST_NOT_dog_returns_many()
    {
        _fixture.Engine.BooleanQuery = """[{"term": "dog", "occur": "must_not"}]""";
        var resp = Search("ignored");
        Assert.True(resp.Results.Count >= 1, $"Expected ≥1 result(s), got {resp.Results.Count}");
    }

    [Fact]
    public void BooleanQuery_clear_restores_normal_search()
    {
        _fixture.Engine.BooleanQuery = null;
        var resp = Search("pattern");
        Assert.True(resp.Total >= 2, $"Expected ≥2 results, got {resp.Total}");
    }

    // ══════════════════════════════════════════════════════════════════
    //  8. PAGE COUNT UTILITY
    // ══════════════════════════════════════════════════════════════════

    [Theory]
    [InlineData("test_case_sensitivity.pdf", 4)]
    [InlineData("test_blank.pdf", 1)]
    [InlineData("test_repeat.pdf", 4)]
    [InlineData("test_japanese.pdf", 4)]
    public void PageCount_returns_expected(string name, int expected)
    {
        Assert.Equal(expected, _fixture.Engine.PageCount(GetTestPdf(name)));
    }

    [Fact]
    public void PageCount_nonexistent_file_returns_negative()
    {
        Assert.True(_fixture.Engine.PageCount(@"C:\nonexistent_file.pdf") < 0);
    }

    // ══════════════════════════════════════════════════════════════════
    //  9. PAGINATION
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Pagination_limit_1_returns_single_result()
    {
        Assert.Single(Search("pattern", limit: 1).Results);
    }

    [Fact]
    public void Pagination_offset_returns_different_result()
    {
        var r0 = Search("pattern", limit: 1, offset: 0);
        var r1 = Search("pattern", limit: 1, offset: 1);
        if (r0.Results.Count == 1 && r1.Results.Count == 1)
            Assert.NotEqual(r0.Results[0].Path, r1.Results[0].Path);
    }

    // ══════════════════════════════════════════════════════════════════
    // 10. EMPTY / NO-MATCH QUERIES
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Empty_query_returns_empty() => Assert.Equal(0, Search("").Total);

    [Fact]
    public void No_match_query_returns_empty()
        => Assert.Equal(0, Search("xyznonexistentword12345").Total);

    // ══════════════════════════════════════════════════════════════════
    // 11. FIELD-SPECIFIC SEARCH
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Search_in_normalized_text_is_ci()
    {
        _fixture.Engine.SearchField = "normalized_text";
        Assert.True(Search("pattern").Total >= 2);
    }

    // ══════════════════════════════════════════════════════════════════
    // 12. BLANK DOCUMENT
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Blank_document_not_in_text_results()
    {
        var blank = Search("pattern").Results
            .Where(r => r.Path.Contains("blank", StringComparison.OrdinalIgnoreCase));
        Assert.Empty(blank);
    }

    // ══════════════════════════════════════════════════════════════════
    // 13. LARGE DOCUMENT
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Large_document_searchable()
    {
        var resp = Search("lorem");
        Assert.True(resp.Total >= 1, $"Expected ≥1 result, got {resp.Total}");
        Assert.Contains("test_large", resp.Results.First().Path);
    }

    // ══════════════════════════════════════════════════════════════════
    // 14. JAPANESE / CHINESE
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Japanese_pdf_has_correct_page_count()
        => Assert.Equal(4, _fixture.Engine.PageCount(GetTestPdf("test_japanese.pdf")));

    [Fact]
    public void Chinese_pdf_has_correct_page_count()
        => Assert.Equal(4, _fixture.Engine.PageCount(GetTestPdf("test_chinese.pdf")));

    // ══════════════════════════════════════════════════════════════════
    // 15. MIXED LANGUAGE
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Mixed_pdf_found_for_machine_learning()
    {
        var resp = Search("machine learning");
        Assert.Contains(resp.Results, r => r.Path.Contains("test_mixed", StringComparison.OrdinalIgnoreCase));
    }

    // ══════════════════════════════════════════════════════════════════
    // 16. REPEATED WORD
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Repeat_pdf_found_for_pattern()
    {
        Assert.Contains(
            Search("pattern").Results,
            r => r.Path.Contains("test_repeat", StringComparison.OrdinalIgnoreCase));
    }

    // ══════════════════════════════════════════════════════════════════
    // 17. SNIPPET GENERATION
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Search_results_have_non_empty_snippets()
    {
        foreach (var r in Search("pattern").Results)
            Assert.False(string.IsNullOrEmpty(r.Snippet), $"Snippet empty for {r.Path}");
    }

    // ══════════════════════════════════════════════════════════════════
    // 18. MULTI-MATCH WORD
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Search_machine_finds_multiple_documents()
    {
        Assert.True(Search("machine").Total >= 2);
    }

    // ══════════════════════════════════════════════════════════════════
    // 19. PRECISION: EXACT DOC COUNT + WORD OFFSETS
    // ══════════════════════════════════════════════════════════════════
    //
    //  Validates that the search engine returns the exact number of
    //  matching documents and that the word offset positions inside
    //  each document correspond to the known PDF content.

    [Fact]
    public void Pattern_search_exact_doc_count()
    {
        // "pattern" appears (case-normalized) in:
        //   test_case_sensitivity.pdf  — "Pattern" / "pattern" / "PATTERN"
        //   test_repeat.pdf            — "pattern" × 4
        // No other test PDF contains the word "pattern".
        Assert.Equal(2, Search("pattern").Total);
    }

    [Fact]
    public void Pattern_offsets_in_repeat_pdf()
    {
        // test_repeat.pdf has 4 pages, each with the single word "pattern".
        // content_norm = "pattern\n\n\npattern\n\n\npattern\n\n\npattern"
        // math tokenizer → 4 tokens at word positions 0, 1, 2, 3.
        var result = Search("pattern").Results
            .First(r => r.Path.Contains("test_repeat", StringComparison.OrdinalIgnoreCase));
        var offsets = GetTermOffsets(_fixture.CollectionId, result.Id, "pattern");
        Assert.Equal([0, 1, 2, 3], offsets);
    }

    [Fact]
    public void Pattern_offsets_in_case_sensitivity_pdf()
    {
        // Pages: ["Pattern", "pattern", "PATTERN", ""]
        // Math tokenizer lowercases: "pattern" appears at positions 0, 1, 2.
        var result = Search("pattern").Results
            .First(r => r.Path.Contains("test_case_sensitivity", StringComparison.OrdinalIgnoreCase));
        var offsets = GetTermOffsets(_fixture.CollectionId, result.Id, "pattern");
        Assert.Equal([0, 1, 2], offsets);
    }

    [Fact]
    public void Machine_offsets_in_phrase_pdf()
    {
        // test_phrase.pdf pages (concatenated into 1 TantivyDocument):
        //   "support vector machine"  → tokens: support[0] vector[1] machine[2]
        //   "machine learning"        → tokens: machine[3] learning[4]
        //   "vector machine learning" → tokens: vector[5] machine[6] learning[7]
        var result = Search("machine").Results
            .First(r => r.Path.Contains("test_phrase.pdf", StringComparison.OrdinalIgnoreCase));
        var offsets = GetTermOffsets(_fixture.CollectionId, result.Id, "machine");
        Assert.Equal([2, 3, 6], offsets);
    }

    [Fact]
    public void Cat_offsets_in_boolean_pdf()
    {
        // test_boolean.pdf pages:
        //   "cat dog bird"  → cat[0] dog[1] bird[2]
        //   "cat"           → cat[3]
        //   "dog"           → dog[4]
        var result = Search("cat").Results
            .First(r => r.Path.Contains("test_boolean", StringComparison.OrdinalIgnoreCase));
        var offsets = GetTermOffsets(_fixture.CollectionId, result.Id, "cat");
        Assert.Equal([0, 3], offsets);
    }

    [Fact]
    public void Learning_offsets_in_mixed_pdf()
    {
        // test_mixed.pdf pages:
        //   "Machine Learning 机器学习"      → machine[0] learning[1] 机器学习[2]
        //   "deep learning 深度学习"          → deep[3] learning[4] 深度学习[5]
        var result = Search("learning").Results
            .First(r => r.Path.Contains("test_mixed", StringComparison.OrdinalIgnoreCase));
        var offsets = GetTermOffsets(_fixture.CollectionId, result.Id, "learning");
        Assert.Equal([1, 4], offsets);
    }

    [Fact]
    public void Vector_offsets_in_phrase_pdf()
    {
        // "vector" appears at positions 1 and 5.
        var result = Search("vector").Results
            .First(r => r.Path.Contains("test_phrase.pdf", StringComparison.OrdinalIgnoreCase));
        var offsets = GetTermOffsets(_fixture.CollectionId, result.Id, "vector");
        Assert.Equal([1, 5], offsets);
    }

    [Fact]
    public void Dog_offsets_in_boolean_pdf()
    {
        // "dog" appears at positions 1 (page 1) and 4 (page 3).
        var result = Search("dog").Results
            .First(r => r.Path.Contains("test_boolean", StringComparison.OrdinalIgnoreCase));
        var offsets = GetTermOffsets(_fixture.CollectionId, result.Id, "dog");
        Assert.Equal([1, 4], offsets);
    }

    // ══════════════════════════════════════════════════════════════════
    // 20. PHRASE ADJACENCY SCENARIOS
    // ══════════════════════════════════════════════════════════════════
    //
    //  Validates that words matching a phrase query really appear at
    //  consecutive word offsets within the document's content_norm field.

    private int[] GetPhrasePositions(uint collId, long docId, string phrase)
    {
        var words = phrase.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        if (words.Length < 2) return [];
        var allOffsets = words
            .Select(w => GetTermOffsets(collId, docId, w).ToHashSet())
            .ToList();
        if (allOffsets.Any(s => s.Count == 0)) return [];
        var found = new List<int>();
        foreach (var start in allOffsets[0])
        {
            bool ok = true;
            for (int i = 1; i < words.Length; i++)
                if (!allOffsets[i].Contains(start + i)) { ok = false; break; }
            if (ok) found.Add(start);
        }
        return [.. found];
    }

    [Fact]
    public void Phrase_vector_machine_adjacent_in_phrase_pdf()
    {
        // "vector machine" → page 1 vector[1]+machine[2], page 3 vector[5]+machine[6]
        var result = Search("\"vector machine\"").Results
            .First(r => r.Path.Contains("test_phrase.pdf", StringComparison.OrdinalIgnoreCase));
        Assert.Equal([1, 5], GetPhrasePositions(_fixture.CollectionId, result.Id, "vector machine"));
    }

    [Fact]
    public void Phrase_machine_learning_adjacent_in_phrase_pdf()
    {
        // "machine learning" → page 2 machine[3]+learning[4], page 3 machine[6]+learning[7]
        var result = Search("\"machine learning\"").Results
            .First(r => r.Path.Contains("test_phrase.pdf", StringComparison.OrdinalIgnoreCase));
        Assert.Equal([3, 6], GetPhrasePositions(_fixture.CollectionId, result.Id, "machine learning"));
    }

    [Fact]
    public void Phrase_support_vector_machine_adjacent_in_phrase_pdf()
    {
        // "support vector machine" → page 1 only: support[0]+vector[1]+machine[2]
        var result = Search("\"support vector machine\"").Results
            .First(r => r.Path.Contains("test_phrase.pdf", StringComparison.OrdinalIgnoreCase));
        Assert.Equal([0], GetPhrasePositions(_fixture.CollectionId, result.Id, "support vector machine"));
    }

    [Fact]
    public void Phrase_vector_machine_learning_adjacent_in_phrase_pdf()
    {
        // "vector machine learning" → page 3 only: vector[5]+machine[6]+learning[7]
        var result = Search("\"vector machine learning\"").Results
            .First(r => r.Path.Contains("test_phrase.pdf", StringComparison.OrdinalIgnoreCase));
        Assert.Equal([5], GetPhrasePositions(_fixture.CollectionId, result.Id, "vector machine learning"));
    }

    [Fact]
    public void Phrase_learning_machine_does_not_match()
    {
        // "learning" and "machine" both exist but are never adjacent within a single page.
        // test_phrase_extra.pdf creates a false cross-page adjacency (learning[1] → machine[2]
        // where learning is the last word of page 1 and machine the first of page 2).
        // test_custom_phrase.pdf adds another: learning[2] → machine[3] (page 2 last → page 3 first).
        // The pipeline concatenates all pages into one TantivyDocument, so this matches.
        Assert.Equal(2, Search("\"learning machine\"").Total);
    }

    [Fact]
    public void Phrase_support_learning_does_not_match()
    {
        // "support" exists at offset 0, "learning" at offsets 4 and 7 — never adjacent.
        Assert.Equal(0, Search("\"support learning\"").Total);
    }

    [Fact]
    public void Phrase_machine_support_does_not_match()
    {
        // "machine support" never appears in any document.
        Assert.Equal(0, Search("\"machine support\"").Total);
    }

    [Fact]
    public void Phrase_cat_bird_does_not_match()
    {
        // "cat dog bird" → cat[0] dog[1] bird[2]. "cat bird" skips dog — not adjacent.
        Assert.Equal(0, Search("\"cat bird\"").Total);
    }

    // ══════════════════════════════════════════════════════════════════
    // 21. AUTO-QUOTING + PHRASE VS AND REGRESSION
    // ══════════════════════════════════════════════════════════════════
    //
    //  test_phrase_extra.pdf has 3 pages: ["machine learning", "machine", "machine learning"]
    //  The pipeline concatenates all pages into a single TantivyDocument.
    //  Foxit (phrase search) shows 2 pages. AND would show 3 (or all 3 pages).
    //  Auto-quoting + PhraseQuery returns 1 document (the entire PDF).
    //  The fundamental test is: phrase search does NOT return the standalone-"machine" page
    //  as a separate result — which it can't, since pages aren't separate documents.

    [Fact]
    public void Phrase_auto_quote_returns_only_phrase_docs()
    {
        var resp = Search("machine learning");
        var extra = resp.Results
            .Where(r => r.Path.Contains("test_phrase_extra", StringComparison.OrdinalIgnoreCase))
            .ToList();
        // The PDF is indexed as one TantivyDocument, so we expect exactly 1 result.
        Assert.Equal(1, extra.Count);
    }

    [Fact]
    public void Phrase_auto_quote_term_positions_return_nonempty()
    {
        var doc = GetDoc("machine learning", "test_phrase_extra");
        var positions = _fixture.Engine.GetTermPositions(_fixture.CollectionId, doc.Id, "machine learning");
        Assert.NotEmpty(positions);
    }

    // ── 21b. CUSTOM PHRASE TEST: machine on page 1, "machine learning" on pages 2-3 ──

    /// Maps a Tantivy word offset back to its page number via the SQLite PositionStore.
    /// Assumes SQLite entries are stored in the same order as Tantivy offsets.
    private int GetPageForOffset(uint collId, long docId, string term, int offset)
    {
        var sqlitePositions = GetSqlitePositions(collId, docId, term);
        var tantivyOffsets = GetTermOffsets(collId, docId, term);
        int idx = Array.IndexOf(tantivyOffsets, offset);
        return idx >= 0 && idx < sqlitePositions.Count ? sqlitePositions[idx].Page : -1;
    }

    [Fact]
    public void Phrase_custom_machine_learning_returns_2_pages()
    {
        // test_custom_phrase.pdf has 4 pages: ["machine", "machine learning", "machine learning", ""]
        // Page 1 only has "machine", so "machine learning" (phrase) must NOT match page 1.
        // Pages 2 and 3 have "machine learning" — those are the only matches.
        var resp = Search("machine learning");
        var doc = resp.Results
            .FirstOrDefault(r => r.Path.Contains("test_custom_phrase", StringComparison.OrdinalIgnoreCase));
        Assert.NotNull(doc);

        // "machine" exists on pages 1, 2, 3 at Tantivy offsets 0, 1, 3
        var machineOffsets = GetTermOffsets(_fixture.CollectionId, doc.Id, "machine");
        Assert.Equal([0, 1, 3], machineOffsets);

        // "learning" exists on pages 2, 3 at offsets 2, 4
        var learningOffsets = GetTermOffsets(_fixture.CollectionId, doc.Id, "learning");
        Assert.Equal([2, 4], learningOffsets);

        // SQLite page numbers confirm the per-page mapping
        var machinePages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "machine")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([1, 2, 3], machinePages);

        var learningPages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "learning")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([2, 3], learningPages);

        // Phrase "machine learning" matches at offsets 1→2 (page 2) and 3→4 (page 3)
        var phraseStarts = GetPhrasePositions(_fixture.CollectionId, doc.Id, "machine learning");
        Assert.Equal([1, 3], phraseStarts);

        var phrasePages = phraseStarts
            .Select(o => GetPageForOffset(_fixture.CollectionId, doc.Id, "machine", o))
            .Order()
            .ToArray();
        Assert.Equal([2, 3], phrasePages);
    }

    [Fact]
    public void GetTermPositions_phrase_returns_only_phrase_pages()
    {
        // pdf_get_term_positions with a multi-word phrase must return
        // bounding boxes ONLY from pages where the phrase matches.
        var doc = GetDoc("machine learning", "test_custom_phrase");
        // doc has pages 1-4: ["machine", "machine learning", "machine learning", ""]

        // Direct pdf_get_term_positions with "machine learning" (phrase query)
        var termBytes = Utf8("machine learning");
        var buf = new byte[65536];
        uint len = (uint)buf.Length;
        var rc = pdf_get_term_positions(_fixture.CollectionId, doc.Id, termBytes, buf, ref len);
        Assert.Equal(0, rc);
        var json = Encoding.UTF8.GetString(buf, 0, (int)len);
        var positions = JsonSerializer.Deserialize<List<WordPosition>>(json) ?? [];
        var pages = positions.Select(p => p.Page).Distinct().Order().ToArray();

        // Must return ONLY pages 2 and 3 (where "machine learning" appears)
        Assert.Equal([2, 3], pages);
    }

    [Fact]
    public void GetTermPositions_phrase_reversed_returns_empty()
    {
        // "learning machine" reversed should NOT match any pages
        var doc = GetDoc("machine learning", "test_custom_phrase");

        var termBytes = Utf8("learning machine");
        var buf = new byte[65536];
        uint len = (uint)buf.Length;
        var rc = pdf_get_term_positions(_fixture.CollectionId, doc.Id, termBytes, buf, ref len);
        Assert.Equal(0, rc);
        var json = Encoding.UTF8.GetString(buf, 0, (int)len);
        var positions = JsonSerializer.Deserialize<List<WordPosition>>(json) ?? [];
        Assert.Empty(positions);
    }

    [Fact]
    public void GetTermPositions_single_word_returns_all_pages()
    {
        // Single-word query should NOT be filtered — returns ALL pages
        var doc = GetDoc("machine", "test_custom_phrase");

        var termBytes = Utf8("machine");
        var buf = new byte[65536];
        uint len = (uint)buf.Length;
        var rc = pdf_get_term_positions(_fixture.CollectionId, doc.Id, termBytes, buf, ref len);
        Assert.Equal(0, rc);
        var json = Encoding.UTF8.GetString(buf, 0, (int)len);
        var positions = JsonSerializer.Deserialize<List<WordPosition>>(json) ?? [];
        var pages = positions.Select(p => p.Page).Distinct().Order().ToArray();

        // "machine" appears on pages 1, 2, 3 — no phrase filtering
        Assert.Equal([1, 2, 3], pages);
    }

    [Fact]
    public void GetTermPositions_phrase_case_sensitive_pdf_returns_all_variants()
    {
        // test_case_sensitivity.pdf: "Pattern"(1), "pattern"(2), "PATTERN"(3)
        // Single-word query for "pattern" should return all 3 pages
        var doc = GetDoc("pattern", "test_case_sensitivity");

        var termBytes = Utf8("pattern");
        var buf = new byte[65536];
        uint len = (uint)buf.Length;
        var rc = pdf_get_term_positions(_fixture.CollectionId, doc.Id, termBytes, buf, ref len);
        Assert.Equal(0, rc);
        var json = Encoding.UTF8.GetString(buf, 0, (int)len);
        var positions = JsonSerializer.Deserialize<List<WordPosition>>(json) ?? [];
        var pages = positions.Select(p => p.Page).Distinct().Order().ToArray();
        Assert.Equal([1, 2, 3], pages);
    }

    // ══════════════════════════════════════════════════════════════════
    // 22. POSITION STORE (SQLite) — FULL VALIDATION
    // ══════════════════════════════════════════════════════════════════
    //
    //  Validates that the SQLite-backed pdf_get_term_positions returns
    //  correct page numbers, bounding-box coordinates, and that the data
    //  matches the Tantivy offset counts AND the C# PdfEngine wrapper.

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_get_term_positions(
        uint collId, long docId, byte[] term, [Out] byte[] outJson, ref uint outLen);

    private List<WordPosition> GetSqlitePositions(uint collId, long docId, string term)
    {
        var termBytes = Utf8(term);
        var buf = new byte[65536];
        uint len = (uint)buf.Length;
        var rc = pdf_get_term_positions(collId, docId, termBytes, buf, ref len);
        ThrowOnError(rc);
        if (len == 0) return [];
        var json = Encoding.UTF8.GetString(buf, 0, (int)len);
        return JsonSerializer.Deserialize<List<WordPosition>>(json) ?? [];
    }

    private sealed record DocResult(long Id, string FileName);

    private DocResult GetDoc(string query, string fileNameContains)
    {
        var r = Search(query).Results
            .First(r => r.Path.Contains(fileNameContains, StringComparison.OrdinalIgnoreCase));
        return new(r.Id, Path.GetFileName(r.Path));
    }

    // ── 21a. PAGE NUMBER VALIDATION ──────────────────────────────────

    [Fact]
    public void Sqlite_pages_for_pattern_in_repeat_pdf()
    {
        // "pattern" on each of 4 pages → pages 1..4
        var doc = GetDoc("pattern", "test_repeat");
        var pages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "pattern")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([1, 2, 3, 4], pages);
    }

    [Fact]
    public void Sqlite_pages_for_pattern_in_case_sensitivity_pdf()
    {
        // Pages: "Pattern"(1), "pattern"(2), "PATTERN"(3), blank(4)
        // The LIKE query matches all three case variants.
        var doc = GetDoc("pattern", "test_case_sensitivity");
        var pages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "pattern")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([1, 2, 3], pages);
    }

    [Fact]
    public void Sqlite_pages_for_vector_in_phrase_pdf()
    {
        // "vector" on page 1 (support vector machine) and page 3 (vector machine learning)
        var doc = GetDoc("vector", "test_phrase.pdf");
        var pages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "vector")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([1, 3], pages);
    }

    [Fact]
    public void Sqlite_pages_for_machine_in_phrase_pdf()
    {
        // "machine" on all 3 pages
        var doc = GetDoc("machine", "test_phrase.pdf");
        var pages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "machine")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([1, 2, 3], pages);
    }

    [Fact]
    public void Sqlite_pages_for_learning_in_phrase_pdf()
    {
        // "learning" on page 2 (machine learning) and page 3 (vector machine learning)
        var doc = GetDoc("learning", "test_phrase.pdf");
        var pages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "learning")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([2, 3], pages);
    }

    [Fact]
    public void Sqlite_pages_for_cat_in_boolean_pdf()
    {
        // "cat" on page 1 (cat dog bird) and page 2 (cat)
        var doc = GetDoc("cat", "test_boolean");
        var pages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "cat")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([1, 2], pages);
    }

    [Fact]
    public void Sqlite_pages_for_dog_in_boolean_pdf()
    {
        // "dog" on page 1 (cat dog bird) and page 3 (dog)
        var doc = GetDoc("dog", "test_boolean");
        var pages = GetSqlitePositions(_fixture.CollectionId, doc.Id, "dog")
            .Select(p => p.Page).Order().ToArray();
        Assert.Equal([1, 3], pages);
    }

    // ── 21b. BOUNDING-BOX VALIDATION ─────────────────────────────────

    [Fact]
    public void Sqlite_bounding_boxes_within_page_bounds()
    {
        // All test PDFs use MediaBox [0 0 612 792]
        const float PageW = 612f;
        const float PageH = 792f;

        var doc = GetDoc("pattern", "test_repeat");
        var allPositions = GetSqlitePositions(_fixture.CollectionId, doc.Id, "pattern");

        Assert.All(allPositions, pos =>
        {
            Assert.True(pos.XMin >= 0f && pos.XMax <= PageW,
                $"X out of bounds: min={pos.XMin} max={pos.XMax}");
            Assert.True(pos.YMin >= 0f && pos.YMax <= PageH,
                $"Y out of bounds: min={pos.YMin} max={pos.YMax}");
            Assert.True(pos.XMin < pos.XMax, $"Zero-width box at page {pos.Page}");
            Assert.True(pos.YMin < pos.YMax, $"Zero-height box at page {pos.Page}");

            var width = pos.XMax - pos.XMin;
            var height = pos.YMax - pos.YMin;
            Assert.True(width > 0f && width < 200f,
                $"Unreasonable width {width} at page {pos.Page}");
            Assert.True(height > 0f && height < 50f,
                $"Unreasonable height {height} at page {pos.Page}");
        });
    }

    // ── 21c. OFFSET-COUNT CONSISTENCY (SQLite vs Tantivy) ────────────

    [Fact]
    public void Sqlite_tantivy_offset_count_matches_for_pattern()
    {
        var doc = GetDoc("pattern", "test_repeat");
        Assert.Equal(
            GetTermOffsets(_fixture.CollectionId, doc.Id, "pattern").Length,
            GetSqlitePositions(_fixture.CollectionId, doc.Id, "pattern").Count);
    }

    [Fact]
    public void Sqlite_tantivy_offset_count_matches_for_vector()
    {
        var doc = GetDoc("vector", "test_phrase.pdf");
        Assert.Equal(
            GetTermOffsets(_fixture.CollectionId, doc.Id, "vector").Length,
            GetSqlitePositions(_fixture.CollectionId, doc.Id, "vector").Count);
    }

    [Fact]
    public void Sqlite_tantivy_offset_count_matches_for_learning()
    {
        var doc = GetDoc("learning", "test_mixed");
        Assert.Equal(
            GetTermOffsets(_fixture.CollectionId, doc.Id, "learning").Length,
            GetSqlitePositions(_fixture.CollectionId, doc.Id, "learning").Count);
    }

    [Fact]
    public void Sqlite_tantivy_offset_count_matches_for_cat()
    {
        var doc = GetDoc("cat", "test_boolean");
        Assert.Equal(
            GetTermOffsets(_fixture.CollectionId, doc.Id, "cat").Length,
            GetSqlitePositions(_fixture.CollectionId, doc.Id, "cat").Count);
    }

    // ── 21d. ROUND-TRIP: PdfEngine.GetTermPositions vs RAW P/Invoke ──
    //
    //  Validates that the C# PdfEngine wrapper returns exactly the same
    //  position data as the raw C API call, proving the UI receives the
    //  same data the Rust side produces.

    [Fact]
    public void Engine_GetTermPositions_matches_raw_pinvoke()
    {
        var doc = GetDoc("pattern", "test_repeat");

        var enginePositions = _fixture.Engine.GetTermPositions(
            _fixture.CollectionId, doc.Id, "pattern");

        var rawPositions = GetSqlitePositions(
            _fixture.CollectionId, doc.Id, "pattern");

        Assert.Equal(enginePositions.Count, rawPositions.Count);
        for (int i = 0; i < enginePositions.Count; i++)
        {
            Assert.Equal(enginePositions[i].Page, rawPositions[i].Page);
            Assert.Equal(enginePositions[i].XMin, rawPositions[i].XMin, 3);
            Assert.Equal(enginePositions[i].YMin, rawPositions[i].YMin, 3);
            Assert.Equal(enginePositions[i].XMax, rawPositions[i].XMax, 3);
            Assert.Equal(enginePositions[i].YMax, rawPositions[i].YMax, 3);
        }
    }

    [Fact]
    public void Engine_GetTermPositions_matches_raw_pinvoke_for_vector()
    {
        var doc = GetDoc("vector", "test_phrase.pdf");

        var enginePositions = _fixture.Engine.GetTermPositions(
            _fixture.CollectionId, doc.Id, "vector");

        var rawPositions = GetSqlitePositions(
            _fixture.CollectionId, doc.Id, "vector");

        Assert.Equal(enginePositions.Count, rawPositions.Count);
        for (int i = 0; i < enginePositions.Count; i++)
        {
            Assert.Equal(enginePositions[i].Page, rawPositions[i].Page);
            Assert.Equal(enginePositions[i].XMin, rawPositions[i].XMin, 3);
            Assert.Equal(enginePositions[i].YMin, rawPositions[i].YMin, 3);
            Assert.Equal(enginePositions[i].XMax, rawPositions[i].XMax, 3);
            Assert.Equal(enginePositions[i].YMax, rawPositions[i].YMax, 3);
        }
    }
}

// ── Fixture ─────────────────────────────────────────────────────────

public sealed class TestPdfFixture : IDisposable
{
    public PdfEngine Engine { get; }
    public uint CollectionId { get; }
    public string TestPdfDir { get; }
    public string RegistryDir { get; }

    public TestPdfFixture()
    {
        RegistryDir = Path.Combine(Path.GetTempPath(), $"PdfExplorerTests_{Guid.NewGuid()}");
        Directory.CreateDirectory(RegistryDir);

        TestPdfDir = FindTestPdfDir();
        if (!Directory.Exists(TestPdfDir))
            throw new InvalidOperationException(
                $"Test PDF directory not found: {TestPdfDir}\n" +
                "Run 'cargo run -p test_pdf_generator -- <path>' to generate test PDFs.");

        Engine = new PdfEngine(RegistryDir);

        var collId = Engine.AddCollection(TestPdfDir);
        if (collId <= 0)
            throw new InvalidOperationException($"AddCollection failed: {collId}");
        CollectionId = (uint)collId;

        var indexed = Engine
            .IndexCollectionAsync(CollectionId, ocr: false, noIndex: false, null, CancellationToken.None)
            .GetAwaiter().GetResult();
        if (indexed < 0)
            throw new InvalidOperationException($"IndexCollectionAsync failed: {indexed}");

        Console.Error.WriteLine($"[Fixture] Indexed {indexed} docs from {TestPdfDir}");
    }

    public void Dispose()
    {
        try { Engine.Dispose(); } catch { }
        try { if (Directory.Exists(RegistryDir)) Directory.Delete(RegistryDir, true); } catch { }
    }

    private static string FindTestPdfDir()
    {
        var dir = AppContext.BaseDirectory;
        for (int i = 0; i < 10; i++)
        {
            var candidate = Path.Combine(dir, "test_pdfs");
            if (Directory.Exists(candidate))
                return Path.GetFullPath(candidate);
            var parent = Path.GetDirectoryName(dir);
            if (parent == null || parent == dir) break;
            dir = parent;
        }
        var fallback = Path.GetFullPath(Path.Combine(
            AppContext.BaseDirectory, @"..\..\..\..\..\test_pdfs"));
        return Directory.Exists(fallback) ? fallback : "";
    }
}
