//! Clipboard payloads and the on-disk record schema.
//!
//! `ClipRecord`'s field names *are* the file format: `serde_json` writes them
//! verbatim, exactly like the C# build's `System.Text.Json`, so both versions
//! read and write the same `.clip.json`. Do not rename a field without a
//! migration that every other machine can follow.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum ClipKind {
    Text = 0,
    Image = 1,
    Files = 2,
}

impl ClipKind {
    pub fn name(self) -> &'static str {
        match self {
            ClipKind::Text => "text",
            ClipKind::Image => "image",
            ClipKind::Files => "files",
        }
    }

    /// Used when reading existing records back off disk.
    #[allow(dead_code)]
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
    /// Newline-joined absolute paths, the same shape the C# build stored.
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

    /// The exact bytes the content hash is computed over. Must stay identical to
    /// the C# build's `Blob ?? UTF8(text)` for cross-version dedupe to work.
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

/// On-disk schema, version 1. Field order matches the C# build only for
/// readability; nothing depends on it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipRecord {
    pub v: u32,
    pub id: String,
    pub at: i64,
    pub machine: String,
    pub kind: String,
    pub hash: String,

    /// Full text, or a truncated prefix when `blob` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    pub truncated: bool,

    /// Payload size in bytes, for display only.
    pub length: i64,

    /// Sibling file name holding the heavy payload (image bytes, long text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}
