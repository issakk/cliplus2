//! Hand-declared Win32 bindings.
//!
//! Every declaration here has been exercised by a build that works: `IntPtr`
//! becomes `isize`, `[DllImport("user32.dll")]` becomes `#[link(name = "user32")]`,
//! and `LayoutKind.Sequential` becomes `#[repr(C)]`.
//!
//! Deliberately dependency-free. A crate would supply the same declarations,
//! but these are already known to work and there is no version or feature-name
//! surface left to guess at. A wrong `repr(C)` layout
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
pub type HGLOBAL = isize;
pub type HANDLE = isize;
pub type HKEY = isize;
pub type HMODULE = isize;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;
pub type PCWSTR = *const u16;

pub type WNDPROC = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

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

pub const WM_QUIT: u32 = 0x0012;

pub const INPUT_KEYBOARD: u32 = 1;
pub const KEYEVENTF_KEYUP: u32 = 0x0002;
pub const VK_CONTROL: u16 = 0x11;
pub const VK_V: u16 = 0x56;
pub const VK_INSERT: u16 = 0x2D;

pub const MONITOR_DEFAULTTONEAREST: u32 = 2;

pub const ERROR_ALREADY_EXISTS: u32 = 183;

/// Enough access to ask a process for its image name, and to do it to processes
/// this one could not open any other way (an elevated editor, say).
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

/// Enough access to read a process token, which is how a window's elevation
/// gets checked before a paste is sent at it.
pub const TOKEN_QUERY: u32 = 0x0008;

/// `TokenElevation` in `TOKEN_INFORMATION_CLASS` — the one query this needs.
const TOKEN_INFO_ELEVATION: u32 = 20;

/// `GetAncestor`'s root option: the top-level window a child belongs to.
pub const GA_ROOT: u32 = 2;

// --- tray icon ---
pub const NIM_ADD: u32 = 0;
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

/// GetSystemMetrics indexes for the sizes the shell draws icons at. The tray
/// and the small title-bar slot share `SM_CXSMICON`; the taskbar and Alt-Tab
/// use `SM_CXICON`.
pub const SM_CXICON: i32 = 11;
pub const SM_CXSMICON: i32 = 49;
pub const SW_SHOWNORMAL: i32 = 1;

// --- registry ---
pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001u32 as i32 as isize;
pub const KEY_QUERY_VALUE: u32 = 0x0001;
pub const KEY_SET_VALUE: u32 = 0x0002;
pub const REG_SZ: u32 = 1;
pub const ERROR_SUCCESS: i32 = 0;
/// GlobalAlloc flag: the block can move, which is what the clipboard requires.
pub const GMEM_MOVEABLE: u32 = 0x0002;

/// Hit-test code for "the title bar": the popup hands it to `DefWindowProc` so
/// Windows runs the window move itself. Not a control id, it just happens to be 2.
/// What `begin_drag_move` sends, because the button came down on the window's own
/// background rather than on a child control.

pub const VK_SHIFT: i32 = 0x10;
pub const VK_MENU: i32 = 0x12;
pub const VK_LWIN: i32 = 0x5B;
pub const VK_RWIN: i32 = 0x5C;

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

/// x64 layout, with the padding counted out:
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
pub struct FILETIME {
    pub dw_low_date_time: u32,
    pub dw_high_date_time: u32,
}

/// Focus and active window reported by one GUI thread's input queue.
///
/// `GetGUIThreadInfo` reports the state of the *queue*, which is how launcher
/// palettes (Listary, Quicker, …) hold the caret while the window behind them
/// stays in front: the focus sits in their window without them ever being the
/// foreground. A simulated keystroke follows this focus, not the foreground.
#[repr(C)]
pub struct GUITHREADINFO {
    pub cb_size: u32,
    pub flags: u32,
    pub hwnd_active: HWND,
    pub hwnd_focus: HWND,
    pub hwnd_capture: HWND,
    pub hwnd_menu_owner: HWND,
    pub hwnd_move_size: HWND,
    pub hwnd_caret: HWND,
    pub rc_caret: RECT,
}

/// The `TokenElevation` answer: nonzero when the process runs elevated.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TOKEN_ELEVATION {
    pub token_is_elevated: u32,
}


// ------------------------------------------------------------------ dwmapi.dll

// Dark title bars. One call, and only one — the rest of the dark look comes
// from uxtheme and the palette above.
#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: HWND, attribute: u32, value: *const c_void, size: u32) -> i32;
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
    fn PostThreadMessageW(idThread: u32, Msg: u32, wParam: WPARAM, lParam: LPARAM) -> i32;
    fn DestroyWindow(hWnd: HWND) -> i32;

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
    pub fn EmptyClipboard() -> i32;

    pub fn GetForegroundWindow() -> HWND;
    pub fn GetWindowTextW(hWnd: HWND, lpString: *mut u16, nMaxCount: i32) -> i32;
    pub fn GetWindowTextLengthW(hWnd: HWND) -> i32;
    fn SetForegroundWindow(hWnd: HWND) -> i32;
    fn GetCursorPos(lpPoint: *mut POINT) -> i32;
    fn GetWindowThreadProcessId(hWnd: HWND, lpdwProcessId: *mut u32) -> u32;
    fn AttachThreadInput(idAttach: u32, idAttachTo: u32, fAttach: i32) -> i32;
    /// Focus of the input queue `idThread` belongs to — the window a keystroke
    /// sent right now would land in, foreground window or not.
    fn GetGUIThreadInfo(idThread: u32, info: *mut GUITHREADINFO) -> i32;
    pub fn IsWindow(hWnd: HWND) -> i32;
    pub fn GetParent(hWnd: HWND) -> HWND;
    /// The top-level window `hWnd` is nested in; a root returns itself.
    fn GetAncestor(hWnd: HWND, gaFlags: u32) -> HWND;
    pub fn GetClassNameW(hWnd: HWND, lpClassName: *mut u16, nMaxCount: i32) -> i32;

    fn MonitorFromPoint(pt: POINT, dwFlags: u32) -> HANDLE;
    fn GetMonitorInfoW(hMonitor: HANDLE, lpmi: *mut MONITORINFO) -> i32;

    fn SendInput(cInputs: u32, pInputs: *const INPUT, cbSize: i32) -> u32;
    fn SetProcessDpiAwarenessContext(value: HANDLE) -> i32;
    fn MessageBoxW(hWnd: HWND, lpText: PCWSTR, lpCaption: PCWSTR, uType: u32) -> i32;

    // --- popup support ---
    pub fn GetKeyState(nVirtKey: i32) -> i16;
    pub fn GetAsyncKeyState(nVirtKey: i32) -> i16;
    fn ScreenToClient(hWnd: HWND, lpPoint: *mut POINT) -> i32;
    fn ClientToScreen(hWnd: HWND, lpPoint: *mut POINT) -> i32;

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
    pub fn GetSystemMetrics(nIndex: i32) -> i32;
    pub fn GetModuleFileNameW(hModule: HINSTANCE, lpFilename: *mut u16, nSize: u32) -> u32;
    pub fn CreateIconFromResourceEx(
        presbits: *const u8,
        dwResSize: u32,
        fIcon: i32,
        dwVer: u32,
        cxDesired: i32,
        cyDesired: i32,
        flags: u32,
    ) -> HICON;
    pub fn FindWindowW(lpClassName: PCWSTR, lpWindowName: PCWSTR) -> HWND;
}

// ---------------------------------------------------------------- kernel32.dll

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryExW(lpLibFileName: PCWSTR, hFile: HANDLE, dwFlags: u32) -> HMODULE;
    /// The ordinal form as well as the name form: the dark-mode entry points in
    /// uxtheme are exported by number only.
    fn GetProcAddress(hModule: HMODULE, lpProcName: *const u8) -> *mut c_void;
    fn GetModuleHandleW(lpModuleName: PCWSTR) -> HINSTANCE;
    fn GetCurrentThreadId() -> u32;
    fn GetLocalTime(lpSystemTime: *mut SYSTEMTIME);
    pub fn GlobalAlloc(uFlags: u32, dwBytes: usize) -> HGLOBAL;
    pub fn GlobalFree(hMem: HGLOBAL) -> HGLOBAL;
    pub fn GlobalLock(hMem: HGLOBAL) -> *mut c_void;
    pub fn GlobalUnlock(hMem: HGLOBAL) -> i32;
    pub fn GlobalSize(hMem: HGLOBAL) -> usize;
    pub fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> HANDLE;
    pub fn QueryFullProcessImageNameW(
        hProcess: HANDLE,
        dwFlags: u32,
        lpExeName: *mut u16,
        lpdwSize: *mut u32,
    ) -> i32;
    pub fn CloseHandle(hObject: HANDLE) -> i32;
    fn GetCurrentProcess() -> HANDLE;

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

// ----------------------------------------------------------------- advapi32.dll

#[link(name = "advapi32")]
extern "system" {
    pub fn OpenProcessToken(
        ProcessHandle: HANDLE,
        DesiredAccess: u32,
        TokenHandle: *mut HANDLE,
    ) -> i32;
    pub fn GetTokenInformation(
        TokenHandle: HANDLE,
        TokenInformationClass: u32,
        TokenInformation: *mut c_void,
        TokenInformationLength: u32,
        ReturnLength: *mut u32,
    ) -> i32;
}

// ------------------------------------------------------------------ uxtheme.dll

#[link(name = "uxtheme")]
extern "system" {
    /// The one dark-mode entry point Microsoft exports by name: point a control at
    /// the theme class it should be drawn with.
    fn SetWindowTheme(hWnd: HWND, pszSubAppName: PCWSTR, pszSubIdList: PCWSTR) -> i32;
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
/// Done through Win32 rather than a date crate on purpose: the folders already
/// on disk are named in LOCAL year-month, and a correct local offset is the one
/// thing time crates are awkward at.
/// Local wall-clock time for a unix timestamp.
///
/// Done through Win32 rather than a date crate on purpose: the folders already
/// on disk are named in LOCAL year-month, and a correct local offset is the one
/// thing time crates are awkward at.
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

/// Ends a thread's `GetMessageW` loop that has no window of its own to borrow:
/// `WM_QUIT` goes straight to the thread. The target must already be pumping
/// (or at least have created its queue), which is always true for the callers.
pub fn post_thread_message(thread_id: u32, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    unsafe { PostThreadMessageW(thread_id, msg, wparam, lparam) != 0 }
}

pub fn current_thread_id() -> u32 {
    unsafe { GetCurrentThreadId() }
}

/// Greys a control in and out. The previous state is rarely interesting; the
/// callers re-enable unconditionally when their operation finishes.

// ------------------------------------------------------------------ app icon

/// The icon file itself, compiled into the exe. The tray and both window
/// classes read it from here, so the icon shows even when no resource was
/// linked in (the build script's resource is what Explorer and the taskbar
/// shortcuts display; this is what the running process displays).
const APP_ICON: &[u8] = include_bytes!("../../assets/clipplus.ico");

/// An HICON at `size` device pixels out of the embedded .ico.
///
/// The directory is scanned for the entry closest to the request and Windows
/// resamples that one image; the file carries every size the shell asks for,
/// so the nearest entry is normally an exact match and nothing is resampled
/// at all. That beats handing the tray the largest entry and letting it be
/// scaled down, which turned the 256-pixel image into mush at 16.
///
/// Any problem falls back to the stock application icon: a generic icon is
/// far better than no icon, because the tray icon is the only way to quit.
pub fn app_icon(size: u32) -> HICON {
    unsafe fn fallback() -> HICON {
        LoadIconW(0, IMI_APPLICATION as usize as *const u16)
    }

    // ICONDIR: reserved(2) type(2) count(2), then 16 bytes per entry — width,
    // height, colours, reserved, planes, bits, byte length, data offset. A
    // zero side in the directory means 256.
    if APP_ICON.len() < 6 || u16::from_le_bytes([APP_ICON[2], APP_ICON[3]]) != 1 {
        crate::log::warn("embedded icon is not an icon file; using the system one");
        return unsafe { fallback() };
    }

    let count = u16::from_le_bytes([APP_ICON[4], APP_ICON[5]]) as usize;
    let mut best: Option<(u32, usize, usize)> = None; // (distance, offset, bytes)

    for index in 0..count {
        let base = 6 + index * 16;
        if APP_ICON.len() < base + 16 {
            break;
        }

        let side = APP_ICON[base].max(1) as u32;
        let bytes = u32::from_le_bytes([
            APP_ICON[base + 8],
            APP_ICON[base + 9],
            APP_ICON[base + 10],
            APP_ICON[base + 11],
        ]) as usize;
        let offset = u32::from_le_bytes([
            APP_ICON[base + 12],
            APP_ICON[base + 13],
            APP_ICON[base + 14],
            APP_ICON[base + 15],
        ]) as usize;

        if offset + bytes > APP_ICON.len() {
            continue;
        }

        let distance = side.abs_diff(size);
        if best.map(|(seen, _, _)| distance < seen).unwrap_or(true) {
            best = Some((distance, offset, bytes));
        }
    }

    let Some((_, offset, bytes)) = best else {
        crate::log::warn("embedded icon has no usable image; using the system one");
        return unsafe { fallback() };
    };

    let handle = unsafe {
        CreateIconFromResourceEx(
            APP_ICON[offset..].as_ptr(),
            bytes as u32,
            1,           // fIcon
            0x0003_0000, // version 3.0
            size as i32,
            size as i32,
            0,
        )
    };

    if handle == 0 {
        crate::log::warn(&format!(
            "CreateIconFromResourceEx failed, err {}; using the system icon",
            last_error()
        ));
        return unsafe { fallback() };
    }

    handle
}

/// The size the shell draws small icons at, in device pixels — the tray slot
/// and the title bar both use it, and it already carries the monitor's DPI.
pub fn small_icon_size() -> u32 {
    unsafe { GetSystemMetrics(SM_CXSMICON) as u32 }
}

/// The large icon size: taskbar and Alt-Tab.
fn large_icon_size() -> u32 {
    unsafe { GetSystemMetrics(SM_CXICON) as u32 }
}

/// The window icon for the egui dialogs, as RGBA straight out of the embedded
/// .ico. The file's large entries are PNG-compressed, and the PNG decoder this
/// crate already ships reads one of those directly — no HICON, no GDI, no
/// DIB wrangling for what is just a handful of title-bar pixels.
pub fn window_icon_rgba() -> Option<(usize, usize, Vec<u8>)> {
    const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    if APP_ICON.len() < 6 || u16::from_le_bytes([APP_ICON[2], APP_ICON[3]]) != 1 {
        return None;
    }

    let count = u16::from_le_bytes([APP_ICON[4], APP_ICON[5]]) as usize;
    let mut best: Option<(usize, usize)> = None; // (bytes, offset)

    for index in 0..count {
        let base = 6 + index * 16;
        if APP_ICON.len() < base + 16 {
            break;
        }

        let bytes = u32::from_le_bytes([
            APP_ICON[base + 8],
            APP_ICON[base + 9],
            APP_ICON[base + 10],
            APP_ICON[base + 11],
        ]) as usize;
        let offset = u32::from_le_bytes([
            APP_ICON[base + 12],
            APP_ICON[base + 13],
            APP_ICON[base + 14],
            APP_ICON[base + 15],
        ]) as usize;

        if offset + bytes > APP_ICON.len() {
            continue;
        }
        if !APP_ICON[offset..offset + 8].starts_with(&PNG_SIGNATURE) {
            continue;
        }
        if best.map(|(seen, _)| bytes > seen).unwrap_or(true) {
            best = Some((bytes, offset));
        }
    }

    let (bytes, offset) = best?;
    let (width, height, rgba) = crate::thumb::decode_png_rgba(&APP_ICON[offset..offset + bytes])?;
    Some((width, height, rgba))
}


/// Titles are a title bar, not a document, and this one ends up in a row and in a
/// search: cut to something both can hold.
const TITLE_CHARS: usize = 120;

/// The window in front, as `(executable, title)` — what a clipboard capture
/// stores beside the clip.
///
/// Best-effort on both halves, empty on failure: the caller is in the middle of
/// storing a clip, and losing a window title must not cost the clip itself. Read
/// it while the copy is still that window's — the only moment it means anything.
pub fn foreground_context() -> (String, String) {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd == 0 {
        return (String::new(), String::new());
    }

    // GetWindowTextW rather than WM_GETTEXT is what makes this work at all: the
    // system keeps every window's caption, other processes' included, and hands
    // it over without asking anyone.
    let title: String = window_text(hwnd)
        .trim()
        .chars()
        .take(TITLE_CHARS)
        .collect();

    (process_name_of(hwnd), title)
}

/// The executable that owns `hwnd`, by name only.
fn process_name_of(hwnd: HWND) -> String {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return String::new();
    }

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process == 0 {
        return String::new();
    }

    let mut buffer = vec![0u16; 512];
    let mut length = buffer.len() as u32;
    let written =
        unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe { CloseHandle(process) };

    if written == 0 {
        return String::new();
    }

    buffer.truncate(length as usize);
    let full = String::from_utf16_lossy(&buffer);

    // Paths are `\`-separated on Windows; `/` costs nothing and covers one that
    // arrived some other way.
    match full.rsplit(|c| c == '\\' || c == '/').next() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => String::new(),
    }
}

/// Current text of a control. The paste target's caption comes through here.
pub fn window_text(hwnd: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return String::new();
        }

        let mut buffer = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        if copied <= 0 {
            return String::new();
        }

        buffer.truncate(copied as usize);
        String::from_utf16_lossy(&buffer)
    }
}

/// A daemon has no window to fail in front of, so a fatal startup problem would
/// otherwise be completely invisible to whoever just double-clicked the exe.

pub fn message_box(title: &str, text: &str, flags: u32) -> i32 {
    let title = wide(title);
    let text = wide(text);
    unsafe {
        MessageBoxW(
            0,
            text.as_ptr(),
            title.as_ptr(),
            flags | MB_SETFOREGROUND | MB_TOPMOST,
        )
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
    background: HBRUSH,
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
        background,
    )
}

/// `background` is either a real brush or the special `COLOR_* + 1` value
/// that makes the system pick a stock one; 0 leaves it unpainted, which is
/// what the popup wants because it fills itself in WM_ERASEBKGND.
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
    background: HBRUSH,
) -> HWND {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfn_wnd_proc: Some(proc),
            h_instance: instance,
            // Every top-level window of ours shows in the title bar and, for
            // the settings and cleanup windows, the taskbar; without these
            // the shell draws the generic application icon there.
            h_icon: app_icon(large_icon_size()),
            h_icon_sm: app_icon(small_icon_size()),
            hbr_background: background,
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

pub fn destroy_window(hwnd: HWND) {
    unsafe {
        DestroyWindow(hwnd);
    }
}



/// Logical pixels to physical. Sizes in this crate are written at 96 DPI and
/// multiplied by the monitor's scale before they are used: the popup's
/// remembered layout is in these units and its park position divides them out.
pub fn scaled(value: i32, scale: f64) -> i32 {
    (value as f64 * scale).round() as i32
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


pub fn set_foreground(hwnd: HWND) -> bool {
    unsafe { hwnd != 0 && SetForegroundWindow(hwnd) != 0 }
}

/// Whether the user is physically holding the key right now, no matter which
/// window the events are being routed to — the async state reads the hardware
/// stream, not this thread's queue.
pub fn key_held(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}


/// The process that owns `hwnd`. Zero when the answer is unavailable, which is
/// the same "unknown" every other window query here reports.
pub fn process_id_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid
}

/// Whether `hwnd` belongs to this process — the popup, the settings window and
/// the tray are all excluded from paste-target duty for the same reason: a
/// paste aimed at ourselves is never what the hotkey meant.
pub fn is_own_process_window(hwnd: HWND) -> bool {
    hwnd != 0 && process_id_of(hwnd) == std::process::id()
}

/// The top-level window `hwnd` lives in. A window that owns input but is never
/// shown in front (a launcher palette's search box, for one) is still reachable
/// through it.
pub fn root_window(hwnd: HWND) -> HWND {
    if hwnd == 0 {
        return 0;
    }
    unsafe { GetAncestor(hwnd, GA_ROOT) }
}

/// The window class `hwnd` was registered under, empty on failure. Read-only,
/// and it works on other processes' windows.
pub fn window_class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let copied = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..copied as usize])
}

/// The window that owns the keyboard focus in the foreground thread's queue.
///
/// This — not the foreground window — is where a keystroke sent right now
/// lands. Zero when nothing reports a focus; the callers fall back to the
/// foreground in that case.
pub fn keyboard_focus_owner() -> HWND {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground == 0 {
        return 0;
    }
    let thread = unsafe { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) };
    if thread == 0 {
        return 0;
    }

    let mut info = GUITHREADINFO {
        cb_size: std::mem::size_of::<GUITHREADINFO>() as u32,
        flags: 0,
        hwnd_active: 0,
        hwnd_focus: 0,
        hwnd_capture: 0,
        hwnd_menu_owner: 0,
        hwnd_move_size: 0,
        hwnd_caret: 0,
        rc_caret: RECT::default(),
    };
    if unsafe { GetGUIThreadInfo(thread, &mut info) } == 0 {
        return 0;
    }
    info.hwnd_focus
}

/// The foreign window that owns the keyboard focus right now, or zero.
///
/// Own-process windows never count, and neither does a focus that points at
/// something that no longer exists. A window holding the focus is by that fact
/// alone taking input, so no visibility check is applied — launcher palettes
/// hold the caret in windows that are not shown the way ordinary windows are.
pub fn foreign_input_owner() -> HWND {
    let focus = keyboard_focus_owner();
    if focus == 0 || !is_window(focus) || is_own_process_window(focus) {
        return 0;
    }
    focus
}

fn is_window(hwnd: HWND) -> bool {
    unsafe { IsWindow(hwnd) != 0 }
}

/// Whether the process that owns `hwnd` runs elevated.
///
/// `None` means unknown: protected processes refuse even the limited query,
/// and the caller has to decide what an unknown is worth. Every handle is
/// closed on the way out.
pub fn process_is_elevated(hwnd: HWND) -> Option<bool> {
    let pid = process_id_of(hwnd);
    if pid == 0 {
        return None;
    }

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process == 0 {
        return None;
    }

    let mut token: HANDLE = 0;
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    unsafe { CloseHandle(process) };
    if opened == 0 {
        return None;
    }

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_INFO_ELEVATION,
            &mut elevation as *mut TOKEN_ELEVATION as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    unsafe { CloseHandle(token) };

    (ok != 0).then(|| elevation.token_is_elevated != 0)
}

/// Whether this process runs elevated, asked once and remembered: the answer
/// cannot change while the process is alive.
pub fn own_process_is_elevated() -> bool {
    static ELEVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ELEVATED.get_or_init(|| {
        let mut token: HANDLE = 0;
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return false;
        }

        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = unsafe {
            GetTokenInformation(
                token,
                TOKEN_INFO_ELEVATION,
                &mut elevation as *mut TOKEN_ELEVATION as *mut c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
        };
        unsafe { CloseHandle(token) };

        ok != 0 && elevation.token_is_elevated != 0
    })
}


/// Synthesises a paste chord into whatever window currently has focus.
///
/// Consoles predate Ctrl+V as a paste chord: the classic console host, Windows
/// Terminal, PuTTY and mintty all understand Shift+Insert, and older console
/// settings have Ctrl+V doing nothing at all. Everyone else gets Ctrl+V.
pub fn send_paste_keystroke(shift_insert: bool) -> bool {
    // `VK_SHIFT` is declared as i32 for `GetKeyState`; `SendInput` wants u16.
    let (modifier, key) = if shift_insert {
        (VK_SHIFT as u16, VK_INSERT)
    } else {
        (VK_CONTROL, VK_V)
    };
    let strokes = [
        key_stroke(modifier, false),
        key_stroke(key, false),
        key_stroke(key, true),
        key_stroke(modifier, true),
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

// ------------------------------------------------------------------- dark mode

// Windows has no dark mode a plain Win32 app can simply ask for. What it has is a
// handful of switches the shell uses on itself, and a dark theme class in
// aero.msstyles that only Explorer was meant to wear. This is the smallest useful
// part of that: enough to colour the list's scrollbar, which is the one thing in
// the popup drawn by Windows rather than by this crate.
//
// All of it is best-effort. A build without these entry points, or without the
// theme class, leaves the control with the system's light theming — a white stripe
// down a dark popup, which is what it had before.

/// The dark theme class the scrollbar lives in, on Windows 10 1809 and later.
const ORD_SET_PREFERRED_APP_MODE: usize = 135;

/// `LoadLibraryExW` flag: resolve against the system directory only, so a stray
/// `uxtheme.dll` sitting next to the exe is never the one that gets loaded.
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;

/// The process-wide half. Called once at startup, before any control exists,
/// because a window created first keeps whatever theme it was made with.
pub fn allow_dark_mode() {
    let Some(entry) = uxtheme_export(ORD_SET_PREFERRED_APP_MODE) else {
        return;
    };

    // `AllowDark` is 1, and it is one integer argument either way: 1903 and later
    // call this `SetPreferredAppMode`, 1809 called it `AllowDarkModeForApp`, and
    // both mean "this process may draw dark".
    let set_app_mode: unsafe extern "system" fn(i32) -> i32 =
        unsafe { std::mem::transmute(entry) };
    unsafe {
        set_app_mode(1);
    }
}


/// uxtheme's own copy, asked for one of the exports it keeps to ordinals. `None`
/// means this Windows has nothing to offer, which is an answer rather than an error.
fn uxtheme_export(ordinal: usize) -> Option<*mut c_void> {
    unsafe {
        let name = wide("uxtheme.dll");
        let module = LoadLibraryExW(name.as_ptr(), 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
        if module == 0 {
            return None;
        }

        // The low word of the pointer is the ordinal: that is how `GetProcAddress`
        // is told to look one up instead of a name.
        let found = GetProcAddress(module, ordinal as *const u8);
        if found.is_null() {
            None
        } else {
            Some(found)
        }
    }
}



/// Dark title bar for an egui viewport window. eframe/winit pick the title-bar
/// theme from the system, so a light-mode Windows would put a light bar over
/// our dark face; the egui side never sees an HWND, so the window is found by
/// title — the three dialog titles are unique to this process.
///
/// Returns false when the window does not exist (yet); callers retry on the
/// next frame until it answers.
pub fn dark_title_bar_titled(title: &str) -> bool {
    let wide = wide(title);
    let hwnd = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
    if hwnd == 0 {
        return false;
    }

    dark_title_bar(hwnd);
    true
}

/// Dark title bar. Windows 10 1903 and later understand attribute 20; older
/// systems fail the call silently and keep the light one — same best-effort
/// contract as the rest of the dark-mode hack.
pub fn dark_title_bar(hwnd: HWND) {
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    let on: i32 = 1;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &on as *const i32 as *const c_void,
            4,
        );
    }
}
