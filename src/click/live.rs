//! Live warp: cùng kiểu "thấu kính nước" như `metal.rs`, nhưng trong lúc thấu
//! kính sống (1,2 s) thì vùng quanh con trỏ được **chụp lại theo nhịp** (~12
//! hình/giây) nên hình bên trong cập nhật gần như liên tục — mà không cần luồng
//! video ScreenCaptureKit: không có frame buffer nằm trong RAM, không stream chạy
//! nền, không đụng đường capture mà app record màn hình đang dùng.
//!
//! Phần dùng chung với `metal.rs` nằm ở `lens.rs`; module này chỉ khai báo chính
//! sách chụp lại. Mỗi lần chụp vẫn dùng `click::capture_patch` (ảnh 2x, ~20 ms
//! trên M1) nên cần quyền Screen Recording như bản tĩnh.
//!
//! Muốn bỏ: xoá file này + `pub mod live;` trong `click/mod.rs` + nhánh
//! `CLICK_LIVE` trong `spawn()` và mục tương ứng trong `CLICK_MENU` ở `main.rs`.

use crate::click::lens::Effect;
use crate::click::{Bounds, Frame};

/// Nhịp chụp lại: 12 hình/giây là mức còn nhẹ máy (warp lại chỉ 18 fps).
const CAPTURE_INTERVAL: f64 = 1.0 / 12.0;
/// Tối đa số lần chụp lại mỗi frame: khi nhiều thấu kính chồng nhau (spam click)
/// thì giữ cho main thread không bị giữ quá lâu.
const MAX_CAPTURES_PER_UPDATE: usize = 2;

static EFFECT: Effect = Effect::new(Some(CAPTURE_INTERVAL), MAX_CAPTURES_PER_UPDATE);

pub fn spawn(x: f64, y: f64) {
    EFFECT.spawn(x, y);
}

pub fn update(dt: f64) {
    EFFECT.update(dt);
}

pub fn clear() {
    EFFECT.clear();
}

/// Bỏ hết thấu kính đang hiện (màn hình vừa đổi: ảnh chụp cũ đã sai chỗ).
pub fn reset() {
    EFFECT.clear();
}

pub fn bounds() -> Option<Bounds> {
    EFFECT.bounds()
}

pub fn draw(frame: &Frame) {
    EFFECT.draw(frame);
}
