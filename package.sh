#!/usr/bin/env bash
# Đóng gói mac_fireworks thành ứng dụng macOS (.app) và tuỳ chọn tạo .dmg
# Cách dùng:
#   ./package.sh          -> dist/MacFireworks.app
#   ./package.sh --dmg    -> dist/MacFireworks.app + dist/MacFireworks-<version>.dmg
set -euo pipefail

cd "$(dirname "$0")"

BIN_NAME="mac_fireworks"
APP_NAME="Mac Fireworks"
BUNDLE_NAME="MacFireworks.app"
BUNDLE_ID="com.macfireworks.app"
MIN_MACOS="11.0"
ICON_SRC="assets/AppIcon.png"

MAKE_DMG=0
for arg in "$@"; do
    case "$arg" in
        --dmg) MAKE_DMG=1 ;;
        -h|--help)
            sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "Tham số không hợp lệ: $arg (dùng --help để xem hướng dẫn)" >&2
            exit 1
            ;;
    esac
done

VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
VERSION="${VERSION:-0.1.0}"

DIST="dist"
APP="$DIST/$BUNDLE_NAME"
CONTENTS="$APP/Contents"

echo "==> Biên dịch bản release ($BIN_NAME $VERSION)"
cargo build --release

echo "==> Tạo bundle $APP"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "target/release/$BIN_NAME" "$CONTENTS/MacOS/$BIN_NAME"

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
    <!-- Ứng dụng chạy nền dạng overlay, không hiện icon trên Dock -->
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

# Âm thanh khi click (CC0, ~0.13s)
if [[ -f assets/click.wav ]]; then
    cp assets/click.wav "$CONTENTS/Resources/click.wav"
else
    echo "    (không có assets/click.wav — app sẽ chạy không tiếng)"
fi

if [[ -f "$ICON_SRC" ]]; then
    echo "==> Tạo icon từ $ICON_SRC"
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

# Ký bằng chứng chỉ có sẵn nếu có: TCC sẽ nhớ quyền theo danh tính chứng chỉ,
# nên build lại không làm mất quyền Accessibility / Input Monitoring.
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

if [[ $MAKE_DMG -eq 1 ]]; then
    DMG="$DIST/MacFireworks-$VERSION.dmg"
    echo "==> Tạo $DMG"
    STAGE="$(mktemp -d)"
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    rm -f "$DMG"
    hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" > /dev/null
    rm -rf "$STAGE"
    echo "Đã tạo: $DMG"
fi

echo "Hoàn tất: $APP"
echo "Mở app: open \"$APP\""
