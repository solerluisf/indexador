using System.Windows;
using System.Windows.Controls;
using PdfExplorer.Services;

namespace PdfExplorer.Views;

public partial class IndexTab : Page
{
    private readonly PdfEngine _engine;

    public IndexTab()
    {
        InitializeComponent();
        _engine = App.Engine;
        LoadCollections();
        Loaded += OnLoaded;
    }

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        CheckTesseractStatus();
    }

    private void LoadCollections()
    {
        CollectionList.ItemsSource = _engine.Collections;
    }

    private void OnAddCollection(object sender, RoutedEventArgs e)
    {
        var dialog = new Microsoft.Win32.OpenFolderDialog();
        if (dialog.ShowDialog() == true)
        {
            _engine.AddCollection(dialog.FolderName);
            LoadCollections();
        }
    }

    private void OnRemoveCollection(object sender, RoutedEventArgs e)
    {
        if (CollectionList.SelectedItem is Models.CollectionInfo coll)
        {
            _engine.RemoveCollection((uint)coll.Id);
            LoadCollections();
        }
    }

    private void OnOcrToggled(object sender, RoutedEventArgs e)
    {
        CheckTesseractStatus();
    }

    private void CheckTesseractStatus()
    {
        if (_engine is null) return;

        if (OcrCheckbox.IsChecked != true)
        {
            OcrStatus.Text = "";
            return;
        }

        var tesseractPath = _engine.FindTesseract();
        if (tesseractPath != null)
        {
            OcrStatus.Text = $"Tesseract found: {tesseractPath}";
            OcrStatus.Foreground = System.Windows.Media.Brushes.Gray;
        }
        else
        {
            OcrStatus.Text = "Tesseract not found — image PDFs will be indexed without text";
            OcrStatus.Foreground = System.Windows.Media.Brushes.OrangeRed;
        }
    }

    private async void OnIndex(object sender, RoutedEventArgs e)
    {
        if (CollectionList.SelectedItem is not Models.CollectionInfo coll) return;

        IndexBtn.IsEnabled = false;
        CancelBtn.IsEnabled = true;
        ProgressBar.Visibility = Visibility.Visible;
        StatusLabel.Content = "Indexing...";
        LogHelper.Log("IndexTab", $"Indexing started: collId={coll.Id}, ocr={OcrCheckbox.IsChecked == true}");

        var progress = new Progress<(long current, long total)>(p =>
        {
            LogHelper.Log("IndexTab", $"Indexing progress: {p.current} / {p.total}");
            StatusLabel.Content = $"Indexing... {p.current} / {p.total}";
            if (p.total > 0)
                ProgressBar.Value = (double)p.current / p.total * 100;
        });

        var result = await _engine.IndexCollectionAsync((uint)coll.Id,
            ocr: OcrCheckbox.IsChecked == true, noIndex: false,
            progress, CancellationToken.None);

        if (result >= 0)
        {
            LogHelper.Log("IndexTab", $"Indexing completed: {result} documents indexed");
            StatusLabel.Content = $"Done — {result} documents indexed";
        }
        else
        {
            StatusLabel.Content = "Failed";
            var err = _engine.LastError;
            if (!string.IsNullOrEmpty(err))
                StatusLabel.Content = $"Failed: {err}";
            LogHelper.Log("IndexTab", $"Indexing failed: {err}");
        }
        IndexBtn.IsEnabled = true;
        CancelBtn.IsEnabled = false;
    }

    private void OnCancel(object sender, RoutedEventArgs e)
    {
        _engine.CancelIndexing();
        StatusLabel.Content = "Cancelling...";
    }
}
