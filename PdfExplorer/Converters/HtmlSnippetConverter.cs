using System;
using System.Globalization;
using System.Text.RegularExpressions;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Data;
using System.Windows.Documents;
using System.Windows.Media;

namespace PdfExplorer.Converters;

/// <summary>
/// Converts a snippet string containing simple HTML (e.g. &lt;b&gt;term&lt;/b&gt;)
/// into a TextBlock with bold Runs so the matched terms stand out in WPF.
/// </summary>
[ValueConversion(typeof(string), typeof(TextBlock))]
public sealed class HtmlSnippetConverter : IValueConverter
{
    private static readonly Regex BoldRegex = new(
        @"<b>(.*?)</b>",
        RegexOptions.Compiled | RegexOptions.IgnoreCase);

    public object? Convert(object value, Type targetType, object parameter, CultureInfo culture)
    {
        if (value is not string text)
            return null;

        var tb = new TextBlock
        {
            TextWrapping = TextWrapping.Wrap,
            FontSize = 10,
            Opacity = 0.6,
        };

        if (Application.Current?.TryFindResource("ControlForeground") is SolidColorBrush fg)
            tb.Foreground = fg;

        if (string.IsNullOrEmpty(text))
            return tb;

        int lastIndex = 0;
        foreach (Match m in BoldRegex.Matches(text))
        {
            if (m.Index > lastIndex)
            {
                tb.Inlines.Add(new Run(text.Substring(lastIndex, m.Index - lastIndex)));
            }

            var boldRun = new Run(m.Groups[1].Value)
            {
                FontWeight = FontWeights.Bold,
            };
            if (Application.Current?.TryFindResource("SearchHighlightBackground") is SolidColorBrush bg)
                boldRun.Background = bg;
            tb.Inlines.Add(boldRun);
            lastIndex = m.Index + m.Length;
        }

        if (lastIndex < text.Length)
        {
            tb.Inlines.Add(new Run(text.Substring(lastIndex)));
        }

        return tb;
    }

    public object ConvertBack(object value, Type targetType, object parameter, CultureInfo culture)
    {
        throw new NotSupportedException();
    }
}
