//! Hand-declared Win32 bindings.
//!
//! This is a mechanical port of the C# `Native.cs` that already works on the
//! user's machine: `LayoutKind.Sequential` becomes `#[repr(C)]`, `IntPtr`
//! becomes `isize`, `[DllImport("user32.dll")]` becomes `#[link(name = "user32")]`.
//!
//! Deliberately dependency-free. A crate would supply the same declarations,
//! but this way every signature has exactly one verified reference, and there is
//! no version or feature-name surface left to guess at. A wrong `repr(C)` layout
//! is the one class of mistake a compiler cannot catch, so it is worth having
//! only one source of truth for it.

#![allow(dead_code, non_snake_case)]

use std::ffi::c_void;

// --------------------------------------------------------------------- aliases

pub type HWND = isize;
pub type HINSTANCE = isize;
pub type HMENU = isize;
pub type HICON = isize;
pub type HCURSOR = isize;
pub type HBRUSH = isize;
pub type HDC = isize;
pub type HFONT = isize;
pub type HGDIOBJ = isize;
pub type HGLOBAL = isize;
pub type HANDLE = isize;
pub type HKEY = isize;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;
pub type PCWSTR = *const u16;

pub type WNDPROC = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
pub type SUBCLASSPROC =
    unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, usize, usize) -> LRESULT;

// ------------------------------------------------------------------- constants

pub const WM_DESTROY: u32 = 0x0002;
pub const WM_CLIPBOARDUPDATE: u32 = 0x031D;
pub const WM_HOTKEY: u32 = 0x0312;

pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_NOREPEAT: u32 = 0x4000;

pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;

pub const INPUT_KEYBOARD: u32 = 1;
pub const KEYEVENTF_KEYUP: u32 = 0x0002;
pub const VK_CONTROL: u16 = 0x11;
pub const VK_V: u16 = 0x56;

pub const MONITOR_DEFAULTTONEAREST: u32 = 2;

pub const ERROR_ALREADY_EXISTS: u32 = 183;

// --- tray icon ---
pub const NIM_ADD: u32 = 0;
pub const NIM_MODIFY: u32 = 1;
pub const NIM_DELETE: u32 = 2;
pub const NIF_MESSAGE: u32 = 0x0001;
pub const NIF_ICON: u32 = 0x0002;
pub const NIF_TIP: u32 = 0x0004;

pub const WM_APP: u32 = 0x8000;
pub const WM_NULL: u32 = 0x0000;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONUP: u32 = 0x0205;

pub const MF_STRING: u32 = 0x0000;
pub const MF_SEPARATOR: u32 = 0x0800;
pub const MF_CHECKED: u32 = 0x0008;
pub const TPM_RIGHTBUTTON: u32 = 0x0002;
pub const TPM_RETURNCMD: u32 = 0x0100;

/// MAKEINTRESOURCE(IDI_APPLICATION), the generic application icon.
pub const IMI_APPLICATION: u16 = 32512;
pub const SW_SHOWNORMAL: i32 = 1;

// --- registry ---
pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001u32 as i32 as isize;
pub const KEY_QUERY_VALUE: u32 = 0x0001;
pub const KEY_SET_VALUE: u32 = 0x0002;
pub const REG_SZ: u32 = 1;
pub const ERROR_SUCCESS: i32 = 0;
/// GlobalAlloc flag: the block can move, which is what the clipboard requires.
pub const GMEM_MOVEABLE: u32 = 0x0002;

// --- popup window and its child controls ---
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_VSCROLL: u32 = 0x0020_0000;
pub const ES_AUTOHSCROLL: u32 = 0x0080;
pub const LBS_NOTIFY: u32 = 0x0001;
pub const LBS_OWNERDRAWFIXED: u32 = 0x0010;
pub const LBS_HASSTRINGS: u32 = 0x0040;
pub const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;
pub const LB_ADDSTRING: u32 = 0x0180;
pub const LB_RESETCONTENT: u32 = 0x0184;
pub const LB_SETCURSEL: u32 = 0x0186;
pub const LB_GETCURSEL: u32 = 0x0188;
pub const LB_SETITEMHEIGHT: u32 = 0x01A0;

pub const WM_ACTIVATE: u32 = 0x0006;
pub const WM_SETFOCUS: u32 = 0x0007;
pub const WM_DRAWITEM: u32 = 0x002B;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_CTLCOLOREDIT: u32 = 0x0133;
pub const WM_CTLCOLORLISTBOX: u32 = 0x0134;

pub const EN_CHANGE: u32 = 0x0300;
pub const LBN_DBLCLK: u32 = 2;

pub const SW_SHOW: i32 = 5;
pub const SW_HIDE: i32 = 0;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_SHOWWINDOW: u32 = 0x0040;
pub const HWND_TOPMOST: HWND = -1;

pub const VK_RETURN: i32 = 0x0D;
pub const VK_ESCAPE: i32 = 0x1B;
pub const VK_PRIOR: i32 = 0x21;
pub const VK_NEXT: i32 = 0x22;
pub const VK_UP: i32 = 0x26;
pub const VK_DOWN: i32 = 0x28;
pub const VK_P: i32 = 0x50;

pub const ODS_SELECTED: u32 = 0x0001;

pub const DT_LEFT: u32 = 0x0000;
pub const DT_VCENTER: u32 = 0x0004;
pub const DT_SINGLELINE: u32 = 0x0020;
pub const DT_NOPREFIX: u32 = 0x0800;
pub const DT_END_ELLIPSIS: u32 = 0x8000;

pub const TRANSPARENT_BK: i32 = 1;

// CreateFontW arguments.
pub const FW_NORMAL: i32 = 400;
pub const FW_SEMIBOLD: i32 = 600;
pub const CHARSET_DEFAULT: u32 = 1;
pub const QUALITY_CLEARTYPE: u32 = 5;

pub const MB_OK: u32 = 0x0000_0000;
pub const MB_ICONERROR: u32 = 0x0000_0010;
pub const MB_ICONWARNING: u32 = 0x0000_0030;
pub const MB_SETFOREGROUND: u32 = 0x0001_0000;
pub const MB_TOPMOST: u32 = 0x0004_0000;

// --------------------------------------------------------------------- structs

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SYSTEMTIME {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

/// x64 layout, verified against the existing C# app:
/// HWND(8) message(4) pad(4) WPARAM(8) LPARAM(8) time(4) POINT(8) = 48 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: u32,
    pub w_param: WPARAM,
    pub l_param: LPARAM,
    pub time: u32,
    pub pt: POINT,
}

#[repr(C)]
pub struct WNDCLASSEXW {
    pub cb_size: u32,
    pub style: u32,
    pub lpfn_wnd_proc: Option<WNDPROC>,
    pub cb_cls_extra: i32,
    pub cb_wnd_extra: i32,
    pub h_instance: HINSTANCE,
    pub h_icon: HICON,
    pub h_cursor: HCURSOR,
    pub hbr_background: HBRUSH,
    pub lpsz_menu_name: PCWSTR,
    pub lpsz_class_name: PCWSTR,
    pub h_icon_sm: HICON,
}

impl Default for WNDCLASSEXW {
    fn default() -> Self {
        // All fields are integer, pointer or fn-pointer types, so the all-zero
        // bit pattern is a valid "unset" value for every one of them.
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MONITORINFO {
    pub cb_size: u32,
    pub rc_monitor: RECT,
    pub rc_work: RECT,
    pub dw_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MOUSEINPUT {
    pub dx: i32,
    pub dy: i32,
    pub mouse_data: u32,
    pub dw_flags: u32,
    pub time: u32,
    pub dw_extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KEYBDINPUT {
    pub w_vk: u16,
    pub w_scan: u16,
    pub dw_flags: u32,
    pub time: u32,
    pub dw_extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HARDWAREINPUT {
    pub u_msg: u32,
    pub w_param_l: u16,
    pub w_param_h: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union INPUT_UNION {
    pub mi: MOUSEINPUT,
    pub ki: KEYBDINPUT,
    pub hi: HARDWAREINPUT,
}

/// Size on x64 must be 40; `SendInput` rejects the call when `cbSize` differs.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct INPUT {
    pub kind: u32,
    pub u: INPUT_UNION,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BITMAPINFOHEADER {
    pub bi_size: u32,
    pub bi_width: i32,
    pub bi_height: i32,
    pub bi_planes: u16,
    pub bi_bit_count: u16,
    pub bi_compression: u32,
    pub bi_size_image: u32,
    pub bi_x_pels_per_meter: i32,
    pub bi_y_pels_per_meter: i32,
    pub bi_clr_used: u32,
    pub bi_clr_important: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILETIME {
    pub dw_low_date_time: u32,
    pub dw_high_date_time: u32,
}


// ------------------------------------------------------------------ user32.dll

#[link(name = "user32")]
extern "system" {
    fn RegisterClassExW(param0: *const WNDCLASSEXW) -> u16;
    fn CreateWindowExW(
        dwExStyle: u32,
        lpClassName: PCWSTR,
        lpWindowName: PCWSTR,
        dwStyle: u32,
        X: i32,
        Y: i32,
        nWidth: i32,
        nHeight: i32,
        hWndParent: HWND,
        hMenu: HMENU,
        hInstance: HINSTANCE,
        lpParam: *const c_void,
    ) -> HWND;
    fn DefWindowProcW(hWnd: HWND, Msg: u32, wParam: WPARAM, lParam: LPARAM) -> LRESULT;
    fn GetMessageW(lpMsg: *mut MSG, hWnd: HWND, wMsgFilterMin: u32, wMsgFilterMax: u32) -> i32;
    fn TranslateMessage(lpMsg: *const MSG) -> i32;
    fn DispatchMessageW(lpMsg: *const MSG) -> i32;
    fn PostQuitMessage(nExitCode: i32);
    fn PostMessageW(hWnd: HWND, Msg: u32, wParam: WPARAM, lParam: LPARAM) -> i32;
    fn DestroyWindow(hWnd: HWND) -> i32;
    pub fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;

    fn RegisterHotKey(hWnd: HWND, id: i32, fsModifiers: u32, vk: u32) -> i32;
    fn UnregisterHotKey(hWnd: HWND, id: i32) -> i32;

    fn AddClipboardFormatListener(hwnd: HWND) -> i32;
    fn RemoveClipboardFormatListener(hwnd: HWND) -> i32;
    fn GetClipboardSequenceNumber() -> u32;
    pub fn OpenClipboard(hWndNewOwner: HWND) -> i32;
    pub fn CloseClipboard() -> i32;
    pub fn IsClipboardFormatAvailable(format: u32) -> i32;
    pub fn GetClipboardData(uFormat: u32) -> HANDLE;
    pub fn SetClipboardData(uFormat: u32, hMem: HANDLE) -> HANDLE;
    pub fn RegisterClipboardFormatW(lpszFormat: PCWSTR) -> u32;
    pub fn EnumClipboardFormats(format: u32) -> u32;
    pub fn EmptyClipboard() -> i32;

    pub fn GetForegroundWindow() -> HWND;
    pub fn GetWindowTextW(hWnd: HWND, lpString: *mut u16, nMaxCount: i32) -> i32;
    pub fn GetWindowTextLengthW(hWnd: HWND) -> i32;
    pub fn SetWindowTextW(hWnd: HWND, lpString: PCWSTR) -> i32;
    fn SetForegroundWindow(hWnd: HWND) -> i32;
    fn GetCursorPos(lpPoint: *mut POINT) -> i32;
    fn GetWindowThreadProcessId(hWnd: HWND, lpdwProcessId: *mut u32) -> u32;
    fn AttachThreadInput(idAttach: u32, idAttachTo: u32, fAttach: i32) -> i32;

    fn MonitorFromPoint(pt: POINT, dwFlags: u32) -> HANDLE;
    fn GetMonitorInfoW(hMonitor: HANDLE, lpmi: *mut MONITORINFO) -> i32;

    fn SendInput(cInputs: u32, pInputs: *const INPUT, cbSize: i32) -> u32;
    fn SetProcessDpiAwarenessContext(value: HANDLE) -> i32;
    fn MessageBoxW(hWnd: HWND, lpText: PCWSTR, lpCaption: PCWSTR, uType: u32) -> i32;

    // --- popup support ---
    pub fn SendMessageW(hWnd: HWND, Msg: u32, wParam: WPARAM, lParam: LPARAM) -> LRESULT;
    pub fn SetFocus(hWnd: HWND) -> HWND;
    pub fn GetKeyState(nVirtKey: i32) -> i16;
    pub fn IsWindowVisible(hWnd: HWND) -> i32;
    pub fn SetWindowPos(
        hWnd: HWND,
        hWndInsertAfter: HWND,
        X: i32,
        Y: i32,
        cx: i32,
        cy: i32,
        uFlags: u32,
    ) -> i32;
    pub fn GetClientRect(hWnd: HWND, lpRect: *mut RECT) -> i32;
    pub fn InvalidateRect(hWnd: HWND, lpRect: *const RECT, bErase: i32) -> i32;
    pub fn GetDpiForWindow(hwnd: HWND) -> u32;

    // --- tray icon and its menu ---
    pub fn CreatePopupMenu() -> HMENU;
    pub fn DestroyMenu(hMenu: HMENU) -> i32;
    pub fn AppendMenuW(hMenu: HMENU, uFlags: u32, uIDNewItem: usize, lpNewItem: PCWSTR) -> i32;
    pub fn TrackPopupMenu(
        hMenu: HMENU,
        uFlags: u32,
        x: i32,
        y: i32,
        nReserved: i32,
        hWnd: HWND,
        prcRect: *const RECT,
    ) -> i32;
    pub fn LoadIconW(hInstance: HINSTANCE, lpIconName: PCWSTR) -> HICON;
    pub fn GetModuleFileNameW(hModule: HINSTANCE, lpFilename: *mut u16, nSize: u32) -> u32;
}

// ---------------------------------------------------------------- kernel32.dll

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(lpModuleName: PCWSTR) -> HINSTANCE;
    fn GetCurrentThreadId() -> u32;
    fn GetLocalTime(lpSystemTime: *mut SYSTEMTIME);
    pub fn GlobalAlloc(uFlags: u32, dwBytes: usize) -> HGLOBAL;
    pub fn GlobalFree(hMem: HGLOBAL) -> HGLOBAL;
    pub fn GlobalLock(hMem: HGLOBAL) -> *mut c_void;
    pub fn GlobalUnlock(hMem: HGLOBAL) -> i32;
    pub fn GlobalSize(hMem: HGLOBAL) -> usize;
    fn CreateMutexW(
        lpMutexAttributes: *const c_void,
        bInitialOwner: i32,
        lpName: PCWSTR,
    ) -> HANDLE;
    fn GetLastError() -> u32;
    fn SetLastError(dwErrCode: u32);
    fn FileTimeToSystemTime(lpFileTime: *const FILETIME, lpSystemTime: *mut SYSTEMTIME) -> i32;
    fn SystemTimeToTzSpecificLocalTime(
        lpTimeZoneInformation: *const c_void,
        lpUniversalTime: *const SYSTEMTIME,
        lpLocalTime: *mut SYSTEMTIME,
    ) -> i32;
}

// ---------------------------------------------------------------- shell32.dll

#[link(name = "shell32")]
extern "system" {
    pub fn Shell_NotifyIconW(dwMessage: u32, lpData: *mut NOTIFYICONDATAW) -> i32;
    pub fn ShellExecuteW(
        hwnd: HWND,
        lpOperation: PCWSTR,
        lpFile: PCWSTR,
        lpParameters: PCWSTR,
        lpDirectory: PCWSTR,
        nShowCmd: i32,
    ) -> HINSTANCE;
    pub fn DragQueryFileW(hDrop: HANDLE, iFile: u32, lpszFile: *mut u16, cch: u32) -> u32;
}

// ------------------------------------------------------------------- gdi32.dll

#[link(name = "gdi32")]
extern "system" {
    pub fn CreateFontW(
        cHeight: i32,
        cWidth: i32,
        cEscapement: i32,
        cOrientation: i32,
        cWeight: i32,
        bItalic: u32,
        bUnderline: u32,
        bStrikeOut: u32,
        iCharSet: u32,
        iOutPrecision: u32,
        iClipPrecision: u32,
        iQuality: u32,
        iPitchAndFamily: u32,
        pszFaceName: PCWSTR,
    ) -> HFONT;
    pub fn CreateSolidBrush(color: u32) -> HBRUSH;
    pub fn DeleteObject(ho: HGDIOBJ) -> i32;
    pub fn SelectObject(hdc: HDC, h: HGDIOBJ) -> HGDIOBJ;
    pub fn SetTextColor(hdc: HDC, color: u32) -> u32;
    pub fn SetBkColor(hdc: HDC, color: u32) -> u32;
    pub fn SetBkMode(hdc: HDC, mode: i32) -> i32;
    pub fn DrawTextW(hdc: HDC, lpchText: PCWSTR, cchText: i32, lprc: *mut RECT, format: u32) -> i32;
    pub fn FillRect(hdc: HDC, lprc: *const RECT, hbr: HBRUSH) -> i32;
    pub fn FrameRect(hdc: HDC, lprc: *const RECT, hbr: HBRUSH) -> i32;
}

// ---------------------------------------------------------------- comctl32.dll

#[link(name = "comctl32")]
extern "system" {
    pub fn SetWindowSubclass(
        hWnd: HWND,
        pfnSubclass: SUBCLASSPROC,
        uIdSubclass: usize,
        dwRefData: usize,
    ) -> i32;
    pub fn DefSubclassProc(hWnd: HWND, uMsg: u32, wParam: WPARAM, lParam: LPARAM) -> LRESULT;
}

/// What the listbox hands us on `WM_DRAWITEM`. x64 layout:
/// 5 UINTs (20) + 4 pad + HWND(8) + HDC(8) + RECT(16) + ULONG_PTR(8) = 64.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DRAWITEMSTRUCT {
    pub ctl_type: u32,
    pub ctl_id: u32,
    pub item_id: u32,
    pub item_action: u32,
    pub item_state: u32,
    pub hwnd_item: HWND,
    pub hdc: HDC,
    pub rc_item: RECT,
    pub item_data: usize,
}

// ---------------------------------------------------------------- advapi32.dll

#[link(name = "advapi32")]
extern "system" {
    pub fn RegOpenKeyExW(
        hKey: HKEY,
        lpSubKey: PCWSTR,
        ulOptions: u32,
        samDesired: u32,
        phkResult: *mut HKEY,
    ) -> i32;
    pub fn RegQueryValueExW(
        hKey: HKEY,
        lpValueName: PCWSTR,
        lpReserved: *mut u32,
        lpType: *mut u32,
        lpData: *mut u8,
        lpcbData: *mut u32,
    ) -> i32;
    pub fn RegSetValueExW(
        hKey: HKEY,
        lpValueName: PCWSTR,
        Reserved: u32,
        dwType: u32,
        lpData: *const u8,
        cbData: u32,
    ) -> i32;
    pub fn RegDeleteValueW(hKey: HKEY, lpValueName: PCWSTR) -> i32;
    pub fn RegCloseKey(hKey: HKEY) -> i32;
}

pub const TIP_CHARS: usize = 128;
pub const INFO_CHARS: usize = 256;
pub const INFO_TITLE_CHARS: usize = 64;

/// Tray icon payload. Declared field by field so the compiler inserts the same
/// padding the C header relies on:
/// cbSize(4) pad(4) hWnd(8) uID(4) uFlags(4) uCallbackMessage(4) pad(4)
/// hIcon(8) szTip(256) dwState(4) dwStateMask(4) szInfo(512) uTimeout(4)
/// szInfoTitle(128) dwInfoFlags(4) guidItem(16) hBalloonIcon(8) = 976 bytes.
#[repr(C)]
pub struct NOTIFYICONDATAW {
    pub cb_size: u32,
    pub hwnd: HWND,
    pub id: u32,
    pub flags: u32,
    pub callback_message: u32,
    pub icon: HICON,
    pub tip: [u16; TIP_CHARS],
    pub state: u32,
    pub state_mask: u32,
    pub info: [u16; INFO_CHARS],
    pub timeout_or_version: u32,
    pub info_title: [u16; INFO_TITLE_CHARS],
    pub info_flags: u32,
    pub guid_item: GUID,
    pub balloon_icon: HICON,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

// ------------------------------------------------------------------- helpers

/// NUL-terminated UTF-16, the only string form Win32 W APIs accept.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

pub fn local_timestamp() -> String {
    let mut st = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.year, st.month, st.day, st.hour, st.minute, st.second, st.milliseconds
    )
}

/// Unix milliseconds to the 100-nanosecond ticks-since-1601 that FILETIME uses.
pub fn unix_ms_to_file_time(ms: i64) -> FILETIME {
    const EPOCH_DIFFERENCE_MS: i64 = 11_644_473_600_000;
    let ticks = (ms.max(-EPOCH_DIFFERENCE_MS) + EPOCH_DIFFERENCE_MS) as u64 * 10_000;
    FILETIME {
        dw_low_date_time: (ticks & 0xFFFF_FFFF) as u32,
        dw_high_date_time: (ticks >> 32) as u32,
    }
}

/// Local-time year and month for a unix timestamp.
///
/// Done through Win32 rather than a date crate on purpose: the history folder
/// is bucketed by LOCAL year-month because the C# build wrote it that way, and
/// getting a correct local offset is the one thing time crates are awkward at.
/// Local wall-clock time for a unix timestamp.
///
/// Done through Win32 rather than a date crate on purpose: the history folder
/// is bucketed by LOCAL year-month because the C# build wrote it that way, and
/// getting a correct local offset is the one thing time crates are awkward at.
pub fn local_datetime(ms: i64) -> SYSTEMTIME {
    let file_time = unix_ms_to_file_time(ms);
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();

    unsafe {
        if FileTimeToSystemTime(&file_time, &mut utc) == 0 {
            return SYSTEMTIME::default();
        }

        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            // DST-transition edge cases can refuse; UTC is a fine fallback.
            local = utc;
        }
    }

    local
}

pub fn local_year_month(ms: i64) -> (u16, u16) {
    let local = local_datetime(ms);
    if local.year == 0 {
        (1970, 1)
    } else {
        (local.year, local.month)
    }
}

pub const MDT_EFFECTIVE_DPI: i32 = 0;

#[link(name = "shcore")]
extern "system" {
    fn GetDpiForMonitor(hmonitor: HANDLE, dpiType: i32, dpiX: *mut u32, dpiY: *mut u32) -> i32;
}

/// Effective DPI of the monitor nearest to a point. 96 when unknown.
pub fn dpi_at(point: POINT) -> u32 {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        if monitor == 0 {
            return 96;
        }

        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == 0
            && dpi_x > 0
        {
            dpi_x
        } else {
            96
        }
    }
}

pub fn set_per_monitor_dpi_aware() {
    // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 is the pseudo-handle -4.
    const PER_MONITOR_AWARE_V2: HANDLE = -4;
    unsafe {
        // Only fails on pre-1703 Windows, where the default awareness applies.
        let _ = SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2);
    }
}

/// Returns false when another instance already owns the name.
pub fn acquire_single_instance(name: &[u16]) -> bool {
    unsafe {
        SetLastError(0);
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle == 0 {
            // Cannot tell; better to run than to refuse to start.
            crate::log::warn(&format!(
                "CreateMutexW failed, err {}; continuing without a single-instance guard",
                GetLastError()
            ));
            return true;
        }

        // The handle is intentionally leaked: it must stay alive for the whole
        // process lifetime for the guard to mean anything.
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

pub fn create_message_window(class_name: &[u16], title: &[u16], proc: WNDPROC) -> HWND {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfn_wnd_proc: Some(proc),
            h_instance: instance,
            lpsz_class_name: class_name.as_ptr(),
            ..Default::default()
        };

        if RegisterClassExW(&class) == 0 {
            crate::log::warn(&format!(
                "RegisterClassExW failed, err {}",
                GetLastError()
            ));
        }

        // Never shown: no WS_VISIBLE, and WS_EX_TOOLWINDOW keeps it out of the
        // taskbar and alt-tab. CreateWindowExW does not show anything by itself.
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            0,
            0,
            instance,
            std::ptr::null(),
        )
    }
}

pub fn run_message_loop() {
    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg, 0, 0, 0) };
        if result <= 0 {
            // 0 is WM_QUIT, -1 is an error; either way there is nothing to pump.
            break;
        }

        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn def_window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub fn post_quit_message(code: i32) {
    unsafe { PostQuitMessage(code) };
}

pub fn post_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    unsafe { PostMessageW(hwnd, msg, wparam, lparam) != 0 }
}

/// A daemon has no window to fail in front of, so a fatal startup problem would
/// otherwise be completely invisible to whoever just double-clicked the exe.
pub fn message_box(title: &str, text: &str, flags: u32) {
    let title = wide(title);
    let text = wide(text);
    unsafe {
        MessageBoxW(
            0,
            text.as_ptr(),
            title.as_ptr(),
            flags | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// Registers `class_name` and creates the window. Returns 0 on failure.
pub fn create_window(
    class_name: &str,
    title: &str,
    proc: WNDPROC,
    style: u32,
    ex_style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> HWND {
    let class_wide = wide(class_name);
    let title_wide = wide(title);
    create_window_wide(
        &class_wide,
        &title_wide,
        proc,
        style,
        ex_style,
        x,
        y,
        width,
        height,
    )
}

fn create_window_wide(
    class_name: &[u16],
    title: &[u16],
    proc: WNDPROC,
    style: u32,
    ex_style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> HWND {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfn_wnd_proc: Some(proc),
            h_instance: instance,
            lpsz_class_name: class_name.as_ptr(),
            ..Default::default()
        };

        if RegisterClassExW(&class) == 0 {
            crate::log::warn(&format!("RegisterClassExW failed, err {}", GetLastError()));
        }

        CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            width,
            height,
            0,
            0,
            instance,
            std::ptr::null(),
        )
    }
}

/// Creates a child control from one of the Win32 system classes ("EDIT",
/// "LISTBOX", "STATIC", ...), which need no registration of ours.
pub fn create_child(
    class_name: &str,
    title: &str,
    style: u32,
    parent: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> HWND {
    let class_wide = wide(class_name);
    let title_wide = wide(title);

    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        CreateWindowExW(
            0,
            class_wide.as_ptr(),
            title_wide.as_ptr(),
            style,
            x,
            y,
            width,
            height,
            parent,
            0,
            instance,
            std::ptr::null(),
        )
    }
}

pub fn destroy_window(hwnd: HWND) {
    unsafe {
        DestroyWindow(hwnd);
    }
}

pub fn register_hotkey(hwnd: HWND, id: i32, modifiers: u32, vk: u32) -> bool {
    unsafe { RegisterHotKey(hwnd, id, modifiers, vk) != 0 }
}

pub fn unregister_hotkey(hwnd: HWND, id: i32) {
    unsafe {
        UnregisterHotKey(hwnd, id);
    }
}

pub fn add_clipboard_listener(hwnd: HWND) -> bool {
    unsafe { AddClipboardFormatListener(hwnd) != 0 }
}

pub fn remove_clipboard_listener(hwnd: HWND) {
    unsafe {
        RemoveClipboardFormatListener(hwnd);
    }
}

pub fn clipboard_sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

pub fn cursor_position() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        GetCursorPos(&mut pt);
    }
    pt
}

/// Work area of the monitor nearest to a point, in device pixels.
pub fn work_area_at(point: POINT) -> RECT {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        if monitor == 0 {
            return RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            };
        }

        let mut info = MONITORINFO {
            cb_size: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            };
        }

        info.rc_work
    }
}

/// Brings a window to the foreground even when the shell's foreground lock
/// would refuse the request. Sharing the input queue with the thread that
/// currently owns the foreground is the documented way past that lock.
pub fn force_foreground(hwnd: HWND) -> bool {
    unsafe {
        if hwnd == 0 {
            return false;
        }

        let foreground = GetForegroundWindow();
        if foreground == hwnd {
            return true;
        }

        let mut pid = 0u32;
        let foreground_thread = GetWindowThreadProcessId(foreground, &mut pid);
        let current_thread = GetCurrentThreadId();

        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(foreground_thread, current_thread, 1) != 0;

        let ok = SetForegroundWindow(hwnd) != 0;

        if attached {
            AttachThreadInput(foreground_thread, current_thread, 0);
        }

        ok
    }
}

pub fn set_foreground(hwnd: HWND) -> bool {
    unsafe { hwnd != 0 && SetForegroundWindow(hwnd) != 0 }
}

/// Synthesises Ctrl+V into whatever window currently has focus.
pub fn send_ctrl_v() -> bool {
    let strokes = [
        key_stroke(VK_CONTROL, false),
        key_stroke(VK_V, false),
        key_stroke(VK_V, true),
        key_stroke(VK_CONTROL, true),
    ];

    let sent = unsafe {
        SendInput(
            strokes.len() as u32,
            strokes.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };

    if sent != strokes.len() as u32 {
        crate::log::warn(&format!(
            "SendInput accepted {}/{} strokes, err {}",
            sent,
            strokes.len(),
            last_error()
        ));
        return false;
    }

    true
}

fn key_stroke(vk: u16, up: bool) -> INPUT {
    INPUT {
        kind: INPUT_KEYBOARD,
        u: INPUT_UNION {
            ki: KEYBDINPUT {
                w_vk: vk,
                w_scan: 0,
                dw_flags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dw_extra_info: 0,
            },
        },
    }
}
