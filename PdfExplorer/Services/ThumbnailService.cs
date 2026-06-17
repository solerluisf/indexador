using System.IO;
using System.Windows.Media;
using System.Windows.Media.Imaging;

namespace PdfExplorer.Services;

public sealed class ThumbnailService : IDisposable
{
    private readonly ThumbnailRenderer _renderer = new();
    private readonly SemaphoreSlim _semaphore = new(4, 4);
    private readonly Dictionary<string, ThumbnailRawResult> _memCache = new();
    private readonly Queue<string> _memCacheOrder = new();
    private readonly object _memCacheLock = new();
    private readonly uint _maxWidth;
    private const int MaxMemCacheEntries = 100;
    private bool _disposed;

    public ThumbnailService(uint maxWidth = 100)
    {
        _maxWidth = maxWidth;
    }

    public async Task<ThumbnailRawResult?> GetThumbnailAsync(string pdfPath, CancellationToken ct)
    {
        if (_disposed)
            throw new ObjectDisposedException(nameof(ThumbnailService));

        var cacheKey = ThumbnailRenderer.ComputeCacheKey(pdfPath);

        // 1. Memory cache
        lock (_memCacheLock)
        {
            if (_memCache.TryGetValue(cacheKey, out var cached))
                return cached;
        }

        // 1b. Disk cache
        var diskPath = ThumbnailRenderer.GetThumbCachePath(pdfPath);
        var fromDisk = ThumbnailRenderer.LoadFromDiskCache(diskPath);
        if (fromDisk is not null)
        {
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
                _memCache[cacheKey] = fromDisk;
            }
            return fromDisk;
        }

        ct.ThrowIfCancellationRequested();

        // 2. Render
        await _semaphore.WaitAsync(ct);
        try
        {
            // Double-check memory cache after acquiring semaphore
            lock (_memCacheLock)
            {
                if (_memCache.TryGetValue(cacheKey, out var cached2))
                    return cached2;
            }

            var raw = await _renderer.RenderAsync(pdfPath, _maxWidth, ct);
            if (raw is null)
                return null;

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

            ThumbnailRenderer.SaveToDiskCache(raw, diskPath);

            return raw;
        }
        catch (OperationCanceledException)
        {
            throw;
        }
        catch (Exception)
        {
            return null;
        }
        finally
        {
            _semaphore.Release();
        }
    }

    public void Dispose()
    {
        if (_disposed) return;
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
