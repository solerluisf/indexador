using System.Collections.ObjectModel;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using PdfExplorer.Models;
using PdfExplorer.Services;
using PdfExplorer.ViewModels;

namespace PdfExplorer.Views;

public partial class SearchTab : Page
{
    private readonly PdfEngine _engine;
    private readonly PdfiumPageRenderer _renderer = new(150);
    private readonly Dictionary<(string, int), PageRenderItem> _globalPageCache = new();
    private readonly Queue<(string, int)> _globalPageCacheOrder = new();
    private readonly object _globalCacheLock = new();
    private const int MaxGlobalCacheEntries = 50;
    private readonly object _renderCacheLock = new();
    private readonly ThumbnailService _thumbService = new();
    private CancellationTokenSource? _thumbCts;
    private const int ThumbnailPreloadCount = 30;
    private int _currentPage;
    private long _totalHits;
    private string _lastQuery = string.Empty;
    private PdfViewState _state = new();
    private uint _selectedCollId;

    private static void Log(string msg)
    {
        Console.Error.WriteLine($"[SearchTab] {msg}");
        LogHelper.Log("SearchTab", msg);
    }

    public SearchTab()
    {
        Log("Constructor start");
        InitializeComponent();
        _engine = App.Engine;
        Loaded += OnLoaded;
        Log("Constructor end, engine=" + (_engine is not null ? "ok" : "null"));
    }

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        CollectionFilter.ItemsSource = _engine.Collections;
        CollectionFilter.SelectedIndex = -1;
    }

    private void OnCollectionFilterChanged(object sender, SelectionChangedEventArgs e)
    {
        if (CollectionFilter.SelectedItem is CollectionInfo coll)
        {
            _selectedCollId = (uint)coll.Id;
            SearchBox.IsEnabled = true;
            SearchButton.IsEnabled = true;
            SearchBox.Focus();
        }
        else
        {
            _selectedCollId = 0;
            SearchBox.IsEnabled = false;
            SearchButton.IsEnabled = false;
        }
    }

    // ── Search ──────────────────────────────────────────────────────

    private async void OnSearchClick(object sender, RoutedEventArgs e)
    {
        Log("OnSearchClick");
        _currentPage = 0;
        ClearViewer();
        await RunSearch();
    }

    private async Task RunSearch()
    {
        var query = SearchBox.Text;
        if (string.IsNullOrWhiteSpace(query)) { Log("RunSearch: empty query"); return; }
        _lastQuery = query;
        Log($"RunSearch: query='{query}', page={_currentPage}");

        try
        {
            var results = _engine.Search(query, limit: 1000, offset: _currentPage * 1000, collId: _selectedCollId);
            _totalHits = results.Total;
            Log($"RunSearch: totalHits={_totalHits}, results count={results.Results.Count}");

            Log("RunSearch: creating ViewModels...");
            var viewModels = results.Results.Select(r => new SearchResultViewModel(r)).ToList();
            Log($"RunSearch: created {viewModels.Count} ViewModels");

            ResultsList.ItemsSource = viewModels;
            Log("RunSearch: ItemsSource assigned");

            // Start thumbnail preloading
            Log("RunSearch: cancelling previous thumbnail CTS");
            _thumbCts?.Cancel();
            _thumbCts = new CancellationTokenSource();
            Log($"RunSearch: starting PreloadThumbnailsAsync with count={Math.Min(viewModels.Count, ThumbnailPreloadCount)}");
            _ = PreloadThumbnailsAsync(viewModels, ThumbnailPreloadCount, _thumbCts.Token);

            var totalPages = _totalHits > 0 ? (int)System.Math.Ceiling(_totalHits / 1000.0) : 0;
            PageInfo.Content = $"{_currentPage + 1} / {totalPages}";
            PrevPage.IsEnabled = _currentPage > 0;
            NextPage.IsEnabled = _currentPage + 1 < totalPages;
            ResultCountLabel.Text = $"{_totalHits} result(s)";
        }
        catch (Exception ex)
        {
            Log($"RunSearch error: {ex}");
            ResultCountLabel.Text = $"Search error: {ex.Message}";
        }
    }

    private async Task PreloadThumbnailsAsync(List<SearchResultViewModel> items, int count, CancellationToken ct)
    {
        Log($"PreloadThumbnailsAsync START: items={items.Count}, count={count}");
        var toLoad = items.Take(count).ToList();
        Log($"PreloadThumbnailsAsync: will process {toLoad.Count} items");

        foreach (var vm in toLoad)
        {
            if (ct.IsCancellationRequested)
            {
                Log("PreloadThumbnailsAsync: cancellation requested, breaking loop");
                break;
            }

            Log($"PreloadThumbnailsAsync: processing item '{vm.FileName}'");
            try
            {
                Log($"PreloadThumbnailsAsync: calling GetThumbnailAsync for '{vm.FileName}'");
                var raw = await _thumbService.GetThumbnailAsync(vm.Path, ct);
                Log($"PreloadThumbnailsAsync: GetThumbnailAsync returned {(raw is null ? "NULL" : $"{raw.Width}x{raw.Height}")} for '{vm.FileName}'");

                if (raw is null || ct.IsCancellationRequested)
                {
                    Log($"PreloadThumbnailsAsync: no raw data for '{vm.FileName}'");
                    continue;
                }

                // Create BitmapSource on UI thread — WPF media objects must be created on STA thread
                Log($"PreloadThumbnailsAsync: dispatching bitmap creation for '{vm.FileName}'");
                await Dispatcher.InvokeAsync(() =>
                {
                    try
                    {
                        var bmp = BitmapSource.Create(
                            raw.Width,
                            raw.Height,
                            96,
                            96,
                            PixelFormats.Bgra32,
                            null,
                            raw.Pixels,
                            raw.Stride);
                        bmp.Freeze();
                        Log($"PreloadThumbnailsAsync: BitmapSource created ({bmp.PixelWidth}x{bmp.PixelHeight})");

                        vm.Thumbnail = bmp;
                        Log($"PreloadThumbnailsAsync: Thumbnail assigned for '{vm.FileName}'");
                    }
                    catch (Exception ex)
                    {
                        Log($"PreloadThumbnailsAsync EXCEPTION creating bitmap for '{vm.FileName}': {ex.GetType().Name}: {ex.Message}");
                    }
                });
            }
            catch (OperationCanceledException)
            {
                Log($"PreloadThumbnailsAsync: OperationCanceledException for '{vm.FileName}', breaking");
                break;
            }
            catch (Exception ex)
            {
                Log($"PreloadThumbnailsAsync EXCEPTION for '{vm.FileName}': {ex.GetType().Name}: {ex.Message}");
                Log($"PreloadThumbnailsAsync EXCEPTION stack: {ex.StackTrace}");
            }
        }

        Log("PreloadThumbnailsAsync END");
    }

    private async void OnNextPage(object sender, RoutedEventArgs e)
    {
        try
        {
            _currentPage++;
            ClearViewer();
            await RunSearch();
        }
        catch (Exception ex)
        {
            Log($"OnNextPage error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private async void OnPrevPage(object sender, RoutedEventArgs e)
    {
        try
        {
            _currentPage--;
            ClearViewer();
            await RunSearch();
        }
        catch (Exception ex)
        {
            Log($"OnPrevPage error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    // ── Document selection + filtered rendering ─────────────────────

    private async void OnResultSelected(object sender, SelectionChangedEventArgs e)
    {
        try
        {

            if (ResultsList.SelectedItem is not SearchResultViewModel result)
            {
                Log("OnResultSelected: no selection");
                return;
            }

            var t0 = DateTime.UtcNow;
            Log($"OnResultSelected: id={result.Id}, path={result.Path}, collId={result.CollectionId}, query='{_lastQuery}'");

            ClearViewer();
            _state.PdfPath = result.Path;
            StatusLabel.Text = System.IO.Path.GetFileName(result.Path);

            // Read the PDF bytes once and pass to both search + render (fixes double-load)
            byte[] pdfBytes;
            try
            {
                pdfBytes = System.IO.File.ReadAllBytes(result.Path);
                Log($"Read PDF file: {pdfBytes.Length} bytes");
            }
            catch (Exception ex)
            {
                Log($"Failed to read PDF file: {ex.Message}");
                StatusLabel.Text = $"Failed to read PDF: {ex.Message}";
                return;
            }

            // Fetch term positions via PDFium text search (case-insensitive)
            try
            {
                _state.Positions = _engine.SearchTextInPdf(
                    pdfBytes,
                    _lastQuery
                );
                var t1 = DateTime.UtcNow;
                Log($"SearchTextInPdf returned {_state.Positions.Count} positions (took {(t1 - t0).TotalMilliseconds:F0}ms)");
            }
            catch (Exception ex)
            {
                Log($"SearchTextInPdf warning: {ex.GetType().Name}: {ex.Message}");
                _state.Positions = new List<WordPosition>();
            }

            // Fallback: try the indexed position store when PDFium finds nothing
            if (_state.Positions.Count == 0 && result.CollectionId.HasValue)
            {
                try
                {
                    Log($"SearchTextInPdf found nothing — trying GetTermPositions from position store");
                    _state.Positions = _engine.GetTermPositions(
                        (uint)result.CollectionId.Value,
                        result.Id,
                        _lastQuery
                    );
                    Log($"GetTermPositions returned {_state.Positions.Count} positions");
                }
                catch (Exception ex)
                {
                    Log($"GetTermPositions warning: {ex.GetType().Name}: {ex.Message}");
                    _state.Positions = new List<WordPosition>();
                }
            }

            if (_state.Positions.Count > 0)
            {
                Log($"First position: page={_state.Positions[0].Page}, x_min={_state.Positions[0].XMin}, word_text={_state.Positions[0].WordText}");
                var lines = new List<string>(_state.Positions.Count + 1);
                lines.Add($"Positions ({_state.Positions.Count}):");
                foreach (var p in _state.Positions)
                {
                    var word = string.IsNullOrWhiteSpace(p.WordText) ? "?" : p.WordText;
                    lines.Add($"  p{p.Page} \"{word}\" ({p.XMin:F1},{p.YMin:F1})-({p.XMax:F1},{p.YMax:F1})");
                }
                WordsField.Text = string.Join("\n", lines);
            }
            else
            {
                WordsField.Text = result.Snippet;
            }

            var tPos = DateTime.UtcNow;

            // Load PDF from the bytes we already read
            try
            {
                Log($"Loading PDF: {result.Path}");
                _renderer.OpenDocument(pdfBytes, result.Path);
                var t2 = DateTime.UtcNow;
                Log($"PDF loaded, page count={_renderer.GetPageCount()} (took {(t2 - tPos).TotalMilliseconds:F0}ms)");
            }
            catch (Exception ex)
            {
                Log($"LoadDocumentAsync error: {ex.GetType().Name}: {ex.Message}");
                StatusLabel.Text = $"Failed to load PDF: {ex.Message}";
                return;
            }

            if (_state.Positions.Count == 0)
            {
                Log("No positions found — showing first page without highlights");
                StatusLabel.Text += " — no highlights";
            }

            var tPdf = DateTime.UtcNow;

            // Determine which pages match (sorted, 0-based)
            _state.MatchingPages = _state.Positions
                .Select(p => p.Page - 1)
                .Where(p => p >= 0)
                .Distinct()
                .OrderBy(p => p)
                .ToList();

            // Fallback: if no matches, show the first page of the PDF
            if (_state.MatchingPages.Count == 0)
            {
                _state.MatchingPages = new List<int> { 0 };
                Log("No matching pages — fallback to page 1");
            }

            Log($"Matching pages ({_state.MatchingPages.Count}): [{string.Join(", ", _state.MatchingPages.Select(p => p + 1))}]");

            // Group positions by page
            _state.PositionsByPage = _state.Positions
                .GroupBy(p => p.Page - 1)
                .ToDictionary(g => g.Key, g => g.ToList());

            _state.TotalMatchPages = _state.MatchingPages.Count;

            // Render only the first match page; remaining pages load on scroll
            _state.CurrentMatchIndex = 0;
            _state.CurrentPositionIndex = -1;
            _state.PageElements = new List<Border?>(_state.MatchingPages.Count);
            for (int i = 0; i < _state.MatchingPages.Count; i++)
                _state.PageElements.Add(null);

            var firstItem = await GetOrRenderPageAsync(_state.MatchingPages[0]);
            var t3 = DateTime.UtcNow;
            Log($"First page rendered (took {(t3 - tPdf).TotalMilliseconds:F0}ms)");
            AddPageToStack(0, firstItem);
            UpdateMatchNav();
            UpdatePositionNav();
            ScrollToMatch(0);

            PageScroller.ScrollChanged += OnPageScroll;
            _state.IsLoadingNextPage = false;

            var tEnd = DateTime.UtcNow;
            Log($"OnResultSelected complete (total {(tEnd - t0).TotalMilliseconds:F0}ms)");
        }
        catch (Exception ex)
        {
            Log($"OnResultSelected UNHANDLED ERROR: {ex.GetType().Name}: {ex.Message}\n{ex.StackTrace}");
            StatusLabel.Text = $"Error: {ex.Message}";
        }
    }

    // ── Lazy rendering ───────────────────────────────────────────────

    private async Task<PageRenderItem> GetOrRenderPageAsync(int pageIdx)
    {
        var cacheKey = (_state.PdfPath, pageIdx);

        // Check global cache first (outside lock for fast path)
        lock (_renderCacheLock)
        {
            if (_globalPageCache.TryGetValue(cacheKey, out var cached))
            {
                _state.PageCache[pageIdx] = cached;
                return cached;
            }

            if (_state.PageCache.TryGetValue(pageIdx, out var cachedLocal))
                return cachedLocal;
        }

        Log($"Rendering page {pageIdx + 1} (0-based={pageIdx})");
        StatusLabel.Text = $"Rendering page {pageIdx + 1}...";

        var pagePositions = _state.PositionsByPage.GetValueOrDefault(pageIdx, new List<WordPosition>());
        try
        {
            var raw = _renderer.RenderPageRaw(pageIdx, pagePositions);
            var item = await Dispatcher.InvokeAsync(() => PdfiumPageRenderer.CreatePageItem(raw, pagePositions));
            Log($"  rendered: image={(item.PageImage is not null ? $"{item.ImagePixelWidth}x{item.ImagePixelHeight}" : "null")}");

            lock (_renderCacheLock)
            {
                _state.PageCache[pageIdx] = item;
            }
            AddToGlobalCache(cacheKey, item);
            return item;
        }
        catch (Exception ex)
        {
            Log($"RenderPage error: {ex.GetType().Name}: {ex.Message}");
            var fallback = new PageRenderItem
            {
                PageNumber = pageIdx + 1,
                ImagePixelWidth = 0,
                PdfPageWidth = 0,
                Positions = pagePositions,
            };
            lock (_renderCacheLock)
            {
                _state.PageCache[pageIdx] = fallback;
            }
            AddToGlobalCache(cacheKey, fallback);
            return fallback;
        }
    }

    private void AddPageToStack(int matchIndex, PageRenderItem item)
    {
        Log($"AddPageToStack: matchIndex={matchIndex}, page={item.PageNumber}, imgSize={item.ImagePixelWidth}x{item.ImagePixelHeight}");

        // Validate state is still valid for this document view
        if (matchIndex < 0 || matchIndex >= _state.PageElements.Count)
        {
            Log($"AddPageToStack: matchIndex={matchIndex} out of range (PageElements.Count={_state.PageElements.Count}) — stale state, discarding");
            return;
        }

        if (_state.PageElements[matchIndex] is not null)
        {
            Log($"AddPageToStack: matchIndex={matchIndex} already rendered — skipping");
            return;
        }

        var image = new Image
        {
            Source = item.PageImage,
            Stretch = Stretch.Uniform,
            HorizontalAlignment = HorizontalAlignment.Left,
        };

        if (item.PageImage is null && item.ImagePixelWidth > 0)
            image.Source = null;

        var border = new Border
        {
            BorderBrush = Brushes.Silver,
            BorderThickness = new Thickness(1),
            Margin = new Thickness(0, 0, 0, 10),
            Padding = new Thickness(5),
        };

        var header = new TextBlock
        {
            Text = item.PageImage is null
                ? $"{item.PageHeader} — render failed"
                : item.PageHeader,
            FontWeight = FontWeights.SemiBold,
            Margin = new Thickness(0, 0, 0, 4),
        };

        var stack = new StackPanel();
        stack.Children.Add(header);
        stack.Children.Add(image);
        border.Child = stack;
        PageStack.Children.Add(border);
        _state.PageElements[matchIndex] = border;

        Log($"AddPageToStack: matchIndex={matchIndex}, page={item.PageNumber}");
    }

    // ── Match navigation ────────────────────────────────────────────

    private async void OnPrevMatch(object sender, RoutedEventArgs e)
    {
        try
        {
            if (_state.CurrentMatchIndex <= 0) return;
            var prevIdx = _state.CurrentMatchIndex - 1;
            _state.CurrentMatchIndex = prevIdx;

            if (_state.PageElements[prevIdx] is null)
            {
                var item = await GetOrRenderPageAsync(_state.MatchingPages[prevIdx]);
                AddPageToStack(prevIdx, item);
            }
            ScrollToMatch(prevIdx);
        }
        catch (Exception ex)
        {
            Log($"OnPrevMatch error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private async void OnNextMatch(object sender, RoutedEventArgs e)
    {
        try
        {
            if (_state.CurrentMatchIndex >= _state.TotalMatchPages - 1) return;
            var nextIdx = _state.CurrentMatchIndex + 1;
            _state.CurrentMatchIndex = nextIdx;

            if (_state.PageElements[nextIdx] is null)
            {
                var item = await GetOrRenderPageAsync(_state.MatchingPages[nextIdx]);
                AddPageToStack(nextIdx, item);
            }
            ScrollToMatch(nextIdx);
        }
        catch (Exception ex)
        {
            Log($"OnNextMatch error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private void ScrollToMatch(int index)
    {
        Log($"ScrollToMatch({index})");
        if (index < 0 || index >= _state.MatchingPages.Count) return;
        if (index >= _state.PageElements.Count)
        {
            Log($"ScrollToMatch: index={index} >= _state.PageElements.Count={_state.PageElements.Count}");
            return;
        }
        _state.CurrentMatchIndex = index;

        var element = _state.PageElements[index];
        element?.BringIntoView();

        UpdateMatchNav();
    }

    private void UpdateMatchNav()
    {
        MatchInfo.Content = _state.TotalMatchPages > 0
            ? $"Match page {_state.CurrentMatchIndex + 1} of {_state.TotalMatchPages}"
            : "0 matches";
        PrevMatch.IsEnabled = _state.CurrentMatchIndex > 0;
        NextMatch.IsEnabled = _state.CurrentMatchIndex < _state.TotalMatchPages - 1;
    }

    private async void OnPageScroll(object sender, ScrollChangedEventArgs e)
    {
        try
        {
            if (_state.IsLoadingNextPage) return;

            var nextIdx = _state.PageElements.FindIndex(b => b is null);
            if (nextIdx < 0 || nextIdx >= _state.MatchingPages.Count) return;

            // Load next page when close to the bottom of the rendered content
            var remaining = PageScroller.ScrollableHeight - PageScroller.VerticalOffset;
            var threshold = Math.Max(200, PageScroller.ViewportHeight * 0.5);
            if (remaining > threshold) return;

            _state.IsLoadingNextPage = true;
            try
            {
                var item = await GetOrRenderPageAsync(_state.MatchingPages[nextIdx]);
                AddPageToStack(nextIdx, item);
            }
            finally
            {
                _state.IsLoadingNextPage = false;
            }
        }
        catch (Exception ex)
        {
            Log($"OnPageScroll error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    // ── Position (individual match) navigation ─────────────────────

    private void UpdatePositionNav()
    {
        var count = _state.Positions.Count;
        PositionInfo.Content = count > 0 && _state.CurrentPositionIndex >= 0
            ? $"{_state.CurrentPositionIndex + 1} / {count}"
            : "0 / 0";
        PrevPosition.IsEnabled = count > 0 && _state.CurrentPositionIndex > 0;
        NextPosition.IsEnabled = count > 0 && _state.CurrentPositionIndex < count - 1;
    }

    private async void OnPrevPosition(object sender, RoutedEventArgs e)
    {
        try
        {
            if (_state.Positions.Count == 0 || _state.CurrentPositionIndex <= 0) return;
            _state.CurrentPositionIndex--;
            await NavigateToPosition(_state.CurrentPositionIndex);
        }
        catch (Exception ex)
        {
            Log($"OnPrevPosition error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private async void OnNextPosition(object sender, RoutedEventArgs e)
    {
        try
        {
            if (_state.Positions.Count == 0 || _state.CurrentPositionIndex >= _state.Positions.Count - 1) return;
            _state.CurrentPositionIndex++;
            await NavigateToPosition(_state.CurrentPositionIndex);
        }
        catch (Exception ex)
        {
            Log($"OnNextPosition error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private async Task NavigateToPosition(int posIdx)
    {
        if (posIdx < 0 || posIdx >= _state.Positions.Count)
        {
            Log($"NavigateToPosition: invalid posIdx={posIdx} (count={_state.Positions.Count})");
            return;
        }

        var pos = _state.Positions[posIdx];
        var pageIdx = pos.Page - 1;

        var matchIdx = _state.MatchingPages.IndexOf(pageIdx);
        if (matchIdx < 0 || matchIdx >= _state.PageElements.Count)
        {
            Log($"NavigateToPosition: matchIdx={matchIdx} out of range (pages={_state.PageElements.Count})");
            return;
        }

        // Ensure the page is loaded
        if (_state.PageElements[matchIdx] is null)
        {
            var item = await GetOrRenderPageAsync(pageIdx);
            AddPageToStack(matchIdx, item);
        }

        _state.CurrentMatchIndex = matchIdx;
        _state.PageElements[matchIdx]?.BringIntoView();
        UpdateMatchNav();

        UpdatePositionNav();
    }

    // ── Helpers ─────────────────────────────────────────────────────

    private void AddToGlobalCache((string, int) key, PageRenderItem item)
    {
        lock (_globalCacheLock)
        {
            if (!_globalPageCache.ContainsKey(key))
            {
                _globalPageCacheOrder.Enqueue(key);
                while (_globalPageCacheOrder.Count > MaxGlobalCacheEntries)
                {
                    var oldest = _globalPageCacheOrder.Dequeue();
                    _globalPageCache.Remove(oldest);
                }
            }
            _globalPageCache[key] = item;
        }
    }

    private void ClearViewer()
    {
        PageScroller.ScrollChanged -= OnPageScroll;
        PageStack.Children.Clear();
        _state = new PdfViewState();
        _state.CurrentPositionIndex = -1;

        // Clear global cache too (fixes stale entries from previous documents)
        lock (_globalCacheLock)
        {
            _globalPageCache.Clear();
            _globalPageCacheOrder.Clear();
        }

        PageScroller.HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled;
        MatchInfo.Content = "0 matches";
        PrevMatch.IsEnabled = false;
        NextMatch.IsEnabled = false;
        PositionInfo.Content = "0 / 0";
        PrevPosition.IsEnabled = false;
        NextPosition.IsEnabled = false;
        WordsField.Text = "";

        // Close the native PDF handle to avoid use-after-free when switching documents
        try
        {
            _renderer.CloseDocument();
            Log("ClearViewer: renderer document closed");
        }
        catch (Exception ex)
        {
            Log($"ClearViewer: error closing renderer document: {ex.Message}");
        }
    }
}
