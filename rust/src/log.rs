//! Append-only file logger.
//!
//! A tray app has no console, so this file is the only way to diagnose anything
//! on the target machine.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_LOG_BYTES: u64 = 1024 * 1024;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static GATE: Mutex<()> = Mutex::new(());

pub fn init(app_dir: &Path) {
    let _ = fs::create_dir_all(app_dir);
    let _ = LOG_PATH.set(app_dir.join("clipplus.log"));
}

pub fn path() -> Option<&'static Path> {
    LOG_PATH.get().map(PathBuf::as_path)
}

pub fn info(message: &str) {
    write("INFO ", message);
}

pub fn warn(message: &str) {
    write("WARN ", message);
}

pub fn error(message: &str) {
    write("ERROR", message);
}

fn write(level: &str, message: &str) {
    let Some(path) = LOG_PATH.get() else {
        return;
    };

    // Recover from a poisoned lock instead of losing the message: logging is
    // exactly what you want working when something else already went wrong.
    let _guard = GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > MAX_LOG_BYTES {
            let _ = fs::remove_file(path);
        }
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "{} [{}] {}",
            crate::win::local_timestamp(),
            level,
            message
        );
    }
}
