#![windows_subsystem = "windows"]

mod autostart;
mod cleanup_window;
mod clip;
mod clipboard;
mod edit_window;
mod egui_app;
mod index;
mod log;
mod paste;
mod platform;
mod tray;
mod settings;
mod settings_window;
mod store;
mod thumb;
mod win;

use std::sync::{Arc, OnceLock, RwLock};

/// Deliberately unchanged since the previous build, which used the same name: an
/// older ClipPlus still installed would otherwise run alongside this one and
/// capture every clip twice.
const INSTANCE_MUTEX: &str = "Local\\ClipPlus.SingleInstance";

// A lock rather than a OnceLock: the settings window can change the hotkey at
// runtime, so the settings are not a value that is written once and read
// forever.
static SETTINGS: RwLock<Option<settings::Settings>> = RwLock::new(None);
static STORE: OnceLock<Arc<store::Store>> = OnceLock::new();

fn main() {
    log::init(&settings::app_dir());

    // A GUI binary has no stderr, so a panic would otherwise vanish.
    std::panic::set_hook(Box::new(|info| log::error(&format!("panic: {info}"))));

    log::info("=== ClipPlus (rust) starting ===");
    let settings = settings::Settings::load();
    log::info(&format!(
        "machine={} syncRoot={} hotkey={}",
        settings.machine_id,
        settings.sync_root.display(),
        settings.hotkey
    ));
    set_settings(settings.clone());

    if !win::acquire_single_instance(&win::wide(INSTANCE_MUTEX)) {
        log::warn("another ClipPlus instance owns the single-instance mutex; exiting");
        // Silent exit is the worst possible outcome here: the mutex is shared
        // with an older ClipPlus install on purpose, so "nothing happened" is
        // almost always the tray instance still running.
        win::message_box(
            "ClipPlus",
            "另一个 ClipPlus 正在运行，本次启动已退出。\n\n新旧两版共用同一个单实例锁，写的是同一个历史目录，\n同时运行会把每条剪贴捕获两遍。请先在托盘里退出那一个。",
            win::MB_OK | win::MB_ICONWARNING,
        );
        return;
    }

    // Only now that we know we are the single running instance: building the
    // store walks the whole history folder, which would be wasted work for a
    // second launch that is about to exit.
    let store = Arc::new(store::Store::new(settings));
    let _ = STORE.set(Arc::clone(&store));

    // Hashing and disk I/O never touch the window thread.
    {
        let store = Arc::clone(&store);
        std::thread::spawn(move || store.run_writer());
    }
    {
        let store = Arc::clone(&store);
        std::thread::spawn(move || store.run_rescan_loop());
    }
    {
        let store = Arc::clone(&store);
        std::thread::spawn(move || store.run_retention_loop());
    }

    // Bound to a named variable on purpose: dropping the watcher silently stops
    // every filesystem event, and `let _ =` would drop it right here.
    let _watcher = store.start_watcher();

    // Dark-mode hack for the tray menu, which is still a Win32 `TrackPopupMenu`;
    // the egui popup paints its own dark theme and needs none of it. A window
    // keeps the theme it was made with, so this has to come before any window.
    win::allow_dark_mode();
    win::set_per_monitor_dpi_aware();

    // The three secondary windows stay Win32 on purpose: plain system-control
    // dialogs, living on this same thread and pumped by winit's shared message
    // loop once the egui loop starts. DPI awareness is set above so they scale.
    if !settings_window::create() {
        log::error("settings window could not be created; the tray entry will do nothing");
    }
    if !cleanup_window::create() {
        log::error("cleanup window could not be created; the settings 清理 button will do nothing");
    }
    if !edit_window::create() {
        log::error("edit window could not be created; the row menu 编辑 entry will do nothing");
    }

    egui_app::run(store);
    log::info("=== ClipPlus stopping ===");
}

/// Snapshot of the live settings. Cloned, because callers outlive the lock.
pub fn current_settings() -> Option<settings::Settings> {
    SETTINGS.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// The store handle for threads outside main — the cleanup window's background
/// scans and runs. `None` only before startup has finished, in which case the
/// caller has nothing to work on anyway.
pub fn store() -> Option<std::sync::Arc<store::Store>> {
    STORE.get().cloned()
}

pub fn set_settings(updated: settings::Settings) {
    // Every settings change arrives here — at startup and again on each save —
    // which makes this the one place the settings-window scale is applied from.
    win::set_settings_scale(updated.settings_scale);
    *SETTINGS.write().unwrap_or_else(|p| p.into_inner()) = Some(updated);
}

/// Remembers where the popup was left and how big, so the next open puts it back
/// there instead of jumping to wherever the cursor happens to be.
///
/// Goes into the settings file because that is already the one place this app keeps
/// state between runs. The settings window never touches these fields — it saves a
/// copy of the current settings with only the fields it shows replaced — so a drag
/// and a settings save cannot lose each other's work.
///
/// `size` is in logical pixels at 96 DPI, the units the popup's own constants are
/// written in, so a monitor with another scale gets the size it would have had
/// rather than the one measured on the display it was stretched on.
pub fn remember_popup_layout(position: (i32, i32), size: (i32, i32)) {
    let Some(mut settings) = current_settings() else {
        return;
    };

    if settings.popup_position == Some(position) && settings.popup_size == Some(size) {
        return;
    }

    settings.popup_position = Some(position);
    settings.popup_size = Some(size);

    if let Err(err) = settings.save() {
        log::error(&format!("popup layout could not be saved: {err}"));
        return;
    }

    set_settings(settings);
}

/// Drops the global hotkey for as long as the settings window is recording a new
/// one. Without this, pressing the combination that is already registered would
/// fire the popup instead of reaching the field.
pub fn suspend_hotkey() {
    platform::request_suspend_hotkey();
}

/// (Re)registers the hotkey from the current settings.
///
/// Called at startup and again whenever the settings window saves, which is
/// what makes a hotkey change take effect without a restart. The registration
/// itself happens on the platform thread, where the hotkey has to live.
pub fn apply_hotkey() {
    platform::request_apply_hotkey();
}
