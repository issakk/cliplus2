#![windows_subsystem = "windows"]

mod autostart;
mod clip;
mod clipboard;
mod index;
mod log;
mod popup;
mod tray;
mod settings;
mod settings_window;
mod store;
mod win;

use crate::clip::ClipPayload;

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// Deliberately unchanged since the previous build, which used the same name: an
/// older ClipPlus still installed would otherwise run alongside this one and
/// capture every clip twice.
const INSTANCE_MUTEX: &str = "Local\\ClipPlus.SingleInstance";

const HOTKEY_ID: i32 = 0xC1A0;

// A lock rather than a OnceLock: the settings window can change the hotkey at
// runtime, so the settings are not a value that is written once and read
// forever.
static SETTINGS: RwLock<Option<settings::Settings>> = RwLock::new(None);
static LAST_CLIPBOARD_SEQUENCE: AtomicU32 = AtomicU32::new(0);
static STORE: OnceLock<Arc<store::Store>> = OnceLock::new();

/// The hidden window the hotkey is registered against. Needed to re-register
/// it when the setting changes at runtime.
static MESSAGE_WINDOW: AtomicIsize = AtomicIsize::new(0);

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

    // Built once at startup so the first hotkey press has no window-creation
    // latency in front of it.
    if !popup::create(Arc::clone(&store)) {
        log::error("popup could not be created; the hotkey will do nothing");
    }

    win::set_per_monitor_dpi_aware();

    // Coerce at a let-binding rather than inline, so the safe-fn to unsafe-fn
    // pointer conversion has an unambiguous coercion site.
    let window_proc: win::WNDPROC = wnd_proc;

    let hwnd = win::create_window(
        "ClipPlus.MessageWindow",
        "ClipPlus",
        window_proc,
        win::WS_POPUP,
        win::WS_EX_TOOLWINDOW,
        0,
        0,
        0,
        0,
        0, // never painted: a message-only window is never shown
    );

    if hwnd == 0 {
        let detail = format!("CreateWindowExW failed, err {}", win::last_error());
        log::error(&detail);
        win::message_box("ClipPlus", &detail, win::MB_OK | win::MB_ICONERROR);
        return;
    }
    log::info(&format!("message window ready (hwnd {hwnd:#x})"));

    // Recorded before registering: apply_hotkey reads it.
    MESSAGE_WINDOW.store(hwnd, Ordering::SeqCst);
    apply_hotkey();

    if !settings_window::create() {
        log::error("settings window could not be created; the tray entry will do nothing");
    }

    if win::add_clipboard_listener(hwnd) {
        log::info("clipboard listener registered");
    } else {
        log::error(&format!(
            "AddClipboardFormatListener failed, err {}",
            win::last_error()
        ));
    }

    if let Some(settings) = current_settings() {
        tray::add(hwnd, &settings);
    }
    log::info("entering message loop");
    win::run_message_loop();

    // The tray had to be removed before the loop ended for the menu's Quit
    // path; repeating it here is harmless and covers a WM_CLOSE shutdown.
    tray::remove();
    win::remove_clipboard_listener(hwnd);
    win::destroy_window(hwnd);
    log::info("=== ClipPlus stopping ===");
}

/// The three per-kind switches are consulted at capture time rather than at
/// startup, which is what lets the settings window turn them off and have it
/// take effect on the next copy instead of the next launch.
fn capture_enabled(payload: &ClipPayload) -> bool {
    let Some(settings) = current_settings() else {
        return true;
    };

    match payload {
        ClipPayload::Text(_) => settings.capture_text,
        ClipPayload::Image(_) => settings.capture_images,
        ClipPayload::Files(_) => settings.capture_files,
    }
}

/// Snapshot of the live settings. Cloned, because callers outlive the lock.
pub fn current_settings() -> Option<settings::Settings> {
    SETTINGS.read().unwrap_or_else(|p| p.into_inner()).clone()
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
    let hwnd = MESSAGE_WINDOW.load(Ordering::SeqCst);
    if hwnd != 0 {
        win::unregister_hotkey(hwnd, HOTKEY_ID);
    }
}

/// (Re)registers the hotkey from the current settings.
///
/// Called at startup and again whenever the settings window saves, which is
/// what makes a hotkey change take effect without a restart.
pub fn apply_hotkey() {
    let hwnd = MESSAGE_WINDOW.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }

    let Some(settings) = current_settings() else {
        return;
    };

    let Some(hotkey) = settings::parse_hotkey(&settings.hotkey) else {
        log::error(&format!(
            "hotkey '{}' could not be parsed; expected e.g. Win+Alt+V",
            settings.hotkey
        ));
        return;
    };

    // Drop the old registration first: re-registering the same combination
    // while it is still held by this process fails against ourselves.
    win::unregister_hotkey(hwnd, HOTKEY_ID);

    if win::register_hotkey(hwnd, HOTKEY_ID, hotkey.modifiers | win::MOD_NOREPEAT, hotkey.vk) {
        log::info(&format!("hotkey registered: {}", settings.hotkey));
    } else {
        let detail = format!(
            "RegisterHotKey failed for '{}', err {} (most likely already taken by another program)",
            settings.hotkey,
            win::last_error()
        );
        log::error(&detail);
        win::message_box("ClipPlus", &detail, win::MB_OK | win::MB_ICONERROR);
    }
}

extern "system" fn wnd_proc(
    hwnd: win::HWND,
    message: u32,
    wparam: win::WPARAM,
    lparam: win::LPARAM,
) -> win::LRESULT {
    match message {
        win::WM_CLIPBOARDUPDATE => {
            // The OS notifies on every format change and one Ctrl+C can raise
            // several; the sequence number filters those for free.
            let sequence = win::clipboard_sequence_number();
            if LAST_CLIPBOARD_SEQUENCE.swap(sequence, Ordering::SeqCst) != sequence {
                if let Some(payload) = clipboard::read() {
                    let description = match &payload {
                        ClipPayload::Text(text) => format!("text, {} chars", text.chars().count()),
                        ClipPayload::Files(paths) => format!("{} file(s)", paths.len()),
                        ClipPayload::Image(png) => format!("image, {} PNG bytes", png.len()),
                    };
                    if capture_enabled(&payload) {
                        log::info(&format!("captured {description}"));
                        if let Some(store) = STORE.get() {
                            store.enqueue(payload, clipboard::capture_context());
                        }
                    } else {
                        log::info(&format!("ignored {description} (switched off)"));
                    }
                }
            }
            0
        }

        tray::CALLBACK_MESSAGE => {
            tray::handle_callback(lparam);
            0
        }

        win::WM_HOTKEY => {
            if wparam as i32 == HOTKEY_ID {
                popup::toggle();
            }
            0
        }

        win::WM_DESTROY => {
            win::post_quit_message(0);
            0
        }

        _ => win::def_window_proc(hwnd, message, wparam, lparam),
    }
}
