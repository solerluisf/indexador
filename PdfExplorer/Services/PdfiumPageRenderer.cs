using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

public sealed class PdfiumPageRenderer : IDisposable
{
    // PDFium is not thread-safe for concurrent open/close/render across
    // different document handles. All native calls must be serialized.
    internal static readonly object GlobalPdfiumLock = new();

    private readonly object _lock = new();
    private int _docHandle = -1;
    private int _pageCount;
    private readonly double _targetDpi;
    private GCHandle? _pinnedPdfData;
    private readonly ConcurrentDictionary<int, byte[]> _highlightJsonCache = new();
    private readonly ConcurrentDictionary<int, (double w, double h)> _pageDimsCache = new();

    public double TargetDpi => _targetDpi;

    // ── C API imports ─────────────────────────────────────────────

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_open_document(byte[] path);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_open_document_mem(byte[] data, int len);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_document_page_count(int handle);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_render_page_bgra(
        int handle,
        int pageIndex,
        double dpi,
        byte[]? highlightJson,
        out int outWidth,
        out int outHeight,
        out int outStride,
        out double outWidthPts,
        out double outHeightPts,
        out IntPtr outPixels);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern void pdf_free_bitmap(IntPtr pixels);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_close_document(int handle);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_get_page_dimensions(
        int handle, int pageIndex,
        out double outWidthPts, out double outHeightPts);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_render_page_to_buffer(
        int handle,
        int pageIndex,
        double dpi,
        byte[]? highlightJson,
        IntPtr buffer,
        int width,
        int height,
        int stride,
        out double outWidthPts,
        out double outHeightPts);

    // ── Constructor ───────────────────────────────────────────────

    public PdfiumPageRenderer(double targetDpi = 150)
    {
        _targetDpi = targetDpi;
        Log($"Created, targetDpi={targetDpi}");
    }

    // ── Document loading ──────────────────────────────────────────

    public void OpenDocument(string pdfPath)
    {
        lock (_lock)
        {
            // Always close previous document before opening a new one (U2 fix).
            if (_docHandle >= 0)
            {
                Log($"OpenDocument: closing previous handle={_docHandle}");
                UnpinPdfData();
                lock (GlobalPdfiumLock)
                {
                    pdf_close_document(_docHandle);
                }
                _docHandle = -1;
                _pageCount = 0;
                _highlightJsonCache.Clear();
                _pageDimsCache.Clear();
            }

            // Read file inside the lock to avoid TOCTOU race and redundant reads.
            var pdfBytes = System.IO.File.ReadAllBytes(pdfPath);

            // Pin the byte array — FPDF_LoadMemDocument stores a pointer to the data
            // internally and does NOT copy it. The GC must not move the array.
            _pinnedPdfData = GCHandle.Alloc(pdfBytes, GCHandleType.Pinned);

            Log($"OpenDocument: '{pdfPath}' ({pdfBytes.Length} bytes)");
            int rc;
            lock (GlobalPdfiumLock)
            {
                rc = pdf_open_document_mem(pdfBytes, pdfBytes.Length);
            }
            Log($"  pdf_open_document_mem returned {rc}");

            if (rc < 0)
            {
                UnpinPdfData();
                throw new InvalidOperationException($"Failed to open PDF: {pdfPath} (error {rc})");
            }

            _docHandle = rc;
            int count;
            lock (GlobalPdfiumLock)
            {
                count = pdf_document_page_count(_docHandle);
            }
            _pageCount = count;
            Log($"  Document opened, handle={_docHandle}, pages={_pageCount}");
        }
    }

    public int GetPageCount()
    {
        lock (_lock) { return _pageCount; }
    }

    public (double WidthPts, double HeightPts) GetPageDimensions(int pageIndex)
    {
        if (_pageDimsCache.TryGetValue(pageIndex, out var cached))
            return cached;

        lock (_lock)
        {
            if (_docHandle < 0)
                throw new InvalidOperationException("No document open. Call OpenDocument first.");

            // Double-check cache after acquiring lock
            if (_pageDimsCache.TryGetValue(pageIndex, out cached))
                return cached;

            lock (GlobalPdfiumLock)
            {
                int rc = pdf_get_page_dimensions(
                    _docHandle, pageIndex, out double w, out double h);
                if (rc < 0)
                    throw new InvalidOperationException(
                        $"Failed to get page {pageIndex} dimensions (rc={rc})");
                _pageDimsCache[pageIndex] = (w, h);
                return (w, h);
            }
        }
    }

    [Obsolete("Use GetPageCount() instead")]
    public int PageCount => GetPageCount();

    /// <summary>
    /// Opens a PDF document from a byte array (already read from disk).
    /// Use this overload to avoid reading the file twice when the caller
    /// already has the bytes in memory.
    /// </summary>
    public void OpenDocument(byte[] pdfData, string? debugPath = null)
    {
        lock (_lock)
        {
            if (_docHandle >= 0)
            {
                Log($"OpenDocument: closing previous handle={_docHandle}");
                UnpinPdfData();
                lock (GlobalPdfiumLock)
                {
                    pdf_close_document(_docHandle);
                }
                _docHandle = -1;
                _pageCount = 0;
                _highlightJsonCache.Clear();
                _pageDimsCache.Clear();
            }

            // Pin the byte array — FPDF_LoadMemDocument stores a pointer internally
            // and does NOT copy the data. The GC must not move the array while
            // the document is open.
            _pinnedPdfData = GCHandle.Alloc(pdfData, GCHandleType.Pinned);

            Log($"OpenDocument: '{debugPath ?? "(bytes)"}' ({pdfData.Length} bytes)");
            int rc;
            lock (GlobalPdfiumLock)
            {
                rc = pdf_open_document_mem(pdfData, pdfData.Length);
            }
            Log($"  pdf_open_document_mem returned {rc}");

            if (rc < 0)
            {
                UnpinPdfData();
                throw new InvalidOperationException($"Failed to open PDF (error {rc})");
            }

            _docHandle = rc;
            int count;
            lock (GlobalPdfiumLock)
            {
                count = pdf_document_page_count(_docHandle);
            }
            _pageCount = count;
            Log($"  Document opened, handle={_docHandle}, pages={_pageCount}");
        }
    }

    // ── Raw page rendering (thread-safe, no WPF objects) ─────────

    /// <summary>
    /// Renders a page to raw BGRA pixel data. Thread-safe — no WPF objects created.
    /// The returned <see cref="RenderRawResult"/> must be converted to a <see cref="PageRenderItem"/>
    /// on the UI thread via <see cref="CreatePageItem"/>.
    /// </summary>
    public RenderRawResult RenderPageRaw(int pageIndex, List<WordPosition> pagePositions)
    {
        var t0 = DateTime.UtcNow;
        Log($"RenderPageRaw: page={pageIndex + 1}, dpi={_targetDpi}");

        // Cache the serialized highlight JSON per page — positions don't change
        // during the document session, and serialization allocates a temporary
        // string + byte array for every render.
        var highlightJson = pagePositions.Count > 0
            ? _highlightJsonCache.GetOrAdd(pageIndex, _ => Utf8Bytes(JsonSerializer.Serialize(pagePositions)))
            : null;

        int handle, w, h, stride;
        double wPts, hPts;
        IntPtr pixels;

        // Lock covers the native call to prevent CloseDocument from closing
        // the handle while we use it (use-after-free race, U2).
        lock (_lock)
        {
            handle = _docHandle;
            if (handle < 0)
                throw new InvalidOperationException("No document open. Call OpenDocument first.");

            int rc;
            lock (GlobalPdfiumLock)
            {
                rc = pdf_render_page_bgra(
                    handle, pageIndex, _targetDpi, highlightJson,
                    out w, out h, out stride, out wPts, out hPts, out pixels);
            }

            var t1 = DateTime.UtcNow;
            Log($"  pdf_render_page_bgra rc={rc}, {w}x{h} stride={stride} pts={wPts:F1}x{hPts:F1} (took {(t1 - t0).TotalMilliseconds:F1}ms)");

            if (rc < 0 || pixels == IntPtr.Zero)
            {
                Log($"  Render FAILED (rc={rc})");
                return new RenderRawResult
                {
                    PageIndex = pageIndex, Width = 0, Height = 0, Stride = 0,
                    WidthPts = 0, HeightPts = 0, Pixels = [], Success = false,
                };
            }
        }

        // Copy native pixels to managed buffer outside the lock.
        // Use try-finally to ensure pdf_free_bitmap is called even if Marshal.Copy throws (U8).
        byte[] buffer;
        try
        {
            var totalBytes = (long)stride * h;
            if (totalBytes > int.MaxValue)
                throw new InvalidOperationException($"Bitmap too large: {stride} * {h} = {totalBytes}");
            buffer = new byte[(int)totalBytes];
            Marshal.Copy(pixels, buffer, 0, buffer.Length);
        }
        finally
        {
            pdf_free_bitmap(pixels);
        }

        var t2 = DateTime.UtcNow;
        Log($"  Pixel copy OK ({w}x{h}) (took {(t2 - t0).TotalMilliseconds:F1}ms)");

        return new RenderRawResult
        {
            PageIndex = pageIndex, Width = w, Height = h, Stride = stride,
            WidthPts = wPts, HeightPts = hPts, Pixels = buffer, Success = true,
        };
    }

    // ── Direct-to-buffer rendering (zero-copy) ────────────────────

    /// <summary>
    /// Renders a page directly into a caller-allocated buffer (pinned byte[] or
    /// WriteableBitmap.BackBuffer). Zero-copy — Rust writes directly into the
    /// buffer, no Marshal.Copy, no internal bitmap allocation.
    /// Must be called from a background thread (not the UI thread).
    /// </summary>
    public void RenderToBuffer(int pageIndex, List<WordPosition> pagePositions,
        IntPtr buffer, int width, int height, int stride,
        out double wPts, out double hPts)
    {
        var t0 = DateTime.UtcNow;
        Log($"RenderToBuffer: page={pageIndex + 1}, {width}x{height} stride={stride}");

        // Cache the serialized highlight JSON per page
        var highlightJson = pagePositions.Count > 0
            ? _highlightJsonCache.GetOrAdd(pageIndex, _ => Utf8Bytes(JsonSerializer.Serialize(pagePositions)))
            : null;

        int rc;
        lock (_lock)
        {
            if (_docHandle < 0)
                throw new InvalidOperationException("No document open. Call OpenDocument first.");

            lock (GlobalPdfiumLock)
            {
                rc = pdf_render_page_to_buffer(
                    _docHandle, pageIndex, _targetDpi, highlightJson,
                    buffer, width, height, stride,
                    out wPts, out hPts);
            }
        }

        var t1 = DateTime.UtcNow;
        Log($"  pdf_render_page_to_buffer rc={rc}, wPts={wPts:F1}, hPts={hPts:F1} (took {(t1 - t0).TotalMilliseconds:F1}ms)");

        if (rc < 0)
            throw new InvalidOperationException($"RenderToBuffer failed (rc={rc})");
    }

    // ── Bitmap creation (must be called on UI thread) ─────────────

    /// <summary>
    /// Converts a <see cref="RenderRawResult"/> into a <see cref="PageRenderItem"/>
    /// with a frozen <see cref="WriteableBitmap"/>. Must be called on the UI thread.
    /// </summary>
    public static PageRenderItem CreatePageItem(RenderRawResult raw, List<WordPosition> pagePositions)
    {
        if (!raw.Success || raw.Width <= 0 || raw.Height <= 0)
        {
            return new PageRenderItem
            {
                PageNumber = raw.PageIndex + 1,
                PageImage = null,
                ImagePixelWidth = 0,
                ImagePixelHeight = 0,
                PdfPageWidth = 0,
                PdfPageHeight = 0,
                Positions = pagePositions,
            };
        }

        // BitmapSource.Create + Freeze is faster than WriteableBitmap for
        // read-only images: no Lock/Unlock/Marshal.Copy overhead.
        var bitmap = BitmapSource.Create(
            raw.Width, raw.Height,
            96.0, 96.0,
            PixelFormats.Bgra32,
            null,
            raw.Pixels,
            raw.Stride);
        bitmap.Freeze();

        return new PageRenderItem
        {
            PageNumber = raw.PageIndex + 1,
            PageImage = bitmap,
            ImagePixelWidth = raw.Width,
            ImagePixelHeight = raw.Height,
            PdfPageWidth = raw.WidthPts,
            PdfPageHeight = raw.HeightPts,
            Positions = pagePositions,
        };
    }

    // ── Synchronous convenience (backward compat, must be on STA thread) ──

    /// <summary>
    /// Renders a page and returns a <see cref="PageRenderItem"/> with a frozen bitmap.
    /// Calls <see cref="RenderPageRaw"/> then <see cref="CreatePageItem"/> on the same thread.
    /// For new code, prefer calling <see cref="RenderPageRaw"/> from a background thread
    /// and <see cref="CreatePageItem"/> back on the UI thread.
    /// </summary>
    public PageRenderItem RenderPage(int pageIndex, List<WordPosition> pagePositions)
    {
        var raw = RenderPageRaw(pageIndex, pagePositions);
        return CreatePageItem(raw, pagePositions);
    }

    // ── Cleanup ───────────────────────────────────────────────────

    public void CloseDocument()
    {
        lock (_lock)
        {
            _highlightJsonCache.Clear();
            _pageDimsCache.Clear();

            if (_docHandle < 0)
            {
                UnpinPdfData();
                return;
            }

            Log($"CloseDocument: handle={_docHandle}");
            lock (GlobalPdfiumLock)
            {
                pdf_close_document(_docHandle);
            }
            _docHandle = -1;
            _pageCount = 0;
            UnpinPdfData();
        }
    }

    public void Dispose()
    {
        CloseDocument();
    }

    private void UnpinPdfData()
    {
        if (_pinnedPdfData.HasValue)
        {
            _pinnedPdfData.Value.Free();
            _pinnedPdfData = null;
        }
    }

    // ── Helpers ───────────────────────────────────────────────────

    private static void Log(string msg) =>
        Console.Error.WriteLine($"[PdfiumPageRenderer] {msg}");

    private static byte[] Utf8Bytes(string s)
    {
        var bytes = System.Text.Encoding.UTF8.GetBytes(s);
        var result = new byte[bytes.Length + 1];
        Buffer.BlockCopy(bytes, 0, result, 0, bytes.Length);
        return result;
    }
}

// ── Raw render result (no WPF objects) ────────────────────────────

public readonly record struct RenderRawResult
{
    public int PageIndex { get; init; }
    public int Width { get; init; }
    public int Height { get; init; }
    public int Stride { get; init; }
    public double WidthPts { get; init; }
    public double HeightPts { get; init; }
    public byte[] Pixels { get; init; }
    public bool Success { get; init; }
}
