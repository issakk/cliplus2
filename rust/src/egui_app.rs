//! The egui popup.
//!
//! Enabled with `CLIPPLUS_UI=egui`; the legacy Win32 popup stays the default
//! until Phase 4. Phase 1 proved the three lifelines (IME, instant open,
//! paste-back); Phase 2 brings the feature set to parity with the legacy
//! popup — filter chips, machine tabs, the row context menu, image
//! thumbnails, background hydration of `.bin` payloads, multi-selection and
//! remembered layout.
//!
//! Threading: the platform thread sends `PlatformEvent`s over a channel; a
//! worker thread decodes thumbnails; hydrate threads read `.bin` payloads.
//! All results come back over channels drained in `logic`, which runs even
//! for a hidden window. The sink calls `request_repaint` so a hotkey press
//! never sleeps in the channel forever.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

use crate::clip::{ClipKind, ClipPayload};
use crate::index::{ChipFilter, ClipSummary};
use crate::log;
use crate::platform::{self, PlatformEvent};
use crate::popup::{placed, HEIGHT, MIN_HEIGHT, MIN_WIDTH, WIDTH};
use crate::store::{MachineTab, Store};
use crate::thumb;
use crate::win;

/// Same cap as the legacy popup: plenty for one open, small enough that a
/// refill stays a keystroke's work.
const MAX_RESULTS: usize = 300;

/// Row height in points. The legacy list uses 46 logical pixels at 96 DPI;
/// egui points are the same thing at that scale.
const ROW_HEIGHT: f32 = 46.0;

/// 缩略图解码目标长边(设备像素):36 点的行内图按 2× 屏也够清晰。
const THUMB_PX: usize = 96;
/// 缓存条数:160 张 96px 纹理约 6 MB 显存,足够覆盖一屏可见行的来回滚动。
const THUMB_CACHE_CAP: usize = 160;

const DEFAULT_SIZE: (i32, i32) = (WIDTH, HEIGHT);

const COLOR_BG: egui::Color32 = egui::Color32::from_rgb(0x1E, 0x1E, 0x1E);
const COLOR_INPUT_BG: egui::Color32 = egui::Color32::from_rgb(0x2A, 0x2A, 0x2A);
const COLOR_SELECTED: egui::Color32 = egui::Color32::from_rgb(0x99, 0x5A, 0x3C);
const COLOR_HOVER: egui::Color32 = egui::Color32::from_rgb(0x2E, 0x2E, 0x2E);
const COLOR_TEXT: egui::Color32 = egui::Color32::from_rgb(0xE6, 0xE6, 0xE6);
const COLOR_META: egui::Color32 = egui::Color32::from_rgb(0x8C, 0x8C, 0x8C);
const COLOR_PIN: egui::Color32 = egui::Color32::from_rgb(0x4A, 0xA2, 0xD2);

/// The context handle the platform thread needs to wake the UI loop. Set in
/// the app creator, before the first frame — a hotkey pressed microseconds
/// after launch still gets its repaint.
static UI_CTX: OnceLock<egui::Context> = OnceLock::new();

/// Where the egui shell runs from `main`. Returns when the app quits; the
/// caller's only job afterwards is the final log line.
pub fn run(store: Arc<Store>) {
    let (event_tx, event_rx) = mpsc::channel::<PlatformEvent>();

    platform::start(Box::new(move |event| {
        // A lost send only means the UI loop is gone; the app is quitting.
        let _ = event_tx.send(event);
        if let Some(ctx) = UI_CTX.get() {
            ctx.request_repaint();
        }
    }));

    let (thumb_tx, thumb_rx) = spawn_thumb_worker(Arc::clone(&store));
    let (hydrate_tx, hydrate_rx) = mpsc::channel();

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ClipPlus")
            // 预建窗口,启动即隐藏:热键按下时只剩"显示"这一步,秒开保留。
            .with_visible(false)
            .with_decorations(false)
            .with_resizable(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_inner_size([DEFAULT_SIZE.0 as f32, DEFAULT_SIZE.1 as f32])
            .with_min_inner_size([MIN_WIDTH as f32, MIN_HEIGHT as f32]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    let app = App::new(store, event_rx, thumb_tx, thumb_rx, hydrate_tx, hydrate_rx);
    let result = eframe::run_native(
        "ClipPlus",
        native,
        Box::new(move |cc| {
            let _ = UI_CTX.set(cc.egui_ctx.clone());
            install_fonts(&cc.egui_ctx);
            apply_style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );

    if let Err(err) = result {
        log::error(&format!("egui event loop failed: {err}"));
    }

    // Both exit paths — the tray's Quit (handled in `logic`) and anything that
    // tears the window down on its own — end with the platform thread here.
    platform::request_quit();
    platform::shutdown();
}

/// 微软雅黑,运行时从系统字体目录加载,不进 exe。读不到就退回 egui 自带字体:
/// 界面照常工作,只是中文没有字形——记进日志,不要静默。
fn install_fonts(ctx: &egui::Context) {
    const CANDIDATES: [&str; 2] = [
        r"C:\Windows\Fonts\msyh.ttc",  // Microsoft YaHei, index 0 = Regular
        r"C:\Windows\Fonts\msyhl.ttc", // YaHei Light fallback
    ];

    let Some(bytes) = CANDIDATES.iter().find_map(|path| std::fs::read(path).ok()) else {
        log::warn("no system CJK font found; the egui popup will miss Chinese glyphs");
        return;
    };

    ctx.add_font(FontInsert::new(
        "microsoft_yahei",
        egui::FontData::from_owned(bytes),
        vec![
            InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: FontPriority::Highest,
            },
            InsertFontFamily {
                family: egui::FontFamily::Monospace,
                priority: FontPriority::Highest,
            },
        ],
    ));
    log::info("egui popup: system CJK font loaded");
}

/// 把 egui 默认主题往旧弹窗的配色上拉:同一套深色,输入框、选中色都对照旧常量。
/// 弹窗永远是暗色,不跟随系统亮暗切换,所以两套主题样式一并改掉。
fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.visuals.panel_fill = COLOR_BG;
        style.visuals.window_fill = COLOR_BG;
        // TextEdit 的底色是 extreme_bg。
        style.visuals.extreme_bg_color = COLOR_INPUT_BG;
        style.visuals.selection.bg_fill = COLOR_SELECTED;
    });
}

/// One worker, serially decoding PNG payloads into small RGBA thumbnails.
/// `.bin` payloads read from disk (possibly a cloud placeholder) here, never
/// on the UI thread.
fn spawn_thumb_worker(
    store: Arc<Store>,
) -> (
    mpsc::Sender<String>,
    mpsc::Receiver<(String, usize, usize, Vec<u8>)>,
) {
    let (req_tx, req_rx) = mpsc::channel::<String>();
    let (done_tx, done_rx) = mpsc::channel();

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
}

/// The row context menu's outcomes, collected inside the menu closure and run
/// after it, so the closure never holds a mutable borrow across the action.
enum RowAction {
    Paste,
    Copy,
    Pin,
    Delete,
}

struct App {
    store: Arc<Store>,
    events: mpsc::Receiver<PlatformEvent>,

    /// Mirrors the real window visibility; `ViewportCommand::Visible` lands
    /// asynchronously, so the flag is what the rest of the logic reads.
    visible: bool,
    /// Window that had focus when the popup opened: the paste target.
    target: isize,
    /// When the popup was shown; focus-loss auto-hide waits out the first
    /// moments while the focus is still travelling to the window.
    shown_at: Instant,
    /// A message box this popup put up is the one thing allowed to take the
    /// focus — the focus-loss auto-hide holds its breath while it is open.
    modal_open: bool,

    search: String,
    /// Bottom-up, exactly like the legacy list: the newest clip is the last
    /// row, the one next to the search box.
    items: Vec<ClipSummary>,
    /// The caret row: what Enter pastes, what the highlight follows.
    selected: usize,
    /// Where a Shift extension started; only meaningful while `extending`.
    anchor: usize,
    extending: bool,
    focus_search: bool,
    scroll_to_newest: bool,

    // 筛选与机器
    tab: Option<String>,
    tabs: Vec<MachineTab>,
    chips: Chips,

    // 缩略图
    thumb_tx: mpsc::Sender<String>,
    thumb_rx: mpsc::Receiver<(String, usize, usize, Vec<u8>)>,
    thumbs: HashMap<String, egui::TextureHandle>,
    thumb_order: VecDeque<String>,
    thumb_pending: HashSet<String>,

    // blob 水合
    hydrate_tx: mpsc::Sender<(usize, HydrateAction, ClipPayload)>,
    hydrate_rx: mpsc::Receiver<(usize, HydrateAction, ClipPayload)>,
    /// What a hydration is in flight for, shown as a loading hint.
    hydrating: Option<HydrateAction>,
    /// Bumped by every close and every new request; a hydrate result whose
    /// generation no longer matches is dropped.
    generation: usize,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: Arc<Store>,
        events: mpsc::Receiver<PlatformEvent>,
        thumb_tx: mpsc::Sender<String>,
        thumb_rx: mpsc::Receiver<(String, usize, usize, Vec<u8>)>,
        hydrate_tx: mpsc::Sender<(usize, HydrateAction, ClipPayload)>,
        hydrate_rx: mpsc::Receiver<(usize, HydrateAction, ClipPayload)>,
    ) -> Self {
        Self {
            store,
            events,
            visible: false,
            target: 0,
            shown_at: Instant::now(),
            modal_open: false,
            search: String::new(),
            items: Vec::new(),
            selected: 0,
            anchor: 0,
            extending: false,
            focus_search: false,
            scroll_to_newest: false,
            tab: None,
            tabs: Vec::new(),
            chips: Chips::default(),
            thumb_tx,
            thumb_rx,
            thumbs: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_pending: HashSet::new(),
            hydrate_tx,
            hydrate_rx,
            hydrating: None,
            generation: 0,
        }
    }

    /// Platform events, drained here rather than in `ui`: `logic` also runs
    /// for a hidden window, which is the popup's normal state.
    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlatformEvent::Hotkey | PlatformEvent::TrayToggle => self.toggle(ctx),
                PlatformEvent::Quit => self.quit(ctx),
            }
        }
    }

    /// Thumbnail and hydration results, both delivered by background threads.
    fn drain_workers(&mut self, ctx: &egui::Context) {
        while let Ok((stem, w, h, rgba)) = self.thumb_rx.try_recv() {
            let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
            let handle = ctx.load_texture(format!("thumb:{stem}"), image, egui::TextureOptions::LINEAR);
            self.thumb_pending.remove(&stem);
            if self.thumbs.insert(stem.clone(), handle).is_none() {
                self.thumb_order.push_back(stem);
            }
            if self.thumb_order.len() > THUMB_CACHE_CAP {
                if let Some(old) = self.thumb_order.pop_front() {
                    self.thumbs.remove(&old);
                }
            }
        }

        while let Ok((gen, action, payload)) = self.hydrate_rx.try_recv() {
            if gen != self.generation {
                continue;
            }
            self.hydrating = None;
            match action {
                HydrateAction::Paste => {
                    let target = self.target;
                    self.hide(ctx);
                    crate::paste::paste_back(target, &payload);
                }
                HydrateAction::Copy => {
                    if !crate::clipboard::write(&payload) {
                        log::warn("copy failed: clipboard write");
                    }
                }
            }
        }
    }

    fn toggle(&mut self, ctx: &egui::Context) {
        if self.visible {
            self.hide(ctx);
        } else {
            self.show(ctx);
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        // Before the window takes focus, or it is already too late.
        self.target = crate::paste::capture_paste_target();

        self.search.clear();
        // The two menus are filters, reset with the box; the scope boxes and
        // the machine tab are preferences and stay.
        self.chips.kind = None;
        self.chips.time = TimeChip::All;
        self.refresh_tabs();
        self.refill();
        self.focus_search = true;

        // Same placement rule as the legacy popup: remembered position wins,
        // otherwise the monitor under the cursor; everything clamped to the
        // work area of that monitor.
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

        let width_px = win::scaled(size.0.max(MIN_WIDTH), scale).min(area.right - area.left);
        let height_px = win::scaled(size.1.max(MIN_HEIGHT), scale).min(area.bottom - area.top);
        let (left, top) = placed(remembered, &area, width_px, height_px);

        // egui 的定位/尺寸命令按逻辑点换算(内部乘 pixels_per_point),旧布局
        // 常量是物理像素:用锚定屏的 scale 折算。窗口当前所在屏与锚定屏 scale
        // 不同时会有一次性的偏差,显示后自动被 winit 的 DPI 事件纠正。
        let ppp = (scale as f32).max(0.25);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            left as f32 / ppp,
            top as f32 / ppp,
        )));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            width_px as f32 / ppp,
            height_px as f32 / ppp,
        )));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);

        self.visible = true;
        self.shown_at = Instant::now();
        log::info(&format!(
            "egui popup shown at {left},{top} {width_px}x{height_px} scale {scale:.2} ({} rows)",
            self.items.len()
        ));
    }

    fn hide(&mut self, ctx: &egui::Context) {
        if !self.visible {
            return;
        }

        // The same pair of fields the legacy popup saved: physical position,
        // logical (96-DPI) size — egui reports both rect in points, and on
        // Windows points * pixels_per_point is exactly physical.
        let ppp = ctx.pixels_per_point();
        if let Some(outer) = ctx.input(|i| i.viewport().outer_rect) {
            let position = (
                (outer.min.x * ppp).round() as i32,
                (outer.min.y * ppp).round() as i32,
            );
            let size = (outer.width().round() as i32, outer.height().round() as i32);
            crate::remember_popup_layout(position, size);
        }

        // A hydration that has not finished yet was started for a popup the
        // user can no longer see; it should never act.
        self.generation += 1;
        self.hydrating = None;
        self.visible = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        log::info("egui popup hidden");
    }

    fn quit(&mut self, ctx: &egui::Context) {
        log::info("quit requested; closing the egui loop");
        self.visible = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        platform::request_quit();
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
    fn refill(&mut self) {
        let chip_filter = ChipFilter {
            in_app: self.chips.in_app,
            in_title: self.chips.in_title,
            kind: self.chips.kind,
            since_ms: self.chips.time.since_ms(crate::settings::now_ms()),
        };
        let machine = self.tab.clone();
        let mut items = self
            .store
            .query(machine.as_deref(), &self.search, &chip_filter, MAX_RESULTS);
        items.reverse();

        self.selected = items.len().saturating_sub(1);
        self.anchor = self.selected;
        self.extending = false;
        self.items = items;
        // The view follows the newest row on every refill, the way the legacy
        // list's LB_SETTOPINDEX did after each keystroke.
        self.scroll_to_newest = true;
    }

    /// The rows a Delete covers: the Shift range, or the caret row alone.
    fn selected_rows(&self) -> Vec<usize> {
        if self.extending {
            let lo = self.anchor.min(self.selected);
            let hi = self.anchor.max(self.selected);
            (lo..=hi).collect()
        } else if self.items.is_empty() {
            Vec::new()
        } else {
            vec![self.selected]
        }
    }

    fn move_selection(&mut self, delta: isize, extend: bool) {
        if self.items.is_empty() {
            return;
        }
        if !extend {
            self.anchor = self.selected;
            self.extending = false;
        } else {
            self.extending = true;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, self.items.len() as isize - 1) as usize;
    }

    fn select_all(&mut self) {
        if self.items.is_empty() {
            return;
        }
        // The list is bottom-up: "everything" is the range 0..=last.
        self.anchor = 0;
        self.selected = self.items.len() - 1;
        self.extending = true;
    }

    fn cycle_tab(&mut self, dir: isize) {
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
        self.refill();
    }

    /// Starts reading a `.bin` payload on a worker thread. The result comes
    /// back through `hydrate_rx`; Esc, a click elsewhere or a newer request
    /// all bump the generation, and a stale result is dropped when it lands.
    fn begin_hydrate(&mut self, action: HydrateAction, stem: String) {
        self.generation += 1;
        self.hydrating = Some(action);

        let store = Arc::clone(&self.store);
        let tx = self.hydrate_tx.clone();
        let generation = self.generation;
        std::thread::spawn(move || {
            if let Some(payload) = store.read_payload(&stem) {
                let _ = tx.send((generation, action, payload));
            }
        });
    }

    /// Enter: paste into the window the popup took focus from.
    fn commit(&mut self, ctx: &egui::Context) {
        let Some(item) = self.items.get(self.selected) else {
            return;
        };

        // A blob can be a cloud placeholder whose read downloads over the
        // network — it must never happen on the UI thread. The inline payload
        // is a memory read and goes straight through.
        if item.has_blob {
            let stem = item.stem.clone();
            self.begin_hydrate(HydrateAction::Paste, stem);
            return;
        }

        let stem = item.stem.clone();
        let Some(payload) = self.store.read_payload(&stem) else {
            log::warn(&format!("nothing pasteable for {stem}"));
            self.hide(ctx);
            return;
        };

        let target = self.target;
        self.hide(ctx);
        crate::paste::paste_back(target, &payload);
    }

    /// Ctrl+C: copy without pasting.
    fn copy_selected(&mut self) {
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        let stem = item.stem.clone();
        if item.has_blob {
            self.begin_hydrate(HydrateAction::Copy, stem);
            return;
        }
        if let Some(payload) = self.store.read_payload(&stem) {
            if !crate::clipboard::write(&payload) {
                log::warn("copy failed: clipboard write");
            }
        }
    }

    /// Ctrl+P: pin or unpin the caret row. Pinning moves the row to the top,
    /// so follow the item rather than the index.
    fn toggle_pin(&mut self) {
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        let (stem, pinned) = (item.stem.clone(), item.pinned);

        if !self.store.set_pinned(&stem, !pinned) {
            return;
        }

        self.refill();
        if let Some(position) = self.items.iter().position(|item| item.stem == stem) {
            self.selected = position;
            self.anchor = position;
            self.extending = false;
        }
    }

    /// Delete: everything the Shift range (or the caret row) covers, with the
    /// same warning the legacy popup shows — a delete syncs to every machine.
    fn delete_selected(&mut self) {
        let stems: Vec<String> = self
            .selected_rows()
            .into_iter()
            .filter_map(|index| self.items.get(index).map(|item| item.stem.clone()))
            .collect();
        if stems.is_empty() {
            return;
        }

        let count = stems.len();
        let question = format!(
            "删除选中的 {count} 条记录？\n\n\
             同步目录里的记录会一起删掉，其他机器同步之后也会跟着消失，删了找不回来。\n\n\
             别的机器当月那份只能先记个「已删」的空标记（一样马上看不见），由那台机器自己清。"
        );

        self.modal_open = true;
        let confirmed =
            win::message_box("ClipPlus 删除", &question, win::MB_YESNO | win::MB_ICONWARNING)
                == win::IDYES;
        self.modal_open = false;

        if !confirmed {
            return;
        }

        let (deleted, marked) = self.store.delete_selected(&stems);
        log::info(&format!("{deleted} clip(s) deleted by hand, {marked} tombstoned"));
        self.refill();
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (esc, enter, up, down, page_up, page_down, shift, delete, ctrl_p, ctrl_c, ctrl_a, ctrl_tab) =
            ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::Escape),
                    i.key_pressed(egui::Key::Enter),
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::PageUp),
                    i.key_pressed(egui::Key::PageDown),
                    i.modifiers.shift,
                    !i.modifiers.ctrl && i.key_pressed(egui::Key::Delete),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::P),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::C),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::A),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::Tab),
                )
            });

        if esc {
            self.hide(ctx);
        }
        if enter {
            self.commit(ctx);
        }
        // 一页 = 缺省 8 行可见再留一行重叠,与旧弹窗的 page_rows 同一约定;
        // 精确行数随窗口拉伸,后续接真实可见行数。
        if up {
            self.move_selection(-1, shift);
        }
        if down {
            self.move_selection(1, shift);
        }
        if page_up {
            self.move_selection(-7, shift);
        }
        if page_down {
            self.move_selection(7, shift);
        }
        if delete {
            self.delete_selected();
        }
        if ctrl_p {
            self.toggle_pin();
        }
        if ctrl_c {
            self.copy_selected();
        }
        if ctrl_a {
            self.select_all();
        }
        if ctrl_tab {
            self.cycle_tab(if shift { -1 } else { 1 });
        }
    }

    /// Background drag on the panels' empty space, the way the legacy popup's
    /// background `WM_LBUTTONDOWN` dragged the window.
    fn start_drag_on_background(ui: &mut egui::Ui, ctx: &egui::Context, id: &str) {
        let background = ui.interact(
            ui.max_rect(),
            egui::Id::new(id),
            egui::Sense::drag(),
        );
        if background.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }

    fn draw_row(&mut self, ui: &mut egui::Ui, index: usize) {
        // Clone out of the list so the mutable work below never overlaps the
        // borrow the widgets need.
        let (stem, preview, meta, pinned, kind, has_blob) = {
            let Some(item) = self.items.get(index) else {
                return;
            };
            (
                item.stem.clone(),
                item.preview.clone(),
                item.meta.clone(),
                item.pinned,
                item.kind,
                item.has_blob,
            )
        };

        let selected = if self.extending {
            let lo = self.anchor.min(self.selected);
            let hi = self.anchor.max(self.selected);
            index >= lo && index <= hi
        } else {
            index == self.selected
        };

        let (rect, response) = ui
            .allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), egui::Sense::click());

        let bg = if selected {
            COLOR_SELECTED
        } else if response.hovered() {
            COLOR_HOVER
        } else {
            egui::Color32::TRANSPARENT
        };
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, bg);

        // 图片行:左侧缩略图,有缓存画纹理,没有就画占位块并请求后台解码。
        let mut text_left = rect.left() + 8.0;
        if kind == ClipKind::Image {
            let side = ROW_HEIGHT - 12.0;
            let thumb_rect = egui::Rect::from_min_size(
                egui::pos2(rect.left() + 6.0, rect.center().y - side / 2.0),
                egui::vec2(side, side),
            );
            match self.thumbs.get(&stem) {
                Some(texture) => {
                    painter.image(
                        texture.id(),
                        thumb_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                None => {
                    painter.rect_filled(thumb_rect, 4.0, COLOR_INPUT_BG);
                    if !self.thumb_pending.contains(&stem) {
                        self.thumb_pending.insert(stem.clone());
                        let _ = self.thumb_tx.send(stem.clone());
                    }
                }
            }
            text_left = thumb_rect.right() + 8.0;
        }

        painter.text(
            egui::pos2(text_left, rect.top() + 4.0),
            egui::Align2::LEFT_TOP,
            &preview,
            egui::FontId::proportional(15.0),
            COLOR_TEXT,
        );
        let mut meta = meta;
        if pinned {
            meta.insert_str(0, "📌 ");
        }
        painter.text(
            egui::pos2(text_left, rect.bottom() - 4.0),
            egui::Align2::LEFT_BOTTOM,
            &meta,
            egui::FontId::proportional(11.5),
            if pinned { COLOR_PIN } else { COLOR_META },
        );

        let shift = ui.input(|i| i.modifiers.shift);
        if response.clicked() {
            if shift {
                self.extending = true;
            } else {
                self.extending = false;
                self.anchor = index;
            }
            self.selected = index;
        }

        let mut action: Option<RowAction> = None;
        response.context_menu(|ui| {
            if ui.button("粘贴（Enter）").clicked() {
                action = Some(RowAction::Paste);
            }
            if ui.button("复制（Ctrl+C）").clicked() {
                action = Some(RowAction::Copy);
            }
            let pin_label = if pinned { "取消固定（Ctrl+P）" } else { "固定（Ctrl+P）" };
            if ui.button(pin_label).clicked() {
                action = Some(RowAction::Pin);
            }
            if ui.button("删除（Delete）").clicked() {
                action = Some(RowAction::Delete);
            }
            if has_blob {
                ui.label(
                    egui::RichText::new("（含 .bin,操作在后台读取）")
                        .size(11.0)
                        .color(COLOR_META),
                );
            }
        });
        match action {
            Some(RowAction::Paste) => {
                self.selected = index;
                self.anchor = index;
                self.extending = false;
                let ctx = ui.ctx().clone();
                self.commit(&ctx);
            }
            Some(RowAction::Copy) => {
                self.selected = index;
                self.anchor = index;
                self.extending = false;
                self.copy_selected();
            }
            Some(RowAction::Pin) => {
                self.selected = index;
                self.anchor = index;
                self.extending = false;
                self.toggle_pin();
            }
            Some(RowAction::Delete) => {
                self.selected = index;
                self.anchor = index;
                self.extending = false;
                self.delete_selected();
            }
            None => {}
        }
    }
}

impl eframe::App for App {
    /// Runs for a hidden window too — platform events, thumbnails and
    /// hydration results cannot wait here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);
        self.drain_workers(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.visible {
            return;
        }
        let ctx = ui.ctx().clone();

        // 失焦即收起,对应旧弹窗的 WM_ACTIVATE;刚显示的头 300ms 不检查,
        // 焦点还在路上;弹窗自己拉起的删除确认框是唯一例外。
        if !self.modal_open
            && self.shown_at.elapsed() > Duration::from_millis(300)
            && !ctx.input(|i| i.viewport().focused.unwrap_or(true))
        {
            self.hide(&ctx);
            return;
        }

        self.handle_keys(&ctx);

        // 0.36 的面板都长在传入的根 Ui 上,不再接 Context。先加的贴底边:
        // 搜索条在最下,机器条在它上面。
        egui::Panel::bottom("search_bar")
            .frame(egui::Frame::default().fill(COLOR_BG).inner_margin(8.0))
            .show(ui, |ui| {
                Self::start_drag_on_background(ui, &ctx, "drag_search");

                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(self.chips.in_app, format!("{} 应用", check_mark(self.chips.in_app)))
                        .clicked()
                    {
                        self.chips.in_app = !self.chips.in_app;
                        self.refill();
                    }
                    if ui
                        .selectable_label(
                            self.chips.in_title,
                            format!("{} 标题", check_mark(self.chips.in_title)),
                        )
                        .clicked()
                    {
                        self.chips.in_title = !self.chips.in_title;
                        self.refill();
                    }

                    let kind_label = match self.chips.kind {
                        None => "类型 ▾".to_string(),
                        Some(kind) => format!("类型:{}", kind.label()),
                    };
                    ui.menu_button(kind_label, |ui| {
                        if ui
                            .selectable_label(self.chips.kind.is_none(), "全部")
                            .clicked()
                        {
                            self.chips.kind = None;
                            self.refill();
                        }
                        for kind in [ClipKind::Text, ClipKind::Image, ClipKind::Files] {
                            if ui
                                .selectable_label(self.chips.kind == Some(kind), kind.label())
                                .clicked()
                            {
                                self.chips.kind = Some(kind);
                                self.refill();
                            }
                        }
                    });

                    let time_label = if self.chips.time == TimeChip::All {
                        "时间 ▾".to_string()
                    } else {
                        format!("时间:{}", self.chips.time.label())
                    };
                    ui.menu_button(time_label, |ui| {
                        for preset in [TimeChip::All, TimeChip::Today, TimeChip::Seven, TimeChip::Thirty] {
                            if ui
                                .selectable_label(self.chips.time == preset, preset.label())
                                .clicked()
                            {
                                self.chips.time = preset;
                                self.refill();
                            }
                        }
                    });

                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("搜索内容 …")
                            .desired_width(ui.available_width())
                            .font(egui::FontId::proportional(15.0)),
                    );
                    if self.focus_search {
                        response.request_focus();
                        self.focus_search = false;
                    }
                    if response.changed() {
                        self.refill();
                    }
                });
            });

        egui::Panel::bottom("tab_strip")
            .frame(
                egui::Frame::default()
                    .fill(COLOR_BG)
                    .inner_margin(egui::Margin {
                        left: 8,
                        right: 8,
                        top: 4,
                        bottom: 4,
                    }),
            )
            .show(ui, |ui| {
                Self::start_drag_on_background(ui, &ctx, "drag_tabs");

                // Tabs live in the store as (id, label); clone the labels out
                // so the mutable tab switch below never overlaps the borrow.
                let tabs: Vec<(Option<String>, String)> = self
                    .tabs
                    .iter()
                    .map(|tab| (tab.id.clone(), tab.label.clone()))
                    .collect();
                ui.horizontal(|ui| {
                    for (id, label) in tabs {
                        let active = self.tab == id;
                        if ui.selectable_label(active, label).clicked() {
                            self.tab = id.clone();
                            self.refill();
                        }
                    }
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(COLOR_BG).inner_margin(8.0))
            .show(ui, |ui| {
                Self::start_drag_on_background(ui, &ctx, "drag_list");

                if let Some(action) = self.hydrating {
                    let hint = match action {
                        HydrateAction::Paste => "正在加载完整内容，稍后自动粘贴 …",
                        HydrateAction::Copy => "正在加载完整内容 …",
                    };
                    ui.label(egui::RichText::new(hint).size(12.0).color(COLOR_META));
                }

                if self.items.is_empty() {
                    ui.label(
                        egui::RichText::new("没有匹配的记录")
                            .size(14.0)
                            .color(COLOR_META),
                    );
                    return;
                }

                let mut rows = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if self.scroll_to_newest {
                    // Content height: the area clamps it to the bottom edge.
                    rows = rows.vertical_scroll_offset(self.items.len() as f32 * ROW_HEIGHT);
                    self.scroll_to_newest = false;
                }
                rows.show_rows(ui, ROW_HEIGHT, self.items.len(), |ui, range| {
                    for index in range {
                        self.draw_row(ui, index);
                    }
                });
            });
    }
}
