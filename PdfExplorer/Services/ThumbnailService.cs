using System.IO;
using System.Windows.Media;
using System.Windows.Media.Imaging;

namespace PdfExplorer.Services;

/// <summary>
/// Hybrid thumbnail cache (memory) with bounded concurrency.
/// Returns raw BGRA pixels; the UI thread must convert to BitmapImage.
/// </summary>
public sealed class ThumbnailService : IDisposable
{
    private readonly ThumbnailRenderer _renderer = new();
    private readonly SemaphoreSlim _semaphore = new(4, 4); // max 4 concurrent renders
    private readonly Dictionary<string, ThumbnailRawResult> _memCache = new();
    private readonly Queue<string> _memCacheOrder = new();
    private readonly object _memCacheLock = new();
    private readonly uint _maxWidth;
    private const int MaxMemCacheEntries = 100;
    private bool _disposed;

    private static void Log(string msg)
    {
        Console.Error.WriteLine($"[ThumbnailService] {msg}");
        LogHelper.Log("ThumbnailService", msg);
    }

    public ThumbnailService(uint maxWidth = 100)
    {
        Log($"Constructor: maxWidth={maxWidth}");
        _maxWidth = maxWidth;
    }

    /// <summary>
    /// Returns raw BGRA thumbnail data for the given PDF path.
    /// Checks memory → render pipeline.
    /// </summary>
    public async Task<ThumbnailRawResult?> GetThumbnailAsync(string pdfPath, CancellationToken ct)
    {
        if (_disposed)
            throw new ObjectDisposedException(nameof(ThumbnailService));

        Log($"GetThumbnailAsync START: {pdfPath}");

        var cacheKey = ThumbnailRenderer.ComputeCacheKey(pdfPath);

        // 1. Memory cache
        lock (_memCacheLock)
        {
            if (_memCache.TryGetValue(cacheKey, out var cached))
            {
                Log($"GetThumbnailAsync HIT (memory): {cached.Width}x{cached.Height}");
                return cached;
            }
        }
        Log("GetThumbnailAsync MISS (memory)");

        ct.ThrowIfCancellationRequested();

        // 2. Render
        Log($"GetThumbnailAsync STEP: waiting for semaphore (current count={_semaphore.CurrentCount})");
        await _semaphore.WaitAsync(ct);
        Log("GetThumbnailAsync STEP: semaphore acquired");
        try
        {
            // Double-check memory cache after acquiring semaphore
            lock (_memCacheLock)
            {
                if (_memCache.TryGetValue(cacheKey, out var cached2))
                {
                    Log("GetThumbnailAsync HIT (memory) after semaphore");
                    return cached2;
                }
            }

            Log("GetThumbnailAsync STEP: calling renderer...");
            var raw = await _renderer.RenderAsync(pdfPath, _maxWidth, ct);
            if (raw is null)
            {
                Log("GetThumbnailAsync STEP: renderer returned NULL");
                return null;
            }
            Log($"GetThumbnailAsync STEP: renderer returned {raw.Width}x{raw.Height}");

            lock (_memCacheLock)
            {
                if (!_memCache.ContainsKey(cacheKey))
                {
                    _memCacheOrder.Enqueue(cacheKey);
                    while (_memCacheOrder.Count > MaxMemCacheEntries)
                    {
                        var oldest = _memCacheOrder.Dequeue();
                        _memCache.Remove(oldest);
                    }
                }
                _memCache[cacheKey] = raw;
            }

            Log("GetThumbnailAsync SUCCESS");
            return raw;
        }
        catch (OperationCanceledException)
        {
            Log("GetThumbnailAsync CANCELLED");
            throw;
        }
        catch (Exception ex)
        {
            Log($"GetThumbnailAsync EXCEPTION: {ex.GetType().Name}: {ex.Message}");
            return null;
        }
        finally
        {
            _semaphore.Release();
            Log("GetThumbnailAsync STEP: semaphore released");
        }
    }

    public void Dispose()
    {
        if (_disposed) return;
        Log("ThumbnailService.Dispose");
        _disposed = true;
        _renderer.Dispose();
        _semaphore.Dispose();
        lock (_memCacheLock)
        {
            _memCache.Clear();
            _memCacheOrder.Clear();
        }
    }
}
