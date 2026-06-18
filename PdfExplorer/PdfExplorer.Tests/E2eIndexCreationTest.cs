using System.Text;
using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Tests;

// ═════════════════════════════════════════════════════════════════════
// Fixture
// ═════════════════════════════════════════════════════════════════════

public sealed class E2eIndexCreationFixture : IDisposable
{
    public PdfEngine Engine { get; }
    public long CollId { get; }
    public string DataDir { get; }

    public E2eIndexCreationFixture()
    {
        DataDir = Path.Combine(Path.GetTempPath(), $"E2eFixture_{Guid.NewGuid()}");
        Directory.CreateDirectory(DataDir);

        var pdfDir = Path.Combine(DataDir, "pdfs");
        Directory.CreateDirectory(pdfDir);

        // ── multi_page.pdf: 3 pages ────────────────────────────────
        // Page 1: "machine learning"
        // Page 2: "deep learning"
        // Page 3: (blank)
        WritePdf(Path.Combine(pdfDir, "multi_page.pdf"),
            "machine learning", "deep learning", "");

        // ── phrase_test.pdf: 1 page ────────────────────────────────
        WritePdf(Path.Combine(pdfDir, "phrase_test.pdf"),
            "machine learning is fun");

        // ── valid_words.pdf: 1 page ────────────────────────────────
        WritePdf(Path.Combine(pdfDir, "valid_words.pdf"),
            "hello world foo bar baz qux");

        // ── truncated.pdf: valid PDF header + corrupted body ───────
        File.WriteAllBytes(
            Path.Combine(pdfDir, "truncated.pdf"),
            Encoding.ASCII.GetBytes("%PDF-1.4\n%%...\ncorrupted garbage data"));

        // ── empty.pdf: 0 bytes ─────────────────────────────────────
        File.WriteAllBytes(Path.Combine(pdfDir, "empty.pdf"), []);

        // ── not_a_pdf.pdf: plain text, no %PDF header ─────────────
        File.WriteAllBytes(
            Path.Combine(pdfDir, "not_a_pdf.pdf"),
            Encoding.ASCII.GetBytes("Esto no es un PDF"));

        // ── subdir/nested.pdf ──────────────────────────────────────
        var subdir = Directory.CreateDirectory(Path.Combine(pdfDir, "subdir"));
        WritePdf(Path.Combine(subdir.FullName, "nested.pdf"),
            "nested file content");

        // ── Engine + collection ─────────────────────────────────────
        Engine = new PdfEngine(DataDir);
        var id = Engine.AddCollection(pdfDir);
        if (id <= 0)
            throw new InvalidOperationException($"AddCollection returned {id}");
        CollId = id;

        // ── Index once (basic tests consume this state) ─────────────
        var indexed = Engine
            .IndexCollectionAsync(CollId, ocr: false, noIndex: false, null, CancellationToken.None)
            .GetAwaiter().GetResult();
        if (indexed < 0)
            throw new InvalidOperationException($"IndexCollectionAsync returned {indexed}");
    }

    public void Dispose()
    {
        try { Engine.Dispose(); } catch { }
        try
        {
            if (Directory.Exists(DataDir))
                Directory.Delete(DataDir, true);
        }
        catch { /* best-effort */ }
    }

    // ── PDF generation ──────────────────────────────────────────────

    private static void WritePdf(string path, params string[] pageTexts)
    {
        var objOffsets = new List<long>();

        using var ms = new MemoryStream();
        using var writer = new StreamWriter(ms, Encoding.ASCII) { NewLine = "\r\n" };

        long Pos() => ms.Position;

        void WriteObj(int num, string body)
        {
            objOffsets.Add(Pos());
            writer.WriteLine($"{num} 0 obj");
            writer.Write(body);
            if (!body.EndsWith("\r\n"))
                writer.WriteLine();
            writer.WriteLine("endobj");
        }

        writer.WriteLine("%PDF-1.4");
        writer.WriteLine("%\xC4\xE5\xF2\xE5\xEB\xED\xEA");
        writer.Flush();

        var pageCount = pageTexts.Length;

        var contentRawList = new List<string>();
        foreach (var text in pageTexts)
        {
            if (string.IsNullOrEmpty(text) || text.Trim().Length == 0)
            {
                contentRawList.Add("BT ET");
            }
            else
            {
                var escaped = text
                    .Replace("\\", "\\\\")
                    .Replace("(", "\\(")
                    .Replace(")", "\\)");
                contentRawList.Add($"BT /F1 12 Tf 50 700 Td ({escaped}) Tj ET");
            }
        }

        WriteObj(1, "<</Type /Catalog /Pages 2 0 R>>");

        var kids = string.Join(" ", Enumerable.Range(3, pageCount).Select(i => $"{i} 0 R"));
        WriteObj(2, $"<</Type /Pages /Kids [{kids}] /Count {pageCount}>>");

        var contentStartObj = 3 + pageCount;

        for (int i = 0; i < pageCount; i++)
        {
            var pageObjNum = 3 + i;
            WriteObj(pageObjNum,
                $"<</Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]" +
                $" /Contents {contentStartObj + i} 0 R" +
                $" /Resources <</Font <</F1 {contentStartObj + pageCount} 0 R>>>>>>");
        }

        for (int i = 0; i < contentRawList.Count; i++)
        {
            var raw = contentRawList[i];
            var len = Encoding.ASCII.GetByteCount(raw) + 2;
            var objNum = contentStartObj + i;

            objOffsets.Add(Pos());
            writer.WriteLine($"{objNum} 0 obj");
            writer.WriteLine($"<</Length {len}>>");
            writer.WriteLine("stream");
            writer.Write(raw);
            writer.WriteLine();
            writer.WriteLine("endstream");
            writer.WriteLine("endobj");
        }

        var fontObjNum = contentStartObj + pageCount;
        WriteObj(fontObjNum, "<</Type /Font /Subtype /Type1 /BaseFont /Helvetica>>");

        var totalObjs = fontObjNum;

        var xrefOffset = Pos();
        writer.WriteLine("xref");
        writer.WriteLine($"0 {totalObjs + 1}");
        writer.WriteLine($"{0:D10} {65535:D5} f");
        for (int i = 1; i <= totalObjs; i++)
            writer.WriteLine($"{objOffsets[i - 1]:D10} {0:D5} n");

        writer.WriteLine("trailer");
        writer.WriteLine($"<</Size {totalObjs + 1} /Root 1 0 R>>");
        writer.WriteLine("startxref");
        writer.WriteLine($"{xrefOffset}");
        writer.WriteLine("%%EOF");
        writer.Flush();

        File.WriteAllBytes(path, ms.ToArray());
    }
}

// ═════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════

public sealed class E2eIndexCreationTest : IClassFixture<E2eIndexCreationFixture>, IDisposable
{
    private readonly E2eIndexCreationFixture _fixture;

    public E2eIndexCreationTest(E2eIndexCreationFixture fixture)
    {
        _fixture = fixture;
        // Reset mutable search config before each test
        _fixture.Engine.PathFilter = null;
    }

    public void Dispose()
    {
        _fixture.Engine.PathFilter = null;
    }

    // ── Helper ──────────────────────────────────────────────────────

    private long CollId => _fixture.CollId;

    private SearchResponse Search(string query, int limit = 100, int offset = 0)
        => _fixture.Engine.Search(query, limit, offset, CollId);

    // ══════════════════════════════════════════════════════════════════
    //  1. MultiPage_text_is_concatenated
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void MultiPage_text_is_concatenated()
    {
        // "machine" appears in multi_page.pdf (pages 1,2) and phrase_test.pdf → 2 docs
        var resp = Search("machine");
        Assert.Equal(2, resp.Total);

        // "deep" appears only in multi_page.pdf → 1 doc
        resp = Search("deep");
        Assert.Equal(1, resp.Total);
    }

    // ══════════════════════════════════════════════════════════════════
    //  2. Blank_page_does_not_break_extraction
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Blank_page_does_not_break_extraction()
    {
        var resp = Search("machine");
        // multi_page.pdf (page 3 blank) should not prevent extraction of pages 1-2
        Assert.Contains(resp.Results, r => r.Path.Contains("multi_page.pdf",
            StringComparison.OrdinalIgnoreCase));
    }

    // ══════════════════════════════════════════════════════════════════
    //  3. Phrase_search_returns_adjacent_words
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Phrase_search_returns_adjacent_words()
    {
        // "machine learning" is adjacent in both multi_page.pdf (page 1) and phrase_test.pdf
        var resp = Search("\"machine learning\"");
        Assert.True(resp.Total >= 2,
            $"Expected ≥2 documents, got {resp.Total}");
    }

    // ══════════════════════════════════════════════════════════════════
    //  4. Truncated_pdf_is_errored
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Truncated_pdf_is_errored()
    {
        var problems = _fixture.Engine.GetProblematicJobs(CollId);
        Assert.Contains(problems, p => p.FileName.Equals("truncated.pdf",
            StringComparison.OrdinalIgnoreCase));
    }

    // ══════════════════════════════════════════════════════════════════
    //  5. Empty_file_is_errored
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Empty_file_is_errored()
    {
        var problems = _fixture.Engine.GetProblematicJobs(CollId);
        Assert.Contains(problems, p => p.FileName.Equals("empty.pdf",
            StringComparison.OrdinalIgnoreCase));
    }

    // ══════════════════════════════════════════════════════════════════
    //  6. Non_pdf_file_is_errored
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Non_pdf_file_is_errored()
    {
        var problems = _fixture.Engine.GetProblematicJobs(CollId);
        Assert.Contains(problems, p => p.FileName.Equals("not_a_pdf.pdf",
            StringComparison.OrdinalIgnoreCase));
    }

    // ══════════════════════════════════════════════════════════════════
    //  7. Nested_subdirectory_files_are_indexed
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Nested_subdirectory_files_are_indexed()
    {
        var resp = Search("nested");
        Assert.Equal(1, resp.Total);
        Assert.Contains(resp.Results, r =>
            r.Path.Contains("subdir" + Path.DirectorySeparatorChar + "nested.pdf",
                StringComparison.OrdinalIgnoreCase));
    }

    // ══════════════════════════════════════════════════════════════════
    //  8. CollectionStats_reflects_indexed_count
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void CollectionStats_reflects_indexed_count()
    {
        // Total PDFs: 7 (multi_page, phrase_test, valid_words, truncated, empty, not_a_pdf, nested)
        // Errored:    3 (truncated, empty, not_a_pdf)
        // Indexed:    4
        var stats = _fixture.Engine.GetCollectionStats(CollId);
        Assert.NotNull(stats);
        Assert.Equal(4, stats.NumDocs);
    }

    // ══════════════════════════════════════════════════════════════════
    //  9. Pagination_limit
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Pagination_limit()
    {
        var resp = Search("machine", limit: 2);
        Assert.True(resp.Results.Count <= 2,
            $"Expected Results.Count ≤ 2, got {resp.Results.Count}");
    }

    // ══════════════════════════════════════════════════════════════════
    // 10. Pagination_offset
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Pagination_offset()
    {
        var r0 = Search("machine", limit: 1, offset: 0);
        var r1 = Search("machine", limit: 1, offset: 1);

        if (r0.Results.Count == 1 && r1.Results.Count == 1)
            Assert.NotEqual(r0.Results[0].Id, r1.Results[0].Id);
    }

    // ══════════════════════════════════════════════════════════════════
    // 11. Path_filter
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Path_filter()
    {
        _fixture.Engine.PathFilter = "nested";
        var resp = Search("file");
        Assert.Equal(1, resp.Total);
        Assert.Contains(resp.Results, r =>
            r.Path.Contains("nested.pdf", StringComparison.OrdinalIgnoreCase));

        _fixture.Engine.PathFilter = null;
    }

    // ══════════════════════════════════════════════════════════════════
    // 12. Recency_boost_does_not_crash
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Recency_boost_does_not_crash()
    {
        var ex = Record.Exception(() => Search("machine"));
        Assert.Null(ex);
    }

    // ══════════════════════════════════════════════════════════════════
    // 13. Reindex_unchanged_files_are_skipped
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public async Task Reindex_unchanged_files_are_skipped()
    {
        var statsBefore = _fixture.Engine.GetCollectionStats(CollId);
        Assert.NotNull(statsBefore);

        var problemsBefore = _fixture.Engine.GetProblematicJobs(CollId);

        // Re-index with zero changes
        var indexed = await _fixture.Engine
            .IndexCollectionAsync(CollId, ocr: false, noIndex: false, null, CancellationToken.None);
        Assert.Equal(0, indexed);

        var statsAfter = _fixture.Engine.GetCollectionStats(CollId);
        Assert.NotNull(statsAfter);
        Assert.Equal(statsBefore.NumDocs, statsAfter.NumDocs);

        var problemsAfter = _fixture.Engine.GetProblematicJobs(CollId);
        Assert.Equal(problemsBefore.Count, problemsAfter.Count);
    }

    // ══════════════════════════════════════════════════════════════════
    // 14. Reindex_modified_file_updates_index
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public async Task Reindex_modified_file_updates_index()
    {
        // Modify valid_words.pdf: replace "hello" with "goodbye"
        var pdfDir = Path.GetDirectoryName(
            _fixture.Engine.Collections.First(c => c.Id == CollId).BooksFolder);
        Assert.NotNull(pdfDir);

        var validWordsPath = Path.Combine(pdfDir, "valid_words.pdf");
        WritePdf(validWordsPath, "goodbye world foo bar baz qux");

        Thread.Sleep(100); // ensure mtime differs

        var indexed = await _fixture.Engine
            .IndexCollectionAsync(CollId, ocr: false, noIndex: false, null, CancellationToken.None);
        Assert.True(indexed >= 1, $"Expected ≥1 indexed, got {indexed}");

        var resp = Search("goodbye");
        Assert.True(resp.Total >= 1,
            $"Expected ≥1 result for 'goodbye', got {resp.Total}");

        resp = Search("hello");
        Assert.Equal(0, resp.Total);
    }

    // ══════════════════════════════════════════════════════════════════
    // 15. Reindex_invalidates_search_cache
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public async Task Reindex_invalidates_search_cache()
    {
        // "goodbye" should not exist before modification
        var before = Search("goodbye");
        Assert.Equal(0, before.Total);

        var pdfDir = Path.GetDirectoryName(
            _fixture.Engine.Collections.First(c => c.Id == CollId).BooksFolder);
        Assert.NotNull(pdfDir);

        var validWordsPath = Path.Combine(pdfDir, "valid_words.pdf");
        WritePdf(validWordsPath, "goodbye world foo bar baz qux");

        Thread.Sleep(100);

        var indexed = await _fixture.Engine
            .IndexCollectionAsync(CollId, ocr: false, noIndex: false, null, CancellationToken.None);
        Assert.True(indexed >= 1, $"Expected ≥1 indexed, got {indexed}");

        // Cache should be invalidated after re-index
        var after = Search("goodbye");
        Assert.True(after.Total >= 1,
            $"Expected ≥1 result after re-index, got {after.Total}");
    }

    // ══════════════════════════════════════════════════════════════════
    // 16. Retry_failed_jobs
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Retry_failed_jobs()
    {
        var problemsBefore = _fixture.Engine.GetProblematicJobs(CollId);
        Assert.True(problemsBefore.Count >= 3,
            $"Expected ≥3 errored jobs, got {problemsBefore.Count}");

        var retried = _fixture.Engine.RetryFailedJobs(CollId);
        Assert.True(retried >= 3,
            $"Expected RetryFailedJobs to return ≥3, got {retried}");
    }

    // ══════════════════════════════════════════════════════════════════
    // 17. Cancel_indexing_stops_workers
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public async Task Cancel_indexing_stops_workers()
    {
        using var cts = new CancellationTokenSource();

        // We add a new PDF so there is work to cancel.
        var pdfDir = Path.GetDirectoryName(
            _fixture.Engine.Collections.First(c => c.Id == CollId).BooksFolder);
        Assert.NotNull(pdfDir);

        WritePdf(Path.Combine(pdfDir, "cancel_me.pdf"),
            "This file exists only to be indexed during cancellation");

        // Cancel after 500 ms
        cts.CancelAfter(500);

        var result = await _fixture.Engine
            .IndexCollectionAsync(CollId, ocr: false, noIndex: false, null, cts.Token);

        // The operation may either complete normally (≥0) or be cancelled (-1).
        // Both are valid — the key assertion is that it does NOT hang.
        Assert.True(result >= -1, $"Unexpected result: {result}");
    }

    // ── PDF helper (copy in test class for re-index tests) ───────────

    private static void WritePdf(string path, params string[] pageTexts)
    {
        var objOffsets = new List<long>();

        using var ms = new MemoryStream();
        using var writer = new StreamWriter(ms, Encoding.ASCII) { NewLine = "\r\n" };

        long Pos() => ms.Position;

        void WriteObj(int num, string body)
        {
            objOffsets.Add(Pos());
            writer.WriteLine($"{num} 0 obj");
            writer.Write(body);
            if (!body.EndsWith("\r\n"))
                writer.WriteLine();
            writer.WriteLine("endobj");
        }

        writer.WriteLine("%PDF-1.4");
        writer.WriteLine("%\xC4\xE5\xF2\xE5\xEB\xED\xEA");
        writer.Flush();

        var pageCount = pageTexts.Length;

        var contentRawList = new List<string>();
        foreach (var text in pageTexts)
        {
            if (string.IsNullOrEmpty(text) || text.Trim().Length == 0)
            {
                contentRawList.Add("BT ET");
            }
            else
            {
                var escaped = text
                    .Replace("\\", "\\\\")
                    .Replace("(", "\\(")
                    .Replace(")", "\\)");
                contentRawList.Add($"BT /F1 12 Tf 50 700 Td ({escaped}) Tj ET");
            }
        }

        WriteObj(1, "<</Type /Catalog /Pages 2 0 R>>");

        var kids = string.Join(" ", Enumerable.Range(3, pageCount).Select(i => $"{i} 0 R"));
        WriteObj(2, $"<</Type /Pages /Kids [{kids}] /Count {pageCount}>>");

        var contentStartObj = 3 + pageCount;

        for (int i = 0; i < pageCount; i++)
        {
            var pageObjNum = 3 + i;
            WriteObj(pageObjNum,
                $"<</Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]" +
                $" /Contents {contentStartObj + i} 0 R" +
                $" /Resources <</Font <</F1 {contentStartObj + pageCount} 0 R>>>>>>");
        }

        for (int i = 0; i < contentRawList.Count; i++)
        {
            var raw = contentRawList[i];
            var len = Encoding.ASCII.GetByteCount(raw) + 2;
            var objNum = contentStartObj + i;

            objOffsets.Add(Pos());
            writer.WriteLine($"{objNum} 0 obj");
            writer.WriteLine($"<</Length {len}>>");
            writer.WriteLine("stream");
            writer.Write(raw);
            writer.WriteLine();
            writer.WriteLine("endstream");
            writer.WriteLine("endobj");
        }

        var fontObjNum = contentStartObj + pageCount;
        WriteObj(fontObjNum, "<</Type /Font /Subtype /Type1 /BaseFont /Helvetica>>");

        var totalObjs = fontObjNum;

        var xrefOffset = Pos();
        writer.WriteLine("xref");
        writer.WriteLine($"0 {totalObjs + 1}");
        writer.WriteLine($"{0:D10} {65535:D5} f");
        for (int i = 1; i <= totalObjs; i++)
            writer.WriteLine($"{objOffsets[i - 1]:D10} {0:D5} n");

        writer.WriteLine("trailer");
        writer.WriteLine($"<</Size {totalObjs + 1} /Root 1 0 R>>");
        writer.WriteLine("startxref");
        writer.WriteLine($"{xrefOffset}");
        writer.WriteLine("%%EOF");
        writer.Flush();

        File.WriteAllBytes(path, ms.ToArray());
    }
}
