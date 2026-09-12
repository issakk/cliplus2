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
    // MAKEINTRESOURCE: a small integer carried in the pointer slot, resolved by
    data.icon = load_icon();

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

/// Builds an HICON from the embedded .ico.
///
/// `CreateIconFromResourceEx` wants a single image — the BITMAPINFOHEADER blob
/// — not the whole container, so the directory entry is read to find one. The
/// largest entry wins and Windows scales it down for the tray, which beats
/// shipping three separately selected sizes for a 16px target.
///
/// Any problem falls back to the stock application icon: a generic tray icon is
/// far better than no tray icon, because the tray icon is the only way to quit.
fn load_icon() -> win::HICON {
    const ICON: &[u8] = include_bytes!("../../assets/clipplus.ico");

    unsafe fn fallback() -> win::HICON {
        win::LoadIconW(0, win::IMI_APPLICATION as usize as *const u16)
    }

    // ICONDIR: reserved(2) type(2) count(2), then 16 bytes per entry.
    if ICON.len() < 6 || u16::from_le_bytes([ICON[2], ICON[3]]) != 1 {
        log::warn("embedded icon is not an icon file; using the system one");
        return unsafe { fallback() };
    }

    let count = u16::from_le_bytes([ICON[4], ICON[5]]) as usize;
    let mut best: Option<(u32, usize, usize)> = None;

    for index in 0..count {
        let base = 6 + index * 16;
        if ICON.len() < base + 16 {
            break;
        }

        // A zero width or height means 256, but this file has no such entry.
        let width = ICON[base] as u32;
        let height = ICON[base + 1] as u32;
        let size = u32::from_le_bytes([
            ICON[base + 8],
            ICON[base + 9],
            ICON[base + 10],
            ICON[base + 11],
        ]) as usize;
        let offset = u32::from_le_bytes([
            ICON[base + 12],
            ICON[base + 13],
            ICON[base + 14],
            ICON[base + 15],
        ]) as usize;

        if offset + size > ICON.len() {
            continue;
        }

        let area = width.max(1) * height.max(1);
        if best.map(|(seen, _, _)| area > seen).unwrap_or(true) {
            best = Some((area, offset, size));
        }
    }

    let Some((_, offset, size)) = best else {
        log::warn("embedded icon has no usable image; using the system one");
        return unsafe { fallback() };
    };

    let handle = unsafe {
        win::CreateIconFromResourceEx(
            ICON[offset..].as_ptr(),
            size as u32,
            1,           // fIcon
            0x0003_0000, // version 3.0
            0,
            0,
            0,
        )
    };

    if handle == 0 {
        log::warn(&format!(
            "CreateIconFromResourceEx failed, err {}; using the system icon",
            win::last_error()
        ));
        return unsafe { fallback() };
    }

    handle
}
