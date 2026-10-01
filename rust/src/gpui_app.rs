//! The GPUI popup.
//!
//! The one popup: gpui draws it (DirectX on Windows), the egui one is gone.
//! Filter chips, machine tabs, the row context menu, image thumbnails,
//! background hydration of `.bin` payloads, multi-selection, remembered layout.
//!
//! Threading: the platform thread sends `PlatformEvent`s over a flume channel;
//! a worker thread decodes thumbnails; hydrate threads read `.bin` payloads.
//! All results come back over channels drained by async tasks on the gpui
//! executor, which run even for a hidden window. The channel receive itself
//! wakes the loop — egui needed an explicit `request_repaint` for that, gpui
//! does not.
//!
//! The three Win32 dialogs keep living on this same thread: gpui's message
//! loop is a plain GetMessageW, which dispatches their windows as before.
//! One gap: gpui translates keystrokes (WM_KEYDOWN → WM_CHAR) only for its
//! own windows, so a keyboard hook reintroduces that step for everything
//! else — see `win::install_dialog_key_translation`.
//!
//! Window management gpui does not expose (show/hide/move/resize on demand)
//! goes through the raw HWND, obtained via the public `HasWindowHandle`
//! impl, and the same win32 calls the legacy popup used.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flume::{Receiver, Sender};
use image::{Frame, ImageBuffer};
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

use gpui::{
    actions, div, fill, img, point, prelude::*, px, rgb, rgba, size, uniform_list, App,
    Application, Bounds, ClipboardItem, Context, CursorStyle, Div, Element, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, GlobalElementId, InspectorElementId,
    KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels, Point,
    Render, RenderImage, ScrollStrategy, ScrollWheelEvent, ShapedLine, SharedString, StyledImage,
    Style, TextRun, UTF16Selection, UnderlineStyle, UniformListScrollHandle, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
};
use crate::clip::{ClipKind, ClipPayload};
use crate::index::{ChipFilter, ClipSummary};
use crate::log;
use crate::platform::{self, PlatformEvent};
use crate::store::{MachineTab, Store};
use crate::thumb;
use crate::win;

/// Same cap as the legacy popup: plenty for one open, small enough that a
/// refill stays a keystroke's work.
const MAX_RESULTS: usize = 300;

/// Row height in points. The legacy list used 46 logical pixels at 96 DPI.
const ROW_HEIGHT: f32 = 46.0;

/// 窗口边缘这条看不见的带子属于缩放——旧弹窗的 `GRIP` 是同一个数,单位也是
/// 96 DPI 下的逻辑像素。
const RESIZE_GRIP: f32 = 5.0;

/// 露脸前先在屏幕外停多远(物理像素)。gpui 的 DirectX 表面在窗口隐藏期间不
/// 接收 WM_PAINT,所以和 egui 时代一样:先在没有任何显示器的地方"亮着"画一
/// 帧,再挪到目的地——`-32000` 是 Windows 自己最小化窗口时用的那类坐标。
const PARK_OFFSET: i32 = 30_000;

/// The open-at size the popup has always had (8 rows plus the bottom bands),
/// and the floor a drag enforces.
const DEFAULT_SIZE: (i32, i32) = (620, 458);
const MIN_SIZE: (i32, i32) = (420, 228);

/// 缩略图解码目标长边(设备像素):36 点的行内图按 2× 屏也够清晰。
const THUMB_PX: usize = 96;
/// 缓存条数:160 张 96px 纹理约 6 MB 显存,足够覆盖一屏可见行的来回滚动。
const THUMB_CACHE_CAP: usize = 160;

// 调色板沿用 egui 版的同一套深色。gpui 的 rgb() 不是 const fn,所以这里存
// 裸的十六进制值,使用处再包一层。
const COLOR_BG: u32 = 0x1E1E1E;
const COLOR_INPUT_BG: u32 = 0x2A2A2A;
const COLOR_SELECTED: u32 = 0x995A3C;
const COLOR_HOVER: u32 = 0x2E2E2E;
const COLOR_TEXT: u32 = 0xE6E6E6;
const COLOR_META: u32 = 0x8C8C8C;
const COLOR_PIN: u32 = 0x4AA2D2;
const COLOR_BORDER: u32 = 0x3A3A3A;
/// 遮罩:纯黑打底,略透。
const COLOR_SCRIM: u32 = 0x000000A6;

/// 界面文本字体。DirectWrite 直接解析系统字体族,微软雅黑不在包里也能找到;
/// 找不到时 gpui 自己走系统回退,不需要 egui 时代的手工加载。
const FONT_FAMILY: &str = "Microsoft YaHei";

actions!(
    clipplus,
    [
        HidePopup,
        Commit,
        MoveUp,
        MoveUpExtend,
        MoveDown,
        MoveDownExtend,
        PageUp,
        PageDown,
        DeleteRecords,
        TogglePin,
        CopyRecords,
        EditClip,
        SelectAllRecords,
        NextTab,
        PrevTab,
        ConfirmDelete,
        CancelDelete,
        SearchBackspace,
        SearchDelete,
        SearchLeft,
        SearchRight,
        SearchSelectLeft,
        SearchSelectRight,
        SearchSelectAll,
        SearchPaste,
        SearchCut,
        SearchHome,
        SearchEnd,
    ]
);

/// Where the gpui shell runs from `main`. Returns when the app quits; the
/// caller's only job afterwards is the final log line.
pub fn run(store: Arc<Store>) {
    let (event_tx, event_rx) = flume::unbounded::<PlatformEvent>();

    platform::start(Box::new(move |event| {
        // A lost send only means the UI loop is gone; the app is quitting.
        let _ = event_tx.send(event);
    }));

    let (thumb_tx, thumb_rx) = spawn_thumb_worker(Arc::clone(&store));
    let (hydrate_tx, hydrate_rx) = flume::unbounded();
    let (edit_tx, edit_rx) = flume::unbounded::<Option<String>>();

    // 编辑窗口是两种弹窗共用的 Win32 实件:结束的回话经这条通道回 App。
    crate::edit_window::set_finish_callback(Box::new(move |saved| {
        let _ = edit_tx.send(saved);
    }));

    Application::new().run(move |cx: &mut App| {
        bind_keys(cx);

        let (left, top, width_px, height_px, scale) = target_geometry();        let ppp = (scale as f32).max(0.25);
        let window_bounds = WindowBounds::Windowed(Bounds {
            // 逻辑点口径:gpui 的 bounds 按 px() 落地,内部再乘缩放。
            origin: point(px(left as f32 / ppp), px(top as f32 / ppp)),
            size: size(px(width_px as f32 / ppp), px(height_px as f32 / ppp)),
        });

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(window_bounds),
                    // 预建窗口,启动即隐藏(show:false):热键按下时只剩"显示"这一步。
                    show: false,
                    focus: false,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    window_min_size: Some(size(px(MIN_SIZE.0 as f32), px(MIN_SIZE.1 as f32))),
                    window_background: WindowBackgroundAppearance::Opaque,
                    app_id: Some("ClipPlus".to_string()),
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|cx| {
                        PopupApp::new(Arc::clone(&store), hydrate_tx.clone(), thumb_tx.clone(), cx)
                    })
                },
            )
            .expect("gpui popup window could not be created");

        // 把 HWND 交出去:窗口管理(显隐/挪动)和键盘钩子的过滤都要用它。
        let _ = window.update(cx, |_, window, cx| {
            match window.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => {
                    win::set_gpui_window(handle.hwnd.get() as isize);
                    log::info(&format!(
                        "gpui popup window ready (hwnd {:#x})",
                        handle.hwnd.get()
                    ));
                }
                other => log::error(&format!("gpui window handle unavailable: {other:?}")),
            }
            cx.observe_window_activation(window, PopupApp::on_activated)
                .detach();
        });

        // 三个 Win32 对话框的编辑控件靠这条钩子继续收到 WM_CHAR。
        let hook = win::install_dialog_key_translation(win::current_thread_id());
        if hook == 0 {
            log::error("dialog key hook could not be installed; the Win32 dialogs cannot type");
        } else {
            log::info("dialog key hook installed");
        }

        // 弹窗是唯一的 gpui 窗口:它被 Alt+F4 之类的路径关掉时,进程没有理由
        // 再挂着(egui 版窗口关闭即退出循环,同一约定)。
        cx.on_window_closed(|_cx| {
            log::info("popup window closed; quitting");
            platform::request_quit();
        })
        .detach();

        // 平台事件:热键/托盘/退出。flume 的 async recv 就是唤醒机制。
        {
            let window = window.clone();
            cx.spawn(async move |cx| {
                while let Ok(event) = event_rx.recv_async().await {
                    let _ = cx.update(|cx| {
                        let _ = window.update(cx, |view, window, cx| {
                            view.handle_platform_event(event, window, cx);
                        });
                    });
                }
            })
            .detach();
        }
        // 缩略图解码结果。
        {
            let window = window.clone();
            cx.spawn(async move |cx| {
                while let Ok(result) = thumb_rx.recv_async().await {
                    let _ = cx.update(|cx| {
                        let _ = window.update(cx, |view, _, cx| view.drain_thumb(result, cx));
                    });
                }
            })
            .detach();
        }
        // blob 水合结果。
        {
            let window = window.clone();
            cx.spawn(async move |cx| {
                while let Ok(result) = hydrate_rx.recv_async().await {
                    let _ = cx.update(|cx| {
                        let _ = window.update(cx, |view, _, cx| view.drain_hydrate(result, cx));
                    });
                }
            })
            .detach();
        }
        // 编辑窗口收尾。
        {
            let window = window.clone();
            cx.spawn(async move |cx| {
                while let Ok(saved) = edit_rx.recv_async().await {
                    let _ = cx.update(|cx| {
                        let _ = window.update(cx, |view, window, cx| {
                            view.drain_edit(saved, window, cx);
                        });
                    });
                }
            })
            .detach();
        }
    });

    // Both exit paths — the tray's Quit and anything that tears the window
    // down on its own — end with the platform thread here.
    platform::request_quit();
    platform::shutdown();
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // 搜索框自己的编辑键:context 限定在 SearchInput,只有它持焦点时才参与
        // 匹配;gpui 的 binding 就近优先,这天然复刻了 egui 版"焦点在搜索框上时
        // Delete/Ctrl+A 归搜索框"的路由。
        KeyBinding::new("backspace", SearchBackspace, Some("SearchInput")),
        KeyBinding::new("delete", SearchDelete, Some("SearchInput")),
        KeyBinding::new("left", SearchLeft, Some("SearchInput")),
        KeyBinding::new("right", SearchRight, Some("SearchInput")),
        KeyBinding::new("shift-left", SearchSelectLeft, Some("SearchInput")),
        KeyBinding::new("shift-right", SearchSelectRight, Some("SearchInput")),
        KeyBinding::new("ctrl-a", SearchSelectAll, Some("SearchInput")),
        KeyBinding::new("ctrl-v", SearchPaste, Some("SearchInput")),
        KeyBinding::new("ctrl-x", SearchCut, Some("SearchInput")),
        KeyBinding::new("home", SearchHome, Some("SearchInput")),
        KeyBinding::new("end", SearchEnd, Some("SearchInput")),
        // 列表键。注意 Windows 上 gpui 把 "cmd" 解析成 Win 键,主修饰键必须
        // 写 "ctrl"。
        KeyBinding::new("escape", HidePopup, Some("ClipPlus")),
        KeyBinding::new("enter", Commit, Some("ClipPlus")),
        KeyBinding::new("up", MoveUp, Some("ClipPlus")),
        KeyBinding::new("shift-up", MoveUpExtend, Some("ClipPlus")),
        KeyBinding::new("down", MoveDown, Some("ClipPlus")),
        KeyBinding::new("shift-down", MoveDownExtend, Some("ClipPlus")),
        KeyBinding::new("pageup", PageUp, Some("ClipPlus")),
        KeyBinding::new("pagedown", PageDown, Some("ClipPlus")),
        KeyBinding::new("delete", DeleteRecords, Some("ClipPlus")),
        KeyBinding::new("ctrl-p", TogglePin, Some("ClipPlus")),
        KeyBinding::new("ctrl-c", CopyRecords, Some("ClipPlus")),
        KeyBinding::new("ctrl-e", EditClip, Some("ClipPlus")),
        KeyBinding::new("ctrl-a", SelectAllRecords, Some("ClipPlus")),
        KeyBinding::new("ctrl-tab", NextTab, Some("ClipPlus")),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some("ClipPlus")),
    ]);
}

/// One worker, serially decoding PNG payloads into small RGBA thumbnails.
/// `.bin` payloads read from disk (possibly a cloud placeholder) here, never
/// on the UI thread.
fn spawn_thumb_worker(
    store: Arc<Store>,
) -> (
    Sender<String>,
    Receiver<(String, usize, usize, Vec<u8>)>,
) {
    let (req_tx, req_rx) = flume::unbounded::<String>();
    let (done_tx, done_rx) = flume::unbounded();

    let spawned = std::thread::Builder::new()
        .name("clipplus-thumbs".into())
        .spawn(move || {
            while let Ok(stem) = req_rx.recv() {
                let Some(payload) = store.read_payload(&stem) else {
                    continue;
                };
                let ClipPayload::Image(png_bytes) = payload else {
                    continue;
                };
                let Some((w, h, rgba)) = thumb::decode_png_rgba(&png_bytes) else {
                    continue;
                };
                let (w, h, rgba) = thumb::downscale_rgba(&rgba, w, h, THUMB_PX);
                let _ = done_tx.send((stem, w, h, rgba));
            }
        });

    if let Err(err) = spawned {
        log::error(&format!("thumbnail worker could not be spawned: {err}"));
    }

    (req_tx, done_rx)
}

/// The time button's presets. Held as the preset rather than a timestamp, so
/// the cutoff follows the clock: "近7天" picked yesterday means seven days
/// from now, not from when it was picked.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum TimeChip {
    #[default]
    All,
    Today,
    Seven,
    Thirty,
}

impl TimeChip {
    fn label(self) -> &'static str {
        match self {
            TimeChip::All => "全部",
            TimeChip::Today => "今天",
            TimeChip::Seven => "近7天",
            TimeChip::Thirty => "近30天",
        }
    }

    /// The inclusive lower bound the preset means right now, in unix ms. "今天"
    /// is local midnight, found by subtracting the time of day.
    fn since_ms(self, now: i64) -> Option<i64> {
        const DAY_MS: i64 = 24 * 60 * 60 * 1000;

        match self {
            TimeChip::All => None,
            TimeChip::Today => {
                let t = win::local_datetime(now);
                let into_day = i64::from(t.hour) * 3_600_000
                    + i64::from(t.minute) * 60_000
                    + i64::from(t.second) * 1_000
                    + i64::from(t.milliseconds);
                Some(now - into_day)
            }
            TimeChip::Seven => Some(now - 7 * DAY_MS),
            TimeChip::Thirty => Some(now - 30 * DAY_MS),
        }
    }
}

/// What the four buttons currently hold. The two boxes are search *scope* —
/// where bare terms look — so they persist across opens: ticking 应用 is a
/// preference, not a query. The two menus are filters, and reset when the
/// popup opens.
#[derive(Clone, PartialEq, Eq, Default)]
struct Chips {
    in_app: bool,
    in_title: bool,
    kind: Option<ClipKind>,
    time: TimeChip,
}

fn check_mark(on: bool) -> &'static str {
    if on {
        "☑"
    } else {
        "☐"
    }
}

/// What a hydration was started for: the payload comes back later and the
/// action decides where it goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HydrateAction {
    Paste,
    Copy,
    Edit,
}

/// 右键菜单挂起时的状态:锚点(窗口坐标)与点中的行。
#[derive(Clone)]
struct MenuState {
    origin: Point<Pixels>,
    index: usize,
}

/// 筛选下拉(类型/时间)挂起时的状态。
#[derive(Clone, Copy, PartialEq, Debug)]
enum DropdownKind {
    Type,
    Time,
}

#[derive(Clone)]
struct DropdownState {
    kind: DropdownKind,
    origin: Point<Pixels>,
}

/// 缩放方向。gpui 没有可用的原生 size 循环(弹窗样式是 style 0,没有
/// THICKFRAME),缩放是手动的:边缘带按下记起点,move 里 SetWindowPos。
#[derive(Clone, Copy, PartialEq, Debug)]
enum ResizeDir {
    West,
    East,
    North,
    South,
    NorthWest,
    NorthEast,
    SouthWest,
    SouthEast,
}

impl ResizeDir {
    fn cursor(self) -> CursorStyle {
        match self {
            ResizeDir::West | ResizeDir::East => CursorStyle::ResizeLeftRight,
            ResizeDir::North | ResizeDir::South => CursorStyle::ResizeUpDown,
            ResizeDir::NorthWest | ResizeDir::SouthEast => CursorStyle::ResizeUpLeftDownRight,
            ResizeDir::NorthEast | ResizeDir::SouthWest => CursorStyle::ResizeUpRightDownLeft,
        }
    }
}

/// 指针按下后正在进行的窗口手势。全部以按下瞬间的指针位置和窗口矩形为基准。
#[derive(Clone, Copy)]
enum DragState {
    Move {
        start_cursor: Point<Pixels>,
        start_rect: win::RECT,
        scale: f64,
    },
    Resize {
        dir: ResizeDir,
        start_cursor: Point<Pixels>,
        start_rect: win::RECT,
        scale: f64,
    },
}

struct PopupApp {
    store: Arc<Store>,
    /// 搜索框实体:文本归它所有,版本号变了才重灌列表(光标移动不算)。
    input: Entity<SearchInput>,
    input_focus: FocusHandle,
    /// 根焦点:行点击/确认框挂起时把焦点从搜索框挪到这里。
    root_focus: FocusHandle,

    /// Mirrors the real window visibility.
    visible: bool,
    /// 已经在屏幕外"亮着"、等首帧画完:画完就挪到位。焦点在停车帧就抢。
    parked: bool,
    /// Window that had focus when the popup opened: the paste target.
    target: isize,
    /// When the popup was shown; focus-loss auto-hide waits out the first
    /// moments while the focus is still travelling to the window.
    shown_at: Instant,
    /// A message box this popup put up is the one thing allowed to take the
    /// focus — the focus-loss auto-hide holds its breath while it is open.
    modal_open: bool,
    /// 热键触发的收起挂起中:组合键还没物理松开,等松开再真的藏。
    hotkey_hide_pending: bool,
    /// 挂起的最后期限:键一直不松(卡键)也不能把弹窗吊着一秒以上。
    hotkey_hide_deadline: Instant,

    /// Bottom-up, exactly like the legacy list: the newest clip is the last
    /// row, the one next to the search box.
    items: Vec<ClipSummary>,
    /// 选中行集:点击、Shift 扩选、Ctrl 点选、Ctrl+A 攒出来的任意集合。
    selected: HashSet<usize>,
    /// 焦点行:Enter 粘贴、Ctrl+P 固定、菜单动作落在它上面的那一行。
    caret: usize,
    /// Shift 扩选的起点。
    anchor: usize,
    /// 鼠标正按着拖动时,按下那一行的下标。None = 没在拖。
    drag_from: Option<usize>,
    /// 列表滚动控制柄。滚动请求直接落在柄上,由 gpui 延迟到下一次布局执行。
    list_handle: UniformListScrollHandle,
    /// 窗口手势(背景拖动 / 边缘缩放)。
    drag: Option<DragState>,

    /// 待确认的删除:要删的 stem。有值时弹窗里挂着删除确认框。
    confirm_delete: Option<Vec<String>>,
    /// 右键菜单 / 筛选下拉,同一时刻最多挂一个(遮罩互斥)。
    menu: Option<MenuState>,
    dropdown: Option<DropdownState>,

    // 筛选与机器
    tab: Option<String>,
    tabs: Vec<MachineTab>,
    chips: Chips,

    // 缩略图
    thumb_tx: Sender<String>,
    thumbs: HashMap<String, Arc<RenderImage>>,
    thumb_order: VecDeque<String>,
    thumb_pending: HashSet<String>,

    // blob 水合。第四项是 `None` 表示这一趟什么都没读出来——也要回话,
    // 不然提示一直挂着、动作悄悄不发生。
    hydrate_tx: Sender<(usize, HydrateAction, Vec<String>, Option<ClipPayload>)>,
    hydrating: Option<HydrateAction>,
    /// Bumped by every close and every new request; a hydrate result whose
    /// generation no longer matches is dropped.
    generation: usize,
}

impl PopupApp {
    fn new(
        store: Arc<Store>,
        hydrate_tx: Sender<(usize, HydrateAction, Vec<String>, Option<ClipPayload>)>,
        thumb_tx: Sender<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| SearchInput::new(cx));
        let input_focus = input.read(cx).focus_handle.clone();
        let root_focus = cx.focus_handle();

        let app = Self {
            store,
            input,
            input_focus,
            root_focus,
            visible: false,
            parked: false,
            target: 0,
            shown_at: Instant::now(),
            modal_open: false,
            hotkey_hide_pending: false,
            hotkey_hide_deadline: Instant::now(),
            items: Vec::new(),
            selected: HashSet::new(),
            caret: 0,
            anchor: 0,
            drag_from: None,
            list_handle: UniformListScrollHandle::new(),
            drag: None,
            confirm_delete: None,
            menu: None,
            dropdown: None,
            tab: None,
            tabs: Vec::new(),
            chips: Chips::default(),
            thumb_tx,
            thumbs: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_pending: HashSet::new(),
            hydrate_tx,
            hydrating: None,
            generation: 0,
        };

        // 搜索文本变了才重灌:input 实体只在自己的**内容**变化时发事件,光标
        // 和选择的挪动只走 notify,订阅这边把两者天然分开。
        cx.subscribe(&app.input, |this, _input, event, cx| {
            match event {
                SearchInputEvent::Changed => {
                    this.refill(cx);
                    cx.notify();
                }
            }
        })
        .detach();

        app
    }

    fn hwnd(&self) -> isize {
        win::gpui_window()
    }

    /// Platform events, drained by the async task that owns the channel.
    fn handle_platform_event(&mut self, event: PlatformEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PlatformEvent::Hotkey | PlatformEvent::TrayToggle => self.toggle(window, cx),
            PlatformEvent::Quit => {
                // 平台线程在收到这条后向主线程投 WM_QUIT,gpui 的循环随之退出。
                log::info("quit requested; leaving the gpui loop");
                platform::request_quit();
            }
        }
    }

    fn drain_thumb(&mut self, (stem, w, h, rgba): (String, usize, usize, Vec<u8>), cx: &mut Context<Self>) {
        let Some(image) = render_image(w, h, rgba) else {
            return;
        };
        self.thumb_pending.remove(&stem);
        if self.thumbs.insert(stem.clone(), image).is_none() {
            self.thumb_order.push_back(stem.clone());
        }
        if self.thumb_order.len() > THUMB_CACHE_CAP {
            if let Some(old) = self.thumb_order.pop_front() {
                self.thumbs.remove(&old);
            }
        }
        cx.notify();
    }

    fn drain_hydrate(
        &mut self,
        (gen, action, stems, payload): (usize, HydrateAction, Vec<String>, Option<ClipPayload>),
        cx: &mut Context<Self>,
    ) {
        if gen != self.generation {
            return;
        }
        self.hydrating = None;
        // 一条都没读出来时不静默:提示收掉、日志留痕,弹窗留在原地,
        // 让用户自己决定下一步(旧路径在这里什么都不做)。
        let Some(payload) = payload else {
            log::warn(&format!(
                "hydrate read nothing for {} row(s); keeping the popup open",
                stems.len()
            ));
            cx.notify();
            return;
        };
        match action {
            HydrateAction::Paste => self.paste_after_hide(payload, cx),
            HydrateAction::Copy => {
                // 与 inline 路径同一收尾:写成功才收起弹窗。
                if crate::clipboard::write(&payload) {
                    log::info(&format!("copied {} clip(s) from the history list", stems.len()));
                    self.hide(cx);
                } else {
                    log::warn("clipboard write failed; keeping the popup open");
                }
            }
            HydrateAction::Edit => {
                if let Some(stem) = stems.first() {
                    self.open_editor(stem, &payload);
                }
            }
        }
        cx.notify();
    }

    /// 编辑窗口收尾:取消 = 原样收回焦点;保存 = 重灌列表并跟到那一行。
    fn drain_edit(&mut self, saved: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.modal_open = false;
        if let Some(stem) = &saved {
            self.refill(cx);
            if let Some(position) = self.items.iter().position(|item| &item.stem == stem) {
                self.point_at(position);
            }
        }
        if self.visible {
            window.focus(&self.input_focus);
        }
        cx.notify();
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 准备中(pending)也算"开着":那一瞬间再按热键应当是取消打开。
        if self.visible {
            self.hide_after_hotkey_release(cx);
        } else {
            self.show(window, cx);
        }
    }

    /// 热键按下时的收起路径:热键吞掉的是 key-down,配对的 key-up 会落到
    /// 此刻持有焦点的窗口。弹窗当场藏掉的话焦点立刻回到目标程序,那串悬空
    /// 的 key-up(Ctrl↑/W↑,从来没有配对的 down)就进了别人家——输入法/
    /// AltGr 状态机会把它补全成一次右 Alt,误触前台软件的热键(微信的
    /// "按住说话"就是这么被凭空拉起来的)。等组合键物理松开再藏,让孤儿
    /// key-up 落进本窗口自灭。
    fn hide_after_hotkey_release(&mut self, cx: &mut Context<Self>) {
        if hotkey_keys_all_up() {
            self.hide(cx);
        } else {
            // 键还按着:挂起,交给轮询任务每 30ms 复查,松手即藏;最长吊一秒,
            // 卡键也不能把弹窗留在屏幕上。
            self.hotkey_hide_pending = true;
            self.hotkey_hide_deadline = Instant::now() + Duration::from_secs(1);
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(30))
                        .await;
                    let still_waiting = this.update(cx, |this, cx| {
                        if !this.hotkey_hide_pending {
                            return false;
                        }
                        if hotkey_keys_all_up() || Instant::now() >= this.hotkey_hide_deadline {
                            this.hotkey_hide_pending = false;
                            this.hide(cx);
                            return false;
                        }
                        true
                    });
                    if !matches!(still_waiting, Ok(true)) {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Before the window takes focus, or it is already too late.
        self.target = crate::paste::capture_paste_target();

        self.input.update(cx, |input, cx| input.reset(cx));
        // 上一次弹窗留下的菜单/确认框不能跟过来:那批 stem 是上一轮选的。
        self.confirm_delete = None;
        self.menu = None;
        self.dropdown = None;
        // 同上:拖动扩选的起点不跨弹窗——它说的是"指针正按着",下次打开没人按。
        self.drag_from = None;
        self.drag = None;
        // The two menus are filters, reset with the box; the scope boxes and
        // the machine tab are preferences and stay.
        self.chips.kind = None;
        self.chips.time = TimeChip::All;
        self.refresh_tabs();
        self.refill(cx);

        // 先在屏幕外亮出来,让 gpui 把内容画进表面(隐藏窗口收不到 WM_PAINT,
        // 画不了);首帧之后挪到位——直接亮在目标位置的话,窗口会先带着空表面
        // 出现在屏幕正中,那就是"和弹窗一样大的空黑框"。
        let (left, top, width_px, height_px, scale) = self.target_geometry();
        let hwnd = self.hwnd();
        win::set_window_pos(hwnd, left - PARK_OFFSET, top, width_px, height_px, win::SWP_NOZORDER);
        win::show_window(hwnd, win::SW_SHOW);
        win::set_foreground(hwnd);
        // 焦点在停车帧就抢:热键吞掉的 key-down 的配对 key-up 马上就到(用户
        // 松手),晚两帧再抢它们就落进目标程序,输入法层会把悬空的 Ctrl↑
        // 补全成右 Alt,误触微信这类前台软件的热键。屏幕外的窗口照样可以
        // 合法持焦点,用户什么也看不见。
        window.focus(&self.input_focus);

        self.visible = true;
        self.parked = true;
        self.shown_at = Instant::now();
        log::info(&format!(
            "gpui popup parked off-screen for its first paint; target {left},{top} {width_px}x{height_px} scale {scale:.2} ({} rows)",
            self.items.len()
        ));

        // 停车的第二半:首帧画完(表面里有内容了)再挪到该在的位置。等一帧不
        // 是保险起见凑数:这一帧就是把内容画进去的那一帧,挪动必须排在它后面。
        let entity = cx.entity();
        window.on_next_frame(move |_window, cx: &mut App| {
            entity.update(cx, |this, cx| {
                if !this.parked || !this.visible {
                    return;
                }
                this.parked = false;
                let (left, top, _, _, _) = this.target_geometry();
                win::set_window_pos(
                    this.hwnd(),
                    left,
                    top,
                    0,
                    0,
                    win::SWP_NOSIZE | win::SWP_NOZORDER | win::SWP_NOACTIVATE,
                );
                this.shown_at = Instant::now();
                log::info(&format!("gpui popup shown at {left},{top}"));
                cx.notify();
            });
        });
        cx.notify();
    }

    /// 按记住的位置/尺寸算出窗口该在哪、多大(没有记住就居中在鼠标所在的屏幕)。
    /// 返回物理位置/尺寸和这一屏的缩放。见模块底部的同名自由函数。
    fn target_geometry(&self) -> (i32, i32, i32, i32, f64) {
        target_geometry()
    }

    fn hide(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }

        // 还在屏幕外停车、用户根本没见过的窗口:位置是停车点,不是用户摆的,
        // 什么都不能往设置里写。
        let was_parked = self.parked;
        self.parked = false;

        if !was_parked {
            // 与 egui 版同一对字段:物理位置 + 96-DPI 逻辑尺寸。
            if let Some(rect) = win::window_rect(self.hwnd()) {
                let scale = win::dpi_at(win::POINT { x: rect.left, y: rect.top }) as f64 / 96.0;
                let position = (rect.left, rect.top);
                let size = (
                    ((rect.right - rect.left) as f64 / scale).round() as i32,
                    ((rect.bottom - rect.top) as f64 / scale).round() as i32,
                );
                crate::remember_popup_layout(position, size);
            }
        }

        // A hydration that has not finished yet was started for a popup the
        // user can no longer see; it should never act.
        self.generation += 1;
        self.hydrating = None;
        // 确认框与挂起的菜单不跟着弹窗过夜。
        self.confirm_delete = None;
        self.menu = None;
        self.dropdown = None;
        // 别的路径(Esc、失焦)先藏了的话,挂起中的热键收起就作废。
        self.hotkey_hide_pending = false;
        self.visible = false;
        self.drag = None;

        // 收起之后窗口还在,趁它藏着把下次要用的尺寸摆好:停车那一帧就不必再
        // 改尺寸。位置不用管——下次显示时窗口本来就要先停到屏幕外。
        let (_, _, width_px, height_px, _) = self.target_geometry();
        win::set_window_pos(
            self.hwnd(),
            0,
            0,
            width_px,
            height_px,
            win::SWP_NOMOVE | win::SWP_NOZORDER | win::SWP_NOACTIVATE,
        );
        win::show_window(self.hwnd(), win::SW_HIDE);
        log::info("gpui popup hidden");
        cx.notify();
    }

    /// 收起弹窗,并且**藏好之后**才注入 Ctrl+V。hide() 里是同步的 SW_HIDE,
    /// 焦点立刻回到目标程序,paste 的等待循环马上就能退出来。
    fn paste_after_hide(&mut self, payload: ClipPayload, cx: &mut Context<Self>) {
        let target = self.target;
        self.hide(cx);
        std::thread::spawn(move || crate::paste::paste_back(target, &payload));
    }

    /// 失焦即收起,对应旧弹窗的 WM_ACTIVATE;刚显示的头 300ms 不检查,焦点还
    /// 在路上;弹窗自己拉起的确认框/菜单是唯一例外。
    fn on_activated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !window.is_window_active()
            && self.visible
            && !self.parked
            && !self.modal_open
            && self.shown_at.elapsed() > Duration::from_millis(300)
        {
            self.hide(cx);
        }
    }

    /// The machine strip is derived from what is on disk, so it is refreshed
    /// on every open: a database that syncs in adds or removes a tab.
    fn refresh_tabs(&mut self) {
        self.tabs = self.store.machines();
        if !self.tabs.iter().any(|tab| tab.id == self.tab) {
            self.tab = None;
        }
    }

    /// Refills from the index, bottom-up. Also runs per keystroke, exactly
    /// like `fill_list` in the legacy popup.
    /// Refills from the index, bottom-up. Also runs per keystroke, exactly
    /// like `fill_list` in the legacy popup.
    fn refill(&mut self, cx: &mut Context<Self>) {
        let chip_filter = ChipFilter {
            in_app: self.chips.in_app,
            in_title: self.chips.in_title,
            kind: self.chips.kind,
            since_ms: self.chips.time.since_ms(crate::settings::now_ms()),
        };
        let machine = self.tab.clone();
        // 搜索文本归 input 实体所有,这里现场读,不留镜像字段。
        let search = self.input.read(cx).content.to_string();
        let mut items = self
            .store
            .query(machine.as_deref(), &search, &chip_filter, MAX_RESULTS);
        items.reverse();

        let newest = items.len().saturating_sub(1);
        self.items = items;
        self.point_at(newest);
        // The view follows the newest row on every refill, the way the legacy
        // list's LB_SETTOPINDEX did after each keystroke. strict:已可见也要归位,
        // 和 egui 版强制滚到内容底是同一件事。gpui 的 scroll_to_item 是延迟到
        // 下一次布局落地的,在这里直接叫即可,不必攒到渲染帧。
        self.list_handle
            .scroll_to_item_strict(newest, ScrollStrategy::Bottom);
    }

    /// Selection and caret both land on one row — the state after a plain
    /// click or any action that re-points the popup.
    fn point_at(&mut self, index: usize) {
        self.selected.clear();
        if self.items.get(index).is_some() {
            self.selected.insert(index);
        }
        self.caret = index;
        self.anchor = index;
    }

    /// 右键菜单点在 `index` 行上之后的选择:点在已选中的行里保留整个多选
    /// (复制/删除是对整个选区的操作),点在外面才把选择挪过来。
    fn point_at_menu_row(&mut self, index: usize) {
        if self.selected.contains(&index) {
            self.caret = index;
        } else {
            self.point_at(index);
        }
    }

    /// The rows a Delete or a copy covers, in list order (oldest first). An
    /// empty selection means there is nothing to act on.
    fn selected_rows(&self) -> Vec<usize> {
        let mut rows: Vec<usize> = self.selected.iter().copied().collect();
        rows.sort_unstable();
        rows
    }

    fn move_selection(&mut self, delta: isize, extend: bool) {
        if self.items.is_empty() {
            return;
        }
        let next = (self.caret as isize + delta).clamp(0, self.items.len() as isize - 1) as usize;
        if extend {
            self.extend_selection(next, false);
        } else {
            self.point_at(next);
        }
        self.scroll_caret_into_view();
    }

    /// 拖动扩选:锚点连到指针下的那一行。`additive` 是 Ctrl 按住时并入而不是
    /// 替换(Explorer 的习惯;旧弹窗只有 Shift 点,没有拖)。
    fn extend_selection(&mut self, index: usize, additive: bool) {
        let (lo, hi) = (self.anchor.min(index), self.anchor.max(index));
        if additive {
            self.selected.extend(lo..=hi);
        } else {
            self.selected = (lo..=hi).collect();
        }
        self.caret = index;
    }

    /// 一行被"按下"时的选择语义:普通点 = 单选,Shift = 从锚点扩选,Ctrl = 原地
    /// 切换。单击和"从这一行开始拖动"共用它——手势不同,语义一个样。
    fn press_row(&mut self, index: usize, shift: bool, ctrl: bool) {
        let (selected, anchor) = pressed_state(&self.selected, self.anchor, index, shift, ctrl);
        self.selected = selected;
        self.anchor = anchor;
        self.caret = index;
    }

    /// 方向键/翻页把 caret 挪出可见区时,把列表跟着挪过去。旧 listbox 的
    /// `LB_SETCURSEL` 自带"把光标带进视野";少了这一步 caret 会一路走出屏幕。
    /// 非 strict:已在视野里就一步都不动。
    fn scroll_caret_into_view(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let (top, bottom) = self.visible_item_range();
        if self.caret < top {
            self.list_handle.scroll_to_item(self.caret, ScrollStrategy::Top);
        } else if self.caret > bottom {
            self.list_handle
                .scroll_to_item(self.caret, ScrollStrategy::Bottom);
        }
    }

    /// 当前可见的行范围(闭区间)。滚轮转发和翻页键都按它算。
    fn visible_item_range(&self) -> (usize, usize) {
        let state = self.list_handle.0.borrow();
        (state.base_handle.top_item(), state.base_handle.bottom_item())
    }

    /// 翻页键一页跳几行:实际可见行数再留一行重叠,与旧弹窗的 page_rows
    /// 同一约定——跳走的那页永远带着来的那行。
    fn visible_page(&self) -> isize {
        let (top, bottom) = self.visible_item_range();
        (bottom.saturating_sub(top) as isize).max(1)
    }

    fn select_all(&mut self) {
        // The list is bottom-up: "everything" is the range 0..=last.
        self.selected = (0..self.items.len()).collect();
        self.caret = self.items.len().saturating_sub(1);
        self.anchor = 0;
    }

    fn cycle_tab(&mut self, dir: isize, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let current = self
            .tabs
            .iter()
            .position(|tab| tab.id == self.tab)
            .unwrap_or(0);
        let next = (current as isize + dir).rem_euclid(self.tabs.len() as isize) as usize;
        self.tab = self.tabs[next].id.clone();
        self.refill(cx);
    }

    /// Starts reading `.bin` payloads on a worker thread. The result comes
    /// back through `hydrate_tx`; Esc, a click elsewhere or a newer request
    /// all bump the generation, and a stale result is dropped when it lands.
    fn begin_hydrate(&mut self, action: HydrateAction, stems: Vec<String>) {
        if stems.is_empty() {
            return;
        }
        self.generation += 1;
        self.hydrating = Some(action);

        let store = Arc::clone(&self.store);
        let tx = self.hydrate_tx.clone();
        let generation = self.generation;
        std::thread::spawn(move || {
            let result = if action == HydrateAction::Copy && stems.len() > 1 {
                let mut parts = Vec::with_capacity(stems.len());
                for stem in &stems {
                    match store.read_payload(stem) {
                        Some(payload) => parts.push(payload),
                        None => log::warn(&format!("nothing copyable for {stem}")),
                    }
                }
                crate::clip::join_payloads(parts)
            } else {
                store.read_payload(&stems[0])
            };

            // 成败都回话:第四项 `None` 就是"没读出来"。
            let _ = tx.send((generation, action, stems, result));
        });
    }

    /// Enter: paste the caret row into the window the popup took focus from.
    fn commit(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(self.caret) else {
            return;
        };

        // A blob can be a cloud placeholder whose read downloads over the
        // network — it must never happen on the UI thread. The inline payload
        // is a memory read and goes straight through.
        if item.has_blob {
            let stem = item.stem.clone();
            self.begin_hydrate(HydrateAction::Paste, vec![stem]);
            return;
        }

        let stem = item.stem.clone();
        let Some(payload) = self.store.read_payload(&stem) else {
            log::warn(&format!("nothing pasteable for {stem}"));
            self.hide(cx);
            return;
        };

        self.paste_after_hide(payload, cx);
    }

    /// Ctrl+C: copy without pasting. One row keeps its own type (images copy
    /// as images); several rows join into one text block, images left out —
    /// the same rule `join_payloads` has always implemented. Blob-bearing
    /// rows go through the hydrate worker either way.
    ///
    /// 复制成功就把弹窗收起来(旧弹窗两条路径都是 copy 完 hide);写剪贴板失败
    /// 则留在原地,不然用户连重试的机会都没有。
    fn copy_selected(&mut self, cx: &mut Context<Self>) {
        let rows = self.selected_rows();
        if rows.is_empty() {
            return;
        }

        let summaries: Vec<ClipSummary> = rows
            .into_iter()
            .filter_map(|index| self.items.get(index).cloned())
            .collect();
        if summaries.is_empty() {
            return;
        }

        let stems: Vec<String> = summaries.iter().map(|s| s.stem.clone()).collect();
        if summaries.iter().any(|s| s.has_blob) {
            self.begin_hydrate(HydrateAction::Copy, stems);
            return;
        }

        let payloads: Vec<ClipPayload> = summaries
            .iter()
            .filter_map(|s| self.store.read_payload(&s.stem))
            .collect();
        let Some(payload) = crate::clip::join_payloads(payloads) else {
            log::warn("nothing copyable in that selection");
            return;
        };
        if !crate::clipboard::write(&payload) {
            log::warn("clipboard write failed; keeping the popup open");
            return;
        }

        log::info(&format!(
            "copied {} clip(s) from the history list",
            summaries.len()
        ));
        self.hide(cx);
    }

    /// Ctrl+E / row menu 编辑:文本类条目开编辑窗;含 .bin 的先在后台读。
    fn edit_selected(&mut self) {
        let Some(item) = self.items.get(self.caret) else {
            return;
        };
        let stem = item.stem.clone();

        if !self.store.can_edit(&stem) {
            return;
        }

        if item.has_blob {
            self.begin_hydrate(HydrateAction::Edit, vec![stem]);
            return;
        }

        let Some(payload) = self.store.read_payload(&stem) else {
            log::warn(&format!("nothing editable for {stem}"));
            return;
        };

        self.open_editor(&stem, &payload);
    }

    /// Hands one clip's text to the Win32 editor, holding the popup up the way
    /// a message box does. The editor is a shared window on this same thread;
    /// its closing comes back through the edit channel.
    fn open_editor(&mut self, stem: &str, payload: &ClipPayload) {
        let ClipPayload::Text(text) = payload else {
            log::warn("edit is text-only; refusing a non-text payload");
            return;
        };

        self.modal_open = true;
        let opened = crate::edit_window::show(stem, text);
        if !opened {
            // A hold with no editor behind it would pin the popup on screen
            // forever.
            self.modal_open = false;
        }
    }

    /// Ctrl+P: pin or unpin the caret row. Pinning moves the row to the top,
    /// so follow the item rather than the index.
    fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(self.caret) else {
            return;
        };
        let (stem, pinned) = (item.stem.clone(), item.pinned);

        if !self.store.set_pinned(&stem, !pinned) {
            return;
        }

        self.refill(cx);
        if let Some(position) = self.items.iter().position(|item| item.stem == stem) {
            self.point_at(position);
        }
    }

    /// Delete: everything the selection covers. The confirm is a pending
    /// state the frame draws as an in-window modal, dark like the rest.
    fn delete_selected(&mut self) {
        let stems: Vec<String> = self
            .selected_rows()
            .into_iter()
            .filter_map(|index| self.items.get(index).map(|item| item.stem.clone()))
            .collect();
        if stems.is_empty() {
            return;
        }

        self.confirm_delete = Some(stems);
    }

    /// The delete confirmation's outcome. Enter and 删除 run it — the old
    /// MessageBox made its first button the default, and that button was Yes.
    /// Esc, the backdrop and 取消 throw it away.
    fn run_confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(stems) = self.confirm_delete.take() else {
            return;
        };
        self.dropdown = None;
        self.menu = None;

        let caret = self.caret;
        let (deleted, marked) = self.store.delete_selected(&stems);
        log::info(&format!(
            "{} clip(s) deleted by hand, {} tombstoned",
            deleted, marked
        ));

        // 旧弹窗删完把光标留给顶替那一行的位置(而不是跳回最新),屏幕上
        // 不跳走:重灌后把光标夹回范围内,并把它滚回视野中间。
        self.refill(cx);
        let index = caret.min(self.items.len().saturating_sub(1));
        self.point_at(index);
        self.list_handle
            .scroll_to_item_strict(index, ScrollStrategy::Center);
        cx.notify();
    }

    fn cancel_confirm(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        cx.notify();
    }

    // ------------------------------------------------------------- 动作入口

    fn on_escape(&mut self, _: &HidePopup, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() || self.dropdown.take().is_some() {
            cx.notify();
            return;
        }
        if self.confirm_delete.take().is_some() {
            // 确认框挂起时焦点在根上;取消后焦点还给搜索框。
            self.confirm_delete = None;
            window.focus(&self.input_focus);
            cx.notify();
            return;
        }
        self.hide(cx);
    }

    fn on_commit(&mut self, _: &Commit, _window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.is_some() {
            self.run_confirm_delete(cx);
        } else {
            self.commit(cx);
        }
    }

    fn on_move_up(&mut self, _: &MoveUp, _window: &mut Window, _cx: &mut Context<Self>) {
        self.move_selection(-1, false);
    }

    fn on_move_up_extend(&mut self, _: &MoveUpExtend, _window: &mut Window, _cx: &mut Context<Self>) {
        self.move_selection(-1, true);
    }

    fn on_move_down(&mut self, _: &MoveDown, _window: &mut Window, _cx: &mut Context<Self>) {
        self.move_selection(1, false);
    }

    fn on_move_down_extend(
        &mut self,
        _: &MoveDownExtend,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.move_selection(1, true);
    }

    fn on_page_up(&mut self, _: &PageUp, _window: &mut Window, _cx: &mut Context<Self>) {
        self.move_selection(-self.visible_page(), false);
    }

    fn on_page_down(&mut self, _: &PageDown, _window: &mut Window, _cx: &mut Context<Self>) {
        self.move_selection(self.visible_page(), false);
    }

    fn on_delete(&mut self, _: &DeleteRecords, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.is_some() {
            self.run_confirm_delete(cx);
            return;
        }
        self.delete_selected();
        if self.confirm_delete.is_some() {
            // 确认框挂起时把焦点从搜索框挪走:Enter/Esc/Ctrl+C 一律先过确认框,
            // 搜索框里的选中文字不再被复制出去。
            window.focus(&self.root_focus);
        }
        cx.notify();
    }

    fn on_toggle_pin(&mut self, _: &TogglePin, _window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_pin(cx);
    }

    fn on_copy(&mut self, _: &CopyRecords, _window: &mut Window, cx: &mut Context<Self>) {
        // egui 版把 Ctrl+C 整个从搜索框手里抢走:不管焦点在哪,复制的是选中的
        // 记录。搜索框因此不绑定自己的复制动作。
        self.copy_selected(cx);
    }

    fn on_edit(&mut self, _: &EditClip, _window: &mut Window, _cx: &mut Context<Self>) {
        self.edit_selected();
    }

    fn on_select_all(&mut self, _: &SelectAllRecords, _window: &mut Window, _cx: &mut Context<Self>) {
        self.select_all();
    }

    fn on_next_tab(&mut self, _: &NextTab, _window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, cx);
    }

    fn on_prev_tab(&mut self, _: &PrevTab, _window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, cx);
    }

    fn on_confirm_delete(&mut self, _: &ConfirmDelete, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_confirm_delete(cx);
    }

    fn on_cancel_delete(&mut self, _: &CancelDelete, _window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_confirm(cx);
    }

    // ------------------------------------------------------------- 鼠标手势

    /// 背景(面板空白处)按下:开始拖动窗口。行、按钮、输入框的按下事件都在
    /// 各自那里 stop_propagation,落到这里的只剩空白。
    fn on_background_down(&mut self, event: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left || self.drag.is_some() {
            return;
        }
        let Some(rect) = win::window_rect(self.hwnd()) else {
            return;
        };
        let scale = win::dpi_at(win::POINT { x: rect.left, y: rect.top }) as f64 / 96.0;
        log::info("background drag: the popup moves with the pointer");
        self.drag = Some(DragState::Move {
            start_cursor: event.position,
            start_rect: rect,
            scale,
        });
        cx.notify();
    }

    fn on_window_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else {
            return;
        };
        match drag {
            DragState::Move { start_cursor, start_rect, scale } => {
                let dx = (f64::from(event.position.x - start_cursor.x) * scale).round() as i32;
                let dy = (f64::from(event.position.y - start_cursor.y) * scale).round() as i32;
                win::set_window_pos(
                    self.hwnd(),
                    start_rect.left + dx,
                    start_rect.top + dy,
                    0,
                    0,
                    win::SWP_NOSIZE | win::SWP_NOZORDER | win::SWP_NOACTIVATE,
                );
            }
            DragState::Resize { dir, start_cursor, start_rect, scale } => {
                let dx = (f64::from(event.position.x - start_cursor.x) * scale).round() as i32;
                let dy = (f64::from(event.position.y - start_cursor.y) * scale).round() as i32;
                let min_w = (MIN_SIZE.0 as f64 * scale).round() as i32;
                let min_h = (MIN_SIZE.1 as f64 * scale).round() as i32;
                let (mut left, mut top) = (start_rect.left, start_rect.top);
                let (mut right, mut bottom) = (start_rect.right, start_rect.bottom);
                match dir {
                    ResizeDir::West => left = (left + dx).min(right - min_w),
                    ResizeDir::East => right = (right + dx).max(left + min_w),
                    ResizeDir::North => top = (top + dy).min(bottom - min_h),
                    ResizeDir::South => bottom = (bottom + dy).max(top + min_h),
                    ResizeDir::NorthWest => {
                        left = (left + dx).min(right - min_w);
                        top = (top + dy).min(bottom - min_h);
                    }
                    ResizeDir::NorthEast => {
                        right = (right + dx).max(left + min_w);
                        top = (top + dy).min(bottom - min_h);
                    }
                    ResizeDir::SouthWest => {
                        left = (left + dx).min(right - min_w);
                        bottom = (bottom + dy).max(top + min_h);
                    }
                    ResizeDir::SouthEast => {
                        right = (right + dx).max(left + min_w);
                        bottom = (bottom + dy).max(top + min_h);
                    }
                }
                win::set_window_pos(
                    self.hwnd(),
                    left,
                    top,
                    right - left,
                    bottom - top,
                    win::SWP_NOZORDER | win::SWP_NOACTIVATE,
                );
            }
        }
        cx.notify();
    }

    fn on_window_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.button == MouseButton::Left {
            if self.drag.take().is_some() || self.drag_from.take().is_some() {
                cx.notify();
            }
        }
    }

    /// 滚轮落在搜索条/机器条/遮罩上时列表也跟着翻——旧弹窗的约定。列表自己的
    /// 滚动由 uniform_list 处理,这里只管列表之外的部分。
    fn on_root_wheel(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        if self
            .list_handle
            .0
            .borrow()
            .base_handle
            .bounds()
            .contains(&event.position)
        {
            return; // 列表自己的滚动,别算两遍
        }
        // gpui 的 offset 语义:向下滚(看更新的内容)delta 为负,和 egui 一致。
        let delta = f32::from(event.delta.pixel_delta(px(ROW_HEIGHT)).y);
        if delta == 0.0 {
            return;
        }
        let rows = ((delta.abs() / ROW_HEIGHT).ceil() as usize).max(1);
        let top = self.list_handle.0.borrow().base_handle.top_item();
        let last = self.items.len().saturating_sub(1);
        let target = if delta < 0.0 {
            (top + rows).min(last)
        } else {
            top.saturating_sub(rows)
        };
        self.list_handle
            .scroll_to_item_strict(target, ScrollStrategy::Top);
        cx.notify();
    }
}

impl Render for PopupApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let confirming = self.confirm_delete.is_some();
        let overlay_open = self.menu.is_some() || self.dropdown.is_some() || confirming;
        let drag_cursor = self.drag.map(|drag| match drag {
            DragState::Move { .. } => CursorStyle::ClosedHand,
            DragState::Resize { dir, .. } => dir.cursor(),
        });

        div()
            .key_context("ClipPlus")
            .track_focus(&self.root_focus)
            .font_family(FONT_FAMILY)
            .text_color(rgb(COLOR_TEXT))
            .bg(rgb(COLOR_BG))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_commit))
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_move_up_extend))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_move_down_extend))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_toggle_pin))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_edit))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_confirm_delete))
            .on_action(cx.listener(Self::on_cancel_delete))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_background_down))
            .on_mouse_move(cx.listener(Self::on_window_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_window_up))
            .on_scroll_wheel(cx.listener(Self::on_root_wheel))
            .child(self.render_list_area(cx))
            .child(self.render_tab_strip(cx))
            .child(self.render_search_strip(cx))
            // 边缘缩放带盖在内容上,只有窗口边缘那几像素。
            .child(self.render_resize_strips(cx))
            // 遮罩:菜单/下拉/确认框任一挂着时挡住底下的所有交互。
            .when(overlay_open, |el| {
                el.child(self.render_scrim(cx))
            })
            .when_some(self.menu.clone(), |el, menu| {
                el.child(self.render_context_menu(menu, cx))
            })
            .when_some(self.dropdown.clone(), |el, dropdown| {
                el.child(self.render_dropdown(dropdown, cx))
            })
            .when(confirming, |el| el.child(self.render_confirm_modal(cx)))
            // 拖动进行中压一整层光标,指针出窗也不丢形状。
            .children(drag_cursor.map(|cursor| {
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .cursor(cursor)
            }))
    }
}

impl PopupApp {
    /// 画界面本体。列表区占满剩余高度,机器条和搜索条贴底。
    fn render_list_area(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = self.hydrating.map(|action| match action {
            HydrateAction::Paste => "正在加载完整内容，稍后自动粘贴 …",
            HydrateAction::Copy | HydrateAction::Edit => "正在加载完整内容 …",
        });

        div()
            .flex_1()
            .min_h(px(0.0))
            .relative()
            .child(self.render_grip_marks())
            .when_some(hint, |el, hint| {
                el.child(
                    div()
                        .absolute()
                        .left(px(8.0))
                        .top(px(6.0))
                        .text_size(px(12.0))
                        .text_color(rgb(COLOR_META))
                        .child(hint),
                )
            })
            .when(self.items.is_empty(), |el| {
                el.child(
                    div()
                        .p(px(8.0))
                        .text_size(px(14.0))
                        .text_color(rgb(COLOR_META))
                        .child("没有匹配的记录"),
                )
            })
            .when(!self.items.is_empty(), |el| {
                el.child(self.render_rows(cx))
            })
    }

    fn render_rows(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.items.len();
        uniform_list(
            "history",
            count,
            cx.processor(
                move |this: &mut Self,
                      range: Range<usize>,
                      window: &mut Window,
                      cx: &mut Context<Self>| {
                    range
                        .map(|index| this.render_row(index, window, cx))
                        .collect::<Vec<Div>>()
                },
            ),
        )
        .track_scroll(self.list_handle.clone())
        .size_full()
    }

    /// 右上角两道短杠:旧的 Win32 弹窗用它提示"这儿能拉",位置照旧。
    fn render_grip_marks(&self) -> impl IntoElement {
        div()
            .absolute()
            .top(px(6.0))
            .right(px(8.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(div().w(px(8.0)).h(px(1.0)).bg(rgb(COLOR_META)))
            .child(div().w(px(8.0)).h(px(1.0)).bg(rgb(COLOR_META)))
    }

    fn render_row(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        // Clone out of the list so the mutable work below never overlaps the
        // borrow the widgets need.
        let Some(item) = self.items.get(index) else {
            return div().h(px(ROW_HEIGHT));
        };
        let (stem, preview, meta, pinned, kind) = (
            item.stem.clone(),
            item.preview.clone(),
            item.meta.clone(),
            item.pinned,
            item.kind,
        );
        let selected = self.selected.contains(&index);
        let hover_bg = if selected {
            rgb(COLOR_SELECTED)
        } else {
            rgb(COLOR_HOVER)
        };

        let mut row = div()
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .px(px(8.0))
            .rounded(px(6.0))
            .bg(if selected {
                rgb(COLOR_SELECTED)
            } else {
                rgb(COLOR_BG)
            })
            .hover(move |style| style.bg(hover_bg))
            // 普通点 = 单选,Shift = 从锚点扩选,Ctrl = 原地切换;按住拖动走同一
            // 套"按下"语义,然后从锚点连到指针那一行。松开时的清理由根级
            // on_mouse_up 负责,行上不重复挂。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    log::info(&format!(
                        "row {index} pressed (shift={}, ctrl={})",
                        event.modifiers.shift, event.modifiers.control
                    ));
                    this.press_row(index, event.modifiers.shift, event.modifiers.control);
                    this.drag_from = Some(index);
                    window.focus(&this.root_focus);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                    this.point_at_menu_row(index);
                    this.menu = Some(MenuState {
                        origin: event.position,
                        index,
                    });
                    cx.stop_propagation();
                    cx.notify();
                }),
            );

        // 图片行:左侧缩略图,有缓存画纹理,没有就画占位块并请求后台解码。
        if kind == ClipKind::Image {
            let side = ROW_HEIGHT - 12.0;
            if let Some(texture) = self.thumbs.get(&stem) {
                row = row.child(
                    img(texture.clone())
                        .w(px(side))
                        .h(px(side))
                        .mr(px(8.0))
                        .rounded(px(4.0))
                        .flex_shrink_0()
                        .object_fit(ObjectFit::ScaleDown),
                );
            } else {
                // 懒解码:请求只对"这一帧实际渲染到的行"发,pending 集合保证每
                // 个 stem 只发一次,不发 notify——渲染期副作用到发送为止,不会
                // 成环。结果经通道回 drain_thumb。
                if !self.thumb_pending.contains(&stem) {
                    self.thumb_pending.insert(stem.clone());
                    let _ = self.thumb_tx.send(stem.clone());
                }
                row = row.child(
                    div()
                        .w(px(side))
                        .h(px(side))
                        .mr(px(8.0))
                        .rounded(px(4.0))
                        .flex_shrink_0()
                        .bg(rgb(COLOR_INPUT_BG)),
                );
            }
        }

        let meta_text = if pinned {
            format!("📌 {meta}")
        } else {
            meta
        };
        let meta_color = if pinned { COLOR_PIN } else { COLOR_META };
        row.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .flex()
                .flex_col()
                .justify_between()
                .py(px(4.0))
                .child(
                    div()
                        .text_size(px(15.0))
                        .text_color(rgb(COLOR_TEXT))
                        .truncate()
                        .child(preview),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(meta_color))
                        .truncate()
                        .child(meta_text),
                ),
        )
    }

    /// 边缘缩放带。winit 时代无边框窗口靠答 WM_NCHITTEST 补原生缩放;gpui 的
    /// 弹窗样式没有 THICKFRAME,这里换成认边缘的透明带 + 手动 SetWindowPos。
    fn render_resize_strips(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        const CORNER: f32 = 12.0;
        let dirs = [
            ResizeDir::West,
            ResizeDir::East,
            ResizeDir::North,
            ResizeDir::South,
            ResizeDir::NorthWest,
            ResizeDir::NorthEast,
            ResizeDir::SouthWest,
            ResizeDir::SouthEast,
        ];
        div().children(dirs.map(|dir| {
            // 每条带的定位:边到边留出 CORNER 给角带,面带只用 RESIZE_GRIP 粗。
            let positioned = match dir {
                ResizeDir::West => div().left(px(0.0)).w(px(RESIZE_GRIP)).top(px(CORNER)).bottom(px(CORNER)),
                ResizeDir::East => div().right(px(0.0)).w(px(RESIZE_GRIP)).top(px(CORNER)).bottom(px(CORNER)),
                ResizeDir::North => div().top(px(0.0)).h(px(RESIZE_GRIP)).left(px(CORNER)).right(px(CORNER)),
                ResizeDir::South => div().bottom(px(0.0)).h(px(RESIZE_GRIP)).left(px(CORNER)).right(px(CORNER)),
                ResizeDir::NorthWest => div().left(px(0.0)).top(px(0.0)).w(px(CORNER)).h(px(CORNER)),
                ResizeDir::NorthEast => div().right(px(0.0)).top(px(0.0)).w(px(CORNER)).h(px(CORNER)),
                ResizeDir::SouthWest => div().left(px(0.0)).bottom(px(0.0)).w(px(CORNER)).h(px(CORNER)),
                ResizeDir::SouthEast => div().right(px(0.0)).bottom(px(0.0)).w(px(CORNER)).h(px(CORNER)),
            };
            positioned
                .absolute()
                .cursor(dir.cursor())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        // "窗口变大/变小"要靠这行定位:是不是这条路径拖的窗口。
                        if let Some(rect) = win::window_rect(this.hwnd()) {
                            let scale =
                                win::dpi_at(win::POINT { x: rect.left, y: rect.top }) as f64
                                    / 96.0;
                            log::info(&format!("resize grip engaged: {dir:?}"));
                            this.drag = Some(DragState::Resize {
                                dir,
                                start_cursor: event.position,
                                start_rect: rect,
                                scale,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }),
                )
        }))
    }

    fn render_tab_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // Tabs live in the store as (id, label); clone the labels out so the
        // mutable tab switch below never overlaps the borrow.
        let tabs: Vec<(Option<String>, String)> = self
            .tabs
            .iter()
            .map(|tab| (tab.id.clone(), tab.label.clone()))
            .collect();
        let active_tab = self.tab.clone();

        div()
            .px(px(8.0))
            .py(px(4.0))
            .flex()
            .flex_row()
            .gap(px(2.0))
            .children(tabs.into_iter().map(|(id, label)| {
                let active = active_tab == id;
                div()
                    .id(SharedString::from(format!("tab-{label}")))
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .text_size(px(12.5))
                    .text_color(rgb(if active { COLOR_TEXT } else { COLOR_META }))
                    .bg(rgb(if active { COLOR_HOVER } else { COLOR_BG }))
                    .hover(|style| style.bg(rgb(COLOR_HOVER)))
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                        this.tab = id.clone();
                        this.refill(cx);
                        cx.notify();
                    }))
                    .child(label)
            }))
    }

    fn render_search_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let chips = self.chips.clone();

        let kind_label = match chips.kind {
            None => "类型 ▾".to_string(),
            Some(kind) => format!("类型:{}", kind.label()),
        };
        let time_label = if chips.time == TimeChip::All {
            "时间 ▾".to_string()
        } else {
            format!("时间:{}", chips.time.label())
        };

        div()
            .px(px(8.0))
            .py(px(8.0))
            .pt(px(4.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .id("chip-in-app")
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .text_size(px(12.5))
                    .text_color(rgb(if chips.in_app { COLOR_TEXT } else { COLOR_META }))
                    .bg(rgb(if chips.in_app { COLOR_HOVER } else { COLOR_BG }))
                    .hover(|style| style.bg(rgb(COLOR_HOVER)))
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                        this.chips.in_app = !this.chips.in_app;
                        this.refill(cx);
                        cx.notify();
                    }))
                    .child(format!("{} 应用", check_mark(chips.in_app))),
            )
            .child(
                div()
                    .id("chip-in-title")
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .text_size(px(12.5))
                    .text_color(rgb(if chips.in_title { COLOR_TEXT } else { COLOR_META }))
                    .bg(rgb(if chips.in_title { COLOR_HOVER } else { COLOR_BG }))
                    .hover(|style| style.bg(rgb(COLOR_HOVER)))
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                        this.chips.in_title = !this.chips.in_title;
                        this.refill(cx);
                        cx.notify();
                    }))
                    .child(format!("{} 标题", check_mark(chips.in_title))),
            )
            .child(
                div()
                    .id("chip-kind")
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .text_size(px(12.5))
                    .text_color(rgb(if chips.kind.is_some() { COLOR_TEXT } else { COLOR_META }))
                    .bg(rgb(if chips.kind.is_some() { COLOR_HOVER } else { COLOR_BG }))
                    .hover(|style| style.bg(rgb(COLOR_HOVER)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            this.dropdown = Some(DropdownState {
                                kind: DropdownKind::Type,
                                origin: event.position,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(kind_label),
            )
            .child(
                div()
                    .id("chip-time")
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .text_size(px(12.5))
                    .text_color(rgb(if chips.time != TimeChip::All { COLOR_TEXT } else { COLOR_META }))
                    .bg(rgb(if chips.time != TimeChip::All { COLOR_HOVER } else { COLOR_BG }))
                    .hover(|style| style.bg(rgb(COLOR_HOVER)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            this.dropdown = Some(DropdownState {
                                kind: DropdownKind::Time,
                                origin: event.position,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(time_label),
            )
            // 搜索框占满剩余宽度;它自己渲染文本和光标。
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(self.input.clone()),
            )
    }

    /// 遮罩。点击 = 关掉最上面的挂起物:菜单/下拉直接关,确认框算取消
    /// (和系统确认框点外面一个意思)。
    fn render_scrim(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(rgba(COLOR_SCRIM))
            .on_mouse_down(MouseButton::Left, cx.listener(
                |this, _: &MouseDownEvent, window: &mut Window, cx| {
                    if this.menu.take().is_some() || this.dropdown.take().is_some() {
                        cx.notify();
                        return;
                    }
                    if this.confirm_delete.take().is_some() {
                        window.focus(&this.input_focus);
                        cx.notify();
                    }
                },
            ))
    }

    fn menu_item(
        cx: &mut Context<Self>,
        label: SharedString,
        enabled: bool,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let item = div()
            .id(SharedString::from(format!("menu-{label}")))
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(4.0))
            .text_size(px(13.0))
            .text_color(rgb(if enabled { COLOR_TEXT } else { COLOR_META }))
            .when(enabled, |el| el.hover(|style| style.bg(rgb(COLOR_HOVER))))
            .child(label);
        if enabled {
            item.on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                this.menu = None;
                on_click(this, window, cx);
                cx.notify();
            }))
        } else {
            item
        }
    }

    fn render_context_menu(&self, menu: MenuState, cx: &mut Context<Self>) -> impl IntoElement {
        let item = self.items.get(menu.index);
        let can_edit = item
            .map(|item| self.store.can_edit(&item.stem))
            .unwrap_or(false);
        let pinned = item.map(|item| item.pinned).unwrap_or(false);
        let has_blob = item.map(|item| item.has_blob).unwrap_or(false);

        div()
            .absolute()
            .left(menu.origin.x)
            .top(menu.origin.y)
            .w(px(190.0))
            .bg(rgb(0x252525))
            .border_1()
            .border_color(rgb(COLOR_BORDER))
            .rounded(px(6.0))
            .py(px(4.0))
            .flex()
            .flex_col()
            .child(Self::menu_item(
                cx,
                "粘贴（Enter）".into(),
                true,
                |this, _, cx| this.commit(cx),
            ))
            .child(Self::menu_item(
                cx,
                "复制（Ctrl+C）".into(),
                true,
                |this, _, cx| this.copy_selected(cx),
            ))
            .child(Self::menu_item(
                // 与旧菜单同一条规则:只有文本、且归本机当月可写时才可编辑。
                cx,
                "编辑（Ctrl+E）".into(),
                can_edit,
                |this, _, _| this.edit_selected(),
            ))
            .child(Self::menu_item(
                cx,
                if pinned {
                    "取消固定（Ctrl+P）".into()
                } else {
                    "固定（Ctrl+P）".into()
                },
                true,
                |this, _, cx| this.toggle_pin(cx),
            ))
            .child(
                div()
                    .mx(px(8.0))
                    .my(px(3.0))
                    .h(px(1.0))
                    .bg(rgb(COLOR_BORDER)),
            )
            .child(Self::menu_item(
                cx,
                "删除（Delete）".into(),
                true,
                |this, window, _| {
                    this.delete_selected();
                    if this.confirm_delete.is_some() {
                        window.focus(&this.root_focus);
                    }
                },
            ))
            .child(
                div()
                    .mx(px(8.0))
                    .my(px(3.0))
                    .h(px(1.0))
                    .bg(rgb(COLOR_BORDER)),
            )
            .child(Self::menu_item(
                cx,
                "全选（Ctrl+A）".into(),
                true,
                |this, _, _| this.select_all(),
            ))
            .when(has_blob, |el| {
                el.child(
                    div()
                        .px(px(12.0))
                        .pt(px(4.0))
                        .text_size(px(11.0))
                        .text_color(rgb(COLOR_META))
                        .child("（含 .bin,操作在后台读取）"),
                )
            })
    }

    fn render_dropdown(&self, dropdown: DropdownState, cx: &mut Context<Self>) -> impl IntoElement {
        let kind_label = dropdown.kind;

        let entries: Vec<(String, bool)> = match dropdown.kind {
            DropdownKind::Type => {
                let mut entries = vec![("全部".to_string(), self.chips.kind.is_none())];
                for kind in [ClipKind::Text, ClipKind::Image, ClipKind::Files] {
                    entries.push((kind.label().to_string(), self.chips.kind == Some(kind)));
                }
                entries
            }
            DropdownKind::Time => [
                TimeChip::All,
                TimeChip::Today,
                TimeChip::Seven,
                TimeChip::Thirty,
            ]
            .into_iter()
            .map(|preset| (preset.label().to_string(), self.chips.time == preset))
            .collect(),
        };

        div()
            .absolute()
            .left(dropdown.origin.x)
            .top(dropdown.origin.y + px(22.0))
            .w(px(120.0))
            .bg(rgb(0x252525))
            .border_1()
            .border_color(rgb(COLOR_BORDER))
            .rounded(px(6.0))
            .py(px(4.0))
            .flex()
            .flex_col()
            .children(entries.into_iter().map(move |(label, active)| {
                div()
                    .id(SharedString::from(format!("dd-{kind_label:?}-{label}")))
                    .px(px(12.0))
                    .py(px(5.0))
                    .rounded(px(4.0))
                    .text_size(px(12.5))
                    .text_color(rgb(if active { COLOR_TEXT } else { COLOR_META }))
                    .hover(|style| style.bg(rgb(COLOR_HOVER)))
                    .on_click({
                        let label = label.clone();
                        cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                            match kind_label {
                                DropdownKind::Type => {
                                    this.chips.kind = match label.as_str() {
                                        "全部" => None,
                                        "文本" => Some(ClipKind::Text),
                                        "图片" => Some(ClipKind::Image),
                                        _ => Some(ClipKind::Files),
                                    };
                                }
                                DropdownKind::Time => {
                                    this.chips.time = match label.as_str() {
                                        "全部" => TimeChip::All,
                                        "今天" => TimeChip::Today,
                                        "近7天" => TimeChip::Seven,
                                        _ => TimeChip::Thirty,
                                    };
                                }
                            }
                            this.dropdown = None;
                            this.refill(cx);
                            cx.notify();
                        })
                    })
                    .child(label.clone())
            }))
    }

    /// 删除确认,画在所有面板之上,和 egui 版同一份文案。
    fn render_confirm_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.confirm_delete.as_ref().map(|s| s.len()).unwrap_or(0);
        let question = vec![
            format!("删除选中的 {count} 条记录？"),
            String::new(),
            "同步目录里的记录会一起删掉，其他机器同步之后也会跟着消失，删了找不回来。".to_string(),
            String::new(),
            "别的机器当月那份只能先记个「已删」的空标记（一样马上看不见），由那台机器自己清。".to_string(),
        ];

        div()
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(380.0))
                    .bg(rgb(0x252525))
                    .border_1()
                    .border_color(rgb(COLOR_BORDER))
                    .rounded(px(8.0))
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .children(question.into_iter().map(|line| {
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(COLOR_TEXT))
                            .child(line)
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .justify_end()
                            .child(
                                div()
                                    .id("confirm-delete")
                                    .px(px(14.0))
                                    .py(px(6.0))
                                    .rounded(px(4.0))
                                    .text_size(px(13.0))
                                    .bg(rgb(COLOR_SELECTED))
                                    .hover(|style| style.bg(rgb(COLOR_HOVER)))
                                    .on_click(cx.listener(
                                        |this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                                            this.run_confirm_delete(cx);
                                        },
                                    ))
                                    .child("删除"),
                            )
                            .child(
                                div()
                                    .id("confirm-cancel")
                                    .px(px(14.0))
                                    .py(px(6.0))
                                    .rounded(px(4.0))
                                    .text_size(px(13.0))
                                    .bg(rgb(COLOR_INPUT_BG))
                                    .hover(|style| style.bg(rgb(COLOR_HOVER)))
                                    .on_click(cx.listener(
                                        |this, _: &gpui::ClickEvent, window: &mut Window, cx| {
                                            this.confirm_delete = None;
                                            window.focus(&this.input_focus);
                                            cx.notify();
                                        },
                                    ))
                                    .child("取消"),
                            ),
                    ),
            )
    }
}

/// 按下列表里的一行之后的选择集与锚点。纯函数,单独测:普通点、Shift 扩选、
/// Ctrl 切换这三条语义旧弹窗就有,"点了不选中"的毛病正是从这儿冒出来的。
fn pressed_state(
    selected: &HashSet<usize>,
    anchor: usize,
    index: usize,
    shift: bool,
    ctrl: bool,
) -> (HashSet<usize>, usize) {
    if shift {
        let (lo, hi) = (anchor.min(index), anchor.max(index));
        ((lo..=hi).collect(), anchor)
    } else if ctrl {
        let mut next = selected.clone();
        if !next.remove(&index) {
            next.insert(index);
        }
        (next, index)
    } else {
        ([index].into_iter().collect(), index)
    }
}

/// 当前热键组合涉及的每个键是否都已物理松开。
///
/// 热键吞的是 key-down,松手产生的 key-up 会投给焦点窗口;热键收起路径用
/// 这个等"用户已松手",让那串孤儿 key-up 落进本窗口自灭。组合键解析不出
/// 来时返回 true——不知道该等什么,就不等。
fn hotkey_keys_all_up() -> bool {
    let Some(settings) = crate::current_settings() else {
        return true;
    };
    let Some(hotkey) = crate::settings::parse_hotkey(&settings.hotkey) else {
        return true;
    };

    let mut vks: Vec<i32> = Vec::new();
    if hotkey.modifiers & win::MOD_CONTROL != 0 {
        vks.push(win::VK_CONTROL as i32);
    }
    if hotkey.modifiers & win::MOD_ALT != 0 {
        vks.push(win::VK_MENU);
    }
    if hotkey.modifiers & win::MOD_SHIFT != 0 {
        vks.push(win::VK_SHIFT);
    }
    if hotkey.modifiers & win::MOD_WIN != 0 {
        // 左右 Win 是两颗物理键,任一按着都算组合还没松。
        vks.push(win::VK_LWIN);
        vks.push(win::VK_RWIN);
    }
    vks.push(hotkey.vk as i32);

    vks.iter().all(|vk| !win::key_held(*vk))
}

/// 解码好的 RGBA 进 gpui 的图片管线。失败(尺寸对不上字节数)返回 None。
fn render_image(w: usize, h: usize, rgba: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = ImageBuffer::from_raw(w as u32, h as u32, rgba)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

/// Where the popup opens: the remembered position if it still fits the work
/// area of the monitor it lands on, otherwise centred on that monitor — and
/// clamped back on screen either way.
fn placed(
    remembered: Option<(i32, i32)>,
    area: &win::RECT,
    width: i32,
    height: i32,
) -> (i32, i32) {
    let (left, top) = remembered.unwrap_or_else(|| {
        let left = area.left + (area.right - area.left - width) / 2;
        let top = area.top + (area.bottom - area.top - height) / 2;
        (left, top)
    });

    // `max(area.left)`: a window wider than the work area cannot be clamped into it,
    // and `clamp` panics when its bounds cross. The top-left corner is the fallback.
    (
        left.clamp(area.left, (area.right - width).max(area.left)),
        top.clamp(area.top, (area.bottom - height).max(area.top)),
    )
}

/// 按记住的位置/尺寸算出窗口该在哪、多大(没有记住就居中在鼠标所在的屏幕)。
/// 返回物理位置/尺寸和这一屏的缩放。不依赖任何实例状态,run() 建窗前也要用。
fn target_geometry() -> (i32, i32, i32, i32, f64) {
    let saved = crate::current_settings();
    let remembered = saved.as_ref().and_then(|s| s.popup_position);
    let size = saved
        .as_ref()
        .and_then(|s| s.popup_size)
        .unwrap_or(DEFAULT_SIZE);
    let cursor = win::cursor_position();
    let anchor = remembered
        .map(|(x, y)| win::POINT { x, y })
        .unwrap_or(cursor);
    let area = win::work_area_at(anchor);
    let scale = win::dpi_at(anchor) as f64 / 96.0;
    // ponytail: 这里用的是**锚定屏**的缩放,而 gpui 落地窗口 bounds 时用的是
    // 创建窗口时那块屏的缩放。两块屏缩放不同时这个换算是偏的——本机两块都是
    // 150%,改了也没法验证;要动就先把混合 DPI 的机器摆上。

    let width_px = win::scaled(size.0.max(MIN_SIZE.0), scale).min(area.right - area.left);
    let height_px = win::scaled(size.1.max(MIN_SIZE.1), scale).min(area.bottom - area.top);
    let (left, top) = placed(remembered, &area, width_px, height_px);

    (left, top, width_px, height_px, scale)
}

// ---------------------------------------------------------------- 搜索框

/// 搜索框内容变了。光标/选择的挪动不发这个,只 notify。
enum SearchInputEvent {
    Changed,
}

impl EventEmitter<SearchInputEvent> for SearchInput {}

/// 单行搜索框。gpui 本体不带文本输入控件,这是按 gpui 官方 input 示例裁出来
/// 的最小实件:单行、占位文本、光标、选择、中文 IME(marked text)全走
/// EntityInputHandler。内容变化以事件通知订阅方,光标移动只 notify。
struct SearchInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

impl SearchInput {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: "搜索内容 …".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.content = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        self.is_selecting = false;
        cx.emit(SearchInputEvent::Changed);
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify()
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn backspace(&mut self, _: &SearchBackspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &SearchDelete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.selected_range.end), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn left(&mut self, _: &SearchLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &SearchRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SearchSelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SearchSelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.selected_range.end), cx);
    }

    fn select_all(&mut self, _: &SearchSelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &SearchHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &SearchEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn paste(&mut self, _: &SearchPaste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn cut(&mut self, _: &SearchCut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn on_input_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        window.focus(&self.focus_handle);

        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
        cx.stop_propagation();
    }

    fn on_input_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_input_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }
}

impl EntityInputHandler for SearchInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        cx.emit(SearchInputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());

        cx.emit(SearchInputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;

        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<SearchInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    selection: Option<gpui::PaintQuad>,
    cursor: Option<gpui::PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let marked_range = input.marked_range.clone();
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), rgb(COLOR_META).into())
        } else {
            (content, style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(2.0), bounds.bottom() - bounds.top()),
                    ),
                    rgb(COLOR_TEXT),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x995A3C80),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            selection,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let line = prepaint.line.take().unwrap();
        let _ = line.paint(bounds.origin, window.line_height(), window, cx);

        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for SearchInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("SearchInput")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .bg(rgb(COLOR_INPUT_BG))
            .rounded(px(4.0))
            .h(px(30.0))
            .w_full()
            .px(px(8.0))
            .line_height(px(22.0))
            .text_size(px(15.0))
            .font_family(FONT_FAMILY)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_input_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_input_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_input_up))
            .on_mouse_move(cx.listener(Self::on_input_move))
            .child(TextElement { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按下一行之后的三种语义(普通点 / Shift 扩选 / Ctrl 切换)。"点了不选中"
    /// 就是这一段丢的,三条各钉一个。
    #[test]
    fn a_pressed_row_answers_with_the_selection_it_means() {
        let none: HashSet<usize> = HashSet::new();

        // 普通点:单选,锚点跟到这一行。
        let (selected, anchor) = pressed_state(&none, 0, 7, false, false);
        assert_eq!(selected, HashSet::from([7]));
        assert_eq!(anchor, 7);

        // Shift 点:从原来那个锚点连到这一行,锚点不动。
        let (selected, anchor) = pressed_state(&HashSet::from([7]), 3, 5, true, false);
        assert_eq!(selected, (3..=5).collect::<HashSet<usize>>());
        assert_eq!(anchor, 3);

        // Ctrl 点:在原来的集合里原地切换,锚点跟着走。
        let (selected, anchor) = pressed_state(&HashSet::from([7]), 3, 5, false, true);
        assert_eq!(selected, HashSet::from([5, 7]));
        assert_eq!(anchor, 5);
        let (selected, _) = pressed_state(&selected, 5, 5, false, true);
        assert!(!selected.contains(&5));
    }

    fn work_area() -> win::RECT {
        win::RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        }
    }

    /// Placement has two jobs — put it back where it was, and bring it back on
    /// screen when that is no longer a place — and the second one only ever shows
    /// up after a monitor changes, which is a bad moment to discover it is wrong.
    #[test]
    fn remembered_position_wins_unless_it_is_off_screen() {
        // Nothing remembered: centred, so the first open does not depend on the mouse.
        assert_eq!(placed(None, &work_area(), 600, 400), (660, 320));

        // Remembered: exactly where it was left, including against the right edge.
        assert_eq!(placed(Some((1300, 500)), &work_area(), 600, 400), (1300, 500));

        // From a monitor that is gone now: pulled back into this one.
        assert_eq!(placed(Some((2500, -300)), &work_area(), 600, 400), (1320, 0));

        // Wider than the screen it has to fit on: the top-left corner is the best
        // that can be done, and this is the case that panics without the `max`.
        assert_eq!(placed(Some((-500, -500)), &work_area(), 2000, 1200), (0, 0));
    }

    /// The time button's presets become absolute bounds at refill time, so the
    /// cutoff follows the clock: "近7天" picked yesterday means seven days from
    /// now, and "今天" is local midnight however far the UTC offset sits.
    #[test]
    fn time_chips_answer_with_absolute_bounds() {
        let now = crate::settings::now_ms();

        assert_eq!(TimeChip::All.since_ms(now), None);

        // Midnight is the instant minus the time of day, which is the whole trick.
        let t = win::local_datetime(now);
        let into_day = i64::from(t.hour) * 3_600_000
            + i64::from(t.minute) * 60_000
            + i64::from(t.second) * 1_000
            + i64::from(t.milliseconds);
        assert_eq!(TimeChip::Today.since_ms(now), Some(now - into_day));
        // The bound is inclusive: a clip captured at exactly midnight is "today".
        assert!(TimeChip::Today.since_ms(now).is_some_and(|since| since <= now));

        assert_eq!(TimeChip::Seven.since_ms(now), Some(now - 7 * 86_400_000));
        assert_eq!(TimeChip::Thirty.since_ms(now), Some(now - 30 * 86_400_000));
    }
}
