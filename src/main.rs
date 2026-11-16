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

mod click;

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
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    /// Bật lại event tap sau khi macOS tự ngắt nó (callback chậm quá thời gian cho phép).
    fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
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
/// Click do event tap (chạy ở thread riêng) ghi lại, main thread sẽ vẽ hiệu ứng.
/// AppKit/Core Image không được dùng từ thread của event tap.
static PENDING_CLICKS: Mutex<Vec<(f64, f64)>> = Mutex::new(Vec::new());
/// Pool phát theo từng loại âm thanh, **nạp khi cần**: mỗi loại có 4 bản NSSound
/// để bấm liên tiếp không cắt nhau. `None` = loại đó chưa được nạp.
static SOUND_POOLS: Mutex<Vec<Option<Vec<usize>>>> = Mutex::new(Vec::new());
/// UID loa trong của máy (tính một lần, dùng lại cho mọi NSSound)
static SOUND_DEVICE_UID: OnceLock<Option<usize>> = OnceLock::new();
/// Loại âm thanh đang chọn (chỉ số trong SOUND_KINDS)
static SOUND_KIND: AtomicU8 = AtomicU8::new(0);

/// Tên file trong bundle, thứ tự này cũng là thứ tự trong popup chọn âm thanh.
/// 5 mục đầu giữ nguyên thứ tự cũ để cài đặt đã lưu không bị lệch.
const SOUND_KINDS: [&str; 22] = [
    "click",
    "pop",
    "chime",
    "firework",
    "boom",
    "click-wooden",
    "button-tap",
    "bubble-pop",
    "bubble-pop-2",
    "rock-on-wood",
    "squeak-toy",
    "duck-quack",
    "menu-beep",
    "notification",
    "pluck",
    "gameboy-pluck",
    "coin",
    "coin-received",
    "drop-coin",
    "game-bonus",
    "water-droplet",
    "firework-2",
];

const SOUND_LABELS: [&str; 22] = [
    "Click",
    "Pop",
    "Chime",
    "Firework",
    "Boom",
    "Wooden click",
    "Button tap",
    "Bubble pop",
    "Bubble pop 2",
    "Rock on wood",
    "Squeak toy",
    "Duck quack",
    "Menu beep",
    "Notification",
    "Pluck",
    "Gameboy pluck",
    "Coin",
    "Coin received",
    "Drop coin",
    "Game bonus",
    "Water droplet",
    "Firework 2",
];
static SOUND_NEXT: AtomicUsize = AtomicUsize::new(0);
static SOUND_VOLUME_BITS: AtomicU32 = AtomicU32::new(0.45f32.to_bits());

// ---------- Hiệu ứng ----------

/// Hiệu ứng khi click: pháo hoa, thấu kính nước, hoặc tắt hẳn (chọn một)
const CLICK_FIREWORKS: u8 = 0;
const CLICK_OFF: u8 = 2;
/// Giữ giá trị 3 như bản trước để `defaults` đã lưu không bị đổi nghĩa.
const CLICK_METAL: u8 = 3;

/// Các mục của popup "Click effects": (giá trị lưu, tên hiển thị).
/// Giá trị giữ nguyên để không lệch với dữ liệu đã lưu (0 = fireworks, 2 = off,
/// 3 = metal; 1 là "ripple" đã bỏ nên giờ rơi về fireworks).
const CLICK_MENU: [(u8, &str); 3] = [
    (CLICK_FIREWORKS, "Fireworks"),
    (CLICK_METAL, "Metal warp"),
    (CLICK_OFF, "Off"),
];
static CLICK_EFFECT: AtomicU8 = AtomicU8::new(CLICK_FIREWORKS);
/// Neon trail bật/tắt độc lập, có thể chạy cùng hiệu ứng click
static NEON_ON: AtomicBool = AtomicBool::new(false);

// ---------- Skin của dải neon ----------

const SKIN_CLASSIC: u8 = 0;
const SKIN_PIXEL_LOVE: u8 = 1;
const SKIN_CALLIGRAPHY: u8 = 2;
const SKIN_BAT: u8 = 3;
const SKIN_NORTHERN_LIGHTS: u8 = 4;

static SKIN: AtomicU8 = AtomicU8::new(SKIN_CLASSIC);
/// Con trỏ có đang được hiển thị không (video fullscreen sẽ ẩn con trỏ)
static CURSOR_VISIBLE: AtomicBool = AtomicBool::new(true);
/// Hiệu ứng đang bị ẩn (con trỏ ẩn, hoặc chuột im quá lâu nên macOS đã tự ẩn con trỏ)
static EFFECT_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Chuột đứng yên quá lâu thì coi như con trỏ đã bị hệ thống ẩn (macOS ẩn con trỏ
/// sau vài giây khi xem video fullscreen). Hiệu ứng phải ẩn theo con trỏ.
const IDLE_HIDE_SECS: f64 = 5.0;

/// Mẫu vị trí chuột (toạ độ Cocoa) do CGEventTap ghi lại, kèm thời điểm nhận.
/// Tap bắt được mọi event chuột nên mẫu dày hơn nhiều so với 1 mẫu/frame.
static MOUSE_SAMPLES: Mutex<Vec<MouseSample>> = Mutex::new(Vec::new());
/// Mach port của event tap, để bật lại khi macOS tự ngắt (0 = chưa có)
static EVENT_TAP_PORT: AtomicUsize = AtomicUsize::new(0);

const MAX_MOUSE_SAMPLES: usize = 512;

// ---------- Menu bar + panel settings ----------

static SETTINGS_WINDOW: OnceLock<usize> = OnceLock::new();
/// (window number, frame) của các cửa sổ overlay — dùng để chụp màn hình nằm dưới
/// đúng cửa sổ overlay đang chứa điểm click.
static OVERLAY_WINDOWS: OnceLock<Vec<(u32, CGRect)>> = OnceLock::new();
pub(crate) fn overlay_windows() -> Option<&'static [(u32, CGRect)]> {
    OVERLAY_WINDOWS.get().map(|v| v.as_slice())
}

static STATUS_ITEM: OnceLock<usize> = OnceLock::new();
static VOLUME_LABEL: OnceLock<usize> = OnceLock::new();
static VOLUME_TITLE: OnceLock<usize> = OnceLock::new();
static VOLUME_SLIDER: OnceLock<usize> = OnceLock::new();
static SOUND_LABEL: OnceLock<usize> = OnceLock::new();
static SOUND_POPUP: OnceLock<usize> = OnceLock::new();
static SOUND_CHECKBOX: OnceLock<usize> = OnceLock::new();
static SKIN_LABEL: OnceLock<usize> = OnceLock::new();
static SKIN_POPUP: OnceLock<usize> = OnceLock::new();

pub(crate) fn nsstring(s: &str) -> id {
    unsafe { NSString::alloc(nil).init_str(s) }
}

/// Ẩn/hiện cụm chọn âm thanh + âm lượng: chỉ hiện khi "Play sound on click" bật.
unsafe fn set_sound_rows_hidden(hidden: bool) {
    let flag = if hidden { YES } else { NO };
    for slot in [
        &SOUND_LABEL,
        &SOUND_POPUP,
        &VOLUME_TITLE,
        &VOLUME_LABEL,
        &VOLUME_SLIDER,
    ] {
        if let Some(view) = slot.get() {
            let _: () = msg_send![*view as id, setHidden: flag];
        }
    }
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

fn load_effect_preferences() {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];

        let click_key = nsstring("ClickEffect");
        let click: id = msg_send![defaults, objectForKey: click_key];
        if click != nil {
            let value: i64 = msg_send![defaults, integerForKey: click_key];
            let effect = match value {
                2 => CLICK_OFF,
                3 => CLICK_METAL,
                _ => CLICK_FIREWORKS,
            };
            CLICK_EFFECT.store(effect, Ordering::Relaxed);
        } else {
            // Bản cũ dùng checkbox "Fireworks on click" -> FireworksEnabled
            let legacy_key = nsstring("FireworksEnabled");
            let legacy: id = msg_send![defaults, objectForKey: legacy_key];
            if legacy != nil {
                let enabled: bool = msg_send![defaults, boolForKey: legacy_key];
                let effect = if enabled {
                    CLICK_FIREWORKS
                } else {
                    CLICK_OFF
                };
                CLICK_EFFECT.store(effect, Ordering::Relaxed);
            }
        }

        let neon_key = nsstring("NeonEnabled");
        let neon: id = msg_send![defaults, objectForKey: neon_key];
        if neon != nil {
            let neon_on: bool = msg_send![defaults, boolForKey: neon_key];
            NEON_ON.store(neon_on, Ordering::Relaxed);
            return;
        }

        // Bản cũ lưu một key "Effect" duy nhất (0 = fireworks, 1 = neon)
        let legacy_key = nsstring("Effect");
        let legacy: id = msg_send![defaults, objectForKey: legacy_key];
        if legacy != nil {
            let value: i64 = msg_send![defaults, integerForKey: legacy_key];
            NEON_ON.store(value == 1, Ordering::Relaxed);
        }
    }
}

fn click_effect() -> u8 {
    CLICK_EFFECT.load(Ordering::Relaxed)
}

/// Vị trí của hiệu ứng đang chọn trong popup (xem `CLICK_MENU`).
fn click_effect_index() -> i64 {
    let current = click_effect();
    CLICK_MENU
        .iter()
        .position(|(value, _)| *value == current)
        .unwrap_or(0) as i64
}

fn neon_on() -> bool {
    NEON_ON.load(Ordering::Relaxed)
}

fn load_skin_preference() {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("Skin");
        let stored: id = msg_send![defaults, objectForKey: key];
        if stored != nil {
            let value: i64 = msg_send![defaults, integerForKey: key];
            let skin = match value {
                1 => SKIN_PIXEL_LOVE,
                2 => SKIN_CALLIGRAPHY,
                3 => SKIN_BAT,
                4 => SKIN_NORTHERN_LIGHTS,
                _ => SKIN_CLASSIC,
            };
            SKIN.store(skin, Ordering::Relaxed);
        }
    }
}

/// Đọc loại âm thanh đã chọn lần trước (key "Sound")
fn load_sound_kind_preference() {
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("Sound");
        let stored: id = msg_send![defaults, objectForKey: key];
        if stored != nil {
            let value: i64 = msg_send![defaults, integerForKey: key];
            let kind = value.clamp(0, SOUND_KINDS.len() as i64 - 1) as u8;
            SOUND_KIND.store(kind, Ordering::Relaxed);
        }
    }
}

fn sound_index() -> i64 {
    SOUND_KIND.load(Ordering::Relaxed) as i64
}

fn skin_index() -> i64 {
    match SKIN.load(Ordering::Relaxed) {
        SKIN_PIXEL_LOVE => 1,
        SKIN_CALLIGRAPHY => 2,
        SKIN_BAT => 3,
        SKIN_NORTHERN_LIGHTS => 4,
        _ => 0,
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
/// UID loa trong, tính một lần rồi dùng lại (tránh hỏi CoreAudio mỗi lần nạp pool).
fn sound_device_uid() -> Option<usize> {
    *SOUND_DEVICE_UID.get_or_init(|| unsafe {
        // Ghim phát ra loa trong của máy. Nếu để NSSound tự chọn, nó đi theo đường
        // "alert sound" và có thể ra cả Bluetooth/AirPlay — mỗi lần đánh thức thiết bị
        // ngoài là một lần macOS hiện HUD âm lượng.
        let uid = match builtin_output_uid() {
            Some(uid) => Some(uid),
            None => {
                eprintln!("Không tìm thấy loa trong của máy: phát ra thiết bị ra mặc định.");
                default_output_device_uid()
            }
        };
        if uid.is_none() {
            eprintln!("Không đọc được thiết bị âm thanh: dùng đường phát mặc định.");
        }
        uid.map(|uid| uid as usize)
    })
}

/// Nạp 4 bản NSSound cho một loại âm thanh.
unsafe fn load_sound_pool(kind: usize) -> Option<Vec<usize>> {
    let name = SOUND_KINDS[kind];
    let bundle: id = msg_send![class!(NSBundle), mainBundle];
    let path: id = msg_send![
        bundle,
        pathForResource: nsstring(name)
        ofType: nsstring("wav")
    ];
    if path == nil {
        eprintln!("Không thấy {name}.wav trong bundle.");
        return None;
    }

    let device_uid = sound_device_uid();
    let mut pool = Vec::with_capacity(4);
    for _ in 0..4 {
        let sound: id = msg_send![class!(NSSound), alloc];
        let sound: id = msg_send![sound, initWithContentsOfFile: path byReference: YES];
        if sound != nil {
            let _: () = msg_send![sound, setVolume: sound_volume()];
            if let Some(uid) = device_uid {
                let _: () = msg_send![sound, setPlaybackDeviceIdentifier: uid as id];
            }
            pool.push(sound as usize);
        }
    }
    Some(pool)
}

/// Bảo đảm pool của loại `kind` đã nạp (gọi lần đầu khi cần phát).
fn ensure_sound_pool(kind: usize) {
    let mut pools = match SOUND_POOLS.lock() {
        Ok(pools) => pools,
        Err(poisoned) => poisoned.into_inner(),
    };
    if pools.len() < SOUND_KINDS.len() {
        pools.resize(SOUND_KINDS.len(), None);
    }
    if pools[kind].is_none() {
        pools[kind] = unsafe { load_sound_pool(kind) };
    }
}

/// Chỉ nạp sẵn pool của loại đang chọn (4 NSSound) — không nạp cả 22 loại lúc khởi động.
fn preload_sound_kind() {
    ensure_sound_pool(SOUND_KIND.load(Ordering::Relaxed) as usize);
}

fn play_click_sound() {
    if !sound_enabled() {
        return;
    }
    let kind = SOUND_KIND.load(Ordering::Relaxed) as usize;
    ensure_sound_pool(kind);

    let pools = match SOUND_POOLS.lock() {
        Ok(pools) => pools,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(pool) = pools.get(kind).and_then(|pool| pool.as_ref()) else {
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

        // Sound + volume chỉ hiện khi "Play sound on click" đang bật
        set_sound_rows_hidden(!enabled);

        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("SoundEnabled");
        let _: () = msg_send![defaults, setBool: enabled forKey: key];
    }
}

extern "C" fn change_volume(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let value: f64 = msg_send![sender, doubleValue];
        let volume = set_sound_volume(value as f32);
        // Chỉ chỉnh những pool đang có (các loại chưa dùng thì chưa nạp)
        if let Ok(pools) = SOUND_POOLS.lock() {
            for sound in pools.iter().flatten().flatten() {
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

extern "C" fn change_click_effect(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let index: i64 = msg_send![sender, indexOfSelectedItem];
        let effect = CLICK_MENU
            .get(index.max(0) as usize)
            .map(|(value, _)| *value)
            .unwrap_or(CLICK_FIREWORKS);
        CLICK_EFFECT.store(effect, Ordering::Relaxed);

        // Đổi hiệu ứng (hoặc tắt hẳn) thì dọn cảnh cũ cho khỏi "dính" lại
        click::clear();
        if let Ok(mut scene) = get_scene().lock() {
            scene.particles.clear();
            update_dirty_bounds(&mut scene);
        }

        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("ClickEffect");
        let _: () = msg_send![defaults, setInteger: effect as i64 forKey: key];
    }
}

extern "C" fn toggle_neon(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let state: i64 = msg_send![sender, state];
        let enabled = state != 0;
        NEON_ON.store(enabled, Ordering::Relaxed);

        // Skin chỉ có ý nghĩa với neon trail -> chỉ hiện ô chọn skin khi neon bật
        let hidden = if enabled { NO } else { YES };
        if let Some(label) = SKIN_LABEL.get() {
            let _: () = msg_send![*label as id, setHidden: hidden];
        }
        if let Some(popup) = SKIN_POPUP.get() {
            let _: () = msg_send![*popup as id, setHidden: hidden];
        }

        // Tắt neon thì xoá dải sáng (và đốm lấp lánh của neon) ngay
        if let Ok(mut scene) = get_scene().lock() {
            scene.trail.clear();
            scene.head_init = false;
            scene.idle_secs = 0.0;
            if !enabled {
                update_dirty_bounds(&mut scene);
            }
        }

        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("NeonEnabled");
        let _: () = msg_send![defaults, setBool: enabled forKey: key];
    }
}

extern "C" fn change_sound(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let index: i64 = msg_send![sender, indexOfSelectedItem];
        let kind = index.clamp(0, SOUND_KINDS.len() as i64 - 1) as u8;
        SOUND_KIND.store(kind, Ordering::Relaxed);

        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = nsstring("Sound");
        let _: () = msg_send![defaults, setInteger: kind as i64 forKey: key];

        // Nghe thử luôn để biết đã chọn đúng âm thanh nào
        play_click_sound();
    }
}

extern "C" fn change_skin(_this: &Object, _sel: Sel, sender: id) {
    unsafe {
        let index: i64 = msg_send![sender, indexOfSelectedItem];
        let skin = match index {
            1 => SKIN_PIXEL_LOVE,
            2 => SKIN_CALLIGRAPHY,
            3 => SKIN_BAT,
            4 => SKIN_NORTHERN_LIGHTS,
            _ => SKIN_CLASSIC,
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
            sel!(changeClickEffect:),
            change_click_effect as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(toggleNeon:),
            toggle_neon as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(changeSound:),
            change_sound as extern "C" fn(&Object, Sel, id),
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
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 346.0));
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
    let _: () = msg_send![title, setFrame: NSRect::new(NSPoint::new(20.0, 300.0), NSSize::new(280.0, 20.0))];
    let _: () = msg_send![content, addSubview: title];

    // ================= Neon trail =================
    let neon_box: id = msg_send![
        class!(NSButton),
        checkboxWithTitle: nsstring("Neon trail follows cursor")
        target: controller
        action: sel!(toggleNeon:)
    ];
    let state: i64 = if neon_on() { 1 } else { 0 };
    let _: () = msg_send![neon_box, setState: state];
    let _: () = msg_send![neon_box, setFrame: NSRect::new(NSPoint::new(20.0, 268.0), NSSize::new(280.0, 22.0))];
    let _: () = msg_send![content, addSubview: neon_box];

    // Skin phụ thuộc neon: ngay dưới, thụt vào
    let skin_label: id = msg_send![class!(NSTextField), labelWithString: nsstring("Skin")];
    let _: () = msg_send![skin_label, setFrame: NSRect::new(NSPoint::new(36.0, 240.0), NSSize::new(60.0, 18.0))];
    let _: () = msg_send![content, addSubview: skin_label];

    let skin_popup: id = msg_send![class!(NSPopUpButton), alloc];
    let skin_popup: id = msg_send![
        skin_popup,
        initWithFrame: NSRect::new(NSPoint::new(100.0, 234.0), NSSize::new(200.0, 26.0))
        pullsDown: NO
    ];
    let _: () = msg_send![skin_popup, addItemWithTitle: nsstring("Classic")];
    let _: () = msg_send![skin_popup, addItemWithTitle: nsstring("Pixel love")];
    let _: () = msg_send![skin_popup, addItemWithTitle: nsstring("Calligraphy brush")];
    let _: () = msg_send![skin_popup, addItemWithTitle: nsstring("Bat blade")];
    let _: () = msg_send![skin_popup, addItemWithTitle: nsstring("Northern lights")];
    let _: () = msg_send![skin_popup, selectItemAtIndex: skin_index()];
    let _: () = msg_send![skin_popup, setTarget: controller];
    let _: () = msg_send![skin_popup, setAction: sel!(changeSkin:)];
    let _: () = msg_send![content, addSubview: skin_popup];

    // Skin chỉ hiện khi neon trail đang bật
    let hidden = if neon_on() { NO } else { YES };
    let _: () = msg_send![skin_label, setHidden: hidden];
    let _: () = msg_send![skin_popup, setHidden: hidden];
    let _ = SKIN_LABEL.set(skin_label as usize);
    let _ = SKIN_POPUP.set(skin_popup as usize);

    // ================= Click effects: pháo hoa hay sóng nước =================
    let click_label: id = msg_send![class!(NSTextField), labelWithString: nsstring("Click effects")];
    let _: () = msg_send![click_label, setFrame: NSRect::new(NSPoint::new(36.0, 204.0), NSSize::new(90.0, 18.0))];
    let _: () = msg_send![content, addSubview: click_label];

    let click_popup: id = msg_send![class!(NSPopUpButton), alloc];
    let click_popup: id = msg_send![
        click_popup,
        initWithFrame: NSRect::new(NSPoint::new(130.0, 198.0), NSSize::new(170.0, 26.0))
        pullsDown: NO
    ];
    for (_, title) in CLICK_MENU.iter() {
        let _: () = msg_send![click_popup, addItemWithTitle: nsstring(title)];
    }
    let _: () = msg_send![click_popup, selectItemAtIndex: click_effect_index()];
    let _: () = msg_send![click_popup, setTarget: controller];
    let _: () = msg_send![click_popup, setAction: sel!(changeClickEffect:)];
    let _: () = msg_send![content, addSubview: click_popup];

    // ================= Sound (ngang hàng với Fireworks) =================
    let checkbox: id = msg_send![
        class!(NSButton),
        checkboxWithTitle: nsstring("Play sound on click")
        target: controller
        action: sel!(toggleSound:)
    ];
    let state: i64 = if sound_enabled() { 1 } else { 0 };
    let _: () = msg_send![checkbox, setState: state];
    let _: () = msg_send![checkbox, setFrame: NSRect::new(NSPoint::new(20.0, 176.0), NSSize::new(280.0, 22.0))];
    let _: () = msg_send![content, addSubview: checkbox];
    let _ = SOUND_CHECKBOX.set(checkbox as usize);

    // Sound + Volume phụ thuộc "Play sound on click": thụt vào
    let sound_label: id = msg_send![class!(NSTextField), labelWithString: nsstring("Sound")];
    let _: () = msg_send![sound_label, setFrame: NSRect::new(NSPoint::new(36.0, 148.0), NSSize::new(60.0, 18.0))];
    let _: () = msg_send![content, addSubview: sound_label];

    let sound_popup: id = msg_send![class!(NSPopUpButton), alloc];
    let sound_popup: id = msg_send![
        sound_popup,
        initWithFrame: NSRect::new(NSPoint::new(100.0, 142.0), NSSize::new(200.0, 26.0))
        pullsDown: NO
    ];
    for label in SOUND_LABELS {
        let _: () = msg_send![sound_popup, addItemWithTitle: nsstring(label)];
    }
    let _: () = msg_send![sound_popup, selectItemAtIndex: sound_index()];
    let _: () = msg_send![sound_popup, setTarget: controller];
    let _: () = msg_send![sound_popup, setAction: sel!(changeSound:)];
    let _: () = msg_send![content, addSubview: sound_popup];
    let _ = SOUND_LABEL.set(sound_label as usize);
    let _ = SOUND_POPUP.set(sound_popup as usize);

    let volume_label: id = msg_send![class!(NSTextField), labelWithString: nsstring("Volume")];
    let _: () = msg_send![volume_label, setFrame: NSRect::new(NSPoint::new(36.0, 114.0), NSSize::new(120.0, 18.0))];
    let _: () = msg_send![content, addSubview: volume_label];
    let _ = VOLUME_TITLE.set(volume_label as usize);

    let percent_text = format!("{}%", (sound_volume() * 100.0).round() as i32);
    let percent: id = msg_send![class!(NSTextField), labelWithString: nsstring(&percent_text)];
    let percent_font: id = msg_send![class!(NSFont), monospacedDigitSystemFontOfSize: 12.0f64 weight: 0.0f64];
    let _: () = msg_send![percent, setFont: percent_font];
    let _: () = msg_send![percent, setAlignment: 2i64];
    let percent_color: id = msg_send![class!(NSColor), secondaryLabelColor];
    let _: () = msg_send![percent, setTextColor: percent_color];
    let _: () = msg_send![percent, setFrame: NSRect::new(NSPoint::new(240.0, 114.0), NSSize::new(60.0, 18.0))];
    let _: () = msg_send![content, addSubview: percent];
    let _ = VOLUME_LABEL.set(percent as usize);

    let slider: id = msg_send![class!(NSSlider), alloc];
    let slider: id = msg_send![slider, initWithFrame: NSRect::new(NSPoint::new(36.0, 88.0), NSSize::new(264.0, 24.0))];
    let _: () = msg_send![slider, setMinValue: 0.0f64];
    let _: () = msg_send![slider, setMaxValue: 1.0f64];
    let _: () = msg_send![slider, setDoubleValue: sound_volume() as f64];
    let _: () = msg_send![slider, setContinuous: YES];
    let _: () = msg_send![slider, setTarget: controller];
    let _: () = msg_send![slider, setAction: sel!(changeVolume:)];
    let _: () = msg_send![content, addSubview: slider];
    let _ = VOLUME_SLIDER.set(slider as usize);

    // Sound + volume chỉ hiện khi "Play sound on click" đang bật
    set_sound_rows_hidden(!sound_enabled());

    // ================= Chân cửa sổ =================
    let hint: id = msg_send![class!(NSTextField), labelWithString: nsstring("Both effects can be enabled at the same time.")];
    let _: () = msg_send![hint, setFrame: NSRect::new(NSPoint::new(20.0, 56.0), NSSize::new(280.0, 18.0))];
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

/// Trạng thái vẽ của overlay: hạt pháo hoa, dải neon và vùng cần vẽ lại.
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
    /// Vị trí con trỏ ở frame trước: dùng làm tín hiệu "chuột đang di chuyển" khi
    /// event tap không ghi được mẫu nào.
    last_cursor: Option<(f64, f64)>,
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
            last_cursor: None,
        })
    })
}

fn screen_capture_granted() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

fn request_screen_capture_access() {
    unsafe {
        let _ = CGRequestScreenCaptureAccess();
    }
    eprintln!(
        "Hiệu ứng cần quyền Screen Recording để phản chiếu hình phía sau:\n  \
         System Settings > Privacy & Security > Screen Recording > bật Mac Fireworks, rồi mở lại app."
    );
}

struct TrailPoint {
    x: f64,
    y: f64,
    life: f64,
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
    // Vùng của các hiệu ứng click (sóng nước, warp, hybrid, ...)
    if let Some((x0, y0, x1, y1)) = click::bounds() {
        let cx = (x0 + x1) / 2.0;
        let cy = (y0 + y1) / 2.0;
        let radius = ((x1 - x0).max(y1 - y0) / 2.0) + 2.0;
        include(cx, cy, radius);
    }
    if neon_on() && !EFFECT_HIDDEN.load(Ordering::Relaxed) {
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

/// Vẽ một lớp của nét cọ: dựng dải kín từ trục các điểm, bề rộng do `half_width`
/// quyết định — bề rộng không đều nên mép trông như vết cọ thật.
unsafe fn fill_brush_layer<F>(
    context: &CGContext,
    points: &[(f64, f64)],
    half_width: F,
    r: f64,
    g: f64,
    b: f64,
    alpha: f64,
) where
    F: Fn(usize, f64) -> f64,
{
    let count = points.len();
    if count < 2 {
        return;
    }

    context.begin_path();
    for i in 0..count {
        let (nx, ny) = ribbon_normal(points, i);
        let t = i as f64 / (count - 1) as f64;
        let hw = half_width(i, t).max(0.0);
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
        let t = i as f64 / (count - 1) as f64;
        let hw = half_width(i, t).max(0.0);
        context.add_line_to_point(points[i].0 - nx * hw, points[i].1 - ny * hw);
    }
    context.close_path();
    context.set_rgb_fill_color(r, g, b, alpha);
    context.fill_path();
}

/// Nhiễu tất định (0..1) theo chỉ số điểm — mép cọ nhấp nhô mà vẫn ổn định.
fn brush_noise(i: usize) -> f64 {
    let v = (i as f64 * 12.9898 + 78.233).sin() * 43758.5453;
    (v - v.floor()).abs()
}

/// Skin "Calligraphy brush": nét mực đen loang, mép không đều, đầu cọ đậm, kèm
/// vài đốm mực bắn ra hai bên như bút lông thật.
unsafe fn draw_brush_trail(points: &[(f64, f64)]) {
    let count = points.len();
    if count < 2 {
        return;
    }
    let Some(context) = current_cg_context() else {
        return;
    };

    let profile = |i: usize, t: f64| -> f64 {
        // dày dần về phía đầu, mép nhấp nhô theo nhiễu tất định
        (2.2 + 8.5 * t) * (0.72 + 0.45 * brush_noise(i))
    };

    // Viền sáng rất nhẹ để nét mực vẫn thấy trên nền tối
    fill_brush_layer(
        &context,
        points,
        |i, t| profile(i, t) * 1.7,
        0.92,
        0.92,
        0.96,
        0.08,
    );

    // Quầng mực loang -> thân mực -> lõi đậm
    fill_brush_layer(
        &context,
        points,
        |i, t| profile(i, t) * 1.35,
        0.13,
        0.13,
        0.15,
        0.12,
    );
    fill_brush_layer(&context, points, |i, t| profile(i, t), 0.09, 0.09, 0.11, 0.85);
    fill_brush_layer(
        &context,
        points,
        |i, t| profile(i, t) * 0.45,
        0.03,
        0.03,
        0.04,
        0.55,
    );

    // Đầu cọ: một chấm mực đậm ngay tại con trỏ
    if let Some((hx, hy)) = points.last().copied() {
        context.set_rgb_fill_color(0.04, 0.04, 0.05, 0.85);
        context.fill_ellipse_in_rect(CGRect::new(
            &CGPoint::new(hx - 3.2, hy - 3.2),
            &CGSize::new(6.4, 6.4),
        ));
    }

    // Mực bắn hai bên nét
    let mut index = 6;
    while index < count {
        let n = brush_noise(index * 7 + 3);
        if n > 0.42 {
            let (nx, ny) = ribbon_normal(points, index);
            let side = if n > 0.72 { 1.0 } else { -1.0 };
            let offset = 6.0 + n * 15.0;
            let radius = 0.7 + n * 1.7;
            let x = points[index].0 + nx * offset * side;
            let y = points[index].1 + ny * offset * side;
            context.set_rgb_fill_color(0.06, 0.06, 0.08, 0.5);
            context.fill_ellipse_in_rect(CGRect::new(
                &CGPoint::new(x - radius, y - radius),
                &CGSize::new(radius * 2.0, radius * 2.0),
            ));
        }
        index += 9;
    }
}

/// Thêm silhouette một con dơi vào path hiện tại (toạ độ cục bộ, xoay theo hướng bay).
fn add_bat_path(context: &CGContext, cx: f64, cy: f64, angle: f64, size: f64, flap: f64) {
    let (sin_a, cos_a) = angle.sin_cos();
    let wing_top = 2.2 - 1.9 * flap;
    let wing_tip = 1.4 - 2.4 * flap;
    // Cánh có ba mũi nhọn (mép sau răng cưa) đúng dáng dơi
    let shape: [(f64, f64); 16] = [
        (0.0, 2.6),
        (0.8, 1.6),
        (2.1, wing_top),
        (3.6, wing_tip),
        (2.7, 0.4),
        (3.1, -0.8),
        (1.7, -0.3),
        (1.1, -1.2),
        (0.5, -1.9),
        (-0.5, -1.9),
        (-1.1, -1.2),
        (-1.7, -0.3),
        (-3.1, -0.8),
        (-2.7, 0.4),
        (-3.6, wing_tip),
        (-2.1, wing_top),
    ];

    for (i, (x, y)) in shape.iter().enumerate() {
        let px = cx + (x * cos_a - y * sin_a) * size;
        let py = cy + (x * sin_a + y * cos_a) * size;
        if i == 0 {
            context.move_to_point(px, py);
        } else {
            context.add_line_to_point(px, py);
        }
    }
    context.close_path();
}

fn build_bat_paths(
    context: &CGContext,
    points: &[(f64, f64)],
    count: usize,
    indexes: &[usize],
    phase: f64,
) {
    context.begin_path();
    for &index in indexes {
        let t = index as f64 / (count - 1) as f64;
        let (nx, ny) = ribbon_normal(points, index);
        let angle = ny.atan2(nx) + TAU / 4.0;
        let flap = (phase + index as f64 * 0.5).sin();
        // lệch nhẹ hai bên để bầy dơi không xếp thành một hàng cứng
        let (nx, ny) = ribbon_normal(points, index);
        let sway = ((index as f64 * 1.7).sin()) * 3.5;
        add_bat_path(
            context,
            points[index].0 + nx * sway,
            points[index].1 + ny * sway,
            angle,
            2.0 + 1.6 * t,
            flap,
        );
    }
}

/// Skin "Bat blade": bầy dơi tím bay theo con trỏ, cánh đập theo nhịp.
unsafe fn draw_bat_trail(points: &[(f64, f64)], phase: f64) {
    let count = points.len();
    if count < 2 {
        return;
    }
    let Some(context) = current_cg_context() else {
        return;
    };

    const STRIDE: usize = 9;
    let mut buckets: [Vec<usize>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut index = STRIDE;
    while index < count {
        let t = index as f64 / (count - 1) as f64;
        let level = ((t * 2.99) as usize).min(2);
        buckets[level].push(index);
        index += STRIDE;
    }

    for (level, indexes) in buckets.iter().enumerate() {
        if indexes.is_empty() {
            continue;
        }
        let fill_alpha = 0.45 + 0.25 * level as f64;

        // Quầng tím -> viền tím -> thân dơi đen tím
        build_bat_paths(&context, points, count, indexes, phase);
        context.set_rgb_stroke_color(0.62, 0.35, 0.95, 0.12);
        context.set_line_width(7.0);
        context.stroke_path();

        build_bat_paths(&context, points, count, indexes, phase);
        context.set_rgb_stroke_color(0.78, 0.5, 1.0, 0.30);
        context.set_line_width(2.0);
        context.stroke_path();

        build_bat_paths(&context, points, count, indexes, phase);
        context.set_rgb_fill_color(0.13, 0.08, 0.2, fill_alpha);
        context.fill_path();
    }
}

/// Skin "Northern lights": nhiều dải cực quang chồng nhau (xanh lá -> teal ->
/// cyan -> tím), mép lượn sóng như rèm sáng, thêm các vệt shimmer dọc dải.
unsafe fn draw_aurora_trail(points: &[(f64, f64)], phase: f64) {
    let count = points.len();
    if count < 2 {
        return;
    }
    let Some(context) = current_cg_context() else {
        return;
    };

    // (r, g, b, tỉ lệ bề rộng, alpha)
    const LAYERS: [(f64, f64, f64, f64, f64); 4] = [
        (0.10, 1.00, 0.45, 3.0, 0.30),
        (0.10, 0.85, 0.65, 1.8, 0.40),
        (0.45, 0.35, 1.00, 1.0, 0.45),
        (0.60, 1.00, 0.85, 0.40, 0.70),
    ];

    for (layer, (r, g, b, scale, alpha)) in LAYERS.iter().enumerate() {
        let layer_phase = phase + layer as f64 * 1.3;
        fill_brush_layer(
            &context,
            points,
            |i, t| {
                let wave = 0.62 + 0.5 * (i as f64 * 0.26 + layer_phase).sin();
                (2.5 + 9.0 * t) * wave * scale * 0.5
            },
            *r,
            *g,
            *b,
            *alpha,
        );
    }

    // Vệt shimmer chạy dọc dải
    let mut index = 8;
    while index < count {
        let t = index as f64 / (count - 1) as f64;
        let shimmer = (phase * 2.0 + index as f64 * 0.6).sin();
        if shimmer > 0.55 {
            let (nx, ny) = ribbon_normal(points, index);
            let half = (2.5 + 9.0 * t) * 1.5;
            context.set_rgb_stroke_color(0.85, 1.0, 0.92, 0.22 * shimmer);
            context.set_line_width(2.0);
            context.begin_path();
            context.move_to_point(points[index].0 - ny * half, points[index].1 + nx * half);
            context.add_line_to_point(points[index].0 + ny * half, points[index].1 - nx * half);
            context.stroke_path();
        }
        index += 11;
    }
}

/// Mô phỏng một frame (gọi từ timer trên main thread, mỗi frame đúng một lần).
fn tick() {
    let now = Instant::now();

    // Click đến từ event tap: dựng hiệu ứng ở main thread (AppKit/CI an toàn thread)
    let clicks: Vec<(f64, f64)> = match PENDING_CLICKS.lock() {
        Ok(mut pending) => std::mem::take(&mut *pending),
        Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
    };
    for (x, y) in clicks {
        match click_effect() {
            CLICK_FIREWORKS => spawn_firework(x, y),
            effect => click::spawn(effect, x, y),
        }
    }

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

    // Tuổi của các hiệu ứng click (sóng nước, warp, bong bóng kính, hybrid)
    click::update(dt);

    if !neon_on() {
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
    if vis_frame % 4 == 0 {
        let visible = unsafe { CGCursorIsVisible() };
        CURSOR_VISIBLE.store(visible, Ordering::Relaxed);
    }

    if !CURSOR_VISIBLE.load(Ordering::Relaxed) {
        EFFECT_HIDDEN.store(true, Ordering::Relaxed);
        scene.trail.clear();
        scene.particles.clear();
        scene.head_init = false;
        update_dirty_bounds(&mut scene);
        return;
    }

    let cursor: NSPoint = unsafe { msg_send![class!(NSEvent), mouseLocation] };
    let cursor = (cursor.x, cursor.y);

    // Con trỏ đổi vị trí tức là vẫn có tương tác, kể cả khi event tap bị hệ thống
    // ngắt nên không có mẫu nào: nếu chỉ dựa vào mẫu, hiệu ứng đã ẩn sẽ nằm im
    // mãi mãi dù chuột vẫn di chuyển.
    let cursor_moved = match scene.last_cursor {
        Some((x, y)) => (x - cursor.0).abs() > 0.5 || (y - cursor.1).abs() > 0.5,
        None => true,
    };
    scene.last_cursor = Some(cursor);

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
    let moving = !samples.is_empty() || cursor_moved;
    if scene.was_moving && !moving {
        scene.full_refresh = true;
    }
    scene.was_moving = moving;

    if samples.is_empty() && !cursor_moved {
        scene.idle_secs += dt;
    } else {
        scene.idle_secs = 0.0;
    }

    // Chuột im quá lâu -> macOS ẩn con trỏ (thường gặp khi xem video fullscreen),
    // nên hiệu ứng cũng phải biến mất. `idle_secs` không bị reset ở đây để hiệu ứng
    // ở trạng thái ẩn cho tới khi chuột di chuyển lại.
    if scene.idle_secs > IDLE_HIDE_SECS {
        EFFECT_HIDDEN.store(true, Ordering::Relaxed);
        scene.trail.clear();
        scene.particles.clear();
        scene.head_init = false;
        update_dirty_bounds(&mut scene);
        return;
    }
    EFFECT_HIDDEN.store(false, Ordering::Relaxed);

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

pub(crate) fn current_cg_context_ref() -> Option<CGContextRef> {
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
pub(crate) fn current_cg_context() -> Option<CGContext> {
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

pub(crate) fn fill_oval(cx: f64, cy: f64, radius: f64, r: f64, g: f64, b: f64, alpha: f64) {
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

/// Vẽ một vòng tròn (nét) bằng Core Graphics.
pub(crate) fn stroke_circle(
    cx: f64,
    cy: f64,
    radius: f64,
    r: f64,
    g: f64,
    b: f64,
    alpha: f64,
    width: f64,
) {
    if radius <= 0.5 || alpha <= 0.002 {
        return;
    }
    let Some(context) = current_cg_context() else {
        return;
    };
    context.set_rgb_stroke_color(
        r.clamp(0.0, 1.0),
        g.clamp(0.0, 1.0),
        b.clamp(0.0, 1.0),
        alpha.clamp(0.0, 1.0),
    );
    context.set_line_width(width);
    context.stroke_ellipse_in_rect(CGRect::new(
        &CGPoint::new(cx - radius, cy - radius),
        &CGSize::new(radius * 2.0, radius * 2.0),
    ));
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

        // Hiệu ứng click (thấu kính nước) tự vẽ phần Core Graphics
        click::draw(&click::Frame { offset_x, offset_y });

        // Dải sáng neon bám theo con trỏ
        if neon_on()
            && !EFFECT_HIDDEN.load(Ordering::Relaxed)
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

            let skin = SKIN.load(Ordering::Relaxed);
            if skin == SKIN_PIXEL_LOVE {
                draw_pixel_trail(&smooth);
            } else if skin == SKIN_CALLIGRAPHY {
                draw_brush_trail(&smooth);
            } else if skin == SKIN_BAT {
                draw_bat_trail(&smooth, scene.hue_phase * TAU * 3.0);
            } else if skin == SKIN_NORTHERN_LIGHTS {
                draw_aurora_trail(&smooth, scene.hue_phase * TAU * 2.0);
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
        let mut overlay_windows: Vec<(u32, CGRect)> = Vec::new();
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

            let window_frame = CGRect::new(
                &CGPoint::new(frame.origin.x, frame.origin.y),
                &CGSize::new(frame.size.width, frame.size.height),
            );
            let number: i64 = msg_send![window, windowNumber];
            overlay_windows.push((number as u32, window_frame));
        }
        let _ = OVERLAY_WINDOWS.set(overlay_windows);

        eprintln!("Overlay đã tạo cho {} màn hình.", views.len());

        // Tiếng click, hiệu ứng, panel settings và icon trên menu bar
        load_sound_preference();
        load_sound_kind_preference();
        load_effect_preferences();
        load_skin_preference();
        preload_sound_kind();

        // Sóng nước thật cần chụp màn hình để bẻ cong ảnh phía sau
        if click_effect() == CLICK_METAL && !screen_capture_granted() {
            request_screen_capture_access();
        }

        // Quả pháo chào ngay khi mở app (chỉ khi đang chọn hiệu ứng pháo hoa)
        if click_effect() == CLICK_FIREWORKS {
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
                            if neon_on() {
                                let loc = event.location();
                                push_mouse_sample(
                                    loc.x,
                                    screen_height - loc.y,
                                    Instant::now(),
                                );
                            }
                        }
                        CGEventType::TapDisabledByTimeout
                        | CGEventType::TapDisabledByUserInput => {
                            // macOS tự ngắt tap khi callback của mình chạy quá lâu.
                            // Không bật lại thì app mất hẵn mẫu chuột.
                            let port = EVENT_TAP_PORT.load(Ordering::Relaxed);
                            if port != 0 {
                                CGEventTapEnable(port as *mut std::ffi::c_void, true);
                            }
                        }
                        _ => {
                            let loc = event.location();
                            let cocoa_y = screen_height - loc.y;
                            if let Ok(mut clicks) = PENDING_CLICKS.lock() {
                                clicks.push((loc.x, cocoa_y));
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
                    EVENT_TAP_PORT.store(
                        tap.mach_port.as_concrete_TypeRef() as usize,
                        Ordering::Relaxed,
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

            let (dirty, idle, warp_only) = match get_scene().lock() {
                Ok(scene) => (
                    scene.dirty_bounds,
                    scene.idle_secs > 0.12,
                    // Chỉ còn thấu kính warp đang động: không hạt, không dải neon
                    scene.particles.is_empty()
                        && scene.trail.is_empty()
                        && click::metal::animating(),
                ),
                Err(poisoned) => {
                    let scene = poisoned.into_inner();
                    (
                        scene.dirty_bounds,
                        scene.idle_secs > 0.12,
                        scene.particles.is_empty()
                            && scene.trail.is_empty()
                            && click::metal::animating(),
                    )
                }
            };

            // Chỉ có thấu kính warp: ảnh warp chỉ dựng lại 18 fps
            // (`warp::RENDER_INTERVAL`) nên vẽ lại overlay ~20 fps là đủ liền mạch.
            static WARP_PARITY: AtomicUsize = AtomicUsize::new(0);
            // Chuột đứng yên thì chỉ cần 30 fps cho các đốm lấp lánh -> nhẹ CPU
            static PARITY: AtomicUsize = AtomicUsize::new(0);
            let skip_frame = if warp_only {
                // Timer chạy ~62,5 fps: vẽ 1 trong 3 frame -> ~20,8 fps
                WARP_PARITY.fetch_add(1, Ordering::Relaxed) % 3 != 0
            } else {
                idle && PARITY.fetch_add(1, Ordering::Relaxed) % 2 == 1
            };
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
