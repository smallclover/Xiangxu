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
mod lang;
mod monitor;
mod ocr;
mod resources;
mod state;
mod translate;

use capture::ScreenRect;
use state::{AdjustHandle, MonitorConfig, TranslateResult, TranslateState};
use translate::AppMode;

fn main() -> Result<(), eframe::Error> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("象胥 实时翻译")
            .with_inner_size([400.0, 520.0])
            .with_min_inner_size([340.0, 300.0])
            .with_always_on_top()
            .with_decorations(true),
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

struct XiangxuApp {
    state: TranslateState,
    config: Arc<Mutex<MonitorConfig>>,
    rx: mpsc::Receiver<TranslateResult>,
    tx: mpsc::Sender<TranslateResult>,
    /// 模型下载进度（设置面板的“模型资源”区读取）。
    dl_progress: Arc<Mutex<download::DownloadProgress>>,
}

impl XiangxuApp {
    fn new(ctx: &egui::Context) -> Self {
        // 启动即清理上一次（可能崩溃）残留的 llama-server 进程，回收 CPU/显存，
        // 避免软件一打开就因残留的 3B 视觉模型仍在加载而整体卡顿。
        translate::cleanup_orphan_servers();
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
            self.state
                .last_error = Some("请先点“选择区域”框选要翻译的屏幕范围".into());
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

    fn enter_selection(&mut self, ctx: &egui::Context) {
        if self.enter_fullscreen_bg(ctx) {
            self.state.selecting = true;
            self.state.sel_start = None;
            self.state.sel_end = None;
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
            self.state.selecting = false;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            self.sync_config();
        }
    }

    fn cancel_selection(&mut self, ctx: &egui::Context) {
        self.state.selecting = false;
        self.state.sel_start = None;
        self.state.sel_end = None;
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
    }

    // ---------- 取窗吸附（P0）----------

    /// 进入取窗模式：全屏取景，移动鼠标实时高亮目标窗口，左键确认、右键/Esc 取消。
    fn start_attach_pick(&mut self, ctx: &egui::Context) {
        // 先记住面板当前（未全屏）的位置，作为吸附偏移的基准
        self.state.pick_panel_pos = ctx.input(|i| i.viewport().outer_rect).map(|r| r.min);
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
                        egui::RichText::new("移动鼠标选择要吸附的窗口（橙色高亮），左键确认，右键或 Esc 取消")
                            .color(egui::Color32::WHITE)
                            .size(18.0),
                    ),
                );

                // 实时高亮鼠标下的窗口（用全局光标位置 + 枚举可见窗口，
                // 不能用 WindowFromPoint——全屏取景时它只会返回本应用自己的窗口）
                let ppp = ctx.pixels_per_point();
                let off = self.state.sel_monitor_offset;
                self.state.pick_hovered = cursor_pos_physical()
                    .and_then(|(px, py)| find_visible_window_at(px, py));
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

    /// 进入区域调整模式：全屏取景，显示现有区域，拖内部移动、拖角/边缩放。
    fn start_adjust(&mut self, ctx: &egui::Context) {
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

    /// 确认调整：把调整后的矩形写回区域（物理坐标），并重算吸附偏移。
    fn confirm_adjust(&mut self, ctx: &egui::Context) {
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
        }
        self.state.adjusting = false;
        self.state.adjust_rect = None;
        self.state.adjust_drag = None;
        self.state.adjust_start_ptr = None;
        self.state.adjust_start_rect = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        self.sync_config();
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
                                egui::pos2(
                                    (r.x - off.0) as f32 / ppp,
                                    (r.y - off.1) as f32 / ppp,
                                ),
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
                let step = if ctx.input(|i| i.modifiers.shift) { 10.0 } else { 1.0 };
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

    fn render_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(ui.style())
                    .inner_margin(egui::Margin::symmetric(10, 8)),
            )
            .show_inside(ui, |ui| {
                self.render_panel_body(ui);
            });
    }

    /// 面板主体（纵向布局）。
    fn render_panel_body(&mut self, ui: &mut egui::Ui) {
        // 顶栏：标题 + 监控状态
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("象胥").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, color) = if self.state.monitoring {
                    ("● 监控中", egui::Color32::from_rgb(70, 200, 120))
                } else {
                    ("○ 已停止", egui::Color32::GRAY)
                };
                ui.label(egui::RichText::new(text).color(color).strong());
            });
        });

        // 后端摘要 + 吸附指示
        // 后端摘要（单独一行，避免与吸附状态挤在同一行而重叠）
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(self.backend_hint())
                    .size(11.0)
                    .color(egui::Color32::from_gray(140)),
            );
        });
        // 吸附状态（独立一行；标题过长时截断显示）
        if self.state.attach_enabled {
            let t = if self.state.attach_title.trim().is_empty() {
                "已吸附窗口".to_string()
            } else {
                format!("已吸附: {}", truncate_chars(&self.state.attach_title, 18))
            };
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(t)
                        .size(11.0)
                        .color(egui::Color32::from_rgb(120, 170, 230)),
                );
            });
        }
        ui.separator();

        // 原文 / 译文卡片
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

        // 操作按钮
        ui.horizontal_wrapped(|ui| {
            let (label, fill) = if self.state.monitoring {
                ("■ 停止监控", egui::Color32::from_rgb(190, 80, 80))
            } else {
                ("▶ 开始监控", egui::Color32::from_rgb(70, 160, 90))
            };
            if ui
                .add(
                    egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE))
                        .fill(fill),
                )
                .clicked()
            {
                self.toggle_monitoring();
            }
            if ui.button("选择区域").clicked() {
                let ctx = ui.ctx().clone();
                self.enter_selection(&ctx);
            }
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
            let attach_label = if self.state.attach_enabled {
                "取消吸附"
            } else {
                "吸附窗口"
            };
            if ui.button(attach_label).clicked() {
                let ctx = ui.ctx().clone();
                if self.state.attach_enabled {
                    // 取消吸附：区域定格在当前实际位置（不再跟随），避免跳回旧坐标
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
                    // 进入取窗模式：移动鼠标高亮目标窗口，点击确认吸附
                    self.start_attach_pick(&ctx);
                }
            }
            if ui.button("复制译文").clicked() {
                self.copy_target();
            }
            if ui.button("立即翻译").clicked() {
                let ctx = ui.ctx().clone();
                self.force_translate(&ctx);
            }
            if ui.button("清空").clicked() {
                self.state.source_text.clear();
                self.state.target_text.clear();
                self.state.ocr_working = false;
                self.state.translate_working = false;
            }
        });

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

        // 区域信息 + 边框显示开关（吸附时显示实际截取位置）
        ui.horizontal_wrapped(|ui| {
            match self.effective_region() {
                Some(r) => {
                    let tag = if self.state.attach_enabled { "（跟随窗口）" } else { "" };
                    ui.label(
                        egui::RichText::new(format!(
                            "区域: {}×{} @ ({},{}){}",
                            r.w, r.h, r.x, r.y, tag
                        ))
                        .size(11.0)
                        .color(egui::Color32::from_gray(140)),
                    );
                }
                None => {
                    ui.label(
                        egui::RichText::new("尚未框选区域（点「选择区域」选游戏对话框）")
                            .size(11.0)
                            .color(egui::Color32::from_rgb(220, 150, 60)),
                    );
                }
            }
            ui.checkbox(&mut self.state.show_region_border, "显示边框");
        });

        ui.separator();
        // 设置 / 历史 / 错误（底部滚动区）
        egui::ScrollArea::vertical()
            .max_height(ui.available_height().max(40.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::CollapsingHeader::new("高级设置 / API")
                    .default_open(false)
                    .show(ui, |ui| {
                        self.render_settings(ui);
                    });
                ui.separator();
                egui::CollapsingHeader::new("历史记录").show(ui, |ui| {
                    if self.state.history.is_empty() {
                        ui.label(
                            egui::RichText::new("暂无记录")
                                .size(11.0)
                                .color(egui::Color32::from_gray(140)),
                        );
                    }
                    for h in self.state.history.iter().rev().take(20) {
                        ui.label(
                            egui::RichText::new(format!("{}  →  {}", h.source, h.target))
                                .size(12.0),
                        );
                        ui.separator();
                    }
                });
                if let Some(err) = &self.state.last_error {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err);
                }
            });
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
            self.state
                .last_error = Some("吸附的游戏窗口已关闭，已自动取消吸附".into());
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
    fn render_region_overlay(&self, ctx: &egui::Context) {
        // 用"实际截取区域"：吸附时跟随窗口当前位置
        let Some(r) = self.effective_region() else { return };
        if !self.state.show_region_border {
            return;
        }
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
            ("bottom", egui::pos2(rx - B, ry + rh - B), egui::vec2(rw + 2.0 * B, B)),
            ("left", egui::pos2(rx, ry - B), egui::vec2(B, rh + 2.0 * B)),
            ("right", egui::pos2(rx + rw - B, ry - B), egui::vec2(B, rh + 2.0 * B)),
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                    ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
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
            |ctx, _class| {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(label_pos));
                ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
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

    fn render_settings(&mut self, ui: &mut egui::Ui) {
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
                if any_missing && !downloading { "（有缺失）" } else { "" }
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
                    ui.label(
                        egui::RichText::new("✓ 全部模型已就绪，可直接使用。")
                            .color(green),
                    );
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
        if self
            .dl_progress
            .lock()
            .map(|p| p.running)
            .unwrap_or(false)
        {
            ui.ctx().request_repaint();
        }

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

        egui::CollapsingHeader::new("识图（视觉模型 Qwen2.5-VL）").show(ui, |ui| {
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
                ui.add(
                    egui::DragValue::new(&mut self.state.translator.vl_ngl)
                        .range(0..=99),
                );
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
        if ui.button("应用设置").clicked() {
            self.sync_config();
        }
    }

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
                // 旧写法先把整屏大的 Image 加进布局，把感应区挤到屏幕外，
                // 导致拖拽根本记录不到、也确认不了。
                let available = ui.available_size();
                let (rect, response) =
                    ui.allocate_exact_size(available, egui::Sense::click_and_drag());

                if let Some(tex) = &self.state.bg_texture {
                    ui.put(
                        rect,
                        egui::Image::from_texture(tex).max_size(rect.size()),
                    );
                }

                ui.put(
                    egui::Rect::from_min_size(
                        rect.min + egui::vec2(16.0, 16.0),
                        egui::vec2(rect.width() - 32.0, 28.0),
                    ),
                    egui::Label::new(
                        egui::RichText::new("拖拽框选要翻译的区域，松开确认（Esc 取消）")
                            .color(egui::Color32::WHITE)
                            .size(18.0),
                    ),
                );

                if let Some(pos) = response.interact_pointer_pos() {
                    if response.dragged() {
                        if self.state.sel_start.is_none() {
                            self.state.sel_start = Some(pos);
                        }
                        self.state.sel_end = Some(pos);
                    }
                }
                if let (Some(a), Some(b)) = (self.state.sel_start, self.state.sel_end) {
                    let r = egui::Rect::from_two_pos(a, b);
                    ui.painter().rect(
                        r,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 30),
                        egui::Stroke::new(2.0_f32, egui::Color32::WHITE),
                        egui::StrokeKind::Inside,
                    );
                }
                // 只有在面板里真正框选过（sel_start/sel_end 都存在）才确认，
                // 否则忽略这次松手——避免点击“选择区域”或全屏切换带来的
                // 误触发让选择界面一闪而过、根本选不了。
                if response.drag_stopped() {
                    if self.state.sel_start.is_some() && self.state.sel_end.is_some() {
                        self.confirm_selection(&ctx);
                    } else {
                        self.state.sel_start = None;
                        self.state.sel_end = None;
                    }
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

        // 吸附游戏窗口：跟随窗口移动（每帧，取窗/框选/调整时暂停）
        if !self.state.attach_picking && !self.state.selecting && !self.state.adjusting {
            self.update_attach(ui.ctx());
        }

        // 区域边框浮层：透明置顶小窗，标出框选区域位置（吸附时跟随窗口）
        if !self.state.attach_picking && !self.state.selecting && !self.state.adjusting {
            self.render_region_overlay(ui.ctx());
        }

        if self.state.selecting {
            self.render_selection(ui);
        } else if self.state.attach_picking {
            self.render_picker(ui);
        } else if self.state.adjusting {
            self.render_adjust(ui);
        } else {
            self.render_panel(ui);
        }

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
                .hint_text(if copy_source { "等待识别…" } else { "等待翻译…" }),
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
        if GetWindowRect(hwnd, &mut r) == 0 || *x < r.left || *x >= r.right || *y < r.top || *y >= r.bottom {
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
    if data.2 != 0 {
        Some(data.2)
    } else {
        None
    }
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
