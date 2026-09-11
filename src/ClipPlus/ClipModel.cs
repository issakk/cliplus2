using System;

namespace ClipPlus;

public enum ClipKind
{
    Text = 0,
    Image = 1,
    Files = 2,
}

/// <summary>
/// A clipboard payload. The same shape is used for capturing the live clipboard
/// and for writing an entry back into it.
/// </summary>
internal sealed class ClipPayload
{
    public ClipPayload(ClipKind kind, string? text, byte[]? blob)
    {
        Kind = kind;
        Text = text;
        Blob = blob;
    }

    public ClipKind Kind { get; }

    /// <summary>Text content, or the newline-joined path list when <see cref="Kind"/> is Files.</summary>
    public string? Text { get; }

    /// <summary>PNG bytes when <see cref="Kind"/> is Image.</summary>
    public byte[]? Blob { get; }
}

/// <summary>
/// On-disk schema v1. <c>System.Text.Json</c> serialises the property names as
/// written, so these lowercase names ARE the file format. Do not rename them
/// without a migration, other machines read these files.
/// </summary>
internal sealed class ClipRecord
{
    public int v { get; set; } = 1;
    public string id { get; set; } = "";
    public long at { get; set; }
    public string machine { get; set; } = "";
    public string kind { get; set; } = "text";
    public string hash { get; set; } = "";

    /// <summary>Full text, or a truncated prefix when <see cref="blob"/> is set.</summary>
    public string? text { get; set; }

    public bool truncated { get; set; }

    /// <summary>Payload size in bytes, for display only.</summary>
    public long length { get; set; }

    /// <summary>Sibling file name holding the heavy payload (image bytes, long text).</summary>
    public string? blob { get; set; }
}

/// <summary>
/// One indexed entry. Public (with public properties) because WPF data binding
/// reflects over it.
/// </summary>
public sealed class ClipItem
{
    public ClipItem(string stem, long at, string machine, ClipKind kind, string hash, string jsonPath)
    {
        Stem = stem;
        At = at;
        Machine = machine;
        Kind = kind;
        Hash = hash;
        JsonPath = jsonPath;
    }

    public string Stem { get; }

    /// <summary>Capture time, unix milliseconds.</summary>
    public long At { get; }

    public string Machine { get; }

    public ClipKind Kind { get; }

    public string Hash { get; }

    /// <summary>Absolute path of the <c>.clip.json</c> file this came from.</summary>
    public string JsonPath { get; }

    /// <summary>Full text, or the retained prefix when <see cref="HasBlob"/> is true.</summary>
    public string Text { get; set; } = "";

    public bool HasBlob { get; set; }

    public string BlobPath { get; set; } = "";

    /// <summary>Ready-to-render one-liner, computed once at index time.</summary>
    public string Preview { get; set; } = "";

    /// <summary>Secondary line: kind, time, origin.</summary>
    public string Meta { get; set; } = "";

    /// <summary>Path of the empty marker file whose existence means "pinned".</summary>
    public string PinPath { get; set; } = "";

    /// <summary>
    /// True when the sibling .pin marker exists. Pin state lives in its own
    /// empty file rather than a field on the clip, so the clip itself stays
    /// immutable — pinning never rewrites a file that is already synced.
    /// </summary>
    public bool IsPinned { get; set; }

    /// <summary>Rendered as a star in the list. Refreshed when the list reloads.</summary>
    public string PinMark => IsPinned ? "★" : "";
}
