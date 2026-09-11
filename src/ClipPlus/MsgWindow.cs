using System;
using System.Runtime.InteropServices;
using System.Windows.Interop;

namespace ClipPlus;

/// <summary>
/// A hidden window whose only job is to receive WM_CLIPBOARDUPDATE and WM_HOTKEY.
/// Both are pushed by the OS, so an idle ClipPlus burns no CPU at all — there is
/// no clipboard polling anywhere in this app.
/// </summary>
internal sealed class MsgWindow : IDisposable
{
    private const int HotkeyId = 0xC1A0;
    private const int WS_EX_TOOLWINDOW = 0x00000080;

    private readonly HwndSource _source;
    private uint _lastSequence;
    private bool _hotkeyRegistered;

    public MsgWindow()
    {
        var parameters = new HwndSourceParameters("ClipPlus.MessageWindow")
        {
            // HwndSource never calls ShowWindow, so with no WS_VISIBLE this window
            // stays invisible. WS_EX_TOOLWINDOW keeps it out of taskbar and alt-tab.
            WindowStyle = 0,
            ExtendedWindowStyle = WS_EX_TOOLWINDOW,
            Width = 1,
            Height = 1,
        };

        _source = new HwndSource(parameters);
        _source.AddHook(WndProc);

        if (!Native.AddClipboardFormatListener(_source.Handle))
        {
            Log.Error($"AddClipboardFormatListener failed, win32 error {Marshal.GetLastWin32Error()}");
        }
    }

    public event Action? ClipboardUpdated;

    public event Action? HotkeyPressed;

    public bool RegisterHotkey(uint modifiers, uint key)
    {
        _hotkeyRegistered = Native.RegisterHotKey(
            _source.Handle,
            HotkeyId,
            modifiers | Native.MOD_NOREPEAT,
            key);

        if (!_hotkeyRegistered)
        {
            Log.Error($"RegisterHotKey failed, win32 error {Marshal.GetLastWin32Error()}");
        }

        return _hotkeyRegistered;
    }

    /// <summary>Swaps the hotkey without restarting the app.</summary>
    public bool ReregisterHotkey(uint modifiers, uint key)
    {
        if (_hotkeyRegistered)
        {
            Native.UnregisterHotKey(_source.Handle, HotkeyId);
            _hotkeyRegistered = false;
        }

        return RegisterHotkey(modifiers, key);
    }

    public void Dispose()
    {
        Native.RemoveClipboardFormatListener(_source.Handle);
        if (_hotkeyRegistered)
        {
            Native.UnregisterHotKey(_source.Handle, HotkeyId);
        }

        _source.RemoveHook(WndProc);
        _source.Dispose();
    }

    private IntPtr WndProc(IntPtr hwnd, int msg, IntPtr wParam, IntPtr lParam, ref bool handled)
    {
        if (msg == Native.WM_CLIPBOARDUPDATE)
        {
            // The OS notifies on every format change, and a single Ctrl+C can
            // raise several. The sequence number filters that for free.
            var sequence = Native.GetClipboardSequenceNumber();
            if (sequence != _lastSequence)
            {
                _lastSequence = sequence;
                ClipboardUpdated?.Invoke();
            }
        }
        else if (msg == Native.WM_HOTKEY && wParam.ToInt32() == HotkeyId)
        {
            handled = true;
            HotkeyPressed?.Invoke();
        }

        return IntPtr.Zero;
    }
}
