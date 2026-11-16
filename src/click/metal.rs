//! Metal warp: ảnh chụp màn hình quanh điểm click được warp bằng Core Image
//! (`CIBumpDistortion`) rồi vẽ lại thành một "thấu kính nước": phồng lên, lõm
//! xuống và lan rộng dần.
//!
//! Cả vùng ảnh được warp một lần nên mép sóng mượt, không bị banding. Phần Core
//! Image nằm ở `warp.rs`; ở đây chỉ tính bán kính/độ phồng theo tuổi và cache ảnh
//! đã dựng (`warp::RENDER_INTERVAL`, 18 fps) để không phải warp lại mỗi frame.
//!
//! Cần quyền Screen Recording vì phải đọc pixel của app khác.
//! Muốn bỏ: xoá file này + `pub mod metal;` trong `click/mod.rs`.

use crate::click::warp::WarpImage;
use crate::click::{capture_patch, warp, Bounds, Frame, ScreenPatch};
use std::f64::consts::TAU;
use std::sync::Mutex;

const LIFE: f64 = 1.2;
/// Nửa cạnh vùng màn hình chụp quanh điểm click (point). Phải phủ hết bán kính
/// clip của thấu kính: radius tối đa 72 * 1.08 ≈ 78 pt (xem `warp::WarpImage`).
/// Chụp 2x nên vùng 160×160 pt đã là 320×320 px — nhỏ hơn nhiều so với 126 trước
/// đây (504×504 px) mà thấu kính chỉ nhỏ đi ~37% đường kính.
const PATCH_HALF: f64 = 80.0;
const MAX_HITS: usize = 6;

struct Hit {
    x: f64,
    y: f64,
    age: f64,
    patch: Option<ScreenPatch>,
    /// Ảnh warp đã dựng (dùng lại giữa các frame)
    image: Option<WarpImage>,
    /// Tuổi tại lần dựng ảnh gần nhất
    rendered_at: f64,
}

static HITS: Mutex<Vec<Hit>> = Mutex::new(Vec::new());

fn hits() -> std::sync::MutexGuard<'static, Vec<Hit>> {
    match HITS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub fn spawn(x: f64, y: f64) {
    let patch = capture_patch(x, y, PATCH_HALF);
    let mut list = hits();
    while list.len() >= MAX_HITS {
        list.remove(0);
    }
    list.push(Hit {
        x,
        y,
        age: 0.0,
        patch,
        image: None,
        rendered_at: f64::NEG_INFINITY,
    });
}

pub fn update(dt: f64) {
    let mut list = hits();
    list.retain_mut(|hit| {
        hit.age += dt;
        hit.age < LIFE
    });
}

pub fn clear() {
    hits().clear();
}

/// Có thấu kính nào đang động không (còn hiệu lực vẽ từng frame).
pub fn animating() -> bool {
    !hits().is_empty()
}

pub fn bounds() -> Option<Bounds> {
    let mut union: Option<Bounds> = None;
    for hit in hits().iter() {
        let b = crate::click::square_around(hit.x, hit.y, 80.0);
        union = Some(match union {
            Some(u) => (u.0.min(b.0), u.1.min(b.1), u.2.max(b.2), u.3.max(b.3)),
            None => b,
        });
    }
    union
}

/// Bán kính thấu kính và độ phồng hiện tại theo tuổi của cú click.
fn lens(age: f64) -> (f64, f64) {
    let t = (age / LIFE).clamp(0.0, 1.0);
    let radius = 20.0 + 52.0 * (1.0 - (1.0 - t).powi(2));
    // Phồng -> lõm -> phồng nhẹ rồi tắt, giống mặt nước bị ấn
    let wave = (t * TAU * 1.3).sin();
    let scale = 0.55 * wave * (1.0 - t).powf(1.1);
    (radius, scale)
}

pub fn draw(frame: &Frame) {
    let mut list = hits();
    for hit in list.iter_mut() {
        let Some(patch) = hit.patch.as_ref() else {
            continue;
        };
        let (radius, scale) = lens(hit.age);
        if scale.abs() < 0.004 || radius < 2.0 {
            hit.image = None;
            continue;
        }

        if hit.image.is_none() || hit.age - hit.rendered_at >= warp::RENDER_INTERVAL {
            hit.image = warp::render(patch, hit.x, hit.y, radius, scale);
            hit.rendered_at = hit.age;
        }
        if let Some(image) = hit.image.as_ref() {
            image.draw(patch, hit.x, hit.y, radius, frame);
        }

        // Viền sáng mảnh đúng vị trí thấu kính cho dễ thấy mặt nước
        let fade = (1.0 - (hit.age / LIFE).clamp(0.0, 1.0)).powf(1.5);
        crate::stroke_circle(
            hit.x - frame.offset_x,
            hit.y - frame.offset_y,
            radius,
            0.80,
            0.93,
            1.0,
            0.16 * fade,
            1.0,
        );
    }
}
