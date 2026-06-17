using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using PdfExplorer.Services;

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
        var mapper = new PdfCoordinateMapper(PdfPageWidth, PdfPageHeight, ImagePixelWidth, ImagePixelHeight);
        var rects = new List<Rect>(positions.Count);
        foreach (var p in positions)
        {
            var pdfRect = PdfRect.FromLtrb(p.XMin, p.YMin, p.XMax, p.YMax);
            rects.Add(mapper.ToRenderRect(pdfRect));
        }
        return rects;
    }
}
