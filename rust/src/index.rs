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
}

/// What the list needs to render one row. Deliberately small: the full text can
/// be kilobytes, and this is cloned per keystroke.
#[derive(Clone, Debug)]
pub struct ClipSummary {
    pub stem: String,
    pub preview: String,
    pub meta: String,
    pub pinned: bool,
}

impl ClipItem {
    pub fn from_record(
        record: &ClipRecord,
        db_path: PathBuf,
        stem: String,
        pinned: bool,
        local_machine: &str,
    ) -> ClipItem {
        let kind = ClipKind::from_name(&record.kind);
        let text = record.text.clone().unwrap_or_default();
        let has_blob = record.blob.is_some();

        let blob_path = match &record.blob {
            Some(name) => db_path.with_file_name(name),
            None => PathBuf::new(),
        };

        let preview = build_preview(kind, &text);
        let meta = build_meta(kind, record.at, &record.machine, has_blob, local_machine);

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
            meta,
        }
    }

    pub fn pin_path(&self) -> PathBuf {
        pin_path_for(&self.db_path, &self.stem)
    }

    pub fn summary(&self) -> ClipSummary {
        ClipSummary {
            stem: self.stem.clone(),
            preview: self.preview.clone(),
            meta: self.meta.clone(),
            pinned: self.pinned,
        }
    }
}

/// `<stem>.pin`, in the database's own folder. Pinning adds a sibling marker
/// rather than rewriting the row, so it costs nothing and cannot conflict.
pub fn pin_path_for(db_path: &std::path::Path, stem: &str) -> PathBuf {
    let folder = db_path.parent().unwrap_or(std::path::Path::new(""));
    folder.join(format!("{stem}{}", crate::store::PIN_SUFFIX))
}

#[derive(Default)]
pub struct Index {
    /// Descending by `at`, maintained by binary insertion so a query never sorts.
    items: Vec<ClipItem>,
    stems: HashSet<String>,
    hashes: HashSet<String>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.items.len()
    }


    pub fn has_hash(&self, hash: &str) -> bool {
        self.hashes.contains(hash)
    }

    pub fn find(&self, stem: &str) -> Option<&ClipItem> {
        self.items.iter().find(|item| item.stem == stem)
    }

    /// Returns false when the stem was already known.
    pub fn insert(&mut self, item: ClipItem) -> bool {
        if !self.stems.insert(item.stem.clone()) {
            return false;
        }

        if !item.hash.is_empty() {
            self.hashes.insert(item.hash.clone());
        }

        let position = self
            .items
            .partition_point(|existing| existing.at > item.at);
        self.items.insert(position, item);
        true
    }

    pub fn forget(&mut self, stem: &str) -> Option<ClipItem> {
        let position = self.items.iter().position(|item| item.stem == stem)?;
        let item = self.items.remove(position);
        self.stems.remove(&item.stem);

        // Only drop the hash when no other clip still carries it: the same
        // content can legitimately have been stored by a second machine.
        if !self.items.iter().any(|other| other.hash == item.hash) {
            self.hashes.remove(&item.hash);
        }

        Some(item)
    }

    /// Drops every entry that came out of one container. Used when a database
    /// changed (its rows are re-read from scratch) or disappeared.
    pub fn forget_db(&mut self, db_path: &std::path::Path) {
        let doomed: Vec<String> = self
            .items
            .iter()
            .filter(|item| item.db_path == db_path)
            .map(|item| item.stem.clone())
            .collect();

        for stem in doomed {
            self.forget(&stem);
        }
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

    /// Every instance that has a clip here, newest activity first. One pass: the
    /// items are already sorted by time, so the first clip seen for an instance
    /// is its newest.
    pub fn machines(&self) -> Vec<(String, i64)> {
        let mut out: Vec<(String, i64)> = Vec::new();

        for item in &self.items {
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

    /// Pinned rows first, then newest first. Two passes rather than a sort.
    pub fn query(
        &self,
        machine: Option<&str>,
        needle: Option<&str>,
        limit: usize,
    ) -> Vec<ClipSummary> {
        let mut out = Vec::with_capacity(limit.min(self.items.len()));

        for want_pinned in [true, false] {
            for item in &self.items {
                if item.pinned != want_pinned {
                    continue;
                }

                if let Some(machine) = machine {
                    if item.machine != machine {
                        continue;
                    }
                }

                if let Some(needle) = needle {
                    if !matches(item, needle) {
                        continue;
                    }
                }

                out.push(item.summary());
                if out.len() >= limit {
                    return out;
                }
            }
        }

        out
    }
}

fn matches(item: &ClipItem, needle_lower: &str) -> bool {
    contains_ignore_case(&item.text, needle_lower) || contains_ignore_case(&item.meta, needle_lower)
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

fn build_preview(kind: ClipKind, text: &str) -> String {
    if kind == ClipKind::Image {
        return "[图片]".to_string();
    }

    let first_line = text.lines().next().unwrap_or("").trim();

    if kind == ClipKind::Files {
        let count = text.lines().filter(|line| !line.trim().is_empty()).count();
        return format!("[文件 x{count}] {first_line}");
    }

    if first_line.is_empty() {
        return if text.contains('\n') {
            "(多行文本)".to_string()
        } else {
            "(空文本)".to_string()
        };
    }

    truncate_chars(first_line, PREVIEW_CHARS)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }

    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

fn build_meta(
    kind: ClipKind,
    at: i64,
    machine: &str,
    has_blob: bool,
    local_machine: &str,
) -> String {
    let label = match kind {
        ClipKind::Image => "图片",
        ClipKind::Files => "文件",
        ClipKind::Text => "文本",
    };

    let when = win::local_datetime(at);
    let time = format!(
        "{:02}-{:02} {:02}:{:02}",
        when.month, when.day, when.hour, when.minute
    );

    let who = if machine.is_empty() {
        "?".to_string()
    } else if machine == local_machine {
        "本机".to_string()
    } else {
        machine.to_string()
    };

    if has_blob {
        format!("{label} · {time} · {who} · 完整内容在 .bin")
    } else {
        format!("{label} · {time} · {who}")
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
        };

        ClipItem::from_record(
            &record,
            PathBuf::from("C:/sync/clips.db"),
            stem.to_string(),
            false,
            "local",
        )
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
            .query(Some("aaa"), None, 10)
            .into_iter()
            .map(|summary| summary.stem)
            .collect();
        assert_eq!(mine, vec!["a2".to_string(), "a1".to_string()]);

        assert_eq!(index.query(None, None, 10).len(), 3);
        assert!(index.query(Some("bbb"), Some("clip a"), 10).is_empty());
    }
}
