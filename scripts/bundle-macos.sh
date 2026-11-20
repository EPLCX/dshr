#!/usr/bin/env bash
# 在 macOS 上打包桌面版：通用二进制（Apple 芯片 + Intel）→ DSHR Desktop.app → DSHR-Desktop-<版本>-macos.dmg。
# GPUI 需要 Xcode 的 Metal 编译器与 macOS SDK，只能在 Mac（或 macOS CI）上运行。
# 用法：scripts/bundle-macos.sh            # 通用二进制
#       ARCHS=aarch64 scripts/bundle-macos.sh   # 只打 Apple 芯片
# 需要：Xcode（或 Command Line Tools + Metal 工具链）、rustup。
# 签名：默认 ad-hoc；设置 CODESIGN_IDENTITY="Developer ID Application: ..." 时用正式证书签名。
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"

APP_NAME="DSHR Desktop"
BUNDLE_ID="${BUNDLE_ID:-dev.dshr.desktop}"
BIN="dshr-desktop"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
ARCHS="${ARCHS:-aarch64 x86_64}"
MIN_MACOS="${MIN_MACOS:-11.0}"
OUT="$ROOT/target/macos"
APP="$OUT/$APP_NAME.app"

export MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS"

# 1. 编译
bins=()
for arch in $ARCHS; do
  target="$arch-apple-darwin"
  rustup target add "$target" >/dev/null
  echo "==> cargo build --release -p dshr-desktop --target $target"
  cargo build --release -p dshr-desktop --target "$target"
  bins+=("$ROOT/target/$target/release/$BIN")
done

# 2. 组装 .app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
if [ "${#bins[@]}" -gt 1 ]; then
  lipo -create "${bins[@]}" -output "$APP/Contents/MacOS/$BIN"
else
  cp "${bins[0]}" "$APP/Contents/MacOS/$BIN"
fi
cp assets/tray.png "$APP/Contents/Resources/tray.png"
# 许可证与第三方声明（scripts/third-party-notices.ps1 生成）
cp LICENSE THIRD_PARTY_NOTICES.md "$APP/Contents/Resources/"

# 图标：从 1024 的 icon.png 生成 icns
ICONSET="$OUT/icon.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) assets/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>$BIN</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleDevelopmentRegion</key><string>zh_CN</string>
  <key>CFBundleLocalizations</key><array><string>zh_CN</string><string>en</string></array>
  <key>LSMinimumSystemVersion</key><string>$MIN_MACOS</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

# 3. 签名
codesign --force --deep --options runtime --timestamp=none \
  --sign "${CODESIGN_IDENTITY:--}" "$APP"
codesign --verify --verbose "$APP"

# 4. DMG（带 /Applications 快捷方式）
DMG="$OUT/${APP_NAME// /-}-$VERSION-macos.dmg"
STAGE="$OUT/dmg"
rm -rf "$STAGE" "$DMG" && mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"

echo "==> $APP"
echo "==> $DMG"
