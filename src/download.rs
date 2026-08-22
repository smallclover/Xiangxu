//! 模型资源下载：一键下载缺失的 GGUF 模型文件。
//!
//! - 下载在后台线程进行（不阻塞 UI），进度通过共享状态 `DownloadProgress` 上报，
//!   egui 是立即模式，UI 每帧读取即可。
//! - 使用系统 native-tls（Windows 上即 schannel），不引入 rustls/ring 等重依赖。
//! - 已存在的文件自动跳过（按最终路径判断），可断点续传式的重跑。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

/// 单个待下载资源。
pub struct ResourceSpec {
    /// 显示名。
    pub name: &'static str,
    /// 下载 URL（Hugging Face resolve 链接，自动跟随重定向到 CDN）。
    pub url: &'static str,
    /// 相对项目根目录的保存路径。
    pub rel_path: &'static str,
    /// 体积提示。
    pub size_hint: &'static str,
}

/// 模型资源清单。顺序即下载顺序（翻译 1.5B → 视觉 3B → mmproj）。
pub const RESOURCES: &[ResourceSpec] = &[
    ResourceSpec {
        name: "翻译模型 Qwen2.5-1.5B (Q4_K_M)",
        url: "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf",
        rel_path: "assets/models/qwen2.5-1.5b-instruct-q4_k_m.gguf",
        size_hint: "~1.0 GB",
    },
    ResourceSpec {
        name: "视觉模型 Qwen2.5-VL-3B (Q4_K_M)",
        url: "https://huggingface.co/ggml-org/Qwen2.5-VL-3B-Instruct-GGUF/resolve/main/Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf",
        rel_path: "assets/models/qwen2.5-vl-3b/Qwen2.5-VL-3B-Instruct.Q4_K_H.gguf",
        size_hint: "~1.8 GB",
    },
    ResourceSpec {
        name: "视觉投影 mmproj (f16)",
        url: "https://huggingface.co/ggml-org/Qwen2.5-VL-3B-Instruct-GGUF/resolve/main/mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf",
        rel_path: "assets/models/qwen2.5-vl-3b/Qwen2.5-VL-3B-Instruct.mmproj.gguf",
        size_hint: "~1.3 GB",
    },
];

/// 下载进度（UI 每帧读取的共享状态）。
pub struct DownloadProgress {
    /// 是否有下载任务进行中。
    pub running: bool,
    /// 每个资源是否已就绪（文件存在且非空）。
    pub done: Vec<bool>,
    /// 当前正在下载的资源下标。
    pub current: Option<usize>,
    /// 当前文件已下载字节数。
    pub bytes_done: u64,
    /// 当前文件总字节数（0 = 服务器未给 content-length）。
    pub bytes_total: u64,
    /// 最近一次错误信息。
    pub error: Option<String>,
}

impl DownloadProgress {
    pub fn new() -> Self {
        Self {
            running: false,
            done: RESOURCES.iter().map(|r| file_ready(r.rel_path)).collect(),
            current: None,
            bytes_done: 0,
            bytes_total: 0,
            error: None,
        }
    }

    /// 重新扫描所有资源的存在性（下载完成后 / 手动点击“检查”时调用）。
    pub fn refresh(&mut self) {
        for (i, r) in RESOURCES.iter().enumerate() {
            self.done[i] = file_ready(r.rel_path);
        }
    }

    /// 缺失资源的数量。
    pub fn missing_count(&self) -> usize {
        self.done.iter().filter(|&&d| !d).count()
    }

    /// 当前文件下载进度百分比（0.0~1.0；总字节未知时按 0 处理）。
    pub fn current_fraction(&self) -> f32 {
        if self.bytes_total == 0 {
            0.0
        } else {
            (self.bytes_done as f32 / self.bytes_total as f32).clamp(0.0, 1.0)
        }
    }

    /// 人类可读的下载量文本，如 "512.3 MB / 1.8 GB"。
    pub fn bytes_text(&self) -> String {
        format!(
            "{} / {}",
            human_size(self.bytes_done),
            if self.bytes_total > 0 {
                human_size(self.bytes_total)
            } else {
                "?".to_string()
            }
        )
    }
}

/// 判断资源文件是否已就绪（存在且非空）。
fn file_ready(rel: &str) -> bool {
    let p = project_dir().join(rel);
    match std::fs::metadata(&p) {
        Ok(m) => m.len() > 0,
        Err(_) => false,
    }
}

/// 项目根目录：优先当前工作目录（绿色版 exe 被解压到任意目录都能用），
/// 退回编译期 CARGO_MANIFEST_DIR（cargo run 场景）。
fn project_dir() -> PathBuf {
    let rel = Path::new("assets");
    if rel.exists() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else if let Some(dir) = option_env!("CARGO_MANIFEST_DIR") {
        PathBuf::from(dir)
    } else {
        PathBuf::from(".")
    }
}

/// 启动后台下载。已有下载进行中时直接忽略。
pub fn start_download(prog: Arc<Mutex<DownloadProgress>>) {
    {
        let mut p = prog.lock().unwrap();
        if p.running {
            return;
        }
        p.running = true;
        p.current = None;
        p.bytes_done = 0;
        p.bytes_total = 0;
        p.error = None;
    }
    thread::spawn(move || run_download(prog));
}

fn run_download(prog: Arc<Mutex<DownloadProgress>>) {
    let base = project_dir();
    let mut err: Option<String> = None;

    for (i, r) in RESOURCES.iter().enumerate() {
        if file_ready(r.rel_path) {
            continue; // 已就绪
        }
        let out_path = base.join(r.rel_path);
        if let Some(parent) = out_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // 先下载到 .part 临时文件，成功后改名，避免留下半个文件被误判为“已就绪”。
        let tmp_path = out_path.with_extension("part");

        {
            let mut p = prog.lock().unwrap();
            p.current = Some(i);
            p.bytes_done = 0;
            p.bytes_total = 0;
        }

        match download_file(r.url, &tmp_path, &prog) {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
                    err = Some(format!("{}: 重命名失败 {}", r.name, e));
                    let _ = std::fs::remove_file(&tmp_path);
                    break;
                }
                {
                    let mut p = prog.lock().unwrap();
                    p.done[i] = true;
                    p.current = None;
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                err = Some(format!("{}: {}", r.name, e));
                break;
            }
        }
    }

    {
        let mut p = prog.lock().unwrap();
        p.running = false;
        p.current = None;
        p.bytes_done = 0;
        p.bytes_total = 0;
        p.error = err;
        p.refresh();
    }
}

/// 下载单个文件，边读边更新进度。
fn download_file(
    url: &str,
    out: &Path,
    prog: &Arc<Mutex<DownloadProgress>>,
) -> Result<(), String> {
    let resp = ureq::get(url)
        .header("User-Agent", "Xiangxu/0.1")
        .call()
        .map_err(|e| format!("请求失败: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let total: u64 = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    {
        let mut p = prog.lock().unwrap();
        p.bytes_total = total;
        p.bytes_done = 0;
    }

    let mut reader = resp.into_body().into_reader();
    let mut file = File::create(out).map_err(|e| format!("无法创建文件: {}", e))?;
    let mut buf = [0u8; 128 * 1024];
    let mut done: u64 = 0;

    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载中断: {}", e))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入失败: {}", e))?;
        done += n as u64;
        // 每 256KB 上报一次，避免频繁加锁拖慢下载
        if done % (256 * 1024) < 128 * 1024 {
            let mut p = prog.lock().unwrap();
            p.bytes_done = done;
        }
    }

    file.flush().map_err(|e| format!("写入失败: {}", e))?;
    // 最后兜底上报一次，确保进度显示 100%
    {
        let mut p = prog.lock().unwrap();
        p.bytes_done = done;
    }
    Ok(())
}

/// 字节数转人类可读文本。
fn human_size(b: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if b as f64 >= GB {
        format!("{:.2} GB", b as f64 / GB)
    } else {
        format!("{:.1} MB", b as f64 / MB)
    }
}
