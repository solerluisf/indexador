using System.Text.Json;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

public static class OverlayHelper
{
    public static List<WordPosition> DeserializePositions(string json)
    {
        if (string.IsNullOrWhiteSpace(json))
            return new List<WordPosition>();
        try
        {
            return JsonSerializer.Deserialize<List<WordPosition>>(json) ?? new List<WordPosition>();
        }
        catch (JsonException)
        {
            return new List<WordPosition>();
        }
    }
}
