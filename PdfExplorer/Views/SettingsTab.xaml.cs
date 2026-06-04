using System.Windows;
using System.Windows.Controls;
using PdfExplorer.Services;

namespace PdfExplorer.Views;

public partial class SettingsTab : Page
{
    private readonly PdfEngine _engine;
    private bool _wired;

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

        if (_wired) return;
        _wired = true;
        WireEvents();
        ApplyDefaults();
    }

    private void WireEvents()
    {
        FuzzyDistance.SelectionChanged += (_, _) => { ApplyFuzzyDistance(); _engine.SaveSettings(); };
        StemEnabled.Checked += (_, _) => { _engine.StemEnabled = true; _engine.SaveSettings(); };
        StemEnabled.Unchecked += (_, _) => { _engine.StemEnabled = false; _engine.SaveSettings(); };
        RecencyWeight.ValueChanged += (_, e) => { _engine.RecencyWeight = (float)e.NewValue; _engine.SaveSettings(); };

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
    }

    private void ApplyDefaults()
    {
        ApplyFuzzyDistance();
        _engine.StemEnabled = StemEnabled.IsChecked == true;
        _engine.RecencyWeight = (float)RecencyWeight.Value;

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

    private void ApplyFuzzyDistance()
    {
        _engine.FuzzyDistance = (uint)FuzzyDistance.SelectedIndex;
    }

    private void ApplyRamBuffer()
    {
        var values = new ulong[] { 268_435_456, 536_870_912, 1_073_741_824, 2_147_483_648, 4_294_967_296 };
        _engine.RamBuffer = values[RamBuffer.SelectedIndex];
    }
}
