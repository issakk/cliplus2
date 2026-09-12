#![windows_subsystem = "windows"]

mod clip;
mod clipboard;
mod index;
mod log;
mod popup;
mod settings;
mod store;
mod win;

use crate::clip::ClipPayload;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

/// Deliberately the same name the C# build uses. Both versions write into the
/// same machine-sharded history folder, so letting them run together would
/// capture every clip twice.
const INSTANCE_MUTEX: &str = "Local\\ClipPlus.SingleInstance";

const HOTKEY_ID: i32 = 0xC1A0;

static SETTINGS: OnceLock<settings::Settings> = OnceLock::new();
static LAST_CLIPBOARD_SEQUENCE: AtomicU32 = AtomicU32::new(0);
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
    let _ = SETTINGS.set(settings.clone());

    if !win::acquire_single_instance(&win::wide(INSTANCE_MUTEX)) {
        log::warn("another ClipPlus instance owns the single-instance mutex; exiting");
        // Silent exit is the worst possible outcome here: the C# build and this
        // one share the mutex on purpose, so "nothing happened" is almost always
        // the tray instance still running.
        win::message_box(
            "ClipPlus",
            "另一个 ClipPlus 正在运行，本次启动已退出。\n\nC# 版和 Rust 版共用同一个单实例锁，写的是同一个历史目录，\n同时运行会把每条剪贴捕获两遍。请先在托盘里退出那一个。",
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
    );

    if hwnd == 0 {
        let detail = format!("CreateWindowExW failed, err {}", win::last_error());
        log::error(&detail);
        win::message_box("ClipPlus", &detail, win::MB_OK | win::MB_ICONERROR);
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
                    log::info(&format!("captured {description}"));
                    if let Some(store) = STORE.get() {
                        store.enqueue(payload);
                    }
                }
            }
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
