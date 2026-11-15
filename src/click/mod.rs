//! Hiệu ứng khi click. Mỗi hiệu ứng nằm trong một file riêng để dễ thêm/bỏ:
//!
//! * `metal.rs`  — warp ảnh chụp màn hình thành "thấu kính nước" (Core Image)
//! * `warp.rs`   — phần warp ảnh dùng chung (dựng ảnh + cache)
//!
//! Muốn bỏ một hiệu ứng: xoá file tương ứng, xoá `pub mod` ở đây, xoá nhánh
//! trong `spawn()` và mục trong `CLICK_MENU` ở `main.rs`.

pub mod metal;
pub mod warp;

use cocoa::base::{id, nil};
use cocoa::foundation::NSSize;
use core_graphics::display::{CGDisplayBounds, CGMainDisplayID};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::sys::CGImageRef;
use core_graphics::window::{kCGWindowImageBestResolution, kCGWindowListOptionOnScreenBelowWindow};
use objc::runtime::Class;
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::CString;
use std::sync::OnceLock;

extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut std::ffi::c_void;
}

/// Nạp CoreImage (không link tĩnh) rồi lấy class. `class!()` sẽ panic nếu class
/// chưa tồn tại, nên dùng hàm này để kiểm tra an toàn.
pub fn ci_class(name: &str) -> Option<&'static Class> {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| unsafe {
        if let Ok(path) = CString::new("/System/Library/Frameworks/CoreImage.framework/CoreImage") {
            dlopen(path.as_ptr(), 1 /* RTLD_LAZY */);
        }
    });
    Class::get(name)
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCreateImage(
        bounds: CGRect,
        list_option: u32,
        window_id: u32,
        image_option: u32,
    ) -> CGImageRef;
    fn CGImageRelease(image: CGImageRef);
}

/// Vùng cần vẽ lại, toạ độ Cocoa toàn cục: (x0, y0, x1, y1)
pub type Bounds = (f64, f64, f64, f64);

/// Một cửa sổ overlay đang vẽ frame này.
pub struct Frame {
    pub offset_x: f64,
    pub offset_y: f64,
}

/// Ảnh chụp màn hình phía sau một điểm click.
pub struct ScreenPatch {
    /// `NSImage` đã retain (+1)
    pub image: id,
    /// `CIImage` cùng nội dung, đã retain (+1), dùng cho Core Image
    pub ci: id,
    /// Kích thước ảnh trên màn hình, tính bằng point
    pub size: (f64, f64),
    /// Gốc dưới-trái của vùng ảnh, toạ độ Cocoa toàn cục
    pub x: f64,
    pub y: f64,
}

impl Drop for ScreenPatch {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.image, release];
            let _: () = msg_send![self.ci, release];
        }
    }
}

// NSImage/CIImage chỉ được tạo và đọc trên main thread (event tap, timer và
// drawRect đều chạy trên main thread) nên chia sẻ qua Mutex là an toàn.
unsafe impl Send for ScreenPatch {}

/// Chụp vùng màn hình quanh điểm click (không gồm overlay của chính app).
/// Trả về `None` nếu chưa được cấp quyền Screen Recording.
pub fn capture_patch(cx: f64, cy: f64, half: f64) -> Option<ScreenPatch> {
    unsafe {
        let (window_number, frame) = overlay_window_at(cx, cy)?;

        let x0 = (cx - half).max(frame.origin.x);
        let y0 = (cy - half).max(frame.origin.y);
        let x1 = (cx + half).min(frame.origin.x + frame.size.width);
        let y1 = (cy + half).min(frame.origin.y + frame.size.height);
        let w = x1 - x0;
        let h = y1 - y0;
        if w < 40.0 || h < 40.0 {
            return None;
        }

        // CGWindowListCreateImage dùng toạ độ gốc trên-trái của màn hình chính
        let screen_height = CGDisplayBounds(CGMainDisplayID()).size.height;
        let capture = CGRect::new(&CGPoint::new(x0, screen_height - y1), &CGSize::new(w, h));
        let raw = CGWindowListCreateImage(
            capture,
            kCGWindowListOptionOnScreenBelowWindow,
            window_number,
            kCGWindowImageBestResolution,
        );
        if raw.is_null() {
            return None;
        }

        let Some(ci_class) = ci_class("CIImage") else {
            CGImageRelease(raw);
            return None;
        };
        // imageWithCGImage: trả object autorelease -> phải retain để giữ tới lúc Drop
        let ci: id = msg_send![ci_class, imageWithCGImage: raw];
        let ci: id = msg_send![ci, retain];
        let image: id = msg_send![class!(NSImage), alloc];
        let image: id = msg_send![image, initWithCGImage: raw size: NSSize::new(w, h)];
        CGImageRelease(raw);
        if image == nil || ci == nil {
            if image != nil {
                let _: () = msg_send![image, release];
            }
            return None;
        }

        Some(ScreenPatch {
            image,
            ci,
            size: (w, h),
            x: x0,
            y: y0,
        })
    }
}

/// (window number, frame) của cửa sổ overlay chứa điểm (toạ độ Cocoa).
fn overlay_window_at(x: f64, y: f64) -> Option<(u32, CGRect)> {
    crate::overlay_windows()?
        .iter()
        .find(|(_, frame)| {
            x >= frame.origin.x
                && x <= frame.origin.x + frame.size.width
                && y >= frame.origin.y
                && y <= frame.origin.y + frame.size.height
        })
        .copied()
}

/// Gọi khi click, theo hiệu ứng đang chọn (trừ pháo hoa và Off).
pub fn spawn(effect: u8, x: f64, y: f64) {
    if effect == crate::CLICK_METAL {
        metal::spawn(x, y);
    }
}

/// Cập nhật mọi hiệu ứng (main thread, mỗi frame một lần).
pub fn update(dt: f64) {
    metal::update(dt);
}

/// Vẽ phần Core Graphics của các hiệu ứng (bên trong `drawRect`).
pub fn draw(frame: &Frame) {
    metal::draw(frame);
}

/// Vùng cần làm sạch/vẽ lại của tất cả hiệu ứng (toạ độ Cocoa toàn cục).
pub fn bounds() -> Option<Bounds> {
    let mut union: Option<Bounds> = None;
    let mut add = |b: Option<Bounds>| {
        if let Some(b) = b {
            union = Some(match union {
                Some(u) => (u.0.min(b.0), u.1.min(b.1), u.2.max(b.2), u.3.max(b.3)),
                None => b,
            });
        }
    };
    add(metal::bounds());
    union
}

/// Xoá sạch mọi thứ (khi đổi hiệu ứng hoặc tắt).
pub fn clear() {
    metal::clear();
}

/// Hình chữ nhật (Cocoa) của một điểm ± bán kính.
pub fn square_around(x: f64, y: f64, radius: f64) -> Bounds {
    (x - radius, y - radius, x + radius, y + radius)
}
