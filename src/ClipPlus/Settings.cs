using System;
using System.Diagnostics;
using System.IO;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace ClipPlus;

/// <summary>
/// Persisted configuration. Lives at %LOCALAPPDATA%\ClipPlus\settings.json.
/// Edit it and restart the app; there is deliberately no settings UI.
/// </summary>
internal sealed class Settings
{
    private static readonly JsonSerializerOptions JsonOpts = new()
    {
        WriteIndented = true,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    // ---------------------------------------------------------------- persisted

    /// <summary>Global hotkey: Ctrl/Alt/Shift/Win plus one key (A-Z, 0-9, F1-F24).</summary>
    public string Hotkey { get; set; } = "Win+Alt+V";

    /// <summary>Payloads bigger than this are dropped rather than written to the sync folder.</summary>
    public int MaxBlobBytes { get; set; } = 10 * 1024 * 1024;

    /// <summary>Text longer than this goes to a .bin blob with only a preview kept inline.</summary>
    public int InlineTextLimit { get; set; } = 8192;

    /// <summary>Safety-net re-scan period for missed filesystem watcher events.</summary>
    public int RescanSeconds { get; set; } = 60;

    /// <summary>
    /// Delete this machine's own clips older than N days. 0 disables it: a
    /// background process that deletes user data stays opt-in. Pinned clips are
    /// never touched regardless of age.
    /// </summary>
    public int RetentionDays { get; set; }

    public bool CaptureText { get; set; } = true;
    public bool CaptureImages { get; set; } = true;
    public bool CaptureFiles { get; set; } = true;

    /// <summary>Absolute history folder. Leave null to auto-detect OneDrive.</summary>
    public string? SyncRootOverride { get; set; }

    // ----------------------------------------------------------------- resolved

    /// <summary>Root of the shared history folder. Not persisted; derived on load.</summary>
    [JsonIgnore]
    public string SyncRoot { get; private set; } = "";

    /// <summary>This machine's private sub-tree. Only this process ever writes here.</summary>
    [JsonIgnore]
    public string HistoryRoot => Path.Combine(SyncRoot, MachineId);

    public static string AppDataDir { get; } = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
        "ClipPlus");

    public static string SettingsPath => Path.Combine(AppDataDir, "settings.json");

    private static string? _machineId;

    /// <summary>
    /// Stable 8-hex id, generated once. Sharding writes by machine is what makes
    /// concurrent sync conflict-free: nobody ever writes another machine's files.
    /// </summary>
    public static string MachineId => _machineId ??= LoadMachineId();

    public static Settings Load()
    {
        Directory.CreateDirectory(AppDataDir);

        Settings settings;
        try
        {
            settings = File.Exists(SettingsPath)
                ? JsonSerializer.Deserialize<Settings>(File.ReadAllText(SettingsPath), JsonOpts) ?? new Settings()
                : new Settings();
        }
        catch (Exception ex)
        {
            Log.Error("settings.json unreadable, using defaults", ex);
            settings = new Settings();
        }

        settings.SyncRoot = ResolveSyncRoot(settings.SyncRootOverride);
        try
        {
            // Always rewrite, so a first run leaves an editable file behind.
            settings.Save();
        }
        catch (Exception ex)
        {
            Log.Error("settings.json write failed", ex);
        }

        return settings;
    }

    public void Save()
    {
        Directory.CreateDirectory(AppDataDir);
        File.WriteAllText(SettingsPath, JsonSerializer.Serialize(this, JsonOpts));
    }

    public (uint Mods, uint Vk)? ParseHotkey() => ParseHotkeyText(Hotkey);

    /// <summary>Returns null when the hotkey string is unusable.</summary>
    public static (uint Mods, uint Vk)? ParseHotkeyText(string? text)
    {
        uint mods = 0;
        uint vk = 0;
        var parts = (text ?? "").Split('+', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        if (parts.Length == 0)
        {
            return null;
        }

        foreach (var part in parts)
        {
            switch (part.ToLowerInvariant())
            {
                case "win":
                case "windows":
                case "super":
                    mods |= Native.MOD_WIN;
                    break;
                case "ctrl":
                case "control":
                    mods |= Native.MOD_CONTROL;
                    break;
                case "alt":
                    mods |= Native.MOD_ALT;
                    break;
                case "shift":
                    mods |= Native.MOD_SHIFT;
                    break;
                default:
                    if (vk != 0)
                    {
                        return null; // two non-modifier keys
                    }

                    vk = VirtualKeyFrom(part);
                    if (vk == 0)
                    {
                        return null;
                    }

                    break;
            }
        }

        return vk == 0 ? null : (mods, vk);
    }

    private static uint VirtualKeyFrom(string name)
    {
        var s = name.Trim().ToUpperInvariant();
        if (s.Length == 1)
        {
            var c = s[0];
            return c is (>= 'A' and <= 'Z') or (>= '0' and <= '9') ? c : 0u;
        }

        if (s[0] == 'F' && int.TryParse(s.AsSpan(1), out var n) && n is >= 1 and <= 24)
        {
            return (uint)(0x70 + n - 1); // VK_F1 .. VK_F24
        }

        return 0;
    }

    private static string ResolveSyncRoot(string? over)
    {
        if (!string.IsNullOrWhiteSpace(over))
        {
            return Path.GetFullPath(over);
        }

        foreach (var variable in new[] { "OneDriveCommercial", "OneDriveConsumer", "OneDrive" })
        {
            var dir = Environment.GetEnvironmentVariable(variable);
            if (!string.IsNullOrWhiteSpace(dir))
            {
                return Path.Combine(dir, "ClipPlus");
            }
        }

        // No known sync provider. Everything still works, just single-machine.
        return Path.Combine(AppDataDir, "sync");
    }

    private static string LoadMachineId()
    {
        var path = Path.Combine(AppDataDir, "machine.id");
        try
        {
            Directory.CreateDirectory(AppDataDir);
            if (File.Exists(path))
            {
                var existing = File.ReadAllText(path).Trim();
                if (existing.Length > 0)
                {
                    return existing;
                }
            }

            var id = Guid.NewGuid().ToString("N")[..8];
            File.WriteAllText(path, id);
            return id;
        }
        catch (Exception ex)
        {
            Log.Error("machine.id unavailable", ex);
            var fallback = new string(Array.FindAll(Environment.MachineName.ToCharArray(), char.IsLetterOrDigit));
            return fallback.Length == 0 ? "node" : fallback[..Math.Min(12, fallback.Length)];
        }
    }
}

/// <summary>Registry Run-key integration. HKCU only, no elevation needed.</summary>
internal static class AutoStart
{
    private const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    private const string ValueName = "ClipPlus";

    public static bool IsEnabled()
    {
        try
        {
            using var key = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(RunKey, false);
            return key?.GetValue(ValueName) is not null;
        }
        catch (Exception ex)
        {
            Log.Error("autostart read failed", ex);
            return false;
        }
    }

    public static void Set(bool enabled)
    {
        try
        {
            using var key = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(RunKey, true);
            if (key is null)
            {
                Log.Error("HKCU Run key not found");
                return;
            }

            if (enabled)
            {
                var exe = Environment.ProcessPath;
                if (string.IsNullOrEmpty(exe))
                {
                    Log.Error("ProcessPath unavailable, cannot register autostart");
                    return;
                }

                key.SetValue(ValueName, "\"" + exe + "\"");
            }
            else
            {
                key.DeleteValue(ValueName, false);
            }
        }
        catch (Exception ex)
        {
            Log.Error("autostart write failed", ex);
        }
    }
}
