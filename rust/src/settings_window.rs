//! The settings window.
//!
//! A plain top-level window with system controls rather than a dialog resource:
//! a `.rc` file would mean a resource compiler step in the build, and the layout
//! here is fixed enough that a scripted dialog template buys nothing.
//!
//! Only two of these values can change without a restart. That is stated in the
//! window itself instead of being hidden, because silently ignoring a saved
//! field is worse than saying so.

use std::sync::OnceLock;

use crate::log;
use crate::settings::{self, Settings};
use crate::win::{self, HWND, LPARAM, LRESULT, WPARAM};

/// Client area in logical pixels; the frame is added around it at creation.
const CLIENT_WIDTH: i32 = 560;
const CLIENT_HEIGHT: i32 = 344;

const MARGIN: i32 = 16;
const LABEL_WIDTH: i32 = 168;
const FIELD_WIDTH: i32 = 340;
const ROW_HEIGHT: i32 = 24;
const ROW_STEP: i32 = 34;

const ID_HOTKEY: usize = 1;
const ID_SYNC_ROOT: usize = 2;
const ID_RETENTION: usize = 3;
const ID_MAX_BLOB_MB: usize = 4;
const ID_INLINE_LIMIT: usize = 5;
const ID_RESCAN: usize = 6;
const ID_CAPTURE_TEXT: usize = 7;
const ID_CAPTURE_IMAGES: usize = 8;
const ID_CAPTURE_FILES: usize = 9;
const ID_SAVE: usize = 10;
const ID_CANCEL: usize = 11;

static WINDOW: OnceLock<HWND> = OnceLock::new();

fn field(id: usize) -> HWND {
    WINDOW.get().map(|hwnd| win::child_by_id(*hwnd, id)).unwrap_or(0)
}

pub fn create() -> bool {
    let style = win::WS_CAPTION | win::WS_SYSMENU | win::WS_CLIPCHILDREN;
    let ex_style = 0;

    // The frame has to be added around the client size, not included in it,
    // otherwise the layout below would be clipped by the title bar.
    let mut frame = win::RECT {
        left: 0,
        top: 0,
        right: CLIENT_WIDTH,
        bottom: CLIENT_HEIGHT,
    };
    unsafe {
        win::AdjustWindowRectEx(&mut frame, style, 0, ex_style);
    }

    let width = frame.right - frame.left;
    let height = frame.bottom - frame.top;

    let window_proc: win::WNDPROC = window_proc;
    let hwnd = win::create_window(
        "ClipPlus.Settings",
        "ClipPlus 设置",
        window_proc,
        style,
        ex_style,
        0,
        0,
        width,
        height,
        win::COLOR_BTNFACE_BRUSH,
    );

    if hwnd == 0 {
        log::error(&format!(
            "settings window CreateWindowExW failed, err {}",
            win::last_error()
        ));
        return false;
    }

    let field_style = win::WS_CHILD | win::WS_VISIBLE | win::WS_BORDER | win::WS_TABSTOP;
    let label_style = win::WS_CHILD | win::WS_VISIBLE;
    let check_style = win::WS_CHILD | win::WS_VISIBLE | win::WS_TABSTOP | win::BS_AUTOCHECKBOX;
    let button_style = win::WS_CHILD | win::WS_VISIBLE | win::WS_TABSTOP | win::BS_PUSHBUTTON;

    let field_x = MARGIN + LABEL_WIDTH + 8;

    let rows: [(usize, &str); 6] = [
        (ID_HOTKEY, "热键（例 Win+Alt+V）"),
        (ID_SYNC_ROOT, "同步目录（留空 = 自动）"),
        (ID_RETENTION, "保留天数（0 = 不清理）"),
        (ID_MAX_BLOB_MB, "单条上限（MB）"),
        (ID_INLINE_LIMIT, "内联文本上限（字符）"),
        (ID_RESCAN, "重扫间隔（秒）"),
    ];

    for (index, (id, label)) in rows.iter().copied().enumerate() {
        let y = MARGIN + index as i32 * ROW_STEP;

        win::create_child_id("STATIC", label, label_style, hwnd, 0, MARGIN, y + 4, LABEL_WIDTH, ROW_HEIGHT);

        // Numeric fields reject non-digits at the control level, so the only
        // validation left is range.
        let numeric = id != ID_HOTKEY && id != ID_SYNC_ROOT;
        let style = if numeric {
            field_style | win::ES_AUTOHSCROLL | win::ES_NUMBER
        } else {
            field_style | win::ES_AUTOHSCROLL
        };

        win::create_child_id("EDIT", "", style, hwnd, id, field_x, y, FIELD_WIDTH, ROW_HEIGHT);
    }

    let check_y = MARGIN + 6 * ROW_STEP + 6;
    let checks: [(usize, &str); 3] = [
        (ID_CAPTURE_TEXT, "记录文本"),
        (ID_CAPTURE_IMAGES, "记录图片"),
        (ID_CAPTURE_FILES, "记录文件"),
    ];

    for (index, (id, label)) in checks.iter().copied().enumerate() {
        let x = MARGIN + index as i32 * 140;
        win::create_child_id("BUTTON", label, check_style, hwnd, id, x, check_y, 130, ROW_HEIGHT);
    }

    win::create_child_id(
        "STATIC",
        "热键与三个记录开关立即生效；其余项需要重启 ClipPlus。",
        label_style,
        hwnd,
        0,
        MARGIN,
        check_y + ROW_STEP,
        CLIENT_WIDTH - MARGIN * 2,
        ROW_HEIGHT,
    );

    let button_y = check_y + ROW_STEP * 2 + 6;
    win::create_child_id(
        "BUTTON",
        "保存",
        button_style | win::BS_DEFPUSHBUTTON,
        hwnd,
        ID_SAVE,
        MARGIN,
        button_y,
        110,
        26,
    );
    win::create_child_id(
        "BUTTON",
        "取消",
        button_style,
        hwnd,
        ID_CANCEL,
        MARGIN + 120,
        button_y,
        110,
        26,
    );

    if WINDOW.set(hwnd).is_err() {
        log::error("settings window already created");
        return false;
    }

    log::info(&format!("settings window ready (hwnd {hwnd:#x})"));
    true
}

pub fn show() {
    let Some(current) = crate::current_settings() else {
        return;
    };
    let Some(hwnd) = WINDOW.get().copied() else {
        return;
    };

    populate(&current);

    // Centre on whichever monitor the tray click came from.
    let cursor = win::cursor_position();
    let area = win::work_area_at(cursor);

    let mut frame = win::RECT {
        left: 0,
        top: 0,
        right: CLIENT_WIDTH,
        bottom: CLIENT_HEIGHT,
    };
    unsafe {
        win::AdjustWindowRectEx(
            &mut frame,
            win::WS_CAPTION | win::WS_SYSMENU | win::WS_CLIPCHILDREN,
            0,
            0,
        );
    }
    let width = frame.right - frame.left;
    let height = frame.bottom - frame.top;

    let left = area.left + (area.right - area.left - width) / 2;
    let top = area.top + (area.bottom - area.top - height) / 3;

    unsafe {
        win::SetWindowPos(hwnd, 0, left, top, width, height, win::SWP_SHOWWINDOW);
        win::set_foreground(hwnd);
    }

    log::info("settings window shown");
}

fn set_text(id: usize, text: &str) {
    let hwnd = field(id);
    if hwnd == 0 {
        return;
    }

    let wide = win::wide(text);
    unsafe {
        win::SetWindowTextW(hwnd, wide.as_ptr());
    }
}

fn populate(current: &Settings) {
    set_text(ID_HOTKEY, &current.hotkey);
    set_text(
        ID_SYNC_ROOT,
        current.sync_root_override.as_deref().unwrap_or(""),
    );
    set_text(ID_RETENTION, &current.retention_days.to_string());
    set_text(
        ID_MAX_BLOB_MB,
        &(current.max_blob_bytes / (1024 * 1024)).to_string(),
    );
    set_text(ID_INLINE_LIMIT, &current.inline_text_limit.to_string());
    set_text(ID_RESCAN, &current.rescan_seconds.to_string());

    set_checked(ID_CAPTURE_TEXT, current.capture_text);
    set_checked(ID_CAPTURE_IMAGES, current.capture_images);
    set_checked(ID_CAPTURE_FILES, current.capture_files);
}

fn set_checked(id: usize, checked: bool) {
    let hwnd = field(id);
    if hwnd == 0 {
        return;
    }

    // BM_SETCHECK, and the box already changed its own state on click.
    unsafe {
        win::SendMessageW(hwnd, win::BM_SETCHECK, usize::from(checked), 0);
    }
}

fn is_checked(id: usize) -> bool {
    let hwnd = field(id);
    if hwnd == 0 {
        return false;
    }

    unsafe { win::SendMessageW(hwnd, win::BM_GETCHECK, 0, 0) != 0 }
}

/// Returns a human-readable reason when a field is unusable.
fn parse_or(default: u32, id: usize) -> Result<u32, String> {
    let hwnd = field(id);
    if hwnd == 0 {
        return Ok(default);
    }

    let text = win::window_text(hwnd);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }

    trimmed
        .parse::<u32>()
        .map_err(|_| format!("“{trimmed}” 不是有效的非负整数"))
}

fn save() {
    let Some(mut updated) = crate::current_settings() else {
        return;
    };

    let hotkey = win::window_text(field(ID_HOTKEY)).trim().to_string();
    if settings::parse_hotkey(&hotkey).is_none() {
        complain(&format!(
            "热键 “{hotkey}” 无法识别。\n\n写法示例：Win+Alt+V、Ctrl+Shift+F9"
        ));
        return;
    }

    let retention = match parse_or(0, ID_RETENTION) {
        Ok(value) => value,
        Err(reason) => {
            complain(&format!("保留天数：{reason}"));
            return;
        }
    };

    let max_blob_mb = match parse_or(10, ID_MAX_BLOB_MB) {
        Ok(value) => value,
        Err(reason) => {
            complain(&format!("单条上限：{reason}"));
            return;
        }
    };

    let inline_limit = match parse_or(8192, ID_INLINE_LIMIT) {
        Ok(value) => value,
        Err(reason) => {
            complain(&format!("内联文本上限：{reason}"));
            return;
        }
    };

    let rescan = match parse_or(60, ID_RESCAN) {
        Ok(value) => value,
        Err(reason) => {
            complain(&format!("重扫间隔：{reason}"));
            return;
        }
    };

    if rescan < 10 {
        complain("重扫间隔不能小于 10 秒。");
        return;
    }

    if max_blob_mb == 0 {
        complain("单条上限不能为 0，否则什么都存不下来。");
        return;
    }

    let sync_root = win::window_text(field(ID_SYNC_ROOT)).trim().to_string();

    updated.hotkey = hotkey;
    updated.sync_root_override = if sync_root.is_empty() {
        None
    } else {
        Some(sync_root)
    };
    updated.retention_days = retention;
    updated.max_blob_bytes = max_blob_mb as u64 * 1024 * 1024;
    updated.inline_text_limit = inline_limit as usize;
    updated.rescan_seconds = rescan as u64;
    updated.capture_text = is_checked(ID_CAPTURE_TEXT);
    updated.capture_images = is_checked(ID_CAPTURE_IMAGES);
    updated.capture_files = is_checked(ID_CAPTURE_FILES);

    if let Err(err) = updated.save() {
        complain(&format!("写入 settings.json 失败：{err}"));
        return;
    }

    crate::set_settings(updated);
    crate::apply_hotkey();

    log::info("settings saved");
    hide();
}

fn complain(message: &str) {
    win::message_box("ClipPlus 设置", message, win::MB_OK | win::MB_ICONWARNING);
}

pub fn hide() {
    if let Some(hwnd) = WINDOW.get().copied() {
        unsafe {
            win::ShowWindow(hwnd, win::SW_HIDE);
        }
    }
}

extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        win::WM_COMMAND => {
            // LOWORD is the control id, HIWORD the notification code.
            let id = wparam & 0xFFFF;
            let notification = ((wparam >> 16) & 0xFFFF) as u32;

            // Keep the default push-button behaviour for Enter.
            if notification == win::BN_CLICKED || notification == 0 {
                match id {
                    ID_SAVE => save(),
                    ID_CANCEL => hide(),
                    _ => {}
                }
            }
            0
        }

        // Closing hides rather than destroys, so the window can be reopened
        // without rebuilding every control.
        win::WM_CLOSE => {
            hide();
            0
        }

        _ => win::def_window_proc(hwnd, message, wparam, lparam),
    }
}
