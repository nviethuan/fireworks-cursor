#![allow(unexpected_cfgs, deprecated)]

use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSBackingStoreBuffered, NSColor,
    NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize};
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use rand::Rng;
use std::f64::consts::TAU;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Instant;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGRequestPostEventAccess() -> bool;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

fn os_major_version() -> u32 {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|v| v.trim().split('.').next().and_then(|m| m.parse().ok()))
        .unwrap_or(0)
}

/// Tên pane quyền trong System Settings, đổi tên từ macOS 27 (Golden Gate).
fn permission_pane_name(major: u32) -> &'static str {
    if major >= 27 {
        "Device Control and Data Access"
    } else {
        "Accessibility"
    }
}

fn print_permission_hint(accessibility: bool, listen_event: bool) {
    let major = os_major_version();
    let pane = permission_pane_name(major);
    let mark = |ok: bool| if ok { "đã cấp" } else { "CHƯA cấp" };

    // Tap toàn hệ thống bị kiểm tra bởi 2 service TCC khác nhau, thiếu một là im lặng
    eprintln!("Thiếu quyền nên click chưa được theo dõi:");
    eprintln!("  • {pane}: {}", mark(accessibility));
    eprintln!("  • Input Monitoring: {}", mark(listen_event));
    eprintln!("Mở System Settings → Privacy & Security, bật \"Mac Fireworks\" ở cả hai mục {pane} và Input Monitoring.");
    eprintln!("  • Nếu đã bật mà vẫn không ăn: chọn nó, bấm \"-\" để xoá, rồi \"+\" thêm lại /Applications/MacFireworks.app");
    eprintln!("    (mỗi lần build lại, chữ ký ad-hoc đổi nên quyền cũ hết hiệu lực — TCC báo \"Failed to match code requirement\").");
    eprintln!("  • Nếu chưa có trong danh sách: bấm \"+\" → chọn /Applications/MacFireworks.app → bật công tắc.");
    eprintln!("Reset nhanh: tccutil reset Accessibility com.macfireworks.app && tccutil reset ListenEvent com.macfireworks.app");
    eprintln!("Sau khi bật: mở lại app (tap chỉ được tạo lúc khởi động).");

    // macOS 27 đổi tên pane nên deep-link cũ không còn trỏ đúng chỗ.
    let url = if major >= 27 {
        "x-apple.systempreferences:com.apple.preference.security"
    } else {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
    };
    let _ = std::process::Command::new("open").arg(url).spawn();
}

/// Quyền cần cho event tap toàn hệ thống:
///   • Accessibility / "Device Control and Data Access" (post event access)
///   • Input Monitoring (listen event access) — WindowServer kiểm tra khi tạo tap
/// Nếu thiếu, nhờ macOS hiện hộp thoại xin quyền để app xuất hiện trong danh sách.
fn accessibility_granted() -> bool {
    let accessibility = unsafe { AXIsProcessTrusted() };
    let listen_event = unsafe { CGPreflightListenEventAccess() };

    if accessibility && listen_event {
        return true;
    }

    if !accessibility {
        let _ = unsafe { CGRequestPostEventAccess() };
    }
    if !listen_event {
        let _ = unsafe { CGRequestListenEventAccess() };
    }

    print_permission_hint(accessibility, listen_event);
    false
}

struct Particle {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    life: f64,
    decay: f64,
    r: f64,
    g: f64,
    b: f64,
    size: f64,
    gravity: f64,
    twinkle_phase: f64,
    twinkle_speed: f64,
}

struct Scene {
    particles: Vec<Particle>,
    last_frame: Option<Instant>,
}

static SCENE: OnceLock<Mutex<Scene>> = OnceLock::new();

fn get_scene() -> &'static Mutex<Scene> {
    SCENE.get_or_init(|| {
        Mutex::new(Scene {
            particles: Vec::with_capacity(600),
            last_frame: None,
        })
    })
}

fn spawn_firework(x: f64, y: f64) {
    let mut rng = rand::thread_rng();
    let palette = [
        (1.0, 0.25, 0.35), // Đỏ hồng
        (1.0, 0.85, 0.15), // Vàng rực
        (0.15, 0.9, 1.0),  // Xanh cyan
        (0.65, 0.35, 1.0), // Tím neon
        (0.25, 1.0, 0.5),  // Xanh lá
    ];
    let (r, g, b) = palette[rng.gen_range(0..palette.len())];

    let mut scene = match get_scene().lock() {
        Ok(scene) => scene,
        Err(poisoned) => poisoned.into_inner(),
    };

    // Tia sáng loé trắng ở tâm khi pháo hoa nổ
    scene.particles.push(Particle {
        x,
        y,
        vx: 0.0,
        vy: 0.0,
        life: 1.0,
        decay: 0.14,
        r: 1.0,
        g: 1.0,
        b: 1.0,
        size: 9.0,
        gravity: 0.0,
        twinkle_phase: TAU / 4.0, // luôn sáng tối đa
        twinkle_speed: 0.0,
    });

    // Nhiều hạt nhỏ: tán xạ mịn, mềm và "nhuyễn" hơn
    for _ in 0..72 {
        let angle = rng.gen_range(0.0..TAU);
        // sqrt() giúp mật độ hạt dày đều ra mép vòng nổ
        let speed = 1.5 + rng.gen::<f64>().sqrt() * 4.6;
        scene.particles.push(Particle {
            x,
            y,
            vx: angle.cos() * speed,
            vy: angle.sin() * speed,
            life: 1.0,
            decay: 0.009 + rng.gen::<f64>() * 0.008,
            r,
            g,
            b,
            size: 1.2 + rng.gen::<f64>() * 2.0,
            gravity: 0.055 + rng.gen::<f64>() * 0.05,
            twinkle_phase: rng.gen_range(0.0..TAU),
            twinkle_speed: 0.3 + rng.gen::<f64>() * 0.6,
        });
    }
}

unsafe fn fill_oval(cx: f64, cy: f64, radius: f64, r: f64, g: f64, b: f64, alpha: f64) {
    if radius <= 0.2 || alpha <= 0.002 {
        return;
    }
    let color: id = msg_send![
        class!(NSColor),
        colorWithCalibratedRed: r.clamp(0.0, 1.0)
        green: g.clamp(0.0, 1.0)
        blue: b.clamp(0.0, 1.0)
        alpha: alpha.clamp(0.0, 1.0)
    ];
    let _: () = msg_send![color, setFill];

    let rect = NSRect::new(
        NSPoint::new(cx - radius, cy - radius),
        NSSize::new(radius * 2.0, radius * 2.0),
    );
    let path: id = msg_send![class!(NSBezierPath), bezierPathWithOvalInRect: rect];
    let _: () = msg_send![path, fill];
}

extern "C" fn draw_rect(_this: &Object, _sel: Sel, _rect: NSRect) {
    unsafe {
        let mut scene = match get_scene().lock() {
            Ok(scene) => scene,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Bước thời gian thật -> chuyển động mượt, không phụ thuộc tốc độ timer
        let now = Instant::now();
        let dt = scene
            .last_frame
            .map(|last| (now - last).as_secs_f64())
            .unwrap_or(1.0 / 60.0)
            .clamp(0.001, 0.05);
        scene.last_frame = Some(now);

        let f = dt * 60.0;
        let damping = 0.985_f64.powf(f);

        scene.particles.retain_mut(|p| {
            p.x += p.vx * f;
            p.y += p.vy * f;
            p.vy -= p.gravity * f;
            p.vx *= damping;
            p.vy *= damping;
            p.life -= p.decay * f;
            p.twinkle_phase += p.twinkle_speed * f;
            p.life > 0.0
        });

        for p in scene.particles.iter() {
            let fade = p.life.powf(1.35);
            let twinkle = 0.45 + 0.55 * (0.5 + 0.5 * p.twinkle_phase.sin());
            let alpha = (fade * twinkle).clamp(0.0, 1.0);
            let core = p.size * (0.35 + 0.65 * p.life);

            // Quầng sáng neon nhiều lớp: toả mềm ra xung quanh
            fill_oval(p.x, p.y, core * 4.4, p.r, p.g, p.b, alpha * 0.06);
            fill_oval(p.x, p.y, core * 2.7, p.r, p.g, p.b, alpha * 0.12);
            fill_oval(p.x, p.y, core * 1.7, p.r, p.g, p.b, alpha * 0.24);

            // Ruột nóng: pha trắng để hạt trông "phát sáng"
            let hot_r = p.r + (1.0 - p.r) * 0.7;
            let hot_g = p.g + (1.0 - p.g) * 0.7;
            let hot_b = p.b + (1.0 - p.b) * 0.7;
            fill_oval(p.x, p.y, core * 0.85, hot_r, hot_g, hot_b, alpha * 0.95);
        }
    }
}

fn register_view_class() -> *const Class {
    let superclass = class!(NSView);
    let mut decl = ClassDecl::new("FireworkView", superclass).unwrap();
    unsafe {
        decl.add_method(
            sel!(drawRect:),
            draw_rect as extern "C" fn(&Object, Sel, NSRect),
        );
    }
    decl.register()
}

fn main() {
    let trusted = accessibility_granted();

    unsafe {
        let _pool = NSAutoreleasePool::new(nil);
        let app = NSApp();
        app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);

        let main_screen: id = msg_send![class!(NSScreen), mainScreen];
        let screen_frame: NSRect = msg_send![main_screen, frame];
        let screen_height = screen_frame.size.height;

        let window = NSWindow::alloc(nil).initWithContentRect_styleMask_backing_defer_(
            screen_frame,
            NSWindowStyleMask::NSBorderlessWindowMask,
            NSBackingStoreBuffered,
            NO,
        );
        window.setBackgroundColor_(NSColor::clearColor(nil));
        window.setOpaque_(NO);
        window.setIgnoresMouseEvents_(YES);
        window.setLevel_(1000);
        window.setCollectionBehavior_(
            NSWindowCollectionBehavior::NSWindowCollectionBehaviorCanJoinAllSpaces
                | NSWindowCollectionBehavior::NSWindowCollectionBehaviorStationary,
        );

        let view_class = register_view_class();
        let view: id = msg_send![view_class, alloc];
        let view: id = msg_send![view, initWithFrame: screen_frame];
        window.setContentView_(view);
        window.makeKeyAndOrderFront_(nil);

        // Một quả pháo chào ngay khi mở app: thấy được hiệu ứng kể cả khi
        // chưa cấp quyền Trợ năng.
        spawn_firework(screen_frame.size.width / 2.0, screen_height / 2.0);

        if trusted {
        thread::spawn(move || {
            let tap = CGEventTap::new(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::LeftMouseDown],
                move |_proxy, _etype, event| {
                    let loc = event.location();
                    let cocoa_y = screen_height - loc.y;
                    spawn_firework(loc.x, cocoa_y);
                    None
                },
            );

            match tap {
                Ok(tap) => {
                    let loop_source = tap
                        .mach_port
                        .create_runloop_source(0)
                        .expect("Không tạo được RunLoop source");
                    let run_loop = CFRunLoopGetCurrent();
                    CFRunLoopAddSource(
                        run_loop,
                        loop_source.as_concrete_TypeRef(),
                        kCFRunLoopCommonModes,
                    );
                    tap.enable();
                    CFRunLoopRun();
                }
                Err(_) => {
                    eprintln!("Không tạo được CGEventTap: macOS chưa cho phép theo dõi sự kiện.");
                    print_permission_hint(AXIsProcessTrusted(), CGPreflightListenEventAccess());
                }
            }
        });
        } else {
            eprintln!("Đang chạy chế độ chỉ hiển thị pháo hoa, click chưa được theo dõi.");
        }

        let view_ptr = view as usize;
        let timer_block = block::ConcreteBlock::new(move |_timer: id| {
            let v = view_ptr as id;
            let _: () = msg_send![v, setNeedsDisplay: YES];
        })
        .copy();

        let _: id = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.016
            repeats: YES
            block: &*timer_block
        ];

        app.run();
    }
}
