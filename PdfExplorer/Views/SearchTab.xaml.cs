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
    private readonly IViewerMediator _viewerMediator;
    private readonly ISearchMediator _searchMediator;
    private readonly INavigationMediator _navigationMediator;
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
        _viewerMediator = new ViewerMediator();
        _viewerMediator.StateChanged += OnViewerStateChanged;
        _viewerMediator.ViewModelsBuilt += OnViewModelsBuilt;
        _searchMediator = new SearchMediator(_engine, new ThumbnailService());
        _searchMediator.SearchCompleted += OnSearchCompleted;
        _searchMediator.SearchFailed += OnSearchFailed;
        _navigationMediator = new NavigationMediator();
        _navigationMediator.StateChanged += OnNavStateChanged;
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

    private void OnViewModelsBuilt(object? sender, EventArgs e)
    {
        try { PageList.ItemsSource = _viewerMediator.PageViewModels; }
        catch (Exception ex) { Log($"OnViewModelsBuilt error: {ex.Message}"); }
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

            ClearViewer();

            _isLoading = true;
            ResultsList.IsEnabled = false;

            var ct = _viewerMediator.CurrentRenderToken;

            var t0 = DateTime.UtcNow;
            Log($"OnResultSelected: id={result.Id}, path={result.Path}, collId={result.CollectionId}, query='{_searchMediator.LastQuery}'");
            StatusLabel.Text = System.IO.Path.GetFileName(result.Path);

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
                _isLoading = false;
                ResultsList.IsEnabled = true;
                return;
            }

            // Fetch term positions
            List<WordPosition> positions;
            if (result.CollectionId.HasValue)
            {
                try
                {
                    positions = await _viewerMediator.FetchPositionsAsync(
                        _engine, (uint)result.CollectionId.Value, result.Id, _searchMediator.LastQuery);
                }
                catch (Exception ex)
                {
                    Log($"GetTermPositions warning: {ex.GetType().Name}: {ex.Message}");
                    positions = new List<WordPosition>();
                }
            }
            else
            {
                Log("No collection ID available - cannot fetch indexed positions");
                positions = new List<WordPosition>();
            }

            _viewerMediator.SetPositions(positions, _navigationMediator, _searchMediator.LastQuery, isBooleanMode: SearchModeCombo.SelectedIndex == 1);
            _viewerMediator.OpenDocument(pdfBytes, result.Path);

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

            BuildOrDeferViewModels(ct);

            var tEnd = DateTime.UtcNow;
            Log($"OnResultSelected complete (total {(tEnd - t0).TotalMilliseconds:F0}ms)");
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

        double targetPx;
        double viewH = PageScroller.ViewportHeight;

        if (scrollToTop)
        {
            PageList.UpdateLayout();
            var container = PageList.ItemContainerGenerator.ContainerFromIndex(pageIdx) as FrameworkElement;
            if (container is not null)
            {
                container.UpdateLayout();
                Point containerOrigin = container.TransformToAncestor(PageScroller).Transform(new Point(0, 0));
                targetPx = PageScroller.VerticalOffset + containerOrigin.Y;
            }
            else
            {
                targetPx = AccumulatePageHeightBefore(pageIdx);
            }
            targetPx = Math.Max(0, Math.Min(targetPx, PageScroller.ScrollableHeight));
            PageScroller.ScrollToVerticalOffset(targetPx);
            return;
        }

        var posIdx = Enumerable.Range(0, _viewerMediator.Positions.Count).FirstOrDefault(i => _viewerMediator.Positions[i].Page - 1 == pageIdx, -1);
        if (posIdx >= 0 && _viewerMediator.PageViewModels is not null && pageIdx < _viewerMediator.PageViewModels.Count)
        {
            var pos = _viewerMediator.Positions[posIdx];
            var (wPts, hPts) = _viewerMediator.Renderer.GetPageDimensions(pageIdx);
            int rotation = _viewerMediator.Renderer.GetPageRotation(pageIdx);
            var mapper = new PdfCoordinateMapper(wPts, hPts, 0, 0);
            double normalizedY = mapper.ToNormalizedCenterY(PdfRect.FromLtrb(pos.XMin, pos.YMin, pos.XMax, pos.YMax), rotation);

            double wordContentY = 0;
            bool refined = false;

            PageList.UpdateLayout();

            var container = PageList.ItemContainerGenerator.ContainerFromIndex(pageIdx) as FrameworkElement;
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
                wordContentY = AccumulatePageHeightBefore(pageIdx);
                wordContentY += LayoutConstants.WordOffsetWithinItem(
                    availW,
                    _viewerMediator.PageViewModels[pageIdx].ImagePixelWidth,
                    _viewerMediator.PageViewModels[pageIdx].ImagePixelHeight,
                    normalizedY);
            }

            targetPx = wordContentY - viewH / 2;
        }
        else
        {
            targetPx = AccumulatePageHeightBefore(pageIdx);
        }

        targetPx = Math.Max(0, Math.Min(targetPx, PageScroller.ScrollableHeight));
        PageScroller.ScrollToVerticalOffset(targetPx);
        Log($"ScrollToMatch: targetPx={targetPx:F1}");
    }

    private double AccumulatePageHeightBefore(int pageIdx)
    {
        if (_viewerMediator.PageViewModels is null || pageIdx <= 0) return 0;
        double availW = LayoutConstants.AvailWidth(PageScroller.ViewportWidth);
        double total = 0;
        for (int i = 0; i < pageIdx; i++)
        {
            var vm = _viewerMediator.PageViewModels[i];
            total += LayoutConstants.TotalItemHeight(
                availW, vm.ImagePixelWidth, vm.ImagePixelHeight);
        }
        return total;
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
        var matchIdx = _navigationMediator.FindMatchForPage(pageIdx);
        if (matchIdx < 0)
        {
            Log($"ScrollToPosition: matchIdx={matchIdx} out of range — returning early");
            return;
        }

        // Get normalized Y position of the word within its page (0 = top, 1 = bottom)
        var (wPts, hPts) = _viewerMediator.Renderer.GetPageDimensions(pageIdx);
        int rotation = _viewerMediator.Renderer.GetPageRotation(pageIdx);
        var mapper = new PdfCoordinateMapper(wPts, hPts, 0, 0);
        double normalizedY = mapper.ToNormalizedCenterY(PdfRect.FromLtrb(pos.XMin, pos.YMin, pos.XMax, pos.YMax), rotation);
        Log($"ScrollToPosition: pagePts={wPts:F1}x{hPts:F1} rotation={rotation} normalizedY={normalizedY:F4}");

        // Ensure target page is rendered
        double viewH = PageScroller.ViewportHeight;
        var pagePositions = _viewerMediator.PositionsByPage.TryGetValue(pageIdx, out var pp) ? pp : new List<WordPosition>();
        var pathBefore = _viewerMediator.PdfPath;
        await ((IPdfRenderingService)this).GetOrRenderPageAsync(pageIdx, pagePositions);
        if (_viewerMediator.PdfPath != pathBefore) return;

        // Flush dispatcher queue
        await Dispatcher.InvokeAsync(() => { }, System.Windows.Threading.DispatcherPriority.Background);
        if (_viewerMediator.PdfPath != pathBefore) return;
        Log($"ScrollToPosition: dispatcher flushed");

        // Compute scroll target
        double wordContentY = 0;
        bool refined = false;

        PageList.UpdateLayout();

        var targetContainer = PageList.ItemContainerGenerator.ContainerFromIndex(pageIdx) as FrameworkElement;
        if (targetContainer is not null)
        {
            targetContainer.UpdateLayout();
            var imgControl = FindChild<Image>(targetContainer);
            if (imgControl is not null && imgControl.Source is not null && imgControl.ActualHeight > 0)
            {
                Rect pdfRealBounds = GetActualImageRect(imgControl);
                if (pdfRealBounds != Rect.Empty)
                {
                    double wordY = pdfRealBounds.Top + normalizedY * pdfRealBounds.Height;
                    Point relativeWord = imgControl.TransformToAncestor(PageScroller).Transform(new Point(0, wordY));
                    wordContentY = PageScroller.VerticalOffset + relativeWord.Y;
                    refined = true;
                }
            }
        }

        if (!refined)
        {
            double availW = LayoutConstants.AvailWidth(PageScroller.ViewportWidth);
            double accBefore = AccumulatePageHeightBefore(pageIdx);
            var fallbackVm = _viewerMediator.PageViewModels?[pageIdx];
            if (fallbackVm is null) return;
            double offsetWithin = LayoutConstants.WordOffsetWithinItem(availW, fallbackVm.ImagePixelWidth, fallbackVm.ImagePixelHeight, normalizedY);
            wordContentY = accBefore + offsetWithin;
        }

        double target = wordContentY - viewH / 2;
        target = Math.Max(0, Math.Min(target, PageScroller.ScrollableHeight));
        PageScroller.ScrollToVerticalOffset(target);
        Log($"ScrollToPosition: targetPx={target:F1}");
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
        try
        {
            if (PageScroller.ViewportWidth > 0)
            {
                _viewerMediator.BuildPageViewModels();
                PageList.ItemsSource = _viewerMediator.PageViewModels;
                _navigationMediator.GotoInitialPosition();
                FinishLoading();
                _ = Dispatcher.InvokeAsync(async () =>
                {
                    try
                    {
                        Log("BuildOrDeferViewModels: deferred ScrollToPosition(0) at Loaded");
                        if (_viewerMediator.Positions.Count > 0)
                            await ScrollToPosition(0);
                    }
                    catch (Exception ex) { Log($"BuildOrDeferViewModels scroll error: {ex.Message}"); }
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
                    try
                    {
                        _viewerMediator.BuildPageViewModels();
                        PageList.ItemsSource = _viewerMediator.PageViewModels;
                        _navigationMediator.GotoInitialPosition();
                        FinishLoading();
                        _ = Dispatcher.InvokeAsync(async () =>
                        {
                            try
                            {
                                Log("BuildOrDeferViewModels: deferred ScrollToPosition(0) at Loaded (deferred path)");
                                if (_viewerMediator.Positions.Count > 0)
                                    await ScrollToPosition(0);
                            }
                            catch (Exception ex) { Log($"BuildOrDeferViewModels deferred scroll error: {ex.Message}"); }
                        }, DispatcherPriority.Loaded);
                    }
                    catch (Exception ex) { Log($"BuildOrDeferViewModels deferred build error: {ex.Message}"); }
                }
            };
            PageScroller.LayoutUpdated += handler;
        }
        catch (Exception ex) { Log($"BuildOrDeferViewModels error: {ex.Message}"); }
    }

    private void FinishLoading()
    {
        _isLoading = false;
        ResultsList.IsEnabled = true;
    }

    private void ClearViewer()
    {
        _searchMediator.CancelThumbnails();
        _viewerMediator.Clear();
        _navigationMediator.Reset();

        PageList.ItemsSource = null;
        PageScroller.HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled;
        WordsField.Text = "";
    }
}
