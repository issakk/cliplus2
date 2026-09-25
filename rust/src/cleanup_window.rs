//! The cleanup window: two one-shot tools over the stored history.
//!
//! Reached from the settings window's 清理… button. Neither tool is a setting —
//! both are actions whose numbers must be seen before anything is destroyed —
//! so they live here rather than among the fields that 保存 writes back.
//!
//! Each button runs the same shape: a background thread scans (it stats every
//! blob file, and a sync placeholder can turn that into a network round trip,
//! so the window thread stays out of it) and posts the result back; this thread
//! shows the numbers in a yes/no dialog; a yes starts a second background
//! thread that does the deleting and posts a report. The buttons grey out for
//! the whole ride, so two cleanups cannot interleave.
//!
//! Scans see the index the way it was when they started — a capture that lands
//! mid-scan is simply not in the numbers, and the next run will find it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::log;
use crate::store::{BinPreview, BinScan, BinScope, DupeScan};
use crate::win::{self, HWND, LPARAM, LRESULT, WPARAM};

/// One scan-confirm-run ride at a time, across window closes and reopens: the
/// buttons grey out for the normal path, but a window hidden mid-scan and
/// shown again would otherwise allow a second start while the first result is
/// still in the mail. `start_*` claims it, and every path that consumes or
/// drops a scan result releases it.
static CLEANUP_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Scan finished on the background thread; `lparam` carries a `Box` the window
/// procedure takes ownership of.
const WM_BINS_SCANNED: u32 = win::WM_APP + 1;
const WM_BINS_DONE: u32 = win::WM_APP + 2;
const WM_DUPES_SCANNED: u32 = win::WM_APP + 3;
const WM_DUPES_DONE: u32 = win::WM_APP + 4;

const ID_GROUP_BINS: usize = 1;
const ID_BINS_TIME: usize = 2;
const ID_BINS_DAYS: usize = 3;
const ID_BINS_COUNT: usize = 4;
const ID_BINS_KEEP: usize = 5;
const ID_BINS_NOTE: usize = 6;
const ID_BINS_RUN: usize = 7;
const ID_GROUP_DUPES: usize = 8;
const ID_DUPES_NOTE: usize = 9;
const ID_DUPES_RUN: usize = 10;
const ID_STATUS: usize = 11;
const ID_CLOSE: usize = 12;
/// The two labels beside the numeric fields, one per row.
const SUFFIX_ID_BASE: usize = 100;

/// Defaults the way a first run wants them; the fields keep whatever the user
/// last typed until the window is closed for good (they are never saved —
/// these are one-shot numbers, not settings).
const DEFAULT_DAYS: u32 = 30;
const DEFAULT_KEEP: u32 = 100;

const WINDOW_STYLE: u32 = win::WS_CAPTION | win::WS_SYSMENU | win::WS_CLIPCHILDREN;

const MARGIN: i32 = 16;
const CONTENT_WIDTH: i32 = 560;
/// Inset of the controls inside their group box, on every side that matters.
const GROUP_PAD: i32 = 14;
const ROW_HEIGHT: i32 = 24;
const BUTTON_WIDTH: i32 = 120;
const BUTTON_HEIGHT: i32 = 26;
const RADIO_WIDTH: i32 = 84;
const FIELD_WIDTH: i32 = 64;
/// Room for three lines of the note (and the gap before the button is the
/// same thought as everywhere below: written as offsets, not as absolutes).
const NOTE1_HEIGHT: i32 = 54;
const NOTE2_HEIGHT: i32 = 40;
const STATUS_HEIGHT: i32 = 68;

const CLIENT_WIDTH: i32 = MARGIN * 2 + CONTENT_WIDTH;
const INNER_X: i32 = MARGIN + GROUP_PAD;
const NOTE_WIDTH: i32 = CONTENT_WIDTH - GROUP_PAD * 2;
const EDIT_X: i32 = INNER_X + RADIO_WIDTH + 8;
const SUFFIX_X: i32 = EDIT_X + FIELD_WIDTH + 8;
const SUFFIX_WIDTH: i32 = MARGIN + CONTENT_WIDTH - SUFFIX_X - GROUP_PAD;

const G1_TOP: i32 = MARGIN;
const R1_Y: i32 = G1_TOP + 30;
const R2_Y: i32 = R1_Y + 32;
const NOTE1_Y: i32 = R2_Y + 34;
const BTN1_Y: i32 = NOTE1_Y + NOTE1_HEIGHT + 8;
const G1_BOTTOM: i32 = BTN1_Y + BUTTON_HEIGHT + 10;

const G2_TOP: i32 = G1_BOTTOM + 10;
const NOTE2_Y: i32 = G2_TOP + 26;
const BTN2_Y: i32 = NOTE2_Y + NOTE2_HEIGHT + 8;
const G2_BOTTOM: i32 = BTN2_Y + BUTTON_HEIGHT + 10;

const STATUS_TOP: i32 = G2_BOTTOM + 12;
const BUTTON_TOP: i32 = STATUS_TOP + STATUS_HEIGHT + 12;
const CLIENT_HEIGHT: i32 = BUTTON_TOP + BUTTON_HEIGHT + MARGIN;

/// The frame this window is created with; the outside size is computed from it
/// in more than one place, same as the settings window.
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

/// The monitor's scale times the user's own settings scale — this window is
/// part of the settings surface, so it follows the same multiplier.
fn window_scale(dpi_scale: f64) -> f64 {
    dpi_scale * win::settings_scale_factor()
}

static WINDOW: OnceLock<HWND> = OnceLock::new();

fn field(id: usize) -> HWND {
    WINDOW
        .get()
        .map(|hwnd| win::child_by_id(*hwnd, id))
        .unwrap_or(0)
}

pub fn create() -> bool {
    let (width, height) = frame_size(1.0);

    let window_proc: win::WNDPROC = window_proc;
    let hwnd = win::create_window(
        "ClipPlus.Cleanup",
        "ClipPlus 清理",
        window_proc,
        WINDOW_STYLE,
        0,
        0,
        0,
        width,
        height,
        win::COLOR_BTNFACE_BRUSH,
    );

    if hwnd == 0 {
        log::error(&format!(
            "cleanup window CreateWindowExW failed, err {}",
            win::last_error()
        ));
        return false;
    }

    let field_style = win::WS_CHILD | win::WS_VISIBLE | win::WS_BORDER | win::WS_TABSTOP;
    let label_style = win::WS_CHILD | win::WS_VISIBLE;
    let group_style = win::WS_CHILD | win::WS_VISIBLE | win::BS_GROUPBOX;
    let radio_style = win::WS_CHILD | win::WS_VISIBLE | win::WS_TABSTOP | win::BS_AUTORADIOBUTTON;
    let button_style = win::WS_CHILD | win::WS_VISIBLE | win::WS_TABSTOP | win::BS_PUSHBUTTON;

    win::create_child_id(
        "BUTTON",
        "清理 .bin 大条目（图片、超长文本）",
        group_style,
        hwnd,
        ID_GROUP_BINS,
        MARGIN,
        G1_TOP,
        CONTENT_WIDTH,
        G1_BOTTOM - G1_TOP,
    );

    // The two radios are created together so `WS_GROUP` makes them one arrow-key
    // group; the days edit closes the group again so the arrow keys stop there.
    win::create_child_id(
        "BUTTON",
        "按时间",
        radio_style | win::WS_GROUP,
        hwnd,
        ID_BINS_TIME,
        INNER_X,
        R1_Y,
        RADIO_WIDTH,
        ROW_HEIGHT,
    );
    win::create_child_id(
        "BUTTON",
        "按条数",
        radio_style,
        hwnd,
        ID_BINS_COUNT,
        INNER_X,
        R2_Y,
        RADIO_WIDTH,
        ROW_HEIGHT,
    );

    win::create_child_id(
        "EDIT",
        "",
        field_style | win::ES_NUMBER | win::WS_GROUP,
        hwnd,
        ID_BINS_DAYS,
        EDIT_X,
        R1_Y,
        FIELD_WIDTH,
        ROW_HEIGHT,
    );
    win::create_child_id(
        "EDIT",
        "",
        field_style | win::ES_NUMBER,
        hwnd,
        ID_BINS_KEEP,
        EDIT_X,
        R2_Y,
        FIELD_WIDTH,
        ROW_HEIGHT,
    );
    win::create_child_id(
        "STATIC",
        "天以前的（0 = 全部）",
        label_style,
        hwnd,
        SUFFIX_ID_BASE,
        SUFFIX_X,
        R1_Y,
        SUFFIX_WIDTH,
        ROW_HEIGHT,
    );
    win::create_child_id(
        "STATIC",
        "个，其余清理（0 = 全部）",
        label_style,
        hwnd,
        SUFFIX_ID_BASE + 1,
        SUFFIX_X,
        R2_Y,
        SUFFIX_WIDTH,
        ROW_HEIGHT,
    );

    win::create_child_id(
        "STATIC",
        "只删带 .bin 的条目——图片和超长文本，整条删除，短文本和文件列表不受影响。固定的条目永远保留；别机当月的条目删不了，会跳过。",
        label_style,
        hwnd,
        ID_BINS_NOTE,
        INNER_X,
        NOTE1_Y,
        NOTE_WIDTH,
        NOTE1_HEIGHT,
    );

    win::create_child_id(
        "BUTTON",
        "清理 .bin…",
        button_style,
        hwnd,
        ID_BINS_RUN,
        INNER_X,
        BTN1_Y,
        BUTTON_WIDTH,
        BUTTON_HEIGHT,
    );

    win::create_child_id(
        "BUTTON",
        "清理重复文本",
        group_style,
        hwnd,
        ID_GROUP_DUPES,
        MARGIN,
        G2_TOP,
        CONTENT_WIDTH,
        G2_BOTTOM - G2_TOP,
    );

    win::create_child_id(
        "STATIC",
        "同一段文本存了多份时（两台机器都复制过最常见），每组保留最新一份，固定的副本也保留，其余删除。",
        label_style,
        hwnd,
        ID_DUPES_NOTE,
        INNER_X,
        NOTE2_Y,
        NOTE_WIDTH,
        NOTE2_HEIGHT,
    );

    win::create_child_id(
        "BUTTON",
        "去重…",
        button_style,
        hwnd,
        ID_DUPES_RUN,
        INNER_X,
        BTN2_Y,
        BUTTON_WIDTH,
        BUTTON_HEIGHT,
    );

    win::create_child_id(
        "STATIC",
        "",
        label_style,
        hwnd,
        ID_STATUS,
        MARGIN,
        STATUS_TOP,
        CONTENT_WIDTH,
        STATUS_HEIGHT,
    );

    win::create_child_id(
        "BUTTON",
        "关闭",
        button_style,
        hwnd,
        ID_CLOSE,
        MARGIN,
        BUTTON_TOP,
        BUTTON_WIDTH,
        BUTTON_HEIGHT,
    );

    if WINDOW.set(hwnd).is_err() {
        log::error("cleanup window already created");
        return false;
    }

    put_text(ID_BINS_DAYS, &DEFAULT_DAYS.to_string());
    put_text(ID_BINS_KEEP, &DEFAULT_KEEP.to_string());
    set_checked(ID_BINS_TIME, true);

    log::info(&format!("cleanup window ready (hwnd {hwnd:#x})"));
    true
}

fn put_text(id: usize, text: &str) {
    let hwnd = field(id);
    if hwnd == 0 {
        return;
    }

    let wide = win::wide(text);
    unsafe {
        win::SetWindowTextW(hwnd, wide.as_ptr());
    }
}

fn set_checked(id: usize, checked: bool) {
    let hwnd = field(id);
    if hwnd == 0 {
        return;
    }

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

/// Places every control for the monitor's scale and hands it the matching font,
/// on each show and again on `WM_DPICHANGED` — the same drill as the settings
/// window, because this one also opens on whichever monitor invited it.
fn layout(hwnd: HWND, scale: f64) {
    let margin = win::scaled(MARGIN, scale);
    let content_width = win::scaled(CONTENT_WIDTH, scale);
    let group_pad = win::scaled(GROUP_PAD, scale);
    let inner_x = margin + group_pad;
    let note_width = content_width - group_pad * 2;
    let radio_width = win::scaled(RADIO_WIDTH, scale);
    let field_width = win::scaled(FIELD_WIDTH, scale);
    let edit_x = inner_x + radio_width + win::scaled(8, scale);
    let suffix_x = edit_x + field_width + win::scaled(8, scale);
    let suffix_width = margin + content_width - suffix_x - group_pad;
    let row_height = win::scaled(ROW_HEIGHT, scale);
    let note1_height = win::scaled(NOTE1_HEIGHT, scale);
    let note2_height = win::scaled(NOTE2_HEIGHT, scale);
    let status_height = win::scaled(STATUS_HEIGHT, scale);
    let button_width = win::scaled(BUTTON_WIDTH, scale);
    let button_height = win::scaled(BUTTON_HEIGHT, scale);
    let font = win::ui_font_for_scale(scale);

    let place = |id: usize, x: i32, y: i32, width: i32, height: i32| {
        let control = win::child_by_id(hwnd, id);
        if control == 0 {
            return;
        }

        unsafe {
            win::SendMessageW(control, win::WM_SETFONT, font as usize, 1);
            win::SetWindowPos(control, 0, x, y, width, height, win::SWP_NOACTIVATE);
        }
    };
    let scaled_y = |value: i32| win::scaled(value, scale);

    place(
        ID_GROUP_BINS,
        margin,
        scaled_y(G1_TOP),
        content_width,
        scaled_y(G1_BOTTOM - G1_TOP),
    );
    place(
        ID_BINS_TIME,
        inner_x,
        scaled_y(R1_Y),
        radio_width,
        row_height,
    );
    place(
        ID_BINS_COUNT,
        inner_x,
        scaled_y(R2_Y),
        radio_width,
        row_height,
    );
    place(
        ID_BINS_DAYS,
        edit_x,
        scaled_y(R1_Y),
        field_width,
        row_height,
    );
    place(
        ID_BINS_KEEP,
        edit_x,
        scaled_y(R2_Y),
        field_width,
        row_height,
    );
    place(
        SUFFIX_ID_BASE,
        suffix_x,
        scaled_y(R1_Y),
        suffix_width,
        row_height,
    );
    place(
        SUFFIX_ID_BASE + 1,
        suffix_x,
        scaled_y(R2_Y),
        suffix_width,
        row_height,
    );
    place(
        ID_BINS_NOTE,
        inner_x,
        scaled_y(NOTE1_Y),
        note_width,
        note1_height,
    );
    place(
        ID_BINS_RUN,
        inner_x,
        scaled_y(BTN1_Y),
        button_width,
        button_height,
    );

    place(
        ID_GROUP_DUPES,
        margin,
        scaled_y(G2_TOP),
        content_width,
        scaled_y(G2_BOTTOM - G2_TOP),
    );
    place(
        ID_DUPES_NOTE,
        inner_x,
        scaled_y(NOTE2_Y),
        note_width,
        note2_height,
    );
    place(
        ID_DUPES_RUN,
        inner_x,
        scaled_y(BTN2_Y),
        button_width,
        button_height,
    );

    place(
        ID_STATUS,
        margin,
        scaled_y(STATUS_TOP),
        content_width,
        status_height,
    );
    place(
        ID_CLOSE,
        margin,
        scaled_y(BUTTON_TOP),
        button_width,
        button_height,
    );
}

pub fn show() {
    let Some(hwnd) = WINDOW.get().copied() else {
        return;
    };

    // A previous operation may have greyed the buttons out and ended while the
    // window was closed; a fresh open starts clean either way.
    set_busy(false);
    set_status("");

    let cursor = win::cursor_position();
    let area = win::work_area_at(cursor);
    let scale = window_scale(win::dpi_at(cursor) as f64 / 96.0);
    let (width, height) = frame_size(scale);

    let left = area.left + (area.right - area.left - width) / 2;
    let top = area.top + (area.bottom - area.top - height) / 3;

    layout(hwnd, scale);

    unsafe {
        win::SetWindowPos(hwnd, 0, left, top, width, height, win::SWP_SHOWWINDOW);
        win::set_foreground(hwnd);
    }

    log::info("cleanup window shown");
}

fn hide() {
    if let Some(hwnd) = WINDOW.get().copied() {
        unsafe {
            win::ShowWindow(hwnd, win::SW_HIDE);
        }
    }
}

fn complain(message: &str) {
    win::message_box("ClipPlus 清理", message, win::MB_OK | win::MB_ICONWARNING);
}

fn set_status(text: &str) {
    put_text(ID_STATUS, text);
}

/// Greys both action buttons for the whole scan-confirm-run ride, so a second
/// cleanup cannot start while one is in flight. The numeric fields and radios
/// stay live — they are read again before anything is deleted.
fn set_busy(busy: bool) {
    let Some(hwnd) = WINDOW.get().copied() else {
        return;
    };

    win::enable_window(win::child_by_id(hwnd, ID_BINS_RUN), !busy);
    win::enable_window(win::child_by_id(hwnd, ID_DUPES_RUN), !busy);
}

fn parse_field(id: usize, default: u32) -> Result<u32, String> {
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

/// Which scope the radios name. Only the active row's number is read.
fn read_scope() -> Result<BinScope, String> {
    if is_checked(ID_BINS_TIME) {
        parse_field(ID_BINS_DAYS, DEFAULT_DAYS).map(BinScope::OlderThanDays)
    } else {
        parse_field(ID_BINS_KEEP, DEFAULT_KEEP).map(BinScope::KeepNewest)
    }
}

/// The scope, said the way the confirm dialog says it.
fn scope_line(scope: BinScope) -> String {
    match scope {
        BinScope::OlderThanDays(0) | BinScope::KeepNewest(0) => "全部 .bin 大条目".to_string(),
        BinScope::OlderThanDays(days) => format!("{days} 天以前的 .bin 大条目"),
        BinScope::KeepNewest(keep) => format!("最新 {keep} 个以外的 .bin 大条目"),
    }
}

fn human_bytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

/// Claims the in-flight slot, or reports that somebody holds it. The buttons
/// are usually grey already; this is the guard behind them.
fn claim_in_flight() -> bool {
    !CLEANUP_IN_FLIGHT.swap(true, Ordering::SeqCst)
}

fn release_in_flight() {
    CLEANUP_IN_FLIGHT.store(false, Ordering::SeqCst);
}

fn start_bins() {
    let scope = match read_scope() {
        Ok(scope) => scope,
        Err(reason) => {
            complain(&reason);
            return;
        }
    };

    let Some(store) = crate::store() else {
        return;
    };
    let Some(hwnd) = WINDOW.get().copied() else {
        return;
    };
    if !claim_in_flight() {
        complain("上一次清理还在进行，等它结束再试。");
        return;
    }

    set_busy(true);
    set_status("正在扫描…");

    std::thread::spawn(move || {
        let scan = store.scan_bin_cleanup(scope);
        let preview = BinPreview {
            scan,
            scope_line: scope_line(scope),
        };
        win::post_message(
            hwnd,
            WM_BINS_SCANNED,
            0,
            Box::into_raw(Box::new(preview)) as LPARAM,
        );
    });
}

fn start_dupes() {
    let Some(store) = crate::store() else {
        return;
    };
    let Some(hwnd) = WINDOW.get().copied() else {
        return;
    };
    if !claim_in_flight() {
        complain("上一次清理还在进行，等它结束再试。");
        return;
    }

    set_busy(true);
    set_status("正在扫描…");

    std::thread::spawn(move || {
        let scan = store.scan_duplicates();
        win::post_message(
            hwnd,
            WM_DUPES_SCANNED,
            0,
            Box::into_raw(Box::new(scan)) as LPARAM,
        );
    });
}

fn confirm_bins(preview: BinPreview) {
    let BinPreview { scan, scope_line } = preview;

    if scan.doomed.is_empty() {
        set_busy(false);
        release_in_flight();
        let mut text = "没有可清理的 .bin 大条目。".to_string();
        if scan.skipped_live_month > 0 {
            text.push_str(&format!(
                "\n（另跳过 {} 条别机当月的）",
                scan.skipped_live_month
            ));
        }
        set_status(&text);
        win::message_box("ClipPlus 清理", &text, win::MB_OK | win::MB_ICONINFORMATION);
        return;
    }

    let mut summary = format!(
        "范围：{scope_line}\n\n命中 {} 条（图片 {} · 超长文本 {}），预计释放 {}。",
        scan.doomed.len(),
        scan.images,
        scan.overlong_texts,
        human_bytes(scan.blob_bytes)
    );
    if scan.skipped_live_month > 0 {
        summary.push_str(&format!(
            "\n另跳过 {} 条（别机当月，从这里删不了）。",
            scan.skipped_live_month
        ));
    }

    set_status(&summary);
    let mut confirm = summary.clone();
    confirm.push_str("\n\n整条删除、不可恢复；固定的条目不受影响。确定清理？");

    if win::message_box(
        "ClipPlus 清理",
        &confirm,
        win::MB_YESNO | win::MB_ICONWARNING,
    ) != win::IDYES
    {
        set_busy(false);
        release_in_flight();
        set_status("已取消，没有改动。");
        return;
    }

    let Some(store) = crate::store() else {
        set_busy(false);
        release_in_flight();
        return;
    };
    let Some(hwnd) = WINDOW.get().copied() else {
        return;
    };

    set_status("正在清理…");

    std::thread::spawn(move || {
        let BinScan {
            doomed, blob_bytes, ..
        } = scan;
        let removed = store.run_bin_cleanup(doomed);
        let report = format!(
            "已清理 {removed} 个 .bin 大条目，释放约 {}。",
            human_bytes(blob_bytes)
        );
        win::post_message(
            hwnd,
            WM_BINS_DONE,
            0,
            Box::into_raw(Box::new(report)) as LPARAM,
        );
    });
}

fn confirm_dupes(scan: DupeScan) {
    if scan.doomed.is_empty() {
        set_busy(false);
        release_in_flight();
        set_status("没有发现重复文本。");
        win::message_box(
            "ClipPlus 清理",
            "没有发现重复文本。",
            win::MB_OK | win::MB_ICONINFORMATION,
        );
        return;
    }

    let mut summary = format!(
        "发现 {} 组重复文本，将删除 {} 条：每组保留最新一份",
        scan.groups,
        scan.doomed.len()
    );
    if scan.groups_with_pin > 0 {
        summary.push_str(&format!(
            "，其中 {} 组的固定副本也保留",
            scan.groups_with_pin
        ));
    }
    summary.push('。');

    set_status(&summary);
    let confirm = format!("{summary}\n\n确定清理？");

    if win::message_box(
        "ClipPlus 清理",
        &confirm,
        win::MB_YESNO | win::MB_ICONWARNING,
    ) != win::IDYES
    {
        set_busy(false);
        release_in_flight();
        set_status("已取消，没有改动。");
        return;
    }

    let Some(store) = crate::store() else {
        set_busy(false);
        release_in_flight();
        return;
    };
    let Some(hwnd) = WINDOW.get().copied() else {
        return;
    };

    set_status("正在去重…");

    std::thread::spawn(move || {
        let (deleted, tombstoned) = store.run_duplicate_cleanup(scan.doomed);
        let report = if tombstoned > 0 {
            format!("已删除 {deleted} 条重复文本，另以墓碑提交 {tombstoned} 条（别机当月，由所属机器执行）。")
        } else {
            format!("已删除 {deleted} 条重复文本。")
        };
        win::post_message(
            hwnd,
            WM_DUPES_DONE,
            0,
            Box::into_raw(Box::new(report)) as LPARAM,
        );
    });
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        win::WM_COMMAND => {
            let id = wparam & 0xFFFF;
            let notification = ((wparam >> 16) & 0xFFFF) as u32;

            if notification == win::BN_CLICKED || notification == 0 {
                match id as usize {
                    ID_BINS_RUN => start_bins(),
                    ID_DUPES_RUN => start_dupes(),
                    ID_CLOSE => hide(),
                    _ => {}
                }
            }
            0
        }

        WM_BINS_SCANNED => {
            let preview = unsafe { Box::from_raw(lparam as *mut BinPreview) };

            // The window was closed while the scan ran: the numbers are of no
            // use to anybody now, and the next `show` resets the buttons.
            if win::IsWindowVisible(hwnd) == 0 {
                set_busy(false);
                release_in_flight();
                return 0;
            }

            confirm_bins(*preview);
            0
        }

        WM_DUPES_SCANNED => {
            let scan = unsafe { Box::from_raw(lparam as *mut DupeScan) };

            if win::IsWindowVisible(hwnd) == 0 {
                set_busy(false);
                release_in_flight();
                return 0;
            }

            confirm_dupes(*scan);
            0
        }

        // Both reports are plain text by the time they get here.
        WM_BINS_DONE | WM_DUPES_DONE => {
            let report = unsafe { Box::from_raw(lparam as *mut String) };

            set_busy(false);
            release_in_flight();
            set_status(&report);
            win::message_box(
                "ClipPlus 清理",
                &report,
                win::MB_OK | win::MB_ICONINFORMATION,
            );
            0
        }

        win::WM_CLOSE => {
            hide();
            0
        }

        win::WM_DPICHANGED => {
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
