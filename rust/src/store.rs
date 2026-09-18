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

    /// Full folder walk: every database under the sync root, plus pin markers.
    /// Databases are stamped, so one that has not changed costs a `stat`.
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

            if name == DB_NAME {
                self.refresh_db(&path, false);
            } else if name.ends_with(PIN_SUFFIX) {
                let stem = stem_of(&path, PIN_SUFFIX);
                if !stem.is_empty() {
                    pinned.insert(stem);
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

        {
            let stems: HashSet<String> = doomed.iter().map(|item| item.stem.clone()).collect();
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.forget_many(&stems);
        }

        log::info(&format!(
            "retention removed {} clip(s) older than {days} day(s)",
            doomed.len()
        ));
    }

    // ------------------------------------------------------------------- deletion

    /// Deletes the clips the user picked in the list, and reports how many were left
    /// alone because another instance is still writing their month.
    ///
    /// This machine's own rows are always its own to delete. Another instance's are
    /// deletable once their month is over, because a month that is over has no
    /// writer: nothing ever writes a past bucket, so the file is only ever read and
    /// a stray deletion there costs nothing — two machines deleting from one frozen
    /// month can lose a deletion to the sync client's last-writer-wins, never a clip.
    ///
    /// A live month does have a writer, and it is not us. The whole layout rests on
    /// one writer per file; a second one working from a snapshot that may be an hour
    /// old would not undo a rival deletion, it would drop every clip that machine
    /// captured since that snapshot was taken.
    ///
    /// Returns `(deleted, refused)`.
    pub fn delete_selected(&self, stems: &[String]) -> (usize, usize) {
        let month = settings::month_bucket(settings::now_ms());

        let (doomed, refused) = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            let mut doomed: Vec<ClipItem> = Vec::new();
            let mut refused = 0;

            for stem in stems {
                match index.find(stem) {
                    Some(item) if deletable(item, &self.settings.machine_id, &month) => {
                        doomed.push(item.clone());
                    }
                    Some(_) => refused += 1,
                    // Already gone: the file was reloaded since the popup was
                    // drawn. Nothing to delete and nothing to complain about.
                    None => {}
                }
            }

            (doomed, refused)
        };

        if doomed.is_empty() {
            return (0, refused);
        }

        delete_clips(&doomed);

        {
            let removed: HashSet<String> = doomed.iter().map(|item| item.stem.clone()).collect();
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.forget_many(&removed);
        }

        log::info(&format!(
            "deleted {} clip(s) by hand ({refused} refused)",
            doomed.len()
        ));

        (doomed.len(), refused)
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

/// Deletes a batch of clips: the rows of every database involved, then the blob and
/// pin files beside them.
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

        let _ = fs::remove_dir_all(&dir);
    }
}
