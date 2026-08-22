//! 应用状态：UI 状态、监控配置、翻译结果。

use crate::capture::ScreenRect;
use crate::lang::Lang;
use crate::translate::TranslatorConfig;

/// 后台监控线程读取的配置（与主线程共享）。
#[derive(Clone)]
pub struct MonitorConfig {
    pub running: bool,
    pub region: Option<ScreenRect>,
    pub source_lang: Lang,
    pub target_lang: Lang,
    pub translator: TranslatorConfig,
    /// 吸附目标窗口句柄（0 = 未吸附）。>0 时截取区域会跟随该窗口移动。
    pub attach_hwnd: isize,
    /// 截取区域相对窗口左上角的偏移（物理像素）。窗口移动时 region = 窗口 + 偏移。
    pub attach_region_offset: Option<(i32, i32)>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            running: false,
            region: None,
            source_lang: Lang::En,
            target_lang: Lang::Zh,
            translator: TranslatorConfig::default(),
            attach_hwnd: 0,
            attach_region_offset: None,
        }
    }
}

/// 单次翻译结果。
#[derive(Clone, Default)]
pub struct TranslateResult {
    pub source: String,
    pub target: String,
    /// true = 这是"实时原文预览"（一次完整的 OCR 结果），只刷新原文、不清掉已显示的译文。
    pub live: bool,
    /// true = 这是流式译文中间结果（翻译尚未完成，target 为已生成的部分译文）。
    /// 只更新译文框、不进历史；完成时会再发一条 streaming=false 的最终结果。
    pub streaming: bool,
    /// true = 这是流式原文中间结果（OCR 尚未完成，source 为已转录的部分原文）。
    /// 只逐字更新原文框、不清译文、不进历史；OCR 完成时会再发 live=true 的完整原文。
    pub source_streaming: bool,
    /// true = 对话框已确认为空，清空界面显示。
    pub clear: bool,
}

/// UI 状态。
pub struct TranslateState {
    pub source_text: String,
    pub target_text: String,
    pub history: Vec<TranslateResult>,

    pub monitoring: bool,
    pub selecting: bool,

    // 框选拖拽状态
    pub sel_start: Option<egui::Pos2>,
    pub sel_end: Option<egui::Pos2>,
    pub sel_scale: f32,
    pub sel_monitor_offset: (i32, i32),
    pub bg_texture: Option<egui::TextureHandle>,

    // 设置
    pub translator: TranslatorConfig,
    pub source_lang: Lang,
    pub target_lang: Lang,

    pub last_error: Option<String>,

    // 已选区域（源语言/目标语言/翻译后端都从这里读取）
    pub region: Option<ScreenRect>,

    /// 是否在屏幕上用边框浮层标出框选区域（透明置顶、点击穿透）。
    pub show_region_border: bool,

    // 吸附游戏窗口：面板跟随该窗口移动（窗口句柄，isize 存 HWND）。
    pub attach_enabled: bool,
    /// 目标窗口句柄（0 = 无）。窗口关闭时自动取消吸附。
    pub attach_hwnd: isize,
    /// 面板位置相对窗口左上角的偏移（points），拖动面板会重新记录。
    pub attach_offset: Option<egui::Vec2>,
    /// 截取区域相对窗口左上角的偏移（物理像素），用于区域跟随窗口（P1）。
    pub attach_region_offset: Option<(i32, i32)>,
    /// 目标窗口标题（仅用于界面显示）。
    pub attach_title: String,

    /// 取窗模式（P0）：全屏取景，移动鼠标实时高亮目标窗口，左键确认吸附。
    pub attach_picking: bool,
    /// 取窗模式中当前鼠标指向的窗口句柄。
    pub pick_hovered: Option<isize>,
    /// 进入取窗前的主面板位置（points）——吸附偏移以它为基准，
    /// 避免全屏取景时用全屏位置算偏移导致面板乱跑。
    pub pick_panel_pos: Option<egui::Pos2>,

    /// 区域调整模式（全屏拖拽移动/缩放已有区域）。
    pub adjusting: bool,
    /// 当前调整中的矩形（UI 坐标）。
    pub adjust_rect: Option<egui::Rect>,
    /// 正在拖动的部位。
    pub adjust_drag: Option<AdjustHandle>,
    /// 拖动开始时的指针位置（UI 坐标）。
    pub adjust_start_ptr: Option<egui::Pos2>,
    /// 拖动开始时的矩形。
    pub adjust_start_rect: Option<egui::Rect>,

    // 工作状态指示（界面据此显示 识别中… / 翻译中…）
    pub ocr_working: bool,
    pub translate_working: bool,
}

/// 区域调整模式中正在拖动的部位（整体移动 / 四角四边缩放）。
#[derive(Clone, Copy, PartialEq)]
pub enum AdjustHandle {
    Move,
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl Default for TranslateState {
    fn default() -> Self {
        Self {
            source_text: String::new(),
            target_text: String::new(),
            history: Vec::new(),
            monitoring: false,
            selecting: false,
            sel_start: None,
            sel_end: None,
            sel_scale: 1.0,
            sel_monitor_offset: (0, 0),
            bg_texture: None,
            translator: TranslatorConfig::default(),
            source_lang: Lang::En,
            target_lang: Lang::Zh,
            last_error: None,
            region: None,
            show_region_border: true,
            attach_enabled: false,
            attach_hwnd: 0,
            attach_offset: None,
            attach_region_offset: None,
            attach_title: String::new(),
            attach_picking: false,
            pick_hovered: None,
            pick_panel_pos: None,
            adjusting: false,
            adjust_rect: None,
            adjust_drag: None,
            adjust_start_ptr: None,
            adjust_start_rect: None,
            ocr_working: false,
            translate_working: false,
        }
    }
}

impl TranslateState {
    pub fn push_result(&mut self, r: TranslateResult) {
        // 对话框确认为空：清掉界面，等下一句重新显示。
        if r.clear {
            self.source_text.clear();
            self.target_text.clear();
            self.ocr_working = false;
            self.translate_working = false;
            return;
        }
        // 流式原文中间结果：只逐字更新原文框（不进历史、不动译文）。
        // 译文是否清空由 OCR 完成后的 live 预览决定。
        if r.source_streaming {
            self.ocr_working = true;
            if !r.source.trim().is_empty() {
                self.source_text = r.source;
            }
            return;
        }
        // 流式译文中间结果：只更新译文框（不进历史、不清原文）。
        // 仅当该流对应的原文仍是当前显示的原文时才上屏，避免旧句的部分译文
        // 覆盖新句的原文/译文（新句的最终译文很快会到）。
        if r.streaming {
            self.translate_working = true;
            if !r.source.is_empty() && r.source == self.source_text {
                self.target_text = r.target;
            }
            return;
        }
        // 实时原文预览：更新原文，并清空旧译文，避免"慢一拍"——下一句原文已来、
        // 译文还停在上一句。译文区会等到当前句翻译完成后被重新填入。
        if r.live {
            self.ocr_working = false;
            if !r.source.trim().is_empty() {
                if r.source != self.source_text {
                    self.target_text.clear();
                }
                self.source_text = r.source;
            }
            return;
        }
        if r.source.trim().is_empty() {
            return;
        }
        // 翻译最终结果
        self.ocr_working = false;
        self.translate_working = false;
        self.source_text = r.source.clone();
        self.target_text = r.target.clone();
        let changed = self
            .history
            .last()
            .map(|h| h.source != r.source)
            .unwrap_or(true);
        if changed {
            self.history.push(r);
            if self.history.len() > 50 {
                self.history.remove(0);
            }
        }
    }
}
