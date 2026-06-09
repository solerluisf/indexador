using PdfExplorer.Services;

// Simple console test to verify pdf_render_thumbnail works from C#
var renderer = new ThumbnailRenderer();
var testPdf = @"C:\Users\Magnesium\Documents\Software\Indexador\Dev\test_pdfs\pattern1_tm_scale.pdf";

Console.WriteLine($"Testing thumbnail render for: {testPdf}");
Console.WriteLine($"File exists: {File.Exists(testPdf)}");

try
{
    var bytes = await renderer.RenderAsync(testPdf, 100, CancellationToken.None);
    if (bytes is not null)
    {
        Console.WriteLine($"Success! Rendered {bytes.Length} bytes.");
        var cacheKey = ThumbnailRenderer.ComputeCacheKey(testPdf);
        Console.WriteLine($"Cache key: {cacheKey}");
    }
    else
    {
        Console.WriteLine("Render returned null (renderer not available or file invalid).");
    }
}
catch (Exception ex)
{
    Console.WriteLine($"Exception: {ex.GetType().Name}: {ex.Message}");
    Console.WriteLine(ex.StackTrace);
}

renderer.Dispose();
Console.WriteLine("Done.");
