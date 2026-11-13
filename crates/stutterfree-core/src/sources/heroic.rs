//! Heroic (native and Flatpak): Legendary (Epic) installed.json, sideloaded apps and GOG (HeroicLinuxSource.cs).

use super::{Diag, GameSourceScanner, ScanRoots};
use crate::engine_detector;
use crate::models::{GameInfo, GameSource, SourceDiagnostic};
use crate::paths::{canonical_path, xdg_config_home_with};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct HeroicSource {
    roots: ScanRoots,
}

impl HeroicSource {
    pub fn new(roots: ScanRoots) -> Self {
        HeroicSource { roots }
    }
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn is_true(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool) == Some(true)
}

fn not_blank(o: &Option<String>) -> bool {
    o.as_deref().map(|x| !x.trim().is_empty()).unwrap_or(false)
}

impl GameSourceScanner for HeroicSource {
    fn source_type(&self) -> GameSource {
        GameSource::Heroic
    }

    fn diagnose(&self) -> SourceDiagnostic {
        let mut d = Diag::new();
        let home = self.roots.home();
        let xdg_config = xdg_config_home_with(Some(&home), self.roots.xdg_config.as_deref());
        let default_config = home.join(".config");
        let mut candidates: Vec<(PathBuf, &str, bool)> = Vec::new();
        if canonical_path(&xdg_config) != canonical_path(&default_config) {
            candidates.push((xdg_config.join("heroic"), "Native Heroic ($XDG_CONFIG_HOME/heroic)", false));
        }
        candidates.push((home.join(".config/heroic"), "Native Heroic (~/.config/heroic)", false));
        candidates.push((
            home.join(".var/app/com.heroicgameslauncher.hgl/config/heroic"),
            "Flatpak Heroic (~/.var/app/com.heroicgameslauncher.hgl/config/heroic)",
            true,
        ));
        let mut active = Vec::new();
        let mut seen = HashSet::new();
        for (p, desc, fp) in candidates {
            let exists = p.is_dir();
            d.check(&p, desc, exists, fp);
            if exists && seen.insert(canonical_path(&p)) {
                active.push((p, fp));
            }
        }
        for (cfg, fp) in active {
            let mut seen_ids = HashSet::new();
            legendary(&mut d, &cfg, fp, &mut seen_ids);
            sideload(&mut d, &cfg, fp, &mut seen_ids);
            gog(&mut d, &cfg, fp, &mut seen_ids);
        }
        d.finish(GameSource::Heroic, None)
    }
}

fn push_game(d: &mut Diag, cfg: &Path, fp: bool, app_id: &str, title: &str, install: &str, exe: String) {
    let prefix = read_game_prefix(cfg, app_id, Some(title));
    let (engine, est) = engine_detector::detect(install);
    d.found.push(GameInfo::new(
        if fp { format!("flatpak_{app_id}") } else { app_id.to_string() },
        title.to_string(),
        GameSource::Heroic,
        install.to_string(),
        exe,
        prefix,
        None,
        engine,
        est,
        fp,
    ));
}

fn legendary(d: &mut Diag, cfg: &Path, fp: bool, seen: &mut HashSet<String>) {
    let file = cfg.join("legendaryConfig/legendary/installed.json");
    let exists = file.is_file();
    d.check(&file, if fp { "Flatpak Heroic Legendary (Epic) installed.json" } else { "Heroic Legendary (Epic) installed.json" }, exists, fp);
    if !exists {
        return;
    }
    let fs_ = file.to_string_lossy().into_owned();
    let doc: Value = match std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
        Ok(v) => v,
        Err(e) => {
            d.skip("installed.json", fs_, format!("Invalid JSON: {e}"), fp);
            return;
        }
    };
    let Some(obj) = doc.as_object() else { return };
    for (app_id, entry) in obj {
        let title = s(entry, "title").filter(|t| !t.trim().is_empty()).unwrap_or_else(|| app_id.clone());
        let label = format!("{title} ({app_id})");
        if is_true(entry, "is_dlc") {
            d.skip(label, fs_.clone(), "DLC entry", fp);
            continue;
        }
        let install = s(entry, "install_path");
        let exe_name = s(entry, "executable");
        let Some(install) = install.filter(|p| !p.trim().is_empty() && Path::new(p).is_dir()) else {
            d.skip(label, s(entry, "install_path").unwrap_or_else(|| fs_.clone()), "Game install directory missing on disk", fp);
            continue;
        };
        let mut exe = match exe_name.filter(|e| !e.trim().is_empty()) {
            Some(e) => Path::new(&install).join(e.replace('\\', "/")).to_string_lossy().into_owned(),
            None => engine_detector::find_executable(&install).unwrap_or_default(),
        };
        if !Path::new(&exe).is_file() {
            exe = engine_detector::find_executable(&install).unwrap_or_default();
        }
        if exe.is_empty() || !Path::new(&exe).is_file() {
            d.skip(label, install, "No Windows .exe found (Linux native build or empty folder)", fp);
            continue;
        }
        if !seen.insert(app_id.to_ascii_lowercase()) {
            continue;
        }
        push_game(d, cfg, fp, app_id, &title, &install, exe);
    }
}

fn sideload(d: &mut Diag, cfg: &Path, fp: bool, seen: &mut HashSet<String>) {
    let file = cfg.join("sideload_apps/library.json");
    let exists = file.is_file();
    d.check(&file, if fp { "Flatpak Heroic sideload_apps/library.json" } else { "Heroic sideload_apps/library.json" }, exists, fp);
    if !exists {
        return;
    }
    let fs_ = file.to_string_lossy().into_owned();
    let doc: Value = match std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
        Ok(v) => v,
        Err(e) => {
            d.skip("sideload_apps/library.json", fs_, format!("Invalid JSON: {e}"), fp);
            return;
        }
    };
    let Some(games) = doc.get("games").and_then(Value::as_array) else { return };
    for item in games {
        let Some(app_id) = s(item, "app_name").filter(|a| !a.trim().is_empty()) else { continue };
        let title = s(item, "title").filter(|t| !t.trim().is_empty()).unwrap_or_else(|| app_id.clone());
        let label = format!("{title} ({app_id})");
        if item.get("is_installed").and_then(Value::as_bool) == Some(false) {
            d.skip(label, fs_.clone(), "Marked as not installed", fp);
            continue;
        }
        let mut exe: Option<String> = None;
        if let Some(inst) = item.get("install").filter(|v| v.is_object()) {
            if is_true(inst, "is_dlc") {
                d.skip(label, fs_.clone(), "DLC entry", fp);
                continue;
            }
            exe = s(inst, "executable");
        }
        let folder = s(item, "folder_name");
        let mut install: Option<String> = None;
        if not_blank(&exe) && Path::new(exe.as_deref().unwrap()).is_file() {
            install = Some(engine_detector::infer_install_root(exe.as_deref().unwrap(), folder.as_deref()));
        } else if not_blank(&folder) && Path::new(folder.as_deref().unwrap()).is_dir() {
            install = folder.clone();
            exe = engine_detector::find_executable(folder.as_deref().unwrap());
        }
        let Some(install) = install.filter(|i| !i.trim().is_empty() && Path::new(i).is_dir()) else {
            d.skip(label, folder.or(exe).unwrap_or_else(|| fs_.clone()), "Game install directory missing on disk", fp);
            continue;
        };
        let Some(exe) = exe.filter(|e| !e.trim().is_empty() && Path::new(e).is_file() && crate::util::ends_with_ci(e, ".exe")) else {
            d.skip(label, install, "No Windows .exe found", fp);
            continue;
        };
        if !seen.insert(app_id.to_ascii_lowercase()) {
            continue;
        }
        push_game(d, cfg, fp, &app_id, &title, &install, exe);
    }
}

fn gog(d: &mut Diag, cfg: &Path, fp: bool, seen: &mut HashSet<String>) {
    let file = cfg.join("gog_store/installed.json");
    let exists = file.is_file();
    d.check(&file, if fp { "Flatpak Heroic GOG installed.json" } else { "Heroic GOG installed.json" }, exists, fp);
    if !exists {
        return;
    }
    let fs_ = file.to_string_lossy().into_owned();
    let doc: Value = match std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
        Ok(v) => v,
        Err(e) => {
            d.skip("gog_store/installed.json", fs_, format!("Invalid JSON: {e}"), fp);
            return;
        }
    };
    let Some(arr) = doc.get("installed").and_then(Value::as_array) else { return };
    for item in arr {
        let Some(app_id) = s(item, "appName").filter(|a| !a.trim().is_empty()) else { continue };
        if is_true(item, "is_dlc") {
            d.skip(app_id, fs_.clone(), "GOG DLC entry", fp);
            continue;
        }
        let Some(install) = s(item, "install_path").filter(|p| !p.trim().is_empty() && Path::new(p).is_dir()) else {
            d.skip(app_id, s(item, "install_path").unwrap_or_else(|| fs_.clone()), "GOG install directory missing on disk", fp);
            continue;
        };
        let Some(exe) = engine_detector::find_executable(&install) else {
            d.skip(app_id, install, "No Windows .exe found (Linux native GOG build or empty folder)", fp);
            continue;
        };
        if !seen.insert(app_id.to_ascii_lowercase()) {
            continue;
        }
        let mut title = crate::util::file_name(Path::new(&crate::paths::trim_sep(&install)));
        if title.trim().is_empty() {
            title = app_id.clone();
        }
        push_game(d, cfg, fp, &app_id, &title, &install, exe);
    }
}

fn read_game_prefix(cfg: &Path, game_id: &str, title: Option<&str>) -> Option<String> {
    let game_cfg = cfg.join("GamesConfig").join(format!("{game_id}.json"));
    if let Ok(text) = std::fs::read_to_string(&game_cfg) {
        if let Some(p) = read_wine_prefix(&text, game_id).filter(|p| !p.trim().is_empty()) {
            return Some(p);
        }
    }
    let global = cfg.join("config.json");
    if let Ok(text) = std::fs::read_to_string(&global) {
        if let Ok(doc) = serde_json::from_str::<Value>(&text) {
            if let Some(ds) = doc.get("defaultSettings").filter(|v| v.is_object()) {
                let base = s(ds, "defaultWinePrefix").or_else(|| s(ds, "defaultWinePrefixDir"));
                if let (Some(base), Some(title)) = (base.filter(|b| !b.trim().is_empty()), title.filter(|t| !t.trim().is_empty())) {
                    for cand in [Path::new(&base).join(title), Path::new(&base).join("default").join(title)] {
                        if cand.is_dir() {
                            return Some(cand.to_string_lossy().into_owned());
                        }
                    }
                }
                if let Some(wp) = s(ds, "winePrefix").filter(|w| !w.trim().is_empty()) {
                    return Some(wp);
                }
            }
        }
    }
    None
}

/// The game's Wine prefix from its Heroic config file (settings under a key named after the game's id; a plain
/// top-level "winePrefix" is accepted too).
pub fn read_wine_prefix(json: &str, game_id: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(json).ok()?;
    let settings = doc.get(game_id).filter(|v| v.is_object()).unwrap_or(&doc);
    s(settings, "winePrefix")
}

/// The Proton binary (`wineVersion.bin`) a Heroic config names, when it is a Proton script.
pub fn read_proton_binary(json: &str, game_id: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(json).ok()?;
    let settings = doc.get(game_id).filter(|v| v.is_object()).unwrap_or(&doc);
    let wv = settings.get("wineVersion").filter(|v| v.is_object())?;
    let kind = s(wv, "type");
    let bin = s(wv, "bin").filter(|b| !b.trim().is_empty())?;
    (kind.map(|k| k.eq_ignore_ascii_case("proton")).unwrap_or(false) || crate::util::ends_with_ci(&bin, "/proton")).then_some(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_and_proton_from_config() {
        let j = r#"{"Fortnite":{"winePrefix":"/home/u/Games/Heroic/Prefixes/default/F","wineVersion":{"type":"proton","bin":"/x/GE-Proton/proton"}},"version":"v0"}"#;
        assert_eq!(read_wine_prefix(j, "Fortnite").as_deref(), Some("/home/u/Games/Heroic/Prefixes/default/F"));
        assert_eq!(read_proton_binary(j, "Fortnite").as_deref(), Some("/x/GE-Proton/proton"));
        assert_eq!(read_wine_prefix(r#"{"winePrefix":"/p"}"#, "x").as_deref(), Some("/p"));
    }
}
