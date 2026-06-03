using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

public sealed class PdfEngine : IDisposable
{
    private const string Dll = "pdf_extractor_capi.dll";

    private static void Log(string msg)
    {
        Console.Error.WriteLine($"[PdfEngine] {msg}");
        LogHelper.Log("PdfEngine", msg);
    }

    // ── Lifecycle ──────────────────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern uint pdf_api_version();

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_create_registry(byte[] registryDir);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_list_collections(byte[] outJson, ref uint outLen);

    public PdfEngine(string registryDir)
    {
        Log($"Calling PdfEngine({registryDir})");
        var ver = pdf_api_version();
        if (ver != 1)
            throw new InvalidOperationException($"Unsupported API version: {ver}");

        var rc = pdf_create_registry(Utf8(registryDir));
        if (rc != 0)
            ThrowOnError(rc, this);

        Collections = ListCollections();
        Log("PdfEngine done");
    }

    public IReadOnlyList<CollectionInfo> Collections { get; private set; }

    // ── Collections ────────────────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_add_collection(byte[] booksFolder);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_remove_collection(uint collId);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_collection_stats(uint collId, byte[] outJson, ref uint outLen);

    public long AddCollection(string booksFolder)
    {
        Log($"Calling AddCollection({booksFolder})");
        var rc = pdf_add_collection(Utf8(booksFolder));
        if (rc < 0) ThrowOnError(rc, this);
        Collections = ListCollections();
        Log($"AddCollection returned: {rc}");
        return rc;
    }

    public void RemoveCollection(uint collId)
    {
        Log($"Calling RemoveCollection({collId})");
        var rc = pdf_remove_collection(collId);
        if (rc != 0) ThrowOnError(rc, this);
        Collections = ListCollections();
        Log($"RemoveCollection returned: {rc}");
    }

    public CollectionStats? GetCollectionStats(uint collId)
    {
        Log($"Calling GetCollectionStats({collId})");
        var json = CallBuf((buf, ref len) => pdf_collection_stats(collId, buf, ref len));
        Log($"GetCollectionStats returned: {json}");
        return JsonSerializer.Deserialize<CollectionStats>(json);
    }

    // ── Search ─────────────────────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_search(byte[] query, uint limit, uint offset, byte[] outJson, ref uint outLen);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_search_collection(uint collId, byte[] query, uint limit, uint offset, byte[] outJson, ref uint outLen);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_search_all(byte[] query, uint limit, uint offset, byte[] outJson, ref uint outLen);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_get_term_positions(uint collId, long docId, byte[] term, byte[] outJson, ref uint outLen);

    public SearchResponse Search(string query, int limit = 1000, int offset = 0, uint? collId = null)
    {
        if (query is not null && query.Contains(' ') && !query.Contains('"') && !query.Contains('\'')
            && !query.Contains(" AND ") && !query.Contains(" OR ")
            && query[0] != '+' && query[0] != '-')
        {
            query = $"\"{query}\"";
        }
        Log($"Calling Search(query='{query}', limit={limit}, offset={offset}, collId={collId})");
        var q = Utf8(query);
        var json = collId.HasValue
            ? CallBuf((buf, ref len) => pdf_search_collection(collId.Value, q, (uint)limit, (uint)offset, buf, ref len))
            : CallBuf((buf, ref len) => pdf_search_all(q, (uint)limit, (uint)offset, buf, ref len));
        var result = JsonSerializer.Deserialize<SearchResponse>(json) ?? new SearchResponse(0, Array.Empty<SearchResult>());
        Log($"Search returned: total={result.Total}");
        return result;
    }

    public List<WordPosition> GetTermPositions(uint collId, long docId, string term)
    {
        Log($"Calling GetTermPositions(collId={collId}, docId={docId}, term='{term}')");
        var json = CallBuf((buf, ref len) => pdf_get_term_positions(collId, docId, Utf8(term), buf, ref len));
        var result = JsonSerializer.Deserialize<List<WordPosition>>(json) ?? new List<WordPosition>();
        Log($"GetTermPositions returned: {result.Count} positions");
        return result;
    }

    // ── Search config setters ──────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_fuzzy_distance(uint value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_stem(uint value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_search_field(byte[]? value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_path_filter(byte[]? value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_recency_weight(float value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_field_weights(byte[]? json);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_collection_boost(uint collId, float weight);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_boolean_query(byte[]? json);

    public uint FuzzyDistance { set => pdf_set_fuzzy_distance(value); }
    public bool StemEnabled { set => pdf_set_stem(value ? 1u : 0u); }
    public string? SearchField { set => pdf_set_search_field(value is not null ? Utf8(value) : null); }
    public string? PathFilter { set => pdf_set_path_filter(value is not null ? Utf8(value) : null); }
    public float RecencyWeight { set => pdf_set_recency_weight(value); }
    public string? FieldWeights { set => pdf_set_field_weights(value is not null ? Utf8(value) : null); }
    public string? BooleanQuery { set => pdf_set_boolean_query(value is not null ? Utf8(value) : null); }
    public void SetCollectionBoost(uint collId, float weight) => pdf_set_collection_boost(collId, weight);

    // ── Indexing ───────────────────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_index_collection(uint collId, uint flags, IntPtr progressCb);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern void pdf_cancel_indexing();

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_is_cancel_requested();

    public async Task<int> IndexCollectionAsync(uint collId, bool ocr, bool noIndex,
        IProgress<(long current, long total)>? progress, CancellationToken ct)
    {
        Log($"Calling IndexCollectionAsync(collId={collId}, ocr={ocr}, noIndex={noIndex})");
        using var reg = ct.Register(() => pdf_cancel_indexing());
        var flags = (ocr ? 1u : 0u) | (noIndex ? 2u : 0u);

        var cb = new ProgressCallback((current, total) =>
        {
            progress?.Report(((long)current, (long)total));
            if (ct.IsCancellationRequested)
                pdf_cancel_indexing();
        });

        var progressCb = Marshal.GetFunctionPointerForDelegate(cb);
        var rc = await Task.Run(() =>
        {
            _ = cb;
            return pdf_index_collection(collId, flags, progressCb);
        });
        Log($"IndexCollectionAsync returned: {rc}");
        return rc;
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void ProgressCallback(ulong current, ulong total);

    public void CancelIndexing()
    {
        Log("Calling CancelIndexing()");
        pdf_cancel_indexing();
        Log("CancelIndexing done");
    }
    public bool IsCancelRequested
    {
        get
        {
            var result = pdf_is_cancel_requested() != 0;
            Log($"IsCancelRequested returned: {result}");
            return result;
        }
    }

    // ── Indexer tuning setters ─────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_ram_buffer(ulong value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_indexer_batch_size(uint value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_commit_interval(uint value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_commit_timeout(uint value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_extract_workers(uint value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_channel_capacity(uint value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_tesseract_path(byte[]? value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_ocr_language(byte[]? value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_ocr_workers(uint value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_ocr_max_dim(uint value);

    public ulong RamBuffer { set => pdf_set_ram_buffer(value); }
    public uint IndexerBatchSize { set => pdf_set_indexer_batch_size(value); }
    public uint CommitInterval { set => pdf_set_commit_interval(value); }
    public uint CommitTimeout { set => pdf_set_commit_timeout(value); }
    public uint ExtractWorkers { set => pdf_set_extract_workers(value); }
    public uint ChannelCapacity { set => pdf_set_channel_capacity(value); }
    public string? TesseractPath { set => pdf_set_tesseract_path(value is not null ? Utf8(value) : null); }
    public string? OcrLanguage { set => pdf_set_ocr_language(value is not null ? Utf8(value) : null); }
    public uint OcrWorkers { set => pdf_set_ocr_workers(value); }
    public uint OcrMaxDim { set => pdf_set_ocr_max_dim(value); }

    // ── Utilities ──────────────────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_page_count(byte[] path);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_last_error(byte[] outBuf, ref uint outLen);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_find_tesseract(byte[] outBuf, ref uint outLen);

    public int PageCount(string pdfPath)
    {
        Log($"Calling PageCount({pdfPath})");
        var result = pdf_page_count(Utf8(pdfPath));
        Log($"PageCount returned: {result}");
        return result;
    }

    public string? FindTesseract()
    {
        Log("Calling FindTesseract()");
        var buf = new byte[512];
        uint len = (uint)buf.Length;
        var rc = pdf_find_tesseract(buf, ref len);
        if (rc == -1 || len == 0)
        {
            Log("FindTesseract returned: null");
            return null;
        }
        var result = Encoding.UTF8.GetString(buf, 0, (int)len);
        Log($"FindTesseract returned: '{result}'");
        return result;
    }

    public string LastError
    {
        get
        {
            var buf = new byte[512];
            uint len = (uint)buf.Length;
            var rc = pdf_last_error(buf, ref len);
            return rc == 0 ? Encoding.UTF8.GetString(buf, 0, (int)len) : string.Empty;
        }
    }

    // ── Helpers ────────────────────────────────────────────────────

    private static byte[] Utf8(string s) => Encoding.UTF8.GetBytes(s + "\0");

    private delegate int BufCall(byte[] buf, ref uint len);

    private string CallBuf(BufCall nativeCall, int initialSize = 4096)
    {
        var buf = new byte[initialSize];
        uint len = (uint)buf.Length;
        var rc = nativeCall(buf, ref len);
        if (rc == -4)
        {
            buf = new byte[len];
            rc = nativeCall(buf, ref len);
        }
        if (rc != 0)
            ThrowOnError(rc, this);
        return Encoding.UTF8.GetString(buf, 0, (int)len);
    }

    private static void ThrowOnError(int rc, PdfEngine engine)
    {
        var detail = engine.LastError;
        throw new InvalidOperationException(
            string.IsNullOrEmpty(detail) ? $"DLL error {rc}" : $"DLL error {rc}: {detail}");
    }

    private List<CollectionInfo> ListCollections()
    {
        var json = CallBuf((buf, ref len) => pdf_list_collections(buf, ref len));
        return JsonSerializer.Deserialize<List<CollectionInfo>>(json) ?? new List<CollectionInfo>();
    }

    public void Dispose()
    {
        // No native cleanup needed — GC handles P/Invoke stubs
    }
}
