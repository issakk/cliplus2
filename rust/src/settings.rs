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

    #[serde(rename = "CaptureText")]
    pub capture_text: bool,

    #[serde(rename = "CaptureImages")]
    pub capture_images: bool,

    #[serde(rename = "CaptureFiles")]
    pub capture_files: bool,

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
            capture_text: true,
            capture_images: true,
            capture_files: true,
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

    if vk == 0 {
        None
    } else {
        Some(Hotkey { modifiers, vk })
    }
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
