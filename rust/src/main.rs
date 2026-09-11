#![windows_subsystem = "windows"]

mod log;
mod settings;
mod win;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

/// Deliberately the same name the C# build uses. Both versions write into the
/// same machine-sharded history folder, so letting them run together would
/// capture every clip twice.
const INSTANCE_MUTEX: &str = "Local\\ClipPlus.SingleInstance";

const HOTKEY_ID: i32 = 0xC1A0;

static SETTINGS: OnceLock<settings::Settings> = OnceLock::new();
static LAST_CLIPBOARD_SEQUENCE: AtomicU32 = AtomicU32::new(0);

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
    let _ = SETTINGS.set(settings);

    if !win::acquire_single_instance(&win::wide(INSTANCE_MUTEX)) {
        log::warn("another ClipPlus instance owns the single-instance mutex; exiting");
        return;
    }

    win::set_per_monitor_dpi_aware();

    // Coerce at a let-binding rather than inline, so the safe-fn to unsafe-fn
    // pointer conversion has an unambiguous coercion site.
    let window_proc: win::WNDPROC = wnd_proc;

    let hwnd = win::create_message_window(
        &win::wide("ClipPlus.MessageWindow"),
        &win::wide("ClipPlus"),
        window_proc,
    );

    if hwnd == 0 {
        log::error(&format!(
            "CreateWindowExW failed, err {}",
            win::last_error()
        ));
        return;
    }
    log::info(&format!("message window ready (hwnd {hwnd:#x})"));

    register_hotkey(hwnd);

    if win::add_clipboard_listener(hwnd) {
        log::info("clipboard listener registered");
    } else {
        log::error(&format!(
            "AddClipboardFormatListener failed, err {}",
            win::last_error()
        ));
    }

    log::info("entering message loop");
    win::run_message_loop();

    win::remove_clipboard_listener(hwnd);
    win::destroy_window(hwnd);
    log::info("=== ClipPlus stopping ===");
}

fn register_hotkey(hwnd: win::HWND) {
    let Some(settings) = SETTINGS.get() else {
        return;
    };

    let Some(hotkey) = settings::parse_hotkey(&settings.hotkey) else {
        log::error(&format!(
            "hotkey '{}' could not be parsed; expected e.g. Win+Alt+V",
            settings.hotkey
        ));
        return;
    };

    if win::register_hotkey(
        hwnd,
        HOTKEY_ID,
        hotkey.modifiers | win::MOD_NOREPEAT,
        hotkey.vk,
    ) {
        log::info(&format!("hotkey registered: {}", settings.hotkey));
    } else {
        log::error(&format!(
            "RegisterHotKey failed for '{}', err {} (most likely already taken)",
            settings.hotkey,
            win::last_error()
        ));
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
                log::info(&format!("clipboard changed (sequence {sequence})"));
            }
            0
        }

        win::WM_HOTKEY => {
            if wparam as i32 == HOTKEY_ID {
                log::info("hotkey pressed");
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
