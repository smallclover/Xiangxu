//! 嵌入式资源：中文字体（防止 egui 渲染中文出现方块/乱码）。
pub const APP_FONT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/msyhl.ttf"
));
