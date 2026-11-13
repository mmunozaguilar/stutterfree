//! Engine guess, exe discovery and install-root inference (Games/GameEngineDetector.cs).

use crate::models::GameEngine;
use crate::util::{any_file, contains_ci, ends_with_ci, file_len, file_name, starts_with_ci};
use std::fs;
use std::path::{Path, PathBuf};

const IGNORED_EXE_SUBSTRINGS: [&str; 37] = [
    "CrashReport",
    "UnityCrashHandler",
    "UnityBugReporter",
    "DXSETUP",
    "vcredist",
    "vc_redist",
    "EasyAntiCheat",
    "BEService",
    "BattlEye",
    "EpicWebHelper",
    "EpicOnlineServices",
    "EOSBootstrapper",
    "UnrealCEFSubProcess",
    "cef_browser_process",
    "cef_subprocess",
    "cef_helper",
    "QtWebEngineProcess",
    "msedgewebview2",
    "WebView2",
    "D3D12StateObjectCompiler",
    "d3dconfig",
    "7za",
    "7z.exe",
    "8za",
    "crashpad",
    "CheckEnv",
    "UEPrereq",
    "REDprelauncher",
    "ErrorReporter",
    "InstallerMessage",
    "WinMTR-Report",
    "minidump-stackwalk",
    "crs-handler",
    "unins000",
    "uninstall",
    "cleanup",
    "benchmark",
];
const IGNORED_ORIGINAL: &str = ".original.exe";

const BINARY_FOLDER_EXACT: [&str; 7] = ["bin", "binaries", "shipping", "release", "retail", "final", "amd64"];
const BINARY_FOLDER_PREFIXES: [&str; 5] = ["win64", "win32", "win_", "x64", "x86"];
const BOUNDARY_FOLDERS: [&str; 15] =
    ["home", "usr", "opt", "var", "mnt", "media", "run", "tmp", "common", "steamapps", "drive_c", "Program Files", "Program Files (x86)", "Games", "Heroic"];

fn is_ignored_exe(s: &str) -> bool {
    IGNORED_EXE_SUBSTRINGS.iter().any(|ig| contains_ci(s, ig)) || contains_ci(s, IGNORED_ORIGINAL)
}

fn is_binary_subfolder_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    BINARY_FOLDER_EXACT.iter().any(|n| n.eq_ignore_ascii_case(name)) || BINARY_FOLDER_PREFIXES.iter().any(|p| starts_with_ci(name, p))
}

fn is_boundary_directory(dir: &Path) -> bool {
    if dir.parent().is_none() {
        return true;
    }
    let full = crate::paths::trim_sep(&dir.to_string_lossy());
    let home = crate::paths::trim_sep(&crate::paths::home_dir().to_string_lossy());
    if !home.is_empty() && full == home {
        return true;
    }
    if dir.join("drive_c").is_dir() {
        return true;
    }
    let name = file_name(dir);
    BOUNDARY_FOLDERS.iter().any(|b| b.eq_ignore_ascii_case(&name))
}

fn walk_up_from_binary_subfolders(start: &Path) -> PathBuf {
    // 1. Unreal Engine layout: <Root>/<Project>/Binaries/<Platform>/Game.exe
    let mut dir = Some(start);
    while let Some(d) = dir {
        let Some(parent) = d.parent() else { break };
        if file_name(d).eq_ignore_ascii_case("Binaries") {
            let project = parent;
            if let Some(root) = project.parent() {
                if root.join("Engine").is_dir() {
                    return root.to_path_buf();
                }
            }
            return project.to_path_buf();
        }
        dir = Some(parent);
    }

    // 1b. Non-standard Unreal store sub-folders: <Root>/platform/steam/PBZ.exe or <Root>/steam/PBZ.exe with <Root>/Engine
    let name = file_name(start);
    if let Some(parent) = start.parent() {
        if matches!(name.as_str(), "steam" | "epic" | "gog" | "win64" | "Steam" | "Epic" | "GOG") {
            if file_name(parent).eq_ignore_ascii_case("platform") {
                if let Some(pp) = parent.parent() {
                    if !is_boundary_directory(pp) && pp.join("Engine").is_dir() {
                        return pp.to_path_buf();
                    }
                }
            }
            if !is_boundary_directory(parent) && parent.join("Engine").is_dir() {
                return parent.to_path_buf();
            }
        }
    }

    // 2. Multi-level binary sub-folder layouts: Bin/Win64SharedMasterSteamPGO, bin/x64_dx12, win64, x64/Release
    let mut current = start.to_path_buf();
    for _ in 0..3 {
        let Some(parent) = current.parent().map(Path::to_path_buf) else { break };
        if !is_binary_subfolder_name(&file_name(&current)) || is_boundary_directory(&parent) {
            break;
        }
        current = parent;
    }
    current
}

/// The true root of a game installation from an exe path (and an optional launcher folder hint): Unreal
/// (<Root>/<Project>/Binaries/Win64), CryEngine (<Root>/Bin/Win64...), REDengine (<Root>/bin/x64), Dagor (<Root>/win64)...
pub fn infer_install_root(exe_path: &str, hint_folder: Option<&str>) -> String {
    let exe = Path::new(exe_path);
    let exe_dir = exe.parent().unwrap_or(exe);
    let inferred = walk_up_from_binary_subfolders(exe_dir);

    if let Some(hint) = hint_folder.filter(|h| !h.trim().is_empty()) {
        let hint_p = Path::new(hint);
        if hint_p.is_dir() && !hint_p.join("drive_c").is_dir() {
            if let (Ok(full_hint), Ok(full_exe)) = (std::path::absolute(hint_p), std::path::absolute(exe)) {
                let fh = crate::paths::trim_sep(&full_hint.to_string_lossy());
                if !is_boundary_directory(Path::new(&fh)) && full_exe.to_string_lossy().starts_with(&format!("{fh}/")) {
                    let walked = crate::paths::trim_sep(&walk_up_from_binary_subfolders(Path::new(&fh)).to_string_lossy());
                    let fi = crate::paths::trim_sep(&std::path::absolute(&inferred).unwrap_or(inferred.clone()).to_string_lossy());
                    if !is_boundary_directory(Path::new(&walked)) && (fi == walked || fi.starts_with(&format!("{walked}/"))) {
                        return walked;
                    }
                }
            }
        }
    }
    inferred.to_string_lossy().into_owned()
}

pub fn is_non_unreal_pak(file_path: &str) -> bool {
    let norm = file_path.replace('\\', "/");
    for seg in ["/locales/", "/cef/", "/cef64/", "/cef32/", "/chromium/", "/webview/", "/webui/", "/electron/"] {
        if contains_ci(&norm, seg) {
            return true;
        }
    }
    let name = norm.rsplit('/').next().unwrap_or("").to_string();
    let lname = name.to_ascii_lowercase();
    if ["resources.pak", "content_resources.pak", "devtools_resources.pak", "blink_resources.pak", "snapshot_blob.pak"].contains(&lname.as_str())
        || ["chrome_", "cef", "ui_resources", "views_resources", "headless_lib_"].iter().any(|p| lname.starts_with(p))
    {
        return true;
    }
    // Locale pak naming pattern (en-US.pak, zh-CN.pak, es-419.pak)
    let stem = Path::new(&name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if (2..=6).contains(&stem.chars().count()) && stem.contains('-') && stem.chars().all(|c| c.is_alphanumeric() || c == '-') {
        let dash = stem.find('-').unwrap_or(0);
        if dash == 2 || dash == 3 {
            return true;
        }
    }
    false
}

fn any_top(dir: &Path, pattern: &str) -> bool {
    let Ok(rd) = fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| e.path().is_file() && crate::util::name_matches(&e.file_name().to_string_lossy(), pattern, false))
}

/// First `limit` files under `dir` matching `pattern` (case-sensitive, as .NET on Linux), with an extra filter.
fn take_files(dir: &Path, pattern: &str, limit: usize, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                stack.push(p);
            } else if crate::util::name_matches(&e.file_name().to_string_lossy(), pattern, false) && keep(&p) {
                out.push(p);
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// The engine guess and a shader-count estimate shown before anything is compiled.
pub fn detect(install_path: &str) -> (GameEngine, i32) {
    let root = Path::new(install_path);
    if !root.is_dir() {
        return (GameEngine::Unknown, 0);
    }
    let f = |rel: &[&str]| rel.iter().fold(root.to_path_buf(), |p, s| p.join(s));

    // 1. REDengine (Cyberpunk 2077 / The Witcher 3)
    if f(&["bin", "x64", "Cyberpunk2077.exe"]).is_file() || f(&["r6", "config", "settings.json"]).is_file() {
        return (GameEngine::RedEngine, 12000);
    }
    if f(&["bin", "x64", "witcher3.exe"]).is_file()
        || f(&["bin", "x64_dx12", "witcher3.exe"]).is_file()
        || f(&["content", "content0", "shaderdx12_0.cache"]).is_file()
    {
        return (GameEngine::RedEngine, 8500);
    }
    // 2. FromSoftware
    if f(&["Game", "eldenring.exe"]).is_file()
        || f(&["eldenring.exe"]).is_file()
        || f(&["Game", "nightreign.exe"]).is_file()
        || f(&["nightreign.exe"]).is_file()
        || f(&["Game", "DarkSoulsIII.exe"]).is_file()
        || f(&["DarkSoulsRemastered.exe"]).is_file()
    {
        return (GameEngine::FromSoftware, 9500);
    }
    // 3. Capcom RE Engine
    if any_top(root, "re_chunk_*.pak") {
        return (GameEngine::ReEngine, 7200);
    }
    // 4. Dagor, Northlight, Dawn
    if f(&["compiledShaders"]).is_dir() || any_top(root, "*.vromfs.bin") {
        return (GameEngine::Dagor, 8500);
    }
    if f(&["data", "shaders", "build", "pc_dxil"]).is_dir() {
        return (GameEngine::Northlight, 8500);
    }
    if f(&["bin", "rawpso.store2"]).is_file() {
        return (GameEngine::DirectX12, 8500);
    }
    // 5. CryEngine: keep Engine/Engine.pak from matching UE4
    let is_cry = f(&["Engine", "Engine.pak"]).is_file()
        || f(&["Engine", "ShaderCache.pak"]).is_file()
        || f(&["Engine", "ShadersBin.pak"]).is_file()
        || any_file(root, "CryRenderD3D*.dll")
        || any_file(root, "WHGame.dll");
    if is_cry {
        let dx12 = any_file(root, "*d3d12*.dll") || any_file(root, "*dx12*.dll") || any_file(root, "nvngx_dlss*.dll");
        return if dx12 { (GameEngine::DirectX12, 6000) } else { (GameEngine::DirectX11, 3500) };
    }
    // 6. Unreal Engine 4 & 5
    let has_unreal_engine_dir = f(&["Engine", "Binaries"]).is_dir() || f(&["Engine", "Content"]).is_dir();
    if !take_files(root, "*.ucas", 1, |_| true).is_empty() {
        return (GameEngine::UnrealEngine5, 14000);
    }
    if !take_files(root, "*.pak", 1, |p| !is_non_unreal_pak(&p.to_string_lossy())).is_empty() || has_unreal_engine_dir {
        return (GameEngine::UnrealEngine4, 8000);
    }
    // 7. Unity
    let has_data_dir = fs::read_dir(root)
        .map(|rd| rd.flatten().any(|e| e.path().is_dir() && e.file_name().to_string_lossy().ends_with("_Data")))
        .unwrap_or(false);
    if has_data_dir || f(&["UnityPlayer.dll"]).is_file() {
        return (GameEngine::Unity, 4500);
    }
    // 8. Generic DirectX 12
    if any_file(root, "*d3d12*.dll") {
        return (GameEngine::DirectX12, 6000);
    }
    (GameEngine::DirectX11, 3500)
}

fn is_launcher_stub(path: &Path) -> bool {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    ends_with_ci(&stem, "Launcher") || stem.eq_ignore_ascii_case("wgc") || starts_with_ci(&stem, "wgc_")
}

/// The game's main Windows exe in an install folder.
pub fn find_executable(install_path: &str) -> Option<String> {
    let root = Path::new(install_path);
    if !root.is_dir() {
        return None;
    }
    // 1. Unreal Shipping exe
    if let Some(s) = crate::gamefiles::shipping_exe(install_path, None) {
        if Path::new(&s).is_file() {
            return Some(s);
        }
    }
    let mut exes: Vec<PathBuf> = crate::util::files_recursive(root)
        .into_iter()
        .filter(|f| f.extension().map(|e| e == "exe").unwrap_or(false))
        .filter(|f| {
            let s = f.to_string_lossy();
            !is_ignored_exe(&s)
                && !contains_ci(&s, "/Engine/Binaries/")
                && !contains_ci(&s, "/crashreporter/")
                && !contains_ci(&s, "/cef/")
                && !contains_ci(&s, "/cef64/")
                && !contains_ci(&s, "/cef32/")
                && !contains_ci(&s, "/__Installer/")
                && !contains_ci(&s, "/PatchData/")
        })
        .collect();
    exes.sort();
    if exes.is_empty() {
        return None;
    }
    let non_launchers: Vec<PathBuf> = exes.iter().filter(|f| !is_launcher_stub(f)).cloned().collect();
    let pool = if non_launchers.is_empty() { exes } else { non_launchers };

    // 2. Unity <Game>.exe paired with <Game>_Data
    if let Some(u) = pool.iter().find(|f| {
        let mut s = f.with_extension("").into_os_string();
        s.push("_Data");
        Path::new(&s).is_dir()
    }) {
        return Some(u.to_string_lossy().into_owned());
    }

    let largest = |v: &mut dyn Iterator<Item = &PathBuf>| -> Option<PathBuf> {
        let mut best: Option<(u64, &PathBuf)> = None;
        for p in v {
            let l = file_len(p);
            if best.map(|(bl, _)| l > bl).unwrap_or(true) {
                best = Some((l, p));
            }
        }
        best.map(|(_, p)| p.clone())
    };
    let largest_overall = largest(&mut pool.iter())?;
    let largest_bytes = file_len(&largest_overall);
    let shipping = largest(&mut pool.iter().filter(|f| contains_ci(&f.to_string_lossy(), "Shipping")));
    let mut x64 = largest(&mut pool.iter().filter(|f| {
        let s = f.to_string_lossy();
        contains_ci(&s, "Win64") || contains_ci(&s, "x64_dx12") || contains_ci(&s, "x64")
    }));
    // keep small utilities in Binaries/Win64 from beating a much larger real game exe
    if let Some(x) = &x64 {
        if largest_bytes > 0 && file_len(x) < largest_bytes / 4 {
            x64 = None;
        }
    }
    let candidate = shipping.or(x64).unwrap_or(largest_overall);
    let candidate_s = candidate.to_string_lossy().into_owned();
    let resolved = crate::gamefiles::game_exe(install_path, &candidate_s);
    if !resolved.is_empty() && Path::new(&resolved).is_file() {
        return Some(resolved);
    }
    Some(candidate_s)
}

/// The exe that really runs: upgrades a small bootstrapper stub or a helper exe to the Unreal Shipping exe or a much
/// larger main exe in the install.
pub fn resolve_real_game_executable(install_path: &str, configured: Option<&str>) -> Option<String> {
    let exe = match configured.filter(|e| !e.is_empty() && Path::new(e).is_file()) {
        Some(e) => e.to_string(),
        None => return find_executable(install_path).or_else(|| configured.map(str::to_string)),
    };
    let mut exe = exe;
    let resolved = crate::gamefiles::game_exe(install_path, &exe);
    if !resolved.is_empty() && Path::new(&resolved).is_file() {
        exe = resolved;
    }
    if contains_ci(&exe, "Shipping") {
        return Some(exe);
    }
    let name = file_name(Path::new(&exe));
    let ignored_helper = is_ignored_exe(&name);
    let current_len = file_len(Path::new(&exe));
    if ignored_helper || current_len < 10 * 1024 * 1024 {
        if let Some(found) = find_executable(install_path) {
            let fl = file_len(Path::new(&found));
            if Path::new(&found).is_file() && (ignored_helper || (fl >= 25 * 1024 * 1024 && fl >= current_len * 4)) {
                return Some(found);
            }
        }
    }
    Some(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_unreal_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Game");
        let bin = root.join("Proj").join("Binaries").join("Win64");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(root.join("Engine")).unwrap();
        let exe = bin.join("Proj-Win64-Shipping.exe");
        fs::write(&exe, b"MZ").unwrap();
        assert_eq!(infer_install_root(&exe.to_string_lossy(), None), root.to_string_lossy());
        assert_eq!(find_executable(&root.to_string_lossy()).unwrap(), exe.to_string_lossy());
    }

    #[test]
    fn non_unreal_paks() {
        assert!(is_non_unreal_pak("/g/locales/en-US.pak"));
        assert!(is_non_unreal_pak("/g/es-419.pak"));
        assert!(!is_non_unreal_pak("/g/Content/Paks/pakchunk0-Windows.pak"));
    }

    #[test]
    fn detects_unity() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("Game_Data")).unwrap();
        fs::write(dir.path().join("Game.exe"), b"MZ").unwrap();
        assert_eq!(detect(&dir.path().to_string_lossy()).0, GameEngine::Unity);
        assert_eq!(find_executable(&dir.path().to_string_lossy()).unwrap(), dir.path().join("Game.exe").to_string_lossy());
    }
}
