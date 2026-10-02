//! The egui popup.
//!
//! The one popup: egui draws it on the GPU (glow), the Win32 one is gone.
//! Filter chips, machine tabs, the row context menu, image thumbnails,
//! background hydration of `.bin` payloads, multi-selection, remembered layout.
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
use crate::store::{MachineTab, Store};
use crate::thumb;
use crate::win;

/// Same cap as the legacy popup: plenty for one open, small enough that a
/// refill stays a keystroke's work.
const MAX_RESULTS: usize = 300;

/// Row height in points. The legacy list used 46 logical pixels at 96 DPI;
/// egui points are the same thing at that scale.
const ROW_HEIGHT: f32 = 46.0;

/// 窗口边缘这条看不见的带子属于缩放——旧弹窗的 `GRIP` 是同一个数,单位也是
/// 96 DPI 下的逻辑像素(egui 的 point 就是这个口径)。
const RESIZE_GRIP: f32 = 5.0;

/// 露脸前先在屏幕外停多远(物理像素)。Windows 不给隐藏窗口发重绘消息,所以
/// 表面里的内容只能"亮着"画;亮着又不能让用户看见,就先停在没有任何显示器
/// 的地方画一帧——`-32000` 是 Windows 自己最小化窗口时用的那类坐标,任何
/// 多屏桌面都到不了。
const PARK_OFFSET: i32 = 30_000;

/// The open-at size the popup has always had (8 rows plus the bottom bands),
/// and the floor a drag enforces. Kept as plain numbers since the layout
/// constants they were derived from died with the legacy popup.
const DEFAULT_SIZE: (i32, i32) = (620, 458);
const MIN_SIZE: (i32, i32) = (420, 228);

/// 缩略图解码目标长边(设备像素):36 点的行内图按 2× 屏也够清晰。
const THUMB_PX: usize = 96;
/// 缓存条数:160 张 96px 纹理约 6 MB 显存,足够覆盖一屏可见行的来回滚动。
const THUMB_CACHE_CAP: usize = 160;

/// 弹窗与三个次级对话框共用的一套深色调:对话框从 dialogs.rs 引这几样。
pub const COLOR_BG: egui::Color32 = egui::Color32::from_rgb(0x1E, 0x1E, 0x1E);
pub const COLOR_INPUT_BG: egui::Color32 = egui::Color32::from_rgb(0x2A, 0x2A, 0x2A);
const COLOR_SELECTED: egui::Color32 = egui::Color32::from_rgb(0x99, 0x5A, 0x3C);
const COLOR_HOVER: egui::Color32 = egui::Color32::from_rgb(0x2E, 0x2E, 0x2E);
pub const COLOR_TEXT: egui::Color32 = egui::Color32::from_rgb(0xE6, 0xE6, 0xE6);
pub const COLOR_META: egui::Color32 = egui::Color32::from_rgb(0x8C, 0x8C, 0x8C);
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
            // 这一行只是起点——eframe 会自己把它显出来,真正让它藏着的是 `enforce_hidden`。
            .with_visible(false)
            .with_decorations(false)
            .with_resizable(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_inner_size([DEFAULT_SIZE.0 as f32, DEFAULT_SIZE.1 as f32])
            .with_min_inner_size([MIN_SIZE.0 as f32, MIN_SIZE.1 as f32]),
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
    // 按住超过 0.8 秒 egui 就不再认这是点击(触屏长按的默认值)。鼠标上那等于
    // "按慢一点就选不中",而且按住不动反而会被判成拖动——列表整块背景正是
    // "拖着移动窗口",于是手势变成窗口跟着指针跑。桌面没有这条规矩,旧 Win32
    // 弹窗也没有,关掉它。
    ctx.options_mut(|options| options.input_options.max_click_duration = f64::INFINITY);
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
    Edit,
}

/// The row context menu's outcomes, collected inside the menu closure and run
/// after it, so the closure never holds a mutable borrow across the action.
enum RowAction {
    Paste,
    Copy,
    Edit,
    Pin,
    Delete,
    SelectAll,
}

struct App {
    store: Arc<Store>,
    events: mpsc::Receiver<PlatformEvent>,

    /// Mirrors the real window visibility; `ViewportCommand::Visible` lands
    /// asynchronously, so the flag is what the rest of the logic reads.
    visible: bool,
    /// 已经在屏幕外"亮着"、正在等第一帧画完:画完就挪到位并抢焦点。
    /// 这一段用户什么都看不到(窗口在屏幕外),但窗口已经是可见状态,eframe
    /// 会正常跑 ui() 并绘制——这是让表面里有内容的唯一办法。
    parked: bool,
    /// 离屏幕外那几帧还差几帧(见 `unpark`)。
    park_frames: u32,
    /// 启动时趁窗口藏着摆过一次位置尺寸了(避免第一次热键现场改尺寸)。
    geometry_applied: bool,
    /// 已经为"窗口本该藏着"记过一次日志(见 `enforce_hidden`)。
    hide_logged: bool,
    /// Window that had focus when the popup opened: the paste target.
    target: isize,
    /// When the popup was shown; focus-loss auto-hide waits out the first
    /// moments while the focus is still travelling to the window.
    shown_at: Instant,
    /// A message box this popup put up is the one thing allowed to take the
    /// focus — the focus-loss auto-hide holds its breath while it is open.
    modal_open: bool,
    /// 热键触发的收起挂起中:组合键还没物理松开,等松开再真的藏(见 `toggle`)。
    hotkey_hide_pending: bool,
    /// 挂起的最后期限:键一直不松(卡键)也不能把弹窗吊着一秒以上。
    hotkey_hide_deadline: Instant,
    /// 收起那一刻置位:`Visible(false)` 所在的这一帧会先完整上屏(命令比画面
    /// 晚一步落地),`ui()` 靠它把最后一份内容补画进这一帧——见 `draw_final_frame`。
    just_hidden: bool,

    search: String,
    /// Bottom-up, exactly like the legacy list: the newest clip is the last
    /// row, the one next to the search box.
    items: Vec<ClipSummary>,
    /// 选中行集:点击、Shift 扩选、Ctrl 点选、Ctrl+A 攒出来的任意集合,
    /// Delete 与多选复制按它算。
    selected: HashSet<usize>,
    /// 焦点行:Enter 粘贴、Ctrl+P 固定、菜单动作落在它上面的那一行。
    caret: usize,
    /// Shift 扩选的起点。
    anchor: usize,
    /// 鼠标正按着拖动时,按下那一行的下标(拖动扩选的起点)。None = 没在拖。
    drag_from: Option<usize>,
    focus_search: bool,
    scroll_to_newest: bool,
    /// 搜索框当前是否持有焦点:持有的时候 Delete 是删字,不是删记录。
    search_focused: bool,
    /// 上一帧列表实际可见的行数,翻页键按它算。
    visible_rows: usize,
    /// 指针这一刻是不是压在窗口边缘的缩放带上:是的话面板空白不让抢成拖动。
    grip_active: bool,
    /// 列表当前的滚动偏移,滚轮的转发以上一帧的值为基准。
    list_offset: f32,
    /// 下一帧要强制设置的列表偏移(滚轮转发 / 删除后归位),设了就压过自然滚动。
    pending_offset: Option<f32>,
    /// 上一帧列表的屏幕矩形:判断滚轮落在列表上还是落在那两条按钮带上。
    list_rect: Option<egui::Rect>,

    /// 待确认的删除:要删的 stem。有值时弹窗里挂着删除确认框(egui 自己的
    /// 深色模态,不再拉起系统 MessageBox)。
    confirm_delete: Option<Vec<String>>,

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
    // blob 水合。第四项是 `None` 表示这一趟什么都没读出来(.bin 丢了、或
    // 全是图片拼不出文本)——也要回话,不然提示一直挂着、动作悄悄不发生。
    hydrate_tx: mpsc::Sender<(usize, HydrateAction, Vec<String>, Option<ClipPayload>)>,
    hydrate_rx: mpsc::Receiver<(usize, HydrateAction, Vec<String>, Option<ClipPayload>)>,
    /// What a hydration is in flight for, shown as a loading hint.
    hydrating: Option<HydrateAction>,
    /// Bumped by every close and every new request; a hydrate result whose
    /// generation no longer matches is dropped.
    generation: usize,

    /// 三个次级对话框(设置/清理/编辑)的 egui 视口。
    dialogs: crate::dialogs::Dialogs,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: Arc<Store>,
        events: mpsc::Receiver<PlatformEvent>,
        thumb_tx: mpsc::Sender<String>,
        thumb_rx: mpsc::Receiver<(String, usize, usize, Vec<u8>)>,
        hydrate_tx: mpsc::Sender<(usize, HydrateAction, Vec<String>, Option<ClipPayload>)>,
        hydrate_rx: mpsc::Receiver<(usize, HydrateAction, Vec<String>, Option<ClipPayload>)>,
    ) -> Self {
        Self {
            store,
            events,
            visible: false,
            parked: false,
            park_frames: 0,
            geometry_applied: false,
            hide_logged: false,
            target: 0,
            shown_at: Instant::now(),
            modal_open: false,
            hotkey_hide_pending: false,
            hotkey_hide_deadline: Instant::now(),
            just_hidden: false,
            search: String::new(),
            items: Vec::new(),
            selected: HashSet::new(),
            caret: 0,
            anchor: 0,
            drag_from: None,
            focus_search: false,
            scroll_to_newest: false,
            search_focused: false,
            visible_rows: 8,
            grip_active: false,
            list_offset: 0.0,
            pending_offset: None,
            list_rect: None,
            confirm_delete: None,
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
            dialogs: crate::dialogs::Dialogs::new(),
        }
    }

    /// Platform events, drained here rather than in `ui`: `logic` also runs
    /// for a hidden window, which is the popup's normal state.
    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlatformEvent::Hotkey | PlatformEvent::TrayToggle => self.toggle(ctx),
                // 托盘菜单的「设置…」:对话框自己按当前设置填字段。
                PlatformEvent::TraySettings => self.dialogs.open_settings(),
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

        while let Ok((gen, action, stems, payload)) = self.hydrate_rx.try_recv() {
            if gen != self.generation {
                continue;
            }
            self.hydrating = None;
            // 一条都没读出来时不静默:提示收掉、日志留痕,弹窗留在原地,
            // 让用户自己决定下一步(旧路径在这里什么都不做)。
            let Some(payload) = payload else {
                log::warn(&format!(
                    "hydrate read nothing for {} row(s); keeping the popup open",
                    stems.len()
                ));
                continue;
            };
            match action {
                HydrateAction::Paste => {
                    self.paste_after_hide(ctx, payload);
                }
                HydrateAction::Copy => {
                    // 与 inline 路径同一收尾:写成功才收起弹窗。
                    if crate::clipboard::write(&payload) {
                        log::info(&format!(
                            "copied {} clip(s) from the history list",
                            stems.len()
                        ));
                        self.hide(ctx);
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
        }
    }

    fn toggle(&mut self, ctx: &egui::Context) {
        // 准备中(pending)也算"开着":那一瞬间再按热键应当是取消打开。
        if self.visible {
            self.hide_after_hotkey_release(ctx);
        } else {
            self.show(ctx);
        }
    }

    /// 热键按下时的收起路径:热键吞掉的是 key-down,配对的 key-up 会落到
    /// 此刻持有焦点的窗口。弹窗当场藏掉的话焦点立刻回到目标程序,那串悬空
    /// 的 key-up(Ctrl↑/W↑,从来没有配对的 down)就进了别人家——输入法/
    /// AltGr 状态机会把它补全成一次右 Alt,误触前台软件的热键(微信的
    /// "按住说话"就是这么被凭空拉起来的)。等组合键物理松开再藏,让孤儿
    /// key-up 落进本窗口自灭。
    fn hide_after_hotkey_release(&mut self, ctx: &egui::Context) {
        if hotkey_keys_all_up() {
            self.hide(ctx);
        } else {
            // 键还按着:挂起,交给 logic() 每帧复查,松手即藏;最长吊一秒,
            // 卡键也不能把弹窗留在屏幕上。
            self.hotkey_hide_pending = true;
            self.hotkey_hide_deadline = Instant::now() + Duration::from_secs(1);
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        // Before the window takes focus, or it is already too late.
        self.target = crate::paste::capture_paste_target();

        self.search.clear();
        // 上一次弹窗留下的删除确认不能跟过来:那批 stem 是上一轮选的,而确认
        // 框里的 Enter 直接删除——用户按的却是"粘贴"。
        self.confirm_delete = None;
        // 同上:拖动扩选的起点不跨弹窗——它说的是"指针正按着",下次打开没人按。
        self.drag_from = None;
        // The two menus are filters, reset with the box; the scope boxes and
        // the machine tab are preferences and stay.
        self.chips.kind = None;
        self.chips.time = TimeChip::All;
        self.refresh_tabs();
        self.refill();
        self.focus_search = true;

        // 先在屏幕外亮出来,让 eframe 把内容画进表面(隐藏窗口收不到重绘消息,
        // 画不了);下一帧再挪到位并抢焦点——直接亮在目标位置的话,窗口会先带着
        // 空表面出现在屏幕正中,那就是"和弹窗一样大的空黑框"。
        let (left, top, width_px, height_px, scale) = self.target_geometry();
        let ppp = (scale as f32).max(0.25);
        self.apply_size(ctx, width_px, height_px, ppp);
        self.move_window_to(ctx, park_position(left, top, ppp));

        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        // 焦点在停车帧就抢:热键吞掉的 key-down 的配对 key-up 马上就到(用户
        // 松手),晚两帧再抢它们就落进目标程序,输入法层会把悬空的 Ctrl↑
        // 补全成右 Alt,误触微信这类前台软件的热键。屏幕外的窗口照样可以
        // 合法持焦点,用户什么也看不见。
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();

        self.visible = true;
        self.parked = true;
        self.park_frames = 0;
        self.shown_at = Instant::now();
        log::info(&format!(
            "egui popup parked off-screen for its first paint; target {left},{top} {width_px}x{height_px} scale {scale:.2} ({} rows)",
            self.items.len()
        ));
    }

    /// 停车的第二半:屏幕外那一帧已经画完(表面里有内容了),把窗口挪到它该在
    /// 的位置并抢焦点——用户看到的是画好的窗口,不是一块没画过的表面。
    ///
    /// 等两帧不是保险起见凑数:第一帧才是把内容画进去的那一帧,这一帧的挪动
    /// 必须排在它后面。
    fn unpark(&mut self, ctx: &egui::Context) {
        if !self.parked {
            return;
        }
        self.park_frames += 1;
        if self.park_frames < 2 {
            ctx.request_repaint();
            return;
        }

        self.parked = false;
        let (left, top, _, _, scale) = self.target_geometry();
        let ppp = (scale as f32).max(0.25);
        self.move_window_to(ctx, egui::pos2(left as f32 / ppp, top as f32 / ppp));

        // 焦点已在停车帧抢过;这里只在状态未知时补一次。用户在这两帧里点去
        // 了别的窗口就不往回抢——失焦自动收起会接手,抢回来反而是焦点闪烁。
        if ctx.input(|i| i.viewport().focused).is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        ctx.request_repaint();

        self.shown_at = Instant::now();
        log::info(&format!("egui popup shown at {left},{top}"));
    }

    /// 按记住的位置/尺寸算出窗口该在哪、多大(没有记住就居中在鼠标所在的屏幕)。
    /// 返回物理位置/尺寸和这一屏的缩放。
    fn target_geometry(&self) -> (i32, i32, i32, i32, f64) {
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
        // ponytail: 这里用的是**锚定屏**的缩放,而 egui 落地位置/尺寸命令时乘的
        // 是窗口**当前**那块屏的 pixels_per_point(见 egui-winit 的
        // `process_viewport_command`)。两块屏缩放不同时这个换算是偏的——本机
        // 两块都是 150%,改了也没法验证;要动就先把混合 DPI 的机器摆上,
        // 顺便确认跨屏时的 WM_DPICHANGED 重算有没有把尺寸那一路补回来。

        let width_px = win::scaled(size.0.max(MIN_SIZE.0), scale).min(area.right - area.left);
        let height_px = win::scaled(size.1.max(MIN_SIZE.1), scale).min(area.bottom - area.top);
        let (left, top) = placed(remembered, &area, width_px, height_px);

        (left, top, width_px, height_px, scale)
    }

    /// 只在和现状不一样时才发尺寸命令:同样的尺寸再发一次也会让 GL 表面重建,
    /// 白白闪一下。
    ///
    /// egui 的尺寸命令按逻辑点换算(内部乘 pixels_per_point),旧布局常量是物理
    /// 像素:用锚定屏的 scale 折算。
    fn apply_size(&self, ctx: &egui::Context, width_px: i32, height_px: i32, ppp: f32) {
        let wanted = egui::vec2(width_px as f32 / ppp, height_px as f32 / ppp);
        let current = ctx.input(|i| i.viewport().inner_rect).map(|rect| rect.size());
        if current.map_or(true, |size| (size - wanted).length() > 1.0) {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(wanted));
        }
    }

    /// 同上,位置一侧。挪动不会重建表面,所以这一步放在停车之后是安全的。
    fn move_window_to(&self, ctx: &egui::Context, position: egui::Pos2) {
        let current = ctx.input(|i| i.viewport().outer_rect).map(|rect| rect.min);
        if current.map_or(true, |now| (now - position).length() > 1.0) {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
        }
    }

    /// 启动时窗口还藏着:现在就把上次的位置和尺寸摆好,第一次热键按下时不必
    /// 现场改尺寸(改尺寸正是表面重建的来源)。
    fn prepare_startup_geometry(&mut self, ctx: &egui::Context) {
        if self.geometry_applied || self.visible {
            return;
        }
        self.geometry_applied = true;
        let (left, top, width_px, height_px, scale) = self.target_geometry();
        let ppp = (scale as f32).max(0.25);
        self.apply_size(ctx, width_px, height_px, ppp);
        self.move_window_to(ctx, egui::pos2(left as f32 / ppp, top as f32 / ppp));
    }

    /// 刚显示的头 150 毫秒多要几帧:第一帧的内容可能是在尺寸或 DPI 还没完全
    /// 落定时画的,让窗口自己收敛(十来个空帧,开销可以忽略)。
    fn keep_repainting_just_after_show(&self, ctx: &egui::Context) {
        if self.visible && self.shown_at.elapsed() < Duration::from_millis(150) {
            ctx.request_repaint();
        }
    }

    /// eframe 自己会把窗口显出来:它建窗口时先藏着,画出第一帧后再 `set_visible(true)`
    /// (`epi_integration::post_rendering` —— 就是它"启动不闪白"的那个做法),
    /// `with_visible(false)` 在它面前毫无作用。那一下显出来的是没画过任何东西的表面,
    /// 于是启动后桌面上就杵着一个和弹窗一样大的空黑框,直到第一次按热键才被顶掉。
    ///
    /// 那一帧拦不住(命令都在同一帧末尾落地),所以每一帧都把它按回去:窗口本来就该
    /// 藏着。重复的 `Visible(false)` 一路到底都是空转 —— winit 只在可见性真的变了才动窗口。
    fn enforce_hidden(&mut self, ctx: &egui::Context) {
        if self.visible {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        if !self.hide_logged {
            self.hide_logged = true;
            log::info("popup kept hidden: eframe shows the root window after its first frame");
        }
    }

    fn hide(&mut self, ctx: &egui::Context) {
        if !self.visible {
            return;
        }

        // 还在屏幕外停车、用户根本没见过的窗口:位置是停车点,不是用户摆的,
        // 什么都不能往设置里写。
        let was_parked = self.parked;
        self.parked = false;

        if !was_parked {
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
        }

        // A hydration that has not finished yet was started for a popup the
        // user can no longer see; it should never act.
        self.generation += 1;
        self.hydrating = None;
        // 同上:确认框不跟着弹窗过夜。
        self.confirm_delete = None;
        // 别的路径(Esc、失焦)先藏了的话,挂起中的热键收起就作废。
        self.hotkey_hide_pending = false;
        self.visible = false;
        // 这帧的 `Visible(false)` 还要等画完、swap 完才落地,让 `ui()` 把最后
        // 一份内容补画进这一帧(见 `draw_final_frame`),别让默认清屏色闪出来。
        self.just_hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));

        // 收起之后窗口还在,趁它藏着把下次要用的尺寸摆好:停车那一帧就不必再
        // 改尺寸(改尺寸会重建表面,把刚画好的内容丢掉)。位置不用管——下次
        // 显示时窗口本来就要先停到屏幕外,停在哪儿都一样。
        let (_, _, width_px, height_px, scale) = self.target_geometry();
        let ppp = (scale as f32).max(0.25);
        self.apply_size(ctx, width_px, height_px, ppp);

        // 这一行是关掉那一刻窗口的真实尺寸(点)。"按着 shift 多选窗口自己变大"
        // 要和"从来没变过"区分开:拿它和显示时的目标尺寸一比就知道是不是有人
        // 在会话中途改了窗口大小(以及是哪条路径干的——缩放那两条各有日志)。
        match ctx.input(|i| i.viewport().outer_rect) {
            Some(outer) => log::info(&format!(
                "egui popup hidden; window was {:.0}x{:.0} points",
                outer.width(),
                outer.height()
            )),
            None => log::info("egui popup hidden"),
        }
    }

    /// 收起弹窗,并且只在**窗口真的藏起来之后**才注入 Ctrl+V。
    ///
    /// `hide()` 只是把 `Visible(false)` 排进队列,egui 在帧末才落地;而
    /// `paste.rs` 等的正是"焦点离开我们"。在同一帧里同步调用的话,弹窗在整个
    /// 等待期间都还可见(还压在最上面),等待循环看不到焦点换手,于是每次都
    /// 白等满 300ms,最后靠 `set_foreground` 硬抢前台再把键打进去。换一条线程
    /// 就不一样了:UI 线程能把这一帧跑完、隐藏落地,等待循环立刻就能退出来。
    fn paste_after_hide(&mut self, ctx: &egui::Context, payload: ClipPayload) {
        let target = self.target;
        self.hide(ctx);
        std::thread::spawn(move || crate::paste::paste_back(target, &payload));
    }

    fn quit(&mut self, ctx: &egui::Context) {
        log::info("quit requested; closing the egui loop");
        self.visible = false;
        self.parked = false;
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

        let newest = items.len().saturating_sub(1);
        self.items = items;
        self.point_at(newest);
        // The view follows the newest row on every refill, the way the legacy
        // list's LB_SETTOPINDEX did after each keystroke.
        self.scroll_to_newest = true;
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

    /// 右键菜单点在 `index` 行上之后的选择:和旧弹窗一样,点在已选中的行里
    /// 保留整个多选(复制/删除是对整个选区的操作),点在外面才把选择挪过来。
    /// 焦点行总是跟到点的这一行,粘贴/编辑/固定作用的就是它。
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

    /// 方向键/翻页把 caret 挪出可见区时,把列表跟着挪过去。
    ///
    /// 旧 listbox 的 `LB_SETCURSEL` 自带"把光标带进视野",egui 的 ScrollArea
    /// 不会:少了这一步 caret 会一路走出屏幕,看着像卡住,而 Enter 粘的还是
    /// 那行看不见的记录。
    fn scroll_caret_into_view(&mut self) {
        let Some(rect) = self.list_rect else {
            return;
        };
        if let Some(offset) = caret_offset(self.caret, self.list_offset, rect.height()) {
            self.scroll_to_newest = false;
            self.pending_offset = Some(offset);
        }
    }

    fn select_all(&mut self) {
        // The list is bottom-up: "everything" is the range 0..=last.
        self.selected = (0..self.items.len()).collect();
        self.caret = self.items.len().saturating_sub(1);
        self.anchor = 0;
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

    /// Starts reading `.bin` payloads on a worker thread. The result comes
    /// back through `hydrate_rx`; Esc, a click elsewhere or a newer request
    /// all bump the generation, and a stale result is dropped when it lands.
    ///
    /// A multi-row copy joins the texts in the worker — one job, one result,
    /// no matter how many cloud placeholders the selection hides.
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
    fn commit(&mut self, ctx: &egui::Context) {
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
            self.hide(ctx);
            return;
        };

        self.paste_after_hide(ctx, payload);
    }

    /// Ctrl+C: copy without pasting. One row keeps its own type (images copy
    /// as images); several rows join into one text block, images left out —
    /// the same rule `join_payloads` has always implemented. Blob-bearing
    /// rows go through the hydrate worker either way.
    ///
    /// 复制成功就把弹窗收起来(旧弹窗两条路径都是 copy 完 hide);写剪贴板失败
    /// 则留在原地,不然用户连重试的机会都没有。
    fn copy_selected(&mut self, ctx: &egui::Context) {
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
        self.hide(ctx);
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

    /// Hands one clip's text to the editor dialog, holding the popup up the way
    /// a message box does. Its closing comes back through `App::ui` next frame.
    fn open_editor(&mut self, stem: &str, payload: &ClipPayload) {
        let ClipPayload::Text(text) = payload else {
            log::warn("edit is text-only; refusing a non-text payload");
            return;
        };

        self.modal_open = true;
        self.dialogs.open_edit(stem, text);
    }

    /// Ctrl+P: pin or unpin the caret row. Pinning moves the row to the top,
    /// so follow the item rather than the index.
    fn toggle_pin(&mut self) {
        let Some(item) = self.items.get(self.caret) else {
            return;
        };
        let (stem, pinned) = (item.stem.clone(), item.pinned);

        if !self.store.set_pinned(&stem, !pinned) {
            return;
        }

        self.refill();
        if let Some(position) = self.items.iter().position(|item| item.stem == stem) {
            self.point_at(position);
        }
    }

    /// Delete: everything the selection covers. The confirm used to be a system
    /// MessageBox at this point; now it is a pending state the frame draws as an
    /// in-window egui modal, so the warning is dark like the rest of the popup.
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

    /// The delete confirmation, drawn over everything else in this window.
    ///
    /// Esc, the backdrop and 取消 throw it away; Enter and 删除 run it — the old
    /// MessageBox made its first button the default, and that button was Yes.
    /// Esc and Enter arrive as flags because the frame consumed them before the
    /// search box could.
    fn draw_delete_confirm(&mut self, ctx: &egui::Context, entered: bool, escaped: bool) {
        let Some(stems) = self.confirm_delete.clone() else {
            return;
        };

        let count = stems.len();
        let question = format!(
            "删除选中的 {count} 条记录？\n\n\
             同步目录里的记录会一起删掉，其他机器同步之后也会跟着消失，删了找不回来。\n\n\
             别的机器当月那份只能先记个「已删」的空标记（一样马上看不见），由那台机器自己清。"
        );

        let mut confirmed = false;
        let mut cancelled = false;

        let modal = egui::Modal::new(egui::Id::new("delete_confirm")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.label(egui::RichText::new(question).size(13.0).color(COLOR_TEXT));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("删除").clicked() {
                    confirmed = true;
                }
                if ui.button("取消").clicked() {
                    cancelled = true;
                }
            });
        });

        // 遮罩上的点击(点到框外)也是取消,和系统确认框一个意思。
        if modal.should_close() || escaped {
            cancelled = true;
        }
        if entered {
            confirmed = true;
        }

        if confirmed {
            self.confirm_delete = None;
            let caret = self.caret;
            let (deleted, marked) = self.store.delete_selected(&stems);
            log::info(&format!("{deleted} clip(s) deleted by hand, {marked} tombstoned"));

            // 旧弹窗删完把光标留给顶替那一行的位置(而不是跳回最新),屏幕上
            // 不跳走:重灌后把光标夹回范围内,并把它滚回视野中间。
            self.refill();
            let index = caret.min(self.items.len().saturating_sub(1));
            self.point_at(index);
            self.scroll_to_newest = false;
            let half = self.list_rect.map_or(0.0, |rect| rect.height()) / 2.0;
            self.pending_offset =
                Some((index as f32 * ROW_HEIGHT - half + ROW_HEIGHT / 2.0).max(0.0));
        } else if cancelled {
            self.confirm_delete = None;
        }
    }

    /// 滚轮落在搜索条/机器条上时列表也跟着翻——旧弹窗的约定,那两条不在
    /// ScrollArea 里,滚轮不会被它消费,所以在这里把增量加到列表偏移上。
    /// 口径照 ScrollArea 自己:`offset -= delta`,向下滚(delta < 0)偏移变大。
    fn forward_wheel_to_list(&mut self, ctx: &egui::Context) {
        let Some(pointer) = ctx.input(|i| i.pointer.hover_pos()) else {
            return;
        };
        if self.list_rect.is_some_and(|rect| rect.contains(pointer)) {
            return; // 列表自己的滚动,别算两遍
        }

        let delta = ctx.input(|i| i.smooth_scroll_delta.y);
        if delta == 0.0 {
            return;
        }
        self.pending_offset = Some((self.list_offset - delta).max(0.0));
    }

    /// 拖动边缘缩放。
    ///
    /// winit 的无边框窗口把客户区铺满整窗(WM_NCCALCSIZE),原生缩放边框因此
    /// 不存在——旧弹窗当年靠自己答 WM_NCHITTEST 补的就是这一手,这里换成
    /// 认边缘 + `BeginResize`。指针压在带上返回 true,面板的空白拖动让位。
    fn handle_resize(&self, ctx: &egui::Context) -> bool {
        let rect = ctx.viewport_rect();
        let Some(position) = ctx.input(|i| i.pointer.hover_pos()) else {
            return false;
        };
        let Some(direction) = resize_grip(position, rect) else {
            return false;
        };

        if !rect.contains(position) {
            // 按住不放时 egui 也会报窗口外的坐标,而边缘判断只有上界没有下界:
            // 窗外那半边会被算成左/上缩放带。
            return false;
        }
        ctx.set_cursor_icon(cursor_for(direction));
        if ctx.input(|i| i.pointer.primary_pressed()) {
            // "多选时窗口自己变大"要靠这行定位:是不是这条路径拖的窗口。
            log::info(&format!(
                "resize grip engaged at {:.0},{:.0}",
                position.x, position.y
            ));
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
        true
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // Ctrl+C 认事件不认按键:egui-winit 在进队列之前就把这个组合键翻成
        // `Event::Copy` 并且 return,`Key::C` 的按键事件根本不会产生——按
        // `key_pressed(Key::C)` 找它永远找不到。顺手把事件从队列里摘掉,搜索
        // 框就不会再把选中的搜索文字也复制一遍(旧弹窗把这个组合键从搜索框
        // 手里抢走,做的是同一件事)。
        let copy_command = ctx.input_mut(|i| {
            let pressed = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            i.events.retain(|e| !matches!(e, egui::Event::Copy));
            pressed
        });

        let (esc, enter, up, down, page_up, page_down, shift, delete, ctrl_p, ctrl_e, ctrl_a, ctrl_tab) =
            ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::Escape),
                    i.key_pressed(egui::Key::Enter),
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::PageUp),
                    i.key_pressed(egui::Key::PageDown),
                    i.modifiers.shift,
                    i.key_pressed(egui::Key::Delete),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::P),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::E),
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
        // 一页 = 实际可见行数再留一行重叠(上一帧 show_rows 量出来的),与旧
        // 弹窗的 page_rows 同一约定:跳走的那页永远带着来的那行。
        let page = self.visible_rows.saturating_sub(1).max(1) as isize;
        if up {
            self.move_selection(-1, shift);
        }
        if down {
            self.move_selection(1, shift);
        }
        if page_up {
            self.move_selection(-page, shift);
        }
        if page_down {
            self.move_selection(page, shift);
        }
        // 跟着滚的只有"键盘把 caret 挪了"这一种。每帧无条件跑的话滚轮会被它按住:
        // 往上滚到选中那行贴住视图底边,再滚就被它每帧拽回来——看着就是"滚不动了"
        // (它本来也只该管方向键/翻页,旧 listbox 的 LB_SETCURSEL 就是这个范围)。
        if up || down || page_up || page_down {
            self.scroll_caret_into_view();
        }
        // Delete 在搜索框里是删字,只有焦点不在搜索框上才是删记录——旧弹窗的
        // 两个窗口过程也是这么分的。
        if delete && !self.search_focused {
            self.delete_selected();
        }
        if ctrl_p {
            self.toggle_pin();
        }
        if copy_command {
            self.copy_selected(ctx);
        }
        if ctrl_e {
            self.edit_selected();
        }
        if ctrl_a && !self.search_focused {
            self.select_all();
        }
        if ctrl_tab {
            self.cycle_tab(if shift { -1 } else { 1 });
        }
    }

    /// Background drag on the panels' empty space, the way the legacy popup's
    /// background `WM_LBUTTONDOWN` dragged the window.
    fn start_drag_on_background(&self, ui: &mut egui::Ui, ctx: &egui::Context, id: &str) {
        // 边缘那一圈留给缩放,不抢成拖动。
        if self.grip_active {
            return;
        }

        let background = ui.interact(
            ui.max_rect(),
            egui::Id::new(id),
            egui::Sense::drag(),
        );
        if background.drag_started() {
            // 同上:靠这行区分"窗口变大"到底是缩放还是拖动。
            log::info("background drag: the popup moves with the pointer");
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

        let selected = self.selected.contains(&index);

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ROW_HEIGHT),
            egui::Sense::click_and_drag(),
        );

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

        // 旧弹窗的列表键:普通点 = 单选,Shift 点 = 从锚点扩选,Ctrl 点 = 原地
        // 切换选中。按住拖动走同一套"按下"语义,然后从锚点连到指针那一行——行
        // 必须自己声明 drag 把手势吃掉:交给列表背景就是"拖着移动窗口",用户
        // 看到的正是"多选的时候窗口自己动/变大"。
        let (shift, ctrl) = ui.input(|i| (i.modifiers.shift, i.modifiers.ctrl));
        if response.clicked() {
            log::info(&format!("row {index} clicked (shift={shift}, ctrl={ctrl})"));
            self.press_row(index, shift, ctrl);
        } else if response.drag_started() {
            log::info(&format!("drag-select from row {index}"));
            self.drag_from = Some(index);
            self.press_row(index, shift, ctrl);
        } else if self.drag_from.is_some_and(|from| from != index)
            && ui.input(|i| i.pointer.is_decidedly_dragging())
            && response.contains_pointer()
        {
            self.extend_selection(index, ctrl);
        }
        if response.drag_stopped() {
            self.drag_from = None;
        }

        let mut action: Option<RowAction> = None;
        response.context_menu(|ui| {
            if ui.button("粘贴（Enter）").clicked() {
                action = Some(RowAction::Paste);
            }
            if ui.button("复制（Ctrl+C）").clicked() {
                action = Some(RowAction::Copy);
            }
            // 与旧菜单同一条规则:只有文本、且归本机当月可写时才可编辑。
            let can_edit = self.store.can_edit(&stem);
            if ui
                .add_enabled(can_edit, egui::Button::new("编辑（Ctrl+E）"))
                .clicked()
            {
                action = Some(RowAction::Edit);
            }
            let pin_label = if pinned { "取消固定（Ctrl+P）" } else { "固定（Ctrl+P）" };
            if ui.button(pin_label).clicked() {
                action = Some(RowAction::Pin);
            }
            ui.separator();
            if ui.button("删除（Delete）").clicked() {
                action = Some(RowAction::Delete);
            }
            ui.separator();
            if ui.button("全选（Ctrl+A）").clicked() {
                action = Some(RowAction::SelectAll);
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
                self.point_at_menu_row(index);
                let ctx = ui.ctx().clone();
                self.commit(&ctx);
            }
            Some(RowAction::Copy) => {
                self.point_at_menu_row(index);
                let ctx = ui.ctx().clone();
                self.copy_selected(&ctx);
            }
            Some(RowAction::Edit) => {
                self.point_at_menu_row(index);
                self.edit_selected();
            }
            Some(RowAction::Pin) => {
                self.point_at_menu_row(index);
                self.toggle_pin();
            }
            Some(RowAction::Delete) => {
                self.point_at_menu_row(index);
                self.delete_selected();
            }
            Some(RowAction::SelectAll) => {
                self.select_all();
            }
            None => {}
        }
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

impl eframe::App for App {
    /// Runs for a hidden window too — platform events, thumbnails and
    /// hydration results cannot wait here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);
        // 热键收起在等组合键松手:松了(或超时)就补上那一步藏。
        if self.hotkey_hide_pending
            && (hotkey_keys_all_up() || Instant::now() >= self.hotkey_hide_deadline)
        {
            self.hotkey_hide_pending = false;
            self.hide(ctx);
        }
        self.drain_workers(ctx);
        self.prepare_startup_geometry(ctx);
        self.enforce_hidden(ctx);
        self.keep_repainting_just_after_show(ctx);

        // 对话框开着时保持低频醒来:弹窗藏着的时候 eframe 可能整趟 pass 都
        // 不跑,扫描回传和输入路由都得有帧可用。每秒几帧,代价可忽略。
        if self.dialogs.any_open() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // 三个次级对话框与弹窗同一套 egui;deferred 视口在那扇窗口自己的
        // pass 里画,弹窗藏着时也照常,所以注册走在可见性早退之前。
        self.dialogs.show(&ctx, &self.store);
        // 编辑窗上一帧关掉的话,收尾从这里回话——保存了哪个 stem,或取消。
        if let Some(outcome) = self.dialogs.take_edit_outcome() {
            self.modal_open = false;
            if let crate::dialogs::EditOutcome::Saved(stem) = outcome {
                self.refill();
                if let Some(position) = self.items.iter().position(|item| item.stem == stem) {
                    self.point_at(position);
                }
            }
            if self.visible {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        // 藏着的时候 eframe 也可能照样跑这一趟:它只按 `ViewportInfo` 判断可见性,
        // 而 winit 在 Windows 上从不填那个字段。这里是双保险,别当真它不跑。
        if !self.visible {
            // 收起的那一帧别空着:`Visible(false)` 比画面晚一步落地,hide 所在的
            // 这一帧会先完整上屏,画空了就是 eframe 默认清屏色(近黑)闪一下——
            // 热键收起、失焦收起走的都是这里。补画最后一份内容,和上一帧无缝
            // 衔接;之后的空转帧窗口已经藏了,不值得再画。
            if self.just_hidden {
                self.draw_final_frame(&ctx, ui);
            }
            return;
        }

        // 停在屏幕外等首帧的窗口还没有焦点,也没有人会去点它;这一趟只是把内容
        // 画进表面,别的什么都不做。
        if self.parked {
            self.draw(&ctx, ui);
            self.unpark(&ctx);
            return;
        }

        // 失焦即收起,对应旧弹窗的 WM_ACTIVATE;刚显示的头 300ms 不检查,
        // 焦点还在路上;弹窗自己拉起的删除确认框是唯一例外。
        if !self.modal_open
            && self.shown_at.elapsed() > Duration::from_millis(300)
            && !ctx.input(|i| i.viewport().focused.unwrap_or(true))
        {
            self.hide(&ctx);
            // 这帧同样必须带内容才上屏(见 ui() 开头那条),不然失焦收起也闪黑。
            self.draw_final_frame(&ctx, ui);
            return;
        }

        // 边缘缩放先行:指针压在缩放带上时,面板的空白拖动这次让位。
        self.grip_active = self.handle_resize(&ctx);

        // 删除确认挂着的时候只有确认框里的按键算数:Esc/Enter 在这里就吃掉,
        // 免得焦点还在搜索框上时被文本框先接走;底下的列表一动不动。
        let confirming = self.confirm_delete.is_some();
        let (mut confirm_entered, mut confirm_escaped) = (false, false);
        if confirming {
            ctx.input_mut(|i| {
                // 确认框挂着的时候搜索框仍然有焦点:不把这两个事件摘掉,egui 的
                // 文本框会把它们当普通文本操作——选中的搜索文字进了剪贴板,还会
                // 给历史添一条没人要的记录。
                i.events
                    .retain(|e| !matches!(e, egui::Event::Copy | egui::Event::Cut));
                confirm_escaped = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
                confirm_entered = i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
            });
        } else {
            self.handle_keys(&ctx);
            self.forward_wheel_to_list(&ctx);
        }

        self.draw(&ctx, ui);

        // 确认框画在所有面板之上(egui 的模态自己带遮罩)。
        if confirming {
            self.draw_delete_confirm(&ctx, confirm_entered, confirm_escaped);
        }
    }
}

impl App {
    /// 收起前最后一帧的补画。`Visible(false)` 这类 viewport 命令在帧画完、swap
    /// 完之后才处理,`hide()` 所在的那一帧因此会先完整上屏:画空了,用户看到
    /// 的就是一帧 eframe 默认清屏色(近黑)再消失——"收起先黑一下"就是它。
    /// `hide()` 置 `just_hidden`,热键收起/失焦收起这类画不了内容的路径从这里
    /// 补画一次;Esc/Enter 这类在 `ui()` 中途收起的路径后面本来就有 `draw()`,
    /// 置了标记也不重复上屏——标记下一帧才消费,而那时窗口已经藏了。
    fn draw_final_frame(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        self.just_hidden = false;
        self.draw(ctx, ui);
    }

    /// 画界面本体。停车那一帧走的就是这里:内容必须先画进表面,窗口才能露面。
    fn draw(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        // 0.36 的面板都长在传入的根 Ui 上,不再接 Context。先加的贴底边:
        // 搜索条在最下,机器条在它上面。
        egui::Panel::bottom("search_bar")
            .frame(egui::Frame::default().fill(COLOR_BG).inner_margin(8.0))
            .show(ui, |ui| {
                self.start_drag_on_background(ui, ctx, "drag_search");

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
                    self.search_focused = response.has_focus();
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
                self.start_drag_on_background(ui, ctx, "drag_tabs");

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
                self.start_drag_on_background(ui, ctx, "drag_list");

                // 右上角两道短杠:旧的 Win32 弹窗用它提示"这儿能拉",位置照旧。
                // 画在早退之前,列表空着时也在。
                let rect = ui.max_rect();
                for i in 0..2 {
                    let y = rect.top() + 6.0 + i as f32 * 4.0;
                    ui.painter().line_segment(
                        [
                            egui::pos2(rect.right() - 14.0, y),
                            egui::pos2(rect.right() - 6.0, y),
                        ],
                        egui::Stroke::new(1.0, COLOR_META),
                    );
                }

                if let Some(action) = self.hydrating {
                    let hint = match action {
                        HydrateAction::Paste => "正在加载完整内容，稍后自动粘贴 …",
                        HydrateAction::Copy | HydrateAction::Edit => "正在加载完整内容 …",
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

                // 行距必须和 ROW_HEIGHT 一个口径:ScrollArea 的 show_rows 是按
                // `ROW_HEIGHT + item_spacing.y` 摆放每一行的,而这个文件里所有滚动/
                // 定位算法(scroll_caret_into_view、scroll_to_newest、删除后归位)都
                // 按 ROW_HEIGHT 算。留着默认那 3 点间距,下标 i 那一行就比算法以为
                // 的位置低 3*i 点——可见的行会被算成"在上面",于是点一下列表就往上
                // 跳一大截(点第 284 行跳 700 多点,一屏),用户看着就是"跳到别的地方、
                // 选不中"。间距归零,两个口径只剩一个。
                ui.spacing_mut().item_spacing.y = 0.0;

                let mut rows = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if self.scroll_to_newest {
                    // Content height: the area clamps it to the bottom edge.
                    rows = rows.vertical_scroll_offset(self.items.len() as f32 * ROW_HEIGHT);
                    self.scroll_to_newest = false;
                    self.pending_offset = None;
                } else if let Some(offset) = self.pending_offset.take() {
                    rows = rows.vertical_scroll_offset(offset);
                }

                let output = rows.show_rows(ui, ROW_HEIGHT, self.items.len(), |ui, range| {
                    // 翻页键按上一帧实际可见行数算。
                    self.visible_rows = range.len();
                    for index in range.clone() {
                        self.draw_row(ui, index);
                    }
                });

                // 下一帧的滚轮转发要知道列表在哪、滚到哪了。
                self.list_offset = output.state.offset.y;
                self.list_rect = Some(output.inner_rect);
            });
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

/// caret 该滚到哪个偏移:`None` = 它已经在视野里,这一帧什么都不用做。
///
/// 行高口径必须和 ScrollArea 摆放行的口径一致(见 `draw` 里那行 item_spacing):
/// 对不上时"已经在视野里"会被算成"在上面",于是每点一行列表就往上跳一大截。
fn caret_offset(caret: usize, offset: f32, view_height: f32) -> Option<f32> {
    let top = caret as f32 * ROW_HEIGHT;
    let bottom = top + ROW_HEIGHT;
    if top < offset {
        Some(top.max(0.0))
    } else if bottom > offset + view_height {
        Some((bottom - view_height).max(0.0))
    } else {
        None
    }
}

/// 停车点:目标位置的左边 `PARK_OFFSET` 物理像素处,尺寸不变。没有任何显示器
/// 会延伸到那个坐标上,所以窗口在那儿是"亮着但没人看得见"。
fn park_position(left: i32, top: i32, ppp: f32) -> egui::Pos2 {
    egui::pos2((left - PARK_OFFSET) as f32 / ppp, top as f32 / ppp)
}

/// 指针是否压在窗口边缘的缩放带上,压着的话是哪个方向。纯函数,单独测:
/// 算错一个方向就是"某条边拖不动"或者"点进去变成了缩放"。
///
/// 角落两侧同时命中,四个角在 match 里排在四条边前面。
fn resize_grip(position: egui::Pos2, rect: egui::Rect) -> Option<egui::viewport::ResizeDirection> {
    use egui::viewport::ResizeDirection as Dir;

    let left = position.x - rect.left() < RESIZE_GRIP;
    let right = rect.right() - position.x < RESIZE_GRIP;
    let top = position.y - rect.top() < RESIZE_GRIP;
    let bottom = rect.bottom() - position.y < RESIZE_GRIP;

    match (left, right, top, bottom) {
        (true, _, true, _) => Some(Dir::NorthWest),
        (_, true, true, _) => Some(Dir::NorthEast),
        (true, _, _, true) => Some(Dir::SouthWest),
        (_, true, _, true) => Some(Dir::SouthEast),
        (true, _, _, _) => Some(Dir::West),
        (_, true, _, _) => Some(Dir::East),
        (_, _, true, _) => Some(Dir::North),
        (_, _, _, true) => Some(Dir::South),
        _ => None,
    }
}

fn cursor_for(direction: egui::viewport::ResizeDirection) -> egui::CursorIcon {
    use egui::viewport::ResizeDirection as Dir;

    match direction {
        Dir::North | Dir::South => egui::CursorIcon::ResizeVertical,
        Dir::East | Dir::West => egui::CursorIcon::ResizeHorizontal,
        Dir::NorthWest | Dir::SouthEast => egui::CursorIcon::ResizeNwSe,
        Dir::NorthEast | Dir::SouthWest => egui::CursorIcon::ResizeNeSw,
    }
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

    /// caret 已经在视野里时不能再滚。"点一下就跳到别的地方"就是这条被算错:
    /// 行高口径和 ScrollArea 的摆放口径一旦对不上,屏幕上的可见行会被当成在
    /// 视野上面,于是每点一行就往上跳 3*下标 点。
    #[test]
    fn a_visible_caret_never_scrolls_the_list() {
        let view = 505.0;
        let offset = 284.0 * ROW_HEIGHT;

        // 视图顶停在 284 行上:这一行和下一行都在视野里。
        assert_eq!(caret_offset(284, offset, view), None);
        assert_eq!(caret_offset(285, offset, view), None);

        // 顶上一行:跟着往上。
        assert_eq!(caret_offset(200, offset, view), Some(200.0 * ROW_HEIGHT));

        // 底下一行:滚到让它整个露出来为止。
        let shows_row_300 = 301.0 * ROW_HEIGHT - view;
        assert_eq!(caret_offset(300, offset, view), Some(shows_row_300));
    }

    /// 缩放带是看不见的,这是它唯一的定义:算错一个方向就是某条边拖不动,
    /// 或者点进窗口边缘却开始缩放。
    #[test]
    fn the_border_answers_with_the_side_it_is_on() {
        use egui::viewport::ResizeDirection as Dir;

        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(600.0, 440.0));

        // 四个角:两条边同时命中,靠 match 的先后分出方向。
        assert_eq!(resize_grip(egui::pos2(102.0, 52.0), rect), Some(Dir::NorthWest));
        assert_eq!(resize_grip(egui::pos2(697.0, 52.0), rect), Some(Dir::NorthEast));
        assert_eq!(resize_grip(egui::pos2(102.0, 487.0), rect), Some(Dir::SouthWest));
        assert_eq!(resize_grip(egui::pos2(697.0, 487.0), rect), Some(Dir::SouthEast));

        // 四条边,以及带外紧挨着的那一像素。
        assert_eq!(resize_grip(egui::pos2(100.0, 300.0), rect), Some(Dir::West));
        assert_eq!(resize_grip(egui::pos2(699.0, 300.0), rect), Some(Dir::East));
        assert_eq!(resize_grip(egui::pos2(400.0, 50.0), rect), Some(Dir::North));
        assert_eq!(resize_grip(egui::pos2(400.0, 489.0), rect), Some(Dir::South));
        assert_eq!(resize_grip(egui::pos2(105.0, 300.0), rect), None);
        assert_eq!(resize_grip(egui::pos2(400.0, 55.0), rect), None);

        // 窗口里头:那是列表和按钮带,点它不该缩放。
        assert_eq!(resize_grip(egui::pos2(400.0, 300.0), rect), None);
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
