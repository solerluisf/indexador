using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Tests;

public class OverlayHelperTests
{
    // ── DeserializePositions: Basic flows ───────────────────────────

    [Fact]
    public void DeserializePositions_with_valid_json_returns_correct_positions()
    {
        var json = """[{"page":1,"x_min":10.5,"y_min":20.3,"x_max":50.7,"y_max":60.9}]""";
        var positions = OverlayHelper.DeserializePositions(json);

        Assert.Single(positions);
        Assert.Equal(1, positions[0].Page);
        Assert.Equal(10.5f, positions[0].XMin);
        Assert.Equal(20.3f, positions[0].YMin);
        Assert.Equal(50.7f, positions[0].XMax);
        Assert.Equal(60.9f, positions[0].YMax);
    }

    [Fact]
    public void DeserializePositions_with_multiple_positions_returns_all()
    {
        var json = """[{"page":1,"x_min":10,"y_min":20,"x_max":30,"y_max":40},{"page":1,"x_min":50,"y_min":60,"x_max":70,"y_max":80}]""";
        var positions = OverlayHelper.DeserializePositions(json);

        Assert.Equal(2, positions.Count);
    }

    // ── DeserializePositions: Alternative flows ─────────────────────

    [Fact]
    public void DeserializePositions_with_empty_array_returns_empty_list()
    {
        Assert.Empty(OverlayHelper.DeserializePositions("[]"));
    }

    [Fact]
    public void DeserializePositions_with_null_json_returns_empty_list()
    {
        Assert.Empty(OverlayHelper.DeserializePositions("null"));
    }

    [Fact]
    public void DeserializePositions_with_extra_fields_ignores_them()
    {
        var json = """[{"page":1,"x_min":10,"y_min":20,"x_max":50,"y_max":60,"unknown":"ignored"}]""";
        var positions = OverlayHelper.DeserializePositions(json);

        Assert.Single(positions);
        Assert.Equal(1, positions[0].Page);
        Assert.Equal(10f, positions[0].XMin);
    }

    [Fact]
    public void DeserializePositions_with_partial_data_defaults_missing_fields()
    {
        var json = """[{"page":1}]""";
        var positions = OverlayHelper.DeserializePositions(json);

        Assert.Single(positions);
        Assert.Equal(1, positions[0].Page);
        Assert.Equal(0f, positions[0].XMin);
        Assert.Equal(0f, positions[0].YMax);
    }

    // ── DeserializePositions: Error flows ───────────────────────────

    [Fact]
    public void DeserializePositions_with_malformed_json_returns_empty_list()
    {
        Assert.Empty(OverlayHelper.DeserializePositions("{bad json}"));
    }

    [Fact]
    public void DeserializePositions_with_empty_string_returns_empty_list()
    {
        Assert.Empty(OverlayHelper.DeserializePositions(string.Empty));
    }

    [Fact]
    public void DeserializePositions_with_whitespace_returns_empty_list()
    {
        Assert.Empty(OverlayHelper.DeserializePositions("   "));
    }

    [Fact]
    public void DeserializePositions_with_negative_coordinates_still_deserializes()
    {
        var json = """[{"page":1,"x_min":-10,"y_min":-20,"x_max":50,"y_max":60}]""";
        var positions = OverlayHelper.DeserializePositions(json);

        Assert.Single(positions);
        Assert.Equal(-10f, positions[0].XMin);
        Assert.Equal(-20f, positions[0].YMin);
    }

    // ── PageRenderItem.GetHighlightRects tests ──────────────────────

    [Fact]
    public void GetHighlightRects_with_valid_positions_returns_scaled_rects()
    {
        var item = new PageRenderItem
        {
            PageNumber = 1,
            ImagePixelWidth = 800,
            ImagePixelHeight = 1000,
            PdfPageWidth = 400,
            PdfPageHeight = 500,
            Positions = new List<WordPosition>
            {
                new(1, 10, 20, 50, 60),
            },
        };

        var rects = item.GetHighlightRects();

        Assert.Single(rects);
        // scaleX = 800/400 = 2, scaleY = 1000/500 = 2
        // x = 10 * 2 = 20
        // y = (500 - 60) * 2 = 880
        // w = max(2, (50-10)*2) = 80
        // h = max(2, (60-20)*2) = 80
        Assert.Equal(20, rects[0].X);
        Assert.Equal(880, rects[0].Y);
        Assert.Equal(80, rects[0].Width);
        Assert.Equal(80, rects[0].Height);
    }

    [Fact]
    public void GetHighlightRects_with_zero_image_returns_empty()
    {
        var item = new PageRenderItem
        {
            PageNumber = 1,
            ImagePixelWidth = 0,
            ImagePixelHeight = 0,
            PdfPageWidth = 400,
            PdfPageHeight = 500,
            Positions = new List<WordPosition>
            {
                new(1, 10, 20, 50, 60),
            },
        };

        Assert.Empty(item.GetHighlightRects());
    }

    [Fact]
    public void GetHighlightRects_with_empty_positions_returns_empty()
    {
        var item = new PageRenderItem
        {
            PageNumber = 1,
            ImagePixelWidth = 800,
            ImagePixelHeight = 1000,
            PdfPageWidth = 400,
            PdfPageHeight = 500,
            Positions = new List<WordPosition>(),
        };

        Assert.Empty(item.GetHighlightRects());
    }

    [Fact]
    public void GetHighlightRects_enforces_minimum_highlight_size()
    {
        var item = new PageRenderItem
        {
            PageNumber = 1,
            ImagePixelWidth = 800,
            ImagePixelHeight = 1000,
            PdfPageWidth = 400,
            PdfPageHeight = 500,
            Positions = new List<WordPosition>
            {
                new(1, 10, 20, 10.5f, 20.5f), // very small highlight: 0.5pt * 2 = 1px → clamped to 2
            },
        };

        var rects = item.GetHighlightRects();

        Assert.Single(rects);
        Assert.Equal(2, rects[0].Width);
        Assert.Equal(2, rects[0].Height);
    }

    [Fact]
    public void GetHighlightRects_flips_y_axis_correctly()
    {
        // PDF origin bottom-left, WPF origin top-left
        var item = new PageRenderItem
        {
            PageNumber = 1,
            ImagePixelWidth = 200,
            ImagePixelHeight = 400,
            PdfPageWidth = 100,
            PdfPageHeight = 200,
            Positions = new List<WordPosition>
            {
                new(1, 0, 0, 10, 10), // bottom-left of PDF → top of WPF
            },
        };

        var rects = item.GetHighlightRects();

        // scaleX = 200/100 = 2, scaleY = 400/200 = 2
        // y = (200 - 10) * 2 = 380 (near bottom of WPF canvas since it was near top of PDF)
        // Wait: PDF bottom-left (0,0), so (0,0,10,10) is bottom-left
        // After flip: y = (200 - 10) * 2 = 380
        // Hmm, that's near the bottom of the WPF canvas (height=400), so bottom-left → bottom-left. Correct!
        Assert.Equal(380, rects[0].Y);
    }
}
