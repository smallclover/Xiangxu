//! 翻译后端：三种运行模式（见 `AppMode`）——识图统一由本地视觉模型(Qwen2.5-VL)完成，
//! 翻译由 本地 llama.cpp / 远程 OpenAI 兼容 API / 模拟 提供。
//!
//! 远程翻译使用系统内置的 curl.exe（Windows 10+ 自带）调用 OpenAI 兼容端点，
//! 因此不引入额外网络依赖，整项目可离线编译；填入 Key 即可真翻译。

use crate::glossary;
use crate::lang::Lang;
use image::DynamicImage;
use serde_json::json;
use std::io::{Cursor, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// 运行模式：识图统一由本地视觉模型(Qwen2.5-VL)完成；
/// 三种模式的差别仅在于"翻译"由 本地 llama.cpp / 远程 API / 模拟 提供。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppMode {
    /// 测试用：识图(真实视觉模型) + 模拟翻译，不依赖翻译服务端或 API Key。
    Mock,
    /// 本地模型识图 + 本地模型翻译（llama.cpp，完全离线）。
    LocalVisionLocal,
    /// 本地模型识图 + 远程模型翻译（OpenAI 兼容 API）。
    LocalVisionRemote,
}

#[derive(Clone)]
pub struct TranslatorConfig {
    /// 运行模式：识图始终用本地视觉模型；翻译可选 本地 / 远程 / 模拟。
    pub mode: AppMode,
    /// 远程翻译(API)端点 base_url（如 https://api.deepseek.com）。
    pub endpoint: String,
    /// 远程翻译(API) Key。
    pub api_key: String,
    /// 远程翻译(API)模型名。
    pub model: String,
    /// 本地翻译服务端：llama.cpp 服务端可执行文件路径（如 llama-server.exe）。
    pub local_server: String,
    /// 本地翻译 GGUF 模型路径（如 qwen2.5-1.5b-instruct-q4_k_m.gguf）。
    pub local_model: String,
    /// 本地翻译服务端监听端口（默认 8080）。
    pub local_port: u16,
    /// 本地翻译 GPU 层数卸载（-ngl）。99 约等于全部卸载到 GPU（CUDA 版服务端）。
    pub local_ngl: i32,
    /// 视觉模型识图：VL GGUF 模型路径（如 Qwen2.5-VL-3B q4）。
    pub vl_model: String,
    /// 视觉模型识图：vision 投影(mmproj)路径，必需。
    pub vl_mmproj: String,
    /// 视觉模型识图服务端监听端口（默认 8081，与翻译 server 8080 区分）。
    pub vl_port: u16,
    /// 视觉模型识图 GPU 层数卸载（-ngl）。CUDA 版服务端建议 99。
    pub vl_ngl: i32,
    /// 送视觉模型前的最大边长（像素）。识图耗时主要取决于图像预填充 token 数，
    /// 而 token 数随分辨率平方增长：调小此值可显著加快 OCR（如 512~640），
    /// 但过小会降低小字号文字的识别率。默认 1024，速度优先可调小。
    pub vl_max_side: u32,
}

impl Default for TranslatorConfig {
    fn default() -> Self {
        // 只支持 CUDA 版服务端（llama-cuda，需 NVIDIA 显卡 + CUDA 13.3 运行时 DLL）。
        let local_server = default_local_server();
        Self {
            // 默认即「本地识图 + 本地翻译」：完全离线、无需 Key，贴合本机已下好的模型。
            mode: AppMode::LocalVisionLocal,
            endpoint: "https://api.deepseek.com".to_string(),
            api_key: String::new(),
            model: "deepseek-v4-flash".to_string(),
            // 自动选择：llama-cuda/llama-server.exe（含旧目录名兼容）。
            local_server,
            local_model: manifest_path("assets/models/qwen2.5-1.5b-instruct-q4_k_m.gguf"),
            local_port: 8080,
            // 全部层卸载到 GPU（-ngl 99）加速推理。
            local_ngl: 99,
            // 视觉模型识图：默认指向已归位的 Qwen2.5-VL-3B（Q4_K_H 混合量化）。
            vl_model: manifest_path(
                "assets/models/qwen2.5-vl-3b/Qwen2.5-VL-3B-Instruct.Q4_K_H.gguf",
            ),
            vl_mmproj: manifest_path(
                "assets/models/qwen2.5-vl-3b/Qwen2.5-VL-3B-Instruct.mmproj.gguf",
            ),
            vl_port: 8081,
            // CUDA 版服务端 → 识图同样走 GPU（-ngl 99）。
            vl_ngl: 99,
            // 识图默认长边上限 1024px；觉得识别慢可调小到 512~640 换速度。
            vl_max_side: 1024,
        }
    }
}

/// 选择本地 llama-server 可执行文件（自动检测，启动时执行一次，代价极小）：
/// 只支持 CUDA 版（`llama-cuda/llama-server.exe`，含旧目录名 `llama-b10453-bin-win-cuda-13.3-x64`）。
/// 均不存在时返回默认 `llama-cuda` 路径，由用户下载 CUDA 版后自动生效。
fn default_local_server() -> String {
    for rel in [
        "llama-cuda/llama-server.exe",
        "llama-b10453-bin-win-cuda-13.3-x64/llama-server.exe",
    ] {
        let exe = manifest_path(rel);
        if Path::new(&exe).exists() {
            return exe;
        }
    }
    manifest_path("llama-cuda/llama-server.exe")
}

/// 把相对包目录的路径解析为可用的路径。
/// 优先返回**运行时相对路径**（绿色版 exe 解压到任意目录都能解析到旁边的模型/llama 目录）；
/// 仅当相对路径不存在时才退回编译期 CARGO_MANIFEST_DIR 的绝对路径（cargo run 场景兜底）。
fn manifest_path(rel: &str) -> String {
    if Path::new(rel).exists() {
        return rel.to_string();
    }
    match option_env!("CARGO_MANIFEST_DIR") {
        Some(dir) => std::path::Path::new(dir)
            .join(rel)
            .to_string_lossy()
            .into_owned(),
        None => rel.to_string(),
    }
}

/// 翻译入口：识图统一由本地视觉模型完成，此处仅按"翻译后端"模式分发。
/// - `Mock`           ：仅模拟翻译（测试用，不依赖服务端或 API Key）。
/// - `LocalVisionLocal`：本地 llama.cpp 模型翻译（完全离线）。
/// - `LocalVisionRemote`：远程 OpenAI 兼容 API 翻译（需要 API Key）。
pub fn translate(text: &str, target: Lang, cfg: &TranslatorConfig) -> String {
    // 术语预处理：与流式入口同一策略（预替换 + 保留译名提示）。
    let (replaced, hits) = glossary::global().apply(text);
    let text: &str = &replaced;
    match cfg.mode {
        AppMode::Mock => mock_translate(text),
        AppMode::LocalVisionLocal => translate_local(text, target, &hits, cfg),
        AppMode::LocalVisionRemote => {
            if cfg.api_key.trim().is_empty() {
                // 远程模式但未填 Key：回退模拟，避免界面空白。
                return mock_translate(text);
            }
            match translate_via_curl(&cfg.endpoint, &cfg.api_key, &cfg.model, text, target, &hits) {
                Ok(t) if !t.trim().is_empty() => t,
                Ok(_) => mock_translate(text),
                Err(e) => format!("[翻译失败] {}", e),
            }
        }
    }
}

/// 翻译入口（流式变体）：与 `translate` 语义相同，但会把"已生成的部分译文"
/// 通过 `on_chunk` 逐段回调上屏，首 token（通常数百毫秒）即开始显示，
/// 不再等整句生成完才一次性上屏——这是"译文不够及时"最直接的解法。
/// - 本地模型：真流式（SSE），逐字回调；
/// - 远程 API / 模拟：不支持流式，完成后一次性回调整段结果。
pub fn translate_stream(
    text: &str,
    target: Lang,
    cfg: &TranslatorConfig,
    on_chunk: &mut dyn FnMut(&str),
) -> String {
    // 术语预处理：预替换专有名词 + 生成"保留译名"提示，所有翻译后端共用。
    let (replaced, hits) = glossary::global().apply(text);
    let text: &str = &replaced;
    match cfg.mode {
        AppMode::Mock => {
            let t = mock_translate(text);
            on_chunk(&t);
            t
        }
        AppMode::LocalVisionLocal => translate_local_stream(text, target, &hits, cfg, on_chunk),
        AppMode::LocalVisionRemote => {
            if cfg.api_key.trim().is_empty() {
                // 远程模式但未填 Key：回退模拟，避免界面空白。
                let t = mock_translate(text);
                on_chunk(&t);
                return t;
            }
            match translate_via_curl(&cfg.endpoint, &cfg.api_key, &cfg.model, text, target, &hits) {
                Ok(t) if !t.trim().is_empty() => {
                    on_chunk(&t);
                    t
                }
                Ok(_) => {
                    let t = mock_translate(text);
                    on_chunk(&t);
                    t
                }
                Err(e) => {
                    let t = format!("[翻译失败] {}", e);
                    on_chunk(&t);
                    t
                }
            }
        }
    }
}

/// 本地模型翻译：启动/复用本机 llama.cpp 的 OpenAI 兼容服务端，用本地 HTTP 客户端请求翻译。
///
/// 这样"本地模型"对调用方而言等价于一个永远在 `127.0.0.1:port` 的"本地 API"，
/// 无需额外网络依赖、无需 Key、不触发云端的 429/超时。
fn translate_local(
    text: &str,
    target: Lang,
    hits: &[(String, String)],
    cfg: &TranslatorConfig,
) -> String {
    if cfg.local_server.trim().is_empty() || cfg.local_model.trim().is_empty() {
        return "[本地模型] 请在设置里指定 llama-server.exe 与模型(.gguf)路径".to_string();
    }
    if let Err(e) = ensure_local_server(cfg) {
        return format!("[本地模型] {}", e);
    }
    let model = Path::new(&cfg.local_model)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("local")
        .to_string();
    // 服务端兼容 OpenAI /chat/completions。直接用本地 HTTP 客户端请求，
    // 不依赖外部 curl.exe——此前每次 /health 探测都调 curl，而本机 Defender 会对
    // curl.exe 实时扫描、单次启动约 2s，180 次轮询把启动等待拖到 450s 而超时，
    // 表现为"服务端已加载却始终翻译不出"。改用 TcpStream 原生请求后探测为毫秒级。
    // 偶发 503（模型刚加载完 / 槽位暂忙）时重试，避免首句翻译直接失败。
    //
    // 输出质量校验（小模型两大常见毛病）：①复读原文（输出=输入）②预替换的中文术语
    // 被改写/回译。命中任一情况即带着"加强指令"重试；重试用尽仍差则兜底返回最后一次结果。
    let mut last_err = String::new();
    let mut best: Option<String> = None;
    for attempt in 0..3 {
        match translate_local_http(cfg.local_port, &model, text, target, hits, attempt > 0) {
            Ok(t) if !t.trim().is_empty() => {
                if !translation_bad(text, &t, hits) {
                    return t;
                }
                best = Some(t);
            }
            Ok(_) => return mock_translate(text),
            Err(e) => {
                last_err = e;
                if attempt < 2 {
                    thread::sleep(Duration::from_millis(800));
                }
            }
        }
    }
    if let Some(t) = best {
        return t;
    }
    format!("[本地模型] {}", last_err)
}

/// 本地模型翻译（流式）：复用/拉起 llama.cpp 服务端，SSE 流式返回译文片段。
///
/// 两阶段策略：
///   1. **流式生成**：逐字回调上屏，首 token 数百毫秒即开始显示；
///   2. **质量校验**：整句完成后检查"复读原文/术语被改写"。不合格则改走**非流式**
///      加强重试（不再回调 `on_chunk`——UI 是整体替换式上屏，最终返回值会覆盖
///      之前流式显示的坏译文，因此不会出现重复拼接；流式期间若再回调反而会拼出
///      两次生成的内容）。
/// 网络层失败的重试沿用旧逻辑：已经产出过部分译文就不再重试，避免界面错乱。
fn translate_local_stream(
    text: &str,
    target: Lang,
    hits: &[(String, String)],
    cfg: &TranslatorConfig,
    on_chunk: &mut dyn FnMut(&str),
) -> String {
    if cfg.local_server.trim().is_empty() || cfg.local_model.trim().is_empty() {
        return "[本地模型] 请在设置里指定 llama-server.exe 与模型(.gguf)路径".to_string();
    }
    if let Err(e) = ensure_local_server(cfg) {
        return format!("[本地模型] {}", e);
    }
    let model = Path::new(&cfg.local_model)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("local")
        .to_string();
    let mut last_err = String::new();
    let emitted = std::cell::Cell::new(false);
    let mut cb = |chunk: &str| {
        emitted.set(true);
        on_chunk(chunk);
    };
    let mut best: Option<String> = None;
    // 第一阶段：流式生成。
    for attempt in 0..3 {
        match translate_local_http_stream(cfg.local_port, &model, text, target, hits, &mut cb) {
            Ok(t) if !t.trim().is_empty() => {
                best = Some(t);
                break;
            }
            Ok(_) => return mock_translate(text),
            Err(e) => {
                last_err = e;
                // 已出过流（部分译文已上屏）：不再重试，避免重复内容。
                if emitted.get() {
                    break;
                }
                if attempt < 2 {
                    thread::sleep(Duration::from_millis(800));
                }
            }
        }
    }
    // 第二阶段：质量校验，不合格走非流式加强重试（不回调 on_chunk）。
    if let Some(t) = &best {
        if !translation_bad(text, t, hits) {
            return t.clone();
        }
    }
    for attempt in 0..2 {
        match translate_local_http(cfg.local_port, &model, text, target, hits, true) {
            Ok(t) if !t.trim().is_empty() => {
                if !translation_bad(text, &t, hits) {
                    return t;
                }
                best = Some(t);
            }
            Ok(_) => break,
            Err(e) => {
                last_err = e;
                if attempt == 0 {
                    thread::sleep(Duration::from_millis(800));
                }
            }
        }
    }
    if let Some(t) = best {
        return t;
    }
    format!("[本地模型] {}", last_err)
}

/// 通过本地 HTTP 客户端调用 llama.cpp 的 OpenAI 兼容 /chat/completions。
/// 与 `translate_via_curl` 不同，这里不依赖外部 curl.exe，而是用 Rust 标准库的
/// TcpStream 直接发 HTTP/1.1 请求，避免 Windows 上每次启动 curl 被 Defender 扫描的 ~2s 延迟。
fn translate_local_http(
    port: u16,
    model: &str,
    text: &str,
    target: Lang,
    hits: &[(String, String)],
    force: bool,
) -> Result<String, String> {
    let messages = build_messages(text, target, glossary::term_hint(hits).as_deref(), force);
    let body = json!({
        "model": model,
        "messages": messages,
        "temperature": 0.3,
        "stream": false,
    });
    let body_str = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    let resp = http_request("POST", port, "/v1/chat/completions", Some(&body_str), 25000)?;
    let v: serde_json::Value = serde_json::from_str(&resp)
        .map_err(|e| format!("解析接口响应失败: {} | 原始返回: {}", e, resp))?;
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(content)
}

/// 通过本地 HTTP 客户端**流式**调用 llama.cpp 的 OpenAI 兼容 /v1/chat/completions（SSE）。
/// 模型边生成边把已产出的译文片段回调给 `on_chunk`，让译文框"跟着打字机出字"，
/// 首 token 通常数百毫秒即到，感知延迟远小于等整句生成完。
fn translate_local_http_stream(
    port: u16,
    model: &str,
    text: &str,
    target: Lang,
    hits: &[(String, String)],
    on_chunk: &mut dyn FnMut(&str),
) -> Result<String, String> {
    let messages = build_messages(text, target, glossary::term_hint(hits).as_deref(), false);
    let body = json!({
        "model": model,
        "messages": messages,
        "temperature": 0.3,
        "stream": true,
    });
    let body_str = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    // 流式生成可能持续数秒（1.5B 短句通常 <2s），放宽读取超时避免中途断流。
    http_request_stream(port, "/v1/chat/completions", &body_str, 60000, on_chunk)
}

/// 极简 HTTP/1.1 客户端：向 127.0.0.1:port 发送一次请求并读回响应体。
/// 仅用于本地服务端（HTTP，非 HTTPS）。不依赖外部 curl.exe，规避其启动延迟。
fn http_request(
    method: &str,
    port: u16,
    path: &str,
    body: Option<&str>,
    read_timeout_ms: u32,
) -> Result<String, String> {
    let addr = format!("127.0.0.1:{}", port)
        .to_socket_addrs()
        .map_err(|e| format!("解析地址失败: {}", e))?
        .next()
        .ok_or_else(|| "无法解析 127.0.0.1".to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(600))
        .map_err(|e| format!("连接本地服务端失败(127.0.0.1:{}): {}", port, e))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(read_timeout_ms as u64)))
        .ok();

    let body_str = body.unwrap_or("");
    let req = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        method,
        path,
        port,
        body_str.len(),
        body_str
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("发送请求失败: {}", e))?;

    let mut resp: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut header_end: Option<usize> = None;
    let mut content_len: Option<usize> = None;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                resp.extend_from_slice(&buf[..n]);
                if header_end.is_none() {
                    if let Some(i) = find_subslice(&resp, b"\r\n\r\n") {
                        header_end = Some(i + 4);
                        let head = String::from_utf8_lossy(&resp[..i]);
                        for line in head.lines() {
                            if let Some(rest) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                content_len = rest.trim().parse().ok();
                            }
                        }
                    }
                }
                // 已知 Content-Length 且已收齐 body 即停止，避免 keep-alive 导致阻塞。
                if let (Some(he), Some(cl)) = (header_end, content_len) {
                    if resp.len() >= he + cl {
                        break;
                    }
                }
            }
            Err(e) => return Err(format!("读取响应失败: {}", e)),
        }
    }
    let text = String::from_utf8_lossy(&resp);
    let he = header_end.ok_or_else(|| "响应无完整头部".to_string())?;
    let status_line = text.lines().next().unwrap_or("");
    if !status_line.contains(" 200 ") {
        return Err(format!("HTTP 状态异常: {}", status_line.trim()));
    }
    Ok(text[he..].to_string())
}

/// 在字节切片中查找子切片位置（用于定位 HTTP 头部结束的 \r\n\r\n）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 极简**流式** HTTP/1.1 客户端（SSE）：POST 到本地服务端，逐行解析 `data:` 事件，
/// 把每个 delta 内容块即时回调给 `on_data`，返回累积的完整内容。
/// 与 `http_request` 的区别：不缓存整包再解析，而是边收边回调，用于译文流式上屏。
fn http_request_stream(
    port: u16,
    path: &str,
    body: &str,
    read_timeout_ms: u32,
    on_data: &mut dyn FnMut(&str),
) -> Result<String, String> {
    let addr = format!("127.0.0.1:{}", port)
        .to_socket_addrs()
        .map_err(|e| format!("解析地址失败: {}", e))?
        .next()
        .ok_or_else(|| "无法解析 127.0.0.1".to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(600))
        .map_err(|e| format!("连接本地服务端失败(127.0.0.1:{}): {}", port, e))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(read_timeout_ms as u64)))
        .ok();

    let req = format!(
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        port,
        body.len(),
        body
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("发送请求失败: {}", e))?;

    let mut buf = [0u8; 4096];
    let mut line_buf: Vec<u8> = Vec::new();
    let mut status_ok = false;
    let mut full = String::new();
    let mut done = false;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                line_buf.extend_from_slice(&buf[..n]);
                while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = line_buf.drain(..=pos).collect();
                    if handle_sse_line(&line, &mut status_ok, &mut full, on_data) {
                        done = true;
                        break;
                    }
                }
                if done {
                    break;
                }
            }
            Err(e) => return Err(format!("读取流式响应失败: {}", e)),
        }
    }
    // 服务器可能在最后一块数据后不带换行，补处理一次残留行。
    if !done && !line_buf.is_empty() {
        let line = std::mem::take(&mut line_buf);
        let _ = handle_sse_line(&line, &mut status_ok, &mut full, on_data);
    }
    if !status_ok {
        return Err(format!(
            "HTTP 状态异常: {}",
            full.lines().next().unwrap_or("(无响应)")
        ));
    }
    Ok(full)
}

/// 处理一行 SSE 数据：返回 true 表示收到流结束标记（`[DONE]` 或 finish_reason=stop）。
fn handle_sse_line(
    line: &[u8],
    status_ok: &mut bool,
    full: &mut String,
    on_data: &mut dyn FnMut(&str),
) -> bool {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return false;
    }
    // 状态行：HTTP/1.1 200 OK
    if let Some(rest) = line.strip_prefix("HTTP/") {
        let code: u16 = rest
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        *status_ok = code == 200;
        return false;
    }
    // SSE 数据行：data: {...}
    if let Some(payload) = line.strip_prefix("data:") {
        let payload = payload.trim();
        if payload == "[DONE]" {
            return true;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
            if let Some(delta) = v["choices"][0]["delta"]["content"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                full.push_str(delta);
                on_data(delta);
            }
            // finish_reason=stop 也视为流结束（部分 llama-server 版本不发 [DONE]）。
            if v["choices"][0]["finish_reason"].as_str() == Some("stop") {
                return true;
            }
        }
    }
    false
}

/// 本地翻译服务端启动互斥锁：串行化「预热线程」与「首次翻译线程」对服务端的拉起，
/// 避免两个线程同时走到 spawn（llama.cpp 的 SO_REUSEADDR 会让两份服务端同时绑上
/// 8080，健康探测/请求可能命中错误实例）。
static SERVER_START_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn server_start_lock() -> &'static Mutex<()> {
    SERVER_START_LOCK.get_or_init(|| Mutex::new(()))
}

/// 确保本地 llama.cpp 服务端已启动并真正就绪；未启动时拉起，轮询 /health 直到模型加载完成。
///
/// 关键：先清理所有可能残留的旧 llama-server 进程。llama.cpp 在 Windows 上用
/// SO_REUSEADDR，多个服务端能同时绑定同一端口；若直接复用残留进程，健康探测可能
/// 命中半死/未就绪的实例，导致偶发 503 或 90s 启动超时（已实测多份进程抢 8080）。
fn ensure_local_server(cfg: &TranslatorConfig) -> Result<(), String> {
    // 全局互斥：同一时刻只允许一个线程执行"检查/清理/拉起/等待就绪"全过程。
    let _start_guard = server_start_lock()
        .lock()
        .map_err(|_| "内部锁异常".to_string())?;
    // 本进程持有的子进程如果还活着且已就绪，直接复用，不再拉起。
    {
        let mut guard = local_server_handle()
            .lock()
            .map_err(|_| "内部锁异常".to_string())?;
        if let Some(child) = guard.as_mut() {
            let alive = child.try_wait().map(|s| s.is_none()).unwrap_or(false);
            if alive && server_healthy(cfg.local_port) {
                return Ok(());
            }
            // 子进程已退出或不可达：放弃旧句柄，稍后清理并重启。
            let _ = child.kill();
            *guard = None;
        }
    }
    // 清理残留的 llama-server 时，必须保留本应用正在运行的视觉模型 OCR 服务端
    // （端口 8081，同样叫 llama-server.exe）：否则翻译服务端一启动就把 OCR 端杀了，
    // OCR 要冷启动几十秒才恢复，界面会长时间断流/卡顿。
    let mut keep: Vec<u32> = Vec::new();
    if let Ok(mut g) = vl_server_handle().try_lock()
        && let Some(child) = g.as_mut()
        && child.try_wait().map(|s| s.is_none()).unwrap_or(false)
    {
        keep.push(child.id());
    }
    kill_existing_server_except(&keep);

    let child = Command::new(&cfg.local_server)
        .args([
            "-m",
            &cfg.local_model,
            "--port",
            &cfg.local_port.to_string(),
            "--host",
            "127.0.0.1",
            "--ctx-size",
            "4096",
            "-ngl",
            &cfg.local_ngl.to_string(),
        ])
        .stdout(open_server_log())
        .stderr(open_server_log())
        // 关键：GUI 程序的 stdin 是无效句柄，继承给子进程会让 llama-server 在
        // 启动后立刻因 stdin 异常而退出（日志里 "cleaning up before exit"）。
        // 显式置为 null，等价于用 NUL 重定向，服务端可稳定常驻。
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("无法启动 {}: {}", cfg.local_server, e))?;
    {
        let mut guard = local_server_handle()
            .lock()
            .map_err(|_| "内部锁异常".to_string())?;
        *guard = Some(child);
    }

    // 轮询 /health 就绪（最多约 90s；CUDA 首次加载更慢）。
    // 子进程若已退出说明启动失败——最常见是 CUDA 版缺少运行库(cudart/cublas)而崩溃。
    // 立刻报错并给出可操作建议，而不是傻等整个就绪窗口。
    for _ in 0..180 {
        if let Ok(mut guard) = local_server_handle().try_lock() {
            if let Some(child) = guard.as_mut() {
                if let Ok(Some(_code)) = child.try_wait() {
                    return Err(
                        "[本地模型] llama-server 进程启动后立刻退出（崩溃）。\n\
                         最常见原因：CUDA 版服务端缺少对应的 CUDA 运行库 \
                         (cudart64_*/cublas64_*/cublasLt64_*.dll，需与 CUDA 13.3 匹配的运行时且位于 exe 同目录)。\n\
                         解决：① 运行 download_cuda_runtime.ps1 一键补装运行库；\
                         ② 或把翻译模式切到「本地识图+远程翻译」(填好 API Key 走云端)。"
                            .to_string(),
                    );
                }
            }
        }
        if server_healthy(cfg.local_port) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    let mut detail = format!(
        "本地模型服务端在端口 {} 启动超时（模型可能加载失败：检查显卡驱动/CUDA、显存，或把 GPU 层数(-ngl)调小）",
        cfg.local_port
    );
    if let Ok(log) = std::fs::read_to_string(std::env::temp_dir().join("xiangxu_llama.log")) {
        let mut tail: Vec<&str> = log.lines().rev().take(10).collect();
        tail.reverse();
        let t = tail.join("\n");
        if !t.trim().is_empty() {
            detail.push_str(&format!("\n服务端日志(末尾):\n{}", t));
        }
    }
    detail.push_str("\n（完整日志见临时目录 xiangxu_llama.log）");
    Err(detail)
}

/// 进程级持有本地服务端子进程，整个应用生命周期内常驻。
fn local_server_handle() -> &'static Mutex<Option<Child>> {
    static HANDLE: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
    HANDLE.get_or_init(|| Mutex::new(None))
}

/// 结束所有残留的 llama-server 进程（保留 `keep` 中列出的 PID）。
/// 本应用是唯一使用方，直接全杀安全；用于启动前清理与退出时回收，
/// 避免 SO_REUSEADDR 导致的同端口多实例堆积（会令健康探测命中陈旧实例而超时）。
///
/// 关键：**不能无差别全杀**——本应用同时跑着两个 llama-server 实例
/// （8080 翻译 + 8081 视觉模型 OCR）。翻译服务端启动时若把 OCR 服务端也杀了，
/// OCR 要冷启动几十秒才能恢复，表现为"翻译到框里越来越慢/断流"。
/// 因此启动翻译服务端前只清理「非本应用当前持有」的残留进程。
fn kill_existing_server_except(keep: &[u32]) {
    #[cfg(windows)]
    {
        // taskkill 支持 /FI "PID ne <pid>" 过滤器（多个 /FI 为 AND），
        // 与 /IM 组合可"杀同名但排除指定 PID"。
        let mut args: Vec<String> = vec!["/F".into(), "/IM".into(), "llama-server.exe".into()];
        for pid in keep {
            args.push("/FI".into());
            args.push(format!("PID ne {}", pid));
        }
        let _ = std::process::Command::new("taskkill").args(&args).output();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("pkill")
            .args(["-f", "llama-server"])
            .output();
    }
}

/// 全杀（保留空集）：仅用于应用启动清理与退出回收——此刻没有任何服务端在提供服务。
fn kill_existing_server() {
    kill_existing_server_except(&[]);
}

/// 应用退出时回收本地模型服务端（由 `eframe::App::on_exit` 调用）。
pub fn kill_local_server() {
    kill_existing_server();
}

/// 服务端是否真正就绪：GET /health 返回 HTTP 200 表示模型已加载、可接受翻译请求。
/// 仅探测 TCP 端口不够——llama.cpp 常在绑定端口后仍在后台加载模型，
/// 此时 /v1/chat/completions 会返回 503；必须用 /health 确认模型已就绪。
/// 改用本地 HTTP 客户端，避免反复启动 curl.exe 带来的 ~2s/次 启动延迟。
fn server_healthy(port: u16) -> bool {
    http_request("GET", port, "/health", None, 5000).is_ok()
}

/// 把 llama-server 的 stdout/stderr 追加写到临时目录日志，便于排查启动失败。
fn open_server_log() -> Stdio {
    let path = std::env::temp_dir().join("xiangxu_llama.log");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    }
}

/// 视觉模型 OCR 服务端启动节流：避免服务端启动失败后每帧都重复尝试拉起（进程抖动）。
static VL_START_THROTTLE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// 视觉模型 OCR 服务端拉起由 `maybe_start_vl_server` / `ensure_vl_server` 负责（内含 15s 节流）。

/// 视觉模型服务端就绪状态缓存：由监控线程按 ~1s 节流在**后台**刷新，UI 线程只读此值，
/// 从而避免每帧在 UI 线程里同步做网络健康探测。
/// 关键：端口未监听时 `server_healthy` 的连接会阻塞数百毫秒~数秒（取决于 Windows 网络栈/防火墙），
/// 若放在 UI 线程每帧调用，会导致点击截图、打开设置等任何触发重绘的操作都明显卡顿。
static VL_READY: OnceLock<AtomicBool> = OnceLock::new();
fn vl_ready_cell() -> &'static AtomicBool {
    VL_READY.get_or_init(|| AtomicBool::new(false))
}

/// 由监控线程按 ~1s 节流调用：探测视觉模型服务端是否就绪并写入缓存。
/// 放在后台线程，UI 永不因这次网络请求而阻塞。
pub fn refresh_vl_ready(port: u16) {
    vl_ready_cell().store(server_healthy(port), Ordering::Relaxed);
}

/// 视觉模型 OCR 服务端是否已就绪（供 UI 显示「预热中…」状态）。直接读缓存，零网络开销。
pub fn vl_server_ready(_cfg: &TranslatorConfig) -> bool {
    vl_ready_cell().load(Ordering::Relaxed)
}

/// 启动时清理上一会话（崩溃未正常退出）可能残留的 llama-server 进程，
/// 回收其占用的 CPU/显存，避免软件一打开就因残留的 3B 视觉模型正在加载而整体卡顿。
/// 正常退出时 `on_exit` 已全杀，故此处仅针对崩溃遗留；调用安全（本应用是唯一使用方）。
pub fn cleanup_orphan_servers() {
    kill_existing_server();
}

/// 进程级持有视觉模型 OCR 服务端子进程（独立端口，与翻译服务端隔离）。
fn vl_server_handle() -> &'static Mutex<Option<Child>> {
    static HANDLE: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
    HANDLE.get_or_init(|| Mutex::new(None))
}

/// 确保视觉模型 OCR 服务端（跑 Qwen2.5-VL + mmproj 的第二个 llama-server 实例）已启动并就绪。
/// 与翻译服务端（端口 8080）相互独立，且**不**调用 kill_existing_server，
/// 以免误杀翻译服务端；若 vl_port 上已有健康实例则直接复用（兼容上次遗留的常驻进程）。
fn ensure_vl_server(cfg: &TranslatorConfig) -> Result<(), String> {
    if cfg.vl_model.trim().is_empty() || cfg.vl_mmproj.trim().is_empty() {
        return Err("[视觉OCR] 请在设置里指定 VL 模型(.gguf)与 mmproj 路径".to_string());
    }
    // 节流：若 15s 内刚失败过，直接返回，不再重复拉起
    {
        let g = VL_START_THROTTLE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| "内部锁异常".to_string())?;
        if let Some(t) = *g {
            if t.elapsed() < Duration::from_secs(15) {
                return Err(
                    "[视觉OCR] 服务端启动失败，15 秒内暂不重试（检查 mmproj/模型是否匹配、llama-server 版本是否支持 Qwen2.5-VL）"
                        .to_string(),
                );
            }
        }
    }
    // 本进程持有的子进程若健康则复用
    {
        let mut guard = vl_server_handle()
            .lock()
            .map_err(|_| "内部锁异常".to_string())?;
        if let Some(child) = guard.as_mut() {
            let alive = child.try_wait().map(|s| s.is_none()).unwrap_or(false);
            if alive && server_healthy(cfg.vl_port) {
                return Ok(());
            }
            let _ = child.kill();
            *guard = None;
        }
    }
    // 端口已被别的实例占着且健康：直接复用，避免重复拉起
    if server_healthy(cfg.vl_port) {
        return Ok(());
    }
    let exe = if cfg.local_server.trim().is_empty() {
        "llama-server.exe".to_string()
    } else {
        cfg.local_server.clone()
    };
    *VL_START_THROTTLE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "内部锁异常".to_string())? = Some(Instant::now());
    let child = Command::new(&exe)
        .args([
            "-m",
            &cfg.vl_model,
            "--mmproj",
            &cfg.vl_mmproj,
            "--port",
            &cfg.vl_port.to_string(),
            "--host",
            "127.0.0.1",
            "--ctx-size",
            "4096",
            "-ngl",
            &cfg.vl_ngl.to_string(),
        ])
        .stdout(open_server_log())
        .stderr(open_server_log())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("[视觉OCR] 无法启动 {}: {}", exe, e))?;
    {
        let mut guard = vl_server_handle()
            .lock()
            .map_err(|_| "内部锁异常".to_string())?;
        *guard = Some(child);
    }
    for _ in 0..180 {
        if let Ok(mut guard) = vl_server_handle().try_lock() {
            if let Some(child) = guard.as_mut() {
                if let Ok(Some(_code)) = child.try_wait() {
                    return Err(
                        "[视觉OCR] llama-server(VL) 启动后立刻退出。请确认 mmproj 与模型匹配、\
                         且 llama-server 版本支持 Qwen2.5-VL 多模态（较新版 llama.cpp）。"
                            .to_string(),
                    );
                }
            }
        }
        if server_healthy(cfg.vl_port) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "视觉模型 OCR 服务端在端口 {} 启动超时（模型可能加载失败）",
        cfg.vl_port
    ))
}

/// 极简 base64 编码（RFC 4648），用于把截图塞进多模态请求的 data URI，避免额外依赖。
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 在后台（重新）拉起视觉模型 OCR 服务端，但仅在「本进程未持有存活实例」时真正启动一次，
/// 避免每帧都 spawn 进程。`ensure_vl_server` 内部会复用健康实例、重启死亡实例，并有 15s 节流，
/// 因此服务端因故退出后能自动拉起、监控线程自动恢复识别（修复“只识别一次后没反应”）。
fn maybe_start_vl_server(cfg: &TranslatorConfig) {
    // 若已持有存活子进程（可能仍在加载模型），交给它即可，不再打扰。
    if let Ok(mut g) = vl_server_handle().try_lock() {
        if let Some(child) = g.as_mut() {
            if child.try_wait().map(|s| s.is_none()).unwrap_or(false) {
                return; // 子进程还活着，等它就绪
            }
            // 子进程已死：清掉句柄，下方重新拉起。
            let _ = child.kill();
            *g = None;
        }
    }
    // 端口若已被别的健康实例占着，ensure_vl_server 会直接复用，不会重复拉起。
    let cfg = cfg.clone();
    thread::spawn(move || {
        let _ = ensure_vl_server(&cfg);
    });
}

/// 公开入口：用户点“开始监控”时主动预热视觉模型服务端，使其在首帧到达前就开始加载，
/// 缩短第一次识别的等待（3B 模型 + mmproj 冷启动需几十秒）。
pub fn prefetch_vl_server(cfg: &TranslatorConfig) {
    maybe_start_vl_server(cfg);
}

/// 公开入口：用户点“开始监控”时同步预热**本地翻译**服务端（仅本地模式）。
/// 否则第一句翻译要现场拉起 llama-server 并等 1.5B 模型加载（数秒~数十秒），
/// 表现为"第一句迟迟不出译文"。与视觉模型预热并行进行，互不阻塞。
pub fn prefetch_translation_server(cfg: &TranslatorConfig) {
    if cfg.mode != AppMode::LocalVisionLocal {
        return;
    }
    if cfg.local_server.trim().is_empty() || cfg.local_model.trim().is_empty() {
        return;
    }
    let cfg = cfg.clone();
    thread::spawn(move || {
        // 稍等片刻，让视觉模型预热先把自己的子进程句柄登记好，
        // 这样翻译服务端启动前的清理不会误杀正在加载的 OCR 服务端。
        thread::sleep(Duration::from_millis(500));
        let _ = ensure_local_server(&cfg);
    });
}

/// 用本地视觉模型（Qwen2.5-VL）对截图做**流式** OCR：把图像以 PNG+base64 走多模态
/// /v1/chat/completions（stream=true），模型边转录边把已产出的文字片段回调给 `on_chunk`，
/// 让原文框"跟着打字机出字"，避免等整段转录完才一次性上屏（3B 模型单次推理 1~3s，
/// 流式能把"界面卡着不动"的等待变成逐字浮现）。效果等价于人眼读图，能识别
/// 艺术字/彩色字（传统字形匹配引擎做不到）。
///
/// 返回约定：
/// - `Ok(text)` 非空：成功转录出原文。
/// - `Ok("")`   ：画面无文字（视觉模型回 NO_TEXT），调用方据此清空界面、不翻译。
/// - `Err(_)`   ：视觉模型服务端尚未就绪（冷启动中），调用方跳过本帧、不阻塞。
pub fn recognize_text_vl_stream(
    img: &DynamicImage,
    cfg: &TranslatorConfig,
    on_chunk: &mut dyn FnMut(&str),
) -> Result<String, String> {
    // 不在 OCR 热路径上阻塞等待服务端冷启动：未就绪就（重新）拉起服务端（后台、不阻塞），
    // 本帧直接返回错误，由调用方跳过本帧、冷却结束后再试，保证监控线程不卡死、能自动恢复。
    if !server_healthy(cfg.vl_port) {
        maybe_start_vl_server(cfg);
        return Err("[视觉OCR] 服务端尚未就绪，本帧跳过".to_string());
    }

    // 等比降采样到长边上限（可在设置里调小提速）：减少图像预填充开销
    // （上传体积 + 服务端 token 数），加快 OCR；游戏文本字号大，识别率几乎无损。
    // 下限 256px 防止用户调得过小导致完全看不清字。
    let max_side = cfg.vl_max_side.max(256);
    let img = if img.width().max(img.height()) > max_side {
        let scale = max_side as f32 / img.width().max(img.height()) as f32;
        img.resize(
            (img.width() as f32 * scale) as u32,
            (img.height() as f32 * scale) as u32,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img.clone()
    };

    // 编码为 PNG（内存中）
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut cursor = Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| format!("[视觉OCR] 图像编码失败: {}", e))?;
    }
    let data_uri = format!("data:image/png;base64,{}", base64_encode(&buf));
    let prompt = "If there is no readable text in the image, reply with exactly the words NO_TEXT and nothing else. \
        Otherwise transcribe all text visible in the image, preserving the original language and line breaks. \
        Output ONLY the transcribed text, with no commentary, no quotes, and no markdown.";
    let body = json!({
        "model": "vl",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": data_uri}},
                {"type": "text", "text": prompt}
            ]
        }],
        "temperature": 0.0,
        "stream": true
    });
    let body_str = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    // 视觉模型推理（尤其图像预填充）比纯文本慢，放宽读取超时到 120s。
    let content = http_request_stream(
        cfg.vl_port,
        "/v1/chat/completions",
        &body_str,
        120000,
        on_chunk,
    )?;
    let content = content.trim().to_string();
    // 画面无文字：视觉模型按要求回 NO_TEXT，映射为空串，调用方据此清空界面、
    // 不会把场景当成文字去“解释/翻译”。
    if content.eq_ignore_ascii_case("NO_TEXT") {
        return Ok(String::new());
    }
    Ok(content)
}

fn mock_translate(text: &str) -> String {
    format!("[模拟译文] {}", text.trim())
}

/// 统一构造翻译消息（system + user 两角色）。
/// 指令放 **system** 角色：Qwen2.5 等指令模型对 system 指令的遵循度显著高于
/// "指令+正文挤在同一条 user 消息"的写法——后者正是"复读原文""改写已译术语"
/// 两个症状的主要来源。术语保留提示（glossary 预替换后传入）也放 system。
/// `force`：质量校验失败后的重试置 true，附加更强硬的指令。
fn build_messages(
    text: &str,
    target: Lang,
    term_hint: Option<&str>,
    force: bool,
) -> serde_json::Value {
    let mut system = format!(
        "You are a professional game text translator. Translate the user's text into {}. \
         Output ONLY the translation — no explanations, no quotes, no markdown. \
         The input text may already contain correctly translated Chinese proper nouns \
         (game terms). Copy them into the output EXACTLY as written: never re-translate \
         them, never transliterate them, never rephrase them.",
        target.target_hint(),
    );
    if let Some(hint) = term_hint {
        system.push('\n');
        system.push_str(hint);
    }
    if force {
        system.push_str(
            "\nIMPORTANT: Your previous reply was rejected because it copied the input \
             untranslated or altered the translated proper nouns. This time you MUST output \
             a real, complete translation and keep every Chinese proper noun character-for-character.",
        );
    }
    json!([
        {"role": "system", "content": system},
        {"role": "user", "content": text},
    ])
}

/// 译文质量校验：返回 true 表示**不合格、需要重试**。
/// 覆盖小模型（1.5B）的两类高发失败：
///   1. **复读原文**：输出与输入归一化后一致（等于没翻译）。仅当输入确实含
///      拉丁字母时判定，纯数字/符号文本翻译前后相同属正常。
///   2. **术语丢失**：glossary 预替换进正文的中文术语没有出现在输出里
///      （被模型改写/回译/丢弃）。比较前去掉全部空白，兼容"101 号道路"这类
///      多打一个空格的无害差异。
fn translation_bad(input: &str, output: &str, hits: &[(String, String)]) -> bool {
    let norm = |s: &str| {
        s.trim()
            .to_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    };
    let has_latin = input.chars().any(|c| c.is_ascii_alphabetic());
    if has_latin && norm(input) == norm(output) {
        return true;
    }
    let out = norm(output);
    hits.iter().any(|(_, zh)| !out.contains(&norm(zh)))
}

/// 通过系统 curl.exe 调用 OpenAI 兼容的聊天补全接口。
fn translate_via_curl(
    endpoint: &str,
    api_key: &str,
    model: &str,
    text: &str,
    target: Lang,
    hits: &[(String, String)],
) -> Result<String, String> {
    let messages = build_messages(text, target, glossary::term_hint(hits).as_deref(), false);
    let body = json!({
        "model": model,
        "messages": messages,
        "temperature": 0.3,
        "stream": false,
    });
    let body_str = serde_json::to_string(&body).map_err(|e| e.to_string())?;

    // 允许用户填 base_url（如 https://api.deepseek.com）或完整端点 URL。
    // SDK 文档通常给出 base_url，而直接 curl 需要 /chat/completions 路径。
    let endpoint = normalize_chat_endpoint(endpoint)?;

    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "-S",
            "-f",
            "-X",
            "POST",
            &endpoint,
            "--connect-timeout",
            "8",
            "--max-time",
            "25",
            "-H",
            "Content-Type: application/json",
            "-H",
            &format!("Authorization: Bearer {}", api_key),
            "-d",
            &body_str,
        ])
        .output()
        .map_err(|e| {
            format!(
                "调用 curl 失败（请确认系统已安装 curl.exe，Windows 10+ 通常自带）: {}",
                e
            )
        })?;

    if !output.status.success() {
        // -f 让 curl 在 HTTP >= 400 时返回非 0，并在 stderr 给出原因（如 404/401）。
        let stderr = String::from_utf8_lossy(&output.stderr);
        let body = String::from_utf8_lossy(&output.stdout);
        let detail = if !body.trim().is_empty() {
            format!("响应体: {}", body.trim())
        } else if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            format!("curl 退出码 {:?}", output.status.code())
        };
        return Err(format!(
            "HTTP 请求失败（端点/Key/模型可能不对）: {}",
            detail
        ));
    }

    let resp: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| {
        format!(
            "解析接口响应失败: {} | 原始返回: {}",
            e,
            String::from_utf8_lossy(&output.stdout)
        )
    })?;

    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(content)
}

/// 把用户输入的 base_url 或完整端点统一为 `/chat/completions` 完整路径。
fn normalize_chat_endpoint(endpoint: &str) -> Result<String, String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("端点 URL 不能为空".to_string());
    }
    if trimmed.ends_with("/chat/completions") {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("{}/chat/completions", trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 端到端测试共享模型进程；串行使用，避免另一个测试回收正在推理的服务端。
    static MODEL_TEST_LOCK: Mutex<()> = Mutex::new(());
    use crate::lang::Lang;

    #[test]
    fn quality_check_detects_echo() {
        let hits = vec![("Route 101".into(), "101号道路".into())];
        // 复读原文（输入含拉丁字母、输出=输入）→ 不合格。
        // 注：真实链路里 input 是术语预替换后的文本，此处直接模拟之。
        assert!(translation_bad(
            "Go to 101号道路 now.",
            "Go to 101号道路 now.",
            &hits
        ));
        // 正常翻译（术语保留）→ 合格。
        assert!(!translation_bad(
            "Go to 101号道路 now.",
            "现在前往101号道路。",
            &hits
        ));
        // 纯数字/符号输入翻译前后相同 → 不算复读。
        assert!(!translation_bad("123 456", "123 456", &[]));
    }

    #[test]
    fn quality_check_detects_term_loss() {
        let hits = vec![
            ("Treecko".into(), "木守宫".into()),
            ("Route 101".into(), "101号道路".into()),
        ];
        // 术语被改写（木守宫 → 草系宝可梦）→ 不合格。
        assert!(translation_bad(
            "木守宫 appeared on 101号道路!",
            "草系宝可梦出现在101号道路上！",
            &hits
        ));
        // 输出里术语带多余空格（101 号道路）→ 空白差异不影响判定，合格。
        assert!(!translation_bad(
            "木守宫 appeared on 101号道路!",
            "木守宫出现在 101 号道路上！",
            &hits
        ));
        // 输出与输入仅大小写/空白不同（即没翻译）→ 仍判复读，不合格。
        assert!(translation_bad("Go to the shop", " go to the shop ", &[]));
    }

    #[test]
    fn build_messages_roles() {
        let hits = vec![("Treecko".into(), "木守宫".into())];
        let msgs = build_messages(
            "木守宫 uses Pound!",
            Lang::Zh,
            glossary::term_hint(&hits).as_deref(),
            false,
        );
        let arr = msgs.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[1]["content"], "木守宫 uses Pound!");
        // 术语提示注入 system；force 模式追加加强指令。
        let sys = arr[0]["content"].as_str().unwrap();
        assert!(sys.contains("Treecko"));
        let forced = build_messages("hi", Lang::Zh, None, true);
        assert!(forced[0]["content"].as_str().unwrap().contains("IMPORTANT"));
    }

    /// 自检测试：真正拉起本地 llama.cpp 服务端，跑一句翻译，验证端到端链路。
    /// 运行：cargo test selftest_local_translate -- --nocapture
    #[test]
    fn selftest_local_translate() {
        let _guard = MODEL_TEST_LOCK.lock().unwrap();
        let mut cfg = TranslatorConfig::default();
        cfg.mode = AppMode::LocalVisionLocal;
        eprintln!(
            "SELFTEST cfg: server={} model={} port={}",
            cfg.local_server, cfg.local_model, cfg.local_port
        );
        let out = translate("Press [ENTER] to begin the quest.", Lang::Zh, &cfg);
        eprintln!("SELFTEST RESULT: {:?}", out);
        assert!(!out.is_empty(), "翻译结果为空");
        assert!(
            !out.starts_with("[本地模型]"),
            "翻译返回了本地模型错误: {}",
            out
        );
        kill_existing_server();
    }

    /// 自检测试：验证本地模型**流式**翻译端到端（SSE 逐字回调 + 拼接一致性）。
    /// 运行：cargo test selftest_local_translate_stream -- --nocapture
    #[test]
    fn selftest_local_translate_stream() {
        let _guard = MODEL_TEST_LOCK.lock().unwrap();
        let mut cfg = TranslatorConfig::default();
        cfg.mode = AppMode::LocalVisionLocal;
        let mut parts: Vec<String> = Vec::new();
        let out = translate_stream(
            "Press [ENTER] to begin the quest.",
            Lang::Zh,
            &cfg,
            &mut |chunk| {
                parts.push(chunk.to_string());
                eprintln!("SELFTEST STREAM CHUNK: {:?}", chunk);
            },
        );
        eprintln!("SELFTEST STREAM RESULT: {:?}", out);
        let joined: String = parts.concat();
        eprintln!("SELFTEST STREAM JOINED: {:?}", joined);
        assert_eq!(joined, out, "流式片段拼接应与最终结果完全一致");
        assert!(!out.is_empty(), "翻译结果为空");
        assert!(
            !out.starts_with("[本地模型]"),
            "翻译返回了本地模型错误: {}",
            out
        );
        kill_existing_server();
    }

    /// 自检测试：验证服务端自动选择逻辑（只选 CUDA 版，默认全部卸载到 GPU）。
    /// 运行：cargo test default_server_detect -- --nocapture
    #[test]
    fn default_server_detect() {
        let cfg = TranslatorConfig::default();
        eprintln!("DEFAULT local_server = {}", cfg.local_server);
        eprintln!("DEFAULT local_ngl = {}", cfg.local_ngl);
        eprintln!("DEFAULT vl_ngl = {}", cfg.vl_ngl);
        // 只支持 CUDA 版服务端：默认路径必须指向 llama-cuda（或旧目录名）。
        assert!(
            cfg.local_server.contains("llama-cuda") || cfg.local_server.contains("cuda-13.3"),
            "默认服务端应指向 CUDA 版: {}",
            cfg.local_server
        );
        // GPU 卸载默认拉满（-ngl 99）。
        assert_eq!(cfg.local_ngl, 99, "翻译默认应全部卸载到 GPU");
        assert_eq!(cfg.vl_ngl, 99, "识图默认应全部卸载到 GPU");
    }

    /// 自检测试：**流式 OCR** 端到端（真拉起视觉模型服务端，截取主屏幕流式识图）。
    /// 验证：SSE 流式识图不报错；片段拼接经清洗后与完整结果一致（有文字时逐字出流、
    /// 无文字时模型回 NO_TEXT）。运行：cargo test selftest_vl_stream_ocr -- --nocapture
    #[test]
    fn selftest_vl_stream_ocr() {
        let _guard = MODEL_TEST_LOCK.lock().unwrap();
        let mut cfg = TranslatorConfig::default();
        cfg.mode = AppMode::LocalVisionLocal;
        cfg.vl_max_side = 640; // 测试提速：小图预填充快
        let img = match crate::capture::capture_primary() {
            Ok((i, _, _)) => i.resize(480, 270, image::imageops::FilterType::Lanczos3),
            Err(e) => {
                eprintln!("SELFTEST VL: 主屏截图失败，跳过: {}", e);
                return;
            }
        };
        // 正常监控会在后台预热并跳过冷启动帧；测试需等模型就绪后验证流式链路。
        ensure_vl_server(&cfg).expect("视觉模型服务端预热失败");
        let mut chunks: Vec<String> = Vec::new();
        let t0 = Instant::now();
        let out = recognize_text_vl_stream(&img, &cfg, &mut |c| {
            chunks.push(c.to_string());
            eprintln!("SELFTEST VL CHUNK: {:?}", c);
        });
        eprintln!("SELFTEST VL ELAPSED: {:?}", t0.elapsed());
        match out {
            Ok(text) => {
                eprintln!("SELFTEST VL RESULT: {:?}", text);
                let joined: String = chunks.concat();
                // 与函数内部相同的清洗：trim + NO_TEXT → 空串
                let cleaned = joined.trim();
                let cleaned = if cleaned.eq_ignore_ascii_case("NO_TEXT") {
                    ""
                } else {
                    cleaned
                };
                assert_eq!(cleaned, text, "流式片段拼接(清洗后)应与完整结果一致");
                assert!(!chunks.is_empty(), "流式 OCR 至少应回调过片段");
                if text.trim().is_empty() {
                    eprintln!("SELFTEST VL: 画面无文字 (NO_TEXT)，流式链路验证通过");
                } else {
                    eprintln!(
                        "SELFTEST VL: 流式识别成功，{} 个片段 -> {:?}",
                        chunks.len(),
                        text
                    );
                }
            }
            Err(e) => panic!("视觉模型流式 OCR 失败: {}", e),
        }
        kill_existing_server();
    }
}
