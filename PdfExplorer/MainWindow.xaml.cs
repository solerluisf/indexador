using System.Windows;
using System.Windows.Media;

namespace PdfExplorer;

public partial class MainWindow : Window
{
    private bool _maximized;
    private Rect _restoreBounds;

    public MainWindow()
    {
        InitializeComponent();
        MinimizeBtn.Click += (_, _) => WindowState = WindowState.Minimized;
        MaximizeBtn.Click += OnMaximizeClick;
        CloseBtn.Click += (_, _) => Close();
    }

    private void OnMaximizeClick(object sender, RoutedEventArgs e)
    {
        if (_maximized)
        {
            Left = _restoreBounds.Left;
            Top = _restoreBounds.Top;
            Width = _restoreBounds.Width;
            Height = _restoreBounds.Height;
            _maximized = false;
        }
        else
        {
            _restoreBounds = new Rect(Left, Top, Width, Height);
            var wa = SystemParameters.WorkArea;
            Left = wa.Left;
            Top = wa.Top;
            Width = wa.Width;
            Height = wa.Height;
            _maximized = true;
        }
        UpdateMaximizeState();
    }

    private void OnWindowStateChanged(object sender, EventArgs e)
    {
        if (WindowState == WindowState.Minimized)
            return;

        if (_maximized)
        {
            var wa = SystemParameters.WorkArea;
            Dispatcher.BeginInvoke(new Action(() =>
            {
                Left = wa.Left;
                Top = wa.Top;
                Width = wa.Width;
                Height = wa.Height;
            }), System.Windows.Threading.DispatcherPriority.ApplicationIdle);
        }
    }

    private void UpdateMaximizeState()
    {
        MaximizeBtn.ToolTip = _maximized ? "Restore" : "Maximize";
        MaxIcon.Visibility = _maximized ? Visibility.Collapsed : Visibility.Visible;
        RestIcon.Visibility = _maximized ? Visibility.Visible : Visibility.Collapsed;
    }

    private void OnCloseMouseEnter(object sender, System.Windows.Input.MouseEventArgs e)
    {
        CloseBtn.Background = new SolidColorBrush(Color.FromRgb(0xE8, 0x11, 0x23));
        CloseIcon.Stroke = Brushes.White;
    }

    private void OnCloseMouseLeave(object sender, System.Windows.Input.MouseEventArgs e)
    {
        CloseBtn.Background = Brushes.Transparent;
        CloseIcon.Stroke = (Brush)FindResource("PageForeground");
    }
}
