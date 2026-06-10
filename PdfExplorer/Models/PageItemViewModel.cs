using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Windows.Media.Imaging;

namespace PdfExplorer.Models;

internal sealed class PageItemViewModel : INotifyPropertyChanged
{
    public int MatchIndex { get; }
    public int PageNumber { get; }
    public string PageHeader => $"Page {PageNumber}";
    public double EstimatedHeight { get; }

    private BitmapSource? _imageSource;
    public BitmapSource? ImageSource
    {
        get => _imageSource;
        set
        {
            if (ReferenceEquals(_imageSource, value))
                return;
            _imageSource = value;
            OnPropertyChanged();
            OnPropertyChanged(nameof(HasImage));
        }
    }

    public bool HasImage => _imageSource is not null;

    public PageItemViewModel(int matchIndex, int pageNumber, double estimatedHeight)
    {
        MatchIndex = matchIndex;
        PageNumber = pageNumber;
        EstimatedHeight = estimatedHeight;
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    private void OnPropertyChanged([CallerMemberName] string? name = null)
    {
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
    }
}