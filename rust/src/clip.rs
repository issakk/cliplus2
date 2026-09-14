//! Clipboard payloads and the row schema.
//!
//! `ClipRecord` *is* one row of a synced database: it is built when a clip is
//! captured and rebuilt when a database is read back. Its field names are the
//! column names in the SQL, so renaming one means changing both.

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

/// One clip: what was copied, when, and by which machine.
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
}
