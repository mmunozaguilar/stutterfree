//! Lutris (native and Flatpak) from its pga.db (LutrisLinuxSource.cs). Layouts handled:
//! 1. a game installed in its own folder (`directory` is the game's folder);
//! 2. an Epic Games Store game installed through Lutris (service "egs"): `directory` is the launcher's Wine prefix, and
//!    the game's folder comes from the Epic launcher's manifest inside that prefix;
//! 3. a game added by its exe (`directory` empty, the game's YAML config names `exe` and `prefix`).

use super::{Diag, GameSourceScanner, ScanRoots};
use crate::engine_detector;
use crate::models::{GameInfo, GameSource, SourceDiagnostic};
use crate::paths::{canonical_path, xdg_config_home_with, xdg_data_home_with};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct LutrisSource {
    roots: ScanRoots,
}

impl LutrisSource {
    pub fn new(roots: ScanRoots) -> Self {
        LutrisSource { roots }
    }
}

struct Row {
    id: i64,
    slug: Option<String>,
    name: Option<String>,
    runner: Option<String>,
    directory: Option<String>,
    service: Option<String>,
    service_id: Option<String>,
    configpath: Option<String>,
}

fn read_rows(db: &Path) -> Result<Vec<Row>, String> {
    let conn = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("SELECT id, slug, name, runner, directory, installed, service, service_id, configpath FROM games WHERE installed = 1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                slug: r.get(1)?,
                name: r.get(2)?,
                runner: r.get(3)?,
                directory: r.get(4)?,
                service: r.get(6)?,
                service_id: r.get(7)?,
                configpath: r.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

fn blank(s: &Option<String>) -> bool {
    s.as_deref().map(|x| x.trim().is_empty()).unwrap_or(true)
}

impl GameSourceScanner for LutrisSource {
    fn source_type(&self) -> GameSource {
        GameSource::Lutris
    }

    fn diagnose(&self) -> SourceDiagnostic {
        let mut d = Diag::new();
        let home = self.roots.home();
        let xdg_data = xdg_data_home_with(Some(&home), self.roots.xdg_data.as_deref());
        let xdg_config = xdg_config_home_with(Some(&home), self.roots.xdg_config.as_deref());
        let default_data = home.join(".local/share");
        let default_config = home.join(".config");

        let mut instances: Vec<(PathBuf, PathBuf, &str, bool)> = Vec::new();
        if canonical_path(&xdg_data) != canonical_path(&default_data) || canonical_path(&xdg_config) != canonical_path(&default_config) {
            instances.push((xdg_data.join("lutris"), xdg_config.join("lutris"), "Native Lutris ($XDG_DATA_HOME/lutris)", false));
        }
        instances.push((home.join(".local/share/lutris"), home.join(".config/lutris"), "Native Lutris (~/.local/share/lutris)", false));
        instances.push((
            home.join(".var/app/net.lutris.Lutris/data/lutris"),
            home.join(".var/app/net.lutris.Lutris/config/lutris"),
            "Flatpak Lutris (~/.var/app/net.lutris.Lutris/data/lutris)",
            true,
        ));

        let mut seen_dbs = HashSet::new();
        for (data_dir, config_dir, desc, fp) in instances {
            let db = data_dir.join("pga.db");
            let exists = db.is_file();
            d.check(&db, format!("{desc} pga.db"), exists, fp);
            if !exists || !seen_dbs.insert(canonical_path(&db)) {
                continue;
            }
            let mut config_dirs: Vec<PathBuf> = Vec::new();
            for c in [data_dir.join("games"), config_dir.join("games"), home.join(".local/share/lutris/games"), home.join(".config/lutris/games")] {
                if !config_dirs.contains(&c) {
                    config_dirs.push(c);
                }
            }
            match read_rows(&db) {
                Ok(rows) => {
                    for row in rows {
                        scan_row(&mut d, row, &data_dir, &config_dirs, fp, &db);
                    }
                }
                Err(e) => d.skip("pga.db", db.to_string_lossy(), format!("SQLite read error: {e}"), fp),
            }
        }
        d.finish(GameSource::Lutris, None)
    }
}

fn scan_row(d: &mut Diag, row: Row, data_dir: &Path, config_dirs: &[PathBuf], fp: bool, db: &Path) {
    let raw_id = row.id.to_string();
    let slug = row.slug.clone().unwrap_or_else(|| raw_id.clone());
    let name = row.name.clone().unwrap_or_else(|| slug.clone());
    let label = format!("{name} (id={raw_id}, slug={slug})");
    if let Some(runner) = row.runner.as_deref().filter(|r| !r.trim().is_empty()) {
        if !runner.eq_ignore_ascii_case("wine") && !runner.eq_ignore_ascii_case("proton") {
            d.skip(label, row.directory.clone().unwrap_or_else(|| db.to_string_lossy().into()), format!("Non-Wine/Proton runner '{runner}'"), fp);
            return;
        }
    }
    let install_path: String;
    let mut exe: Option<String> = None;
    let mut prefix = row.directory.clone();
    let mut gl_cache = row.directory.clone();
    let directory = row.directory.clone().filter(|x| !x.trim().is_empty() && Path::new(x).is_dir());
    if let Some(dir) = directory {
        let mut ip = dir.clone();
        if row.service.as_deref().map(|s| s.eq_ignore_ascii_case("egs")).unwrap_or(false) && !blank(&row.service_id) {
            match resolve_epic_game(&dir, row.service_id.as_deref().unwrap()) {
                Some((i, e)) => {
                    ip = i;
                    exe = e;
                }
                None => {
                    d.skip(
                        label,
                        dir,
                        format!("Epic Games Store manifest for '{}' missing or incomplete in Wine prefix", row.service_id.unwrap_or_default()),
                        fp,
                    );
                    return;
                }
            }
        } else if Path::new(&dir).join("drive_c").is_dir() {
            let cfg = read_game_config_from_dirs(config_dirs, row.configpath.as_deref());
            match cfg.get("exe") {
                Some(cfg_exe) if Path::new(cfg_exe).is_file() && !crate::util::ends_with_ci(cfg_exe, "EpicGamesLauncher.exe") => {
                    exe = Some(cfg_exe.clone());
                    ip = engine_detector::infer_install_root(cfg_exe, None);
                    if let Some(p) = cfg.get("prefix").filter(|p| !p.trim().is_empty()) {
                        prefix = Some(p.clone());
                    }
                }
                _ => {
                    d.skip(label, dir, "Bare Wine prefix / launcher container (not a standalone game)", fp);
                    return;
                }
            }
        }
        install_path = ip;
    } else {
        // A game added with its exe only: Lutris then points the NVIDIA cache at the exe's folder.
        let cfg = read_game_config_from_dirs(config_dirs, row.configpath.as_deref());
        let Some(cfg_exe) = cfg.get("exe").filter(|e| Path::new(e).is_file()).cloned() else {
            d.skip(label, row.configpath.clone().unwrap_or_else(|| db.to_string_lossy().into()), "Directory empty and no existing 'exe' found in Lutris YAML config", fp);
            return;
        };
        install_path = engine_detector::infer_install_root(&cfg_exe, None);
        prefix = cfg.get("prefix").filter(|p| !p.trim().is_empty() && (Path::new(p).is_dir() || Path::new(p).is_absolute())).cloned();
        gl_cache = crate::util::parent_str(&cfg_exe);
        exe = Some(cfg_exe);
    }
    let exe = match exe.or_else(|| engine_detector::find_executable(&install_path)) {
        Some(e) if !e.is_empty() => e,
        _ => {
            d.skip(label, install_path, "No Windows .exe found in install directory", fp);
            return;
        }
    };
    let (engine, est) = engine_detector::detect(&install_path);
    let mut banner = data_dir.join("coverart").join(format!("{slug}.jpg"));
    if !banner.is_file() {
        banner = data_dir.join("banners").join(format!("{slug}.jpg"));
    }
    let banner = banner.is_file().then(|| banner.to_string_lossy().into_owned());
    let mut g = GameInfo::new(
        if fp { format!("flatpak_{raw_id}") } else { raw_id },
        name,
        GameSource::Lutris,
        install_path,
        exe,
        prefix,
        banner,
        engine,
        est,
        fp,
    );
    g.gl_cache_path = gl_cache;
    d.found.push(g);
}

pub fn read_game_config_from_dirs(dirs: &[PathBuf], config_path: Option<&str>) -> HashMap<String, String> {
    let Some(cp) = config_path.filter(|c| !c.trim().is_empty()) else { return HashMap::new() };
    for dir in dirs {
        let file = dir.join(format!("{cp}.yml"));
        if let Ok(text) = std::fs::read_to_string(&file) {
            return parse_game_section(text.lines());
        }
    }
    HashMap::new()
}

/// The `key: value` lines directly under the top-level `game:` of a Lutris config (two-space indent). Keys are
/// lowercased (the C# dictionary compared them case-insensitively).
pub fn parse_game_section<'a>(lines: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut in_game = false;
    for raw in lines {
        if raw.is_empty() || raw.trim_start().starts_with('#') {
            continue;
        }
        let first = raw.chars().next().unwrap();
        if !first.is_whitespace() {
            in_game = raw.trim_end() == "game:";
            continue;
        }
        let b = raw.as_bytes();
        if !in_game || b.len() < 3 || b[0] != b' ' || b[1] != b' ' || b[2] == b' ' {
            continue;
        }
        let Some(colon) = raw.find(':') else { continue };
        if colon < 2 {
            continue;
        }
        let key = raw[2..colon].trim().to_ascii_lowercase();
        let value = raw[colon + 1..].trim().trim_matches(|c| c == '\'' || c == '"').to_string();
        if !key.is_empty() && !value.is_empty() {
            out.insert(key, value);
        }
    }
    out
}

/// The install folder and exe of the Epic game `app_name` in the Wine prefix, from the Epic launcher's manifests.
pub fn resolve_epic_game(prefix: &str, app_name: &str) -> Option<(String, Option<String>)> {
    let manifests = Path::new(prefix).join("drive_c/ProgramData/Epic/EpicGamesLauncher/Data/Manifests");
    let rd = std::fs::read_dir(&manifests).ok()?;
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|e| e == "item").unwrap_or(false)).collect();
    files.sort();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let an = doc.get("AppName").and_then(|v| v.as_str()).unwrap_or("");
        if !an.eq_ignore_ascii_case(app_name) {
            continue;
        }
        if doc.get("bIsIncompleteInstall").and_then(|v| v.as_bool()) == Some(true) {
            return None;
        }
        let location = doc.get("InstallLocation").and_then(|v| v.as_str());
        let install = windows_path_to_linux(prefix, location)?;
        if !Path::new(&install).is_dir() {
            return None;
        }
        let exe = doc
            .get("LaunchExecutable")
            .and_then(|v| v.as_str())
            .filter(|l| !l.trim().is_empty())
            .map(|l| Path::new(&install).join(l.replace('\\', "/")))
            .filter(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned());
        return Some((install, exe));
    }
    None
}

/// A Windows path inside a Wine prefix ("C:\Program Files\X") as a Linux path; None for a drive it can't map.
pub fn windows_path_to_linux(prefix: &str, win: Option<&str>) -> Option<String> {
    let w = win?;
    let chars: Vec<char> = w.chars().collect();
    if w.trim().is_empty() || chars.len() < 3 || chars[1] != ':' {
        return None;
    }
    let letter = chars[0].to_ascii_lowercase();
    let rest: String = chars[2..].iter().collect::<String>().replace('\\', "/").trim_start_matches('/').to_string();
    let root = if letter == 'c' { Path::new(prefix).join("drive_c") } else { Path::new(prefix).join("dosdevices").join(format!("{letter}:")) };
    root.is_dir().then(|| root.join(rest).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_game_section() {
        let y = "game:\n  exe: /games/x/X.exe\n  prefix: '/games/pfx'\n  args: \nsystem:\n  exe: nope\n";
        let m = parse_game_section(y.lines());
        assert_eq!(m.get("exe").map(String::as_str), Some("/games/x/X.exe"));
        assert_eq!(m.get("prefix").map(String::as_str), Some("/games/pfx"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn epic_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let pfx = dir.path().to_string_lossy().into_owned();
        let man = dir.path().join("drive_c/ProgramData/Epic/EpicGamesLauncher/Data/Manifests");
        std::fs::create_dir_all(&man).unwrap();
        let game = dir.path().join("drive_c/Program Files/Epic Games/G");
        std::fs::create_dir_all(game.join("Bin")).unwrap();
        std::fs::write(game.join("Bin/G.exe"), b"MZ").unwrap();
        std::fs::write(
            man.join("a.item"),
            r#"{"AppName":"abc","InstallLocation":"C:\\Program Files\\Epic Games\\G","LaunchExecutable":"Bin\\G.exe"}"#,
        )
        .unwrap();
        let (i, e) = resolve_epic_game(&pfx, "ABC").unwrap();
        assert_eq!(i, game.to_string_lossy());
        assert!(e.unwrap().ends_with("Bin/G.exe"));
    }
}
