//! In-memory index of everything found in the synced databases.
//!
//! The databases are the source of truth; this is a projection that a restart
//! rebuilds from scratch. Nothing here is ever persisted, which is exactly why
//! there is no cache to go stale.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::clip::{ClipKind, ClipRecord};
use crate::win;

const PREVIEW_CHARS: usize = 160;

#[derive(Clone, Debug)]
pub struct ClipItem {
    pub stem: String,
    pub at: i64,
    pub machine: String,
    pub kind: ClipKind,
    pub hash: String,
    /// The database this entry lives in. Its folder is where the `.bin` and
    /// `.pin` siblings go.
    pub db_path: PathBuf,
    /// Full text, or the retained prefix when `has_blob` is set.
    pub text: String,
    pub has_blob: bool,
    pub blob_path: PathBuf,
    pub pinned: bool,
    /// Precomputed once at index time so a keystroke never formats anything.
    preview: String,
    meta: String,
    /// The values the second line is built from, kept apart so `app:chrome` cannot
    /// reach a window title and `title:` cannot reach an application. Only the
    /// qualified search terms read these; the row paints `meta`.
    when: String,
    who: String,
    app: String,
    title: String,
}

/// What the list needs to render one row. Deliberately small: the full text can
/// be kilobytes, and this is cloned per keystroke.
#[derive(Clone, Debug)]
pub struct ClipSummary {
    pub stem: String,
    pub preview: String,
    pub meta: String,
    pub pinned: bool,
    /// Whether part of the payload lives in a `.bin`: the popup reads those in
    /// the background rather than on the window thread, and this is how it
    /// tells them apart from the instant inline ones.
    pub has_blob: bool,
    /// 文本/图片/文件:图片行要走缩略图管线,其余行保持纯文本渲染。
    pub kind: ClipKind,
}

impl ClipItem {
    pub fn from_record(
        record: &ClipRecord,
        db_path: PathBuf,
        stem: String,
        pinned: bool,
        local_machine: &str,
        now_year: u16,
    ) -> ClipItem {
        let kind = ClipKind::from_name(&record.kind);
        let text = record.text.clone().unwrap_or_default();
        let has_blob = record.blob.is_some();

        let blob_path = match &record.blob {
            Some(name) => db_path.with_file_name(name),
            None => PathBuf::new(),
        };

        let preview = build_preview(kind, &text, has_blob);
        let line = Meta::of(record, kind, has_blob, local_machine, now_year);

        ClipItem {
            stem,
            at: record.at,
            machine: record.machine.clone(),
            kind,
            hash: record.hash.clone(),
            db_path,
            text,
            has_blob,
            blob_path,
            pinned,
            preview,
            meta: line.text(),
            when: line.when,
            who: line.who,
            app: line.app,
            title: line.title,
        }
    }

    pub fn pin_path(&self) -> PathBuf {
        pin_path_for(&self.db_path, &self.stem)
    }

    /// The tombstone beside this clip, if one was left for it.
    pub fn hidden_path(&self) -> PathBuf {
        hidden_path_for(&self.db_path, &self.stem)
    }

    pub fn summary(&self) -> ClipSummary {
        ClipSummary {
            stem: self.stem.clone(),
            preview: self.preview.clone(),
            meta: self.meta.clone(),
            pinned: self.pinned,
            has_blob: self.has_blob,
            kind: self.kind,
        }
    }
}

/// `<stem>.pin`, in the database's own folder. Pinning adds a sibling marker
/// rather than rewriting the row, so it costs nothing and cannot conflict.
pub fn pin_path_for(db_path: &std::path::Path, stem: &str) -> PathBuf {
    let folder = db_path.parent().unwrap_or(std::path::Path::new(""));
    folder.join(format!("{stem}{}", crate::store::PIN_SUFFIX))
}

/// `<stem>.del`, in the database's own folder: a tombstone. Same shape and the same
/// reasoning as the pin marker — an empty file has identical content on every
/// machine, so two machines leaving one cannot be seen as a conflict.
///
/// It is a *request* as much as a record: the row it names is still there, and only
/// the machine that owns it may take it out. Until that machine does, this file is
/// what keeps the clip off the list everywhere.
pub fn hidden_path_for(db_path: &std::path::Path, stem: &str) -> PathBuf {
    let folder = db_path.parent().unwrap_or(std::path::Path::new(""));
    folder.join(format!("{stem}{}", crate::store::HIDDEN_SUFFIX))
}

#[derive(Default)]
pub struct Index {
    /// Descending by `at`, maintained by binary insertion so a query never sorts.
    items: Vec<ClipItem>,
    stems: HashSet<String>,
    hashes: HashSet<String>,
    /// Stems another instance has tombstoned. Held as a set rather than as a flag on
    /// the item, because the items are rebuilt from scratch whenever a database
    /// changes: a flag would come back false and the clip would reappear, whereas a
    /// read consults this set and `forget_db` never touches it.
    hidden: HashSet<String>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.items.len()
    }


    pub fn has_hash(&self, hash: &str) -> bool {
        self.hashes.contains(hash)
    }

    /// The newest visible clip carrying `hash`, for the capture path: a re-copied
    /// clip has to surface the copy already stored instead of being swallowed by
    /// the "already stored" answer, and surfacing means knowing which row to move.
    /// Items are newest first, so the first match is the one; hidden copies do not
    /// count — the same visibility rule `has_hash` answers under, so the two never
    /// disagree about whether a capture is a re-copy.
    pub fn newest_by_hash(&self, hash: &str) -> Option<ClipItem> {
        self.items
            .iter()
            .find(|item| item.hash == hash && !self.hidden.contains(&item.stem))
            .cloned()
    }

    pub fn find(&self, stem: &str) -> Option<&ClipItem> {
        self.items.iter().find(|item| item.stem == stem)
    }

    /// Returns false when the stem was already known.
    pub fn insert(&mut self, item: ClipItem) -> bool {
        if !self.stems.insert(item.stem.clone()) {
            return false;
        }

        // A hidden clip does not answer the capture path with its hash — see
        // `rebuild_hashes` — and a database reload puts its rows back through here.
        if !item.hash.is_empty() && !self.hidden.contains(&item.stem) {
            self.hashes.insert(item.hash.clone());
        }

        let position = self
            .items
            .partition_point(|existing| existing.at > item.at);
        self.items.insert(position, item);
        true
    }

    /// Adds a batch the way `insert` adds one item, in a single merge rather than
    /// one shift per row. This is the load path: a month of history is thousands
    /// of rows, and each `insert` moves everything after its position.
    ///
    /// The batch does not have to be sorted; stems already known are skipped.
    pub fn insert_many(&mut self, items: Vec<ClipItem>) {
        let mut batch: Vec<ClipItem> = Vec::with_capacity(items.len());

        for item in items {
            if !self.stems.insert(item.stem.clone()) {
                continue;
            }

            // Same guard as `insert`: this is the path a reloaded database comes
            // back in, and a tombstoned row must not put its hash back in the set.
            if !item.hash.is_empty() && !self.hidden.contains(&item.stem) {
                self.hashes.insert(item.hash.clone());
            }

            batch.push(item);
        }

        if batch.is_empty() {
            return;
        }

        // Descending by time, and reversed first so that two clips captured in the
        // same millisecond keep the order they were captured in: a stable sort of the
        // reversed batch puts the later one first, which is where `insert` puts it
        // (before everything of the same age).
        batch.reverse();
        batch.sort_by(|left, right| right.at.cmp(&left.at));

        // Sized before either run is moved out of its vector.
        let mut merged = Vec::with_capacity(self.items.len() + batch.len());
        let mut incoming = batch.into_iter().peekable();
        let mut existing = std::mem::take(&mut self.items).into_iter().peekable();

        loop {
            // `>=` on a tie, matching where `insert` puts an item of the same age.
            let take_incoming = match (incoming.peek(), existing.peek()) {
                (Some(new), Some(old)) => new.at >= old.at,
                (Some(_), None) => true,
                (None, _) => false,
            };

            let next = if take_incoming {
                incoming.next()
            } else {
                existing.next()
            };

            match next {
                Some(item) => merged.push(item),
                None => break,
            }
        }

        self.items = merged;
    }

    /// Drops a batch by stem. Retention removes its clips in a burst, and one scan
    /// of the whole index per stem is quadratic.
    pub fn forget_many(&mut self, stems: &HashSet<String>) {
        let mut dropped: HashSet<String> = HashSet::new();

        self.items.retain(|item| {
            if !stems.contains(&item.stem) {
                return true;
            }

            if !item.hash.is_empty() {
                dropped.insert(item.hash.clone());
            }
            false
        });

        for stem in stems {
            self.stems.remove(stem);
        }

        self.forget_hashes(&dropped);
    }

    /// Takes a batch of hashes out of the set, except the ones another *visible* clip
    /// still carries: the same content can legitimately have been stored by two
    /// machines, and it can also be hiding behind a tombstone — which does not count,
    /// see `rebuild_hashes`.
    fn forget_hashes(&mut self, dropped: &HashSet<String>) {
        if dropped.is_empty() {
            return;
        }

        self.hashes.retain(|hash| {
            !dropped.contains(hash)
                || self
                    .items
                    .iter()
                    .any(|item| !self.hidden.contains(&item.stem) && &item.hash == hash)
        });
    }

    /// Rebuilds `hashes` from the clips that can still be seen.
    ///
    /// `hashes` is the capture path's "already stored" set, so a clip nobody can see
    /// must not be in it: with a tombstone still answering for its hash, copying that
    /// content again would store nothing and then show nothing either. One pass, and
    /// only when the hidden set actually changed.
    fn rebuild_hashes(&mut self) {
        self.hashes = self
            .items
            .iter()
            .filter(|item| !item.hash.is_empty() && !self.hidden.contains(&item.stem))
            .map(|item| item.hash.clone())
            .collect();
    }

    /// Drops every entry that came out of one container. Used when a database
    /// changed (its rows are re-read from scratch) or disappeared.
    ///
    /// One pass over the index rather than a `forget` per stem: this runs again for
    /// every change to a database, and each of those scans the whole index.
    pub fn forget_db(&mut self, db_path: &std::path::Path) {
        // Taken rather than borrowed, because the stem and hash sets are updated
        // from inside the loop and holding a borrow of the items would not allow it.
        let existing = std::mem::take(&mut self.items);
        let mut kept = Vec::with_capacity(existing.len());
        let mut dropped: HashSet<String> = HashSet::new();

        for item in existing {
            if item.db_path == db_path {
                self.stems.remove(&item.stem);
                if !item.hash.is_empty() {
                    dropped.insert(item.hash);
                }
            } else {
                kept.push(item);
            }
        }

        self.items = kept;
        self.forget_hashes(&dropped);
    }

    pub fn set_pinned(&mut self, stem: &str, pinned: bool) -> bool {
        match self.items.iter_mut().find(|item| item.stem == stem) {
            Some(item) => {
                item.pinned = pinned;
                true
            }
            None => false,
        }
    }

    /// Re-applies the authoritative pin set gathered by a full folder walk.
    pub fn apply_pin_state(&mut self, pinned: &HashSet<String>) {
        for item in &mut self.items {
            item.pinned = pinned.contains(&item.stem);
        }
    }

    /// Re-applies the authoritative hidden set gathered by a full folder walk.
    ///
    /// Replaced rather than merged: the walk saw every marker in the folder, so a
    /// tombstone somebody removed by hand is a clip that comes back. Skipped when the
    /// set is unchanged, which is the normal case — this runs on every rescan.
    pub fn apply_hidden_state(&mut self, hidden: &HashSet<String>) {
        if self.hidden == *hidden {
            return;
        }

        self.hidden = hidden.clone();
        self.rebuild_hashes();
    }

    /// Hides a batch the user just deleted. Additive, unlike the walk's replace: the
    /// popup knows the rows it was asked about, not every marker in the folder.
    pub fn hide_many(&mut self, stems: &HashSet<String>) {
        self.hidden.extend(stems.iter().cloned());
        self.rebuild_hashes();
    }

    /// One marker appeared or went away: the single-row version of the two above, for
    /// the file watcher, which sees one file at a time. Answers whether the set
    /// changed — a stem nobody has a row for is still worth remembering, because the
    /// database it belongs to may only sync in later.
    pub fn set_hidden(&mut self, stem: &str, hidden: bool) -> bool {
        let changed = if hidden {
            self.hidden.insert(stem.to_string())
        } else {
            self.hidden.remove(stem)
        };

        if changed {
            self.rebuild_hashes();
        }

        changed
    }

    /// Clips another instance has tombstoned here: ours to take out for real, because
    /// this is the only machine allowed to write them. Pinned ones are left alone — a
    /// pin is a promise that this clip survives cleanup, made by whoever pinned it,
    /// and a tombstone does not get to break it.
    pub fn hidden_clips(&self, local_machine: &str) -> Vec<ClipItem> {
        self.items
            .iter()
            .filter(|item| {
                item.machine == local_machine && !item.pinned && self.hidden.contains(&item.stem)
            })
            .cloned()
            .collect()
    }

    /// Every instance that has a clip here, newest activity first. One pass: the
    /// items are already sorted by time, so the first clip seen for an instance
    /// is its newest.
    pub fn machines(&self) -> Vec<(String, i64)> {
        let mut out: Vec<(String, i64)> = Vec::new();

        for item in &self.items {
            // An instance whose clips are all tombstoned has nothing to show, and a
            // tab that opens on an empty list is worse than no tab at all.
            if self.hidden.contains(&item.stem) {
                continue;
            }

            if !out.iter().any(|(id, _)| id == &item.machine) {
                out.push((item.machine.clone(), item.at));
            }
        }

        out
    }

    /// Clips eligible for retention: this machine's own, old enough, not pinned.
    pub fn stale_clips(&self, cutoff_ms: i64, local_machine: &str) -> Vec<ClipItem> {
        self.items
            .iter()
            .filter(|item| {
                item.at < cutoff_ms && !item.pinned && item.machine == local_machine
            })
            .cloned()
            .collect()
    }

    /// Every clip that is not tombstoned, in index order (newest first), for the
    /// cleanup scans. A snapshot rather than a borrow: the scans stat blob files
    /// afterwards and must not hold the index lock across that disk I/O — the
    /// capture path needs that lock on every copy.
    ///
    /// Tombstoned rows are excluded: they are on their way out already, and
    /// counting them would promise a cleanup of rows the reaper is about to take
    /// anyway.
    pub fn visible_items(&self) -> Vec<ClipItem> {
        self.items
            .iter()
            .filter(|item| !self.hidden.contains(&item.stem))
            .cloned()
            .collect()
    }

    /// Pinned rows first, then newest first. One pass over the items rather than
    /// a pinned pass followed by a rest pass: with a needle that matches little
    /// or nothing the whole index is walked, and that walk happens on the popup's
    /// window thread once per keystroke, so the constant factor is worth keeping
    /// down.
    ///
    /// `rest` is capped at `limit` even though its final need (`limit` minus the
    /// pinned count) is not known until the walk ends; the tail is cut below, and
    /// the extra rows it may hold are exactly the ones a second pass would have
    /// collected anyway, in the same order.
    ///
    /// `needle` is the search box, split into terms: every one of them has to match,
    /// and a term without a `field:` prefix reads the clip's own text — plus the
    /// source fields the scope boxes add. `chips` also carries the two menu filters,
    /// which constrain every row the same way the machine tab does, pinned ones
    /// included — a filter is a filter.
    pub fn query(
        &self,
        machine: Option<&str>,
        needle: Option<&str>,
        chips: &ChipFilter,
        limit: usize,
    ) -> Vec<ClipSummary> {
        let terms = needle.map(parse_query).unwrap_or_default();
        let mut pinned: Vec<ClipSummary> = Vec::new();
        let mut rest: Vec<ClipSummary> = Vec::new();

        for item in &self.items {
            // Tombstoned clips are not here for the user, whichever tab is open.
            if self.hidden.contains(&item.stem) {
                continue;
            }

            if let Some(want) = machine {
                if item.machine != want {
                    continue;
                }
            }

            if !chips.allows(item) {
                continue;
            }

            // Empty terms match everything; skipping the call keeps the plain
            // browse path free of a per-row round trip through the parser.
            if !terms.is_empty() && !matches(item, &terms, chips) {
                continue;
            }

            if item.pinned {
                pinned.push(item.summary());
                // The pinned half is full and nothing unpinned can come before
                // it: the answer is complete even though the walk is not.
                if pinned.len() >= limit {
                    return pinned;
                }
            } else if pinned.len() + rest.len() < limit {
                rest.push(item.summary());
            }
        }

        let mut out = pinned;
        out.extend(rest.into_iter().take(limit.saturating_sub(out.len())));
        out
    }
}

/// One search term: which field it reads, and the word to look for in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Text,
    When,
    Machine,
    App,
    Title,
    Kind,
}

/// What the buttons beside the search box hold, in two families. The two boxes
/// are *scope*: checked, bare terms also look in the source fields, and a search
/// only ever gets wider. The two menus are *filters*: a kind or a time bound the
/// rows have to pass, and a row that fails stays hidden even if its text matched.
///
/// `Default` is everything off, which is what every popup opens with. The time
/// is an absolute bound so this struct never reads the clock; the popup turns
/// "今天/近7天" into that bound at query time.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct ChipFilter {
    /// Bare terms also read the exe name the clip was copied from.
    pub in_app: bool,
    /// Bare terms also read the window title at capture time.
    pub in_title: bool,
    pub kind: Option<ClipKind>,
    pub since_ms: Option<i64>,
}

impl ChipFilter {
    /// Whether one item passes the two filters. Off is the common state, and two
    /// `None`s answer without touching the row. The boxes do not appear here on
    /// purpose: they are not a condition a row can fail, only more places to look.
    fn allows(&self, item: &ClipItem) -> bool {
        self.kind.is_none_or(|kind| item.kind == kind)
            && self.since_ms.is_none_or(|since| item.at >= since)
    }
}

/// Splits the search box into terms: whitespace-separated, each one either a bare
/// word or `field:value`.
///
/// A prefix that is not one of the known names is left in the term, so a clip that
/// says `12:30` or `http://…` is still found by what it says rather than being read
/// as a field nobody has. A Chinese IME turns the colon full-width — `app：chrome`
/// — and a term that then read as plain text would find nothing where a field was
/// meant, so both spellings name a field. The values are already lowercased by the
/// caller, which is what the case-insensitive compare below wants.
fn parse_query(raw: &str) -> Vec<(Field, String)> {
    raw.split_whitespace()
        .map(|token| {
            match token
                .split_once(':')
                .or_else(|| token.split_once('：'))
                .and_then(|(name, value)| field_of(name).map(|field| (field, value.to_string())))
            {
                Some(term) => term,
                None => (Field::Text, token.to_string()),
            }
        })
        .collect()
}

fn field_of(name: &str) -> Option<Field> {
    Some(match name {
        "time" => Field::When,
        "machine" => Field::Machine,
        "app" => Field::App,
        "title" => Field::Title,
        "kind" => Field::Kind,
        _ => return None,
    })
}

/// Every term has to match: `foo bar` is what says both, and `kind:image foo` is the
/// images that say foo.
fn matches(item: &ClipItem, terms: &[(Field, String)], chips: &ChipFilter) -> bool {
    terms
        .iter()
        .all(|(field, value)| matches_term(item, *field, value, chips))
}

/// One term against one item. Two fields answer to more than one spelling: the
/// machine to what the row shows (`本机`) as well as to its id, and the kind to
/// `图片` as well as to `image`.
fn matches_term(item: &ClipItem, field: Field, needle: &str, chips: &ChipFilter) -> bool {
    let hits = |haystack: &str| contains_ignore_case(haystack, needle);

    match field {
        // A bare term reads the clip's text; the source fields join in when their
        // boxes are checked, and together they are one OR — the term is found in
        // any of the places the scope covers. Qualified terms (`app:` `title:`)
        // do not come through here, so the boxes never narrow a search.
        Field::Text => {
            hits(&item.text)
                || (chips.in_app && hits(&item.app))
                || (chips.in_title && hits(&item.title))
        }
        Field::When => hits(&item.when),
        Field::Machine => hits(&item.machine) || hits(&item.who),
        Field::App => hits(&item.app),
        Field::Title => hits(&item.title),
        Field::Kind => hits(item.kind.label()) || hits(item.kind.name()),
    }
}

/// ASCII-case-insensitive substring search with no allocation.
///
/// Byte-wise is safe here: UTF-8 continuation bytes all have the high bit set,
/// so a non-ASCII byte can never compare equal to an ASCII needle byte, and CJK
/// passes through untouched.
fn contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }

    let hay = haystack.as_bytes();
    let needle = needle_lower.as_bytes();
    if needle.len() > hay.len() {
        return false;
    }

    let first = needle[0];
    let last_start = hay.len() - needle.len();

    let mut index = 0;
    while index <= last_start {
        if hay[index].to_ascii_lowercase() == first
            && hay[index..index + needle.len()]
                .iter()
                .zip(needle)
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
            return true;
        }
        index += 1;
    }

    false
}

/// What one row shows above the meta line.
///
/// A row is one line tall, so a multi-line clip cannot show any of its other
/// lines — the marker in front is the only way it can say that there are some.
/// It goes in front rather than at the end because a long first line is exactly
/// when it matters, and an ellipsis would eat a suffix.
fn build_preview(kind: ClipKind, text: &str, has_blob: bool) -> String {
    if kind == ClipKind::Image {
        return "[图片]".to_string();
    }

    // The first line with anything in it: a clip that opens with a blank line
    // still has to show what it says.
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");

    if kind == ClipKind::Files {
        let count = text.lines().filter(|line| !line.trim().is_empty()).count();
        return format!("[文件 x{count}] {first}");
    }

    if first.is_empty() {
        return if text.lines().count() > 1 {
            "(多行空文本)".to_string()
        } else {
            "(空文本)".to_string()
        };
    }

    let marker = match text.lines().count() {
        0 | 1 => String::new(),
        // Only the head of an over-long clip is kept inline, so a count of what
        // is here would be a count of the head, not of what was copied.
        _ if has_blob => "[多行] ".to_string(),
        lines => format!("[{lines} 行] "),
    };

    format!("{marker}{}", truncate_chars(first, PREVIEW_CHARS))
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }

    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

/// The timestamp a row carries: `MM-DD hh:mm`, with the year in front of it only
/// when the clip is not from this one. Those four characters come out of the source
/// column on every row, and the list is in time order — a timestamp without a year
/// is from this year, and one with a year is old enough that `01-05` alone would
/// read as five days ago.
fn format_when(at: i64, now_year: u16) -> String {
    let when = win::local_datetime(at);

    if when.year == now_year {
        format!(
            "{:02}-{:02} {:02}:{:02}",
            when.month, when.day, when.hour, when.minute
        )
    } else {
        format!(
            "{}-{:02}-{:02} {:02}:{:02}",
            when.year, when.month, when.day, when.hour, when.minute
        )
    }
}

/// The row's second line, held as its parts.
///
/// One function builds both, so the line and the searchable fields cannot drift:
/// the line is what gets painted, and the parts are what the qualified search terms
/// read — `app:` finds an application, `title:` a window title, never each other.
struct Meta {
    label: &'static str,
    when: String,
    who: String,
    app: String,
    title: String,
    blob_note: &'static str,
}

impl Meta {
    fn of(
        record: &ClipRecord,
        kind: ClipKind,
        has_blob: bool,
        local_machine: &str,
        now_year: u16,
    ) -> Meta {
        let who = if record.machine.is_empty() {
            "?".to_string()
        } else if record.machine == local_machine {
            "本机".to_string()
        } else {
            record.machine.clone()
        };

        Meta {
            label: kind.label(),
            when: format_when(record.at, now_year),
            who,
            app: record.app.clone(),
            title: record.title.clone(),
            blob_note: if has_blob { " · 完整内容在 .bin" } else { "" },
        }
    }

    /// The line the row shows.
    fn text(&self) -> String {
        let Meta {
            label,
            when,
            who,
            app,
            title,
            blob_note,
        } = self;

        // The window it came out of, when there was one to read: which application,
        // and what that window said. A clip from before this was recorded — or from a
        // window this process could not read — simply stops after the machine, and
        // the line then reads exactly as it always did.
        let mut source = String::new();
        for part in [app, title] {
            if !part.is_empty() {
                source.push_str(" · ");
                source.push_str(part);
            }
        }

        format!("{label} · {when} · {who}{source}{blob_note}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(stem: &str, machine: &str, at: i64) -> ClipItem {
        let record = ClipRecord {
            id: stem.to_string(),
            at,
            machine: machine.to_string(),
            kind: "text".to_string(),
            hash: format!("hash-{stem}"),
            text: Some(format!("clip {stem}")),
            length: 8,
            blob: None,
            app: String::new(),
            title: String::new(),
        };

        ClipItem::from_record(
            &record,
            PathBuf::from("C:/sync/clips.db"),
            stem.to_string(),
            false,
            "local",
            crate::settings::current_year(),
        )
    }

    /// The same, but with everything the row's second line is built from filled in:
    /// the fields the qualified search terms read. `machine` is who captured it;
    /// the chips feed off it when the app menu has to match the tab that is open.
    fn sourced(
        stem: &str,
        machine: &str,
        at: i64,
        kind: &str,
        text: &str,
        app: &str,
        title: &str,
    ) -> ClipItem {
        let record = ClipRecord {
            id: stem.to_string(),
            at,
            machine: machine.to_string(),
            kind: kind.to_string(),
            hash: format!("hash-{stem}"),
            text: Some(text.to_string()),
            length: text.len() as i64,
            blob: None,
            app: app.to_string(),
            title: title.to_string(),
        };

        ClipItem::from_record(
            &record,
            PathBuf::from("C:/sync/clips.db"),
            stem.to_string(),
            false,
            "local",
            crate::settings::current_year(),
        )
    }

    /// The search box reads the clip's own text unless a term names a field, and the
    /// fields stay apart from each other: `app:chrome` cannot reach a window title and
    /// `title:` cannot reach an application. A prefix nobody knows stays a word, so a
    /// clip that says `12:30` is still found by what it says.
    #[test]
    fn the_filter_reads_text_unless_a_term_names_a_field() {
        let mut index = Index::default();
        // Distinct timestamps, newest first: the list is in time order, so this is the
        // order the rows come back in.
        index.insert(sourced("a", "local", 3_000, "text", "hello world", "chrome.exe", "Inbox"));
        index.insert(sourced(
            "b",
            "local",
            2_000,
            "image",
            "",
            "paint.exe",
            "chrome.exe — untitled",
        ));
        index.insert(sourced("c", "local", 1_000, "text", "meeting at 12:30", "teams.exe", "Calendar"));

        // The same lowercasing the store does before it hands the filter over.
        let hits = |needle: &str| {
            let needle = needle.trim().to_ascii_lowercase();
            index
                .query(None, Some(&needle), &ChipFilter::default(), 10)
                .into_iter()
                .map(|summary| summary.stem)
                .collect::<Vec<_>>()
        };

        // Content only: the app name on another row's title is not content.
        assert_eq!(hits("hello"), vec!["a"]);
        assert_eq!(hits("chrome"), Vec::<String>::new());

        // Words are separate terms now, so order and distance stop mattering.
        assert_eq!(hits("world hello"), vec!["a"]);
        assert_eq!(hits("hello nope"), Vec::<String>::new());

        // One prefix per field, and each one reads only its own part.
        assert_eq!(hits("app:chrome"), vec!["a"]);
        assert_eq!(hits("title:chrome"), vec!["b"]);
        assert_eq!(hits("title:inbox"), vec!["a"]);
        assert_eq!(hits("kind:image"), vec!["b"]);
        assert_eq!(hits("kind:图片"), vec!["b"]);
        assert_eq!(hits("app:paint kind:image"), vec!["b"]);

        // The machine answers to what the row shows as well as to its id, and the
        // time to what the row prints.
        assert_eq!(hits("machine:本机"), vec!["a", "b", "c"]);
        assert_eq!(hits("machine:local"), vec!["a", "b", "c"]);

        // A colon in the text is text: the prefix is only a prefix when it names a
        // field this program has.
        assert_eq!(hits("12:30"), vec!["c"]);
        assert_eq!(hits("meeting"), vec!["c"]);

        // The full-width colon a Chinese IME types is the same prefix, not a
        // term that silently finds nothing.
        assert_eq!(hits("app：chrome"), vec!["a"]);
        assert_eq!(hits("kind：图片"), vec!["b"]);
        // …and a full-width colon in the *text* stays a text term too: no field
        // is named `12`, so the term is what the clip has to say — and clip c
        // says `12:30` half-width, which is not the same bytes.
        assert_eq!(hits("12：30"), Vec::<String>::new());
    }

    /// A tombstone is how one machine asks another to drop a clip it may not delete
    /// itself. Two things have to hold for that to work: the row goes off every list
    /// at once, and its hash stops answering the capture path — otherwise copying that
    /// content again would be "already stored" and then show up nowhere.
    #[test]
    fn a_tombstoned_clip_is_off_the_list_and_off_the_hash_set() {
        let mut index = Index::default();
        index.insert(item("a", "local", 300));
        index.insert(item("b", "local", 200));
        index.insert(item("c", "other", 100));

        let hidden: HashSet<String> = ["b"].iter().map(|stem| stem.to_string()).collect();
        index.hide_many(&hidden);

        let visible = |index: &Index| {
            index
                .query(None, None, &ChipFilter::default(), 10)
                .into_iter()
                .map(|row| row.stem)
                .collect::<Vec<_>>()
        };

        assert_eq!(visible(&index), vec!["a", "c"]);
        assert!(!index.has_hash("hash-b"));

        // The machine that owns the row works from here, and it leaves a pinned clip
        // alone: a pin is a promise that this clip survives cleanup.
        assert_eq!(index.hidden_clips("local").len(), 1);
        index.set_pinned("b", true);
        assert!(index.hidden_clips("local").is_empty());
        index.set_pinned("b", false);

        // A marker somebody removed by hand is a clip that comes back.
        assert!(index.set_hidden("b", false));
        assert_eq!(visible(&index), vec!["a", "b", "c"]);
        assert!(index.has_hash("hash-b"));
    }

    /// Re-copying a clip has to find the copy already stored, and the newest one
    /// at that: that is the row a re-copy re-dates. A tombstoned copy does not
    /// answer for the content — the same visibility rule `has_hash` goes by — so
    /// the two can never disagree about whether a capture is a re-copy.
    #[test]
    fn newest_by_hash_answers_the_newest_visible_copy() {
        let mut index = Index::default();
        let mut oldest = item("c", "local", 100);
        oldest.hash = "shared".to_string();
        let mut newer = item("b", "other", 200);
        newer.hash = "shared".to_string();
        let mut newest = item("a", "local", 300);
        newest.hash = "shared".to_string();
        index.insert(oldest);
        index.insert(newer);
        index.insert(newest);

        assert_eq!(index.newest_by_hash("shared").unwrap().stem, "a");

        // The newest copy sits behind a tombstone: it does not count, and the
        // answer falls to the copy that can still be seen.
        let hidden: HashSet<String> = ["a"].iter().map(|stem| stem.to_string()).collect();
        index.hide_many(&hidden);
        assert_eq!(index.newest_by_hash("shared").unwrap().stem, "b");

        assert!(index.newest_by_hash("hash-x").is_none());
    }

    /// The instance strip and the per-instance filter are what the popup tabs
    /// are built on: one entry per machine, newest activity first, and a tab
    /// that lists only its own clips.
    #[test]
    fn machines_are_listed_once_newest_first() {
        let mut index = Index::default();
        index.insert(item("a1", "aaa", 100));
        index.insert(item("b1", "bbb", 300));
        index.insert(item("a2", "aaa", 200));

        let machines: Vec<String> = index.machines().into_iter().map(|(id, _)| id).collect();
        assert_eq!(machines, vec!["bbb".to_string(), "aaa".to_string()]);

        let mine: Vec<String> = index
            .query(Some("aaa"), None, &ChipFilter::default(), 10)
            .into_iter()
            .map(|summary| summary.stem)
            .collect();
        assert_eq!(mine, vec!["a2".to_string(), "a1".to_string()]);

        assert_eq!(
            index
                .query(None, None, &ChipFilter::default(), 10)
                .len(),
            3
        );
        assert!(index
            .query(Some("bbb"), Some("clip a"), &ChipFilter::default(), 10)
            .is_empty());
    }

    /// The two checkboxes beside the search box are scope, not filters: checked,
    /// a bare term also looks in the exe name and the window title. The row has to
    /// pass only its text — a row matching through a widened source is found, and
    /// a search can only get wider than it was. Qualified terms (`app:` `title:`)
    /// never needed the boxes and do not change under them.
    #[test]
    fn checked_boxes_widen_where_bare_terms_look() {
        let mut index = Index::default();
        index.insert(sourced("a", "local", 3_000, "text", "hello", "chrome.exe", "Inbox"));
        index.insert(sourced("b", "local", 2_000, "text", "world", "paint.exe", "untitled page"));
        // An image has no text at all: with a box checked its source is the only
        // place a bare term can find it.
        index.insert(sourced("c", "local", 1_000, "image", "", "code.exe", "chrome.exe — editor"));

        let stems = |needle: &str, chips: ChipFilter| -> Vec<String> {
            let needle = needle.to_ascii_lowercase();
            index
                .query(None, Some(&needle), &chips, 10)
                .into_iter()
                .map(|summary| summary.stem)
                .collect()
        };

        // Boxes off: `chrome` sits on two rows and must not be found — this is
        // the behaviour everything before the boxes had.
        assert_eq!(stems("chrome", ChipFilter::default()), Vec::<String>::new());

        // 应用 checked: bare terms reach the exe name…
        assert_eq!(
            stems(
                "chrome",
                ChipFilter {
                    in_app: true,
                    ..Default::default()
                }
            ),
            vec!["a"]
        );
        assert_eq!(
            stems(
                "paint",
                ChipFilter {
                    in_app: true,
                    ..Default::default()
                }
            ),
            vec!["b"]
        );

        // …标题 checked, instead: row a's title says Inbox, row c's says chrome —
        // the same word, found in a different place.
        assert_eq!(
            stems(
                "chrome",
                ChipFilter {
                    in_title: true,
                    ..Default::default()
                }
            ),
            vec!["c"]
        );
        assert_eq!(
            stems(
                "untitled",
                ChipFilter {
                    in_title: true,
                    ..Default::default()
                }
            ),
            vec!["b"]
        );

        // The two boxes are one OR over the places a term may live…
        assert_eq!(
            stems(
                "chrome",
                ChipFilter {
                    in_app: true,
                    in_title: true,
                    ..Default::default()
                }
            ),
            vec!["a", "c"]
        );
        // …and still an AND across terms: the second word has to hit somewhere too.
        assert_eq!(
            stems(
                "chrome editor",
                ChipFilter {
                    in_app: true,
                    in_title: true,
                    ..Default::default()
                }
            ),
            vec!["c"]
        );

        // The qualified spellings work exactly as before, boxes or no boxes.
        assert_eq!(stems("app:chrome", ChipFilter::default()), vec!["a"]);
        assert_eq!(stems("title:untitled", ChipFilter::default()), vec!["b"]);
    }

    /// The two menus are real filters, unlike the boxes: a kind or a time bound a
    /// row can fail, chips combine as two conditions, and they AND with whatever
    /// terms the box holds — a filter that cannot reach the rows a term matched
    /// only pretends to work.
    #[test]
    fn menu_chips_narrow_the_list() {
        let mut index = Index::default();
        index.insert(sourced("a", "local", 3_000, "text", "hello", "chrome.exe", "Inbox"));
        index.insert(sourced("b", "local", 2_000, "image", "", "paint.exe", "untitled"));
        index.insert(sourced("c", "local", 1_000, "text", "world", "chrome.exe", "Docs"));

        let stems = |chips: ChipFilter| -> Vec<String> {
            index
                .query(None, None, &chips, 10)
                .into_iter()
                .map(|summary| summary.stem)
                .collect()
        };

        assert_eq!(
            stems(ChipFilter {
                kind: Some(ClipKind::Image),
                ..Default::default()
            }),
            vec!["b"]
        );
        // Inclusive bound: the clip captured exactly at midnight is in.
        assert_eq!(
            stems(ChipFilter {
                since_ms: Some(2_000),
                ..Default::default()
            }),
            vec!["a", "b"]
        );
        // Two chips are two conditions, not two answers.
        assert_eq!(
            stems(ChipFilter {
                kind: Some(ClipKind::Image),
                since_ms: Some(2_500),
                ..Default::default()
            }),
            Vec::<String>::new()
        );

        // A filter also has to survive terms — and cut them down.
        let typed = |chips: ChipFilter| -> Vec<String> {
            index
                .query(None, Some("hello"), &chips, 10)
                .into_iter()
                .map(|summary| summary.stem)
                .collect()
        };
        assert_eq!(
            typed(ChipFilter {
                kind: Some(ClipKind::Text),
                ..Default::default()
            }),
            vec!["a"]
        );
        assert!(typed(ChipFilter {
            kind: Some(ClipKind::Image),
            ..Default::default()
        })
        .is_empty());
    }

    /// `insert_many` is the load path — one merge for a whole month instead of one
    /// shift per row — so it has to land the rows exactly where `insert` would,
    /// including two clips captured in the same millisecond.
    #[test]
    fn batch_insert_and_forget_match_one_at_a_time() {
        let batch = || {
            vec![
                item("a", "aaa", 100),
                item("b", "aaa", 300),
                item("c", "aaa", 200),
                item("d", "aaa", 200),
                item("e", "aaa", 400),
            ]
        };

        let mut one_at_a_time = Index::default();
        for entry in batch() {
            assert!(one_at_a_time.insert(entry));
        }

        let mut merged = Index::default();
        merged.insert_many(batch());

        fn order(index: &Index) -> Vec<String> {
            index
                .query(None, None, &ChipFilter::default(), 10)
                .into_iter()
                .map(|row| row.stem)
                .collect()
        }

        assert_eq!(order(&merged), order(&one_at_a_time));

        // A stem that is already known is skipped here too, which is what keeps a
        // reload of a machine's own month from doubling its rows.
        let mut again = Index::default();
        again.insert(item("a", "aaa", 100));
        again.insert_many(batch());
        assert_eq!(
            again
                .query(None, None, &ChipFilter::default(), 10)
                .len(),
            5
        );

        // And the batch forget takes exactly those rows back out, newest first.
        let stems: HashSet<String> = ["a", "c"]
            .iter()
            .map(|stem| stem.to_string())
            .collect();
        merged.forget_many(&stems);
        assert_eq!(order(&merged), vec!["e", "b", "d"]);
    }

    /// A row is one line tall, so the preview is the only place that can say a clip
    /// has more lines than the one it shows. Each case gets a look: one line,
    /// several, only blanks, and the over-long clip that keeps just its head inline.
    #[test]
    fn preview_marks_what_does_not_fit() {
        assert_eq!(build_preview(ClipKind::Text, "one line", false), "one line");
        assert_eq!(build_preview(ClipKind::Text, "a\nb", false), "[2 行] a");
        assert_eq!(
            build_preview(ClipKind::Text, "\n\nfirst\nsecond", false),
            "[4 行] first"
        );
        assert_eq!(build_preview(ClipKind::Text, "a\nb", true), "[多行] a");
        assert_eq!(build_preview(ClipKind::Text, "\n\n\n", false), "(多行空文本)");
        assert_eq!(build_preview(ClipKind::Text, "", false), "(空文本)");
        assert_eq!(build_preview(ClipKind::Image, "", false), "[图片]");
        assert_eq!(
            build_preview(ClipKind::Files, "C:\\a.txt\nC:\\b.txt", false),
            "[文件 x2] C:\\a.txt"
        );
    }

    /// The timestamp on a row has no year on purpose, which is only readable because
    /// the list is in time order. What must not happen is the other way round: a clip
    /// from a previous year keeping the short form, where `01-05` reads as five days
    /// ago rather than a year and a month.
    #[test]
    fn the_year_is_written_only_when_it_is_not_this_one() {
        const DAY: i64 = 24 * 60 * 60 * 1000;

        let now = crate::settings::now_ms();
        let this_year = crate::settings::current_year();
        let when = win::local_datetime(now);

        assert_eq!(
            format_when(now, this_year),
            format!(
                "{:02}-{:02} {:02}:{:02}",
                when.month, when.day, when.hour, when.minute
            )
        );

        // 400 days back is a previous calendar year whichever side of New Year this
        // happens to run on, and it is still the short form that has to change.
        let then = now - 400 * DAY;
        let when = win::local_datetime(then);
        assert!(when.year < this_year);

        assert_eq!(
            format_when(then, this_year),
            format!(
                "{}-{:02}-{:02} {:02}:{:02}",
                when.year, when.month, when.day, when.hour, when.minute
            )
        );
    }
}
