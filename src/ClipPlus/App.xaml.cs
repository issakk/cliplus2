using System;
using System.Diagnostics;
using System.Threading;
using System.Threading.Tasks;
using System.Windows;

namespace ClipPlus;

/// <summary>
/// Wires the four pieces together: message window (capture + hotkey),
/// store (files + index), popup (search) and tray (lifecycle).
/// </summary>
public partial class App : Application
{
    private Mutex? _mutex;
    private Settings _settings = null!;
    private ClipStore _store = null!;
    private MsgWindow _msg = null!;
    private TrayIcon _tray = null!;
    private PopupWindow _popup = null!;

    protected override void OnStartup(StartupEventArgs e)
    {
        base.OnStartup(e);

        // Two instances would write into the same synced folder and index the
        // same files twice. Bail out early rather than corrupt anything.
        _mutex = new Mutex(true, @"Local\ClipPlus.SingleInstance", out var isFirstInstance);
        if (!isFirstInstance)
        {
            Log.Warn("another ClipPlus instance is already running, exiting");
            Shutdown();
            return;
        }

        DispatcherUnhandledException += (_, args) =>
        {
            Log.Error("unhandled dispatcher exception", args.Exception);
            args.Handled = true;
        };

        Log.Info("=== ClipPlus starting ===");

        _settings = Settings.Load();
        Log.Info($"machine={Settings.MachineId} syncRoot={_settings.SyncRoot} hotkey={_settings.Hotkey}");

        _store = new ClipStore(_settings);
        _store.Start();

        _popup = new PopupWindow(_store);
        _popup.Commit += OnCommit;

        _msg = new MsgWindow();
        _msg.ClipboardUpdated += OnClipboardUpdated;
        _msg.HotkeyPressed += () => _popup.Toggle();

        var hotkeyOk = _settings.ParseHotkey() is { } hotkey
                       && _msg.RegisterHotkey(hotkey.Mods, hotkey.Vk);

        _tray = new TrayIcon(ShowPopup, OpenSyncFolder, _store.Rescan, ExitApp);

        _tray.Notify(
            "ClipPlus",
            hotkeyOk
                ? $"已启动，按 {_settings.Hotkey} 打开历史"
                : $"热键 {_settings.Hotkey} 注册失败，请修改 {Settings.SettingsPath} 后重启",
            isError: !hotkeyOk);
    }

    protected override void OnExit(ExitEventArgs e)
    {
        Log.Info("=== ClipPlus stopping ===");

        try
        {
            _store?.Dispose();
        }
        catch (Exception ex)
        {
            Log.Error("store dispose failed", ex);
        }

        try
        {
            _msg?.Dispose();
        }
        catch (Exception ex)
        {
            Log.Error("message window dispose failed", ex);
        }

        try
        {
            _tray?.Dispose();
        }
        catch (Exception ex)
        {
            Log.Error("tray dispose failed", ex);
        }

        try
        {
            _mutex?.ReleaseMutex();
        }
        catch (Exception)
        {
            // Not the owning thread, or never acquired. Nothing to do.
        }

        _mutex?.Dispose();

        base.OnExit(e);
    }

    private void OnClipboardUpdated()
    {
        try
        {
            var payload = PasteHelper.ReadClipboard(_settings);
            if (payload is not null)
            {
                // Queued only; hashing and disk I/O happen on the store's writer thread.
                _store.Capture(payload);
            }
        }
        catch (Exception ex)
        {
            Log.Error("capture failed", ex);
        }
    }

    private void ShowPopup() => _popup.ShowPopup();

    private async void OnCommit(ClipItem item)
    {
        try
        {
            var target = _popup.TargetWindow;
            var payload = _store.ReadPayload(item);
            _popup.Hide();

            if (payload is null || !PasteHelper.WriteClipboard(payload))
            {
                Log.Warn("nothing pasteable for " + item.Stem);
                return;
            }

            if (target != IntPtr.Zero)
            {
                // We are still the foreground process at this point, so Windows
                // honours the request. Do it after hiding, never before.
                Native.SetForegroundWindow(target);
            }

            // The window manager needs a beat to actually move focus back before
            // the keystroke is injected, otherwise Ctrl+V lands in the wrong place.
            await Task.Delay(120);

            Native.SendCtrlV();
        }
        catch (Exception ex)
        {
            Log.Error("paste-back failed", ex);
        }
    }

    private void OpenSyncFolder()
    {
        try
        {
            Process.Start(new ProcessStartInfo(_settings.SyncRoot) { UseShellExecute = true });
        }
        catch (Exception ex)
        {
            Log.Error("cannot open sync folder: " + _settings.SyncRoot, ex);
        }
    }

    private void ExitApp() => Shutdown();
}
