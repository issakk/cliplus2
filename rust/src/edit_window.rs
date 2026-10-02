//! The clip editor.
//!
//! A plain top-level window with system controls, the settings window's
//! conventions: a dialog face rather than the popup's dark one, created once at
//! startup and shown hidden, every size in logical pixels scaled at layout
//! time. One multiline `EDIT` holds the clip's text, 保存 writes it through
//! `Store::edit_text`, Esc or 取消 throws the typing away.
//!
//! The popup stays open underneath on purpose — `modal_open` covers it the way
//! it covers a message box — so saving drops the user back where they were,
//! with the row's new text on screen.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::log;
use crate::win::{self, scaled, HWND, LPARAM, LRESULT, WPARAM};

/// Client area in logical pixels; the frame is added around it at creation.
const CLIENT_WIDTH: i32 = 560;
const CLIENT_HEIGHT: i32 = 360;

const MARGIN: i32 = 12;
const BUTTON_WIDTH: i32 = 88;
const BUTTON_HEIGHT: i32 = 26;
const BUTTON_GAP: i32 = 8;

/// Smallest the user can drag it down to: enough editor to be worth opening.
const MIN_WIDTH: i32 = 380;
const MIN_HEIGHT: i32 = 240;

const ID_TEXT: usize = 1;
const ID_SAVE: usize = 2;
const ID_CANCEL: usize = 3;
const ID_HINT: usize = 4;

/// The one control that has to intercept its own keystrokes: Esc and
/// Ctrl+Enter mean cancel and save, and the EDIT would otherwise turn the
/// first into nothing and the second into a newline.
const EDIT_SUBCLASS_ID: usize = 1;

static WINDOW: OnceLock<HWND> = OnceLock::new();
/// The stem the editor is holding. `None` whenever the editor is not showing
/// anything, which is also the guard against saving into the wrong row.
static EDITING: Mutex<Option<String>> = Mutex::new(None);
/// The scale the layout was last run at, so `WM_SIZE` re-layouts cheaply.
static SCALE: AtomicIsize = AtomicIsize::new(100);

/// The window's frame style: resizable, a real title bar to drag it by, and
/// `WS_CLIPCHILDREN` so the editor does not flicker on every resize.
const WINDOW_STYLE: u32 = win::WS_CAPTION
    | win::WS_SYSMENU
    | win::WS_THICKFRAME
    | win::WS_MINIMIZEBOX
    | win::WS_CLIPCHILDREN;

/// The frame around a client area of the layout's size, at one scale.
fn frame_size(scale: f64) -> (i32, i32) {
    let mut frame = win::RECT {
        left: 0,
        top: 0,
        right: scaled(CLIENT_WIDTH, scale),
        bottom: scaled(CLIENT_HEIGHT, scale),
    };

    unsafe {
        win::AdjustWindowRectEx(&mut frame, WINDOW_STYLE, 0, 0);
    }

    (frame.right - frame.left, frame.bottom - frame.top)
}

fn current_scale() -> f64 {
    match SCALE.load(Ordering::SeqCst) {
        key if key > 0 => key as f64 / 100.0,
        _ => 1.0,
    }
}

fn edit() -> HWND {
    WINDOW
        .get()
        .map(|hwnd| win::child_by_id(*hwnd, ID_TEXT))
        .unwrap_or(0)
}

fn editing_stem() -> Option<String> {
    EDITING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// Created once at startup, like the other windows: the first 编辑 costs no
/// window-creation latency.
pub fn create() -> bool {
    let window_proc: win::WNDPROC = window_proc;
    let (width, height) = frame_size(1.0);

    let hwnd = win::create_window(
        "ClipPlus.Edit",
        "ClipPlus 编辑",
        window_proc,
        WINDOW_STYLE,
        0,
        0,
        0,
        width,
        height,
        win::dialog_brush(),
    );

    if hwnd == 0 {
        log::error(&format!(
            "edit window CreateWindowExW failed, err {}",
            win::last_error()
        ));
        return false;
    }

    let edit_style = win::WS_CHILD
        | win::WS_VISIBLE
        | win::WS_BORDER
        | win::WS_TABSTOP
        | win::WS_VSCROLL
        | win::ES_MULTILINE
        | win::ES_AUTOVSCROLL;
    let text = win::create_child_id("EDIT", "", edit_style, hwnd, ID_TEXT, 0, 0, 0, 0);

    let button_style = win::WS_CHILD | win::WS_VISIBLE | win::WS_TABSTOP | win::BS_PUSHBUTTON;
    let save = win::create_child_id(
        "BUTTON",
        "保存",
        button_style | win::BS_DEFPUSHBUTTON,
        hwnd,
        ID_SAVE,
        0,
        0,
        0,
        0,
    );
    let cancel = win::create_child_id(
        "BUTTON",
        "取消",
        button_style,
        hwnd,
        ID_CANCEL,
        0,
        0,
        0,
        0,
    );

    let hint = win::create_child_id(
        "STATIC",
        "Ctrl+Enter 保存 · Esc 取消",
        win::WS_CHILD | win::WS_VISIBLE,
        hwnd,
        ID_HINT,
        0,
        0,
        0,
        0,
    );

    if text == 0 || save == 0 || cancel == 0 || hint == 0 {
        log::error("edit window child controls could not be created");
        return false;
    }

    if WINDOW.set(hwnd).is_err() {
        log::error("edit window already created");
        return false;
    }

    let subclass: win::SUBCLASSPROC = edit_proc;
    if unsafe { win::SetWindowSubclass(text, subclass, EDIT_SUBCLASS_ID, 0) } == 0 {
        log::warn("SetWindowSubclass failed for the editor; Ctrl+Enter and Esc will not work");
    }

    // Dark look, same palette as the popup — see settings_window.
    win::dark_title_bar(hwnd);
    win::dark_theme(hwnd);
    win::dark_theme_children(hwnd);

    log::info(&format!("edit window ready (hwnd {hwnd:#x})"));
    true
}

/// Fills the editor with one clip's text and puts it in front. Reports whether
/// the editor actually opened — the caller holds the popup open on its behalf
/// and has to know when to release it again.
///
/// Already showing? The new clip replaces what was there — the single window
/// is the editor, and two open editors over one store would race each other's
/// save anyway.
pub fn show(stem: &str, text: &str) -> bool {
    let Some(hwnd) = WINDOW.get().copied() else {
        return false;
    };
    let text_box = edit();
    if text_box == 0 {
        return false;
    }

    // Lone `\n` does not read as a line break to an EDIT control; normalize
    // for display and save whatever the control holds afterwards. A clip that
    // was saved with bare `\n` line endings comes out with `\r\n` once edited
    // — an accepted consequence of editing, not a silent rewrite.
    let display = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let wide = win::wide(&display);
    unsafe {
        win::SetWindowTextW(text_box, wide.as_ptr());
    }

    *EDITING.lock().unwrap_or_else(|p| p.into_inner()) = Some(stem.to_string());

    // Centred on the monitor the pointer is on — the same convention the other
    // windows run under.
    let cursor = win::cursor_position();
    let area = win::work_area_at(cursor);
    let scale = win::dpi_at(cursor) as f64 / 96.0;
    let (width, height) = frame_size(scale);

    let left = area.left + (area.right - area.left - width) / 2;
    let top = area.top + (area.bottom - area.top - height) / 3;

    // Sized before it is shown, and laid out in between: `layout` measures the
    // real client area, which only says the right thing once the resize has
    // landed — otherwise the first frame on a scaled monitor places every
    // control for 96 DPI.
    unsafe {
        win::SetWindowPos(
            hwnd,
            0,
            left,
            top,
            width,
            height,
            win::SWP_NOACTIVATE | win::SWP_NOZORDER,
        );
    }

    layout(scale);

    unsafe {
        win::ShowWindow(hwnd, win::SW_SHOW);
        win::set_foreground(hwnd);
        win::SetFocus(text_box);
    }

    true
}

fn hide() {
    if let Some(hwnd) = WINDOW.get().copied() {
        unsafe {
            win::ShowWindow(hwnd, win::SW_HIDE);
        }
    }
    *EDITING.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// Saves what the editor holds into the clip it was opened for. An error keeps
/// the window up — the text the user typed is the thing being complained
/// about, and closing would throw it away.
fn save() {
    let Some(stem) = editing_stem() else {
        cancel();
        return;
    };
    let Some(store) = crate::store() else {
        return;
    };

    let text = win::window_text(edit());
    match store.edit_text(&stem, &text) {
        Ok(()) => {
            log::info(&format!("saved an edit to {stem}"));
            hide();
            notify_finished(Some(stem));
        }
        Err(reason) => {
            win::message_box(
                "ClipPlus 编辑",
                &reason,
                win::MB_OK | win::MB_ICONWARNING,
            );
        }
    }
}

/// Throws the typing away and closes. The popup learns of it either way: even
/// a cancel has to release the `modal_open` hold and hand the focus back.
fn cancel() {
    hide();
    notify_finished(None);
}

/// 编辑结束的去处。打开编辑器的 UI 在 `set_finish_callback` 里登记回调,
/// 默认(没有 UI 登记)只记日志。
static FINISH_CALLBACK: Mutex<Option<Box<dyn Fn(Option<String>) + Send>>> = Mutex::new(None);

pub fn set_finish_callback(callback: Box<dyn Fn(Option<String>) + Send>) {
    *FINISH_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()) = Some(callback);
}

fn notify_finished(saved: Option<String>) {
    let callback = FINISH_CALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    match callback.as_ref() {
        Some(callback) => callback(saved),
        // egui 弹窗在 run_native 之前就登记好了;走到这里说明编辑窗在
        // 没有 UI 的情况下被打开过,收尾无从谈起,只能记下来。
        None => {
            log::warn(&format!(
                "edit window finished (saved = {}) but no owner was registered",
                saved.is_some()
            ));
        }
    }
}

/// Places every control for the scale and hands them the matching font.
fn layout(scale: f64) {
    let Some(hwnd) = WINDOW.get().copied() else {
        return;
    };

    SCALE.store((scale * 100.0) as isize, Ordering::SeqCst);

    let margin = scaled(MARGIN, scale);
    let (client_w, client_h) = match win::client_size(hwnd) {
        Some(size) => size,
        None => (scaled(CLIENT_WIDTH, scale), scaled(CLIENT_HEIGHT, scale)),
    };

    let button_width = scaled(BUTTON_WIDTH, scale);
    let button_height = scaled(BUTTON_HEIGHT, scale);
    let button_gap = scaled(BUTTON_GAP, scale);
    let buttons_top = client_h - margin - button_height;

    let places = [
        (
            ID_TEXT,
            margin,
            margin,
            client_w - margin * 2,
            buttons_top - margin * 2,
        ),
        (
            ID_SAVE,
            client_w - margin - button_width * 2 - button_gap,
            buttons_top,
            button_width,
            button_height,
        ),
        (
            ID_CANCEL,
            client_w - margin - button_width,
            buttons_top,
            button_width,
            button_height,
        ),
        (
            ID_HINT,
            margin,
            buttons_top,
            client_w - margin * 3 - button_width * 2 - button_gap,
            button_height,
        ),
    ];

    let font = win::ui_font_for_scale(scale);
    for (id, x, y, width, height) in places {
        let control = win::child_by_id(hwnd, id);
        if control == 0 {
            continue;
        }

        unsafe {
            win::SendMessageW(control, win::WM_SETFONT, font as usize, 1);
            win::SetWindowPos(control, 0, x, y, width, height, win::SWP_NOACTIVATE);
        }
    }
}

extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        win::WM_COMMAND => {
            let id = wparam & 0xFFFF;
            let notification = ((wparam >> 16) & 0xFFFF) as u32;

            if notification == win::BN_CLICKED || notification == 0 {
                match id as usize {
                    ID_SAVE => save(),
                    ID_CANCEL => cancel(),
                    _ => {}
                }
            }
            0
        }

        win::WM_CLOSE => {
            cancel();
            0
        }

        win::WM_SIZE => {
            layout(current_scale());
            0
        }

        win::WM_GETMINMAXINFO => {
            win::def_window_proc(hwnd, message, wparam, lparam);

            if let Some(info) = unsafe { (lparam as *mut win::MINMAXINFO).as_mut() } {
                let scale = current_scale();
                info.pt_min_track_size = win::POINT {
                    x: scaled(MIN_WIDTH, scale),
                    y: scaled(MIN_HEIGHT, scale),
                };
            }
            0
        }

        // Dragged onto another display: take the size and the layout that
        // display needs instead of keeping the old ones until close.
        win::WM_DPICHANGED => {
            let suggested = unsafe { &*(lparam as *const win::RECT) };
            let scale = win::dpi_scale_of(hwnd);

            unsafe {
                win::SetWindowPos(
                    hwnd,
                    0,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    win::SWP_NOZORDER | win::SWP_NOACTIVATE,
                );
            }

            layout(scale);
            0
        }

        // Dark palette, shared with settings_window and the popup. The big
        // multiline EDIT is the WM_CTLCOLOREDIT case.
        win::WM_CTLCOLOREDIT | win::WM_CTLCOLORLISTBOX => {
            win::set_dialog_text(wparam as win::HDC);
            win::dialog_input_brush() as win::LRESULT
        }
        win::WM_CTLCOLORSTATIC => {
            // The key hint beside the buttons is secondary text.
            win::dialog_static_text(wparam as win::HDC, lparam as win::HWND, &[ID_HINT]);
            win::dialog_brush() as win::LRESULT
        }

        _ => win::def_window_proc(hwnd, message, wparam, lparam),
    }
}

/// The editor's keystrokes: Esc cancels, Ctrl+Enter saves. Every other key —
/// arrows, clipboard shortcuts, IME composition — is the EDIT's own business.
extern "system" fn edit_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    if message == win::WM_KEYDOWN {
        match wparam as i32 {
            win::VK_ESCAPE => {
                cancel();
                return 0;
            }
            win::VK_RETURN => {
                let control_down = unsafe { win::GetKeyState(win::VK_CONTROL as i32) } < 0;
                if control_down {
                    save();
                    return 0;
                }
                // Bare Enter falls through: inside a text editor it is a
                // newline, exactly what the user is there to type.
            }
            _ => {}
        }
    }

    unsafe { win::DefSubclassProc(hwnd, message, wparam, lparam) }
}
