//! Storage: capture queue, disk layout, index, pinning and retention.
//!
//! Layout and ordering rules are inherited from the C# build and must not drift,
//! because both versions read the same folder:
//!
//! * One clip is one immutable `.clip.json`, plus an optional `.bin` sibling for
//!   heavy payloads and an optional empty `.pin` marker.
//! * Writes are sharded by machine id, so only the originating machine ever
//!   writes a given file and no sync client can ever see a conflict.
//! * The blob is written first. A reader that sees a blob with no JSON simply
//!   ignores it, whereas a JSON pointing at a missing blob is a broken entry.
//! * The JSON is written to `.tmp` and renamed into place, so the file appearing
//!   IS the commit point.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::clip::{ClipKind, ClipPayload, ClipRecord};
use crate::index::{self, ClipItem, ClipSummary, Index};
use crate::log;
use crate::settings::{self, Settings};

pub const JSON_SUFFIX: &str = ".clip.json";
pub const PIN_SUFFIX: &str = ".pin";
const BIN_SUFFIX: &str = ".bin";

/// How much of an over-long text is kept inline for searching. Matches the C#
/// build's `RetainedChars`.
const RETAINED_CHARS: usize = 512;

pub struct Store {
    settings: Settings,
    index: Mutex<Index>,
    queue: Mutex<Vec<ClipPayload>>,
    signal: Condvar,
}

impl Store {
    pub fn new(settings: Settings) -> Store {
        let store = Store {
            settings,
            index: Mutex::new(Index::default()),
            queue: Mutex::new(Vec::new()),
            signal: Condvar::new(),
        };

        // Synchronous, deliberately: the hash set it builds is what stops a
        // restart from re-writing clips that are already on disk.
        store.rescan();
        store
    }

    // ------------------------------------------------------------------ capture

    /// Hands a captured payload to the writer thread. Called from the window
    /// thread, so it does no hashing and no disk I/O.
    pub fn enqueue(&self, payload: ClipPayload) {
        if payload.is_empty() {
            return;
        }

        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            queue.push(payload);
        }

        self.signal.notify_one();
    }

    fn drain(&self, wait: Duration) -> Vec<ClipPayload> {
        let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());

        if queue.is_empty() {
            let (guard, _) = self
                .signal
                .wait_timeout(queue, wait)
                .unwrap_or_else(|p| p.into_inner());
            queue = guard;
        }

        std::mem::take(&mut *queue)
    }

    pub fn run_writer(&self) {
        loop {
            for payload in self.drain(Duration::from_millis(500)) {
                if let Err(err) = self.persist(payload) {
                    log::error(&format!("clip persist failed: {err}"));
                }
            }
        }
    }

    fn persist(&self, payload: ClipPayload) -> Result<(), String> {
        let body = payload.body();
        if body.len() as u64 > self.settings.max_blob_bytes {
            log::warn(&format!(
                "dropped {} clip of {} bytes (MaxBlobBytes={})",
                payload.kind().name(),
                body.len(),
                self.settings.max_blob_bytes
            ));
            return Ok(());
        }

        let hash = hash_of(payload.kind(), &body);
        {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            if index.has_hash(&hash) {
                // Already stored: re-copied, pasted back out of history, or
                // written by the other build. The file is immutable.
                return Ok(());
            }
        }

        let now = settings::now_ms();
        let stem = settings::unique_stem(now);
        let directory = self
            .settings
            .history_root()
            .join(settings::month_bucket(now));
        fs::create_dir_all(&directory)
            .map_err(|err| format!("create_dir_all {}: {err}", directory.display()))?;

        let (inline, blob_name) = plan_payload(&payload, &stem, &self.settings);

        if let Some(name) = &blob_name {
            let path = directory.join(name);
            fs::write(&path, &body).map_err(|err| format!("write {}: {err}", path.display()))?;
        }

        let record = ClipRecord {
            v: 1,
            id: stem.clone(),
            at: now,
            machine: self.settings.machine_id.clone(),
            kind: payload.kind().name().to_string(),
            hash,
            text: inline,
            truncated: blob_name.is_some() && payload.kind() == ClipKind::Text,
            length: body.len() as i64,
            blob: blob_name,
        };

        let json_path = directory.join(format!("{stem}{JSON_SUFFIX}"));
        let temp_path = directory.join(format!("{stem}{JSON_SUFFIX}.tmp"));
        let json = serde_json::to_string(&record).map_err(|err| err.to_string())?;

        fs::write(&temp_path, json)
            .map_err(|err| format!("write {}: {err}", temp_path.display()))?;

        // The rename is the publish step: after it, the clip exists.
        fs::rename(&temp_path, &json_path)
            .map_err(|err| format!("publish {}: {err}", json_path.display()))?;

        let item = ClipItem::from_record(
            &record,
            json_path,
            stem,
            false,
            &self.settings.machine_id,
        );

        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(item);

        Ok(())
    }

    // ------------------------------------------------------------------- reading

    /// Pinned rows first, then newest first. `filter` is matched with an
    /// allocation-free case-insensitive substring search.
    pub fn query(&self, filter: &str, limit: usize) -> Vec<ClipSummary> {
        let needle = filter.trim().to_ascii_lowercase();
        let index = self.index.lock().unwrap_or_else(|p| p.into_inner());

        if needle.is_empty() {
            index.query(None, limit)
        } else {
            index.query(Some(&needle), limit)
        }
    }

    pub fn count(&self) -> usize {
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Hydrates a clip for paste-back. Touches disk only when it has a blob.
    pub fn read_payload(&self, stem: &str) -> Option<ClipPayload> {
        let (kind, text, has_blob, blob_path) = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            let item = index.find(stem)?;
            (
                item.kind,
                item.text.clone(),
                item.has_blob,
                item.blob_path.clone(),
            )
        };

        match kind {
            ClipKind::Image => {
                if !has_blob {
                    return None;
                }
                fs::read(&blob_path).ok().map(ClipPayload::Image)
            }

            ClipKind::Text => {
                if has_blob {
                    fs::read_to_string(&blob_path).ok().map(ClipPayload::Text)
                } else {
                    Some(ClipPayload::Text(text))
                }
            }

            ClipKind::Files => Some(ClipPayload::Files(
                text.lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| line.to_string())
                    .collect(),
            )),
        }
    }

    // ---------------------------------------------------------------- indexing

    /// Full folder walk. Add-only: entries whose files disappeared are pruned by
    /// the watcher, or implicitly by the next restart rebuilding the index.
    pub fn rescan(&self) {
        let root = self.settings.sync_root.clone();
        let mut pinned = HashSet::new();
        self.walk(&root, &mut pinned);

        let (clips, pinned_count) = {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.apply_pin_state(&pinned);
            (index.len(), pinned.len())
        };

        log::info(&format!(
            "rescan: {clips} clip(s) indexed, {pinned_count} pinned"
        ));
    }

    fn walk(&self, directory: &Path, pinned: &mut HashSet<String>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                self.walk(&path, pinned);
                continue;
            }

            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };

            if name.ends_with(PIN_SUFFIX) {
                let stem = stem_of(&path, PIN_SUFFIX);
                if !stem.is_empty() {
                    pinned.insert(stem);
                }
            } else if name.ends_with(JSON_SUFFIX) {
                self.ingest_file(&path);
            }
        }
    }

    fn ingest_file(&self, path: &Path) {
        let file_stem = stem_of(path, JSON_SUFFIX);
        if file_stem.is_empty() {
            return;
        }

        {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            if index.has_stem(&file_stem) {
                return;
            }
        }

        // A half-written file from a sync client just gets skipped; the periodic
        // rescan picks it up once it is complete.
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let Ok(record) = serde_json::from_str::<ClipRecord>(&text) else {
            return;
        };

        let stem = if record.id.is_empty() {
            file_stem
        } else {
            record.id.clone()
        };
        let pinned = index::pin_path_for(path).exists();

        let item = ClipItem::from_record(
            &record,
            path.to_path_buf(),
            stem,
            pinned,
            &self.settings.machine_id,
        );

        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(item);
    }

    // --------------------------------------------------------------------- pins

    /// Pins by writing an empty sibling marker, never by touching the clip.
    ///
    /// The marker lives in the synced folder on purpose: pinning is most useful
    /// for exactly the snippets you want on every machine, so it has to travel.
    /// Two machines creating the same marker write identical (empty) content.
    pub fn set_pinned(&self, stem: &str, pinned: bool) -> bool {
        let pin_path = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            match index.find(stem) {
                Some(item) => item.pin_path(),
                None => return false,
            }
        };

        if pin_path.as_os_str().is_empty() {
            return false;
        }

        if pinned {
            if let Err(err) = fs::write(&pin_path, []) {
                log::error(&format!("pin {}: {err}", pin_path.display()));
                return false;
            }
        } else {
            remove_file(&pin_path);
        }

        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .set_pinned(stem, pinned);

        log::info(&format!(
            "{} {stem}",
            if pinned { "pinned" } else { "unpinned" }
        ));
        true
    }

    // ----------------------------------------------------------------- retention

    /// Deletes this machine's own clips older than `RetentionDays`.
    ///
    /// Deliberately confined to this machine's directory: single-writer-per-
    /// directory is what makes the folder conflict free, and it is worth more
    /// than reclaiming a retired machine's disk. Pinned clips are never eligible.
    pub fn prune_old(&self) {
        let days = self.settings.retention_days;
        if days == 0 {
            return;
        }

        let cutoff = settings::now_ms() - (days as i64) * 86_400_000;
        let doomed = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.stale_clips(cutoff, &self.settings.machine_id)
        };

        if doomed.is_empty() {
            return;
        }

        for item in &doomed {
            remove_file(&item.json_path);
            if item.has_blob && !item.blob_path.as_os_str().is_empty() {
                remove_file(&item.blob_path);
            }
            remove_file(&item.pin_path());
        }

        {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            for item in &doomed {
                index.forget(&item.stem);
            }
        }

        log::info(&format!(
            "retention removed {} clip(s) older than {days} day(s)",
            doomed.len()
        ));
    }

    // -------------------------------------------------------------------- watch

    pub fn on_path_changed(&self, path: &Path) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if name.ends_with(JSON_SUFFIX) {
            self.ingest_file(path);
        } else if name.ends_with(PIN_SUFFIX) {
            let stem = stem_of(path, PIN_SUFFIX);
            let exists = path.exists();
            self.index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_pinned(&stem, exists);
        }
    }

    pub fn on_path_removed(&self, path: &Path) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if name.ends_with(JSON_SUFFIX) {
            let stem = stem_of(path, JSON_SUFFIX);
            let removed = {
                let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
                index.forget(&stem)
            };

            if let Some(item) = removed {
                log::info(&format!("clip removed: {}", item.stem));
            }
        } else if name.ends_with(PIN_SUFFIX) {
            let stem = stem_of(path, PIN_SUFFIX);
            self.index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_pinned(&stem, false);
        }
    }

    /// The returned watcher must be kept alive for the process lifetime;
    /// dropping it silently stops all events.
    pub fn start_watcher(self: &Arc<Self>) -> Option<notify::RecommendedWatcher> {
        let store = Arc::clone(self);

        let callback = move |result: Result<notify::Event, notify::Error>| {
            let Ok(event) = result else {
                return;
            };

            match event.kind {
                notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {
                    for path in &event.paths {
                        store.on_path_changed(path);
                    }
                }
                notify::EventKind::Remove(_) => {
                    for path in &event.paths {
                        store.on_path_removed(path);
                    }
                }
                _ => {}
            }
        };

        let mut watcher = match notify::recommended_watcher(callback) {
            Ok(watcher) => watcher,
            Err(err) => {
                log::warn(&format!(
                    "watcher unavailable, relying on the periodic rescan: {err}"
                ));
                return None;
            }
        };

        match watcher.watch(&self.settings.sync_root, notify::RecursiveMode::Recursive) {
            Ok(()) => log::info(&format!("watching {}", self.settings.sync_root.display())),
            Err(err) => log::warn(&format!(
                "watcher cannot watch {}: {err}",
                self.settings.sync_root.display()
            )),
        }

        Some(watcher)
    }

    // -------------------------------------------------------------------- loops

    pub fn run_rescan_loop(&self) {
        let period = Duration::from_secs(self.settings.rescan_seconds.max(10));
        loop {
            thread::sleep(period);
            self.rescan();
        }
    }

    pub fn run_retention_loop(&self) {
        // Let the first scan settle before deleting anything.
        thread::sleep(Duration::from_secs(60));
        loop {
            self.prune_old();
            thread::sleep(Duration::from_secs(6 * 3600));
        }
    }
}

/// Decides what stays inline and whether a `.bin` sibling is needed.
/// Mirrors `ClipStore.Persist` in the C# build exactly.
fn plan_payload(
    payload: &ClipPayload,
    stem: &str,
    settings: &Settings,
) -> (Option<String>, Option<String>) {
    let blob_name = format!("{stem}{BIN_SUFFIX}");

    match payload {
        ClipPayload::Text(text) => {
            if text.chars().count() > settings.inline_text_limit {
                let retained: String = text.chars().take(RETAINED_CHARS).collect();
                (Some(retained), Some(blob_name))
            } else {
                (Some(text.clone()), None)
            }
        }

        // Images always go to a blob, so listing the history never has to touch
        // a multi-megabyte file.
        ClipPayload::Image(_) => (None, Some(blob_name)),

        // A path list is short by construction, so it always stays inline.
        ClipPayload::Files(paths) => (Some(paths.join("\n")), None),
    }
}

/// SHA-256 over a 4-byte kind prefix plus the payload.
///
/// The kind is mixed in so identical bytes stored as text and as an image are
/// different clips. Uppercase hex, matching `Convert.ToHexString` in C#.
fn hash_of(kind: ClipKind, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((kind as i32).to_le_bytes());
    hasher.update(body);
    let digest = hasher.finalize();

    let mut out = String::with_capacity(digest.len() * 2);
    for &byte in digest.as_slice() {
        let _ = write!(out, "{byte:02X}");
    }
    out
}

fn stem_of(path: &Path, suffix: &str) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match name.strip_suffix(suffix) {
        Some(stem) => stem.to_string(),
        None => String::new(),
    }
}

fn remove_file(path: &Path) {
    if path.as_os_str().is_empty() {
        return;
    }

    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log::error(&format!("delete {}: {err}", path.display())),
    }
}

