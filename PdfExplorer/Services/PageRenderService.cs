using System.Windows.Threading;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

internal sealed class PageRenderService : IDisposable
{
    private readonly PdfiumPageRenderer _renderer;
    private readonly RenderQueue _queue;
    private readonly Dispatcher _dispatcher;
    private readonly Dictionary<int, List<WordPosition>> _positionsByPage;
    private readonly CancellationTokenSource _cts = new();
    private Task _loopTask = Task.CompletedTask;

    public PageRenderService(
        PdfiumPageRenderer renderer,
        RenderQueue queue,
        Dispatcher dispatcher,
        Dictionary<int, List<WordPosition>> positionsByPage)
    {
        _renderer = renderer;
        _queue = queue;
        _dispatcher = dispatcher;
        _positionsByPage = positionsByPage;
    }

    public void Start()
    {
        _loopTask = RunLoopAsync(_cts.Token);
    }

    private async Task RunLoopAsync(CancellationToken ct)
    {
        while (!ct.IsCancellationRequested)
        {
            try
            {
                var req = await _queue.DequeueAsync(ct);
                if (req is null)
                    continue;

                var positions = _positionsByPage.GetValueOrDefault(req.PageIndex, new List<WordPosition>());

                var raw = _renderer.RenderPageRaw(req.PageIndex, positions);

                if (_dispatcher.HasShutdownStarted)
                    break;

                await _dispatcher.InvokeAsync(() =>
                {
                    var item = PdfiumPageRenderer.CreatePageItem(raw, positions);
                    OnPageRendered?.Invoke(req.MatchIndex, req.PageIndex, item);
                });
            }
            catch (OperationCanceledException)
            {
                break;
            }
            catch (Exception ex)
            {
                Log($"Render error: {ex.GetType().Name}: {ex.Message}");
            }
        }
    }

    public event Action<int, int, PageRenderItem>? OnPageRendered;

    public void Cancel()
    {
        _cts.Cancel();
        _queue.Cancel();
    }

    public void Dispose()
    {
        _cts.Cancel();
        _cts.Dispose();
    }

    private static void Log(string msg) =>
        Console.Error.WriteLine($"[PageRenderService] {msg}");
}