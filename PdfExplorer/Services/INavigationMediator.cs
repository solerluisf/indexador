using PdfExplorer.Models;

namespace PdfExplorer.Services;

public interface INavigationMediator
{
    event EventHandler<EventArgs>? StateChanged;

    int CurrentMatchIndex { get; }
    int CurrentPositionIndex { get; }
    int TotalMatchPages { get; }
    int TotalPositions { get; }
    int TotalPhraseCount { get; }
    int CurrentPhraseIndex { get; }
    bool CanGoNextMatch { get; }
    bool CanGoPrevMatch { get; }
    bool CanGoNextPosition { get; }
    bool CanGoPrevPosition { get; }

    void SetContext(IReadOnlyList<WordPosition> positions, IReadOnlyList<int> matchingPages, IReadOnlyList<string>? matchedTerms = null, bool isBooleanMode = false);
    bool GotoNextMatch();
    bool GotoPrevMatch();
    bool GotoNextPosition();
    bool GotoPrevPosition();
    void GotoPosition(int posIdx);
    void GotoPage(int pageIdx);
    void GotoInitialPosition();
    int GetMatchPage(int matchIdx);
    int FindMatchForPage(int pageIdx);
    void Reset();
}
