using System.Buffers;
using System.Collections.Concurrent;
using System.Collections.ObjectModel;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Threading;
using PdfExplorer.Models;
using PdfExplorer.Services;
using PdfExplorer.ViewModels;

namespace PdfExplorer.Views;

public partial class SearchTab : Page, IPdfRenderingService
{
    private readonly PdfEngine _engine;
    private readonly PdfiumPageRenderer _renderer = new(150);
    private readonly Dictionary<(string, int), PageRenderItem> _globalPageCache = new();
    private readonly Queue<(string, int)> _globalPageCacheOrder = new();
    private readonly object _globalCacheLock = new();
    private const int MaxGlobalCacheEntries = 50;
    private readonly object _renderCacheLock = new();
    private readonly ConcurrentDictionary<int, Task<PageRenderItem>> _pendingRenders = new();
    private readonly ThumbnailService _thumbService = new();
    private CancellationTokenSource? _thumbCts;
    private CancellationTokenSource? _selectionCts;
    private const int ThumbnailPreloadCount = 30;
    private readonly SemaphoreSlim _searchLock = new(1, 1);
    private int _currentPage;
    private long _totalHits;
    private string _lastQuery = string.Empty;
    private PdfViewState _state = new();
    private uint? _selectedCollId;
    private bool _isLoading;
    private bool _isNavigating;

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
        _currentPage = 0;
        if (CollectionFilter.SelectedItem is CollectionInfo coll)
        {
            _selectedCollId = (uint)coll.Id;
            SearchBox.IsEnabled = true;
            SearchButton.IsEnabled = true;
            SearchBox.Focus();
        }
        else
        {
            _selectedCollId = null;
            SearchBox.IsEnabled = false;
            SearchButton.IsEnabled = false;
        }
    }

    // ── Search ──────────────────────────────────────────────────────

    private void OnSearchBoxKeyDown(object sender, KeyEventArgs e)
    {
        if (e.Key == Key.Enter)
        {
            OnSearchClick(sender, e);
        }
    }

    private async void OnSearchClick(object sender, RoutedEventArgs e)
    {
        if (!await _searchLock.WaitAsync(0))
        {
            Log("OnSearchClick: skipped (search already in progress)");
            return;
        }
        try
        {
            Log("OnSearchClick");
            _currentPage = 0;
            ClearViewer();
            await RunSearch();
        }
        catch (Exception ex)
        {
            Log($"OnSearchClick error: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            _searchLock.Release();
        }
    }

    private async Task RunSearch()
    {
        var query = SearchBox.Text;
        if (string.IsNullOrWhiteSpace(query)) { Log("RunSearch: empty query"); return; }
        _lastQuery = query;
        Log($"RunSearch: query='{query}', page={_currentPage}");

        try
        {
            var results = await Task.Run(() => _engine.Search(query, limit: 1000, offset: _currentPage * 1000, collId: _selectedCollId));
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
            var thumbCt = _thumbCts.Token;
            Log($"RunSearch: starting PreloadThumbnailsAsync with count={Math.Min(viewModels.Count, ThumbnailPreloadCount)}");
            _ = PreloadThumbnailsAsync(viewModels, ThumbnailPreloadCount, thumbCt)
                .ContinueWith(t =>
                {
                    if (t.Exception is null)
                        _ = LoadRemainingThumbnailsAsync(viewModels, ThumbnailPreloadCount, thumbCt)
                            .ContinueWith(t2 => Log($"LoadRemaining faulted: {t2.Exception?.InnerException?.Message}"),
                                TaskContinuationOptions.OnlyOnFaulted);
                    else
                        Log($"PreloadThumbnails faulted: {t.Exception?.InnerException?.Message}");
                }, TaskContinuationOptions.NotOnCanceled);

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

        // Collect raw thumbnail data from background threads first (no UI thread touch)
        var rawResults = new List<(SearchResultViewModel vm, ThumbnailRawResult? raw)>(toLoad.Count);
        foreach (var vm in toLoad)
        {
            if (ct.IsCancellationRequested) break;

            try
            {
                var raw = await _thumbService.GetThumbnailAsync(vm.Path, ct);
                if (raw is not null && !ct.IsCancellationRequested)
                    rawResults.Add((vm, raw));
            }
            catch (OperationCanceledException) { break; }
            catch (Exception ex)
            {
                Log($"PreloadThumbnailsAsync error for '{vm.FileName}': {ex.Message}");
            }
        }

        if (rawResults.Count == 0 || ct.IsCancellationRequested)
        {
            Log("PreloadThumbnailsAsync: no thumbnails to create");
            return;
        }

        // Single batch dispatch to UI thread — all BitmapSource creations in one InvokeAsync
        await Dispatcher.InvokeAsync(() =>
        {
            ct.ThrowIfCancellationRequested();

            if (ResultsList.ItemsSource is not System.Collections.IList list)
            {
                Log("PreloadThumbnailsAsync: ItemsSource is gone");
                return;
            }

            foreach (var (vm, raw) in rawResults)
            {
                if (!list.Contains(vm)) continue;

                var bmp = BitmapSource.Create(
                    raw.Width, raw.Height, 96, 96,
                    PixelFormats.Bgra32, null, raw.Pixels, raw.Stride);
                bmp.Freeze();
                vm.Thumbnail = bmp;
            }
        });

        Log("PreloadThumbnailsAsync END");
    }

    private async Task LoadRemainingThumbnailsAsync(List<SearchResultViewModel> items, int skipCount, CancellationToken ct)
    {
        var remaining = items.Skip(skipCount).ToList();
        Log($"LoadRemainingThumbnailsAsync START: {remaining.Count} items to load (skipped first {skipCount})");

        foreach (var vm in remaining)
        {
            if (ct.IsCancellationRequested) break;
            if (vm.Thumbnail is not null) continue;

            try
            {
                var raw = await _thumbService.GetThumbnailAsync(vm.Path, ct);
                if (raw is null || ct.IsCancellationRequested) continue;

                await Dispatcher.InvokeAsync(() =>
                {
                    ct.ThrowIfCancellationRequested();

                    if (ResultsList.ItemsSource is not System.Collections.IList list) return;
                    if (!list.Contains(vm)) return;

                    var bmp = BitmapSource.Create(
                        raw.Width, raw.Height, 96, 96,
                        PixelFormats.Bgra32, null, raw.Pixels, raw.Stride);
                    bmp.Freeze();
                    vm.Thumbnail = bmp;
                });
            }
            catch (OperationCanceledException) { break; }
            catch (Exception ex)
            {
                Log($"LoadRemainingThumbnailsAsync error for '{vm.FileName}': {ex.Message}");
            }
        }

        Log("LoadRemainingThumbnailsAsync END");
    }

    private async void OnNextPage(object sender, RoutedEventArgs e)
    {
        if (!await _searchLock.WaitAsync(0))
        {
            Log("OnNextPage: skipped (search already in progress)");
            return;
        }
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
        finally
        {
            _searchLock.Release();
        }
    }

    private async void OnPrevPage(object sender, RoutedEventArgs e)
    {
        if (!await _searchLock.WaitAsync(0))
        {
            Log("OnPrevPage: skipped (search already in progress)");
            return;
        }
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
        finally
        {
            _searchLock.Release();
        }
    }

    // ── Document selection + filtered rendering ─────────────────────

    private async void OnResultSelected(object sender, SelectionChangedEventArgs e)
    {
        try
        {
            if (_isLoading)
            {
                Log("OnResultSelected: skipped (already loading)");
                return;
            }

            if (ResultsList.SelectedItem is not SearchResultViewModel result)
            {
                Log("OnResultSelected: no selection");
                return;
            }

            // Cancel any previous selection rendering and clear state first.
            // ClearViewer must run BEFORE creating the new _selectionCts,
            // otherwise ClearViewer would cancel the token we just created.
            _selectionCts?.Cancel();
            ClearViewer();

            // Physically disable UI to prevent reentrant clicks during async load
            _isLoading = true;
            ResultsList.IsEnabled = false;

            _selectionCts = new CancellationTokenSource();
            var ct = _selectionCts.Token;

            var t0 = DateTime.UtcNow;
            Log($"OnResultSelected: id={result.Id}, path={result.Path}, collId={result.CollectionId}, query='{_lastQuery}'");

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

            // Fetch term positions from indexed position store (SQLite)
            // This is much more efficient than re-extracting the PDF with PDFium
            if (result.CollectionId.HasValue)
            {
                try
                {
                    Log($"Fetching term positions from indexed position store");
                    _state.Positions = await Task.Run(() => _engine.GetTermPositions(
                        (uint)result.CollectionId.Value,
                        result.Id,
                        _lastQuery
                    ));
                    var before = _state.Positions.Count;
                    _state.Positions = _state.Positions
                        .OrderBy(p => p.Page)
                        .ThenBy(p => p.YMax)
                        .DistinctBy(p => (p.Page, p.XMin, p.YMin, p.XMax, p.YMax))
                        .ToList();
                    var dupes = before - _state.Positions.Count;
                    var t1 = DateTime.UtcNow;
                    Log($"GetTermPositions returned {_state.Positions.Count} positions ({dupes} dupes removed, took {(t1 - t0).TotalMilliseconds:F0}ms)");
                    // Diagnostic: dump first 20 positions with page/Y to stderr
                    for (int di = 0; di < Math.Min(20, _state.Positions.Count); di++)
                    {
                        var dp = _state.Positions[di];
                        Log($"  pos[{di}] page={dp.Page} YMin={dp.YMin:F2} YMax={dp.YMax:F2} word='{dp.WordText}'");
                    }
                }
                catch (Exception ex)
                {
                    Log($"GetTermPositions warning: {ex.GetType().Name}: {ex.Message}");
                    _state.Positions = new List<WordPosition>();
                }
            }
            else
            {
                Log("No collection ID available - cannot fetch indexed positions");
                _state.Positions = new List<WordPosition>();
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

            // Build virtualized page view models with pre-calculated heights
            _state.CurrentMatchIndex = 0;
            _state.CurrentPositionIndex = -1;

            if (ct.IsCancellationRequested)
            {
                Log("OnResultSelected: cancelled before build");
                return;
            }

            BuildOrDeferViewModels(ct);

            var tEnd = DateTime.UtcNow;
            Log($"OnResultSelected complete (total {(tEnd - t0).TotalMilliseconds:F0}ms)");
        }
        catch (Exception ex)
        {
            Log($"OnResultSelected UNHANDLED ERROR: {ex.GetType().Name}: {ex.Message}\n{ex.StackTrace}");
            StatusLabel.Text = $"Error: {ex.Message}";
        }
    }

    // ── Lazy rendering (IPdfRenderingService) ───────────────────────

    async Task<PageRenderItem> IPdfRenderingService.GetOrRenderPageAsync(int pageIdx, List<WordPosition> pagePositions)
    {
        // Capture the cancellation token at entry — before GetOrAdd,
        // so that if ClearViewer cancels _selectionCts during an in-flight
        // render, any new request for the same page is cancelled immediately
        // instead of wasting ~100ms on a stale native call.
        var ct = _selectionCts?.Token ?? CancellationToken.None;

        // Coalesce concurrent render requests for the same page.
        // Multiple PdfPageView controls can request the same pageIdx
        // simultaneously during rapid navigation.
        try
        {
            return await _pendingRenders.GetOrAdd(pageIdx, idx => RenderPageInternalAsync(idx, pagePositions, ct));
        }
        finally
        {
            _pendingRenders.TryRemove(pageIdx, out _);
        }
    }

    private async Task<PageRenderItem> RenderPageInternalAsync(int pageIdx, List<WordPosition> pagePositions, CancellationToken ct)
    {
        var capturedState = _state;
        var cacheKey = (capturedState.PdfPath, pageIdx);

        // Check global cache first (outside lock for fast path)
        lock (_renderCacheLock)
        {
            if (_globalPageCache.TryGetValue(cacheKey, out var cached))
            {
                if (ReferenceEquals(_state, capturedState))
                    _state.PageCache[pageIdx] = cached;
                return cached;
            }

            if (capturedState.PageCache.TryGetValue(pageIdx, out var cachedLocal))
                return cachedLocal;
        }

        ct.ThrowIfCancellationRequested();

        Log($"Rendering page {pageIdx + 1} (0-based={pageIdx})");
        StatusLabel.Text = $"Rendering page {pageIdx + 1}...";

        try
        {
            // Get page dimensions first (needed to allocate the buffer)
            var (wPts, hPts) = _renderer.GetPageDimensions(pageIdx);
            int pixW = (int)(wPts * _renderer.TargetDpi / 72.0);
            int pixH = (int)(hPts * _renderer.TargetDpi / 72.0);
            int stride = pixW * 4;

            // Rent buffer from pool and pin it — Rust will render directly into it
            int bufferSize = stride * pixH;
            var buffer = ArrayPool<byte>.Shared.Rent(bufferSize);
            try
            {
                var pin = GCHandle.Alloc(buffer, GCHandleType.Pinned);
                try
                {
                    IntPtr ptr = pin.AddrOfPinnedObject();

                    // Native render on background thread (zero-copy: writes into buffer)
                    await Task.Run(() =>
                    {
                        _renderer.RenderToBuffer(pageIdx, pagePositions,
                            ptr, pixW, pixH, stride,
                            out var _, out var _);
                    }, ct);

                    ct.ThrowIfCancellationRequested();

                    // Create the frozen WPF bitmap on the UI thread
                    var item = await Dispatcher.InvokeAsync(() =>
                    {
                        ct.ThrowIfCancellationRequested();

                        var bitmap = BitmapSource.Create(
                            pixW, pixH, 96, 96,
                            PixelFormats.Bgra32, null,
                            buffer, stride);
                        bitmap.Freeze();

                        return new PageRenderItem
                        {
                            PageNumber = pageIdx + 1,
                            PageImage = bitmap,
                            ImagePixelWidth = pixW,
                            ImagePixelHeight = pixH,
                            PdfPageWidth = wPts,
                            PdfPageHeight = hPts,
                            Positions = pagePositions,
                        };
                    });

                    Log($"  rendered: {pixW}x{pixH}");

                    lock (_renderCacheLock)
                    {
                        if (ReferenceEquals(_state, capturedState))
                            _state.PageCache[pageIdx] = item;
                    }
                    AddToGlobalCache(cacheKey, item);
                    return item;
                }
                finally
                {
                    pin.Free();
                }
            }
            finally
            {
                ArrayPool<byte>.Shared.Return(buffer);
            }
        }
        catch (OperationCanceledException)
        {
            throw;
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
                if (ReferenceEquals(_state, capturedState))
                    _state.PageCache[pageIdx] = fallback;
            }
            AddToGlobalCache(cacheKey, fallback);
            return fallback;
        }
    }

    // ── Match navigation ────────────────────────────────────────────

    private async void OnPrevMatch(object sender, RoutedEventArgs e)
    {
        if (_isLoading) return;
        try
        {
            if (_state.CurrentMatchIndex <= 0) return;
            var prevIdx = _state.CurrentMatchIndex - 1;
            _state.CurrentMatchIndex = prevIdx;
            ScrollToMatch(prevIdx, scrollToTop: true);
        }
        catch (Exception ex)
        {
            Log($"OnPrevMatch error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private async void OnNextMatch(object sender, RoutedEventArgs e)
    {
        if (_isLoading) return;
        try
        {
            if (_state.CurrentMatchIndex >= _state.TotalMatchPages - 1) return;
            var nextIdx = _state.CurrentMatchIndex + 1;
            _state.CurrentMatchIndex = nextIdx;
            ScrollToMatch(nextIdx, scrollToTop: true);
        }
        catch (Exception ex)
        {
            Log($"OnNextMatch error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private void ScrollToMatch(int index, bool scrollToTop = false)
    {
        Log($"ScrollToMatch({index}, scrollToTop={scrollToTop})");
        if (index < 0 || index >= _state.MatchingPages.Count) return;
        _state.CurrentMatchIndex = index;

        int pageIdx = _state.MatchingPages[index];
        int posIdx = _state.Positions.FindIndex(p => p.Page - 1 == pageIdx);
        _state.CurrentPositionIndex = posIdx >= 0 ? posIdx : -1;
        Log($"ScrollToMatch: synced CurrentPositionIndex={_state.CurrentPositionIndex} (pageIdx={pageIdx}, firstPosIdx={posIdx})");

        double targetPx;
        double viewH = PageScroller.ViewportHeight;

        if (scrollToTop)
        {
            PageList.UpdateLayout();
            var container = PageList.ItemContainerGenerator.ContainerFromIndex(index) as FrameworkElement;
            if (container is not null)
            {
                container.UpdateLayout();
                Point containerOrigin = container.TransformToAncestor(PageScroller).Transform(new Point(0, 0));
                targetPx = PageScroller.VerticalOffset + containerOrigin.Y;
            }
            else
            {
                targetPx = AccumulatePageHeightBefore(index);
            }
            targetPx = Math.Max(0, Math.Min(targetPx, PageScroller.ScrollableHeight));
            PageScroller.ScrollToVerticalOffset(targetPx);
            Log($"ScrollToMatch (page top): targetPx={targetPx:F1}");
            UpdateMatchNav();
            UpdatePositionNav();
            return;
        }

        if (posIdx >= 0 && _state.PageViewModels is not null && index < _state.PageViewModels.Count)
        {
            var pos = _state.Positions[posIdx];
            var (wPts, hPts) = _renderer.GetPageDimensions(pageIdx);
            int rotation = _renderer.GetPageRotation(pageIdx);
            var mapper = new PdfCoordinateMapper(wPts, hPts, 0, 0);
            double normalizedY = mapper.ToNormalizedCenterY(PdfRect.FromLtrb(pos.XMin, pos.YMin, pos.XMax, pos.YMax), rotation);

            // Try to use actual WPF layout via PointToScreen
            double wordContentY = 0;
            bool refined = false;

            PageList.UpdateLayout();

            var container = PageList.ItemContainerGenerator.ContainerFromIndex(index) as FrameworkElement;
            if (container is not null)
            {
                container.UpdateLayout();
                var imgControl = FindChild<Image>(container);
                if (imgControl is not null && imgControl.ActualHeight > 0)
                {
                    Rect pdfRealBounds = GetActualImageRect(imgControl);
                if (pdfRealBounds != Rect.Empty)
                {
                    double wordYLocal = pdfRealBounds.Top + normalizedY * pdfRealBounds.Height;
                    Point relativeWord = imgControl.TransformToAncestor(PageScroller).Transform(new Point(0, wordYLocal));
                    wordContentY = PageScroller.VerticalOffset + relativeWord.Y;
                    refined = true;
                }
                }
            }

            if (!refined)
            {
                double availW = LayoutConstants.AvailWidth(PageScroller.ViewportWidth);
                wordContentY = AccumulatePageHeightBefore(index);
                wordContentY += LayoutConstants.WordOffsetWithinItem(
                    availW,
                    _state.PageViewModels[index].ImagePixelWidth,
                    _state.PageViewModels[index].ImagePixelHeight,
                    normalizedY);
            }

            targetPx = wordContentY - viewH / 2;
        }
        else
        {
            targetPx = AccumulatePageHeightBefore(index);
        }

        targetPx = Math.Max(0, Math.Min(targetPx, PageScroller.ScrollableHeight));
        PageScroller.ScrollToVerticalOffset(targetPx);
        Log($"ScrollToMatch: targetPx={targetPx:F1}");
        UpdateMatchNav();
        UpdatePositionNav();
    }

    private double AccumulatePageHeightBefore(int matchIdx)
    {
        if (_state.PageViewModels is null || matchIdx <= 0) return 0;
        double availW = LayoutConstants.AvailWidth(PageScroller.ViewportWidth);
        double total = 0;
        for (int i = 0; i < matchIdx; i++)
        {
            var vm = _state.PageViewModels[i];
            total += LayoutConstants.TotalItemHeight(
                availW, vm.ImagePixelWidth, vm.ImagePixelHeight);
        }
        return total;
    }

    private void UpdateMatchNav()
    {
        MatchInfo.Content = _state.TotalMatchPages > 0
            ? $"Match page {_state.CurrentMatchIndex + 1} of {_state.TotalMatchPages}"
            : "0 matches";
        PrevMatch.IsEnabled = _state.CurrentMatchIndex > 0;
        NextMatch.IsEnabled = _state.CurrentMatchIndex < _state.TotalMatchPages - 1;
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
        Log($"UpdatePositionNav: count={count}, idx={_state.CurrentPositionIndex}, prev={PrevPosition.IsEnabled}, next={NextPosition.IsEnabled}, label='{PositionInfo.Content}'");
    }

    private async void OnPrevPosition(object sender, RoutedEventArgs e)
    {
        if (_isLoading) return;
        try
        {
            if (_isNavigating) { Log("OnPrevPosition: skipped (navigation in progress)"); return; }
            _isNavigating = true;
            if (_state.Positions.Count == 0 || _state.CurrentPositionIndex <= 0) { Log($"OnPrevPosition: guard blocked (count={_state.Positions.Count}, idx={_state.CurrentPositionIndex})"); return; }
            _state.CurrentPositionIndex--;
            Log($"OnPrevPosition: decrementing to {_state.CurrentPositionIndex}");
            await NavigateToPosition(_state.CurrentPositionIndex);
        }
        catch (Exception ex)
        {
            Log($"OnPrevPosition error: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            _isNavigating = false;
        }
    }

    private async void OnNextPosition(object sender, RoutedEventArgs e)
    {
        if (_isLoading) return;
        try
        {
            Log($"OnNextPosition: count={_state.Positions.Count}, idx={_state.CurrentPositionIndex}");
            if (_isNavigating) { Log("OnNextPosition: skipped (navigation in progress)"); return; }
            _isNavigating = true;
            if (_state.Positions.Count == 0 || _state.CurrentPositionIndex >= _state.Positions.Count - 1)
            {
                Log($"OnNextPosition: guard blocked");
                return;
            }
            _state.CurrentPositionIndex++;
            Log($"OnNextPosition: incrementing to {_state.CurrentPositionIndex}");
            await NavigateToPosition(_state.CurrentPositionIndex);
        }
        catch (Exception ex)
        {
            Log($"OnNextPosition error: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            _isNavigating = false;
        }
    }

    private async Task NavigateToPosition(int posIdx)
    {
        Log($"NavigateToPosition({posIdx}) — entering");
        if (_isLoading)
        {
            Log("NavigateToPosition: skipped (loading in progress)");
            return;
        }
        if (posIdx < 0 || posIdx >= _state.Positions.Count)
        {
            Log($"NavigateToPosition: invalid posIdx={posIdx} (count={_state.Positions.Count})");
            return;
        }

        if (PageScroller.ViewportWidth <= 12)
        {
            Log($"NavigateToPosition: ViewportWidth={PageScroller.ViewportWidth} too small, deferring");
            var capturedState = _state;
            EventHandler? handler = null;
            handler = (_, _) =>
            {
                if (!ReferenceEquals(_state, capturedState))
                {
                    PageScroller.LayoutUpdated -= handler;
                    return;
                }
                if (PageScroller.ViewportWidth > 12)
                {
                    PageScroller.LayoutUpdated -= handler;
                    _ = NavigateToPosition(posIdx);
                }
            };
            PageScroller.LayoutUpdated += handler;
            return;
        }

        var pos = _state.Positions[posIdx];
        var pageIdx = pos.Page - 1;
        Log($"NavigateToPosition: page={pos.Page}, word='{pos.WordText}', YMin={pos.YMin:F2}, YMax={pos.YMax:F2}");

        var matchIdx = _state.MatchingPages.IndexOf(pageIdx);
        Log($"NavigateToPosition: matchIdx={matchIdx}, MatchingPages=[{string.Join(",", _state.MatchingPages)}], PageViewModels={(_state.PageViewModels is null ? "null" : _state.PageViewModels.Count.ToString())}");
        if (matchIdx < 0 || _state.PageViewModels is null || matchIdx >= _state.PageViewModels.Count)
        {
            Log($"NavigateToPosition: matchIdx={matchIdx} out of range (pages={_state.PageViewModels?.Count}) — returning early");
            return;
        }

        int prevMatchIdx = _state.CurrentMatchIndex;
        string prevPageStr = prevMatchIdx >= 0 && prevMatchIdx < _state.MatchingPages.Count
            ? (_state.MatchingPages[prevMatchIdx] + 1).ToString()
            : "N/A";
        Log($"NavigateToPosition: page transition {prevMatchIdx}→{matchIdx}, prevPage={prevPageStr}, newPage={pos.Page}");
        _state.CurrentMatchIndex = matchIdx;

        // Get normalized Y position of the word within its page (0 = top, 1 = bottom)
        var (wPts, hPts) = _renderer.GetPageDimensions(pageIdx);
        int rotation = _renderer.GetPageRotation(pageIdx);
        var mapper = new PdfCoordinateMapper(wPts, hPts, 0, 0);
        double normalizedY = mapper.ToNormalizedCenterY(PdfRect.FromLtrb(pos.XMin, pos.YMin, pos.XMax, pos.YMax), rotation);
        Log($"NavigateToPosition: pagePts={wPts:F1}x{hPts:F1} rotation={rotation} normalizedY={normalizedY:F4} centerStored=({pos.XMin + (pos.XMax - pos.XMin) / 2:F1},{pos.YMin + (pos.YMax - pos.YMin) / 2:F1})");

        // Ensure target page is rendered
        double viewH = PageScroller.ViewportHeight;
        var pagePositions = _state.PositionsByPage.TryGetValue(pageIdx, out var pp) ? pp : new List<WordPosition>();
        await ((IPdfRenderingService)this).GetOrRenderPageAsync(pageIdx, pagePositions);

        // Flush dispatcher queue at Background priority to ensure:
        //   Normal   — PageImage setter (from PdfPageView.OnLoaded continuation)
        //   DataBind — binding engine pushes PageImage → Image.Source
        //   Loaded   — layout settles after binding update
        //   Input    — any pending input is processed
        //   Render   — any pending render ops
        await Dispatcher.InvokeAsync(() => { }, System.Windows.Threading.DispatcherPriority.Background);
        Log($"NavigateToPosition: dispatcher flushed");

        // Compute scroll target: use PointToScreen for reliable screen-space coords
        double wordContentY = 0;
        bool refined = false;

        PageList.UpdateLayout();

        var targetContainer = PageList.ItemContainerGenerator.ContainerFromIndex(matchIdx) as FrameworkElement;
        if (targetContainer is not null)
        {
            targetContainer.UpdateLayout();
            var imgControl = FindChild<Image>(targetContainer);
            bool srcOk = imgControl is not null && imgControl.Source is not null;
            bool hOk = imgControl is not null && imgControl.ActualHeight > 0;
            Log($"NavigateToPosition: container found, img={imgControl is not null} srcOk={srcOk} hOk={hOk} actualH={imgControl?.ActualHeight:F1}");
            if (imgControl is not null && imgControl.Source is not null && imgControl.ActualHeight > 0)
            {
                Rect pdfRealBounds = GetActualImageRect(imgControl);
                Log($"NavigateToPosition: GetActualImageRect returned empty={pdfRealBounds == Rect.Empty} bounds=({pdfRealBounds.X:F1},{pdfRealBounds.Y:F1},{pdfRealBounds.Width:F1},{pdfRealBounds.Height:F1})");
                if (pdfRealBounds != Rect.Empty)
                {
                    double wordY = pdfRealBounds.Top + normalizedY * pdfRealBounds.Height;
                    Point relativeWord = imgControl.TransformToAncestor(PageScroller).Transform(new Point(0, wordY));
                    wordContentY = PageScroller.VerticalOffset + relativeWord.Y;
                    refined = true;
                    Log($"NavigateToPosition: REFINED wordY={wordY:F1} relToScroller=({relativeWord.X:F0},{relativeWord.Y:F0}) vOff={PageScroller.VerticalOffset:F1} wordContentY={wordContentY:F1}");
                }
            }
            else
            {
                Log($"NavigateToPosition: Image not usable for refined path");
            }
        }
        else
        {
            Log($"NavigateToPosition: container unavailable (matchIdx={matchIdx})");
        }

        if (!refined)
        {
            double availW = LayoutConstants.AvailWidth(PageScroller.ViewportWidth);
            double accBefore = AccumulatePageHeightBefore(matchIdx);
            var fallbackVm = _state.PageViewModels![matchIdx];
            double offsetWithin = LayoutConstants.WordOffsetWithinItem(availW, fallbackVm.ImagePixelWidth, fallbackVm.ImagePixelHeight, normalizedY);
            wordContentY = accBefore + offsetWithin;
            Log($"NavigateToPosition: FALLBACK availW={availW:F1} accBefore={accBefore:F1} vmSize={fallbackVm.ImagePixelWidth}x{fallbackVm.ImagePixelHeight} offsetWithin={offsetWithin:F1} wordContentY={wordContentY:F1}");
        }

        double target = wordContentY - viewH / 2;
        target = Math.Max(0, Math.Min(target, PageScroller.ScrollableHeight));
        Log($"NavigateToPosition: viewH={viewH:F1} target={target:F1} scrollableH={PageScroller.ScrollableHeight:F1}");

        PageScroller.ScrollToVerticalOffset(target);
        _ = Dispatcher.InvokeAsync(() =>
        {
            double afterOff = PageScroller.VerticalOffset;
            double wordViewportY = wordContentY - afterOff;
            WordMarker.Margin = new Thickness(0, wordViewportY - 8, 0, 0);
            WordMarker.Visibility = Visibility.Visible;
            Log($"NavigateToPosition: AFTER_SCROLL offset={afterOff:F1} wordViewportY={wordViewportY:F1} center={viewH / 2:F1} delta={Math.Abs(wordViewportY - viewH / 2):F1}");
        }, System.Windows.Threading.DispatcherPriority.Loaded);

        UpdateMatchNav();
        UpdatePositionNav();
    }


    // ── Helpers ────────────────────────────────────────────────────

    private static T? FindChild<T>(DependencyObject parent) where T : DependencyObject
    {
        for (int i = 0; i < VisualTreeHelper.GetChildrenCount(parent); i++)
        {
            var child = VisualTreeHelper.GetChild(parent, i);
            if (child is T t) return t;
            var found = FindChild<T>(child);
            if (found != null) return found;
        }
        return null;
    }

    private static Rect GetActualImageRect(Image imageControl)
    {
        var source = imageControl.Source;
        if (source is null) return Rect.Empty;

        double bitmapW = source.Width;
        double bitmapH = source.Height;
        double controlW = imageControl.ActualWidth;
        double controlH = imageControl.ActualHeight;

        if (bitmapW <= 0 || bitmapH <= 0 || controlW <= 0 || controlH <= 0)
            return Rect.Empty;

        double scale = Math.Min(controlW / bitmapW, controlH / bitmapH);
        double actualPdfW = bitmapW * scale;
        double actualPdfH = bitmapH * scale;
        double offsetX = (controlW - actualPdfW) / 2;
        double offsetY = (controlH - actualPdfH) / 2;

        return new Rect(offsetX, offsetY, actualPdfW, actualPdfH);
    }

    // ── Build page models (no virtualization) ──────────────────────

    private void BuildOrDeferViewModels(CancellationToken ct)
    {
        if (PageScroller.ViewportWidth > 0)
        {
            BuildPageViewModels();
            UpdatePositionNav();
            FinishLoading();
            _ = Dispatcher.InvokeAsync(async () =>
            {
                Log("BuildOrDeferViewModels: deferred NavigateToPosition(0) at Loaded");
                if (_state.Positions.Count > 0)
                {
                    _state.CurrentPositionIndex = 0;
                    await NavigateToPosition(0);
                }
            }, DispatcherPriority.Loaded);
            return;
        }

        Log("BuildOrDeferViewModels: viewport not yet laid out — deferring");
        EventHandler? handler = null;
        handler = (_, _) =>
        {
            if (ct.IsCancellationRequested)
            {
                PageScroller.LayoutUpdated -= handler;
                return;
            }

            if (PageScroller.ViewportWidth > 35)
            {
                PageScroller.LayoutUpdated -= handler;
                Log("BuildOrDeferViewModels: deferred build now");
                BuildPageViewModels();
                UpdatePositionNav();
                FinishLoading();
                _ = Dispatcher.InvokeAsync(async () =>
                {
                    Log("BuildOrDeferViewModels: deferred NavigateToPosition(0) at Loaded (deferred path)");
                    if (_state.Positions.Count > 0)
                    {
                        _state.CurrentPositionIndex = 0;
                        await NavigateToPosition(0);
                    }
                }, DispatcherPriority.Loaded);
            }
        };
        PageScroller.LayoutUpdated += handler;
    }

    private void BuildPageViewModels()
    {
        var list = new List<PdfPageViewModel>(_state.MatchingPages.Count);

        for (int i = 0; i < _state.MatchingPages.Count; i++)
        {
            int pageIdx = _state.MatchingPages[i];
            _state.PositionsByPage.TryGetValue(pageIdx, out var pos);

            // Estimate pixel dimensions from PDF page size + render DPI.
            // These match the actual bitmap produced by RenderPageInternalAsync.
            var (wPts, hPts) = _renderer.GetPageDimensions(pageIdx);
            int pixW = (int)(wPts * _renderer.TargetDpi / 72.0);
            int pixH = (int)(hPts * _renderer.TargetDpi / 72.0);

            list.Add(new PdfPageViewModel
            {
                PageIndex = pageIdx,
                MatchIndex = i,
                ImagePixelWidth = pixW,
                ImagePixelHeight = pixH,
                Positions = pos ?? new List<WordPosition>(),
            });
        }

        _state.PageViewModels = new ObservableCollection<PdfPageViewModel>(list);
        PageList.ItemsSource = _state.PageViewModels;
    }

    private void FinishLoading()
    {
        _isLoading = false;
        ResultsList.IsEnabled = true;
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
        // Cancel any in-flight renders and thumbnail preloads
        _thumbCts?.Cancel();
        _selectionCts?.Cancel();
        _selectionCts = null;

        _pendingRenders.Clear();
        PageList.ItemsSource = null;
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
