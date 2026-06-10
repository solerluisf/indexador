using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Views;

public partial class PdfPageView : UserControl
{
    private bool _rendered;

    public PdfPageView()
    {
        InitializeComponent();
    }

    private async void OnLoaded(object sender, RoutedEventArgs e)
    {
        if (_rendered || DataContext is not PdfPageViewModel vm)
            return;

        vm.IsLoading = true;
        try
        {
            var tab = FindVisualParent<SearchTab>(this);
            if (tab is null) return;

            var service = tab as IPdfRenderingService;
            if (service is null) return;

            var item = await service.GetOrRenderPageAsync(vm.PageIndex, vm.Positions);

            await Dispatcher.InvokeAsync(() =>
            {
                if (DataContext is PdfPageViewModel currentVm && currentVm.PageIndex == vm.PageIndex)
                {
                    currentVm.PageImage = item.PageImage;
                    _rendered = true;
                }
            });
        }
        catch (Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"[PdfPageView] Render error: {ex.Message}");
        }
        finally
        {
            await Dispatcher.InvokeAsync(() =>
            {
                if (DataContext is PdfPageViewModel currentVm && currentVm.PageIndex == vm.PageIndex)
                    currentVm.IsLoading = false;
            });
        }
    }

    private void OnUnloaded(object sender, RoutedEventArgs e)
    {
        if (DataContext is PdfPageViewModel vm)
            vm.PageImage = null;
        _rendered = false;
    }

    private static T? FindVisualParent<T>(DependencyObject child) where T : DependencyObject
    {
        while (child is not null and not T)
            child = VisualTreeHelper.GetParent(child);
        return child as T;
    }
}
