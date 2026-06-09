using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Tests;

/// <summary>
/// Integration tests for PdfiumPageRenderer using the Rust CAPI
/// (pdf_extractor_capi.dll). All pdfium.dll calls are routed through
/// the CAPI where they work reliably (no CRT allocator mismatch).
/// </summary>
public sealed class PdfiumPageRendererTests
{
    private readonly string _testPdfDir;

    public PdfiumPageRendererTests()
    {
        _testPdfDir = FindTestPdfDir();
        if (!Directory.Exists(_testPdfDir))
            throw new InvalidOperationException(
                $"Test PDF directory not found: {_testPdfDir}\n" +
                "Run 'cargo run -p test_pdf_generator -- <path>' to generate test PDFs.");
    }

    // ══════════════════════════════════════════════════════════════════
    //  1. BASIC FLOWS
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void LoadDocument_returns_correct_page_count()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_case_sensitivity.pdf"));
        Assert.Equal(4, r.PageCount);
    }

    [Fact]
    public void RenderPage_returns_non_null_image()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_case_sensitivity.pdf"));
        var item = r.RenderPage(0, new List<WordPosition>());
        Assert.NotNull(item.PageImage);
    }

    [Fact]
    public void RenderPage_returns_correct_dimensions()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_blank.pdf"));
        var item = r.RenderPage(0, new List<WordPosition>());
        Assert.True(item.ImagePixelWidth > 0);
        Assert.True(item.ImagePixelHeight > 0);
        Assert.True(item.PdfPageWidth > 0);
        Assert.True(item.PdfPageHeight > 0);
    }

    [Fact]
    public void RenderPage_sets_correct_page_number()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_case_sensitivity.pdf"));
        var page0 = r.RenderPage(0, new List<WordPosition>());
        var page2 = r.RenderPage(2, new List<WordPosition>());
        Assert.Equal(1, page0.PageNumber);
        Assert.Equal(3, page2.PageNumber);
    }

    // ══════════════════════════════════════════════════════════════════
    //  2. ALTERNATIVE FLOWS
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void OpenDocument_same_path_twice_is_idempotent()
    {
        var r = new PdfiumPageRenderer();
        var path = GetPath("test_blank.pdf");
        r.OpenDocument(path);
        var count1 = r.PageCount;
        r.OpenDocument(path);
        Assert.Equal(count1, r.PageCount);
    }

    [Fact]
    public void RenderPage_with_positions_preserves_word_text()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_repeat.pdf"));
        var positions = new List<WordPosition>
        {
            new(1, 100, 700, 160, 720, WordText: "pattern"),
        };
        var item = r.RenderPage(0, positions);
        Assert.NotNull(item.PageImage);
        Assert.Single(item.Positions);
        Assert.Equal("pattern", item.Positions[0].WordText);
    }

    [Fact]
    public void RenderPage_with_default_target_width()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_blank.pdf"));
        var item = r.RenderPage(0, new List<WordPosition>());
        Assert.True(item.ImagePixelWidth > 0);
    }

    [Fact]
    public void RenderPage_returns_consistent_pdf_dimensions_across_pages()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_case_sensitivity.pdf"));

        var page0 = r.RenderPage(0, new List<WordPosition>());
        var page2 = r.RenderPage(2, new List<WordPosition>());

        // All pages of the same PDF should have the same dimensions
        Assert.Equal(page0.PdfPageWidth, page2.PdfPageWidth, 3);
        Assert.Equal(page0.PdfPageHeight, page2.PdfPageHeight, 3);
    }

    // ══════════════════════════════════════════════════════════════════
    //  3. ERROR FLOWS
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void OpenDocument_nonexistent_file_throws()
    {
        var r = new PdfiumPageRenderer();
        var badPath = Path.Combine(_testPdfDir, "nonexistent_file.pdf");
        var ex = Assert.Throws<InvalidOperationException>(() => r.OpenDocument(badPath));
        Assert.Contains("Failed to open PDF", ex.Message);
    }

    [Fact]
    public void RenderPage_without_loading_throws()
    {
        var r = new PdfiumPageRenderer();
        var ex = Assert.Throws<InvalidOperationException>(() =>
            r.RenderPage(0, new List<WordPosition>()));
        Assert.Contains("No document open", ex.Message);
    }

    [Fact]
    public void RenderPage_out_of_range_returns_fallback()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_blank.pdf"));
        var item = r.RenderPage(999, new List<WordPosition>());
        Assert.Null(item.PageImage);
    }

    [Fact]
    public void RenderPage_negative_index_returns_fallback()
    {
        var r = new PdfiumPageRenderer();
        r.OpenDocument(GetPath("test_blank.pdf"));
        var item = r.RenderPage(-1, new List<WordPosition>());
        Assert.Null(item.PageImage);
    }

    // ── Helpers ─────────────────────────────────────────────────────

    private string GetPath(string name)
    {
        var full = Path.GetFullPath(Path.Combine(_testPdfDir, name));
        Assert.True(File.Exists(full), $"Test PDF not found: {full}");
        return full;
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
