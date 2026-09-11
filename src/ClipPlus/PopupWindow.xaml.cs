using System;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Interop;

namespace ClipPlus;

/// <summary>
/// The search popup. Deliberately not the app's MainWindow: it is shown and
/// hidden on demand and the process outlives it.
/// </summary>
public partial class PopupWindow : Window
{
    private const int MaxResults = 300;

    private readonly ClipStore _store;
    private readonly IntPtr _handle;

    // internal, not public: ClipStore is an internal type and a public constructor
    // cannot expose it without a CS0051 accessibility error.
    internal PopupWindow(ClipStore store)
    {
        _store = store;

        InitializeComponent();

        // Force the HWND up front so TargetWindow can be compared against it, and
        // so the first show has no handle-creation latency.
        _handle = new WindowInteropHelper(this).EnsureHandle();

        // Wired here rather than in XAML: the event fires during InitializeComponent
        // and would otherwise run against half-built state.
        SearchBox.TextChanged += OnSearchTextChanged;
    }

    /// <summary>Raised with the chosen entry; the app owns the paste-back.</summary>
    public event Action<ClipItem>? Commit;

    /// <summary>Window that had focus before the popup opened. The paste target.</summary>
    public IntPtr TargetWindow { get; private set; }

    public void Toggle()
    {
        if (IsVisible)
        {
            Hide();
            return;
        }

        ShowPopup();
    }

    public void ShowPopup()
    {
        // Captured before this window takes focus, otherwise it is already too late.
        TargetWindow = Native.GetForegroundWindow();
        if (TargetWindow == _handle)
        {
            TargetWindow = IntPtr.Zero;
        }

        SearchBox.Text = "";
        Reload();
        PositionAtCursor();

        Show();
        Activate();

        // Windows' foreground lock sometimes refuses activation. Without getting
        // past it the user's typing would silently land in the window underneath.
        if (!IsActive)
        {
            Native.ForceForeground(_handle);
        }
        SearchBox.Focus();
        Keyboard.Focus(SearchBox);

        // Second pass: now that the window sits on its monitor, the composition
        // target knows the real scaling and the placement can be corrected.
        PositionAtCursor();
    }

    private void Reload()
    {
        var items = _store.Query(SearchBox.Text, MaxResults);
        ResultList.ItemsSource = items;
        ResultList.SelectedIndex = items.Count > 0 ? 0 : -1;
    }

    private void PositionAtCursor()
    {
        var (cursorX, cursorY) = Native.CursorPosition();
        var area = System.Windows.Forms.Screen
            .FromPoint(new System.Drawing.Point(cursorX, cursorY))
            .WorkingArea;

        // DIPs per device pixel. Before the window is shown there is no composition
        // target, so fall back to the DPI of the monitor under the cursor.
        double dipPerPixel;
        var transform = PresentationSource.FromVisual(this)?.CompositionTarget?.TransformFromDevice;
        if (transform is { } matrix && matrix.M11 > 0)
        {
            dipPerPixel = matrix.M11;
        }
        else
        {
            dipPerPixel = 96.0 / Native.DpiAt(cursorX, cursorY);
        }

        var widthPixels = Width / dipPerPixel;
        var heightPixels = Height / dipPerPixel;

        var leftPixels = cursorX + 16.0;
        var topPixels = cursorY + 16.0;

        if (leftPixels + widthPixels > area.Right)
        {
            leftPixels = area.Right - widthPixels;
        }

        if (topPixels + heightPixels > area.Bottom)
        {
            topPixels = cursorY - heightPixels - 16.0;
        }

        Left = Math.Max(area.Left, leftPixels) * dipPerPixel;
        Top = Math.Max(area.Top, topPixels) * dipPerPixel;
    }

    private void OnSearchTextChanged(object? sender, TextChangedEventArgs e) => Reload();

    private void OnDeactivated(object? sender, EventArgs e) => Hide();

    private void OnSelectionChanged(object? sender, SelectionChangedEventArgs e)
    {
        if (ResultList.SelectedItem is not null)
        {
            ResultList.ScrollIntoView(ResultList.SelectedItem);
        }
    }

    private void OnResultDoubleClick(object? sender, MouseButtonEventArgs e)
    {
        if (ResultList.SelectedItem is ClipItem)
        {
            Accept();
        }
    }

    private void OnPreviewKeyDown(object? sender, KeyEventArgs e)
    {
        switch (e.Key)
        {
            case Key.Escape:
                e.Handled = true;
                Hide();
                break;

            case Key.Enter:
                e.Handled = true;
                Accept();
                break;

            case Key.Up:
                e.Handled = true;
                MoveSelection(-1);
                break;

            case Key.Down:
                e.Handled = true;
                MoveSelection(1);
                break;

            case Key.PageUp:
                e.Handled = true;
                MoveSelection(-8);
                break;

            case Key.PageDown:
                e.Handled = true;
                MoveSelection(8);
                break;
        }
    }

    private void MoveSelection(int delta)
    {
        var count = ResultList.Items.Count;
        if (count == 0)
        {
            return;
        }

        var current = ResultList.SelectedIndex;
        var next = current < 0
            ? delta > 0 ? 0 : count - 1
            : Math.Clamp(current + delta, 0, count - 1);

        ResultList.SelectedIndex = next;
        ResultList.ScrollIntoView(ResultList.Items[next]);
    }

    private void Accept()
    {
        if (ResultList.SelectedItem is ClipItem item)
        {
            Commit?.Invoke(item);
        }
    }
}
