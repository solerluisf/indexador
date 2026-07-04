using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Threading;
using PdfExplorer.Models;
using PdfExplorer.Services;
using PdfExplorer.ViewModels;


namespace PdfExplorer.Views;

public partial class SearchTab : UserControl, IPdfRenderingService
{
    private readonly PdfEngine _engine;
    private readonly IViewerMediator _viewerMediator;
    private readonly ISearchMediator _searchMediator;
    private readonly INavigationMediator _navigationMediator;
    private uint? _selectedCollId;
    private bool _isLoading;
    private bool _isNavigating;
    private const double LineScrollPx = 24.0;
    private const double ViewportScrollFactor = 0.9;
    private const double PageGap = 8.0;
    private const double PageHeaderHeight = 20.0;

    private sealed class PageElement
    {
        public required StackPanel Panel { get; init; }
        public required Image Image { get; init; }
    }

    private readonly Dictionary<int, PageElement> _pageElements = new();
    private (double WidthPts, double HeightPts)[] _pageDimensions = [];
    private double[] _pageYOffsets = [];
    private double _totalContentHeight;
    private int _totalPages;
    private double _renderDpi = 150;
    private double _currentRenderDpi = 150;
    private int _zoomMode = 3;
    private bool _pendingRefresh;

    private static readonly double[] ZoomMultipliers = [0.5, 0.75, 1.0, -1, -2, 1.5, 2.0];

    private static readonly Dictionary<Key, Func<double, double, double, double>> ScrollStrategies = new()
    {
        [Key.Down] = (offset, _, maxH) => Math.Min(offset + LineScrollPx, maxH),
        [Key.Up] = (offset, _, _) => Math.Max(offset - LineScrollPx, 0),
        [Key.PageDown] = (offset, viewH, maxH) => Math.Min(offset + viewH * ViewportScrollFactor, maxH),
        [Key.PageUp] = (offset, viewH, _) => Math.Max(offset - viewH * ViewportScrollFactor, 0),
    };

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
        _viewerMediator = new ViewerMediator();
        _viewerMediator.StateChanged += OnViewerStateChanged;
        _viewerMediator.ViewModelsBuilt += OnPageDataReady;
        _searchMediator = new SearchMediator(_engine, new ThumbnailService());
        _searchMediator.SearchCompleted += OnSearchCompleted;
        _searchMediator.SearchFailed += OnSearchFailed;
        _navigationMediator = new NavigationMediator();
        _navigationMediator.StateChanged += OnNavStateChanged;
        Loaded += OnLoaded;
        IsVisibleChanged += OnIsVisibleChanged;
        InitRenderDpi();
        Log("Constructor end, engine=" + (_engine is not null ? "ok" : "null"));
    }

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        CollectionFilter.ItemsSource = _engine.Collections;
        if (_selectedCollId.HasValue)
        {
            var coll = _engine.Collections.FirstOrDefault(c => c.Id == _selectedCollId.Value);
            if (coll is not null)
                CollectionFilter.SelectedItem = coll;
        }
    }

    private void OnCollectionFilterChanged(object sender, SelectionChangedEventArgs e)
    {
        _searchMediator.ResetPage();
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

    private void OnSearchModeChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_engine is null) return;
        var isBoolean = SearchModeCombo.SelectedIndex == 1;
        _engine.SearchBooleanMode = isBoolean;
    }

    // ── Zoom ────────────────────────────────────────────────────────

    private void OnZoomChanged(object sender, SelectionChangedEventArgs e)
    {
        _zoomMode = ZoomCombo.SelectedIndex;
        UpdateDpiAndRefresh();
    }

    private void InitRenderDpi()
    {
        _renderDpi = App.RenderDpi;
        _viewerMediator.Renderer.TargetDpi = _renderDpi;
        App.RenderDpiChanged += OnRenderDpiChanged;
        App.RenderInvertedChanged += OnRenderInvertedChanged;
        App.HighlightColorChanged += OnHighlightColorChanged;
    }

    private void OnHighlightColorChanged()
    {
        if (_totalPages == 0) return;
        RemoveAllPageElements();
        _viewerMediator.InvalidateAllPages();
        _pendingRefresh = true;
        if (IsVisible)
            RefreshPending();
    }

    private void OnIsVisibleChanged(object sender, DependencyPropertyChangedEventArgs e)
    {
        if (IsVisible && _pendingRefresh)
            RefreshPending();
    }

    private void RefreshPending()
    {
        _pendingRefresh = false;
        UpdateVisiblePages();
    }

    private void OnRenderInvertedChanged()
    {
        if (_totalPages == 0) return;
        RemoveAllPageElements();
        _viewerMediator.InvalidateAllPages();
        UpdateVisiblePages();
    }

    private void OnRenderDpiChanged()
    {
        int newDpi = App.RenderDpi;
        if (newDpi == _renderDpi || _totalPages == 0) return;
        _renderDpi = newDpi;
        _viewerMediator.Renderer.TargetDpi = _renderDpi;
        Log($"DPI changed to {_renderDpi}");

            var (anchorIdx, anchorOff) = SaveScrollAnchor();
            RemoveAllPageElements();
            _viewerMediator.InvalidateAllPages();
            ComputeRenderDpi();
            RecomputePageYOffsets();
            LayoutCanvas();
        RestoreScrollAnchor(anchorIdx, anchorOff);
        UpdateVisiblePages();
    }

    private (int idx, double off) SaveScrollAnchor()
    {
        double scroll = PageScroller.VerticalOffset;
        for (int i = 0; i < _totalPages; i++)
            if (GetPageBottom(i) > scroll)
                return (i, scroll - GetPageY(i));
        return _totalPages > 0 ? (0, 0) : (-1, 0);
    }

    private void RestoreScrollAnchor(int idx, double off)
    {
        if (idx < 0 || idx >= _totalPages) return;
        double newScroll = GetPageY(idx) + off;
        newScroll = Math.Max(0, Math.Min(newScroll, PageScroller.ScrollableHeight));
        PageScroller.ScrollToVerticalOffset(newScroll);
    }

    private void UpdateDpiAndRefresh()
    {
        if (_totalPages == 0) return;

        var (anchorIdx, anchorOff) = SaveScrollAnchor();

        ComputeRenderDpi();

        Log($"UpdateDpiAndRefresh: dpi={_currentRenderDpi:F0}, viewW={PageScroller.ViewportWidth:F0}");

        RelayoutPageContainers();

        RestoreScrollAnchor(anchorIdx, anchorOff);
        UpdateVisiblePages();
    }

    private void ComputeRenderDpi()
    {
        if (_totalPages == 0) return;

        double viewW = PageScroller.ViewportWidth;
        double baseDpi = _viewerMediator.Renderer.TargetDpi;
        double mult = _zoomMode >= 0 && _zoomMode < ZoomMultipliers.Length
            ? ZoomMultipliers[_zoomMode]
            : -1;

        if (mult >= 0)
        {
            _currentRenderDpi = baseDpi * mult;
            return;
        }

        if (_zoomMode == 3)
        {
            double maxW = _pageDimensions.Max(d => d.WidthPts);
            if (maxW > 0 && viewW > 0)
                _currentRenderDpi = 72.0 * viewW / maxW;
            else
                _currentRenderDpi = baseDpi;
            return;
        }

        if (_zoomMode == 4)
        {
            double maxW = _pageDimensions.Max(d => d.WidthPts);
            double maxH = _pageDimensions.Max(d => d.HeightPts);
            if (maxW > 0 && maxH > 0 && viewW > 0)
            {
                double viewH = PageScroller.ViewportHeight;
                double dpiW = 72.0 * viewW / maxW;
                double dpiH = 72.0 * viewH / maxH;
                _currentRenderDpi = Math.Min(dpiW, dpiH);
            }
            else
                _currentRenderDpi = baseDpi;
            return;
        }

        _currentRenderDpi = baseDpi;
    }

    private double GetRenderedHeight(int pageIdx)
    {
        if (pageIdx < 0 || pageIdx >= _pageDimensions.Length) return 0;
        var (wPts, hPts) = _pageDimensions[pageIdx];
        return hPts * _currentRenderDpi / 72.0;
    }

    private double GetPageDisplayHeight(int pageIdx)
    {
        double wPts = _pageDimensions[pageIdx].WidthPts;
        double hPts = _pageDimensions[pageIdx].HeightPts;
        double viewW = PageScroller.ViewportWidth;
        double displayW = Math.Min(wPts * _currentRenderDpi / 72.0, viewW);
        return hPts * displayW / wPts;
    }

    private double GetPageY(int pageIdx)
    {
        if (pageIdx < 0 || pageIdx >= _pageYOffsets.Length)
            return 0;
        return _pageYOffsets[pageIdx];
    }

    private double GetPageBottom(int pageIdx)
    {
        return GetPageY(pageIdx) + GetPageDisplayHeight(pageIdx) + PageGap + PageHeaderHeight;
    }

    private void RecomputePageYOffsets()
    {
        _pageYOffsets = new double[_totalPages];
        double y = 0;
        for (int i = 0; i < _totalPages; i++)
        {
            _pageYOffsets[i] = y;
            y += GetPageDisplayHeight(i) + PageGap + PageHeaderHeight;
        }
        _totalContentHeight = y;
    }

    private double GetTotalContentHeight()
    {
        return _totalContentHeight;
    }

    // ── Canvas page management ──────────────────────────────────────

    private double _lastViewW;

    private void OnPageScrollerScroll(object sender, ScrollChangedEventArgs e)
    {
        if (_totalPages > 0)
        {
            Log($"Scroll: VOffset={PageScroller.VerticalOffset:F0} VPH={PageScroller.ViewportHeight:F0} " +
                $"SH={PageScroller.ScrollableHeight:F0} ExtH={PageScroller.ExtentHeight:F0} " +
                $"Page0Y={GetPageY(0):F0} PageLastY={GetPageY(_totalPages - 1):F0}");

            if (PageScroller.Template?.FindName("PART_VerticalScrollBar", PageScroller) is ScrollBar sb &&
                sb.Template?.FindName("PART_Track", sb) is Track t)
            {
                Log($"Track: Rev={t.IsDirectionReversed} Orient={t.Orientation} " +
                    $"Val={t.Value:F0} Min={t.Minimum:F0} Max={t.Maximum:F0} VS={t.ViewportSize:F0}");
            }
            else
            {
                Log("Track: NOT FOUND");
            }
        }
        UpdateVisiblePages();
    }

    private void OnPageScrollerSizeChanged(object sender, SizeChangedEventArgs e)
    {
        DetectCanvasResize();
    }

    private void DetectCanvasResize()
    {
        if (_totalPages == 0) return;
        double viewW = PageScroller.ViewportWidth;
        if (viewW <= 0) return;
        if (Math.Abs(viewW - _lastViewW) <= 1) return;

        Log($"DetectCanvasResize: viewW={viewW:F0} (was {_lastViewW:F0}), zoom={_zoomMode}, dpi={_currentRenderDpi:F0}");
        _lastViewW = viewW;
        if (_zoomMode >= 3)
            UpdateDpiAndRefresh();
        else
        {
            var (anchorIdx, anchorOff) = SaveScrollAnchor();
            RelayoutPageContainers();
            RestoreScrollAnchor(anchorIdx, anchorOff);
            UpdateVisiblePages();
        }
    }

    private void UpdateVisiblePages()
    {
        if (_totalPages == 0) return;

        double viewW = PageScroller.ViewportWidth;
        if (viewW <= 0) return;

        double scrollTop = PageScroller.VerticalOffset;
        double scrollBottom = scrollTop + PageScroller.ViewportHeight;
        double buffer = PageScroller.ViewportHeight;
        double rangeTop = Math.Max(0, scrollTop - buffer);
        double rangeBottom = scrollBottom + buffer;

        HashSet<int> needed = new();
        for (int i = 0; i < _totalPages; i++)
        {
            double itemTop = GetPageY(i);
            double itemBottom = GetPageBottom(i);
            if (itemBottom < rangeTop || itemTop > rangeBottom)
                continue;
            needed.Add(i);
        }

        foreach (var key in _pageElements.Keys.ToList())
        {
            if (!needed.Contains(key))
                RemovePageElement(key);
        }

        double viewCenter = scrollTop + PageScroller.ViewportHeight / 2;
        foreach (int pageIdx in needed
            .Where(p => !_pageElements.ContainsKey(p))
            .OrderBy(p =>
            {
                double c = (GetPageY(p) + GetPageBottom(p)) / 2;
                return Math.Abs(c - viewCenter);
            }))
        {
            _ = EnsurePageElementAsync(pageIdx);
        }
    }

    private void RelayoutPageContainers()
    {
        double viewW = PageScroller.ViewportWidth;
        if (viewW <= 0) return;
        RecomputePageYOffsets();

        foreach (var (pageIdx, elem) in _pageElements.ToList())
        {
            double wPts = _pageDimensions[pageIdx].WidthPts;
            double hPts = _pageDimensions[pageIdx].HeightPts;
            double displayW = Math.Min(wPts * _currentRenderDpi / 72.0, viewW);
            double displayH = hPts * displayW / wPts;

            elem.Panel.Width = viewW;
            elem.Image.Width = displayW;
            elem.Image.Height = displayH;
            Canvas.SetTop(elem.Panel, GetPageY(pageIdx));

        }
        LayoutCanvas();
    }

    private void RemovePageElement(int pageIdx)
    {
        if (_pageElements.TryGetValue(pageIdx, out var elem))
        {
            PageCanvas.Children.Remove(elem.Panel);
            _pageElements.Remove(pageIdx);
        }
    }

    private void RemoveAllPageElements()
    {
        foreach (var key in _pageElements.Keys.ToList())
            RemovePageElement(key);
        _pageElements.Clear();
    }

    private async Task EnsurePageElementAsync(int pageIdx)
    {
        if (_pageElements.ContainsKey(pageIdx))
            return;

        var pathBefore = _viewerMediator.PdfPath;
        var pagePositions = _viewerMediator.PositionsByPage.TryGetValue(pageIdx, out var pp)
            ? pp : new List<WordPosition>();

        PageRenderItem? renderItem;
        try
        {
            renderItem = await ((IPdfRenderingService)this).GetOrRenderPageAsync(pageIdx, pagePositions);
        }
        catch (OperationCanceledException) { return; }
        catch (Exception ex)
        {
            Debug.WriteLine($"[SearchTab] Render error: {ex.Message}");
            return;
        }

        if (renderItem?.PageImage is null) return;

        if (_viewerMediator.PdfPath != pathBefore) return;

        await Dispatcher.InvokeAsync(() =>
        {
            if (_pageElements.ContainsKey(pageIdx))
                return;

            if (_viewerMediator.PdfPath != pathBefore) return;

            double viewW = PageScroller.ViewportWidth;
            if (viewW <= 0) return;

            double pageY = GetPageY(pageIdx);

            double wPts = _pageDimensions[pageIdx].WidthPts;
            double hPts = _pageDimensions[pageIdx].HeightPts;
            double displayW = Math.Min(wPts * _currentRenderDpi / 72.0, viewW);
            double displayH = hPts * displayW / wPts;

            Log($"EnsurePageElement: page={pageIdx + 1}, viewW={viewW:F0}, dispW={displayW:F0}, dispH={displayH:F0}, pageY={pageY:F0}");

            var header = new TextBlock
            {
                Text = $"Page {pageIdx + 1}",
                FontWeight = FontWeights.SemiBold,
                FontSize = 12,
                Height = PageHeaderHeight,
                HorizontalAlignment = HorizontalAlignment.Center,
                Foreground = (System.Windows.Media.Brush)FindResource("PageForeground"),
            };

            var image = new Image
            {
                Source = renderItem.PageImage,
                Stretch = Stretch.Uniform,
                Width = displayW,
                Height = displayH,
                HorizontalAlignment = HorizontalAlignment.Center,
            };

            var panel = new StackPanel
            {
                Width = viewW,
            };
            panel.Children.Add(header);
            panel.Children.Add(image);

            Canvas.SetTop(panel, pageY);
            Canvas.SetLeft(panel, 0);

            var elem = new PageElement
            {
                Panel = panel,
                Image = image,
            };
            _pageElements[pageIdx] = elem;
            PageCanvas.Children.Add(panel);
        });
    }

    private void LayoutCanvas()
    {
        PageCanvas.Height = GetTotalContentHeight();
        Log($"LayoutCanvas: H={PageCanvas.Height:F0}");
    }

    // ── Search ──────────────────────────────────────────────────────

    private void OnSearchBoxKeyDown(object sender, KeyEventArgs e)
    {
        if (e.Key == Key.Enter)
        {
            OnSearchClick(sender, e);
        }
    }

    private void OnViewerKeyDown(object sender, KeyEventArgs e)
    {
        if (_isLoading || _totalPages == 0)
            return;

        if (ScrollStrategies.TryGetValue(e.Key, out var strategy))
        {
            double offset = PageScroller.VerticalOffset;
            double viewH = PageScroller.ViewportHeight;
            double maxH = PageScroller.ScrollableHeight;

            PageScroller.ScrollToVerticalOffset(strategy(offset, viewH, maxH));
            e.Handled = true;
        }
    }

    private async void OnSearchClick(object sender, RoutedEventArgs e)
    {
        Log("OnSearchClick");
        ClearViewer();
        await _searchMediator.SearchAsync(SearchBox.Text, _selectedCollId);
    }

    private void OnSearchCompleted(object? sender, SearchResultsEventArgs e)
    {
        _ = Dispatcher.InvokeAsync(() =>
        {
            try
            {
                ResultsList.ItemsSource = e.Results;
                PageInfo.Content = $"{e.CurrentPage + 1} / {e.TotalPages}";
                PrevPage.IsEnabled = e.CurrentPage > 0;
                NextPage.IsEnabled = e.CurrentPage + 1 < e.TotalPages;
                ResultCountLabel.Text = $"{e.TotalHits} result(s)";
            }
            catch (Exception ex) { Log($"OnSearchCompleted handler error: {ex.Message}"); }
        });
    }

    private void OnSearchFailed(object? sender, SearchErrorEventArgs e)
    {
        _ = Dispatcher.InvokeAsync(() =>
        {
            try { ResultCountLabel.Text = $"Search error: {e.Error}"; }
            catch (Exception ex) { Log($"OnSearchFailed handler error: {ex.Message}"); }
        });
    }

    private void OnViewerStateChanged(object? sender, ViewerStateChangedEventArgs e)
    {
        try { WordsField.Text = _viewerMediator.PositionsDebugText; }
        catch (Exception ex) { Log($"OnViewerStateChanged error: {ex.Message}"); }
    }

    private void OnPageDataReady(object? sender, EventArgs e)
    {
        try
        {
            Canvas.SetTop(PageCanvas, 0);
            Canvas.SetLeft(PageCanvas, 0);
            UpdateVisiblePages();
        }
        catch (Exception ex) { Log($"OnPageDataReady error: {ex.Message}"); }
    }

    private async void OnNextPage(object sender, RoutedEventArgs e)
    {
        try
        {
            ClearViewer();
            await _searchMediator.NextPageAsync(_selectedCollId);
        }
        catch (Exception ex) { Log($"OnNextPage error: {ex.Message}"); }
    }

    private async void OnPrevPage(object sender, RoutedEventArgs e)
    {
        try
        {
            ClearViewer();
            await _searchMediator.PrevPageAsync(_selectedCollId);
        }
        catch (Exception ex) { Log($"OnPrevPage error: {ex.Message}"); }
    }

    // ── Document selection ──────────────────────────────────────────

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

            ClearViewer();

            _isLoading = true;
            ResultsList.IsEnabled = false;

            var ct = _viewerMediator.CurrentRenderToken;

            var sw = Stopwatch.StartNew();
            Log($"OnResultSelected: id={result.Id}, path={result.Path}, collId={result.CollectionId}, query='{_searchMediator.LastQuery}'");
            StatusLabel.Text = System.IO.Path.GetFileName(result.Path);

            byte[] pdfBytes;
            try
            {
                pdfBytes = System.IO.File.ReadAllBytes(result.Path);
                Log($"File read: {sw.Elapsed.TotalMilliseconds:F0}ms, {pdfBytes.Length} bytes");
            }
            catch (Exception ex)
            {
                Log($"Failed to read PDF file: {ex.Message}");
                StatusLabel.Text = $"Failed to read PDF: {ex.Message}";
                _isLoading = false;
                ResultsList.IsEnabled = true;
                return;
            }

            // Fetch term positions
            List<WordPosition> positions;
            var matchedTerms = result.MatchedTerms?.ToList() ?? new List<string>();
            var phraseGroups = result.PhraseGroups?
                .Select(g => g.ToList())
                .ToList() ?? new List<List<string>>();
            if (result.CollectionId.HasValue && matchedTerms.Count > 0)
            {
                try
                {
                    positions = await _viewerMediator.FetchPositionsAsync(
                        _engine, (uint)result.CollectionId.Value, result.Id, matchedTerms, phraseGroups);
                    Log($"Fetch positions: {sw.Elapsed.TotalMilliseconds:F0}ms, count={positions.Count}");
                }
                catch (Exception ex)
                {
                    Log($"GetTermPositions warning: {ex.GetType().Name}: {ex.Message}");
                    positions = new List<WordPosition>();
                }
            }
            else
            {
                Log("No collection ID or matched terms available - cannot fetch indexed positions");
                positions = new List<WordPosition>();
            }

            _viewerMediator.SetPositions(positions, _navigationMediator, matchedTerms, isBooleanMode: SearchModeCombo.SelectedIndex == 1);

            _viewerMediator.OpenDocument(pdfBytes, result.Path);
            int pageCount = _viewerMediator.Renderer.GetPageCount();
            Log($"Open document: {sw.Elapsed.TotalMilliseconds:F0}ms, pages={pageCount}");

            if (positions.Count == 0)
            {
                Log("No positions found — showing all pages without highlights");
                StatusLabel.Text += " — no highlights";
            }

            if (ct.IsCancellationRequested)
            {
                Log("OnResultSelected: cancelled before build");
                return;
            }

            BuildPageData();
            Log($"Build page data: {sw.Elapsed.TotalMilliseconds:F0}ms");

            Log($"OnResultSelected complete (total {sw.Elapsed.TotalMilliseconds:F0}ms)");
        }
        catch (Exception ex)
        {
            Log($"OnResultSelected UNHANDLED ERROR: {ex}");
            StatusLabel.Text = $"Error: {ex.Message}";
        }
    }

    // ── Lazy rendering (IPdfRenderingService) ───────────────────────

    async Task<PageRenderItem> IPdfRenderingService.GetOrRenderPageAsync(int pageIdx, List<WordPosition> pagePositions)
    {
        return await _viewerMediator.GetOrRenderPageAsync(pageIdx, pagePositions, _viewerMediator.CurrentRenderToken);
    }

    // ── Match navigation ────────────────────────────────────────────

    private async void OnPrevMatch(object sender, RoutedEventArgs e)
    {
        if (_isLoading) return;
        try
        {
            if (!_navigationMediator.GotoPrevMatch()) return;
            ScrollToMatch(_navigationMediator.CurrentMatchIndex, scrollToTop: true);
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
            if (!_navigationMediator.GotoNextMatch()) return;
            ScrollToMatch(_navigationMediator.CurrentMatchIndex, scrollToTop: true);
        }
        catch (Exception ex)
        {
            Log($"OnNextMatch error: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private void OnNavStateChanged(object? sender, EventArgs e)
    {
        MatchInfo.Content = _navigationMediator.TotalMatchPages > 0
            ? $"Match page {_navigationMediator.CurrentMatchIndex + 1} of {_navigationMediator.TotalMatchPages}"
            : "0 matches";
        PrevMatch.IsEnabled = _navigationMediator.CanGoPrevMatch;
        NextMatch.IsEnabled = _navigationMediator.CanGoNextMatch;

        var phraseCount = _navigationMediator.TotalPhraseCount;
        PositionInfo.Content = phraseCount > 0
            ? $"{_navigationMediator.CurrentPhraseIndex + 1} / {phraseCount}"
            : "0 / 0";
        PrevPosition.IsEnabled = _navigationMediator.CanGoPrevPosition;
        NextPosition.IsEnabled = _navigationMediator.CanGoNextPosition;

    }

    private void ScrollToMatch(int index, bool scrollToTop = false)
    {
        Log($"ScrollToMatch({index}, scrollToTop={scrollToTop})");
        var matchingPages = _viewerMediator.MatchingPages;
        if (index < 0 || index >= matchingPages.Count) return;

        int pageIdx = matchingPages[index];

        if (scrollToTop)
        {
            double target = GetPageY(pageIdx);
            PageScroller.ScrollToVerticalOffset(target);
            return;
        }

        // Try to scroll to the first position on this page
        var posIdx = Enumerable.Range(0, _viewerMediator.Positions.Count)
            .FirstOrDefault(i => _viewerMediator.Positions[i].Page - 1 == pageIdx, -1);
        if (posIdx >= 0)
        {
            _ = ScrollToPosition(posIdx);
            return;
        }

        double targetPx = GetPageY(pageIdx);
        PageScroller.ScrollToVerticalOffset(targetPx);
        Log($"ScrollToMatch: targetPx={targetPx:F1}");
    }

    // ── Position (individual match) navigation ─────────────────────

    private async void OnPrevPosition(object sender, RoutedEventArgs e)
    {
        if (_isLoading) return;
        try
        {
            if (_isNavigating) { Log("OnPrevPosition: skipped (navigation in progress)"); return; }
            _isNavigating = true;
            if (!_navigationMediator.GotoPrevPosition()) return;
            Log($"OnPrevPosition: to index {_navigationMediator.CurrentPositionIndex}");
            await ScrollToPosition(_navigationMediator.CurrentPositionIndex);
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
            Log($"OnNextPosition: index={_navigationMediator.CurrentPositionIndex}");
            if (_isNavigating) { Log("OnNextPosition: skipped (navigation in progress)"); return; }
            _isNavigating = true;
            if (!_navigationMediator.GotoNextPosition()) return;
            Log($"OnNextPosition: to index {_navigationMediator.CurrentPositionIndex}");
            await ScrollToPosition(_navigationMediator.CurrentPositionIndex);
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

    private async Task ScrollToPosition(int posIdx)
    {
        Log($"ScrollToPosition({posIdx}) — entering");
        if (_isLoading)
        {
            Log("ScrollToPosition: skipped (loading in progress)");
            return;
        }
        if (posIdx < 0 || posIdx >= _viewerMediator.Positions.Count)
        {
            Log($"ScrollToPosition: invalid posIdx={posIdx} (count={_viewerMediator.Positions.Count})");
            return;
        }

        if (PageScroller.ViewportWidth <= 12)
        {
            Log($"ScrollToPosition: ViewportWidth={PageScroller.ViewportWidth} too small, deferring");
            var capturedPath = _viewerMediator.PdfPath;
            EventHandler? handler = null;
            handler = (_, _) =>
            {
                if (_viewerMediator.PdfPath != capturedPath)
                {
                    PageScroller.LayoutUpdated -= handler;
                    return;
                }
                if (PageScroller.ViewportWidth > 12)
                {
                    PageScroller.LayoutUpdated -= handler;
                    _ = ScrollToPosition(posIdx);
                }
            };
            PageScroller.LayoutUpdated += handler;
            return;
        }

        var pos = _viewerMediator.Positions[posIdx];
        var pageIdx = pos.Page - 1;

        if (pageIdx < 0 || pageIdx >= _totalPages)
        {
            Log($"ScrollToPosition: page {pageIdx} out of range");
            return;
        }

        var (wPts, hPts) = _pageDimensions[pageIdx];
        int rotation = _viewerMediator.Renderer.GetPageRotation(pageIdx);
        var mapper = new PdfCoordinateMapper(wPts, hPts, 0, 0);
        double normalizedY = mapper.ToNormalizedCenterY(PdfRect.FromLtrb(pos.XMin, pos.YMin, pos.XMax, pos.YMax), rotation);
        Log($"ScrollToPosition: pagePts={wPts:F1}x{hPts:F1} rotation={rotation} normalizedY={normalizedY:F4}");

        double pageY = GetPageY(pageIdx);
        double renderH = GetPageDisplayHeight(pageIdx);
        double wordOffset = normalizedY * renderH;
        double target = pageY + PageHeaderHeight + wordOffset - PageScroller.ViewportHeight / 2;

        target = Math.Max(0, Math.Min(target, PageScroller.ScrollableHeight));
        PageScroller.ScrollToVerticalOffset(target);
        Log($"ScrollToPosition: targetPx={target:F1}");
    }

    // ── Build page data ─────────────────────────────────────────────

    private void BuildPageData()
    {
        try
        {
            if (PageScroller.ViewportWidth <= 0)
            {
                Log("BuildPageData: viewport not laid out yet, deferring...");
                EventHandler? handler = null;
                handler = (_, _) =>
                {
                    if (PageScroller.ViewportWidth > 35)
                    {
                        PageScroller.LayoutUpdated -= handler;
                        DoBuildPageData();
                    }
                };
                PageScroller.LayoutUpdated += handler;
                return;
            }

            DoBuildPageData();
        }
        catch (Exception ex) { Log($"BuildPageData error: {ex.Message}"); }
    }

    private void DoBuildPageData()
    {
        var allDims = _viewerMediator.Renderer.GetAllPageDimensions();
        _totalPages = allDims.Length;
        _pageDimensions = allDims;

        _viewerMediator.Renderer.TargetDpi = _renderDpi;
        ComputeRenderDpi();
        RecomputePageYOffsets();
        LayoutCanvas();
        _lastViewW = PageScroller.ViewportWidth;

        _viewerMediator.BuildPageViewModels();
        _navigationMediator.GotoInitialPosition();
        FinishLoading();

        Log($"DoBuildPageData: {_totalPages} pages, dpi={_currentRenderDpi:F0}, viewW={_lastViewW:F0}, zoom={_zoomMode}");

        _ = Dispatcher.InvokeAsync(() =>
        {
            Log($"PostLayout: CanvasH={PageCanvas.ActualHeight:F0} VOffset={PageScroller.VerticalOffset:F0} " +
                $"VPH={PageScroller.ViewportHeight:F0} SH={PageScroller.ScrollableHeight:F0} " +
                $"EH={PageScroller.ExtentHeight:F0}");
        }, DispatcherPriority.Loaded);

        _ = Dispatcher.InvokeAsync(async () =>
        {
            try
            {
                if (_viewerMediator.Positions.Count > 0)
                    await ScrollToPosition(0);
            }
            catch (Exception ex) { Log($"BuildPageData scroll error: {ex.Message}"); }
            UpdateVisiblePages();
        }, DispatcherPriority.Loaded);
    }

    private void FinishLoading()
    {
        _isLoading = false;
        ResultsList.IsEnabled = true;

        if (ResultsList.ItemsSource is IReadOnlyList<SearchResultViewModel> items)
        {
            var missing = items.Where(vm => vm.Thumbnail is null).ToList();
            if (missing.Count > 0)
                _searchMediator.RetryPendingThumbnails(missing);
        }
    }

    private void ClearViewer()
    {
        _viewerMediator.Clear();
        _navigationMediator.Reset();

        RemoveAllPageElements();
        _pageDimensions = [];
        _pageYOffsets = [];
        _totalContentHeight = 0;
        _totalPages = 0;
        PageScroller.ScrollToVerticalOffset(0);
        WordsField.Text = "";
    }
}
