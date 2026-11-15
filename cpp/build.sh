#!/usr/bin/env bash
# Build bản C++ của Mac Fireworks thành dist/MacFireworksCpp.app
#
#   ./build.sh            -> build + bundle + ký
#   ./build.sh --clean    -> xoá build/ trước khi build
#
# Bundle id riêng (com.macfireworks.cpp) nên cài song song với bản Rust và có
# `defaults`/quyền TCC độc lập -> so sánh được hai bản trên cùng máy.
set -euo pipefail

cd "$(dirname "$0")"
CPP_DIR="$(pwd)"
ROOT_DIR="$(cd .. && pwd)"

BIN_NAME="mac_fireworks_cpp"
APP_NAME="Mac Fireworks C++"
BUNDLE_NAME="MacFireworksCpp.app"
BUNDLE_ID="com.macfireworks.cpp"
MIN_MACOS="11.0"
VERSION="0.1.0"
ICON_SRC="$ROOT_DIR/assets/AppIcon.png"

CLEAN=0
for arg in "$@"; do
    case "$arg" in
        --clean) CLEAN=1 ;;
        -h|--help)
            sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "Tham số không hợp lệ: $arg (dùng --help để xem hướng dẫn)" >&2
            exit 1
            ;;
    esac
done

if [[ $CLEAN -eq 1 ]]; then
    rm -rf build dist
fi

echo "==> Biên dịch ($BIN_NAME $VERSION, clang++ $(clang++ --version | head -n 1 | sed 's/.*clang++ //'))"
mkdir -p build dist
# -fobjc-arc: ARC quản lý object ObjC, phần C++ dùng std::vector/std::mutex
# -Wall -Wextra: bắt lỗi sớm; -Wno-unused-parameter cho callback của CoreAudio
clang++ -std=c++17 -fobjc-arc -O2 -DNDEBUG \
    -mmacosx-version-min="$MIN_MACOS" \
    -Wall -Wextra -Wno-unused-parameter \
    "$CPP_DIR/src/main.mm" \
    -o "build/$BIN_NAME" \
    -framework Cocoa \
    -framework CoreGraphics \
    -framework CoreImage \
    -framework CoreAudio \
    -framework ApplicationServices \
    -framework QuartzCore \
    -framework CoreFoundation

APP="dist/$BUNDLE_NAME"
CONTENTS="$APP/Contents"
echo "==> Tạo bundle $APP"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "build/$BIN_NAME" "$CONTENTS/MacOS/$BIN_NAME"

cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>vi</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleExecutable</key>
    <string>$BIN_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MIN_MACOS</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
PLIST
plutil -lint "$CONTENTS/Info.plist" > /dev/null

# Âm thanh dùng chung với bản Rust
shopt -s nullglob
WAVS=("$ROOT_DIR"/assets/*.wav)
shopt -u nullglob
if [[ ${#WAVS[@]} -gt 0 ]]; then
    for wav in "${WAVS[@]}"; do
        cp "$wav" "$CONTENTS/Resources/$(basename "$wav")"
    done
else
    echo "    (không có file .wav trong assets — app sẽ chạy không tiếng)"
fi

if [[ -f "$ICON_SRC" ]]; then
    echo "==> Tạo icon từ assets/AppIcon.png"
    ICONSET="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "$ICONSET"
    for size in 16 32 128 256 512; do
        sips -z $size $size "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" > /dev/null
        sips -z $((size * 2)) $((size * 2)) "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" > /dev/null
    done
    iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/AppIcon.icns"
    rm -rf "$(dirname "$ICONSET")"
    /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string AppIcon" "$CONTENTS/Info.plist"
fi

# Ký cùng chứng chỉ như bản Rust: TCC nhớ quyền theo danh tính chứng chỉ, nên
# build lại không làm mất quyền Accessibility / Input Monitoring / Screen Recording.
IDENTITY="${CODESIGN_IDENTITY:-}"
if [[ -z "$IDENTITY" ]]; then
    IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n 's/.*"\(Apple Development: .*\)"/\1/p' | head -n 1)"
fi
if [[ -z "$IDENTITY" ]]; then
    IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -n 1)"
fi

if [[ -n "$IDENTITY" ]]; then
    echo "==> Ký bằng chứng chỉ: $IDENTITY"
    codesign --force --sign "$IDENTITY" "$APP"
else
    echo "==> Không có chứng chỉ ký code, dùng ad-hoc (mỗi lần build lại sẽ mất quyền TCC)"
    codesign --force --sign - "$APP"
fi
codesign --verify --strict "$APP"
echo "    Designated requirement: $(codesign -d -r- "$APP" 2>&1 | sed -n 's/^designated => //p')"

echo "Hoàn tất: $CPP_DIR/$APP"
echo "Cài lại: ./reinstall.sh"
