using System.Buffers;
using System.Collections.Concurrent;
using System.Collections.ObjectModel;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using PdfExplorer.Models;
using PdfExplorer.ViewModels;

namespace PdfExplorer.Services;

public sealed class ViewerMediator : IViewerMediator
{
    private readonly PdfiumPageRenderer _renderer = new(150);
    private readonly Dictionary<(string, int), PageRenderItem> _globalPageCache = new();
    private readonly Queue<(string, int)> _globalPageCacheOrder = new();
    private readonly object _globalCacheLock = new();
    private const int MaxGlobalCacheEntries = 50;
    private readonly object _renderCacheLock = new();
    private readonly ConcurrentDictionary<int, Task<PageRenderItem>> _pendingRenders = new();
    private CancellationTokenSource? _selectionCts;

    private string _pdfPath = string.Empty;
    private List<WordPosition> _positions = new();
    private List<int> _matchingPages = new();
    private Dictionary<int, List<WordPosition>> _positionsByPage = new();
    private Dictionary<int, PageRenderItem> _pageCache = new();
    private ObservableCollection<PdfPageViewModel>? _pageViewModels;
    private string _positionsDebugText = string.Empty;

    public event EventHandler<ViewerStateChangedEventArgs>? StateChanged;
    public event EventHandler<EventArgs>? ViewModelsBuilt;
    public event EventHandler<EventArgs>? Cleared;

    public PdfiumPageRenderer Renderer => _renderer;
    public string PdfPath => _pdfPath;
    public CancellationToken CurrentRenderToken => _selectionCts?.Token ?? CancellationToken.None;
    public IReadOnlyList<WordPosition> Positions => _positions;
    public IReadOnlyList<int> MatchingPages => _matchingPages;
    public IReadOnlyDictionary<int, List<WordPosition>> PositionsByPage => _positionsByPage;
    public ObservableCollection<PdfPageViewModel>? PageViewModels => _pageViewModels;
    public string PositionsDebugText => _positionsDebugText;

    // ── Document ──────────────────────────────────────────────────

    public void OpenDocument(byte[] pdfBytes, string path)
    {
        _pdfPath = path;
        _renderer.OpenDocument(pdfBytes, path);
    }

    public void CloseDocument()
    {
        try
        {
            _renderer.CloseDocument();
        }
        catch { }
    }

    // ── Positions ─────────────────────────────────────────────────

    public async Task<List<WordPosition>> FetchPositionsAsync(PdfEngine engine, uint collId, long docId, string query)
    {
        var positions = await Task.Run(() => engine.GetTermPositions(collId, docId, query));
        var before = positions.Count;
        positions = positions
            .OrderBy(p => p.Page)
            .ThenBy(p => p.YMax)
            .DistinctBy(p => (p.Page, p.XMin, p.YMin, p.XMax, p.YMax))
            .ToList();
        var dupes = before - positions.Count;
        Log($"GetTermPositions returned {positions.Count} positions ({dupes} dupes removed)");
        return positions;
    }

    public void SetPositions(List<WordPosition> positions, INavigationMediator navMediator, string? query = null, bool isBooleanMode = false)
    {
        _positions = positions ?? new List<WordPosition>();

        _matchingPages = _positions
            .Select(p => p.Page - 1)
            .Where(p => p >= 0)
            .Distinct()
            .OrderBy(p => p)
            .ToList();

        if (_matchingPages.Count == 0)
            _matchingPages = new List<int> { 0 };

        _positionsByPage = _positions
            .GroupBy(p => p.Page - 1)
            .ToDictionary(g => g.Key, g => g.ToList());

        // Build debug text
        if (_positions.Count > 0)
        {
            var lines = new List<string>(_positions.Count + 1);
            lines.Add($"Positions ({_positions.Count}):");
            foreach (var p in _positions)
            {
                var word = string.IsNullOrWhiteSpace(p.WordText) ? "?" : p.WordText;
                lines.Add($"  p{p.Page} \"{word}\" ({p.XMin:F1},{p.YMin:F1})-({p.XMax:F1},{p.YMax:F1})");
            }
            _positionsDebugText = string.Join("\n", lines);
        }
        else
        {
            _positionsDebugText = string.Empty;
        }

        navMediator.SetContext(_positions, _matchingPages, query, isBooleanMode);

        StateChanged?.Invoke(this, new ViewerStateChangedEventArgs
        {
            PdfPath = _pdfPath,
            Positions = _positions,
            MatchingPages = _matchingPages,
        });
    }

    // ── ViewModels ────────────────────────────────────────────────

    public void BuildPageViewModels()
    {
        var list = new List<PdfPageViewModel>(_matchingPages.Count);

        for (int i = 0; i < _matchingPages.Count; i++)
        {
            int pageIdx = _matchingPages[i];
            _positionsByPage.TryGetValue(pageIdx, out var pos);

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

        _pageViewModels = new ObservableCollection<PdfPageViewModel>(list);
        ViewModelsBuilt?.Invoke(this, EventArgs.Empty);
    }

    // ── Render ────────────────────────────────────────────────────

    public Task<PageRenderItem> GetOrRenderPageAsync(int pageIdx, List<WordPosition> pagePositions, CancellationToken ct)
    {
        ct = _selectionCts?.Token ?? CancellationToken.None;
        try
        {
            return _pendingRenders.GetOrAdd(pageIdx, idx => RenderPageInternalAsync(idx, pagePositions, ct));
        }
        finally
        {
            _pendingRenders.TryRemove(pageIdx, out _);
        }
    }

    private async Task<PageRenderItem> RenderPageInternalAsync(int pageIdx, List<WordPosition> pagePositions, CancellationToken ct)
    {
        var cacheKey = (_pdfPath, pageIdx);

        lock (_renderCacheLock)
        {
            if (_globalPageCache.TryGetValue(cacheKey, out var cached))
            {
                if (!_pageCache.ContainsKey(pageIdx))
                    _pageCache[pageIdx] = cached;
                return cached;
            }

            if (_pageCache.TryGetValue(pageIdx, out var cachedLocal))
                return cachedLocal;
        }

        ct.ThrowIfCancellationRequested();

        Log($"Rendering page {pageIdx + 1}");

        try
        {
            var (wPts, hPts) = _renderer.GetPageDimensions(pageIdx);
            int pixW = (int)(wPts * _renderer.TargetDpi / 72.0);
            int pixH = (int)(hPts * _renderer.TargetDpi / 72.0);
            int stride = pixW * 4;

            int bufferSize = stride * pixH;
            var buffer = ArrayPool<byte>.Shared.Rent(bufferSize);
            try
            {
                var pin = GCHandle.Alloc(buffer, GCHandleType.Pinned);
                try
                {
                    IntPtr ptr = pin.AddrOfPinnedObject();

                    await Task.Run(() =>
                    {
                        _renderer.RenderToBuffer(pageIdx, pagePositions,
                            ptr, pixW, pixH, stride,
                            out var _, out var _);
                    }, ct);

                    ct.ThrowIfCancellationRequested();

                    var bitmap = BitmapSource.Create(
                        pixW, pixH, 96, 96,
                        PixelFormats.Bgra32, null,
                        buffer, stride);
                    bitmap.Freeze();

                    var item = new PageRenderItem
                    {
                        PageNumber = pageIdx + 1,
                        PageImage = bitmap,
                        ImagePixelWidth = pixW,
                        ImagePixelHeight = pixH,
                        PdfPageWidth = wPts,
                        PdfPageHeight = hPts,
                        Positions = pagePositions,
                    };

                    lock (_renderCacheLock)
                    {
                        _pageCache[pageIdx] = item;
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
                Positions = pagePositions,
            };
            lock (_renderCacheLock)
            {
                _pageCache[pageIdx] = fallback;
            }
            AddToGlobalCache(cacheKey, fallback);
            return fallback;
        }
    }

    // ── Cache ─────────────────────────────────────────────────────

    public void InvalidateAllPages()
    {
        lock (_renderCacheLock)
        {
            _pageCache.Clear();
        }
    }

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

    // ── Lifecycle ─────────────────────────────────────────────────

    public void Clear()
    {
        _selectionCts?.Cancel();
        _selectionCts = null;

        _pendingRenders.Clear();

        _pdfPath = string.Empty;
        _positions = new List<WordPosition>();
        _matchingPages = new List<int>();
        _positionsByPage = new Dictionary<int, List<WordPosition>>();
        _pageViewModels = null;
        _pageCache = new Dictionary<int, PageRenderItem>();
        _positionsDebugText = string.Empty;

        lock (_globalCacheLock)
        {
            _globalPageCache.Clear();
            _globalPageCacheOrder.Clear();
        }

        CloseDocument();

        Cleared?.Invoke(this, EventArgs.Empty);
    }

    public CancellationTokenSource? CreateSelectionCts()
    {
        _selectionCts = new CancellationTokenSource();
        return _selectionCts;
    }

    public void Dispose()
    {
        CloseDocument();
    }

    private static void Log(string msg)
    {
        Console.Error.WriteLine($"[ViewerMediator] {msg}");
        LogHelper.Log("ViewerMediator", msg);
    }
}
