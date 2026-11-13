//! Proton integration (Drivers/ProtonRunner.cs): which Proton a game uses, running the warmer inside the game's
//! prefix, the runtime / build stamps behind stale detection, and putting the compiled caches where the game reads them.

use crate::models::{AntiCheat, GameInfo, GameSource, WarmerEvent};
use crate::nvcache::{self, DEFAULT_MAX_NVIDIA_PART_BYTES};
use crate::paths::{canonical_path, data_dir, home_dir, xdg_config_home, xdg_data_home};
use crate::recorder;
use crate::settings::{self, AppSettings};
use crate::util::{contains_ci, ends_with_ci, file_len, file_name, files_recursive, mtime_ticks, non_empty, subdirs};
use regex::{Regex, RegexBuilder};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub const MAX_PIPELINES_PER_WARMER_BATCH: usize = 40_000;
const INSTALLED_MANIFEST: &str = "installed-files.txt";
const NON_STEAM_DRIVER_CACHE_SUBDIRS: [&str; 5] = ["AMDv1", "mesa_shader_cache_sf", "mesa_shader_cache_db", "mesa_shader_cache", "DXVK_state_cache"];

/// A cancellation flag shared with long operations.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst)
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn p(s: &str) -> &Path {
    Path::new(s)
}

pub fn is_flatpak_game(id: Option<&str>, install: Option<&str>, prefix: Option<&str>) -> bool {
    id.map(|i| crate::util::starts_with_ci(i, "flatpak_")).unwrap_or(false)
        || install.map(|i| i.contains("/.var/app/")).unwrap_or(false)
        || prefix.map(|i| i.contains("/.var/app/")).unwrap_or(false)
}

pub fn resolve_steam_root(id: Option<&str>, install: Option<&str>, prefix: Option<&str>) -> PathBuf {
    let home = home_dir();
    let flatpak = home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
    if is_flatpak_game(id, install, prefix) && flatpak.is_dir() {
        return flatpak;
    }
    let local = home.join(".local/share/Steam");
    for c in [xdg_data_home().join("Steam"), local.clone(), home.join(".steam/steam"), flatpak] {
        if c.is_dir() {
            return c;
        }
    }
    local
}

fn steam_root_of(game: &GameInfo) -> PathBuf {
    resolve_steam_root(Some(&game.id), Some(&game.install_path), game.prefix_path.as_deref())
}

pub fn all_steam_roots() -> Vec<PathBuf> {
    let home = home_dir();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for c in [xdg_data_home().join("Steam"), home.join(".local/share/Steam"), home.join(".steam/steam"), home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam")] {
        if c.is_dir() && seen.insert(canonical_path(&c)) {
            out.push(c);
        }
    }
    out
}

pub fn all_steam_shader_cache_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for r in all_steam_roots() {
        let sc = r.join("steamapps/shadercache");
        if sc.is_dir() && seen.insert(canonical_path(&sc)) {
            out.push(sc);
        }
    }
    out
}

pub fn steam_shader_cache_path(game_id: &str, install: Option<&str>, prefix: Option<&str>) -> PathBuf {
    resolve_steam_root(Some(game_id), install, prefix).join("steamapps/shadercache").join(crate::models::raw_id_of(game_id))
}

pub fn non_steam_shader_cache_path(game_id: &str, prefix: Option<&str>) -> PathBuf {
    match prefix.filter(|p| !p.is_empty()) {
        Some(pfx) => Path::new(pfx).join("shadercache").join(game_id),
        None => data_dir().join("prefixes").join(game_id).join("shadercache").join(game_id),
    }
}

/// Where the compile writes this game's caches.
pub fn target_cache_path(game: &GameInfo) -> PathBuf {
    if game.source == GameSource::Steam {
        steam_shader_cache_path(&game.id, Some(&game.install_path), game.prefix_path.as_deref())
    } else {
        non_steam_shader_cache_path(&game.id, game.prefix_path.as_deref())
    }
}

pub fn flatpak_launcher_nvidia_gl_cache_path(source: GameSource) -> Option<PathBuf> {
    let app = match source {
        GameSource::Steam => "com.valvesoftware.Steam",
        GameSource::Heroic => "com.heroicgameslauncher.hgl",
        GameSource::Lutris => "net.lutris.Lutris",
        _ => return None,
    };
    let root = home_dir().join(".var/app").join(app);
    if !root.is_dir() {
        return None;
    }
    let cache = if source == GameSource::Steam { root.join(".cache") } else { root.join("cache") };
    Some(cache.join("nvidia/GLCache"))
}

/// A file inside a shadercache folder that is a real compiled GPU/vkd3d/DXVK cache, not a Fossilize download or metadata.
pub fn is_compiled_shader_cache_file(path: &Path) -> bool {
    let norm = path.to_string_lossy();
    if contains_ci(&norm, "/fozpipelines") || contains_ci(&norm, "/fozmedia") {
        return false;
    }
    let name = file_name(path);
    !(ends_with_ci(&name, ".stamp") || ends_with_ci(&name, ".txt") || name.eq_ignore_ascii_case("TOUCH"))
}

/// Lexical normalization (Path.GetFullPath without touching the disk).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn wine_lib_dir(proton_dir: &Path) -> Option<PathBuf> {
    ["files/lib64/wine", "dist/lib64/wine", "files/lib/wine", "dist/lib/wine"].iter().map(|s| proton_dir.join(s)).find(|d| d.is_dir())
}

// ---- which Proton ---------------------------------------------------------------------------------------------------

pub fn find_proton_binary_for_game(game: &GameInfo) -> Option<String> {
    let raw = game.raw_id().to_string();
    let home = home_dir();
    match game.source {
        GameSource::Steam if !raw.is_empty() => {
            if let Some(r) = resolve_steam_proton_for_app(&steam_root_of(game), &raw, game.prefix_path.as_deref(), true) {
                return Some(r);
            }
        }
        GameSource::Heroic if !raw.is_empty() => {
            let native = [xdg_config_home().join("heroic"), home.join(".config/heroic")];
            let flat = home.join(".var/app/com.heroicgameslauncher.hgl/config/heroic");
            let mut dirs: Vec<PathBuf> = if game.is_flatpak {
                std::iter::once(flat).chain(native).collect()
            } else {
                native.into_iter().chain(std::iter::once(flat)).collect()
            };
            dirs.dedup();
            let mut seen = HashSet::new();
            for h in dirs.into_iter().filter(|d| seen.insert(d.clone())) {
                if let Ok(t) = fs::read_to_string(h.join("GamesConfig").join(format!("{raw}.json"))) {
                    if let Some(b) = crate::sources::heroic::read_proton_binary(&t, &raw).filter(|b| p(b).is_file()) {
                        return Some(b);
                    }
                }
                if let Ok(t) = fs::read_to_string(h.join("config.json")) {
                    if let Some(b) = crate::sources::heroic::read_proton_binary(&t, "defaultSettings").filter(|b| p(b).is_file()) {
                        return Some(b);
                    }
                }
                let tools = h.join("tools/proton");
                let mut ds = subdirs(&tools);
                ds.reverse();
                if let Some(x) = ds.into_iter().map(|d| d.join("proton")).find(|x| x.is_file()) {
                    return Some(x.to_string_lossy().into_owned());
                }
            }
        }
        GameSource::Lutris => {
            if let Some(pfx) = game.prefix_path.as_deref().filter(|x| !x.is_empty()) {
                let parent = p(pfx.trim_end_matches('/')).parent().map(Path::to_path_buf).unwrap_or_default();
                for cfg in [p(pfx).join("config_info"), parent.join("config_info")] {
                    if let Ok(text) = fs::read_to_string(&cfg) {
                        for line in text.lines() {
                            if let Some(i) = line.find("/files/").filter(|i| *i > 0) {
                                let bin = Path::new(&line[..i]).join("proton");
                                if bin.is_file() {
                                    return Some(bin.to_string_lossy().into_owned());
                                }
                            }
                        }
                    }
                }
            }
            let flat = [
                home.join(".var/app/net.lutris.Lutris/data/lutris/runners/proton"),
                home.join(".var/app/net.lutris.Lutris/data/lutris/runners/wine"),
            ];
            let native = [
                xdg_data_home().join("lutris/runners/proton"),
                xdg_data_home().join("lutris/runners/wine"),
                home.join(".local/share/lutris/runners/proton"),
                home.join(".local/share/lutris/runners/wine"),
            ];
            let dirs: Vec<PathBuf> =
                if game.is_flatpak { flat.into_iter().chain(native).collect() } else { native.into_iter().chain(flat).collect() };
            let mut seen = HashSet::new();
            for r in dirs.into_iter().filter(|d| seen.insert(d.clone())) {
                let mut ds = subdirs(&r);
                ds.reverse();
                if let Some(x) = ds.into_iter().map(|d| d.join("proton")).find(|x| x.is_file()) {
                    return Some(x.to_string_lossy().into_owned());
                }
            }
        }
        _ => {}
    }
    find_proton_binary(None)
}

fn re_name() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?s)"name"\s+"([^"]*)""#).unwrap())
}

/// A `"name" { ... }` block of a VDF text, braces included.
pub fn extract_vdf_block(vdf: &str, block: &str) -> Option<String> {
    let re = RegexBuilder::new(&format!(r#""{}"\s*\{{"#, regex::escape(block))).case_insensitive(true).build().ok()?;
    let m = re.find(vdf)?;
    let bytes = vdf.as_bytes();
    let start = m.end() - 1;
    let mut depth = 0i32;
    let mut in_quotes = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if c == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            in_quotes = !in_quotes;
        } else if !in_quotes {
            if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    return Some(vdf[start..=i].to_string());
                }
            }
        }
    }
    None
}

pub fn steam_library_folders(steam_root: &Path) -> Vec<PathBuf> {
    let mut libs = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |x: PathBuf| {
        if x.is_dir() && seen.insert(canonical_path(&x)) {
            libs.push(x);
        }
    };
    add(steam_root.to_path_buf());
    for v in [steam_root.join("steamapps/libraryfolders.vdf"), steam_root.join("config/libraryfolders.vdf")] {
        if let Ok(c) = fs::read_to_string(&v) {
            for l in crate::sources::steam::library_paths(&c) {
                add(PathBuf::from(l));
            }
        }
    }
    libs
}

pub fn try_read_proton_binary_from_config_info(config_info: &Path) -> Option<String> {
    let text = fs::read_to_string(config_info).ok()?;
    for line in text.lines() {
        let idx = line.find("/files/").or_else(|| line.find("/dist/"));
        if let Some(i) = idx.filter(|i| *i > 0) {
            let bin = Path::new(&line[..i]).join("proton");
            if bin.is_file() {
                return Some(bin.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn compat_dir_of(prefix: &str) -> PathBuf {
    let t = prefix.trim_end_matches('/');
    if t.ends_with("/pfx") {
        p(t).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(t))
    } else {
        PathBuf::from(t)
    }
}

pub fn does_prefix_match_proton(compat_data: &Path, proton: &str) -> bool {
    if compat_data.as_os_str().is_empty() || proton.is_empty() {
        return false;
    }
    let dir = compat_dir_of(&compat_data.to_string_lossy());
    match try_read_proton_binary_from_config_info(&dir.join("config_info")) {
        Some(pp) => canonical_path(p(&pp)) == canonical_path(p(proton)),
        None => false,
    }
}

pub fn resolve_steam_proton_for_app(steam_root: &Path, raw_id: &str, prefix: Option<&str>, allow_system: bool) -> Option<String> {
    if raw_id.trim().is_empty() {
        return None;
    }
    let libs = steam_library_folders(steam_root);
    let compat_block = fs::read_to_string(steam_root.join("config/config.vdf")).ok().and_then(|v| extract_vdf_block(&v, "CompatToolMapping"));

    // 1. the per-game override in CompatToolMapping always wins
    let mut override_cleared = false;
    if let Some(block) = &compat_block {
        if let Some(app) = extract_vdf_block(block, raw_id) {
            if let Some(c) = re_name().captures(&app) {
                let tool = c[1].trim().to_string();
                if !tool.is_empty() {
                    if let Some(r) = resolve_compat_tool_name(&tool, steam_root, Some(&libs), allow_system) {
                        return Some(r);
                    }
                } else {
                    override_cleared = true; // "Force the use of a specific Steam Play compatibility tool" unticked
                }
            }
        }
    }
    // 2. an override explicitly cleared: the global default
    if override_cleared {
        if let Some(r) = compat_block.as_deref().and_then(|b| default_compat_tool(b, steam_root, &libs, allow_system)) {
            return Some(r);
        }
    }
    // 3. compatdata/<AppID>/config_info
    let mut infos: Vec<PathBuf> = Vec::new();
    if let Some(pfx) = prefix.filter(|x| !x.is_empty()) {
        infos.push(compat_dir_of(pfx).join("config_info"));
    }
    for l in &libs {
        infos.push(l.join("steamapps/compatdata").join(raw_id).join("config_info"));
    }
    let mut seen = HashSet::new();
    for ci in infos.into_iter().filter(|x| seen.insert(x.clone())) {
        if let Some(r) = try_read_proton_binary_from_config_info(&ci) {
            return Some(r);
        }
    }
    // 4. the global default for a game without a prefix yet
    if let Some(r) = compat_block.as_deref().and_then(|b| default_compat_tool(b, steam_root, &libs, allow_system)) {
        return Some(r);
    }
    // 5. any Proton in this Steam root / its libraries
    let mut cands = Vec::new();
    let mut tools = subdirs(&steam_root.join("compatibilitytools.d"));
    tools.reverse();
    cands.extend(tools.into_iter().map(|d| d.join("proton")));
    for l in &libs {
        let common = l.join("steamapps/common");
        if !common.is_dir() {
            continue;
        }
        for n in ["Proton - Experimental", "Proton Hotfix", "Proton 10.0 (Beta)", "Proton 10.0", "Proton 9.0 (Beta)", "Proton 9.0", "Proton 8.0"] {
            cands.push(common.join(n).join("proton"));
        }
        let mut ps: Vec<PathBuf> = subdirs(&common).into_iter().filter(|d| file_name(d).starts_with("Proton")).collect();
        ps.reverse();
        cands.extend(ps.into_iter().map(|d| d.join("proton")));
    }
    cands.into_iter().find(|c| c.is_file()).map(|c| c.to_string_lossy().into_owned())
}

fn default_compat_tool(block: &str, steam_root: &Path, libs: &[PathBuf], allow_system: bool) -> Option<String> {
    let d = extract_vdf_block(block, "0")?;
    let tool = re_name().captures(&d)?[1].trim().to_string();
    if tool.is_empty() {
        None
    } else {
        resolve_compat_tool_name(&tool, steam_root, Some(libs), allow_system)
    }
}

pub fn resolve_compat_tool_name(tool: &str, steam_root: &Path, libs: Option<&[PathBuf]>, allow_system: bool) -> Option<String> {
    if tool.trim().is_empty() {
        return None;
    }
    let owned;
    let libs = match libs {
        Some(l) => l,
        None => {
            owned = steam_library_folders(steam_root);
            &owned
        }
    };
    let home = home_dir();
    let mut compat_dirs = vec![steam_root.join("compatibilitytools.d")];
    if allow_system {
        compat_dirs.push(home.join(".local/share/Steam/compatibilitytools.d"));
        compat_dirs.push(home.join(".steam/root/compatibilitytools.d"));
        compat_dirs.push(PathBuf::from("/usr/share/steam/compatibilitytools.d"));
        compat_dirs.push(PathBuf::from("/usr/local/share/steam/compatibilitytools.d"));
    }
    let mut seen = HashSet::new();
    compat_dirs.retain(|d| seen.insert(d.clone()));

    // 1. a folder of that name
    for d in &compat_dirs {
        let c = d.join(tool).join("proton");
        if c.is_file() {
            return Some(c.to_string_lossy().into_owned());
        }
    }
    for l in libs {
        let c = l.join("steamapps/common").join(tool).join("proton");
        if c.is_file() {
            return Some(c.to_string_lossy().into_owned());
        }
    }
    // 2. compatibilitytool.vdf manifests (internal tool ids like proton_experimental, custom AUR tools)
    let mut roots: Vec<PathBuf> = Vec::new();
    for d in &compat_dirs {
        roots.extend(subdirs(d));
    }
    for l in libs {
        roots.extend(subdirs(&l.join("steamapps/common")).into_iter().filter(|d| file_name(d).starts_with("Proton")));
    }
    let mut seen = HashSet::new();
    let install_re = Regex::new(r#""install_path"\s+"([^"]+)""#).unwrap();
    for td in roots.into_iter().filter(|d| seen.insert(d.clone())) {
        let Ok(text) = fs::read_to_string(td.join("compatibilitytool.vdf")) else { continue };
        let tools_block = extract_vdf_block(&text, "compat_tools").unwrap_or_else(|| text.clone());
        if let Some(tb) = extract_vdf_block(&tools_block, tool) {
            let rel = install_re.captures(&tb).map(|c| c[1].to_string()).unwrap_or_else(|| ".".into());
            let resolved = normalize(&td.join(rel).join("proton"));
            if resolved.is_file() {
                return Some(resolved.to_string_lossy().into_owned());
            }
        }
    }
    // 3. Valve's own names
    let ver = Regex::new(r"(?i)^proton_(\d+)").unwrap().captures(tool).map(|c| c[1].to_string());
    let mut cands = Vec::new();
    for l in libs {
        let common = l.join("steamapps/common");
        if !common.is_dir() {
            continue;
        }
        if tool.eq_ignore_ascii_case("proton_experimental") {
            cands.push(common.join("Proton - Experimental/proton"));
        } else if tool.eq_ignore_ascii_case("proton_hotfix") {
            cands.push(common.join("Proton Hotfix/proton"));
        } else if tool.eq_ignore_ascii_case("proton_next") {
            cands.push(common.join("Proton Next/proton"));
        } else if let Some(major) = &ver {
            cands.push(common.join(format!("Proton {major}.0 (Beta)")).join("proton"));
            cands.push(common.join(format!("Proton {major}.0")).join("proton"));
            let mut ds: Vec<PathBuf> = subdirs(&common).into_iter().filter(|d| file_name(d).starts_with(&format!("Proton {major}"))).collect();
            ds.reverse();
            cands.extend(ds.into_iter().map(|d| d.join("proton")));
        }
    }
    cands.into_iter().find(|c| c.is_file()).map(|c| c.to_string_lossy().into_owned())
}

pub fn find_proton_binary(preferred: Option<&str>) -> Option<String> {
    let home = home_dir();
    let roots = all_steam_roots();
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(pv) = preferred.filter(|x| !x.is_empty()) {
        for r in &roots {
            cands.push(r.join("compatibilitytools.d").join(pv).join("proton"));
        }
        cands.push(Path::new("/usr/share/steam/compatibilitytools.d").join(pv).join("proton"));
    }
    let rev = |d: &Path| {
        let mut v = subdirs(d);
        v.reverse();
        v
    };
    for r in &roots {
        cands.extend(rev(&r.join("compatibilitytools.d")).into_iter().map(|d| d.join("proton")));
    }
    let mut seen = HashSet::new();
    for h in [xdg_config_home().join("heroic/tools/proton"), home.join(".config/heroic/tools/proton"), home.join(".var/app/com.heroicgameslauncher.hgl/config/heroic/tools/proton")] {
        if seen.insert(h.clone()) {
            cands.extend(rev(&h).into_iter().map(|d| d.join("proton")));
        }
    }
    cands.extend(rev(Path::new("/usr/share/steam/compatibilitytools.d")).into_iter().map(|d| d.join("proton")));
    for r in &roots {
        let common = r.join("steamapps/common");
        if common.is_dir() {
            for n in ["Proton - Experimental", "Proton 9.0 (Beta)", "Proton 9.0", "Proton 8.0"] {
                cands.push(common.join(n).join("proton"));
            }
            let mut ps: Vec<PathBuf> = subdirs(&common).into_iter().filter(|d| file_name(d).starts_with("Proton")).collect();
            ps.reverse();
            cands.extend(ps.into_iter().map(|d| d.join("proton")));
        }
    }
    cands.push(PathBuf::from("/usr/bin/proton"));
    cands.into_iter().find(|c| c.is_file()).map(|c| c.to_string_lossy().into_owned())
}

pub fn find_warmer_binary() -> Option<PathBuf> {
    crate::paths::find_native("shaderless_warm.exe")
}

static VDF_CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, (i64, String)>>> = OnceLock::new();

/// A cheap signature of launcher Proton assignments (Steam's CompatToolMapping + Heroic GamesConfig timestamps), so
/// the open window notices a Proton switch.
pub fn launcher_proton_config_signature() -> String {
    let mut sb = String::new();
    let cache = VDF_CACHE.get_or_init(|| Mutex::new(Default::default()));
    for sr in all_steam_roots() {
        let cfg = sr.join("config/config.vdf");
        if !cfg.is_file() {
            continue;
        }
        let ticks = mtime_ticks(&cfg);
        let mut c = cache.lock().unwrap();
        let block = match c.get(&cfg) {
            Some((t, b)) if *t == ticks => b.clone(),
            _ => {
                let b = fs::read_to_string(&cfg).ok().and_then(|v| extract_vdf_block(&v, "CompatToolMapping")).unwrap_or_default();
                c.insert(cfg.clone(), (ticks, b.clone()));
                b
            }
        };
        sb.push_str(&format!("S:{}={};", sr.display(), block));
    }
    let home = home_dir();
    let mut seen = HashSet::new();
    for h in [xdg_config_home().join("heroic/GamesConfig"), home.join(".config/heroic/GamesConfig"), home.join(".var/app/com.heroicgameslauncher.hgl/config/heroic/GamesConfig")] {
        if !seen.insert(h.clone()) || !h.is_dir() {
            continue;
        }
        let mut max = 0i64;
        let mut count = 0;
        for f in crate::util::files_in(&h).into_iter().filter(|f| f.extension().map(|e| e == "json").unwrap_or(false)) {
            count += 1;
            max = max.max(mtime_ticks(&f));
        }
        sb.push_str(&format!("H:{}={}:{};", h.display(), count, max));
    }
    sb
}

// ---- stamps ---------------------------------------------------------------------------------------------------------

fn nvidia_kernel_version() -> String {
    fs::read_to_string("/sys/module/nvidia/version").map(|s| s.trim().to_string()).unwrap_or_else(|_| "default".into())
}

pub fn proton_stamp(game: &GameInfo) -> String {
    let Some(proton) = find_proton_binary_for_game(game) else { return "unknown".into() };
    let lib = p(&proton).parent().and_then(wine_lib_dir);
    let (mut vkd3d, mut dxvk) = (0, 0);
    if let Some(l) = lib {
        vkd3d = file_len(&l.join("vkd3d-proton/x86_64-windows/d3d12core.dll"));
        dxvk = file_len(&l.join("dxvk/x86_64-windows/d3d11.dll"));
    }
    format!("{proton}|{vkd3d}|{dxvk}")
}

pub fn runtime_cache_stamp_mismatch_reason(game: &GameInfo) -> Option<String> {
    let target = target_cache_path(game);
    let stamp = target.join("shaderless_runtime.stamp");
    let legacy = target.join("scskiller_runtime.stamp");
    if !stamp.is_file() && legacy.is_file() {
        let _ = fs::rename(&legacy, &stamp);
    }
    if !stamp.is_file() {
        return None;
    }
    let proton = find_proton_binary_for_game(game)?;
    let core_sig = d3d12core_sig(game, &proton);
    let nv = nvidia_kernel_version();
    let base = format!("{proton}|{core_sig}");
    let current = format!("{base}|{nv}");
    let old = fs::read_to_string(&stamp).ok()?.trim().to_string();
    if old == current || old == base {
        return None;
    }
    let parts: Vec<&str> = old.split('|').collect();
    if parts.len() >= 3 {
        let old_nv = parts[2];
        if !old_nv.eq_ignore_ascii_case("default") && !nv.eq_ignore_ascii_case("default") && old_nv != nv {
            return Some("driver updated".into());
        }
    }
    Some("Proton updated".into())
}

pub fn is_runtime_cache_stamp_valid(game: &GameInfo) -> bool {
    runtime_cache_stamp_mismatch_reason(game).is_none()
}

/// Shaderless itself compiled and stamped caches for this game (not Steam's own downloaded or gameplay caches).
pub fn has_shaderless_compiled_artifacts(game: &GameInfo) -> bool {
    let target = target_cache_path(game);
    if target.is_dir()
        && (target.join("shaderless_runtime.stamp").is_file() || target.join("scskiller_runtime.stamp").is_file() || target.join(INSTALLED_MANIFEST).is_file())
    {
        return true;
    }
    recorder::warmed_keys_path(&game.id).is_file() || recorder::game_dir(&game.id).join(crate::recordings::WARMED_FILE).is_file()
}

pub fn game_build_stamp(game: &GameInfo) -> String {
    let mut exe = game.executable_path.clone();
    if exe.is_empty() && p(&game.install_path).is_dir() {
        exe = crate::engine_detector::find_executable(&game.install_path).unwrap_or_default();
    }
    let (mut exe_len, mut exe_ticks) = (0u64, 0i64);
    if !exe.is_empty() && p(&exe).is_file() {
        exe_len = file_len(p(&exe));
        exe_ticks = mtime_ticks(p(&exe));
    }
    let (mut count, mut latest, mut total) = (0i64, 0i64, 0u64);
    if !game.install_path.is_empty() && p(&game.install_path).is_dir() {
        for f in crate::util::files_recursive_depth(p(&game.install_path), 4) {
            let ext = f.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if matches!(ext.as_str(), "pak" | "utoc" | "ucas" | "bundle" | "archive" | "ba2") {
                count += 1;
                total += file_len(&f);
                latest = latest.max(mtime_ticks(&f));
            }
        }
    }
    format!("{exe_len}:{exe_ticks}|{count}:{total}:{latest}")
}

pub fn latest_game_content_write_utc(stamp: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = stamp.filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("unknown"))?;
    let parts: Vec<&str> = s.split('|').collect();
    let mut max = 0i64;
    if let Some(e) = parts.first() {
        let ep: Vec<&str> = e.split(':').collect();
        if ep.len() == 2 {
            if let Ok(t) = ep[1].parse::<i64>() {
                max = max.max(t);
            }
        }
    }
    if let Some(a) = parts.get(1) {
        let ap: Vec<&str> = a.split(':').collect();
        if ap.len() == 3 {
            if let Ok(t) = ap[2].parse::<i64>() {
                max = max.max(t);
            }
        }
    }
    (max > 0).then(|| crate::util::ticks_to_utc(max)).flatten()
}

fn is_low_latency_configured(game: &GameInfo, var: &str) -> bool {
    if std::env::var(var).map(|v| v == "1").unwrap_or(false) {
        return true;
    }
    let home = home_dir();
    let mut confs = Vec::new();
    if !game.name.is_empty() {
        confs.push(home.join(".local/share/goverlay/gameconfig").join(&game.name).join("bgmod.conf"));
    }
    confs.push(home.join(".local/share/goverlay/gameconfig/global/bgmod.conf"));
    let re = Regex::new(&format!(r"(?m)^\s*{}\s*=\s*1\b", regex::escape(var))).unwrap();
    confs.iter().any(|c| fs::read_to_string(c).map(|t| re.is_match(&t)).unwrap_or(false))
}

// ---- the warmer -----------------------------------------------------------------------------------------------------

/// Runs the warmer for the work folder's shaderless.db / shaderless_gen.db, split into batches of at most
/// `max_items_per_batch` pipelines so one NVIDIA writable bucket never nears 2 GiB. Returns whether every pass exited 0.
#[allow(clippy::too_many_arguments)]
pub fn run_warmer(
    game: &GameInfo,
    work_dir: &Path,
    on_event: &mut dyn FnMut(WarmerEvent),
    cancel: &CancelToken,
    is_incremental: bool,
    max_items_per_batch: usize,
    max_nvidia_part_bytes: i64,
    preserve_existing_cache: bool,
) -> bool {
    let rec_db = work_dir.join("shaderless.db");
    let gen_db = work_dir.join("shaderless_gen.db");
    if max_items_per_batch > 0 && recorder::count_executable_items(&rec_db, &gen_db) > max_items_per_batch {
        let batch_root = work_dir.join("_warmer_batches");
        let slices = recorder::split_databases_into_batches(&rec_db, &gen_db, &batch_root, max_items_per_batch);
        if slices.len() > 1 {
            let rec_bak = work_dir.join("shaderless.db.full_bak");
            let gen_bak = work_dir.join("shaderless_gen.db.full_bak");
            let had_rec = rec_db.is_file();
            let had_gen = gen_db.is_file();
            let result = (|| -> bool {
                if had_rec && fs::rename(&rec_db, &rec_bak).is_err() {
                    return false;
                }
                if had_gen && fs::rename(&gen_db, &gen_bak).is_err() {
                    return false;
                }
                let grand: i32 = slices.iter().map(|s| s.executable_item_count as i32).sum();
                let (mut cum_processed, mut cum_succeeded, mut cum_failed) = (0i32, 0i32, 0i32);
                let mut all_ok = true;
                for (b, slice) in slices.iter().enumerate() {
                    if cancel.is_cancelled() {
                        return false;
                    }
                    if fs::copy(&slice.batch_db_path, &rec_db).is_err() {
                        all_ok = false;
                        break;
                    }
                    let _ = fs::remove_file(&gen_db);
                    let first = b == 0;
                    let last = b == slices.len() - 1;
                    let (mut bp, mut bs, mut bf) = (0i32, 0i32, 0i32);
                    let skip = slice.skip_prefix_count as i32;
                    let exec = slice.executable_item_count as i32;
                    let ok = run_single_warmer_pass(
                        game,
                        work_dir,
                        &mut |ev: WarmerEvent| match ev.event.as_str() {
                            "init" => {
                                if first {
                                    on_event(ev)
                                }
                            }
                            "start" => {
                                if first {
                                    on_event(WarmerEvent { event: "start".into(), total: Some(grand), ..Default::default() })
                                }
                            }
                            "error" => on_event(ev),
                            "progress" => {
                                let adj = (ev.done.unwrap_or(0) - skip).clamp(0, exec);
                                let f = ev.failed.unwrap_or(0).max(0);
                                bp = adj;
                                bf = f;
                                bs = (adj - f).max(0);
                                on_event(WarmerEvent {
                                    event: "progress".into(),
                                    done: Some(cum_processed + adj),
                                    total: Some(grand),
                                    failed: Some(cum_failed + f),
                                    ..Default::default()
                                });
                            }
                            "done" => {
                                let f = ev.failed.unwrap_or(0).max(0);
                                let s = (ev.done.unwrap_or(0) - skip).clamp(0, exec);
                                bs = s;
                                bf = f;
                                bp = exec.min(s + f);
                            }
                            _ => {}
                        },
                        cancel,
                        if first { is_incremental } else { true },
                        slice.skip_prefix_count,
                        max_nvidia_part_bytes,
                        preserve_existing_cache,
                    );
                    cum_processed += bp;
                    cum_succeeded += bs;
                    cum_failed += bf;
                    if !ok {
                        all_ok = false;
                        break;
                    }
                    if !last {
                        on_event(WarmerEvent { event: "progress".into(), done: Some(cum_processed), total: Some(grand), failed: Some(cum_failed), ..Default::default() });
                    }
                }
                on_event(WarmerEvent { event: "done".into(), done: Some(cum_succeeded), total: Some(grand), failed: Some(cum_failed), ..Default::default() });
                all_ok
            })();
            let _ = fs::remove_file(&rec_db);
            if had_rec && rec_bak.is_file() {
                let _ = fs::rename(&rec_bak, &rec_db);
            }
            if had_gen && gen_bak.is_file() {
                let _ = fs::rename(&gen_bak, &gen_db);
            }
            let _ = fs::remove_dir_all(&batch_root);
            return result;
        }
    }
    run_single_warmer_pass(game, work_dir, on_event, cancel, is_incremental, 0, max_nvidia_part_bytes, preserve_existing_cache)
}

fn sha256_12(s: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(s.as_bytes());
    hex::encode(h)[..12].to_string()
}

#[allow(clippy::too_many_arguments)]
fn run_single_warmer_pass(
    game: &GameInfo,
    work_dir: &Path,
    on_event: &mut dyn FnMut(WarmerEvent),
    cancel: &CancelToken,
    is_incremental: bool,
    start_at: usize,
    max_nvidia_part_bytes: i64,
    preserve_existing_cache: bool,
) -> bool {
    let Some(warmer) = find_warmer_binary() else {
        on_event(WarmerEvent::error("Native warmer executable (shaderless_warm.exe) not found."));
        return false;
    };
    let Some(proton) = find_proton_binary_for_game(game) else {
        on_event(WarmerEvent::error("Proton runtime not found on this system."));
        return false;
    };
    let compat_client = steam_root_of(game);
    let raw = game.raw_id().to_string();
    let mut compat_data = game_compat_dir(game);
    let game_compat = compat_data.clone();
    let mut initialized = compat_data.join("pfx/system.reg").is_file() || compat_data.join("system.reg").is_file();
    let matches = initialized && (!compat_data.join("config_info").is_file() || does_prefix_match_proton(&compat_data, &proton));
    let mut isolated = false;
    if !initialized || !matches {
        compat_data = data_dir().join("proton_prefix").join(sha256_12(&canonical_path(p(&proton))).to_ascii_lowercase());
        isolated = true;
        initialized = compat_data.join("pfx/system.reg").is_file()
            && (!compat_data.join("config_info").is_file() || does_prefix_match_proton(&compat_data, &proton));
    }
    let _ = fs::create_dir_all(&compat_data);
    if !initialized {
        initialize_proton_prefix_headless(&proton, &compat_client, &compat_data, work_dir, cancel);
        if cancel.is_cancelled() {
            return false;
        }
    }

    let target = target_cache_path(game);
    let vkd3d = target.join("VKD3D_shader_cache");
    let dxvk = target.join("DXVK_state_cache");
    let nv_base = target.join("nvidiav1");
    let gl = nv_base.join("GLCache");
    let amd = target.join("AMDv1");
    for d in [&target, &nv_base, &vkd3d, &dxvk, &gl, &amd] {
        let _ = fs::create_dir_all(d);
    }

    let resolved_exe = crate::engine_detector::resolve_real_game_executable(&game.install_path, Some(&game.executable_path))
        .unwrap_or_else(|| game.executable_path.clone());
    let exe_name = if !resolved_exe.is_empty() && ends_with_ci(&resolved_exe, ".exe") { file_name(p(&resolved_exe)) } else { "game.exe".into() };
    let game_exe_dir = crate::util::parent_str(&resolved_exe);

    // the game's Agility SDK D3D12 folder, linked into the work folder
    if let Some(ged) = &game_exe_dir {
        let src = p(ged).join("D3D12");
        let dst = work_dir.join("D3D12");
        if src.is_dir() && !dst.exists() && std::os::unix::fs::symlink(&src, &dst).is_err() {
            let _ = crate::util::copy_dir(&src, &dst);
        }
    }

    let staged = stage_proton_runtime_dlls(game, &proton, &game_compat, &compat_data, work_dir);
    let anti_cheat = recorder::detect_anti_cheat(game) != AntiCheat::None;
    prepare_driver_caches_for_warmer(
        &target,
        &vkd3d,
        &gl,
        &proton,
        &d3d12core_sig(game, &proton),
        is_incremental,
        if anti_cheat { None } else { game_exe_dir.as_deref() },
        max_nvidia_part_bytes,
        if anti_cheat { None } else { Some(&game.install_path) },
        preserve_existing_cache,
    );

    // the warmer under the game's exe name, so vkd3d-proton and the driver key the cache identically
    let mut target_exe = work_dir.join(&exe_name);
    if fs::copy(&warmer, &target_exe).is_err() {
        target_exe = warmer.clone();
    }

    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).max(2);
    let s = settings::load();
    let ro_names = nvcache::build_nvidia_read_only_app_names(Some(&gl), None);
    let mut cmd = Command::new("nice");
    // "runinprefix" (direct wine) rather than "run" (wraps via steam.exe and shows a conhost window)
    cmd.arg("-n").arg("10").arg(&proton).arg("runinprefix").arg(&target_exe).arg(work_dir).arg(&exe_name).arg("--threads").arg(threads.to_string());
    if start_at > 0 {
        cmd.arg("--start").arg(start_at.to_string());
    }
    // debugging: STUTTERFREE_WARM_LOG=<file> keeps the warmer's stderr with vkd3d-proton's errors (why a pipeline failed)
    let warm_log = std::env::var_os("STUTTERFREE_WARM_LOG").and_then(|p| std::fs::File::create(p).ok());
    if warm_log.is_some() {
        cmd.env("VKD3D_DEBUG", "err").env("VKD3D_SHADER_DEBUG", "err");
    }
    let stderr = warm_log.map(Stdio::from).unwrap_or_else(Stdio::null);
    cmd.current_dir(work_dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(stderr).process_group(0);
    cmd.env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &compat_client)
        .env("STEAM_COMPAT_DATA_PATH", &compat_data)
        .env("STEAM_COMPAT_SHADER_PATH", &target);
    if game.source == GameSource::Steam {
        cmd.env("SteamAppId", &raw).env("SteamGameId", &raw);
    }
    cmd.env("WINEDEBUG", "-all")
        .env("WINEDLLOVERRIDES", "d3d12,d3d12core,dxgi,d3d11=n,b;winemenubuilder.exe=d;mscoree=d;mshtml=d")
        .env("PROTON_USE_XALIA", "0")
        .env("PROTONFIXES_DISABLE", "1");
    if isolated {
        for k in ["PROTON_FSR4_UPGRADE", "PROTON_DLSS_UPGRADE", "PROTON_XESS_UPGRADE", "PROTON_USE_OPTISCALER"] {
            cmd.env(k, "0");
        }
    }
    if is_low_latency_configured(game, "PROTON_VKD3D_LOWLATENCY") {
        cmd.env("PROTON_VKD3D_LOWLATENCY", "1");
    }
    if is_low_latency_configured(game, "PROTON_DXVK_LOWLATENCY") {
        cmd.env("PROTON_DXVK_LOWLATENCY", "1");
    }
    cmd.env("VKD3D_SHADER_CACHE_PATH", &vkd3d)
        .env("DXVK_STATE_CACHE_PATH", &dxvk)
        .env("__GL_SHADER_DISK_CACHE_PATH", &nv_base)
        .env("__GL_SHADER_DISK_CACHE", "1")
        .env("__GL_SHADER_DISK_CACHE_APP_NAME", "steamapp_shader_cache")
        .env("__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME", &ro_names)
        .env("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP", "1")
        .env("__GL_SHADER_DISK_CACHE_SIZE", s.max_cache_size_bytes().to_string())
        .env("AMD_VK_PIPELINE_CACHE_PATH", &amd)
        .env("AMD_VK_PIPELINE_CACHE_FILENAME", "steamapp_shader_cache")
        .env("AMD_VK_USE_PIPELINE_CACHE", "1")
        .env("MESA_SHADER_CACHE_DIR", &target)
        .env("MESA_SHADER_CACHE_MAX_SIZE", s.mesa_max_cache_size())
        .env("MESA_GLSL_CACHE_DIR", &target)
        .env("MESA_GLSL_CACHE_MAX_SIZE", s.mesa_max_cache_size());
    if game.source == GameSource::Steam {
        cmd.env("MESA_DISK_CACHE_SINGLE_FILE", "1");
    }

    let result = match cmd.spawn() {
        Err(e) => {
            on_event(WarmerEvent::error(e.to_string()));
            false
        }
        Ok(mut child) => {
            let pid = child.id() as i32;
            let done = Arc::new(AtomicBool::new(false));
            let watcher = {
                let done = done.clone();
                let cancel = cancel.clone();
                std::thread::spawn(move || {
                    while !done.load(Ordering::SeqCst) {
                        if cancel.is_cancelled() {
                            unsafe { libc::kill(-pid, libc::SIGKILL) };
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                })
            };
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if let Some(ev) = parse_warmer_line(&line) {
                        on_event(ev);
                    }
                }
            }
            let status = child.wait();
            done.store(true, Ordering::SeqCst);
            let _ = watcher.join();
            if cancel.is_cancelled() {
                false
            } else {
                // anti-cheat games: nothing copied next to the exe
                finalize_driver_caches(
                    &target,
                    &vkd3d,
                    &gl,
                    if anti_cheat { None } else { game_exe_dir.as_deref() },
                    max_nvidia_part_bytes,
                    if anti_cheat { None } else { Some(&game.install_path) },
                );
                install_caches_where_game_reads(game, &vkd3d, &gl);
                ensure_proton_local_shader_cache_setting(&game_compat, Some(&proton), None);
                if compat_data != game_compat {
                    ensure_proton_local_shader_cache_setting(&compat_data, Some(&proton), None);
                }
                match status {
                    Ok(s) if s.success() => true,
                    // say why: a batch that stops here ends the compile, which otherwise reads as done
                    Ok(s) => {
                        on_event(WarmerEvent::error(format!("the warmer stopped early ({s})")));
                        false
                    }
                    Err(e) => {
                        on_event(WarmerEvent::error(format!("the warmer couldn't be waited for: {e}")));
                        false
                    }
                }
            }
        }
    };
    if target_exe != warmer {
        let _ = fs::remove_file(&target_exe);
    }
    for d in staged {
        let _ = fs::remove_file(d);
    }
    result
}

/// One JSON line of the warmer's stdout.
pub fn parse_warmer_line(line: &str) -> Option<WarmerEvent> {
    let l = line.trim();
    if !(l.starts_with('{') && l.ends_with('}')) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(l).ok()?;
    let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).map(|x| x as i32);
    Some(WarmerEvent {
        event: v.get("event")?.as_str()?.to_string(),
        threads: i("threads"),
        done: i("done"),
        total: i("total"),
        failed: i("failed"),
        message: v.get("message").and_then(|m| m.as_str()).map(str::to_string),
    })
}

/// Initializes an isolated Proton prefix headlessly ("getcompatpath /"): no steam.exe, no Wine windows.
fn initialize_proton_prefix_headless(proton: &str, client: &Path, compat: &Path, work: &Path, cancel: &CancelToken) {
    let mut cmd = Command::new("nice");
    cmd.args(["-n", "10", proton, "getcompatpath", "/"])
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", client)
        .env("STEAM_COMPAT_DATA_PATH", compat)
        .env("WINEDEBUG", "-all")
        .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree=d;mshtml=d")
        .env("PROTON_USE_XALIA", "0")
        .env("PROTONFIXES_DISABLE", "1");
    for k in ["PROTON_FSR4_UPGRADE", "PROTON_DLSS_UPGRADE", "PROTON_XESS_UPGRADE", "PROTON_USE_OPTISCALER"] {
        cmd.env(k, "0");
    }
    if let Ok(mut child) = cmd.spawn() {
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {
                    if cancel.is_cancelled() {
                        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }
}

/// Puts the exact vkd3d-proton / DXVK DLLs of the game's Proton next to the warmer.
/// The game's STEAM_COMPAT_DATA_PATH (Proton wants the parent of 'pfx').
fn game_compat_dir(game: &GameInfo) -> PathBuf {
    match game.prefix_path.as_deref().filter(|x| !x.is_empty()) {
        Some(pfx) => compat_dir_of(pfx),
        None if game.source == GameSource::Steam => steam_root_of(game).join("steamapps/compatdata").join(game.raw_id()),
        None => data_dir().join("prefixes").join(&game.id),
    }
}

/// Which copy of a vkd3d-proton / DXVK DLL the warmer runs with: the low-latency build when configured, else the game
/// prefix's own when the prefix belongs to this Proton (just after a switch it holds the old one), else Proton's.
fn runtime_dll(game: &GameInfo, proton: &str, game_compat: &Path, dll: &str) -> Option<PathBuf> {
    let lib = wine_lib_dir(p(proton).parent()?);
    let vkd3d = dll.starts_with("d3d12");
    let (ll_var, ll_sub, sub) = if vkd3d {
        ("PROTON_VKD3D_LOWLATENCY", "vkd3d-low-latency/x86_64-windows", "vkd3d-proton/x86_64-windows")
    } else {
        ("PROTON_DXVK_LOWLATENCY", "dxvk-low-latency/x86_64-windows", "dxvk/x86_64-windows")
    };
    let prefer = is_low_latency_configured(game, ll_var);
    let sub = if prefer && lib.as_ref().map(|l| l.join(ll_sub).is_dir()).unwrap_or(false) { ll_sub } else { sub };
    let from_lib = || lib.as_ref().map(|l| l.join(sub).join(dll)).filter(|c| c.is_file());
    if prefer {
        if let Some(c) = from_lib() {
            return Some(c);
        }
    }
    let prefix_matches = !game_compat.join("config_info").is_file() || does_prefix_match_proton(game_compat, proton);
    let own = game_compat.join("pfx/drive_c/windows/system32").join(dll);
    if prefix_matches && fs::symlink_metadata(&own).map(|m| m.is_file() && m.len() > 65536).unwrap_or(false) {
        return Some(own);
    }
    from_lib()
}

/// The vkd3d-proton build in a runtime stamp: the size of the d3d12core.dll the warmer runs with ("default" when none).
fn d3d12core_sig(game: &GameInfo, proton: &str) -> String {
    runtime_dll(game, proton, &game_compat_dir(game), "d3d12core.dll").map(|c| file_len(&c).to_string()).unwrap_or_else(|| "default".into())
}

/// Puts the exact vkd3d-proton / DXVK DLLs of the game's Proton next to the warmer.
fn stage_proton_runtime_dlls(game: &GameInfo, proton: &str, game_compat: &Path, active_compat: &Path, work: &Path) -> Vec<PathBuf> {
    let mut staged = Vec::new();
    let active_sys32 = active_compat.join("pfx/drive_c/windows/system32");
    for dll in ["d3d12.dll", "d3d12core.dll", "dxgi.dll", "d3d11.dll"] {
        let Some(src) = runtime_dll(game, proton, game_compat, dll) else { continue };
        if active_compat != game_compat && active_sys32.is_dir() {
            let _ = fs::copy(&src, active_sys32.join(dll));
        }
        let dest = work.join(dll);
        if !dest.exists() && fs::copy(&src, &dest).is_ok() {
            staged.push(dest);
        }
    }
    staged
}

fn leaf_dirs_with_bins(gl: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = files_recursive(gl)
        .into_iter()
        .filter(|f| f.extension().map(|e| e == "bin").unwrap_or(false))
        .filter_map(|f| f.parent().map(Path::to_path_buf))
        .collect();
    v.sort();
    v.dedup();
    v
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_driver_caches_for_warmer(
    target: &Path,
    vkd3d: &Path,
    gl: &Path,
    proton: &str,
    core_sig: &str,
    is_incremental: bool,
    game_exe_dir: Option<&str>,
    max_part: i64,
    install: Option<&str>,
    preserve: bool,
) {
    let base = format!("{proton}|{core_sig}");
    let current = format!("{base}|{}", nvidia_kernel_version());
    let stamp = target.join("shaderless_runtime.stamp");
    let legacy = target.join("scskiller_runtime.stamp");
    if !stamp.is_file() && legacy.is_file() {
        let _ = fs::rename(&legacy, &stamp);
    }
    let mut changed = true;
    if let Ok(old) = fs::read_to_string(&stamp) {
        let old = old.trim();
        if old == current || old == base {
            changed = false;
        }
    }
    let _ = fs::write(&stamp, &current);
    // a driver / Proton change always starts clean; a full compile too unless asked to keep this runtime's cache
    let reset = changed || (!is_incremental && !preserve);

    if gl.is_dir() {
        for dir in leaf_dirs_with_bins(gl) {
            if reset {
                for f in crate::util::files_in(&dir) {
                    let n = file_name(&f);
                    if nvcache::is_nvidia_read_only_cache_file_name(&n) || n.starts_with("steamapp_shader_cache") {
                        let _ = fs::remove_file(&f);
                    }
                }
            } else {
                nvcache::pack_nvidia_gl_cache_directory(&dir, max_part);
            }
        }
    }
    if vkd3d.is_dir() {
        if reset {
            for f in crate::util::files_in(vkd3d) {
                if file_name(&f).contains(".cache") {
                    let _ = fs::remove_file(&f);
                }
            }
            for wd in game_vkd3d_working_dirs(install, game_exe_dir) {
                let _ = fs::remove_file(wd.join("vkd3d-proton.cache"));
                let _ = fs::remove_file(wd.join("vkd3d-proton.cache.write"));
            }
        } else {
            for wf in crate::util::files_in(vkd3d).into_iter().filter(|f| file_name(f).ends_with(".cache.write")) {
                if file_len(&wf) > 48 {
                    let s = wf.to_string_lossy();
                    let fin = PathBuf::from(&s[..s.len() - ".write".len()]);
                    if nvcache::merge_or_replace_vkd3d_cache(&wf, &fin).is_ok() {
                        let _ = fs::remove_file(&wf);
                    }
                }
            }
        }
    }
    if reset && target.is_dir() {
        for sub in ["DXVK_state_cache", "AMDv1", "mesa_shader_cache_sf", "mesa_shader_cache_db", "mesa_shader_cache"] {
            let d = target.join(sub);
            if d.is_dir() && fs::remove_dir_all(&d).is_ok() && (sub == "DXVK_state_cache" || sub == "AMDv1") {
                let _ = fs::create_dir_all(&d);
            }
        }
    }
}

pub fn finalize_driver_caches(target: &Path, vkd3d: &Path, gl: &Path, game_exe_dir: Option<&str>, max_part: i64, install: Option<&str>) {
    // 1. flatten a nested GLCache/GLCache
    let nested = target.join("nvidiav1/GLCache/GLCache");
    if nested.is_dir() && crate::util::copy_dir(&nested, gl).is_ok() {
        let _ = fs::remove_dir_all(&nested);
    }
    // 2. pack the writable buckets
    if gl.is_dir() {
        for dir in leaf_dirs_with_bins(gl) {
            nvcache::pack_nvidia_gl_cache_directory(&dir, max_part);
        }
    }
    // 3. promote vkd3d .cache.write files and sync to the game's working folders
    if vkd3d.is_dir() {
        for wf in crate::util::files_in(vkd3d).into_iter().filter(|f| file_name(f).ends_with(".cache.write")) {
            if file_len(&wf) == 0 {
                continue;
            }
            let s = wf.to_string_lossy();
            let fin = PathBuf::from(&s[..s.len() - ".write".len()]);
            if nvcache::merge_or_replace_vkd3d_cache(&wf, &fin).is_ok() {
                let _ = fs::remove_file(&wf);
            } else {
                let _ = fs::copy(&wf, &fin);
            }
        }
        let work_dirs = game_vkd3d_working_dirs(install, game_exe_dir);
        for wd in &work_dirs {
            let w = wd.join("vkd3d-proton.cache.write");
            if file_len(&w) > 48 && nvcache::merge_or_replace_vkd3d_cache(&w, &wd.join("vkd3d-proton.cache")).is_ok() {
                let _ = fs::remove_file(&w);
            }
        }
        if !work_dirs.is_empty() {
            let largest = crate::util::files_in(vkd3d)
                .into_iter()
                .filter(|f| f.extension().map(|e| e == "cache").unwrap_or(false))
                .max_by_key(|f| file_len(f));
            if let Some(l) = largest.filter(|l| file_len(l) > 48) {
                for wd in &work_dirs {
                    let gc = wd.join("vkd3d-proton.cache");
                    if nvcache::merge_or_replace_vkd3d_cache(&l, &gc).is_ok() && file_len(&gc) > file_len(&l) {
                        let _ = fs::copy(&gc, &l);
                    }
                }
            }
        }
    }
}

/// Every folder where vkd3d-proton may read/write a relative vkd3d-proton.cache for the game: the exe's folder, plus
/// each Unreal <Project>/Binaries/Win64 when the exe sits elsewhere.
pub fn game_vkd3d_working_dirs(install: Option<&str>, game_exe_dir: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |c: PathBuf| {
        if c.is_dir() {
            let full = PathBuf::from(crate::paths::trim_sep(&std::path::absolute(&c).unwrap_or(c.clone()).to_string_lossy()));
            if seen.insert(full.clone()) {
                dirs.push(full);
            }
        }
    };
    if let Some(e) = game_exe_dir.filter(|e| !e.is_empty()) {
        add(PathBuf::from(e));
    }
    if let (Some(i), Some(_)) = (install.filter(|i| !i.is_empty() && p(i).is_dir()), game_exe_dir.filter(|e| !e.is_empty())) {
        for sub in subdirs(p(i)) {
            if file_name(&sub).eq_ignore_ascii_case("Engine") {
                continue;
            }
            add(sub.join("Binaries/Win64"));
        }
    }
    dirs
}

pub fn finalize_game_driver_caches(game: &GameInfo) {
    let target = target_cache_path(game);
    if !target.is_dir() {
        return;
    }
    let vkd3d = target.join("VKD3D_shader_cache");
    let gl = target.join("nvidiav1/GLCache");
    let ac = recorder::detect_anti_cheat(game) != AntiCheat::None;
    let exe_dir = if ac { None } else { recorder::exe_dir(game) };
    finalize_driver_caches(&target, &vkd3d, &gl, exe_dir.as_deref(), DEFAULT_MAX_NVIDIA_PART_BYTES, if ac { None } else { Some(&game.install_path) });
    install_caches_where_game_reads(game, &vkd3d, &gl);
}

/// A game's NVIDIA cache that hit the legacy single-file ~2 GiB ceiling before parts existed.
pub fn is_nvidia_cache_truncated(game: &GameInfo) -> bool {
    let target = if game.source == GameSource::Steam { steam_root_of(game).join("steamapps/shadercache").join(game.raw_id()) } else { target_cache_path(game) };
    let gl = target.join("nvidiav1/GLCache");
    if !gl.is_dir() {
        return false;
    }
    for dir in leaf_dirs_with_bins(&gl) {
        let files = crate::util::files_in(&dir);
        let has_parts = files.iter().any(|f| {
            let n = file_name(f);
            n.starts_with("shaderless_part") && n.ends_with(".bin") && nvcache::try_parse_shaderless_part_index(&n) > 0 && file_len(f) > 64 && f.with_extension("toc").is_file()
        });
        for f in files.iter().filter(|f| f.extension().map(|e| e == "bin").unwrap_or(false)) {
            if !nvcache::is_nvidia_read_only_cache_file_name(&file_name(f)) {
                continue;
            }
            let len = file_len(f);
            if (!has_parts && len >= 2_000_000_000) || len >= 2_050_000_000 {
                return true;
            }
        }
    }
    false
}

fn read_manifest(path: &Path) -> Vec<String> {
    fs::read_to_string(path).map(|t| t.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

/// Where a non-Steam game reads its caches: vkd3d-proton under <prefix>/shadercache/VKD3D_shader_cache, NVIDIA under
/// the game's __GL_SHADER_DISK_CACHE_PATH/GLCache (Lutris: the prefix, or the exe's folder for a game added by its exe),
/// Mesa / AMD / DXVK under <compatdata>/shadercache. What the compile built is linked there and listed in
/// installed-files.txt; Flatpak launchers also get the NVIDIA files in their sandbox cache.
pub fn install_caches_where_game_reads(game: &GameInfo, vkd3d: &Path, gl: &Path) {
    let Some(target) = vkd3d.parent().map(Path::to_path_buf) else { return };
    let manifest = target.join(INSTALLED_MANIFEST);
    let previous = read_manifest(&manifest);
    let mut installed: Vec<String> = Vec::new();
    let nv_ro_files = |gl: &Path| -> Vec<PathBuf> {
        files_recursive(gl).into_iter().filter(|f| nvcache::is_nvidia_read_only_cache_file_name(&file_name(f)) && file_len(f) > 0).collect()
    };
    let prune = |installed: &[String]| {
        let cur: HashSet<&String> = installed.iter().collect();
        for old in &previous {
            if !cur.contains(old) && nvcache::is_nvidia_read_only_cache_file_name(&file_name(p(old))) {
                let _ = fs::remove_file(old);
            }
        }
    };
    let write_manifest = |installed: &[String]| {
        let mut uniq: Vec<&String> = Vec::new();
        for i in installed {
            if !uniq.contains(&i) {
                uniq.push(i);
            }
        }
        let text: String = uniq.iter().map(|s| format!("{s}\n")).collect();
        let _ = fs::write(&manifest, text);
    };

    let flatpak = game.is_flatpak || is_flatpak_game(Some(&game.id), Some(&game.install_path), game.prefix_path.as_deref());
    if flatpak && gl.is_dir() {
        if let Some(fp_gl) = flatpak_launcher_nvidia_gl_cache_path(game.source) {
            for f in nv_ro_files(gl) {
                let dest = fp_gl.join(crate::util::relative(gl, &f));
                if nvcache::link_or_copy(&f, &dest).is_ok() {
                    installed.push(dest.to_string_lossy().into_owned());
                }
            }
        }
    }
    if game.source == GameSource::Steam {
        prune(&installed);
        if !installed.is_empty() {
            write_manifest(&installed);
        } else {
            let _ = fs::remove_file(&manifest);
        }
        return;
    }
    let Some(prefix) = game.prefix_path.clone().filter(|x| !x.is_empty()) else { return };
    let compat = compat_dir_of(&prefix);
    let mut sc_roots = vec![compat.join("shadercache")];
    let other = p(&prefix).join("shadercache");
    if !sc_roots.contains(&other) {
        sc_roots.push(other);
    }
    // 1. vkd3d-proton
    if vkd3d.is_dir() {
        for root in &sc_roots {
            let gv = root.join("VKD3D_shader_cache");
            let _ = fs::create_dir_all(&gv);
            for c in crate::util::files_in(vkd3d).into_iter().filter(|f| f.extension().map(|e| e == "cache").unwrap_or(false) && file_len(f) > 0) {
                let dest = gv.join(file_name(&c));
                if nvcache::merge_or_replace_vkd3d_cache(&c, &dest).is_ok() {
                    installed.push(dest.to_string_lossy().into_owned());
                }
            }
        }
    }
    // 2. Mesa / AMD / DXVK
    if target.is_dir() {
        for sub in NON_STEAM_DRIVER_CACHE_SUBDIRS {
            let src = target.join(sub);
            if !src.is_dir() {
                continue;
            }
            let files: Vec<PathBuf> = files_recursive(&src).into_iter().filter(|f| file_len(f) > 0).collect();
            if files.is_empty() {
                continue;
            }
            for root in &sc_roots {
                let dst = root.join(sub);
                for f in &files {
                    let dest = dst.join(crate::util::relative(&src, f));
                    if nvcache::link_or_copy(f, &dest).is_ok() {
                        installed.push(dest.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    // 3. NVIDIA
    let primary = match game.gl_cache_path.as_deref().filter(|x| !x.is_empty()) {
        Some(g) => PathBuf::from(g),
        None if game.source == GameSource::Heroic => compat.join("shadercache/nvidiav1"),
        None => PathBuf::from(&prefix),
    };
    let mut nv_targets = vec![primary.join("GLCache")];
    for root in &sc_roots {
        let t = root.join("nvidiav1/GLCache");
        if !nv_targets.contains(&t) {
            nv_targets.push(t);
        }
    }
    if gl.is_dir() {
        let files = nv_ro_files(gl);
        for t in &nv_targets {
            for f in &files {
                let dest = t.join(crate::util::relative(gl, f));
                if nvcache::link_or_copy(f, &dest).is_ok() {
                    installed.push(dest.to_string_lossy().into_owned());
                }
            }
        }
    }
    prune(&installed);
    write_manifest(&installed);
}

/// Whether the compiled cache files and Proton settings landed where the game reads them.
pub fn verify_cache_installed(game: &GameInfo) -> (bool, String) {
    let target = if game.source == GameSource::Steam { steam_root_of(game).join("steamapps/shadercache").join(game.raw_id()) } else { target_cache_path(game) };
    if game.source != GameSource::Steam {
        if let Some(prefix) = game.prefix_path.as_deref().filter(|x| !x.is_empty()) {
            let manifest = target.join(INSTALLED_MANIFEST);
            if !manifest.is_file() {
                return (false, "installed cache manifest missing".into());
            }
            let files = read_manifest(&manifest);
            if files.is_empty() || files.iter().any(|f| !non_empty(p(f))) {
                return (false, "installed cache files missing or empty in game prefix".into());
            }
            let compat = compat_dir_of(prefix);
            if compat.is_dir() && !compat.join("user_settings.py").is_file() {
                return (false, "Proton user_settings.py missing in prefix".into());
            }
            return (true, "cache verified".into());
        }
    }
    if !target.is_dir() {
        return (false, "shader cache folder missing".into());
    }
    if !files_recursive(&target).iter().any(|f| is_compiled_shader_cache_file(f) && file_len(f) > 64) {
        return (false, "no non-empty cache files found in shader cache folder".into());
    }
    (true, "cache verified".into())
}

/// Whether the game's exe runs (a /proc/*/cmdline argument names it), Shaderless's own warmer aside.
pub fn is_game_running(game: &GameInfo) -> bool {
    is_game_running_in(game, Path::new("/proc"))
}

pub fn is_game_running_in(game: &GameInfo, proc_root: &Path) -> bool {
    let mut names: HashSet<String> = HashSet::new();
    if ends_with_ci(&game.executable_path, ".exe") {
        names.insert(file_name(p(&game.executable_path)).to_lowercase());
    }
    if let Some(dir) = recorder::exe_dir(game).filter(|d| p(d).is_dir()) {
        for f in crate::util::files_in(p(&dir)) {
            let n = file_name(&f);
            if ends_with_ci(&n, ".exe") && !contains_ci(&n, "CrashReport") && !contains_ci(&n, " Prerequisite") && !crate::util::starts_with_ci(&n, "UEPrereq") {
                names.insert(n.to_lowercase());
            }
        }
    }
    if names.is_empty() || !proc_root.is_dir() {
        return false;
    }
    let work_root = data_dir().join("work").to_string_lossy().to_lowercase();
    let Ok(rd) = fs::read_dir(proc_root) else { return false };
    for e in rd.flatten() {
        let pid = e.file_name().to_string_lossy().into_owned();
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = fs::read(e.path().join("cmdline")) else { continue };
        if raw.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(&raw).to_lowercase();
        if text.contains(&work_root) || text.contains("/shaderless/work/") || text.contains("/stutterfree/work/") || text.contains("\\shaderless\\work\\") {
            continue;
        }
        for arg in text.split('\0').filter(|a| !a.is_empty()) {
            let a = arg.replace('\\', "/");
            let fname = a.rsplit('/').next().unwrap_or(&a);
            if names.contains(fname) {
                return true;
            }
        }
    }
    false
}

/// Removes what [`install_caches_where_game_reads`] put in the game's cache folders.
pub fn remove_installed_caches(prefix: &Path, game_id: &str) {
    let own = prefix.join("shadercache").join(game_id);
    let manifest = own.join(INSTALLED_MANIFEST);
    if manifest.is_file() {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for path in read_manifest(&manifest) {
            if p(&path).is_file() && fs::remove_file(&path).is_ok() {
                if let Some(par) = p(&path).parent() {
                    if !dirs.contains(&par.to_path_buf()) {
                        dirs.push(par.to_path_buf());
                    }
                }
            }
        }
        dirs.sort_by_key(|d| std::cmp::Reverse(d.as_os_str().len()));
        for d in dirs {
            if fs::read_dir(&d).map(|mut r| r.next().is_none()).unwrap_or(false) {
                let _ = fs::remove_dir(&d);
            }
        }
        return;
    }
    // compiles from before installed-files.txt existed
    let own_gl = own.join("nvidiav1/GLCache");
    for f in files_recursive(&own_gl).into_iter().filter(|f| nvcache::is_nvidia_read_only_cache_file_name(&file_name(f))) {
        let _ = fs::remove_file(prefix.join("GLCache").join(crate::util::relative(&own_gl, &f)));
    }
    for c in crate::util::files_in(&own.join("VKD3D_shader_cache")).into_iter().filter(|f| f.extension().map(|e| e == "cache").unwrap_or(false)) {
        let _ = fs::remove_file(prefix.join("shadercache/VKD3D_shader_cache").join(file_name(&c)));
    }
    for sub in NON_STEAM_DRIVER_CACHE_SUBDIRS {
        let own_sub = own.join(sub);
        for f in files_recursive(&own_sub) {
            let _ = fs::remove_file(prefix.join("shadercache").join(sub).join(crate::util::relative(&own_sub, &f)));
        }
    }
}

// ---- Proton's user_settings.py --------------------------------------------------------------------------------------

const USER_SETTINGS_TEMPLATE: &str = r#"import os, sys, struct, glob, shutil

# shaderless-glcache-merge-v5 max_gb=@MAX_GB@
_MAX_PART_BYTES = @PART@
_shader_path = os.environ.get("STEAM_COMPAT_SHADER_PATH", "")
_from_steam = bool(_shader_path)
if not _shader_path:
    _compat_data = os.environ.get("STEAM_COMPAT_DATA_PATH", "").rstrip("/\\")
    if _compat_data.endswith(("pfx",)) and os.path.basename(_compat_data) == "pfx":
        _compat_data = os.path.dirname(_compat_data)
    if _compat_data:
        _cand = os.path.join(_compat_data, "shadercache")
        if os.path.isdir(_cand):
            _shader_path = _cand
user_settings = {
    "PROTON_LOCAL_SHADER_CACHE": "1",
    "__GL_SHADER_DISK_CACHE": "1",
    "__GL_SHADER_DISK_CACHE_SIZE": "@MAX_BYTES@",
    "__GL_SHADER_DISK_CACHE_SKIP_CLEANUP": "1",
    "MESA_SHADER_CACHE_MAX_SIZE": "@MESA@",
    "MESA_GLSL_CACHE_MAX_SIZE": "@MESA@",
}
def _is_nvdc(b):
    return len(b) >= 32 and (len(b) - 32) % 24 == 0 and b[:4] in (b"CDVN", b"NVDC")
def _part_idx(name):
    if name.endswith((".bin", ".toc")): name = name[:-4]
    if name.startswith("shaderless_part") and name[15:].isdigit():
        v = int(name[15:])
        return v if v > 0 else -1
    return -1
def _merge_nv_runtime_buckets(gl_root):
    try:
        if not os.path.isdir(gl_root): return
        by_dir = {}
        for bpath in glob.glob(os.path.join(gl_root, "**", "steamapp_shader_cache*.bin"), recursive=True):
            by_dir.setdefault(os.path.dirname(bpath), []).append(bpath)
        for bdir, bpaths in by_dir.items():
            valid_wb = []
            for bp in sorted(bpaths, key=lambda p: -os.path.getsize(p) if os.path.exists(p) else 0):
                tp = bp[:-4] + ".toc"
                blen = os.path.getsize(bp) if os.path.exists(bp) else 0
                if blen <= 64 or not os.path.exists(tp):
                    for p in (bp, tp):
                        try:
                            if os.path.exists(p): os.remove(p)
                        except Exception: pass
                    continue
                valid_wb.append((bp, tp, blen))
            if not valid_wb: continue
            mbin = os.path.join(bdir, "steamapp_merged_shader_cache.bin")
            mtoc = os.path.join(bdir, "steamapp_merged_shader_cache.toc")
            rbin = os.path.join(bdir, "steam_shader_cache.bin")
            rtoc = os.path.join(bdir, "steam_shader_cache.toc")
            if os.path.exists(mbin) and os.path.exists(rbin):
                try:
                    if os.path.samefile(mbin, rbin):
                        os.remove(rbin)
                        if os.path.exists(rtoc): os.remove(rtoc)
                except Exception: pass
            hdr = None
            for bp, tp, _ in valid_wb:
                with open(tp, "rb") as f: h = f.read(32)
                if len(h) == 32 and h[:4] in (b"CDVN", b"NVDC") and (os.path.getsize(tp) - 32) % 24 == 0:
                    hdr = h; break
            if hdr is None: continue
            keys = set()
            ro_tocs = [mtoc, rtoc] + sorted(
                [p for p in glob.glob(os.path.join(bdir, "shaderless_part*.toc")) if _part_idx(os.path.basename(p)) > 0],
                key=lambda p: _part_idx(os.path.basename(p)))
            for rt in ro_tocs:
                rb = rt[:-4] + ".bin"
                if os.path.exists(rt) and os.path.exists(rb) and os.path.getsize(rb) > 64:
                    with open(rt, "rb") as f: dt = f.read()
                    if _is_nvdc(dt) and dt[:32] == hdr:
                        keys.update(dt[i:i+16] for i in range(32, len(dt), 24))
            if not keys and not os.path.exists(mbin) and valid_wb[0][2] <= _MAX_PART_BYTES:
                bp0, tp0, _ = valid_wb.pop(0)
                shutil.move(bp0, mbin)
                shutil.move(tp0, mtoc)
                with open(mtoc, "rb") as f: dt = f.read()
                if _is_nvdc(dt): keys.update(dt[i:i+16] for i in range(32, len(dt), 24))
            def _slot_name(s):
                return "steamapp_merged_shader_cache" if s == 0 else f"shaderless_part{s}"
            slot = 0
            db = dt = None
            dpos = 0
            created_new = False
            db_path = dt_path = ""
            def _close_slot():
                nonlocal db, dt, created_new
                if db: db.close(); db = None
                if dt: dt.close(); dt = None
                if created_new and dpos <= 32:
                    for p in (db_path, dt_path):
                        if p and os.path.exists(p): os.remove(p)
            def _ensure_slot(rlen_rec):
                nonlocal slot, db, dt, dpos, created_new, db_path, dt_path
                if db is not None and (dpos <= 32 or dpos + rlen_rec <= _MAX_PART_BYTES):
                    return
                if db is not None:
                    _close_slot()
                    slot += 1
                while True:
                    sn = _slot_name(slot)
                    db_path = os.path.join(bdir, sn + ".bin")
                    dt_path = os.path.join(bdir, sn + ".toc")
                    if os.path.exists(db_path) and os.path.exists(dt_path) and os.path.getsize(db_path) >= 32:
                        elen = os.path.getsize(db_path)
                        with open(dt_path, "rb") as fh: ehdr = fh.read(32)
                        if ehdr == hdr and (elen <= 32 or elen + rlen_rec <= _MAX_PART_BYTES):
                            db = open(db_path, "ab")
                            dt = open(dt_path, "ab")
                            dpos = db.tell()
                            created_new = False
                            return
                        slot += 1
                        continue
                    db = open(db_path, "wb")
                    dt = open(dt_path, "wb")
                    db.write(hdr)
                    dt.write(hdr)
                    dpos = 32
                    created_new = True
                    return
            try:
                for bp, tp, blen in valid_wb:
                    with open(tp, "rb") as f: stoc = f.read()
                    if _is_nvdc(stoc) and stoc[:32] == hdr:
                        with open(bp, "rb") as sb:
                            for i in range(32, len(stoc), 24):
                                k = stoc[i:i+16]
                                if k in keys: continue
                                off, plen = struct.unpack_from("<II", stoc, i+16)
                                rlen_rec = 32 + plen
                                if off < 32 or off + rlen_rec > blen or rlen_rec > 67108864: continue
                                sb.seek(off)
                                rec = sb.read(rlen_rec)
                                if len(rec) == rlen_rec and rec[:4] == b"\x9d\xa1\x46\x98" and struct.unpack_from("<I", rec, 28)[0] == plen:
                                    _ensure_slot(rlen_rec)
                                    db.write(rec)
                                    dt.write(k + struct.pack("<II", dpos, plen))
                                    dpos += rlen_rec
                                    keys.add(k)
                    os.remove(bp)
                    os.remove(tp)
            finally:
                _close_slot()
    except Exception:
        pass
def _build_ro_names(gl_roots, existing_ro=""):
    names = []
    seen = set()
    def _add(n):
        n = n.strip()
        if n and n not in seen:
            seen.add(n); names.append(n)
    for tok in (existing_ro or "").split(";"):
        if _part_idx(tok) < 0: _add(tok)
    _add("steam_shader_cache")
    _add("steamapp_merged_shader_cache")
    try:
        pidxs = set()
        for gl_root in gl_roots:
            if gl_root and os.path.isdir(gl_root):
                for p in glob.glob(os.path.join(gl_root, "**", "shaderless_part*.bin"), recursive=True):
                    pi = _part_idx(os.path.basename(p))
                    if pi > 0 and os.path.getsize(p) > 64 and os.path.exists(p[:-4] + ".toc"):
                        pidxs.add(pi)
        for pi in sorted(pidxs):
            _add(f"shaderless_part{pi}")
    except Exception:
        pass
    return ";".join(names)
_env_gl_base = os.environ.get("__GL_SHADER_DISK_CACHE_PATH", "")
_env_gl_root = os.path.join(_env_gl_base, "GLCache") if _env_gl_base else ""
_gl_roots = []
if _shader_path:
    for _sub in ("nvidiav1", "VKD3D_shader_cache", "DXVK_state_cache", "AMDv1", "mesa"):
        try:
            os.makedirs(os.path.join(_shader_path, _sub), exist_ok=True)
        except Exception:
            pass
    _gl_root = os.path.join(_shader_path, "nvidiav1", "GLCache")
    _gl_roots.append(_gl_root)
    _merge_nv_runtime_buckets(_gl_root)
    user_settings["__GL_SHADER_DISK_CACHE_PATH"] = os.path.join(_shader_path, "nvidiav1")
    user_settings["__GL_SHADER_DISK_CACHE_APP_NAME"] = "steamapp_shader_cache"
    user_settings["VKD3D_SHADER_CACHE_PATH"] = os.path.join(_shader_path, "VKD3D_shader_cache")
    user_settings["DXVK_STATE_CACHE_PATH"] = os.path.join(_shader_path, "DXVK_state_cache")
    user_settings["AMD_VK_PIPELINE_CACHE_PATH"] = os.path.join(_shader_path, "AMDv1")
    user_settings["AMD_VK_PIPELINE_CACHE_FILENAME"] = "steamapp_shader_cache"
    user_settings["AMD_VK_USE_PIPELINE_CACHE"] = "1"
    user_settings["MESA_SHADER_CACHE_DIR"] = _shader_path
    user_settings["MESA_GLSL_CACHE_DIR"] = _shader_path
    if _from_steam:
        user_settings["MESA_DISK_CACHE_SINGLE_FILE"] = "1"
if _env_gl_root and _env_gl_root not in _gl_roots and os.path.isdir(_env_gl_root):
    _gl_roots.append(_env_gl_root)
    _merge_nv_runtime_buckets(_env_gl_root)
if _gl_roots:
    user_settings["__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME"] = _build_ro_names(_gl_roots, os.environ.get("__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME", ""))
try:
    _main = sys.modules.get("__main__")
    _sess = getattr(_main, "g_session", None)
    if _sess is not None and hasattr(_sess, "env"):
        for _k in (
            "__GL_SHADER_DISK_CACHE",
            "__GL_SHADER_DISK_CACHE_SIZE",
            "__GL_SHADER_DISK_CACHE_SKIP_CLEANUP",
            "__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME",
            "MESA_SHADER_CACHE_MAX_SIZE",
            "MESA_GLSL_CACHE_MAX_SIZE",
        ):
            if _k in user_settings:
                _sess.env.pop(_k, None)
except Exception:
    pass
"#;

pub fn build_proton_user_settings_script(s: &AppSettings, max_part: i64) -> String {
    let part = if max_part >= 256 { max_part } else { DEFAULT_MAX_NVIDIA_PART_BYTES };
    USER_SETTINGS_TEMPLATE
        .replace("@MAX_GB@", &s.effective_max_cache_size_gb().to_string())
        .replace("@PART@", &part.to_string())
        .replace("@MAX_BYTES@", &s.max_cache_size_bytes().to_string())
        .replace("@MESA@", &s.mesa_max_cache_size())
}

pub fn refresh_installed_proton_shader_cache_settings(s: &AppSettings) {
    ensure_proton_local_shader_cache_setting(Path::new(""), None, Some(s));
}

/// Writes Proton's user_settings.py (PROTON_LOCAL_SHADER_CACHE, cache sizes, the NVIDIA read-only parts) into the game's
/// compat folder and every Proton install found, unless it already holds this version and size.
pub fn ensure_proton_local_shader_cache_setting(compat: &Path, proton: Option<&str>, s: Option<&AppSettings>) {
    let loaded;
    let s = match s {
        Some(s) => s,
        None => {
            loaded = settings::load();
            &loaded
        }
    };
    let tag = format!("shaderless-glcache-merge-v5 max_gb={}", s.effective_max_cache_size_gb());
    let content = build_proton_user_settings_script(s, DEFAULT_MAX_NVIDIA_PART_BYTES);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if !compat.as_os_str().is_empty() && compat.is_dir() {
        dirs.push(compat.to_path_buf());
    }
    if let Some(pd) = proton.and_then(|x| p(x).parent()).filter(|d| d.is_dir()) {
        dirs.push(pd.to_path_buf());
    }
    if !settings::settings_path_overridden() {
        let home = home_dir();
        for sr in all_steam_roots() {
            dirs.extend(subdirs(&sr.join("compatibilitytools.d")));
            dirs.extend(subdirs(&sr.join("steamapps/common")).into_iter().filter(|d| file_name(d).starts_with("Proton") && d.join("proton").is_file()));
        }
        for r in [
            xdg_config_home().join("heroic/tools/proton"),
            home.join(".config/heroic/tools/proton"),
            home.join(".var/app/com.heroicgameslauncher.hgl/config/heroic/tools/proton"),
            xdg_data_home().join("lutris/runners/proton"),
            home.join(".local/share/lutris/runners/proton"),
            home.join(".var/app/net.lutris.Lutris/data/lutris/runners/proton"),
        ] {
            dirs.extend(subdirs(&r).into_iter().filter(|d| d.join("proton").is_file()));
        }
    }
    let mut seen = HashSet::new();
    for d in dirs.into_iter().filter(|d| seen.insert(d.clone())) {
        let f = d.join("user_settings.py");
        let current = fs::read_to_string(&f).unwrap_or_default();
        if !current.contains(&tag) {
            let _ = fs::write(&f, &content);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vdf_blocks() {
        let vdf = r#""InstallConfigStore" { "Software" { "Valve" { "Steam" { "CompatToolMapping" { "0" { "name" "proton_experimental" } "123" { "name" "GE-Proton9-1" "config" "" } } } } } }"#;
        let block = extract_vdf_block(vdf, "CompatToolMapping").unwrap();
        let app = extract_vdf_block(&block, "123").unwrap();
        assert_eq!(&re_name().captures(&app).unwrap()[1], "GE-Proton9-1");
        let def = extract_vdf_block(&block, "0").unwrap();
        assert_eq!(&re_name().captures(&def).unwrap()[1], "proton_experimental");
    }

    #[test]
    fn warmer_lines() {
        let e = parse_warmer_line(r#"{"event":"progress","done":10,"total":20,"failed":1}"#).unwrap();
        assert_eq!(e.event, "progress");
        assert_eq!(e.done, Some(10));
        assert!(parse_warmer_line("wine: noise").is_none());
    }

    #[test]
    fn user_settings_script() {
        let s = AppSettings::default();
        let py = build_proton_user_settings_script(&s, DEFAULT_MAX_NVIDIA_PART_BYTES);
        assert!(py.contains("# shaderless-glcache-merge-v5 max_gb=10"));
        assert!(py.contains("_MAX_PART_BYTES = 1850000000"));
        assert!(py.contains(r#"rec[:4] == b"\x9d\xa1\x46\x98""#));
        assert!(py.contains(r#".rstrip("/\\")"#));
        assert!(!py.contains('@'));
    }

    #[test]
    fn stamps_use_the_dll_the_warmer_runs() {
        // the prefix's d3d12core differs from Proton's: compile and stale check must read the same one (was: forever "Proton updated")
        let dir = tempfile::tempdir().unwrap();
        let proton_dir = dir.path().join("Proton-X");
        let lib = proton_dir.join("files/lib/wine/vkd3d-proton/x86_64-windows");
        fs::create_dir_all(&lib).unwrap();
        fs::write(proton_dir.join("proton"), b"#!/bin/sh").unwrap();
        fs::write(lib.join("d3d12core.dll"), vec![0u8; 70_000]).unwrap();
        let compat = dir.path().join("compatdata/1");
        let sys32 = compat.join("pfx/drive_c/windows/system32");
        fs::create_dir_all(&sys32).unwrap();
        fs::write(sys32.join("d3d12core.dll"), vec![0u8; 80_000]).unwrap();
        let proton = proton_dir.join("proton").to_string_lossy().into_owned();
        fs::write(compat.join("config_info"), format!("X\n{}/files/share/fonts/\n", proton_dir.display())).unwrap();
        let g = GameInfo::new("1".into(), "g".into(), GameSource::Steam, "/x".into(), "/x/g.exe".into(),
            Some(compat.join("pfx").to_string_lossy().into_owned()), None, crate::models::GameEngine::Unity, 0, false);
        assert_eq!(d3d12core_sig(&g, &proton), "80000");
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        stage_proton_runtime_dlls(&g, &proton, &compat, &compat, &work);
        assert_eq!(file_len(&work.join("d3d12core.dll")).to_string(), d3d12core_sig(&g, &proton));
    }

    #[test]
    fn content_stamp() {
        let t = latest_game_content_write_utc(Some("10:638000000000000000|3:100:638100000000000000")).unwrap();
        assert!(t.timestamp() > 0);
        assert!(latest_game_content_write_utc(Some("unknown")).is_none());
    }
}
