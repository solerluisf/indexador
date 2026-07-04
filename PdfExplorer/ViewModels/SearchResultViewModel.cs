using System.ComponentModel;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.ViewModels;

/// <summary>
/// Presentation wrapper around <see cref="SearchResult"/> that supports
/// asynchronous thumbnail loading via <see cref="INotifyPropertyChanged"/>.
/// </summary>
public sealed class SearchResultViewModel : INotifyPropertyChanged
{
    private readonly SearchResult _model;
    private BitmapSource? _thumbnail;
    private bool _isLoadingThumbnail;

    public SearchResultViewModel(SearchResult model)
    {
        _model = model ?? throw new ArgumentNullException(nameof(model));
    }

    // ── Passthrough properties ────────────────────────────────────

    public long Id => _model.Id;
    public double Score => _model.Score;
    public string Path => _model.Path;
    public string Snippet => _model.Snippet;
    public long? CollectionId => _model.CollectionId;
    public string FileName => _model.FileName;
    public string FolderPath => _model.FolderPath;
    public IReadOnlyList<string>? MatchedTerms => _model.MatchedTerms;
    public IReadOnlyList<IReadOnlyList<string>>? PhraseGroups => _model.PhraseGroups;

    // ── Observable thumbnail ──────────────────────────────────────

    public BitmapSource? Thumbnail
    {
        get => _thumbnail;
        set
        {
            if (!ReferenceEquals(_thumbnail, value))
            {
                _thumbnail = value;
                if (value is not null)
                {
                    Console.Error.WriteLine($"[SearchResultViewModel] Thumbnail SET for {FileName}: {value.PixelWidth}x{value.PixelHeight}");
                    LogHelper.Log("SearchResultViewModel", $"Thumbnail SET for {FileName}: {value.PixelWidth}x{value.PixelHeight}");
                }
                else
                {
                    Console.Error.WriteLine($"[SearchResultViewModel] Thumbnail CLEARED for {FileName}");
                    LogHelper.Log("SearchResultViewModel", $"Thumbnail CLEARED for {FileName}");
                }
                OnPropertyChanged(nameof(Thumbnail));
            }
        }
    }

    public bool IsLoadingThumbnail
    {
        get => _isLoadingThumbnail;
        set
        {
            if (_isLoadingThumbnail != value)
            {
                _isLoadingThumbnail = value;
                OnPropertyChanged(nameof(IsLoadingThumbnail));
            }
        }
    }

    // ── INotifyPropertyChanged ────────────────────────────────────

    public event PropertyChangedEventHandler? PropertyChanged;

    private void OnPropertyChanged(string propertyName)
        => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));
}
