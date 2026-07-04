using System.Text.Json;
using System.Text.Json.Serialization;

namespace PdfExplorer.Models;

[JsonConverter(typeof(SearchStrategyConverter))]
public enum SearchStrategy
{
    AutoPhrase,
    BooleanPhrase,
}

public class SearchStrategyConverter : JsonConverter<SearchStrategy>
{
    public override SearchStrategy Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        var str = reader.GetString();
        return str switch
        {
            "auto_phrase" => SearchStrategy.AutoPhrase,
            "boolean_phrase" => SearchStrategy.BooleanPhrase,
            _ => throw new JsonException($"Unknown strategy: {str}"),
        };
    }

    public override void Write(Utf8JsonWriter writer, SearchStrategy value, JsonSerializerOptions options)
    {
        writer.WriteStringValue(value switch
        {
            SearchStrategy.AutoPhrase => "auto_phrase",
            SearchStrategy.BooleanPhrase => "boolean_phrase",
            _ => throw new ArgumentOutOfRangeException(nameof(value)),
        });
    }
}

public class SearchRequestV2
{
    [JsonPropertyName("query")]
    public string Query { get; set; } = string.Empty;

    [JsonPropertyName("strategy")]
    public SearchStrategy Strategy { get; set; } = SearchStrategy.AutoPhrase;

    [JsonPropertyName("limit")]
    public int Limit { get; set; } = 50;

    [JsonPropertyName("offset")]
    public int Offset { get; set; } = 0;

    [JsonPropertyName("path_filter")]
    public string? PathFilter { get; set; }

    [JsonPropertyName("collection_id")]
    public long? CollectionId { get; set; }
}

public record SearchV2Result(
    [property: JsonPropertyName("score")] double Score,
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("snippet")] string? Snippet,
    [property: JsonPropertyName("positions")] IReadOnlyList<PagePositionV2>? Positions,
    [property: JsonPropertyName("matched_terms")] IReadOnlyList<string>? MatchedTerms,
    [property: JsonPropertyName("phrase_groups")] IReadOnlyList<IReadOnlyList<string>>? PhraseGroups
);

public record PagePositionV2(
    [property: JsonPropertyName("page")] int Page,
    [property: JsonPropertyName("x")] float X,
    [property: JsonPropertyName("y")] float Y,
    [property: JsonPropertyName("width")] float Width,
    [property: JsonPropertyName("height")] float Height
);

public record SearchV2Response(
    [property: JsonPropertyName("success")] bool Success,
    [property: JsonPropertyName("total_count")] long TotalCount,
    [property: JsonPropertyName("page")] int Page,
    [property: JsonPropertyName("page_size")] int PageSize,
    [property: JsonPropertyName("query")] string Query,
    [property: JsonPropertyName("strategy")] string Strategy,
    [property: JsonPropertyName("results")] IReadOnlyList<SearchV2Result> Results,
    [property: JsonPropertyName("metadata")] IReadOnlyDictionary<string, object>? Metadata
);
