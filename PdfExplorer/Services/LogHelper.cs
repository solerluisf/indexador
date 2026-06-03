using System.Globalization;
using System.IO;

namespace PdfExplorer.Services;

internal static class LogHelper
{
    private static readonly string LogPath;
    private static readonly object Lock = new();

    static LogHelper()
    {
        var dir = Path.Combine(Path.GetTempPath(), "PdfExplorer");
        Directory.CreateDirectory(dir);
        LogPath = Path.Combine(dir, "cs.log");
    }

    public static void Log(string source, string message)
    {
        var timestamp = DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss.fff", CultureInfo.InvariantCulture);
        var line = $"{timestamp} [{source}] {message}";
        lock (Lock)
        {
            try
            {
                File.AppendAllText(LogPath, line + Environment.NewLine);
            }
            catch (Exception ex)
            {
                try { File.AppendAllText(LogPath.Replace(".log", "-error.log"), $"FAILED to log: {ex.Message}\n"); } catch { }
            }
        }
    }
}