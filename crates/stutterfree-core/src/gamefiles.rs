//! Install-folder heuristics (SCSKiller.Core Games/GameFiles.cs): the Unreal Shipping exe, anti-cheat detection, and
//! recognising Shaderless's own recorder d3d12.dll.

use crate::models::{AntiCheat, CoreGame, Store};
use crate::util::{contains_ci, ends_with_ci, starts_with_ci};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// An export only our recorder d3d12.dll has.
const PROXY_MARKER: &[u8] = b"SCSKiller_StartWarm";

pub fn is_our_proxy(dll: &Path) -> bool {
    match fs::read(dll) {
        Ok(bytes) => bytes.windows(PROXY_MARKER.len()).any(|w| w == PROXY_MARKER),
        Err(_) => false,
    }
}

fn dirs_flat(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    v.sort();
    v
}

/// The <Project>/Binaries/Win* folders of the install root and of projects one or two levels under it (Win32 left
/// out: a 32-bit exe never creates the game's D3D12 device).
fn unreal_folders(install: &Path) -> Vec<PathBuf> {
    let mut roots = vec![install.to_path_buf()];
    for d in dirs_flat(install) {
        roots.push(d.clone());
        roots.extend(dirs_flat(&d));
    }
    let mut out = Vec::new();
    for r in roots {
        let b = r.join("Binaries");
        if !b.is_dir() {
            continue;
        }
        for w in dirs_flat(&b) {
            let n = crate::util::file_name(&w);
            if n.starts_with("Win") && !n.eq_ignore_ascii_case("Win32") {
                out.push(w);
            }
        }
    }
    out
}

/// <Name>-<Platform>-Shipping.exe in a Binaries/<Platform> folder.
pub fn is_shipping(exe: &str) -> bool {
    let p = Path::new(exe);
    let Some(dir) = p.parent() else { return false };
    let binaries = dir.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().eq_ignore_ascii_case("Binaries")).unwrap_or(false);
    let platform = crate::util::file_name(dir);
    binaries && ends_with_ci(&crate::util::file_name(p), &format!("-{platform}-Shipping.exe"))
}

/// <Name> of <Name>-<Platform>-Shipping.exe.
fn build_name(shipping: &Path) -> String {
    let name = crate::util::file_name(shipping);
    let platform = shipping.parent().map(crate::util::file_name).unwrap_or_default();
    let suffix = format!("-{platform}-Shipping.exe").len();
    name.get(..name.len().saturating_sub(suffix)).unwrap_or("").to_string()
}

/// <Project> of <Project>/Binaries/<Platform>/the exe.
fn project_dir(shipping: &Path) -> PathBuf {
    shipping.parent().and_then(Path::parent).and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default()
}

const NOT_THE_GAME_BUILD: [&str; 5] = ["Server", "Editor", "Tool", "Benchmark", "CrashReport"];
const HELPERS: [&str; 10] =
    ["EpicOnlineServices", "EOSBootstrapper", "EpicWebHelper", "CrashReport", "UnrealCEFSubProcess", "Prereq", "setup", "redist", "installer", "unins"];
const STAGING: [&str; 5] = ["PatchData", "__Installer", "Backup", "Backups", "Staging"];

/// An installer, redistributable or engine helper shipped beside a game's exe, by its file name.
pub fn is_helper(exe: &Path) -> bool {
    let n = crate::util::file_name(exe);
    HELPERS.iter().any(|h| contains_ci(&n, h))
}

fn rel_components(base: &Path, p: &Path) -> Vec<String> {
    p.strip_prefix(base)
        .map(|r| r.components().filter_map(|c| matches!(c, Component::Normal(_)).then(|| c.as_os_str().to_string_lossy().into_owned())).collect())
        .unwrap_or_default()
}

fn in_staging(install: &Path, path: &Path) -> bool {
    let dir = path.parent().unwrap_or(path);
    rel_components(install, dir).iter().any(|seg| STAGING.iter().any(|s| s.eq_ignore_ascii_case(seg)))
}

fn is_engine_folder(install: &Path, path: &Path) -> bool {
    rel_components(install, path).first().map(|s| s.eq_ignore_ascii_case("Engine")).unwrap_or(false) && rel_components(install, path).len() > 1
}

fn depth_under(install: &Path, p: &Path) -> usize {
    rel_components(install, p).len()
}

/// An Unreal game's own process: <Name>-<Platform>-Shipping.exe in a <Project>/Binaries/<Platform> folder of the
/// install root or a project one or two levels under it, with an Engine folder beside the project or in the root.
/// Engine's, a patcher's copy, helpers and tools aside; of exes named alike, the one nearest the root. One such exe is
/// the game's; of several, the one whose name or project is `named`'s name or the install folder's. None when there is
/// none, or several and none or more than one tied.
pub fn shipping_exe(install_dir: &str, named: Option<&str>) -> Option<String> {
    let install = Path::new(install_dir);
    if !install.is_dir() {
        return None;
    }
    let unreal_root = install.join("Engine").is_dir();
    let mut by_name: Vec<(String, PathBuf)> = Vec::new();
    for d in unreal_folders(install) {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
        files.sort();
        for f in files {
            let n = crate::util::file_name(&f);
            if !n.ends_with("-Shipping.exe") {
                continue;
            }
            let fs_ = f.to_string_lossy();
            if !is_shipping(&fs_) || is_engine_folder(install, &f) || in_staging(install, &f) || is_helper(&f) {
                continue;
            }
            let bn = build_name(&f);
            if NOT_THE_GAME_BUILD.iter().any(|x| contains_ci(&bn, x)) {
                continue;
            }
            let proj = project_dir(&f);
            if !(unreal_root || proj.parent().map(|p| p.join("Engine").is_dir()).unwrap_or(false)) {
                continue;
            }
            let key = n.to_lowercase();
            match by_name.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => {
                    if depth_under(install, &f) < depth_under(install, &slot.1) {
                        slot.1 = f;
                    }
                }
                None => by_name.push((key, f)),
            }
        }
    }
    let all: Vec<PathBuf> = by_name.into_iter().map(|(_, p)| p).collect();
    if all.len() <= 1 {
        return all.first().map(|p| p.to_string_lossy().into_owned());
    }
    let mut names = Vec::new();
    if let Some(n) = named {
        names.push(Path::new(n).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    }
    names.push(crate::util::file_name(Path::new(crate::paths::trim_sep(install_dir).as_str())));
    let tied: Vec<&PathBuf> = all
        .iter()
        .filter(|f| {
            let bn = build_name(f);
            let pn = crate::util::file_name(&project_dir(f));
            names.iter().any(|n| n.eq_ignore_ascii_case(&bn) || n.eq_ignore_ascii_case(&pn))
        })
        .collect();
    if tied.len() == 1 {
        Some(tied[0].to_string_lossy().into_owned())
    } else {
        None
    }
}

/// The exe SCSKiller uses for a game whose store or user names `exe`: a Shipping exe it names, else the Unreal game's
/// Shipping exe over a bootstrap stub or launcher; else `exe`.
pub fn game_exe(install_dir: &str, exe: &str) -> String {
    if is_shipping(exe) {
        exe.to_string()
    } else {
        shipping_exe(install_dir, Some(exe)).unwrap_or_else(|| exe.to_string())
    }
}

// ---- anti-cheat -----------------------------------------------------------------------------------------------------

const MARKERS: &[(&str, AntiCheat)] = &[
    ("EasyAntiCheat", AntiCheat::EasyAntiCheat),
    ("EasyAntiCheat_EOS", AntiCheat::EasyAntiCheat),
    ("start_protected_game.exe", AntiCheat::EasyAntiCheat),
    ("EasyAntiCheat_EOS_Setup.exe", AntiCheat::EasyAntiCheat),
    ("EasyAntiCheat_Setup.exe", AntiCheat::EasyAntiCheat),
    ("BattlEye", AntiCheat::BattlEye),
    ("BEService.exe", AntiCheat::BattlEye),
    ("BEService_x64.exe", AntiCheat::BattlEye),
    ("BELauncher.exe", AntiCheat::BattlEye),
    ("BEClient_x64.dll", AntiCheat::BattlEye),
    ("BEClient.dll", AntiCheat::BattlEye),
    ("EAAntiCheat.Installer.exe", AntiCheat::Other),
    ("GameGuard", AntiCheat::Other),
    ("XIGNCODE", AntiCheat::Other),
    ("nProtect", AntiCheat::Other),
    ("randgrid.sys", AntiCheat::Other),
    ("NCGuardSDK", AntiCheat::Other),
    ("NCGuard", AntiCheat::Other),
    ("AntiCheatExpert", AntiCheat::Other),
    ("AceAntibotClient", AntiCheat::Other),
    ("TP3Helper.exe", AntiCheat::Other),
    ("SGuard", AntiCheat::Other),
    ("SGuard64.exe", AntiCheat::Other),
    ("SGuardSvc64.exe", AntiCheat::Other),
    ("ACE-Base64.dll", AntiCheat::Other),
    ("ACE-Base.dat", AntiCheat::Other),
    ("ACE-Setup64.exe", AntiCheat::Other),
    ("ACE-Service64.exe", AntiCheat::Other),
    ("ACE-ATS64.dll", AntiCheat::Other),
    ("ACE-CSI64.dll", AntiCheat::Other),
    ("ACE-DFS64.dll", AntiCheat::Other),
    ("TenProtect", AntiCheat::Other),
    ("TesSafe.sys", AntiCheat::Other),
    ("HoYoKProtect.sys", AntiCheat::Other),
    ("mhypbase.dll", AntiCheat::Other),
    ("mhyprot.sys", AntiCheat::Other),
    ("mhyprot2.sys", AntiCheat::Other),
    ("mhyprot3.sys", AntiCheat::Other),
    ("ACE-BASE.sys", AntiCheat::Other),
    ("GenshinImpact.exe", AntiCheat::Other),
    ("YuanShen.exe", AntiCheat::Other),
    ("StarRail.exe", AntiCheat::Other),
    ("ZenlessZoneZero.exe", AntiCheat::Other),
    ("BH3.exe", AntiCheat::Other),
    ("HYP.exe", AntiCheat::Other),
    ("HYPHelper.exe", AntiCheat::Other),
    ("HYPWorker.exe", AntiCheat::Other),
    ("EAAntiCheat.GameServiceLauncher.exe", AntiCheat::Other),
    ("EAAntiCheat.GameServiceLauncher.dll", AntiCheat::Other),
    ("vgk.sys", AntiCheat::Other),
    ("vgc.exe", AntiCheat::Other),
    ("VALORANT.exe", AntiCheat::Other),
    ("VALORANT-Win64-Shipping.exe", AntiCheat::Other),
    ("League of Legends.exe", AntiCheat::Other),
    ("LeagueClient.exe", AntiCheat::Other),
    ("LeagueClientUx.exe", AntiCheat::Other),
    ("LeagueClientUxRender.exe", AntiCheat::Other),
    ("LoR.exe", AntiCheat::Other),
    ("Lion-Win64-Shipping.exe", AntiCheat::Other),
    ("RiotClientServices.exe", AntiCheat::Other),
    ("RiotClientUx.exe", AntiCheat::Other),
    ("RiotClientUxRender.exe", AntiCheat::Other),
    ("NeacClient.exe", AntiCheat::Other),
    ("NeacSafe64.sys", AntiCheat::Other),
    ("NeacSafe64_ex.sys", AntiCheat::Other),
    ("NEP2.dll", AntiCheat::Other),
    ("NEPCleaner.exe", AntiCheat::Other),
    ("BlackCall.aes", AntiCheat::Other),
    ("BlackCall64.aes", AntiCheat::Other),
    ("BlackCat64.sys", AntiCheat::Other),
    ("HShield", AntiCheat::Other),
    ("PunkBuster", AntiCheat::Other),
    ("PnkBstrA.exe", AntiCheat::Other),
    ("pbsvc.exe", AntiCheat::Other),
    ("pbsv.dll", AntiCheat::Other),
    ("equ8_conf.json", AntiCheat::Other),
    (".build.info", AntiCheat::Other),
    (".product.db", AntiCheat::Other),
    ("Warframe.x64.exe", AntiCheat::Other),
    ("arbiter.dll", AntiCheat::Other),
    ("gameguard.des", AntiCheat::Other),
    ("DenuvoAC", AntiCheat::Other),
    ("denuvo-anti-cheat.sys", AntiCheat::Other),
    ("denuvo-anti-cheat-runtime.dll", AntiCheat::Other),
    ("denuvo-anti-cheat-update-service.exe", AntiCheat::Other),
    ("Denuvo Anti-Cheat Installer.exe", AntiCheat::Other),
    ("*.xem", AntiCheat::Other),
    ("*_BE.exe", AntiCheat::BattlEye),
];

pub fn marker(name: &str) -> AntiCheat {
    for (m, kind) in MARKERS {
        let hit = if let Some(rest) = m.strip_prefix('*') { ends_with_ci(name, rest) } else { name.eq_ignore_ascii_case(m) };
        if hit {
            return *kind;
        }
    }
    AntiCheat::None
}

/// Entries one detection reads at most.
pub const MAX_ENTRIES: usize = 2_000_000;
/// The time one detection takes at most.
pub const BUDGET: Duration = Duration::from_secs(30);

const LIBRARIES: [&str; 22] = [
    "steamapps",
    "SteamLibrary",
    "XboxGames",
    "WindowsApps",
    "ModifiableWindowsApps",
    "Epic Games",
    "EpicGames",
    "GOG Games",
    "GOG Galaxy",
    "EA Games",
    "Origin Games",
    "Ubisoft Game Launcher",
    "Battle.net",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "Windows",
    "Users",
    "Desktop",
    "Downloads",
    "Documents",
    "OneDrive",
];

/// A store's folder of many games, steamapps/common, or the user's own / app-data folders (ManualSource.IsLibrary).
fn is_library(full: &Path) -> bool {
    let name = crate::util::file_name(full);
    if LIBRARIES.iter().any(|l| l.eq_ignore_ascii_case(&name)) {
        return true;
    }
    if name.eq_ignore_ascii_case("common")
        && full.parent().map(|p| crate::util::file_name(p).eq_ignore_ascii_case("steamapps")).unwrap_or(false)
    {
        return true;
    }
    let specials = [crate::paths::home_dir(), crate::paths::xdg_data_home(), crate::paths::xdg_config_home()];
    let fk = crate::paths::trim_sep(&full.to_string_lossy());
    specials.iter().any(|s| crate::paths::trim_sep(&s.to_string_lossy()).eq_ignore_ascii_case(&fk))
}

fn riot_games(path: &str) -> bool {
    Path::new(path).components().any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case("Riot Games"))
}

fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

fn full_path(p: &str) -> PathBuf {
    std::path::absolute(p).map(|a| PathBuf::from(crate::paths::trim_sep(&a.to_string_lossy()))).unwrap_or_else(|_| PathBuf::from(p))
}

/// Looks for anti-cheat folders/files by name anywhere under the install, under the exe's folder when it is outside
/// it, and in the names of the folders from the exe's up to the install root. A tree that can't be read whole (a
/// folder it may not list, too many entries, too slow) is [`AntiCheat::Other`]: not known to be clean. A symlink is a
/// name, not followed; one on the way from the install root to the exe is Other. `quick`: the install root's and the
/// exe folder's own entries only. A game added by hand also has the entries of each folder above its install read,
/// one level each, up to a store's folder of games.
pub fn detect_anti_cheat(game: &CoreGame, quick: bool) -> AntiCheat {
    if game.id.starts_with("battlenet:") || game.id.starts_with("hoyoplay:") {
        return AntiCheat::Other;
    }
    if riot_games(&game.install_dir) || riot_games(&game.exe_path) {
        return AntiCheat::Other;
    }
    let install = full_path(&game.install_dir);
    let exe_dir = full_path(&game.exe_path).parent().map(Path::to_path_buf).unwrap_or_else(|| install.clone());
    let install_s = install.to_string_lossy().to_lowercase();
    let inside = |d: &Path| {
        let s = d.to_string_lossy().to_lowercase();
        s == install_s || s.starts_with(&format!("{install_s}/"))
    };
    let clock = Instant::now();
    let mut seen = 0usize;
    let mut visited = std::collections::HashSet::new();

    let mut walk = |root: &Path, deep: bool| -> Result<AntiCheat, ()> {
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            if !visited.insert(dir.to_string_lossy().to_lowercase()) {
                continue;
            }
            if clock.elapsed() > BUDGET {
                return Ok(AntiCheat::Other);
            }
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir == root => continue,
                Err(_) => return Err(()),
            };
            for e in rd {
                let Ok(e) = e else { return Err(()) };
                seen += 1;
                if seen > MAX_ENTRIES || clock.elapsed() > BUDGET {
                    return Ok(AntiCheat::Other);
                }
                let kind = marker(&e.file_name().to_string_lossy());
                if kind != AntiCheat::None {
                    return Ok(kind);
                }
                if deep && !quick && e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    dirs.push(e.path());
                }
            }
        }
        Ok(AntiCheat::None)
    };

    let result = (|| -> Result<AntiCheat, ()> {
        let mut d = Some(exe_dir.clone());
        while let Some(cur) = d {
            if !inside(&cur) || cur.as_os_str().len() <= install.as_os_str().len() {
                break;
            }
            if is_symlink(&cur) {
                return Ok(AntiCheat::Other);
            }
            d = cur.parent().map(Path::to_path_buf);
        }
        let roots: Vec<PathBuf> = if quick || !inside(&exe_dir) { vec![install.clone(), exe_dir.clone()] } else { vec![install.clone()] };
        for r in roots {
            let k = walk(&r, true)?;
            if k != AntiCheat::None {
                return Ok(k);
            }
        }
        let mut d = Some(exe_dir.clone());
        while let Some(cur) = d {
            if !inside(&cur) {
                break;
            }
            let k = marker(&crate::util::file_name(&cur));
            if k != AntiCheat::None {
                return Ok(k);
            }
            d = cur.parent().map(Path::to_path_buf);
        }
        if game.store == Store::Manual {
            let mut d = install.parent().map(Path::to_path_buf);
            while let Some(cur) = d {
                if cur.parent().is_none() || is_library(&cur) {
                    break;
                }
                let k = walk(&cur, false)?;
                if k != AntiCheat::None {
                    return Ok(k);
                }
                d = cur.parent().map(Path::to_path_buf);
            }
        }
        Ok(AntiCheat::None)
    })();
    result.unwrap_or(AntiCheat::Other)
}

/// Is `name` the file name of an exe that is plainly not the game (shared by exe discovery).
pub fn starts_or_contains(name: &str, list: &[&str]) -> bool {
    list.iter().any(|x| starts_with_ci(name, x) || contains_ci(name, x))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers() {
        assert_eq!(marker("easyanticheat"), AntiCheat::EasyAntiCheat);
        assert_eq!(marker("Game_BE.exe"), AntiCheat::BattlEye);
        assert_eq!(marker("game.exe"), AntiCheat::None);
    }

    #[test]
    fn finds_shipping_and_anticheat() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("MyGame");
        let bin = root.join("MyGame").join("Binaries").join("Win64");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(root.join("Engine")).unwrap();
        let exe = bin.join("MyGame-Win64-Shipping.exe");
        fs::write(&exe, b"MZ").unwrap();
        fs::write(root.join("MyGame.exe"), b"MZ").unwrap();
        let root_s = root.to_string_lossy().into_owned();
        assert_eq!(shipping_exe(&root_s, None).as_deref(), Some(exe.to_string_lossy().as_ref()));
        assert_eq!(game_exe(&root_s, &root.join("MyGame.exe").to_string_lossy()), exe.to_string_lossy());

        let g = CoreGame { id: "1".into(), name: "g".into(), store: Store::Steam, install_dir: root_s.clone(), exe_path: exe.to_string_lossy().into() };
        assert_eq!(detect_anti_cheat(&g, false), AntiCheat::None);
        fs::create_dir_all(root.join("EasyAntiCheat")).unwrap();
        assert_eq!(detect_anti_cheat(&g, false), AntiCheat::EasyAntiCheat);
    }
}
