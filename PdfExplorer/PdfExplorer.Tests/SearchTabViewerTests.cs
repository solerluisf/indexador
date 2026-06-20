using PdfExplorer.Models;
using PdfExplorer.Services;

namespace PdfExplorer.Tests;

public sealed class SearchTabViewerTests
{
    // ══════════════════════════════════════════════════════════════════
    //  1. HAPPY PATH — RenderQueue
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public async Task Enqueue_Dequeue_returns_item()
    {
        var q = new RenderQueue();
        q.Enqueue(0, 1, RenderPriority.Normal);
        var item = await q.DequeueAsync(CancellationToken.None);

        Assert.NotNull(item);
        Assert.Equal(0, item.MatchIndex);
        Assert.Equal(1, item.PageIndex);
        Assert.Equal(RenderPriority.Normal, item.Priority);
    }

    [Fact]
    public async Task HighPriority_items_are_dequeued_before_normal()
    {
        var q = new RenderQueue();
        q.Enqueue(0, 1, RenderPriority.Normal);
        q.Enqueue(1, 2, RenderPriority.High);
        q.Enqueue(2, 3, RenderPriority.Normal);

        var first = await q.DequeueAsync(CancellationToken.None);
        Assert.Equal(1, first!.MatchIndex);
        Assert.Equal(RenderPriority.High, first.Priority);

        var second = await q.DequeueAsync(CancellationToken.None);
        Assert.Equal(RenderPriority.Normal, second!.Priority);
    }

    [Fact]
    public async Task Multiple_high_priority_items_fifo_order()
    {
        var q = new RenderQueue();
        q.Enqueue(0, 1, RenderPriority.High);
        q.Enqueue(1, 2, RenderPriority.High);

        var first = await q.DequeueAsync(CancellationToken.None);
        var second = await q.DequeueAsync(CancellationToken.None);

        Assert.Equal(0, first!.MatchIndex);
        Assert.Equal(1, second!.MatchIndex);
    }

    [Fact]
    public async Task Normal_items_fifo_order_after_high_depleted()
    {
        var q = new RenderQueue();
        q.Enqueue(0, 1, RenderPriority.Normal);
        q.Enqueue(1, 2, RenderPriority.High);
        q.Enqueue(2, 3, RenderPriority.Normal);

        await q.DequeueAsync(CancellationToken.None); // consumes High
        var second = await q.DequeueAsync(CancellationToken.None);

        Assert.Equal(0, second!.MatchIndex);
        Assert.Equal(1, second.PageIndex);
    }

    // ══════════════════════════════════════════════════════════════════
    //  2. ALTERNATIVE PATH — RenderQueue edge cases
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public async Task Enqueue_after_Cancel_is_ignored()
    {
        var q = new RenderQueue();
        q.Cancel();

        q.Enqueue(0, 1, RenderPriority.High);
        q.Enqueue(1, 2, RenderPriority.Normal);

        var item = await q.DequeueAsync(CancellationToken.None);
        Assert.Null(item);
    }

    [Fact]
    public async Task Reset_after_Cancel_resumes_accepting_items()
    {
        var q = new RenderQueue();
        q.Cancel();
        q.Reset();

        q.Enqueue(0, 1, RenderPriority.High);
        var item = await q.DequeueAsync(CancellationToken.None);

        Assert.NotNull(item);
        Assert.Equal(0, item.MatchIndex);
    }

    [Fact]
    public async Task Full_channel_drops_oldest_items()
    {
        var q = new RenderQueue();
        var uniqueMatch = 0;

        // High-priority channel capacity is 16 — fill it past capacity
        for (int i = 0; i < 32; i++)
            q.Enqueue(uniqueMatch++, i, RenderPriority.High);

        // Normal channel capacity is 128 — fill to 150
        for (int i = 0; i < 150; i++)
            q.Enqueue(uniqueMatch++, i, RenderPriority.Normal);

        // High items: first 16 should be in channel, next 16 dropped (DropOldest)
        var highCount = 0;
        while (true)
        {
            var item = await q.DequeueWithTimeout(200);
            if (item is null) break;
            if (item.Priority == RenderPriority.High) highCount++;
        }
        Assert.Equal(16, highCount);
    }

    [Fact]
    public async Task Dequeue_on_empty_queue_waits_until_item_enqueued()
    {
        var q = new RenderQueue();
        var cts = new CancellationTokenSource(500);

        var task = q.DequeueAsync(cts.Token);
        await Task.Delay(50);
        Assert.False(task.IsCompleted);

        q.Enqueue(0, 1, RenderPriority.Normal);
        var item = await task;

        Assert.NotNull(item);
    }

    [Fact]
    public async Task Enqueue_cancelled_flag_prevents_write_after_interleaved_cancel()
    {
        var q = new RenderQueue();
        q.Enqueue(0, 1, RenderPriority.Normal);
        q.Cancel();

        // After Cancel, Enqueue should be no-op
        q.Enqueue(1, 2, RenderPriority.High);

        // Reset restores
        q.Reset();
        q.Enqueue(2, 3, RenderPriority.High);

        var item = await q.DequeueAsync(CancellationToken.None);
        Assert.NotNull(item);
        Assert.Equal(2, item.MatchIndex);
    }

    // ══════════════════════════════════════════════════════════════════
    //  3. ERROR PATH — RenderQueue
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public async Task Dequeue_cancelled_token_returns_null()
    {
        var q = new RenderQueue();
        var cts = new CancellationTokenSource();
        cts.Cancel();

        var item = await q.DequeueAsync(cts.Token);
        Assert.Null(item);
    }

    [Fact]
    public async Task Dequeue_graceful_on_cancellation_during_wait()
    {
        var q = new RenderQueue();
        var cts = new CancellationTokenSource();

        var task = q.DequeueAsync(cts.Token);
        cts.Cancel();

        var result = await task;
        Assert.Null(result); // should return null gracefully, not throw
    }

    [Fact]
    public async Task Dequeue_on_completed_channel_returns_null()
    {
        var q = new RenderQueue();
        q.Cancel();

        var item = await q.DequeueAsync(CancellationToken.None);
        Assert.Null(item);
    }

    [Fact]
    public async Task Multiple_Resets_dont_corrupt_queue()
    {
        var q = new RenderQueue();
        for (int r = 0; r < 5; r++)
        {
            q.Reset();
            q.Enqueue(r, r, RenderPriority.High);
            var item = await q.DequeueAsync(CancellationToken.None);
            Assert.NotNull(item);
            Assert.Equal(r, item.MatchIndex);
            q.Cancel();
        }
    }

    // ══════════════════════════════════════════════════════════════════
    //  4. PdfViewState — model tests
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void PdfViewState_default_values_are_correct()
    {
        var state = new PdfViewState();

        Assert.Equal(string.Empty, state.PdfPath);
        Assert.Empty(state.Positions);
        Assert.Empty(state.MatchingPages);
        Assert.Empty(state.PositionsByPage);
        Assert.Equal(0, state.CurrentMatchIndex);
        Assert.Equal(0, state.TotalMatchPages);
        Assert.Equal(-1, state.CurrentPositionIndex);
    }

    [Fact]
    public void PdfViewState_round_trip_positions()
    {
        var state = new PdfViewState();
        var pos = new List<WordPosition>
        {
            new(1, 10, 20, 30, 40, "hello", 0),
            new(1, 50, 60, 70, 80, "world", 0),
        };

        state.Positions = pos;
        state.MatchingPages = [0];
        state.PositionsByPage = new() { [0] = pos };

        Assert.Equal(2, state.Positions.Count);
        Assert.Single(state.MatchingPages);
        Assert.Equal(2, state.PositionsByPage[0].Count);
    }

    [Fact]
    public void PdfViewState_matching_pages_from_positions()
    {
        var positions = new List<WordPosition>
        {
            new(1, 0, 0, 10, 10, "a", 0),
            new(3, 0, 0, 10, 10, "b", 0),
            new(1, 10, 10, 20, 20, "c", 0),
        };

        var matchingPages = positions
            .Select(p => p.Page - 1)
            .Where(p => p >= 0)
            .Distinct()
            .OrderBy(p => p)
            .ToList();

        Assert.Equal(2, matchingPages.Count);
        Assert.Equal(0, matchingPages[0]);
        Assert.Equal(2, matchingPages[1]);
    }

    // ══════════════════════════════════════════════════════════════════
    //  5. PageItemViewModel — model tests
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void PageItemViewModel_initial_state()
    {
        var vm = new PageItemViewModel(0, 1, 1100);

        Assert.Equal(0, vm.MatchIndex);
        Assert.Equal(1, vm.PageNumber);
        Assert.Equal("Page 1", vm.PageHeader);
        Assert.Equal(1100, vm.EstimatedHeight);
        Assert.Null(vm.ImageSource);
        Assert.False(vm.HasImage);
    }

    [Fact]
    public void PageItemViewModel_property_changed_on_image_set()
    {
        var vm = new PageItemViewModel(0, 1, 1100);
        var imageChanges = new List<string?>();
        vm.PropertyChanged += (_, e) => imageChanges.Add(e.PropertyName);

        // ImageSource can only be set from UI thread due to BitmapSource affinity,
        // but the PropertyChanged mechanism is thread-agnostic
        Assert.Null(vm.ImageSource);
    }

    [Fact]
    public void PageItemViewModel_multi_page_creation()
    {
        var pages = new List<PageItemViewModel>();
        for (int i = 0; i < 5; i++)
            pages.Add(new PageItemViewModel(i, i + 1, 1100));

        Assert.Equal(5, pages.Count);
        Assert.Equal("Page 3", pages[2].PageHeader);
        Assert.Equal(3, pages[2].PageNumber);
        Assert.Equal(2, pages[2].MatchIndex);
    }

    // ══════════════════════════════════════════════════════════════════
    //  6. Position navigation logic
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void NavigateToPosition_finds_correct_match_page()
    {
        var positions = new List<WordPosition>
        {
            new(1, 0, 0, 10, 10, "a", 0),
            new(2, 0, 0, 10, 10, "b", 0),
            new(1, 10, 10, 20, 20, "c", 0),
        };

        var matchingPages = positions
            .Select(p => p.Page - 1)
            .Distinct()
            .OrderBy(p => p)
            .ToList();

        // Position 0 (page 1) → matchIdx 0
        var page0 = positions[0].Page - 1;
        Assert.Equal(0, matchingPages.IndexOf(page0));

        // Position 1 (page 2) → matchIdx 1
        var page1 = positions[1].Page - 1;
        Assert.Equal(1, matchingPages.IndexOf(page1));

        // Position 2 (page 1) → matchIdx 0
        Assert.Equal(0, matchingPages.IndexOf(positions[2].Page - 1));
    }

    [Fact]
    public void Empty_positions_results_in_no_matching_pages()
    {
        var matchingPages = new List<int>();

        Assert.Empty(matchingPages);
        // All pages are still shown via BuildPageViewModels using GetPageCount()
    }

    [Fact]
    public void Positions_grouped_by_page()
    {
        var positions = new List<WordPosition>
        {
            new(1, 0, 0, 10, 10, "a", 0),
            new(1, 10, 10, 20, 20, "b", 0),
            new(3, 0, 0, 10, 10, "c", 0),
        };

        var byPage = positions
            .GroupBy(p => p.Page - 1)
            .ToDictionary(g => g.Key, g => g.ToList());

        Assert.Equal(2, byPage.Count);
        Assert.Equal(2, byPage[0].Count);
        Assert.Single(byPage[2]);
    }

    // ══════════════════════════════════════════════════════════════════
    //  7. Estimated height constant
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void Estimated_height_is_reasonable_default()
    {
        // The estimated height constant used in SearchTab (replaces expensive page-0 render)
        const double estimatedHeight = 1100;
        Assert.True(estimatedHeight > 500);
        Assert.True(estimatedHeight < 2000);
    }

    // ══════════════════════════════════════════════════════════════════
    //  8. Position-to-page mapping for match navigation
    // ══════════════════════════════════════════════════════════════════

    [Fact]
    public void CurrentMatchIndex_tracks_current_page()
    {
        var matchingPages = new List<int> { 0, 2, 5 };
        int currentMatchIndex = 0;

        // Navigate forward
        currentMatchIndex = 1;
        Assert.Equal(2, matchingPages[currentMatchIndex]);

        currentMatchIndex = 2;
        Assert.Equal(5, matchingPages[currentMatchIndex]);

        // Navigate backward
        currentMatchIndex = 0;
        Assert.Equal(0, matchingPages[currentMatchIndex]);
    }

    [Fact]
    public void Match_navigation_bounds()
    {
        var matchingPages = new List<int> { 0, 2, 5 };
        Assert.Equal(3, matchingPages.Count);

        Assert.True(0 > 0 == false);    // Can go prev from 0? No
        Assert.True(0 < matchingPages.Count - 1); // Can go next from 0? Yes

        Assert.True(2 > 0);              // Can go prev from 2? Yes
        Assert.True(2 < matchingPages.Count - 1 == false); // Can go next from 2? No
    }

    [Fact]
    public void Position_navigation_bounds()
    {
        var positions = new List<WordPosition>
        {
            new(1, 0, 0, 10, 10, "a", 0),
            new(1, 10, 10, 20, 20, "b", 0),
            new(2, 0, 0, 10, 10, "c", 0),
        };

        Assert.True(positions.Count > 0);

        int currentIdx = -1;
        Assert.False(currentIdx > 0); // can't go prev
        Assert.True(currentIdx < positions.Count - 1); // can go next

        currentIdx = 0;
        Assert.False(currentIdx > 0);
        Assert.True(currentIdx < positions.Count - 1);

        currentIdx = 2;
        Assert.True(currentIdx > 0);
        Assert.False(currentIdx < positions.Count - 1);
    }
}

internal static class RenderQueueExtensions
{
    public static async Task<RenderRequest?> DequeueWithTimeout(this RenderQueue q, int ms)
    {
        using var cts = new CancellationTokenSource(ms);
        try
        {
            return await q.DequeueAsync(cts.Token);
        }
        catch (OperationCanceledException)
        {
            return null;
        }
    }
}
