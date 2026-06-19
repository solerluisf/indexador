using System.Collections.Generic;
using System.Linq;
using PdfExplorer.Models;

namespace PdfExplorer.Services;

public sealed class NavigationMediator : INavigationMediator
{
    private IReadOnlyList<WordPosition> _positions = Array.Empty<WordPosition>();
    private IReadOnlyList<int> _matchingPages = Array.Empty<int>();
    private List<int> _matchingPagesList = new();
    private List<int> _phraseStarts = new();
    private bool _isPhraseSearch;
    private int _currentMatchIndex;
    private int _currentPositionIndex = -1;
    private int _totalMatchPages;
    private bool _isNavigating;

    public event EventHandler<EventArgs>? StateChanged;

    public int CurrentMatchIndex => _currentMatchIndex;
    public int CurrentPositionIndex => _currentPositionIndex;
    public int TotalMatchPages => _totalMatchPages;
    public int TotalPositions => _positions.Count;
    public int TotalPhraseCount => _phraseStarts.Count;
    public int CurrentPhraseIndex
    {
        get
        {
            if (_phraseStarts.Count == 0 || _currentPositionIndex < 0)
                return 0;
            int idx = _phraseStarts.BinarySearch(_currentPositionIndex);
            if (idx < 0)
                idx = ~idx - 1;
            return idx >= 0 ? idx : 0;
        }
    }

    public bool CanGoNextMatch => _currentMatchIndex < _totalMatchPages - 1;
    public bool CanGoPrevMatch => _currentMatchIndex > 0;
    public bool CanGoNextPosition => _phraseStarts.Count > 0 && CurrentPhraseIndex < _phraseStarts.Count - 1;
    public bool CanGoPrevPosition => _phraseStarts.Count > 0 && CurrentPhraseIndex > 0;

    public void SetContext(IReadOnlyList<WordPosition> positions, IReadOnlyList<int> matchingPages, string? query = null, bool isBooleanMode = false)
    {
        _positions = positions ?? Array.Empty<WordPosition>();
        _matchingPages = matchingPages ?? Array.Empty<int>();
        _matchingPagesList = matchingPages is List<int> list ? list : matchingPages?.ToList() ?? new List<int>();
        _totalMatchPages = _matchingPages.Count;
        _isPhraseSearch = !isBooleanMode && IsPhraseQuery(query);
        _phraseStarts = _isPhraseSearch ? BuildPhraseStarts(_positions) : BuildPositionsAsPhrases(_positions);
    }

    public bool GotoNextMatch()
    {
        if (_currentMatchIndex >= _totalMatchPages - 1)
            return false;
        _currentMatchIndex++;
        SyncPositionFromMatch();
        FireStateChanged();
        return true;
    }

    public bool GotoPrevMatch()
    {
        if (_currentMatchIndex <= 0)
            return false;
        _currentMatchIndex--;
        SyncPositionFromMatch();
        FireStateChanged();
        return true;
    }

    public bool GotoNextPosition()
    {
        int phraseIdx = CurrentPhraseIndex;
        if (_phraseStarts.Count == 0 || phraseIdx >= _phraseStarts.Count - 1)
            return false;
        _currentPositionIndex = _phraseStarts[phraseIdx + 1];
        SyncMatchFromPosition();
        FireStateChanged();
        return true;
    }

    public bool GotoPrevPosition()
    {
        int phraseIdx = CurrentPhraseIndex;
        if (_phraseStarts.Count == 0 || phraseIdx <= 0)
            return false;
        _currentPositionIndex = _phraseStarts[phraseIdx - 1];
        SyncMatchFromPosition();
        FireStateChanged();
        return true;
    }

    public void GotoPosition(int posIdx)
    {
        if (posIdx < 0 || posIdx >= _positions.Count)
            return;
        _currentPositionIndex = posIdx;
        SyncMatchFromPosition();
        FireStateChanged();
    }

    public void GotoPage(int pageIdx)
    {
        var matchIdx = FindMatchForPage(pageIdx);
        if (matchIdx < 0)
            return;
        _currentMatchIndex = matchIdx;
        SyncPositionFromMatch();
        FireStateChanged();
    }

    public void GotoInitialPosition()
    {
        _currentMatchIndex = 0;
        _currentPositionIndex = _positions.Count > 0 ? 0 : -1;
        FireStateChanged();
    }

    public int GetMatchPage(int matchIdx)
    {
        if (matchIdx < 0 || matchIdx >= _matchingPages.Count)
            return -1;
        return _matchingPages[matchIdx];
    }

    public int FindMatchForPage(int pageIdx)
    {
        return _matchingPagesList.IndexOf(pageIdx);
    }

    public void Reset()
    {
        _positions = Array.Empty<WordPosition>();
        _matchingPages = Array.Empty<int>();
        _matchingPagesList = new List<int>();
        _phraseStarts = new List<int>();
        _currentMatchIndex = 0;
        _currentPositionIndex = -1;
        _totalMatchPages = 0;
        _isNavigating = false;
        FireStateChanged();
    }

    private void SyncPositionFromMatch()
    {
        if (_currentMatchIndex < 0 || _currentMatchIndex >= _matchingPages.Count)
        {
            _currentPositionIndex = -1;
            return;
        }
        var pageIdx = _matchingPages[_currentMatchIndex];
        var posIdx = -1;
        for (int i = 0; i < _positions.Count; i++)
        {
            if (_positions[i].Page - 1 == pageIdx)
            {
                posIdx = i;
                break;
            }
        }
        _currentPositionIndex = posIdx >= 0 ? posIdx : -1;
    }

    private void SyncMatchFromPosition()
    {
        if (_currentPositionIndex < 0 || _currentPositionIndex >= _positions.Count)
        {
            _currentMatchIndex = 0;
            return;
        }
        var pageIdx = _positions[_currentPositionIndex].Page - 1;
        _currentMatchIndex = _matchingPagesList.IndexOf(pageIdx);
        if (_currentMatchIndex < 0)
            _currentMatchIndex = 0;
    }

    private void FireStateChanged()
    {
        StateChanged?.Invoke(this, EventArgs.Empty);
    }

    private static List<int> BuildPhraseStarts(IReadOnlyList<WordPosition> positions)
    {
        var starts = new List<int>(positions.Count);
        for (int i = 0; i < positions.Count; i++)
        {
            if (i == 0 ||
                positions[i].Page != positions[i - 1].Page ||
                positions[i].WordOffset != positions[i - 1].WordOffset + 1)
            {
                starts.Add(i);
            }
        }
        return starts;
    }

    private static List<int> BuildPositionsAsPhrases(IReadOnlyList<WordPosition> positions)
    {
        var starts = new List<int>(positions.Count);
        for (int i = 0; i < positions.Count; i++)
            starts.Add(i);
        return starts;
    }

    private static bool IsPhraseQuery(string? query)
    {
        if (string.IsNullOrWhiteSpace(query))
            return false;
        var trimmed = query.Trim();
        if (!trimmed.Contains(' '))
            return false;
        if (trimmed.Contains('"') || trimmed.Contains('(') || trimmed.Contains(')')
            || trimmed.Contains('+') || trimmed.Contains('-'))
            return false;
        return !trimmed.Split(' ', StringSplitOptions.RemoveEmptyEntries).Any(w =>
            w.Equals("AND", StringComparison.OrdinalIgnoreCase)
            || w.Equals("OR", StringComparison.OrdinalIgnoreCase)
            || w.Equals("NOT", StringComparison.OrdinalIgnoreCase));
    }
}
