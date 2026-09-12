//! The search popup.
//!
//! Raw Win32 on purpose: the search box is a real `EDIT` control, so IME, CJK
//! input, selection and the caret are the system's problem rather than mine. The
//! list is a real `LISTBOX` with owner-drawn rows, which keeps scrolling,
//! keyboard navigation and hit testing out of this file too.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::clipboard;
use crate::index::ClipSummary;
use crate::log;
use crate::store::Store;
use crate::win::{self, HBRUSH, HWND, LPARAM, LRESULT, WPARAM};

// Layout in logical pixels at 96 DPI; everything is scaled by the monitor's
// effective DPI at show time.
const WIDTH: i32 = 620;
const HEIGHT: i32 = 442;
const PAD: i32 = 10;
const SEARCH_HEIGHT: i32 = 30;
const GAP: i32 = 8;
const ROW_HEIGHT: i32 = 44;
const MAX_RESULTS: usize = 300;
const SUBCLASS_ID: usize = 1;

/// COLORREF is 0x00BBGGRR, not RGB.
const COLOR_BG: u32 = 0x001E_1E1E;
const COLOR_INPUT_BG: u32 = 0x002A_2A2A;
const COLOR_SELECTED: u32 = 0x0099_5A3C;
const COLOR_TEXT: u32 = 0x00E6_E6E6;
const COLOR_META: u32 = 0x008C_8C8C;
const COLOR_PIN: u32 = 0x004A_A2D2;

struct Popup {
    hwnd: HWND,
    search: HWND,
    list: HWND,
    store: Arc<Store>,
    items: Mutex<Vec<ClipSummary>>,
    /// Window that had focus before the popup opened: the paste target.
    target: AtomicIsize,
    visible: AtomicBool,
    font_scale: AtomicIsize,
    font_main: AtomicIsize,
    font_meta: AtomicIsize,
    brush_bg: HBRUSH,
    brush_input: HBRUSH,
    brush_selected: HBRUSH,
}

static POPUP: OnceLock<Popup> = OnceLock::new();

fn popup() -> Option<&'static Popup> {
    POPUP.get()
}

/// Creates the popup hidden. Doing this at startup rather than on first use
/// keeps the first hotkey press free of window-creation latency.
pub fn create(store: Arc<Store>) -> bool {
    let window_proc: win::WNDPROC = window_proc;
    let hwnd = win::create_window(
        "ClipPlus.Popup",
        "ClipPlus",
        window_proc,
        win::WS_POPUP,
        win::WS_EX_TOOLWINDOW,
        0,
        0,
        WIDTH,
        HEIGHT,
    );

    if hwnd == 0 {
        log::error(&format!(
            "popup CreateWindowExW failed, err {}",
            win::last_error()
        ));
        return false;
    }

    let search = win::create_child(
        "EDIT",
        "",
        win::WS_CHILD | win::WS_VISIBLE | win::ES_AUTOHSCROLL,
        hwnd,
        PAD,
        PAD,
        WIDTH - PAD * 2,
        SEARCH_HEIGHT,
    );

    let list_style = win::WS_CHILD
        | win::WS_VISIBLE
        | win::WS_VSCROLL
        | win::LBS_NOTIFY
        | win::LBS_OWNERDRAWFIXED
        | win::LBS_HASSTRINGS
        | win::LBS_NOINTEGRALHEIGHT;

    let list_top = PAD + SEARCH_HEIGHT + GAP;
    let list = win::create_child(
        "LISTBOX",
        "",
        list_style,
        hwnd,
        PAD,
        list_top,
        WIDTH - PAD * 2,
        HEIGHT - list_top - PAD,
    );

    if search == 0 || list == 0 {
        log::error("popup child controls could not be created");
        return false;
    }

    let state = Popup {
        hwnd,
        search,
        list,
        store: Arc::clone(&store),
        items: Mutex::new(Vec::new()),
        target: AtomicIsize::new(0),
        visible: AtomicBool::new(false),
        font_scale: AtomicIsize::new(0),
        font_main: AtomicIsize::new(0),
        font_meta: AtomicIsize::new(0),
        brush_bg: unsafe { win::CreateSolidBrush(COLOR_BG) },
        brush_input: unsafe { win::CreateSolidBrush(COLOR_INPUT_BG) },
        brush_selected: unsafe { win::CreateSolidBrush(COLOR_SELECTED) },
    };

    if POPUP.set(state).is_err() {
        log::error("popup already created");
        return false;
    }

    // The EDIT eats the keys we care about, so intercept them in a subclass
    // proc and forward everything else to the control's own handling.
    let subclass: win::SUBCLASSPROC = search_proc;
    let installed = unsafe {
        win::SetWindowSubclass(search, subclass, SUBCLASS_ID, 0)
    } != 0;

    if !installed {
        log::warn(&format!(
            "SetWindowSubclass failed, err {}; keyboard shortcuts will not work",
            win::last_error()
        ));
    }

    log::info(&format!("popup window ready (hwnd {hwnd:#x})"));
    true
}

pub fn toggle() {
    if is_visible() {
        hide();
    } else {
        show();
    }
}

pub fn is_visible() -> bool {
    popup().map(|p| p.visible.load(Ordering::SeqCst)).unwrap_or(false)
}

pub fn show() {
    let Some(p) = popup() else {
        return;
    };

    // Captured BEFORE this window takes focus, otherwise it is already too late.
    let previous = unsafe { win::GetForegroundWindow() };
    p.target.store(
        if previous == p.hwnd { 0 } else { previous },
        Ordering::SeqCst,
    );

    let empty = win::wide("");
    unsafe {
        win::SetWindowTextW(p.search, empty.as_ptr());
    }
    reload();

    let cursor = win::cursor_position();
    let area = win::work_area_at(cursor);
    let scale = win::dpi_at(cursor) as f64 / 96.0;

    let width = (WIDTH as f64 * scale) as i32;
    let height = (HEIGHT as f64 * scale) as i32;
    let pad = (PAD as f64 * scale) as i32;
    let search_height = (SEARCH_HEIGHT as f64 * scale) as i32;
    let gap = (GAP as f64 * scale) as i32;

    let mut left = cursor.x + pad;
    let mut top = cursor.y + pad;
    if left + width > area.right {
        left = area.right - width;
    }
    if top + height > area.bottom {
        top = cursor.y - height - pad;
    }
    left = left.max(area.left);
    top = top.max(area.top);

    ensure_fonts(p, scale);

    let inner = width - pad * 2;
    let list_top = pad + search_height + gap;
    let list_height = height - list_top - pad;

    unsafe {
        win::SendMessageW(p.list, win::LB_SETITEMHEIGHT, 0, ROW_HEIGHT as LPARAM);
        win::SetWindowPos(p.search, 0, pad, pad, inner, search_height, win::SWP_NOACTIVATE);
        win::SetWindowPos(p.list, 0, pad, list_top, inner, list_height, win::SWP_NOACTIVATE);
        win::SetWindowPos(
            p.hwnd,
            win::HWND_TOPMOST,
            left,
            top,
            width,
            height,
            win::SWP_SHOWWINDOW,
        );
        win::SetFocus(p.search);
    }

    log::info(&format!("popup shown at {left},{top} {width}x{height} ({} rows)", {
        p.items.lock().unwrap_or_else(|e| e.into_inner()).len()
    }));
    p.visible.store(true, Ordering::SeqCst);
}

pub fn hide() {
    let Some(p) = popup() else {
        return;
    };

    unsafe {
        win::ShowWindow(p.hwnd, win::SW_HIDE);
    }
    p.visible.store(false, Ordering::SeqCst);
}

fn ensure_fonts(p: &'static Popup, scale: f64) {
    let key = (scale * 100.0) as isize;
    if p.font_scale.load(Ordering::SeqCst) == key && key != 0 {
        return;
    }

    let face = win::wide("Microsoft YaHei UI");
    let main_height = -((16.0 * scale) as i32);
    let meta_height = -((12.0 * scale) as i32);

    unsafe {
        let main = win::CreateFontW(
            main_height,
            0,
            0,
            0,
            win::FW_NORMAL,
            0,
            0,
            0,
            win::CHARSET_DEFAULT,
            0,
            0,
            win::QUALITY_CLEARTYPE,
            0,
            face.as_ptr(),
        );
        let meta = win::CreateFontW(
            meta_height,
            0,
            0,
            0,
            win::FW_NORMAL,
            0,
            0,
            0,
            win::CHARSET_DEFAULT,
            0,
            0,
            win::QUALITY_CLEARTYPE,
            0,
            face.as_ptr(),
        );

        let old_main = p.font_main.swap(main, Ordering::SeqCst);
        let old_meta = p.font_meta.swap(meta, Ordering::SeqCst);
        if old_main != 0 {
            win::DeleteObject(old_main);
        }
        if old_meta != 0 {
            win::DeleteObject(old_meta);
        }
    }

    p.font_scale.store(key, Ordering::SeqCst);
}

fn reload() {
    let Some(p) = popup() else {
        return;
    };

    let filter = window_text(p.search);
    let summaries = p.store.query(&filter, MAX_RESULTS);

    unsafe {
        win::SendMessageW(p.list, win::LB_RESETCONTENT, 0, 0);

        for summary in &summaries {
            // The listbox keeps its own copy under LBS_HASSTRINGS, so this
            // temporary only has to outlive the call.
            let text = win::wide(&summary.preview);
            win::SendMessageW(p.list, win::LB_ADDSTRING, 0, text.as_ptr() as LPARAM);
        }

        if !summaries.is_empty() {
            win::SendMessageW(p.list, win::LB_SETCURSEL, 0, 0);
        }

        win::InvalidateRect(p.list, std::ptr::null(), 1);
    }

    *p.items.lock().unwrap_or_else(|e| e.into_inner()) = summaries;
}

fn commit() {
    let Some(p) = popup() else {
        return;
    };

    let Some(stem) = selected_stem() else {
        return;
    };

    let target = p.target.load(Ordering::SeqCst);

    let Some(payload) = p.store.read_payload(&stem) else {
        log::warn(&format!("nothing pasteable for {stem}"));
        hide();
        return;
    };

    hide();

    if !clipboard::write(&payload) {
        log::warn("clipboard write failed; not injecting a keystroke");
        return;
    }

    let started = Instant::now();
    if target != 0 {
        win::set_foreground(target);
    }

    // Poll instead of sleeping a flat amount. The C# build used a fixed 120 ms
    // here, which was the single largest cost on the whole paste path.
    let deadline = started + Duration::from_millis(300);
    while Instant::now() < deadline {
        if unsafe { win::GetForegroundWindow() } == target {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // A floor: the foreground switch landing does not mean the target's focused
    // control is ready to receive the keystroke yet.
    std::thread::sleep(Duration::from_millis(10));

    win::send_ctrl_v();
    log::info(&format!(
        "paste-back took {} ms",
        started.elapsed().as_millis()
    ));
}

fn toggle_pin() {
    let Some(p) = popup() else {
        return;
    };

    let Some(index) = selected_index() else {
        return;
    };

    let (stem, pinned) = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        match items.get(index) {
            Some(summary) => (summary.stem.clone(), summary.pinned),
            None => return,
        }
    };

    if !p.store.set_pinned(&stem, !pinned) {
        return;
    }

    // Pinning moves the row to the top, so follow the item rather than the index.
    reload();

    let moved = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        items.iter().position(|summary| summary.stem == stem)
    };

    if let Some(position) = moved {
        unsafe {
            win::SendMessageW(p.list, win::LB_SETCURSEL, position, 0);
        }
    }
}

fn selected_index() -> Option<usize> {
    let p = popup()?;
    let index = unsafe { win::SendMessageW(p.list, win::LB_GETCURSEL, 0, 0) } as i32;
    if index < 0 {
        None
    } else {
        Some(index as usize)
    }
}

fn selected_stem() -> Option<String> {
    let p = popup()?;
    let index = selected_index()?;
    let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
    items.get(index).map(|summary| summary.stem.clone())
}

fn move_selection(delta: i32) {
    let Some(p) = popup() else {
        return;
    };

    let count = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        items.len() as i32
    };
    if count == 0 {
        return;
    }

    let next = match selected_index() {
        None => {
            if delta > 0 {
                0
            } else {
                count - 1
            }
        }
        Some(current) => (current as i32 + delta).clamp(0, count - 1),
    };

    unsafe {
        win::SendMessageW(p.list, win::LB_SETCURSEL, next as usize, 0);
    }
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let length = win::GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return String::new();
        }

        let mut buffer = vec![0u16; length as usize + 1];
        let copied = win::GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        if copied <= 0 {
            return String::new();
        }

        buffer.truncate(copied as usize);
        String::from_utf16_lossy(&buffer)
    }
}

// ------------------------------------------------------------------ window procs

extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        win::WM_CTLCOLOREDIT => {
            // The parent paints its children's non-client cruft, so the colour
            // has to be set here and the brush handed back.
            if let Some(p) = popup() {
                let dc = wparam as win::HDC;
                unsafe {
                    win::SetTextColor(dc, COLOR_TEXT);
                    win::SetBkColor(dc, COLOR_INPUT_BG);
                }
                return p.brush_input as LRESULT;
            }
            0
        }

        win::WM_DRAWITEM => {
            draw_item(lparam);
            1
        }

        win::WM_COMMAND => {
            let notification = ((wparam >> 16) & 0xFFFF) as u32;

            if notification == win::EN_CHANGE {
                reload();
            } else if notification == win::LBN_DBLCLK {
                commit();
            }
            0
        }

        win::WM_ACTIVATE => {
            // WA_INACTIVE == 0: the user clicked somewhere else.
            if (wparam & 0xFFFF) == 0 {
                hide();
            }
            0
        }

        _ => win::def_window_proc(hwnd, message, wparam, lparam),
    }
}

/// The search box swallows the keys we care about, so they are intercepted here
/// and everything else is handed back to the control.
extern "system" fn search_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    if message == win::WM_KEYDOWN {
        let key = wparam as i32;
        // VK_CONTROL is declared as u16 for SendInput; GetKeyState wants i32.
        let control_down = unsafe { win::GetKeyState(win::VK_CONTROL as i32) } < 0;

        match key {
            win::VK_ESCAPE => {
                hide();
                return 0;
            }
            win::VK_RETURN => {
                commit();
                return 0;
            }
            win::VK_UP => {
                move_selection(-1);
                return 0;
            }
            win::VK_DOWN => {
                move_selection(1);
                return 0;
            }
            win::VK_PRIOR => {
                move_selection(-8);
                return 0;
            }
            win::VK_NEXT => {
                move_selection(8);
                return 0;
            }
            win::VK_P if control_down => {
                toggle_pin();
                return 0;
            }
            _ => {}
        }
    }

    unsafe { win::DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn draw_item(lparam: LPARAM) {
    let Some(p) = popup() else {
        return;
    };

    let Some(item) = (unsafe { (lparam as *const win::DRAWITEMSTRUCT).as_ref() }) else {
        return;
    };

    let index = item.item_id as usize;
    let summary = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        match items.get(index) {
            Some(summary) => summary.clone(),
            None => return,
        }
    };

    let selected = item.item_state & win::ODS_SELECTED != 0;
    let font_main = p.font_main.load(Ordering::SeqCst);
    let font_meta = p.font_meta.load(Ordering::SeqCst);

    unsafe {
        let dc = item.hdc;
        let rect = item.rc_item;

        let background = if selected { p.brush_selected } else { p.brush_bg };
        win::FillRect(dc, &rect, background);
        win::SetBkMode(dc, win::TRANSPARENT_BK);

        let mut left = rect.left + 10;

        // First line: optional pin star, then the preview.
        let mut line = win::RECT {
            left,
            top: rect.top + 6,
            right: rect.right - 10,
            bottom: rect.top + 6 + 18,
        };

        let previous = win::SelectObject(dc, font_main);

        if summary.pinned {
            let mut star = line;
            star.right = left + 18;
            win::SetTextColor(dc, COLOR_PIN);
            let text = win::wide("★");
            win::DrawTextW(dc, text.as_ptr(), -1, &mut star, text_flags());
            left += 20;
        }

        let mut preview = line;
        preview.left = left;
        win::SetTextColor(dc, COLOR_TEXT);
        let text = win::wide(&summary.preview);
        win::DrawTextW(dc, text.as_ptr(), -1, &mut preview, text_flags());
        line = preview;

        // Second line: kind, time, origin.
        let mut meta = win::RECT {
            left: line.left,
            top: rect.top + 25,
            right: rect.right - 10,
            bottom: rect.top + 25 + 15,
        };

        win::SelectObject(dc, font_meta);
        win::SetTextColor(dc, COLOR_META);
        let text = win::wide(&summary.meta);
        win::DrawTextW(dc, text.as_ptr(), -1, &mut meta, text_flags());

        win::SelectObject(dc, previous);
    }
}

fn text_flags() -> u32 {
    win::DT_LEFT | win::DT_SINGLELINE | win::DT_VCENTER | win::DT_END_ELLIPSIS | win::DT_NOPREFIX
}
