namespace PdfExplorer.Models;

public sealed class PdfPageViewModel
{
    public int PageIndex { get; init; }
    public int MatchIndex { get; init; }
    public double WidthPts { get; init; }
    public double HeightPts { get; init; }
    public List<WordPosition> Positions { get; set; } = new();
}
