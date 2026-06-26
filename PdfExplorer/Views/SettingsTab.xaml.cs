using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using PdfExplorer.Services;

namespace PdfExplorer.Views;

public partial class SettingsTab : Page
{
    private readonly PdfEngine _engine;
    private bool _wired;
    private bool _themeInit;
    private bool _loading;

    public SettingsTab()
    {
        InitializeComponent();
        _engine = App.Engine;
        Loaded += OnLoaded;
    }

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        // Populate collection boost sliders
        BoostsPanel.Children.Clear();
        foreach (var coll in _engine.Collections)
        {
            var row = new StackPanel { Orientation = Orientation.Horizontal, Margin = new Thickness(0, 2, 0, 2) };
            row.Children.Add(new Label { Content = coll.Label, Width = 200 });
            var slider = new Slider
            {
                Minimum = 0.1,
                Maximum = 5.0,
                TickFrequency = 0.1,
                Value = 1.0,
                Width = 200,
                HorizontalAlignment = HorizontalAlignment.Left
            };
            var id = (uint)coll.Id;
            slider.ValueChanged += (_, _) => _engine.SetCollectionBoost(id, (float)slider.Value);
            row.Children.Add(slider);
            BoostsPanel.Children.Add(row);
        }

        var dpiValues = new[] { 72, 96, 150, 200, 300, 400, 600 };
        int currentDpi = App.RenderDpi;
        int dpiIdx = Array.IndexOf(dpiValues, currentDpi);
        if (dpiIdx >= 0)
            DpiSelector.SelectedIndex = dpiIdx;

        if (_wired) return;
        _wired = true;
        WireEvents();
        ApplyDefaults();
    }

    private void WireEvents()
    {
        _themeInit = false;
        InvertPdfCheckbox.IsChecked = _engine.Settings.InvertPdf;
        App.RenderInverted = _engine.Settings.InvertPdf;

        var theme = _engine.ThemeName ?? "Light";
        ThemeSelector.SelectedIndex = theme switch
        {
            "Dark" => 1,
            "LightBlue" => 2,
            _ => 0
        };
        _themeInit = true;

        TesseractPath.LostFocus += (_, _) => { _engine.TesseractPath = TesseractPath.Text; _engine.SaveSettings(); };
        OcrLanguage.LostFocus += (_, _) => { _engine.OcrLanguage = OcrLanguage.Text; _engine.SaveSettings(); };
        OcrWorkers.ValueChanged += (_, e) => { _engine.OcrWorkers = (uint)e.NewValue; _engine.SaveSettings(); };
        OcrMaxDim.ValueChanged += (_, e) => { _engine.OcrMaxDim = (uint)e.NewValue; _engine.SaveSettings(); };

        RamBuffer.SelectionChanged += (_, _) => { ApplyRamBuffer(); _engine.SaveSettings(); };
        BatchSize.ValueChanged += (_, e) => { _engine.IndexerBatchSize = (uint)e.NewValue; _engine.SaveSettings(); };
        CommitInterval.ValueChanged += (_, e) => { _engine.CommitInterval = (uint)e.NewValue; _engine.SaveSettings(); };
        CommitTimeout.ValueChanged += (_, e) => { _engine.CommitTimeout = (uint)e.NewValue; _engine.SaveSettings(); };
        ExtractWorkers.ValueChanged += (_, e) => { _engine.ExtractWorkers = (uint)e.NewValue; _engine.SaveSettings(); };
        ChannelCapacity.ValueChanged += (_, e) => { _engine.ChannelCapacity = (uint)e.NewValue; _engine.SaveSettings(); };

        _loading = true;
        InitHighlightControls();
        _loading = false;
    }

    private void InitHighlightControls()
    {
        var s = _engine.Settings;
        HighlightAlphaSlider.Value = s.HighlightAlpha * 100 / 255;
        UpdateHighlightPreview(s.HighlightRed, s.HighlightGreen, s.HighlightBlue, s.HighlightAlpha);
    }

    private void OnSelectColor(object sender, RoutedEventArgs e)
    {
        try
        {
            var s = _engine.Settings;
            var initial = Color.FromArgb(s.HighlightAlpha, s.HighlightRed, s.HighlightGreen, s.HighlightBlue);
            var owner = Window.GetWindow(this);
            LogHelper.Log("SettingsTab", $"OnSelectColor: owner={(owner is not null ? "ok" : "NULL")}");
            var picked = NativeColorDialog.Show(owner, initial);
            if (picked is not null)
            {
                var c = picked.Value;
                LogHelper.Log("SettingsTab", $"OnSelectColor: picked R={c.R} G={c.G} B={c.B} alpha={s.HighlightAlpha}");
                _engine.SetHighlightColor(c.R, c.G, c.B, s.HighlightAlpha);
                _engine.SaveSettings();
                UpdateHighlightPreview(c.R, c.G, c.B, s.HighlightAlpha);
                App.NotifyHighlightColorChanged();
            }
            else
            {
                LogHelper.Log("SettingsTab", "OnSelectColor: dialog returned null (cancel?)");
            }
        }
        catch (Exception ex)
        {
            LogHelper.Log("SettingsTab", $"OnSelectColor error: {ex.Message}");
            MessageBox.Show($"Color selection error:\n{ex.Message}", "Error", MessageBoxButton.OK, MessageBoxImage.Error);
        }
    }

    private void OnHighlightAlphaChanged(object sender, RoutedEventArgs e)
    {
        if (_engine is null || _loading) return;
        var s = _engine.Settings;
        int pct = (int)HighlightAlphaSlider.Value;
        var alpha = (byte)(pct * 255 / 100);
        HighlightAlphaLabel.Content = $"{pct}%";
        _engine.SetHighlightColor(s.HighlightRed, s.HighlightGreen, s.HighlightBlue, alpha);
        _engine.SaveSettings();
        UpdateHighlightPreview(s.HighlightRed, s.HighlightGreen, s.HighlightBlue, alpha);
        App.NotifyHighlightColorChanged();
    }

    private void UpdateHighlightPreview(byte r, byte g, byte b, byte a)
    {
        HighlightPreview.Background = new SolidColorBrush(Color.FromArgb(a, r, g, b));
    }

    private void OnThemeChanged(object sender, SelectionChangedEventArgs e)
    {
        var name = ThemeSelector.SelectedIndex switch
        {
            1 => "Dark",
            2 => "LightBlue",
            _ => "Light"
        };
        App.ApplyTheme(name);

        // Auto-toggle PDF inversion with Dark theme (only on explicit user changes)
        if (_themeInit)
        {
            InvertPdfCheckbox.IsChecked = name == "Dark";
            App.RenderInverted = name == "Dark";
        }
    }

    private void OnInvertPdfToggled(object sender, RoutedEventArgs e)
    {
        App.RenderInverted = InvertPdfCheckbox.IsChecked == true;
    }

    private void OnDpiChanged(object sender, SelectionChangedEventArgs e)
    {
        if (DpiSelector.SelectedItem is ComboBoxItem item
            && int.TryParse(item.Content?.ToString(), out int dpi) && dpi > 0)
        {
            App.RenderDpi = dpi;
        }
    }

    private void ApplyDefaults()
    {
        _engine.TesseractPath = TesseractPath.Text;
        _engine.OcrLanguage = OcrLanguage.Text;
        _engine.OcrWorkers = (uint)OcrWorkers.Value;
        _engine.OcrMaxDim = (uint)OcrMaxDim.Value;

        ApplyRamBuffer();
        _engine.IndexerBatchSize = (uint)BatchSize.Value;
        _engine.CommitInterval = (uint)CommitInterval.Value;
        _engine.CommitTimeout = (uint)CommitTimeout.Value;
        _engine.ExtractWorkers = (uint)ExtractWorkers.Value;
        _engine.ChannelCapacity = (uint)ChannelCapacity.Value;
    }

    private void ApplyRamBuffer()
    {
        var values = new ulong[] { 268_435_456, 536_870_912, 1_073_741_824, 2_147_483_648, 3_000_000_000, 4_294_967_296 };
        _engine.RamBuffer = values[RamBuffer.SelectedIndex];
    }
}
