//! The write side of the store: content hashing, dedupe, and atomic publish.
//!
//! Layout and ordering rules are inherited from the C# build and must not drift,
//! because both versions read the same folder:
//!
//! * One clip is one immutable `.clip.json`, plus an optional `.bin` sibling for
//!   heavy payloads.
//! * The blob is written first. A reader that sees a blob with no JSON simply
//!   ignores it, whereas a JSON pointing at a missing blob is a broken entry.
//! * The JSON is written to `.tmp` and renamed into place, so the file appearing
//!   IS the commit point. A sync client can never hand another machine half a clip.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::clip::{ClipKind, ClipPayload, ClipRecord};
use crate::log;
use crate::settings::{self, Settings};

const JSON_SUFFIX: &str = ".clip.json";
const BIN_SUFFIX: &str = ".bin";

/// How much of an over-long text is kept inline for searching. Matches the C#
/// build's `RetainedChars`.
const RETAINED_CHARS: usize = 512;

static QUEUE: Mutex<Vec<ClipPayload>> = Mutex::new(Vec::new());
static QUEUE_SIGNAL: Condvar = Condvar::new();

/// Hands a captured payload to the writer thread. Called from the window thread,
/// so it does no hashing or disk I/O.
pub fn enqueue(payload: ClipPayload) {
    if payload.is_empty() {
        return;
    }

    {
        let mut queue = QUEUE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.push(payload);
    }

    QUEUE_SIGNAL.notify_one();
}

fn drain(wait: Duration) -> Vec<ClipPayload> {
    let mut queue = QUEUE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    if queue.is_empty() {
        let (guard, _) = QUEUE_SIGNAL
            .wait_timeout(queue, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue = guard;
    }

    std::mem::take(&mut *queue)
}

pub struct Store {
    settings: Settings,
    known: Mutex<HashSet<String>>,
}

impl Store {
    pub fn new(settings: Settings) -> Store {
        // Seeded from disk so re-copying something from a previous session does
        // not write a second file for it.
        let started = Instant::now();
        let mut known = HashSet::new();
        seed_known_hashes(&settings.sync_root, &mut known);
        log::info(&format!(
            "indexed {} clip hash(es) in {} ms",
            known.len(),
            started.elapsed().as_millis()
        ));

        Store {
            settings,
            known: Mutex::new(known),
        }
    }

    pub fn run_writer(&self) {
        loop {
            for payload in drain(Duration::from_millis(500)) {
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
            let known = self.known.lock().unwrap_or_else(|p| p.into_inner());
            if known.contains(&hash) {
                // Already stored: re-copied here, or pasted back out of history.
                // The file is immutable, so there is nothing to write.
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
            hash: hash.clone(),
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

        self.known
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(hash);

        Ok(())
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

fn seed_known_hashes(root: &Path, into: &mut HashSet<String>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            seed_known_hashes(&path, into);
            continue;
        }

        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(JSON_SUFFIX) {
            continue;
        }

        // A half-written file left by a sync client just gets skipped here; the
        // full scan in a later pass will pick it up once it is complete.
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<ClipRecord>(&text) else {
            continue;
        };

        if !record.hash.is_empty() {
            into.insert(record.hash);
        }
    }
}
