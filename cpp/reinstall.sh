#!/usr/bin/env bash
# Build rồi cài bản C++ vào /Applications (cùng chỗ với bản Rust, khác tên bundle)
#
#   ./reinstall.sh            -> build + copy + mở app
#   ./reinstall.sh --no-open  -> build + copy, không mở app
#
# Bản Rust vẫn nguyên: /Applications/MacFireworks.app
# Bản C++: /Applications/MacFireworksCpp.app  (bundle id com.macfireworks.cpp)
set -euo pipefail

cd "$(dirname "$0")"

TARGET="/Applications/MacFireworksCpp.app"
SOURCE="dist/MacFireworksCpp.app"
OPEN_AFTER=1
for arg in "$@"; do
    case "$arg" in
        --no-open) OPEN_AFTER=0 ;;
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

./build.sh

echo "==> Dừng bản đang chạy (nếu có)"
pkill -x mac_fireworks_cpp 2>/dev/null || true
sleep 0.3

echo "==> Cài vào $TARGET"
rm -rf "$TARGET"
ditto "$SOURCE" "$TARGET"

# Kiểm tra chữ ký sau khi copy (ditto giữ nguyên metadata, nhưng kiểm cho chắc)
codesign --verify --strict "$TARGET"
echo "    OK: $(codesign -dv "$TARGET" 2>&1 | sed -n 's/^Identifier=//p')"

if [[ $OPEN_AFTER -eq 1 ]]; then
    echo "==> Mở app"
    open "$TARGET"
fi

echo "Xong. Menu bar có icon ✦; bấm vào để mở panel."
