using System;
using System.Windows;

namespace PdfExplorer.Services.Input;

public class DragInfo : EventArgs
{
    public double DeltaY { get; init; }
    public double TotalDeltaY { get; init; }
    public Point Position { get; init; }
    public bool IsHorizontal { get; init; }
}
