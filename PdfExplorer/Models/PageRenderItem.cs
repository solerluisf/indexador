using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;

namespace PdfExplorer.Models;

public sealed class PageRenderItem
{
    public int PageNumber { get; init; }
    public BitmapSource? PageImage { get; set; }
    public int ImagePixelWidth { get; set; }
    public int ImagePixelHeight { get; set; }
    public double PdfPageWidth { get; init; }
    public double PdfPageHeight { get; init; }
    public string PageHeader => $"Page {PageNumber}";
    public List<WordPosition> Positions { get; init; } = new();

    public List<Rect> GetHighlightRects(List<WordPosition> positions)
    {
        if (ImagePixelWidth <= 0) return new List<Rect>();
        var scaleX = ImagePixelWidth / PdfPageWidth;
        var scaleY = ImagePixelHeight / PdfPageHeight;
        var rects = new List<Rect>(positions.Count);
        foreach (var p in positions)
        {
            var x = p.XMin * scaleX;
            var y = (PdfPageHeight - p.YMax) * scaleY;
            var w = Math.Max(8, (p.XMax - p.XMin) * scaleX);
            var h = Math.Max(8, (p.YMax - p.YMin) * scaleY);
            rects.Add(new Rect(x, y, w, h));
        }
        return rects;
    }
}
