using System.Text.Json.Serialization;

namespace PdfExplorer.Models;

public record CollectionInfo(
    [property: JsonPropertyName("id")] long Id,
    [property: JsonPropertyName("books_folder")] string BooksFolder,
    [property: JsonPropertyName("label")] string Label,
    [property: JsonPropertyName("data_dir")] string DataDir,
    [property: JsonPropertyName("doc_count")] long DocCount,
    [property: JsonPropertyName("last_indexed")] string? LastIndexed,
    [property: JsonPropertyName("created_at")] string CreatedAt
);

public record CollectionStats(
    [property: JsonPropertyName("num_docs")] long NumDocs,
    [property: JsonPropertyName("num_segments")] long NumSegments,
    [property: JsonPropertyName("size_bytes")] long SizeBytes
);
