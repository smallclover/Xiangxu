//! 文字识别（OCR）后端：统一由本地视觉模型(Qwen2.5-VL)完成「看图转录」原文。
//!
//! 视觉模型能识别艺术字/彩色字（传统字形匹配引擎做不到），且无需 OCR 语言包、
//! 无需额外的 UWP/ocrs 依赖，整项目可离线编译。识别为空的画面返回空串，
//! 由监控线程据此进入空闲并清空界面，不会把场景当成文字去“解释/翻译”。
//!
//! 本模块走**流式**接口：模型转录过程中，已产出的文字片段通过回调逐段上屏，
//! 原文框跟着打字机出字（3B 模型单次推理 1~3s，流式消除"卡着不动"的等待感）。

use crate::translate::TranslatorConfig;
use image::DynamicImage;

/// 统一入口：对截图做**流式** OCR，返回识别出的原文文本。
///
/// 模型边转录边调用 `on_chunk`（每段已产出的文字），调用方据此逐字刷新原文框；
/// 返回值是完整转录文本。本函数只是「本地视觉模型识图」这一唯一后端对外的薄封装，
/// 便于监控线程与未来可能的其他识图实现解耦：
/// - `Ok(text)` 非空：成功转录出的原文。
/// - `Ok("")`      ：画面无文字（视觉模型回 NO_TEXT），监控据此清空界面。
/// - `Err(_)`      ：视觉模型服务端尚未就绪（冷启动中），调用方跳过本帧、不阻塞。
pub fn recognize_text_stream(
    img: DynamicImage,
    cfg: &TranslatorConfig,
    on_chunk: &mut dyn FnMut(&str),
) -> std::result::Result<String, String> {
    crate::translate::recognize_text_vl_stream(&img, cfg, on_chunk)
}
