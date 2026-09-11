using System;
using System.IO;
using System.Text;

namespace ClipPlus;

/// <summary>
/// Minimal append-only file logger. A tray app has no console, so this is the
/// only way to diagnose anything on a user's machine.
/// </summary>
internal static class Log
{
    private const long MaxBytes = 1024 * 1024;

    private static readonly object Gate = new();
    private static readonly string LogFile = Path.Combine(Settings.AppDataDir, "clipplus.log");

    public static string FilePath => LogFile;

    public static void Info(string message) => Write("INFO ", message);

    public static void Warn(string message) => Write("WARN ", message);

    public static void Error(string message, Exception? ex = null)
        => Write("ERROR", ex is null ? message : message + " :: " + ex);

    private static void Write(string level, string message)
    {
        try
        {
            lock (Gate)
            {
                Directory.CreateDirectory(Settings.AppDataDir);
                var info = new FileInfo(LogFile);
                if (info.Exists && info.Length > MaxBytes)
                {
                    info.Delete();
                }

                File.AppendAllText(
                    LogFile,
                    $"{DateTime.Now:yyyy-MM-dd HH:mm:ss.fff} [{level}] {message}{Environment.NewLine}",
                    Encoding.UTF8);
            }
        }
        catch
        {
            // Logging must never take the app down.
        }
    }
}
