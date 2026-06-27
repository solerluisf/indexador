using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;

namespace PdfExplorer.Services;

public sealed class ThumbnailRawResult
{
    public byte[] Pixels { get; init; } = Array.Empty<byte>();
    public int Width { get; init; }
    public int Height { get; init; }
    public int Stride { get; init; }
}

public sealed class ThumbnailRenderer : IDisposable
{
    private const string Dll = "pdf_extractor_capi.dll";
    private bool _disposed;

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_open_document_mem(byte[] data, int len);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_document_page_count(int handle);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_render_page_bgra_no_invert(
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

    public async Task<ThumbnailRawResult?> RenderAsync(string pdfPath, uint maxWidth, CancellationToken ct)
    {
        if (_disposed)
            throw new ObjectDisposedException(nameof(ThumbnailRenderer));

        return await Task.Run(() =>
        {
            try
            {
                ct.ThrowIfCancellationRequested();

                if (!File.Exists(pdfPath))
                    return null;

                var pdfBytes = File.ReadAllBytes(pdfPath);

                ct.ThrowIfCancellationRequested();

                lock (PdfiumPageRenderer.GlobalPdfiumLock)
                {
                    var dataHandle = GCHandle.Alloc(pdfBytes, GCHandleType.Pinned);
                    try
                    {
                        var docHandle = pdf_open_document_mem(pdfBytes, pdfBytes.Length);
                        if (docHandle < 0)
                            return null;

                        try
                        {
                            var pageCount = pdf_document_page_count(docHandle);
                            if (pageCount == 0)
                                return null;

                            double dpi = 25.0;

                            var rc = pdf_render_page_bgra_no_invert(
                                docHandle, 0, dpi, null,
                                out var w, out var h, out var stride,
                                out var wPts, out var hPts, out var pixels);

                            if (rc < 0 || pixels == IntPtr.Zero)
                                return null;

                            byte[] buffer;
                            try
                            {
                                var totalBytes = (long)stride * h;
                                if (totalBytes > int.MaxValue)
                                    throw new InvalidOperationException($"Bitmap too large: {stride} * {h}");
                                buffer = new byte[(int)totalBytes];
                                Marshal.Copy(pixels, buffer, 0, buffer.Length);
                            }
                            finally
                            {
                                pdf_free_bitmap(pixels);
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
                        }
                    }
                    finally
                    {
                        dataHandle.Free();
                    }
                }
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception)
            {
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

    // ── Disk cache ──────────────────────────────────────────────────

    private static string ThumbCacheDir =>
        Path.Combine(Path.GetTempPath(), "PdfExplorer", "ThumbCache");

    public static string GetThumbCachePath(string pdfPath) =>
        Path.Combine(ThumbCacheDir, ComputeCacheKey(pdfPath) + ".thumb");

    public static void SaveToDiskCache(ThumbnailRawResult raw, string cachePath)
    {
        try
        {
            Directory.CreateDirectory(ThumbCacheDir);
            var tmpPath = cachePath + ".tmp";
            using (var fs = new FileStream(tmpPath, FileMode.Create, FileAccess.Write))
            using (var bw = new BinaryWriter(fs))
            {
                bw.Write(raw.Width);
                bw.Write(raw.Height);
                bw.Write(raw.Stride);
                bw.Write(raw.Pixels.Length);
                bw.Write(raw.Pixels);
            }
            File.Move(tmpPath, cachePath, overwrite: true);
        }
        catch
        {
        }
    }

    public static ThumbnailRawResult? LoadFromDiskCache(string cachePath)
    {
        try
        {
            if (!File.Exists(cachePath)) return null;
            using var fs = new FileStream(cachePath, FileMode.Open, FileAccess.Read, FileShare.Read);
            using var br = new BinaryReader(fs);
            int w = br.ReadInt32();
            int h = br.ReadInt32();
            int stride = br.ReadInt32();
            int len = br.ReadInt32();
            var pixels = br.ReadBytes(len);
            return new ThumbnailRawResult { Width = w, Height = h, Stride = stride, Pixels = pixels };
        }
        catch
        {
            return null;
        }
    }

    public void Dispose()
    {
        _disposed = true;
    }
}
