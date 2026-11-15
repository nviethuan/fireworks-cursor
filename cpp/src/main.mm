// Mac Fireworks — bản C++ (Objective-C++)
//
// Bản port 1:1 của app Rust (src/main.rs + src/click/*) để so sánh:
//   * menu bar item + panel settings
//   * overlay toàn màn hình trên mỗi display, vẽ pháo hoa / neon trail (5 skin)
//   * hiệu ứng click: Fireworks / Metal warp / Off
//   * âm thanh click (22 loại, ghim ra loa trong của máy)
//
// Biên dịch: ./build.sh   (ra dist/MacFireworksCpp.app)
// Cài lại:   ./reinstall.sh
//
// Khác biệt có chủ đích so với bản Rust:
//   * Toàn bộ logic nằm trong một file .mm, dùng std::vector/std::mutex của C++17
//   * ARC quản lý object ObjC (bản Rust tự retain/release qua objc crate)
//   * Không có "lớp" abstraction nào thêm: cần so sánh hiệu năng ở mức "cùng ý tưởng"

#import <Cocoa/Cocoa.h>
#import <ApplicationServices/ApplicationServices.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreImage/CoreImage.h>
#import <QuartzCore/QuartzCore.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <deque>
#include <mutex>
#include <random>
#include <string>
#include <thread>
#include <unordered_set>
#include <vector>

// ==================== Hằng số ====================

static const double TAU = M_PI * 2.0;

// Hiệu ứng click
static const int CLICK_FIREWORKS = 0;
static const int CLICK_OFF = 2;
static const int CLICK_METAL = 3;
static const int CLICK_VALUES[] = {CLICK_FIREWORKS, CLICK_METAL, CLICK_OFF};
static const char *CLICK_TITLES[] = {"Fireworks", "Metal warp", "Off"};

// Skin neon
static const int SKIN_CLASSIC = 0;
static const int SKIN_PIXEL_LOVE = 1;
static const int SKIN_CALLIGRAPHY = 2;
static const int SKIN_BAT = 3;
static const int SKIN_NORTHERN_LIGHTS = 4;

static const double IDLE_HIDE_SECS = 5.0;
static const size_t MAX_MOUSE_SAMPLES = 512;
static const size_t MAX_TRAIL_POINTS = 320;
static const size_t PAINT_HISTORY_FRAMES = 45;

// Metal warp
static const double METAL_LIFE = 1.2;
static const double METAL_PATCH_HALF = 126.0;  // phủ clip 114 * 1.08
static const size_t METAL_MAX_HITS = 6;
static const double WARP_RENDER_INTERVAL = 1.0 / 18.0;

static const char *SOUND_KINDS[] = {
    "click",          "pop",          "chime",        "firework",
    "boom",           "click-wooden", "button-tap",   "bubble-pop",
    "bubble-pop-2",   "rock-on-wood", "squeak-toy",   "duck-quack",
    "menu-beep",      "notification", "pluck",        "gameboy-pluck",
    "coin",           "coin-received", "drop-coin",   "game-bonus",
    "water-droplet",  "firework-2",
};
static const char *SOUND_LABELS[] = {
    "Click",       "Pop",          "Chime",         "Firework",
    "Boom",        "Wooden click", "Button tap",    "Bubble pop",
    "Bubble pop 2","Rock on wood", "Squeak toy",    "Duck quack",
    "Menu beep",   "Notification", "Pluck",         "Gameboy pluck",
    "Coin",        "Coin received","Drop coin",     "Game bonus",
    "Water droplet","Firework 2",
};
static const int SOUND_COUNT = (int)(sizeof(SOUND_KINDS) / sizeof(SOUND_KINDS[0]));

// ==================== Trạng thái toàn cục ====================

static std::atomic<bool> gSoundEnabled{true};
static std::atomic<bool> gSoundPending{false};
static std::atomic<bool> gNeonOn{false};
static std::atomic<bool> gCursorVisible{true};
static std::atomic<bool> gEffectHidden{false};
static std::atomic<int> gClickEffect{CLICK_FIREWORKS};
static std::atomic<int> gSkin{SKIN_CLASSIC};
static std::atomic<int> gSoundKind{0};
static std::atomic<float> gSoundVolume{0.45f};

// Click do event tap ghi lại (thread riêng) → main thread dựng hiệu ứng
static std::mutex gClickMutex;
static std::vector<CGPoint> gPendingClicks;

// Mẫu vị trí chuột do event tap ghi lại
struct MouseSample {
    double x, y;
    std::chrono::steady_clock::time_point at;
};
static std::mutex gSampleMutex;
static std::vector<MouseSample> gSamples;

static std::atomic<CFMachPortRef> gEventTapPort{nullptr};

// Panel settings + menu bar
static NSWindow *gSettingsWindow = nil;
static NSStatusItem *gStatusItem = nil;
static NSTextField *gVolumeLabel = nil;
static NSTextField *gVolumeTitle = nil;
static NSSlider *gVolumeSlider = nil;
static NSTextField *gSoundLabel = nil;
static NSPopUpButton *gSoundPopup = nil;
static NSTextField *gSkinLabel = nil;
static NSPopUpButton *gSkinPopup = nil;

// Mỗi màn hình: (view, offset)
struct OverlayView {
    // Giữ tham chiếu mạnh tới window: ARC sẽ giải phóng NSWindow nếu không ai giữ,
    // và cửa sổ biến mất dù đã orderFront (khác với Rust: objc crate tự retain).
    NSWindow *window;
    NSView *view;
    double offsetX;
    double offsetY;
};
static std::vector<OverlayView> gOverlays;
static std::atomic<bool> gTapStarted{false};
// (window number, frame Cocoa) — để chụp màn hình nằm dưới overlay
struct OverlayWindowInfo {
    uint32_t number;
    CGRect frame;
};
static std::vector<OverlayWindowInfo> gOverlayWindows;

static double gScreenHeight = 0.0;

// ==================== Tiện ích CoreGraphics ====================

static CGContextRef currentContext() {
    NSGraphicsContext *gc = [NSGraphicsContext currentContext];
    return gc ? (CGContextRef)[gc CGContext] : NULL;
}

static void fillOval(double cx, double cy, double radius, double r, double g, double b, double a) {
    if (radius <= 0.2 || a <= 0.002) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;
    CGContextSetRGBFillColor(ctx, std::clamp(r, 0.0, 1.0), std::clamp(g, 0.0, 1.0),
                             std::clamp(b, 0.0, 1.0), std::clamp(a, 0.0, 1.0));
    CGContextFillEllipseInRect(ctx, CGRectMake(cx - radius, cy - radius, radius * 2, radius * 2));
}

static void strokeCircle(double cx, double cy, double radius, double r, double g, double b,
                         double a, double width) {
    if (radius <= 0.5 || a <= 0.002) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;
    CGContextSetRGBStrokeColor(ctx, std::clamp(r, 0.0, 1.0), std::clamp(g, 0.0, 1.0),
                               std::clamp(b, 0.0, 1.0), std::clamp(a, 0.0, 1.0));
    CGContextSetLineWidth(ctx, width);
    CGContextStrokeEllipseInRect(ctx, CGRectMake(cx - radius, cy - radius, radius * 2, radius * 2));
}

// ==================== Âm thanh ====================

static std::vector<NSArray<NSSound *> *> gSoundPools(SOUND_COUNT, nil);
static std::atomic<size_t> gSoundNext{0};
static NSString *gSoundDeviceUID = nil;
static bool gSoundDeviceResolved = false;

static NSString *deviceUID(AudioObjectID device) {
    AudioObjectPropertyAddress addr = {kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal,
                                       kAudioObjectPropertyElementMain};
    CFStringRef uid = NULL;
    UInt32 size = sizeof(uid);
    if (AudioObjectGetPropertyData(device, &addr, 0, NULL, &size, &uid) != noErr || !uid) return nil;
    return (__bridge_transfer NSString *)uid;
}

static NSString *defaultOutputDeviceUID() {
    AudioObjectPropertyAddress addr = {kAudioHardwarePropertyDefaultOutputDevice,
                                       kAudioObjectPropertyScopeGlobal,
                                       kAudioObjectPropertyElementMain};
    AudioObjectID device = 0;
    UInt32 size = sizeof(device);
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &addr, 0, NULL, &size, &device) != noErr ||
        device == 0)
        return nil;
    return deviceUID(device);
}

// Chỉ lấy loa trong của máy (bỏ micro built-in và thiết bị ngoài)
static NSString *builtinOutputUID() {
    AudioObjectPropertyAddress addr = {kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal,
                                       kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &addr, 0, NULL, &size) != noErr ||
        size < sizeof(AudioObjectID))
        return nil;

    std::vector<AudioObjectID> devices(size / sizeof(AudioObjectID));
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &addr, 0, NULL, &size, devices.data()) !=
        noErr)
        return nil;

    for (AudioObjectID device : devices) {
        AudioObjectPropertyAddress transport = {kAudioDevicePropertyTransportType,
                                                kAudioObjectPropertyScopeGlobal,
                                                kAudioObjectPropertyElementMain};
        UInt32 transportValue = 0;
        UInt32 valueSize = sizeof(transportValue);
        if (AudioObjectGetPropertyData(device, &transport, 0, NULL, &valueSize, &transportValue) != noErr)
            continue;
        if (transportValue != kAudioDeviceTransportTypeBuiltIn) continue;

        AudioObjectPropertyAddress streams = {kAudioDevicePropertyStreams,
                                              kAudioObjectPropertyScopeOutput,
                                              kAudioObjectPropertyElementMain};
        UInt32 streamSize = 0;
        if (AudioObjectGetPropertyDataSize(device, &streams, 0, NULL, &streamSize) != noErr ||
            streamSize == 0)
            continue;

        NSString *uid = deviceUID(device);
        if (uid) return uid;
    }
    return nil;
}

static NSString *soundDeviceUID() {
    if (!gSoundDeviceResolved) {
        gSoundDeviceResolved = true;
        gSoundDeviceUID = builtinOutputUID();
        if (!gSoundDeviceUID) gSoundDeviceUID = defaultOutputDeviceUID();
    }
    return gSoundDeviceUID;
}

static NSArray<NSSound *> *loadSoundPool(int kind) {
    NSString *path = [[NSBundle mainBundle] pathForResource:[NSString stringWithUTF8String:SOUND_KINDS[kind]]
                                                     ofType:@"wav"];
    if (!path) {
        fprintf(stderr, "Không thấy %s.wav trong bundle.\n", SOUND_KINDS[kind]);
        return nil;
    }
    NSString *uid = soundDeviceUID();
    NSMutableArray<NSSound *> *pool = [NSMutableArray arrayWithCapacity:4];
    for (int i = 0; i < 4; i++) {
        NSSound *sound = [[NSSound alloc] initWithContentsOfFile:path byReference:YES];
        if (!sound) continue;
        sound.volume = gSoundVolume.load();
        if (uid) [sound setPlaybackDeviceIdentifier:uid];
        [pool addObject:sound];
    }
    return pool;
}

static void ensureSoundPool(int kind) {
    if (kind < 0 || kind >= SOUND_COUNT) return;
    if (gSoundPools[kind] == nil) gSoundPools[kind] = loadSoundPool(kind);
}

static void playClickSound() {
    if (!gSoundEnabled.load()) return;
    int kind = gSoundKind.load();
    ensureSoundPool(kind);
    NSArray<NSSound *> *pool = gSoundPools[kind];
    if (!pool || pool.count == 0) return;
    size_t start = gSoundNext.fetch_add(1) % pool.count;
    for (size_t i = 0; i < pool.count; i++) {
        NSSound *sound = pool[(start + i) % pool.count];
        if (!sound.isPlaying) {
            [sound play];
            return;
        }
    }
    [pool[start] play];
}

// ==================== Preferences ====================

static void loadPreferences() {
    NSUserDefaults *d = [NSUserDefaults standardUserDefaults];

    if ([d objectForKey:@"ClickEffect"] != nil) {
        NSInteger value = [d integerForKey:@"ClickEffect"];
        int effect = CLICK_FIREWORKS;
        if (value == 2) effect = CLICK_OFF;
        else if (value == 3) effect = CLICK_METAL;
        gClickEffect.store(effect);
    } else if ([d objectForKey:@"FireworksEnabled"] != nil) {
        gClickEffect.store([d boolForKey:@"FireworksEnabled"] ? CLICK_FIREWORKS : CLICK_OFF);
    }

    if ([d objectForKey:@"NeonEnabled"] != nil) {
        gNeonOn.store([d boolForKey:@"NeonEnabled"]);
    } else if ([d objectForKey:@"Effect"] != nil) {
        gNeonOn.store([d integerForKey:@"Effect"] == 1);
    }

    if ([d objectForKey:@"SoundEnabled"] != nil) gSoundEnabled.store([d boolForKey:@"SoundEnabled"]);
    if ([d objectForKey:@"SoundVolume"] != nil)
        gSoundVolume.store(std::clamp((float)[d floatForKey:@"SoundVolume"], 0.0f, 1.0f));

    if ([d objectForKey:@"Sound"] != nil) {
        NSInteger value = [d integerForKey:@"Sound"];
        gSoundKind.store((int)std::clamp<NSInteger>(value, 0, SOUND_COUNT - 1));
    }

    if ([d objectForKey:@"Skin"] != nil) {
        NSInteger value = [d integerForKey:@"Skin"];
        gSkin.store((value >= 1 && value <= 4) ? (int)value : SKIN_CLASSIC);
    }
}

// ==================== Quyền hệ thống ====================

static int osMajorVersion() {
    NSOperatingSystemVersion v = [[NSProcessInfo processInfo] operatingSystemVersion];
    return (int)v.majorVersion;
}

static const char *permissionPaneName(int major) {
    return major >= 27 ? "Device Control and Data Access" : "Accessibility";
}

static void openPermissionPane() {
    const char *url = osMajorVersion() >= 27
                          ? "x-apple.systempreferences:com.apple.preference.security"
                          : "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
    NSTask *task = [[NSTask alloc] init];
    task.launchPath = @"/usr/bin/open";
    task.arguments = @[[NSString stringWithUTF8String:url]];
    @try {
        [task launch];
    } @catch (NSException *e) {
    }
}

// Hộp thoại thay cho dòng stderr: app chạy dạng menu bar nên không ai đọc stderr.
static void showPermissionAlert(bool accessibility, bool listenEvent) {
    NSAlert *alert = [[NSAlert alloc] init];
    alert.messageText = @"Mac Fireworks C++ chưa được cấp quyền Accessibility";
    NSMutableString *text = [NSMutableString string];
    [text appendString:@"Thiếu quyền nên app không nhận được sự kiện click:\n"];
    if (!accessibility) [text appendString:@"  • Accessibility: CHƯA cấp\n"];
    if (!listenEvent) [text appendString:@"  • Input Monitoring: CHƯA cấp\n"];
    [text appendString:@"\nSystem Settings → Privacy & Security → Accessibility, bấm \"+\" "
                      @"và chọn /Applications/MacFireworksCpp.app.\n"];
    [text appendString:@"Nếu app đã có trong danh sách mà vẫn không ăn: chọn nó, bấm \"-\" rồi "
                      @"\"+\" thêm lại.\n\nApp tự nhận quyền mới sau vài giây, không cần mở lại."];
    alert.informativeText = text;
    [alert addButtonWithTitle:@"Mở System Settings"];
    [alert addButtonWithTitle:@"Để sau"];
    [NSApp activateIgnoringOtherApps:YES];
    if ([alert runModal] == NSAlertFirstButtonReturn) openPermissionPane();
}

static void printPermissionHint(bool accessibility, bool listenEvent) {
    const char *pane = permissionPaneName(osMajorVersion());
    fprintf(stderr, "Thiếu quyền nên click chưa được theo dõi:\n");
    fprintf(stderr, "  • %s: %s\n", pane, accessibility ? "đã cấp" : "CHƯA cấp");
    fprintf(stderr, "  • Input Monitoring: %s\n", listenEvent ? "đã cấp" : "CHƯA cấp");
    fprintf(stderr,
            "Mở System Settings → Privacy & Security, bật \"Mac Fireworks C++\" ở cả hai mục %s và "
            "Input Monitoring.\n",
            pane);
    fprintf(stderr, "  • Nếu đã bật mà vẫn không ăn: chọn nó, bấm \"-\" để xoá, rồi \"+\" thêm lại "
                    "/Applications/MacFireworksCpp.app\n");
    fprintf(stderr,
            "Reset nhanh: tccutil reset Accessibility com.macfireworks.cpp && tccutil reset ListenEvent "
            "com.macfireworks.cpp\n");
}

static bool accessibilityGranted() {
    bool accessibility = AXIsProcessTrusted();
    bool listenEvent = CGPreflightListenEventAccess();
    if (accessibility && listenEvent) return true;
    if (!accessibility) CGRequestPostEventAccess();
    if (!listenEvent) CGRequestListenEventAccess();
    printPermissionHint(accessibility, listenEvent);
    return false;
}

static bool screenCaptureGranted() { return CGPreflightScreenCaptureAccess(); }

static void requestScreenCapture() {
    CGRequestScreenCaptureAccess();
    fprintf(stderr,
            "Hiệu ứng cần quyền Screen Recording để phản chiếu hình phía sau:\n  System Settings > "
            "Privacy & Security > Screen Recording > bật Mac Fireworks C++, rồi mở lại app.\n");
}

// ==================== Scene ====================

struct Particle {
    double x, y, vx, vy;
    double life, decay;
    double r, g, b;
    double size, gravity;
    double twinklePhase, twinkleSpeed;
};

struct TrailPoint {
    double x, y, life;
};

struct Bounds {
    double x0, y0, x1, y1;
    bool ok = false;
};

static void addBounds(Bounds &b, double x, double y, double radius) {
    double x0 = x - radius, y0 = y - radius, x1 = x + radius, y1 = y + radius;
    if (!b.ok) {
        b = {x0, y0, x1, y1, true};
        return;
    }
    b.x0 = std::min(b.x0, x0);
    b.y0 = std::min(b.y0, y0);
    b.x1 = std::max(b.x1, x1);
    b.y1 = std::max(b.y1, y1);
}

struct Scene {
    std::vector<Particle> particles;
    std::vector<TrailPoint> trail;
    double headX = 0, headY = 0;
    bool headInit = false;
    double idleSecs = 0;
    double huePhase = 0;
    bool hasLastFrame = false;
    std::chrono::steady_clock::time_point lastFrame;
    std::vector<Bounds> paintHistory;
    Bounds dirty;
    bool wasMoving = false;
    bool fullRefresh = false;
    bool hasLastCursor = false;
    double lastCursorX = 0, lastCursorY = 0;
};

static Scene gScene;
static std::mutex gSceneMutex;

static std::mt19937 &rng() {
    static std::mt19937 engine{std::random_device{}()};
    return engine;
}
static double randDouble(double lo, double hi) {
    return std::uniform_real_distribution<double>(lo, hi)(rng());
}

// ==================== Metal warp ====================

struct ScreenPatch {
    CGImageRef image = NULL;
    CIImage *ci = nil;
    double width = 0, height = 0;  // point
    double x = 0, y = 0;           // gốc dưới-trái, toạ độ Cocoa
    bool ok = false;
};

struct WarpImage {
    CGImageRef image = NULL;
    double dx = 0, dy = 0, dw = 0, dh = 0;  // vùng đã dựng, so với gốc patch
};

struct MetalHit {
    double x = 0, y = 0, age = 0;
    ScreenPatch patch;
    WarpImage warped;
    double renderedAt = -1e9;
};

static std::vector<MetalHit> gMetalHits;

static const OverlayWindowInfo *overlayWindowAt(double x, double y) {
    for (const auto &info : gOverlayWindows) {
        if (x >= info.frame.origin.x && x <= info.frame.origin.x + info.frame.size.width &&
            y >= info.frame.origin.y && y <= info.frame.origin.y + info.frame.size.height)
            return &info;
    }
    return nullptr;
}

static ScreenPatch capturePatch(double cx, double cy, double half) {
    ScreenPatch patch;
    const OverlayWindowInfo *info = overlayWindowAt(cx, cy);
    if (!info) return patch;

    double x0 = std::max(cx - half, (double)info->frame.origin.x);
    double y0 = std::max(cy - half, (double)info->frame.origin.y);
    double x1 = std::min(cx + half, (double)(info->frame.origin.x + info->frame.size.width));
    double y1 = std::min(cy + half, (double)(info->frame.origin.y + info->frame.size.height));
    double w = x1 - x0, h = y1 - y0;
    if (w < 40 || h < 40) return patch;

    // CGWindowListCreateImage dùng gốc trên-trái của màn hình chính
    CGRect captureRect = CGRectMake(x0, gScreenHeight - y1, w, h);
    CGImageRef raw = CGWindowListCreateImage(captureRect, kCGWindowListOptionOnScreenBelowWindow,
                                            info->number, kCGWindowImageBestResolution);
    if (!raw) return patch;

    patch.image = raw;
    patch.ci = [CIImage imageWithCGImage:raw];
    patch.width = w;
    patch.height = h;
    patch.x = x0;
    patch.y = y0;
    patch.ok = true;
    return patch;
}

static void releasePatch(ScreenPatch &patch) {
    if (patch.image) CGImageRelease(patch.image);
    patch.image = NULL;
    patch.ci = nil;
    patch.ok = false;
}

// Dựng ảnh warp cho một thấu kính (bản port của warp.rs::render)
static bool renderWarp(const ScreenPatch &patch, double x, double y, double radius, double scale,
                       WarpImage &out) {
    if (!patch.ok || std::abs(scale) < 0.004 || radius < 2.0) return false;

    CGRect extent = patch.ci.extent;
    if (extent.size.width < 1 || extent.size.height < 1) return false;
    double toPixels = extent.size.width / std::max(1.0, patch.width);

    double clipRadius = std::min(radius * 1.08, std::min(patch.width, patch.height) * 0.5);
    if (clipRadius < 2.0) return false;

    double cxPx = (x - patch.x) * toPixels;
    double cyPx = (y - patch.y) * toPixels;
    double halfPx = clipRadius * toPixels;

    // Giao với extent ảnh
    double x0 = std::max(cxPx - halfPx, extent.origin.x);
    double y0 = std::max(cyPx - halfPx, extent.origin.y);
    double x1 = std::min(cxPx + halfPx, extent.origin.x + extent.size.width);
    double y1 = std::min(cyPx + halfPx, extent.origin.y + extent.size.height);
    if (x1 - x0 < 1 || y1 - y0 < 1) return false;
    CGRect cropRect = CGRectMake(x0, y0, x1 - x0, y1 - y0);

    CIFilter *filter = [CIFilter filterWithName:@"CIBumpDistortion"];
    if (!filter) return false;
    [filter setValue:patch.ci forKey:@"inputImage"];
    [filter setValue:[CIVector vectorWithX:cxPx Y:cyPx] forKey:@"inputCenter"];
    [filter setValue:@(std::clamp(radius * toPixels, 1.0, 400.0)) forKey:@"inputRadius"];
    [filter setValue:@(scale) forKey:@"inputScale"];
    CIImage *output = filter.outputImage;
    if (!output) return false;
    CIImage *cropped = [output imageByCroppingToRect:cropRect];

    // Tạo context mới mỗi lần: dùng lại context làm Core Image ném exception
    CIContext *context = [CIContext contextWithOptions:@{kCIContextUseSoftwareRenderer : @YES}];
    if (!context) return false;
    CGImageRef cg = [context createCGImage:cropped fromRect:cropRect];
    if (!cg) return false;

    if (out.image) CGImageRelease(out.image);
    out.image = cg;
    out.dx = x0 / toPixels;
    out.dy = y0 / toPixels;
    out.dw = (x1 - x0) / toPixels;
    out.dh = (y1 - y0) / toPixels;
    return true;
}

static void drawWarp(const WarpImage &warp, const ScreenPatch &patch, double x, double y,
                     double radius, double offsetX, double offsetY) {
    if (!warp.image) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;
    double cx = x - offsetX, cy = y - offsetY;
    double clipRadius = std::min(radius * 1.08, std::min(patch.width, patch.height) * 0.5);

    CGContextSaveGState(ctx);
    CGContextAddEllipseInRect(ctx, CGRectMake(cx - clipRadius, cy - clipRadius, clipRadius * 2,
                                              clipRadius * 2));
    CGContextClip(ctx);
    CGContextDrawImage(ctx, CGRectMake(patch.x + warp.dx - offsetX, patch.y + warp.dy - offsetY,
                                       warp.dw, warp.dh),
                       warp.image);
    CGContextRestoreGState(ctx);
}

static std::pair<double, double> metalLens(double age) {
    double t = std::clamp(age / METAL_LIFE, 0.0, 1.0);
    double radius = 26.0 + 88.0 * (1.0 - (1.0 - t) * (1.0 - t));
    double wave = std::sin(t * TAU * 1.3);
    double scale = 0.55 * wave * std::pow(1.0 - t, 1.1);
    return {radius, scale};
}

static void spawnMetal(double x, double y) {
    MetalHit hit;
    hit.x = x;
    hit.y = y;
    hit.patch = capturePatch(x, y, METAL_PATCH_HALF);
    if (gMetalHits.size() >= METAL_MAX_HITS) {
        releasePatch(gMetalHits.front().patch);
        if (gMetalHits.front().warped.image) CGImageRelease(gMetalHits.front().warped.image);
        gMetalHits.erase(gMetalHits.begin());
    }
    gMetalHits.push_back(hit);
}

static void updateMetal(double dt) {
    for (auto &hit : gMetalHits) hit.age += dt;
    for (auto it = gMetalHits.begin(); it != gMetalHits.end();) {
        if (it->age >= METAL_LIFE) {
            releasePatch(it->patch);
            if (it->warped.image) CGImageRelease(it->warped.image);
            it = gMetalHits.erase(it);
        } else {
            ++it;
        }
    }
}

static void clearMetal() {
    for (auto &hit : gMetalHits) {
        releasePatch(hit.patch);
        if (hit.warped.image) CGImageRelease(hit.warped.image);
    }
    gMetalHits.clear();
}

static Bounds metalBounds() {
    Bounds b;
    for (const auto &hit : gMetalHits) addBounds(b, hit.x, hit.y, 118.0);
    return b;
}

static void drawMetal(double offsetX, double offsetY) {
    for (auto &hit : gMetalHits) {
        if (!hit.patch.ok) continue;
        auto [radius, scale] = metalLens(hit.age);
        if (std::abs(scale) < 0.004 || radius < 2.0) continue;

        if (!hit.warped.image || hit.age - hit.renderedAt >= WARP_RENDER_INTERVAL) {
            if (renderWarp(hit.patch, hit.x, hit.y, radius, scale, hit.warped)) hit.renderedAt = hit.age;
        }
        drawWarp(hit.warped, hit.patch, hit.x, hit.y, radius, offsetX, offsetY);

        double fade = std::pow(1.0 - std::clamp(hit.age / METAL_LIFE, 0.0, 1.0), 1.5);
        strokeCircle(hit.x - offsetX, hit.y - offsetY, radius, 0.80, 0.93, 1.0, 0.16 * fade, 1.0);
    }
}

static void clearClickEffects() {
    clearMetal();
}

// ==================== Pháo hoa + neon ====================

struct Color {
    double r, g, b;
};

static Color neonColor(double phase) {
    double h = (phase - std::floor(phase)) * 6.0;
    double x = 1.0 - std::abs(std::fmod(h, 2.0) - 1.0);
    switch ((int)h) {
        case 0: return {1.0, x, 0.35};
        case 1: return {x, 1.0, 0.35};
        case 2: return {0.35, 1.0, x};
        case 3: return {0.35, x, 1.0};
        case 4: return {x, 0.35, 1.0};
        default: return {1.0, 0.35, x};
    }
}

static void spawnFirework(double x, double y) {
    static const double palette[][3] = {
        {1.0, 0.25, 0.35}, {1.0, 0.85, 0.15}, {0.15, 0.9, 1.0}, {0.65, 0.35, 1.0}, {0.25, 1.0, 0.5},
    };
    int index = (int)randDouble(0, 5);
    double r = palette[index][0], g = palette[index][1], b = palette[index][2];

    std::lock_guard<std::mutex> lock(gSceneMutex);

    // Tia sáng loé trắng ở tâm
    Particle core{};
    core.x = x;
    core.y = y;
    core.life = 1.0;
    core.decay = 0.14;
    core.r = core.g = core.b = 1.0;
    core.size = 9.0;
    core.twinklePhase = TAU / 4.0;
    gScene.particles.push_back(core);

    for (int i = 0; i < 72; i++) {
        double angle = randDouble(0, TAU);
        double speed = 1.5 + std::sqrt(randDouble(0, 1)) * 4.6;
        Particle p{};
        p.x = x;
        p.y = y;
        p.vx = std::cos(angle) * speed;
        p.vy = std::sin(angle) * speed;
        p.life = 1.0;
        p.decay = 0.009 + randDouble(0, 1) * 0.008;
        p.r = r;
        p.g = g;
        p.b = b;
        p.size = 1.2 + randDouble(0, 1) * 2.0;
        p.gravity = 0.055 + randDouble(0, 1) * 0.05;
        p.twinklePhase = randDouble(0, TAU);
        p.twinkleSpeed = 0.3 + randDouble(0, 1) * 0.6;
        gScene.particles.push_back(p);
    }
}

static std::pair<double, double> ribbonNormal(const std::vector<std::pair<double, double>> &points,
                                              size_t i) {
    size_t prev = i == 0 ? 0 : i - 1;
    size_t next = std::min(i + 1, points.size() - 1);
    double dx = points[next].first - points[prev].first;
    double dy = points[next].second - points[prev].second;
    double len = std::sqrt(dx * dx + dy * dy);
    if (len < 0.0001) return {0.0, 0.0};
    return {-dy / len, dx / len};
}

static void fillRibbon(const std::vector<std::pair<double, double>> &points, double maxWidth,
                       double r, double g, double b, double alpha) {
    size_t count = points.size();
    if (count < 2 || alpha <= 0.004 || maxWidth <= 0.2) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;

    auto halfWidth = [&](size_t i) {
        double t = (double)i / (double)(count - 1);
        return maxWidth * 0.5 * std::pow(t, 0.7);
    };

    CGContextBeginPath(ctx);
    for (size_t i = 0; i < count; i++) {
        auto [nx, ny] = ribbonNormal(points, i);
        double hw = halfWidth(i);
        double x = points[i].first + nx * hw, y = points[i].second + ny * hw;
        if (i == 0) CGContextMoveToPoint(ctx, x, y);
        else CGContextAddLineToPoint(ctx, x, y);
    }
    for (size_t i = count; i-- > 0;) {
        auto [nx, ny] = ribbonNormal(points, i);
        double hw = halfWidth(i);
        CGContextAddLineToPoint(ctx, points[i].first - nx * hw, points[i].second - ny * hw);
    }
    CGContextClosePath(ctx);
    CGContextSetRGBFillColor(ctx, std::clamp(r, 0.0, 1.0), std::clamp(g, 0.0, 1.0),
                             std::clamp(b, 0.0, 1.0), std::clamp(alpha, 0.0, 1.0));
    CGContextFillPath(ctx);
}

// Dải kín với bề rộng tuỳ ý (dùng cho cọ mực + cực quang)
template <typename F>
static void fillBrushLayer(const std::vector<std::pair<double, double>> &points, F halfWidthAt,
                           double r, double g, double b, double alpha) {
    size_t count = points.size();
    if (count < 2) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;

    CGContextBeginPath(ctx);
    for (size_t i = 0; i < count; i++) {
        auto [nx, ny] = ribbonNormal(points, i);
        double t = (double)i / (double)(count - 1);
        double hw = std::max(0.0, halfWidthAt(i, t));
        double x = points[i].first + nx * hw, y = points[i].second + ny * hw;
        if (i == 0) CGContextMoveToPoint(ctx, x, y);
        else CGContextAddLineToPoint(ctx, x, y);
    }
    for (size_t i = count; i-- > 0;) {
        auto [nx, ny] = ribbonNormal(points, i);
        double t = (double)i / (double)(count - 1);
        double hw = std::max(0.0, halfWidthAt(i, t));
        CGContextAddLineToPoint(ctx, points[i].first - nx * hw, points[i].second - ny * hw);
    }
    CGContextClosePath(ctx);
    CGContextSetRGBFillColor(ctx, r, g, b, alpha);
    CGContextFillPath(ctx);
}

static double brushNoise(size_t i) {
    double v = std::sin(i * 12.9898 + 78.233) * 43758.5453;
    return std::abs(v - std::floor(v));
}

static Color pixelLoveColor(double t) {
    t = std::clamp(t, 0.0, 1.0);
    return {1.0, 0.18 + 0.45 * t, 0.55 + 0.35 * t};
}

static const uint8_t PIXEL_HEART[6] = {0b0110110, 0b1111111, 0b1111111, 0b0111110, 0b0011100, 0b0001000};

static void drawPixelHeart(CGContextRef ctx, double cx, double cy, double cell, double r, double g,
                           double b, double alpha) {
    CGContextSetRGBFillColor(ctx, r, g, b, alpha);
    double width = 7.0 * cell, height = 6.0 * cell;
    for (int row = 0; row < 6; row++) {
        for (int col = 0; col < 7; col++) {
            if ((PIXEL_HEART[row] & (1 << (6 - col))) == 0) continue;
            double x = cx - width * 0.5 + col * cell;
            double y = cy - height * 0.5 + (5 - row) * cell;
            double size = std::max(cell - 0.7, 1.0);
            CGContextFillRect(ctx, CGRectMake(x, y, size, size));
        }
    }
}

static void drawPixelTrail(const std::vector<std::pair<double, double>> &points) {
    size_t count = points.size();
    if (count < 2) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;

    const double CELL = 6.0;
    const double ALPHA[4] = {0.45, 0.65, 0.82, 0.95};

    std::vector<std::pair<double, double>> buckets[4];
    std::unordered_set<uint64_t> seen;
    for (size_t i = 0; i < count; i++) {
        double t = (double)i / (double)(count - 1);
        auto [nx, ny] = ribbonNormal(points, i);
        double half = (1.0 + 1.6 * t) * CELL;
        int lanes = (int)std::max(0.0, std::round(half / CELL));
        for (int lane = -lanes; lane <= lanes; lane++) {
            double px = points[i].first + nx * lane * CELL;
            double py = points[i].second + ny * lane * CELL;
            int64_t cx = (int64_t)std::llround(px / CELL);
            int64_t cy = (int64_t)std::llround(py / CELL);
            uint64_t key = ((uint64_t)(uint32_t)cx << 32) | (uint32_t)cy;
            if (!seen.insert(key).second) continue;
            int level = std::min(3, (int)(t * 3.99));
            buckets[level].push_back({cx * CELL, cy * CELL});
        }
    }

    // Quầng hồng mờ phía sau
    CGContextSetRGBFillColor(ctx, 1.0, 0.42, 0.78, 0.07);
    for (auto &bucket : buckets) {
        for (size_t i = 0; i < bucket.size(); i += 2) {
            double size = CELL * 2.2;
            CGContextFillRect(ctx, CGRectMake(bucket[i].first - size * 0.5,
                                              bucket[i].second - size * 0.5, size, size));
        }
    }

    for (int level = 0; level < 4; level++) {
        if (buckets[level].empty()) continue;
        Color cellColor = pixelLoveColor((level + 0.5) / 4.0);
        CGContextSetRGBFillColor(ctx, cellColor.r, cellColor.g, cellColor.b, ALPHA[level]);
        double size = CELL - 0.8;
        for (auto &cell : buckets[level]) {
            CGContextFillRect(ctx, CGRectMake(cell.first - size * 0.5, cell.second - size * 0.5, size,
                                              size));
        }
    }

    // Trái tim rải dọc dải
    for (size_t index = 20; index < count; index += 20) {
        double t = (double)index / (double)(count - 1);
        double x = std::round(points[index].first / CELL) * CELL;
        double y = std::round(points[index].second / CELL) * CELL;
        drawPixelHeart(ctx, x, y, 2.8 + 0.8 * t, 1.0, 0.96, 1.0, 0.6 + 0.32 * t);
    }

    // Đầu dải: trái tim lớn
    auto [hx, hy] = points.back();
    hx = std::round(hx / CELL) * CELL;
    hy = std::round(hy / CELL) * CELL;
    drawPixelHeart(ctx, hx, hy, 3.6, 1.0, 0.98, 1.0, 0.95);
    Color headColor = pixelLoveColor(1.0);
    CGContextSetRGBFillColor(ctx, headColor.r, headColor.g, headColor.b, 0.10);
    CGContextFillEllipseInRect(ctx, CGRectMake(hx - 24, hy - 24, 48, 48));
}

static void drawBrushTrail(const std::vector<std::pair<double, double>> &points) {
    size_t count = points.size();
    if (count < 2) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;

    auto profile = [](size_t i, double t) {
        return (2.2 + 8.5 * t) * (0.72 + 0.45 * brushNoise(i));
    };

    fillBrushLayer(points, [&](size_t i, double t) { return profile(i, t) * 1.7; }, 0.92, 0.92, 0.96, 0.08);
    fillBrushLayer(points, [&](size_t i, double t) { return profile(i, t) * 1.35; }, 0.13, 0.13, 0.15, 0.12);
    fillBrushLayer(points, [&](size_t i, double t) { return profile(i, t); }, 0.09, 0.09, 0.11, 0.85);
    fillBrushLayer(points, [&](size_t i, double t) { return profile(i, t) * 0.45; }, 0.03, 0.03, 0.04, 0.55);

    auto [hx, hy] = points.back();
    CGContextSetRGBFillColor(ctx, 0.04, 0.04, 0.05, 0.85);
    CGContextFillEllipseInRect(ctx, CGRectMake(hx - 3.2, hy - 3.2, 6.4, 6.4));

    for (size_t index = 6; index < count; index += 9) {
        double n = brushNoise(index * 7 + 3);
        if (n <= 0.42) continue;
        auto [nx, ny] = ribbonNormal(points, index);
        double side = n > 0.72 ? 1.0 : -1.0;
        double offset = 6.0 + n * 15.0;
        double radius = 0.7 + n * 1.7;
        double x = points[index].first + nx * offset * side;
        double y = points[index].second + ny * offset * side;
        CGContextSetRGBFillColor(ctx, 0.06, 0.06, 0.08, 0.5);
        CGContextFillEllipseInRect(ctx, CGRectMake(x - radius, y - radius, radius * 2, radius * 2));
    }
}

static void addBatPath(CGContextRef ctx, double cx, double cy, double angle, double size, double flap) {
    double sinA = std::sin(angle), cosA = std::cos(angle);
    double wingTop = 2.2 - 1.9 * flap;
    double wingTip = 1.4 - 2.4 * flap;
    const double shape[16][2] = {
        {0.0, 2.6},   {0.8, 1.6},   {2.1, wingTop}, {3.6, wingTip}, {2.7, 0.4},  {3.1, -0.8},
        {1.7, -0.3},  {1.1, -1.2},  {0.5, -1.9},    {-0.5, -1.9},   {-1.1, -1.2}, {-1.7, -0.3},
        {-3.1, -0.8}, {-2.7, 0.4},  {-3.6, wingTip}, {-2.1, wingTop},
    };
    for (int i = 0; i < 16; i++) {
        double x = shape[i][0], y = shape[i][1];
        double px = cx + (x * cosA - y * sinA) * size;
        double py = cy + (x * sinA + y * cosA) * size;
        if (i == 0) CGContextMoveToPoint(ctx, px, py);
        else CGContextAddLineToPoint(ctx, px, py);
    }
    CGContextClosePath(ctx);
}

static void buildBatPaths(CGContextRef ctx, const std::vector<std::pair<double, double>> &points,
                          size_t count, const std::vector<size_t> &indexes, double phase) {
    CGContextBeginPath(ctx);
    for (size_t index : indexes) {
        double t = (double)index / (double)(count - 1);
        auto [nx, ny] = ribbonNormal(points, index);
        double angle = std::atan2(ny, nx) + TAU / 4.0;
        double flap = std::sin(phase + index * 0.5);
        double sway = std::sin(index * 1.7) * 3.5;
        addBatPath(ctx, points[index].first + nx * sway, points[index].second + ny * sway, angle,
                   2.0 + 1.6 * t, flap);
    }
}

static void drawBatTrail(const std::vector<std::pair<double, double>> &points, double phase) {
    size_t count = points.size();
    if (count < 2) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;

    const size_t STRIDE = 9;
    std::vector<size_t> buckets[3];
    for (size_t index = STRIDE; index < count; index += STRIDE) {
        double t = (double)index / (double)(count - 1);
        int level = std::min(2, (int)(t * 2.99));
        buckets[level].push_back(index);
    }

    for (int level = 0; level < 3; level++) {
        if (buckets[level].empty()) continue;
        double fillAlpha = 0.45 + 0.25 * level;

        buildBatPaths(ctx, points, count, buckets[level], phase);
        CGContextSetRGBStrokeColor(ctx, 0.62, 0.35, 0.95, 0.12);
        CGContextSetLineWidth(ctx, 7.0);
        CGContextStrokePath(ctx);

        buildBatPaths(ctx, points, count, buckets[level], phase);
        CGContextSetRGBStrokeColor(ctx, 0.78, 0.5, 1.0, 0.30);
        CGContextSetLineWidth(ctx, 2.0);
        CGContextStrokePath(ctx);

        buildBatPaths(ctx, points, count, buckets[level], phase);
        CGContextSetRGBFillColor(ctx, 0.13, 0.08, 0.2, fillAlpha);
        CGContextFillPath(ctx);
    }
}

static void drawAuroraTrail(const std::vector<std::pair<double, double>> &points, double phase) {
    size_t count = points.size();
    if (count < 2) return;
    CGContextRef ctx = currentContext();
    if (!ctx) return;

    const double LAYERS[4][5] = {
        {0.10, 1.00, 0.45, 3.0, 0.30},
        {0.10, 0.85, 0.65, 1.8, 0.40},
        {0.45, 0.35, 1.00, 1.0, 0.45},
        {0.60, 1.00, 0.85, 0.40, 0.70},
    };

    for (int layer = 0; layer < 4; layer++) {
        double layerPhase = phase + layer * 1.3;
        double scale = LAYERS[layer][3];
        fillBrushLayer(
            points,
            [&](size_t i, double t) {
                double wave = 0.62 + 0.5 * std::sin(i * 0.26 + layerPhase);
                return (2.5 + 9.0 * t) * wave * scale * 0.5;
            },
            LAYERS[layer][0], LAYERS[layer][1], LAYERS[layer][2], LAYERS[layer][4]);
    }

    for (size_t index = 8; index < count; index += 11) {
        double t = (double)index / (double)(count - 1);
        double shimmer = std::sin(phase * 2.0 + index * 0.6);
        if (shimmer <= 0.55) continue;
        auto [nx, ny] = ribbonNormal(points, index);
        double half = (2.5 + 9.0 * t) * 1.5;
        CGContextSetRGBStrokeColor(ctx, 0.85, 1.0, 0.92, 0.22 * shimmer);
        CGContextSetLineWidth(ctx, 2.0);
        CGContextBeginPath(ctx);
        CGContextMoveToPoint(ctx, points[index].first - ny * half, points[index].second + nx * half);
        CGContextAddLineToPoint(ctx, points[index].first + ny * half, points[index].second - nx * half);
        CGContextStrokePath(ctx);
    }
}

static void pushTrailSegment(std::vector<TrailPoint> &trail, const std::pair<double, double> &from,
                             const std::pair<double, double> &to) {
    const double POINT_SPACING = 4.0;
    const size_t MAX_POINTS_PER_SEGMENT = 48;

    double dx = to.first - from.first;
    double dy = to.second - from.second;
    double distance = std::sqrt(dx * dx + dy * dy);
    if (distance < 0.3) {
        trail.push_back({to.first, to.second, 1.0});
        return;
    }
    size_t steps = (size_t)std::clamp(std::ceil(distance / POINT_SPACING), 1.0, (double)MAX_POINTS_PER_SEGMENT);
    for (size_t step = 1; step <= steps; step++) {
        double t = (double)step / (double)steps;
        trail.push_back({from.first + dx * t, from.second + dy * t, 1.0});
    }
}

// Vùng cần vẽ lại = hợp nội dung 45 frame gần nhất
static void updateDirtyBounds() {
    Bounds bounds;
    for (const auto &p : gScene.particles) addBounds(bounds, p.x, p.y, p.size * 4.4 + 6.0);

    Bounds metal = metalBounds();
    if (metal.ok) {
        double cx = (metal.x0 + metal.x1) / 2.0;
        double cy = (metal.y0 + metal.y1) / 2.0;
        double radius = std::max(metal.x1 - metal.x0, metal.y1 - metal.y0) / 2.0 + 2.0;
        addBounds(bounds, cx, cy, radius);
    }

    if (gNeonOn.load() && !gEffectHidden.load()) {
        for (const auto &point : gScene.trail) addBounds(bounds, point.x, point.y, 40.0);
    }

    if (bounds.ok) {
        if (gScene.paintHistory.size() >= PAINT_HISTORY_FRAMES) gScene.paintHistory.erase(gScene.paintHistory.begin());
        gScene.paintHistory.push_back(bounds);
    }

    Bounds dirty;
    for (const auto &b : gScene.paintHistory) {
        if (!dirty.ok) {
            dirty = b;
            continue;
        }
        dirty.x0 = std::min(dirty.x0, b.x0);
        dirty.y0 = std::min(dirty.y0, b.y0);
        dirty.x1 = std::max(dirty.x1, b.x1);
        dirty.y1 = std::max(dirty.y1, b.y1);
    }
    if (dirty.ok) {
        dirty.x0 -= 3.0;
        dirty.y0 -= 3.0;
        dirty.x1 += 3.0;
        dirty.y1 += 3.0;
    }
    gScene.dirty = dirty;
}

// ==================== Vòng lặp frame ====================

static void pushMouseSample(double x, double y) {
    std::lock_guard<std::mutex> lock(gSampleMutex);
    if (gSamples.size() >= MAX_MOUSE_SAMPLES)
        gSamples.erase(gSamples.begin(), gSamples.begin() + (gSamples.size() - MAX_MOUSE_SAMPLES + 1));
    gSamples.push_back({x, y, std::chrono::steady_clock::now()});
}

static void tick() {
    auto now = std::chrono::steady_clock::now();

    // Click từ event tap → dựng hiệu ứng ở main thread
    std::vector<CGPoint> clicks;
    {
        std::lock_guard<std::mutex> lock(gClickMutex);
        clicks.swap(gPendingClicks);
    }
    for (const auto &point : clicks) {
        int effect = gClickEffect.load();
        if (effect == CLICK_FIREWORKS) spawnFirework(point.x, point.y);
        else if (effect == CLICK_METAL) spawnMetal(point.x, point.y);
    }

    std::lock_guard<std::mutex> lock(gSceneMutex);

    double dt = 1.0 / 60.0;
    if (gScene.hasLastFrame) {
        dt = std::chrono::duration<double>(now - gScene.lastFrame).count();
        dt = std::clamp(dt, 0.001, 0.05);
    }
    gScene.lastFrame = now;
    gScene.hasLastFrame = true;

    double f = dt * 60.0;
    double damping = std::pow(0.985, f);

    for (auto &p : gScene.particles) {
        p.x += p.vx * f;
        p.y += p.vy * f;
        p.vy -= p.gravity * f;
        p.vx *= damping;
        p.vy *= damping;
        p.life -= p.decay * f;
        p.twinklePhase += p.twinkleSpeed * f;
    }
    gScene.particles.erase(std::remove_if(gScene.particles.begin(), gScene.particles.end(),
                                          [](const Particle &p) { return p.life <= 0.0; }),
                           gScene.particles.end());

    updateMetal(dt);

    if (!gNeonOn.load()) {
        gScene.trail.clear();
        gScene.headInit = false;
        gScene.idleSecs = 0.0;
        updateDirtyBounds();
        return;
    }

    // CGCursorIsVisible() là IPC sang WindowServer: kiểm tra ~6 lần/giây là đủ
    static size_t visFrame = 0;
    if (visFrame++ % 4 == 0) gCursorVisible.store(CGCursorIsVisible());

    if (!gCursorVisible.load()) {
        gEffectHidden.store(true);
        gScene.trail.clear();
        gScene.particles.clear();
        gScene.headInit = false;
        updateDirtyBounds();
        return;
    }

    NSPoint cursor = [NSEvent mouseLocation];
    bool cursorMoved = !gScene.hasLastCursor ||
                       std::abs(gScene.lastCursorX - cursor.x) > 0.5 ||
                       std::abs(gScene.lastCursorY - cursor.y) > 0.5;
    gScene.lastCursorX = cursor.x;
    gScene.lastCursorY = cursor.y;
    gScene.hasLastCursor = true;

    std::vector<MouseSample> samples;
    {
        std::lock_guard<std::mutex> sampleLock(gSampleMutex);
        samples.swap(gSamples);
    }
    samples.erase(std::remove_if(samples.begin(), samples.end(),
                                 [&](const MouseSample &s) {
                                     return std::chrono::duration<double>(now - s.at).count() >= 0.5;
                                 }),
                  samples.end());

    if (!gScene.headInit) {
        gScene.headX = cursor.x;
        gScene.headY = cursor.y;
        gScene.headInit = true;
        samples.clear();
    }

    bool moving = !samples.empty() || cursorMoved;
    if (gScene.wasMoving && !moving) gScene.fullRefresh = true;
    gScene.wasMoving = moving;

    if (samples.empty() && !cursorMoved) gScene.idleSecs += dt;
    else gScene.idleSecs = 0.0;

    if (gScene.idleSecs > IDLE_HIDE_SECS) {
        gEffectHidden.store(true);
        gScene.trail.clear();
        gScene.particles.clear();
        gScene.headInit = false;
        updateDirtyBounds();
        return;
    }
    gEffectHidden.store(false);

    gScene.huePhase = std::fmod(gScene.huePhase + dt * 0.55, 1.0);

    for (auto &point : gScene.trail) point.life -= dt * 1.6;
    gScene.trail.erase(std::remove_if(gScene.trail.begin(), gScene.trail.end(),
                                      [](const TrailPoint &p) { return p.life <= 0.0; }),
                       gScene.trail.end());

    std::pair<double, double> previous{gScene.headX, gScene.headY};
    if (!gScene.trail.empty()) previous = {gScene.trail.back().x, gScene.trail.back().y};

    if (samples.empty()) {
        pushTrailSegment(gScene.trail, previous, {cursor.x, cursor.y});
        previous = {cursor.x, cursor.y};
    } else {
        for (const auto &sample : samples) {
            pushTrailSegment(gScene.trail, previous, {sample.x, sample.y});
            previous = {sample.x, sample.y};
        }
    }
    gScene.headX = previous.first;
    gScene.headY = previous.second;

    if (gScene.trail.size() > MAX_TRAIL_POINTS)
        gScene.trail.erase(gScene.trail.begin(), gScene.trail.begin() + (gScene.trail.size() - MAX_TRAIL_POINTS));

    // Đứng yên → lấp lánh neon tại con trỏ
    if (gScene.idleSecs > 0.12 && gScene.particles.size() < 500 && randDouble(0, 1) < 0.75) {
        double angle = randDouble(0, TAU);
        double speed = 0.15 + randDouble(0, 1) * 0.7;
        Color sparkColor = neonColor(gScene.huePhase + randDouble(0, 1) * 0.15);
        Particle p{};
        p.x = cursor.x + std::cos(angle) * 2.0;
        p.y = cursor.y + std::sin(angle) * 2.0;
        p.vx = std::cos(angle) * speed;
        p.vy = std::sin(angle) * speed;
        p.life = 1.0;
        p.decay = 0.022 + randDouble(0, 1) * 0.02;
        p.r = sparkColor.r;
        p.g = sparkColor.g;
        p.b = sparkColor.b;
        p.size = 1.0 + randDouble(0, 1) * 1.6;
        p.gravity = 0.01;
        p.twinklePhase = randDouble(0, TAU);
        p.twinkleSpeed = 0.5 + randDouble(0, 1) * 1.0;
        gScene.particles.push_back(p);
    }

    updateDirtyBounds();
}

// ==================== Vẽ ====================

static void drawScene(NSRect dirtyRect, double offsetX, double offsetY, double maxX, double maxY) {
    CGContextRef ctx = currentContext();
    if (!ctx) return;
    CGContextClearRect(ctx, CGRectMake(dirtyRect.origin.x, dirtyRect.origin.y,
                                       std::max(1.0, dirtyRect.size.width),
                                       std::max(1.0, dirtyRect.size.height)));

    std::lock_guard<std::mutex> lock(gSceneMutex);

    for (const auto &p : gScene.particles) {
        double x = p.x - offsetX, y = p.y - offsetY;
        if (x < -40 || y < -40 || x > maxX + 40 || y > maxY + 40) continue;

        double fade = std::pow(p.life, 1.35);
        double twinkle = 0.45 + 0.55 * (0.5 + 0.5 * std::sin(p.twinklePhase));
        double alpha = std::clamp(fade * twinkle, 0.0, 1.0);
        double core = p.size * (0.35 + 0.65 * p.life);

        fillOval(x, y, core * 4.4, p.r, p.g, p.b, alpha * 0.06);
        fillOval(x, y, core * 2.7, p.r, p.g, p.b, alpha * 0.12);
        fillOval(x, y, core * 1.7, p.r, p.g, p.b, alpha * 0.24);

        double hotR = p.r + (1.0 - p.r) * 0.7;
        double hotG = p.g + (1.0 - p.g) * 0.7;
        double hotB = p.b + (1.0 - p.b) * 0.7;
        fillOval(x, y, core * 0.85, hotR, hotG, hotB, alpha * 0.95);
    }

    drawMetal(offsetX, offsetY);

    if (gNeonOn.load() && !gEffectHidden.load() && gScene.trail.size() > 2) {
        size_t count = gScene.trail.size();
        std::vector<std::pair<double, double>> points;
        points.reserve(count);
        for (const auto &point : gScene.trail) points.push_back({point.x - offsetX, point.y - offsetY});

        // Làm mượt nhẹ đường đi
        std::vector<std::pair<double, double>> smooth;
        smooth.reserve(count);
        for (size_t i = 0; i < count; i++) {
            auto a = points[i == 0 ? 0 : i - 1];
            auto b = points[i];
            auto c = points[std::min(i + 1, count - 1)];
            smooth.push_back({(a.first + 2 * b.first + c.first) / 4.0,
                              (a.second + 2 * b.second + c.second) / 4.0});
        }

        int skin = gSkin.load();
        if (skin == SKIN_PIXEL_LOVE) {
            drawPixelTrail(smooth);
        } else if (skin == SKIN_CALLIGRAPHY) {
            drawBrushTrail(smooth);
        } else if (skin == SKIN_BAT) {
            drawBatTrail(smooth, gScene.huePhase * TAU * 3.0);
        } else if (skin == SKIN_NORTHERN_LIGHTS) {
            drawAuroraTrail(smooth, gScene.huePhase * TAU * 2.0);
        } else {
            Color ribbonColor = neonColor(gScene.huePhase);
            double r = ribbonColor.r, g = ribbonColor.g, b = ribbonColor.b;
            fillRibbon(smooth, 48.0, r, g, b, 0.07);
            fillRibbon(smooth, 30.0, r, g, b, 0.14);
            fillRibbon(smooth, 20.0, r, g, b, 0.78);
            fillRibbon(smooth, 8.0, 1.0, 1.0, 1.0, 0.55);

            auto [hx, hy] = smooth.back();
            double pulse = 0.55 + 0.45 * (std::sin(gScene.huePhase * TAU * 3.0) * 0.5 + 0.5);
            fillOval(hx, hy, 30.0, r, g, b, 0.05 * pulse);
            fillOval(hx, hy, 15.0, r, g, b, 0.12 * pulse);
            fillOval(hx, hy, 7.0, 1.0, 1.0, 1.0, 0.6 * pulse);
        }
    }
}

// ==================== View ====================

@interface FireworkView : NSView
@end

@implementation FireworkView
- (BOOL)isOpaque {
    return NO;
}
- (void)drawRect:(NSRect)dirtyRect {
    NSWindow *window = self.window;
    NSRect frame = window ? window.frame : NSMakeRect(0, 0, 0, 0);
    drawScene(dirtyRect, frame.origin.x, frame.origin.y, frame.size.width, frame.size.height);
}
@end

// ==================== Controller (action của panel) ====================

@interface FireworksController : NSObject
- (void)openSettings:(id)sender;
- (void)toggleSound:(id)sender;
- (void)changeVolume:(id)sender;
- (void)changeClickEffect:(id)sender;
- (void)toggleNeon:(id)sender;
- (void)changeSound:(id)sender;
- (void)changeSkin:(id)sender;
- (void)quitApp:(id)sender;
@end

static void setSoundRowsHidden(bool hidden) {
    for (id view in @[ gSoundLabel ?: (id)[NSNull null], gSoundPopup ?: (id)[NSNull null],
                       gVolumeTitle ?: (id)[NSNull null], gVolumeLabel ?: (id)[NSNull null],
                       gVolumeSlider ?: (id)[NSNull null] ]) {
        if (view == (id)[NSNull null]) continue;
        [view setHidden:hidden];
    }
}

@implementation FireworksController
- (void)openSettings:(id)sender {
    [NSApp activateIgnoringOtherApps:YES];
    [gSettingsWindow makeKeyAndOrderFront:nil];
}

- (void)toggleSound:(id)sender {
    bool enabled = [(NSButton *)sender state] != 0;
    gSoundEnabled.store(enabled);
    setSoundRowsHidden(!enabled);
    [[NSUserDefaults standardUserDefaults] setBool:enabled forKey:@"SoundEnabled"];
}

- (void)changeVolume:(id)sender {
    float volume = std::clamp((float)[(NSSlider *)sender doubleValue], 0.0f, 1.0f);
    gSoundVolume.store(volume);
    for (NSArray<NSSound *> *pool : gSoundPools) {
        if (!pool) continue;
        for (NSSound *sound in pool) sound.volume = volume;
    }
    gVolumeLabel.stringValue = [NSString stringWithFormat:@"%d%%", (int)std::lround(volume * 100)];
    [[NSUserDefaults standardUserDefaults] setFloat:volume forKey:@"SoundVolume"];
}

- (void)changeClickEffect:(id)sender {
    NSInteger index = [(NSPopUpButton *)sender indexOfSelectedItem];
    int effect = (index >= 0 && index < 3) ? CLICK_VALUES[index] : CLICK_FIREWORKS;
    gClickEffect.store(effect);

    clearClickEffects();
    {
        std::lock_guard<std::mutex> lock(gSceneMutex);
        gScene.particles.clear();
        updateDirtyBounds();
    }
    [[NSUserDefaults standardUserDefaults] setInteger:effect forKey:@"ClickEffect"];
}

- (void)toggleNeon:(id)sender {
    bool enabled = [(NSButton *)sender state] != 0;
    gNeonOn.store(enabled);
    gSkinLabel.hidden = !enabled;
    gSkinPopup.hidden = !enabled;

    {
        std::lock_guard<std::mutex> lock(gSceneMutex);
        gScene.trail.clear();
        gScene.headInit = false;
        gScene.idleSecs = 0.0;
        if (!enabled) updateDirtyBounds();
    }
    [[NSUserDefaults standardUserDefaults] setBool:enabled forKey:@"NeonEnabled"];
}

- (void)changeSound:(id)sender {
    NSInteger index = std::clamp<NSInteger>([(NSPopUpButton *)sender indexOfSelectedItem], 0, SOUND_COUNT - 1);
    gSoundKind.store((int)index);
    [[NSUserDefaults standardUserDefaults] setInteger:index forKey:@"Sound"];
    playClickSound();  // nghe thử
}

- (void)changeSkin:(id)sender {
    NSInteger index = [(NSPopUpButton *)sender indexOfSelectedItem];
    int skin = (index >= 1 && index <= 4) ? (int)index : SKIN_CLASSIC;
    gSkin.store(skin);
    {
        std::lock_guard<std::mutex> lock(gSceneMutex);
        gScene.trail.clear();
        gScene.headInit = false;
    }
    [[NSUserDefaults standardUserDefaults] setInteger:skin forKey:@"Skin"];
}

- (void)quitApp:(id)sender {
    [NSApp terminate:nil];
}
@end

// ==================== Panel + menu bar ====================

static int clickEffectIndex() {
    int current = gClickEffect.load();
    for (int i = 0; i < 3; i++)
        if (CLICK_VALUES[i] == current) return i;
    return 0;
}

static NSWindow *buildSettingsWindow(FireworksController *controller) {
    NSWindow *window =
        [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, 320, 346)
                                    styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable
                                      backing:NSBackingStoreBuffered
                                        defer:NO];
    window.title = @"Mac Fireworks C++";
    window.releasedWhenClosed = NO;
    [window center];
    NSView *content = window.contentView;

    NSTextField *title = [NSTextField labelWithString:@"Mac Fireworks C++"];
    title.font = [NSFont boldSystemFontOfSize:13.0];
    title.frame = NSMakeRect(20, 300, 280, 20);
    [content addSubview:title];

    NSButton *neonBox = [NSButton checkboxWithTitle:@"Neon trail follows cursor"
                                             target:controller
                                             action:@selector(toggleNeon:)];
    neonBox.state = gNeonOn.load() ? NSControlStateValueOn : NSControlStateValueOff;
    neonBox.frame = NSMakeRect(20, 268, 280, 22);
    [content addSubview:neonBox];

    gSkinLabel = [NSTextField labelWithString:@"Skin"];
    gSkinLabel.frame = NSMakeRect(36, 240, 60, 18);
    [content addSubview:gSkinLabel];

    gSkinPopup = [[NSPopUpButton alloc] initWithFrame:NSMakeRect(100, 234, 200, 26) pullsDown:NO];
    for (NSString *name in @[ @"Classic", @"Pixel love", @"Calligraphy brush", @"Bat blade",
                              @"Northern lights" ]) {
        [gSkinPopup addItemWithTitle:name];
    }
    [gSkinPopup selectItemAtIndex:gSkin.load()];
    gSkinPopup.target = controller;
    gSkinPopup.action = @selector(changeSkin:);
    [content addSubview:gSkinPopup];

    bool neonOn = gNeonOn.load();
    gSkinLabel.hidden = !neonOn;
    gSkinPopup.hidden = !neonOn;

    NSTextField *clickLabel = [NSTextField labelWithString:@"Click effects"];
    clickLabel.frame = NSMakeRect(36, 204, 90, 18);
    [content addSubview:clickLabel];

    NSPopUpButton *clickPopup = [[NSPopUpButton alloc] initWithFrame:NSMakeRect(130, 198, 170, 26)
                                                            pullsDown:NO];
    for (int i = 0; i < 3; i++) [clickPopup addItemWithTitle:[NSString stringWithUTF8String:CLICK_TITLES[i]]];
    [clickPopup selectItemAtIndex:clickEffectIndex()];
    clickPopup.target = controller;
    clickPopup.action = @selector(changeClickEffect:);
    [content addSubview:clickPopup];

    NSButton *soundBox = [NSButton checkboxWithTitle:@"Play sound on click"
                                              target:controller
                                              action:@selector(toggleSound:)];
    soundBox.state = gSoundEnabled.load() ? NSControlStateValueOn : NSControlStateValueOff;
    soundBox.frame = NSMakeRect(20, 176, 280, 22);
    [content addSubview:soundBox];

    gSoundLabel = [NSTextField labelWithString:@"Sound"];
    gSoundLabel.frame = NSMakeRect(36, 148, 60, 18);
    [content addSubview:gSoundLabel];

    gSoundPopup = [[NSPopUpButton alloc] initWithFrame:NSMakeRect(100, 142, 200, 26) pullsDown:NO];
    for (int i = 0; i < SOUND_COUNT; i++)
        [gSoundPopup addItemWithTitle:[NSString stringWithUTF8String:SOUND_LABELS[i]]];
    [gSoundPopup selectItemAtIndex:gSoundKind.load()];
    gSoundPopup.target = controller;
    gSoundPopup.action = @selector(changeSound:);
    [content addSubview:gSoundPopup];

    gVolumeTitle = [NSTextField labelWithString:@"Volume"];
    gVolumeTitle.frame = NSMakeRect(36, 114, 120, 18);
    [content addSubview:gVolumeTitle];

    gVolumeLabel = [NSTextField
        labelWithString:[NSString stringWithFormat:@"%d%%", (int)std::lround(gSoundVolume.load() * 100)]];
    gVolumeLabel.font = [NSFont monospacedDigitSystemFontOfSize:12.0
                                                        weight:NSFontWeightRegular];
    gVolumeLabel.alignment = NSTextAlignmentRight;
    gVolumeLabel.textColor = [NSColor secondaryLabelColor];
    gVolumeLabel.frame = NSMakeRect(240, 114, 60, 18);
    [content addSubview:gVolumeLabel];

    gVolumeSlider = [[NSSlider alloc] initWithFrame:NSMakeRect(36, 88, 264, 24)];
    gVolumeSlider.minValue = 0.0;
    gVolumeSlider.maxValue = 1.0;
    gVolumeSlider.doubleValue = gSoundVolume.load();
    gVolumeSlider.continuous = YES;
    gVolumeSlider.target = controller;
    gVolumeSlider.action = @selector(changeVolume:);
    [content addSubview:gVolumeSlider];

    setSoundRowsHidden(!gSoundEnabled.load());

    NSTextField *hint = [NSTextField labelWithString:@"Both effects can be enabled at the same time."];
    hint.frame = NSMakeRect(20, 56, 280, 18);
    hint.textColor = [NSColor secondaryLabelColor];
    [content addSubview:hint];

    NSButton *quit = [NSButton buttonWithTitle:@"Quit Mac Fireworks"
                                       target:controller
                                       action:@selector(quitApp:)];
    quit.frame = NSMakeRect(20, 20, 170, 30);
    [content addSubview:quit];

    return window;
}

static void buildStatusItem(FireworksController *controller) {
    NSStatusBar *bar = [NSStatusBar systemStatusBar];
    gStatusItem = [bar statusItemWithLength:NSVariableStatusItemLength];
    NSStatusBarButton *button = gStatusItem.button;

    NSImage *image = [NSImage imageWithSystemSymbolName:@"sparkles"
                              accessibilityDescription:@"Mac Fireworks C++"];
    if (image) {
        [image setTemplate:YES];
        button.image = image;
    } else {
        button.title = @"✦";
    }
    button.target = controller;
    button.action = @selector(openSettings:);
}

// ==================== Event tap ====================

static CGEventRef eventTapCallback(CGEventTapProxy proxy, CGEventType type, CGEventRef event,
                                   void *context) {
    if (type == kCGEventMouseMoved || type == kCGEventLeftMouseDragged) {
        if (gNeonOn.load()) {
            CGPoint loc = CGEventGetLocation(event);
            pushMouseSample(loc.x, gScreenHeight - loc.y);
        }
    } else if (type == kCGEventTapDisabledByTimeout || type == kCGEventTapDisabledByUserInput) {
        CFMachPortRef port = gEventTapPort.load();
        if (port) CGEventTapEnable(port, true);
    } else {
        CGPoint loc = CGEventGetLocation(event);
        {
            std::lock_guard<std::mutex> lock(gClickMutex);
            gPendingClicks.push_back(CGPointMake(loc.x, gScreenHeight - loc.y));
        }
        gSoundPending.store(true);
    }
    return NULL;
}

// Trả về false nếu macOS chưa cho tạo tap (chưa đủ quyền) — thread gọi hàm này
// sẽ hạ cờ gTapStarted để timer 1,5 s thử lại.
static bool startEventTap() {
    CFMachPortRef tap = CGEventTapCreate(
        kCGHIDEventTap, kCGHeadInsertEventTap, kCGEventTapOptionListenOnly,
        CGEventMaskBit(kCGEventLeftMouseDown) | CGEventMaskBit(kCGEventMouseMoved) |
            CGEventMaskBit(kCGEventLeftMouseDragged),
        eventTapCallback, NULL);
    if (!tap) {
        fprintf(stderr, "Không tạo được CGEventTap: macOS chưa cho phép theo dõi sự kiện.\n");
        printPermissionHint(AXIsProcessTrusted(), CGPreflightListenEventAccess());
        return false;
    }
    gEventTapPort.store(tap);
    CFRunLoopSourceRef source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0);
    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
    CGEventTapEnable(tap, true);
    CFRunLoopRun();
    return true;
}

// Quyền có thể được cấp trong lúc app đang chạy (System Settings), nên kiểm tra lại
// định kỳ thay vì chỉ một lần lúc khởi động.
static void startTapIfPermitted() {
    if (gTapStarted.load()) return;
    if (!AXIsProcessTrusted() || !CGPreflightListenEventAccess()) return;
    bool expected = false;
    if (!gTapStarted.compare_exchange_strong(expected, true)) return;
    std::thread([] {
        if (!startEventTap()) gTapStarted.store(false);
    }).detach();
    fprintf(stderr, "Đã bật theo dõi click (Accessibility + Input Monitoring đã cấp).\n");
}

// ==================== main ====================

int main(int argc, const char *argv[]) {
    accessibilityGranted();  // hỏi quyền + log; quyền có thể được cấp sau khi app đã chạy

    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];

        // CGEvent dùng gốc trên-trái của màn hình chính
        CGRect mainBounds = CGDisplayBounds(CGMainDisplayID());
        gScreenHeight = mainBounds.size.height;

        // Mỗi màn hình một cửa sổ overlay riêng
        for (NSScreen *screen in [NSScreen screens]) {
            NSRect frame = screen.frame;
            NSWindow *window = [[NSWindow alloc] initWithContentRect:frame
                                                          styleMask:NSWindowStyleMaskBorderless
                                                            backing:NSBackingStoreBuffered
                                                              defer:NO];
            window.backgroundColor = [NSColor clearColor];
            window.opaque = NO;
            window.ignoresMouseEvents = YES;
            window.level = 1000;
            window.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces |
                                        NSWindowCollectionBehaviorStationary;

            FireworkView *view = [[FireworkView alloc] initWithFrame:NSMakeRect(0, 0, frame.size.width,
                                                                              frame.size.height)];
            window.contentView = view;
            [window makeKeyAndOrderFront:nil];

            gOverlays.push_back({window, view, frame.origin.x, frame.origin.y});
            gOverlayWindows.push_back({(uint32_t)window.windowNumber,
                                       CGRectMake(frame.origin.x, frame.origin.y, frame.size.width,
                                                  frame.size.height)});
        }
        fprintf(stderr, "Overlay đã tạo cho %zu màn hình.\n", gOverlays.size());

        loadPreferences();
        ensureSoundPool(gSoundKind.load());

        if (gClickEffect.load() == CLICK_METAL && !screenCaptureGranted()) requestScreenCapture();

        // Quả pháo chào khi mở app (chỉ khi đang chọn pháo hoa)
        if (gClickEffect.load() == CLICK_FIREWORKS)
            spawnFirework(mainBounds.size.width / 2.0, gScreenHeight / 2.0);

        FireworksController *controller = [[FireworksController alloc] init];
        gSettingsWindow = buildSettingsWindow(controller);
        buildStatusItem(controller);

        startTapIfPermitted();
        if (!gTapStarted.load()) {
            fprintf(stderr, "Đang chạy chế độ chỉ hiển thị pháo hoa, click chưa được theo dõi.\n");
            showPermissionAlert(AXIsProcessTrusted(), CGPreflightListenEventAccess());
        }

        // Bật tap ngay khi người dùng cấp quyền, khỏi phải thoát/mở lại app
        // Common modes để timer vẫn chạy khi alert đang mở (runModal)
        NSTimer *permissionWatch =
            [NSTimer timerWithTimeInterval:1.5
                                   repeats:YES
                                     block:^(NSTimer *t) {
                                       startTapIfPermitted();
                                     }];
        [[NSRunLoop mainRunLoop] addTimer:permissionWatch forMode:NSRunLoopCommonModes];

        __block size_t parity = 0;
        NSTimer *timer = [NSTimer scheduledTimerWithTimeInterval:0.016
                                                         repeats:YES
                                                           block:^(NSTimer *t) {
                                                             if (gSoundPending.exchange(false))
                                                                 playClickSound();
                                                             tick();

                                                             Bounds dirty;
                                                             bool idle;
                                                             bool fullRefresh;
                                                             {
                                                                 std::lock_guard<std::mutex> lock(gSceneMutex);
                                                                 dirty = gScene.dirty;
                                                                 idle = gScene.idleSecs > 0.12;
                                                                 fullRefresh = gScene.fullRefresh;
                                                                 gScene.fullRefresh = false;
                                                             }

                                                             // Chuột đứng yên: 30 fps là đủ
                                                             if (idle && (parity++ % 2) == 1) return;

                                                             if (fullRefresh) {
                                                                 for (auto &overlay : gOverlays)
                                                                     [overlay.view setNeedsDisplay:YES];
                                                                 return;
                                                             }
                                                             if (!dirty.ok) return;

                                                             NSSize size = NSMakeSize(std::max(1.0, dirty.x1 - dirty.x0),
                                                                                      std::max(1.0, dirty.y1 - dirty.y0));
                                                             for (auto &overlay : gOverlays) {
                                                                 NSRect rect = NSMakeRect(dirty.x0 - overlay.offsetX,
                                                                                         dirty.y0 - overlay.offsetY,
                                                                                         size.width, size.height);
                                                                 [overlay.view setNeedsDisplayInRect:rect];
                                                             }
                                                           }];
        (void)timer;

        [app run];
    }
    return 0;
}
