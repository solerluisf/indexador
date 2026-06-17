using System.Windows;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

public sealed class PdfCoordinateMapper
{
    private readonly double _pageW;
    private readonly double _pageH;
    private readonly double _srcW;
    private readonly double _srcH;

    public PdfCoordinateMapper(double pageW, double pageH, double srcW, double srcH)
    {
        _pageW = pageW;
        _pageH = pageH;
        _srcW = srcW;
        _srcH = srcH;
    }

    public double ToRenderX(double storedX)
        => storedX * (_srcW / _pageW);

    public double ToRenderY(double storedY)
        => (_pageH - storedY) * (_srcH / _pageH);

    public Rect ToRenderRect(PdfRect r) => new(
        ToRenderX(r.TopLeft.X),
        ToRenderY(r.BottomRight.Y),
        Math.Max(8, r.Width * (_srcW / _pageW)),
        Math.Max(8, r.Height * (_srcH / _pageH)));

    public double ToLayoutX(double storedX, double layoutW)
        => storedX * (layoutW / _pageW);

    public double ToLayoutY(double storedY, double layoutH)
        => (_pageH - storedY) * (layoutH / _pageH);

    public Rect ToLayoutRect(PdfRect r, double layoutW, double layoutH) => new(
        ToLayoutX(r.TopLeft.X, layoutW),
        ToLayoutY(r.BottomRight.Y, layoutH),
        Math.Max(8, r.Width * (layoutW / _pageW)),
        Math.Max(8, r.Height * (layoutH / _pageH)));

    /// <param name="rotation">Page rotation: 0=0°, 1=90°, 2=180°, 3=270° CW.</param>
    public double ToNormalizedCenterY(PdfRect r, int rotation = 0)
    {
        double cx = r.Center.X;
        double cy = r.Center.Y;

        double renderY = rotation switch
        {
            1 => _pageH - cx,  // 90°: stored X → render Y (inverted)
            2 => cy,            // 180°: stored Y → render Y (no flip)
            3 => cx,            // 270°: stored X → render Y (no flip)
            _ => cy,            // 0°: stored Y → render Y (identity)
        };

        return Math.Clamp(renderY / _pageH, 0.0, 1.0);
    }
}
