using System;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;

namespace ClipPlus;

/// <summary>
/// Tray icon and context menu. Uses the WinForms NotifyIcon because it already
/// ships with the desktop framework — hand-rolling Shell_NotifyIcon plus a
/// message loop would be pure boilerplate for the same result.
/// </summary>
internal sealed class TrayIcon : IDisposable
{
    private readonly System.Windows.Forms.NotifyIcon _notifyIcon;
    private readonly Icon _icon;

    public TrayIcon(Action openHistory, Action openSyncFolder, Action rescan, Action exit)
    {
        _icon = BuildIcon();

        var menu = new System.Windows.Forms.ContextMenuStrip();
        menu.Items.Add("打开剪贴板历史", null, (_, _) => openHistory());
        menu.Items.Add("重新扫描同步目录", null, (_, _) => rescan());
        menu.Items.Add("打开同步文件夹", null, (_, _) => openSyncFolder());

        var autoStart = new System.Windows.Forms.ToolStripMenuItem("开机自启动")
        {
            CheckOnClick = true,
            Checked = AutoStart.IsEnabled(),
        };
        autoStart.CheckedChanged += (_, _) => AutoStart.Set(autoStart.Checked);
        menu.Items.Add(autoStart);

        menu.Items.Add(new System.Windows.Forms.ToolStripSeparator());
        menu.Items.Add("退出", null, (_, _) => exit());

        _notifyIcon = new System.Windows.Forms.NotifyIcon
        {
            Icon = _icon,
            Text = "ClipPlus",
            Visible = true,
            ContextMenuStrip = menu,
        };

        _notifyIcon.DoubleClick += (_, _) => openHistory();
    }

    public void Notify(string title, string text, bool isError = false)
    {
        try
        {
            _notifyIcon.BalloonTipTitle = title;
            _notifyIcon.BalloonTipText = text;
            _notifyIcon.BalloonTipIcon = isError
                ? System.Windows.Forms.ToolTipIcon.Error
                : System.Windows.Forms.ToolTipIcon.Info;
            _notifyIcon.ShowBalloonTip(5000);
        }
        catch (Exception ex)
        {
            Log.Warn("balloon tip failed: " + ex.Message);
        }
    }

    public void Dispose()
    {
        _notifyIcon.Visible = false;
        _notifyIcon.Dispose();
        _icon.Dispose();
    }

    /// <summary>Draws the placeholder icon at runtime so the repo needs no .ico binary.</summary>
    private static Icon BuildIcon()
    {
        using var bitmap = new Bitmap(32, 32, PixelFormat.Format32bppArgb);

        using (var graphics = Graphics.FromImage(bitmap))
        {
            graphics.SmoothingMode = SmoothingMode.AntiAlias;
            graphics.Clear(Color.Transparent);

            using var board = new SolidBrush(Color.FromArgb(255, 76, 132, 255));
            graphics.FillRectangle(board, 6, 5, 20, 24);

            using var clip = new SolidBrush(Color.FromArgb(255, 250, 205, 60));
            graphics.FillRectangle(clip, 11, 1, 10, 8);

            using var rule = new Pen(Color.FromArgb(190, 255, 255, 255), 2f);
            graphics.DrawLine(rule, 10, 15, 22, 15);
            graphics.DrawLine(rule, 10, 20, 22, 20);
        }

        var handle = bitmap.GetHicon();
        try
        {
            // Clone() so the returned Icon owns its handle and can be disposed
            // independently of the borrowed one.
            using var borrowed = Icon.FromHandle(handle);
            return (Icon)borrowed.Clone();
        }
        finally
        {
            Native.DestroyIcon(handle);
        }
    }
}
