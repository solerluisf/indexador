using System.Text.Json.Serialization;

namespace PdfExplorer.Models;

public record ProblematicJob(
    [property: JsonPropertyName("id")] long Id,
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("ocr_flag")] bool OcrFlag,
    [property: JsonPropertyName("error")] string? Error,
    [property: JsonPropertyName("no_positions")] bool NoPositions
)
{
    public string FileName => System.IO.Path.GetFileName(Path);
    public string FolderPath => System.IO.Path.GetDirectoryName(Path) ?? "";
    public string Issue => Error ?? (NoPositions
        ? (OcrFlag ? "OCR sin posiciones" : "Sin posiciones de palabra")
        : Status);
}
