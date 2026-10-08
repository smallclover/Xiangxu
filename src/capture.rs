//! 屏幕区域截取：使用 xcap 按物理坐标截取指定区域。
//! 坐标空间与 xcap 的 Monitor 物理像素一致。

use image::DynamicImage;
use xcap::Monitor;

/// 屏幕物理像素区域。
#[derive(Clone, Copy, Debug)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// 截取指定区域。找到包含该区域的显示器，截取后裁出对应部分。
pub fn capture_region(rect: ScreenRect) -> Result<DynamicImage, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    for m in monitors {
        let mx = m.x().map_err(|e| e.to_string())?;
        let my = m.y().map_err(|e| e.to_string())?;
        let mw = m.width().map_err(|e| e.to_string())?;
        let mh = m.height().map_err(|e| e.to_string())?;
        if rect.x >= mx
            && rect.y >= my
            && (rect.x + rect.w as i32) <= (mx + mw as i32)
            && (rect.y + rect.h as i32) <= (my + mh as i32)
        {
            let img = match m.capture_image() {
                Ok(i) => i,
                Err(_) => continue,
            };
            let lx = (rect.x - mx).max(0) as u32;
            let ly = (rect.y - my).max(0) as u32;
            let cropped = image::imageops::crop_imm(&img, lx, ly, rect.w, rect.h).to_image();
            return Ok(DynamicImage::ImageRgba8(cropped));
        }
    }
    Err("所选区域不在任何显示器范围内".to_string())
}

/// 截取主显示器，作为“框选区域”时的背景底图，并返回其缩放系数与偏移（用于坐标换算）。
pub fn capture_primary() -> Result<(DynamicImage, f32, (i32, i32)), String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    let mut fallback: Option<(DynamicImage, f32, (i32, i32))> = None;
    for m in monitors {
        let offset = (
            m.x().map_err(|e| e.to_string())?,
            m.y().map_err(|e| e.to_string())?,
        );
        let scale = m.scale_factor().map_err(|e| e.to_string())? as f32;
        let img = match m.capture_image() {
            Ok(i) => i,
            Err(_) => continue,
        };
        let data: (DynamicImage, f32, (i32, i32)) = (DynamicImage::ImageRgba8(img), scale, offset);
        if m.is_primary().map_err(|e| e.to_string())? {
            return Ok(data);
        }
        if fallback.is_none() {
            fallback = Some(data);
        }
    }
    fallback.ok_or_else(|| "未找到显示器".to_string())
}

/// 窗口屏幕矩形（物理像素）(left, top, right, bottom)；窗口无效/已销毁时返回 None。
/// 用于「吸附游戏窗口」时把截取区域换算到窗口当前的位置。
#[cfg(windows)]
pub fn window_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe { GetWindowRect(hwnd as _, &mut r) };
    if ok != 0 {
        Some((r.left, r.top, r.right, r.bottom))
    } else {
        None
    }
}

/// 窗口是否处于最小化状态（最小化时窗口矩形会缩到任务栏，不应跟随/截屏）。
#[cfg(windows)]
pub fn window_iconic(hwnd: isize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
    unsafe { IsIconic(hwnd as _) != 0 }
}

#[cfg(not(windows))]
pub fn window_rect(_hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    None
}

#[cfg(not(windows))]
pub fn window_iconic(_hwnd: isize) -> bool {
    false
}
