using System.Text.Json.Serialization;

namespace PdfExplorer.Models;

public record WordPosition(
    [property: JsonPropertyName("page")] int Page,
    [property: JsonPropertyName("x_min")] float XMin,
    [property: JsonPropertyName("y_min")] float YMin,
    [property: JsonPropertyName("x_max")] float XMax,
    [property: JsonPropertyName("y_max")] float YMax
);
