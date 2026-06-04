using System.Windows;
using PdfExplorer.Services;

namespace PdfExplorer;

public partial class App : Application
{
    public static PdfEngine Engine { get; private set; } = null!;

    protected override void OnStartup(StartupEventArgs e)
    {
        try
        {
            var registryDir = System.IO.Path.Combine(
                System.IO.Path.GetDirectoryName(System.Reflection.Assembly.GetExecutingAssembly().Location)!,
                "registry");
            System.IO.Directory.CreateDirectory(registryDir);
            Engine = new PdfEngine(registryDir);
            base.OnStartup(e);
        }
        catch (System.Exception ex)
        {
            MessageBox.Show($"Startup error: {ex.Message}\n\n{ex.StackTrace}", "PdfExplorer", MessageBoxButton.OK, MessageBoxImage.Error);
            Shutdown();
        }
    }

    protected override void OnExit(ExitEventArgs e)
    {
        Engine?.SaveSettings();
        base.OnExit(e);
    }
}
