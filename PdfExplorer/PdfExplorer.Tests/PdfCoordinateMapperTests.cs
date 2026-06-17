using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Tests;

public class PdfCoordinateMapperTests
{
    private static readonly PdfCoordinateMapper _mapper = new(612, 792, 1275, 1650);

    [Fact]
    public void ToNormalizedCenterY_top_of_stored_returns_0()
    {
        var r = PdfRect.FromLtrb(0, 0, 10, 2);
        Assert.Equal(0.0, _mapper.ToNormalizedCenterY(r), 2);
    }

    [Fact]
    public void ToNormalizedCenterY_bottom_of_stored_returns_1()
    {
        var r = PdfRect.FromLtrb(0, 790, 10, 792);
        Assert.Equal(1.0, _mapper.ToNormalizedCenterY(r), 2);
    }

    [Fact]
    public void ToNormalizedCenterY_middle_returns_05()
    {
        var r = PdfRect.FromLtrb(0, 395, 10, 397);
        Assert.Equal(0.5, _mapper.ToNormalizedCenterY(r), 2);
    }

    // ── Rotation-aware ToNormalizedCenterY tests ────────────────────

    [Fact]
    public void ToNormalizedCenterY_rotation0_same_as_before()
    {
        var mapper = new PdfCoordinateMapper(612, 792, 0, 0);

        // stored Y=0 → normalized 0
        var top = PdfRect.FromLtrb(0, 0, 10, 2);
        Assert.Equal(0.0, mapper.ToNormalizedCenterY(top, 0), 2);

        // stored Y=792 → normalized 1
        var bottom = PdfRect.FromLtrb(0, 790, 10, 792);
        Assert.Equal(1.0, mapper.ToNormalizedCenterY(bottom, 0), 2);

        // stored Y=396 → normalized 0.5
        var mid = PdfRect.FromLtrb(0, 395, 10, 397);
        Assert.Equal(0.5, mapper.ToNormalizedCenterY(mid, 0), 2);
    }

    [Fact]
    public void ToNormalizedCenterY_rotation180_matches_rotation0()
    {
        var mapper = new PdfCoordinateMapper(612, 792, 0, 0);

        var r = PdfRect.FromLtrb(100, 200, 110, 210);
        double r0 = mapper.ToNormalizedCenterY(r, 0);
        double r180 = mapper.ToNormalizedCenterY(r, 2);
        // For 180°, render Y = stored Y (same as 0°)
        Assert.Equal(r0, r180, 6);
    }

    [Fact]
    public void ToNormalizedCenterY_rotation90_uses_stored_X()
    {
        // 90°: render size is (u_h, u_w) = (792, 612)
        var mapper = new PdfCoordinateMapper(792, 612, 0, 0);

        // stored X=0 → render Y = _pageH - 0 = 612 → normalized 1
        var left = PdfRect.FromLtrb(0, 0, 10, 10);
        Assert.Equal(1.0, mapper.ToNormalizedCenterY(left, 1), 2);

        // stored X=612 → render Y = 612 - 612 = 0 → normalized 0
        var right = PdfRect.FromLtrb(612, 0, 622, 10);
        Assert.Equal(0.0, mapper.ToNormalizedCenterY(right, 1), 2);

        // stored X=306 → render Y = 612 - 306 = 306 → normalized 0.5
        var mid = PdfRect.FromLtrb(306, 0, 316, 10);
        Assert.Equal(0.5, mapper.ToNormalizedCenterY(mid, 1), 2);
    }

    [Fact]
    public void ToNormalizedCenterY_rotation270_uses_stored_X_direct()
    {
        // 270°: render size is (u_h, u_w) = (792, 612)
        var mapper = new PdfCoordinateMapper(792, 612, 0, 0);

        // stored X=0 → render Y = 0 → normalized 0
        var left = PdfRect.FromLtrb(0, 0, 10, 10);
        Assert.Equal(0.0, mapper.ToNormalizedCenterY(left, 3), 2);

        // stored X=612 → render Y = 612 → normalized 1
        var right = PdfRect.FromLtrb(612, 0, 622, 10);
        Assert.Equal(1.0, mapper.ToNormalizedCenterY(right, 3), 2);

        // stored X=306 → render Y = 306 → normalized 0.5
        var mid = PdfRect.FromLtrb(306, 0, 316, 10);
        Assert.Equal(0.5, mapper.ToNormalizedCenterY(mid, 3), 2);
    }

    [Fact]
    public void ToRenderX_maps_stored_to_pixels()
    {
        double result = _mapper.ToRenderX(306);
        double expected = 306 * (1275.0 / 612.0);
        Assert.Equal(expected, result, 3);
    }

    [Fact]
    public void ToRenderY_flips_axis()
    {
        double result = _mapper.ToRenderY(0);
        Assert.Equal(1650.0, result, 3);
    }

    [Fact]
    public void ToRenderRect_matches_original_GetHighlightRects()
    {
        var item = new PageRenderItem
        {
            PageNumber = 1,
            ImagePixelWidth = 1275,
            ImagePixelHeight = 1650,
            PdfPageWidth = 612,
            PdfPageHeight = 792,
        };
        var positions = new List<WordPosition> { new(1, 10, 20, 50, 60, null) };
        var expected = item.GetHighlightRects(positions);

        var pdfRect = PdfRect.FromLtrb(10, 20, 50, 60);
        var actual = _mapper.ToRenderRect(pdfRect);

        Assert.Equal(expected[0].X, actual.X, 3);
        Assert.Equal(expected[0].Y, actual.Y, 3);
        Assert.Equal(expected[0].Width, actual.Width, 3);
        Assert.Equal(expected[0].Height, actual.Height, 3);
    }

    [Fact]
    public void ToLayoutRect_scales_to_layout_space()
    {
        var r = PdfRect.FromLtrb(10, 20, 50, 60);
        var actual = _mapper.ToLayoutRect(r, 800, 1000);

        double scaleX = 800.0 / 612.0;
        double scaleY = 1000.0 / 792.0;
        Assert.Equal(10 * scaleX, actual.X, 3);
        Assert.Equal((792 - 60) * scaleY, actual.Y, 3);
        Assert.Equal(40 * scaleX, actual.Width, 3);
        Assert.Equal(40 * scaleY, actual.Height, 3);
    }

    [Fact]
    public void ToLayoutRect_enforces_minimum_size()
    {
        var r = PdfRect.FromLtrb(10, 20, 10.1, 20.1);
        var actual = _mapper.ToLayoutRect(r, 800, 1000);

        Assert.True(actual.Width >= 8);
        Assert.True(actual.Height >= 8);
    }
}
