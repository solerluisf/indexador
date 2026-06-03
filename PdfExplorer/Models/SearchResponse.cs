using System.Text.Json.Serialization;

namespace PdfExplorer.Models;

public record SearchResult(
    [property: JsonPropertyName("id")] long Id,
    [property: JsonPropertyName("score")] double Score,
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("snippet")] string Snippet,
    [property: JsonPropertyName("collection_id")] long? CollectionId
)
{
    public string FileName => System.IO.Path.GetFileName(Path);
    public string FolderPath => System.IO.Path.GetDirectoryName(Path) ?? "";
}

public record SearchResponse(
    [property: JsonPropertyName("total")] long Total,
    [property: JsonPropertyName("results")] IReadOnlyList<SearchResult> Results
);
