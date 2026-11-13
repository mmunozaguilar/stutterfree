#!/usr/bin/env bash
# Builds Shaderless's two Windows-side binaries with mingw-w64 (they run inside Proton, next to/inside the game):
#   native/d3d12.dll            the recorder (recorder.cpp, exports as in d3d12.def)
#   native/shaderless_warm.exe  the warmer   (warmer.cpp + SCSKiller's warm11.cpp)
# Needs: x86_64-w64-mingw32-g++ (Arch/CachyOS: mingw-w64-gcc, Debian/Ubuntu: g++-mingw-w64-x86-64-posix)
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
N="$DIR/native"
OUT="${1:-$N}"
CXX="${CXX:-x86_64-w64-mingw32-g++}"
FLAGS=(-O2 -s -std=c++20 -static -static-libgcc -static-libstdc++)
mkdir -p "$OUT"

echo "== d3d12.dll (recorder)"
"$CXX" "${FLAGS[@]}" -shared -o "$OUT/d3d12.dll" "$N/recorder.cpp" "$N/d3d12.def" -lbcrypt -ldxguid -luuid

echo "== shaderless_warm.exe (warmer)"
"$CXX" "${FLAGS[@]}" -municode -I"$N" -o "$OUT/shaderless_warm.exe" "$N/warmer.cpp" "$N/warm11.cpp" \
    -ld3d12 -ld3d11 -ldxgi -ld3dcompiler -lbcrypt -ldxguid -luuid -lole32

echo "done: $OUT"
