using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Threading;
using PdfExplorer.Services;

namespace PdfExplorer;

public partial class App : Application
{
    public static PdfEngine Engine { get; private set; } = null!;
    public static int RenderDpi
    {
        get => Engine?.RenderDpi ?? 150;
        set
        {
            if (Engine is not null) { Engine.RenderDpi = value; Engine.SaveSettings(); }
            RenderDpiChanged?.Invoke();
        }
    }
    public static event Action? RenderDpiChanged;

    public static bool RenderInverted
    {
        get => Engine?.RenderInverted ?? false;
        set
        {
            if (Engine is not null) { Engine.RenderInverted = value; Engine.SaveSettings(); }
            RenderInvertedChanged?.Invoke();
        }
    }
    public static event Action? RenderInvertedChanged;

    protected override void OnStartup(StartupEventArgs e)
    {
        // Global exception handlers (must be registered BEFORE any UI work)
        AppDomain.CurrentDomain.UnhandledException += OnAppDomainUnhandledException;
        Current.DispatcherUnhandledException += OnDispatcherUnhandledException;
        TaskScheduler.UnobservedTaskException += OnUnobservedTaskException;

        try
        {
            var registryDir = System.IO.Path.Combine(
                System.IO.Path.GetDirectoryName(System.Reflection.Assembly.GetExecutingAssembly().Location)!,
                "registry");
            System.IO.Directory.CreateDirectory(registryDir);
            Engine = new PdfEngine(registryDir);
            ApplyTheme(Engine.ThemeName ?? "Light");
            base.OnStartup(e);
            if (MainWindow is not null)
                MainWindow.SourceInitialized += OnMainWindowSourceInitialized;
        }
        catch (System.Exception ex)
        {
            LogHelper.Log("App", $"Startup fatal: {ex}");
            MessageBox.Show($"Startup error: {ex.Message}\n\n{ex.StackTrace}", "PdfExplorer", MessageBoxButton.OK, MessageBoxImage.Error);
            Shutdown();
        }
    }

    public static void ApplyTheme(string name)
    {
        var uri = new Uri($"Themes/{name}.xaml", UriKind.Relative);
        var dict = new ResourceDictionary { Source = uri };

        var merged = Current.Resources.MergedDictionaries;
        merged.Clear();
        merged.Add(dict);

        Engine.ThemeName = name;
        Engine.SaveSettings();

        var hwnd = GetMainWindowHandle();
        if (hwnd != IntPtr.Zero)
            SetTitleBarTheme(hwnd, name);
    }

    private void OnMainWindowSourceInitialized(object? sender, EventArgs e)
    {
        try
        {
            var hwnd = new WindowInteropHelper(MainWindow).Handle;
            if (hwnd == IntPtr.Zero)
                hwnd = new WindowInteropHelper(MainWindow).EnsureHandle();
            SetTitleBarTheme(hwnd, Engine.ThemeName ?? "Light");
        }
        catch (Exception ex)
        {
            LogHelper.Log("App", $"SourceInitialized error: {ex.Message}");
        }
    }

    private static IntPtr GetMainWindowHandle()
    {
        if (Current.MainWindow is null) return IntPtr.Zero;
        var source = PresentationSource.FromVisual(Current.MainWindow) as HwndSource;
        return source?.Handle ?? IntPtr.Zero;
    }

    [DllImport("dwmapi.dll")]
    private static extern int DwmSetWindowAttribute(IntPtr hwnd, int attr, ref int attrValue, int attrSize);

    private static void SetTitleBarTheme(IntPtr hwnd, string themeName)
    {
        if (hwnd == IntPtr.Zero) return;
        try
        {
            int useDark = themeName == "Dark" ? 1 : 0;
            // Try attribute 20 (Windows 10 2004+) then 19 (undocumented older builds)
            int hr = DwmSetWindowAttribute(hwnd, 20, ref useDark, sizeof(int));
            if (hr != 0)
                hr = DwmSetWindowAttribute(hwnd, 19, ref useDark, sizeof(int));
            if (hr != 0)
                LogHelper.Log("App", $"DWM dark mode unavailable: hr={hr}");
        }
        catch (Exception ex)
        {
            LogHelper.Log("App", $"DWM error: {ex.Message}");
        }
    }

    protected override void OnExit(ExitEventArgs e)
    {
        try
        {
            Engine?.SaveSettings();
            Engine?.Dispose();
        }
        catch (Exception ex)
        {
            LogHelper.Log("App", $"Dispose error in OnExit: {ex}");
        }
        base.OnExit(e);
    }

    // ── Global exception handlers ─────────────────────────────────

    private void OnDispatcherUnhandledException(object sender, DispatcherUnhandledExceptionEventArgs e)
    {
        var ex = e.Exception;
        var inner = ex.InnerException;
        var msg = inner is not null
            ? $"{ex.Message}\n\nInner: {inner.GetType().Name}: {inner.Message}\n{inner.StackTrace}"
            : ex.ToString();
        LogHelper.Log("App", $"DispatcherUnhandledException: {msg}");
        MessageBox.Show($"Unhandled UI error:\n{msg}", "PdfExplorer", MessageBoxButton.OK, MessageBoxImage.Error);
        e.Handled = true;
    }

    private void OnAppDomainUnhandledException(object sender, UnhandledExceptionEventArgs e)
    {
        var ex = e.ExceptionObject as Exception;
        LogHelper.Log("App", $"AppDomainUnhandledException (terminating={e.IsTerminating}): {ex?.ToString()}");
    }

    private void OnUnobservedTaskException(object? sender, UnobservedTaskExceptionEventArgs e)
    {
        LogHelper.Log("App", $"UnobservedTaskException: {e.Exception?.ToString()}");
        e.SetObserved();
    }
}
