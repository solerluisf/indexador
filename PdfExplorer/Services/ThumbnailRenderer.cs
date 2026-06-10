using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;

namespace PdfExplorer.Services;

/// <summary>
/// Raw BGRA thumbnail result (no WPF objects — safe for background threads).
/// </summary>
public sealed class ThumbnailRawResult
{
    public byte[] Pixels { get; init; } = Array.Empty<byte>();
    public int Width { get; init; }
    public int Height { get; init; }
    public int Stride { get; init; }
}

/// <summary>
/// Renders PDF thumbnails via the same PDFium pipeline used by the main viewer
/// (<c>pdf_open_document_mem</c> → <c>pdf_render_page_bgra</c>).
/// Avoids the problematic <c>pdf_render_thumbnail</c> path.
/// </summary>
public sealed class ThumbnailRenderer : IDisposable
{
    private const string Dll = "pdf_extractor_capi.dll";
    private bool _disposed;

    private static void Log(string msg)
    {
        Console.Error.WriteLine($"[ThumbnailRenderer] {msg}");
        LogHelper.Log("ThumbnailRenderer", msg);
    }

    // ── C API imports (same ones used by PdfiumPageRenderer) ───────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_open_document_mem(byte[] data, int len);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_document_page_count(int handle);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
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

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern void pdf_free_bitmap(IntPtr pixels);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_close_document(int handle);

    /// <summary>
    /// Renders the first page of the given PDF to raw BGRA pixel data.
    /// Thread-safe — runs on thread pool.
    /// </summary>
    public async Task<ThumbnailRawResult?> RenderAsync(string pdfPath, uint maxWidth, CancellationToken ct)
    {
        if (_disposed)
            throw new ObjectDisposedException(nameof(ThumbnailRenderer));

        Log($"RenderAsync START: path={pdfPath}, maxWidth={maxWidth}");

        return await Task.Run(() =>
        {
            try
            {
                ct.ThrowIfCancellationRequested();

                if (!File.Exists(pdfPath))
                {
                    Log($"RenderAsync ABORT: file not found: {pdfPath}");
                    return null;
                }

                // Read PDF bytes (outside lock — I/O only)
                var pdfBytes = File.ReadAllBytes(pdfPath);
                Log($"RenderAsync: read {pdfBytes.Length} bytes");

                ct.ThrowIfCancellationRequested();

                // Serialize all PDFium calls globally — PDFium is not thread-safe
                // for concurrent open/close/render across different handles.
                Log("RenderAsync: acquiring global PDFium lock");
                lock (PdfiumPageRenderer.GlobalPdfiumLock)
                {
                    Log("RenderAsync: global PDFium lock acquired");

                    // Pin the byte array — FPDF_LoadMemDocument stores a pointer
                    // internally and does NOT copy the data.
                    var dataHandle = GCHandle.Alloc(pdfBytes, GCHandleType.Pinned);
                    try
                    {
                        // Open document
                        var docHandle = pdf_open_document_mem(pdfBytes, pdfBytes.Length);
                        Log($"RenderAsync: pdf_open_document_mem returned {docHandle}");
                        if (docHandle < 0)
                        {
                            Log("RenderAsync ABORT: failed to open document");
                            return null;
                        }

                        try
                        {
                            var pageCount = pdf_document_page_count(docHandle);
                            Log($"RenderAsync: pageCount={pageCount}");
                            if (pageCount == 0)
                            {
                                Log("RenderAsync ABORT: empty PDF");
                                return null;
                            }

                            double dpi = 50.0;

                            Log($"RenderAsync: calling pdf_render_page_bgra(page=0, dpi={dpi})");
                            var rc = pdf_render_page_bgra(
                                docHandle, 0, dpi, null,
                                out var w, out var h, out var stride,
                                out var wPts, out var hPts, out var pixels);

                            Log($"RenderAsync: pdf_render_page_bgra rc={rc}, {w}x{h} stride={stride}");

                            if (rc < 0 || pixels == IntPtr.Zero)
                            {
                                Log("RenderAsync ABORT: render failed");
                                return null;
                            }

                            byte[] buffer;
                            try
                            {
                                var totalBytes = (long)stride * h;
                                if (totalBytes > int.MaxValue)
                                    throw new InvalidOperationException($"Bitmap too large: {stride} * {h}");
                                buffer = new byte[(int)totalBytes];
                                Marshal.Copy(pixels, buffer, 0, buffer.Length);
                                Log($"RenderAsync: copied {buffer.Length} bytes");
                            }
                            finally
                            {
                                pdf_free_bitmap(pixels);
                                Log("RenderAsync: bitmap freed");
                            }

                            return new ThumbnailRawResult
                            {
                                Pixels = buffer,
                                Width = w,
                                Height = h,
                                Stride = stride,
                            };
                        }
                        finally
                        {
                            pdf_close_document(docHandle);
                            Log("RenderAsync: document closed");
                        }
                    }
                    finally
                    {
                        dataHandle.Free();
                        Log("RenderAsync: pinned data freed");
                    }
                }
            }
            catch (OperationCanceledException)
            {
                Log("RenderAsync CANCELLED");
                throw;
            }
            catch (Exception ex)
            {
                Log($"RenderAsync EXCEPTION: {ex.GetType().Name}: {ex.Message}");
                return null;
            }
        }, ct);
    }

    public static string ComputeCacheKey(string pdfPath)
    {
        try
        {
            var lastWrite = File.GetLastWriteTimeUtc(pdfPath);
            var raw = Encoding.UTF8.GetBytes(pdfPath + "|" + lastWrite.Ticks);
            return Convert.ToHexString(SHA256.HashData(raw));
        }
        catch
        {
            var raw = Encoding.UTF8.GetBytes(pdfPath);
            return Convert.ToHexString(SHA256.HashData(raw));
        }
    }

    public void Dispose()
    {
        _disposed = true;
    }
}
