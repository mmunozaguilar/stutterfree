//! Steam (native and Flatpak): libraryfolders.vdf + appmanifest_*.acf (SteamLinuxSource.cs).

use super::{Diag, GameSourceScanner, ScanRoots};
use crate::engine_detector;
use crate::models::{GameInfo, GameSource, SourceDiagnostic};
use crate::paths::{canonical_path, xdg_data_home_with};
use regex::Regex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct SteamSource {
    roots: ScanRoots,
}

impl SteamSource {
    pub fn new(roots: ScanRoots) -> Self {
        SteamSource { roots }
    }
}

fn re_path() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#""path"\s+"([^"]+)""#).unwrap())
}

fn field(text: &str, name: &str) -> String {
    Regex::new(&format!(r#""{name}"\s+"([^"]+)""#))
        .ok()
        .and_then(|r| r.captures(text))
        .map(|c| c[1].to_string())
        .unwrap_or_default()
}

/// The library paths a libraryfolders.vdf lists.
pub fn library_paths(vdf: &str) -> Vec<String> {
    re_path().captures_iter(vdf).map(|c| c[1].replace("\\\\", "/")).collect()
}

impl GameSourceScanner for SteamSource {
    fn source_type(&self) -> GameSource {
        GameSource::Steam
    }

    fn diagnose(&self) -> SourceDiagnostic {
        let mut d = Diag::new();
        let home = self.roots.home();
        let xdg_data = xdg_data_home_with(Some(&home), self.roots.xdg_data.as_deref());
        let default_data = home.join(".local").join("share");

        let mut candidates: Vec<(PathBuf, &str, bool)> = Vec::new();
        if canonical_path(&xdg_data) != canonical_path(&default_data) {
            candidates.push((xdg_data.join("Steam"), "Native Steam ($XDG_DATA_HOME/Steam)", false));
        }
        candidates.push((home.join(".local/share/Steam"), "Native Steam (~/.local/share/Steam)", false));
        candidates.push((home.join(".steam/steam"), "Native Steam symlink (~/.steam/steam)", false));
        candidates.push((home.join(".steam/root"), "Native Steam symlink (~/.steam/root)", false));
        candidates.push((
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
            "Flatpak Steam (~/.var/app/com.valvesoftware.Steam/.local/share/Steam)",
            true,
        ));
        candidates.push((
            home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
            "Flatpak Steam symlink (~/.var/app/com.valvesoftware.Steam/data/Steam)",
            true,
        ));

        let mut active: Vec<(PathBuf, bool)> = Vec::new();
        let mut seen_roots = HashSet::new();
        for (p, desc, fp) in candidates {
            let exists = p.is_dir();
            d.check(&p, desc, exists, fp);
            if exists && seen_roots.insert(canonical_path(&p)) {
                active.push((p, fp));
            }
        }

        for (steam_root, fp) in active {
            let mut libraries = vec![steam_root.clone()];
            let mut seen_libs: HashSet<String> = [canonical_path(&steam_root)].into_iter().collect();
            let vdf = steam_root.join("steamapps").join("libraryfolders.vdf");
            let vdf_exists = vdf.is_file();
            d.check(&vdf, if fp { "Flatpak Steam libraryfolders.vdf" } else { "Native Steam libraryfolders.vdf" }, vdf_exists, fp);
            if vdf_exists {
                match std::fs::read_to_string(&vdf) {
                    Ok(content) => {
                        for lib in library_paths(&content) {
                            let lp = PathBuf::from(&lib);
                            let exists = lp.is_dir();
                            if seen_libs.insert(canonical_path(&lp)) {
                                d.check(&lp, if fp { "Flatpak Steam library folder" } else { "Steam library folder" }, exists, fp);
                                if exists {
                                    libraries.push(lp);
                                }
                            }
                        }
                    }
                    Err(e) => d.skip("libraryfolders.vdf", vdf.to_string_lossy(), format!("Unreadable VDF: {e}"), fp),
                }
            }

            let mut seen_ids: HashSet<String> = HashSet::new();
            for library in libraries {
                let steamapps = library.join("steamapps");
                let Ok(rd) = std::fs::read_dir(&steamapps) else {
                    if steamapps.is_dir() {
                        d.skip("steamapps", steamapps.to_string_lossy(), "Cannot enumerate manifests", fp);
                    }
                    continue;
                };
                let mut manifests: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        let n = crate::util::file_name(p);
                        n.starts_with("appmanifest_") && n.ends_with(".acf") && p.is_file()
                    })
                    .collect();
                manifests.sort();
                for manifest in manifests {
                    scan_manifest(&mut d, &manifest, &steamapps, &steam_root, fp, &mut seen_ids);
                }
            }
        }
        d.finish(GameSource::Steam, None)
    }
}

fn scan_manifest(d: &mut Diag, manifest: &Path, steamapps: &Path, steam_root: &Path, fp: bool, seen_ids: &mut HashSet<String>) {
    let mname = crate::util::file_name(manifest);
    let text = match std::fs::read_to_string(manifest) {
        Ok(t) => t,
        Err(e) => {
            d.skip(mname, manifest.to_string_lossy(), format!("Manifest parse error: {e}"), fp);
            return;
        }
    };
    let app_id = field(&text, "appid");
    let name = field(&text, "name");
    let install_dir = field(&text, "installdir");
    if app_id.trim().is_empty() || install_dir.trim().is_empty() {
        d.skip(mname, manifest.to_string_lossy(), "Manifest missing appid or installdir", fp);
        return;
    }
    let label = if name.trim().is_empty() { format!("{install_dir} ({app_id})") } else { format!("{name} ({app_id})") };
    let lower = install_dir.to_ascii_lowercase();
    if lower.starts_with("steamlinuxruntime") || lower.starts_with("proton") || lower == "steamworks shared" {
        d.skip(label, manifest.to_string_lossy(), "Steam runtime / Proton compatibility tool", fp);
        return;
    }
    if !seen_ids.insert(app_id.to_ascii_lowercase()) {
        return;
    }
    let game_path = steamapps.join("common").join(&install_dir);
    if !game_path.is_dir() {
        d.skip(label, game_path.to_string_lossy(), "Game install directory missing on disk", fp);
        return;
    }
    let mut prefix = steamapps.join("compatdata").join(&app_id).join("pfx");
    if !prefix.is_dir() {
        let root_prefix = steam_root.join("steamapps").join("compatdata").join(&app_id).join("pfx");
        prefix = if root_prefix.is_dir() { root_prefix } else { PathBuf::new() };
    }
    let gp = game_path.to_string_lossy().into_owned();
    let Some(exe) = engine_detector::find_executable(&gp) else {
        d.skip(label, gp, "No Windows .exe found (Linux native build or empty folder)", fp);
        return;
    };
    let (engine, estimated) = engine_detector::detect(&gp);
    let banner = steam_banner(steam_root, &app_id);
    let id = if fp { format!("flatpak_{app_id}") } else { app_id.clone() };
    let pfx = prefix.to_string_lossy().into_owned();
    d.found.push(GameInfo::new(
        id,
        if name.trim().is_empty() { install_dir.clone() } else { name },
        GameSource::Steam,
        gp,
        exe,
        (!pfx.is_empty()).then_some(pfx),
        banner,
        engine,
        estimated,
        fp,
    ));
}

/// The game's artwork from Steam's library cache, a wide image first (the cards are wide): current Steam keeps it per
/// app (`librarycache/<id>/header.jpg`, `librarycache/<id>/<hash>/library_header.jpg`), older Steam flat
/// (`librarycache/<id>_header.jpg`).
pub fn steam_banner(steam_root: &Path, app_id: &str) -> Option<String> {
    let cache = steam_root.join("appcache").join("librarycache");
    let per_app = cache.join(app_id);
    let find = |name: &str| -> Option<PathBuf> {
        let direct = per_app.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        crate::util::subdirs(&per_app).into_iter().map(|d| d.join(name)).find(|p| p.is_file())
    };
    let legacy = |name: &str| Some(cache.join(format!("{app_id}_{name}"))).filter(|p| p.is_file());
    find("library_header.jpg")
        .or_else(|| find("header.jpg"))
        .or_else(|| legacy("header.jpg"))
        .or_else(|| find("library_hero.jpg"))
        .or_else(|| legacy("library_hero.jpg"))
        .or_else(|| find("library_capsule.jpg"))
        .or_else(|| find("library_600x900.jpg"))
        .or_else(|| legacy("library_600x900.jpg"))
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn banners_new_and_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("appcache/librarycache");
        fs::create_dir_all(cache.join("10/abc")).unwrap();
        fs::write(cache.join("10/abc/library_header.jpg"), b"jpg").unwrap();
        fs::write(cache.join("10/library_hero.jpg"), b"jpg").unwrap();
        fs::write(cache.join("20_header.jpg"), b"jpg").unwrap();
        assert!(steam_banner(dir.path(), "10").unwrap().ends_with("10/abc/library_header.jpg"));
        assert!(steam_banner(dir.path(), "20").unwrap().ends_with("20_header.jpg"));
        assert!(steam_banner(dir.path(), "30").is_none());
    }

    use super::*;
    use std::fs;

    #[test]
    fn discovers_a_steam_game() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let steam = home.join(".local/share/Steam");
        let apps = steam.join("steamapps");
        let game = apps.join("common/Cool Game");
        fs::create_dir_all(&game).unwrap();
        fs::create_dir_all(apps.join("compatdata/123/pfx")).unwrap();
        fs::write(game.join("Cool.exe"), b"MZ").unwrap();
        fs::write(apps.join("appmanifest_123.acf"), "\"AppState\"\n{\n\t\"appid\"\t\t\"123\"\n\t\"name\"\t\t\"Cool Game\"\n\t\"installdir\"\t\t\"Cool Game\"\n}\n").unwrap();
        fs::write(apps.join("appmanifest_9.acf"), "\"appid\" \"9\"\n\"name\" \"Proton 9.0\"\n\"installdir\" \"Proton 9.0\"\n").unwrap();
        let src = SteamSource::new(ScanRoots { home: Some(home.to_path_buf()), xdg_data: Some(home.join(".local/share").to_string_lossy().into()), xdg_config: None });
        let diag = src.diagnose();
        assert_eq!(diag.discovered_games.len(), 1, "{diag:#?}");
        let g = &diag.discovered_games[0];
        assert_eq!(g.id, "123");
        assert_eq!(g.name, "Cool Game");
        assert!(g.prefix_path.as_deref().unwrap().ends_with("compatdata/123/pfx"));
        assert!(diag.skipped_items.iter().any(|s| s.reason.contains("Proton")));
    }

    #[test]
    fn vdf_paths() {
        let v = r#""libraryfolders" { "0" { "path" "/mnt/games/SteamLibrary" } "1" { "path" "/home/u/.local/share/Steam" } }"#;
        assert_eq!(library_paths(v), vec!["/mnt/games/SteamLibrary", "/home/u/.local/share/Steam"]);
    }
}
