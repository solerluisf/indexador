using System.Text.Json.Serialization;

namespace PdfExplorer.Models;

public class AppSettings
{
    public string? TesseractPath { get; set; }
    public string? OcrLanguage { get; set; } = "eng";
    public uint OcrWorkers { get; set; } = 4;
    public uint OcrMaxDim { get; set; } = 3000;

    public ulong RamBuffer { get; set; } = 3_000_000_000;
    public uint IndexerBatchSize { get; set; } = 500;
    public uint CommitInterval { get; set; } = 5000;
    public uint CommitTimeout { get; set; } = 30;
    public uint ExtractWorkers { get; set; } = 8;
    public uint ChannelCapacity { get; set; } = 500;

    [JsonInclude]
    public Dictionary<uint, float> CollectionBoosts { get; set; } = new();

    public string? PathFilter { get; set; }
    public string? BooleanQuery { get; set; }
    public string? ThemeName { get; set; } = "Light";
    public int RenderDpi { get; set; } = 150;
}
