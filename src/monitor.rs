//! 后台监控线程：周期性截屏 -> OCR -> 回传原文；
//! 检测到文字时每帧高频 OCR（跟手），连续无文字时自动降频省资源；
//! 翻译在独立工作线程中进行（调用 API），避免网络延迟阻塞监控节奏。
//!
//! 实时性设计（解决"英文不及时"）：
//!   - 每次 OCR 得到非空文本，只要与上一帧预览不同就立刻把英文原文推上界面，
//!     不等它"稳定"。打字机逐字出现时原文也跟着逐字刷新。
//! 频率/超时设计（解决"高频超时/429"）：
//!   - 翻译任务不再走会堆积的队列，而是一个"只保留最新"的覆盖式槽位：
//!     后到的文本会覆盖先到的，翻译线程永远只翻译最近一次的内容。
//!   - 翻译线程对 API 调用做最小间隔限流，避免突发打挂 DeepSeek。
//!   - 相似文本去重 + 翻译缓存，重复句子不重复调用。
//!   - 只在"句子停顿"或"出现句末标点"时才真正投递翻译任务，从源头压住请求量。

use crate::capture;
use crate::capture::{ScreenRect, capture_region};
use crate::lang::Lang;
use crate::ocr::recognize_text_stream;
use crate::state::{MonitorConfig, TranslateResult};
use crate::translate;
use egui::Context;
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// 归一化后的文本差异超过该比例，才认为是"真的换了句子"（而非 OCR 抖动）。
const TEXT_CHANGE_RATIO: f64 = 0.22;

/// 有文字（对话框出现）时的高频轮询间隔：每帧都截屏+OCR，越短越跟手。
const ACTIVE_INTERVAL_MS: u64 = 100;
/// 空闲（连续多帧无文字）时的低频轮询间隔：对话框消失时降低采样频率，省 CPU。
/// 取 600ms：既保证停顿后对话框重现能在 ~0.6s 内被采到（跟手），又不过度空跑截屏。
const IDLE_INTERVAL_MS: u64 = 600;

/// 两次 API 翻译调用之间的最小间隔（限流），缓解高频 429 / 超时。
const TRANSLATE_MIN_GAP_MS: u64 = 700;

/// 连续多帧识别为空，才确认对话框真的消失并清空界面（避免瞬时空白误清）。
const EMPTY_RESET_FRAMES: u32 = 2;

/// 视觉模型 OCR 单次推理较重（3B 模型，CPU 上可达 1s+），限制两次 OCR 的最小间隔，
/// 避免每帧都把截图喂给慢速视觉模型导致 CPU 打满、软件与游戏一起卡顿；
/// 也避免服务端单推理槽被请求堆满而越拖越慢。
/// 这是「服务端失败需重试」时的较长冷却，防止失败抖动狂拉服务端。
const OCR_COOLDOWN_MS: u64 = 1200;

/// 画面「有变化」时的最小 OCR 间隔（极短）：新对话框弹出 / 打字机逐字出现 / 场景动画时，
/// 只要距上次 OCR 超过这个值就尽快重识别，不再被 OCR_COOLDOWN_MS 挡住——
/// 这样「新句子出现」能跟着画面变化尽快上屏，修「原文显示慢（即使短句）」。
/// 仍保留它是为了避免动画帧每 100ms 狂打慢速视觉模型（OCR 本身是同步阻塞的，
/// 一次约 1s，所以实际速率被模型速度自然限制，不会真的每 300ms 一次）。
const OCR_CHANGED_MS: u64 = 300;

/// 翻译任务（覆盖式槽位中的内容）。
struct TranslateJob {
    source: String,
    target: Lang,
}

/// 启动监控线程（以及配套的后台翻译线程）。
pub fn spawn_monitor(config: Arc<Mutex<MonitorConfig>>, ctx: Context, tx: Sender<TranslateResult>) {
    // 覆盖式"待翻译"槽位：监控线程把最新原文写进来，翻译线程取走并翻译。
    // 后写的会覆盖先写的，因此翻译线程永远只处理"最近一次"的内容，
    // 打字机过程中的中间碎片不会被逐个排队、堆积成超时。
    let pending: Arc<Mutex<Option<TranslateJob>>> = Arc::new(Mutex::new(None));
    let cfg_for_worker = Arc::clone(&config);
    let ctx_for_worker = ctx.clone();
    let tx_for_worker = tx.clone();
    spawn_translation_worker(
        pending.clone(),
        tx_for_worker,
        cfg_for_worker,
        ctx_for_worker,
    );

    thread::spawn(move || {
        // 空闲/高频模式切换：未检测到文字时进入空闲低频轮询，省 CPU。
        let mut idle = false;
        // 上一帧已上屏的"实时原文预览"，用于去重，避免每帧都推同样内容。
        let mut last_live: Option<String> = None;
        // 上一句已经投递过翻译任务的内容，用于去重，避免相似抖动反复发起调用。
        let mut last_job: Option<String> = None;
        // 连续识别为空的帧数，用于判断对话框是否真的消失。
        let mut empty_count: u32 = 0;
        // 视觉模型 OCR 的帧基线（48×48 灰度缩略图）：画面无"有意义变化"时跳过重算，
        // 避免光标闪烁/抗锯齿抖动等细微变化让同一句话反复重 OCR、反复流式重放。
        let mut last_thumb: Option<Vec<u8>> = None;
        // OCR 结果是否成功（区分"静态画面已识别"与"服务端未就绪待重试"）。
        let mut last_ocr_ok: bool = false;
        // 上次真正发起 OCR 的时刻（用于冷却节流，避免打满视觉模型）。
        let mut last_ocr_time: Instant = Instant::now();
        // 上次刷新「视觉模型服务端就绪」缓存的时刻（UI 只读该缓存，避免每帧做网络探测）。
        let mut last_health_check: Instant = Instant::now();

        loop {
            // 空闲时降低轮询频率（少做无谓的截屏+OCR）；有文字时恢复每帧高频。
            let interval = if idle {
                IDLE_INTERVAL_MS
            } else {
                ACTIVE_INTERVAL_MS
            };
            thread::sleep(Duration::from_millis(interval));

            let (running, region, tgt, translator, attach_hwnd, attach_off) = {
                let c = config.lock().unwrap();
                (
                    c.running,
                    c.region,
                    c.target_lang,
                    c.translator.clone(),
                    c.attach_hwnd,
                    c.attach_region_offset,
                )
            };

            // 后台按 ~1s 节流刷新「视觉模型服务端就绪」缓存：UI 线程只读缓存，不在重绘路径上做网络请求，
            // 否则端口未监听时连接会阻塞数百毫秒~数秒，导致点击截图/打开设置等任意重绘操作都卡顿。
            let now = Instant::now();
            if now.duration_since(last_health_check) >= Duration::from_millis(1000) {
                last_health_check = now;
                translate::refresh_vl_ready(translator.vl_port);
            }

            if !running {
                idle = false;
                last_live = None;
                last_job = None;
                empty_count = 0;
                last_thumb = None;
                last_ocr_ok = false;
                last_ocr_time = Instant::now();
                *pending.lock().unwrap() = None;
                continue;
            }
            // 截取区域：吸附了游戏窗口时，区域跟随窗口当前矩形实时换算（P1），
            // 游戏窗口移动/改分辨率/切全屏都不需要重新框选。
            let region = match region {
                Some(r) => {
                    if attach_hwnd != 0 {
                        match capture::window_rect(attach_hwnd) {
                            Some((l, t, _, _)) if !capture::window_iconic(attach_hwnd) => {
                                match attach_off {
                                    Some((ox, oy)) => Some(ScreenRect {
                                        x: l + ox,
                                        y: t + oy,
                                        w: r.w,
                                        h: r.h,
                                    }),
                                    None => Some(r),
                                }
                            }
                            // 窗口已失效或最小化：本帧跳过（等 UI 侧自动取消吸附）
                            _ => None,
                        }
                    } else {
                        Some(r)
                    }
                }
                None => None,
            };
            let region = match region {
                Some(r) => r,
                None => continue,
            };

            let img = match capture_region(region) {
                Ok(i) => i,
                Err(_) => continue,
            };

            // 识图统一由本地视觉模型(Qwen2.5-VL)完成。该模型单次推理较重（3B、CPU 可达 1s+），
            // 若每帧都喂图会让 CPU 打满、软件与游戏一起卡顿，且服务端单推理槽串行、请求堆积会
            // 越拖越慢。这里做三层节流，核心是「静态跳过、变化快识别」：
            //   1) 画面与「上次成功识别的那一帧」完全一致（无论有无文字）→ 直接跳过，
            //      静态对话框 / 静态空场景都不再反复跑模型（省算力、不卡顿、也不空跑）。
            //   2) 画面有变化（新对话框 / 打字机逐字 / 场景动画）→ 只受极短 OCR_CHANGED_MS 限制，
            //      尽快重识别；这样新句子出现不再被旧 1200ms 冷却挡住，原文能跟着画面尽快上屏。
            //   3) 画面未变但上次 OCR 失败（服务端未就绪）→ 仍受 OCR_COOLDOWN_MS 保护后重试，
            //      避免失败抖动狂拉服务端。
            // 服务端未就绪时 recognize_text_stream 返回 Err 且不锁定帧基线，冷却结束后再试，
            // 由 translate 层负责在子进程死亡时重新拉起，从而自动恢复识别（修复“只识别一次”）。
            let thumb = frame_thumb(&img);
            let now = Instant::now();
            let since_ocr = now.duration_since(last_ocr_time).as_millis() as u64;
            // "有变化"= 与上次成功识别的那一帧相比存在**有意义**的像素差异
            // （超过阈值数量的像素亮度差超过阈值）。光标闪烁/抗锯齿抖动等细微变化
            // 不会触发重 OCR，避免同一句话被反复重新转录、流式反复重放；
            // 真实文字变化（新字/新句出现）会明显改变大量像素，不会漏。
            let changed = match &last_thumb {
                Some(prev) => significant_change(prev, &thumb),
                None => true,
            };
            if !changed && last_ocr_ok {
                // 无有意义变化且上次已成功识别（有字或无字）：无需任何处理（省算力、不刷屏）。
                continue;
            }
            if changed {
                // 画面有变化：尽快 OCR，只给极短冷却避免动画帧狂打慢速视觉模型；
                // 因为 OCR 是同步阻塞（一次约 1s），实际速率被模型速度自然限制，不会真的每 300ms 一次。
                if since_ocr < OCR_CHANGED_MS {
                    continue;
                }
            } else {
                // 画面未变但上次 OCR 失败（服务端未就绪）：冷却后才重试，避免失败抖动。
                if since_ocr < OCR_COOLDOWN_MS {
                    continue;
                }
            }

            // 冷却结束且（画面有变 或 上次失败需重试）：真正发起 OCR。
            // 走**流式**识图：模型边转录边把已产出的文字片段回调上屏，
            // 原文框跟着打字机出字，避免等整段转录完（3B 模型 CPU 1~3s）才一次性弹出。
            last_ocr_time = now;
            let ocr_t0 = Instant::now();
            let mut acc = String::new(); // 本帧已转录的累计文本
            let mut last_sent = String::new(); // 上次上屏的原文（去重）
            let mut sent_live = false; // 本帧是否已为新句子发过"清旧译文"的预览
            let text = {
                let mut cb = |chunk: &str| {
                    acc.push_str(chunk);
                    let trimmed = acc.trim();
                    // NO_TEXT 前缀暂存：空场景模型会输出 "NO_TEXT"，
                    // 不能把它逐字刷上原文框（等转录完成后统一判定为空）。
                    let no_text_prefix =
                        !trimmed.is_empty() && trimmed.len() <= 7 && "NO_TEXT".starts_with(trimmed);
                    if no_text_prefix || acc == last_sent {
                        return;
                    }
                    last_sent = acc.clone();
                    // 跨帧重放抑制：若累计文本仍是"已上屏文本"（上一句）的前缀，
                    // 说明这是同一句话被重新转录（画面微变触发重 OCR），
                    // 不逐字重放；等它超出已显示部分（出现真实新字符）才继续上屏。
                    let replay = match &last_live {
                        Some(prev) => {
                            let p = normalize(prev);
                            let c = normalize(&acc);
                            !c.is_empty() && p.starts_with(&c)
                        }
                        None => false,
                    };
                    if replay {
                        return;
                    }
                    if !sent_live {
                        sent_live = true;
                        // 新句子的第一个可见字符：若与上一句不是续写关系（打字机逐字），
                        // 先发一条 live 预览清掉旧译文，避免"新原文配旧译文"的错位；
                        // 续写（同句打字机/微调）则保留译文，等新译文流式覆盖。
                        let is_new_sentence = match &last_live {
                            Some(prev) => !text_continues(prev, &acc),
                            None => true,
                        };
                        if is_new_sentence {
                            let _ = tx.send(TranslateResult {
                                source: acc.clone(),
                                target: String::new(),
                                live: true,
                                streaming: false,
                                source_streaming: false,
                                clear: false,
                            });
                            ctx.request_repaint();
                            return;
                        }
                    }
                    // 续写/后续片段：只逐字更新原文框，不动译文。
                    let _ = tx.send(TranslateResult {
                        source: acc.clone(),
                        target: String::new(),
                        live: false,
                        streaming: false,
                        source_streaming: true,
                        clear: false,
                    });
                    ctx.request_repaint();
                };
                match recognize_text_stream(img, &translator, &mut cb) {
                    Ok(t) => t,
                    Err(_) => {
                        // 视觉模型服务端未就绪（冷启动中/已退出）：不锁定哈希，冷却结束后再重试；
                        // translate 层会在子进程死亡时重新拉起，故能自动恢复。
                        last_ocr_ok = false;
                        continue;
                    }
                }
            };
            tracing::debug!(
                elapsed_ms = ocr_t0.elapsed().as_millis() as u64,
                text_len = text.chars().count(),
                "OCR 完成"
            );
            // 成功完成一次 OCR（无论结果是否有文字）：锁定本帧基线并标记成功，
            // 这样「静态画面」也会被跳过、不再每帧空跑视觉模型，只在画面有意义变化时重新识别。
            last_thumb = Some(thumb);
            last_ocr_ok = true;

            if text.trim().is_empty() {
                // 对话框暂时为空（消失/场景切换中）：清空观察状态，并进入空闲低频模式。
                empty_count += 1;
                idle = true;
                if empty_count >= EMPTY_RESET_FRAMES {
                    // 确认对话框真的没了：清空界面，并清掉翻译去重基准，
                    // 这样对话框重现后的第一句会作为全新内容重新翻译。
                    let _ = tx.send(TranslateResult {
                        source: String::new(),
                        target: String::new(),
                        live: false,
                        streaming: false,
                        source_streaming: false,
                        clear: true,
                    });
                    ctx.request_repaint();
                    last_live = None;
                    last_job = None;
                    *pending.lock().unwrap() = None;
                }
                continue;
            }
            empty_count = 0;
            // 检测到文字：切回高频每帧 OCR 模式。
            idle = false;

            // 1) 实时原文：每次 OCR 内容（归一化后）确实不同就立刻上屏，不等"稳定"。
            //    打字机逐字出现时，原文也跟着逐字刷新，彻底解决"英文不及时"。
            //    用精确归一化相等判断变化（而非 22% 容差），OCR 排版抖动不会刷屏。
            let live_changed = match &last_live {
                Some(prev) => normalize(&text) != normalize(prev),
                None => true,
            };
            if live_changed {
                let _ = tx.send(TranslateResult {
                    source: text.clone(),
                    target: String::new(),
                    live: true,
                    streaming: false,
                    source_streaming: false,
                    clear: false,
                });
                ctx.request_repaint();
                last_live = Some(text.clone());
            }

            // 2) 翻译投递：OCR 每识别出"与上一句明显不同的像样文本"就投递翻译，
            //    worker 负责限流(700ms)与去重缓存，pending 为覆盖式只留最新，
            //    所以打字途中狂变的碎片不会堆积，只会被翻译成最终那一句。
            //    looks_like_text 过滤纯噪点帧（太短或不含任何字母/汉字），避免无意义请求。
            let looks_like_text = text.trim().len() >= 3
                && text
                    .chars()
                    .any(|c| c.is_ascii_alphanumeric() || (c as u32) >= 0x2E80);
            if looks_like_text && !texts_stable(&text, last_job.as_deref()) {
                *pending.lock().unwrap() = Some(TranslateJob {
                    source: text.clone(),
                    target: tgt,
                });
                last_job = Some(text.clone());
            }
        }
    });
}

/// 后台翻译线程：取"最新待翻译"文本（覆盖式，只取最近一次），限流后调用 API，
/// 把结果回传 UI。与监控线程并行，因此网络延迟不会拖慢截图与 OCR 的节奏。
///
/// 实时性改进（解决"译文不够及时"）：
///   - 走 `translate_stream` 流式接口：本地模型 SSE 逐字出译文，UI 译文框跟着打字机刷新，
///     首 token 数百毫秒即上屏，不再等整句生成完。
///   - 限流只对远程 API 生效（防 429）；本地模型不限流，省掉每句白等的 700ms。
///   - 发最终结果前检查 pending：若监控线程已投递更新的句子，丢弃过期结果，
///     避免"旧句译文覆盖新句原文/译文"的闪现。
fn spawn_translation_worker(
    pending: Arc<Mutex<Option<TranslateJob>>>,
    tx_ui: Sender<TranslateResult>,
    config: Arc<Mutex<MonitorConfig>>,
    ctx: Context,
) {
    thread::spawn(move || {
        // 翻译缓存：相同句子（同目标语言）不再重复调用 API。
        let mut cache: HashMap<String, String> = HashMap::new();
        let mut last_call: Option<Instant> = None;

        loop {
            // 取最新待翻译文本；没有就短暂休眠等待，避免空转。
            let job = pending.lock().unwrap().take();
            let job = match job {
                Some(j) => j,
                None => {
                    thread::sleep(Duration::from_millis(60));
                    continue;
                }
            };

            let cfg = config.lock().unwrap().translator.clone();
            // 限流：本地模型是单机服务、无并发压力，不设最小间隔（省掉每句白等的 700ms）；
            // 远程 API 仍保持最小间隔，缓解高频 429 / 超时。
            let min_gap = match cfg.mode {
                translate::AppMode::LocalVisionLocal => Duration::ZERO,
                _ => Duration::from_millis(TRANSLATE_MIN_GAP_MS),
            };
            if let Some(t) = last_call {
                let elapsed = t.elapsed();
                if elapsed < min_gap {
                    thread::sleep(min_gap - elapsed);
                }
            }
            last_call = Some(Instant::now());

            let cache_key = format!("{}\u{1f}{:?}", job.source, job.target);
            let translated = if let Some(t) = cache.get(&cache_key) {
                t.clone()
            } else {
                let t0 = Instant::now();
                let mut acc = String::new();
                let t = translate::translate_stream(&job.source, job.target, &cfg, &mut |chunk| {
                    acc.push_str(chunk);
                    // 流式中间结果：立即上屏（译文框逐字刷新），不进历史。
                    let _ = tx_ui.send(TranslateResult {
                        source: job.source.clone(),
                        target: acc.clone(),
                        live: false,
                        streaming: true,
                        source_streaming: false,
                        clear: false,
                    });
                    ctx.request_repaint();
                });
                tracing::info!(
                    elapsed_ms = t0.elapsed().as_millis() as u64,
                    "翻译完成 ({} bytes)",
                    t.len()
                );
                cache.insert(cache_key, t.clone());
                t
            };

            // 发最终结果前检查：监控线程若已投递更新的句子（pending 非空），
            // 说明本次译文对应的原文已过期，丢弃最终结果（新句的译文马上会来），
            // 避免"旧句译文覆盖新句原文/译文"的闪现。
            let superseded = pending.lock().unwrap().is_some();
            if superseded {
                continue;
            }
            let _ = tx_ui.send(TranslateResult {
                source: job.source.clone(),
                target: translated,
                live: false,
                streaming: false,
                source_streaming: false,
                clear: false,
            });
            ctx.request_repaint();
        }
    });
}

/// 归一化：转小写并去掉所有空白，用于忽略大小写/排版导致的无关差异。
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// 计算两串字符的编辑距离（Levenshtein），用于衡量文本相似度。
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// 判断新转录文本 cur 是否是上一句 prev 的续写（打字机逐字出现 / 同句微调）。
/// 归一化后比较：cur 以 prev 开头视为续写；prev 以 cur 开头（OCR 抖动读短了）也视为同句。
/// 用于流式 OCR 上屏时决定是否要清掉旧译文（新句子才清）。
fn text_continues(prev: &str, cur: &str) -> bool {
    let p = normalize(prev);
    let c = normalize(cur);
    !c.is_empty() && (c.starts_with(&p) || p.starts_with(&c))
}

/// 判断新文本与旧文本是否"同一句话的抖动"而非真正变化。
fn texts_stable(new: &str, old: Option<&str>) -> bool {
    match old {
        None => false,
        Some(old) => {
            let na = normalize(new);
            let nb = normalize(old);
            if na == nb {
                return true;
            }
            let max_len = na.chars().count().max(nb.chars().count());
            if max_len == 0 {
                return true;
            }
            let d = edit_distance(&na, &nb);
            (d as f64 / max_len as f64) <= TEXT_CHANGE_RATIO
        }
    }
}

/// 缩到 48×48 灰度图，作为帧间差异比较的基线（廉价，仅 2304 字节）。
fn frame_thumb(img: &image::DynamicImage) -> Vec<u8> {
    img.resize_exact(48, 48, image::imageops::FilterType::Nearest)
        .to_luma8()
        .into_raw()
}

/// 两帧缩略图是否存在"有意义的差异"：亮度差超过 `FRAME_DIFF_LUMA` 的像素数
/// 达到 `FRAME_DIFF_PIXELS` 才视为画面真的变了。
/// 目的：跳过光标闪烁、抗锯齿/阴影抖动等细微变化——它们会让"全等哈希"判定为变了，
/// 导致同一句话被反复重 OCR、流式反复重放；真实文字变化（新字/新句）会改变
/// 大量像素，不会被漏掉。
const FRAME_DIFF_PIXELS: usize = 3;
const FRAME_DIFF_LUMA: u8 = 24;
fn significant_change(prev: &[u8], cur: &[u8]) -> bool {
    let mut diff = 0usize;
    for (a, b) in prev.iter().zip(cur.iter()) {
        let d = (*a as i16 - *b as i16).abs();
        if d > FRAME_DIFF_LUMA as i16 {
            diff += 1;
            if diff >= FRAME_DIFF_PIXELS {
                return true;
            }
        }
    }
    false
}
