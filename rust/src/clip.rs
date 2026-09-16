//! Clipboard payloads and the row schema.
//!
//! `ClipRecord` *is* one row of a synced database: it is built when a clip is
//! `ClipRecord` is what one clip looks like in a synced database: it is built
//! when a clip is captured and rebuilt when a database is read back. Its field
//! names are the column names in the SQL — the ones for the source window live in
//! the `context` table beside `clips` (see `store::CONTEXT_SCHEMA`) — so renaming
//! one means changing both.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum ClipKind {
    Text = 0,
    Image = 1,
    Files = 2,
}

impl ClipKind {
    /// The `kind` column. The discriminants above are mixed into the content
    /// hash as well, so both halves of this type are already in the databases.
    pub fn name(self) -> &'static str {
        match self {
            ClipKind::Text => "text",
            ClipKind::Image => "image",
            ClipKind::Files => "files",
        }
    }

    /// Turns the `kind` column back into an enum. An unknown value reads as
    /// text, which is the harmless choice for a hand-edited database.
    pub fn from_name(name: &str) -> ClipKind {
        match name {
            "image" => ClipKind::Image,
            "files" => ClipKind::Files,
            _ => ClipKind::Text,
        }
    }
}

/// A captured clipboard payload, before it is assigned an id or written.
#[derive(Clone, Debug)]
pub enum ClipPayload {
    Text(String),
    /// Newline-joined absolute paths.
    Files(Vec<String>),
    /// PNG bytes.
    Image(Vec<u8>),
}

impl ClipPayload {
    pub fn kind(&self) -> ClipKind {
        match self {
            ClipPayload::Text(_) => ClipKind::Text,
            ClipPayload::Files(_) => ClipKind::Files,
            ClipPayload::Image(_) => ClipKind::Image,
        }
    }

    /// The exact bytes the content hash is computed over. Every machine has to
    /// feed in the same bytes, or the same clip gets stored once per machine.
    pub fn body(&self) -> Vec<u8> {
        match self {
            ClipPayload::Text(text) => text.as_bytes().to_vec(),
            ClipPayload::Files(paths) => paths.join("\n").into_bytes(),
            ClipPayload::Image(png) => png.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ClipPayload::Text(text) => text.is_empty(),
            ClipPayload::Files(paths) => paths.is_empty(),
            ClipPayload::Image(png) => png.is_empty(),
        }
    }
}

/// Where a clip came from: the window that had focus while it was copied.
///
/// Best-effort by nature — a window with no title, or one belonging to a process
/// this one cannot open, yields an empty string. Losing the context must never
/// cost the clip itself, so there is no failure path here at all.
#[derive(Clone, Debug, Default)]
pub struct ClipContext {
    /// Executable name, e.g. `chrome.exe`. The folder it came from says nothing
    /// a reader wants.
    pub app: String,
    pub title: String,
}

/// One clip: what was copied, when, by which machine, and out of which window.
#[derive(Clone, Debug)]
pub struct ClipRecord {
    /// Row key, and the stem of the `.bin` and `.pin` siblings.
    pub id: String,
    pub at: i64,
    pub machine: String,
    pub kind: String,
    pub hash: String,

    /// Full text, or the retained prefix when `blob` is set.
    pub text: Option<String>,

    /// Payload size in bytes, for display only.
    pub length: i64,

    /// Sibling file name holding the heavy payload (image bytes, long text).
    pub blob: Option<String>,

    /// The source window, as read at capture time. Empty when it could not be
    /// read, and empty for every clip recorded before it was recorded at all —
    /// which is why the display has to cope with both being empty.
    pub app: String,
    pub title: String,

/// Several clips as one clipboard payload: one per line, in the order they are
/// listed.
///
/// A single clip is handed back untouched, so an image still copies as an image.
/// With several, text is the only representation that can hold all of them: file
/// lists contribute their paths, and images are dropped (the caller logs that).
pub fn join_payloads(payloads: Vec<ClipPayload>) -> Option<ClipPayload> {
    if payloads.len() == 1 {
        return payloads.into_iter().next();
    }

    let mut lines: Vec<String> = Vec::new();
    for payload in payloads {
        match payload {
            ClipPayload::Text(text) => lines.push(text),
            ClipPayload::Files(paths) => lines.extend(paths),
            ClipPayload::Image(_) => {}
        }
    }

    if lines.is_empty() {
        return None;
    }

    Some(ClipPayload::Text(lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Multi-select copies become one text block, one clip per line; a single
    /// clip keeps its own type so images still paste as images.
    #[test]
    fn several_clips_join_with_newlines() {
        let payloads = vec![
            ClipPayload::Text("一".to_string()),
            ClipPayload::Files(vec!["C:\\a.txt".to_string(), "C:\\b.txt".to_string()]),
            ClipPayload::Text("二".to_string()),
            ClipPayload::Image(vec![1, 2, 3]),
        ];

        match join_payloads(payloads) {
            Some(ClipPayload::Text(text)) => {
                assert_eq!(text, "一\nC:\\a.txt\nC:\\b.txt\n二")
            }
            other => panic!("expected a text payload, got {other:?}"),
        }

        assert!(matches!(
            join_payloads(vec![ClipPayload::Image(vec![1, 2, 3])]),
            Some(ClipPayload::Image(_))
        ));
        assert!(join_payloads(vec![ClipPayload::Image(vec![1]), ClipPayload::Image(vec![2])]).is_none());
        assert!(join_payloads(Vec::new()).is_none());
    }
}
