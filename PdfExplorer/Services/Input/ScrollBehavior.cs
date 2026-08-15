using System;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Input;
using System.Windows.Media;

namespace PdfExplorer.Services.Input;

public static class ScrollBehavior
{
    private static void Log(string msg)
    {
    }

    public static readonly DependencyProperty EnableMouseDragProperty =
        DependencyProperty.RegisterAttached(
            "EnableMouseDrag",
            typeof(bool),
            typeof(ScrollBehavior),
            new PropertyMetadata(false, OnEnableMouseDragChanged));

    public static bool GetEnableMouseDrag(DependencyObject obj) =>
        (bool)obj.GetValue(EnableMouseDragProperty);

    public static void SetEnableMouseDrag(DependencyObject obj, bool value) =>
        obj.SetValue(EnableMouseDragProperty, value);

    private static readonly DependencyProperty IsDraggingProperty =
        DependencyProperty.RegisterAttached(
            "IsDragging", typeof(bool), typeof(ScrollBehavior),
            new PropertyMetadata(false));

    private static readonly DependencyProperty LastPointProperty =
        DependencyProperty.RegisterAttached(
            "LastPoint", typeof(Point), typeof(ScrollBehavior),
            new PropertyMetadata(default(Point)));

    private static readonly DependencyProperty TotalDeltaProperty =
        DependencyProperty.RegisterAttached(
            "TotalDelta", typeof(double), typeof(ScrollBehavior),
            new PropertyMetadata(0.0));

    private static void OnEnableMouseDragChanged(
        DependencyObject d, DependencyPropertyChangedEventArgs e)
    {
        if (d is not ScrollViewer sv) return;
        Log($"EnableMouseDragChanged: NewValue={e.NewValue}");
        if ((bool)e.NewValue)
            Attach(sv);
        else
            Detach(sv);
    }

    private static void Attach(ScrollViewer sv)
    {
        Log("Attach");
        SubscribeMouseHandlers(sv);
        sv.Loaded += OnLoaded;
        sv.Unloaded += OnUnloaded;
    }

    private static void Detach(ScrollViewer sv)
    {
        Log("Detach");
        UnsubscribeMouseHandlers(sv);
        sv.Loaded -= OnLoaded;
        sv.Unloaded -= OnUnloaded;
    }

    private static void SubscribeMouseHandlers(ScrollViewer sv)
    {
        Log("SubscribeMouseHandlers");
        sv.PreviewMouseLeftButtonDown += OnPreviewMouseLeftButtonDown;
        sv.PreviewMouseMove += OnPreviewMouseMove;
        sv.PreviewMouseLeftButtonUp += OnPreviewMouseLeftButtonUp;
        sv.MouseEnter += OnMouseEnter;
        sv.MouseLeave += OnMouseLeave;
    }

    private static void UnsubscribeMouseHandlers(ScrollViewer sv)
    {
        Log("UnsubscribeMouseHandlers");
        sv.PreviewMouseLeftButtonDown -= OnPreviewMouseLeftButtonDown;
        sv.PreviewMouseMove -= OnPreviewMouseMove;
        sv.PreviewMouseLeftButtonUp -= OnPreviewMouseLeftButtonUp;
        sv.MouseEnter -= OnMouseEnter;
        sv.MouseLeave -= OnMouseLeave;
    }

    private static void OnMouseEnter(object sender, MouseEventArgs e)
    {
        if (sender is ScrollViewer sv && Mouse.Captured != sv)
            sv.Cursor = IsOverScrollBar(sv, e.GetPosition(sv)) ? null : Cursors.Hand;
    }

    private static void OnMouseLeave(object sender, MouseEventArgs e)
    {
        if (sender is ScrollViewer sv)
        {
            sv.Cursor = null;
            if (sv.GetValue(IsDraggingProperty) is true)
            {
                Log("MouseLeave -> EndDrag");
                EndDrag(sv);
            }
        }
    }

    private static void OnLoaded(object sender, RoutedEventArgs e)
    {
        Log("OnLoaded");
    }

    private static void OnUnloaded(object sender, RoutedEventArgs e)
    {
        Log("OnUnloaded");
    }

    private static void OnPreviewMouseLeftButtonDown(object sender, MouseButtonEventArgs e)
    {
        if (sender is not ScrollViewer sv) return;

        // Let the native ScrollBar handle clicks on its thumb/track/repeat
        // buttons.  Capturing here would steal the drag from the scrollbar.
        if (IsOverScrollBar(sv, e.GetPosition(sv)))
            return;

        sv.Cursor = Cursors.ScrollAll;
        sv.SetValue(IsDraggingProperty, true);
        sv.SetValue(LastPointProperty, e.GetPosition(sv));
        sv.SetValue(TotalDeltaProperty, 0.0);

        var captured = Mouse.Capture(sv, CaptureMode.Element);
        Log($"PreviewMouseLeftButtonDown: captured={captured}, Captured==sv={Mouse.Captured == sv}");

        InputService.Instance.RaiseDragStarted(new DragInfo
        {
            Position = e.GetPosition(sv),
        });

        e.Handled = true;
    }

    private static void OnPreviewMouseMove(object sender, MouseEventArgs e)
    {
        if (sender is not ScrollViewer sv) return;

        if (Mouse.Captured == sv)
        {
            var current = e.GetPosition(sv);
            var last = (Point)sv.GetValue(LastPointProperty);
            var deltaY = current.Y - last.Y;

            if (Math.Abs(deltaY) < 0.5)
                return;

            var totalDelta = (double)sv.GetValue(TotalDeltaProperty) + deltaY;

            Log($"MouseMove DRAG: deltaY={deltaY:F1}");

            sv.SetValue(LastPointProperty, current);
            sv.SetValue(TotalDeltaProperty, totalDelta);

            InputService.Instance.RaiseDragDelta(new DragInfo
            {
                DeltaY = deltaY,
                TotalDeltaY = totalDelta,
                Position = current,
            });
        }
        else if (sv.IsMouseOver)
        {
            // Only show the grab cursor over the content; leave the scrollbar
            // free to use its own cursor/interaction.
            sv.Cursor = IsOverScrollBar(sv, e.GetPosition(sv)) ? null : Cursors.Hand;
        }
    }

    private static void OnPreviewMouseLeftButtonUp(object sender, MouseButtonEventArgs e)
    {
        if (sender is ScrollViewer sv && Mouse.Captured == sv)
        {
            Log("PreviewMouseLeftButtonUp -> EndDrag");
            EndDrag(sv);
        }
    }

    private static void EndDrag(ScrollViewer sv)
    {
        Mouse.Capture(null);
        sv.Cursor = Cursors.Hand;
        sv.SetValue(IsDraggingProperty, false);

        InputService.Instance.RaiseDragCompleted(new DragInfo
        {
            Position = new Point(0, (double)sv.GetValue(TotalDeltaProperty)),
        });
    }

    /// <summary>
    /// Hit-test whether <paramref name="pt"/> (relative to <paramref name="sv"/>)
    /// currently lies over the <see cref="ScrollBar"/> of this <see cref="ScrollViewer"/>.
    /// </summary>
    /// <remarks>
    /// The drag-scroll feature must not interfere with the native scrollbar:
    /// when the pointer is over the bar we neither capture the click nor force
    /// the grab cursor, so the user can still drag the thumb or click the
    /// track/repeat buttons.
    /// </remarks>
    private static bool IsOverScrollBar(ScrollViewer sv, Point pt)
    {
        if (double.IsNaN(pt.X) || double.IsNaN(pt.Y))
            return false;

        var hitResult = VisualTreeHelper.HitTest(sv, pt);
        DependencyObject? current = hitResult?.VisualHit;
        while (current is not null && !ReferenceEquals(current, sv))
        {
            if (current is ScrollBar)
                return true;
            current = VisualTreeHelper.GetParent(current);
        }
        return false;
    }
}
