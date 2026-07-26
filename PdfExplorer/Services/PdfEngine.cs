using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

public sealed class PdfEngine : IDisposable
{
    private const string Dll = "pdf_extractor_capi.dll";

    // Error codes matching pdf_extractor_capi/src/lib.rs
    public const int ErrGeneral = -1;
    public const int ErrNotFound = -2;
    public const int ErrInvalidParam = -3;
    public const int ErrBufferRetry = -4;
    public const int ErrPoisoned = -100;
    public const int ErrNotInit = -101;
    public const int ErrRegNotInit = -102;
    public const int ErrInvalidUtf8 = -103;
    public const int ErrNullPtr = -105;
    private readonly string _settingsPath;
    private uint? _indexingCollId;

    public AppSettings Settings { get; } = new();

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

        _settingsPath = System.IO.Path.Combine(registryDir, "settings.json");
        LoadSettings();

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

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_get_problematic_jobs(uint collId, byte[] outJson, ref uint outLen);

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

    public List<ProblematicJob> GetProblematicJobs(uint collId)
    {
        Log($"Calling GetProblematicJobs({collId})");
        var json = CallBuf((buf, ref len) => pdf_get_problematic_jobs(collId, buf, ref len));
        var result = JsonSerializer.Deserialize<List<ProblematicJob>>(json) ?? new List<ProblematicJob>();
        Log($"GetProblematicJobs returned: {result.Count} items");
        return result;
    }

    // ── Search ─────────────────────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_search(byte[] query, uint limit, uint offset, byte[] outJson, ref uint outLen);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_search_collection(uint collId, byte[] query, uint limit, uint offset, byte[] outJson, ref uint outLen);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_search_all(byte[] query, uint limit, uint offset, byte[] outJson, ref uint outLen);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_get_term_positions(uint collId, long docId, byte[] inputJson, byte[] outJson, ref uint outLen);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern void pdf_free_string(IntPtr ptr);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern IntPtr pdf_search_v2(byte[] jsonInput);

    public SearchResponse Search(string query, int limit = 1000, int offset = 0, uint? collId = null)
    {
        if (string.IsNullOrWhiteSpace(query))
            return new SearchResponse(0, Array.Empty<SearchResult>());

        Log($"Calling Search(query='{query}', limit={limit}, offset={offset}, collId={collId})");
        var q = Utf8(query);
        var buf = new byte[65536];
        uint len = (uint)buf.Length;
        int rc;

        if (collId.HasValue)
            rc = pdf_search_collection(collId.Value, q, (uint)limit, (uint)offset, buf, ref len);
        else
            rc = pdf_search_all(q, (uint)limit, (uint)offset, buf, ref len);

        int retries = 0;
        const int maxRetries = 10;
        const uint maxBufSize = 50 * 1024 * 1024; // 50 MB
        while (rc == -4 && retries < maxRetries)
        {
            retries++;
            if (len > maxBufSize)
                throw new InvalidOperationException($"Search buffer required {len} bytes (exceeds {maxBufSize})");
            buf = new byte[len];
            if (collId.HasValue)
                rc = pdf_search_collection(collId.Value, q, (uint)limit, (uint)offset, buf, ref len);
            else
                rc = pdf_search_all(q, (uint)limit, (uint)offset, buf, ref len);
        }
        if (rc == -4)
            throw new InvalidOperationException($"Search buffer still insufficient after {maxRetries} retries (last len={len})");

        if (rc != 0)
            ThrowOnError(rc, this);

        var json = Encoding.UTF8.GetString(buf, 0, (int)len);
        var result = JsonSerializer.Deserialize<SearchResponse>(json) ?? new SearchResponse(0, Array.Empty<SearchResult>());
        if (result.Results is null)
            result = new SearchResponse(result.Total, Array.Empty<SearchResult>());
        Log($"Search returned: total={result.Total}");
        return result;
    }

    public List<WordPosition> GetTermPositions(uint collId, long docId, List<string> matchedTerms, List<List<string>> phraseGroups)
    {
        var input = JsonSerializer.Serialize(new { matched_terms = matchedTerms, phrase_groups = phraseGroups });
        Log($"Calling GetTermPositions(collId={collId}, docId={docId}, terms={matchedTerms.Count})");
        var json = CallBuf((buf, ref len) => pdf_get_term_positions(collId, docId, Utf8(input), buf, ref len));
        var result = JsonSerializer.Deserialize<List<WordPosition>>(json) ?? new List<WordPosition>();
        Log($"GetTermPositions returned: {result.Count} positions");
        return result;
    }

    public SearchV2Response? SearchV2(SearchRequestV2 request)
    {
        var jsonInput = JsonSerializer.Serialize(request);
        var ptr = pdf_search_v2(Utf8(jsonInput));
        if (ptr == IntPtr.Zero)
            return null;
        try
        {
            var json = Marshal.PtrToStringUTF8(ptr);
            return json is not null
                ? JsonSerializer.Deserialize<SearchV2Response>(json)
                : null;
        }
        finally
        {
            pdf_free_string(ptr);
        }
    }

    // ── Search config setters ──────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_path_filter(byte[]? value);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_collection_boost(uint collId, float weight);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_boolean_query(byte[]? json);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_search_boolean_mode(int enabled);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_render_inverted(int enabled);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_highlight_color(byte r, byte g, byte b, byte alpha);

    public string? PathFilter { set { Settings.PathFilter = value; pdf_set_path_filter(value is not null ? Utf8(value) : null); } }
    public string? BooleanQuery { set { Settings.BooleanQuery = value; pdf_set_boolean_query(value is not null ? Utf8(value) : null); } }
    public bool SearchBooleanMode { set { pdf_set_search_boolean_mode(value ? 1 : 0); } }
    public string? ThemeName { get => Settings.ThemeName; set => Settings.ThemeName = value; }
    public int RenderDpi { get => Settings.RenderDpi; set => Settings.RenderDpi = value; }
    public bool RenderInverted { get => Settings.InvertPdf; set { Settings.InvertPdf = value; pdf_set_render_inverted(value ? 1 : 0); } }
    public void SetCollectionBoost(uint collId, float weight)
    {
        Settings.CollectionBoosts[collId] = weight;
        pdf_set_collection_boost(collId, weight);
    }

    public void SetHighlightColor(byte r, byte g, byte b, byte alpha)
    {
        Settings.HighlightRed = r;
        Settings.HighlightGreen = g;
        Settings.HighlightBlue = b;
        Settings.HighlightAlpha = alpha;
        pdf_set_highlight_color(r, g, b, alpha);
    }

    // ── Indexing ───────────────────────────────────────────────────

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_index_collection(uint collId, uint flags, IntPtr progressCb);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern void pdf_cancel_indexing(uint collId);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_is_cancel_requested(uint collId);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_close_collection(uint collId);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_close_all();

    public async Task<int> IndexCollectionAsync(uint collId, bool ocr, bool noIndex,
        IProgress<(long current, long total)>? progress, CancellationToken ct)
    {
        Log($"IndexCollectionAsync(collId={collId}, ocr={ocr}, noIndex={noIndex})");
        _indexingCollId = collId;
        var flags = (ocr ? 1u : 0u) | (noIndex ? 2u : 0u);

        var cb = new ProgressCallback((current, total) =>
        {
            progress?.Report(((long)current, (long)total));
            if (ct.IsCancellationRequested)
                pdf_cancel_indexing(collId);
        });

        var cbHandle = GCHandle.Alloc(cb);
        using var reg = ct.Register(() => pdf_cancel_indexing(collId));

        try
        {
            var cbPtr = Marshal.GetFunctionPointerForDelegate(cb);
            return await Task.Run(() => pdf_index_collection(collId, flags, cbPtr));
        }
        finally
        {
            cbHandle.Free();
            _indexingCollId = null;
            Log($"IndexCollectionAsync completed");
        }
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void ProgressCallback(ulong current, ulong total);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void LogCallback(IntPtr msg, uint len);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_log_callback(IntPtr cb);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl, CharSet = CharSet.Ansi)]
    private static extern int pdf_set_log_path(IntPtr path);

    public void SetLogPath(string? path)
    {
        if (path is null)
        {
            pdf_set_log_path(IntPtr.Zero);
            return;
        }
        var bytes = System.Text.Encoding.UTF8.GetBytes(path + "\0");
        var ptr = Marshal.AllocHGlobal(bytes.Length);
        try
        {
            Marshal.Copy(bytes, 0, ptr, bytes.Length);
            pdf_set_log_path(ptr);
        }
        finally
        {
            Marshal.FreeHGlobal(ptr);
        }
    }

    private LogCallback? _logCb;
    private GCHandle? _logCbHandle;
    private LogCallback? _procCb;
    private GCHandle? _procCbHandle;

    public void SetLogCallback(Action<string>? onLog)
    {
        if (onLog is null)
        {
            pdf_set_log_callback(IntPtr.Zero);
            if (_logCbHandle.HasValue)
            {
                _logCbHandle.Value.Free();
                _logCbHandle = null;
            }
            _logCb = null;
            return;
        }

        LogCallback newCb = (msgPtr, len) =>
        {
            try
            {
                var msg = Marshal.PtrToStringUTF8(msgPtr, (int)len);
                if (msg is not null)
                    onLog(msg);
            }
            catch
            {
                // Swallow exceptions from the callback to avoid
                // crashing the native worker thread.
            }
        };
        var newHandle = GCHandle.Alloc(newCb);
        var fnPtr = Marshal.GetFunctionPointerForDelegate(newCb);
        pdf_set_log_callback(fnPtr);

        // Native code now uses the new pointer — safe to release the old one.
        if (_logCbHandle.HasValue)
            _logCbHandle.Value.Free();

        _logCb = newCb;
        _logCbHandle = newHandle;
    }

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    private static extern int pdf_set_process_callback(IntPtr cb);

    /// Register a callback that receives per‑process metrics.
    /// The string format is:   PROC|&lt;thread&gt;|&lt;pid&gt;|&lt;state&gt;|&lt;mem_mb&gt;|&lt;extra&gt;
    public void SetProcessCallback(Action<string>? onEvent)
    {
        if (onEvent is null)
        {
            pdf_set_process_callback(IntPtr.Zero);
            if (_procCbHandle.HasValue)
            {
                _procCbHandle.Value.Free();
                _procCbHandle = null;
            }
            _procCb = null;
            return;
        }

        LogCallback newCb = (msgPtr, len) =>
        {
            try
            {
                var msg = Marshal.PtrToStringUTF8(msgPtr, (int)len);
                if (msg is not null)
                    onEvent(msg);
            }
            catch
            {
                // Swallow exceptions to avoid crashing the native worker thread.
            }
        };
        var newHandle = GCHandle.Alloc(newCb);
        var fnPtr = Marshal.GetFunctionPointerForDelegate(newCb);
        pdf_set_process_callback(fnPtr);

        // Native code now uses the new pointer — safe to release the old one.
        if (_procCbHandle.HasValue)
            _procCbHandle.Value.Free();

        _procCb = newCb;
        _procCbHandle = newHandle;
    }

    public void CancelIndexing()
    {
        Log("Calling CancelIndexing()");
        if (_indexingCollId.HasValue)
            pdf_cancel_indexing(_indexingCollId.Value);
        Log("CancelIndexing done");
    }
    public bool IsCancelRequested
    {
        get
        {
            var result = _indexingCollId.HasValue && pdf_is_cancel_requested(_indexingCollId.Value) != 0;
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

    public ulong RamBuffer { set { Settings.RamBuffer = value; pdf_set_ram_buffer(value); } }
    public uint IndexerBatchSize { set { Settings.IndexerBatchSize = value; pdf_set_indexer_batch_size(value); } }
    public uint CommitInterval { set { Settings.CommitInterval = value; pdf_set_commit_interval(value); } }
    public uint CommitTimeout { set { Settings.CommitTimeout = value; pdf_set_commit_timeout(value); } }
    public uint ExtractWorkers { set { Settings.ExtractWorkers = value; pdf_set_extract_workers(value); } }
    public uint ChannelCapacity { set { Settings.ChannelCapacity = value; pdf_set_channel_capacity(value); } }
    public string? TesseractPath { set { Settings.TesseractPath = value; pdf_set_tesseract_path(value is not null ? Utf8(value) : null); } }
    public string? OcrLanguage { set { Settings.OcrLanguage = value; pdf_set_ocr_language(value is not null ? Utf8(value) : null); } }
    public uint OcrWorkers { set { Settings.OcrWorkers = value; pdf_set_ocr_workers(value); } }
    public uint OcrMaxDim { set { Settings.OcrMaxDim = value; pdf_set_ocr_max_dim(value); } }

    // ── Settings persistence ────────────────────────────────────────

    public void SaveSettings()
    {
        try
        {
            var json = JsonSerializer.Serialize(Settings, new JsonSerializerOptions { WriteIndented = true });
            System.IO.File.WriteAllText(_settingsPath, json);
            Log($"SaveSettings: ThemeName={Settings.ThemeName}, InvertPdf={Settings.InvertPdf}, path={_settingsPath}");
        }
        catch (Exception ex) { Log($"SaveSettings FAILED: {ex.Message}"); }
    }

    public void LoadSettings()
    {
        if (!System.IO.File.Exists(_settingsPath))
        {
            Log($"LoadSettings: file NOT FOUND at {_settingsPath}");
            pdf_set_highlight_color(Settings.HighlightRed, Settings.HighlightGreen, Settings.HighlightBlue, Settings.HighlightAlpha);
            return;
        }
        try
        {
            var json = System.IO.File.ReadAllText(_settingsPath);
            Log($"LoadSettings: raw JSON = {json}");
            var s = JsonSerializer.Deserialize<AppSettings>(json);
            if (s is null) { Log("LoadSettings: deserialize returned null"); return; }

            Settings.TesseractPath = s.TesseractPath;
            Settings.OcrLanguage = s.OcrLanguage;
            Settings.OcrWorkers = s.OcrWorkers;
            Settings.OcrMaxDim = s.OcrMaxDim;
            Settings.RamBuffer = s.RamBuffer;
            Settings.IndexerBatchSize = s.IndexerBatchSize;
            Settings.CommitInterval = s.CommitInterval;
            Settings.CommitTimeout = s.CommitTimeout;
            Settings.ExtractWorkers = s.ExtractWorkers;
            Settings.ChannelCapacity = s.ChannelCapacity;
            Settings.CollectionBoosts = s.CollectionBoosts ?? new();
            Settings.PathFilter = s.PathFilter;
            Settings.BooleanQuery = s.BooleanQuery;
            Settings.ThemeName = s.ThemeName ?? "Light";
            Settings.InvertPdf = s.InvertPdf;
            Settings.RenderDpi = s.RenderDpi;
            Settings.HighlightRed = s.HighlightRed;
            Settings.HighlightGreen = s.HighlightGreen;
            Settings.HighlightBlue = s.HighlightBlue;
            Settings.HighlightAlpha = s.HighlightAlpha;
            Log($"LoadSettings: loaded ThemeName={Settings.ThemeName}, InvertPdf={Settings.InvertPdf}");

            // Push to DLL
            if (Settings.TesseractPath is not null)
                pdf_set_tesseract_path(Utf8(Settings.TesseractPath));
            if (Settings.OcrLanguage is not null)
                pdf_set_ocr_language(Utf8(Settings.OcrLanguage));
            pdf_set_ocr_workers(Settings.OcrWorkers);
            pdf_set_ocr_max_dim(Settings.OcrMaxDim);
            pdf_set_ram_buffer(Settings.RamBuffer);
            pdf_set_indexer_batch_size(Settings.IndexerBatchSize);
            pdf_set_commit_interval(Settings.CommitInterval);
            pdf_set_commit_timeout(Settings.CommitTimeout);
            pdf_set_extract_workers(Settings.ExtractWorkers);
            pdf_set_channel_capacity(Settings.ChannelCapacity);
            foreach (var (id, weight) in Settings.CollectionBoosts)
                pdf_set_collection_boost(id, weight);
            if (Settings.PathFilter is not null)
                pdf_set_path_filter(Utf8(Settings.PathFilter));
            if (Settings.BooleanQuery is not null)
                pdf_set_boolean_query(Utf8(Settings.BooleanQuery));
            pdf_set_render_inverted(Settings.InvertPdf ? 1 : 0);
            pdf_set_highlight_color(Settings.HighlightRed, Settings.HighlightGreen, Settings.HighlightBlue, Settings.HighlightAlpha);
        }
        catch (Exception ex) { Log($"LoadSettings FAILED: {ex.Message}"); }
    }

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
        int retries = 0;
        const int maxRetries = 10;
        const uint maxBufSize = 50 * 1024 * 1024; // 50 MB
        while (rc == ErrBufferRetry && retries < maxRetries)
        {
            retries++;
            if (len > maxBufSize)
                throw new InvalidOperationException($"CallBuf required {len} bytes (exceeds {maxBufSize})");
            buf = new byte[len];
            rc = nativeCall(buf, ref len);
        }
        if (rc == ErrBufferRetry)
            throw new InvalidOperationException($"CallBuf buffer still insufficient after {maxRetries} retries (last len={len})");
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
        pdf_set_log_callback(IntPtr.Zero);
        pdf_set_process_callback(IntPtr.Zero);
        if (_logCbHandle.HasValue)
        {
            _logCbHandle.Value.Free();
            _logCbHandle = null;
        }
        _logCb = null;
        if (_procCbHandle.HasValue)
        {
            _procCbHandle.Value.Free();
            _procCbHandle = null;
        }
        _procCb = null;
        pdf_close_all();
    }

    public void CloseCollection(uint collId)
    {
        pdf_close_collection(collId);
    }
}
