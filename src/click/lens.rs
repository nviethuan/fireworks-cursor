//! Phần dùng chung của hai hiệu ứng "thấu kính nước" (`metal.rs` và `live.rs`):
//! vòng đời một cú click, bán kính/độ phồng theo tuổi, warp + vẽ, vùng cần vẽ lại.
//!
//! Khác biệt duy nhất giữa hai hiệu ứng là **cách lấy ảnh màn hình**:
//!
//! * `Effect::new(None, 0)`  — chụp một lần lúc click (`metal.rs`)
//! * `Effect::new(Some(dt), n)` — chụp lại mỗi `dt` giây trong lúc thấu kính sống,
//!   tối đa `n` lần mỗi frame (`live.rs`)
//!
//! Nhờ vậy sửa hình dáng, nhịp warp hay viền sáng ở đây là cả hai hiệu ứng cùng đổi.

use crate::click::warp::{self, WarpImage};
use crate::click::{capture_patch, Bounds, Frame, ScreenPatch};
use std::f64::consts::TAU;
use std::sync::Mutex;

/// Tuổi thọ một thấu kính (giây).
pub const LIFE: f64 = 1.2;
/// Nửa cạnh vùng màn hình chụp quanh điểm click (point). Phải phủ hết bán kính
/// clip của thấu kính: radius tối đa 72 * 1.08 ≈ 78 pt (xem `warp::WarpImage`).
/// Chụp 2x nên vùng 160×160 pt đã là 320×320 px.
const PATCH_HALF: f64 = 80.0;
/// Số thấu kính tối đa cùng lúc (click dồn dập thì cái cũ nhất bị bỏ).
const MAX_HITS: usize = 6;

struct Hit {
    x: f64,
    y: f64,
    age: f64,
    /// Ảnh chụp gần nhất
    patch: Option<ScreenPatch>,
    /// Ảnh warp đã dựng (dùng lại giữa các frame)
    image: Option<WarpImage>,
    /// Tuổi tại lần dựng ảnh gần nhất
    rendered_at: f64,
    /// Tuổi tại lần chụp ảnh gần nhất
    captured_at: f64,
}

/// Một hiệu ứng thấu kính: danh sách cú click đang sống + chính sách chụp ảnh.
pub struct Effect {
    hits: Mutex<Vec<Hit>>,
    /// `None`: chỉ chụp một lần lúc click. `Some(dt)`: chụp lại mỗi `dt` giây.
    refresh: Option<f64>,
    /// Tối đa số lần chụp lại trong một lần `update` (chỉ dùng khi `refresh` bật).
    budget: usize,
}

impl Effect {
    pub const fn new(refresh: Option<f64>, budget: usize) -> Self {
        Self {
            hits: Mutex::new(Vec::new()),
            refresh,
            budget,
        }
    }

    fn hits(&self) -> std::sync::MutexGuard<'_, Vec<Hit>> {
        match self.hits.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Click mới: chụp ảnh và thêm một thấu kính.
    pub fn spawn(&self, x: f64, y: f64) {
        let patch = capture_patch(x, y, PATCH_HALF);
        let mut list = self.hits();
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
            captured_at: 0.0,
        });
    }

    /// Cập nhật tuổi các thấu kính; nếu bật chụp lại thì làm mới ảnh theo nhịp.
    pub fn update(&self, dt: f64) {
        let mut list = self.hits();
        list.retain_mut(|hit| {
            hit.age += dt;
            hit.age < LIFE
        });

        let (Some(refresh), true) = (self.refresh, self.budget > 0) else {
            return;
        };
        let mut budget = self.budget;
        for hit in list.iter_mut() {
            if budget == 0 {
                break;
            }
            if hit.age - hit.captured_at < refresh {
                continue;
            }
            hit.captured_at = hit.age;
            budget -= 1;
            // Không chụp được (chưa cấp quyền) thì giữ ảnh cũ để thấu kính vẫn hiện
            if let Some(patch) = capture_patch(hit.x, hit.y, PATCH_HALF) {
                hit.patch = Some(patch);
                // Ảnh mới thì phải warp lại ngay, không chờ tới `RENDER_INTERVAL`
                hit.image = None;
            }
        }
    }

    /// Xoá mọi thấu kính (khi đổi hiệu ứng hoặc màn hình thay đổi).
    pub fn clear(&self) {
        self.hits().clear();
    }

    /// Có thấu kính nào đang sống không.
    pub fn animating(&self) -> bool {
        !self.hits().is_empty()
    }

    /// Vùng cần làm sạch/vẽ lại (toạ độ Cocoa toàn cục).
    pub fn bounds(&self) -> Option<Bounds> {
        let mut union: Option<Bounds> = None;
        for hit in self.hits().iter() {
            let b = crate::click::square_around(hit.x, hit.y, PATCH_HALF);
            union = Some(match union {
                Some(u) => (u.0.min(b.0), u.1.min(b.1), u.2.max(b.2), u.3.max(b.3)),
                None => b,
            });
        }
        union
    }

    /// Vẽ các thấu kính (bên trong `drawRect`).
    pub fn draw(&self, frame: &Frame) {
        let mut list = self.hits();
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
