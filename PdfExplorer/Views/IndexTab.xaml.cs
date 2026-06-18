using System.Collections.Generic;
using System.IO;
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
        CollectionList.SelectionChanged += OnCollectionChanged;
        Loaded += OnLoaded;
    }

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        CheckTesseractStatus();
    }

    private void LoadCollections()
    {
        CollectionList.ItemsSource = _engine.Collections;
        ProblematicExpander.Visibility = Visibility.Collapsed;
    }

    private async void OnCollectionChanged(object sender, SelectionChangedEventArgs e)
    {
        ProblematicExpander.Visibility = Visibility.Collapsed;
        ProgressBar.Visibility = Visibility.Collapsed;
        StatusLabel.Content = "Ready";

        if (CollectionList.SelectedItem is not Models.CollectionInfo coll)
            return;

        try
        {
            var stats = _engine.GetCollectionStats((uint)coll.Id);
            if (stats is not null && stats.NumDocs > 0)
            {
                StatusLabel.Content = $"{stats.NumDocs} documents indexed";
                ProgressBar.Value = 100;
                ProgressBar.Visibility = Visibility.Visible;
            }
        }
        catch
        {
            // Collection has no Tantivy index yet — normal for newly added folders
        }

        ShowProblematicFiles((uint)coll.Id);
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
        ErrorLogBox.Visibility = Visibility.Visible;
        ErrorLogBox.Clear();
        ProblematicExpander.Visibility = Visibility.Collapsed;
        StatusLabel.Content = "Indexing...";
        LogHelper.Log("IndexTab", $"Indexing started: collId={coll.Id}, ocr={OcrCheckbox.IsChecked == true}");

        var errors = new List<string>();

        // Route Rust log messages to the error log box
        _engine.SetLogCallback(msg =>
        {
            _ = Dispatcher.InvokeAsync(() =>
            {
                ErrorLogBox.AppendText(msg + Environment.NewLine);
                ErrorLogBox.ScrollToEnd();
            });
        });

        // Also write all pipeline log messages to a file for external tailing
        var logDir = Path.Combine(Path.GetTempPath(), "PdfExplorer");
        Directory.CreateDirectory(logDir);
        _engine.SetLogPath(Path.Combine(logDir, "pipeline.log"));

        // Route per‑process metrics (PROC|thread|pid|state|mem|extra)
        _engine.SetProcessCallback(raw =>
        {
            _ = Dispatcher.InvokeAsync(() =>
            {
                var parts = raw.Split('|');
                if (parts.Length < 6 || parts[0] != "PROC") return;

                var thread = parts[1];
                var pid    = parts[2];
                var state  = parts[3];
                var mem    = parts[4];
                var extra  = parts[5];

                var icon = state switch
                {
                    "started"     => "▶",
                    "running"     => "·",
                    var s when s.StartsWith("exited(0)") => "✓",
                    var s when s.StartsWith("exited(") => "✗",
                    "crashed"     => "💀",
                    _             => "?",
                };

                var memPart = mem != "?" ? $"  {mem} MB" : "";
                var extraPart = !string.IsNullOrEmpty(extra) ? $"  [{extra}]" : "";

                ErrorLogBox.AppendText(
                    $"  {icon}  {thread}  PID:{pid}  {state}{memPart}{extraPart}{Environment.NewLine}");
                ErrorLogBox.ScrollToEnd();
            });
        });

        var progress = new Progress<(long current, long total)>(p =>
        {
            StatusLabel.Content = $"Indexing... {p.current} / {p.total}";
            if (p.total > 0)
                ProgressBar.Value = (double)p.current / p.total * 100;
        });

        try
        {
            var result = await _engine.IndexCollectionAsync((uint)coll.Id,
                ocr: OcrCheckbox.IsChecked == true, noIndex: false,
                progress, CancellationToken.None);

            if (result >= 0)
            {
                var stats = _engine.GetCollectionStats((uint)coll.Id);
                var indexed = stats?.NumDocs ?? result;
                StatusLabel.Content = $"Done — {indexed} documents indexed";
            }
            else
            {
                var err = _engine.LastError;
                StatusLabel.Content = string.IsNullOrEmpty(err) ? "Failed" : $"Failed: {err}";
                if (!string.IsNullOrEmpty(err))
                    errors.Add(err);
            }
        }
        catch (Exception ex)
        {
            StatusLabel.Content = "Failed";
            errors.Add($"Exception: {ex.Message}");
            LogHelper.Log("IndexTab", $"Indexing exception: {ex}");
        }

        // Collect problematic jobs from the database
        try
        {
            var problematic = _engine.GetProblematicJobs((uint)coll.Id);
            if (problematic.Count > 0)
            {
                ProblematicList.ItemsSource = problematic;
                ProblematicExpander.Header = $"Show {problematic.Count} problematic file(s)";
                ProblematicExpander.Visibility = Visibility.Visible;
                ProblematicExpander.IsExpanded = true;

                foreach (var p in problematic)
                    errors.Add($"{p.FileName}: {p.Issue}");
            }
        }
        catch (Exception ex)
        {
            LogHelper.Log("IndexTab", $"Error loading problematic files: {ex.Message}");
            errors.Add($"Failed to load problem list: {ex.Message}");
        }

        // Detach the callbacks — they were only valid during indexing
        _engine.SetLogCallback(null);
        _engine.SetProcessCallback(null);
        _engine.SetLogPath(null); // close pipeline log file

        if (errors.Count > 0)
        {
            ErrorLogBox.AppendText(string.Join(Environment.NewLine, errors));
            ErrorLogBox.Visibility = Visibility.Visible;
            ErrorLogBox.ScrollToEnd();
        }

        IndexBtn.IsEnabled = true;
        CancelBtn.IsEnabled = false;
    }

    private void ShowProblematicFiles(uint collId)
    {
        try
        {
            var items = _engine.GetProblematicJobs(collId);
            if (items.Count > 0)
            {
                ProblematicList.ItemsSource = items;
                ProblematicExpander.Header = $"Show {items.Count} problematic file(s)";
                ProblematicExpander.Visibility = Visibility.Visible;
                ProblematicExpander.IsExpanded = true;
            }
            else
            {
                ProblematicExpander.Visibility = Visibility.Collapsed;
            }
        }
        catch (Exception ex)
        {
            LogHelper.Log("IndexTab", $"Error loading problematic files: {ex.Message}");
        }
    }

    private void OnCancel(object sender, RoutedEventArgs e)
    {
        _engine.CancelIndexing();
        StatusLabel.Content = "Cancelling...";
    }
}
