using PdfExplorer.ViewModels;

namespace PdfExplorer.Services;

public sealed class SearchResultsEventArgs : EventArgs
{
    public required List<SearchResultViewModel> Results { get; init; }
    public required string Query { get; init; }
    public required int CurrentPage { get; init; }
    public required int TotalPages { get; init; }
    public required long TotalHits { get; init; }
}

public sealed class SearchErrorEventArgs : EventArgs
{
    public required string Error { get; init; }
}

public interface ISearchMediator
{
    event EventHandler<SearchResultsEventArgs>? SearchCompleted;
    event EventHandler<SearchErrorEventArgs>? SearchFailed;
    event EventHandler<bool>? IsSearchingChanged;
    event EventHandler<EventArgs>? PageChanged;

    int CurrentPage { get; }
    int TotalPages { get; }
    long TotalHits { get; }
    string LastQuery { get; }
    bool IsSearching { get; }

    Task SearchAsync(string query, uint? collId);
    Task NextPageAsync(uint? collId);
    Task PrevPageAsync(uint? collId);
    void ResetPage();
    void CancelThumbnails();
}
