using System;
using System.Buffers.Binary;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading;
using System.Threading.Channels;
using System.Threading.Tasks;

namespace ClipPlus;

/// <summary>
/// The whole storage layer: writes immutable per-clip files into the synced
/// folder and keeps an in-memory index of everything found there.
///
/// Why one file per clip instead of a database: OneDrive-class sync clients do
/// per-file last-writer-wins, so a database file corrupts the moment two
/// machines touch it. Sharding by machine id means only the originating machine
/// ever writes a given file, which makes conflicts impossible rather than
/// unlikely. "Syncing" then reduces to scanning a folder and unioning by id.
///
/// There is deliberately no deletion propagation in v1 — a clip removed on
/// machine A lingers on machine B until B restarts. See README.
/// </summary>
internal sealed class ClipStore : IDisposable
{
    private const string JsonSuffix = ".clip.json";
    private const string BinSuffix = ".bin";

    private const int PreviewChars = 160;
    private const int RetainedChars = 512;

    private static readonly JsonSerializerOptions JsonOpts = new()
    {
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
        WriteIndented = false,
    };

    private readonly Settings _settings;

    private readonly object _gate = new();
    private readonly List<ClipItem> _items = new();
    private readonly Dictionary<string, ClipItem> _byStem = new(StringComparer.OrdinalIgnoreCase);
    private readonly Dictionary<string, ClipItem> _byHash = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>Captures queue here so hashing and disk I/O never touch the UI thread.</summary>
    private readonly Channel<ClipPayload> _writes = Channel.CreateUnbounded<ClipPayload>(
        new UnboundedChannelOptions { SingleReader = true, SingleWriter = false });

    private readonly ConcurrentQueue<string> _pendingPaths = new();
    private readonly ConcurrentDictionary<string, byte> _pendingSeen = new(StringComparer.OrdinalIgnoreCase);
    private readonly CancellationTokenSource _cts = new();

    private FileSystemWatcher? _watcher;
    private Task? _writeLoop;
    private Task? _ingestLoop;
    private Task? _rescanLoop;

    public ClipStore(Settings settings) => _settings = settings;

    public int Count
    {
        get
        {
            lock (_gate)
            {
                return _items.Count;
            }
        }
    }

    public void Start()
    {
        try
        {
            Directory.CreateDirectory(_settings.HistoryRoot);
        }
        catch (Exception ex)
        {
            Log.Error("history folder not creatable: " + _settings.HistoryRoot, ex);
        }

        try
        {
            Directory.CreateDirectory(_settings.SyncRoot);
        }
        catch (Exception ex)
        {
            Log.Error("sync folder not creatable: " + _settings.SyncRoot, ex);
        }

        _writeLoop = Task.Run(WriteLoopAsync);
        _ingestLoop = Task.Run(IngestLoopAsync);
        _rescanLoop = Task.Run(RescanLoopAsync);

        try
        {
            // OneDrive keeps a real local folder, so a plain watcher works. Events
            // can still be missed (or arrive mid-write), hence the rescan safety net.
            _watcher = new FileSystemWatcher(_settings.SyncRoot)
            {
                IncludeSubdirectories = true,
                Filter = "*" + JsonSuffix,
                NotifyFilter = NotifyFilters.FileName | NotifyFilters.LastWrite | NotifyFilters.Size,
            };
            _watcher.Created += (_, e) => Enqueue(e.FullPath);
            _watcher.Changed += (_, e) => Enqueue(e.FullPath);
            _watcher.Renamed += (_, e) => Enqueue(e.FullPath);
            _watcher.Error += (_, e) => Log.Warn("watcher error: " + e.GetException().Message);
            _watcher.EnableRaisingEvents = true;
        }
        catch (Exception ex)
        {
            Log.Error("filesystem watcher unavailable, falling back to periodic rescan", ex);
        }

        // Off the UI thread: the first scan walks the whole history folder.
        _ = Task.Run(Rescan);
    }

    /// <summary>Index anything not seen yet. Never removes entries; missing files are pruned by a restart.</summary>
    public void Rescan()
    {
        try
        {
            if (!Directory.Exists(_settings.SyncRoot))
            {
                return;
            }

            // Enumerate everything and filter in code: Directory.EnumerateFiles
            // pattern matching goes through 8.3 short names and can false-positive.
            foreach (var path in Directory.EnumerateFiles(_settings.SyncRoot, "*", SearchOption.AllDirectories))
            {
                Enqueue(path);
            }
        }
        catch (Exception ex)
        {
            Log.Error("rescan failed", ex);
        }
    }

    public void Capture(ClipPayload payload) => _writes.Writer.TryWrite(payload);

    /// <summary>Snapshot for the popup. Ordered newest first.</summary>
    public List<ClipItem> Query(string? filter, int limit)
    {
        var needle = filter?.Trim();
        var searching = !string.IsNullOrEmpty(needle);

        lock (_gate)
        {
            var result = new List<ClipItem>(Math.Min(limit, _items.Count));
            foreach (var item in _items)
            {
                if (searching && !Matches(item, needle!))
                {
                    continue;
                }

                result.Add(item);
                if (result.Count >= limit)
                {
                    break;
                }
            }

            return result;
        }
    }

    /// <summary>Hydrates an entry back into a clipboard payload. Touches disk when it has a blob.</summary>
    public ClipPayload? ReadPayload(ClipItem item)
    {
        try
        {
            switch (item.Kind)
            {
                case ClipKind.Image:
                    return item.HasBlob && File.Exists(item.BlobPath)
                        ? new ClipPayload(ClipKind.Image, null, File.ReadAllBytes(item.BlobPath))
                        : null;

                case ClipKind.Text:
                    return item.HasBlob && File.Exists(item.BlobPath)
                        ? new ClipPayload(ClipKind.Text, File.ReadAllText(item.BlobPath, Encoding.UTF8), null)
                        : new ClipPayload(ClipKind.Text, item.Text, null);

                default:
                    return new ClipPayload(ClipKind.Files, item.Text, null);
            }
        }
        catch (Exception ex)
        {
            Log.Error("payload read failed: " + item.JsonPath, ex);
            return null;
        }
    }

    public void Dispose()
    {
        // Drain queued captures first: the write loop exits once the channel completes.
        _writes.Writer.TryComplete();
        try
        {
            _writeLoop?.Wait(2000);
        }
        catch
        {
            // Shutdown races are expected.
        }

        // Cancelled only now, so the polls above cannot hold up exit for a full period.
        _cts.Cancel();

        try
        {
            _watcher?.Dispose();
        }
        catch
        {
            // nothing useful to do while exiting
        }

        foreach (var loop in new[] { _ingestLoop, _rescanLoop })
        {
            try
            {
                loop?.Wait(300);
            }
            catch
            {
                // cancelled on purpose
            }
        }

        // _cts is deliberately not disposed: the loops may still be unwinding and the
        // process is about to exit anyway.
    }

    // ------------------------------------------------------------------ internals

    private void Enqueue(string path)
    {
        if (!path.EndsWith(JsonSuffix, StringComparison.OrdinalIgnoreCase))
        {
            return;
        }

        if (_pendingSeen.TryAdd(path, 0))
        {
            _pendingPaths.Enqueue(path);
        }
    }

    private async Task WriteLoopAsync()
    {
        try
        {
            await foreach (var payload in _writes.Reader.ReadAllAsync(_cts.Token).ConfigureAwait(false))
            {
                try
                {
                    Persist(payload);
                }
                catch (Exception ex)
                {
                    Log.Error("clip persist failed", ex);
                }
            }
        }
        catch (OperationCanceledException)
        {
            // shutting down
        }
    }

    private async Task IngestLoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            try
            {
                await Task.Delay(400, _cts.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                return;
            }

            // Cleared before draining so anything enqueued during the drain is retried.
            _pendingSeen.Clear();

            while (_pendingPaths.TryDequeue(out var path))
            {
                try
                {
                    TryIndex(path);
                }
                catch (Exception ex)
                {
                    Log.Warn($"index skipped {Path.GetFileName(path)}: {ex.Message}");
                }
            }
        }
    }

    private async Task RescanLoopAsync()
    {
        var period = TimeSpan.FromSeconds(Math.Max(10, _settings.RescanSeconds));
        while (!_cts.IsCancellationRequested)
        {
            try
            {
                await Task.Delay(period, _cts.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                return;
            }

            Rescan();
        }
    }

    private void TryIndex(string path)
    {
        var stem = StemOf(path);
        if (stem.Length == 0)
        {
            return;
        }

        lock (_gate)
        {
            if (_byStem.ContainsKey(stem))
            {
                return;
            }
        }

        ClipRecord? record;
        try
        {
            record = JsonSerializer.Deserialize<ClipRecord>(File.ReadAllText(path), JsonOpts);
        }
        catch (Exception)
        {
            // Mid-write by the sync client. The periodic rescan will pick it up.
            return;
        }

        if (record is null || string.IsNullOrEmpty(record.id))
        {
            return;
        }

        var kind = ParseKind(record.kind);
        var hasBlob = !string.IsNullOrEmpty(record.blob);
        var item = new ClipItem(record.id, record.at, record.machine, kind, record.hash ?? "", path)
        {
            Text = record.text ?? "",
            HasBlob = hasBlob,
            BlobPath = hasBlob ? Path.Combine(Path.GetDirectoryName(path) ?? "", record.blob!) : "",
            Preview = BuildPreview(kind, record.text),
            Meta = BuildMeta(kind, record.at, record.machine, hasBlob),
        };

        lock (_gate)
        {
            if (!_byStem.TryAdd(item.Stem, item))
            {
                return;
            }

            InsertSorted(item);
            if (item.Hash.Length > 0)
            {
                _byHash.TryAdd(item.Hash, item);
            }
        }
    }

    private void Persist(ClipPayload payload)
    {
        if (payload.Text is null && payload.Blob is null)
        {
            return;
        }

        var bytes = payload.Blob ?? Encoding.UTF8.GetBytes(payload.Text ?? "");
        if (bytes.Length > _settings.MaxBlobBytes)
        {
            Log.Warn($"dropped {payload.Kind} clip of {bytes.Length} bytes (MaxBlobBytes={_settings.MaxBlobBytes})");
            return;
        }

        var hash = HashOf(payload.Kind, bytes);
        lock (_gate)
        {
            if (_byHash.ContainsKey(hash))
            {
                // Already known: re-copied here, or pasted back out of history.
                // The file is immutable, so there is nothing to write.
                return;
            }
        }

        var now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        var stem = now.ToString(CultureInfo.InvariantCulture) + "-" + Guid.NewGuid().ToString("N");
        var month = DateTimeOffset.FromUnixTimeMilliseconds(now).LocalDateTime
            .ToString("yyyy-MM", CultureInfo.InvariantCulture);
        var dir = Path.Combine(_settings.HistoryRoot, month);
        Directory.CreateDirectory(dir);

        var inline = payload.Text;
        string? blobName = null;

        if (payload.Kind == ClipKind.Image || (payload.Kind == ClipKind.Text && inline is { Length: > 0 } && inline.Length > _settings.InlineTextLimit))
        {
            blobName = stem + BinSuffix;

            // Blob first: a reader that sees a blob with no JSON just ignores it,
            // whereas a JSON pointing at a missing blob would be a broken entry.
            File.WriteAllBytes(Path.Combine(dir, blobName), bytes);

            if (payload.Kind == ClipKind.Text && inline is not null)
            {
                inline = inline.Length <= RetainedChars ? inline : inline[..RetainedChars];
            }
            else
            {
                inline = null;
            }
        }

        var record = new ClipRecord
        {
            id = stem,
            at = now,
            machine = Settings.MachineId,
            kind = KindName(payload.Kind),
            hash = hash,
            text = inline,
            truncated = blobName is not null && payload.Kind == ClipKind.Text,
            length = bytes.Length,
            blob = blobName,
        };

        var jsonPath = Path.Combine(dir, stem + JsonSuffix);
        var tempPath = jsonPath + ".tmp";
        File.WriteAllText(tempPath, JsonSerializer.Serialize(record, JsonOpts));

        // Atomic publish. A half-written .clip.json would be an unparsable entry
        // that the sync client could hand to another machine.
        File.Move(tempPath, jsonPath);

        var item = new ClipItem(stem, now, Settings.MachineId, payload.Kind, hash, jsonPath)
        {
            Text = inline ?? "",
            HasBlob = blobName is not null,
            BlobPath = blobName is null ? "" : Path.Combine(dir, blobName),
            Preview = BuildPreview(payload.Kind, inline),
            Meta = BuildMeta(payload.Kind, now, Settings.MachineId, blobName is not null),
        };

        lock (_gate)
        {
            if (!_byStem.TryAdd(item.Stem, item))
            {
                return;
            }

            _byHash[hash] = item;
            InsertSorted(item);
        }
    }

    private void InsertSorted(ClipItem item) => InsertSortedCore(_items, item);

    /// <summary>Binary search keeps <paramref name="items"/> descending by At without re-sorting on every query.</summary>
    private static void InsertSortedCore(List<ClipItem> items, ClipItem item)
    {
        var lo = 0;
        var hi = items.Count;
        while (lo < hi)
        {
            var mid = lo + ((hi - lo) / 2);
            if (items[mid].At > item.At)
            {
                lo = mid + 1;
            }
            else
            {
                hi = mid;
            }
        }

        items.Insert(lo, item);
    }

    private static bool Matches(ClipItem item, string needle)
        => item.Text.Contains(needle, StringComparison.OrdinalIgnoreCase)
           || item.Meta.Contains(needle, StringComparison.OrdinalIgnoreCase);

    private static string StemOf(string path)
    {
        var name = Path.GetFileName(path);
        return name.EndsWith(JsonSuffix, StringComparison.OrdinalIgnoreCase)
            ? name[..^JsonSuffix.Length]
            : "";
    }

    private static ClipKind ParseKind(string? name) => name switch
    {
        "image" => ClipKind.Image,
        "files" => ClipKind.Files,
        _ => ClipKind.Text,
    };

    private static string KindName(ClipKind kind) => kind switch
    {
        ClipKind.Image => "image",
        ClipKind.Files => "files",
        _ => "text",
    };

    private static string BuildPreview(ClipKind kind, string? text)
    {
        if (kind == ClipKind.Image)
        {
            return "[图片]";
        }

        var raw = text ?? "";
        var breakAt = raw.IndexOfAny(new[] { '\r', '\n' });
        var firstLine = (breakAt >= 0 ? raw[..breakAt] : raw).Trim();

        if (kind == ClipKind.Files)
        {
            var count = raw.Split('\n', StringSplitOptions.RemoveEmptyEntries).Length;
            return $"[文件 x{count}] {firstLine}";
        }

        if (firstLine.Length == 0)
        {
            return breakAt >= 0 ? "(多行文本)" : "(空文本)";
        }

        return firstLine.Length <= PreviewChars ? firstLine : firstLine[..PreviewChars] + "…";
    }

    private static string BuildMeta(ClipKind kind, long at, string? machine, bool hasBlob)
    {
        var label = kind switch
        {
            ClipKind.Image => "图片",
            ClipKind.Files => "文件",
            _ => "文本",
        };

        var time = DateTimeOffset.FromUnixTimeMilliseconds(at).LocalDateTime
            .ToString("MM-dd HH:mm", CultureInfo.InvariantCulture);

        var who = string.IsNullOrEmpty(machine)
            ? "?"
            : machine == Settings.MachineId ? "本机" : machine;

        return hasBlob ? $"{label} · {time} · {who} · 完整内容在 .bin" : $"{label} · {time} · {who}";
    }

    private static string HashOf(ClipKind kind, byte[] payload)
    {
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);

        Span<byte> prefix = stackalloc byte[4];
        BinaryPrimitives.WriteInt32LittleEndian(prefix, (int)kind);

        hash.AppendData(prefix);
        hash.AppendData(payload);

        return Convert.ToHexString(hash.GetHashAndReset());
    }
}
