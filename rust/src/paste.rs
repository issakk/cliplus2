//! 回贴:写剪贴板 → 等焦点回落到目标 → UIPI 检查 → 注入粘贴键。
//!
//! 从 popup.rs 原样提取:这套时序在 Win32 弹窗上调好了,egui 弹窗照抄同一份,
//! 谁也不要再各写一遍。调用方负责把"藏窗口"这一步排在前面——但排队还不够:
//! egui 的 `Visible(false)` 要等到帧末才落地,所以在 UI 线程里同步调用等于
//! 窗口一直可见,下面这个等待循环也就一直等不到焦点换手。调用点在
//! `egui_app::paste_after_hide`,它在别的线程上等。这里的每一步都是纯平台
//! 操作,在哪条线程上都一样。

use std::time::{Duration, Instant};

use crate::clip::ClipPayload;
use crate::clipboard;
use crate::log;
use crate::win;

/// The window that had focus before the popup opened: the paste target.
///
/// A launcher palette holding the caret without owning the foreground wins over
/// the window behind it — the focus is where a keystroke lands. Must be called
/// BEFORE the popup takes focus, otherwise it is already too late.
pub fn capture_paste_target() -> isize {
    // Own windows never take a paste: a popup reopened over itself, or the
    // settings window happening to be in front, would otherwise become the
    // target of the next Enter.
    let previous = unsafe { win::GetForegroundWindow() };
    let mut target = if previous != 0 && !win::is_own_process_window(previous) {
        previous
    } else {
        0
    };

    // The keyboard focus of the foreground thread's queue is where a keystroke
    // would land, and a launcher palette (Listary, Quicker, …) holds that focus
    // without ever owning the foreground. When the two disagree, the window the
    // user was typing in is the paste target, not the one behind it.
    if target != 0 {
        let focus = win::foreign_input_owner();
        if focus != 0 {
            let root = win::root_window(focus);
            if root != 0 && root != target {
                target = root;
            }
        }
    }

    target
}

/// Writes `payload` to the clipboard and pastes it into `target` — the window
/// the popup took focus from.
///
/// Runs on the window thread: the keystroke has to land while the target is in
/// front, and everything after the clipboard write is a few hundred
/// milliseconds at the most.
pub fn paste_back(target: isize, payload: &ClipPayload) {
    if !clipboard::write(payload) {
        log::warn("clipboard write failed; not injecting a keystroke");
        return;
    }

    // Give the input a moment to settle after the popup hides, and take
    // whichever window ends up holding it. A foreign window that owns the
    // keyboard focus receives the keystroke as-is, so it is left exactly where
    // it is: re-activating the recorded target in front of a launcher palette
    // that still holds the caret dismisses the palette and sends the paste into
    // the window behind it. Only when nothing owns the input does the recorded
    // target have to be brought back to the front by hand.
    let started = Instant::now();
    let deadline = started + Duration::from_millis(300);
    let mut receiver = 0;
    while Instant::now() < deadline {
        let focus = win::foreign_input_owner();
        if focus != 0 {
            receiver = focus;
            break;
        }
        if target != 0 && unsafe { win::GetForegroundWindow() } == target {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    if receiver == 0 && target != 0 {
        win::set_foreground(target);
        while Instant::now() < deadline {
            if unsafe { win::GetForegroundWindow() } == target {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        receiver = target;
    }

    // A floor: the foreground switch landing does not mean the target's focused
    // control is ready to receive the keystroke yet.
    std::thread::sleep(Duration::from_millis(10));

    // UIPI lets a process inject input only into windows of its own elevation
    // or lower: at an elevated target the strokes are dropped, or land wherever
    // the system puts them instead. Say so rather than firing blanks.
    if receiver != 0 {
        let root = win::root_window(receiver);
        if paste_blocked_by_uipi(win::own_process_is_elevated(), win::process_is_elevated(root)) {
            log::warn(
                "paste target runs as administrator while ClipPlus does not; \
                 start ClipPlus as administrator to paste there",
            );
            return;
        }
    }

    let shift_insert = receiver != 0 && is_console_window(receiver);
    win::send_paste_keystroke(shift_insert);
    log::info(&format!(
        "paste-back took {} ms",
        started.elapsed().as_millis()
    ));
}

/// Whether `own_elevated` may inject a paste keystroke at `target_elevated`.
///
/// UIPI allows input only at equal or lower elevation, so a plain ClipPlus is
/// blocked exactly by the elevated targets. An unreadable elevation — protected
/// processes refuse even the limited query — counts as not blocked: refusing
/// every paste at such a window would cost more than the rare silent drop.
fn paste_blocked_by_uipi(own_elevated: bool, target_elevated: Option<bool>) -> bool {
    !own_elevated && target_elevated == Some(true)
}

/// Window classes that are terminal hosts, which take Shift+Insert for a paste.
///
/// PuTTY's class carries a per-session suffix, so it is matched by prefix.
fn is_console_class(class: &str) -> bool {
    class == "ConsoleWindowClass"
        || class == "CASCADIA_HOSTING_WINDOW_CLASS"
        || class.starts_with("PuTTY")
        || class == "mintty"
}

/// Whether the window a paste is about to land in is a terminal host.
///
/// The target can be the control that holds the focus inside a terminal rather
/// than the terminal window itself, so the whole parent chain is inspected.
/// Bounded, because window ownership is shallow in practice and a cycle here
/// would hang every paste.
fn is_console_window(hwnd: win::HWND) -> bool {
    let mut current = hwnd;
    for _ in 0..8 {
        if current == 0 {
            break;
        }
        if is_console_class(&win::window_class_name(current)) {
            return true;
        }
        current = unsafe { win::GetParent(current) };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The terminal list is policy, and a wrong entry either takes Shift+Insert
    /// away from a terminal or hands it to a window that never wanted it.
    #[test]
    fn terminal_classes_are_the_ones_that_predate_ctrl_v() {
        assert!(is_console_class("ConsoleWindowClass"));
        assert!(is_console_class("CASCADIA_HOSTING_WINDOW_CLASS"));
        // PuTTY's class carries a per-session suffix.
        assert!(is_console_class("PuTTY"));
        assert!(is_console_class("PuTTY-Configuration"));
        assert!(is_console_class("mintty"));

        assert!(!is_console_class(""));
        assert!(!is_console_class("Notepad"));
        assert!(!is_console_class("Chrome_WidgetWin_1"));
        // Prefix matching must not bleed into unrelated names.
        assert!(!is_console_class("PuttyNote"));
        assert!(!is_console_class("Minttyrus"));
    }

    /// UIPI allows input only at equal or lower elevation, and an unreadable
    /// elevation must not turn into a refusal of every paste.
    #[test]
    fn uipi_blocks_only_an_elevated_target_seen_from_below() {
        // The everyday case: a plain process pasting at a plain window.
        assert!(!paste_blocked_by_uipi(false, Some(false)));
        // The blocked one: the strokes would be dropped, or land elsewhere.
        assert!(paste_blocked_by_uipi(false, Some(true)));
        // UIPI never applies upward.
        assert!(!paste_blocked_by_uipi(true, Some(true)));
        assert!(!paste_blocked_by_uipi(true, Some(false)));
        // Unknown — protected processes refuse even the limited query.
        assert!(!paste_blocked_by_uipi(false, None));
        assert!(!paste_blocked_by_uipi(true, None));
    }
}
