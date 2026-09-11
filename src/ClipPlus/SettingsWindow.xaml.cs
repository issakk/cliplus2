using System;
using System.Globalization;
using System.IO;
using System.Windows;

namespace ClipPlus;

/// <summary>
/// Edits the live <see cref="Settings"/> object in place. Cancel never calls
/// Save, so there is nothing to copy and nothing to roll back.
/// </summary>
public partial class SettingsWindow : Window
{
    private readonly Settings _settings;

    // internal, not public: Settings is an internal type and a public constructor
    // could not expose it without a CS0051 accessibility error.
    internal SettingsWindow(Settings settings)
    {
        _settings = settings;

        InitializeComponent();

        HotkeyBox.Text = settings.Hotkey;
        RetentionBox.Text = settings.RetentionDays.ToString(CultureInfo.InvariantCulture);
        SyncRootBox.Text = settings.SyncRootOverride ?? "";
        CaptureTextBox.IsChecked = settings.CaptureText;
        CaptureImageBox.IsChecked = settings.CaptureImages;
        CaptureFileBox.IsChecked = settings.CaptureFiles;
        AutoStartBox.IsChecked = AutoStart.IsEnabled();
    }

    private void OnSave(object? sender, RoutedEventArgs e)
    {
        var hotkey = HotkeyBox.Text.Trim();
        if (Settings.ParseHotkeyText(hotkey) is null)
        {
            Warn("热键格式不对。需要 Ctrl / Alt / Shift / Win 里至少一个，加一个键，例如 Win+Alt+V。");
            return;
        }

        if (!int.TryParse(RetentionBox.Text.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var days)
            || days < 0)
        {
            Warn("自动清理天数请填 0 或正整数。0 表示不清理。");
            return;
        }

        var syncRoot = SyncRootBox.Text.Trim();
        if (syncRoot.Length > 0)
        {
            try
            {
                syncRoot = Path.GetFullPath(syncRoot);
            }
            catch (Exception)
            {
                Warn("同步目录不是一个有效的路径。");
                return;
            }
        }

        _settings.Hotkey = hotkey;
        _settings.RetentionDays = days;
        _settings.SyncRootOverride = syncRoot.Length == 0 ? null : syncRoot;
        _settings.CaptureText = CaptureTextBox.IsChecked == true;
        _settings.CaptureImages = CaptureImageBox.IsChecked == true;
        _settings.CaptureFiles = CaptureFileBox.IsChecked == true;

        // Autostart is registry state rather than a setting, so it is applied here.
        AutoStart.Set(AutoStartBox.IsChecked == true);

        DialogResult = true;
    }

    private void Warn(string message)
        => MessageBox.Show(this, message, "ClipPlus", MessageBoxButton.OK, MessageBoxImage.Warning);
}
