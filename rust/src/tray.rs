//! Tray icon and context menu.
//!
//! Until this existed there was no way to quit the process short of Task
//! Manager, so the menu is not a convenience feature — it is the exit.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;

use crate::autostart;
use crate::log;
use crate::popup;
use crate::settings::Settings;
use crate::win::{self, HWND, LPARAM};

const ICON_ID: u32 = 1;

/// Arbitrary but must stay in the WM_APP range so it cannot collide with a
/// message Windows itself sends.
pub const CALLBACK_MESSAGE: u32 = win::WM_APP + 1;

const CMD_OPEN_FOLDER: i32 = 1;
const CMD_SETTINGS: i32 = 2;
const CMD_AUTOSTART: i32 = 3;
const CMD_QUIT: i32 = 4;
const CMD_OPEN_SETTINGS_FILE: i32 = 5;

static OWNER: AtomicIsize = AtomicIsize::new(0);
static SYNC_ROOT: OnceLock<String> = OnceLock::new();
static APP_DIR: OnceLock<String> = OnceLock::new();

pub fn add(hwnd: HWND, settings: &Settings) -> bool {
    OWNER.store(hwnd, Ordering::SeqCst);
    let _ = SYNC_ROOT.set(settings.sync_root.to_string_lossy().into_owned());
    let _ = APP_DIR.set(settings.app_dir.to_string_lossy().into_owned());

    // All-zero is the correct "unset" state for every field of this struct.
    let mut data: win::NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cb_size = std::mem::size_of::<win::NOTIFYICONDATAW>() as u32;
    data.hwnd = hwnd;
    data.id = ICON_ID;
    data.flags = win::NIF_MESSAGE | win::NIF_ICON | win::NIF_TIP;
    data.callback_message = CALLBACK_MESSAGE;
    // At the shell's own small-icon size, so the tray draws the entry that was
    // rendered for it instead of resampling a bigger one.
    data.icon = win::app_icon(win::small_icon_size());

    let tip = win::wide("ClipPlus — Win+Alt+V 打开历史");
    let copied = tip.len().min(win::TIP_CHARS);
    data.tip[..copied].copy_from_slice(&tip[..copied]);

    let added = unsafe { win::Shell_NotifyIconW(win::NIM_ADD, &mut data) } != 0;

    if added {
        log::info("tray icon added");
    } else {
        log::error("Shell_NotifyIconW(NIM_ADD) failed; no tray icon, so no menu");
    }

    added
}

pub fn remove() {
    let hwnd = OWNER.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }

    let mut data: win::NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cb_size = std::mem::size_of::<win::NOTIFYICONDATAW>() as u32;
    data.hwnd = hwnd;
    data.id = ICON_ID;

    unsafe {
        win::Shell_NotifyIconW(win::NIM_DELETE, &mut data);
    }
}

/// Tray callback: `lparam` carries the mouse message that triggered it.
pub fn handle_callback(lparam: LPARAM) {
    match (lparam as u32) & 0xFFFF {
        win::WM_LBUTTONUP => popup::toggle(),
        win::WM_RBUTTONUP => show_menu(),
        _ => {}
    }
}

fn show_menu() {
    let hwnd = OWNER.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }

    unsafe {
        let menu = win::CreatePopupMenu();
        if menu == 0 {
            log::warn("CreatePopupMenu failed");
            return;
        }

        append(menu, win::MF_STRING, CMD_OPEN_FOLDER, "打开同步目录");
        append(menu, win::MF_STRING, CMD_SETTINGS, "设置…");
        append(menu, win::MF_STRING, CMD_OPEN_SETTINGS_FILE, "打开设置文件");
        append(menu, win::MF_SEPARATOR, 0, "");
        append(
            menu,
            if autostart::is_enabled() {
                win::MF_STRING | win::MF_CHECKED
            } else {
                win::MF_STRING
            },
            CMD_AUTOSTART,
            "开机自启",
        );
        append(menu, win::MF_SEPARATOR, 0, "");
        append(menu, win::MF_STRING, CMD_QUIT, "退出");

        let point = win::cursor_position();

        // Required before TrackPopupMenu: without it the menu never notices the
        // user clicking away and stays on screen.
        win::set_foreground(hwnd);

        let chosen = win::TrackPopupMenu(
            menu,
            win::TPM_RIGHTBUTTON | win::TPM_RETURNCMD,
            point.x,
            point.y,
            0,
            hwnd,
            std::ptr::null(),
        );

        win::DestroyMenu(menu);

        // Documented workaround: without this the next click on any window gets
        // swallowed by the menu's dismantling.
        win::post_message(hwnd, win::WM_NULL, 0, 0);

        handle_command(chosen);
    }
}

fn append(menu: win::HMENU, flags: u32, id: i32, label: &str) {
    let text = win::wide(label);
    unsafe {
        win::AppendMenuW(menu, flags, id as usize, text.as_ptr());
    }
}

fn handle_command(command: i32) {
    // 0 means the menu was dismissed without a choice.
    match command {
        CMD_OPEN_FOLDER => open(SYNC_ROOT.get().map(String::as_str)),
        CMD_SETTINGS => crate::settings_window::show(),
        CMD_OPEN_SETTINGS_FILE => open(APP_DIR.get().map(String::as_str)),
        CMD_AUTOSTART => {
            let enable = !autostart::is_enabled();
            if autostart::set_enabled(enable) {
                log::info(&format!(
                    "autostart {}",
                    if enable { "enabled" } else { "disabled" }
                ));
            } else {
                log::error("autostart could not be changed");
            }
        }
        CMD_QUIT => {
            log::info("quit requested from the tray");
            remove();
            let hwnd = OWNER.load(Ordering::SeqCst);
            if hwnd != 0 {
                // Destroying the message window raises WM_DESTROY, which is what
                // ends the message loop and runs the shutdown path.
                win::destroy_window(hwnd);
            }
        }
        _ => {}
    }
}

fn open(path: Option<&str>) {
    let Some(path) = path else {
        return;
    };

    let operation = win::wide("open");
    let target = win::wide(path);

    unsafe {
        win::ShellExecuteW(
            0,
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            win::SW_SHOWNORMAL,
        );
    }
}

