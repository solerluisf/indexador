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
    private readonly PdfPageRenderer _renderer = new(800);
    private int _currentPage;
    private long _totalHits;
    private string _lastQuery = string.Empty;
    private List<WordPosition> _lastPositions = new();
    private List<PageRenderItem> _renderedPages = new();
    private int _currentMatchIndex;
    private int _totalMatchPages;

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
        Log("Constructor end, engine=" + (_engine is not null ? "ok" : "null"));
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
            var results = _engine.Search(query, limit: 1000, offset: _currentPage * 1000);
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

        Log($"OnResultSelected: id={result.Id}, path={result.Path}, collId={result.CollectionId}, query='{_lastQuery}'");

        ClearViewer();
        StatusLabel.Text = System.IO.Path.GetFileName(result.Path);

        // Fetch term positions for the current search term
        try
        {
            _lastPositions = _engine.GetTermPositions(
                (uint)(result.CollectionId ?? 0),
                result.Id,
                _lastQuery
            );
            Log($"GetTermPositions returned {_lastPositions.Count} positions");
            if (_lastPositions.Count > 0)
                Log($"First position: page={_lastPositions[0].Page}, x_min={_lastPositions[0].XMin}");
        }
        catch (Exception ex)
        {
            Log($"GetTermPositions error: {ex.GetType().Name}: {ex.Message}");
            StatusLabel.Text = $"Position lookup failed: {ex.Message}";
            return;
        }

        // Load PDF
        try
        {
            Log($"Loading PDF: {result.Path}");
            await _renderer.LoadDocumentAsync(result.Path);
            Log($"PDF loaded, page count={_renderer.PageCount}");
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

        // Determine which pages match (sorted, 0-based)
        var matchingPages = _lastPositions
            .Select(p => p.Page - 1)
            .Where(p => p >= 0)
            .Distinct()
            .OrderBy(p => p)
            .ToList();

        Log($"Matching pages ({matchingPages.Count}): [{string.Join(", ", matchingPages.Select(p => p + 1))}]");

        // Group positions by page
        var positionsByPage = _lastPositions
            .GroupBy(p => p.Page - 1)
            .ToDictionary(g => g.Key, g => g.ToList());

        _renderedPages = new List<PageRenderItem>(matchingPages.Count);
        foreach (var pageIdx in matchingPages)
        {
            var pagePositions = positionsByPage.GetValueOrDefault(pageIdx, new List<WordPosition>());
            Log($"Rendering page {pageIdx + 1} (0-based={pageIdx}), positions={pagePositions.Count}");
            try
            {
                var item = await _renderer.RenderPageAsync(pageIdx, pagePositions);
                Log($"  rendered: image={(item.PageImage is not null ? $"{item.ImagePixelWidth}x{item.ImagePixelHeight}" : "null")}");
                _renderedPages.Add(item);
            }
            catch (Exception ex)
            {
                Log($"  RenderPageAsync error: {ex.GetType().Name}: {ex.Message}");
                _renderedPages.Add(new PageRenderItem
                {
                    PageNumber = pageIdx + 1,
                    ImagePixelWidth = 0,
                    PdfPageWidth = 0,
                    Positions = pagePositions,
                });
            }
        }

        if (_renderedPages.Count == 0)
        {
            Log("No pages rendered successfully");
            StatusLabel.Text += " — failed to render any pages";
            return;
        }

        Log($"Building page view with {_renderedPages.Count} pages");
        BuildPageView();

        _totalMatchPages = _renderedPages.Count;
        _currentMatchIndex = 0;
        UpdateMatchNav();
        ScrollToMatch(0);
        Log("OnResultSelected complete");
    }

    private void BuildPageView()
    {
        PageStack.Children.Clear();

        foreach (var item in _renderedPages)
        {
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

            var image = new Image
            {
                Source = item.PageImage,
                Stretch = Stretch.None,
                HorizontalAlignment = HorizontalAlignment.Left,
            };

            // Fallback text when image is null
            if (item.PageImage is null && item.ImagePixelWidth > 0)
                image.Source = null;

            var canvas = new Canvas
            {
                Width = item.ImagePixelWidth,
                Height = item.ImagePixelHeight,
                IsHitTestVisible = false,
                Background = Brushes.Transparent,
            };

            // Add highlight rectangles
            foreach (var rect in item.GetHighlightRects())
            {
                var r = new Rectangle
                {
                    Width = rect.Width,
                    Height = rect.Height,
                    Fill = new SolidColorBrush(Color.FromArgb(0x60, 0xFF, 0xE6, 0x00)),
                    Stroke = new SolidColorBrush(Color.FromArgb(0x99, 0xFF, 0xB4, 0x00)),
                    StrokeThickness = 1,
                    RadiusX = 1,
                    RadiusY = 1,
                };
                Canvas.SetLeft(r, rect.X);
                Canvas.SetTop(r, rect.Y);
                canvas.Children.Add(r);
            }

            var grid = new Grid();
            grid.Children.Add(image);
            grid.Children.Add(canvas);

            var stack = new StackPanel();
            stack.Children.Add(header);
            stack.Children.Add(grid);
            border.Child = stack;
            PageStack.Children.Add(border);
        }

        var pageNums = string.Join(", ", _renderedPages.Select(p => p.PageNumber));
        Log($"BuildPageView: added {_renderedPages.Count} items to PageStack [{pageNums}]");
    }

    // ── Match navigation ────────────────────────────────────────────

    private void OnPrevMatch(object sender, RoutedEventArgs e)
    {
        if (_currentMatchIndex > 0)
            ScrollToMatch(_currentMatchIndex - 1);
    }

    private void OnNextMatch(object sender, RoutedEventArgs e)
    {
        if (_currentMatchIndex < _totalMatchPages - 1)
            ScrollToMatch(_currentMatchIndex + 1);
    }

    private void ScrollToMatch(int index)
    {
        Log($"ScrollToMatch({index})");
        if (index < 0 || index >= _renderedPages.Count) return;
        _currentMatchIndex = index;

        if (index < PageStack.Children.Count)
        {
            var element = PageStack.Children[index] as FrameworkElement;
            element?.BringIntoView();
        }

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

    // ── Helpers ─────────────────────────────────────────────────────

    private void ClearViewer()
    {
        PageStack.Children.Clear();
        _renderedPages.Clear();
        _lastPositions.Clear();
        _currentMatchIndex = 0;
        _totalMatchPages = 0;
        MatchInfo.Content = "0 matches";
        PrevMatch.IsEnabled = false;
        NextMatch.IsEnabled = false;
    }
}
