using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Views;

public partial class PdfPageView : UserControl
{
    private bool _rendered;
    private CancellationTokenSource? _loadCts;

    public PdfPageView()
    {
        InitializeComponent();
    }

    private async void OnLoaded(object sender, RoutedEventArgs e)
    {
        _loadCts?.Cancel();
        _loadCts = new CancellationTokenSource();
        var ct = _loadCts.Token;

        var vm = DataContext as PdfPageViewModel;
        if (_rendered || vm is null)
            return;

        vm.IsLoading = true;
        try
        {
            var tab = FindVisualParent<SearchTab>(this);
            if (tab is null) return;

            var service = tab as IPdfRenderingService;
            if (service is null) return;

            var item = await service.GetOrRenderPageAsync(vm.PageIndex, vm.Positions);

            if (ct.IsCancellationRequested)
                return;

            await Dispatcher.InvokeAsync(() =>
            {
                if (ct.IsCancellationRequested) return;
                if (DataContext is PdfPageViewModel currentVm && currentVm.PageIndex == vm.PageIndex)
                {
                    currentVm.PageImage = item.PageImage;
                    currentVm.ImagePixelWidth = item.ImagePixelWidth;
                    currentVm.ImagePixelHeight = item.ImagePixelHeight;
                    _rendered = true;
                }
            });
        }
        catch (OperationCanceledException)
        {
            // normal — document changed or control recycled
        }
        catch (Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"[PdfPageView] Render error: {ex.Message}");
        }
        finally
        {
            if (!ct.IsCancellationRequested)
            {
                await Dispatcher.InvokeAsync(() =>
                {
                    if (ct.IsCancellationRequested) return;
                    vm.IsLoading = false;
                });
            }
        }
    }

    private void OnUnloaded(object sender, RoutedEventArgs e)
    {
        _loadCts?.Cancel();
        _loadCts = null;
        _rendered = false;
        // Do NOT null vm.PageImage here — that would poison the cache.
        // The LRU cache in SearchTab manages bitmap lifetime.
    }

    private static T? FindVisualParent<T>(DependencyObject child) where T : DependencyObject
    {
        while (child is not null and not T)
            child = VisualTreeHelper.GetParent(child);
        return child as T;
    }
}
