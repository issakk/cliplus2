//! Storage: capture queue, disk layout, index, pinning and retention.
//!
//! * One database per machine per month: `<machine>/<yyyy-MM>/clips.db`, written
//!   only by the machine it is named after. A sync client resolves the same file
//!   changed twice as last-writer-wins, so one writer per file makes a conflict
//!   physically impossible — no lock, no protocol, no server.
//! * Only the month in progress is ever written. Once a month rolls over its
//!   database is frozen and already uploaded, so the price of a capture is
//!   bounded by one month of history rather than by all of it.
//! * Heavy payloads stay out of the database, as `<stem>.bin` siblings. A sync
//!   client moves whole files: an image stored in a row would re-upload every
//!   image of the month on every capture.
//! * Pins are empty `<stem>.pin` markers in the folder that owns the clip.
//!   An empty file has identical content on every machine, so two machines
//!   creating the same marker cannot be seen as a conflict.
//! * The `INSERT` is the commit point: a reader sees the previous or the new
//!   state, never half a clip. Blobs are written before the row that points
//!   at them, so a crash leaves an unreferenced blob rather than a broken entry.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

// `watch` lives on the Watcher trait, not on the concrete watcher type.
use notify::Watcher as _;
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::clip::{ClipContext, ClipKind, ClipPayload, ClipRecord};
use crate::index::{ClipItem, ClipSummary, Index};
use crate::log;
use crate::settings::{self, Settings};

pub const PIN_SUFFIX: &str = ".pin";
const BIN_SUFFIX: &str = ".bin";
/// The tombstone another instance leaves when it wants a clip gone but may not
/// write the database that holds it. Empty, like the pin marker, so that the same
/// file created on two machines is the same file.
pub const HIDDEN_SUFFIX: &str = ".del";

/// Matched exactly rather than by extension, so an unrelated `.db` that happens
/// to sit in the synced folder is never opened as a clip store.
const DB_NAME: &str = "clips.db";

const DB_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS clips (
    stem    TEXT PRIMARY KEY,
    at      INTEGER NOT NULL,
    machine TEXT NOT NULL,
    kind    TEXT NOT NULL,
    hash    TEXT NOT NULL,
    text    TEXT,
    length  INTEGER NOT NULL,
    blob    TEXT,
    app     TEXT,
    title   TEXT
)";


/// `OR IGNORE`: the stem is the primary key, so a capture that is already stored
/// is a no-op instead of an error.
const INSERT_ROW: &str = "
INSERT OR IGNORE INTO clips (stem, at, machine, kind, hash, text, length, blob, app, title)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";


const SELECT_ROWS: &str = "
SELECT stem, at, machine, kind, hash, text, length, blob, app, title FROM clips";

/// How much of an over-long text is kept inline for searching. The tail exists
/// only in the `.bin` sibling, which a search never opens.
const RETAINED_CHARS: usize = 512;

/// A read can end up behind this machine's own writer thread. A remote file the
/// sync client is halfway through uploading fails differently — it does not open
/// at all — and is handled by keeping the previous snapshot.
const BUSY_TIMEOUT_MS: u64 = 3_000;

/// One tab of the instance strip: which machine's clips the list shows, and
/// what to call it. `id` is `None` for the tab that shows everything.
#[derive(Clone, PartialEq, Eq)]
pub struct MachineTab {
    pub id: Option<String>,
    pub label: String,
}

/// Which heavy clips the `.bin` cleanup targets. "Heavy" means the payload has a
/// `.bin` sibling — every image and every text over the inline limit — which is
/// where the disk space actually goes. Short text and file lists never have
/// one, so a cleanup can never touch them.
///
/// Both scopes treat `0` as "everything".
#[derive(Clone, Copy, Debug)]
pub enum BinScope {
    /// Heavy clips captured before the last N days.
    OlderThanDays(u32),
    /// The N newest heavy clips stay; everything older goes.
    KeepNewest(u32),
}

/// What a `.bin` scan found. `doomed` holds only rows this machine may delete,
/// newest first; the skipped ones are counted, not listed.
pub struct BinScan {
    pub doomed: Vec<ClipItem>,
    /// In range, but sitting in another instance's live month. A bulk cleanup
    /// does not reach into a folder another machine is writing, so these are
    /// left for their owner.
    pub skipped_live_month: usize,
    pub images: usize,
    pub overlong_texts: usize,
    /// The `.bin` bytes still on disk that deleting `doomed` would free.
    pub blob_bytes: u64,
}

/// What a duplicate scan found. Only the copies that would go are listed; the
/// kept ones are counted.
pub struct DupeScan {
    /// Older copies of texts that exist more than once, newest first.
    pub doomed: Vec<ClipItem>,
    /// Distinct texts that had more than one copy.
    pub groups: usize,
    /// Groups in which a pinned copy exists — those groups keep their pin
    /// beside the newest, so a cleanup can leave two copies of one text.
    pub groups_with_pin: usize,
}

/// A `.bin` scan together with the words its scope was asked under, so the
/// confirm dialog repeats what the user chose even if the radios moved while
/// the background scan ran.
pub struct BinPreview {
    pub scan: BinScan,
    pub scope_line: String,
}

pub struct Store {
    settings: Settings,
    index: Mutex<Index>,
    queue: Mutex<Vec<(ClipPayload, ClipContext)>>,
    signal: Condvar,
    /// `(len, mtime)` of every database already read, so the periodic rescan
    /// re-reads what changed instead of every row of every month every minute.
    loaded: Mutex<HashMap<PathBuf, (u64, i64)>>,
    /// This machine's own folder, lowercased and with a trailing separator.
    own_root: String,
}

impl Store {
    pub fn new(settings: Settings) -> Store {
        let own_root = format!(
            "{}{}",
            settings.history_root().to_string_lossy().to_lowercase(),
            std::path::MAIN_SEPARATOR
        );

        let store = Store {
            settings,
            index: Mutex::new(Index::default()),
            queue: Mutex::new(Vec::new()),
            signal: Condvar::new(),
            loaded: Mutex::new(HashMap::new()),
            own_root,
        };

        // Synchronous, deliberately: the index it builds is what stops a
        // restart from re-writing clips that are already stored.
        store.rescan();
        store
    }

    // ------------------------------------------------------------------ capture

    /// Hands a captured payload to the writer thread. Called from the window
    /// thread, so it does no hashing and no disk I/O. The context is read at
    /// capture time and travels with the payload, because the window it describes
    /// is gone by the time this is written.
    pub fn enqueue(&self, payload: ClipPayload, context: ClipContext) {
        if payload.is_empty() {
            return;
        }

        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            queue.push((payload, context));
        }

        self.signal.notify_one();
    }

    fn drain(&self, wait: Duration) -> Vec<(ClipPayload, ClipContext)> {
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
            for (payload, context) in self.drain(Duration::from_millis(500)) {
                if let Err(err) = self.persist(payload, &context) {
                    log::error(&format!("clip persist failed: {err}"));
                }
            }
        }
    }

    fn persist(&self, payload: ClipPayload, context: &ClipContext) -> Result<(), String> {
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
                // Already stored: re-copied from history, pasted back out, or
                // written by another machine. Rows are never rewritten.
                log::info("clip already stored, nothing written");
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

        // The blob goes first: a reader that sees a blob without a row ignores
        // it, whereas a row pointing at a missing blob is a broken entry.
        if let Some(name) = &blob_name {
            let path = directory.join(name);
            fs::write(&path, &body).map_err(|err| format!("write {}: {err}", path.display()))?;
        }

        let record = ClipRecord {
            id: stem.clone(),
            at: now,
            machine: self.settings.machine_id.clone(),
            kind: payload.kind().name().to_string(),
            hash,
            text: inline,
            length: body.len() as i64,
            blob: blob_name,
            app: context.app.clone(),
            title: context.title.clone(),
        };

        let db_path = directory.join(DB_NAME);
        insert_row(&db_path, &record)?;
        self.remember_db(&db_path);

        let item = ClipItem::from_record(
            &record,
            db_path,
            stem,
            false,
            &self.settings.machine_id,
            crate::settings::current_year(),
        );

        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(item);

        Ok(())
    }

    // ------------------------------------------------------------------- reading

    /// One instance's clips, or every instance's when `machine` is `None`.
    /// `filter` is the search box, lowercased here and split into `field:value` terms in
    /// the index; every term is then matched with an allocation-free case-insensitive
    /// substring search against the one field it names.
    pub fn query(&self, machine: Option<&str>, filter: &str, limit: usize) -> Vec<ClipSummary> {
        let needle = filter.trim().to_ascii_lowercase();
        let index = self.index.lock().unwrap_or_else(|p| p.into_inner());

        if needle.is_empty() {
            index.query(machine, None, limit)
        } else {
            index.query(machine, Some(&needle), limit)
        }
    }

    /// The instance strip: everything, then this machine, then the others in the
    /// order they last captured something.
    pub fn machines(&self) -> Vec<MachineTab> {
        let ids = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.machines()
        };

        let mut tabs = Vec::with_capacity(ids.len() + 1);
        tabs.push(MachineTab {
            id: None,
            label: "全部".to_string(),
        });

        for (id, _) in ids {
            let label = if id == self.settings.machine_id {
                "本机".to_string()
            } else {
                id.clone()
            };

            tabs.push(MachineTab { id: Some(id), label });
        }

        tabs
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

    /// Full folder walk: every database under the sync root, plus the pin and
    /// tombstone markers beside them. Databases are stamped, so one that has not
    /// changed costs a `stat`.
    pub fn rescan(&self) {
        let root = self.settings.sync_root.clone();
        let mut pinned = HashSet::new();
        let mut hidden = HashSet::new();
        self.walk(&root, &mut pinned, &mut hidden);

        let (clips, pinned_count, hidden_count) = {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.apply_pin_state(&pinned);
            index.apply_hidden_state(&hidden);
            (index.len(), pinned.len(), hidden.len())
        };

        log::info(&format!(
            "rescan: {clips} clip(s) indexed, {pinned_count} pinned, {hidden_count} hidden"
        ));
    }

    fn walk(&self, directory: &Path, pinned: &mut HashSet<String>, hidden: &mut HashSet<String>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                self.walk(&path, pinned, hidden);
                continue;
            }

            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };

            if name == DB_NAME {
                self.refresh_db(&path, false);
            } else if name.ends_with(PIN_SUFFIX) {
                let stem = stem_of(&path, PIN_SUFFIX);
                if !stem.is_empty() {
                    pinned.insert(stem);
                }
            } else if name.ends_with(HIDDEN_SUFFIX) {
                let stem = stem_of(&path, HIDDEN_SUFFIX);
                if !stem.is_empty() {
                    hidden.insert(stem);
                }
            }
        }
    }

    /// Reads one database, then swaps its rows into the index.
    ///
    /// Read first, forget second: a database that cannot be read — halfway
    /// uploaded by a sync client, or replaced while we read it — leaves the
    /// previous snapshot in place instead of emptying part of the history.
    fn load_db(&self, path: &Path) -> Result<usize, String> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|err| err.to_string())?;
        let _ = conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));

        let mut statement = conn.prepare(SELECT_ROWS).map_err(|err| err.to_string())?;
        let rows = statement
            .query_map([], Row::read)
            .map_err(|err| err.to_string())?;

        let folder = path.parent().unwrap_or(Path::new(""));
        let mut items = Vec::new();

        // Once for the whole database rather than once per row: every row's
        // timestamp is formatted against it.
        let now_year = crate::settings::current_year();
        for row in rows {
            let row = row.map_err(|err| err.to_string())?;
            let pinned = folder
                .join(format!("{}{PIN_SUFFIX}", row.stem))
                .exists();

            let record = ClipRecord {
                id: row.stem.clone(),
                at: row.at,
                machine: row.machine,
                kind: row.kind,
                hash: row.hash,
                text: row.text,
                length: row.length,
                blob: row.blob,
                app: row.app.unwrap_or_default(),
                title: row.title.unwrap_or_default(),
            };

            items.push(ClipItem::from_record(
                &record,
                path.to_path_buf(),
                row.stem,
                pinned,
                &self.settings.machine_id,
                now_year,
            ));
        }

        drop(statement);

        let count = items.len();
        let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        index.forget_db(path);
        // One merge rather than one insert per row: this runs again for every change
        // to a database, and each insert shifts the whole index.
        index.insert_many(items);

        Ok(count)
    }

    /// Re-reads a database whose file changed.
    ///
    /// `skip_own` is for the watcher: this machine's own database changes on
    /// every capture and its rows are already in the index, so a reload there
    /// would be a full read of the month for nothing.
    fn refresh_db(&self, path: &Path, skip_own: bool) {
        if skip_own && self.is_own_db(path) {
            return;
        }

        let Some(stamp) = stamp_of(path) else {
            return;
        };

        {
            let loaded = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
            if loaded.get(path) == Some(&stamp) {
                return;
            }
        }

        match self.load_db(path) {
            Ok(count) => {
                self.loaded
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(path.to_path_buf(), stamp);

                log::info(&format!("{}: {count} clip(s)", path.display()));
            }
            Err(err) => log::warn(&format!(
                "{} unreadable, keeping the previous snapshot: {err}",
                path.display()
            )),
        }
    }

    /// A string compare rather than `Path::starts_with`, which is case-sensitive
    /// and would miss the folder as OneDrive spells it. Getting this wrong only
    /// costs a redundant reload, never correctness.
    fn is_own_db(&self, path: &Path) -> bool {
        path.to_string_lossy()
            .to_lowercase()
            .starts_with(&self.own_root)
    }

    /// Records a database as seen, so the rescan does not read this machine's
    /// own month again: it changes on every capture and its rows are already in
    /// the index.
    fn remember_db(&self, path: &Path) {
        if let Some(stamp) = stamp_of(path) {
            self.loaded
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(path.to_path_buf(), stamp);
        }
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

        delete_clips(&doomed);
        self.forget_deleted(&doomed);

        log::info(&format!(
            "retention removed {} clip(s) older than {days} day(s)",
            doomed.len()
        ));
    }

    // ------------------------------------------------------------------- deletion

    /// Deletes the clips the user picked in the list, and reports `(deleted, marked)`.
    ///
    /// This machine's own rows are its own to delete, and so is any row in a month
    /// that is over: nothing ever writes a past bucket, so a stray deletion there
    /// costs nothing — two machines deleting from one frozen month can lose a deletion
    /// to the sync client's last-writer-wins, never a clip.
    ///
    /// Another instance's live month is the one thing that cannot be written: it has
    /// a writer, and it is not us. A second writer working from a snapshot that may be
    /// an hour old would not undo a rival deletion, it would drop every clip that
    /// machine captured since that snapshot was taken. So those rows are *marked*
    /// instead — an empty `<stem>.del` beside the clip, which every machine reads as
    /// "this one is gone" and which the machine that owns the row acts on in
    /// `reap_hidden`. Hidden here at once, and really gone there within a minute of
    /// that machine looking.
    pub fn delete_selected(&self, stems: &[String]) -> (usize, usize) {
        let month = settings::month_bucket(settings::now_ms());

        let (doomed, marked) = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            let mut doomed: Vec<ClipItem> = Vec::new();
            let mut marked: Vec<ClipItem> = Vec::new();

            for stem in stems {
                match index.find(stem) {
                    Some(item) if deletable(item, &self.settings.machine_id, &month) => {
                        doomed.push(item.clone());
                    }
                    // Another instance's live month: a tombstone is all we may write.
                    Some(item) => marked.push(item.clone()),
                    // Already gone: the file was reloaded since the popup was drawn.
                    None => {}
                }
            }

            (doomed, marked)
        };

        let (deleted, hidden) = self.remove_clips(doomed, marked);

        log::info(&format!(
            "deleted {} clip(s) by hand, tombstoned {}",
            deleted, hidden
        ));

        (deleted, hidden)
    }

    /// The second half of every deletion, once the rows are resolved: rows this
    /// machine may write go out at once, the rest get a tombstone — which every
    /// machine reads as gone and the owner carries out on its next rescan.
    /// Reports `(deleted, marked)`.
    fn remove_clips(&self, doomed: Vec<ClipItem>, marked: Vec<ClipItem>) -> (usize, usize) {
        if !doomed.is_empty() {
            delete_clips(&doomed);
            self.forget_deleted(&doomed);
        }

        // Only the ones whose marker actually landed: a row left on this list with no
        // tombstone behind it is a delete that did not happen, and staying visible is
        // the honest report of that. `write_marker` logged why it failed.
        let mut hidden: HashSet<String> = HashSet::new();
        for item in &marked {
            if write_marker(item) {
                hidden.insert(item.stem.clone());
            }
        }

        if !hidden.is_empty() {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.hide_many(&hidden);
        }

        (doomed.len(), hidden.len())
    }

    /// Drops a batch from the index after `delete_clips` took the rows out. The
    /// hash set is maintained by `forget_many`, so the same content copied again
    /// is stored again instead of vanishing into an "already stored" no-op.
    fn forget_deleted(&self, items: &[ClipItem]) {
        let stems: HashSet<String> = items.iter().map(|item| item.stem.clone()).collect();
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .forget_many(&stems);
    }

    /// Carries out the tombstones another instance left in our folders.
    ///
    /// A tombstone is a request, and this is the only machine that can grant it: the
    /// row belongs to us and the folder it lives in is ours to write. The row, its
    /// blob, its pin and the tombstone itself all go, so the clip is gone for real
    /// rather than hidden.
    ///
    /// Deliberately not on the capture path or the window thread: it runs on the
    /// rescan beat, so a deletion that arrives with a sync becomes real within a
    /// minute — and it was already invisible everywhere the marker had landed.
    pub fn reap_hidden(&self) {
        let doomed = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.hidden_clips(&self.settings.machine_id)
        };

        if doomed.is_empty() {
            return;
        }

        delete_clips(&doomed);
        self.forget_deleted(&doomed);

        log::info(&format!("reaped {} tombstoned clip(s) of our own", doomed.len()));
    }

    // -------------------------------------------------------------- cleanup tools

    /// Counts up a `.bin` cleanup without deleting anything. Index order is
    /// already newest first, so "keep the newest N" is a skip on the same walk
    /// rather than a sort.
    ///
    /// The snapshot is taken under the lock and every stat afterwards runs
    /// without it: this scans while the capture path keeps capturing.
    pub fn scan_bin_cleanup(&self, scope: BinScope) -> BinScan {
        let now = settings::now_ms();
        let month = settings::month_bucket(now);

        let heavy: Vec<ClipItem> = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index
                .visible_items()
                .into_iter()
                .filter(|item| item.has_blob && !item.pinned)
                .collect()
        };

        let in_range: Vec<ClipItem> = match scope {
            BinScope::OlderThanDays(days) => {
                if days == 0 {
                    heavy
                } else {
                    let cutoff = now - (days as i64) * 86_400_000;
                    heavy
                        .into_iter()
                        .filter(|item| item.at < cutoff)
                        .collect()
                }
            }
            BinScope::KeepNewest(keep) => heavy.into_iter().skip(keep as usize).collect(),
        };

        let mut doomed = Vec::new();
        let mut skipped_live_month = 0;
        for item in in_range {
            if deletable(&item, &self.settings.machine_id, &month) {
                doomed.push(item);
            } else {
                skipped_live_month += 1;
            }
        }

        let mut images = 0;
        let mut overlong_texts = 0;
        let mut blob_bytes = 0;
        for item in &doomed {
            match item.kind {
                ClipKind::Image => images += 1,
                _ => overlong_texts += 1,
            }
            // The size of what is actually there; a blob the sync lost still
            // counts as a cleanup (it takes the broken row with it) but frees
            // nothing.
            if let Ok(meta) = fs::metadata(&item.blob_path) {
                blob_bytes += meta.len();
            }
        }

        BinScan {
            doomed,
            skipped_live_month,
            images,
            overlong_texts,
            blob_bytes,
        }
    }

    /// Carries out a `.bin` scan. Whole clips go — row, blob, markers — not just
    /// the file: an entry whose bytes are gone cannot paste anything (images) or
    /// would paste a silent truncation (long text), and a history of entries
    /// that look alive but are not is worse than a shorter honest one. Copying
    /// the same content again afterwards stores it again, like any other
    /// deleted clip.
    pub fn run_bin_cleanup(&self, doomed: Vec<ClipItem>) -> usize {
        if doomed.is_empty() {
            return 0;
        }

        delete_clips(&doomed);
        self.forget_deleted(&doomed);

        log::info(&format!("bin cleanup removed {} heavy clip(s)", doomed.len()));
        doomed.len()
    }

    /// Groups the visible text clips by content hash and lists every copy except
    /// the one each group keeps: the newest, plus any pinned copy — a pin is a
    /// promise that this clip survives cleanup, and it does not get broken here.
    ///
    /// Duplicates mostly come from two machines having captured the same content
    /// before sync could tell either one; within one machine the capture path's
    /// hash set makes them rare. Text only, as asked of it: images have no
    /// inline preview worth keeping and file lists dedupe badly by exact path.
    pub fn scan_duplicates(&self) -> DupeScan {
        let visible = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.visible_items()
        };

        // Index order is newest first, so the first copy seen is the one kept.
        let mut groups: HashMap<String, Vec<ClipItem>> = HashMap::new();
        for item in visible {
            if item.kind == ClipKind::Text && !item.hash.is_empty() {
                groups.entry(item.hash.clone()).or_default().push(item);
            }
        }

        let mut scan = DupeScan {
            doomed: Vec::new(),
            groups: 0,
            groups_with_pin: 0,
        };

        for members in groups.into_values() {
            if members.len() < 2 {
                continue;
            }

            scan.groups += 1;
            let mut kept_newest = false;
            let mut pinned = false;
            for member in members {
                if member.pinned {
                    pinned = true;
                    continue;
                }
                if kept_newest {
                    scan.doomed.push(member);
                } else {
                    kept_newest = true;
                }
            }
            if pinned {
                scan.groups_with_pin += 1;
            }
        }

        scan
    }

    /// Carries out a duplicate scan. The split is the one a hand-delete follows:
    /// rows this machine may write go out at once, rows in another instance's
    /// live month are tombstoned and that machine does the taking out. Reports
    /// `(deleted, marked)`.
    pub fn run_duplicate_cleanup(&self, doomed: Vec<ClipItem>) -> (usize, usize) {
        if doomed.is_empty() {
            return (0, 0);
        }

        let month = settings::month_bucket(settings::now_ms());
        let mut direct = Vec::new();
        let mut marked = Vec::new();
        for item in doomed {
            if deletable(&item, &self.settings.machine_id, &month) {
                direct.push(item);
            } else {
                marked.push(item);
            }
        }

        let (deleted, tombstoned) = self.remove_clips(direct, marked);

        log::info(&format!(
            "duplicate cleanup: {} row(s) deleted, {} tombstoned",
            deleted, tombstoned
        ));

        (deleted, tombstoned)
    }

    // -------------------------------------------------------------------- watch

    pub fn on_path_changed(&self, path: &Path) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if name == DB_NAME {
            self.refresh_db(path, true);
        } else if name.ends_with(PIN_SUFFIX) {
            let stem = stem_of(path, PIN_SUFFIX);
            let exists = path.exists();
            self.index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_pinned(&stem, exists);
        } else if name.ends_with(HIDDEN_SUFFIX) {
            let stem = stem_of(path, HIDDEN_SUFFIX);
            let exists = path.exists();
            self.index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_hidden(&stem, exists);
        }
    }

    pub fn on_path_removed(&self, path: &Path) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if name == DB_NAME {
            self.index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .forget_db(path);
            self.loaded
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(path);

            log::info(&format!("clip database removed: {}", path.display()));
        } else if name.ends_with(PIN_SUFFIX) {
            let stem = stem_of(path, PIN_SUFFIX);
            self.index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_pinned(&stem, false);
        } else if name.ends_with(HIDDEN_SUFFIX) {
            let stem = stem_of(path, HIDDEN_SUFFIX);
            self.index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_hidden(&stem, false);
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
            // The same beat as the rescan that read the markers: a tombstone sitting
            // in one of our own folders is ours to carry out, and the walk above has
            // just brought the set up to date.
            self.reap_hidden();
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


/// One row, straight out of the database.
struct Row {
    stem: String,
    at: i64,
    machine: String,
    kind: String,
    hash: String,
    text: Option<String>,
    length: i64,
    blob: Option<String>,
    /// The source window. `Option` so a row that leaves them NULL reads as empty
    /// rather than failing the whole database; empty is what "could not be read"
    /// looks like downstream.
    app: Option<String>,
    title: Option<String>,
}

impl Row {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
        Ok(Row {
            stem: row.get(0)?,
            at: row.get(1)?,
            machine: row.get(2)?,
            kind: row.get(3)?,
            hash: row.get(4)?,
            text: row.get(5)?,
            length: row.get(6)?,
            blob: row.get(7)?,
            app: row.get(8)?,
            title: row.get(9)?,
        })
    }
}

/// Writes one clip into the database for its month, creating both on first use
/// so the layout stays a pure function of the timestamp.
///
/// The journal mode is left at SQLite's default (DELETE) rather than WAL: write
/// ahead logging puts the new data in `-wal`/`-shm` siblings, which a sync client
/// would have to move as a set, and `-shm` is specific to the machine that wrote
/// it — a copy of just the `.db` would not be readable anywhere else.
fn insert_row(db_path: &Path, record: &ClipRecord) -> Result<(), String> {
    let conn = open_rw(db_path)?;

    conn.execute_batch(DB_SCHEMA)
        .map_err(|err| format!("schema {}: {err}", db_path.display()))?;

    conn.execute(
        INSERT_ROW,
        params![
            &record.id,
            record.at,
            &record.machine,
            &record.kind,
            &record.hash,
            &record.text,
            record.length,
            &record.blob,
            &record.app,
            &record.title,
        ],
    )
    .map_err(|err| format!("insert into {}: {err}", db_path.display()))?;


    Ok(())
}

/// Leaves the tombstone for one clip: an empty `<stem>.del` beside it, in the folder
/// of the database that holds the row.
///
/// Same file and same reasoning as the pin marker — see `hidden_path_for` — and the
/// same failure handling: a marker that cannot be written is logged and the clip
/// stays visible, because a delete nobody can see the record of is worse than one
/// that did not happen.
fn write_marker(item: &ClipItem) -> bool {
    let path = item.hidden_path();
    if path.as_os_str().is_empty() {
        return false;
    }

    match fs::write(&path, []) {
        Ok(()) => true,
        Err(err) => {
            log::error(&format!("tombstone {}: {err}", path.display()));
            false
        }
    }
}

/// Deletes a batch of clips: the rows of every database involved, then the blob, the
/// pin and the tombstone beside them.
///
/// A batch rather than one call per clip because retention empties in bursts — one
/// transaction per database instead of a commit per row, and one file open instead
/// of one per clip.
fn delete_clips(items: &[ClipItem]) {
    let mut stems_by_db: HashMap<&Path, Vec<&str>> = HashMap::new();

    for item in items {
        stems_by_db
            .entry(item.db_path.as_path())
            .or_default()
            .push(item.stem.as_str());
    }

    for (db_path, stems) in stems_by_db {
        delete_rows(db_path, &stems);
    }

    // The files go even when the row could not: an orphaned blob costs disk, a row
    // pointing at a missing blob is a broken entry, so this is the safe order.
    for item in items {
        if item.has_blob && !item.blob_path.as_os_str().is_empty() {
            remove_file(&item.blob_path);
        }

        remove_file(&item.pin_path());
        // The tombstone has done its job the moment the row is gone, so it goes with
        // it — whether it was ours or another instance's request that this happen.
        remove_file(&item.hidden_path());
    }
}

/// Deletes rows from one database in a single transaction. The transaction is the
/// whole point: without it SQLite commits per statement, and each of those is a
/// journal write and an fsync.
fn delete_rows(db_path: &Path, stems: &[&str]) {
    let result = open_rw(db_path).and_then(|mut conn| {
        let transaction = conn.transaction().map_err(|err| err.to_string())?;

        {
            let mut statement = transaction
                .prepare("DELETE FROM clips WHERE stem = ?1")
                .map_err(|err| err.to_string())?;

            for stem in stems {
                statement
                    .execute(params![stem])
                    .map_err(|err| err.to_string())?;
            }
        }

        transaction.commit().map_err(|err| err.to_string())
    });

    if let Err(err) = result {
        log::error(&format!(
            "delete {} clip(s) from {}: {err}",
            stems.len(),
            db_path.display()
        ));
    }
}

/// Whether a row is ours to delete: this machine's own, or one sitting in a month
/// that is over. Buckets are named `yyyy-MM`, so comparing two of them as strings is
/// comparing them as dates — and a month that has not started yet belongs to that
/// other machine just as much as the current one does.
///
/// A row whose database has no month folder to read is left alone rather than
/// guessed at; the layout does not produce one.
fn deletable(item: &ClipItem, local_machine: &str, current_month: &str) -> bool {
    if item.machine == local_machine {
        return true;
    }

    month_of(&item.db_path).is_some_and(|month| month.as_str() < current_month)
}

fn month_of(db_path: &Path) -> Option<String> {
    db_path
        .parent()?
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

fn open_rw(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|err| format!("open {}: {err}", path.display()))?;
    let _ = conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));
    Ok(conn)
}

/// `(len, mtime in ms)`. A sync client that rewrites a file without changing its
/// length still moves the mtime, so the pair is enough to skip an unchanged one.
fn stamp_of(path: &Path) -> Option<(u64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0);

    Some((meta.len(), mtime))
}

/// Whether this payload would need a `.bin` sibling, which is the same question
/// as whether `WriteBlobs` off means dropping it. A path list never does.
pub fn needs_blob(payload: &ClipPayload, settings: &Settings) -> bool {
    match payload {
        ClipPayload::Text(text) => text.chars().count() > settings.inline_text_limit,
        ClipPayload::Image(_) => true,
        ClipPayload::Files(_) => false,
    }
}

/// Decides what stays inline and whether a `.bin` sibling is needed.
/// Inline text is what a search can see, so the split trades coverage against size.
fn plan_payload(
    payload: &ClipPayload,
    stem: &str,
    settings: &Settings,
) -> (Option<String>, Option<String>) {
    let blob_name = format!("{stem}{BIN_SUFFIX}");

    match payload {
        ClipPayload::Text(text) => {
            if needs_blob(payload, settings) {
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
/// different clips. Uppercase hex, and frozen: a hash already in a database has
/// to keep matching.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clipplus-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(stem: &str, text: Option<&str>, blob: Option<&str>) -> ClipRecord {
        ClipRecord {
            id: stem.to_string(),
            at: 1_769_000_000_000,
            machine: "3f9a2c81".to_string(),
            kind: if blob.is_some() { "image" } else { "text" }.to_string(),
            hash: "AB".repeat(32),
            text: text.map(str::to_string),
            length: 5,
            blob: blob.map(str::to_string),
            // Empty by default: the capture side is what fills these in, and a
            // clip from a window that could not be read is the common case.
            app: String::new(),
            title: String::new(),
        }
    }

    /// Turning `WriteBlobs` off drops exactly what this says needs a blob, so the
    /// boundary is worth pinning: text of exactly the limit still stays inline.
    #[test]
    fn needs_blob_follows_the_inline_split() {
        let settings = Settings {
            inline_text_limit: 4,
            ..Settings::default()
        };

        let text = |chars: usize| ClipPayload::Text("x".repeat(chars));
        assert!(!needs_blob(&text(4), &settings));
        assert!(needs_blob(&text(5), &settings));
        assert!(needs_blob(&ClipPayload::Image(vec![1]), &settings));
        assert!(!needs_blob(&ClipPayload::Files(vec!["a".to_string()]), &settings));
    }

    /// A clip now lives only in the database, so the two things that would
    /// silently lose history get a test: the round trip, and the insert of a
    /// stem that is already there (a retried capture must not kill the writer
    /// thread), plus the delete retention leans on.
    #[test]
    fn rows_round_trip_and_delete() {
        let dir = scratch("db");
        let path = dir.join(DB_NAME);

        insert_row(&path, &record("stem-1", Some("hello"), None)).unwrap();
        insert_row(&path, &record("stem-2", None, Some("stem-2.bin"))).unwrap();
        insert_row(&path, &record("stem-1", Some("hello"), None)).unwrap();

        let rows = read_all(&path);
        assert_eq!(rows.len(), 2);

        let first = rows.iter().find(|row| row.stem == "stem-1").unwrap();
        assert_eq!(first.text.as_deref(), Some("hello"));
        assert_eq!(first.blob, None);
        assert_eq!(first.at, 1_769_000_000_000);
        assert_eq!(first.machine, "3f9a2c81");

        let second = rows.iter().find(|row| row.stem == "stem-2").unwrap();
        assert_eq!(second.text, None);
        assert_eq!(second.blob.as_deref(), Some("stem-2.bin"));

        delete_rows(&path, &["stem-1"]);
        let left = read_all(&path);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].stem, "stem-2");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Deleting by hand is the second thing in the app that destroys history, and it
    /// has to respect the line retention already respects — with one exception
    /// retention does not have: another instance's *finished* months are dead files
    /// nobody writes, so those are ours to clean up.
    #[test]
    fn only_a_finished_month_from_another_instance_is_deletable() {
        let item = |machine: &str, path: &str| {
            let mut row = record("stem-1", Some("hello"), None);
            row.machine = machine.to_string();

            ClipItem::from_record(
                &row,
                PathBuf::from(path),
                "stem-1".to_string(),
                false,
                "local",
                crate::settings::current_year(),
            )
        };

        // Ours, whenever it is: we are the only writer of our own folders.
        let mine = item("local", "C:/sync/local/2026-02/clips.db");
        assert!(deletable(&mine, "local", "2026-02"));

        // Another instance's month is over: dead on every machine, fair game.
        let old = item("other", "C:/sync/other/2025-12/clips.db");
        assert!(deletable(&old, "local", "2026-02"));

        // Theirs is still open, and one that has not started yet is the same file a
        // moment from now.
        let open = item("other", "C:/sync/other/2026-02/clips.db");
        assert!(!deletable(&open, "local", "2026-02"));
        let next = item("other", "C:/sync/other/2026-03/clips.db");
        assert!(!deletable(&next, "local", "2026-02"));

        // No month folder to read: left alone rather than assumed deletable.
        let flat = item("other", DB_NAME);
        assert!(!deletable(&flat, "local", "2026-02"));
    }

    fn read_all(path: &Path) -> Vec<Row> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let mut statement = conn.prepare(SELECT_ROWS).unwrap();
        let rows = statement
            .query_map([], Row::read)
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        rows
    }


    /// The source window is two columns on the row itself, so the round trip is
    /// where a mix-up would show: written when it is known, empty when it is not.
    #[test]
    fn source_window_round_trips() {
        let dir = scratch("source");
        let path = dir.join(DB_NAME);

        let mut sourced = record("stem-1", Some("hello"), None);
        sourced.app = "chrome.exe".to_string();
        sourced.title = "GitHub".to_string();

        insert_row(&path, &sourced).unwrap();
        insert_row(&path, &record("stem-2", Some("plain"), None)).unwrap();

        let rows = read_all(&path);
        let sourced = rows.iter().find(|row| row.stem == "stem-1").unwrap();
        assert_eq!(sourced.app.as_deref(), Some("chrome.exe"));
        assert_eq!(sourced.title.as_deref(), Some("GitHub"));

        // A clip out of a window that could not be read stores empty strings,
        // which is what the meta line treats as "no source".
        let plain = rows.iter().find(|row| row.stem == "stem-2").unwrap();
        assert_eq!(plain.app.as_deref(), Some(""));
        assert_eq!(plain.title.as_deref(), Some(""));

        delete_rows(&path, &["stem-1"]);
        assert_eq!(read_all(&path).len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Retention is the only thing in the app that destroys data, and it is
    /// off by default, so its two halves get a test: the row goes, and what it
    /// left beside the database goes with it.
    #[test]
    fn retention_deletes_the_row_not_the_database() {
        let dir = scratch("retention");
        let path = dir.join(DB_NAME);

        insert_row(&path, &record("stem-1", None, Some("stem-1.bin"))).unwrap();
        insert_row(&path, &record("stem-2", Some("bye"), None)).unwrap();
        fs::write(dir.join("stem-1.bin"), b"blob").unwrap();
        fs::write(dir.join("stem-1.pin"), []).unwrap();
        fs::write(dir.join("stem-1.del"), []).unwrap();

        let row = record("stem-1", None, Some("stem-1.bin"));
        let item = ClipItem::from_record(
            &row,
            path.clone(),
            "stem-1".to_string(),
            true,
            "3f9a2c81",
            crate::settings::current_year(),
        );
        delete_clips(&[item]);

        assert!(path.exists(), "the database must outlive its own row");
        assert_eq!(read_all(&path).len(), 1);
        assert!(!dir.join("stem-1.bin").exists());
        assert!(!dir.join("stem-1.pin").exists());
        assert!(!dir.join("stem-1.del").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    /// A row built to order: the tests below need control over the timestamp,
    /// the machine, the kind and the hash, which the `record` helper above
    /// pins to fixed values.
    fn clip_row(
        stem: &str,
        at: i64,
        machine: &str,
        kind: &str,
        hash: &str,
        blob: Option<&str>,
    ) -> ClipRecord {
        ClipRecord {
            id: stem.to_string(),
            at,
            machine: machine.to_string(),
            kind: kind.to_string(),
            hash: hash.to_string(),
            text: if kind == "image" {
                None
            } else {
                Some(format!("clip {stem}"))
            },
            length: 5,
            blob: blob.map(str::to_string),
            app: String::new(),
            title: String::new(),
        }
    }

    /// A store over an empty sync root; rows are written into it afterwards the
    /// way a real month folder looks, and a rescan brings them in.
    fn cleanup_store(name: &str) -> (Store, PathBuf) {
        let dir = scratch(name);
        let mut settings = Settings::default();
        settings.sync_root = dir.join("sync");
        settings.machine_id = "mach1".to_string();

        let store = Store::new(settings);
        (store, dir)
    }

    /// Writes a row (and its `.bin`, if any) where the layout would put it:
    /// `<root>/<machine>/<month>/clips.db`.
    fn plant(store_dir: &Path, machine: &str, row: &ClipRecord) -> PathBuf {
        let folder = store_dir
            .join("sync")
            .join(machine)
            .join(settings::month_bucket(row.at));
        fs::create_dir_all(&folder).unwrap();

        if let Some(name) = &row.blob {
            fs::write(folder.join(name), b"blob-bytes").unwrap();
        }

        let path = folder.join(DB_NAME);
        insert_row(&path, row).unwrap();
        path
    }

    const DAY: i64 = 24 * 60 * 60 * 1000;

    /// The `.bin` scan has to find exactly the heavy clips in range, leave
    /// short text alone, honour pins, size up what deleting would free, and
    /// skip another instance's live month; the run then has to take the row
    /// and the file both. The two scopes get one check each.
    #[test]
    fn bin_cleanup_scans_and_deletes_whole_heavy_clips() {
        let now = settings::now_ms();
        let (store, dir) = cleanup_store("binscan");

        // Inline text: no blob, so no scope ever reaches it.
        plant(&dir, "mach1", &clip_row("s1", now - 2 * DAY, "mach1", "text", "h1", None));
        // An old heavy clip, pinned: the pin is the promise that keeps it.
        let pinned = plant(
            &dir,
            "mach1",
            &clip_row("s3", now - 40 * DAY - 3_600_000, "mach1", "image", "h3", Some("s3.bin")),
        );
        fs::write(pinned.with_file_name("s3.pin"), []).unwrap();
        let old_text = plant(
            &dir,
            "mach1",
            &clip_row("s2", now - 40 * DAY, "mach1", "text", "h2", Some("s2.bin")),
        );
        let old_image = plant(
            &dir,
            "mach1",
            &clip_row("s5", now - 40 * DAY - 2 * 3_600_000, "mach1", "image", "h5", Some("s5.bin")),
        );
        // A fresh heavy clip: too young for the time scope, in the kept half of the count one.
        plant(&dir, "mach1", &clip_row("s4", now - DAY, "mach1", "image", "h4", Some("s4.bin")));
        // Another instance's live month: in range for "everything", but not ours
        // to write. The folder is named by hand so the test cannot flip over at
        // a month boundary the way an at-derived one would in the month's first
        // hours.
        let other_live = dir
            .join("sync")
            .join("other")
            .join(settings::month_bucket(now));
        fs::create_dir_all(&other_live).unwrap();
        insert_row(
            &other_live.join(DB_NAME),
            &clip_row("o1", now - 2 * 3_600_000, "other", "image", "h6", Some("o1.bin")),
        )
        .unwrap();
        fs::write(other_live.join("o1.bin"), b"blob-bytes").unwrap();

        store.rescan();

        let by_time = store.scan_bin_cleanup(BinScope::OlderThanDays(30));
        assert_eq!(
            by_time
                .doomed
                .iter()
                .map(|item| item.stem.as_str())
                .collect::<Vec<_>>(),
            vec!["s2", "s5"]
        );
        assert_eq!((by_time.images, by_time.overlong_texts), (1, 1));
        assert_eq!(by_time.blob_bytes, 20); // two planted .bin files, 10 bytes each
        assert_eq!(by_time.skipped_live_month, 0);

        let by_count = store.scan_bin_cleanup(BinScope::KeepNewest(0));
        // Everything, so the other instance's live month shows up as a skip.
        assert_eq!(by_count.skipped_live_month, 1);
        assert_eq!(by_count.doomed.len(), 3);

        let keep_two = store.scan_bin_cleanup(BinScope::KeepNewest(2));
        // Newest first across machines: o1 and s4 fill the two kept slots, so
        // the older two go — even though o1 itself cannot be deleted from here.
        assert_eq!(
            keep_two
                .doomed
                .iter()
                .map(|item| item.stem.as_str())
                .collect::<Vec<_>>(),
            vec!["s2", "s5"]
        );

        let removed = store.run_bin_cleanup(by_time.doomed);
        assert_eq!(removed, 2);

        assert_eq!(read_all(&old_text).len(), 0);
        assert!(!old_text.with_file_name("s2.bin").exists());
        assert!(!old_image.with_file_name("s5.bin").exists());
        // The pinned clip and everything out of scope are still listed; the
        // pinned one comes first because that is how the list orders them.
        let listed: Vec<String> = store
            .query(None, "", 100)
            .into_iter()
            .map(|row| row.stem)
            .collect();
        assert_eq!(listed, vec!["s3", "o1", "s4", "s1"]);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Duplicates keep the newest copy of each text, plus any pinned copy, and
    /// a copy in another instance's live month goes out as a tombstone rather
    /// than a deletion.
    #[test]
    fn duplicate_cleanup_keeps_the_newest_and_honours_pins() {
        let now = settings::now_ms();
        let (store, dir) = cleanup_store("dupscan");

        // Folders are named by hand rather than derived from the timestamps, so
        // the test does not flip over at a month boundary: every row sits in
        // its machine's current month, and the timestamps only decide the order.
        let this_month = settings::month_bucket(now);
        let plant_text = |machine: &str, stem: &str, at: i64| {
            let row = clip_row(stem, at, machine, "text", "DUP", None);
            let folder = dir.join("sync").join(machine).join(&this_month);
            fs::create_dir_all(&folder).unwrap();
            let path = folder.join(DB_NAME);
            insert_row(&path, &row).unwrap();
            path
        };

        // Five copies of one text. Index order (newest first) decides the kept
        // one: t4. t0 is second, so it goes — but it lives in another
        // instance's current month, so only a tombstone may name it. t2 is
        // pinned and stays beside the newest. t3 and t1 are plain older copies.
        let own_db = plant_text("mach1", "t1", now - 3 * DAY);
        let pinned = plant_text("other", "t2", now - 2 * DAY);
        plant_text("mach1", "t3", now - DAY);
        let live_other = plant_text("other", "t0", now - 2 * 3_600_000);
        plant_text("other", "t4", now - 3_600_000);
        fs::write(pinned.with_file_name("t2.pin"), []).unwrap();

        store.rescan();

        let scan = store.scan_duplicates();
        assert_eq!(scan.groups, 1);
        assert_eq!(scan.groups_with_pin, 1);
        assert_eq!(
            scan.doomed
                .iter()
                .map(|item| item.stem.as_str())
                .collect::<Vec<_>>(),
            vec!["t0", "t3", "t1"]
        );

        let (deleted, tombstoned) = store.run_duplicate_cleanup(scan.doomed);
        assert_eq!((deleted, tombstoned), (2, 1));

        // The own rows are gone for real, the live-month row is marked only.
        assert_eq!(read_all(&own_db).len(), 0);
        assert_eq!(read_all(&live_other).len(), 3);
        assert!(live_other.with_file_name("t0.del").exists());

        // The list shows the kept copies only — the pinned one first, because
        // that is how the list orders them — and the tombstoned one is hidden.
        let listed: Vec<String> = store
            .query(None, "", 100)
            .into_iter()
            .map(|row| row.stem)
            .collect();
        assert_eq!(listed, vec!["t2", "t4"]);

        let _ = fs::remove_dir_all(&dir);
    }
}
