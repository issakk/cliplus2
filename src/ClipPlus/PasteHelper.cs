using System;
using System.Collections.Generic;
using System.Collections.Specialized;
using System.IO;
using System.Threading;
using System.Windows;
using System.Windows.Media.Imaging;

namespace ClipPlus;

/// <summary>
/// Clipboard reads and writes. WPF's Clipboard requires STA, so everything here
/// must run on the UI thread.
/// </summary>
internal static class PasteHelper
{
    private const int Retries = 5;
    private const int RetryDelayMs = 25;

    /// <summary>
    /// Reads the current clipboard. Retries because any application holding the
    /// clipboard open makes OpenClipboard fail transiently — a single failed
    /// attempt must never silently lose a copy.
    /// </summary>
    public static ClipPayload? ReadClipboard(Settings settings)
    {
        for (var attempt = 0; attempt <= Retries; attempt++)
        {
            try
            {
                return ReadCore(settings);
            }
            catch (Exception) when (attempt < Retries)
            {
                Thread.Sleep(RetryDelayMs);
            }
            catch (Exception ex)
            {
                Log.Error("clipboard read failed", ex);
                return null;
            }
        }

        return null;
    }

    public static bool WriteClipboard(ClipPayload payload)
    {
        try
        {
            switch (payload.Kind)
            {
                case ClipKind.Text:
                    if (string.IsNullOrEmpty(payload.Text))
                    {
                        return false;
                    }

                    Clipboard.SetText(payload.Text);
                    return true;

                case ClipKind.Files:
                    var collection = new StringCollection();
                    foreach (var path in (payload.Text ?? "").Split('\n', StringSplitOptions.RemoveEmptyEntries))
                    {
                        collection.Add(path);
                    }

                    if (collection.Count == 0)
                    {
                        return false;
                    }

                    Clipboard.SetFileDropList(collection);
                    return true;

                case ClipKind.Image:
                    if (payload.Blob is null)
                    {
                        return false;
                    }

                    // OnLoad matters: it detaches the frame from the stream so the
                    // image survives after this using-block closes.
                    using (var stream = new MemoryStream(payload.Blob))
                    {
                        var frame = BitmapFrame.Create(
                            stream,
                            BitmapCreateOptions.PreservePixelFormat,
                            BitmapCacheOption.OnLoad);
                        Clipboard.SetImage(frame);
                    }

                    return true;

                default:
                    return false;
            }
        }
        catch (Exception ex)
        {
            Log.Error("clipboard write failed", ex);
            return false;
        }
    }

    private static ClipPayload? ReadCore(Settings settings)
    {
        // Files before text: Explorer puts both on the clipboard and the file
        // drop is the more useful of the two.
        if (settings.CaptureFiles && Clipboard.ContainsFileDropList())
        {
            var paths = new List<string>();
            foreach (var path in Clipboard.GetFileDropList())
            {
                if (!string.IsNullOrEmpty(path))
                {
                    paths.Add(path);
                }
            }

            if (paths.Count > 0)
            {
                return new ClipPayload(ClipKind.Files, string.Join('\n', paths), null);
            }
        }

        // Text before image: Excel and friends offer both, and the text form is
        // the one users expect to paste back.
        if (settings.CaptureText && Clipboard.ContainsText())
        {
            var text = Clipboard.GetText();
            if (!string.IsNullOrEmpty(text))
            {
                return new ClipPayload(ClipKind.Text, text, null);
            }
        }

        if (settings.CaptureImages && Clipboard.ContainsImage())
        {
            var source = Clipboard.GetImage();
            if (source is not null)
            {
                var encoder = new PngBitmapEncoder();
                encoder.Frames.Add(BitmapFrame.Create(source));

                using var buffer = new MemoryStream();
                encoder.Save(buffer);
                return new ClipPayload(ClipKind.Image, null, buffer.ToArray());
            }
        }

        return null;
    }
}
