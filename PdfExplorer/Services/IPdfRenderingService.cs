using PdfExplorer.Models;

namespace PdfExplorer.Services;

internal interface IPdfRenderingService
{
    Task<PageRenderItem> GetOrRenderPageAsync(int pageIdx, List<WordPosition> positions);
}
