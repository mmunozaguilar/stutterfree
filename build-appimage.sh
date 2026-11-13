#!/usr/bin/env bash
# Builds StutterFree-<version>-x86_64.AppImage:
#   1. cargo build --release (stutterfree: the app and, given a command, the command line)
#   2. the Windows-side recorder / warmer with mingw-w64 (./build-native.sh), unless native/ already has them and
#      mingw isn't installed
#   3. an AppDir (AppRun, .desktop, icons, binaries, native helpers) packed with appimagetool
# Needs: cargo, appimagetool (Arch/CachyOS: AUR appimagetool-bin; or set APPIMAGETOOL=/path/to/appimagetool),
#        mingw-w64-gcc for step 2. The AppImage needs no extra libraries at run time (native egui window).
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
OUT="$DIR/dist"
APPDIR="$OUT/StutterFree.AppDir"
TOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"

echo "== cargo build --release"
cargo build --release --locked 2>/dev/null || cargo build --release

echo "== native helpers"
native_stale() {
    [[ ! -f native/d3d12.dll || ! -f native/shaderless_warm.exe ]] && return 0
    [[ -n "$(find native -maxdepth 1 \( -name '*.cpp' -o -name '*.def' -o -name '*.h' \) \( -newer native/d3d12.dll -o -newer native/shaderless_warm.exe \) -print -quit)" ]]
}
if ! native_stale; then
    echo "up to date"
elif command -v "${CXX:-x86_64-w64-mingw32-g++}" >/dev/null; then
    ./build-native.sh
elif [[ ! -f native/d3d12.dll || ! -f native/shaderless_warm.exe ]]; then
    echo "mingw-w64 is missing and native/ has no prebuilt helpers: install mingw-w64-gcc" >&2
    exit 1
fi

echo "== AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/stutterfree/native" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/doc/stutterfree"
install -m755 target/release/stutterfree "$APPDIR/usr/bin/"
install -m644 native/d3d12.dll native/shaderless_warm.exe "$APPDIR/usr/share/stutterfree/native/"
install -m755 packaging/appimage/AppRun "$APPDIR/AppRun"
install -m644 packaging/appimage/stutterfree.desktop "$APPDIR/stutterfree.desktop"
install -m644 packaging/appimage/stutterfree.desktop "$APPDIR/usr/share/applications/stutterfree.desktop"
for s in 64 128 256 512; do
    install -Dm644 "assets/stutterfree-$s.png" "$APPDIR/usr/share/icons/hicolor/${s}x${s}/apps/stutterfree.png"
done
install -Dm644 assets/stutterfree.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/stutterfree.svg"
install -m644 assets/stutterfree-256.png "$APPDIR/stutterfree.png"
ln -sf stutterfree.png "$APPDIR/.DirIcon"
install -m644 LICENSE LICENSE-EXCEPTION.txt README.md "$APPDIR/usr/share/doc/stutterfree/" 2>/dev/null || true

if [[ -z "$TOOL" ]]; then
    echo "AppDir ready at $APPDIR, but appimagetool wasn't found." >&2
    echo "Install it (paru -S appimagetool-bin) or run: APPIMAGETOOL=/path/to/appimagetool $0" >&2
    exit 2
fi

echo "== appimagetool"
TARGET="$OUT/StutterFree-$VERSION-x86_64.AppImage"
ARCH=x86_64 "$TOOL" --no-appstream "$APPDIR" "$TARGET"
chmod +x "$TARGET"
echo "done: $TARGET"
