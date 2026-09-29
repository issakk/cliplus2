//! The egui popup — Phase 1 skeleton.
//!
//! Enabled with `CLIPPLUS_UI=egui`; the legacy Win32 popup stays the default.
//! What is here: hotkey/tray → show/hide, paste-target capture, YaHei at
//! runtime, search over the index, a bottom-anchored list (newest row nearest
//! the search box), keyboard navigation, and paste-back through the shared
//! `paste.rs` path. What is deliberately not here yet: filter chips, machine
//! tabs, the context menu, thumbnails, blob hydration — those land in Phase 2.
//!
//! Threading: the platform thread sends `PlatformEvent`s over a channel. The
//! popup is usually hidden and egui paints nothing then, so the sink also
//! calls `request_repaint` — that is what keeps a hotkey press from sleeping
//! in the channel forever.

use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

use crate::index::{ChipFilter, ClipSummary};
use crate::log;
use crate::platform::{self, PlatformEvent};
use crate::popup::{placed, HEIGHT, MIN_HEIGHT, MIN_WIDTH, WIDTH};
use crate::store::Store;
use crate::win;

/// Same cap as the legacy popup: plenty for one open, small enough that a
/// refill stays a keystroke's work.
const MAX_RESULTS: usize = 300;

/// Row height in points. The legacy list uses 46 logical pixels at 96 DPI;
/// egui points are the same thing at that scale.
const ROW_HEIGHT: f32 = 46.0;

/// 旧弹窗 620×458 的默认开口尺寸(逻辑像素)直接沿用。
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

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::new()
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

    let app = App::new(store, event_rx);
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
fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals.panel_fill = COLOR_BG;
    style.visuals.window_fill = COLOR_BG;
    // TextEdit 的底色是 extreme_bg。
    style.visuals.extreme_bg_color = COLOR_INPUT_BG;
    style.visuals.selection.bg_fill = COLOR_SELECTED;
    ctx.set_style(style);
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

    search: String,
    /// Bottom-up, exactly like the legacy list: the newest clip is the last
    /// row, the one next to the search box.
    items: Vec<ClipSummary>,
    selected: usize,
    focus_search: bool,
    scroll_to_newest: bool,
}

impl App {
    fn new(store: Arc<Store>, events: mpsc::Receiver<PlatformEvent>) -> Self {
        Self {
            store,
            events,
            visible: false,
            target: 0,
            shown_at: Instant::now(),
            search: String::new(),
            items: Vec::new(),
            selected: 0,
            focus_search: false,
            scroll_to_newest: false,
        }
    }

    /// Platform events, drained here rather than in `ui`: `logic` also runs for
    /// a hidden window, which is the popup's normal state.
    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlatformEvent::Hotkey | PlatformEvent::TrayToggle => self.toggle(ctx),
                PlatformEvent::Quit => self.quit(ctx),
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
        // 不同时会有一次性的偏差,骨架阶段接受,Phase 2 走原生句柄消除。
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

    /// Refills from the index, bottom-up. Also runs per keystroke, exactly
    /// like `fill_list` in the legacy popup.
    fn refill(&mut self) {
        let chips = ChipFilter::default();
        let mut items = self.store.query(None, &self.search, &chips, MAX_RESULTS);
        items.reverse();

        self.selected = items.len().saturating_sub(1);
        self.items = items;
        // The view follows the newest row on every refill, the way the legacy
        // list's LB_SETTOPINDEX did after each keystroke.
        self.scroll_to_newest = true;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, self.items.len() as isize - 1) as usize;
    }

    /// Enter: paste into the window the popup took focus from.
    fn commit(&mut self, ctx: &egui::Context) {
        let Some(item) = self.items.get(self.selected) else {
            return;
        };

        // A blob can be a cloud placeholder whose read downloads over the
        // network; the legacy popup hydrates those on a worker thread. Until
        // that arrives here, refusing beats freezing the UI thread.
        if item.has_blob {
            log::warn("egui skeleton: blob-backed items paste in Phase 2; skipped");
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
        if item.has_blob {
            log::warn("egui skeleton: blob-backed items copy in Phase 2; skipped");
            return;
        }
        let stem = item.stem.clone();
        if let Some(payload) = self.store.read_payload(&stem) {
            if !crate::clipboard::write(&payload) {
                log::warn("copy failed: clipboard write");
            }
        }
    }
}

impl eframe::App for App {
    /// Runs for a hidden window too — platform events cannot wait here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.visible {
            return;
        }
        let ctx = ui.ctx().clone();

        // 失焦即收起,对应旧弹窗的 WM_ACTIVATE;刚显示的头 300ms 不检查,
        // 焦点还在路上。
        if self.shown_at.elapsed() > Duration::from_millis(300)
            && !ctx.input(|i| i.viewport().focused.unwrap_or(true))
        {
            self.hide(&ctx);
            return;
        }

        self.handle_keys(&ctx);

        egui::TopBottomPanel::bottom("search_bar")
            .frame(egui::Frame::default().fill(COLOR_BG).inner_margin(8.0))
            .show(&ctx, |ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("搜索内容 …")
                        .desired_width(f32::INFINITY)
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

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(COLOR_BG).inner_margin(8.0))
            .show(&ctx, |ui| {
                if self.items.is_empty() {
                    ui.label(
                        egui::RichText::new("还没有记录")
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

impl App {
    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (esc, enter, up, down, page_up, page_down, copy) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::PageUp),
                i.key_pressed(egui::Key::PageDown),
                i.modifiers.ctrl && i.key_pressed(egui::Key::C),
            )
        });

        if esc {
            self.hide(ctx);
        }
        if enter {
            self.commit(ctx);
        }
        if up {
            self.move_selection(-1);
        }
        if down {
            self.move_selection(1);
        }
        // 一页 = 缺省 8 行可见再留一行重叠,与旧弹窗的 page_rows 同一约定;
        // 实际行数随窗口拉伸,精确值 Phase 2 一并接上。
        if page_up {
            self.move_selection(-7);
        }
        if page_down {
            self.move_selection(7);
        }
        if copy {
            self.copy_selected();
        }
    }

    fn draw_row(&mut self, ui: &mut egui::Ui, index: usize) {
        // Clone out of the list so the mutable work below never overlaps the
        // borrow the widgets need.
        let (preview, mut meta, pinned) = {
            let Some(item) = self.items.get(index) else {
                return;
            };
            (item.preview.clone(), item.meta.clone(), item.pinned)
        };

        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), egui::Sense::click());

        let bg = if index == self.selected {
            COLOR_SELECTED
        } else if response.hovered() {
            COLOR_HOVER
        } else {
            egui::Color32::TRANSPARENT
        };
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, bg);

        let pad = 8.0;
        painter.text(
            egui::pos2(rect.left() + pad, rect.top() + 4.0),
            egui::Align2::LEFT_TOP,
            &preview,
            egui::FontId::proportional(15.0),
            COLOR_TEXT,
        );
        if pinned {
            meta.insert_str(0, "📌 ");
        }
        painter.text(
            egui::pos2(rect.left() + pad, rect.bottom() - 4.0),
            egui::Align2::LEFT_BOTTOM,
            &meta,
            egui::FontId::proportional(11.5),
            if pinned { COLOR_PIN } else { COLOR_META },
        );

        if response.clicked() {
            self.selected = index;
            let ctx = ui.ctx().clone();
            self.commit(&ctx);
        }
    }
}
