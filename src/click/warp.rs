//! Warp ảnh chụp màn hình bằng Core Image (`CIBumpDistortion`) rồi vẽ lại thành
//! một "thấu kính nước": bán kính `radius`, độ phồng `scale` (dương = lồi, âm =
//! lõm).
//!
//! Chi phí: đo trên máy này mỗi lần dựng ảnh tốn ~11 ms, trong đó ~8 ms là tạo
//! `CIContext` (không cache được — Core Image ném exception ở lần `createCGImage`
//! thứ hai nếu dùng lại một context) và ~3 ms là `createCGImage` (không phụ thuộc
//! kích thước vùng ảnh). Vì vậy cách duy nhất để giảm CPU là **giảm số lần dựng**:
//! hiệu ứng gọi `render()` theo `RENDER_INTERVAL` (18 fps) rồi vẽ lại ảnh đã dựng
//! (`WarpImage::draw`, một lần `CGContextDrawImage`) ở mọi frame — hình vẫn mượt vì
//! bán kính/độ phồng đổi rất chậm.

use crate::click::{Frame, ScreenPatch};
use cocoa::base::{id, nil, YES};
use cocoa::foundation::NSPoint;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::sys::{CGContextRef, CGImageRef};
use objc::{class, msg_send, sel, sel_impl};

#[link(name = "CoreImage", kind = "framework")]
extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGContextDrawImage(context: CGContextRef, rect: CGRect, image: CGImageRef);
    fn CGImageRelease(image: CGImageRef);
}

/// Khoảng thời gian tối thiểu giữa hai lần dựng ảnh warp (giây): 18 fps.
///
/// Đo với 8 click dồn dập (3 click/giây, mỗi hit sống 1,2 s trên máy 2 màn hình):
/// 24 fps ~66%, 18 fps ~55%, 15 fps ~50%, không dựng lại (chỉ vẽ ảnh cũ) ~21%.
/// 18 fps là mức cắt được ~1/6 CPU mà mắt vẫn thấy liền mạch.
pub const RENDER_INTERVAL: f64 = 1.0 / 18.0;

fn ci_context() -> Option<id> {
    unsafe {
        let context_class = crate::click::ci_class("CIContext")?;
        let yes: id = msg_send![class!(NSNumber), numberWithBool: YES];
        let key: id = crate::nsstring("kCIContextUseSoftwareRenderer");
        let options: id = msg_send![class!(NSDictionary), dictionaryWithObject: yes forKey: key];
        let context: id = msg_send![context_class, contextWithOptions: options];
        if context == nil {
            None
        } else {
            Some(context)
        }
    }
}

/// Ảnh warp đã dựng, dùng lại được nhiều frame. Giải phóng khi Drop.
pub struct WarpImage {
    cg: CGImageRef,
    /// Vùng ảnh (point, so với gốc `ScreenPatch`) đã dựng
    dst: (f64, f64, f64, f64),
}

// CGImage chỉ được tạo/đọc trên main thread (drawRect, timer đều chạy ở đó) nên
// chia sẻ qua Mutex là an toàn.
unsafe impl Send for WarpImage {}

impl Drop for WarpImage {
    fn drop(&mut self) {
        unsafe {
            CGImageRelease(self.cg);
        }
    }
}

impl WarpImage {
    /// Wrapper (giữ ảnh `image`) — giúp chỗ gọi không phải tự quản lý con trỏ.
    pub fn new(image: CGImageRef, dst: (f64, f64, f64, f64)) -> Self {
        WarpImage { cg: image, dst }
    }

    /// Vẽ ảnh đã warp vào `drawRect`, clip trong vòng tròn `radius * 1.08` (không
    /// vượt quá vùng ảnh đã chụp).
    pub fn draw(&self, patch: &ScreenPatch, x: f64, y: f64, radius: f64, frame: &Frame) {
        let cx = x - frame.offset_x;
        let cy = y - frame.offset_y;
        let Some(cg_context) = crate::current_cg_context_ref() else {
            return;
        };
        unsafe {
            let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
            // Không clip quá vùng ảnh đã chụp: sát mép màn hình ảnh bị cắt ngắn,
            // clip lớn hơn sẽ lộ vòng tròn trống ở rìa.
            let available = patch.size.0.min(patch.size.1) * 0.5;
            let clip_radius = (radius * 1.08).min(available);
            let clip = cocoa::foundation::NSRect::new(
                NSPoint::new(cx - clip_radius, cy - clip_radius),
                cocoa::foundation::NSSize::new(clip_radius * 2.0, clip_radius * 2.0),
            );
            let path: id = msg_send![class!(NSBezierPath), bezierPathWithOvalInRect: clip];
            let _: () = msg_send![path, addClip];

            let (dx, dy, dw, dh) = self.dst;
            let dst = CGRect::new(
                &CGPoint::new(
                    patch.x + dx - frame.offset_x,
                    patch.y + dy - frame.offset_y,
                ),
                &CGSize::new(dw, dh),
            );
            CGContextDrawImage(cg_context, dst, self.cg);
            let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
        }
    }
}

/// Dựng ảnh warp một lần (gọi theo `RENDER_INTERVAL`, không phải mỗi frame).
pub fn render(patch: &ScreenPatch, x: f64, y: f64, radius: f64, scale: f64) -> Option<WarpImage> {
    if scale.abs() < 0.004 || radius < 2.0 {
        return None;
    }

    let context = ci_context()?;
    let (warpped, crop, to_pixels) = build_warpped(patch, x, y, radius, scale)?;

    unsafe {
        let crop_rect = CGRect::new(&CGPoint::new(crop.0, crop.1), &CGSize::new(crop.2, crop.3));
        let cg: CGImageRef = msg_send![context, createCGImage: warpped fromRect: crop_rect];
        if cg.is_null() {
            return None;
        }
        Some(WarpImage::new(
            cg,
            (
                crop.0 / to_pixels,
                crop.1 / to_pixels,
                crop.2 / to_pixels,
                crop.3 / to_pixels,
            ),
        ))
    }
}

/// Áp `CIBumpDistortion` và crop về vùng thấu kính.
/// Trả về (ảnh đã crop, vùng crop trong pixel, tỉ lệ pixel/point).
fn build_warpped(
    patch: &ScreenPatch,
    x: f64,
    y: f64,
    radius: f64,
    scale: f64,
) -> Option<(id, (f64, f64, f64, f64), f64)> {
    let (vector_class, filter_class) = (
        crate::click::ci_class("CIVector")?,
        crate::click::ci_class("CIFilter")?,
    );

    unsafe {
        let extent: CGRect = msg_send![patch.ci, extent];
        if extent.size.width < 1.0 || extent.size.height < 1.0 {
            return None;
        }
        // Pixel ảnh so với point trên màn hình (ảnh Retina = 2x)
        let to_pixels = extent.size.width / patch.size.0.max(1.0);

        // Chỉ dựng đúng hình vuông bao quanh vòng clip thay vì cả patch: rẻ hơn ở
        // các frame đầu khi thấu kính còn nhỏ.
        let clip_radius = (radius * 1.08).min(patch.size.0.min(patch.size.1) * 0.5);
        if clip_radius < 2.0 {
            return None;
        }
        let cx_px = (x - patch.x) * to_pixels;
        let cy_px = (y - patch.y) * to_pixels;
        let half_px = clip_radius * to_pixels;
        let crop = intersect(
            (cx_px - half_px, cy_px - half_px, half_px * 2.0, half_px * 2.0),
            (
                extent.origin.x,
                extent.origin.y,
                extent.size.width,
                extent.size.height,
            ),
        )?;

        let center: id = msg_send![
            vector_class,
            vectorWithX: cx_px
            Y: cy_px
        ];
        let filter: id = msg_send![filter_class, filterWithName: crate::nsstring("CIBumpDistortion")];
        if filter == nil {
            return None;
        }
        let _: () = msg_send![filter, setValue: patch.ci forKey: crate::nsstring("inputImage")];
        let _: () = msg_send![filter, setValue: center forKey: crate::nsstring("inputCenter")];
        let radius_number: id = msg_send![
            class!(NSNumber),
            numberWithDouble: (radius * to_pixels).clamp(1.0, 400.0)
        ];
        let _: () = msg_send![filter, setValue: radius_number forKey: crate::nsstring("inputRadius")];
        let scale_number: id = msg_send![class!(NSNumber), numberWithDouble: scale];
        let _: () = msg_send![filter, setValue: scale_number forKey: crate::nsstring("inputScale")];

        let output: id = msg_send![filter, outputImage];
        if output == nil {
            return None;
        }
        let crop_rect = CGRect::new(&CGPoint::new(crop.0, crop.1), &CGSize::new(crop.2, crop.3));
        let cropped: id = msg_send![output, imageByCroppingToRect: crop_rect];
        Some((cropped, crop, to_pixels))
    }
}

/// Giao của hai hình chữ nhật (x, y, w, h). `None` nếu không chồng nhau.
fn intersect(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> Option<(f64, f64, f64, f64)> {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = (a.0 + a.2).min(b.0 + b.2);
    let y1 = (a.1 + a.3).min(b.1 + b.3);
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        None
    } else {
        Some((x0, y0, x1 - x0, y1 - y0))
    }
}
