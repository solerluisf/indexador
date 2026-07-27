using System;

namespace PdfExplorer.Services.Input;

public sealed class InputService
{
    public static InputService Instance { get; } = new();
    private InputService() { }

    public event EventHandler<DragInfo>? DragStarted;
    public event EventHandler<DragInfo>? DragDelta;
    public event EventHandler<DragInfo>? DragCompleted;

    public void RaiseDragStarted(DragInfo info) => DragStarted?.Invoke(this, info);
    public void RaiseDragDelta(DragInfo info) => DragDelta?.Invoke(this, info);
    public void RaiseDragCompleted(DragInfo info) => DragCompleted?.Invoke(this, info);
}
