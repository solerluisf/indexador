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

    public PdfPageRenderer(double targetWidth = 800)
    {
        _targetWidth = targetWidth;
        Log($"Created, targetWidth={targetWidth}");
    }

    public async Task LoadDocumentAsync(string pdfPath)
    {
        // Strip \\?\ prefix — WinRT API doesn't handle it
        var cleanPath = pdfPath.StartsWith(@"\\?\") ? pdfPath[4..] : pdfPath;

        if (_currentPath == cleanPath && _document is not null)
        {
            Log($"LoadDocumentAsync: already loaded '{cleanPath}'");
            return;
        }

        DisposeDocument();
        Log($"LoadDocumentAsync: loading '{cleanPath}'");

        try
        {
            var file = await StorageFile.GetFileFromPathAsync(cleanPath);
            Log($"  StorageFile OK: {file.Path}");
            _document = await PdfDocument.LoadFromFileAsync(file);
            Log($"  PdfDocument loaded, pages={_document.PageCount}");
            _currentPath = cleanPath;
        }
        catch (Exception ex)
        {
            Log($"  LoadDocumentAsync error: {ex.GetType().Name}: {ex.Message}");
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
            var pageWidth = page.Size.Width;
            var pageHeight = page.Size.Height;
            Log($"  Page {pageIndex + 1}: size={pageWidth}x{pageHeight} pts");

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
            Log($"  RenderToStreamAsync OK, size={stream.Size} bytes");

            // Read all bytes from the WinRT stream using chunked DataReader
            stream.Seek(0);
            var reader = new DataReader(stream.GetInputStreamAt(0));
            var allBytes = new System.IO.MemoryStream((int)stream.Size);
            uint remaining = (uint)stream.Size;
            while (remaining > 0)
            {
                uint chunkSize = Math.Min(remaining, 65536);
                uint loaded = await reader.LoadAsync(chunkSize);
                if (loaded == 0) break;
                var chunk = new byte[loaded];
                reader.ReadBytes(chunk);
                allBytes.Write(chunk, 0, (int)loaded);
                remaining -= loaded;
            }
            reader.Dispose();

            var imgWidth = (int)destWidth;
            var imgHeight = (int)destHeight;

            // Load into BitmapImage directly from memory (no temp file)
            allBytes.Seek(0, System.IO.SeekOrigin.Begin);
            var bitmap = new BitmapImage();
            bitmap.BeginInit();
            bitmap.StreamSource = allBytes;
            bitmap.CacheOption = BitmapCacheOption.OnLoad;
            bitmap.CreateOptions = BitmapCreateOptions.IgnoreColorProfile;
            bitmap.EndInit();
            bitmap.Freeze();

            Log($"  BitmapImage OK ({imgWidth}x{imgHeight})");

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
