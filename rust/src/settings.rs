//! Persisted configuration, the machine id, and path resolution.
//!
//! The JSON keys and `machine.id` are never renamed: both are already on disk on
//! any machine that ran an earlier build, and keeping the id means this build
//! keeps writing into the machine shard its history already lives in.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::log;
use crate::win;

const SETTINGS_FILE: &str = "settings.json";
const MACHINE_ID_FILE: &str = "machine.id";

pub fn app_dir() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    base.join("ClipPlus")
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The local year, which is what a row's timestamp is compared against to decide
/// whether it has to spell the year out.
pub fn current_year() -> u16 {
    win::local_year_month(now_ms()).0
}

/// `<local yyyy-MM>`: local time, because that is how the folders already on disk
pub fn month_bucket(ms: i64) -> String {
    let (year, month) = win::local_year_month(ms);
    format!("{year:04}-{month:02}")
}

static STEM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// File stem for a new clip. Opaque to every reader — `at` inside the JSON
/// carries the timestamp — so the exact shape only has to be unique.
pub fn unique_stem(ms: i64) -> String {
    let counter = STEM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);

    format!(
        "{ms}-{:032x}",
        (nanos << 32) ^ counter ^ (std::process::id() as u64)
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(rename = "Hotkey")]
    pub hotkey: String,

    #[serde(rename = "MaxBlobBytes")]
    pub max_blob_bytes: u64,

    #[serde(rename = "InlineTextLimit")]
    pub inline_text_limit: usize,

    #[serde(rename = "RescanSeconds")]
    pub rescan_seconds: u64,

    /// Percent: the user's own multiplier for the settings window's controls, on
    /// top of the display's DPI. The popup is a fixed-density list and deliberately
    /// does not follow this.
    #[serde(rename = "SettingsScale")]
    pub settings_scale: u32,

    /// Where the popup was last left, in screen coordinates. `None` until it is
    /// dragged somewhere: the first open has nothing to go on and centres itself
    /// on whichever monitor the cursor is on. Written by the popup, not by the
    /// settings window, which only ever saves the fields it shows.
    #[serde(rename = "PopupPosition")]
    pub popup_position: Option<(i32, i32)>,

    /// How big the popup was last left, in logical pixels at 96 DPI — the units its
    /// own constants are written in, so a monitor with a different scale gets a popup
    /// of the size it would have had rather than the one measured before. `None`
    /// until it is stretched, and the popup's own default size until then. Written by
    /// the popup, not by the settings window.
    #[serde(rename = "PopupSize")]
    pub popup_size: Option<(i32, i32)>,

    #[serde(rename = "CaptureText")]
    pub capture_text: bool,

    #[serde(rename = "CaptureImages")]
    pub capture_images: bool,

    #[serde(rename = "CaptureFiles")]
    pub capture_files: bool,

    /// Whether a clip that needs a `.bin` sibling (text over `InlineTextLimit`,
    /// or an image) is written at all. Off drops it instead, the same as if the
    /// kind were switched off: only the first 512 characters would be searchable
    /// anyway, which is not worth a row.
    #[serde(rename = "WriteBlobs")]
    pub write_blobs: bool,

    #[serde(rename = "RetentionDays")]
    pub retention_days: u32,

    #[serde(rename = "SyncRootOverride")]
    pub sync_root_override: Option<String>,

    #[serde(skip)]
    pub app_dir: PathBuf,

    #[serde(skip)]
    pub sync_root: PathBuf,

    #[serde(skip)]
    pub machine_id: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hotkey: "Win+Alt+V".to_string(),
            max_blob_bytes: 10 * 1024 * 1024,
            inline_text_limit: 8192,
            rescan_seconds: 60,
            settings_scale: win::DEFAULT_SETTINGS_SCALE,
            popup_position: None,
            popup_size: None,
            capture_text: true,
            capture_images: true,
            capture_files: true,
            write_blobs: true,
            retention_days: 0,
            sync_root_override: None,
            app_dir: PathBuf::new(),
            sync_root: PathBuf::new(),
            machine_id: String::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        let dir = app_dir();
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(SETTINGS_FILE);

        let mut settings = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(parsed) => parsed,
                Err(err) => {
                    log::error(&format!("settings.json unreadable ({err}), using defaults"));
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        };

        settings.app_dir = dir;
        settings.machine_id = load_machine_id(&settings.app_dir);
        settings.sync_root =
            resolve_sync_root(&settings.app_dir, settings.sync_root_override.as_deref());
        // Hand-edited values are brought back into range here, so nothing
        // downstream has to wonder what a scale of 0 or 9000 would do.
        settings.settings_scale = settings
            .settings_scale
            .clamp(win::MIN_SETTINGS_SCALE, win::MAX_SETTINGS_SCALE);

        // Rewritten every start so a first run leaves an editable file behind.
        if let Err(err) = settings.save() {
            log::error(&format!("settings.json write failed: {err}"));
        }

        settings
    }

    pub fn save(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        fs::write(self.app_dir.join(SETTINGS_FILE), text)
    }

    /// This machine's private sub-tree. Only this process ever writes here.
    pub fn history_root(&self) -> PathBuf {
        self.sync_root.join(&self.machine_id)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
}

impl Hotkey {
    /// Rejects a combination that must not be registered, so neither a
    /// hand-edited settings.json nor the settings window's capture field can
    /// produce one.
    pub fn new(modifiers: u32, vk: u32) -> Option<Hotkey> {
        key_name(vk)?;

        // A bare key would swallow every press of it system-wide, so anything
        // without a modifier has to be a function key.
        if modifiers == 0 && !is_function_key(vk) {
            return None;
        }

        Some(Hotkey { modifiers, vk })
    }

    /// The canonical spelling: the inverse of `parse_hotkey`, in the modifier
    /// order the README documents.
    pub fn text(self) -> String {
        let mut out = String::new();

        // Win first: that is how the shipped default (`Win+Alt+V`) reads, and
        // how the README spells it.
        for (flag, name) in [
            (win::MOD_WIN, "Win"),
            (win::MOD_CONTROL, "Ctrl"),
            (win::MOD_ALT, "Alt"),
            (win::MOD_SHIFT, "Shift"),
        ] {
            if self.modifiers & flag != 0 {
                out.push_str(name);
                out.push('+');
            }
        }

        out.push_str(&key_name(self.vk).unwrap_or_default());
        out
    }
}

/// The spelling `virtual_key` accepts for this code, or `None` for a key with
/// no name — the settings window ignores those rather than letting the field
/// hold something that would not survive a round trip.
pub fn key_name(vk: u32) -> Option<String> {
    if (0x41..=0x5A).contains(&vk) || (0x30..=0x39).contains(&vk) {
        return Some(((vk as u8) as char).to_string());
    }

    if (0x70..=0x87).contains(&vk) {
        return Some(format!("F{}", vk - 0x6F));
    }

    None
}

fn is_function_key(vk: u32) -> bool {
    (0x70..=0x87).contains(&vk)
}

/// Returns `None` when the string is unusable, so the caller can report it
/// rather than silently running without a hotkey.
pub fn parse_hotkey(text: &str) -> Option<Hotkey> {
    let mut modifiers = 0u32;
    let mut vk = 0u32;

    for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "win" | "windows" | "super" => modifiers |= win::MOD_WIN,
            "ctrl" | "control" => modifiers |= win::MOD_CONTROL,
            "alt" => modifiers |= win::MOD_ALT,
            "shift" => modifiers |= win::MOD_SHIFT,
            other => {
                if vk != 0 {
                    return None; // two non-modifier keys
                }
                vk = virtual_key(other)?;
            }
        }
    }

    Hotkey::new(modifiers, vk)
}

fn virtual_key(name: &str) -> Option<u32> {
    let upper = name.to_ascii_uppercase();
    let bytes = upper.as_bytes();

    if bytes.len() == 1 {
        let c = bytes[0];
        return if c.is_ascii_uppercase() || c.is_ascii_digit() {
            Some(c as u32)
        } else {
            None
        };
    }

    if let Some(digits) = upper.strip_prefix('F') {
        if let Ok(n) = digits.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1); // VK_F1 .. VK_F24
            }
        }
    }

    None
}

fn resolve_sync_root(app_dir: &Path, over: Option<&str>) -> PathBuf {
    if let Some(value) = over {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    for variable in ["OneDriveCommercial", "OneDriveConsumer", "OneDrive"] {
        if let Some(value) = env::var_os(variable) {
            let path = PathBuf::from(value);
            if !path.as_os_str().is_empty() {
                return path.join("ClipPlus");
            }
        }
    }

    // No known sync provider: everything still works, just single-machine.
    app_dir.join("sync")
}

fn load_machine_id(dir: &Path) -> String {
    let path = dir.join(MACHINE_ID_FILE);
    if let Ok(text) = fs::read_to_string(&path) {
        let existing = text.trim();
        if !existing.is_empty() {
            return existing.to_string();
        }
    }

    let id = fresh_machine_id();
    if let Err(err) = fs::write(&path, &id) {
        log::error(&format!("machine.id write failed: {err}"));
    }
    id
}

/// Eight lowercase hex characters, the shape an existing `machine.id` already
/// has, so this machine keeps writing into the shard it has always used.
fn fresh_machine_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ ((std::process::id() as u64) << 48);
    format!("{:08x}", (mixed ^ (mixed >> 32)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings window writes what `text` produces and the save path reads
    /// it back through `parse_hotkey`, so the two have to agree — and the
    /// bare-key rule is what stops a hotkey from eating a whole key.
    #[test]
    fn hotkey_text_round_trips() {
        for spelling in ["Win+Alt+V", "Ctrl+Shift+F9", "F5", "Ctrl+0"] {
            let parsed = parse_hotkey(spelling).unwrap_or_else(|| panic!("{spelling}"));
            assert_eq!(parsed.text(), spelling);
            assert_eq!(parse_hotkey(&parsed.text()).map(Hotkey::text), Some(parsed.text()));
        }

        assert_eq!(
            parse_hotkey("ctrl+alt+v").map(Hotkey::text),
            Some("Ctrl+Alt+V".to_string())
        );

        // A bare letter, digit or punctuation key would be registered against
        // the whole system, so those are rejected rather than accepted.
        assert!(parse_hotkey("V").is_none());
        assert!(parse_hotkey("7").is_none());
        assert!(parse_hotkey("Ctrl+Shift").is_none());
        assert!(parse_hotkey("Ctrl+Shift+Q+W").is_none());
        assert!(parse_hotkey("Ctrl+F25").is_none());
    }
}
