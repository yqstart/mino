#!/bin/bash
# ==================== Mino macOS 打包：生成 .app bundle + dmg ====================
# 用法：scripts/package-macos.sh [版本号] [架构: arm64|x64]
# 依赖：已执行 cargo build --release；iconset 由 make-icon.swift 生成。
# dmg 内含 Mino.app + Applications 快捷方式（拖拽即安装，与常规 macOS 应用一致）。
set -e

VERSION="${1:-1.0.2}"
ARCH="${2:-arm64}"
BIN="target/release/mino-app"
APP_NAME="Mino"
APP_SLUG="mino"
EXECUTABLE="mino-app"
APP_DIR="target/release/${APP_NAME}.app"

if [ ! -f "$BIN" ]; then
    echo "错误：未找到 $BIN，请先 cargo build --release" >&2
    exit 1
fi

# ==================== 1. 生成图标（iconset → icns） ====================
ICONSET_DIR="target/release/${APP_SLUG}.iconset"
swift scripts/make-icon.swift target/release > /dev/null
iconutil -c icns "$ICONSET_DIR" -o "target/release/${APP_SLUG}.icns"
rm -rf "$ICONSET_DIR"
echo "图标生成完成"

# ==================== 2. 组装 .app bundle ====================
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$BIN" "$APP_DIR/Contents/MacOS/${EXECUTABLE}"
cp "target/release/${APP_SLUG}.icns" "$APP_DIR/Contents/Resources/${APP_SLUG}.icns"

cat > "$APP_DIR/Contents/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Mino</string>
    <key>CFBundleDisplayName</key>
    <string>Mino</string>
    <key>CFBundleIdentifier</key>
    <string>dev.mino.terminal</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleExecutable</key>
    <string>${EXECUTABLE}</string>
    <key>CFBundleIconFile</key>
    <string>${APP_SLUG}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <!-- 文件夹访问授权弹窗的说明文字（macOS 26 对"文件和文件夹"类授权必填）：
         缺了它弹窗只显示"是否允许 XXX"，用户无法判断用途；且每次更新后
         ad-hoc 重签都会重置授权，没有说明文字更难一次通过。-->
    <key>NSDesktopFolderUsageDescription</key>
    <string>Mino 需要访问你的项目文件夹，以便终端在此目录工作、打开新标签与收藏目录。</string>
    <key>NSDocumentsFolderUsageDescription</key>
    <string>Mino 需要访问你的项目文件夹，以便终端在此目录工作、打开新标签与收藏目录。</string>
    <key>NSDownloadsFolderUsageDescription</key>
    <string>Mino 需要访问你的项目文件夹，以便终端在此目录工作、打开新标签与收藏目录。</string>
</dict>
</plist>
EOF

# ad-hoc 签名：未公证的本地构建也能获得稳定签名（下载后仍需解除隔离，见 docs）。
codesign --force --sign - "$APP_DIR" > /dev/null 2>&1 || true
echo ".app bundle 完成：$APP_DIR"

# ==================== 3. 组装 dmg（含 Applications 快捷方式） ====================
DMG="target/release/${APP_SLUG}-${VERSION}-macos-${ARCH}.dmg"
DMG_STAGE="target/release/dmg-stage"
rm -rf "$DMG_STAGE"
mkdir -p "$DMG_STAGE"
cp -R "$APP_DIR" "$DMG_STAGE/"
ln -s /Applications "$DMG_STAGE/Applications"

rm -f "$DMG"
hdiutil create -volname "$APP_NAME" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DMG" > /dev/null
echo "dmg 完成：$DMG"

# ==================== 4. 本机美化布局（CI 无 GUI 跳过，失败不影响产物） ====================
if [ "${CI:-false}" != "true" ]; then
    MOUNT_ROOT="$(mktemp -d)"
    MOUNT_POINT="$MOUNT_ROOT/$APP_NAME"
    mkdir -p "$MOUNT_POINT"
    if hdiutil attach "$DMG" -nobrowse -readwrite -mountpoint "$MOUNT_POINT" -quiet > /dev/null 2>&1; then
        osascript << 'EOF' > /dev/null 2>&1 || true
tell application "Finder"
    tell disk "Mino"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set the bounds of container window to {120, 120, 680, 480}
        set viewOptions to the icon view options of container window
        set arrangement of viewOptions to not arranged
        set icon size of viewOptions to 96
        set position of item "Mino.app" of container window to {130, 170}
        set position of item "Applications" of container window to {410, 170}
        close
        open
        update without registering applications
    end tell
end tell
EOF
        hdiutil detach "$MOUNT_POINT" -quiet > /dev/null 2>&1 || true
    fi
    rm -rf "$MOUNT_ROOT"
fi

rm -rf "$DMG_STAGE"
echo "dmg 布局完成：$DMG"
