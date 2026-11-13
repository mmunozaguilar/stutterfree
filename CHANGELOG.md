# Changelog

## 0.4.0

Compiling without a recording: StutterFree reads the shaders from the game's files and plans the pipelines itself.

- **Unity (DirectX 11):** serialized files and UnityFS bundles (LZ4/LZMA); every DX11 shader is compiled once.
- **Unreal Engine 4/5:** `.pak` (v3-11) and unencrypted IoStore `.utoc/.ucas` (Oodle, zlib, LZ4);
  `ShaderArchive-*.ushaderbytecode` libraries; engine version and DX11/DX12 detection.
  - DX11: every shader, plus the tessellation pairs (HS+DS) of each material.
  - DX12: Unreal's root signatures rebuilt (rules for 4.20 to 5.8, from each shader's resource counts, with the real
    DXBC checksum) and neutral-state pipelines: compute, VS, VS→PS and VS→GS(→PS) of each material, global chains,
    material PSs behind the global VSs and, on UE5, PSs shared by many materials.
  - Shaders with a `[WaveSize]` the GPU can't run are left out (NVIDIA: 32; AMD: 32-64).
- **FromSoftware:** BHD5/BDT archives (RSA, AES, DCX, BND4).
  - Dark Souls III / Remastered (DX11), with tessellation pairs. DS3's keys are downloaded once from the ones published
    by UXM Selective Unpack.
  - Elden Ring / Nightreign (DX12): the root signature each shader carries, rewritten to the 1.0 version the game creates.
- Tested through Proton: MECCHA CHAMELEON (UE 5.5, 38,426 pipelines), Dark Souls III (4,089), Elden Ring (161,387), all
  without failures.
- **App:** each card shows what was compiled from the files ("From Game Files"); banners no longer flicker when
  scrolling; Ko-fi button; option to scan on startup (also `settings --scan-on-startup on|off` in the CLI).
- **Lighter:** the scan no longer keeps a list of the millions of files in the Mesa cache (818 MB → 173 MB of RAM, and
  faster); OpenGL interface (`glow`); a single executable for the app and the command line. The AppImage goes from 15.4
  to 10.1 MB.
- `plan <game>` shows the plan without compiling. `STUTTERFREE_WARM_LOG=<file>` keeps vkd3d-proton's errors from the
  warmer, and a warmer that stops early is now reported.

## 0.3.0-beta

Rust rewrite of Shaderless 0.2.3-beta, with a native interface (egui) and an AppImage: game detection (Steam, Heroic,
Lutris), Record mode, compiling from the recording through Proton, NVIDIA/Mesa/vkd3d cache management.
