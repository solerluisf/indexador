using Windows.Data.Pdf;
using Windows.Storage;
using Windows.Storage.Streams;
using System.Windows.Media.Imaging;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

public sealed class PdfPageRenderer : IDisposable
{
    private PdfDocument? _document;
    private string? _currentPath;
    private readonly double _targetWidth;

    private static void Log(string msg) => Console.Error.WriteLine($"[PdfPageRenderer] {msg}");

    public PdfPageRenderer(double targetWidth = 900)
    {
        _targetWidth = targetWidth;
        Log($"Created, targetWidth={targetWidth}");
    }

    public async Task LoadDocumentAsync(string pdfPath)
    {
        // Strip \\?\ prefix for WinRT — but keep original for FileStream fallback
        var cleanPath = pdfPath.StartsWith(@"\\?\") ? pdfPath[4..] : pdfPath;

        if (_currentPath == cleanPath && _document is not null)
        {
            Log($"LoadDocumentAsync: already loaded '{cleanPath}'");
            return;
        }

        DisposeDocument();
        Log($"LoadDocumentAsync: loading '{cleanPath}'");

        // Try 1: StorageFile (fast, but may fail with long paths / UNABLE_TO_MASK_PATH)
        try
        {
            var file = await StorageFile.GetFileFromPathAsync(cleanPath);
            Log($"  StorageFile OK: {file.Path}");
            _document = await PdfDocument.LoadFromFileAsync(file);
            Log($"  PdfDocument loaded, pages={_document.PageCount}");
            _currentPath = cleanPath;
            return;
        }
        catch (Exception ex)
        {
            Log($"  StorageFile failed ({ex.GetType().Name}: {ex.Message}) — trying memory stream");
        }

        // Try 2: FileStream + InMemoryRandomAccessStream (handles any path, no WinRT limitations)
        try
        {
            var fileBytes = await System.IO.File.ReadAllBytesAsync(pdfPath);
            Log($"  ReadAllBytesAsync OK, size={fileBytes.Length} bytes");

            var memStream = new InMemoryRandomAccessStream();
            using (var writer = new DataWriter(memStream.GetOutputStreamAt(0)))
            {
                writer.WriteBytes(fileBytes);
                await writer.StoreAsync();
            }
            memStream.Seek(0);

            _document = await PdfDocument.LoadFromStreamAsync(memStream);
            Log($"  PdfDocument loaded from stream, pages={_document.PageCount}");
            _currentPath = cleanPath;
        }
        catch (Exception ex)
        {
            Log($"  Memory stream fallback also failed: {ex.GetType().Name}: {ex.Message}");
            throw;
        }
    }

    public int PageCount => (int)(_document?.PageCount ?? 0);

    public async Task<PageRenderItem> RenderPageAsync(int pageIndex, List<WordPosition> pagePositions)
    {
        if (_document is null)
            throw new InvalidOperationException("No document loaded. Call LoadDocumentAsync first.");

        PdfPage? page = null;
        InMemoryRandomAccessStream? stream = null;

        try
        {
            page = _document.GetPage((uint)pageIndex);
            var pageWidthDips = page.Size.Width;
            var pageHeightDips = page.Size.Height;
            // PdfPage.Size is in DIPs (1/96 inch).  Convert to PDF points (1/72 inch).
            var pageWidth = pageWidthDips * 72.0 / 96.0;
            var pageHeight = pageHeightDips * 72.0 / 96.0;
            Log($"  Page {pageIndex + 1}: size={pageWidthDips}x{pageHeightDips} dips ({pageWidth:F1}x{pageHeight:F1} pts)");

            var t0 = DateTime.UtcNow;
            var dpi = _targetWidth / (pageWidth / 72.0);
            var destWidth = (uint)(pageWidth * dpi / 72.0);
            var destHeight = (uint)(pageHeight * dpi / 72.0);
            Log($"  Render at {destWidth}x{destHeight}px (dpi={dpi:F1})");

            var options = new PdfPageRenderOptions
            {
                DestinationWidth = destWidth,
                DestinationHeight = destHeight,
            };

            stream = new InMemoryRandomAccessStream();
            await page.RenderToStreamAsync(stream, options);
            var t1 = DateTime.UtcNow;
            Log($"  RenderToStreamAsync OK, size={stream.Size} bytes (took {(t1 - t0).TotalMilliseconds:F1}ms)");

            // Read rendered page bytes — single read, single allocation
            stream.Seek(0);
            var reader = new DataReader(stream.GetInputStreamAt(0));
            uint loaded = await reader.LoadAsync((uint)stream.Size);
            var imageBytes = new byte[loaded];
            reader.ReadBytes(imageBytes);
            reader.Dispose();

            var imgWidth = (int)destWidth;
            var imgHeight = (int)destHeight;

            // Load into BitmapImage directly from memory (no temp file)
            var bitmap = new BitmapImage();
            bitmap.BeginInit();
            bitmap.StreamSource = new System.IO.MemoryStream(imageBytes);
            bitmap.CacheOption = BitmapCacheOption.OnLoad;
            bitmap.CreateOptions = BitmapCreateOptions.IgnoreColorProfile;
            bitmap.EndInit();
            bitmap.Freeze();
            var t2 = DateTime.UtcNow;
            Log($"  BitmapImage OK ({imgWidth}x{imgHeight}) (took {(t2 - t1).TotalMilliseconds:F1}ms)");

            return new PageRenderItem
            {
                PageNumber = pageIndex + 1,
                PageImage = bitmap,
                ImagePixelWidth = imgWidth,
                ImagePixelHeight = imgHeight,
                PdfPageWidth = pageWidth,
                PdfPageHeight = pageHeight,
                Positions = pagePositions,
            };
        }
        catch (Exception ex)
        {
            Log($"  RenderPageAsync({pageIndex + 1}) error: {ex.GetType().Name}: {ex.Message}");
            Log($"  Stack: {ex.StackTrace}");
            return new PageRenderItem
            {
                PageNumber = pageIndex + 1,
                PageImage = null,
                ImagePixelWidth = 0,
                ImagePixelHeight = 0,
                PdfPageWidth = page?.Size.Width ?? 0,
                PdfPageHeight = page?.Size.Height ?? 0,
                Positions = pagePositions,
            };
        }
        finally
        {
            stream?.Dispose();
            page?.Dispose();
        }
    }

    private void DisposeDocument()
    {
        if (_document is not null)
        {
            Log("DisposeDocument");
            _document = null;
        }
    }

    public void Dispose()
    {
        DisposeDocument();
        _currentPath = null;
    }
}
