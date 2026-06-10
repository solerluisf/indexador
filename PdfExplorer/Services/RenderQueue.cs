using System.Threading.Channels;

namespace PdfExplorer.Services;

internal enum RenderPriority
{
    High = 0,
    Normal = 1,
    Low = 2
}

internal sealed class RenderRequest
{
    public int MatchIndex { get; }
    public int PageIndex { get; }
    public RenderPriority Priority { get; }

    public RenderRequest(int matchIndex, int pageIndex, RenderPriority priority)
    {
        MatchIndex = matchIndex;
        PageIndex = pageIndex;
        Priority = priority;
    }
}

internal sealed class RenderQueue
{
    private Channel<RenderRequest> _channel;
    private Channel<RenderRequest> _highPriorityChannel;
    private int _cancelled;

    public RenderQueue()
    {
        _channel = CreateChannel();
        _highPriorityChannel = CreateHighChannel();
    }

    private static Channel<RenderRequest> CreateChannel() =>
        Channel.CreateBounded<RenderRequest>(
            new BoundedChannelOptions(128)
            {
                FullMode = BoundedChannelFullMode.DropOldest
            });

    private static Channel<RenderRequest> CreateHighChannel() =>
        Channel.CreateBounded<RenderRequest>(
            new BoundedChannelOptions(16)
            {
                FullMode = BoundedChannelFullMode.DropOldest
            });

    public void Enqueue(int matchIndex, int pageIndex, RenderPriority priority)
    {
        if (Volatile.Read(ref _cancelled) != 0)
            return;

        var target = priority == RenderPriority.High
            ? _highPriorityChannel
            : _channel;

        target.Writer.TryWrite(new RenderRequest(matchIndex, pageIndex, priority));
    }

    public async Task<RenderRequest?> DequeueAsync(CancellationToken ct)
    {
        while (!ct.IsCancellationRequested)
        {
            if (_highPriorityChannel.Reader.TryRead(out var highReq))
                return highReq;
            if (_channel.Reader.TryRead(out var req))
                return req;

            using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
            var hTask = _highPriorityChannel.Reader.WaitToReadAsync(cts.Token).AsTask();
            var nTask = _channel.Reader.WaitToReadAsync(cts.Token).AsTask();

            try
            {
                var completed = await Task.WhenAny(hTask, nTask);
                cts.Cancel();
                if (completed.Status == TaskStatus.RanToCompletion && await completed)
                    continue;
            }
            catch (OperationCanceledException)
            {
                continue;
            }

            return null;
        }
        return null;
    }

    public void Cancel()
    {
        Interlocked.Exchange(ref _cancelled, 1);
        _channel.Writer.TryComplete();
        _highPriorityChannel.Writer.TryComplete();
    }

    public void Reset()
    {
        Interlocked.Exchange(ref _cancelled, 0);
        _channel = CreateChannel();
        _highPriorityChannel = CreateHighChannel();
    }
}