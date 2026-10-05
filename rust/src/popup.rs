//! The search popup.
//!
//! Raw Win32 on purpose: the search box is a real `EDIT` control, so IME, CJK
//! input, selection and the caret are the system's problem rather than mine. The
//! list is a real `LISTBOX` with owner-drawn rows, which keeps scrolling,
//! keyboard navigation and hit testing out of this file too.
//!
//! The window reads bottom-up: the newest clip is the row just above the search
//! box — where the input and the cursor are — and older clips stack up from there.
//!
//! Every pixel value below is a *logical* pixel at 96 DPI and is multiplied by
//! the monitor's scale factor before use — including the row layout, which is
//! what keeps the two text lines from colliding on a scaled display.
//!
//! The window is stretchable although it draws no frame: `WS_THICKFRAME` keeps the
//! resize edges real, `WM_NCCALCSIZE` takes the frame back off them, and
//! `WM_NCHITTEST` hands the edges over by hand. Both where it was left and how
//! big are remembered in `settings.json`, so the next open is the window the user
//! last left behind.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};

use crate::clip::{self, ClipKind, ClipPayload};
use crate::clipboard;
use crate::index::{ChipFilter, ClipSummary};
use crate::log;
use crate::store::{MachineTab, Store};
use crate::thumb;
use crate::win::{self, scaled, HBRUSH, HWND, LPARAM, LRESULT, WPARAM};

pub(crate) const WIDTH: i32 = 620;
const PAD: i32 = 10;
const SEARCH_HEIGHT: i32 = 30;
const GAP: i32 = 8;

/// The instance strip sits between the list and the search box, both of which are
/// anchored to the bottom edge. Tabs are sized to their label — a fixed width
/// ran long machine names into the next tab and the strip off the right edge —
/// and `TAB_WIDTH` survives as the fallback when a measurement fails.
// ponytail: a sixth instance runs off the right edge; wrap then.
const TAB_HEIGHT: i32 = 26;
const TAB_WIDTH: i32 = 96;
const TAB_GAP: i32 = 6;

/// The bottom bands, measured from the client's bottom edge: the search box, the
/// gap over it, the instance strip. The list is what grows above them, and it
/// reads bottom-up — the newest clip is the row nearest the search box — so the
/// input sits down here with it, and the newest row and the cursor end up in the
/// same corner of the window.
const SEARCH_FROM_BOTTOM: i32 = PAD + SEARCH_HEIGHT;
const TAB_FROM_BOTTOM: i32 = SEARCH_FROM_BOTTOM + 6 + TAB_HEIGHT;

/// Everything below the list: the strip, the search box and the padding under it.
const BOTTOM_BANDS: i32 = TAB_FROM_BOTTOM + GAP;

/// Chosen so the list still holds exactly eight whole rows: 10 + 46*8 + 80.
pub(crate) const HEIGHT: i32 = PAD + ROW_HEIGHT * 8 + BOTTOM_BANDS;
const ROW_HEIGHT: i32 = 46;
const LINE1_TOP: i32 = 4;
const LINE1_HEIGHT: i32 = 22;
const LINE2_TOP: i32 = 26;
const LINE2_HEIGHT: i32 = 17;

/// How thick the invisible resize edge is, in logical pixels. It comes out of the
/// padding around the controls, so it has to stay smaller than `PAD`.
const GRIP: i32 = 5;

/// Length of the two bars drawn in the top-right corner, the only visible sign
/// that the window can be stretched. It used to be the bottom-right corner; the
/// search box owns that edge now, and a child control would draw over the bars.
const GRIP_ARM: i32 = 8;

/// Smallest the user can drag it down to: three rows of list, the tab strip and the
/// search box.
pub(crate) const MIN_WIDTH: i32 = 420;
pub(crate) const MIN_HEIGHT: i32 = PAD + ROW_HEIGHT * 3 + BOTTOM_BANDS;

const MAX_RESULTS: usize = 300;
const SUBCLASS_ID: usize = 1;

/// The `?` next to the search box, and the gap before it. Fixed size, like the box it
/// explains, and square so it reads as a button in the corner of the input row.
const HELP_SIZE: i32 = 30;
const HELP_GAP: i32 = 6;
const ID_HELP: usize = 1;

/// The four buttons on the input row, left of the search box. Two checkboxes —
/// whether bare terms also look in the exe name and the window title — and two
/// menus — which kind, how old. Painted like the tabs rather than made of
/// controls, so they share the strip's brushes and none of them can take the
/// keyboard away from the search box.
const FILTER_BOX_WIDTH: i32 = 64;
const FILTER_MENU_WIDTH: i32 = 78;
const FILTER_GAP: i32 = 6;

/// The filter menus' ids. The row menu owns 1..=5, so each menu gets its own range
/// with `全部` first: kind at 100, time at 140.
const MENU_KIND: i32 = 100;
const MENU_TIME: i32 = 140;

/// Which button a rect belongs to. The order is the order they are drawn in,
/// left to right: the two scope boxes first, then the two filter menus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chip {
    AppBox,
    TitleBox,
    Kind,
    Time,
}

const ALL_CHIPS: [Chip; 4] = [Chip::AppBox, Chip::TitleBox, Chip::Kind, Chip::Time];

impl Chip {
    /// Fixed width in logical pixels: wide enough for the widest face the button
    /// shows, and an over-long label is the text's problem, not the layout's.
    fn width(self) -> i32 {
        match self {
            Chip::AppBox | Chip::TitleBox => FILTER_BOX_WIDTH,
            Chip::Kind | Chip::Time => FILTER_MENU_WIDTH,
        }
    }

    /// The face the button shows. A box is a checkbox — `☑` holding, `☐` not —
    /// and a menu is bare with an arrow while nothing is picked, the current
    /// value once one is.
    fn label(self, chips: &Chips) -> String {
        match self {
            Chip::AppBox => format!("{} 应用", check_mark(chips.in_app)),
            Chip::TitleBox => format!("{} 标题", check_mark(chips.in_title)),
            Chip::Kind => match chips.kind {
                None => "类型 ▾".to_string(),
                Some(kind) => format!("类型:{}", kind.label()),
            },
            Chip::Time => match chips.time {
                TimeChip::All => "时间 ▾".to_string(),
                TimeChip::Today => "时间:今天".to_string(),
                TimeChip::Seven => "时间:近7天".to_string(),
                TimeChip::Thirty => "时间:近30天".to_string(),
            },
        }
    }

    /// Whether the button is holding something. The active face is the
    /// selected-row colour, so a popup that shows fewer rows than expected —
    /// or finds more than it used to — explains itself.
    fn active(self, chips: &Chips) -> bool {
        match self {
            Chip::AppBox => chips.in_app,
            Chip::TitleBox => chips.in_title,
            Chip::Kind => chips.kind.is_some(),
            Chip::Time => chips.time != TimeChip::All,
        }
    }
}

fn check_mark(on: bool) -> &'static str {
    if on {
        "☑"
    } else {
        "☐"
    }
}

/// The time button's presets. Held as the preset rather than a timestamp, so the
/// cutoff follows the clock: "近7天" picked yesterday means seven days from now,
/// not from when it was picked.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum TimeChip {
    #[default]
    All,
    Today,
    Seven,
    Thirty,
}

impl TimeChip {
    /// The inclusive lower bound the preset means right now, in unix ms. "今天" is
    /// local midnight, and it is found by subtracting the time of day — exact at
    /// this instant, and indifferent to what the UTC offset or DST is doing.
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
/// where bare terms look — so they persist across opens, like the tabs: ticking
/// 应用 is a preference, not a query. The two menus are filters, and reset with
/// the search box — a popup summoned to paste something is not asking to still
/// be narrowing last session's results.
#[derive(Clone, Default, PartialEq, Eq)]
struct Chips {
    in_app: bool,
    in_title: bool,
    kind: Option<ClipKind>,
    time: TimeChip,
}

/// What the row menu's items come back as. The menu is tracked with `TPM_RETURNCMD`
/// and the id is read from its return value, so these never travel as a `WM_COMMAND`.
const CMD_PASTE: i32 = 1;
const CMD_COPY: i32 = 2;
const CMD_PIN: i32 = 3;
const CMD_DELETE: i32 = 4;
const CMD_SELECT_ALL: i32 = 5;
const CMD_EDIT: i32 = 6;

/// What the `?` says. The buttons are the main path and the prefixes the power
/// user's; both are listed because this is the one place the whole search syntax
/// can be read without leaving the popup.
const HELP_TEXT: &str = "\
默认只搜记录内容（前 512 字），空格分开的每个词都要命中，顺序和距离随便。

搜索框左边四个按钮：
  ☑ 应用 / ☑ 标题   勾上后，输入的词也会去搜来源程序（exe 名）和复制那一刻的窗口标题；
                    勾选会一直记着，不随弹窗关闭重置
  类型 ▾            点开筛 文本 / 图片 / 文件
  时间 ▾            点开筛 今天 / 近7天 / 近30天；类型和时间每次打开弹窗会清空

要打字也可以用字段词缀，和按钮、和普通词都能混用：
  app:chrome        来源程序（只存 exe 名）
  title:报表        当时那个窗口的标题（存 120 字）
  time:02-14        行上显示的时间，也能写 time:2025-02
  machine:本机      机器：本机，或对方的机器 id
  kind:图片         类型：文本/图片/文件，也可写 text/image/files

不认识的词缀不算词缀——文本里写着 12:30 的照样搜得到（全角冒号 ： 也认）。";

/// Posted by a hydrating thread when the payload it was reading is ready. The
/// result itself travels through `Popup::hydrated`; the message only wakes the
/// window thread, which owns everything that happens next (write, hide, paste).
/// `+ 2`, not `+ 1`: the tray's callback message has that, and staying
/// distinct costs nothing even though the two go to different windows.
const WM_APP_HYDRATED: u32 = win::WM_APP + 2;
const WM_APP_TOGGLE: u32 = win::WM_APP + 3;
/// Posted by the thumbnail worker when a decode landed in the slot. The tray's
/// callback message owns `+ 1` and the platform thread's commands `+ 5..= 7`,
/// so `+ 8` keeps every queue in the process unambiguous.
const WM_APP_THUMB: u32 = win::WM_APP + 8;

/// What a row shows in place of its preview while its payload is being read.
const LOADING_HINT: &str = "正在加载完整内容 …";

/// COLORREF is 0x00BBGGRR, not RGB.
const COLOR_BG: u32 = 0x001E_1E1E;
const COLOR_INPUT_BG: u32 = 0x002A_2A2A;
const COLOR_SELECTED: u32 = 0x0099_5A3C;
const COLOR_TEXT: u32 = 0x00E6_E6E6;
const COLOR_META: u32 = 0x008C_8C8C;
const COLOR_PIN: u32 = 0x004A_A2D2;

/// Thumbnails decode into this box edge; the row scales whatever it gets.
const THUMB_PX: usize = 96;
/// The image square in a row: the row height minus this much on each side,
/// centred vertically — the same face every image row shows.
const THUMB_INSET: i32 = 6;
const THUMB_GAP: i32 = 8;
/// The LRU the decoded bitmaps live in, oldest evicted first.
const THUMB_CACHE_CAP: usize = 160;

/// What an empty list says where its rows would be.
const HINT_WHEN_EMPTY: &str = "没有匹配的记录";

/// How long hiding may wait for the hotkey's keys to come back up. The popup
/// is usually hidden by the very combination that opened it; hiding while the
/// keys are still down strands their key-ups in whatever window gets the
/// focus, and an IME can complete a stranded one into a stray 右Alt.
const HOTKEY_RELEASE_WAIT_MS: i64 = 1000;
const HOTKEY_TIMER_ID: usize = 1;
/// How often the wait polls the physical key state.
const HOTKEY_POLL_MS: u32 = 16;
/// Deactivation inside this window after a show does not hide the popup: the
/// first instants can jitter while the foreground changes hands.
const SHOW_GRACE_MS: i64 = 300;

/// What a hydrating job was started for. Paste goes back into the window the
/// popup took focus from; copy only fills the clipboard; edit opens the editor
/// on the full text.
#[derive(Clone, Copy, PartialEq)]
enum HydrateAction {
    Paste,
    Copy,
    Edit,
}

/// One decoded thumbnail as the row will draw it: the GDI bitmap plus the
/// pixel size to fit the square around.
#[derive(Clone, Copy)]
struct ThumbEntry {
    bmp: win::HBITMAP,
    width: i32,
    height: i32,
}

/// A hydration job's result, carried from the worker thread to the window proc.
struct Hydrated {
    /// The job's generation. Anything that supersedes it — a newer paste or
    /// copy, hiding the popup — bumps the counter, and a stale result is
    /// dropped instead of acted on.
    id: usize,
    action: HydrateAction,
    /// Stems the payload was built from, for the logs.
    stems: Vec<String>,
    /// `None` when nothing could be read; each action has its own way to
    /// report that.
    payload: Option<ClipPayload>,
}

/// One decoded thumbnail, from the worker thread to the window proc. The RGBA
/// is already downscaled to the `THUMB_PX` box; turning it into a bitmap is
/// GDI work, which belongs to the thread that will draw it.
struct ThumbResult {
    stem: String,
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

struct Popup {
    hwnd: HWND,
    search: HWND,
    list: HWND,
    /// The `?` that explains the search box. A control of its own so it can be clicked
    /// without the popup having to hit test anything.
    help: HWND,
    store: Arc<Store>,
    items: Mutex<Vec<ClipSummary>>,
    /// Which instance the list is showing. `None` is the "everything" tab.
    tab: Mutex<Option<String>>,
    /// What the four buttons hold. Cleared on open are the two menus, with the
    /// search box; the two scope boxes persist, like the tabs.
    chips: Mutex<Chips>,
    /// The strip as last drawn: hit tested by `tab_click`, and compared so that
    /// a repaint only happens when the set of instances actually changed.
    tabs: Mutex<Vec<MachineTab>>,
    /// Where a Shift+arrow extension started. Reset by any single move, so the
    /// range grows from the same place instead of from the moving caret.
    anchor: AtomicIsize,
    /// Window that had focus before the popup opened: the paste target. A
    /// launcher palette holding the caret without owning the foreground wins
    /// over the window behind it — the focus is where a keystroke lands.
    target: AtomicIsize,
    visible: AtomicBool,
    /// Whether a message box the popup itself put up is open. The popup hides itself
    /// the moment it loses the activation, and a box is the one thing allowed to take
    /// it — otherwise the box would take the popup down with it.
    modal_open: AtomicBool,
    /// The current hydration generation, bumped by every new job and by every
    /// close of the popup. See `Hydrated::id`.
    generation: AtomicUsize,
    /// Handshake from the hydrating thread to the window proc: the thread fills
    /// it and posts `WM_APP_HYDRATED`, and the window side takes it from there.
    hydrated: Mutex<Option<Hydrated>>,
    /// Decoded thumbnails by stem. GDI bitmaps cost a `CreateDIBSection` and a
    /// copy each, so they are kept and shared by every repaint of their row.
    thumbs: Mutex<HashMap<String, ThumbEntry>>,
    /// LRU order for `thumbs`, oldest first; eviction deletes the bitmap.
    thumb_order: Mutex<VecDeque<String>>,
    /// Stems a decode is already running for, so a row's paints cannot queue
    /// the same read twice while it waits.
    thumb_pending: Mutex<HashSet<String>>,
    /// Handshake from the thumbnail worker, the same pattern as `hydrated`:
    /// the worker pushes and posts `WM_APP_THUMB`, the window thread drains.
    thumb_done: Mutex<VecDeque<ThumbResult>>,
    /// `now_ms` of the last show. Deactivations within `SHOW_GRACE_MS` of it
    /// do not hide the popup — the first instants can jitter while the
    /// foreground changes hands.
    shown_at: AtomicIsize,
    /// `now_ms` past which the hotkey release wait gives up and hides anyway.
    /// Zero when no wait is running; see `hide_after_hotkey_release`.
    hotkey_hide_deadline: AtomicIsize,
    /// Scale factor x100, so a plain integer atomic can carry it.
    scale: AtomicIsize,
    font_main: AtomicIsize,
    font_meta: AtomicIsize,
    brush_bg: HBRUSH,
    brush_input: HBRUSH,
    brush_selected: HBRUSH,
    brush_meta: HBRUSH,
}

static POPUP: OnceLock<Popup> = OnceLock::new();

fn popup() -> Option<&'static Popup> {
    POPUP.get()
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
        // Resizable without a frame: `WM_NCCALCSIZE` answers 0, so the frame this
        // style brings never takes a band off the client area.
        win::WS_POPUP | win::WS_THICKFRAME,
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
        HEIGHT - SEARCH_FROM_BOTTOM,
        WIDTH - PAD * 2,
        SEARCH_HEIGHT,
    );

    let list_style = win::WS_CHILD
        | win::WS_VISIBLE
        | win::WS_VSCROLL
        | win::LBS_NOTIFY
        | win::LBS_OWNERDRAWFIXED
        | win::LBS_HASSTRINGS
        | win::LBS_EXTENDEDSEL
        | win::LBS_NOINTEGRALHEIGHT;

    let list = win::create_child(
        "LISTBOX",
        "",
        list_style,
        hwnd,
        PAD,
        PAD,
        WIDTH - PAD * 2,
        HEIGHT - PAD - BOTTOM_BANDS,
    );

    let help = win::create_child_id(
        "STATIC",
        "?",
        win::WS_CHILD
            | win::WS_VISIBLE
            | win::SS_CENTER
            | win::SS_CENTERIMAGE
            | win::SS_NOTIFY,
        hwnd,
        ID_HELP,
        WIDTH - PAD - HELP_SIZE,
        HEIGHT - SEARCH_FROM_BOTTOM,
        HELP_SIZE,
        SEARCH_HEIGHT,
    );

    if search == 0 || list == 0 || help == 0 {
        log::error("popup child controls could not be created");
        return false;
    }

    // What the box can do besides plain text, said in the one place the user looks
    // when it is empty. The buttons sit right beside it and the `?` has the long
    // version, so the banner only has to point at them — a banner that wraps is
    // worse than none. `1` keeps it visible while focused, which the popup does
    // as soon as it opens.
    let cue = win::wide("搜索内容 · 左侧按钮可筛选");
    unsafe {
        win::SendMessageW(search, win::EM_SETCUEBANNER, 1, cue.as_ptr() as LPARAM);
    }

    // The list's scrollbar is the one thing in this window Windows draws itself,
    // and left alone it is a white stripe down a dark popup. Best-effort: on a
    // build without the dark theme class this does nothing at all.
    win::dark_theme(list);

    let state = Popup {
        hwnd,
        search,
        list,
        help,
        store: Arc::clone(&store),
        items: Mutex::new(Vec::new()),
        tab: Mutex::new(None),
        chips: Mutex::new(Chips::default()),
        tabs: Mutex::new(Vec::new()),
        anchor: AtomicIsize::new(0),
        target: AtomicIsize::new(0),
        visible: AtomicBool::new(false),
        modal_open: AtomicBool::new(false),
        generation: AtomicUsize::new(0),
        hydrated: Mutex::new(None),
        thumbs: Mutex::new(HashMap::new()),
        thumb_order: Mutex::new(VecDeque::new()),
        thumb_pending: Mutex::new(HashSet::new()),
        thumb_done: Mutex::new(VecDeque::new()),
        shown_at: AtomicIsize::new(0),
        hotkey_hide_deadline: AtomicIsize::new(0),
        scale: AtomicIsize::new(100),
        font_main: AtomicIsize::new(0),
        font_meta: AtomicIsize::new(0),
        brush_bg: unsafe { win::CreateSolidBrush(COLOR_BG) },
        brush_input: unsafe { win::CreateSolidBrush(COLOR_INPUT_BG) },
        brush_selected: unsafe { win::CreateSolidBrush(COLOR_SELECTED) },
        brush_meta: unsafe { win::CreateSolidBrush(COLOR_META) },
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

    // The list gets the same treatment, because clicking a row moves the focus
    // into it and Enter would otherwise stop working until the search box was
    // clicked again.
    let list_subclass: win::SUBCLASSPROC = list_proc;
    if unsafe { win::SetWindowSubclass(list, list_subclass, SUBCLASS_ID, 0) } == 0 {
        log::warn("SetWindowSubclass failed for the list; Enter and Ctrl+C will need the search box");
    }

    // One worker, serially reading payloads and decoding thumbnails. The reads
    // can be OneDrive placeholder downloads, so they must never happen on the
    // window thread; the results come back through `thumb_done` + a post, the
    // same handshake the hydrating threads use.
    if let Err(err) = spawn_thumb_worker(Arc::clone(&store)) {
        log::warn(&format!(
            "thumbnail worker could not be spawned: {err}; image rows keep their placeholder"
        ));
    }

    log::info(&format!("popup window ready (hwnd {hwnd:#x})"));
    true
}

/// Where the window thread sends decode requests; set once in `create`.
static THUMB_TX: OnceLock<mpsc::Sender<String>> = OnceLock::new();

/// Spawns the thumbnail worker and parks its request sender in `THUMB_TX`.
fn spawn_thumb_worker(store: Arc<Store>) -> Result<(), std::io::Error> {
    let (tx, rx) = mpsc::channel::<String>();
    THUMB_TX
        .set(tx)
        .map_err(|_| std::io::Error::other("thumbnail worker already spawned"))?;

    std::thread::Builder::new()
        .name("clipplus-thumbs".into())
        .spawn(move || {
            for stem in rx {
                // A failed read or decode sends nothing back: the row keeps its
                // placeholder and its `thumb_pending` entry, so the ask never
                // repeats for the same clip — the same answer a broken image
                // gets everywhere else on Windows.
                let Some(payload) = store.read_payload(&stem) else {
                    continue;
                };
                let ClipPayload::Image(png) = payload else {
                    continue;
                };
                let Some((width, height, rgba)) = thumb::decode_png_rgba(&png) else {
                    continue;
                };
                let (width, height, rgba) = thumb::downscale_rgba(&rgba, width, height, THUMB_PX);

                let Some(p) = popup() else {
                    break;
                };
                {
                    let mut done = p.thumb_done.lock().unwrap_or_else(|e| e.into_inner());
                    done.push_back(ThumbResult {
                        stem,
                        width,
                        height,
                        rgba,
                    });
                }
                win::post_message(p.hwnd, WM_APP_THUMB, 0, 0);
            }
        })?;

    Ok(())
}

pub fn toggle() {
    if is_visible() {
        hide_after_hotkey_release();
    } else {
        show();
    }
}

/// Hides the popup — but when the hotkey's keys are still physically down,
/// waits for them to come back up first, polling on a timer and giving up at
/// `HOTKEY_RELEASE_WAIT_MS`. The popup is usually dismissed by the same
/// combination that summoned it; hiding while the keys are down strands their
/// key-ups in whatever window receives them, and an IME can turn a stranded
/// one into a stray 右Alt that fires the hotkey again.
fn hide_after_hotkey_release() {
    let Some(p) = popup() else {
        return;
    };

    let hotkey = crate::current_settings().and_then(|s| crate::settings::parse_hotkey(&s.hotkey));
    let hotkey_held = hotkey.is_some_and(|h| hotkey_keys_held(h));

    if !hotkey_held {
        hide();
        return;
    }

    p.hotkey_hide_deadline.store(
        (crate::settings::now_ms() + HOTKEY_RELEASE_WAIT_MS) as isize,
        Ordering::SeqCst,
    );
    unsafe {
        win::SetTimer(p.hwnd, HOTKEY_TIMER_ID, HOTKEY_POLL_MS, 0);
    }
}

/// Whether any key of the hotkey combination is physically down right now.
/// The async state reads the hardware stream, so it sees the keys even though
/// this window does not have the focus yet. Every modifier of the combination
/// counts, as does the main key itself.
fn hotkey_keys_held(hotkey: crate::settings::Hotkey) -> bool {
    let mut keys: Vec<i32> = Vec::new();
    if hotkey.modifiers & win::MOD_ALT != 0 {
        keys.push(win::VK_MENU);
    }
    if hotkey.modifiers & win::MOD_CONTROL != 0 {
        keys.push(win::VK_CONTROL as i32);
    }
    if hotkey.modifiers & win::MOD_SHIFT != 0 {
        keys.push(win::VK_SHIFT);
    }
    if hotkey.modifiers & win::MOD_WIN != 0 {
        keys.push(win::VK_LWIN);
        keys.push(win::VK_RWIN);
    }

    keys.iter().any(|vk| win::key_held(*vk)) || win::key_held(hotkey.vk as i32)
}

/// The hotkey and the tray icon fire on the platform thread, but window
/// operations belong to the thread that created this one: turn the request
/// into a message on the popup itself and let the main loop run `toggle`.
pub fn request_toggle() {
    if let Some(p) = popup() {
        win::post_message(p.hwnd, WM_APP_TOGGLE, 0, 0);
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
    // (The launcher-palette rule and the rest of the policy live with the paste
    // machinery in `paste.rs`.)
    p.target.store(crate::paste::capture_paste_target(), Ordering::SeqCst);

    let empty = win::wide("");
    unsafe {
        win::SetWindowTextW(p.search, empty.as_ptr());
    }
    // The menus reset with the box; the scope boxes are preferences and stay.
    {
        let mut chips = p.chips.lock().unwrap_or_else(|e| e.into_inner());
        chips.kind = None;
        chips.time = TimeChip::All;
    }
    reload();

    let cursor = win::cursor_position();

    // The position is remembered rather than derived from the mouse: the user put
    // the popup where they want it, and having it follow the cursor on every open
    // is what made dragging it pointless. Only the very first open — before there
    // is anything to remember — has to pick a monitor, and that is whichever one
    // the cursor is on. The size is remembered the same way, in logical pixels, so
    // a monitor with another scale gets the size it would have had.
    let saved = crate::current_settings();
    let remembered = saved.as_ref().and_then(|settings| settings.popup_position);
    let size = saved
        .as_ref()
        .and_then(|settings| settings.popup_size)
        .unwrap_or((WIDTH, HEIGHT));

    let anchor = remembered.map(|(x, y)| win::POINT { x, y }).unwrap_or(cursor);
    let area = win::work_area_at(anchor);
    let scale = win::dpi_at(anchor) as f64 / 96.0;

    // Floored by the same minimum a drag enforces, and never larger than the work
    // area: a size remembered from a monitor that is gone must not open the popup
    // hanging off the screen this one is on.
    let width = scaled(size.0.max(MIN_WIDTH), scale).min(area.right - area.left);
    let height = scaled(size.1.max(MIN_HEIGHT), scale).min(area.bottom - area.top);

    let (left, top) = placed(remembered, &area, width, height);

    ensure_fonts(p, scale);

    unsafe {
        // Scaled: the row boxes have to grow with the font, or the two lines
        // overlap. This is the bug that made the list look wrong at any DPI
        // above 100%.
        let row_height = scaled(ROW_HEIGHT, scale) as LPARAM;
        win::SendMessageW(p.list, win::LB_SETITEMHEIGHT, 0, row_height);

        win::SetWindowPos(
            p.hwnd,
            win::HWND_TOPMOST,
            left,
            top,
            width,
            height,
            win::SWP_SHOWWINDOW,
        );
    }

    // The controls sit inside the client area, which is what `WM_SIZE` reports from
    // here on: this is the same call that lays them out again after a stretch.
    layout(p, width, height, scale);
    scroll_to_newest(p);

    // `SetWindowPos` is meant to activate the window, and from a hotkey it is not
    // dependable about it. A popup that never got the foreground gets no keystrokes
    // — Esc does nothing — and never receives the `WM_ACTIVATE` that hides it again,
    // so clicking elsewhere leaves it on screen. Both halves of that are logged.
    let foreground = win::focus_window(p.hwnd);

    unsafe {
        win::SetFocus(p.search);
    }

    let active = win::foreground_window() == p.hwnd;
    let rows = p.items.lock().unwrap_or_else(|e| e.into_inner()).len();
    log::info(&format!(
        "popup shown at {left},{top} {width}x{height} scale {scale:.2} ({rows} rows), \
         foreground={foreground} active={active}"
    ));
    p.visible.store(true, Ordering::SeqCst);
    p.shown_at
        .store(crate::settings::now_ms() as isize, Ordering::SeqCst);
}

pub fn hide() {
    let Some(p) = popup() else {
        return;
    };

    // A release wait that never fired is over as soon as the popup is gone,
    // whichever path got there first.
    p.hotkey_hide_deadline.store(0, Ordering::SeqCst);
    unsafe {
        win::KillTimer(p.hwnd, HOTKEY_TIMER_ID);
    }

    unsafe {
        win::ShowWindow(p.hwnd, win::SW_HIDE);
    }
    p.visible.store(false, Ordering::SeqCst);

    // A hydration that has not finished yet was started for a popup the user can
    // no longer see; Esc or a click elsewhere means it should never act. The
    // generation check when its result arrives is what drops it.
    p.generation.fetch_add(1, Ordering::SeqCst);
}

/// The `?` beside the search box: what the field prefixes are.
///
/// A message box rather than a panel drawn inside the popup: this is a list of text,
/// and a box brings its own wrapping and its own Esc.
fn show_help() {
    boxed(
        "ClipPlus 搜索",
        HELP_TEXT,
        win::MB_OK | win::MB_ICONINFORMATION,
    );
}

/// A message box the popup has to survive.
///
/// The box takes the activation, and the popup hides itself the moment it loses it —
/// which would take the box down with it. This flag is what keeps the popup up for as
/// long as the box is; it is cleared as soon as the box closes, and the focus goes
/// back to the search box, because by then the box has it.
///
/// Returns the button the user pressed.
fn boxed(title: &str, text: &str, flags: u32) -> i32 {
    let Some(p) = popup() else {
        return win::message_box(title, text, flags);
    };

    p.modal_open.store(true, Ordering::SeqCst);
    let answer = win::message_box(title, text, flags);
    p.modal_open.store(false, Ordering::SeqCst);

    win::focus_window(p.hwnd);
    unsafe {
        win::SetFocus(p.search);
    }

    answer
}

/// Places the two controls inside a client area of `width` x `height`. Shared by
/// the first open and by every stretch, so the two can never disagree about where
/// the list ends.
///
/// The list is anchored by its bottom edge and is only as tall as the rows it
/// holds, so with fewer rows than fit the newest clip stays right above the search
/// box instead of drifting to the top of the window with a hole under it.
fn layout(p: &Popup, width: i32, height: i32, scale: f64) {
    let pad = scaled(PAD, scale);
    let row_height = scaled(ROW_HEIGHT, scale).max(1);
    let list_bottom = (height - scaled(BOTTOM_BANDS, scale)).max(pad);
    let rows = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        rows_for(items.len(), list_bottom - pad, row_height)
    };

    let list_height = rows * row_height;
    let list_top = list_bottom - list_height;

    let help_width = scaled(HELP_SIZE, scale);
    let search_top = height - scaled(SEARCH_FROM_BOTTOM, scale);
    // Four buttons and the gap between the last of them and the box: what the
    // input row hands to the filters before the search box gets its share.
    let filter_span = scaled(
        FILTER_BOX_WIDTH * 2 + FILTER_MENU_WIDTH * 2 + FILTER_GAP * 4,
        scale,
    );

    unsafe {
        win::SetWindowPos(
            p.search,
            0,
            pad + filter_span,
            search_top,
            // The `?` takes its corner out of the box rather than sitting over it,
            // so a long query is never hidden behind the thing that explains it.
            // The filter buttons take a bite off the left edge the same way.
            width - pad * 2 - help_width - scaled(HELP_GAP, scale) - filter_span,
            scaled(SEARCH_HEIGHT, scale),
            win::SWP_NOACTIVATE,
        );
        win::SetWindowPos(
            p.help,
            0,
            width - pad - help_width,
            search_top,
            help_width,
            scaled(SEARCH_HEIGHT, scale),
            win::SWP_NOACTIVATE,
        );
        win::SetWindowPos(
            p.list,
            0,
            pad,
            list_top,
            width - pad * 2,
            list_height,
            win::SWP_NOACTIVATE,
        );
    }
}

/// How many rows the list gets: the rows it holds, or the whole area when it holds
/// none — this is a list with a filter, and a list that collapses to nothing looks
/// broken rather than empty.
///
/// `available` is the space above the bottom bands and `row_height` is one row of
/// it, both in physical pixels. `max(1)` on the way in: a window squeezed below the
/// minimum still has to place its children somewhere, and a row count of zero is not
/// one of the answers.
fn rows_for(count: usize, available: i32, row_height: i32) -> i32 {
    let fits = (available / row_height).max(1);
    if count == 0 {
        fits
    } else {
        (count as i32).min(fits)
    }
}

/// One page of the page keys: the rows that fit, less one, so the row the caret
/// left stays on screen instead of the next page landing with no context. Never
/// zero — a window squeezed down to one row still has to move somewhere.
fn page_rows(visible: i32) -> i32 {
    (visible - 1).max(1)
}

/// Where a band anchored to the bottom edge starts, in physical pixels:
/// `above_bottom` is the distance from the client's bottom edge to the band's top
/// edge, in logical pixels. Zero when the size is not known yet, which draws the
/// strip off the bottom rather than in the wrong place.
fn bottom_band_top(hwnd: HWND, above_bottom: i32, scale: f64) -> i32 {
    match win::client_size(hwnd) {
        Some((_, height)) => (height - scaled(above_bottom, scale)).max(0),
        None => 0,
    }
}

fn ensure_fonts(p: &'static Popup, scale: f64) {
    let key = (scale * 100.0) as isize;
    if p.scale.load(Ordering::SeqCst) == key && p.font_main.load(Ordering::SeqCst) != 0 {
        return;
    }

    // The same size the settings window uses, so the two windows read as one
    // interface: `UI_FONT_HEIGHT` is that size, and this is the one place the
    // popup wants it. The metadata line stays deliberately smaller.
    let main_height = -scaled(win::UI_FONT_HEIGHT, scale);
    let meta_height = -scaled(12, scale);

    unsafe {
        let main = win::ui_font(main_height);
        let meta = win::ui_font(meta_height);

        // The two real controls in the input row, the search box and the `?` beside
        // it, are told their font; they were created at whatever DPI the popup was
        // first opened on, and the rows between them are drawn by this file anyway.
        win::SendMessageW(p.search, win::WM_SETFONT, main as usize, 1);
        win::SendMessageW(p.help, win::WM_SETFONT, main as usize, 1);

        // Stored rather than deleted on replacement: `win::ui_font` owns the
        // handle and hands the same one back for the same height.
        p.font_main.store(main, Ordering::SeqCst);
        p.font_meta.store(meta, Ordering::SeqCst);
    }

    p.scale.store(key, Ordering::SeqCst);
}

/// Re-derives the tab strip and refills the list. Used where the set of
/// instances may have changed — the popup opening, a tab switching — which is
/// also where an O(index) machine walk is affordable.
fn reload() {
    let Some(p) = popup() else {
        return;
    };

    let tabs = p.store.machines();

    // Derived from what is on disk, so a database that syncs in, or one that is
    // cleaned up, adds or removes a tab while the popup is open.
    {
        let mut current = p.tab.lock().unwrap_or_else(|e| e.into_inner());
        if !tabs.iter().any(|tab| tab.id == *current) {
            *current = None;
        }
    }

    let strip_changed = {
        let mut drawn = p.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *drawn != tabs;
        *drawn = tabs;
        changed
    };

    fill_list();

    if strip_changed {
        unsafe {
            win::InvalidateRect(p.hwnd, std::ptr::null(), 1);
        }
    }
}

/// Refills the list from the index. This is the per-keystroke path: search
/// typing cannot change the set of instances, so the machine walk that reload
/// also does stays out of it — with a large history it is the expensive half
/// of a keystroke, and it runs on the window thread.
fn fill_list() {
    let Some(p) = popup() else {
        return;
    };

    let filter = win::window_text(p.search);
    let selected = p.tab.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let chips = p.chips.lock().unwrap_or_else(|e| e.into_inner()).clone();

    // The time preset becomes its bound here rather than in the index, so the
    // cutoff follows the clock on every refill and the index never reads it.
    let chip_filter = ChipFilter {
        in_app: chips.in_app,
        in_title: chips.in_title,
        kind: chips.kind,
        since_ms: chips.time.since_ms(crate::settings::now_ms()),
    };

    // Bottom-up: the newest clip is the last row, the one next to the search box, so
    // this is the reverse of the ranking order the index hands out.
    let mut summaries = p.store.query(selected.as_deref(), &filter, &chip_filter, MAX_RESULTS);
    summaries.reverse();

    // Stored before the list is refilled, so a row drawn from `p.items` is
    // never out of step with the string the list box kept.
    *p.items.lock().unwrap_or_else(|e| e.into_inner()) = summaries.clone();

    unsafe {
        win::SendMessageW(p.list, win::LB_RESETCONTENT, 0, 0);

        for summary in &summaries {
            // The listbox keeps its own copy under LBS_HASSTRINGS, so this
            // temporary only has to outlive the call.
            let text = win::wide(&summary.preview);
            win::SendMessageW(p.list, win::LB_ADDSTRING, 0, text.as_ptr() as LPARAM);
        }

        if summaries.is_empty() {
            // A blank list reads as a broken one, so the empty area still gets
            // one row: the hint, drawn grey by `draw_item` off the empty
            // `items` — and every action on it, commit / delete / edit, no-ops
            // for the same reason: there is no item behind the row.
            let text = win::wide(HINT_WHEN_EMPTY);
            win::SendMessageW(p.list, win::LB_ADDSTRING, 0, text.as_ptr() as LPARAM);
        }

        if !summaries.is_empty() {
            // The newest row is the last one, and it is the row the caret starts on:
            // Enter still pastes what was just copied. A multiple-selection list box
            // keeps the caret and the selection apart, which is why both are set.
            let newest = summaries.len() - 1;
            win::SendMessageW(p.list, win::LB_SETSEL, 1, newest as isize);
            win::SendMessageW(p.list, win::LB_SETCURSEL, newest, 0);
            p.anchor.store(newest as isize, Ordering::SeqCst);
        }

        win::InvalidateRect(p.list, std::ptr::null(), 1);
    }

    // The list is only as tall as the rows it has, so the new count has to reach
    // `layout`, and the view has to end up at the bottom of it.
    if let Some((width, height)) = win::client_size(p.hwnd) {
        layout(p, width, height, current_scale());
    }
    scroll_to_newest(p);
}

/// Scrolls the view to the last row, which is the newest clip.
///
/// `LB_SETCURSEL` is documented to bring the caret into view, but the newest row is
/// the whole point of the bottom-up order and is worth the second call: the count
/// that fits is measured from the control's own height, which `layout` has just set
/// to exactly the number of rows it holds.
fn scroll_to_newest(p: &Popup) {
    let count = p.items.lock().unwrap_or_else(|e| e.into_inner()).len();
    if count == 0 {
        return;
    }

    let visible = visible_rows(p);

    unsafe {
        win::SendMessageW(
            p.list,
            win::LB_SETTOPINDEX,
            (count as i32 - visible).max(0) as usize,
            0,
        );
    }
}

/// How many whole rows the list is showing right now. Measured rather than assumed:
/// the popup stretches, and both the step down to the newest row and the page keys
/// have to agree with what is actually on screen.
fn visible_rows(p: &Popup) -> i32 {
    let row_height = scaled(ROW_HEIGHT, current_scale()).max(1);
    match win::client_size(p.list) {
        Some((_, height)) => (height / row_height).max(1),
        None => 1,
    }
}

/// The wheel is the list's wherever it is spun in the popup. Over the list the
/// control sees the message itself; from the search box — where the cursor sits
/// while typing — or from the strip and the padding it has to be handed over by
/// hand. Whoever calls this swallows the message, so the control gets it once.
fn scroll_list(wparam: WPARAM, lparam: LPARAM) {
    if let Some(p) = popup() {
        unsafe {
            win::SendMessageW(p.list, win::WM_MOUSEWHEEL, wparam, lparam);
        }
    }
}

fn commit() {
    let Some(p) = popup() else {
        return;
    };

    let Some(index) = selected_index() else {
        return;
    };

    let Some(stem) = selected_stem() else {
        return;
    };

    // An inline payload is a memory read; the background detour would only add
    // latency and a flicker of hint text. A blob can be a OneDrive placeholder
    // whose read downloads over the network, and that must never happen on the
    // window thread — the whole popup would sit frozen, not even able to hide.
    let has_blob = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        items
            .get(index)
            .map(|summary| summary.has_blob)
            .unwrap_or(false)
    };

    if !has_blob {
        let Some(payload) = p.store.read_payload(&stem) else {
            log::warn(&format!("nothing pasteable for {stem}"));
            hide();
            return;
        };

        paste_back(&payload);
        return;
    }

    hydrate(HydrateAction::Paste, &[index], vec![stem]);
}

/// Hides the popup and pastes the payload into the window it took focus from.
/// The timing and the elevation checks live in `paste.rs`, the one copy of
/// that sequence this crate has.
fn paste_back(payload: &ClipPayload) {
    let Some(p) = popup() else {
        return;
    };

    let target = p.target.load(Ordering::SeqCst);
    hide();
    crate::paste::paste_back(target, payload);
}

/// Starts reading `stems` on a worker thread and marks their rows with a
/// loading hint while it runs. The result comes back as `WM_APP_HYDRATED`;
/// Esc, a click elsewhere or a newer request all bump the generation, and a
/// result that arrives stale is dropped without acting.
fn hydrate(action: HydrateAction, rows: &[usize], stems: Vec<String>) {
    let Some(p) = popup() else {
        return;
    };

    if stems.is_empty() {
        return;
    }

    let id = p.generation.fetch_add(1, Ordering::SeqCst) + 1;

    {
        let mut items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows {
            if let Some(summary) = items.get_mut(*row) {
                summary.preview = LOADING_HINT.to_string();
            }
        }
    }

    unsafe {
        win::InvalidateRect(p.list, std::ptr::null(), 1);
    }

    let store = Arc::clone(&p.store);
    let hwnd = p.hwnd;
    std::thread::spawn(move || {
        let payload = match action {
            // Both read the one clip whole; only the multi-clip copy joins.
            HydrateAction::Paste | HydrateAction::Edit => store.read_payload(&stems[0]),
            HydrateAction::Copy => joined_payload(&store, &stems),
        };

        {
            let mut slot = p.hydrated.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(Hydrated {
                id,
                action,
                stems,
                payload,
            });
        }

        win::post_message(hwnd, WM_APP_HYDRATED, 0, 0);
    });
}

/// The result a hydrating thread posted back. The popup may have moved on —
/// hidden, re-filtered, another request started — and the generation check is
/// what keeps a stale result from pasting into a window the user never aimed at.
fn finish_hydrated() {
    let Some(p) = popup() else {
        return;
    };

    let done = p.hydrated.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(done) = done else {
        return;
    };

    if p.generation.load(Ordering::SeqCst) != done.id {
        log::info("a superseded hydration finished; dropped");
        return;
    }

    match done.action {
        HydrateAction::Paste => match done.payload {
            Some(payload) => paste_back(&payload),
            None => {
                // Silently closing here would look like the popup ate the
                // Enter key. The hint comes off with the refill and the popup
                // stays where it is, still aimed at the window it captured.
                log::warn(&format!(
                    "nothing pasteable for {}; keeping the popup open",
                    done.stems.join(", ")
                ));
                fill_list();
            }
        },
        HydrateAction::Copy => match done.payload {
            Some(payload) => {
                if clipboard::write(&payload) {
                    log::info(&format!(
                        "copied {} clip(s) from the history list",
                        done.stems.len()
                    ));
                    hide();
                } else {
                    log::warn("clipboard write failed; keeping the popup open");
                    fill_list();
                }
            }
            None => {
                // Already logged inside the worker; refill so the loading hints go.
                fill_list();
            }
        },
        HydrateAction::Edit => {
            if let Some(payload) = done.payload {
                open_editor(&done.stems[0], &payload);
            } else {
                log::warn(&format!("nothing editable for {}", done.stems.join(", ")));
            }
            // The row has been showing the loading hint since the job started;
            // the real preview comes back with a refill either way.
            fill_list();
        }
    }
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

    // Pinning moves the row to the top, so follow the item rather than the
    // index. The strip cannot have changed — pinning is not capturing — so the
    // list alone is refilled.
    fill_list();

    let moved = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        items.iter().position(|summary| summary.stem == stem)
    };

    if let Some(position) = moved {
        select_row(position);
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

    let shift_down = unsafe { win::GetKeyState(win::VK_SHIFT) } < 0;

    unsafe {
        if shift_down {
            // Extend from where the run started, not from wherever the caret has
            // wandered to since.
            let anchor = p.anchor.load(Ordering::SeqCst).max(0);
            let first = anchor.min(next as isize) as usize;
            let last = anchor.max(next as isize) as usize;

            win::SendMessageW(p.list, win::LB_SETSEL, 0, -1);
            let range = first | (last << 16);
            win::SendMessageW(p.list, win::LB_SELITEMRANGE, 1, range as isize);
        } else {
            p.anchor.store(next as isize, Ordering::SeqCst);
            win::SendMessageW(p.list, win::LB_SETSEL, 0, -1);
            win::SendMessageW(p.list, win::LB_SETSEL, 1, next as isize);
        }

        win::SendMessageW(p.list, win::LB_SETCURSEL, next as usize, 0);
    }
}

/// Copies the selection and closes. Enter means "put this where I am"; this
/// means "keep this for whatever I do next", so nothing is pasted.
fn copy_selected() {
    let Some(p) = popup() else {
        return;
    };

    // (index, summary) pairs: the index is what marks the right rows with the
    // loading hint, which selected_summaries alone cannot say.
    let selected: Vec<(usize, ClipSummary)> = {
        let summaries = p.items.lock().unwrap_or_else(|e| e.into_inner());
        selected_indices()
            .into_iter()
            .filter_map(|index| summaries.get(index).map(|summary| (index, summary.clone())))
            .collect()
    };

    if selected.is_empty() {
        return;
    }

    let stems: Vec<String> = selected
        .iter()
        .map(|(_, summary)| summary.stem.clone())
        .collect();

    // Same split as `commit`: inline payloads copy right here, blob-bearing
    // ones go through the worker so a placeholder read cannot freeze the popup.
    if selected.iter().any(|(_, summary)| summary.has_blob) {
        let rows: Vec<usize> = selected.iter().map(|(index, _)| *index).collect();
        hydrate(HydrateAction::Copy, &rows, stems);
        return;
    }

    let Some(payload) = joined_payload(&p.store, &stems) else {
        return; // logged inside
    };

    if !clipboard::write(&payload) {
        log::warn("clipboard write failed; keeping the popup open");
        return;
    }

    log::info(&format!(
        "copied {} clip(s) from the history list",
        selected.len()
    ));
    hide();
}

/// Reads every stem, joins what came back and hands it over. Used on the
/// window thread for inline-only selections and inside the hydrating thread
/// for blob-bearing ones — the reads are the part that can block, which is
/// why the thread exists at all.
fn joined_payload(store: &Store, stems: &[String]) -> Option<ClipPayload> {
    let mut payloads = Vec::with_capacity(stems.len());
    for stem in stems {
        match store.read_payload(stem) {
            Some(payload) => payloads.push(payload),
            None => log::warn(&format!("nothing copyable for {stem}")),
        }
    }

    let images = payloads
        .iter()
        .filter(|payload| matches!(payload, ClipPayload::Image(_)))
        .count();

    if images > 0 && payloads.len() > 1 {
        log::warn(&format!("{images} image(s) left out of a multi-clip copy"));
    }

    let joined = clip::join_payloads(payloads);
    if joined.is_none() {
        log::warn("nothing copyable in that selection");
    }
    joined
}

/// The rows the user has selected, in list order. A multiple-selection list box
/// reports them directly; the caret is the fallback for the one-row case.
fn selected_indices() -> Vec<usize> {
    let Some(p) = popup() else {
        return Vec::new();
    };

    let count = unsafe { win::SendMessageW(p.list, win::LB_GETSELCOUNT, 0, 0) } as usize;
    if count == 0 {
        return selected_index().into_iter().collect();
    }

    let mut buffer = vec![0i32; count];
    let filled = unsafe {
        win::SendMessageW(
            p.list,
            win::LB_GETSELITEMS,
            count,
            buffer.as_mut_ptr() as LPARAM,
        )
    } as usize;

    buffer.truncate(filled.min(count));
    buffer.iter().map(|index| *index as usize).collect()
}

fn select_all() {
    let Some(p) = popup() else {
        return;
    };

    unsafe {
        win::SendMessageW(p.list, win::LB_SETSEL, 1, -1);
    }
    p.anchor.store(0, Ordering::SeqCst);
}

// -------------------------------------------------------------------- row menu

/// The rows the user has selected, in list order. Cloned, because every caller then
/// reloads the list out from under them.
fn selected_summaries() -> Vec<ClipSummary> {
    let Some(p) = popup() else {
        return Vec::new();
    };

    let indices = selected_indices();
    let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
    indices
        .into_iter()
        .filter_map(|index| items.get(index).cloned())
        .collect()
}

/// Moves the caret to one row and makes that row the selection, clamped to what is
/// left of it.
///
/// Used after a reload has moved the rows under the user — a pin, a delete — where
/// `reload`'s own "caret back on the newest row" is not where they were looking.
fn select_row(index: usize) {
    let Some(p) = popup() else {
        return;
    };

    let count = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        items.len()
    };
    if count == 0 {
        return;
    }

    let index = index.min(count - 1);
    unsafe {
        win::SendMessageW(p.list, win::LB_SETSEL, 0, -1);
        win::SendMessageW(p.list, win::LB_SETSEL, 1, index as isize);
        win::SendMessageW(p.list, win::LB_SETCURSEL, index, 0);
    }

    // The same anchor the arrows keep, so a later Shift+arrow extends from here.
    p.anchor.store(index as isize, Ordering::SeqCst);
}

/// Deletes the selected rows, after asking: this is the only thing in the popup that
/// destroys something, and there is nothing here to undo it with.
///
/// This machine's own rows go, and another instance's go once their month is over.
/// What is left is another instance's month still being written: those are hidden by
/// a tombstone — gone from this list now, and really deleted by the machine that owns
/// them the next time it looks, which is what `Store::reap_hidden` is for.
fn delete_selected_rows() {
    let Some(p) = popup() else {
        return;
    };

    let items = selected_summaries();
    if items.is_empty() {
        return;
    }

    let count = items.len();
    let stems: Vec<String> = items.into_iter().map(|item| item.stem).collect();
    let caret = selected_index().unwrap_or(0);

    let question = format!(
        "删除选中的 {count} 条记录？\n\n\
         同步目录里的记录会一起删掉，其他机器同步之后也会跟着消失，删了找不回来。\n\n\
         别的机器当月那份只能先记个「已删」的空标记（一样马上看不见），由那台机器自己清。"
    );

    if boxed("ClipPlus 删除", &question, win::MB_YESNO | win::MB_ICONWARNING) != win::IDYES {
        return;
    }

    let (deleted, marked) = p.store.delete_selected(&stems);
    log::info(&format!("{deleted} clip(s) deleted by hand, {marked} tombstoned"));

    reload();
    select_row(caret);

    // Back to the list rather than the search box, which the box above leaves the
    // focus in: the caret is on the row that took the deleted one's place, and deleting
    // that one too is the likely next move.
    unsafe {
        win::SetFocus(p.list);
    }
}

/// The row under a screen position, if there is one. The listbox answers this itself,
/// in its own client coordinates — which is also how a point that is not over the list
/// at all comes back flagged, so nothing here has to know the row height.
fn row_at(p: &Popup, x: i32, y: i32) -> Option<usize> {
    let (x, y) = win::screen_to_client(p.list, x, y);
    let point = ((x as u16 as usize) | ((y as u16 as usize) << 16)) as LPARAM;
    let answer = unsafe { win::SendMessageW(p.list, win::LB_ITEMFROMPOINT, 0, point) } as u32;

    // The high word says the point was not over an item, and the index in the low word
    // is then the nearest one rather than the one under the cursor.
    if answer >> 16 != 0 {
        return None;
    }

    let index = (answer & 0xFFFF) as usize;
    let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
    items.get(index).map(|_| index)
}

/// Opens the editor on the caret row. The store's own answer decides: text
/// only, and only where this machine may write — the same line the delete path
/// draws. The menu grays the item out on everything else; this is the keyboard
/// path's quiet version of the same rule.
fn edit_selected() {
    let Some(index) = selected_index() else {
        return;
    };

    edit_row(index);
}

/// The one row the editor opens on: the caret row for the keyboard, the
/// right-clicked row for the menu. Re-checked here even though the menu has
/// already grayed itself — the list can have changed under the click.
fn edit_row(index: usize) {
    let Some(p) = popup() else {
        return;
    };

    let stem = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        match items.get(index) {
            Some(summary) => summary.stem.clone(),
            None => return,
        }
    };

    if !p.store.can_edit(&stem) {
        return;
    }

    // A `.bin` read may be a OneDrive placeholder that downloads over the
    // network — the worker, never the window thread. Inline text is a memory
    // read and opens straight away.
    let has_blob = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        items
            .get(index)
            .map(|summary| summary.has_blob)
            .unwrap_or(false)
    };

    if has_blob {
        hydrate(HydrateAction::Edit, &[index], vec![stem]);
        return;
    }

    let Some(payload) = p.store.read_payload(&stem) else {
        log::warn(&format!("nothing editable for {stem}"));
        return;
    };

    open_editor(&stem, &payload);
}

/// Hands one clip's text to the editor, holding the popup open underneath it
/// the way a message box does. Reports whether the editor actually opened.
fn open_editor(stem: &str, payload: &ClipPayload) -> bool {
    let Some(p) = popup() else {
        return false;
    };

    let ClipPayload::Text(text) = payload else {
        log::warn("edit is text-only; refusing a non-text payload");
        return false;
    };

    // The hold goes down again the moment the editor says it did not open —
    // a hold with no editor behind it would pin the popup on screen forever.
    p.modal_open.store(true, Ordering::SeqCst);
    let opened = crate::edit_window::show(stem, text);
    if !opened {
        p.modal_open.store(false, Ordering::SeqCst);
    }
    opened
}

/// The editor is gone, saved or cancelled. Releases the popup's hold, shows
/// the list as it now is — the row's preview changed, or vanished from a
/// filter that no longer matches it — and puts the focus back where the
/// keystrokes live.
pub fn edit_finished(saved: Option<String>) {
    let Some(p) = popup() else {
        return;
    };

    // Cleared only after the popup has the activation back: between the
    // editor hiding and the focus landing, a `WM_ACTIVATE` with this still
    // set is the one thing that keeps the popup up.
    if !p.visible.load(Ordering::SeqCst) {
        p.modal_open.store(false, Ordering::SeqCst);
        return;
    }

    // A cancel changed nothing, so the list and the caret stay as they were;
    // a save refills — the row's preview changed, or vanished from a filter
    // that no longer matches it — and follows the row to where it sits now.
    if let Some(stem) = &saved {
        let position = {
            fill_list();

            let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
            items.iter().position(|summary| summary.stem == *stem)
        };
        if let Some(position) = position {
            select_row(position);
        }
    }

    win::focus_window(p.hwnd);
    unsafe {
        win::SetFocus(p.search);
    }

    p.modal_open.store(false, Ordering::SeqCst);
}

/// The row menu, at the point the right-click happened.
///
/// Right-clicking a row that is not already selected selects it first, the way every
/// list on Windows behaves: the menu then acts on what the user pointed at, or on the
/// whole selection when they pointed at part of one.
fn context_menu(x: i32, y: i32) {
    let Some(p) = popup() else {
        return;
    };

    let Some(index) = row_at(p, x, y) else {
        return;
    };

    if !selected_indices().contains(&index) {
        select_row(index);
    }

    let pinned = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        items.get(index).is_some_and(|item| item.pinned)
    };

    // The editor works on one text row this machine may write; everything
    // else — images, file lists, another instance's live month — grays the
    // item instead of hiding it, so the menu reads the same on every row.
    // Judged on the right-clicked row, which is the row the entry acts on.
    let editable = {
        let items = p.items.lock().unwrap_or_else(|e| e.into_inner());
        match items.get(index) {
            Some(summary) => p.store.can_edit(&summary.stem),
            None => false,
        }
    };

    unsafe {
        let menu = win::CreatePopupMenu();
        if menu == 0 {
            log::warn("CreatePopupMenu failed");
            return;
        }

        append(menu, win::MF_STRING, CMD_PASTE, "粘贴\tEnter");
        append(menu, win::MF_STRING, CMD_COPY, "复制\tCtrl+C");
        append(
            menu,
            if editable {
                win::MF_STRING
            } else {
                win::MF_STRING | win::MF_GRAYED
            },
            CMD_EDIT,
            "编辑\tCtrl+E",
        );
        append(
            menu,
            win::MF_STRING,
            CMD_PIN,
            if pinned {
                "取消固定\tCtrl+P"
            } else {
                "固定\tCtrl+P"
            },
        );
        append(menu, win::MF_SEPARATOR, 0, "");
        append(menu, win::MF_STRING, CMD_DELETE, "删除\tDel");
        append(menu, win::MF_SEPARATOR, 0, "");
        append(menu, win::MF_STRING, CMD_SELECT_ALL, "全选\tCtrl+A");

        // The same two calls the tray menu needs: without the window as foreground the
        // menu never notices a click away from it and stays on screen, and without the
        // trailing `WM_NULL` that first click is swallowed by the menu coming down.
        win::set_foreground(p.hwnd);

        let chosen = win::TrackPopupMenu(
            menu,
            win::TPM_RIGHTBUTTON | win::TPM_RETURNCMD,
            x,
            y,
            0,
            p.hwnd,
            std::ptr::null(),
        );

        win::DestroyMenu(menu);
        win::post_message(p.hwnd, win::WM_NULL, 0, 0);

        // `TPM_RETURNCMD`: the choice comes back here rather than as a `WM_COMMAND`,
        // so there is no id table to keep in step with a message loop.
        match chosen {
            CMD_PASTE => commit(),
            CMD_COPY => copy_selected(),
            CMD_PIN => toggle_pin(),
            CMD_DELETE => delete_selected_rows(),
            CMD_SELECT_ALL => select_all(),
            CMD_EDIT => edit_row(index),
            _ => {} // 0: dismissed without a choice
        }
    }
}

/// The row menu for a `WM_CONTEXTMENU`.
///
/// The coordinates are screen coordinates, and `(-1, -1)` is the keyboard invocation
/// — the menu key or Shift+F10 — which has no pointer position to open a menu at.
/// The caret row is what those keys act on anyway, so there is nothing to open.
///
/// The list's default handling forwards this message to the window that owns it, and
/// the popup's own arm catches it there; which of the two it reaches is not worth
/// depending on, so both call this.
fn row_menu_at(lparam: LPARAM) {
    let x = lparam as i16 as i32;
    let y = (lparam >> 16) as i16 as i32;

    if x == -1 && y == -1 {
        return;
    }

    context_menu(x, y);
}

fn append(menu: win::HMENU, flags: u32, id: i32, label: &str) {
    let text = win::wide(label);
    unsafe {
        win::AppendMenuW(menu, flags, id as usize, text.as_ptr());
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
        // erases itself with this brush — the whole box, when a filter matches
        // nothing and the list keeps the full height it was given.
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

        // The `?` is a static, and a static does not paint its own background: left
        // alone it would be a light rectangle in the corner of a dark input row.
        win::WM_CTLCOLORSTATIC => {
            if let Some(p) = popup() {
                let dc = wparam as win::HDC;
                unsafe {
                    win::SetTextColor(dc, COLOR_META);
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

        // The resize frame is real — `WS_THICKFRAME` is in the style — but the frame
        // itself is not wanted, so answering 0 makes the client area cover the whole
        // window and leaves nothing for Windows to draw around the edges.
        win::WM_NCCALCSIZE if wparam != 0 => 0,

        // The end of a move or of a stretch the system ran, which are the only two
        // things about this window worth writing down. One save covers both, so a
        // drag that only moved does not also rewrite the size.
        win::WM_EXITSIZEMOVE => {
            remember_layout();
            0
        }

        win::WM_SIZE => {
            // The client area changed, so the controls inside it move with it. This
            // is the stretch path; `show` lays out the first open.
            if let Some(p) = popup() {
                let width = (lparam & 0xFFFF) as u16 as i32;
                let height = ((lparam >> 16) & 0xFFFF) as u16 as i32;
                layout(p, width, height, current_scale());
            }
            0
        }

        // Spun over the strip, the padding or the `?`: the search box and the list
        // are child controls, so this window is where Windows walks the message up
        // to, and the rows are what the user is aiming at either way.
        win::WM_MOUSEWHEEL => {
            scroll_list(wparam, lparam);
            0
        }

        // DefWindowProc runs first so the maximum side of the struct is filled in the
        // way Windows fills it; the minimum track size is the one field this window
        // has an opinion about, and without it the window can be dragged down to a
        // sliver of padding with no room for a single row in it.
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

        // The edges are invisible, so this is what makes them work: the codes come
        // back from here and Windows runs the rest of it, cursor and sizing loop
        // included.
        win::WM_NCHITTEST => match resize_edge(hwnd, lparam) {
            Some(code) => code,
            // The client area — the padding and the tab strip — where a drag starts
            // and where the controls hit test themselves.
            None => win::HTCLIENT as LRESULT,
        },

        win::WM_PAINT => {
            paint(hwnd);
            0
        }

        // A right-click on a row opens the row menu there. The list forwards this
        // message up by default; a point anywhere else in the popup has no row under
        // it and the menu stays shut.
        win::WM_CONTEXTMENU => {
            row_menu_at(lparam);
            0
        }

        win::WM_LBUTTONDOWN => {
            // A filter button opens its menu, a tab click switches tabs; anywhere
            // else on the popup's own background starts a window drag. The search
            // box and the list are child controls, so a click that lands on them
            // never gets here, and the outermost pixels are the resize edge rather
            // than this — what is left to grab is the padding inside that, the tab
            // strip, and the space over a list too short to fill it.
            if !filter_click(lparam) && !tab_click(lparam) {
                win::begin_drag_move(hwnd);
            }
            0
        }

        // The window itself can hold the keyboard — a click on its own background
        // does that, and it is also how a drag starts — and then no subclass is in
        // play to translate the keys. The same combinations are handled here too, so
        // Esc and Enter do not depend on which half of the widget is focused.
        win::WM_KEYDOWN => {
            if handle_key(wparam as i32, false) {
                0
            } else {
                win::def_window_proc(hwnd, message, wparam, lparam)
            }
        }

        win::WM_COMMAND => {
            let id = (wparam & 0xFFFF) as usize;
            let notification = ((wparam >> 16) & 0xFFFF) as u32;

            if id == ID_HELP && notification == win::STN_CLICKED {
                show_help();
            } else if notification == win::EN_CHANGE {
                fill_list();
            } else if notification == win::LBN_DBLCLK {
                commit();
            } else if notification == win::LBN_SELCHANGE {
                // Keep the extension anchor in step with the mouse, so a later
                // Shift+arrow extends from where the user just clicked.
                if let (Some(index), Some(p)) = (selected_index(), popup()) {
                    p.anchor.store(index as isize, Ordering::SeqCst);
                }
            }
            0
        }

        // A hydrating thread finished reading its payload. Everything the
        // result does — clipboard, hide, paste-back — belongs to the window
        // thread, which is why it came back as a message instead of being
        // done where the read landed.
        WM_APP_HYDRATED => {
            finish_hydrated();
            0
        }

        WM_APP_THUMB => {
            finish_thumbs();
            0
        }

        WM_APP_TOGGLE => {
            toggle();
            0
        }

        // The hotkey release wait: hide once the combination is physically
        // back up, or when the deadline runs out — whichever happens first.
        // `hide` clears the deadline and the timer, so a hide that arrived
        // through another path (Esc, a click elsewhere) ends the wait too.
        win::WM_TIMER if wparam as usize == HOTKEY_TIMER_ID => {
            let Some(p) = popup() else {
                return 0;
            };

            let deadline = p.hotkey_hide_deadline.load(Ordering::SeqCst);
            if deadline == 0 {
                unsafe {
                    win::KillTimer(hwnd, HOTKEY_TIMER_ID);
                }
                return 0;
            }

            let hotkey =
                crate::current_settings().and_then(|s| crate::settings::parse_hotkey(&s.hotkey));
            let held = hotkey.is_some_and(hotkey_keys_held);

            if !held || crate::settings::now_ms() >= deadline as i64 {
                p.hotkey_hide_deadline.store(0, Ordering::SeqCst);
                unsafe {
                    win::KillTimer(hwnd, HOTKEY_TIMER_ID);
                }
                hide();
            }
            0
        }

        win::WM_ACTIVATE => {
            // WA_INACTIVE == 0: the user clicked somewhere else. The boxes this popup
            // opens itself are the one thing allowed to take the activation — hiding the
            // popup under one would take the box with it — and the flag is cleared when
            // the box closes. The first instants after a show get a grace window of
            // their own: the foreground can jitter while it changes hands.
            let inactive = (wparam & 0xFFFF) == 0;
            let modal_open = popup().is_some_and(|p| p.modal_open.load(Ordering::SeqCst));
            let gracing = popup().is_some_and(|p| {
                let shown = p.shown_at.load(Ordering::SeqCst) as i64;
                shown != 0 && crate::settings::now_ms() - shown < SHOW_GRACE_MS
            });
            if inactive && !modal_open && !gracing {
                hide();
            }
            0
        }

        _ => win::def_window_proc(hwnd, message, wparam, lparam),
    }
}

/// The search box swallows the keys we care about, so they are intercepted here
/// and everything else is handed back to the control.
/// Keys that mean the same thing whichever half of the widget has focus: the
/// search box and the list are two halves of one thing, and the user should not
/// have to know which one the focus is in. Delete is the exception — `typing` says
/// the keystroke is going into the query, where it deletes a character rather than
/// a row.
///
/// Returns true when the key was consumed.
fn handle_key(key: i32, typing: bool) -> bool {
    // VK_CONTROL is declared as u16 for SendInput; GetKeyState wants i32.
    let control_down = unsafe { win::GetKeyState(win::VK_CONTROL as i32) } < 0;

    // A page is what the list actually shows, not a constant: the popup stretches,
    // and eight rows is only the size it opens at. `page_rows` keeps one row of
    // overlap, so a page never lands without the row it came from still on screen.
    let page = popup().map_or(8, |p| page_rows(visible_rows(p)));

    match key {
        win::VK_ESCAPE => hide(),
        win::VK_RETURN => commit(),
        win::VK_UP => move_selection(-1),
        win::VK_DOWN => move_selection(1),
        win::VK_PRIOR => move_selection(-page),
        win::VK_NEXT => move_selection(page),
        win::VK_DELETE if !typing => delete_selected_rows(),
        win::VK_P if control_down => toggle_pin(),
        win::VK_C if control_down => copy_selected(),
        win::VK_E if control_down => edit_selected(),
        win::VK_TAB if control_down => {
            // Shift walks the strip backwards.
            let shift_down = unsafe { win::GetKeyState(win::VK_SHIFT) } < 0;
            cycle_tab(if shift_down { -1 } else { 1 });
        }
        _ => return false,
    }

    true
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
    // The cursor sits here while typing and this is the control with the focus, so
    // this is the common half of the wheel: the box has nothing of its own to
    // scroll, and the list is right above it.
    if message == win::WM_MOUSEWHEEL {
        scroll_list(wparam, lparam);
        return 0;
    }

    if message == win::WM_KEYDOWN && handle_key(wparam as i32, true) {
        return 0;
    }

    unsafe { win::DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// The list gets the same keys: clicking a row moves the focus into it, and
/// without this Enter would stop working until the search box was clicked again.
///
/// Ctrl+A is the list's alone — in a text box it means "select the text", which
/// the EDIT already does for itself.
extern "system" fn list_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    // Typing belongs in the search box. Without this, a click on a row leaves
    // the list focused and the next keystroke does list-box type-ahead instead
    // of filtering. The WM_CHAR is forwarded rather than the key, because
    // TranslateMessage has already been and gone for this one.
    if message == win::WM_CHAR {
        let code = wparam as u32;

        if code >= 0x20 || code == 0x08 {
            if let Some(p) = popup() {
                unsafe {
                    win::SetFocus(p.search);
                    win::SendMessageW(p.search, win::WM_CHAR, wparam, lparam);
                }
            }
            return 0;
        }
    }

    // The right-click menu of a row. The list hands a `WM_CONTEXTMENU` up to the
    // window that owns it by default and the popup's own arm would catch it there;
    // catching it here first is what keeps a right-click on a row from also reaching
    // the popup's background, where it would start a window drag.
    if message == win::WM_CONTEXTMENU {
        row_menu_at(lparam);
        return 0;
    }

    if message == win::WM_KEYDOWN {
        let key = wparam as i32;
        let control_down = unsafe { win::GetKeyState(win::VK_CONTROL as i32) } < 0;

        if key == win::VK_A && control_down {
            select_all();
            return 0;
        }

        if handle_key(key, false) {
            return 0;
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
        items.get(index).cloned()
    };
    // The one row an empty list still shows: the "nothing matched" hint. Every
    // action on it no-ops — there is no item behind the row — so drawing is
    // all it is for.
    let Some(summary) = summary else {
        let mut hint = item.rc_item;
        hint.left += scaled(10, current_scale());
        let previous =
            unsafe { win::SelectObject(item.hdc, p.font_meta.load(Ordering::SeqCst)) };
        unsafe {
            win::SetTextColor(item.hdc, COLOR_META);
            let text = win::wide(HINT_WHEN_EMPTY);
            win::DrawTextW(item.hdc, text.as_ptr(), -1, &mut hint, text_flags());
            win::SelectObject(item.hdc, previous);
        }
        return;
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

        // Image rows open with the thumbnail, a square slightly smaller than
        // the row; every other row keeps the plain text layout. A bitmap that
        // has not been decoded yet gets a placeholder block in the input
        // colour and a one-time ask to the worker.
        if summary.kind == ClipKind::Image {
            let edge = scaled(ROW_HEIGHT - THUMB_INSET * 2, scale);
            let thumb = win::RECT {
                left: rect.left + scaled(THUMB_INSET, scale),
                top: rect.top + (rect.bottom - rect.top - edge) / 2,
                right: rect.left + scaled(THUMB_INSET, scale) + edge,
                bottom: rect.top + (rect.bottom - rect.top - edge) / 2 + edge,
            };
            draw_thumbnail(p, dc, thumb, &summary.stem);
            left = thumb.right + scaled(THUMB_GAP, scale);
        }

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

/// Draws the row's thumbnail into `square`: the decoded bitmap fitted and
/// centred, or a placeholder block plus a one-time ask to the worker. All of
/// it is window-thread work; the only thing that leaves the thread is the
/// channel send, which cannot block.
fn draw_thumbnail(p: &Popup, dc: win::HDC, square: win::RECT, stem: &str) {
    let entry = p
        .thumbs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(stem)
        .copied();

    if let Some(entry) = entry {
        // Fit, not fill: a screenshot is rarely square, and stretching it
        // into one would lie about what was copied.
        let square_w = (square.right - square.left).max(1) as f64;
        let square_h = (square.bottom - square.top).max(1) as f64;
        let fit = (square_w / entry.width as f64).min(square_h / entry.height as f64);
        let width = (entry.width as f64 * fit).round() as i32;
        let height = (entry.height as f64 * fit).round() as i32;
        let left = square.left + ((square.right - square.left) - width) / 2;
        let top = square.top + ((square.bottom - square.top) - height) / 2;

        unsafe {
            let src = win::CreateCompatibleDC(dc);
            let previous = win::SelectObject(src, entry.bmp as win::HGDIOBJ);
            win::SetStretchBltMode(dc, win::HALFTONE);
            win::StretchBlt(
                dc,
                left,
                top,
                width,
                height,
                src,
                0,
                0,
                entry.width,
                entry.height,
                win::SRCCOPY,
            );
            win::SelectObject(src, previous);
            win::DeleteDC(src);
        }
        return;
    }

    unsafe {
        win::FillRect(dc, &square, p.brush_input);
    }

    // Ask the worker once per stem; until the answer lands, every repaint of
    // the row redraws the placeholder without repeating the ask.
    if THUMB_TX.get().is_none() {
        return;
    }
    let fresh = p
        .thumb_pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(stem.to_string());
    if fresh {
        if let Some(sender) = THUMB_TX.get() {
            let _ = sender.send(stem.to_string());
        }
    }
}

/// Takes the worker's finished decodes, turns each into a GDI bitmap and
/// evicts the LRU's oldest member when the cache runs over. The list repaints
/// once afterwards, when the whole batch is in.
fn finish_thumbs() {
    let Some(p) = popup() else {
        return;
    };

    let results: Vec<ThumbResult> = {
        let mut done = p.thumb_done.lock().unwrap_or_else(|e| e.into_inner());
        done.drain(..).collect()
    };
    if results.is_empty() {
        return;
    }

    {
        let mut thumbs = p.thumbs.lock().unwrap_or_else(|e| e.into_inner());
        let mut order = p.thumb_order.lock().unwrap_or_else(|e| e.into_inner());
        let mut pending = p.thumb_pending.lock().unwrap_or_else(|e| e.into_inner());

        for result in results {
            let bmp = dib_from_rgba(result.width, result.height, &result.rgba);
            if bmp == 0 {
                log::warn(&format!("thumbnail bitmap for {} failed", result.stem));
                continue;
            }

            // A re-decode of a stem already in the cache replaces it in place;
            // the old order entry goes first, so the LRU never sees it twice.
            order.retain(|stem| stem != &result.stem);
            if let Some(old) = thumbs.insert(
                result.stem.clone(),
                ThumbEntry {
                    bmp,
                    width: result.width as i32,
                    height: result.height as i32,
                },
            ) {
                unsafe {
                    win::DeleteObject(old.bmp as win::HGDIOBJ);
                }
            }
            // The ask is answered: clear it so an evicted or re-decoded stem
            // may ask again later. A failed bitmap keeps its pending entry —
            // every future ask would fail the same way.
            pending.remove(&result.stem);
            order.push_back(result.stem);

            while order.len() > THUMB_CACHE_CAP {
                let Some(oldest) = order.pop_front() else {
                    break;
                };
                if let Some(old) = thumbs.remove(&oldest) {
                    unsafe {
                        win::DeleteObject(old.bmp as win::HGDIOBJ);
                    }
                }
            }
        }
    }

    unsafe {
        win::InvalidateRect(p.list, std::ptr::null(), 1);
    }
}

/// A top-down 32bpp DIB holding `rgba`. The row blits from it with
/// `StretchBlt` afterwards; made here because `CreateDIBSection` wants a DC
/// and the pixels must land on the thread that will draw them.
fn dib_from_rgba(width: usize, height: usize, rgba: &[u8]) -> win::HBITMAP {
    let info = win::BITMAPINFO {
        bmi_header: win::BITMAPINFOHEADER {
            bi_size: std::mem::size_of::<win::BITMAPINFOHEADER>() as u32,
            bi_width: width as i32,
            // Negative: top-down rows, the order a PNG decodes in.
            bi_height: -(height as i32),
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: win::BI_RGB,
            ..Default::default()
        },
        bmi_colors: 0,
    };

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let dc = unsafe { win::CreateCompatibleDC(0) };
    let bmp = unsafe { win::CreateDIBSection(dc, &info, win::DIB_RGB_COLORS, &mut bits, 0, 0) };
    unsafe {
        win::DeleteDC(dc);
    }

    if bmp == 0 || bits.is_null() {
        return 0;
    }

    let expected = width * height * 4;
    if rgba.len() < expected {
        unsafe {
            win::DeleteObject(bmp as win::HGDIOBJ);
        }
        return 0;
    }

    unsafe {
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), bits as *mut u8, expected);
    }
    bmp
}

/// How wide each tab is drawn, left to right: its label measured in the strip's
/// own font plus a third of a tab of breathing room on each side. Measured per
/// repaint rather than cached — there are two or three tabs, and the paint and
/// the hit test must agree on the rects to the pixel. Takes the caller's own
/// strip snapshot, so the labels and their widths cannot disagree.
fn tab_widths(p: &Popup, tabs: &[MachineTab], scale: f64) -> Vec<i32> {
    let font = p.font_main.load(Ordering::SeqCst);
    let pad = scaled(TAB_WIDTH, scale) / 3;

    unsafe {
        let dc = win::GetDC(p.hwnd);
        let previous = win::SelectObject(dc, font);
        let mut widths = Vec::with_capacity(tabs.len());

        for tab in &tabs {
            let text = win::wide(&tab.label);
            let mut size = win::SIZE::default();
            if win::GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut size) != 0
                && size.cx > 0
            {
                widths.push(size.cx + pad * 2);
            } else {
                widths.push(scaled(TAB_WIDTH, scale));
            }
        }

        win::SelectObject(dc, previous);
        win::ReleaseDC(p.hwnd, dc);
        widths
    }
}

/// Paints the instance strip. Drawn here rather than from a real tab control or
/// from buttons: a tab control cannot be themed dark, and buttons would take
/// focus away from the search box on every click.
fn paint(hwnd: HWND) {
    let Some(p) = popup() else {
        return;
    };

    let mut ps = unsafe { std::mem::zeroed::<win::PAINTSTRUCT>() };
    let dc = unsafe { win::BeginPaint(hwnd, &mut ps) };

    if dc == 0 {
        unsafe {
            win::EndPaint(hwnd, &ps);
        }
        return;
    }

    let scale = current_scale();
    let top = bottom_band_top(hwnd, TAB_FROM_BOTTOM, scale);
    let height = scaled(TAB_HEIGHT, scale);

    let selected = p.tab.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let tabs = p.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let widths = tab_widths(p, &tabs, scale);
    let font = p.font_main.load(Ordering::SeqCst);

    unsafe {
        win::SetBkMode(dc, win::TRANSPARENT_BK);
        let previous = win::SelectObject(dc, font);

        let mut left = scaled(PAD, scale);
        for (tab, width) in tabs.iter().zip(&widths) {
            let active = tab.id == selected;
            let rect = win::RECT {
                left,
                top,
                right: left + width,
                bottom: top + height,
            };

            let background = if active { p.brush_selected } else { p.brush_input };
            win::FillRect(dc, &rect, background);
            win::SetTextColor(dc, if active { COLOR_TEXT } else { COLOR_META });

            let mut text_rect = rect;
            let text = win::wide(&tab.label);
            win::DrawTextW(dc, text.as_ptr(), -1, &mut text_rect, tab_text_flags());

            left = rect.right + scaled(TAB_GAP, scale);
        }

        // The four filter buttons, in the tabs' two faces: input-coloured while
        // bare, selected-coloured while holding a choice, so a popup finding more
        // or fewer rows than expected explains itself. Not controls — a click on
        // one is hit tested in the window proc against these same rects, and
        // nothing here can take the keyboard from the search box. Drawn under the
        // same selected font as the tabs, which is why this sits before the
        // restore below.
        let chips = p.chips.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (rect, chip) in filter_rects(hwnd) {
            let active = chip.active(&chips);

            let background = if active { p.brush_selected } else { p.brush_input };
            win::FillRect(dc, &rect, background);
            win::SetTextColor(dc, if active { COLOR_TEXT } else { COLOR_META });

            let mut text_rect = rect;
            let text = win::wide(&chip.label(&chips));
            win::DrawTextW(dc, text.as_ptr(), -1, &mut text_rect, tab_text_flags());
        }

        win::SelectObject(dc, previous);

        // Two bars in the top-right corner: nothing else says the window can be
        // stretched, because there is no frame to say it. Sized to the padding they
        // sit in, so they never cover a row — and they sit up here because the search
        // box has the bottom-right corner now, where a child control would hide them.
        let mut client = win::RECT::default();
        win::GetClientRect(hwnd, &mut client);
        let arm = scaled(GRIP_ARM, scale);
        let thickness = scaled(2, scale).max(1);
        let right = client.right - scaled(2, scale);
        let grip_top = scaled(2, scale);

        win::FillRect(
            dc,
            &win::RECT {
                left: right - arm,
                top: grip_top,
                right,
                bottom: grip_top + thickness,
            },
            p.brush_meta,
        );
        win::FillRect(
            dc,
            &win::RECT {
                left: right - thickness,
                top: grip_top,
                right,
                bottom: grip_top + arm,
            },
            p.brush_meta,
        );
        win::EndPaint(hwnd, &ps);
    }
}

fn tab_text_flags() -> u32 {
    win::DT_CENTER | win::DT_SINGLELINE | win::DT_VCENTER | win::DT_END_ELLIPSIS | win::DT_NOPREFIX
}

/// The strip is not a control, so clicks are hit tested by hand against the same
/// rectangles `paint` draws.
///
/// Returns whether the click landed on a tab. A `false` is how the caller knows
/// the mouse came down on the popup's own background — the padding, or a gap in
/// the strip — which is the only place a drag can start.
fn tab_click(lparam: LPARAM) -> bool {
    let Some(p) = popup() else {
        return false;
    };

    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;

    let scale = current_scale();
    let top = bottom_band_top(p.hwnd, TAB_FROM_BOTTOM, scale);
    let height = scaled(TAB_HEIGHT, scale);

    if y < top || y >= top + height {
        return false;
    }

    // The same rects `paint` draws, walked linearly: tab widths are measured,
    // so there is no fixed step to divide by.
    let tabs = p.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let widths = tab_widths(p, &tabs, scale);
    let mut left = scaled(PAD, scale);

    for (tab, width) in tabs.iter().zip(&widths) {
        let right = left + width;
        if x >= left && x < right {
            set_tab(tab.id.clone());
            return true;
        }
        left = right + scaled(TAB_GAP, scale);
    }

    false
}

/// Switches the list to one instance. `None` is the tab that shows everything.
fn set_tab(id: Option<String>) {
    let Some(p) = popup() else {
        return;
    };

    *p.tab.lock().unwrap_or_else(|e| e.into_inner()) = id;
    fill_list();
    repaint_tab_strip(p.hwnd);
}

/// Invalidates just the strip's band: which tab is lit changed, and nothing
/// outside the band did. Same reasoning as `repaint_filter_button` — a
/// full-window invalidate erases the children along with everything else, and
/// that is the blink. No erase: the tabs fill their own rects in `paint`, and
/// the gaps between them do not change.
fn repaint_tab_strip(hwnd: HWND) {
    let scale = current_scale();
    let rect = win::RECT {
        left: 0,
        top: bottom_band_top(hwnd, TAB_FROM_BOTTOM, scale),
        right: win::client_size(hwnd).map(|(width, _)| width).unwrap_or(0),
        bottom: bottom_band_top(hwnd, SEARCH_FROM_BOTTOM, scale),
    };

    unsafe {
        win::InvalidateRect(hwnd, &rect, 0);
    }
}

/// The four buttons' rects on the input row, in client coordinates. `paint`
/// draws from these and `filter_click` hit-tests against them, so the two cannot
/// disagree about where a button is — the same trick the tab strip uses.
fn filter_rects(hwnd: HWND) -> Vec<(win::RECT, Chip)> {
    let scale = current_scale();
    let left = scaled(PAD, scale);
    let top = bottom_band_top(hwnd, SEARCH_FROM_BOTTOM, scale);
    let height = scaled(SEARCH_HEIGHT, scale);
    let gap = scaled(FILTER_GAP, scale);

    ALL_CHIPS
        .iter()
        .scan(left, |left, &chip| {
            let width = scaled(chip.width(), scale);
            let rect = win::RECT {
                left: *left,
                top,
                right: *left + width,
                bottom: top + height,
            };
            *left += width + gap;
            Some((rect, chip))
        })
        .collect()
}

/// A click on one of the four buttons: the boxes flip and refill, the menus
/// open. `false` is the tab strip's and the drag's cue, the same contract
/// `tab_click` works under.
fn filter_click(lparam: LPARAM) -> bool {
    let Some(p) = popup() else {
        return false;
    };

    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;

    for (rect, chip) in filter_rects(p.hwnd) {
        if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
            match chip {
                Chip::AppBox | Chip::TitleBox => toggle_filter_box(chip),
                Chip::Kind | Chip::Time => open_filter_menu(p, chip, rect),
            }
            return true;
        }
    }

    false
}

/// Invalidates just one filter button's rect — the face that changed. A
/// full-window invalidate erases the whole client first, children included
/// (this window has no `WS_CLIPCHILDREN`), and that one frame of blanked list
/// and search box reads as the popup blinking on every click. The list refills
/// itself through `fill_list`; nothing outside the button needs the parent.
/// No erase either: `paint` fills the button's rect before drawing on it.
fn repaint_filter_button(hwnd: HWND, chip: Chip) {
    let rect = filter_rects(hwnd)
        .into_iter()
        .find(|(_, found)| *found == chip)
        .map(|(rect, _)| rect);

    if let Some(rect) = rect {
        unsafe {
            win::InvalidateRect(hwnd, &rect, 0);
        }
    }
}

/// One of the two checkbox buttons: a click flips the scope it stands for and
/// the list refills — no menu, a checkbox is its own whole UI.
fn toggle_filter_box(chip: Chip) {
    let Some(p) = popup() else {
        return;
    };

    {
        let mut chips = p.chips.lock().unwrap_or_else(|e| e.into_inner());
        match chip {
            Chip::AppBox => chips.in_app = !chips.in_app,
            Chip::TitleBox => chips.in_title = !chips.in_title,
            // The menus never come here; `filter_click` sends them to their menu.
            Chip::Kind | Chip::Time => {}
        }
    }

    repaint_filter_button(p.hwnd, chip);
    fill_list();
}

/// `MF_CHECKED` when the condition holds, so an open menu shows the choice the
/// button is already holding. `MF_STRING` is zero, so `MF_CHECKED` alone is the
/// two flags OR-ed.
fn menu_flags(checked_on: bool) -> u32 {
    if checked_on {
        win::MF_CHECKED
    } else {
        win::MF_STRING
    }
}

/// Opens the menu one of the two menu buttons offers, dropped from the button's
/// bottom edge like any dropdown. Kind is a fixed list; time is the fixed presets,
/// turned into a bound only when the list refills.
///
/// The same foreground dance the row menu does: without the window as foreground
/// the menu never notices a click away from it, and without the trailing `WM_NULL`
/// that first click is swallowed by the menu coming down.
fn open_filter_menu(p: &Popup, chip: Chip, rect: win::RECT) {
    // The checkboxes have no menu — `filter_click` sends them to
    // `toggle_filter_box`. Guarded here so a box can never reach the menu code
    // below, even if a caller starts passing one by mistake.
    match chip {
        Chip::Kind | Chip::Time => {}
        Chip::AppBox | Chip::TitleBox => return,
    }

    let chips = p.chips.lock().unwrap_or_else(|e| e.into_inner()).clone();

    unsafe {
        let menu = win::CreatePopupMenu();
        if menu == 0 {
            log::warn("CreatePopupMenu failed");
            return;
        }

        match chip {
            Chip::Kind => {
                append(menu, menu_flags(chips.kind.is_none()), MENU_KIND, "全部");
                for (offset, kind) in [ClipKind::Text, ClipKind::Image, ClipKind::Files]
                    .into_iter()
                    .enumerate()
                {
                    append(
                        menu,
                        menu_flags(chips.kind == Some(kind)),
                        MENU_KIND + 1 + offset as i32,
                        kind.label(),
                    );
                }
            }
            Chip::Time => {
                append(menu, menu_flags(chips.time == TimeChip::All), MENU_TIME, "全部");
                append(
                    menu,
                    menu_flags(chips.time == TimeChip::Today),
                    MENU_TIME + 1,
                    "今天",
                );
                append(
                    menu,
                    menu_flags(chips.time == TimeChip::Seven),
                    MENU_TIME + 2,
                    "近7天",
                );
                append(
                    menu,
                    menu_flags(chips.time == TimeChip::Thirty),
                    MENU_TIME + 3,
                    "近30天",
                );
            }
            // Unreachable — guarded at the top of this function — but the match
            // has to be exhaustive over `Chip`.
            Chip::AppBox | Chip::TitleBox => {}
        }

        win::set_foreground(p.hwnd);

        let (x, y) = win::client_to_screen(p.hwnd, rect.left, rect.bottom);
        let chosen = win::TrackPopupMenu(
            menu,
            win::TPM_RIGHTBUTTON | win::TPM_RETURNCMD,
            x,
            y,
            0,
            p.hwnd,
            std::ptr::null(),
        );

        win::DestroyMenu(menu);
        win::post_message(p.hwnd, win::WM_NULL, 0, 0);

        if chosen != 0 {
            apply_filter_choice(chosen);
        }
    }
}

/// Turns a menu id back into the filter it means, then repaints and refills. The
/// id ranges are the ones `open_filter_menu` appended with.
fn apply_filter_choice(id: i32) {
    let Some(p) = popup() else {
        return;
    };

    {
        let mut chips = p.chips.lock().unwrap_or_else(|e| e.into_inner());

        if id >= MENU_TIME {
            chips.time = match id - MENU_TIME {
                1 => TimeChip::Today,
                2 => TimeChip::Seven,
                3 => TimeChip::Thirty,
                _ => TimeChip::All,
            };
        } else if id >= MENU_KIND {
            chips.kind = match id - MENU_KIND {
                1 => Some(ClipKind::Text),
                2 => Some(ClipKind::Image),
                3 => Some(ClipKind::Files),
                _ => None,
            };
        }
    }

    // Only the pressed button's face changed — repaint that one button, not the
    // window; see `repaint_filter_button` for why the whole-window version blinks.
    let chip = if id >= MENU_TIME { Chip::Time } else { Chip::Kind };
    repaint_filter_button(p.hwnd, chip);
    fill_list();
}

fn cycle_tab(delta: i32) {
    let Some(p) = popup() else {
        return;
    };

    let tabs = p.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if tabs.len() < 2 {
        return;
    }

    let current = p.tab.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let position = tabs.iter().position(|tab| tab.id == current).unwrap_or(0) as i32;
    let next = (position + delta).rem_euclid(tabs.len() as i32) as usize;

    set_tab(tabs[next].id.clone());
}

fn text_flags() -> u32 {
    win::DT_LEFT | win::DT_SINGLELINE | win::DT_VCENTER | win::DT_END_ELLIPSIS | win::DT_NOPREFIX
}

/// Records where the popup is now and how big it is, which is where the user just
/// left it. The size is written in logical pixels, the units every constant above
/// is written in, so it comes back the right size on a monitor with another scale.
fn remember_layout() {
    let Some(p) = popup() else {
        return;
    };

    let (Some((x, y)), Some((width, height))) =
        (win::window_position(p.hwnd), win::client_size(p.hwnd))
    else {
        return;
    };

    // The window's own DPI rather than `current_scale()`: that one is stored as a
    // truncated percentage, and a size divided by a slightly wrong scale would
    // drift a little further off on every save.
    let scale = win::dpi_scale_of(p.hwnd);
    let logical = (
        (width as f64 / scale).round() as i32,
        (height as f64 / scale).round() as i32,
    );

    crate::remember_popup_layout((x, y), logical);
}

/// Where the window goes: where it was left last time, or the middle of the work
/// area when there is nothing to remember.
///
/// Pure, and tested, because one of its two jobs only shows up after a monitor goes
/// away: a remembered corner can be off-screen by now — that monitor is unplugged,
/// the resolution changed — and a popup that opens somewhere unreachable is worse
/// than one that moved.
pub(crate) fn placed(
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

/// The resize edge under the cursor, as the hit-test code Windows expects back.
/// `None` is the client area — the padding, the tab strip, the space over a short
/// list — where nothing about resizing happens.
///
/// The coordinates arrive packed as two signed 16-bit values, in screen space: the
/// hit test is answered before anything here knows where the window is.
fn resize_edge(hwnd: HWND, lparam: LPARAM) -> Option<LRESULT> {
    let x = lparam as i16 as i32;
    let y = (lparam >> 16) as i16 as i32;
    let (x, y) = win::screen_to_client(hwnd, x, y);
    let (width, height) = win::client_size(hwnd)?;

    edge_hit(x, y, width, height, scaled(GRIP, current_scale())).map(|code| code as LRESULT)
}

/// Which side a point is on, as a hit-test code, or `None` for the inside.
///
/// Pure, and tested, because this is the whole of the resize: a code that is wrong
/// by one is an edge that does nothing when it is dragged, and the two diagonal
/// codes only exist for the corners, where two of the four sides are hit at once.
fn edge_hit(x: i32, y: i32, width: i32, height: i32, grip: i32) -> Option<usize> {
    let left = x < grip;
    let right = x >= width - grip;
    let top = y < grip;
    let bottom = y >= height - grip;

    match (left, right, top, bottom) {
        (true, _, true, _) => Some(win::HTTOPLEFT),
        (_, true, true, _) => Some(win::HTTOPRIGHT),
        (true, _, _, true) => Some(win::HTBOTTOMLEFT),
        (_, true, _, true) => Some(win::HTBOTTOMRIGHT),
        (true, _, _, _) => Some(win::HTLEFT),
        (_, true, _, _) => Some(win::HTRIGHT),
        (_, _, true, _) => Some(win::HTTOP),
        (_, _, _, true) => Some(win::HTBOTTOM),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The resize border is invisible, so this is the only thing that makes the
    /// window stretchable at all: a code that is off by one is an edge that does
    /// nothing when it is dragged, or a resize where a click was meant.
    #[test]
    fn the_border_answers_with_the_side_it_is_on() {
        let (width, height, grip) = (600, 440, 5);

        // Corners: two sides at once, which is what the four comparisons have to
        // combine into the diagonal codes.
        assert_eq!(edge_hit(0, 0, width, height, grip), Some(win::HTTOPLEFT));
        assert_eq!(edge_hit(599, 0, width, height, grip), Some(win::HTTOPRIGHT));
        assert_eq!(edge_hit(0, 439, width, height, grip), Some(win::HTBOTTOMLEFT));
        assert_eq!(edge_hit(599, 439, width, height, grip), Some(win::HTBOTTOMRIGHT));

        // A band along each side, and the last pixel that is still client area.
        assert_eq!(edge_hit(0, 200, width, height, grip), Some(win::HTLEFT));
        assert_eq!(edge_hit(599, 200, width, height, grip), Some(win::HTRIGHT));
        assert_eq!(edge_hit(200, 0, width, height, grip), Some(win::HTTOP));
        assert_eq!(edge_hit(200, 439, width, height, grip), Some(win::HTBOTTOM));

        // Inside is the client area: no resizing there, that is the popup's own
        // background, the tab strip and the controls.
        assert_eq!(edge_hit(5, 5, width, height, grip), None);
        assert_eq!(edge_hit(300, 220, width, height, grip), None);
        assert_eq!(edge_hit(594, 434, width, height, grip), None);
    }

    /// The list is measured from its bottom edge, and where it ends decides where
    /// the newest clip lands: right above the search box, or at the top of a hole.
    #[test]
    fn the_list_is_as_tall_as_the_rows_it_holds() {
        // Three rows of space (the minimum window), five clips: three rows shown.
        assert_eq!(rows_for(5, 138, 46), 3);
        // Two clips in the same space: the list shrinks to them, so the newest one
        // is still the row next to the search box.
        assert_eq!(rows_for(2, 138, 46), 2);
        // Nothing matched: the area stays, an empty list is not a broken one.
        assert_eq!(rows_for(0, 138, 46), 3);
        // Squeezed to nothing: one row is still placed rather than zero.
        assert_eq!(rows_for(4, 0, 46), 1);
    }

    /// The page keys and the step to the newest row both read the list's real
    /// height, and the page keeps one row of overlap so a jump never lands blind.
    #[test]
    fn a_page_is_one_row_short_of_what_fits() {
        assert_eq!(page_rows(8), 7);
        assert_eq!(page_rows(3), 2);
        // The minimum window still has somewhere to go.
        assert_eq!(page_rows(1), 1);
    }

    /// The terminal list and the UIPI rule moved to `paste.rs` together with
    /// the code they pin down; their tests went with them.

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
