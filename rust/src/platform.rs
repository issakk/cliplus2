//! 平台线程:全局热键、剪贴板监听和托盘图标住在这里。
//!
//! 它们原本挂在主线程的 message-only 窗口上;UI 换到 winit/eframe 之后主线程
//! 事件循环不再归我们,这三样 Win32 消息源整体搬到这个专用线程。它有自己的
//! message-only 窗口和消息循环,事件经 sink 回调交给当前 UI(旧 Win32 弹窗或
//! egui);反方向的控制只走 PostMessage——Win32 消息队列本身就是线程安全的
//! 命令通道,不必另设队列。
//!
//! 这也是窗口线程亲和的要求:热键与剪贴板回调在平台线程上触发,而弹窗的
//! 显隐必须回到创建它的线程去做,所以事件从不对 UI 窗口直接动手。

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::clip::ClipPayload;
use crate::clipboard;
use crate::log;
use crate::settings;
use crate::tray;
use crate::win;

/// 沿用旧值:热键 id 出现在日志里,换新没有收益。
pub const HOTKEY_ID: i32 = 0xC1A0;

/// UI→平台命令,投给平台窗口。编号避开托盘的 WM_APP+1 和弹窗的 WM_APP+2/+3,
/// 虽然落在不同窗口上,全应用不重号更好排查。
const WM_APP_APPLY_HOTKEY: u32 = win::WM_APP + 5;
const WM_APP_SUSPEND_HOTKEY: u32 = win::WM_APP + 6;
const WM_APP_QUIT: u32 = win::WM_APP + 7;

/// 平台线程发给 UI 的事件。sink 在平台线程上被调用。
pub enum PlatformEvent {
    /// 全局热键按下:切换弹窗。
    Hotkey,
    /// 托盘左键:切换弹窗。
    TrayToggle,
    /// 托盘菜单的退出项:各 UI 收尾自己的部分,再回到这里结束进程。
    Quit,
}

/// 事件的去处。
///
/// 旧 UI 把事件转成一条针对目标窗口的 PostMessage(保持窗口线程亲和);
/// egui 把它推进通道并唤醒重绘。退出事件两条路殊途同归:
/// 最终都由平台线程上的 `quit_now` 收尾。
/// `Sync` 是 static 存放的要求,顺带把"闭包内含 !Sync 状态"挡在编译期。
pub type Sink = Box<dyn Fn(PlatformEvent) + Send + Sync>;

static SINK: OnceLock<Sink> = OnceLock::new();
static PLATFORM_WINDOW: AtomicIsize = AtomicIsize::new(0);
static MAIN_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// The OS notifies on every format change and one Ctrl+C can raise several;
/// the sequence number filters those for free.
static LAST_CLIPBOARD_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// 启动平台线程。`sink` 之后就不再更换——UI 形态在进程生命周期内不变。
pub fn start(sink: Sink) {
    MAIN_THREAD_ID.store(win::current_thread_id(), Ordering::SeqCst);
    let _ = SINK.set(sink);

    let spawned = std::thread::Builder::new()
        .name("clipplus-platform".into())
        .spawn(platform_main);
    match spawned {
        Ok(handle) => {
            *THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        }
        Err(err) => {
            // 没有平台线程就没有热键、剪贴板和托盘,这个进程没有意义。
            log::error(&format!("platform thread could not be spawned: {err}"));
            win::post_thread_message(
                MAIN_THREAD_ID.load(Ordering::SeqCst),
                win::WM_QUIT,
                0,
                0,
            );
        }
    }
}

/// 主线程消息循环结束后调用:等平台线程做完收尾(摘托盘、注销剪贴板监听)。
pub fn shutdown() {
    let handle = THREAD.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

/// UI→平台:按当前设置重挂全局热键。设置窗口保存后调用。
pub fn request_apply_hotkey() {
    post_command(WM_APP_APPLY_HOTKEY);
}

/// UI→平台:暂时摘掉热键(热键录制期间),恢复时再 `request_apply_hotkey`。
pub fn request_suspend_hotkey() {
    post_command(WM_APP_SUSPEND_HOTKEY);
}

/// 任意线程可调的退出请求。
pub fn request_quit() {
    post_command(WM_APP_QUIT);
}

fn post_command(message: u32) -> bool {
    let hwnd = PLATFORM_WINDOW.load(Ordering::SeqCst);
    if hwnd == 0 {
        // 平台窗口还没起来(或已经拆掉):命令无处可投。
        log::warn("platform window not ready; command dropped");
        return false;
    }
    win::post_message(hwnd, message, 0, 0)
}

/// 发一个事件给当前 UI。托盘回调与平台线程同线程,直接调用是安全的。
pub(crate) fn emit(event: PlatformEvent) {
    if let Some(sink) = SINK.get() {
        sink(event);
    }
}

fn platform_main() {
    let window_proc: win::WNDPROC = wnd_proc;
    let hwnd = win::create_window(
        "ClipPlus.MessageWindow",
        "ClipPlus",
        window_proc,
        win::WS_POPUP,
        win::WS_EX_TOOLWINDOW,
        0,
        0,
        0,
        0,
        0, // never painted: a message-only window is never shown
    );

    if hwnd == 0 {
        let detail = format!("CreateWindowExW failed, err {}", win::last_error());
        log::error(&detail);
        // 主线程还在它的消息循环里等;直接请它收摊。
        win::post_thread_message(
            MAIN_THREAD_ID.load(Ordering::SeqCst),
            win::WM_QUIT,
            0,
            0,
        );
        return;
    }
    log::info(&format!("platform window ready (hwnd {hwnd:#x})"));
    PLATFORM_WINDOW.store(hwnd, Ordering::SeqCst);

    if win::add_clipboard_listener(hwnd) {
        log::info("clipboard listener registered");
    } else {
        log::error(&format!(
            "AddClipboardFormatListener failed, err {}",
            win::last_error()
        ));
    }

    if let Some(settings) = crate::current_settings() {
        tray::add(hwnd, &settings);
    }

    register_hotkey_from_settings();

    log::info("platform thread entering message loop");
    win::run_message_loop();

    // 正常退出路径上 quit_now 已经摘过托盘;这里兜底覆盖其余收线方式。
    tray::remove();
    win::remove_clipboard_listener(hwnd);
    win::destroy_window(hwnd);
    log::info("platform thread stopped");
}

extern "system" fn wnd_proc(
    hwnd: win::HWND,
    message: u32,
    wparam: win::WPARAM,
    lparam: win::LPARAM,
) -> win::LRESULT {
    match message {
        win::WM_CLIPBOARDUPDATE => {
            handle_clipboard_update();
            0
        }

        tray::CALLBACK_MESSAGE => {
            tray::handle_callback(lparam);
            0
        }

        win::WM_HOTKEY => {
            if wparam as i32 == HOTKEY_ID {
                emit(PlatformEvent::Hotkey);
            }
            0
        }

        WM_APP_APPLY_HOTKEY => {
            register_hotkey_from_settings();
            0
        }

        WM_APP_SUSPEND_HOTKEY => {
            log::info("hotkey suspended: settings hotkey field has focus");
            win::unregister_hotkey(hwnd, HOTKEY_ID);
            0
        }

        WM_APP_QUIT => {
            quit_now();
            0
        }

        win::WM_DESTROY => {
            win::post_quit_message(0);
            0
        }

        _ => win::def_window_proc(hwnd, message, wparam, lparam),
    }
}

fn handle_clipboard_update() {
    let sequence = win::clipboard_sequence_number();
    if LAST_CLIPBOARD_SEQUENCE.swap(sequence, Ordering::SeqCst) != sequence {
        if let Some(payload) = clipboard::read() {
            let description = match &payload {
                ClipPayload::Text(text) => format!("text, {} chars", text.chars().count()),
                ClipPayload::Files(paths) => format!("{} file(s)", paths.len()),
                ClipPayload::Image(png) => format!("image, {} PNG bytes", png.len()),
            };
            if capture_enabled(&payload) {
                log::info(&format!("captured {description}"));
                if let Some(store) = crate::store() {
                    store.enqueue(payload, clipboard::capture_context());
                }
            } else {
                log::info(&format!("ignored {description} (switched off)"));
            }
        }
    }
}

/// The capture switches are consulted at capture time rather than at startup,
/// which is what lets the settings window turn them off and have it take effect
/// on the next copy instead of the next launch.
fn capture_enabled(payload: &ClipPayload) -> bool {
    let Some(settings) = crate::current_settings() else {
        return true;
    };

    let enabled = match payload {
        ClipPayload::Text(_) => settings.capture_text,
        ClipPayload::Image(_) => settings.capture_images,
        ClipPayload::Files(_) => settings.capture_files,
    };

    // A clip that would need a `.bin` sibling is dropped whole when blobs are
    // off, rather than kept as a stub a search cannot see past.
    enabled && (settings.write_blobs || !crate::store::needs_blob(payload, &settings))
}

/// (Re)registers the global hotkey from the current settings.
///
/// Runs on the platform thread — `RegisterHotKey` 的 WM_HOTKEY 投给注册线程,
/// 窗口也归这条线程,别处调用都走 `request_apply_hotkey` 排队过来。
fn register_hotkey_from_settings() {
    let hwnd = PLATFORM_WINDOW.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }

    let Some(settings) = crate::current_settings() else {
        return;
    };

    let Some(hotkey) = settings::parse_hotkey(&settings.hotkey) else {
        log::error(&format!(
            "hotkey '{}' could not be parsed; expected e.g. Win+Alt+V",
            settings.hotkey
        ));
        return;
    };

    // Drop the old registration first: re-registering the same combination
    // while it is still held by this process fails against ourselves.
    win::unregister_hotkey(hwnd, HOTKEY_ID);

    if win::register_hotkey(hwnd, HOTKEY_ID, hotkey.modifiers | win::MOD_NOREPEAT, hotkey.vk) {
        log::info(&format!("hotkey registered: {}", settings.hotkey));
    } else {
        let detail = format!(
            "RegisterHotKey failed for '{}', err {} (most likely already taken by another program)",
            settings.hotkey,
            win::last_error()
        );
        log::error(&detail);
        win::message_box("ClipPlus", &detail, win::MB_OK | win::MB_ICONERROR);
    }
}

/// 在平台线程上收尾:摘托盘、结束主线程循环、结束自己的循环。
fn quit_now() {
    log::info("quit requested");
    tray::remove();

    // 主线程的循环不经过任何窗口,WM_QUIT 直接投给线程是结束它唯一的正规通道。
    win::post_thread_message(MAIN_THREAD_ID.load(Ordering::SeqCst), win::WM_QUIT, 0, 0);

    let hwnd = PLATFORM_WINDOW.load(Ordering::SeqCst);
    if hwnd != 0 {
        // Destroying the message window raises WM_DESTROY, which is what
        // ends this thread's message loop.
        win::destroy_window(hwnd);
    }
}
