#![allow(unexpected_cfgs, deprecated)]

use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSBackingStoreBuffered, NSColor,
    NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize, NSString};
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun};
use core_graphics::display::{CGDisplayBounds, CGMainDisplayID};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use rand::Rng;
use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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

// ---------- Âm thanh khi click ----------

static SOUND_ENABLED: AtomicBool = AtomicBool::new(true);
static SOUND_PENDING: AtomicBool = AtomicBool::new(false);
static SOUND_POOL: OnceLock<Vec<usize>> = OnceLock::new();
static SOUND_NEXT: AtomicUsize = AtomicUsize::new(0);
static SOUND_VOLUME_BITS: AtomicU32 = AtomicU32::new(0.45f32.to_bits());

// ---------- Menu bar + panel settings ----------

static SETTINGS_WINDOW: OnceLock<usize> = OnceLock::new();
static STATUS_ITEM: OnceLock<usize> = OnceLock::new();
static VOLUME_LABEL: OnceLock<usize> = OnceLock::new();

fn nsstring(s: &str) -> id {
    unsafe { NSString::alloc(nil).init_str(s) }
}

fn sound_enabled() -> bool {
    SOUND_ENABLED.load(Ordering::Relaxed)
}

fn sound_volume() -> f32 {
    f32::from_bits(SOUND_VOLUME_BITS.load(Ordering::Relaxed))
}

fn set_sound_volume(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    SOUND_VOLUME_BITS.store(value.to_bits(), Ordering::Relaxed);
    value
}

fn load_sound_preference() {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("SoundEnabled");
        let stored: id = msg_send![defaults, objectForKey: key];
        if stored != nil {
            let enabled: bool = msg_send![defaults, boolForKey: key];
            SOUND_ENABLED.store(enabled, Ordering::Relaxed);
        }

        let volume_key = nsstring("SoundVolume");
        let stored_volume: id = msg_send![defaults, objectForKey: volume_key];
        if stored_volume != nil {
            let volume: f32 = msg_send![defaults, floatForKey: volume_key];
            set_sound_volume(volume);
        }
    }
}

// ---------- CoreAudio: thiết bị ra mặc định ----------

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        object_id: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const std::ffi::c_void,
        data_size: *mut u32,
        data: *mut std::ffi::c_void,
    ) -> i32;

    fn AudioObjectGetPropertyDataSize(
        object_id: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const std::ffi::c_void,
        data_size: *mut u32,
    ) -> i32;
}

const K_AUDIO_OBJECT_SYSTEM_OBJECT: u32 = 1;
const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: u32 = u32::from_be_bytes(*b"dOut");
const K_AUDIO_DEVICE_PROPERTY_DEVICE_UID: u32 = u32::from_be_bytes(*b"uid ");
const K_AUDIO_HARDWARE_PROPERTY_DEVICES: u32 = u32::from_be_bytes(*b"dev#");
const K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE: u32 = u32::from_be_bytes(*b"tran");
const K_AUDIO_DEVICE_TRANSPORT_TYPE_BUILT_IN: u32 = u32::from_be_bytes(*b"bltn");
const K_AUDIO_DEVICE_PROPERTY_STREAMS: u32 = u32::from_be_bytes(*b"stm#");
const K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT: u32 = u32::from_be_bytes(*b"outp");

/// UID của loa/headphone đang là output mặc định (ví dụ loa Bluetooth).
unsafe fn default_output_device_uid() -> Option<id> {
    let address = AudioObjectPropertyAddress {
        selector: K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
        scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    let mut device: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = AudioObjectGetPropertyData(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        &address,
        0,
        std::ptr::null(),
        &mut size,
        &mut device as *mut u32 as *mut std::ffi::c_void,
    );
    if status != 0 || device == 0 {
        return None;
    }

    device_uid(device)
}

/// UID của một thiết bị âm thanh bất kỳ.
unsafe fn device_uid(device: u32) -> Option<id> {
    let address = AudioObjectPropertyAddress {
        selector: K_AUDIO_DEVICE_PROPERTY_DEVICE_UID,
        scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let mut uid: *const std::ffi::c_void = std::ptr::null();
    let mut size = std::mem::size_of::<*const std::ffi::c_void>() as u32;
    let status = AudioObjectGetPropertyData(
        device,
        &address,
        0,
        std::ptr::null(),
        &mut size,
        &mut uid as *mut _ as *mut std::ffi::c_void,
    );
    if status != 0 || uid.is_null() {
        return None;
    }
    Some(uid as id)
}

/// UID của loa trong của máy (built-in output), bỏ qua micro built-in và mọi thiết bị ngoài.
unsafe fn builtin_output_uid() -> Option<id> {
    let address = AudioObjectPropertyAddress {
        selector: K_AUDIO_HARDWARE_PROPERTY_DEVICES,
        scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };

    let mut size: u32 = 0;
    if AudioObjectGetPropertyDataSize(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        &address,
        0,
        std::ptr::null(),
        &mut size,
    ) != 0
        || size < 4
    {
        return None;
    }

    let mut devices: Vec<u32> = vec![0; (size / 4) as usize];
    if AudioObjectGetPropertyData(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        &address,
        0,
        std::ptr::null(),
        &mut size,
        devices.as_mut_ptr() as *mut std::ffi::c_void,
    ) != 0
    {
        return None;
    }

    for device in devices {
        let transport_address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let mut transport: u32 = 0;
        let mut value_size = std::mem::size_of::<u32>() as u32;
        if AudioObjectGetPropertyData(
            device,
            &transport_address,
            0,
            std::ptr::null(),
            &mut value_size,
            &mut transport as *mut u32 as *mut std::ffi::c_void,
        ) != 0
        {
            continue;
        }
        if transport != K_AUDIO_DEVICE_TRANSPORT_TYPE_BUILT_IN {
            continue;
        }

        // Chỉ nhận thiết bị có đường ra (loa), tránh micro của máy
        let streams_address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_STREAMS,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let mut stream_size: u32 = 0;
        if AudioObjectGetPropertyDataSize(device, &streams_address, 0, std::ptr::null(), &mut stream_size) != 0
            || stream_size == 0
        {
            continue;
        }

        if let Some(uid) = device_uid(device) {
            return Some(uid);
        }
    }

    None
}

/// Nạp sẵn vài bản sao để click liên tiếp vẫn phát, không cắt tiếng của nhau.
fn load_sounds() {
    let mut pool: Vec<usize> = Vec::with_capacity(4);
    unsafe {
        let bundle: id = msg_send![class!(NSBundle), mainBundle];
        let path: id = msg_send![
            bundle,
            pathForResource: nsstring("click")
            ofType: nsstring("wav")
        ];
        if path == nil {
            eprintln!("Không thấy click.wav trong bundle: app sẽ chạy không tiếng.");
        } else {
            // Ghim phát ra loa trong của máy. Nếu để NSSound tự chọn, nó đi theo đường
            // "alert sound" và có thể ra cả Bluetooth/AirPlay — mỗi lần đánh thức thiết bị
            // ngoài là một lần macOS hiện HUD âm lượng.
            let device_uid = match builtin_output_uid() {
                Some(uid) => Some(uid),
                None => {
                    eprintln!("Không tìm thấy loa trong của máy: phát ra thiết bị ra mặc định.");
                    default_output_device_uid()
                }
            };
            if device_uid.is_none() {
                eprintln!("Không đọc được thiết bị âm thanh: dùng đường phát mặc định.");
            }
            for _ in 0..4 {
                let sound: id = msg_send![class!(NSSound), alloc];
                let sound: id = msg_send![sound, initWithContentsOfFile: path byReference: YES];
                if sound != nil {
                    let _: () = msg_send![sound, setVolume: sound_volume()];
                    if let Some(uid) = device_uid {
                        let _: () = msg_send![sound, setPlaybackDeviceIdentifier: uid];
                    }
                    pool.push(sound as usize);
                }
            }
        }
    }
    SOUND_POOL.get_or_init(|| pool);
}

fn play_click_sound() {
    if !sound_enabled() {
        return;
    }
    let Some(pool) = SOUND_POOL.get() else {
        return;
    };
    if pool.is_empty() {
        return;
    }
    let start = SOUND_NEXT.fetch_add(1, Ordering::Relaxed) % pool.len();
    unsafe {
        // Ưu tiên bản đang rảnh: không stop() để khỏi đóng/mở lại stream âm thanh
        for offset in 0..pool.len() {
            let sound = pool[(start + offset) % pool.len()] as id;
            let playing: bool = msg_send![sound, isPlaying];
            if !playing {
                let _: bool = msg_send![sound, play];
                return;
            }
        }
        let sound = pool[start] as id;
        let _: bool = msg_send![sound, play];
    }
}

// ---------- Hành động của panel settings ----------

extern "C" fn open_settings(_this: &Object, _sel: Sel, _sender: id) {
    unsafe {
        let _: () = msg_send![NSApp(), activateIgnoringOtherApps: YES];
        if let Some(window) = SETTINGS_WINDOW.get() {
            let window = *window as id;
            let _: () = msg_send![window, makeKeyAndOrderFront: nil];
        }
    }
}

extern "C" fn toggle_sound(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let state: i64 = msg_send![sender, state];
        let enabled = state != 0;
        SOUND_ENABLED.store(enabled, Ordering::Relaxed);
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("SoundEnabled");
        let _: () = msg_send![defaults, setBool: enabled forKey: key];
    }
}

extern "C" fn change_volume(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let value: f64 = msg_send![sender, doubleValue];
        let volume = set_sound_volume(value as f32);
        if let Some(pool) = SOUND_POOL.get() {
            for sound in pool {
                let _: () = msg_send![*sound as id, setVolume: volume];
            }
        }
        if let Some(label) = VOLUME_LABEL.get() {
            let text = format!("{}%", (volume * 100.0).round() as i32);
            let _: () = msg_send![*label as id, setStringValue: nsstring(&text)];
        }

        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("SoundVolume");
        let _: () = msg_send![defaults, setFloat: volume forKey: key];
    }
}

extern "C" fn quit_app(_this: &Object, _sel: Sel, _sender: id) {
    unsafe {
        let _: () = msg_send![NSApp(), terminate: nil];
    }
}

fn register_controller_class() -> *const Class {
    let mut decl = ClassDecl::new("FireworksController", class!(NSObject)).unwrap();
    unsafe {
        decl.add_method(
            sel!(openSettings:),
            open_settings as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(toggleSound:),
            toggle_sound as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(changeVolume:),
            change_volume as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(quitApp:),
            quit_app as extern "C" fn(&Object, Sel, id),
        );
    }
    decl.register()
}

unsafe fn build_settings_window(controller: id) -> id {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 212.0));
    let window = NSWindow::alloc(nil).initWithContentRect_styleMask_backing_defer_(
        frame,
        NSWindowStyleMask::NSTitledWindowMask | NSWindowStyleMask::NSClosableWindowMask,
        NSBackingStoreBuffered,
        NO,
    );
    let _: () = msg_send![window, setTitle: nsstring("Mac Fireworks")];
    let _: () = msg_send![window, setReleasedWhenClosed: NO];
    let _: () = msg_send![window, center];

    let content: id = msg_send![window, contentView];

    let title: id = msg_send![class!(NSTextField), labelWithString: nsstring("Fireworks on click")];
    let font: id = msg_send![class!(NSFont), boldSystemFontOfSize: 13.0f64];
    let _: () = msg_send![title, setFont: font];
    let _: () = msg_send![title, setFrame: NSRect::new(NSPoint::new(20.0, 166.0), NSSize::new(280.0, 20.0))];
    let _: () = msg_send![content, addSubview: title];

    let checkbox: id = msg_send![
        class!(NSButton),
        checkboxWithTitle: nsstring("Play firework sound on click")
        target: controller
        action: sel!(toggleSound:)
    ];
    let state: i64 = if sound_enabled() { 1 } else { 0 };
    let _: () = msg_send![checkbox, setState: state];
    let _: () = msg_send![checkbox, setFrame: NSRect::new(NSPoint::new(20.0, 136.0), NSSize::new(280.0, 22.0))];
    let _: () = msg_send![content, addSubview: checkbox];

    let volume_label: id = msg_send![class!(NSTextField), labelWithString: nsstring("Volume")];
    let _: () = msg_send![volume_label, setFrame: NSRect::new(NSPoint::new(20.0, 110.0), NSSize::new(120.0, 18.0))];
    let _: () = msg_send![content, addSubview: volume_label];

    // Phần trăm âm lượng hiện tại, cập nhật ngay khi kéo slider
    let percent_text = format!("{}%", (sound_volume() * 100.0).round() as i32);
    let percent: id = msg_send![class!(NSTextField), labelWithString: nsstring(&percent_text)];
    let percent_font: id = msg_send![class!(NSFont), monospacedDigitSystemFontOfSize: 12.0f64 weight: 0.0f64];
    let _: () = msg_send![percent, setFont: percent_font];
    let _: () = msg_send![percent, setAlignment: 2i64];
    let percent_color: id = msg_send![class!(NSColor), secondaryLabelColor];
    let _: () = msg_send![percent, setTextColor: percent_color];
    let _: () = msg_send![percent, setFrame: NSRect::new(NSPoint::new(240.0, 110.0), NSSize::new(60.0, 18.0))];
    let _: () = msg_send![content, addSubview: percent];
    let _ = VOLUME_LABEL.set(percent as usize);

    let slider: id = msg_send![class!(NSSlider), alloc];
    let slider: id = msg_send![slider, initWithFrame: NSRect::new(NSPoint::new(20.0, 84.0), NSSize::new(280.0, 24.0))];
    let _: () = msg_send![slider, setMinValue: 0.0f64];
    let _: () = msg_send![slider, setMaxValue: 1.0f64];
    let _: () = msg_send![slider, setDoubleValue: sound_volume() as f64];
    let _: () = msg_send![slider, setContinuous: YES];
    let _: () = msg_send![slider, setTarget: controller];
    let _: () = msg_send![slider, setAction: sel!(changeVolume:)];
    let _: () = msg_send![content, addSubview: slider];

    let hint: id = msg_send![class!(NSTextField), labelWithString: nsstring("Each click fires a neon firework across the screen.")];
    let _: () = msg_send![hint, setFrame: NSRect::new(NSPoint::new(20.0, 58.0), NSSize::new(280.0, 18.0))];
    let color: id = msg_send![class!(NSColor), secondaryLabelColor];
    let _: () = msg_send![hint, setTextColor: color];
    let _: () = msg_send![content, addSubview: hint];

    let quit: id = msg_send![
        class!(NSButton),
        buttonWithTitle: nsstring("Quit Mac Fireworks")
        target: controller
        action: sel!(quitApp:)
    ];
    let _: () = msg_send![quit, setFrame: NSRect::new(NSPoint::new(20.0, 14.0), NSSize::new(170.0, 30.0))];
    let _: () = msg_send![content, addSubview: quit];

    window as id
}

unsafe fn build_status_item(controller: id) {
    let bar: id = msg_send![class!(NSStatusBar), systemStatusBar];
    let item: id = msg_send![bar, statusItemWithLength: -1.0f64];
    let button: id = msg_send![item, button];

    // SF Symbol "sparkles" — biểu tượng pháo hoa sẵn có của hệ thống
    let image: id = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: nsstring("sparkles")
        accessibilityDescription: nsstring("Mac Fireworks")
    ];
    if image != nil {
        let _: () = msg_send![image, setTemplate: YES];
        let _: () = msg_send![button, setImage: image];
    } else {
        let _: () = msg_send![button, setTitle: nsstring("✦")];
    }

    let _: () = msg_send![button, setTarget: controller];
    let _: () = msg_send![button, setAction: sel!(openSettings:)];
    let _: () = msg_send![item, retain];
    let _ = STATUS_ITEM.set(item as usize);
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

extern "C" fn draw_rect(this: &Object, _sel: Sel, _rect: NSRect) {
    unsafe {
        // Mỗi màn hình có một cửa sổ riêng. Hạt lưu ở toạ độ toàn cục (gốc là góc
        // dưới-trái màn hình chính), nên phải trừ gốc cửa sổ này đi mới vẽ đúng chỗ.
        let window: id = msg_send![this as *const Object as id, window];
        let frame: NSRect = if window != nil {
            msg_send![window, frame]
        } else {
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
        };
        let offset_x = frame.origin.x;
        let offset_y = frame.origin.y;
        let max_x = frame.size.width;
        let max_y = frame.size.height;

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
            let x = p.x - offset_x;
            let y = p.y - offset_y;
            // Hạt nằm ngoài màn hình của cửa sổ này thì bỏ qua
            if x < -40.0 || y < -40.0 || x > max_x + 40.0 || y > max_y + 40.0 {
                continue;
            }

            let fade = p.life.powf(1.35);
            let twinkle = 0.45 + 0.55 * (0.5 + 0.5 * p.twinkle_phase.sin());
            let alpha = (fade * twinkle).clamp(0.0, 1.0);
            let core = p.size * (0.35 + 0.65 * p.life);

            // Quầng sáng neon nhiều lớp: toả mềm ra xung quanh
            fill_oval(x, y, core * 4.4, p.r, p.g, p.b, alpha * 0.06);
            fill_oval(x, y, core * 2.7, p.r, p.g, p.b, alpha * 0.12);
            fill_oval(x, y, core * 1.7, p.r, p.g, p.b, alpha * 0.24);

            // Ruột nóng: pha trắng để hạt trông "phát sáng"
            let hot_r = p.r + (1.0 - p.r) * 0.7;
            let hot_g = p.g + (1.0 - p.g) * 0.7;
            let hot_b = p.b + (1.0 - p.b) * 0.7;
            fill_oval(x, y, core * 0.85, hot_r, hot_g, hot_b, alpha * 0.95);
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

        // Không dùng [NSScreen mainScreen]: nó trả về màn hình đang có cửa sổ nhận
        // phím (có thể là màn phụ), làm sai phép lật trục Y. CGEvent dùng toạ độ
        // gốc ở góc trên-trái của màn hình chính, nên lấy đúng chiều cao của nó.
        let main_display_bounds = CGDisplayBounds(CGMainDisplayID());
        let screen_height = main_display_bounds.size.height;

        // Mỗi màn hình (MacBook, màn hình ngoài qua DP/HDMI...) một cửa sổ overlay riêng
        let view_class = register_view_class();
        let mut views: Vec<usize> = Vec::new();
        let screens: id = msg_send![class!(NSScreen), screens];
        let screen_count: usize = msg_send![screens, count];
        for index in 0..screen_count {
            let screen: id = msg_send![screens, objectAtIndex: index];
            let frame: NSRect = msg_send![screen, frame];

            let window = NSWindow::alloc(nil).initWithContentRect_styleMask_backing_defer_(
                frame,
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

            let view: id = msg_send![view_class, alloc];
            let view: id = msg_send![
                view,
                initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), frame.size)
            ];
            window.setContentView_(view);
            window.makeKeyAndOrderFront_(nil);
            views.push(view as usize);
        }

        eprintln!("Overlay đã tạo cho {} màn hình.", views.len());

        // Một quả pháo chào ngay khi mở app: thấy được hiệu ứng kể cả khi
        // chưa cấp quyền Trợ năng.
        spawn_firework(
            main_display_bounds.size.width / 2.0,
            screen_height / 2.0,
        );

        // Tiếng click, panel settings và icon trên menu bar
        load_sound_preference();
        load_sounds();
        let controller: id = msg_send![register_controller_class(), new];
        let _ = SETTINGS_WINDOW.set(build_settings_window(controller) as usize);
        build_status_item(controller);

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
                    // Phát tiếng ở main thread (trong timer) để không đụng AppKit từ thread này
                    SOUND_PENDING.store(true, Ordering::Relaxed);
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

        let timer_block = block::ConcreteBlock::new(move |_timer: id| {
            if SOUND_PENDING.swap(false, Ordering::Relaxed) {
                play_click_sound();
            }
            for view in &views {
                let v = *view as id;
                let _: () = msg_send![v, setNeedsDisplay: YES];
            }
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
