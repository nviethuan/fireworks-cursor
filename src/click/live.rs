//! Hiệu ứng "live warp": giống thấu kính của `metal.rs` nhưng nội dung bên trong
//! lấy từ **frame sống** của ScreenCaptureKit thay vì một ảnh chụp tĩnh lúc click.
//!
//! Nhờ vậy video đang phát vẫn động trong thấu kính, và khi click nút close thì
//! bóng cửa sổ biến mất ngay ở frame kế tiếp (bản `metal.rs` giữ ảnh chụp cũ suốt
//! `LIFE`, nên còn "bóng ma" của cửa sổ đã đóng).
//!
//! Stream chỉ chạy khi có thấu kính đang sống và tự dừng sau [`IDLE_STOP`] giây
//! không dùng. Click đầu tiên có thể trễ ~50 ms (chờ frame đầu) và nếu máy không
//! có ScreenCaptureKit (macOS < 12.3) thì hiệu ứng không vẽ gì.

use cocoa::base::{id, nil, NO, YES};
use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect, CGSize};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Protocol, Sel};
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::{c_char, c_void, CString};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::click::warp::{self, WarpImage};
use crate::click::{ci_class, Bounds, Frame, ScreenPatch};

/// Dừng stream sau chừng này giây không chụp (để không tốn CPU khi không click).
pub const IDLE_STOP: Duration = Duration::from_secs(2);
/// Lúc đang có hiệu ứng chỉ cần ~30 fps.
const STREAM_FPS: i32 = 20;
/// Lỗi khởi động liên tiếp quá số lần này thì thôi, dùng đường dự phòng.
const MAX_START_FAILURES: u32 = 5;

const SC_STREAM_OUTPUT_TYPE_SCREEN: i64 = 0;
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241; // 'BGRA'

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetImageBuffer(buffer: id) -> id;
}
extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> id;
    fn dispatch_release(object: id);
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

/// `CMTime` của CoreMedia (dùng cho `minimumFrameInterval`).
#[repr(C)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CM_TIME_FLAGS_VALID: u32 = 1;

/// Frame mới nhất của stream: `CIImage` (+1) của cả màn hình, đơn vị pixel.
#[derive(Clone, Copy)]
struct Latest {
    ci: usize,
}

struct State {
    /// `SCStream` (+1) đang chạy; `None` khi chưa dùng hoặc đã dừng
    stream: Option<usize>,
    /// Object nhận sample (+1), phải giữ cùng stream
    handler: Option<usize>,
    /// Màn hình stream đang bám: id, khung (toạ độ Cocoa), tỉ lệ pixel/point
    display_id: u32,
    frame: CGRect,
    scale: f64,
    latest: Option<Latest>,
    last_used: Option<Instant>,
    /// Đang khởi động stream ở thread nền
    starting: bool,
    /// Stream đang chạy capture
    capturing: bool,
    failures: u32,
}

const NO_FRAME: CGRect = CGRect {
    origin: CGPoint { x: 0.0, y: 0.0 },
    size: CGSize {
        width: 0.0,
        height: 0.0,
    },
};

static STATE: Mutex<State> = Mutex::new(State {
    stream: None,
    handler: None,
    display_id: 0,
    frame: NO_FRAME,
    scale: 1.0,
    latest: None,
    last_used: None,
    starting: false,
    capturing: false,
    failures: 0,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    match STATE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Nạp ScreenCaptureKit rồi lấy class (dlopen để còn chạy được trên macOS cũ).
fn sck_class(name: &str) -> Option<&'static Class> {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| unsafe {
        if let Ok(path) = CString::new(
            "/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit",
        ) {
            dlopen(path.as_ptr(), 1 /* RTLD_LAZY */);
        }
    });
    Class::get(name)
}

/// ScreenCaptureKit có dùng được không (macOS 12.3+).
pub fn available() -> bool {
    sck_class("SCStream").is_some() && sck_class("SCShareableContent").is_some()
}

/// Class nhận sample: `SCStreamOutput`.
fn handler_class() -> Option<&'static Class> {
    static CLASS: OnceLock<Option<&'static Class>> = OnceLock::new();
    *CLASS.get_or_init(|| unsafe {
        let superclass = Class::get("NSObject")?;
        let mut decl = ClassDecl::new("MacFireworksLiveOutput", superclass)?;
        if let Some(protocol) = Protocol::get("SCStreamOutput") {
            decl.add_protocol(protocol);
        }
        decl.add_method(
            sel!(stream:didOutputSampleBuffer:ofType:),
            did_output as extern "C" fn(&Object, Sel, id, id, i64),
        );
        Some(decl.register())
    })
}

/// Chạy trên queue của stream: giữ lại frame màn hình mới nhất.
extern "C" fn did_output(_this: &Object, _cmd: Sel, _stream: id, sample: id, kind: i64) {
    if kind != SC_STREAM_OUTPUT_TYPE_SCREEN {
        return;
    }
    unsafe {
        // Queue của stream không có autorelease pool: tạo/drain cho mỗi sample
        let pool: id = msg_send![class!(NSAutoreleasePool), new];
        let buffer: id = CMSampleBufferGetImageBuffer(sample);
        if buffer == nil {
            let _: () = msg_send![pool, drain];
            return;
        }
        let Some(ci_class) = ci_class("CIImage") else {
            let _: () = msg_send![pool, drain];
            return;
        };
        let image: id = msg_send![ci_class, imageWithCVPixelBuffer: buffer];
        if image == nil {
            let _: () = msg_send![pool, drain];
            return;
        }
        // imageWithCVPixelBuffer: trả object autorelease -> retain để giữ frame
        let image: id = msg_send![image, retain];
        let latest = Latest {
            ci: image as usize,
        };
        let old = {
            let mut st = state();
            st.latest.replace(latest).map(|old| old.ci)
        };
        if let Some(old) = old {
            let _: () = msg_send![old as id, release];
        }
        let _: () = msg_send![pool, drain];
    }
}

/// `SCDisplay` (+1) của màn hình chứa toạ độ top-left toàn cục (gx, gy).
/// Cửa sổ overlay của chính app: phải loại khỏi bộ lọc, nếu không frame sau sẽ
/// chụp lại thấu kính vừa vẽ (ảnh tự lặp vào trong chính nó).
fn overlay_windows_to_exclude(content: id) -> id {
    unsafe {
        let exclude: id = msg_send![class!(NSMutableArray), array];
        let overlays = crate::overlay_windows();
        if overlays.is_empty() {
            return msg_send![exclude, retain];
        }
        let windows: id = msg_send![content, windows];
        if windows == nil {
            return msg_send![exclude, retain];
        }
        let count: usize = msg_send![windows, count];
        for index in 0..count {
            let window: id = msg_send![windows, objectAtIndex: index];
            // `SCWindow` dùng `windowID` (CGWindowID), không có `windowNumber`
            let number: u32 = msg_send![window, windowID];
            if overlays.iter().any(|(n, _)| *n == number) {
                let _: () = msg_send![exclude, addObject: window];
            }
        }
        msg_send![exclude, retain]
    }
}

fn shareable_display(wanted: u32) -> Option<(id, id)> {
    let sck = sck_class("SCShareableContent")?;
    let (tx, rx) = std::sync::mpsc::channel::<Option<(id, id)>>();
    let on_content = move |content: id, error: id| {
        let picked = unsafe {
            if content == nil || error != nil {
                None
            } else {
                let exclude = overlay_windows_to_exclude(content);
                let displays: id = msg_send![content, displays];
                let count: usize = msg_send![displays, count];
                let mut first: Option<id> = None;
                let mut hit: Option<id> = None;
                for index in 0..count {
                    let display: id = msg_send![displays, objectAtIndex: index];
                    let display_id: u32 = msg_send![display, displayID];
                    if display_id == wanted {
                        hit = Some(display);
                        break;
                    }
                    if first.is_none() {
                        first = Some(display);
                    }
                }
                match hit.or(first) {
                    Some(display) => {
                        let kept: id = msg_send![display, retain];
                        Some((kept, exclude))
                    }
                    None => {
                        let _: () = msg_send![exclude, release];
                        None
                    }
                }
            }
        };
        let _ = tx.send(picked);
    };
    let block = block::ConcreteBlock::new(on_content).copy();
    unsafe {
        let _: () = msg_send![
            sck,
            getShareableContentExcludingDesktopWindows: NO
            onScreenWindowsOnly: YES
            completionHandler: &*block
        ];
    }
    rx.recv_timeout(Duration::from_secs(3)).ok().flatten()
}

/// Số hiệu màn hình (`NSScreenNumber` -> CGDirectDisplayID) của một `NSScreen`.
fn screen_number(screen: id) -> Option<u32> {
    unsafe {
        let description: id = msg_send![screen, deviceDescription];
        if description == nil {
            return None;
        }
        let number: id = msg_send![description, objectForKey: crate::nsstring("NSScreenNumber")];
        if number == nil {
            return None;
        }
        Some(msg_send![number, unsignedIntValue])
    }
}

/// (display id, khung Cocoa, tỉ lệ pixel/point) của màn hình chứa điểm (x, y).
fn screen_for_point(x: f64, y: f64) -> Option<(u32, CGRect, f64)> {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens == nil {
            return None;
        }
        let count: usize = msg_send![screens, count];
        for index in 0..count {
            let screen: id = msg_send![screens, objectAtIndex: index];
            let frame: CGRect = msg_send![screen, frame];
            let inside = x >= frame.origin.x
                && x <= frame.origin.x + frame.size.width
                && y >= frame.origin.y
                && y <= frame.origin.y + frame.size.height;
            if !inside {
                continue;
            }
            let Some(display_id) = screen_number(screen) else {
                continue;
            };
            let backing: f64 = msg_send![screen, backingScaleFactor];
            let scale = if backing >= 1.0 { backing } else { 1.0 };
            return Some((display_id, frame, scale));
        }
    }
    None
}

/// Giải phóng hẳn stream (dùng khi phải chuyển sang màn hình khác).
fn destroy_stream(st: &mut State) {
    if let Some(frame) = st.latest.take() {
        unsafe {
            let _: id = msg_send![frame.ci as id, release];
        }
    }
    st.capturing = false;
    if let Some(stream) = st.stream.take() {
        let noop = block::ConcreteBlock::new(|_error: id| {}).copy();
        unsafe {
            let _: () = msg_send![stream as id, stopCaptureWithCompletionHandler: &*noop];
            let _: () = msg_send![stream as id, release];
        }
    }
    if let Some(handler) = st.handler.take() {
        unsafe {
            let _: id = msg_send![handler as id, release];
        }
    }
    st.display_id = 0;
    st.frame = NO_FRAME;
    st.scale = 1.0;
}

/// Bỏ hết hiệu ứng và stream hiện tại (màn hình vừa thay đổi: cách sắp xếp,
/// độ phân giải, hoặc máy vừa thức dậy).
pub fn reset() {
    hits().clear();
    let mut st = state();
    destroy_stream(&mut st);
    st.starting = false;
    st.failures = 0;
}

/// Khởi động stream cho màn hình `display` (+1) nếu chưa có.
fn start_stream(
    st: &mut State,
    display: id,
    display_id: u32,
    display_frame: CGRect,
    pixels: (f64, f64),
    exclude: id,
) -> bool {
    let (Some(stream_class), Some(filter_class), Some(config_class), Some(handler_class)) = (
        sck_class("SCStream"),
        sck_class("SCContentFilter"),
        sck_class("SCStreamConfiguration"),
        handler_class(),
    ) else {
        unsafe {
            let _: () = msg_send![exclude as id, release];
        }
        return false;
    };

    unsafe {
        let filter: id = msg_send![filter_class, alloc];
        let filter: id = msg_send![filter, initWithDisplay: display excludingWindows: exclude];
        let _: () = msg_send![exclude as id, release];
        if filter == nil {
            return false;
        }
        let config: id = msg_send![config_class, alloc];
        let config: id = msg_send![config, init];
        if config == nil {
            let _: () = msg_send![filter, release];
            return false;
        }
        let _: () = msg_send![config, setWidth: pixels.0 as usize];
        let _: () = msg_send![config, setHeight: pixels.1 as usize];
        let _: () = msg_send![config, setShowsCursor: NO];
        let _: () = msg_send![config, setPixelFormat: PIXEL_FORMAT_BGRA];
        let _: () = msg_send![config, setQueueDepth: 3];
        let interval = CMTime {
            value: 1,
            timescale: STREAM_FPS,
            flags: CM_TIME_FLAGS_VALID,
            epoch: 0,
        };
        let _: () = msg_send![config, setMinimumFrameInterval: interval];

        let handler: id = msg_send![handler_class, new];
        let stream: id = msg_send![stream_class, alloc];
        let stream: id =
            msg_send![stream, initWithFilter: filter configuration: config delegate: handler];
        let _: () = msg_send![filter, release];
        let _: () = msg_send![config, release];
        if stream == nil || handler == nil {
            if stream != nil {
                let _: () = msg_send![stream, release];
            }
            if handler != nil {
                let _: () = msg_send![handler, release];
            }
            return false;
        }

        let label = CString::new("com.macfireworks.capture").unwrap_or_default();
        let queue = dispatch_queue_create(label.as_ptr(), std::ptr::null());
        if queue == nil {
            let _: () = msg_send![stream, release];
            let _: () = msg_send![handler, release];
            return false;
        }
        let mut error: id = nil;
        let added: bool = msg_send![
            stream,
            addStreamOutput: handler
            type: SC_STREAM_OUTPUT_TYPE_SCREEN as i64
            sampleHandlerQueue: queue
            error: &mut error
        ];
        // SCStream giữ queue giúp ta, nên trả lại +1 của lần create
        dispatch_release(queue);
        if !added {
            let _: () = msg_send![stream, release];
            let _: () = msg_send![handler, release];
            return false;
        }

        let noop = block::ConcreteBlock::new(|_error: id| {}).copy();
        let _: () = msg_send![stream, startCaptureWithCompletionHandler: &*noop];

        st.stream = Some(stream as usize);
        st.handler = Some(handler as usize);
        st.display_id = display_id;
        st.frame = display_frame;
        st.scale = if display_frame.size.width > 0.0 {
            pixels.0 / display_frame.size.width
        } else {
            1.0
        };
        st.latest = None;
        st.capturing = true;
        true
    }
}

/// Tạm dừng capture nhưng **giữ** `SCStream`: lần sau chỉ cần start lại, không
/// phải tạo stream mới (tạo/huỷ liên tục làm RSS leo và để lại thread bận).
fn pause_capture(st: &mut State) {
    if let Some(frame) = st.latest.take() {
        unsafe {
            let _: id = msg_send![frame.ci as id, release];
        }
    }
    if let Some(stream) = st.stream {
        if st.capturing {
            let noop = block::ConcreteBlock::new(|_error: id| {}).copy();
            unsafe {
                let _: () = msg_send![stream as id, stopCaptureWithCompletionHandler: &*noop];
            }
            st.capturing = false;
        }
    }
}


/// Tuổi thọ một thấu kính (giây), giống `metal.rs` để hai hiệu ứng nhìn như nhau.
const LIFE: f64 = 1.2;
/// Nửa cạnh vùng lấy từ frame sống quanh điểm click (point).
const PATCH_HALF: f64 = 80.0;

struct Hit {
    x: f64,
    y: f64,
    age: f64,
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

/// Bán kính thấu kính và độ phồng theo tuổi (giống `metal::lens`).
fn lens(age: f64) -> (f64, f64) {
    let t = (age / LIFE).clamp(0.0, 1.0);
    let radius = 20.0 + 52.0 * (1.0 - (1.0 - t).powi(2));
    let wave = (t * std::f64::consts::TAU * 1.3).sin();
    let scale = 0.55 * wave * (1.0 - t).powf(1.1);
    (radius, scale)
}

/// Click mới: thêm thấu kính và bật stream nếu chưa chạy.
pub fn spawn(x: f64, y: f64) {
    if !available() {
        return; // macOS cũ không có ScreenCaptureKit: hiệu ứng này không chạy
    }
    {
        let mut list = hits();
        while list.len() >= 6 {
            list.remove(0);
        }
        list.push(Hit {
            x,
            y,
            age: 0.0,
            image: None,
            rendered_at: f64::NEG_INFINITY,
        });
    }
    let Some((display_id, frame, scale)) = screen_for_point(x, y) else {
        return; // không xác định được màn hình chứa điểm click
    };
    let mut st = state();
    st.last_used = Some(Instant::now());
    // Một `SCStream` chỉ bám một màn hình: click sang màn hình khác (hoặc màn hình
    // vừa đổi kích thước) thì phải bỏ stream cũ rồi dựng stream cho màn hình mới.
    if st.stream.is_some()
        && (st.display_id != display_id
            || st.frame.size.width != frame.size.width
            || st.frame.size.height != frame.size.height)
    {
        destroy_stream(&mut st);
    }
    if let Some(stream) = st.stream {
        if !st.capturing {
            st.capturing = true;
            let noop = block::ConcreteBlock::new(|_error: id| {}).copy();
            unsafe {
                let _: () = msg_send![stream as id, startCaptureWithCompletionHandler: &*noop];
            }
        }
        return;
    }
    if !st.starting && st.failures < MAX_START_FAILURES {
        st.starting = true;
        st.display_id = display_id;
        st.frame = frame;
        st.scale = scale;
        let _ = std::thread::spawn(move || start_in_background(display_id, frame, scale));
    }
}

/// Cập nhật tuổi các thấu kính; dừng stream khi không còn dùng.
pub fn update(dt: f64) {
    {
        let mut list = hits();
        list.retain_mut(|hit| {
            hit.age += dt;
            hit.age < LIFE
        });
    }
    maintain();
}

/// Vùng cần vẽ lại (toạ độ Cocoa toàn cục).
pub fn bounds() -> Option<Bounds> {
    let mut union: Option<Bounds> = None;
    for hit in hits().iter() {
        let b = crate::click::square_around(hit.x, hit.y, PATCH_HALF);
        union = Some(match union {
            Some(u) => (u.0.min(b.0), u.1.min(b.1), u.2.max(b.2), u.3.max(b.3)),
            None => b,
        });
    }
    union
}

/// Xoá mọi thấu kính (khi đổi hiệu ứng).
pub fn clear() {
    hits().clear();
}

/// Vẽ các thấu kính bằng frame sống mới nhất.
pub fn draw(frame: &Frame) {
    let mut list = hits();
    for hit in list.iter_mut() {
        let (radius, scale) = lens(hit.age);
        if scale.abs() < 0.004 || radius < 2.0 {
            continue;
        }
        let due = hit.image.is_none() || hit.age - hit.rendered_at >= warp::RENDER_INTERVAL;
        if due {
            if let Some(patch) = live_patch(hit.x, hit.y) {
                hit.image = warp::render(&patch, hit.x, hit.y, radius, scale);
                hit.rendered_at = hit.age;
            }
        }
        if let Some(image) = hit.image.as_ref() {
            if let Some(patch) = live_patch(hit.x, hit.y) {
                image.draw(&patch, hit.x, hit.y, radius, frame);
            }
        }

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

/// Cắt vùng quanh điểm click từ frame sống mới nhất thành `ScreenPatch` tạm.
fn live_patch(x: f64, y: f64) -> Option<ScreenPatch> {
    let (latest, display_frame, scale) = {
        let st = state();
        (st.latest?, st.frame, st.scale)
    };
    unsafe {
        let x0 = (x - PATCH_HALF).max(display_frame.origin.x);
        let y0 = (y - PATCH_HALF).max(display_frame.origin.y);
        let x1 = (x + PATCH_HALF).min(display_frame.origin.x + display_frame.size.width);
        let y1 = (y + PATCH_HALF).min(display_frame.origin.y + display_frame.size.height);
        let (w, h) = (x1 - x0, y1 - y0);
        if w < 40.0 || h < 40.0 {
            return None;
        }
        // Khung Cocoa (gốc dưới-trái, y hướng lên) -> pixel của frame (gốc dưới-trái).
        // Frame của ScreenCaptureKit đã đúng chiều, nên cạnh dưới của vùng cắt nằm
        // cách đáy màn hình đúng (y0 - frame.origin.y) point.
        let px = (x0 - display_frame.origin.x) * scale;
        let py = (y0 - display_frame.origin.y) * scale;
        let crop = CGRect::new(
            &CGPoint::new(px, py),
            &CGSize::new(w * scale, h * scale),
        );
        let _: id = msg_send![latest.ci as id, retain];
        let cropped: id = msg_send![latest.ci as id, imageByCroppingToRect: crop];
        let _: id = msg_send![latest.ci as id, release];
        if cropped == nil {
            return None;
        }
        // Dời gốc ảnh về (0,0) để `warp` tính pixel/point như ảnh chụp trực tiếp
        let transform = CGAffineTransform::new(1.0, 0.0, 0.0, 1.0, -crop.origin.x, -crop.origin.y);
        let moved: id = msg_send![cropped, imageByApplyingTransform: transform];
        if moved == nil {
            return None;
        }
        let ci: id = msg_send![moved, retain];
        Some(ScreenPatch {
            ci,
            size: (w, h),
            x: x0,
            y: y0,
        })
    }
}

/// Khởi động stream ở thread nền.
fn start_in_background(display_id: u32, display_frame: CGRect, scale: f64) {
    let pool: id = unsafe { msg_send![class!(NSAutoreleasePool), new] };
    let picked = shareable_display(display_id);
    let mut st = state();
    st.starting = false;
    st.last_used = Some(Instant::now());
    let Some((display, exclude)) = picked else {
        st.failures += 1;
        drop(st);
        unsafe {
            let _: () = msg_send![pool, drain];
        }
        return;
    };
    // Frame ở độ phân giải thật (Retina 2x) để thấu kính đủ chi tiết
    let pixels = (
        display_frame.size.width * scale,
        display_frame.size.height * scale,
    );
    let started = start_stream(&mut st, display, display_id, display_frame, pixels, exclude);
    unsafe {
        let _: id = msg_send![display, release];
    }
    if !started {
        st.failures += 1;
    }
    drop(st);
    unsafe {
        let _: () = msg_send![pool, drain];
    }
}

/// Dừng stream khi không còn thấu kính nào và đã quá [`IDLE_STOP`].
fn maintain() {
    let mut st = state();
    if st.stream.is_none() {
        return;
    }
    let busy = !hits().is_empty();
    if busy {
        st.last_used = Some(Instant::now());
        return;
    }
    let idle = st
        .last_used
        .map(|last| last.elapsed() >= IDLE_STOP)
        .unwrap_or(true);
    if idle {
        pause_capture(&mut st);
    }
}
