using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Shapes;
using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Views;

public partial class SearchTab : Page
{
    private readonly PdfEngine _engine;
    private readonly PdfPageRenderer _renderer = new(1920);
    private readonly Dictionary<(string, int), PageRenderItem> _globalPageCache = new();
    private int _currentPage;
    private long _totalHits;
    private string _lastQuery = string.Empty;
    private string _currentPdfPath = string.Empty;
    private List<WordPosition> _lastPositions = new();
    private List<int> _matchingPages = new();
    private Dictionary<int, List<WordPosition>> _positionsByPage = new();
    private Dictionary<int, PageRenderItem> _pageCache = new();
    private int _currentMatchIndex;
    private int _totalMatchPages;
    private List<Border?> _pageElements = new();
    private bool _isLoadingNextPage;
    private int _currentPositionIndex = -1;
    private Dictionary<int, List<Rectangle>> _matchHighlightRects = new();
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
            SearchMode.IsEnabled = true;
            SearchBox.Focus();
        }
        else
        {
            _selectedCollId = 0;
            SearchBox.IsEnabled = false;
            SearchButton.IsEnabled = false;
            SearchMode.IsEnabled = false;
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

            ResultsList.ItemsSource = results.Results;

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

    private void OnSearchModeChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_engine is null) return;
        switch (SearchMode.SelectedIndex)
        {
            case 0:
                _engine.FuzzyDistance = 0;
                _engine.StemEnabled = false;
                break;
            case 1:
                _engine.FuzzyDistance = 1;
                _engine.StemEnabled = false;
                break;
            case 2:
                _engine.FuzzyDistance = 0;
                _engine.StemEnabled = true;
                break;
            case 3:
                _engine.FuzzyDistance = 0;
                _engine.StemEnabled = false;
                break;
        }
        _engine.SaveSettings();
    }

    private async void OnNextPage(object sender, RoutedEventArgs e)
    {
        _currentPage++;
        ClearViewer();
        await RunSearch();
    }

    private async void OnPrevPage(object sender, RoutedEventArgs e)
    {
        _currentPage--;
        ClearViewer();
        await RunSearch();
    }

    // ── Document selection + filtered rendering ─────────────────────

    private async void OnResultSelected(object sender, SelectionChangedEventArgs e)
    {
        if (ResultsList.SelectedItem is not SearchResult result)
        {
            Log("OnResultSelected: no selection");
            return;
        }

        var t0 = DateTime.UtcNow;
        Log($"OnResultSelected: id={result.Id}, path={result.Path}, collId={result.CollectionId}, query='{_lastQuery}'");

        ClearViewer();
        _currentPdfPath = result.Path;
        StatusLabel.Text = System.IO.Path.GetFileName(result.Path);

        // Fetch term positions for the current search term
        try
        {
            _lastPositions = _engine.GetTermPositions(
                _selectedCollId,
                result.Id,
                _lastQuery
            );
            var t1 = DateTime.UtcNow;
            Log($"GetTermPositions returned {_lastPositions.Count} positions (took {(t1 - t0).TotalMilliseconds:F0}ms)");
            if (_lastPositions.Count > 0)
                Log($"First position: page={_lastPositions[0].Page}, x_min={_lastPositions[0].XMin}, word_text={_lastPositions[0].WordText}");

            // Populate positions with word text and coordinates
            if (_lastPositions.Count > 0)
            {
                var lines = new List<string>(_lastPositions.Count + 1);
                lines.Add($"Positions ({_lastPositions.Count}):");
                foreach (var p in _lastPositions)
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
        }
        catch (Exception ex)
        {
            Log($"GetTermPositions error: {ex.GetType().Name}: {ex.Message}");
            StatusLabel.Text = $"Position lookup failed: {ex.Message}";
            return;
        }

        var tPos = DateTime.UtcNow;

        // Load PDF
        try
        {
            Log($"Loading PDF: {result.Path}");
            await _renderer.LoadDocumentAsync(result.Path);
            var t2 = DateTime.UtcNow;
            Log($"PDF loaded, page count={_renderer.PageCount} (took {(t2 - tPos).TotalMilliseconds:F0}ms)");
        }
        catch (Exception ex)
        {
            Log($"LoadDocumentAsync error: {ex.GetType().Name}: {ex.Message}");
            StatusLabel.Text = $"Failed to load PDF: {ex.Message}";
            return;
        }

        if (_lastPositions.Count == 0)
        {
            Log("No positions found — re-index the collection to enable filtered viewer");
            StatusLabel.Text += " — re-index to enable filtered viewer";
            return;
        }

        var tPdf = DateTime.UtcNow;

        // Determine which pages match (sorted, 0-based)
        _matchingPages = _lastPositions
            .Select(p => p.Page - 1)
            .Where(p => p >= 0)
            .Distinct()
            .OrderBy(p => p)
            .ToList();

        Log($"Matching pages ({_matchingPages.Count}): [{string.Join(", ", _matchingPages.Select(p => p + 1))}]");

        // Group positions by page
        _positionsByPage = _lastPositions
            .GroupBy(p => p.Page - 1)
            .ToDictionary(g => g.Key, g => g.ToList());

        _totalMatchPages = _matchingPages.Count;

        if (_matchingPages.Count == 0)
        {
            Log("No matching pages");
            StatusLabel.Text += " — no matching pages";
            return;
        }

        // Render only the first match page; remaining pages load on scroll
        _currentMatchIndex = 0;
        _currentPositionIndex = -1;
        _pageElements = new List<Border?>(_matchingPages.Count);
        for (int i = 0; i < _matchingPages.Count; i++)
            _pageElements.Add(null);

        var firstItem = await GetOrRenderPageAsync(_matchingPages[0]);
        var t3 = DateTime.UtcNow;
        Log($"First page rendered (took {(t3 - tPdf).TotalMilliseconds:F0}ms)");
        AddPageToStack(0, firstItem);
        UpdateMatchNav();
        UpdatePositionNav();
        ScrollToMatch(0);

        PageScroller.ScrollChanged += OnPageScroll;
        _isLoadingNextPage = false;

        var tEnd = DateTime.UtcNow;
        Log($"OnResultSelected complete (total {(tEnd - t0).TotalMilliseconds:F0}ms)");
    }

    // ── Lazy rendering ───────────────────────────────────────────────

    private async Task<PageRenderItem> GetOrRenderPageAsync(int pageIdx)
    {
        var cacheKey = (_currentPdfPath, pageIdx);
        if (_globalPageCache.TryGetValue(cacheKey, out var cached))
        {
            _pageCache[pageIdx] = cached;
            return cached;
        }

        if (_pageCache.TryGetValue(pageIdx, out var cachedLocal))
            return cachedLocal;

        Log($"Rendering page {pageIdx + 1} (0-based={pageIdx})");
        StatusLabel.Text = $"Rendering page {pageIdx + 1}...";

        var pagePositions = _positionsByPage.GetValueOrDefault(pageIdx, new List<WordPosition>());
        try
        {
            var item = await _renderer.RenderPageAsync(pageIdx, pagePositions);
            Log($"  rendered: image={(item.PageImage is not null ? $"{item.ImagePixelWidth}x{item.ImagePixelHeight}" : "null")}");
            _pageCache[pageIdx] = item;
            _globalPageCache[cacheKey] = item;
            return item;
        }
        catch (Exception ex)
        {
            Log($"RenderPageAsync error: {ex.GetType().Name}: {ex.Message}");
            var fallback = new PageRenderItem
            {
                PageNumber = pageIdx + 1,
                ImagePixelWidth = 0,
                PdfPageWidth = 0,
                Positions = pagePositions,
            };
            _pageCache[pageIdx] = fallback;
            _globalPageCache[cacheKey] = fallback;
            return fallback;
        }
    }

    private void AddPageToStack(int matchIndex, PageRenderItem item)
    {
        Log($"AddPageToStack: matchIndex={matchIndex}, page={item.PageNumber}, imgSize={item.ImagePixelWidth}x{item.ImagePixelHeight}, pdfSize={item.PdfPageWidth}x{item.PdfPageHeight}, positions={item.Positions.Count}");

        var image = new Image
        {
            Source = item.PageImage,
            Stretch = Stretch.Uniform,
            HorizontalAlignment = HorizontalAlignment.Left,
        };

        if (item.PageImage is null && item.ImagePixelWidth > 0)
            image.Source = null;

        var pageIdx = _matchingPages[matchIndex];
        var pagePositions = _positionsByPage.GetValueOrDefault(pageIdx, new());
        var highlightBase = item.GetHighlightRects(pagePositions).ToList();
        if (highlightBase.Count > 0)
        {
            var first = highlightBase[0];
            Log($"  highlightBase: count={highlightBase.Count}, first=({first.X:F1},{first.Y:F1} {first.Width:F1}x{first.Height:F1})");
        }
        else
        {
            Log($"  highlightBase: EMPTY");
        }
        var rectList = new List<Rectangle>(highlightBase.Count);

        var canvas = new Canvas
        {
            IsHitTestVisible = false,
            Background = Brushes.Transparent,
            HorizontalAlignment = HorizontalAlignment.Left,
            VerticalAlignment = VerticalAlignment.Top,
        };

        foreach (var r in highlightBase)
        {
            var rect = new Rectangle
            {
                Width = r.Width,
                Height = r.Height,
                Fill = new SolidColorBrush(Color.FromArgb(0xCC, 0xFF, 0xE6, 0x00)),
                Stroke = new SolidColorBrush(Color.FromArgb(0xEE, 0xFF, 0xB4, 0x00)),
                StrokeThickness = 1,
                RadiusX = 1,
                RadiusY = 1,
            };
            Canvas.SetLeft(rect, r.X);
            Canvas.SetTop(rect, r.Y);
            canvas.Children.Add(rect);
            rectList.Add(rect);
        }

        _matchHighlightRects[matchIndex] = rectList;

        image.SizeChanged += (s, e) =>
        {
            var aw = image.ActualWidth;
            var ah = image.ActualHeight;
            if (aw <= 0 || ah <= 0) { Log($"  SizeChanged: aw={aw}, ah={ah} — SKIP"); return; }

            var sFactor = aw / item.ImagePixelWidth;
            Log($"  SizeChanged: aw={aw:F1}, ah={ah:F1}, sFactor={sFactor:F4}, canvasChildren={canvas.Children.Count}");

            canvas.Width = aw;
            canvas.Height = ah;

            for (int i = 0; i < canvas.Children.Count; i++)
            {
                var rect = (Rectangle)canvas.Children[i];
                var hb = highlightBase[i];
                rect.Width = Math.Max(hb.Width * sFactor, 6.0);
                rect.Height = Math.Max(hb.Height * sFactor, 6.0);
                Canvas.SetLeft(rect, hb.X * sFactor);
                Canvas.SetTop(rect, hb.Y * sFactor);
            }
        };

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

        var grid = new Grid();
        grid.Children.Add(image);
        grid.Children.Add(canvas);

        var stack = new StackPanel();
        stack.Children.Add(header);
        stack.Children.Add(grid);
        border.Child = stack;
        PageStack.Children.Add(border);
        _pageElements[matchIndex] = border;

        Log($"AddPageToStack: matchIndex={matchIndex}, page={item.PageNumber}");
    }

    // ── Match navigation ────────────────────────────────────────────

    private async void OnPrevMatch(object sender, RoutedEventArgs e)
    {
        if (_currentMatchIndex <= 0) return;
        var prevIdx = _currentMatchIndex - 1;
        _currentMatchIndex = prevIdx;

        if (_pageElements[prevIdx] is null)
        {
            var item = await GetOrRenderPageAsync(_matchingPages[prevIdx]);
            AddPageToStack(prevIdx, item);
        }
        ScrollToMatch(prevIdx);
    }

    private async void OnNextMatch(object sender, RoutedEventArgs e)
    {
        if (_currentMatchIndex >= _totalMatchPages - 1) return;
        var nextIdx = _currentMatchIndex + 1;
        _currentMatchIndex = nextIdx;

        if (_pageElements[nextIdx] is null)
        {
            var item = await GetOrRenderPageAsync(_matchingPages[nextIdx]);
            AddPageToStack(nextIdx, item);
        }
        ScrollToMatch(nextIdx);
    }

    private void ScrollToMatch(int index)
    {
        Log($"ScrollToMatch({index})");
        if (index < 0 || index >= _matchingPages.Count) return;
        _currentMatchIndex = index;

        var element = _pageElements[index];
        element?.BringIntoView();

        UpdateMatchNav();
    }

    private void UpdateMatchNav()
    {
        MatchInfo.Content = _totalMatchPages > 0
            ? $"Match page {_currentMatchIndex + 1} of {_totalMatchPages}"
            : "0 matches";
        PrevMatch.IsEnabled = _currentMatchIndex > 0;
        NextMatch.IsEnabled = _currentMatchIndex < _totalMatchPages - 1;
    }

    private async void OnPageScroll(object sender, ScrollChangedEventArgs e)
    {
        if (_isLoadingNextPage) return;

        var nextIdx = _pageElements.FindIndex(b => b is null);
        if (nextIdx < 0 || nextIdx >= _matchingPages.Count) return;

        // Load next page when close to the bottom of the rendered content
        var remaining = PageScroller.ScrollableHeight - PageScroller.VerticalOffset;
        if (remaining > 400) return;

        _isLoadingNextPage = true;
        try
        {
            var item = await GetOrRenderPageAsync(_matchingPages[nextIdx]);
            AddPageToStack(nextIdx, item);
        }
        finally
        {
            _isLoadingNextPage = false;
        }
    }

    // ── Position (individual match) navigation ─────────────────────

    private void UpdatePositionNav()
    {
        var count = _lastPositions.Count;
        PositionInfo.Content = count > 0 && _currentPositionIndex >= 0
            ? $"{_currentPositionIndex + 1} / {count}"
            : "0 / 0";
        PrevPosition.IsEnabled = count > 0 && _currentPositionIndex > 0;
        NextPosition.IsEnabled = count > 0 && _currentPositionIndex < count - 1;
    }

    private async void OnPrevPosition(object sender, RoutedEventArgs e)
    {
        if (_lastPositions.Count == 0 || _currentPositionIndex <= 0) return;
        _currentPositionIndex--;
        await NavigateToPosition(_currentPositionIndex);
    }

    private async void OnNextPosition(object sender, RoutedEventArgs e)
    {
        if (_lastPositions.Count == 0 || _currentPositionIndex >= _lastPositions.Count - 1) return;
        _currentPositionIndex++;
        await NavigateToPosition(_currentPositionIndex);
    }

    private async Task NavigateToPosition(int posIdx)
    {
        var pos = _lastPositions[posIdx];
        var pageIdx = pos.Page - 1;

        var matchIdx = _matchingPages.IndexOf(pageIdx);
        if (matchIdx < 0) return;

        // Ensure the page is loaded
        if (_pageElements[matchIdx] is null)
        {
            var item = await GetOrRenderPageAsync(pageIdx);
            AddPageToStack(matchIdx, item);
        }

        // Find which position occurrence within the page
        var pagePositions = _positionsByPage[pageIdx];
        var posInPage = pagePositions.IndexOf(pos);

        if (_matchHighlightRects.TryGetValue(matchIdx, out var rects) && posInPage < rects.Count)
        {
            _currentMatchIndex = matchIdx;
            rects[posInPage].BringIntoView();
            UpdateMatchNav();
        }

        UpdatePositionNav();
    }

    // ── Helpers ─────────────────────────────────────────────────────

    private void ClearViewer()
    {
        PageScroller.ScrollChanged -= OnPageScroll;
        PageStack.Children.Clear();
        _pageElements.Clear();
        _pageCache.Clear();
        _matchingPages.Clear();
        _positionsByPage.Clear();
        _lastPositions.Clear();
        _currentMatchIndex = 0;
        _totalMatchPages = 0;
        _isLoadingNextPage = false;
        _currentPositionIndex = -1;
        _matchHighlightRects.Clear();
        PageScroller.HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled;
        MatchInfo.Content = "0 matches";
        PrevMatch.IsEnabled = false;
        NextMatch.IsEnabled = false;
        PositionInfo.Content = "0 / 0";
        PrevPosition.IsEnabled = false;
        NextPosition.IsEnabled = false;
        WordsField.Text = "";
    }
}
