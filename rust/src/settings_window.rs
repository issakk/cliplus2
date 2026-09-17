//! The settings window.
//!
//! A plain top-level window with system controls rather than a dialog resource:
//! a `.rc` file would mean a resource compiler step in the build, and the layout
//! here is fixed enough that a scripted dialog template buys nothing.
//!
//! Some of these values can change without a restart. That is stated in the
//! window itself instead of being hidden, because silently ignoring a saved
//! field is worse than saying so.

use std::sync::OnceLock;

use crate::log;
use crate::settings::{self, Hotkey, Settings};
use crate::win::{self, HWND, LPARAM, LRESULT, WPARAM};

/// Client area in logical pixels; the frame is added around it at creation.
///
/// Both halves are computed from the layout constants below rather than written
/// down. That is not style: this window was left at its old height when a row was
/// added, and the checkboxes and the buttons ended up below the bottom edge.
const CLIENT_WIDTH: i32 = MARGIN + LABEL_WIDTH + FIELD_GAP + FIELD_WIDTH + MARGIN;
const CLIENT_HEIGHT: i32 = BUTTON_TOP + BUTTON_HEIGHT + MARGIN;

const MARGIN: i32 = 16;
const LABEL_WIDTH: i32 = 220;
const FIELD_WIDTH: i32 = 360;
const ROW_HEIGHT: i32 = 26;
const ROW_STEP: i32 = 38;
/// Between a caption and its field, and between two buttons.
const FIELD_GAP: i32 = 8;
const BUTTON_GAP: i32 = 10;
const BUTTON_WIDTH: i32 = 110;
const BUTTON_HEIGHT: i32 = 26;
const CHECK_WIDTH: i32 = 130;
const CHECK_STEP: i32 = 140;

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
const ID_SETTINGS_SCALE: usize = 12;
const ID_WRITE_BLOBS: usize = 13;

/// The rows, top to bottom: the control id and the caption beside it.
///
/// A constant rather than a local in `create`, because `CHECKS_TOP` below is derived
/// from its length — that is what keeps a new row from pushing the controls below
/// the bottom edge again. `layout` walks the same list, so a row is described in
/// exactly one place.
const ROW_LABELS: [(usize, &str); 7] = [
    (ID_HOTKEY, "热键（点这里按组合键）"),
    (ID_SYNC_ROOT, "同步目录（留空 = 自动）"),
    (ID_RETENTION, "保留天数（0 = 不清理）"),
    (ID_MAX_BLOB_MB, "单条上限（MB）"),
    (ID_INLINE_LIMIT, "内联文本上限（字符）"),
    (ID_RESCAN, "重扫间隔（秒）"),
    (ID_SETTINGS_SCALE, "设置窗口字号（%）"),
];

/// The three bands under the rows: checkboxes, the note, the buttons.
const CHECKS_TOP: i32 = MARGIN + ROW_LABELS.len() as i32 * ROW_STEP + 6;
const NOTE_TOP: i32 = CHECKS_TOP + ROW_STEP;
const BUTTON_TOP: i32 = CHECKS_TOP + ROW_STEP * 2 + 6;

/// Subclass id for the hotkey field, which is the only control here that has to
/// intercept its own keystrokes.
const HOTKEY_SUBCLASS_ID: usize = 1;

/// The row labels and the note are moved by `layout`, which needs a handle for
/// each, so they get ids of their own — a control created without one cannot be
/// found again.
const LABEL_ID_BASE: usize = 100;
const NOTE_ID: usize = 200;
static WINDOW: OnceLock<HWND> = OnceLock::new();

fn field(id: usize) -> HWND {
    WINDOW.get().map(|hwnd| win::child_by_id(*hwnd, id)).unwrap_or(0)
}

/// The frame style this window is created with; the size of the window is
/// computed from it in more than one place.
const WINDOW_STYLE: u32 = win::WS_CAPTION | win::WS_SYSMENU | win::WS_CLIPCHILDREN;

/// The window's outside size for a monitor scale: the client area the layout was
/// designed at plus the frame Windows draws around it. The frame has to be added
/// around the client size rather than included in it, or the layout would be
/// clipped by the title bar.
fn frame_size(scale: f64) -> (i32, i32) {
    let mut frame = win::RECT {
        left: 0,
        top: 0,
        right: win::scaled(CLIENT_WIDTH, scale),
        bottom: win::scaled(CLIENT_HEIGHT, scale),
    };

    unsafe {
        win::AdjustWindowRectEx(&mut frame, WINDOW_STYLE, 0, 0);
    }

    (frame.right - frame.left, frame.bottom - frame.top)
}

/// The scale this window draws at: the monitor's DPI, times the user's own
/// `SettingsScale`.
///
/// The multiplication lives here rather than inside `win::scaled` on purpose: the
/// popup reads that helper too, and it is a fixed-density list that must not follow
/// the setting.
fn window_scale(dpi_scale: f64) -> f64 {
    dpi_scale * win::settings_scale_factor()
}

pub fn create() -> bool {
    let style = WINDOW_STYLE;
    let ex_style = 0;

    let (width, height) = frame_size(1.0);

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

    let field_x = MARGIN + LABEL_WIDTH + FIELD_GAP;

    for (index, (id, label)) in ROW_LABELS.iter().copied().enumerate() {
        let y = MARGIN + index as i32 * ROW_STEP;

        win::create_child_id(
            "STATIC",
            label,
            label_style,
            hwnd,
            LABEL_ID_BASE + index,
            MARGIN,
            y,
            LABEL_WIDTH,
            ROW_HEIGHT,
        );

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

    let check_y = CHECKS_TOP;
    let checks: [(usize, &str); 4] = [
        (ID_CAPTURE_TEXT, "记录文本"),
        (ID_CAPTURE_IMAGES, "记录图片"),
        (ID_CAPTURE_FILES, "记录文件"),
        (ID_WRITE_BLOBS, "超限写 .bin"),
    ];

    for (index, (id, label)) in checks.iter().copied().enumerate() {
        let x = MARGIN + index as i32 * CHECK_STEP;
        win::create_child_id(
            "BUTTON",
            label,
            check_style,
            hwnd,
            id,
            x,
            check_y,
            CHECK_WIDTH,
            ROW_HEIGHT,
        );
    }

    win::create_child_id(
        "STATIC",
        "热键、记录开关和字号立即生效；勾掉「超限写 .bin」= 超长文本和图片直接丢弃。其余项需重启。",
        label_style,
        hwnd,
        NOTE_ID,
        MARGIN,
        NOTE_TOP,
        CLIENT_WIDTH - MARGIN * 2,
        ROW_HEIGHT,
    );

    let button_y = BUTTON_TOP;
    win::create_child_id(
        "BUTTON",
        "保存",
        button_style | win::BS_DEFPUSHBUTTON,
        hwnd,
        ID_SAVE,
        MARGIN,
        button_y,
        BUTTON_WIDTH,
        BUTTON_HEIGHT,
    );
    win::create_child_id(
        "BUTTON",
        "取消",
        button_style,
        hwnd,
        ID_CANCEL,
        MARGIN + BUTTON_WIDTH + BUTTON_GAP,
        button_y,
        BUTTON_WIDTH,
        BUTTON_HEIGHT,
    );

    // The hotkey field records combinations instead of accepting text, which
    // means it has to see the keystrokes before the EDIT turns them into
    // characters. `field()` cannot be used here: the window is not in the
    // static yet.
    let subclass: win::SUBCLASSPROC = hotkey_proc;
    let hotkey_field = win::child_by_id(hwnd, ID_HOTKEY);
    let installed =
        unsafe { win::SetWindowSubclass(hotkey_field, subclass, HOTKEY_SUBCLASS_ID, 0) } != 0;

    if !installed {
        log::warn(&format!(
            "SetWindowSubclass failed for the hotkey field, err {}; it will not record combinations",
            win::last_error()
        ));
    }

    if WINDOW.set(hwnd).is_err() {
        log::error("settings window already created");
        return false;
    }

    log::info(&format!("settings window ready (hwnd {hwnd:#x})"));
    true
}

/// Places every control for the monitor's scale and hands it the matching font.
///
/// Done on each show rather than once at creation: the window opens on whichever
/// monitor the tray click came from, and those do not have to share a scale.
/// `WM_DPICHANGED` re-runs it when the window is dragged to another display.
fn layout(hwnd: HWND, scale: f64) {
    let margin = win::scaled(MARGIN, scale);
    let label_width = win::scaled(LABEL_WIDTH, scale);
    let field_width = win::scaled(FIELD_WIDTH, scale);
    let row_height = win::scaled(ROW_HEIGHT, scale);
    let row_step = win::scaled(ROW_STEP, scale);
    let field_x = margin + label_width + win::scaled(FIELD_GAP, scale);
    let font = win::ui_font_for_scale(scale);

    let place = |id: usize, x: i32, y: i32, width: i32, height: i32| {
        let control = win::child_by_id(hwnd, id);
        if control == 0 {
            return;
        }

        unsafe {
            win::SendMessageW(control, win::WM_SETFONT, font as usize, 1);
            win::SetWindowPos(
                control,
                0,
                x,
                y,
                width,
                height,
                win::SWP_NOACTIVATE,
            );
        }
    };

    for (index, (id, _)) in ROW_LABELS.iter().enumerate() {
        let y = margin + index as i32 * row_step;

        // The caption shares the field's top edge: both draw their text from the
        // top of their box, so an extra offset only pushes the label below the
        // text of the field it belongs to.
        place(LABEL_ID_BASE + index, margin, y, label_width, row_height);
        place(*id, field_x, y, field_width, row_height);
    }

    let check_width = win::scaled(CHECK_WIDTH, scale);
    let check_step = win::scaled(CHECK_STEP, scale);
    let check_y = win::scaled(CHECKS_TOP, scale);
    let checks = [
        ID_CAPTURE_TEXT,
        ID_CAPTURE_IMAGES,
        ID_CAPTURE_FILES,
        ID_WRITE_BLOBS,
    ];

    for (index, id) in checks.into_iter().enumerate() {
        let x = margin + index as i32 * check_step;
        place(id, x, check_y, check_width, row_height);
    }

    place(
        NOTE_ID,
        margin,
        win::scaled(NOTE_TOP, scale),
        win::scaled(CLIENT_WIDTH, scale) - margin * 2,
        row_height,
    );

    let button_y = win::scaled(BUTTON_TOP, scale);
    let button_width = win::scaled(BUTTON_WIDTH, scale);
    let button_height = win::scaled(BUTTON_HEIGHT, scale);

    place(ID_SAVE, margin, button_y, button_width, button_height);
    place(
        ID_CANCEL,
        margin + button_width + win::scaled(BUTTON_GAP, scale),
        button_y,
        button_width,
        button_height,
    );
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


    // Every size in this file is written at 96 DPI, so the monitor's scale has
    // to be applied by hand: Windows does not do it for a per-monitor-DPI
    // process, and a 4K display would otherwise get 96-DPI text. The window is the
    // one place the user's own scale applies on top of that.
    let scale = window_scale(win::dpi_at(cursor) as f64 / 96.0);
    let (width, height) = frame_size(scale);

    let left = area.left + (area.right - area.left - width) / 2;
    let top = area.top + (area.bottom - area.top - height) / 3;

    layout(hwnd, scale);

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
    set_text(ID_SETTINGS_SCALE, &current.settings_scale.to_string());

    set_checked(ID_CAPTURE_TEXT, current.capture_text);
    set_checked(ID_CAPTURE_IMAGES, current.capture_images);
    set_checked(ID_CAPTURE_FILES, current.capture_files);
    set_checked(ID_WRITE_BLOBS, current.write_blobs);
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

    // Kept for the "is this combination even free" probe below: re-registering
    // the combination that is already ours is not a conflict.
    let previous = updated.clone();

    let typed = win::window_text(field(ID_HOTKEY)).trim().to_string();
    let Some(hotkey) = settings::parse_hotkey(&typed) else {
        complain(&format!(
            "热键 “{typed}” 不能用。\n\n点进热键框直接按下组合键即可。需要至少一个修饰键（Ctrl/Alt/Shift/Win），\n只有 F1-F24 能单独使用——单独的字母或数字会吃掉全系统的那个键。"
        ));
        return;
    };

    let unchanged = settings::parse_hotkey(&previous.hotkey).map(Hotkey::text) == Some(hotkey.text());
    if !unchanged && !combination_is_free(hotkey) {
        complain(&format!(
            "“{}” 已经被别的程序占用了，换一个组合。",
            hotkey.text()
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

    let settings_scale = match parse_or(win::DEFAULT_SETTINGS_SCALE, ID_SETTINGS_SCALE) {
        Ok(value) => value,
        Err(reason) => {
            complain(&format!("设置窗口字号：{reason}"));
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

    // Refused rather than clamped: a scale that is silently changed under the
    // cursor is worse than one that is refused, and the range is on screen.
    if !(win::MIN_SETTINGS_SCALE..=win::MAX_SETTINGS_SCALE).contains(&settings_scale) {
        complain(&format!(
            "字号要在 {}% 到 {}% 之间，100% 就跟系统一样大。",
            win::MIN_SETTINGS_SCALE,
            win::MAX_SETTINGS_SCALE
        ));
        return;
    }
    let sync_root = win::window_text(field(ID_SYNC_ROOT)).trim().to_string();

    // Canonical spelling, so a hand-edited variant in settings.json is cleaned
    // up on the next save.
    updated.hotkey = hotkey.text();
    updated.sync_root_override = if sync_root.is_empty() {
        None
    } else {
        Some(sync_root)
    };
    updated.retention_days = retention;
    updated.max_blob_bytes = max_blob_mb as u64 * 1024 * 1024;
    updated.inline_text_limit = inline_limit as usize;
    updated.rescan_seconds = rescan as u64;
    updated.settings_scale = settings_scale;
    updated.capture_text = is_checked(ID_CAPTURE_TEXT);
    updated.capture_images = is_checked(ID_CAPTURE_IMAGES);
    updated.capture_files = is_checked(ID_CAPTURE_FILES);
    updated.write_blobs = is_checked(ID_WRITE_BLOBS);

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
        // Hiding takes the focus off the hotkey field, which is what puts the
        // global hotkey back; doing it here as well means the hotkey cannot be
        // left suspended if that notification never arrives.
        crate::apply_hotkey();

        unsafe {
            win::ShowWindow(hwnd, win::SW_HIDE);
        }
    }
}

/// The hotkey field records instead of accepting text: pressing a combination is
/// easier than spelling it, and it cannot be misspelled.
extern "system" fn hotkey_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    match message {
        win::WM_KEYDOWN | win::WM_SYSKEYDOWN => {
            record(wparam as u32);
            return 0;
        }

        // TranslateMessage turns the key into a character after this point, so
        // the keystroke has to be eaten here as well or it lands in the box.
        win::WM_CHAR | win::WM_SYSCHAR => return 0,

        _ => {}
    }

    unsafe { win::DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// Puts the pressed combination into the field. Modifiers are read from the
/// keyboard rather than collected from earlier messages, which is what makes
/// this work for combinations this process has not registered.
fn record(vk: u32) {
    // A lone modifier is not a combination yet, and a key with no name would
    // leave the field holding something that cannot be saved.
    if is_modifier(vk) {
        return;
    }

    if vk == win::VK_ESCAPE as u32 {
        // Escape puts back what is saved, so a mis-press is undoable.
        if let Some(current) = crate::current_settings() {
            set_text(ID_HOTKEY, &current.hotkey);
        }
        return;
    }

    let Some(hotkey) = Hotkey::new(held_modifiers(), vk) else {
        return;
    };

    log::info(&format!("hotkey field recorded {}", hotkey.text()));
    set_text(ID_HOTKEY, &hotkey.text());
}

fn held_modifiers() -> u32 {
    fn down(vk: u32) -> bool {
        // Bound first: a block directly followed by `<` reads as a type
        // argument list to the parser.
        let state = unsafe { win::GetKeyState(vk as i32) };
        state < 0
    }

    let mut modifiers = 0;

    if down(win::VK_CONTROL as u32) {
        modifiers |= win::MOD_CONTROL;
    }
    if down(win::VK_MENU as u32) {
        modifiers |= win::MOD_ALT;
    }
    if down(win::VK_SHIFT as u32) {
        modifiers |= win::MOD_SHIFT;
    }
    if down(win::VK_LWIN as u32) || down(win::VK_RWIN as u32) {
        modifiers |= win::MOD_WIN;
    }

    modifiers
}

fn is_modifier(vk: u32) -> bool {
    [
        win::VK_SHIFT as u32,
        win::VK_CONTROL as u32,
        win::VK_MENU as u32,
        win::VK_LWIN as u32,
        win::VK_RWIN as u32,
    ]
    .contains(&vk)
}

/// Registers the combination for a moment to find out whether anything else
/// already owns it. Done here so the failure is reported before the settings
/// are written, rather than afterwards by `apply_hotkey`.
fn combination_is_free(hotkey: Hotkey) -> bool {
    // A null window registers against this thread. Our own registration is
    // dropped while the field has focus, so this cannot collide with itself.
    if !win::register_hotkey(0, 0, hotkey.modifiers | win::MOD_NOREPEAT, hotkey.vk) {
        return false;
    }

    win::unregister_hotkey(0, 0);
    true
}

extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        win::WM_COMMAND => {
            // LOWORD is the control id, HIWORD the notification code.
            let id = wparam & 0xFFFF;
            let notification = ((wparam >> 16) & 0xFFFF) as u32;

            // The hotkey field stands the global hotkey down while it records,
            // otherwise pressing the registered combination would fire the popup
            // instead of reaching the field.
            if id as usize == ID_HOTKEY && notification == win::EN_SETFOCUS {
                crate::suspend_hotkey();
            } else if id as usize == ID_HOTKEY && notification == win::EN_KILLFOCUS {
                crate::apply_hotkey();
            }

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

        // Dragged onto a display with a different scale: the window takes the
        // size and the font that display needs instead of keeping the old ones
        // until it is closed and reopened.
        win::WM_DPICHANGED => {
            // LOWORD is the new DPI, and lparam points at the rectangle Windows
            // suggests for the new monitor. Only the position comes from it:
            // this layout is a fixed grid, so its size is computed from the
            // scale instead.
            let dpi = (wparam & 0xFFFF) as u32;
            let monitor = if dpi == 0 {
                win::dpi_scale_of(hwnd)
            } else {
                dpi as f64 / 96.0
            };
            let scale = window_scale(monitor);
            let suggested = unsafe { &*(lparam as *const win::RECT) };
            let (width, height) = frame_size(scale);

            unsafe {
                win::SetWindowPos(
                    hwnd,
                    0,
                    suggested.left,
                    suggested.top,
                    width,
                    height,
                    win::SWP_NOZORDER | win::SWP_NOACTIVATE,
                );
            }

            layout(hwnd, scale);
            0
        }

        _ => win::def_window_proc(hwnd, message, wparam, lparam),
    }
}
