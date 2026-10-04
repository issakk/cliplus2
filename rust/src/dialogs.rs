//! 三个次级对话框（设置 / 清理 / 编辑）的 egui 视口。
//!
//! 曾经是三个 Win32 手摆控件窗口：深色化靠 uxtheme 的暗开关，布局按 96 DPI
//! 逻辑像素手乘，各活各的消息循环。UI 换到 egui 之后这些必要都没了——
//! `show_viewport_deferred` 让它们变成与弹窗同一套渲染、同一套深色主题、
//! 同一套字体的额外视口，主循环照旧只有一条。控件的深浅、字号、间距从此
//! 不再是本模块的事。
//!
//! 线程：视口是 deferred 的，闭包被 egui 持有、在那扇窗口自己的 pass 里跑
//! （同一 UI 线程），所以状态都住在 `Arc<Mutex>` 里跟着闭包走。清理的两段
//! 后台线程（扫描/执行）经 mpsc 回传结果并 `request_repaint` 叫醒主循环，与
//! 缩略图 worker 同款；一次「扫描-确认-执行」全程由 `CLEANUP_IN_FLIGHT` 守
//! 着，窗口关了再开也不许并行两趟。热键录制期间照旧走 suspend/apply 的平台
//! 命令，录制状态的逐帧变化就是挂起与恢复的全部依据。
//!
//! 设置窗口字号（%）旋钮随这次迁移移除：egui 按显示器 DPI 原生渲染，没有
//! 按视口缩放的 API，而弹窗是固定密度列表不能跟着缩；原生窗口时代那个旋钮
//! 补偿的是 Win32 系统控件 9pt 字太小的问题，egui 的默认字号已经不比它小。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::log;
use crate::settings::{self, Hotkey};
use crate::store::{BinPreview, BinScan, BinScope, DupeScan, Store};
use crate::win;

use crate::egui_app::{COLOR_BG, COLOR_INPUT_BG, COLOR_META, COLOR_TEXT};

/// 保存出错时的提示色:够红,在深底上不刺。
const COLOR_ERROR: egui::Color32 = egui::Color32::from_rgb(0xE0, 0x6C, 0x6C);

/// 视口标题。FindWindow 靠它们找窗口补 DWM 深色标题栏,标题换了要跟着改。
const SETTINGS_TITLE: &str = "ClipPlus 设置";
const CLEANUP_TITLE: &str = "ClipPlus 清理";
const EDIT_TITLE: &str = "ClipPlus 编辑";

/// 打开时的点尺寸(96 DPI 逻辑像素,和 egui 的 point 同一口径)。高度只是
/// 开窗首帧的暂定值:那一帧就会按内容的自然高度收口(见 `Opening::fit_height`)。
const SETTINGS_SIZE: [f32; 2] = [640.0, 310.0];
/// 设置面板四周的留白。收口算客户区高度时要把上下两份加回去。
const SETTINGS_MARGIN: f32 = 14.0;
const CLEANUP_SIZE: [f32; 2] = [620.0, 340.0];
/// 清理面板四周的留白,同上。
const CLEANUP_MARGIN: f32 = 16.0;
const EDIT_SIZE: [f32; 2] = [560.0, 360.0];
const EDIT_MIN_SIZE: [f32; 2] = [380.0, 240.0];

/// 一次「扫描-确认-执行」在途的守卫。按钮置灰是常态路径,这里是它背后的
/// 那道闩:窗口关了重开,按钮回来了,清扫也不能并行。
static CLEANUP_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn claim_in_flight() -> bool {
    !CLEANUP_IN_FLIGHT.swap(true, Ordering::SeqCst)
}

fn release_in_flight() {
    CLEANUP_IN_FLIGHT.store(false, Ordering::SeqCst);
}

/// 编辑结束的去处。保存了哪个 stem,或 None = 取消。
pub enum EditOutcome {
    Saved(String),
    Cancelled,
}

/// 常规锁;中毒的锁也要能开——上一帧的 panic 不该永久焊死对话框。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 三个视口加它们的共享件。App 持有,每趟 pass 在 `show` 里把开着的注册出去。
///
/// 状态都在 `Arc<Mutex>` 里,因为 deferred 视口的闭包会被 egui 持有、在那扇
/// 窗口自己的 pass 里调用——state 跟着闭包走,借不动 App。
pub struct Dialogs {
    settings: Arc<Mutex<SettingsDialog>>,
    cleanup: Arc<Mutex<CleanupDialog>>,
    edit: Arc<Mutex<EditDialog>>,
    /// 三个窗口共用的图标,从内嵌 .ico 里拆出来的 RGBA。
    icon: Option<Arc<egui::IconData>>,
}

impl Dialogs {
    pub fn new() -> Self {
        let (scan_tx, scan_rx) = mpsc::channel();
        let icon = win::window_icon_rgba().map(|(width, height, rgba)| {
            Arc::new(egui::IconData {
                width: width as u32,
                height: height as u32,
                rgba,
            })
        });

        Self {
            settings: Arc::new(Mutex::new(SettingsDialog::default())),
            cleanup: Arc::new(Mutex::new(CleanupDialog::new(scan_tx, scan_rx))),
            edit: Arc::new(Mutex::new(EditDialog::default())),
            icon,
        }
    }

    /// 把开着的对话框注册成本趟 pass 的 deferred 视口。弹窗藏着时它们也可能
    /// 开着,所以这一步必须在弹窗可见性早退之前跑。
    ///
    /// 为什么是 deferred 而不是 immediate:eframe 0.36 的后端不执行 immediate
    /// 视口回调,注册即同步等回调、等不到就 panic「user callback was never
    /// called」——点设置闪退就是它。deferred 是 0.36 唯一画得出来的路。
    pub fn show(&self, ctx: &egui::Context, store: &Arc<Store>) {
        // egui 建窗拿不到 hwnd,DWM 深色标题栏只能开窗之后按标题找。
        if lock(&self.settings).opening.open {
            if !lock(&self.settings).opening.themed && win::dark_title_bar_titled(SETTINGS_TITLE) {
                lock(&self.settings).opening.themed = true;
            }
            let builder = lock(&self.settings).builder(self.icon.clone());
            let settings = Arc::clone(&self.settings);
            let cleanup = Arc::clone(&self.cleanup);
            ctx.show_viewport_deferred(settings_viewport(), builder, move |ui, _class| {
                let mut settings = lock(&settings);
                let mut cleanup = lock(&cleanup);
                settings.ui(ui, &mut cleanup);
            });
        }

        if lock(&self.cleanup).opening.open {
            if !lock(&self.cleanup).opening.themed && win::dark_title_bar_titled(CLEANUP_TITLE) {
                lock(&self.cleanup).opening.themed = true;
            }
            let builder = lock(&self.cleanup).builder(self.icon.clone());
            let cleanup = Arc::clone(&self.cleanup);
            let store = Arc::clone(store);
            ctx.show_viewport_deferred(cleanup_viewport(), builder, move |ui, _class| {
                let mut cleanup = lock(&cleanup);
                let ctx = ui.ctx().clone();
                cleanup.ui(ui, &ctx, &store);
            });
        }

        if lock(&self.edit).opening.open {
            if !lock(&self.edit).opening.themed && win::dark_title_bar_titled(EDIT_TITLE) {
                lock(&self.edit).opening.themed = true;
            }
            let builder = lock(&self.edit).builder(self.icon.clone());
            let edit = Arc::clone(&self.edit);
            let store = Arc::clone(store);
            ctx.show_viewport_deferred(edit_viewport(), builder, move |ui, _class| {
                let mut edit = lock(&edit);
                let outcome = edit.ui(ui, &store);
                if outcome.is_some() {
                    edit.opening.open = false;
                    edit.finished = outcome;
                }
            });
        }
    }

    pub fn any_open(&self) -> bool {
        lock(&self.settings).opening.open
            || lock(&self.cleanup).opening.open
            || lock(&self.edit).opening.open
    }

    /// 编辑窗的收尾:上一帧里关掉的话,这里取走(保存了哪个 stem,或取消)。
    pub fn take_edit_outcome(&self) -> Option<EditOutcome> {
        lock(&self.edit).finished.take()
    }

    /// 托盘「设置…」:按当前设置填好每个字段再露面。
    pub fn open_settings(&self) {
        lock(&self.settings).open_dialog();
    }

    /// 弹窗行菜单的「编辑」:拿着 stem 和文本开门。
    pub fn open_edit(&self, stem: &str, text: &str) {
        lock(&self.edit).open_dialog(stem, text);
    }
}

// ----------------------------------------------------------------- 公共小件

/// 把一个以点为单位的窗口摆到光标所在显示器上:横向居中,纵向在留白的三分
/// 之一处——原生窗口时代的摆位。返回 `with_position` 要的点坐标。
fn placed_points(
    cursor: win::POINT,
    area: &win::RECT,
    width_pt: f32,
    height_pt: f32,
) -> egui::Pos2 {
    let scale = (win::dpi_at(cursor) as f64 / 96.0).max(0.25);
    let x =
        (area.left as f64 + (area.right - area.left) as f64 / 2.0) / scale - width_pt as f64 / 2.0;
    let y = (area.top as f64 + ((area.bottom - area.top) as f64 - height_pt as f64 * scale) / 3.0)
        / scale;
    egui::pos2(x as f32, y as f32)
}

/// 视口几何的公共部分:标题、图标、开窗时的位置尺寸(只发一次,之后让
/// winit 自己管,标题栏拖动才有效)。
#[derive(Default)]
struct Opening {
    open: bool,
    /// 首帧之后置位:position/inner_size 只进 builder 一次。
    placed: bool,
    /// DWM 深色标题栏设过了就不用再 FindWindow。
    themed: bool,
    position: Option<egui::Pos2>,
    /// 已按哪一档内容高度收过口。None = 这扇窗还没量过。
    fitted_height: Option<f32>,
}

impl Opening {
    fn builder(
        &mut self,
        title: &str,
        icon: Option<Arc<egui::IconData>>,
        size: [f32; 2],
        min_size: Option<[f32; 2]>,
    ) -> egui::ViewportBuilder {
        let mut builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_resizable(true);
        if let Some(icon) = icon {
            builder = builder.with_icon(icon);
        }
        if !self.placed {
            self.placed = true;
            // 每次都是一扇新窗,上一扇的收口不作数。
            self.fitted_height = None;
            builder = builder.with_inner_size(size);
            if let Some(min_size) = min_size {
                builder = builder.with_min_inner_size(min_size);
            }
            if let Some(position) = self.position {
                builder = builder.with_position(position);
            }
        }
        builder
    }

    /// 开窗时定一次位。没有显示器信息可依就交给系统默认。
    fn place_on_cursor_monitor(&mut self, width_pt: f32, height_pt: f32) {
        let cursor = win::cursor_position();
        let area = win::work_area_at(cursor);
        self.position = Some(placed_points(cursor, &area, width_pt, height_pt));
    }

    /// 窗口高度跟着内容走:量到一份和上次收过的不一样(差半点以上)的内容
    /// 高度,就把客户区收到正好。每帧都发不行——内容没变的帧会白白重建
    /// GL 表面,用户手动拖出的尺寸也会被拽回去;宽度假手拖,只收高度。
    fn fit_height(
        &mut self,
        ctx: &egui::Context,
        fallback_width: f32,
        content_height: f32,
        margin: f32,
    ) {
        if self
            .fitted_height
            .map_or(false, |fitted| (fitted - content_height).abs() < 0.5)
        {
            return;
        }
        self.fitted_height = Some(content_height);
        let width = ctx
            .input(|i| i.viewport().inner_rect)
            .map_or(fallback_width, |rect| rect.width());
        // 显式指名本视口:这是 deferred 闭包,不想赌「当前视口」的语义。
        ctx.send_viewport_cmd_to(ctx.viewport_id(), {
            egui::ViewportCommand::InnerSize(egui::vec2(width, content_height + 2.0 * margin))
        });
    }
}

// ---------------------------------------------------------------- 设置对话框

#[derive(Default)]
struct SettingsDialog {
    opening: Opening,
    hotkey: String,
    sync_root: String,
    retention: String,
    max_blob_mb: String,
    inline_limit: String,
    rescan: String,
    capture_text: bool,
    capture_images: bool,
    capture_files: bool,
    write_blobs: bool,
    /// 热键框处于录制中:任何组合键入框,点别处或 Esc 收工。
    recording: bool,
    error: Option<String>,
}

impl SettingsDialog {
    fn open_dialog(&mut self) {
        let Some(current) = crate::current_settings() else {
            return;
        };

        self.hotkey = current.hotkey.clone();
        self.sync_root = current.sync_root_override.clone().unwrap_or_default();
        self.retention = current.retention_days.to_string();
        self.max_blob_mb = (current.max_blob_bytes / (1024 * 1024)).to_string();
        self.inline_limit = current.inline_text_limit.to_string();
        self.rescan = current.rescan_seconds.to_string();
        self.capture_text = current.capture_text;
        self.capture_images = current.capture_images;
        self.capture_files = current.capture_files;
        self.write_blobs = current.write_blobs;
        self.recording = false;
        self.error = None;
        self.opening.placed = false;
        self.opening.themed = false;
        self.opening
            .place_on_cursor_monitor(SETTINGS_SIZE[0], SETTINGS_SIZE[1]);
        self.opening.open = true;
    }

    fn close(&mut self) {
        self.opening.open = false;
        self.recording = false;
        self.error = None;
    }

    fn builder(&mut self, icon: Option<Arc<egui::IconData>>) -> egui::ViewportBuilder {
        self.opening
            .builder(SETTINGS_TITLE, icon, SETTINGS_SIZE, None)
    }

    fn ui(&mut self, ui: &mut egui::Ui, cleanup: &mut CleanupDialog) {
        let was_recording = self.recording;
        let mut content_height = 0.0;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(COLOR_BG)
                    .inner_margin(SETTINGS_MARGIN),
            )
            .show(ui, |ui| {
                // 数字框只认数字(ES_NUMBER 的等价物),范围校验留到存盘。
                for text in [
                    &mut self.retention,
                    &mut self.max_blob_mb,
                    &mut self.inline_limit,
                    &mut self.rescan,
                ] {
                    text.retain(|c| c.is_ascii_digit());
                }

                egui::Grid::new("settings_rows")
                    .num_columns(2)
                    .spacing([16.0, 9.0])
                    .show(ui, |ui| {
                        ui.label("热键（点这里按组合键）");
                        self.hotkey_widget(ui);
                        ui.end_row();

                        ui.label("同步目录（留空 = 自动）");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.sync_root).desired_width(300.0),
                        );
                        ui.end_row();

                        ui.label("保留天数（0 = 不清理）");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.retention).desired_width(80.0),
                        );
                        ui.end_row();

                        ui.label("单条上限（MB）");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.max_blob_mb).desired_width(80.0),
                        );
                        ui.end_row();

                        ui.label("内联文本上限（字符）");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.inline_limit)
                                .desired_width(80.0),
                        );
                        ui.end_row();

                        ui.label("重扫间隔（秒）");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.rescan).desired_width(80.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.capture_text, "记录文本");
                    ui.checkbox(&mut self.capture_images, "记录图片");
                    ui.checkbox(&mut self.capture_files, "记录文件");
                    ui.checkbox(&mut self.write_blobs, "超限写 .bin");
                });

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "热键和记录开关立即生效；勾掉「超限写 .bin」= 超长文本和图片直接丢弃。其余项需重启。",
                    )
                    .size(12.0)
                    .color(COLOR_META),
                );

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("清理…").clicked() {
                        cleanup.open_dialog();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("取消").clicked() {
                            self.close();
                        }
                        if ui.button("保存").clicked() {
                            self.save();
                        }
                    });
                });

                if let Some(reason) = &self.error {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(reason).size(12.0).color(COLOR_ERROR));
                }

                // 量在内容末尾,拿到的才是整份内容的自然高度。
                content_height = ui.min_size().y;
            });

        // 错误行是设置窗里唯一会变高度的内容,出现/消失由收口跟着长/缩。
        self.opening
            .fit_height(ui.ctx(), SETTINGS_SIZE[0], content_height, SETTINGS_MARGIN);

        // 录制状态的变化就是挂起/恢复热键的全部依据,放在面板外统一看。
        if self.recording != was_recording {
            if self.recording {
                crate::suspend_hotkey();
            } else {
                crate::apply_hotkey();
            }
        }
    }

    /// 热键框:长得像输入框的按钮,点进去进入录制。
    fn hotkey_widget(&mut self, ui: &mut egui::Ui) {
        let label = if self.recording {
            "按下组合键 …（Esc 取消）".to_string()
        } else {
            self.hotkey.clone()
        };
        let response = ui.add_sized(
            [300.0, 26.0],
            egui::Button::new(egui::RichText::new(label).color(if self.recording {
                COLOR_META
            } else {
                COLOR_TEXT
            }))
            .fill(COLOR_INPUT_BG),
        );

        if response.clicked() {
            self.recording = true;
        }

        if !self.recording {
            return;
        }

        // Esc 把保存值放回来,一次误按可撤;组合键入框;点到别处收工。
        let mut escape = false;
        ui.input_mut(|input| {
            escape = input.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
        });
        if escape {
            if let Some(current) = crate::current_settings() {
                self.hotkey = current.hotkey;
            }
            self.recording = false;
            return;
        }

        if let Some(vk) = ui.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    ..
                } => vk_of(*key),
                _ => None,
            })
        }) {
            // 修饰键从键盘读(egui 的 Modifiers 没有Win 键),无名键不进框——
            // 否则存下来的文本过不了 parse_hotkey 的往返。
            if let Some(hotkey) = Hotkey::new(held_modifiers(), vk) {
                log::info(&format!("hotkey field recorded {}", hotkey.text()));
                self.hotkey = hotkey.text();
            }
        }

        if ui.input(|input| input.pointer.primary_clicked()) && !response.hovered() {
            self.recording = false;
        }
    }

    fn save(&mut self) {
        let Some(mut updated) = crate::current_settings() else {
            return;
        };
        let previous = updated.clone();

        let typed = self.hotkey.trim().to_string();
        let Some(hotkey) = settings::parse_hotkey(&typed) else {
            self.error = Some(format!(
                "热键 “{typed}” 不能用。\n\n点进热键框直接按下组合键即可。需要至少一个修饰键（Ctrl/Alt/Shift/Win），\n只有 F1-F24 能单独使用——单独的字母或数字会吃掉全系统的那个键。"
            ));
            return;
        };

        // 保留旧组合的免检:重注册现在这个组合不是冲突。
        let unchanged =
            settings::parse_hotkey(&previous.hotkey).map(Hotkey::text) == Some(hotkey.text());
        if !unchanged && !combination_is_free(hotkey) {
            self.error = Some(format!(
                "“{}” 已经被别的程序占用了，换一个组合。",
                hotkey.text()
            ));
            return;
        }

        let retention = match parse_or(0, &self.retention) {
            Ok(value) => value,
            Err(reason) => {
                self.error = Some(format!("保留天数：{reason}"));
                return;
            }
        };
        let max_blob_mb = match parse_or(10, &self.max_blob_mb) {
            Ok(value) => value,
            Err(reason) => {
                self.error = Some(format!("单条上限：{reason}"));
                return;
            }
        };
        let inline_limit = match parse_or(8192, &self.inline_limit) {
            Ok(value) => value,
            Err(reason) => {
                self.error = Some(format!("内联文本上限：{reason}"));
                return;
            }
        };
        let rescan = match parse_or(60, &self.rescan) {
            Ok(value) => value,
            Err(reason) => {
                self.error = Some(format!("重扫间隔：{reason}"));
                return;
            }
        };

        if rescan < 10 {
            self.error = Some("重扫间隔不能小于 10 秒。".into());
            return;
        }
        if max_blob_mb == 0 {
            self.error = Some("单条上限不能为 0，否则什么都存不下来。".into());
            return;
        }

        let sync_root = self.sync_root.trim().to_string();

        updated.hotkey = hotkey.text();
        updated.sync_root_override = if sync_root.is_empty() {
            None
        } else {
            Some(sync_root)
        };
        updated.retention_days = retention;
        updated.max_blob_bytes = max_blob_mb as u64 * 1024 * 1024;
        updated.inline_text_limit = inline_limit as usize;
        updated.rescan_seconds = rescan as u64;
        updated.capture_text = self.capture_text;
        updated.capture_images = self.capture_images;
        updated.capture_files = self.capture_files;
        updated.write_blobs = self.write_blobs;

        if let Err(err) = updated.save() {
            self.error = Some(format!("写入 settings.json 失败：{err}"));
            return;
        }

        crate::set_settings(updated);
        crate::apply_hotkey();
        log::info("settings saved");
        self.close();
    }
}

/// 空着 = 用默认值;其余照旧逐字报给用户。
fn parse_or(default: u32, text: &str) -> Result<u32, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }

    trimmed
        .parse::<u32>()
        .map_err(|_| format!("“{trimmed}” 不是有效的非负整数"))
}

/// Registers the combination for a moment to find out whether anything else
/// already owns it. Done here so the failure is reported before the settings
/// are written, rather than afterwards by `apply_hotkey`.
fn combination_is_free(hotkey: Hotkey) -> bool {
    // A null window registers against this thread. Our own registration is
    // dropped while the field records, so this cannot collide with itself.
    if !win::register_hotkey(0, 0, hotkey.modifiers | win::MOD_NOREPEAT, hotkey.vk) {
        return false;
    }

    win::unregister_hotkey(0, 0);
    true
}

/// 修饰键从键盘现读,不积攒之前的消息——这样这个进程没注册过的组合也能录。
fn held_modifiers() -> u32 {
    fn down(vk: u32) -> bool {
        let state = unsafe { win::GetKeyState(vk as i32) };
        state < 0
    }

    let mut modifiers = 0;

    if down(win::VK_CONTROL as u32) {
        modifiers |= win::MOD_CONTROL;
    }
    if down(win::VK_MENU as u32) {
        modifiers |= win::MOD_ALT;
    }
    if down(win::VK_SHIFT as u32) {
        modifiers |= win::MOD_SHIFT;
    }
    if down(win::VK_LWIN as u32) || down(win::VK_RWIN as u32) {
        modifiers |= win::MOD_WIN;
    }

    modifiers
}

/// egui 按键 → Windows 虚拟键码。热键拼法只认字母、数字和 F1-F24,其余键
/// (包括左右修饰键本身)一概不录。egui 的 Key 不许 range pattern,而各段
/// 变体在声明序里连续,判别式区间即可——测试钉住了两端。
fn vk_of(key: egui::Key) -> Option<u32> {
    use egui::Key;
    let index = key as u32;

    let (a, z) = (Key::A as u32, Key::Z as u32);
    if (a..=z).contains(&index) {
        return Some(0x41 + index - a);
    }
    let (num0, num9) = (Key::Num0 as u32, Key::Num9 as u32);
    if (num0..=num9).contains(&index) {
        return Some(0x30 + index - num0);
    }
    let (f1, f24) = (Key::F1 as u32, Key::F24 as u32);
    if (f1..=f24).contains(&index) {
        return Some(0x70 + index - f1);
    }

    None
}

// ---------------------------------------------------------------- 清理对话框

/// 后台线程回传的东西。每条到达时都代表一次在途清扫的推进或终结。
enum ScanOutcome {
    /// .bin 扫描完成,等确认。
    Bin(BinPreview),
    /// 重复文本扫描完成,等确认。
    Dupes(DupeScan),
    /// 执行完毕,报告全文。
    Done(String),
}

/// 挂在确认框后面的扫描结果。确认框一关它就有去处:执行,或者作废。
enum PendingCleanup {
    Bin(BinPreview),
    Dupes(DupeScan),
}

impl PendingCleanup {
    /// (状态行的摘要,确认框里的全文)——原生窗口时代 set_status 和
    /// MessageBox 各吃一份的同一组字符串。
    fn texts(&self) -> (String, String) {
        match self {
            PendingCleanup::Bin(BinPreview { scan, scope_line }) => {
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
                let mut confirm = summary.clone();
                confirm.push_str("\n\n整条删除、不可恢复；固定的条目不受影响。确定清理？");
                (summary, confirm)
            }
            PendingCleanup::Dupes(scan) => {
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
                let mut confirm = summary.clone();
                confirm.push_str("\n\n确定清理？");
                (summary, confirm)
            }
        }
    }
}

struct CleanupDialog {
    opening: Opening,
    scan_tx: mpsc::Sender<ScanOutcome>,
    scan_rx: mpsc::Receiver<ScanOutcome>,
    by_time: bool,
    days: String,
    keep: String,
    /// 扫描或执行在途:两个动作按钮一起置灰,数字框照常可改。
    busy: bool,
    status: String,
    confirm: Option<PendingCleanup>,
}

impl CleanupDialog {
    fn new(scan_tx: mpsc::Sender<ScanOutcome>, scan_rx: mpsc::Receiver<ScanOutcome>) -> Self {
        Self {
            opening: Opening::default(),
            scan_tx,
            scan_rx,
            by_time: true,
            days: "30".into(),
            keep: "100".into(),
            busy: false,
            status: String::new(),
            confirm: None,
        }
    }

    /// 打开即净场:上一次的提示和确认不作数(在途的扫描结果由 drain 决定
    /// 去留),数字保留上次输入,和原生窗口一个习惯。
    fn open_dialog(&mut self) {
        if let Some(pending) = self.confirm.take() {
            drop(pending);
            release_in_flight();
        }
        self.busy = false;
        self.status.clear();
        self.opening.placed = false;
        self.opening.themed = false;
        self.opening
            .place_on_cursor_monitor(CLEANUP_SIZE[0], CLEANUP_SIZE[1]);
        self.opening.open = true;
    }

    fn close(&mut self) {
        // 确认挂着的时候关窗,那次清扫没有下文了,在途标记就地释放。
        if self.confirm.take().is_some() {
            release_in_flight();
        }
        self.opening.open = false;
    }

    fn builder(&mut self, icon: Option<Arc<egui::IconData>>) -> egui::ViewportBuilder {
        self.opening
            .builder(CLEANUP_TITLE, icon, CLEANUP_SIZE, None)
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, store: &Arc<Store>) {
        self.drain();
        let mut content_height = 0.0;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(COLOR_BG)
                    .inner_margin(CLEANUP_MARGIN),
            )
            .show(ui, |ui| {
                for text in [&mut self.days, &mut self.keep] {
                    text.retain(|c| c.is_ascii_digit());
                }

                ui.label(
                    egui::RichText::new("清理 .bin 大条目（图片、超长文本）")
                        .size(15.0)
                        .color(COLOR_TEXT),
                );
                ui.separator();
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.by_time, true, "按时间");
                    ui.add(egui::TextEdit::singleline(&mut self.days).desired_width(56.0));
                    ui.label("天以前的（0 = 全部）");
                });
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.by_time, false, "按条数");
                    ui.add(egui::TextEdit::singleline(&mut self.keep).desired_width(56.0));
                    ui.label("个，其余清理（0 = 全部）");
                });

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "只删带 .bin 的条目——图片和超长文本，整条删除，短文本和文件列表不受影响。固定的条目永远保留；别机当月的条目删不了，会跳过。",
                    )
                    .size(12.0)
                    .color(COLOR_META),
                );

                ui.add_space(10.0);
                if ui
                    .add_enabled(!self.busy, egui::Button::new("清理 .bin…"))
                    .clicked()
                {
                    self.start_bins(ctx, store);
                }

                ui.add_space(18.0);
                ui.label(
                    egui::RichText::new("清理重复文本")
                        .size(15.0)
                        .color(COLOR_TEXT),
                );
                ui.separator();
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "同一段文本存了多份时（两台机器都复制过最常见），每组保留最新一份，固定的副本也保留，其余删除。",
                    )
                    .size(12.0)
                    .color(COLOR_META),
                );

                ui.add_space(10.0);
                if ui
                    .add_enabled(!self.busy, egui::Button::new("去重…"))
                    .clicked()
                {
                    self.start_dupes(ctx, store);
                }

                ui.add_space(12.0);
                if !self.status.is_empty() {
                    ui.label(
                        egui::RichText::new(&self.status)
                            .size(12.0)
                            .color(COLOR_META),
                    );
                }

                ui.add_space(10.0);
                // Align::Min 贴住上面的内容;竖直居中会把按钮悬浮到剩余
                // 空间的中部(这一行不在 horizontal 里,没东西给它撑高)。
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if ui.button("关闭").clicked() {
                        self.close();
                    }
                });

                // 量在内容末尾,拿到的才是整份内容的自然高度。
                content_height = ui.min_size().y;
            });

        // 状态行是清理窗里唯一会变高度的内容,扫描报告多长窗口就长多高。
        self.opening
            .fit_height(ui.ctx(), CLEANUP_SIZE[0], content_height, CLEANUP_MARGIN);

        self.draw_confirm(ui, ctx, store);
    }

    /// 后台结果落账。窗口关着也照收:收了才知道该不该释放在途标记。
    fn drain(&mut self) {
        while let Ok(outcome) = self.scan_rx.try_recv() {
            match outcome {
                ScanOutcome::Done(report) => {
                    release_in_flight();
                    self.busy = false;
                    self.status = report;
                }
                ScanOutcome::Bin(preview) => {
                    self.busy = false;
                    if !self.opening.open {
                        release_in_flight();
                        continue;
                    }
                    if preview.scan.doomed.is_empty() {
                        release_in_flight();
                        let mut text = "没有可清理的 .bin 大条目。".to_string();
                        if preview.scan.skipped_live_month > 0 {
                            text.push_str(&format!(
                                "\n（另跳过 {} 条别机当月的）",
                                preview.scan.skipped_live_month
                            ));
                        }
                        self.status = text;
                        continue;
                    }
                    let pending = PendingCleanup::Bin(preview);
                    self.status = pending.texts().0;
                    self.confirm = Some(pending);
                }
                ScanOutcome::Dupes(scan) => {
                    self.busy = false;
                    if !self.opening.open {
                        release_in_flight();
                        continue;
                    }
                    if scan.doomed.is_empty() {
                        release_in_flight();
                        self.status = "没有发现重复文本。".into();
                        continue;
                    }
                    let pending = PendingCleanup::Dupes(scan);
                    self.status = pending.texts().0;
                    self.confirm = Some(pending);
                }
            }
        }
    }

    fn draw_confirm(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, store: &Arc<Store>) {
        let Some(pending) = self.confirm.take() else {
            return;
        };

        let mut confirmed = false;
        let mut cancelled = false;
        let modal = egui::Modal::new(egui::Id::new("cleanup_confirm")).show(ui.ctx(), |ui| {
            ui.set_max_width(430.0);
            ui.label(
                egui::RichText::new(pending.texts().1)
                    .size(13.0)
                    .color(COLOR_TEXT),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("确定清理").clicked() {
                    confirmed = true;
                }
                if ui.button("取消").clicked() {
                    cancelled = true;
                }
            });
        });

        // 点到框外也是取消,和系统确认框一个意思。
        if modal.should_close() {
            cancelled = true;
        }

        if confirmed {
            match pending {
                PendingCleanup::Bin(preview) => self.run_bins(ctx, store, preview.scan),
                PendingCleanup::Dupes(scan) => self.run_dupes(ctx, store, scan),
            }
        } else if cancelled {
            release_in_flight();
            self.busy = false;
            self.status = "已取消，没有改动。".into();
        } else {
            // 还开着,下一帧接着问。
            self.confirm = Some(pending);
        }
    }

    /// Which scope the radios name. Only the active row's number is read.
    fn read_scope(&self) -> Result<BinScope, String> {
        if self.by_time {
            parse_or(30, &self.days).map(BinScope::OlderThanDays)
        } else {
            parse_or(100, &self.keep).map(BinScope::KeepNewest)
        }
    }

    fn start_bins(&mut self, ctx: &egui::Context, store: &Arc<Store>) {
        let scope = match self.read_scope() {
            Ok(scope) => scope,
            Err(reason) => {
                self.status = reason;
                return;
            }
        };
        if !claim_in_flight() {
            self.status = "上一次清理还在进行，等它结束再试。".into();
            return;
        }

        self.busy = true;
        self.status = "正在扫描…".into();

        let store = Arc::clone(store);
        let tx = self.scan_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let scan = store.scan_bin_cleanup(scope);
            let preview = BinPreview {
                scan,
                scope_line: scope_line(scope),
            };
            let _ = tx.send(ScanOutcome::Bin(preview));
            ctx.request_repaint();
        });
    }

    fn start_dupes(&mut self, ctx: &egui::Context, store: &Arc<Store>) {
        if !claim_in_flight() {
            self.status = "上一次清理还在进行，等它结束再试。".into();
            return;
        }

        self.busy = true;
        self.status = "正在扫描…".into();

        let store = Arc::clone(store);
        let tx = self.scan_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let scan = store.scan_duplicates();
            let _ = tx.send(ScanOutcome::Dupes(scan));
            ctx.request_repaint();
        });
    }

    fn run_bins(&mut self, ctx: &egui::Context, store: &Arc<Store>, scan: BinScan) {
        self.status = "正在清理…".into();
        self.busy = true;

        let BinScan {
            doomed, blob_bytes, ..
        } = scan;
        let store = Arc::clone(store);
        let tx = self.scan_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let removed = store.run_bin_cleanup(doomed);
            let report = format!(
                "已清理 {removed} 个 .bin 大条目，释放约 {}。",
                human_bytes(blob_bytes)
            );
            let _ = tx.send(ScanOutcome::Done(report));
            ctx.request_repaint();
        });
    }

    fn run_dupes(&mut self, ctx: &egui::Context, store: &Arc<Store>, scan: DupeScan) {
        self.status = "正在去重…".into();
        self.busy = true;

        let store = Arc::clone(store);
        let tx = self.scan_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let (deleted, tombstoned) = store.run_duplicate_cleanup(scan.doomed);
            let report = if tombstoned > 0 {
                format!("已删除 {deleted} 条重复文本，另以墓碑提交 {tombstoned} 条（别机当月，由所属机器执行）。")
            } else {
                format!("已删除 {deleted} 条重复文本。")
            };
            let _ = tx.send(ScanOutcome::Done(report));
            ctx.request_repaint();
        });
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

// ---------------------------------------------------------------- 编辑对话框

#[derive(Default)]
struct EditDialog {
    opening: Opening,
    /// 正在编辑的行,保存的去处;None 语义不存在——编辑窗只为主窗口而开。
    stem: String,
    text: String,
    error: Option<String>,
    /// 关窗那一帧写进来,下一帧由 `Dialogs::take_edit_outcome` 取走。
    finished: Option<EditOutcome>,
}

impl EditDialog {
    fn open_dialog(&mut self, stem: &str, text: &str) {
        // Lone `\n` does not read as a line break to an editor; normalize for
        // display and save whatever the box holds afterwards. A clip saved
        // with bare `\n` line endings comes out with `\r\n` once edited — an
        // accepted consequence of editing, not a silent rewrite.
        self.text = text.replace("\r\n", "\n").replace('\n', "\r\n");
        self.stem = stem.to_string();
        self.error = None;
        self.opening.placed = false;
        self.opening.themed = false;
        self.opening
            .place_on_cursor_monitor(EDIT_SIZE[0], EDIT_SIZE[1]);
        self.opening.open = true;
    }

    fn builder(&mut self, icon: Option<Arc<egui::IconData>>) -> egui::ViewportBuilder {
        self.opening
            .builder(EDIT_TITLE, icon, EDIT_SIZE, Some(EDIT_MIN_SIZE))
    }

    fn ui(&mut self, ui: &mut egui::Ui, store: &Arc<Store>) -> Option<EditOutcome> {
        // Esc 取消、Ctrl+Enter 保存,先于文本框消费:不然 Enter 进内容,
        // Esc 也轮不到这里。
        let (mut save, mut cancel) = (false, false);
        ui.input_mut(|input| {
            if input.consume_key(egui::Modifiers::CTRL, egui::Key::Enter) {
                save = true;
            }
            if input.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                cancel = true;
            }
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(COLOR_BG).inner_margin(12.0))
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(14),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Ctrl+Enter 保存 · Esc 取消")
                            .size(12.0)
                            .color(COLOR_META),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                        if ui.button("保存").clicked() {
                            save = true;
                        }
                    });
                });
                if let Some(reason) = &self.error {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(reason).size(12.0).color(COLOR_ERROR));
                }
            });

        if cancel {
            return Some(EditOutcome::Cancelled);
        }
        if save {
            // 存失败就留着窗口:用户打的字就是要紧的东西,关掉等于扔掉。
            match store.edit_text(&self.stem, &self.text) {
                Ok(()) => {
                    log::info(&format!("saved an edit to {}", self.stem));
                    return Some(EditOutcome::Saved(self.stem.clone()));
                }
                Err(reason) => self.error = Some(reason),
            }
        }

        None
    }
}

// ---------------------------------------------------------------- 视口标识

/// 每趟 pass 现建 Id:egui 的 Id::new 不保证 const,不值得为三个常量赌一把。
fn settings_viewport() -> egui::ViewportId {
    egui::ViewportId(egui::Id::new("settings_dialog"))
}

fn cleanup_viewport() -> egui::ViewportId {
    egui::ViewportId(egui::Id::new("cleanup_dialog"))
}

fn edit_viewport() -> egui::ViewportId {
    egui::ViewportId(egui::Id::new("edit_dialog"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 录制只认热键拼法叫得出名字的键,而且拼法(0x41..)必须和
    /// `settings::key_name` 对得上,不然录下的组合存不进 settings.json。
    #[test]
    fn egui_keys_map_to_the_vk_codes_the_hotkey_parser_names() {
        assert_eq!(vk_of(egui::Key::W), Some(0x57));
        assert_eq!(vk_of(egui::Key::A), Some(0x41));
        assert_eq!(vk_of(egui::Key::Z), Some(0x5A));
        assert_eq!(vk_of(egui::Key::Num0), Some(0x30));
        assert_eq!(vk_of(egui::Key::Num9), Some(0x39));
        assert_eq!(vk_of(egui::Key::F1), Some(0x70));
        assert_eq!(vk_of(egui::Key::F24), Some(0x87));

        // 修饰键与无名键不成组合。
        assert_eq!(vk_of(egui::Key::ShiftLeft), None);
        assert_eq!(vk_of(egui::Key::ControlRight), None);
        assert_eq!(vk_of(egui::Key::SuperLeft), None);
        assert_eq!(vk_of(egui::Key::Escape), None);
        assert_eq!(vk_of(egui::Key::F25), None);
    }
}
