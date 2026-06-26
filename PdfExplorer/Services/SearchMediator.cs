using System.Collections.ObjectModel;
using System.Diagnostics;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using PdfExplorer.ViewModels;

namespace PdfExplorer.Services;

public sealed class SearchMediator : ISearchMediator, IDisposable
{
    private readonly PdfEngine _engine;
    private readonly ThumbnailService _thumbService;
    private readonly SemaphoreSlim _searchLock = new(1, 1);
    private readonly object _thumbLock = new();
    private CancellationTokenSource? _thumbCts;
    private int _currentPage;
    private long _totalHits;
    private string _lastQuery = string.Empty;
    private bool _isSearching;
    private bool _disposed;
    private const int ThumbnailPreloadCount = 30;

    public event EventHandler<SearchResultsEventArgs>? SearchCompleted;
    public event EventHandler<SearchErrorEventArgs>? SearchFailed;
    public event EventHandler<bool>? IsSearchingChanged;
    public event EventHandler<EventArgs>? PageChanged;

    public int CurrentPage => _currentPage;
    public int TotalPages => _totalHits > 0 ? (int)System.Math.Ceiling(_totalHits / 1000.0) : 0;
    public long TotalHits => _totalHits;
    public string LastQuery => _lastQuery;
    public bool IsSearching => _isSearching;

    public SearchMediator(PdfEngine engine, ThumbnailService thumbService)
    {
        _engine = engine ?? throw new ArgumentNullException(nameof(engine));
        _thumbService = thumbService ?? throw new ArgumentNullException(nameof(thumbService));
    }

    public async Task SearchAsync(string query, uint? collId)
    {
        if (!await _searchLock.WaitAsync(0))
            return;

        try
        {
            SetIsSearching(true);
            _currentPage = 0;
            await ExecuteSearch(query, collId);
        }
        finally
        {
            SetIsSearching(false);
            _searchLock.Release();
        }
    }

    public async Task NextPageAsync(uint? collId)
    {
        if (!await _searchLock.WaitAsync(0))
            return;

        try
        {
            SetIsSearching(true);
            _currentPage++;
            await ExecuteSearch(_lastQuery, collId);
        }
        finally
        {
            SetIsSearching(false);
            _searchLock.Release();
        }
    }

    public async Task PrevPageAsync(uint? collId)
    {
        if (!await _searchLock.WaitAsync(0))
            return;

        try
        {
            SetIsSearching(true);
            _currentPage--;
            await ExecuteSearch(_lastQuery, collId);
        }
        finally
        {
            SetIsSearching(false);
            _searchLock.Release();
        }
    }

    private async Task ExecuteSearch(string query, uint? collId)
    {
        if (string.IsNullOrWhiteSpace(query))
            return;

        _lastQuery = query;

        try
        {
            var sw = Stopwatch.StartNew();
            var results = await Task.Run(() => _engine.Search(query, limit: 1000, offset: _currentPage * 1000, collId: collId));
            _totalHits = results.Total;
            sw.Stop();
            Log($"Search query: {sw.Elapsed.TotalMilliseconds:F0}ms, hits={results.Total}");

            sw.Restart();
            var viewModels = results.Results.Select(r => new SearchResultViewModel(r)).ToList();
            sw.Stop();
            Log($"ViewModel creation: {sw.Elapsed.TotalMilliseconds:F0}ms, count={viewModels.Count}");

            CancelThumbnails();
            CancellationTokenSource cts;
            lock (_thumbLock)
            {
                _thumbCts = new CancellationTokenSource();
                cts = _thumbCts;
            }
            var ct = cts.Token;
            _ = PreloadThumbnailsAsync(viewModels, ThumbnailPreloadCount, ct)
                .ContinueWith(t =>
                {
                    if (t.Exception is null)
                        _ = LoadRemainingThumbnailsAsync(viewModels, ThumbnailPreloadCount, ct);
                }, TaskContinuationOptions.NotOnCanceled);

            SearchCompleted?.Invoke(this, new SearchResultsEventArgs
            {
                Results = viewModels,
                Query = query,
                CurrentPage = _currentPage,
                TotalPages = TotalPages,
                TotalHits = _totalHits,
            });
        }
        catch (Exception ex)
        {
            SearchFailed?.Invoke(this, new SearchErrorEventArgs { Error = ex.Message });
        }
    }

    public void ResetPage()
    {
        _currentPage = 0;
    }

    public void CancelThumbnails()
    {
        lock (_thumbLock)
        {
            _thumbCts?.Cancel();
            _thumbCts?.Dispose();
            _thumbCts = null;
        }
    }

    public void RetryPendingThumbnails(IReadOnlyList<SearchResultViewModel> items)
    {
        if (items is null || items.Count == 0)
            return;

        CancellationToken ct;
        lock (_thumbLock)
        {
            if (_thumbCts is null || _thumbCts.IsCancellationRequested)
            {
                _thumbCts?.Dispose();
                _thumbCts = new CancellationTokenSource();
            }
            ct = _thumbCts.Token;
        }

        _ = Task.Run(async () =>
        {
            foreach (var vm in items)
            {
                if (ct.IsCancellationRequested || vm.Thumbnail is not null)
                    continue;

                try
                {
                    var raw = await _thumbService.GetThumbnailAsync(vm.Path, ct);
                    if (raw is null || ct.IsCancellationRequested)
                        continue;

                    var bmp = BitmapSource.Create(
                        raw.Width, raw.Height, 96, 96,
                        PixelFormats.Bgra32, null, raw.Pixels, raw.Stride);
                    bmp.Freeze();
                    vm.Thumbnail = bmp;
                }
                catch (OperationCanceledException) { }
                catch (Exception) { }
            }
        });
    }

    private void SetIsSearching(bool value)
    {
        if (_isSearching != value)
        {
            _isSearching = value;
            IsSearchingChanged?.Invoke(this, value);
        }
    }

    private async Task PreloadThumbnailsAsync(List<SearchResultViewModel> items, int count, CancellationToken ct)
    {
        var toLoad = items.Take(count).ToList();

        var rawResults = (await Task.WhenAll(toLoad.Select(async vm =>
        {
            if (ct.IsCancellationRequested) return (vm, raw: (ThumbnailRawResult?)null);

            try
            {
                var raw = await _thumbService.GetThumbnailAsync(vm.Path, ct);
                return (vm, raw);
            }
            catch (OperationCanceledException) { return (vm, null); }
            catch (Exception)
            {
                return (vm, null);
            }
        })))
        .Where(r => r.raw is not null)
        .ToList();

        if (rawResults.Count == 0)
            return;

        foreach (var (vm, raw) in rawResults)
        {
            if (raw is null) continue;
            var bmp = BitmapSource.Create(
                raw.Width, raw.Height, 96, 96,
                PixelFormats.Bgra32, null, raw.Pixels, raw.Stride);
            bmp.Freeze();
            vm.Thumbnail = bmp;
        }
    }

    private async Task LoadRemainingThumbnailsAsync(List<SearchResultViewModel> items, int skipCount, CancellationToken ct)
    {
        var remaining = items.Skip(skipCount).ToList();

        await Task.WhenAll(remaining.Select<SearchResultViewModel, Task>(async vm =>
        {
            if (vm.Thumbnail is not null) return;

            try
            {
                var raw = await _thumbService.GetThumbnailAsync(vm.Path, ct);
                if (raw is null) return;

                var bmp = BitmapSource.Create(
                    raw.Width, raw.Height, 96, 96,
                    PixelFormats.Bgra32, null, raw.Pixels, raw.Stride);
                bmp.Freeze();
                vm.Thumbnail = bmp;
            }
            catch (OperationCanceledException) { }
            catch (Exception) { }
        }));
    }

    private static void Log(string msg)
    {
        Console.Error.WriteLine($"[SearchMediator] {msg}");
        LogHelper.Log("SearchMediator", msg);
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        CancelThumbnails();
        _searchLock.Dispose();
    }
}
