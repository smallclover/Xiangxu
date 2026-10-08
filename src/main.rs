//! 象胥 (Xiangxu) —— 实时屏幕翻译浮层（独立进程，不与 CloverViewer 互相干扰）。
//!
//! 架构要点：
//! - 主窗口是一个常驻置顶的小面板，用于显示译文与操作。
//! - 监控在独立后台线程中进行（xcap 截屏 -> 哈希检测 -> OCR -> 翻译），
//!   通过 mpsc 通道回传，不阻塞 UI，也不与主窗口共享任何会导致样式残留的 HWND。
//! - “框选区域”时临时把主窗口全屏化，用户拖拽选定游戏对话框，松开后换算为
//!   物理屏幕坐标，监控线程据此截取。

use eframe::egui;
use egui::ViewportCommand;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

mod capture;
mod download;
mod glossary;
mod lang;
mod monitor;
mod ocr;
mod resources;
mod state;
mod translate;

use capture::ScreenRect;
use state::{AdjustHandle, MonitorConfig, PendingFullscreenMode, TranslateResult, TranslateState};
use translate::AppMode;

fn main() -> Result<(), eframe::Error> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    let options = eframe::NativeOptions {
        // 主窗口 = 常驻无边框工具条（不可最小化、不进任务栏，杜绝
        // "最小化主窗口导致子 viewport（字幕/区域边框）被销毁"的问题——
        // egui 在 root 最小化时会关闭全部子 viewport）。
        viewport: egui::ViewportBuilder::default()
            .with_title("象胥")
            .with_inner_size([464.0, 48.0])
            .with_decorations(false)
            .with_always_on_top()
            .with_resizable(false)
            .with_taskbar(false),
        ..Default::default()
    };

    eframe::run_native(
        "象胥",
        options,
        Box::new(|cc| {
            init_fonts(&cc.egui_ctx);
            Ok(Box::new(XiangxuApp::new(&cc.egui_ctx)))
        }),
    )
}

/// 注册中文字体，避免中文显示为方块 / 乱码。
fn init_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "clover_font".to_owned(),
        Arc::new(egui::FontData::from_static(resources::APP_FONT)),
    );
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "clover_font".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// 控制台左侧导航页。
#[derive(Clone, Copy, PartialEq)]
enum Page {
    Display,
    Region,
    Translate,
    Ocr,
    Models,
    History,
    About,
}

impl Page {
    fn label(self) -> &'static str {
        match self {
            Page::Display => "显示",
            Page::Region => "区域",
            Page::Translate => "翻译",
            Page::Ocr => "识图",
            Page::Models => "模型",
            Page::History => "历史",
            Page::About => "关于",
        }
    }

    const ALL: [Page; 7] = [
        Page::Display,
        Page::Region,
        Page::Translate,
        Page::Ocr,
        Page::Models,
        Page::History,
        Page::About,
    ];
}

/// 字幕窗（独立 viewport）内容与状态：主线程每帧写入，字幕窗回调每帧读取。
#[derive(Default)]
struct SubtitleData {
    source: String,
    target: String,
    working: bool,
    monitoring: bool,
    /// 迷你态：只显示单行译文。
    mini: bool,
    attach_enabled: bool,
    attach_hwnd: isize,
    /// 字幕窗左上角相对游戏窗口左上角的偏移（points，按字幕窗自身 DPI）。
    offset: Option<(f32, f32)>,
    dragging: bool,
    /// 全屏取景（框选/调整/取窗）中：移出屏幕，避免被截进背景图。
    suppressed: bool,
}

struct XiangxuApp {
    state: TranslateState,
    config: Arc<Mutex<MonitorConfig>>,
    rx: mpsc::Receiver<TranslateResult>,
    tx: mpsc::Sender<TranslateResult>,
    /// 模型下载进度（设置面板的“模型资源”区读取）。
    dl_progress: Arc<Mutex<download::DownloadProgress>>,
    /// 当前所在的导航页（纯 UI 状态）。
    page: Page,
    /// 独立字幕窗：数据 + 显隐 + viewport id（独立 HWND，不与主窗口共享）。
    sub_data: Arc<Mutex<SubtitleData>>,
    sub_visible: bool,
    sub_id: egui::ViewportId,
    /// 主窗口形态：true=工具条（默认），false=悬浮球。
    bar_mode: bool,
    /// 设置窗（独立 viewport，点工具条 ⚙ 弹出）。
    settings_open: bool,
    settings_id: egui::ViewportId,
    /// 贴边缩进状态：贴住哪条边 + 是否处于缩进隐藏态。
    dock: Option<DockSide>,
    dock_hidden: bool,
    /// 是否正处于 Windows 原生窗口拖拽（StartDrag / SC_MOVE 模态循环）中。
    /// 松手后据此恢复贴边检测。
    os_dragging: bool,
}

/// 贴边方向（工具条/悬浮球拖到屏幕边缘时缩进）。
#[derive(Clone, Copy, PartialEq)]
enum DockSide {
    Left,
    Right,
}

/// 主窗口两种形态的尺寸（points）。
const BAR_SIZE: egui::Vec2 = egui::vec2(412.0, 48.0);
const BALL_SIZE: egui::Vec2 = egui::vec2(56.0, 56.0);

impl XiangxuApp {
    fn new(ctx: &egui::Context) -> Self {
        // 启动即清理上一次（可能崩溃）残留的 llama-server 进程，回收 CPU/显存，
        // 避免软件一打开就因残留的 3B 视觉模型仍在加载而整体卡顿。
        translate::cleanup_orphan_servers();
        // 全局浅色主题：工具条 / 设置窗是浅色浮层。
        // 字幕窗、区域边框、全屏取景全部使用显式配色（深底白字/高亮色），不受影响。
        ctx.set_visuals(egui::Visuals::light());
        let config = Arc::new(Mutex::new(MonitorConfig::default()));
        let (tx, rx) = mpsc::channel();
        let cfg = Arc::clone(&config);
        let ctx2 = ctx.clone();
        monitor::spawn_monitor(cfg, ctx2, tx.clone());
        Self {
            state: TranslateState::default(),
            config,
            rx,
            tx,
            dl_progress: Arc::new(Mutex::new(download::DownloadProgress::new())),
            page: Page::Display,
            sub_data: Arc::new(Mutex::new(SubtitleData::default())),
            sub_visible: false,
            sub_id: egui::ViewportId::from_hash_of("xiangxu_subtitle"),
            bar_mode: true,
            settings_open: false,
            settings_id: egui::ViewportId::from_hash_of("xiangxu_settings"),
            dock: None,
            dock_hidden: false,
            os_dragging: false,
        }
    }

    /// 将当前 UI 状态同步到后台监控线程读取的配置。
    fn sync_config(&self) {
        if let Ok(mut c) = self.config.lock() {
            c.running = self.state.monitoring;
            c.region = self.state.region;
            c.source_lang = self.state.source_lang;
            c.target_lang = self.state.target_lang;
            c.translator = self.state.translator.clone();
            c.attach_hwnd = self.state.attach_hwnd;
            c.attach_region_offset = self.state.attach_region_offset;
        }
    }

    fn toggle_monitoring(&mut self) {
        if self.state.region.is_none() {
            self.state.last_error = Some("请先点「框选区域」选择要翻译的屏幕范围".into());
            return;
        }
        self.state.monitoring = !self.state.monitoring;
        self.state.last_error = None;
        self.state.ocr_working = false;
        self.state.translate_working = false;
        if self.state.monitoring {
            // 开始监控即预热两个本地模型服务端，缩短首次识别/首句翻译的等待：
            // 视觉模型(OCR)冷启动需几十秒；本地翻译模型(1.5B)也要数秒~数十秒加载。
            translate::prefetch_vl_server(&self.state.translator);
            translate::prefetch_translation_server(&self.state.translator);
        }
        self.sync_config();
    }

    fn copy_target(&mut self) {
        if let Ok(mut clip) = arboard::Clipboard::new() {
            let _ = clip.set_text(self.state.target_text.clone());
        }
    }

    /// 立即强制翻译当前“原文”框里的内容，不等待监控线程的稳定判定。
    /// 用于测试后端/手动触发，或画面抖动导致自动翻译迟迟不发起时。
    fn force_translate(&mut self, ctx: &egui::Context) {
        let source = self.state.source_text.clone();
        if source.trim().is_empty() {
            self.state.last_error = Some("当前原文为空，无法翻译".into());
            return;
        }
        self.state.last_error = None;
        let target = self.state.target_lang;
        let cfg = self.state.translator.clone();
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        thread::spawn(move || {
            let translated = translate::translate(&source, target, &cfg);
            let _ = tx.send(TranslateResult {
                source,
                target: translated,
                live: false,
                streaming: false,
                source_streaming: false,
                clear: false,
            });
            ctx2.request_repaint();
        });
    }

    /// 区域边框浮层是否需要"隐藏"（移出屏幕）：
    /// 框选/调整/取窗等全屏取景模式中，浮层若留在屏幕上会被截进背景图。
    fn overlay_suppressed(&self) -> bool {
        self.state.pending_mode.is_some()
            || self.state.selecting
            || self.state.adjusting
            || self.state.attach_picking
    }

    /// 把区域边框浮层（4 条边框 + 1 个标签）立刻移到屏幕外。
    /// 在点击进入全屏取景的同一帧调用：OuterPosition 是同步 SetWindowPos，
    /// 帧末应用后，下一帧截屏时浮层必然不在屏幕上。
    /// 不用 ViewportCommand::Close —— 窗口销毁是异步的（WM_CLOSE 要等窗口系统
    /// 真正 DestroyWindow），下一帧截图时窗口可能还在屏幕上，边框会被截进背景图。
    fn move_overlays_off_screen(&self, ctx: &egui::Context) {
        let off = egui::pos2(-20000.0, -20000.0);
        for name in ["top", "bottom", "left", "right"] {
            ctx.send_viewport_cmd_to(
                egui::ViewportId::from_hash_of(("xiangxu_border", name)),
                egui::ViewportCommand::OuterPosition(off),
            );
        }
        ctx.send_viewport_cmd_to(
            egui::ViewportId::from_hash_of("xiangxu_border_label"),
            egui::ViewportCommand::OuterPosition(off),
        );
    }

    /// 全屏取景：截主屏做背景并全屏显示（框选区域 / 取窗吸附共用）。
    /// 成功后返回 true；失败时写入 last_error。
    fn enter_fullscreen_bg(&mut self, ctx: &egui::Context) -> bool {
        match capture::capture_primary() {
            Ok((img, scale, offset)) => {
                let w = img.width() as usize;
                let h = img.height() as usize;
                let raw = img.to_rgba8().into_raw();
                let pixels: Vec<egui::Color32> = raw
                    .chunks(4)
                    .map(|c| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]))
                    .collect();
                let color_image = egui::ColorImage {
                    size: [w, h],
                    source_size: egui::Vec2::new(w as f32, h as f32),
                    pixels,
                };
                let tex = ctx.load_texture("sel_bg", color_image, egui::TextureOptions::default());
                self.state.bg_texture = Some(tex);
                self.state.sel_scale = scale;
                self.state.sel_monitor_offset = offset;
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
                true
            }
            Err(e) => {
                self.state.last_error = Some(format!("截图失败: {}", e));
                false
            }
        }
    }

    /// 请求进入框选模式：当帧把浮层移到屏幕外（帧末生效），
    /// 下一帧再真正截屏并全屏取景（此时浮层已在屏幕外，背景干净）。
    fn enter_selection(&mut self, ctx: &egui::Context) {
        self.move_overlays_off_screen(ctx);
        self.state.pending_mode = Some(PendingFullscreenMode::Selection);
        ctx.request_repaint();
    }

    /// 实际进入框选模式（上一帧浮层已关闭，此时截屏背景干净）。
    /// 区域编辑模式：已有区域时把区域高亮显示出来（首帧渲染时换算 UI 坐标），
    /// 支持 拖动移动 / 拖角边缩放 / 空白处拖新框覆盖 / 双击清除 / Esc 完成。
    fn enter_selection_now(&mut self, ctx: &egui::Context) {
        if self.enter_fullscreen_bg(ctx) {
            self.state.selecting = true;
            self.state.sel_start = None;
            self.state.sel_end = None;
            // 编辑状态重置；region 存在时首帧由 render_selection 初始化 adjust_rect
            self.state.adjust_rect = None;
            self.state.adjust_drag = None;
            self.state.adjust_start_ptr = None;
            self.state.adjust_start_rect = None;
        }
    }

    fn confirm_selection(&mut self, ctx: &egui::Context) {
        let mut got_region = false;
        if let (Some(a), Some(b)) = (self.state.sel_start, self.state.sel_end) {
            let ppp = ctx.pixels_per_point();
            let x1 = a.x.min(b.x);
            let y1 = a.y.min(b.y);
            let w = (a.x.max(b.x) - x1).max(0.0);
            let h = (a.y.max(b.y) - y1).max(0.0);
            if w > 4.0 && h > 4.0 {
                let rx = (x1 * ppp) as i32 + self.state.sel_monitor_offset.0;
                let ry = (y1 * ppp) as i32 + self.state.sel_monitor_offset.1;
                let rw = (w * ppp) as u32;
                let rh = (h * ppp) as u32;
                self.state.region = Some(ScreenRect {
                    x: rx,
                    y: ry,
                    w: rw,
                    h: rh,
                });
                self.state.last_error = None;
                got_region = true;
            }
        }
        self.state.sel_start = None;
        self.state.sel_end = None;
        // 只有真的框出够大的区域才退出选择；框太小则留在界面让用户重试。
        if got_region {
            // 已吸附时：新区域也要跟随窗口 → 重算区域相对窗口的偏移
            if self.state.attach_enabled && self.state.attach_hwnd != 0 {
                self.state.attach_region_offset =
                    self.region_offset_from_window(self.state.attach_hwnd);
            }
            // 新框覆盖旧框：旧编辑状态一并作废
            self.state.adjust_rect = None;
            self.state.adjust_drag = None;
            self.state.adjust_start_ptr = None;
            self.state.adjust_start_rect = None;
            self.state.selecting = false;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            self.sync_config();
        }
    }

    fn cancel_selection(&mut self, ctx: &egui::Context) {
        // 编辑模式下移动/缩放是所见即所得：Esc 退出前把改动写回（而不是丢弃）。
        self.write_back_adjust(ctx);
        self.state.selecting = false;
        self.state.sel_start = None;
        self.state.sel_end = None;
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
    }

    // ---------- 取窗吸附（P0）----------

    /// 请求进入取窗模式：先记住面板当前（未全屏）的位置，作为吸附偏移的基准，
    /// 并把浮层移到屏幕外；下一帧进入全屏取景（截图背景干净）。
    fn start_attach_pick(&mut self, ctx: &egui::Context) {
        // 先记住面板当前（未全屏）的位置，作为吸附偏移的基准
        self.state.pick_panel_pos = ctx.input(|i| i.viewport().outer_rect).map(|r| r.min);
        self.move_overlays_off_screen(ctx);
        self.state.pending_mode = Some(PendingFullscreenMode::Pick);
        ctx.request_repaint();
    }

    /// 实际进入取窗模式（全屏取景，移动鼠标实时高亮目标窗口，左键确认、右键/Esc 取消）。
    fn start_attach_pick_now(&mut self, ctx: &egui::Context) {
        if self.enter_fullscreen_bg(ctx) {
            self.state.attach_picking = true;
            self.state.pick_hovered = None;
        } else {
            self.state.pick_panel_pos = None;
        }
    }

    fn cancel_attach_pick(&mut self, ctx: &egui::Context) {
        self.state.attach_picking = false;
        self.state.pick_hovered = None;
        self.state.pick_panel_pos = None;
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
    }

    /// 确认吸附到指定窗口（调用方需保证 hwnd 有效）。
    fn attach_to(&mut self, ctx: &egui::Context, hwnd: isize) {
        self.state.attach_hwnd = hwnd;
        self.state.attach_title = window_title(hwnd);
        // 偏移基准：进入取窗前的面板位置（而非全屏取景时的位置），
        // 否则面板会以全屏位置为基准算偏移，退出全屏后跳到屏幕角落。
        self.state.attach_offset = match self.state.pick_panel_pos {
            Some(pp) => {
                let ppp = ctx.pixels_per_point();
                let (l, t, _, _) = capture::window_rect(hwnd).unwrap_or((0, 0, 0, 0));
                let win_min = egui::pos2(l as f32 / ppp, t as f32 / ppp);
                Some(pp - win_min)
            }
            None => self.panel_offset_from_window(ctx, hwnd),
        };
        self.state.pick_panel_pos = None;
        self.state.attach_region_offset = self.region_offset_from_window(hwnd);
        self.state.attach_enabled = true;
        self.state.attach_picking = false;
        self.state.pick_hovered = None;
        self.state.last_error = None;
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        self.sync_config();
    }

    /// 截取区域相对窗口左上角的偏移（物理像素）；无区域/窗口无效时返回 None。
    fn region_offset_from_window(&self, hwnd: isize) -> Option<(i32, i32)> {
        let r = self.state.region?;
        let (l, t, _, _) = capture::window_rect(hwnd)?;
        Some((r.x - l, r.y - t))
    }

    /// 实际截取区域：吸附时按窗口当前位置换算（与监控线程同一公式），
    /// 用于边框浮层与界面显示。
    fn effective_region(&self) -> Option<ScreenRect> {
        let r = self.state.region?;
        if self.state.attach_enabled
            && self.state.attach_hwnd != 0
            && let Some((l, t, _, _)) = capture::window_rect(self.state.attach_hwnd)
            && let Some((ox, oy)) = self.state.attach_region_offset
        {
            return Some(ScreenRect {
                x: l + ox,
                y: t + oy,
                w: r.w,
                h: r.h,
            });
        }
        Some(r)
    }

    /// 取窗模式全屏界面：截图背景 + 十字光标 + 实时高亮鼠标下的窗口。
    fn render_picker(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.global_style())
                    .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 110)),
            )
            .show_inside(ui, |ui| {
                let available = ui.available_size();
                let (rect, response) =
                    ui.allocate_exact_size(available, egui::Sense::click_and_drag());
                if let Some(tex) = &self.state.bg_texture {
                    ui.put(rect, egui::Image::from_texture(tex).max_size(rect.size()));
                }
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);

                // 顶部提示
                ui.put(
                    egui::Rect::from_min_size(
                        rect.min + egui::vec2(24.0, 20.0),
                        egui::vec2(620.0, 60.0),
                    ),
                    egui::Label::new(
                        egui::RichText::new(
                            "移动鼠标选择要吸附的窗口（橙色高亮），左键确认，右键或 Esc 取消",
                        )
                        .color(egui::Color32::WHITE)
                        .size(18.0),
                    ),
                );

                // 实时高亮鼠标下的窗口（用全局光标位置 + 枚举可见窗口，
                // 不能用 WindowFromPoint——全屏取景时它只会返回本应用自己的窗口）
                let ppp = ctx.pixels_per_point();
                let off = self.state.sel_monitor_offset;
                self.state.pick_hovered =
                    cursor_pos_physical().and_then(|(px, py)| find_visible_window_at(px, py));
                if let Some(hwnd) = self.state.pick_hovered
                    && is_valid_attach_target(hwnd)
                    && let Some((l, t, r, b)) = capture::window_rect(hwnd)
                {
                    let rr = egui::Rect::from_min_max(
                        egui::pos2((l - off.0) as f32 / ppp, (t - off.1) as f32 / ppp),
                        egui::pos2((r - off.0) as f32 / ppp, (b - off.1) as f32 / ppp),
                    );
                    ui.painter().rect_stroke(
                        rr,
                        2.0,
                        egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(255, 170, 60)),
                        egui::StrokeKind::Outside,
                    );
                    let title = truncate_chars(&window_title(hwnd), 30);
                    ui.painter().text(
                        egui::pos2(rr.left() + 6.0, (rr.top() - 26.0).max(4.0)),
                        egui::Align2::LEFT_TOP,
                        format!("点击吸附: {}", title),
                        egui::FontId::proportional(14.0),
                        egui::Color32::from_rgb(255, 190, 90),
                    );
                }

                // 左键确认：以点击位置为准取窗口
                if response.clicked()
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    let px = (pos.x * ppp) as i32 + off.0;
                    let py = (pos.y * ppp) as i32 + off.1;
                    if let Some(hwnd) = find_visible_window_at(px, py)
                        && is_valid_attach_target(hwnd)
                    {
                        self.attach_to(&ctx, hwnd);
                        return;
                    }
                }
                // 右键 / Esc 取消
                if ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary))
                    || ctx.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    self.cancel_attach_pick(&ctx);
                }
            });
    }

    // ---------- 区域调整（A 方案：全屏拖拽移动/缩放）----------

    /// 清除已选区域（并停止监控，因为无区域可截）。
    fn clear_region(&mut self) {
        self.state.region = None;
        self.state.attach_region_offset = None;
        if self.state.monitoring {
            self.state.monitoring = false;
            self.state.ocr_working = false;
            self.state.translate_working = false;
        }
        self.sync_config();
    }

    /// 请求进入区域调整模式：当帧把浮层移到屏幕外（帧末生效），
    /// 下一帧再真正进入（截图背景干净）。
    fn start_adjust(&mut self, ctx: &egui::Context) {
        if self.state.region.is_none() {
            return;
        }
        self.move_overlays_off_screen(ctx);
        self.state.pending_mode = Some(PendingFullscreenMode::Adjust);
        ctx.request_repaint();
    }

    /// 实际进入区域调整模式：全屏取景，显示现有区域，拖内部移动、拖角/边缩放。
    fn start_adjust_now(&mut self, ctx: &egui::Context) {
        if self.state.region.is_none() {
            return;
        }
        if self.enter_fullscreen_bg(ctx) {
            self.state.adjusting = true;
            self.state.adjust_rect = None; // 首次渲染时按当前区域初始化
            self.state.adjust_drag = None;
            self.state.adjust_start_ptr = None;
            self.state.adjust_start_rect = None;
        }
    }

    /// 把编辑中的矩形写回区域（物理坐标），并重算吸附偏移；随后清理编辑状态。
    /// 框选编辑模式与旧「调整区域」模式共用的收尾逻辑。
    fn write_back_adjust(&mut self, ctx: &egui::Context) {
        if let Some(r) = self.state.adjust_rect {
            let ppp = ctx.pixels_per_point();
            let off = self.state.sel_monitor_offset;
            let x = (r.min.x * ppp) as i32 + off.0;
            let y = (r.min.y * ppp) as i32 + off.1;
            let w = (r.width() * ppp).round().max(1.0) as u32;
            let h = (r.height() * ppp).round().max(1.0) as u32;
            self.state.region = Some(ScreenRect { x, y, w, h });
            // 已吸附：重算区域相对窗口的偏移
            if self.state.attach_enabled && self.state.attach_hwnd != 0 {
                self.state.attach_region_offset =
                    self.region_offset_from_window(self.state.attach_hwnd);
            }
            self.sync_config();
        }
        self.state.adjust_rect = None;
        self.state.adjust_drag = None;
        self.state.adjust_start_ptr = None;
        self.state.adjust_start_rect = None;
    }

    /// 确认调整：把调整后的矩形写回区域（物理坐标），并重算吸附偏移。
    fn confirm_adjust(&mut self, ctx: &egui::Context) {
        self.write_back_adjust(ctx);
        self.state.adjusting = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
    }

    /// 取消调整：丢弃改动，退出全屏。
    fn cancel_adjust(&mut self, ctx: &egui::Context) {
        self.state.adjusting = false;
        self.state.adjust_rect = None;
        self.state.adjust_drag = None;
        self.state.adjust_start_ptr = None;
        self.state.adjust_start_rect = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
    }

    /// 区域调整全屏界面：截图背景 + 区域矩形（绿色）+ 8 个手柄 + 完成/取消。
    fn render_adjust(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.global_style())
                    .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 110)),
            )
            .show_inside(ui, |ui| {
                let available = ui.available_size();
                let (rect, _resp) =
                    ui.allocate_exact_size(available, egui::Sense::click_and_drag());
                if let Some(tex) = &self.state.bg_texture {
                    ui.put(rect, egui::Image::from_texture(tex).max_size(rect.size()));
                }
                let ppp = ctx.pixels_per_point();
                let off = self.state.sel_monitor_offset;

                // 初始化调整矩形（从当前区域换算到 UI 坐标）
                if self.state.adjust_rect.is_none() {
                    match self.state.region {
                        Some(r) => {
                            self.state.adjust_rect = Some(egui::Rect::from_min_size(
                                egui::pos2((r.x - off.0) as f32 / ppp, (r.y - off.1) as f32 / ppp),
                                egui::vec2(r.w as f32 / ppp, r.h as f32 / ppp),
                            ));
                        }
                        None => {
                            // 无区域：直接退出
                            self.cancel_adjust(&ctx);
                            return;
                        }
                    }
                }
                let mut cur_rect = match self.state.adjust_rect {
                    Some(r) => r,
                    None => {
                        // 初始化失败（无区域）：退出
                        self.cancel_adjust(&ctx);
                        return;
                    }
                };

                // 拖拽交互
                let pointer = ctx.input(|i| i.pointer.interact_pos());
                if ctx.input(|i| i.pointer.primary_pressed())
                    && let (Some(p), Some(cr)) = (pointer, self.state.adjust_rect)
                    && let Some(h) = hit_test_adjust_handle(&cr, p)
                {
                    self.state.adjust_drag = Some(h);
                    self.state.adjust_start_ptr = Some(p);
                    self.state.adjust_start_rect = Some(cr);
                }
                if ctx.input(|i| i.pointer.primary_down())
                    && let (Some(h), Some(sp), Some(sr)) = (
                        self.state.adjust_drag,
                        self.state.adjust_start_ptr,
                        self.state.adjust_start_rect,
                    )
                    && let Some(p) = pointer
                {
                    let delta = p - sp;
                    cur_rect = apply_adjust(sr, h, delta);
                    self.state.adjust_rect = Some(cur_rect);
                }
                if ctx.input(|i| i.pointer.primary_released()) {
                    self.state.adjust_drag = None;
                    self.state.adjust_start_ptr = None;
                    self.state.adjust_start_rect = None;
                }

                // 方向键微调（Shift 加速）
                let step = if ctx.input(|i| i.modifiers.shift) {
                    10.0
                } else {
                    1.0
                };
                let mut nudge = egui::Vec2::ZERO;
                for (key, v) in [
                    (egui::Key::ArrowLeft, egui::vec2(-step, 0.0)),
                    (egui::Key::ArrowRight, egui::vec2(step, 0.0)),
                    (egui::Key::ArrowUp, egui::vec2(0.0, -step)),
                    (egui::Key::ArrowDown, egui::vec2(0.0, step)),
                ] {
                    if ctx.input(|i| i.key_pressed(key)) {
                        nudge += v;
                    }
                }
                if nudge != egui::Vec2::ZERO {
                    cur_rect = cur_rect.translate(nudge);
                    self.state.adjust_rect = Some(cur_rect);
                }

                // 绘制区域矩形与手柄
                let green = egui::Color32::from_rgb(90, 230, 140);
                ui.painter().rect_stroke(
                    cur_rect,
                    2.0,
                    egui::Stroke::new(2.0_f32, green),
                    egui::StrokeKind::Outside,
                );
                for h in [
                    AdjustHandle::TopLeft,
                    AdjustHandle::Top,
                    AdjustHandle::TopRight,
                    AdjustHandle::Right,
                    AdjustHandle::BottomRight,
                    AdjustHandle::Bottom,
                    AdjustHandle::BottomLeft,
                    AdjustHandle::Left,
                ] {
                    if let Some(r) = adjust_handle_rect(&cur_rect, h) {
                        ui.painter().rect_filled(r, 2.0, green);
                    }
                }
                // 坐标/尺寸信息
                let info = format!(
                    "区域: {}×{} @ ({},{})",
                    (cur_rect.width() * ppp).round() as i32,
                    (cur_rect.height() * ppp).round() as i32,
                    (cur_rect.min.x * ppp) as i32 + off.0,
                    (cur_rect.min.y * ppp) as i32 + off.1,
                );
                ui.painter().text(
                    egui::pos2(cur_rect.left(), (cur_rect.top() - 26.0).max(4.0)),
                    egui::Align2::LEFT_TOP,
                    info,
                    egui::FontId::proportional(14.0),
                    green,
                );
                ui.painter().text(
                    egui::pos2(rect.left() + 24.0, rect.top() + 24.0),
                    egui::Align2::LEFT_TOP,
                    "拖内部移动 / 拖角边缩放 / 方向键微调（Shift 加速）",
                    egui::FontId::proportional(16.0),
                    egui::Color32::WHITE,
                );

                // 完成 / 取消按钮（右上角）
                let done_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.right() - 170.0, rect.top() + 16.0),
                    egui::vec2(74.0, 30.0),
                );
                if ui.put(done_rect, egui::Button::new("完成")).clicked()
                    || ctx.input(|i| i.key_pressed(egui::Key::Enter))
                {
                    self.confirm_adjust(&ctx);
                    return;
                }
                let cancel_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.right() - 88.0, rect.top() + 16.0),
                    egui::vec2(74.0, 30.0),
                );
                if ui.put(cancel_rect, egui::Button::new("取消")).clicked()
                    || ctx.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    self.cancel_adjust(&ctx);
                }
            });
    }

    /// ---------- 主窗口形态切换（工具条 ↔ 悬浮球） ----------
    fn switch_form(&mut self, ctx: &egui::Context, bar: bool) {
        if self.bar_mode == bar {
            return;
        }
        self.bar_mode = bar;
        let size = if bar { BAR_SIZE } else { BALL_SIZE };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
    }

    /// 常驻工具条（主窗口本体，无边框置顶小条）。
    /// 布局参考 Snipaste / PowerToys / Windows 11 浮条：
    /// 现代扁平工具栏：浅色底 + 分组间距 + 纯 painter 图标。
    /// 所有图标（状态点、悬浮球、关闭 ×）都用 line/circle 绘制，
    /// 不依赖任何可能缺失的 Unicode 符号字体。
    /// 吸附改为 toggle：未吸附显示"吸附"，吸附后高亮但仍显示"吸附"，
    /// 避免"取消吸附"四字把布局撑乱。
    fn render_toolbar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();

        // 背景：极浅灰 + 细边框
        ui.painter()
            .rect_filled(rect, 0.0, egui::Color32::from_rgb(248, 249, 252));
        ui.painter().rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(220, 223, 230)),
            egui::StrokeKind::Inside,
        );

        // 背景拖拽感应：条内空白处按下拖动即可移动整窗
        let drag = ui.interact(rect, ui.id().with("bar_drag"), egui::Sense::drag());
        if drag.drag_started() {
            self.begin_os_drag(&ctx);
        }
        self.end_os_drag_if_released(&ctx);

        // 颜色常量
        let text_gray = egui::Color32::from_gray(80);
        let text_dark = egui::Color32::from_gray(40);
        let hover_bg = egui::Color32::from_gray(230);
        let icon_gray = egui::Color32::from_gray(120);
        let status_green = egui::Color32::from_rgb(46, 160, 90);
        let status_gray = egui::Color32::from_gray(160);

        ui.horizontal(|ui| {
            ui.set_min_height(rect.height());
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add_space(10.0);

            // ---- 左侧：拖拽把手 + 状态圆点 ----
            let (grip_rect, grip_resp) =
                ui.allocate_exact_size(egui::vec2(20.0, rect.height()), egui::Sense::drag());
            ui.painter().text(
                grip_rect.center(),
                egui::Align2::CENTER_CENTER,
                "≡",
                egui::FontId::proportional(16.0),
                egui::Color32::from_gray(150),
            );
            if grip_resp.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::Grab);
            }
            if grip_resp.drag_started() {
                self.begin_os_drag(&ctx);
            }

            // 状态圆点：10px 实心圆，hover 看 tooltip
            let (dot_rect, dot_resp) =
                ui.allocate_exact_size(egui::vec2(14.0, rect.height()), egui::Sense::hover());
            let dot_color = if self.state.monitoring {
                status_green
            } else {
                status_gray
            };
            ui.painter()
                .circle_filled(dot_rect.center(), 5.0, dot_color);
            dot_resp.on_hover_text(if self.state.monitoring {
                "监控中"
            } else {
                "已停止"
            });

            ui.add_space(14.0);

            // ---- 主操作组 ----
            let primary_fill = if self.state.monitoring {
                egui::Color32::from_rgb(196, 74, 74)
            } else {
                egui::Color32::from_rgb(46, 160, 90)
            };
            let primary_label = if self.state.monitoring {
                "停止"
            } else {
                "开始"
            };
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(primary_label)
                            .size(13.0)
                            .color(egui::Color32::WHITE),
                    )
                    .fill(primary_fill)
                    .min_size(egui::vec2(54.0, 32.0))
                    .corner_radius(6.0),
                )
                .on_hover_text("开始 / 停止区域监控翻译")
                .clicked()
            {
                self.toggle_monitoring();
            }

            // 扁平 toggle 文字按钮
            let mut toggle_btn = |label: &str, selected: bool, tip: &str| -> bool {
                let fill = if selected {
                    hover_bg
                } else {
                    egui::Color32::TRANSPARENT
                };
                let text_color = if selected { text_dark } else { text_gray };
                ui.add(
                    egui::Button::new(egui::RichText::new(label).size(13.0).color(text_color))
                        .fill(fill)
                        .min_size(egui::vec2(40.0, 32.0))
                        .corner_radius(6.0)
                        .selected(selected),
                )
                .on_hover_text(tip)
                .clicked()
            };

            if toggle_btn(
                "框选",
                false,
                "框选/调整区域：已有区域时可拖动、缩放、双击清除",
            ) {
                self.enter_selection(&ctx);
            }
            if toggle_btn("吸附", self.state.attach_enabled, "吸附/取消吸附游戏窗口") {
                if self.state.attach_enabled {
                    if let Some(eff) = self.effective_region() {
                        self.state.region = Some(eff);
                    }
                    self.state.attach_enabled = false;
                    self.state.attach_hwnd = 0;
                    self.state.attach_offset = None;
                    self.state.attach_region_offset = None;
                    self.state.attach_title.clear();
                    self.sync_config();
                } else {
                    self.start_attach_pick(&ctx);
                }
            }
            if toggle_btn("字幕", self.sub_visible, "显示 / 隐藏独立字幕悬浮窗") {
                self.sub_visible = !self.sub_visible;
            }
            if toggle_btn("设置", self.settings_open, "打开设置 / 控制台窗口") {
                self.settings_open = !self.settings_open;
            }

            // ---- 右侧图标按钮：推到最右 ----
            ui.add_space(ui.available_width() - 80.0);

            // 悬浮球按钮：画一个实心圆
            let (ball_rect, ball_resp) =
                ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::click());
            if ball_resp.hovered() {
                ui.painter().rect_filled(ball_rect, 8.0, hover_bg);
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            ui.painter().circle_filled(
                ball_rect.center(),
                7.0,
                if ball_resp.hovered() {
                    egui::Color32::from_gray(90)
                } else {
                    icon_gray
                },
            );
            if ball_resp.clicked() {
                self.switch_form(&ctx, false);
            }
            ball_resp.on_hover_text("缩为悬浮球");

            // 关闭按钮：画一个 ×
            let (close_rect, close_resp) =
                ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::click());
            let close_hover = close_resp.hovered();
            if close_hover {
                ui.painter()
                    .rect_filled(close_rect, 8.0, egui::Color32::from_rgb(255, 230, 230));
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let close_color = if close_hover {
                egui::Color32::from_rgb(200, 60, 60)
            } else {
                icon_gray
            };
            let c = close_rect.center();
            let r = 5.5;
            ui.painter().line_segment(
                [c + egui::vec2(-r, -r), c + egui::vec2(r, r)],
                egui::Stroke::new(1.5, close_color),
            );
            ui.painter().line_segment(
                [c + egui::vec2(r, -r), c + egui::vec2(-r, r)],
                egui::Stroke::new(1.5, close_color),
            );
            if close_resp.clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            close_resp.on_hover_text("退出象胥");

            ui.add_space(8.0);
        });
    }

    /// 悬浮球形态（主窗口缩成 56×56 小球）：
    /// 拖动移动、单击展开回工具条、贴边可缩进留小耳朵。
    fn render_ball(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();

        let resp = ui.interact(
            rect.shrink(3.0),
            ui.id().with("ball"),
            egui::Sense::click_and_drag(),
        );
        if resp.drag_started() {
            // Windows 原生拖拽（SC_MOVE）：丝滑，无逐帧位移的橡皮筋感
            self.begin_os_drag(&ctx);
        }
        self.end_os_drag_if_released(&ctx);
        if resp.clicked() {
            // 单击球 → 展开为工具条
            self.switch_form(&ctx, true);
        }

        // 球体：监控中绿色 / 停止深灰；工作中有描边动画感（简化为亮描边）
        let (fill, ring) = if self.state.monitoring {
            (
                egui::Color32::from_rgb(46, 160, 90),
                egui::Color32::from_rgb(120, 230, 160),
            )
        } else {
            (
                egui::Color32::from_rgb(48, 54, 62),
                egui::Color32::from_gray(110),
            )
        };
        let body = rect.shrink(4.0);
        ui.painter().rect_filled(body, body.width() / 2.0, fill);
        ui.painter().rect_stroke(
            body,
            body.width() / 2.0,
            egui::Stroke::new(2.0_f32, ring),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            body.center(),
            egui::Align2::CENTER_CENTER,
            "象",
            egui::FontId::proportional(20.0),
            egui::Color32::WHITE,
        );
    }

    /// 设置窗（独立 HWND viewport，immediate 模式可直接借用 self）：
    /// 承载完整控制台界面（状态条 + 原文/译文 + 左导航设置页）。
    /// 有装饰、可最小化——它是纯 UI 窗口，最小化不影响字幕/边框子 viewport。
    fn render_settings_window(&mut self, ctx: &egui::Context) {
        let builder = egui::ViewportBuilder::default()
            .with_title("象胥 设置")
            .with_inner_size([440.0, 540.0])
            .with_min_inner_size([360.0, 320.0]);
        ctx.show_viewport_immediate(self.settings_id, builder, |ctx, _class| {
            if ctx.input(|i| i.viewport().close_requested()) {
                // 用户点了窗口 X：下一帧不再 show，窗口随之关闭
                self.settings_open = false;
                return;
            }
            // 手动构建根 Ui 后走 show_inside（0.34 中 CentralPanel::show 已弃用）
            let mut root = egui::Ui::new(
                ctx.clone(),
                egui::Id::new("xiangxu_settings_root"),
                egui::UiBuilder::new()
                    .layer_id(egui::LayerId::background())
                    .max_rect(ctx.content_rect()),
            );
            root.set_clip_rect(ctx.content_rect());
            let style = ctx.style();
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::central_panel(&style).inner_margin(egui::Margin::symmetric(10, 8)),
                )
                .show_inside(&mut root, |ui| {
                    self.render_panel_body(ui);
                });
        });
    }

    /// ---------- 贴边缩进（工具条/悬浮球拖到屏幕边缘） ----------

    /// 拖动结束：窗口靠近左/右边缘则进入贴边模式。
    /// 进入 Windows 原生窗口拖拽（ViewportCommand::StartDrag → winit drag_window →
    /// SC_MOVE 模态循环）。相比每帧 OuterPosition+delta 的手动位移，原生拖拽
    /// 完全跟随鼠标、零橡皮筋感；拖拽期间清空贴边状态避免缩进逻辑打架。
    fn begin_os_drag(&mut self, ctx: &egui::Context) {
        self.dock = None;
        self.dock_hidden = false;
        self.os_dragging = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    /// 原生拖拽期间鼠标被模态循环捕获、收不到 egui 拖拽事件，
    /// 用“os_dragging 且主键已松开”判断拖拽结束，随后恢复贴边检测。
    fn end_os_drag_if_released(&mut self, ctx: &egui::Context) {
        if self.os_dragging && !ctx.input(|i| i.pointer.primary_down()) {
            self.os_dragging = false;
            self.maybe_dock(ctx);
        }
    }

    fn maybe_dock(&mut self, ctx: &egui::Context) {
        let Some(outer) = ctx.input(|i| i.viewport().outer_rect) else {
            return;
        };
        let Some(mon) = ctx.input(|i| i.viewport().monitor_size) else {
            return;
        };
        const EDGE: f32 = 32.0;
        if outer.left() <= EDGE {
            self.dock = Some(DockSide::Left);
            self.dock_hidden = true; // 先隐藏，鼠标靠近再滑出
        } else if outer.right() >= mon.x - EDGE {
            self.dock = Some(DockSide::Right);
            self.dock_hidden = true;
        } else {
            self.dock = None;
            self.dock_hidden = false;
        }
    }

    /// 每帧：贴边模式下根据全局鼠标位置滑出/缩回。
    /// 全屏取景（框选/调整/取窗）时不处理。
    fn update_dock(&mut self, ctx: &egui::Context) {
        let Some(side) = self.dock else {
            return;
        };
        if self.state.selecting || self.state.adjusting || self.state.attach_picking {
            return;
        }
        let Some(outer) = ctx.input(|i| i.viewport().outer_rect) else {
            return;
        };
        let Some(mon) = ctx.input(|i| i.viewport().monitor_size) else {
            return;
        };
        let ppp = ctx.pixels_per_point();
        // 全局鼠标位置（换算为 points）
        let near_edge = cursor_pos_physical()
            .map(|(x, _y)| {
                let x = x as f32 / ppp;
                match side {
                    DockSide::Left => x <= outer.width() + 40.0,
                    DockSide::Right => x >= mon.x - (outer.width() + 40.0),
                }
            })
            .unwrap_or(false);

        let ear = 10.0; // 缩进后留出的"小耳朵"宽度
        let target_x = match (side, self.dock_hidden, near_edge) {
            (DockSide::Left, true, true) => {
                self.dock_hidden = false;
                Some(0.0)
            }
            (DockSide::Left, false, false) => {
                self.dock_hidden = true;
                Some(-(outer.width() - ear))
            }
            (DockSide::Right, true, true) => {
                self.dock_hidden = false;
                Some(mon.x - outer.width())
            }
            (DockSide::Right, false, false) => {
                self.dock_hidden = true;
                Some(mon.x - ear)
            }
            _ => None,
        };
        if let Some(x) = target_x
            && let Some(pos) = ctx.input(|i| i.viewport().outer_rect).map(|r| r.min)
            && (pos.x - x).abs() > 0.5
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, pos.y)));
        }
        // 贴边期间保持轮询重绘（检测鼠标靠近）
        ctx.request_repaint_after(Duration::from_millis(120));
    }

    /// 设置窗主体（独立 viewport 弹出，承载完整控制台界面）。
    /// 布局分层：标题行 → 单行状态条 → 原文/译文卡片 → 更多操作（折叠）→ 左导航设置页。
    fn render_panel_body(&mut self, ui: &mut egui::Ui) {
        // ---- 标题行 ----
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("象胥 · 设置").strong());
            let (text, color) = if self.state.monitoring {
                ("● 监控中", egui::Color32::from_rgb(70, 200, 120))
            } else {
                ("○ 已停止", egui::Color32::GRAY)
            };
            ui.label(egui::RichText::new(text).size(11.0).color(color).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("关闭").clicked() {
                    self.settings_open = false;
                }
            });
        });

        // ---- 状态条：后端 / 吸附目标 / 区域信息 压缩为一行 ----
        let gray = egui::Color32::from_gray(140);
        let attach_part = if self.state.attach_enabled {
            if self.state.attach_title.trim().is_empty() {
                "吸附中".to_string()
            } else {
                format!("吸附: {}", truncate_chars(&self.state.attach_title, 12))
            }
        } else {
            String::new()
        };
        let has_region = self.state.region.is_some();
        let region_part = match self.effective_region() {
            Some(r) => format!(
                "区域 {}×{}{}",
                r.w,
                r.h,
                if self.state.attach_enabled {
                    "·跟随"
                } else {
                    ""
                }
            ),
            None => "未框选区域".to_string(),
        };
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(self.backend_hint())
                    .size(11.0)
                    .color(gray),
            );
            if !attach_part.is_empty() {
                ui.label(
                    egui::RichText::new(attach_part)
                        .size(11.0)
                        .color(egui::Color32::from_rgb(120, 170, 230)),
                );
            }
            let region_color = if has_region {
                gray
            } else {
                egui::Color32::from_rgb(220, 150, 60)
            };
            ui.label(
                egui::RichText::new(region_part)
                    .size(11.0)
                    .color(region_color),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::Checkbox::new(
                    &mut self.state.show_region_border,
                    egui::RichText::new("边框").size(11.0),
                ));
            });
        });
        ui.separator();

        // ---- 左侧导航 + 右侧内容页 ----
        // 显示(默认)/区域/翻译/识图/模型/历史/关于，设置按功能分区（OBS 式），
        // 不再把全部设置叠进底部折叠区。
        egui::Panel::left("nav_panel")
            .resizable(false)
            .exact_size(64.0)
            .frame(
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(4, 2))
                    .fill(egui::Color32::from_gray(248)),
            )
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    for p in Page::ALL {
                        ui.selectable_value(
                            &mut self.page,
                            p,
                            egui::RichText::new(p.label()).size(12.0),
                        );
                    }
                });
            });

        match self.page {
            Page::Display => self.render_page_display(ui),
            Page::Region => self.render_page_region(ui),
            Page::Translate => self.render_page_translate(ui),
            Page::Ocr => self.render_page_ocr(ui),
            Page::Models => self.render_page_models(ui),
            Page::History => self.render_page_history(ui),
            Page::About => self.render_page_about(ui),
        }
    }

    /// 「显示」页：原文/译文卡片 + 低频操作 + 错误提示。
    fn render_page_display(&mut self, ui: &mut egui::Ui) {
        let ocr_busy = self.state.ocr_working;
        let tr_busy = self.state.translate_working;
        render_text_card(
            ui,
            "原文",
            &mut self.state.source_text,
            ocr_busy,
            "识别中…",
            true,
        );
        render_text_card(
            ui,
            "译文",
            &mut self.state.target_text,
            tr_busy,
            "翻译中…",
            false,
        );

        // 提示：原文已有、译文未出（且当前不在翻译中）
        if !self.state.source_text.trim().is_empty()
            && self.state.target_text.trim().is_empty()
            && !tr_busy
        {
            ui.label(
                egui::RichText::new("原文已识别、译文未出——可点「立即翻译」强制翻译")
                    .size(11.0)
                    .color(egui::Color32::from_gray(150)),
            );
        }

        // ---- 低频操作：折叠收纳（调整/清除/立即翻译/复制/清空）----
        egui::CollapsingHeader::new("更多操作")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let can_adjust = self.state.region.is_some();
                    if ui
                        .add_enabled(can_adjust, egui::Button::new("调整区域"))
                        .clicked()
                    {
                        let ctx = ui.ctx().clone();
                        self.start_adjust(&ctx);
                    }
                    if ui
                        .add_enabled(can_adjust, egui::Button::new("清除区域"))
                        .clicked()
                    {
                        self.clear_region();
                    }
                    if ui.button("立即翻译").clicked() {
                        let ctx = ui.ctx().clone();
                        self.force_translate(&ctx);
                    }
                    if ui.button("复制译文").clicked() {
                        self.copy_target();
                    }
                    if ui.button("清空显示").clicked() {
                        self.state.source_text.clear();
                        self.state.target_text.clear();
                        self.state.ocr_working = false;
                        self.state.translate_working = false;
                    }
                });
            });

        if let Some(err) = &self.state.last_error {
            ui.add_space(4.0);
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err);
        }
    }

    /// 「区域」页：吸附目标 + 截取区域的完整信息与操作。
    fn render_page_region(&mut self, ui: &mut egui::Ui) {
        let gray = egui::Color32::from_gray(140);
        let blue = egui::Color32::from_rgb(30, 100, 200);

        ui.label(egui::RichText::new("目标窗口").strong());
        if self.state.attach_enabled {
            let t = if self.state.attach_title.trim().is_empty() {
                "已吸附窗口".to_string()
            } else {
                truncate_chars(&self.state.attach_title, 30)
            };
            ui.label(egui::RichText::new(t).size(12.0).color(blue));
            if ui.button("取消吸附").clicked() {
                if let Some(eff) = self.effective_region() {
                    self.state.region = Some(eff);
                }
                self.state.attach_enabled = false;
                self.state.attach_hwnd = 0;
                self.state.attach_offset = None;
                self.state.attach_region_offset = None;
                self.state.attach_title.clear();
                self.sync_config();
            }
        } else {
            ui.label(
                egui::RichText::new("未吸附（吸附后区域与面板跟随游戏窗口移动）")
                    .size(11.0)
                    .color(gray),
            );
            if ui.button("吸附窗口").clicked() {
                let ctx = ui.ctx().clone();
                self.start_attach_pick(&ctx);
            }
        }

        ui.add_space(8.0);
        ui.label(egui::RichText::new("截取区域").strong());
        match self.effective_region() {
            Some(r) => {
                let tag = if self.state.attach_enabled {
                    "（跟随窗口）"
                } else {
                    ""
                };
                ui.label(
                    egui::RichText::new(format!("{}×{} @ ({},{}){}", r.w, r.h, r.x, r.y, tag))
                        .size(12.0)
                        .color(gray),
                );
            }
            None => {
                ui.label(
                    egui::RichText::new("尚未框选区域（点「框选区域」选游戏对话框）")
                        .size(12.0)
                        .color(egui::Color32::from_rgb(220, 150, 60)),
                );
            }
        }
        ui.horizontal_wrapped(|ui| {
            let can_adjust = self.state.region.is_some();
            if ui.button("框选区域").clicked() {
                let ctx = ui.ctx().clone();
                self.enter_selection(&ctx);
            }
            if ui
                .add_enabled(can_adjust, egui::Button::new("调整区域"))
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.start_adjust(&ctx);
            }
            if ui
                .add_enabled(can_adjust, egui::Button::new("清除区域"))
                .clicked()
            {
                self.clear_region();
            }
        });
        ui.checkbox(&mut self.state.show_region_border, "显示区域边框");
    }

    /// 「历史记录」页：最近 20 条（新在前）。
    fn render_page_history(&mut self, ui: &mut egui::Ui) {
        let gray = egui::Color32::from_gray(140);
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if self.state.history.is_empty() {
                    ui.label(egui::RichText::new("暂无记录").size(11.0).color(gray));
                }
                for h in self.state.history.iter().rev().take(20) {
                    ui.label(
                        egui::RichText::new(format!("{}  →  {}", h.source, h.target)).size(12.0),
                    );
                    ui.separator();
                }
            });
    }

    /// 「关于」页。
    fn render_page_about(&mut self, ui: &mut egui::Ui) {
        let gray = egui::Color32::from_gray(140);
        ui.heading(egui::RichText::new("象胥 实时翻译").strong());
        ui.label(format!("版本 {}", env!("CARGO_PKG_VERSION")));
        ui.add_space(6.0);
        ui.label("本地实时屏幕翻译，面向 English-only 游戏（如 Pokémon Gamma Emerald）。");
        ui.label(
            egui::RichText::new(
                "识图 Qwen2.5-VL-3B · 翻译 Qwen2.5-1.5B-Instruct · llama.cpp (CUDA)",
            )
            .size(11.0)
            .color(gray),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("模型经 Hugging Face 分发（Apache-2.0）。")
                .size(11.0)
                .color(gray),
        );
    }

    /// 后端信息摘要（识图 + 翻译 + CPU/GPU）。
    fn backend_hint(&self) -> String {
        let t = match self.state.translator.mode {
            AppMode::Mock => "翻译: 模拟".to_string(),
            AppMode::LocalVisionLocal => {
                let stem = std::path::Path::new(&self.state.translator.local_model)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                format!("本地翻译 {}", short_model_name(&stem))
            }
            AppMode::LocalVisionRemote => {
                if self.state.translator.api_key.trim().is_empty() {
                    "远程API(未填Key)".to_string()
                } else {
                    format!("远程API {}", self.state.translator.model)
                }
            }
        };
        let ocr = if translate::vl_server_ready(&self.state.translator) {
            "VL就绪"
        } else {
            "VL预热中"
        };
        let server = &self.state.translator.local_server;
        let engine = if server.contains("llama-cuda") || server.contains("cuda-13.3") {
            "GPU"
        } else {
            "CPU"
        };
        format!("识图 {} · {} · {}", ocr, t, engine)
    }

    // ---------- 吸附游戏窗口 ----------

    /// 当前面板位置相对指定窗口左上角的偏移（points）。
    fn panel_offset_from_window(&self, ctx: &egui::Context, hwnd: isize) -> Option<egui::Vec2> {
        let ppp = ctx.pixels_per_point();
        let (l, t, _, _) = capture::window_rect(hwnd)?;
        let win_min = egui::pos2(l as f32 / ppp, t as f32 / ppp);
        let pos = ctx.input(|i| i.viewport().outer_rect)?.min;
        Some(pos - win_min)
    }

    /// 每帧更新：跟随游戏窗口移动；窗口关闭自动取消吸附；拖动面板重新锚定。
    fn update_attach(&mut self, ctx: &egui::Context) {
        if !self.state.attach_enabled || self.state.selecting || self.state.attach_picking {
            return;
        }
        let hwnd = self.state.attach_hwnd;
        let Some((l, t, _, _)) = capture::window_rect(hwnd) else {
            // 窗口已关闭：先把区域定格在当前实际位置，再取消吸附
            if let Some(eff) = self.effective_region() {
                self.state.region = Some(eff);
            }
            self.state.attach_enabled = false;
            self.state.attach_hwnd = 0;
            self.state.attach_offset = None;
            self.state.attach_region_offset = None;
            self.state.attach_title.clear();
            self.state.last_error = Some("吸附的游戏窗口已关闭，已自动取消吸附".into());
            self.sync_config();
            return;
        };
        // 最小化时不跟随（避免面板跳到任务栏位置）
        if capture::window_iconic(hwnd) {
            return;
        }
        let ppp = ctx.pixels_per_point();
        let win_min = egui::pos2(l as f32 / ppp, t as f32 / ppp);
        if ctx.input(|i| i.pointer.any_down()) {
            // 用户正在拖动面板：重新记录偏移，松手后按新位置跟随
            if let Some(pos) = ctx.input(|i| i.viewport().outer_rect).map(|r| r.min) {
                self.state.attach_offset = Some(pos - win_min);
            }
            return;
        }
        let Some(offset) = self.state.attach_offset else {
            return;
        };
        let target = win_min + offset;
        if let Some(pos) = ctx.input(|i| i.viewport().outer_rect).map(|r| r.min)
            && target.distance(pos) > 0.5
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(target));
        }
    }

    /// 区域边框浮层：4 条细长实心绿色小窗拼成区域边框 + 一个小标签窗。
    /// 不依赖窗口透明（GL 配置无 alpha 时透明窗会变黑块），
    /// 全部用不透明色填充，置顶、点击穿透，只标位置、不挡内容。
    ///
    /// 关键设计：窗口**每帧都保持存活**（show_viewport_immediate 始终调用），
    /// 进入全屏取景（框选/调整/取窗）时只把它们**移出屏幕**，而不是关闭销毁。
    /// 原因：ViewportCommand::Close 的窗口销毁是异步的（发 WM_CLOSE 后要等
    /// 窗口系统消息循环真正 DestroyWindow），下一帧截图时窗口可能还在屏幕上，
    /// 边框会被截进背景图；而 OuterPosition 是同步 SetWindowPos，帧末应用后
    /// 下一帧截图必然看不到浮层。窗口不销毁也避免了重建闪烁与取窗高亮丢失。
    /// 独立字幕窗：置顶小浮层，只负责显示（原文小字 + 译文大字）。
    /// - deferred viewport → 独立 HWND，沿用“HUD 必须独立窗口”约束；
    /// - 不做窗口透明（GL 无 alpha 时透明会变黑块），用深色实底；
    /// - 整窗可拖动，双击切换迷你态；吸附时跟随游戏窗口（拖完重算偏移）；
    /// - 全屏取景（框选/调整/取窗）时移出屏幕，避免被截进背景图
    ///   （OuterPosition 是同步 SetWindowPos，帧末生效，与边框浮层同策略）。
    fn render_subtitle_window(&mut self, ctx: &egui::Context) {
        let sub = self.sub_data.clone();
        let builder = egui::ViewportBuilder::default()
            .with_title("象胥-字幕")
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_inner_size([380.0, 104.0]);
        ctx.show_viewport_deferred(self.sub_id, builder, move |ui, _class| {
            let ctx = ui.ctx().clone();
            let ppp = ctx.pixels_per_point();
            let mut d = sub.lock().unwrap();
            let rect = ui.max_rect();

            // 全屏取景中：整窗移出屏幕，不绘制内容。
            if d.suppressed {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    -20000.0, -20000.0,
                )));
                return;
            }

            let bg = egui::Color32::from_rgb(16, 20, 26);
            ui.painter().rect_filled(rect, 8.0, bg);

            // 整窗拖动感应
            let resp = ui.interact(
                rect,
                ui.id().with("sub_drag"),
                egui::Sense::click_and_drag(),
            );

            // 吸附时游戏窗口左上角（points）
            let win_min = if d.attach_enabled && d.attach_hwnd != 0 {
                capture::window_rect(d.attach_hwnd)
                    .map(|(l, t, _, _)| egui::pos2(l as f32 / ppp, t as f32 / ppp))
            } else {
                None
            };

            if resp.drag_started() {
                // Windows 原生拖拽（SC_MOVE 模态循环）：丝滑，
                // 替代每帧 OuterPosition+delta 的手动位移（有滞后橡皮筋感）。
                d.dragging = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if d.dragging && !ctx.input(|i| i.pointer.primary_down()) {
                // 原生拖拽结束（松手）：重算相对游戏窗口的偏移，之后继续跟随
                d.dragging = false;
                if let (Some(r), Some(wm)) = (ctx.input(|i| i.viewport().outer_rect), win_min) {
                    d.offset = Some((r.min.x - wm.x, r.min.y - wm.y));
                }
            }
            if resp.double_clicked() {
                d.mini = !d.mini;
                let size = if d.mini {
                    egui::vec2(380.0, 46.0)
                } else {
                    egui::vec2(380.0, 104.0)
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }

            // 跟随游戏窗口：未记录偏移时先贴到游戏窗口左下角
            if !d.dragging
                && let Some(wm) = win_min
            {
                if d.offset.is_none()
                    && let Some((_, _, _, b)) = capture::window_rect(d.attach_hwnd)
                {
                    d.offset = Some((24.0, b as f32 / ppp - 130.0));
                }
                if let Some((ox, oy)) = d.offset {
                    let target = wm + egui::vec2(ox, oy);
                    if let Some(r) = ctx.input(|i| i.viewport().outer_rect)
                        && target.distance(r.min) > 0.5
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(target));
                    }
                }
                // 游戏窗口移动不会触发本窗重绘，需要定期刷新
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            if d.working || d.monitoring {
                ctx.request_repaint_after(Duration::from_millis(200));
            }

            // ---- 内容 ----
            let pad = 12.0;
            let w = rect.width() - 2.0 * pad;
            let show_target = if d.target.trim().is_empty() {
                if d.working { "…" } else { "" }
            } else {
                d.target.as_str()
            };
            if d.mini {
                let line_rect = egui::Rect::from_min_size(
                    rect.min + egui::vec2(pad, (rect.height() - 22.0) / 2.0),
                    egui::vec2(w, 22.0),
                );
                let text = if show_target.is_empty() {
                    &d.source
                } else {
                    show_target
                };
                ui.put(
                    line_rect,
                    egui::Label::new(
                        egui::RichText::new(text)
                            .size(15.0)
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .truncate(),
                );
            } else {
                // 原文（小字灰）
                let src_rect = egui::Rect::from_min_size(
                    rect.min + egui::vec2(pad, 10.0),
                    egui::vec2(w, 16.0),
                );
                ui.put(
                    src_rect,
                    egui::Label::new(
                        egui::RichText::new(&d.source)
                            .size(11.0)
                            .color(egui::Color32::from_rgb(150, 160, 175)),
                    )
                    .truncate(),
                );
                // 译文（大字白，可换行）
                let tgt_rect = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + pad, src_rect.bottom() + 4.0),
                    egui::pos2(rect.right() - pad, rect.bottom() - 24.0),
                );
                ui.put(
                    tgt_rect,
                    egui::Label::new(
                        egui::RichText::new(show_target)
                            .size(15.0)
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .wrap(),
                );
                // 状态（右下角）
                let (t, c) = if d.working {
                    ("…处理中", egui::Color32::from_rgb(230, 165, 60))
                } else if d.monitoring {
                    ("● 监控中", egui::Color32::from_rgb(90, 200, 130))
                } else {
                    ("○ 已停止", egui::Color32::from_gray(120))
                };
                ui.painter().text(
                    egui::pos2(rect.right() - pad, rect.bottom() - 10.0),
                    egui::Align2::RIGHT_BOTTOM,
                    t,
                    egui::FontId::proportional(10.0),
                    c,
                );
            }
        });
    }

    fn render_region_overlay(&self, ctx: &egui::Context) {
        // 用"实际截取区域"：吸附时跟随窗口当前位置
        let Some(r) = self.effective_region() else {
            return;
        };
        if !self.state.show_region_border {
            return;
        }
        let suppressed = self.overlay_suppressed();
        let off_screen = egui::pos2(-20000.0, -20000.0);
        let ppp = ctx.pixels_per_point();
        let rx = r.x as f32 / ppp;
        let ry = r.y as f32 / ppp;
        let rw = r.w as f32 / ppp;
        let rh = r.h as f32 / ppp;
        const B: f32 = 3.0; // 边框厚度(px, points)
        const GREEN: egui::Color32 = egui::Color32::from_rgb(90, 230, 140);
        // 四条边条：水平条向左右各延伸 B，垂直条向上下各延伸 B，四角重叠成实心直角。
        let strips: [(&str, egui::Pos2, egui::Vec2); 4] = [
            ("top", egui::pos2(rx - B, ry), egui::vec2(rw + 2.0 * B, B)),
            (
                "bottom",
                egui::pos2(rx - B, ry + rh - B),
                egui::vec2(rw + 2.0 * B, B),
            ),
            ("left", egui::pos2(rx, ry - B), egui::vec2(B, rh + 2.0 * B)),
            (
                "right",
                egui::pos2(rx + rw - B, ry - B),
                egui::vec2(B, rh + 2.0 * B),
            ),
        ];
        for (name, pos, size) in strips {
            let id = egui::ViewportId::from_hash_of(("xiangxu_border", name));
            let ui_id = egui::Id::new(("xiangxu_border_ui", name));
            ctx.show_viewport_immediate(
                id,
                egui::ViewportBuilder::default()
                    .with_title(format!("象胥-区域边框-{name}"))
                    .with_decorations(false)
                    .with_always_on_top()
                    .with_resizable(false)
                    .with_taskbar(false)
                    .with_close_button(false)
                    .with_inner_size(size),
                move |ctx, _class| {
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(if suppressed {
                        off_screen
                    } else {
                        pos
                    }));
                    ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                    if suppressed {
                        return; // 屏幕外，不绘制内容
                    }
                    let mut ui = egui::Ui::new(
                        ctx.clone(),
                        ui_id,
                        egui::UiBuilder::new()
                            .layer_id(egui::LayerId::background())
                            .max_rect(ctx.content_rect()),
                    );
                    ui.set_clip_rect(ctx.content_rect());
                    ui.painter().rect_filled(ui.max_rect(), 0.0, GREEN);
                },
            );
        }
        // 标签小窗：放在区域左上角外侧（上方，空间不够则下方），不遮挡区域内容。
        let label_w = 92.0_f32;
        let label_h = 22.0_f32;
        let label_pos = if ry - label_h - 2.0 >= 0.0 {
            egui::pos2(rx + 4.0, ry - label_h - 2.0)
        } else {
            egui::pos2(rx + 4.0, ry + rh + 2.0)
        };
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("xiangxu_border_label"),
            egui::ViewportBuilder::default()
                .with_title("象胥-区域标签")
                .with_decorations(false)
                .with_always_on_top()
                .with_resizable(false)
                .with_taskbar(false)
                .with_close_button(false)
                .with_inner_size(egui::vec2(label_w, label_h)),
            move |ctx, _class| {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(if suppressed {
                    off_screen
                } else {
                    label_pos
                }));
                ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                if suppressed {
                    return; // 屏幕外，不绘制内容
                }
                let mut ui = egui::Ui::new(
                    ctx.clone(),
                    egui::Id::new("xiangxu_border_label_ui"),
                    egui::UiBuilder::new()
                        .layer_id(egui::LayerId::background())
                        .max_rect(ctx.content_rect()),
                );
                ui.set_clip_rect(ctx.content_rect());
                let rect = ui.max_rect();
                let p = ui.painter();
                p.rect_filled(rect, 3.0, egui::Color32::from_rgb(15, 45, 30));
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "翻译区域",
                    egui::FontId::proportional(12.0),
                    GREEN,
                );
            },
        );
    }

    /// 「模型」页：模型资源检测 + 一键下载。
    fn render_page_models(&mut self, ui: &mut egui::Ui) {
        let gray = egui::Color32::from_gray(140);
        let green = egui::Color32::from_rgb(0, 150, 0);
        let red = egui::Color32::from_rgb(200, 50, 50);
        let blue = egui::Color32::from_rgb(30, 100, 200);

        // ---- 模型资源：检测缺失 + 一键下载 ----
        {
            let dlp = self.dl_progress.clone();
            let (any_missing, downloading) = {
                let dl = dlp.lock().unwrap();
                (dl.missing_count() > 0, dl.running)
            };
            egui::CollapsingHeader::new(format!(
                "模型资源{}",
                if any_missing && !downloading {
                    "（有缺失）"
                } else {
                    ""
                }
            ))
            .default_open(any_missing)
            .show(ui, |ui| {
                let mut start = false;
                let dl = dlp.lock().unwrap();
                for (i, r) in download::RESOURCES.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let status = if dl.done[i] {
                            egui::RichText::new("✓ 已就绪").color(green)
                        } else if dl.current == Some(i) {
                            egui::RichText::new(format!(
                                "下载中 {:.0}%",
                                dl.current_fraction() * 100.0
                            ))
                            .color(blue)
                        } else {
                            egui::RichText::new("未下载").color(red)
                        };
                        ui.label(r.name);
                        ui.label(egui::RichText::new(r.size_hint).size(11.0).color(gray));
                        ui.label(status);
                    });
                }
                // 当前文件进度条
                if dl.running {
                    if let Some(i) = dl.current {
                        ui.add(
                            egui::ProgressBar::new(dl.current_fraction())
                                .text(format!(
                                    "{}  {}",
                                    download::RESOURCES[i].name,
                                    dl.bytes_text()
                                ))
                                .desired_width(ui.available_width()),
                        );
                    }
                } else if dl.missing_count() > 0 {
                    if ui.button("一键下载缺失模型").clicked() {
                        start = true;
                    }
                } else {
                    ui.label(egui::RichText::new("✓ 全部模型已就绪，可直接使用。").color(green));
                }
                if let Some(err) = &dl.error {
                    ui.label(
                        egui::RichText::new(format!("下载失败：{}", err))
                            .color(red)
                            .size(11.0),
                    );
                }
                drop(dl);
                ui.label(
                    egui::RichText::new(
                        "模型来自 Hugging Face 官方仓库（Qwen / ggml-org，Apache-2.0）。\
                         下载完成后自动生效，无需重启。",
                    )
                    .size(11.0)
                    .color(gray),
                );
                if start {
                    download::start_download(dlp);
                }
            });
        }

        // 下载进行中：每帧请求重绘以刷新进度条
        if self.dl_progress.lock().map(|p| p.running).unwrap_or(false) {
            ui.ctx().request_repaint();
        }
    }

    /// 「翻译」页：语言与运行模式 + 远程 API / 本地模型后端配置。
    fn render_page_translate(&mut self, ui: &mut egui::Ui) {
        let gray = egui::Color32::from_gray(140);

        egui::CollapsingHeader::new("语言与运行模式")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    egui::ComboBox::from_label("源语言")
                        .selected_text(self.state.source_lang.label())
                        .show_ui(ui, |ui| {
                            for l in [lang::Lang::En, lang::Lang::Zh, lang::Lang::Ja] {
                                ui.selectable_value(&mut self.state.source_lang, l, l.label());
                            }
                        });
                    egui::ComboBox::from_label("目标语言")
                        .selected_text(self.state.target_lang.label())
                        .show_ui(ui, |ui| {
                            for l in [lang::Lang::Zh, lang::Lang::En, lang::Lang::Ja] {
                                ui.selectable_value(&mut self.state.target_lang, l, l.label());
                            }
                        });
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("翻译后端:");
                    ui.radio_value(&mut self.state.translator.mode, AppMode::Mock, "模拟");
                    ui.radio_value(
                        &mut self.state.translator.mode,
                        AppMode::LocalVisionLocal,
                        "本地模型",
                    );
                    ui.radio_value(
                        &mut self.state.translator.mode,
                        AppMode::LocalVisionRemote,
                        "远程API",
                    );
                });
                ui.label(
                    egui::RichText::new("识图统一由本地视觉模型完成；运行模式只影响翻译后端。")
                        .size(11.0)
                        .color(gray),
                );
            });

        if self.state.translator.mode == AppMode::LocalVisionRemote {
            egui::CollapsingHeader::new("远程翻译 API").show(ui, |ui| {
                // 服务商一键预设：点一下填好端点+模型并切到远程翻译模式
                ui.label("服务商预设（点选自动填好端点与模型）：");
                ui.horizontal_wrapped(|ui| {
                    if ui.button("DeepSeek").clicked() {
                        self.state.translator.endpoint = "https://api.deepseek.com".to_string();
                        self.state.translator.model = "deepseek-v4-flash".to_string();
                        self.state.translator.mode = AppMode::LocalVisionRemote;
                        self.sync_config();
                    }
                    if ui.button("OpenAI").clicked() {
                        self.state.translator.endpoint = "https://api.openai.com/v1".to_string();
                        self.state.translator.model = "gpt-4o-mini".to_string();
                        self.state.translator.mode = AppMode::LocalVisionRemote;
                        self.sync_config();
                    }
                    if ui.button("通义千问").clicked() {
                        self.state.translator.endpoint =
                            "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string();
                        self.state.translator.model = "qwen-plus".to_string();
                        self.state.translator.mode = AppMode::LocalVisionRemote;
                        self.sync_config();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("端点(base_url)");
                    ui.text_edit_singleline(&mut self.state.translator.endpoint);
                });
                ui.label(
                    egui::RichText::new("可填 base_url 或完整 URL；程序自动补 /chat/completions")
                        .size(11.0)
                        .color(gray),
                );

                egui::ComboBox::from_label("模型")
                    .selected_text(&self.state.translator.model)
                    .show_ui(ui, |ui| {
                        for m in [
                            "deepseek-v4-flash",
                            "deepseek-v4-pro",
                            "deepseek-chat",
                            "deepseek-reasoner",
                            "gpt-4o-mini",
                            "gpt-4o",
                            "qwen-plus",
                            "qwen-max",
                        ] {
                            ui.selectable_value(&mut self.state.translator.model, m.to_string(), m);
                        }
                    });
                ui.horizontal(|ui| {
                    ui.label("模型(自定义)");
                    ui.text_edit_singleline(&mut self.state.translator.model);
                });

                ui.horizontal(|ui| {
                    ui.label("API Key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.state.translator.api_key)
                            .password(true),
                    );
                });
                ui.label(
                    egui::RichText::new("支持 OpenAI 兼容端点；未填 Key 时翻译回退到模拟。")
                        .size(11.0)
                        .color(gray),
                );
            });
        }

        if self.state.translator.mode == AppMode::LocalVisionLocal {
            egui::CollapsingHeader::new("本地翻译模型").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("服务端exe");
                    ui.text_edit_singleline(&mut self.state.translator.local_server);
                });
                ui.horizontal(|ui| {
                    ui.label("模型.gguf");
                    ui.text_edit_singleline(&mut self.state.translator.local_model);
                });
                ui.horizontal(|ui| {
                    ui.label("端口");
                    ui.add(
                        egui::DragValue::new(&mut self.state.translator.local_port)
                            .range(1024..=65535),
                    );
                    ui.label("GPU层数(-ngl)");
                    ui.add(
                        egui::DragValue::new(&mut self.state.translator.local_ngl)
                            .range(0..=99),
                    );
                });
                ui.label(
                    egui::RichText::new(
                        "服务端自动选择：llama-cuda（CUDA 13.3 版 llama-server.exe）；识图/翻译共用该 exe。",
                    )
                    .size(11.0)
                    .color(gray),
                );
                ui.label(
                    egui::RichText::new(
                        "需要 NVIDIA 显卡。缺 CUDA 运行库时双击 download_cuda_runtime.ps1 一键补装。",
                    )
                    .size(11.0)
                    .color(gray),
                );
            });
        }

        ui.separator();
        ui.label(
            egui::RichText::new("设置修改即时生效，无需手动保存。")
                .size(11.0)
                .color(gray),
        );
    }

    /// 「识图」页：视觉模型（Qwen2.5-VL）配置。
    fn render_page_ocr(&mut self, ui: &mut egui::Ui) {
        let gray = egui::Color32::from_gray(140);
        egui::CollapsingHeader::new("识图（视觉模型 Qwen2.5-VL）")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("VL模型.gguf");
                    ui.text_edit_singleline(&mut self.state.translator.vl_model);
                });
                ui.horizontal(|ui| {
                    ui.label("mmproj");
                    ui.text_edit_singleline(&mut self.state.translator.vl_mmproj);
                });
                ui.horizontal(|ui| {
                    ui.label("端口");
                    ui.add(
                        egui::DragValue::new(&mut self.state.translator.vl_port)
                            .range(1024..=65535),
                    );
                    ui.label("GPU层数(-ngl)");
                    ui.add(egui::DragValue::new(&mut self.state.translator.vl_ngl).range(0..=99));
                });
                ui.horizontal(|ui| {
                    ui.label("识别长边上限(px)");
                    ui.add(
                        egui::DragValue::new(&mut self.state.translator.vl_max_side)
                            .range(256..=2048),
                    );
                });
                ui.label(
                    egui::RichText::new("长边调小(如 640/512)显著加快识图；-ngl=99 走 GPU 更快。")
                        .size(11.0)
                        .color(gray),
                );
            });
    }

    /// 框选/区域编辑全屏界面：截图背景 + 已有区域高亮（绿，可拖动/缩放/双击清除）
    /// + 新框预览（青，松手确认覆盖）。无区域时退化为纯框选。
    fn render_selection(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.global_style())
                    .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
            )
            .show_inside(ui, |ui| {
                // 关键：先把“整块区域”作为拖拽感应区分配出来，
                // 再把截图当作背景画进同一块 rect（用 put，不推进布局）。
                let available = ui.available_size();
                let (rect, _response) =
                    ui.allocate_exact_size(available, egui::Sense::click_and_drag());

                if let Some(tex) = &self.state.bg_texture {
                    ui.put(rect, egui::Image::from_texture(tex).max_size(rect.size()));
                }

                let ppp = ctx.pixels_per_point();
                let off = self.state.sel_monitor_offset;

                // 首帧：已有区域 → 换算成 UI 坐标进入编辑模式（高亮 + 可拖动）
                if self.state.adjust_rect.is_none() && self.state.region.is_some() {
                    let r = self.state.region.unwrap();
                    self.state.adjust_rect = Some(egui::Rect::from_min_size(
                        egui::pos2((r.x - off.0) as f32 / ppp, (r.y - off.1) as f32 / ppp),
                        egui::vec2(r.w as f32 / ppp, r.h as f32 / ppp),
                    ));
                }

                let pointer = ctx.input(|i| i.pointer.interact_pos());

                // 双击已有区域内部 = 清除区域并退出
                if ctx.input(|i| {
                    i.pointer
                        .button_double_clicked(egui::PointerButton::Primary)
                }) && let Some(p) = pointer
                    && let Some(cr) = self.state.adjust_rect
                    && cr.contains(p)
                {
                    self.clear_region();
                    self.state.selecting = false;
                    self.state.sel_start = None;
                    self.state.sel_end = None;
                    self.state.adjust_rect = None;
                    self.state.adjust_drag = None;
                    self.state.adjust_start_ptr = None;
                    self.state.adjust_start_rect = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                    return;
                }

                // 按下分派：命中已有区域（手柄/内部）→ 调整拖拽；空白 → 画新框
                if ctx.input(|i| i.pointer.primary_pressed())
                    && let Some(p) = pointer
                {
                    if let Some(cr) = self.state.adjust_rect
                        && let Some(h) = hit_test_adjust_handle(&cr, p)
                    {
                        self.state.adjust_drag = Some(h);
                        self.state.adjust_start_ptr = Some(p);
                        self.state.adjust_start_rect = Some(cr);
                        self.state.sel_start = None;
                        self.state.sel_end = None;
                    } else {
                        self.state.sel_start = Some(p);
                        self.state.sel_end = Some(p);
                    }
                }
                // 拖动中：调整（移动/缩放）或新框预览
                if ctx.input(|i| i.pointer.primary_down())
                    && let Some(p) = pointer
                {
                    if let (Some(h), Some(sp), Some(sr)) = (
                        self.state.adjust_drag,
                        self.state.adjust_start_ptr,
                        self.state.adjust_start_rect,
                    ) {
                        self.state.adjust_rect = Some(apply_adjust(sr, h, p - sp));
                    } else if self.state.sel_start.is_some() {
                        self.state.sel_end = Some(p);
                    }
                }
                // 松手：新框 → 确认覆盖退出；调整 → 改动就地生效，继续编辑
                if ctx.input(|i| i.pointer.primary_released()) {
                    let was_adjust = self.state.adjust_drag.is_some();
                    self.state.adjust_drag = None;
                    self.state.adjust_start_ptr = None;
                    self.state.adjust_start_rect = None;
                    if was_adjust {
                        self.state.sel_start = None;
                        self.state.sel_end = None;
                    } else if self.state.sel_start.is_some() && self.state.sel_end.is_some() {
                        self.confirm_selection(&ctx);
                    }
                }

                // 方向键微调（Shift 加速）——仅编辑模式
                let step = if ctx.input(|i| i.modifiers.shift) {
                    10.0
                } else {
                    1.0
                };
                let mut nudge = egui::Vec2::ZERO;
                for (key, v) in [
                    (egui::Key::ArrowLeft, egui::vec2(-step, 0.0)),
                    (egui::Key::ArrowRight, egui::vec2(step, 0.0)),
                    (egui::Key::ArrowUp, egui::vec2(0.0, -step)),
                    (egui::Key::ArrowDown, egui::vec2(0.0, step)),
                ] {
                    if ctx.input(|i| i.key_pressed(key)) {
                        nudge += v;
                    }
                }
                if nudge != egui::Vec2::ZERO
                    && let Some(r) = self.state.adjust_rect
                {
                    self.state.adjust_rect = Some(r.translate(nudge));
                }

                // ---- 绘制：已有区域（绿色 + 8 手柄 + 尺寸）----
                if let Some(cr) = self.state.adjust_rect {
                    let green = egui::Color32::from_rgb(90, 230, 140);
                    ui.painter().rect_stroke(
                        cr,
                        2.0,
                        egui::Stroke::new(2.0_f32, green),
                        egui::StrokeKind::Outside,
                    );
                    for h in [
                        AdjustHandle::TopLeft,
                        AdjustHandle::Top,
                        AdjustHandle::TopRight,
                        AdjustHandle::Right,
                        AdjustHandle::BottomRight,
                        AdjustHandle::Bottom,
                        AdjustHandle::BottomLeft,
                        AdjustHandle::Left,
                    ] {
                        if let Some(r) = adjust_handle_rect(&cr, h) {
                            ui.painter().rect_filled(r, 2.0, green);
                        }
                    }
                    let info = format!(
                        "区域: {}×{} @ ({},{})",
                        (cr.width() * ppp).round() as i32,
                        (cr.height() * ppp).round() as i32,
                        (cr.min.x * ppp) as i32 + off.0,
                        (cr.min.y * ppp) as i32 + off.1,
                    );
                    ui.painter().text(
                        egui::pos2(cr.left(), (cr.top() - 26.0).max(4.0)),
                        egui::Align2::LEFT_TOP,
                        info,
                        egui::FontId::proportional(14.0),
                        green,
                    );
                }

                // ---- 绘制：新框预览（青色，双层描边 + 四角标记 + 尺寸）----
                if let (Some(a), Some(b)) = (self.state.sel_start, self.state.sel_end) {
                    let r = egui::Rect::from_two_pos(a, b);
                    // 双层描边：黑底 4px + 亮青 2px，任何游戏画面上都清晰可辨
                    let cyan = egui::Color32::from_rgb(0, 229, 255);
                    ui.painter().rect(
                        r,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 90),
                        egui::Stroke::new(4.0_f32, egui::Color32::from_black_alpha(210)),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().rect(
                        r,
                        0.0,
                        egui::Color32::TRANSPARENT,
                        egui::Stroke::new(2.0_f32, cyan),
                        egui::StrokeKind::Inside,
                    );
                    // 四角实心标记，进一步强化定位
                    let c = 9.0;
                    let corners = [
                        r.left_top(),
                        r.right_top(),
                        r.left_bottom(),
                        r.right_bottom(),
                    ];
                    for p in corners {
                        ui.painter().rect_filled(
                            egui::Rect::from_center_size(p, egui::vec2(c, c)),
                            1.5,
                            cyan,
                        );
                    }
                    // 尺寸标签（换算为物理像素，与最终区域一致）
                    let (w, h) = ((r.width() * ppp) as i32, (r.height() * ppp) as i32);
                    let label = format!("{w}×{h}");
                    let font = egui::FontId::proportional(13.0);
                    let text_pos = egui::pos2(r.right_bottom().x + 8.0, r.right_bottom().y + 4.0);
                    ui.painter().text(
                        text_pos,
                        egui::Align2::LEFT_TOP,
                        label,
                        font,
                        egui::Color32::WHITE,
                    );
                }

                // ---- 顶部居中提示条（白字带阴影，任何背景可读）----
                let tip = if self.state.adjust_rect.is_some() {
                    "拖动移动 / 拖角边缩放 / 空白处拖出新框 / 双击区域清除 / Esc 完成"
                } else {
                    "拖拽框选要翻译的区域，松开确认（Esc 取消）"
                };
                let tip_pos = egui::pos2(rect.center().x, rect.min.y + 18.0);
                let tip_font = egui::FontId::proportional(16.0);
                ui.painter().text(
                    tip_pos + egui::vec2(1.0, 1.0),
                    egui::Align2::CENTER_TOP,
                    tip,
                    tip_font.clone(),
                    egui::Color32::from_black_alpha(170),
                );
                ui.painter().text(
                    tip_pos,
                    egui::Align2::CENTER_TOP,
                    tip,
                    tip_font,
                    egui::Color32::WHITE,
                );

                // Enter = 完成编辑（写回调整结果并退出）
                if ctx.input(|i| i.key_pressed(egui::Key::Enter))
                    && self.state.adjust_rect.is_some()
                {
                    self.write_back_adjust(&ctx);
                    self.state.selecting = false;
                    self.state.sel_start = None;
                    self.state.sel_end = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                }
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel_selection(&ctx);
        }
    }
}

impl eframe::App for XiangxuApp {
    fn on_exit(&mut self) {
        // 退出时回收本地模型服务端，避免 llama-server 进程残留占用显存/端口。
        crate::translate::kill_local_server();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 同步一次配置（区域/语言/后端可能已变更）
        self.sync_config();

        // 取出后台线程的结果
        while let Ok(r) = self.rx.try_recv() {
            self.state.push_result(r);
        }

        // 字幕窗（独立 HWND viewport）：同步显示数据；全屏取景时函数内部移出屏幕。
        if let Ok(mut d) = self.sub_data.lock() {
            d.source.clone_from(&self.state.source_text);
            d.target.clone_from(&self.state.target_text);
            d.working = self.state.ocr_working || self.state.translate_working;
            d.monitoring = self.state.monitoring;
            d.attach_enabled = self.state.attach_enabled && self.state.attach_hwnd != 0;
            d.attach_hwnd = self.state.attach_hwnd;
            d.suppressed = self.overlay_suppressed();
        }
        if self.sub_visible {
            self.render_subtitle_window(ui.ctx());
        }

        // 处理挂起的全屏进入请求：上一帧已关闭区域边框浮层，
        // 此时截屏背景干净，真正进入框选/调整/取窗模式。
        if let Some(mode) = self.state.pending_mode.take() {
            match mode {
                PendingFullscreenMode::Selection => self.enter_selection_now(ui.ctx()),
                PendingFullscreenMode::Adjust => self.start_adjust_now(ui.ctx()),
                PendingFullscreenMode::Pick => self.start_attach_pick_now(ui.ctx()),
            }
        }

        // 吸附游戏窗口：跟随窗口移动（每帧，取窗/框选/调整时暂停）
        if !self.state.attach_picking && !self.state.selecting && !self.state.adjusting {
            self.update_attach(ui.ctx());
        }

        // 区域边框浮层：透明置顶小窗，标出框选区域位置（吸附时跟随窗口）。
        // 每帧都渲染以保持窗口存活；全屏取景时函数内部会移到屏幕外，
        // 避免被截进背景图（详见 render_region_overlay 注释）。
        self.render_region_overlay(ui.ctx());

        if self.state.selecting {
            self.render_selection(ui);
        } else if self.state.attach_picking {
            self.render_picker(ui);
        } else if self.state.adjusting {
            self.render_adjust(ui);
        } else if self.bar_mode {
            self.render_toolbar(ui);
        } else {
            self.render_ball(ui);
        }

        // 设置窗（独立 HWND viewport）：与主窗口形态无关，随时可弹
        if self.settings_open {
            self.render_settings_window(ui.ctx());
        }

        // 贴边缩进：鼠标靠近滑出 / 离开缩回
        self.update_dock(ui.ctx());

        // 监控中时保持刷新
        if self.state.monitoring {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
    }
}

// ---------- 吸附游戏窗口：Windows 窗口 API 辅助 ----------

/// 手柄在矩形上的位置（10px 见方小方块）。
fn adjust_handle_rect(rect: &egui::Rect, h: AdjustHandle) -> Option<egui::Rect> {
    const S: f32 = 10.0;
    let c = match h {
        AdjustHandle::Move => return None,
        AdjustHandle::TopLeft => rect.min,
        AdjustHandle::Top => egui::pos2(rect.center().x, rect.min.y),
        AdjustHandle::TopRight => egui::pos2(rect.max.x, rect.min.y),
        AdjustHandle::Right => egui::pos2(rect.max.x, rect.center().y),
        AdjustHandle::BottomRight => rect.max,
        AdjustHandle::Bottom => egui::pos2(rect.center().x, rect.max.y),
        AdjustHandle::BottomLeft => egui::pos2(rect.min.x, rect.max.y),
        AdjustHandle::Left => egui::pos2(rect.min.x, rect.center().y),
    };
    Some(egui::Rect::from_center_size(c, egui::Vec2::splat(S)))
}

/// 命中检测：先角、再边、再内部（整体移动）。
fn hit_test_adjust_handle(rect: &egui::Rect, p: egui::Pos2) -> Option<AdjustHandle> {
    for h in [
        AdjustHandle::TopLeft,
        AdjustHandle::TopRight,
        AdjustHandle::BottomRight,
        AdjustHandle::BottomLeft,
        AdjustHandle::Top,
        AdjustHandle::Bottom,
        AdjustHandle::Left,
        AdjustHandle::Right,
    ] {
        if let Some(r) = adjust_handle_rect(rect, h) {
            if r.expand(5.0).contains(p) {
                return Some(h);
            }
        }
    }
    if rect.contains(p) {
        Some(AdjustHandle::Move)
    } else {
        None
    }
}

/// 按拖动的手柄与增量计算新矩形（最小 8px）。
fn apply_adjust(start: egui::Rect, h: AdjustHandle, delta: egui::Vec2) -> egui::Rect {
    const MIN: f32 = 8.0;
    if h == AdjustHandle::Move {
        return start.translate(delta);
    }
    let mut r = start;
    let (mut left, mut right, mut top, mut bottom) = (false, false, false, false);
    match h {
        AdjustHandle::TopLeft => {
            left = true;
            top = true;
        }
        AdjustHandle::Top => top = true,
        AdjustHandle::TopRight => {
            right = true;
            top = true;
        }
        AdjustHandle::Right => right = true,
        AdjustHandle::BottomRight => {
            right = true;
            bottom = true;
        }
        AdjustHandle::Bottom => bottom = true,
        AdjustHandle::BottomLeft => {
            left = true;
            bottom = true;
        }
        AdjustHandle::Left => left = true,
        AdjustHandle::Move => {}
    }
    if left {
        r.min.x = (start.min.x + delta.x).min(start.max.x - MIN);
    }
    if right {
        r.max.x = (start.max.x + delta.x).max(start.min.x + MIN);
    }
    if top {
        r.min.y = (start.min.y + delta.y).min(start.max.y - MIN);
    }
    if bottom {
        r.max.y = (start.max.y + delta.y).max(start.min.y + MIN);
    }
    r
}

/// 按字符数截断字符串，超长时加省略号（安全处理多字节字符）。
fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        chars[..max].iter().collect::<String>() + "…"
    }
}

/// 把 "qwen2.5-1.5b-instruct-q4_k_m" 这类模型文件名缩写为 "qwen2.5-1.5b"（前两段）。
fn short_model_name(stem: &str) -> String {
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 2 {
        format!("{}-{}", parts[0], parts[1])
    } else {
        stem.to_string()
    }
}

/// 原文/译文卡片：标题 + 工作状态 + 复制按钮 + 多行只读文本框。
fn render_text_card(
    ui: &mut egui::Ui,
    title: &str,
    text: &mut String,
    working: bool,
    working_hint: &str,
    copy_source: bool,
) {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).strong());
            if working {
                ui.label(
                    egui::RichText::new(working_hint)
                        .size(11.0)
                        .color(egui::Color32::from_rgb(230, 165, 60)),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("复制").clicked() {
                    let s = text.clone();
                    if let Ok(mut clip) = arboard::Clipboard::new() {
                        let _ = clip.set_text(s);
                    }
                }
            });
        });
        let rows = if copy_source { 2 } else { 3 };
        ui.add(
            egui::TextEdit::multiline(text)
                .interactive(false)
                .desired_rows(rows)
                .hint_text(if copy_source {
                    "等待识别…"
                } else {
                    "等待翻译…"
                }),
        );
    });
}

/// 判断窗口句柄是否为可吸附的合法目标：
/// 非桌面（Progman/WorkerW）、非本翻译面板、非最小化、有非零尺寸。
#[cfg(windows)]
fn is_valid_attach_target(hwnd: isize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;
    if capture::window_iconic(hwnd) {
        return false;
    }
    if let Some((l, t, r, b)) = capture::window_rect(hwnd) {
        if r <= l || b <= t {
            return false;
        }
    } else {
        return false;
    }
    // 桌面/外壳窗口
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd as _, buf.as_mut_ptr(), buf.len() as i32) };
    if n > 0 {
        let cls = String::from_utf16_lossy(&buf[..n as usize]);
        if cls == "Progman" || cls == "WorkerW" {
            return false;
        }
    }
    // 翻译面板自身（按窗口标题识别）
    if window_title(hwnd).contains("象胥") {
        return false;
    }
    true
}

#[cfg(not(windows))]
fn is_valid_attach_target(_hwnd: isize) -> bool {
    false
}

/// 枚举所有顶层窗口（按 Z 序，最上层优先），返回包含点 (x, y) 的
/// **第一个可见、未最小化、非桌面、非本应用**的窗口句柄。
/// 用于取窗模式：全屏取景时 WindowFromPoint 只会返回本应用自己的全屏窗口，
/// 必须枚举窗口并跳过"象胥"系窗口才能找到下面的真实窗口。
#[cfg(windows)]
fn find_visible_window_at(x: i32, y: i32) -> Option<isize> {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowRect, GetWindowTextW, IsIconic, IsWindowVisible,
    };
    // 回调通过 lparam 读写 (x, y, result)
    let mut data: (i32, i32, isize) = (x, y, 0);
    unsafe extern "system" fn proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let (x, y, result) = &mut *(lparam as *mut (i32, i32, isize));
        if *result != 0 {
            return 0; // 已找到
        }
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return 1;
        }
        let mut r = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut r) == 0
            || *x < r.left
            || *x >= r.right
            || *y < r.top
            || *y >= r.bottom
        {
            return 1;
        }
        // 排除桌面/外壳
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n > 0 {
            let cls = String::from_utf16_lossy(&buf[..n as usize]);
            if cls == "Progman" || cls == "WorkerW" {
                return 1;
            }
        }
        // 排除本应用（主面板 + 区域边框浮层等，标题都含"象胥"）
        let mut tbuf = [0u16; 128];
        let tn = GetWindowTextW(hwnd, tbuf.as_mut_ptr(), tbuf.len() as i32);
        if tn > 0 {
            let title = String::from_utf16_lossy(&tbuf[..tn as usize]);
            if title.contains("象胥") {
                return 1;
            }
        }
        *result = hwnd as isize;
        0 // 停止枚举
    }
    unsafe {
        EnumWindows(Some(proc), &mut data as *mut _ as LPARAM);
    }
    if data.2 != 0 { Some(data.2) } else { None }
}

/// 全局鼠标位置（物理像素）；失败返回 None。
#[cfg(windows)]
fn cursor_pos_physical() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut pt) } != 0 {
        Some((pt.x, pt.y))
    } else {
        None
    }
}

/// 窗口标题（截断到 40 字符，供界面显示）。
#[cfg(windows)]
fn window_title(hwnd: isize) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    let mut buf = [0u16; 128];
    let n = unsafe { GetWindowTextW(hwnd as _, buf.as_mut_ptr(), buf.len() as i32) };
    if n > 0 {
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        s.chars().take(40).collect()
    } else {
        String::new()
    }
}

#[cfg(not(windows))]
fn find_visible_window_at(_x: i32, _y: i32) -> Option<isize> {
    None
}
#[cfg(not(windows))]
fn cursor_pos_physical() -> Option<(i32, i32)> {
    None
}
#[cfg(not(windows))]
fn window_title(_hwnd: isize) -> String {
    String::new()
}
