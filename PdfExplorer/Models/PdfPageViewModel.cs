using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Windows.Media.Imaging;

namespace PdfExplorer.Models;

internal sealed class PdfPageViewModel : INotifyPropertyChanged
{
    public int PageIndex { get; init; }
    public int MatchIndex { get; init; }
    public double DisplayHeight { get; set; }
    public List<WordPosition> Positions { get; set; } = new();

    public string PageHeader => $"Page {PageIndex + 1}";

    private BitmapSource? _pageImage;
    public BitmapSource? PageImage
    {
        get => _pageImage;
        set { _pageImage = value; OnPropertyChanged(); }
    }

    private bool _isLoading;
    public bool IsLoading
    {
        get => _isLoading;
        set { _isLoading = value; OnPropertyChanged(); }
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    private void OnPropertyChanged([CallerMemberName] string? name = null) =>
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}
