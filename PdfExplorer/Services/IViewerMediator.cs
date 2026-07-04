using System.Threading;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

public sealed class ViewerStateChangedEventArgs : EventArgs
{
    public required string PdfPath { get; init; }
    public required IReadOnlyList<WordPosition> Positions { get; init; }
    public required IReadOnlyList<int> MatchingPages { get; init; }
}

public interface IViewerMediator
{
    event EventHandler<ViewerStateChangedEventArgs>? StateChanged;
    event EventHandler<EventArgs>? ViewModelsBuilt;
    event EventHandler<EventArgs>? Cleared;

    PdfiumPageRenderer Renderer { get; }
    string PdfPath { get; }
    CancellationToken CurrentRenderToken { get; }
    IReadOnlyList<WordPosition> Positions { get; }
    IReadOnlyList<int> MatchingPages { get; }
    IReadOnlyDictionary<int, List<WordPosition>> PositionsByPage { get; }
    string PositionsDebugText { get; }

    void OpenDocument(byte[] pdfBytes, string path);
    void CloseDocument();

    Task<List<WordPosition>> FetchPositionsAsync(PdfEngine engine, uint collId, long docId, List<string> matchedTerms, List<List<string>> phraseGroups);
    void SetPositions(List<WordPosition> positions, INavigationMediator navMediator, List<string>? matchedTerms = null, bool isBooleanMode = false);

    void BuildPageViewModels();

    Task<PageRenderItem> GetOrRenderPageAsync(int pageIdx, List<WordPosition> pagePositions, CancellationToken ct);

    void InvalidateAllPages();

    void Clear();
}
