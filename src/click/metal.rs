//! Metal warp: chụp màn hình quanh điểm click **một lần** rồi warp bằng Core
//! Image thành "thấu kính nước": phồng lên, lõm xuống, lan rộng dần rồi tắt.
//!
//! Toàn bộ phần dùng chung với `live.rs` (vòng đời thấu kính, nhịp warp, viền
//! sáng, vùng vẽ lại) nằm ở `lens.rs`; module này chỉ nói rằng hiệu ứng này chụp
//! một lần. Sửa hình dáng/nhịp ở `lens.rs` là cả hai hiệu ứng cùng đổi.
//!
//! Cần quyền Screen Recording vì phải đọc pixel của app khác.
//! Muốn bỏ: xoá file này + `pub mod metal;` trong `click/mod.rs` + nhánh
//! `CLICK_METAL` trong `spawn()` và mục tương ứng trong `CLICK_MENU` ở `main.rs`.

use crate::click::lens::Effect;
use crate::click::{Bounds, Frame};

/// `None` = không chụp lại: ảnh đứng yên suốt 1,2 s thấu kính sống.
static EFFECT: Effect = Effect::new(None, 0);

pub fn spawn(x: f64, y: f64) {
    EFFECT.spawn(x, y);
}

pub fn update(dt: f64) {
    EFFECT.update(dt);
}

pub fn clear() {
    EFFECT.clear();
}

/// Có thấu kính nào đang động không (dùng để hạ nhịp vẽ overlay).
pub fn animating() -> bool {
    EFFECT.animating()
}

pub fn bounds() -> Option<Bounds> {
    EFFECT.bounds()
}

pub fn draw(frame: &Frame) {
    EFFECT.draw(frame);
}
