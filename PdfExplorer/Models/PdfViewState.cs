using System.Collections.ObjectModel;

namespace PdfExplorer.Models;

/// <summary>
/// Encapsulates all mutable state for the PDF viewer panel.
/// Reduces the number of instance fields in SearchTab from 21 to 1,
/// and makes it possible to reset the entire viewer atomically.
/// </summary>
internal sealed class PdfViewState
{
    public string PdfPath { get; set; } = string.Empty;
    public List<WordPosition> Positions { get; set; } = new();
    public List<int> MatchingPages { get; set; } = new();
    public Dictionary<int, List<WordPosition>> PositionsByPage { get; set; } = new();
    public Dictionary<int, PageRenderItem> PageCache { get; set; } = new();
    public int CurrentMatchIndex { get; set; }
    public int TotalMatchPages { get; set; }
    public int CurrentPositionIndex { get; set; } = -1;

    // Virtualized page view models
    public ObservableCollection<PdfPageViewModel>? PageViewModels { get; set; }
    public double[] PageOffsets { get; set; } = Array.Empty<double>();
}
