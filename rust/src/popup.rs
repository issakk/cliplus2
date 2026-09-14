//! The search popup.
//!
//! Raw Win32 on purpose: the search box is a real `EDIT` control, so IME, CJK
//! input, selection and the caret are the system's problem rather than mine. The
//! list is a real `LISTBOX` with owner-drawn rows, which keeps scrolling,
//! keyboard navigation and hit testing out of this file too.
//!
//! Every pixel value below is a *logical* pixel at 96 DPI and is multiplied by
//! the monitor's scale factor before use — including the row layout, which is
//! what keeps the two text lines from colliding on a scaled display.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::clipboard;
use crate::index::ClipSummary;
use crate::log;
use crate::store::Store;
use crate::win::{self, HBRUSH, HWND, LPARAM, LRESULT, WPARAM};

const WIDTH: i32 = 620;
const PAD: i32 = 10;
const SEARCH_HEIGHT: i32 = 30;
const GAP: i32 = 8;

/// Chosen so the list holds exactly eight whole rows: 48 + 46*8 + 10 = 426.
const HEIGHT: i32 = 426;
const ROW_HEIGHT: i32 = 46;
const LINE1_TOP: i32 = 4;
const LINE1_HEIGHT: i32 = 22;
const LINE2_TOP: i32 = 26;
const LINE2_HEIGHT: i32 = 17;

/// How far the popup is pushed away from the cursor.
const CURSOR_OFFSET: i32 = 18;

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
    /// Scale factor x100, so a plain integer atomic can carry it.
    scale: AtomicIsize,
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

fn scaled(value: i32, scale: f64) -> i32 {
    (value as f64 * scale).round() as i32
}

/// Current scale factor, defaulting to 1.0 before the first show.
fn current_scale() -> f64 {
    match popup() {
        Some(p) => match p.scale.load(Ordering::SeqCst) {
            key if key > 0 => key as f64 / 100.0,
            _ => 1.0,
        },
        None => 1.0,
    }
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
        0, // no class background: the popup fills itself in WM_ERASEBKGND
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
        scale: AtomicIsize::new(100),
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
    let installed = unsafe { win::SetWindowSubclass(search, subclass, SUBCLASS_ID, 0) } != 0;

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

    let width = scaled(WIDTH, scale);
    let height = scaled(HEIGHT, scale);
    let pad = scaled(PAD, scale);
    let search_height = scaled(SEARCH_HEIGHT, scale);
    let gap = scaled(GAP, scale);
    let offset = scaled(CURSOR_OFFSET, scale);

    // Prefer just below-right of the cursor; flip whichever axis would overflow,
    // then clamp, so a cursor in a screen corner still gets a fully visible
    // window rather than one hanging off the edge.
    let mut left = cursor.x + offset;
    if left + width > area.right {
        left = cursor.x - offset - width;
    }

    let mut top = cursor.y + offset;
    if top + height > area.bottom {
        top = cursor.y - offset - height;
    }

    left = left.clamp(area.left, (area.right - width).max(area.left));
    top = top.clamp(area.top, (area.bottom - height).max(area.top));

    ensure_fonts(p, scale);

    let inner = width - pad * 2;
    let list_top = pad + search_height + gap;
    let list_height = height - list_top - pad;

    unsafe {
        // Scaled: the row boxes have to grow with the font, or the two lines
        // overlap. This is the bug that made the list look wrong at any DPI
        // above 100%.
        let row_height = scaled(ROW_HEIGHT, scale) as LPARAM;
        win::SendMessageW(p.list, win::LB_SETITEMHEIGHT, 0, row_height);

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

    let rows = p.items.lock().unwrap_or_else(|e| e.into_inner()).len();
    log::info(&format!(
        "popup shown at {left},{top} {width}x{height} scale {scale:.2} ({rows} rows)"
    ));
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
    if p.scale.load(Ordering::SeqCst) == key && p.font_main.load(Ordering::SeqCst) != 0 {
        return;
    }

    let face = win::wide("Microsoft YaHei UI");
    let main_height = -scaled(16, scale);
    let meta_height = -scaled(12, scale);

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

    p.scale.store(key, Ordering::SeqCst);
}

fn reload() {
    let Some(p) = popup() else {
        return;
    };

    let filter = win::window_text(p.search);
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

    // Poll instead of sleeping a flat amount: this wait is paid on every single
    // paste, and a flat sleep here was the largest cost on the whole path.
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

// ------------------------------------------------------------------ window procs

extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        // The window class has no background brush and this window is composed
        // by DWM, so without this the padding around the controls is whatever
        // happened to be there.
        win::WM_ERASEBKGND => {
            if let Some(p) = popup() {
                let dc = wparam as win::HDC;
                let mut rect = win::RECT::default();
                unsafe {
                    win::GetClientRect(hwnd, &mut rect);
                    win::FillRect(dc, &rect, p.brush_bg);
                }
                return 1; // erased
            }
            0
        }

        win::WM_CTLCOLOREDIT => {
            // The parent paints its children's backgrounds, so the colour is set
            // here and the brush handed back.
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

        // The list is owner-drawn, so the rows are ours, but the control still
        // erases itself with this brush — including the empty space below the
        // last row.
        win::WM_CTLCOLORLISTBOX => {
            if let Some(p) = popup() {
                let dc = wparam as win::HDC;
                unsafe {
                    win::SetTextColor(dc, COLOR_TEXT);
                    win::SetBkColor(dc, COLOR_BG);
                }
                return p.brush_bg as LRESULT;
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
    let scale = current_scale();

    // Same scaling as the font sizes and LB_SETITEMHEIGHT: if these boxes do not
    // grow with the font, the two lines collide.
    let inset = scaled(10, scale);
    let gap = scaled(20, scale);
    let line1_top = scaled(LINE1_TOP, scale);
    let line1_height = scaled(LINE1_HEIGHT, scale);
    let line2_top = scaled(LINE2_TOP, scale);
    let line2_height = scaled(LINE2_HEIGHT, scale);
    let star_width = scaled(18, scale);

    unsafe {
        let dc = item.hdc;
        let rect = item.rc_item;

        let background = if selected { p.brush_selected } else { p.brush_bg };
        win::FillRect(dc, &rect, background);
        win::SetBkMode(dc, win::TRANSPARENT_BK);

        let mut left = rect.left + inset;

        let line1 = win::RECT {
            left,
            top: rect.top + line1_top,
            right: rect.right - inset,
            bottom: rect.top + line1_top + line1_height,
        };

        let previous = win::SelectObject(dc, font_main);

        if summary.pinned {
            let mut star = line1;
            star.right = left + star_width;
            win::SetTextColor(dc, COLOR_PIN);
            let text = win::wide("★");
            win::DrawTextW(dc, text.as_ptr(), -1, &mut star, text_flags());
            left += gap;
        }

        let mut preview = line1;
        preview.left = left;
        win::SetTextColor(dc, COLOR_TEXT);
        let text = win::wide(&summary.preview);
        win::DrawTextW(dc, text.as_ptr(), -1, &mut preview, text_flags());

        let mut meta = win::RECT {
            left,
            top: rect.top + line2_top,
            right: rect.right - inset,
            bottom: rect.top + line2_top + line2_height,
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
