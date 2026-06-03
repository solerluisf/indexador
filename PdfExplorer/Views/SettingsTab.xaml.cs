using System.Windows;
using System.Windows.Controls;
using PdfExplorer.Services;

namespace PdfExplorer.Views;

public partial class SettingsTab : Page
{
    private readonly PdfEngine _engine;

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
            var id = (uint)coll.Id; // capture for closure
            slider.ValueChanged += (_, _) => _engine.SetCollectionBoost(id, (float)slider.Value);
            row.Children.Add(slider);
            BoostsPanel.Children.Add(row);
        }
    }
}
