#![allow(unexpected_cfgs, deprecated)]

use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSBackingStoreBuffered, NSColor,
    NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize, NSString};
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun};
use core_graphics::context::CGContext;
use core_graphics::display::{CGDisplayBounds, CGMainDisplayID};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::sys::CGContextRef;
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use rand::Rng;
use std::collections::HashSet;
use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
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
    fn CGCursorIsVisible() -> bool;
    fn CGContextClearRect(context: CGContextRef, rect: CGRect);
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

// ---------- Hiệu ứng ----------

const EFFECT_FIREWORKS: u8 = 0;
const EFFECT_NEON: u8 = 1;

static EFFECT: AtomicU8 = AtomicU8::new(EFFECT_FIREWORKS);

// ---------- Skin của dải neon ----------

const SKIN_CLASSIC: u8 = 0;
const SKIN_PIXEL_LOVE: u8 = 1;

static SKIN: AtomicU8 = AtomicU8::new(SKIN_CLASSIC);
/// Con trỏ có đang được hiển thị không (video fullscreen sẽ ẩn con trỏ)
static CURSOR_VISIBLE: AtomicBool = AtomicBool::new(true);

/// Mẫu vị trí chuột (toạ độ Cocoa) do CGEventTap ghi lại, kèm thời điểm nhận.
/// Tap bắt được mọi event chuột nên mẫu dày hơn nhiều so với 1 mẫu/frame.
static MOUSE_SAMPLES: Mutex<Vec<MouseSample>> = Mutex::new(Vec::new());

const MAX_MOUSE_SAMPLES: usize = 512;

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

fn load_effect_preference() {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("Effect");
        let stored: id = msg_send![defaults, objectForKey: key];
        if stored != nil {
            let value: i64 = msg_send![defaults, integerForKey: key];
            let effect = if value == EFFECT_NEON as i64 {
                EFFECT_NEON
            } else {
                EFFECT_FIREWORKS
            };
            EFFECT.store(effect, Ordering::Relaxed);
        }
    }
}

fn load_skin_preference() {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("Skin");
        let stored: id = msg_send![defaults, objectForKey: key];
        if stored != nil {
            let value: i64 = msg_send![defaults, integerForKey: key];
            let skin = if value == SKIN_PIXEL_LOVE as i64 {
                SKIN_PIXEL_LOVE
            } else {
                SKIN_CLASSIC
            };
            SKIN.store(skin, Ordering::Relaxed);
        }
    }
}

fn skin_index() -> i64 {
    if SKIN.load(Ordering::Relaxed) == SKIN_PIXEL_LOVE {
        1
    } else {
        0
    }
}

fn effect_index() -> i64 {
    if EFFECT.load(Ordering::Relaxed) == EFFECT_NEON {
        1
    } else {
        0
    }
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

extern "C" fn change_effect(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let index: i64 = msg_send![sender, indexOfSelectedItem];
        let effect = if index == 1 {
            EFFECT_NEON
        } else {
            EFFECT_FIREWORKS
        };
        EFFECT.store(effect, Ordering::Relaxed);

        // Đổi hiệu ứng thì dọn cảnh cũ cho sạch
        if let Ok(mut scene) = get_scene().lock() {
            scene.particles.clear();
            scene.trail.clear();
            scene.head_init = false;
            scene.idle_secs = 0.0;
        }

        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("Effect");
        let _: () = msg_send![defaults, setInteger: effect as i64 forKey: key];
    }
}

extern "C" fn change_skin(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let index: i64 = msg_send![sender, indexOfSelectedItem];
        let skin = if index == 1 {
            SKIN_PIXEL_LOVE
        } else {
            SKIN_CLASSIC
        };
        SKIN.store(skin, Ordering::Relaxed);

        // Đổi skin thì dọn trail để không lẫn hai kiểu vẽ
        if let Ok(mut scene) = get_scene().lock() {
            scene.trail.clear();
            scene.head_init = false;
        }

        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("Skin");
        let _: () = msg_send![defaults, setInteger: skin as i64 forKey: key];
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
            sel!(changeEffect:),
            change_effect as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(changeSkin:),
            change_skin as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(quitApp:),
            quit_app as extern "C" fn(&Object, Sel, id),
        );
    }
    decl.register()
}

unsafe fn build_settings_window(controller: id) -> id {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 292.0));
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

    let title: id = msg_send![class!(NSTextField), labelWithString: nsstring("Mac Fireworks")];
    let font: id = msg_send![class!(NSFont), boldSystemFontOfSize: 13.0f64];
    let _: () = msg_send![title, setFont: font];
    let _: () = msg_send![title, setFrame: NSRect::new(NSPoint::new(20.0, 246.0), NSSize::new(280.0, 20.0))];
    let _: () = msg_send![content, addSubview: title];

    // Chọn hiệu ứng: Fireworks (click) hay Neon trail (theo con trỏ)
    let effect_label: id = msg_send![class!(NSTextField), labelWithString: nsstring("Effect")];
    let _: () = msg_send![effect_label, setFrame: NSRect::new(NSPoint::new(20.0, 216.0), NSSize::new(70.0, 18.0))];
    let _: () = msg_send![content, addSubview: effect_label];

    let popup: id = msg_send![class!(NSPopUpButton), alloc];
    let popup: id = msg_send![
        popup,
        initWithFrame: NSRect::new(NSPoint::new(100.0, 210.0), NSSize::new(200.0, 26.0))
        pullsDown: NO
    ];
    let _: () = msg_send![popup, addItemWithTitle: nsstring("Fireworks")];
    let _: () = msg_send![popup, addItemWithTitle: nsstring("Neon trail")];
    let _: () = msg_send![popup, selectItemAtIndex: effect_index()];
    let _: () = msg_send![popup, setTarget: controller];
    let _: () = msg_send![popup, setAction: sel!(changeEffect:)];
    let _: () = msg_send![content, addSubview: popup];

    // Skin của dải neon: Classic (dải neon mượt) hay Pixel love (tim pixel)
    let skin_label: id = msg_send![class!(NSTextField), labelWithString: nsstring("Skin")];
    let _: () = msg_send![skin_label, setFrame: NSRect::new(NSPoint::new(20.0, 182.0), NSSize::new(70.0, 18.0))];
    let _: () = msg_send![content, addSubview: skin_label];

    let skin_popup: id = msg_send![class!(NSPopUpButton), alloc];
    let skin_popup: id = msg_send![
        skin_popup,
        initWithFrame: NSRect::new(NSPoint::new(100.0, 176.0), NSSize::new(200.0, 26.0))
        pullsDown: NO
    ];
    let _: () = msg_send![skin_popup, addItemWithTitle: nsstring("Classic")];
    let _: () = msg_send![skin_popup, addItemWithTitle: nsstring("Pixel love")];
    let _: () = msg_send![skin_popup, selectItemAtIndex: skin_index()];
    let _: () = msg_send![skin_popup, setTarget: controller];
    let _: () = msg_send![skin_popup, setAction: sel!(changeSkin:)];
    let _: () = msg_send![content, addSubview: skin_popup];

    let checkbox: id = msg_send![
        class!(NSButton),
        checkboxWithTitle: nsstring("Play firework sound on click")
        target: controller
        action: sel!(toggleSound:)
    ];
    let state: i64 = if sound_enabled() { 1 } else { 0 };
    let _: () = msg_send![checkbox, setState: state];
    let _: () = msg_send![checkbox, setFrame: NSRect::new(NSPoint::new(20.0, 146.0), NSSize::new(280.0, 22.0))];
    let _: () = msg_send![content, addSubview: checkbox];

    let volume_label: id = msg_send![class!(NSTextField), labelWithString: nsstring("Volume")];
    let _: () = msg_send![volume_label, setFrame: NSRect::new(NSPoint::new(20.0, 118.0), NSSize::new(120.0, 18.0))];
    let _: () = msg_send![content, addSubview: volume_label];

    // Phần trăm âm lượng hiện tại, cập nhật ngay khi kéo slider
    let percent_text = format!("{}%", (sound_volume() * 100.0).round() as i32);
    let percent: id = msg_send![class!(NSTextField), labelWithString: nsstring(&percent_text)];
    let percent_font: id = msg_send![class!(NSFont), monospacedDigitSystemFontOfSize: 12.0f64 weight: 0.0f64];
    let _: () = msg_send![percent, setFont: percent_font];
    let _: () = msg_send![percent, setAlignment: 2i64];
    let percent_color: id = msg_send![class!(NSColor), secondaryLabelColor];
    let _: () = msg_send![percent, setTextColor: percent_color];
    let _: () = msg_send![percent, setFrame: NSRect::new(NSPoint::new(240.0, 118.0), NSSize::new(60.0, 18.0))];
    let _: () = msg_send![content, addSubview: percent];
    let _ = VOLUME_LABEL.set(percent as usize);

    let slider: id = msg_send![class!(NSSlider), alloc];
    let slider: id = msg_send![slider, initWithFrame: NSRect::new(NSPoint::new(20.0, 92.0), NSSize::new(280.0, 24.0))];
    let _: () = msg_send![slider, setMinValue: 0.0f64];
    let _: () = msg_send![slider, setMaxValue: 1.0f64];
    let _: () = msg_send![slider, setDoubleValue: sound_volume() as f64];
    let _: () = msg_send![slider, setContinuous: YES];
    let _: () = msg_send![slider, setTarget: controller];
    let _: () = msg_send![slider, setAction: sel!(changeVolume:)];
    let _: () = msg_send![content, addSubview: slider];

    let hint: id = msg_send![class!(NSTextField), labelWithString: nsstring("Click fires a firework; Neon trail follows your cursor.")];
    let _: () = msg_send![hint, setFrame: NSRect::new(NSPoint::new(20.0, 66.0), NSSize::new(280.0, 18.0))];
    let color: id = msg_send![class!(NSColor), secondaryLabelColor];
    let _: () = msg_send![hint, setTextColor: color];
    let _: () = msg_send![content, addSubview: hint];

    let quit: id = msg_send![
        class!(NSButton),
        buttonWithTitle: nsstring("Quit Mac Fireworks")
        target: controller
        action: sel!(quitApp:)
    ];
    let _: () = msg_send![quit, setFrame: NSRect::new(NSPoint::new(20.0, 20.0), NSSize::new(170.0, 30.0))];
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

type MouseSample = (f64, f64, Instant);

fn push_mouse_sample(x: f64, y: f64, at: Instant) {
    let mut buffer = match MOUSE_SAMPLES.lock() {
        Ok(buffer) => buffer,
        Err(poisoned) => poisoned.into_inner(),
    };
    if buffer.len() >= MAX_MOUSE_SAMPLES {
        let excess = buffer.len() - MAX_MOUSE_SAMPLES + 1;
        buffer.drain(0..excess);
    }
    buffer.push((x, y, at));
}

fn take_mouse_samples() -> Vec<MouseSample> {
    match MOUSE_SAMPLES.lock() {
        Ok(mut buffer) => std::mem::take(&mut *buffer),
        Err(poisoned) => {
            let mut buffer = poisoned.into_inner();
            std::mem::take(&mut *buffer)
        }
    }
}

struct TrailPoint {
    x: f64,
    y: f64,
    life: f64,
}

struct Scene {
    particles: Vec<Particle>,
    trail: Vec<TrailPoint>,
    head: (f64, f64),
    head_init: bool,
    idle_secs: f64,
    hue_phase: f64,
    last_frame: Option<Instant>,
    /// Các vùng đã vẽ trong ~0.75 giây gần đây (toạ độ Cocoa toàn cục).
    /// Phải giữ nhiều frame vì khi rê chuột rất nhanh, trail bị giới hạn 320 điểm
    /// nên điểm cũ bị đẩy ra chỉ sau vài frame -> vùng "frame trước" không còn
    /// chứa pixel đã vẽ trước đó, và chúng sẽ nằm lại thành "vệt dính".
    paint_history: Vec<(f64, f64, f64, f64)>,
    /// Vùng cần vẽ lại frame này = hợp các vùng trong `paint_history`
    dirty_bounds: Option<(f64, f64, f64, f64)>,
    /// Frame trước chuột có đang di chuyển không (để biết lúc vừa dừng)
    was_moving: bool,
    /// Yêu cầu vẽ lại toàn bộ overlay một lần (dọn sạch mọi pixel sót lại)
    full_refresh: bool,
}

static SCENE: OnceLock<Mutex<Scene>> = OnceLock::new();

fn get_scene() -> &'static Mutex<Scene> {
    SCENE.get_or_init(|| {
        Mutex::new(Scene {
            particles: Vec::with_capacity(600),
            trail: Vec::with_capacity(320),
            head: (0.0, 0.0),
            head_init: false,
            idle_secs: 0.0,
            hue_phase: 0.0,
            last_frame: None,
            paint_history: Vec::new(),
            dirty_bounds: None,
            was_moving: false,
            full_refresh: false,
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

/// Màu neon chạy vòng theo hue (0..1).
fn neon_color(phase: f64) -> (f64, f64, f64) {
    let h = (phase.fract() + 1.0).fract() * 6.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    match h as u32 {
        0 => (1.0, x, 0.35),
        1 => (x, 1.0, 0.35),
        2 => (0.35, 1.0, x),
        3 => (0.35, x, 1.0),
        4 => (x, 0.35, 1.0),
        _ => (1.0, 0.35, x),
    }
}

/// Pháp tuyến đơn vị của đường tại điểm `i` (dùng để dựng hai biên của dải).
fn ribbon_normal(points: &[(f64, f64)], i: usize) -> (f64, f64) {
    let prev = points[i.saturating_sub(1)];
    let next = points[(i + 1).min(points.len() - 1)];
    let dx = next.0 - prev.0;
    let dy = next.1 - prev.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.0001 {
        (0.0, 0.0)
    } else {
        (-dy / len, dx / len)
    }
}

/// Vẽ cả dải sáng thành **một** path kín (viền trái + viền phải), thuôn dần
/// về phía đuôi. Một lần fill nên không có mối nối, không bị "hạt".
unsafe fn fill_ribbon(
    points: &[(f64, f64)],
    max_width: f64,
    r: f64,
    g: f64,
    b: f64,
    alpha: f64,
) {
    let count = points.len();
    if count < 2 || alpha <= 0.004 || max_width <= 0.2 {
        return;
    }

    let half_width = |i: usize| -> f64 {
        let t = i as f64 / (count - 1) as f64; // 0 = đuôi, 1 = đầu
        max_width * 0.5 * t.powf(0.7)
    };

    let Some(context) = current_cg_context() else {
        return;
    };

    for i in 0..count {
        let (nx, ny) = ribbon_normal(points, i);
        let hw = half_width(i);
        let x = points[i].0 + nx * hw;
        let y = points[i].1 + ny * hw;
        if i == 0 {
            context.move_to_point(x, y);
        } else {
            context.add_line_to_point(x, y);
        }
    }
    for i in (0..count).rev() {
        let (nx, ny) = ribbon_normal(points, i);
        let hw = half_width(i);
        context.add_line_to_point(points[i].0 - nx * hw, points[i].1 - ny * hw);
    }
    context.close_path();

    context.set_rgb_fill_color(
        r.clamp(0.0, 1.0),
        g.clamp(0.0, 1.0),
        b.clamp(0.0, 1.0),
        alpha.clamp(0.0, 1.0),
    );
    context.fill_path();
}

/// Nối `from` -> `to` thành các điểm cách nhau ~4 px để dải sáng không bị đứt.
fn push_trail_segment(trail: &mut Vec<TrailPoint>, from: (f64, f64), to: (f64, f64)) {
    const POINT_SPACING: f64 = 4.0;
    const MAX_POINTS_PER_SEGMENT: usize = 48;

    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance < 0.3 {
        trail.push(TrailPoint {
            x: to.0,
            y: to.1,
            life: 1.0,
        });
        return;
    }

    let steps = ((distance / POINT_SPACING).ceil() as usize).clamp(1, MAX_POINTS_PER_SEGMENT);
    for step in 1..=steps {
        let t = step as f64 / steps as f64;
        trail.push(TrailPoint {
            x: from.0 + dx * t,
            y: from.1 + dy * t,
            life: 1.0,
        });
    }
}

fn union_bounds(
    a: Option<(f64, f64, f64, f64)>,
    b: Option<(f64, f64, f64, f64)>,
) -> Option<(f64, f64, f64, f64)> {
    match (a, b) {
        (Some(a), Some(b)) => Some((
            a.0.min(b.0),
            a.1.min(b.1),
            a.2.max(b.2),
            a.3.max(b.3),
        )),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// Số frame gần nhất được giữ lại để tính vùng vẽ lại (45 frame ≈ 0,75 s, dài
/// hơn tuổi thọ một điểm trail là 0,625 s).
const PAINT_HISTORY_FRAMES: usize = 45;

/// Vùng cần vẽ lại của frame này = hợp nội dung của các frame trong
/// `PAINT_HISTORY_FRAMES` vừa qua. Nhờ vậy mọi pixel đã vẽ còn nhìn thấy được
/// đều nằm trong vùng được xoá, kể cả khi trail đã bị cắt bớt điểm.
fn update_dirty_bounds(scene: &mut Scene) {
    let mut bounds: Option<(f64, f64, f64, f64)> = None;
    let mut include = |x: f64, y: f64, radius: f64| {
        let (x0, y0) = (x - radius, y - radius);
        let (x1, y1) = (x + radius, y + radius);
        bounds = Some(match bounds {
            Some(b) => (b.0.min(x0), b.1.min(y0), b.2.max(x1), b.3.max(y1)),
            None => (x0, y0, x1, y1),
        });
    };

    // Hạt lấp lánh: quầng ngoài cùng là core * 4.4
    for particle in scene.particles.iter() {
        include(particle.x, particle.y, particle.size * 4.4 + 6.0);
    }
    if EFFECT.load(Ordering::Relaxed) == EFFECT_NEON && CURSOR_VISIBLE.load(Ordering::Relaxed) {
        // Dải sáng: nửa bề rộng quầng lớn nhất (48/2) + làm mượt + khử răng cưa,
        // và quầng đầu dải (bán kính 30)
        for point in scene.trail.iter() {
            include(point.x, point.y, 40.0);
        }
    }

    if let Some(bounds) = bounds {
        if scene.paint_history.len() >= PAINT_HISTORY_FRAMES {
            scene.paint_history.remove(0);
        }
        scene.paint_history.push(bounds);
    }

    let mut dirty: Option<(f64, f64, f64, f64)> = None;
    for bounds in scene.paint_history.iter() {
        dirty = union_bounds(dirty, Some(*bounds));
    }
    scene.dirty_bounds = dirty.map(|b| (b.0 - 3.0, b.1 - 3.0, b.2 + 3.0, b.3 + 3.0));
}

/// Màu skin "Pixel love": hồng đậm ở đuôi -> hồng nhạt ở đầu.
fn pixel_love_color(t: f64) -> (f64, f64, f64) {
    let t = t.clamp(0.0, 1.0);
    (1.0, 0.18 + 0.45 * t, 0.55 + 0.35 * t)
}

/// Bitmap trái tim 7x6 cho skin pixel (1 = tô màu).
const PIXEL_HEART: [u8; 6] = [
    0b0110110, 0b1111111, 0b1111111, 0b0111110, 0b0011100, 0b0001000,
];

unsafe fn draw_pixel_heart(
    context: &CGContext,
    cx: f64,
    cy: f64,
    cell: f64,
    r: f64,
    g: f64,
    b: f64,
    alpha: f64,
) {
    context.set_rgb_fill_color(r, g, b, alpha);

    let width = 7.0 * cell;
    let height = 6.0 * cell;
    let mut rects: Vec<CGRect> = Vec::with_capacity(30);
    for (row, bits) in PIXEL_HEART.iter().enumerate() {
        for col in 0..7 {
            if bits & (1 << (6 - col)) == 0 {
                continue;
            }
            let x = cx - width * 0.5 + col as f64 * cell;
            let y = cy - height * 0.5 + (5 - row) as f64 * cell;
            rects.push(CGRect::new(
                &CGPoint::new(x, y),
                &CGSize::new((cell - 0.7).max(1.0), (cell - 0.7).max(1.0)),
            ));
        }
    }
    context.fill_rects(&rects);
}

/// Skin "Pixel love": dải gồm các ô vuông hồng snap về lưới 6 px, rải trái tim
/// pixel dọc theo đường đi, đầu dải là một trái tim lớn.
unsafe fn draw_pixel_trail(points: &[(f64, f64)]) {
    let count = points.len();
    if count < 2 {
        return;
    }
    let Some(context) = current_cg_context() else {
        return;
    };

    const CELL: f64 = 6.0;
    const ALPHA: [f64; 4] = [0.45, 0.65, 0.82, 0.95];

    // Mỗi điểm trải ô vuông theo pháp tuyến -> cả dải thành một băng pixel dày,
    // snap về lưới 6 px nên nhìn đúng chất 8-bit.
    let mut buckets: [Vec<(f64, f64)>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let mut seen: HashSet<(i64, i64)> = HashSet::with_capacity(800);
    for i in 0..count {
        let t = i as f64 / (count - 1) as f64;
        let (nx, ny) = ribbon_normal(points, i);
        let half = (1.0 + 1.6 * t) * CELL;
        let lanes = (half / CELL).round().max(0.0) as i64;
        for lane in -lanes..=lanes {
            let px = points[i].0 + nx * lane as f64 * CELL;
            let py = points[i].1 + ny * lane as f64 * CELL;
            let cx = (px / CELL).round() as i64;
            let cy = (py / CELL).round() as i64;
            if !seen.insert((cx, cy)) {
                continue;
            }
            let level = ((t * 3.99) as usize).min(3);
            buckets[level].push((cx as f64 * CELL, cy as f64 * CELL));
        }
    }

    // Quầng hồng mờ phía sau để dải pixel nổi trên mọi nền
    let glow: Vec<CGRect> = buckets
        .iter()
        .flat_map(|cells| cells.iter().step_by(2))
        .map(|(x, y)| {
            let size = CELL * 2.2;
            CGRect::new(
                &CGPoint::new(x - size * 0.5, y - size * 0.5),
                &CGSize::new(size, size),
            )
        })
        .collect();
    if !glow.is_empty() {
        context.set_rgb_fill_color(1.0, 0.42, 0.78, 0.07);
        context.fill_rects(&glow);
    }

    for (level, cells) in buckets.iter().enumerate() {
        if cells.is_empty() {
            continue;
        }
        let t = (level as f64 + 0.5) / 4.0;
        let (r, g, b) = pixel_love_color(t);
        let size = CELL - 0.8;
        context.set_rgb_fill_color(r, g, b, ALPHA[level]);
        let rects: Vec<CGRect> = cells
            .iter()
            .map(|(x, y)| {
                CGRect::new(
                    &CGPoint::new(x - size * 0.5, y - size * 0.5),
                    &CGSize::new(size, size),
                )
            })
            .collect();
        context.fill_rects(&rects);
    }

    // Trái tim hồng nhạt rải dọc dải
    let step = 20;
    let mut index = step;
    while index < count {
        let t = index as f64 / (count - 1) as f64;
        let (x, y) = points[index];
        draw_pixel_heart(
            &context,
            (x / CELL).round() * CELL,
            (y / CELL).round() * CELL,
            2.8 + 0.8 * t,
            1.0,
            0.96,
            1.0,
            0.6 + 0.32 * t,
        );
        index += step;
    }

    // Đầu dải: trái tim lớn ngay tại con trỏ
    if let Some((x, y)) = points.last().copied() {
        draw_pixel_heart(
            &context,
            (x / CELL).round() * CELL,
            (y / CELL).round() * CELL,
            3.6,
            1.0,
            0.98,
            1.0,
            0.95,
        );
        let (r, g, b) = pixel_love_color(1.0);
        context.set_rgb_fill_color(r, g, b, 0.10);
        context.fill_ellipse_in_rect(CGRect::new(
            &CGPoint::new(x - 24.0, y - 24.0),
            &CGSize::new(48.0, 48.0),
        ));
    }
}

/// Mô phỏng một frame (gọi từ timer trên main thread, mỗi frame đúng một lần).
fn tick() {
    let now = Instant::now();
    let mut scene = match get_scene().lock() {
        Ok(scene) => scene,
        Err(poisoned) => poisoned.into_inner(),
    };

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

    if EFFECT.load(Ordering::Relaxed) != EFFECT_NEON {
        scene.trail.clear();
        scene.head_init = false;
        scene.idle_secs = 0.0;
        // Dải sáng vừa bị xoá -> vẫn phải vẽ lại vùng cũ một lần
        update_dirty_bounds(&mut scene);
        return;
    }

    // Con trỏ bị ẩn (ví dụ đang xem video fullscreen) -> ẩn luôn hiệu ứng
    // CGCursorIsVisible() là một lời gọi IPC sang WindowServer: gọi mỗi frame tốn
    // ~40% CPU. Kiểm tra ~6 lần/giây là đủ để ẩn/hiện theo con trỏ.
    static VIS_FRAME: AtomicUsize = AtomicUsize::new(0);
    let vis_frame = VIS_FRAME.fetch_add(1, Ordering::Relaxed);
    if vis_frame % 10 == 0 {
        let visible = unsafe { CGCursorIsVisible() };
        CURSOR_VISIBLE.store(visible, Ordering::Relaxed);
    }

    if !CURSOR_VISIBLE.load(Ordering::Relaxed) {
        scene.trail.clear();
        scene.head_init = false;
        scene.idle_secs = 0.0;
        update_dirty_bounds(&mut scene);
        return;
    }

    let cursor: NSPoint = unsafe { msg_send![class!(NSEvent), mouseLocation] };
    let cursor = (cursor.x, cursor.y);

    // Mẫu vị trí thật do CGEventTap ghi lại (mỗi event chuột là một mẫu), nên dải
    // sáng bám đúng đường đi kể cả khi chuột flick rất nhanh.
    let mut samples = take_mouse_samples();
    samples.retain(|(_, _, at)| now.duration_since(*at).as_secs_f64() < 0.5);

    if !scene.head_init {
        scene.head = cursor;
        scene.head_init = true;
        samples.clear();
    }

    // Vừa dừng chuột -> vẽ lại toàn bộ một lần cho chắc, dọn mọi pixel còn sót
    let moving = !samples.is_empty();
    if scene.was_moving && !moving {
        scene.full_refresh = true;
    }
    scene.was_moving = moving;

    if samples.is_empty() {
        scene.idle_secs += dt;
    } else {
        scene.idle_secs = 0.0;
    }

    scene.hue_phase = (scene.hue_phase + dt * 0.55) % 1.0;

    for point in scene.trail.iter_mut() {
        point.life -= dt * 1.6;
    }
    scene.trail.retain(|point| point.life > 0.0);

    const MAX_TRAIL_POINTS: usize = 320;
    let mut previous = scene.trail.last().map(|p| (p.x, p.y)).unwrap_or(scene.head);

    if samples.is_empty() {
        // Chuột đứng yên: giữ một điểm tại con trỏ
        push_trail_segment(&mut scene.trail, previous, cursor);
        previous = cursor;
    } else {
        for (x, y, _) in samples {
            push_trail_segment(&mut scene.trail, previous, (x, y));
            previous = (x, y);
        }
    }
    scene.head = previous;

    if scene.trail.len() > MAX_TRAIL_POINTS {
        let excess = scene.trail.len() - MAX_TRAIL_POINTS;
        scene.trail.drain(0..excess);
    }

    // Đứng yên -> lấp lánh neon ngay tại con trỏ
    if scene.idle_secs > 0.12 && scene.particles.len() < 500 {
        let mut rng = rand::thread_rng();
        if rng.gen::<f64>() < 0.75 {
            let angle = rng.gen_range(0.0..TAU);
            let speed = 0.15 + rng.gen::<f64>() * 0.7;
            let (sr, sg, sb) = neon_color(scene.hue_phase + rng.gen::<f64>() * 0.15);
            scene.particles.push(Particle {
                x: cursor.0 + angle.cos() * 2.0,
                y: cursor.1 + angle.sin() * 2.0,
                vx: angle.cos() * speed,
                vy: angle.sin() * speed,
                life: 1.0,
                decay: 0.022 + rng.gen::<f64>() * 0.02,
                r: sr,
                g: sg,
                b: sb,
                size: 1.0 + rng.gen::<f64>() * 1.6,
                gravity: 0.01,
                twinkle_phase: rng.gen_range(0.0..TAU),
                twinkle_speed: 0.5 + rng.gen::<f64>() * 1.0,
            });
        }
    }

    update_dirty_bounds(&mut scene);
}

fn current_cg_context_ref() -> Option<CGContextRef> {
    unsafe {
        let graphics: id = msg_send![class!(NSGraphicsContext), currentContext];
        if graphics == nil {
            return None;
        }
        let context: CGContextRef = msg_send![graphics, CGContext];
        if context.is_null() {
            None
        } else {
            Some(context)
        }
    }
}

/// CGContext hiện tại — vẽ thẳng bằng CoreGraphics nhanh hơn nhiều so với tạo
/// NSColor/NSBezierPath cho từng hình (mỗi frame có hàng trăm hình).
fn current_cg_context() -> Option<CGContext> {
    unsafe {
        let graphics: id = msg_send![class!(NSGraphicsContext), currentContext];
        if graphics == nil {
            return None;
        }
        let context: CGContextRef = msg_send![graphics, CGContext];
        if context.is_null() {
            None
        } else {
            Some(CGContext::from_existing_context_ptr(context))
        }
    }
}

fn fill_oval(cx: f64, cy: f64, radius: f64, r: f64, g: f64, b: f64, alpha: f64) {
    if radius <= 0.2 || alpha <= 0.002 {
        return;
    }
    let Some(context) = current_cg_context() else {
        return;
    };
    context.set_rgb_fill_color(
        r.clamp(0.0, 1.0),
        g.clamp(0.0, 1.0),
        b.clamp(0.0, 1.0),
        alpha.clamp(0.0, 1.0),
    );
    let rect = CGRect::new(
        &CGPoint::new(cx - radius, cy - radius),
        &CGSize::new(radius * 2.0, radius * 2.0),
    );
    context.fill_ellipse_in_rect(rect);
}

extern "C" fn draw_rect(this: &Object, _sel: Sel, dirty_rect: NSRect) {
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

        // Cửa sổ trong suốt: tự xoá sạch đúng vùng sắp vẽ trước khi vẽ frame mới.
        // Vùng này đã gộp nội dung frame trước, nên pixel cũ không thể sót lại
        // thành "vệt dính" khi rê chuột nhanh.
        if let Some(context) = current_cg_context_ref() {
            CGContextClearRect(
                context,
                CGRect::new(
                    &CGPoint::new(dirty_rect.origin.x, dirty_rect.origin.y),
                    &CGSize::new(
                        dirty_rect.size.width.max(1.0),
                        dirty_rect.size.height.max(1.0),
                    ),
                ),
            );
        }

        let scene = match get_scene().lock() {
            Ok(scene) => scene,
            Err(poisoned) => poisoned.into_inner(),
        };

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

        // Dải sáng neon bám theo con trỏ
        if EFFECT.load(Ordering::Relaxed) == EFFECT_NEON
            && CURSOR_VISIBLE.load(Ordering::Relaxed)
            && scene.trail.len() > 2
        {
            let count = scene.trail.len();
            let mut points: Vec<(f64, f64)> = Vec::with_capacity(count);
            for point in scene.trail.iter() {
                points.push((point.x - offset_x, point.y - offset_y));
            }

            // Làm mượt nhẹ đường đi để hai biên của dải không bị gợn
            let mut smooth: Vec<(f64, f64)> = Vec::with_capacity(count);
            for i in 0..count {
                let a = points[i.saturating_sub(1)];
                let b = points[i];
                let c = points[(i + 1).min(count - 1)];
                smooth.push(((a.0 + 2.0 * b.0 + c.0) / 4.0, (a.1 + 2.0 * b.1 + c.1) / 4.0));
            }

            if SKIN.load(Ordering::Relaxed) == SKIN_PIXEL_LOVE {
                draw_pixel_trail(&smooth);
            } else {
                let (r, g, b) = neon_color(scene.hue_phase);
                // Nhiều lớp, mỗi lớp là MỘT path kín: quầng -> thân -> ruột trắng
                fill_ribbon(&smooth, 48.0, r, g, b, 0.07);
                fill_ribbon(&smooth, 30.0, r, g, b, 0.14);
                fill_ribbon(&smooth, 20.0, r, g, b, 0.78);
                fill_ribbon(&smooth, 8.0, 1.0, 1.0, 1.0, 0.55);

                // Đầu dải sáng: quầng neon nhấp nháy tại con trỏ
                if let Some((hx, hy)) = smooth.last().copied() {
                    let pulse =
                        0.55 + 0.45 * ((scene.hue_phase * TAU * 3.0).sin() * 0.5 + 0.5);
                    fill_oval(hx, hy, 30.0, r, g, b, 0.05 * pulse);
                    fill_oval(hx, hy, 15.0, r, g, b, 0.12 * pulse);
                    fill_oval(hx, hy, 7.0, 1.0, 1.0, 1.0, 0.6 * pulse);
                }
            }
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
        // (view, offset_x, offset_y) của từng cửa sổ overlay
        let mut views: Vec<(usize, f64, f64)> = Vec::new();
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
            views.push((view as usize, frame.origin.x, frame.origin.y));
        }

        eprintln!("Overlay đã tạo cho {} màn hình.", views.len());

        // Tiếng click, hiệu ứng, panel settings và icon trên menu bar
        load_sound_preference();
        load_effect_preference();
        load_skin_preference();
        load_sounds();

        // Quả pháo chào ngay khi mở app (chỉ ở chế độ Fireworks)
        if EFFECT.load(Ordering::Relaxed) == EFFECT_FIREWORKS {
            spawn_firework(
                main_display_bounds.size.width / 2.0,
                screen_height / 2.0,
            );
        }
        let controller: id = msg_send![register_controller_class(), new];
        let _ = SETTINGS_WINDOW.set(build_settings_window(controller) as usize);
        build_status_item(controller);

        if trusted {
        thread::spawn(move || {
            let tap = CGEventTap::new(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![
                    CGEventType::LeftMouseDown,
                    CGEventType::MouseMoved,
                    CGEventType::LeftMouseDragged,
                ],
                move |_proxy, etype, event| {
                    match etype {
                        CGEventType::MouseMoved | CGEventType::LeftMouseDragged => {
                            // Ghi mẫu vị trí thật để dải neon bám đúng đường đi
                            if EFFECT.load(Ordering::Relaxed) == EFFECT_NEON {
                                let loc = event.location();
                                push_mouse_sample(
                                    loc.x,
                                    screen_height - loc.y,
                                    Instant::now(),
                                );
                            }
                        }
                        _ => {
                            let loc = event.location();
                            let cocoa_y = screen_height - loc.y;
                            if EFFECT.load(Ordering::Relaxed) == EFFECT_FIREWORKS {
                                spawn_firework(loc.x, cocoa_y);
                            }
                            // Phát tiếng ở main thread (trong timer) để không đụng AppKit từ thread này
                            SOUND_PENDING.store(true, Ordering::Relaxed);
                        }
                    }
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
            tick();

            let (dirty, idle) = match get_scene().lock() {
                Ok(scene) => (scene.dirty_bounds, scene.idle_secs > 0.12),
                Err(poisoned) => {
                    let scene = poisoned.into_inner();
                    (scene.dirty_bounds, scene.idle_secs > 0.12)
                }
            };

            // Chuột đứng yên thì chỉ cần 30 fps cho các đốm lấp lánh -> nhẹ CPU
            static PARITY: AtomicUsize = AtomicUsize::new(0);
            let skip_frame = idle && PARITY.fetch_add(1, Ordering::Relaxed) % 2 == 1;
            if skip_frame {
                return;
            }

            let full_refresh = match get_scene().lock() {
                Ok(mut scene) => {
                    let full = scene.full_refresh;
                    scene.full_refresh = false;
                    full
                }
                Err(poisoned) => {
                    let mut scene = poisoned.into_inner();
                    let full = scene.full_refresh;
                    scene.full_refresh = false;
                    full
                }
            };

            if full_refresh {
                for (view, _, _) in &views {
                    let v = *view as id;
                    let _: () = msg_send![v, setNeedsDisplay: YES];
                }
                return;
            }

            // Chỉ vẽ lại vùng có hiệu ứng (dải sáng + quầng + hạt). `dirty_bounds`
            // đã gộp vùng của 45 frame gần nhất nên pixel cũ cũng được xoá theo.
            if let Some((x0, y0, x1, y1)) = dirty {
                let size = NSSize::new((x1 - x0).max(1.0), (y1 - y0).max(1.0));
                for (view, offset_x, offset_y) in &views {
                    let v = *view as id;
                    let rect =
                        NSRect::new(NSPoint::new(x0 - *offset_x, y0 - *offset_y), size);
                    let _: () = msg_send![v, setNeedsDisplayInRect: rect];
                }
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
