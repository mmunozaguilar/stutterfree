//! Record mode (Games/RecorderManager.cs): the d3d12.dll recorder (built from native/recorder.cpp with mingw-w64)
//! goes next to the game's exe, where Wine loads it before the prefix's vkd3d-proton d3d12.dll. It writes
//! shaderless.db there (the inbox); Shaderless imports it into games/<id>/recording.db and writes shaderless.keys back so
//! nothing is recorded twice.

use crate::gamefiles::{self, is_our_proxy};
use crate::models::{AntiCheat, CoreGame, GameInfo, GameSource, Store};
use crate::psodb::{self, Rec};
use crate::recordings::{self, KEYS_FILE, WARMED_FILE};
use crate::util::non_empty;
use anyhow::{anyhow, bail, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const INI_TEXT: &str =
    "[shaderless]\r\n; written by StutterFree (based on Shaderless, the unofficial Linux port of SCSKiller): the recorder records every pipeline the game creates into shaderless.db\r\nmode=record\r\n";
const ARMED_TEXT: &str = "[shaderless]\r\narmed=1\r\n";

pub fn data_dir() -> PathBuf {
    crate::paths::data_dir()
}

fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c == '/' || c == '\0' || c == ':' { '_' } else { c }).collect()
}

pub fn game_dir(game_id: &str) -> PathBuf {
    data_dir().join("games").join(sanitize(game_id))
}
pub fn recording_path(game_id: &str) -> PathBuf {
    game_dir(game_id).join("recording.db")
}
pub fn compile_note_path(game_id: &str) -> PathBuf {
    game_dir(game_id).join("compile-note.txt")
}
pub fn plan_label_path(game_id: &str) -> PathBuf {
    game_dir(game_id).join("plan-label.txt")
}
pub fn warmed_keys_path(game_id: &str) -> PathBuf {
    game_dir(game_id).join("warmed-keys.txt")
}

/// Remembers a note about the last compile (None removes it), shown on the game's card while its cache exists.
pub fn save_compile_note(game_id: &str, note: Option<&str>) {
    save_text(game_id, compile_note_path(game_id), note);
}

/// Remembers what the last compile planned from the game's files ("38,426 pipelines · Unreal 5.5 D3D12"; None removes it).
pub fn save_plan_label(game_id: &str, label: Option<&str>) {
    save_text(game_id, plan_label_path(game_id), label);
}

pub fn load_plan_label(game_id: &str) -> Option<String> {
    fs::read_to_string(plan_label_path(game_id)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn save_text(game_id: &str, path: PathBuf, note: Option<&str>) {
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        None => {
            let _ = fs::remove_file(path);
        }
        Some(n) => {
            let _ = fs::create_dir_all(game_dir(game_id));
            let _ = fs::write(path, n);
        }
    }
}

pub fn load_compile_note(game_id: &str) -> Option<String> {
    fs::read_to_string(compile_note_path(game_id)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

const LEGACY_PAIRS: [(&str, &str); 9] = [
    ("scskiller.db", "shaderless.db"),
    ("scskiller_gen.db", "shaderless_gen.db"),
    ("scskiller.ini", "shaderless.ini"),
    ("scskiller.armed", "shaderless.armed"),
    ("scskiller.keys", "shaderless.keys"),
    ("scskiller.warmed", "shaderless.warmed"),
    ("scskiller.log", "shaderless.log"),
    ("scskiller_creates.csv", "shaderless_creates.csv"),
    ("scskiller_failed.keys", "shaderless_failed.keys"),
];

/// Renames files of builds from before the rename (scskiller.*) and upgrades an old recorder in place.
pub fn migrate_legacy_files(dir: Option<&Path>) {
    let Some(dir) = dir.filter(|d| d.is_dir()) else { return };
    let dll = dir.join("d3d12.dll");
    let legacy_armed = dir.join("scskiller.armed");
    let legacy_ini = dir.join("scskiller.ini");
    if (legacy_armed.is_file() || legacy_ini.is_file()) && dll.is_file() && is_our_proxy(&dll) {
        if let Some(src) = proxy_source() {
            let _ = fs::copy(src, &dll);
        }
        if legacy_ini.is_file() {
            let _ = fs::write(dir.join("shaderless.ini"), INI_TEXT);
            let _ = fs::remove_file(&legacy_ini);
        }
        if legacy_armed.is_file() {
            let _ = fs::write(dir.join("shaderless.armed"), ARMED_TEXT);
            let _ = fs::remove_file(&legacy_armed);
        }
    }
    for (legacy, current) in LEGACY_PAIRS {
        let lp = dir.join(legacy);
        let cp = dir.join(current);
        if lp.is_file() && !cp.exists() {
            let _ = fs::rename(lp, cp);
        }
    }
}

fn proxy_source() -> Option<PathBuf> {
    crate::paths::find_native("d3d12.dll").filter(|p| is_our_proxy(p))
}

/// The folder of the exe that really runs (an Unreal game's Shipping exe, not its launcher stub).
pub fn exe_dir(game: &GameInfo) -> Option<String> {
    let exe = crate::engine_detector::resolve_real_game_executable(&game.install_path, Some(&game.executable_path))?;
    crate::util::parent_str(&exe)
}

pub fn is_installed(game: &GameInfo) -> bool {
    exe_dir(game).map(|d| is_our_proxy(&Path::new(&d).join("d3d12.dll"))).unwrap_or(false)
}

fn store_of(source: GameSource) -> Store {
    match source {
        GameSource::Steam => Store::Steam,
        GameSource::Heroic => Store::Epic,
        _ => Store::Manual,
    }
}

pub fn to_core(game: &GameInfo, exe_dir: &str) -> CoreGame {
    let exe = if Path::new(&game.executable_path).is_file() && crate::util::parent_str(&game.executable_path).as_deref() == Some(exe_dir) {
        game.executable_path.clone()
    } else {
        crate::util::files_in(Path::new(exe_dir))
            .into_iter()
            .find(|f| f.extension().map(|e| e == "exe").unwrap_or(false))
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| game.executable_path.clone())
    };
    CoreGame { id: game.id.clone(), name: game.name.clone(), store: store_of(game.source), install_dir: game.install_path.clone(), exe_path: exe }
}

/// The anti-cheat found in the game's folder (None if there is none, or the exe wasn't found).
pub fn detect_anti_cheat(game: &GameInfo) -> AntiCheat {
    match exe_dir(game) {
        Some(d) => gamefiles::detect_anti_cheat(&to_core(game, &d), false),
        None => AntiCheat::None,
    }
}

static KEYS_CACHE: OnceLock<Mutex<HashMap<PathBuf, (u64, i64, HashSet<String>)>>> = OnceLock::new();

/// The pipeline keys of a recording, re-read only when its size or write time changed (a scan runs this per game).
fn keys_of(path: &Path) -> HashSet<String> {
    let Ok(m) = fs::metadata(path) else { return HashSet::new() };
    let (len, written) = (m.len(), m.modified().map(crate::util::ticks).unwrap_or(0));
    let cache = KEYS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((l, w, k)) = cache.lock().unwrap().get(path) {
        if *l == len && *w == written {
            return k.clone();
        }
    }
    let keys: HashSet<String> = match psodb::read(path) {
        Ok(it) => it.map_while(Result::ok).filter(|r| !matches!(r.tag, b'B' | b'N' | b'L' | b'W')).map(|r| r.key()).collect(),
        Err(_) => HashSet::new(),
    };
    cache.lock().unwrap().insert(path.to_path_buf(), (len, written, keys.clone()));
    keys
}

fn recorded_keys(game: &GameInfo) -> HashSet<String> {
    let mut keys = HashSet::new();
    let ed = exe_dir(game);
    migrate_legacy_files(ed.as_deref().map(Path::new));
    let mut paths = vec![recording_path(&game.id)];
    if let Some(d) = ed {
        paths.push(Path::new(&d).join("shaderless.db"));
    }
    for p in paths {
        if non_empty(&p) {
            keys.extend(keys_of(&p));
        }
    }
    keys
}

/// The recorder next to the exe is loaded into the running game (mapped in one of its processes). A game that loads
/// d3d12.dll straight from System32 (NVIDIA Streamline's interposer, Cyberpunk 2077) never loads it.
pub fn recorder_loaded(game: &GameInfo) -> bool {
    let Some(dir) = exe_dir(game) else { return false };
    let dll = Path::new(&dir).join("d3d12.dll").to_string_lossy().to_lowercase();
    crate::proton::game_pids(game).iter().any(|pid| {
        fs::read_to_string(format!("/proc/{pid}/maps")).map(|m| m.lines().any(|l| l.to_lowercase().ends_with(&dll))).unwrap_or(false)
    })
}

/// Pipelines recorded so far: Shaderless's copy plus what the inbox holds that the copy doesn't.
pub fn recorded_psos(game: &GameInfo) -> i32 {
    recorded_keys(game).len() as i32
}

/// Recorded pipelines not yet in warmed-keys.txt.
pub fn unwarmed_recorded_psos(game: &GameInfo) -> i32 {
    let warmed = load_warmed_keys(&game.id, None, None);
    if warmed.is_empty() {
        return 0;
    }
    recorded_keys(game).difference(&warmed).count() as i32
}

/// Installs the recorder on an eligible game (Record all games automatically). Skips anti-cheat games and other
/// mods' d3d12.dll.
pub fn try_auto_install(game: &GameInfo) -> bool {
    if is_installed(game) {
        return true;
    }
    if detect_anti_cheat(game) != AntiCheat::None {
        return false;
    }
    let Some(dir) = exe_dir(game).filter(|d| Path::new(d).is_dir()) else { return false };
    let dll = Path::new(&dir).join("d3d12.dll");
    if dll.is_file() && !is_our_proxy(&dll) {
        return false;
    }
    install(game).is_ok()
}

/// Installs and arms the recorder. Refuses next to anti-cheat and over another mod's d3d12.dll.
pub fn install(game: &GameInfo) -> Result<()> {
    let dir = exe_dir(game).ok_or_else(|| anyhow!("{}: the game's exe wasn't found", game.name))?;
    let src = proxy_source().ok_or_else(|| anyhow!("the recorder (native/d3d12.dll) was not found next to StutterFree"))?;
    let ac = gamefiles::detect_anti_cheat(&to_core(game, &dir), false);
    if ac != AntiCheat::None {
        bail!("{} uses {ac}: a d3d12.dll in its folder could get the account banned. Not installing.", game.name);
    }
    let d = Path::new(&dir);
    let dll = d.join("d3d12.dll");
    if dll.is_file() && !is_our_proxy(&dll) {
        bail!("{} already exists and is not StutterFree's (another mod or wrapper). Not replacing it.", dll.display());
    }
    migrate_legacy_files(Some(d));
    migrate_legacy_files(Some(&game_dir(&game.id)));
    fs::copy(&src, &dll)?;
    fs::write(d.join("shaderless.ini"), INI_TEXT)?;
    fs::write(d.join("shaderless.armed"), ARMED_TEXT)?;
    write_keys(game, d);
    sync_warmed_file_to_exe_dir(game, d);
    Ok(())
}

/// Imports what was recorded, then removes the recorder and its files.
pub fn uninstall(game: &GameInfo) -> Result<()> {
    let dir = exe_dir(game).ok_or_else(|| anyhow!("{}: the game's exe wasn't found", game.name))?;
    let _ = import(game, None);
    let d = Path::new(&dir);
    let dll = d.join("d3d12.dll");
    if is_our_proxy(&dll) {
        fs::remove_file(&dll)?;
    }
    for f in [
        "shaderless.ini",
        "shaderless.armed",
        "shaderless.log",
        "shaderless_creates.csv",
        KEYS_FILE,
        WARMED_FILE,
        "scskiller.ini",
        "scskiller.armed",
        "scskiller.log",
        "scskiller_creates.csv",
        "scskiller.keys",
        "scskiller.warmed",
    ] {
        let _ = fs::remove_file(d.join(f));
    }
    for db in ["shaderless.db", "scskiller.db"] {
        let inbox = d.join(db);
        if !non_empty(&inbox) {
            let _ = fs::remove_file(inbox);
        }
    }
    Ok(())
}

/// Merges the game folder's shaderless.db into Shaderless's compact copy and empties it (only while the game is not
/// running and no other process holds the inbox open). Returns the pipelines added.
pub fn import(game: &GameInfo, is_running: Option<&dyn Fn(&GameInfo) -> bool>) -> Result<usize> {
    let Some(dir) = exe_dir(game) else { return Ok(0) };
    let d = Path::new(&dir);
    migrate_legacy_files(Some(d));
    let inbox = d.join("shaderless.db");
    if !non_empty(&inbox) {
        return Ok(0);
    }
    let running = |g: &GameInfo| match is_running {
        Some(f) => f(g),
        None => crate::proton::is_game_running(g),
    };
    if running(game) {
        return Ok(0);
    }
    if is_running.is_none() && is_file_open_by_external_process(&inbox) {
        return Ok(0);
    }
    let store = recording_path(&game.id);
    fs::create_dir_all(store.parent().unwrap())?;
    let added;
    {
        let _lock = recordings::lock(&store)?;
        let len = fs::metadata(&inbox).map(|m| m.len()).unwrap_or(0);
        if len == 0 {
            return Ok(0);
        }
        added = recordings::merge(&store, Some(&inbox))?.len();
        let still_idle = !running(game);
        if still_idle && (is_running.is_some() || !is_file_open_by_external_process(&inbox)) {
            recordings::rotate(&inbox, len);
        }
    }
    write_keys(game, d);
    Ok(added)
}

/// Whether a process other than this one holds `file` open (/proc/<pid>/fd), e.g. the game writing its inbox.
pub fn is_file_open_by_external_process(file: &Path) -> bool {
    let Ok(full) = std::path::absolute(file) else { return false };
    let target = crate::nvcache::file_identity(&full).map(|(id, _)| id);
    let me = std::process::id().to_string();
    let Ok(rd) = fs::read_dir("/proc") else { return false };
    for e in rd.flatten() {
        let pid = e.file_name().to_string_lossy().into_owned();
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = fs::read_dir(e.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else { continue };
            if link == full {
                return true;
            }
            if let Some(t) = target {
                if link.file_name() == full.file_name() && crate::nvcache::file_identity(&link).map(|(id, _)| id) == Some(t) {
                    return true;
                }
            }
        }
    }
    false
}

/// shaderless.keys next to the recorder: what the copy already holds, so the next session records only what's new.
fn write_keys(game: &GameInfo, dir: &Path) {
    if !is_our_proxy(&dir.join("d3d12.dll")) {
        return;
    }
    let store = recording_path(&game.id);
    if !store.is_file() {
        return;
    }
    if let Ok(_l) = recordings::lock(&store) {
        let _ = recordings::write_keys(&store, &dir.join(KEYS_FILE));
    }
}

fn sync_warmed_file_to_exe_dir(game: &GameInfo, dir: &Path) {
    if !is_our_proxy(&dir.join("d3d12.dll")) {
        return;
    }
    migrate_legacy_files(Some(&game_dir(&game.id)));
    let src = game_dir(&game.id).join(WARMED_FILE);
    if src.is_file() {
        let _ = fs::copy(src, dir.join(WARMED_FILE));
    }
}

fn first_line(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().next().map(str::to_string)
}

pub fn warmed_stamp_mismatch_reason(game_id: &str, driver: Option<&str>, proton: Option<&str>) -> Option<String> {
    let line = first_line(&warmed_keys_path(game_id))?;
    let saved = line.strip_prefix("#STAMP:")?.trim().to_string();
    let pipe = saved.find('|').filter(|i| *i > 0)?;
    let (sd, sp) = (&saved[..pipe], &saved[pipe + 1..]);
    if let Some(cd) = driver.filter(|d| !d.is_empty()) {
        if !sd.is_empty() && !sd.eq_ignore_ascii_case("Unknown") && !cd.eq_ignore_ascii_case("Unknown") && sd != cd {
            return Some("driver updated".into());
        }
    }
    if let Some(cp) = proton.filter(|p| !p.is_empty()) {
        if !sp.is_empty() && !sp.eq_ignore_ascii_case("unknown") && !cp.eq_ignore_ascii_case("unknown") && sp != cp {
            return Some("Proton updated".into());
        }
    }
    None
}

/// The keys already compiled. With `expected_stamp`, a file stamped for another driver / Proton counts as empty. With
/// no file, a previous work folder's dbs are seeded from.
pub fn load_warmed_keys(game_id: &str, expected_stamp: Option<&str>, previous_work_dir: Option<&Path>) -> HashSet<String> {
    let mut out = HashSet::new();
    let path = warmed_keys_path(game_id);
    let Ok(text) = fs::read_to_string(&path) else {
        if let Some(prev) = previous_work_dir.filter(|p| p.is_dir()) {
            migrate_legacy_files(Some(prev));
            let db = prev.join("shaderless.db");
            if non_empty(&db) {
                for r in psodb::read_lenient(&db) {
                    if psodb::is_item(r.tag) {
                        out.insert(r.key());
                    }
                }
            }
        }
        return out;
    };
    let mut lines = text.lines();
    let Some(first) = lines.next() else { return out };
    if let Some(stamp) = first.strip_prefix("#STAMP:") {
        if let Some(exp) = expected_stamp.filter(|e| !e.is_empty()) {
            if stamp.trim() != exp {
                return out; // driver or Proton changed: full rebuild
            }
        }
    } else if first.trim().len() == 40 {
        out.insert(first.trim().to_string());
    }
    for l in lines {
        let t = l.trim();
        if t.len() == 40 {
            out.insert(t.to_string());
        }
    }
    out
}

pub fn build_warmed_snapshot(dbs: &[PathBuf]) -> Option<Vec<u8>> {
    let existing: Vec<PathBuf> = dbs.iter().filter(|d| non_empty(d)).cloned().collect();
    if existing.is_empty() {
        return None;
    }
    recordings::warmed(&existing, &[]).ok()
}

pub fn save_warmed_keys(game: &GameInfo, new_keys: &HashSet<String>, stamp: &str, dbs: Option<&[PathBuf]>, snapshot: Option<Vec<u8>>) {
    let gd = game_dir(&game.id);
    let _ = fs::create_dir_all(&gd);
    let mut combined: Vec<String> = load_warmed_keys(&game.id, Some(stamp), None).into_iter().collect();
    combined.extend(new_keys.iter().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()));
    combined.sort();
    combined.dedup();
    let mut text = format!("#STAMP:{stamp}\n");
    for k in &combined {
        text.push_str(k);
        text.push('\n');
    }
    let _ = crate::util::write_atomic(&warmed_keys_path(&game.id), text.as_bytes());
    let bytes = snapshot.or_else(|| dbs.and_then(build_warmed_snapshot));
    if let Some(b) = bytes.filter(|b| !b.is_empty()) {
        let _ = fs::write(gd.join(WARMED_FILE), b);
        if let Some(d) = exe_dir(game) {
            sync_warmed_file_to_exe_dir(game, Path::new(&d));
        }
    }
}

pub fn clear_warmed_keys(game_id: &str, game: Option<&GameInfo>) {
    let _ = fs::remove_file(warmed_keys_path(game_id));
    let _ = fs::remove_file(game_dir(game_id).join(WARMED_FILE));
    if let Some(d) = game.and_then(exe_dir) {
        let _ = fs::remove_file(Path::new(&d).join(WARMED_FILE));
    }
}

/// Filters the planned dbs down to what isn't compiled yet (so a re-compile only compiles new pipelines). Returns
/// (all item keys, executable records kept, already compiled).
pub fn filter_databases_to_unwarmed(rec_db: &Path, gen_db: &Path, warmed: &HashSet<String>) -> (HashSet<String>, usize, usize) {
    let mut all = HashSet::new();
    let mut unwarmed = HashSet::new();
    let mut templates = HashSet::new();
    for db in [rec_db, gen_db] {
        if !non_empty(db) {
            continue;
        }
        for r in psodb::read_lenient(db) {
            if psodb::is_item(r.tag) {
                let k = r.key();
                all.insert(k.clone());
                if warmed.is_empty() || !warmed.contains(&k) {
                    unwarmed.insert(k);
                    if matches!(r.tag, b'P' | b'A') && r.payload.len() >= 20 {
                        templates.insert(psodb::hex(&r.payload[..20]));
                    }
                }
            }
        }
    }
    let already = all.len() - unwarmed.len();
    if warmed.is_empty() || already == 0 || unwarmed.is_empty() {
        return (all, unwarmed.len(), already);
    }
    let mut kept_exec = 0;
    for db in [rec_db, gen_db] {
        if !non_empty(db) {
            continue;
        }
        let mut keep: Vec<Rec> = Vec::new();
        for r in psodb::read_lenient(db) {
            match r.tag {
                b'B' => keep.push(r),
                b'G' | b'C' | b'S' | b'R' | b'A' => {
                    let k = r.key();
                    if unwarmed.contains(&k) || templates.contains(&k) {
                        keep.push(r);
                        kept_exec += 1;
                    }
                }
                b'P' | b'1' | b'2' => {
                    if unwarmed.contains(&r.key()) {
                        keep.push(r);
                        kept_exec += 1;
                    }
                }
                _ => {}
            }
        }
        let _ = psodb::write_raw_file(db, &keep);
    }
    (all, kept_exec, already)
}

/// A graphics PSO with a vertex shader: the game's draws were recorded.
pub fn has_draws(db: &Path) -> bool {
    psodb::read(db).map(|it| it.map_while(Result::ok).any(|r| psodb::has_vertex_stage(&r))).unwrap_or(false)
}

/// Writes a recording's replayable records into `out_db` when the game's files can't be read statically: every
/// recorded 'B' blob, then each pipeline / state object whose blobs (and dependencies) are all there. Returns the number
/// of replayable pipelines written.
pub fn materialize_standalone_recording(store: &Path, out_db: &Path, cancel: &crate::proton::CancelToken) -> Result<usize> {
    if !non_empty(store) {
        return Ok(0);
    }
    let records: Vec<Rec> = {
        let _l = recordings::lock(store)?;
        psodb::read_all(store)?
    };
    if records.is_empty() {
        return Ok(0);
    }
    let available: HashSet<String> = records.iter().filter(|r| r.tag == b'B').filter_map(Rec::blob_hash).collect();
    // Middleware blobs (FSR4 / XeSS DLLs next to the exe) for hash-only recordings come with the planner port (phase 2).
    let mut kept_keys = HashSet::new();
    let mut kept: Vec<&Rec> = Vec::new();
    for r in &records {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        match r.tag {
            b'B' | b'L' | b'N' => {}
            b'W' => kept.push(r),
            t if psodb::is_item(t) => {
                let Ok(refs) = psodb::references([r]) else { continue };
                if !refs.is_subset(&available) {
                    continue;
                }
                if psodb::is_state_object(t) {
                    match psodb::parse_state_object(r) {
                        Ok(so) if so.depends.iter().all(|d| kept_keys.contains(d)) => {}
                        _ => continue,
                    }
                }
                kept_keys.insert(r.key());
                kept.push(r);
            }
            _ => {}
        }
    }
    if kept_keys.is_empty() {
        return Ok(0);
    }
    for r in &records {
        if r.tag == b'N' {
            if let Ok(nv) = psodb::parse_nv_state(r) {
                if kept_keys.contains(&nv.target) {
                    kept.push(r);
                }
            }
        }
    }
    if let Some(d) = out_db.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = crate::util::tmp_path(out_db);
    let result = (|| -> std::io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
        for r in records.iter().filter(|r| r.tag == b'B') {
            psodb::write(&mut w, b'B', &r.payload)?;
        }
        for r in &kept {
            psodb::write(&mut w, r.tag, &r.payload)?;
        }
        w.flush()?;
        drop(w);
        fs::rename(&tmp, out_db)
    })();
    let _ = fs::remove_file(&tmp);
    result?;
    Ok(kept_keys.len())
}

pub struct WarmerBatchSlice {
    pub batch_db_path: PathBuf,
    pub skip_prefix_count: usize,
    pub executable_item_count: usize,
}

pub fn count_executable_items(rec_db: &Path, gen_db: &Path) -> usize {
    let mut keys = HashSet::new();
    let mut plain = 0;
    for db in [rec_db, gen_db] {
        if !non_empty(db) {
            continue;
        }
        for r in psodb::read_lenient(db) {
            match r.tag {
                b'G' | b'C' | b'S' | b'R' | b'A' => {
                    keys.insert(r.key());
                }
                b'P' | b'1' | b'2' => plain += 1,
                _ => {}
            }
        }
    }
    keys.len() + plain
}

/// Splits the dbs into self-contained batch dbs of at most `max_items` executable pipelines each (100k+ PSO games would
/// otherwise outgrow NVIDIA's ~2 GiB writable bucket). Templates and state objects an item depends on from another
/// batch are prepended and skipped with the warmer's `--start`.
pub fn split_databases_into_batches(rec_db: &Path, gen_db: &Path, root: &Path, max_items: usize) -> Vec<WarmerBatchSlice> {
    let max_items = max_items.max(1);
    let mut blobs: HashMap<String, Vec<u8>> = HashMap::new();
    let mut unique: Vec<Rec> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut plan: Vec<Rec> = Vec::new();
    let mut d3d11: Vec<Rec> = Vec::new();
    for db in [rec_db, gen_db] {
        if !non_empty(db) {
            continue;
        }
        for r in psodb::read_lenient(db) {
            match r.tag {
                b'B' => {
                    if let Some(h) = r.blob_hash() {
                        blobs.entry(h).or_insert(r.payload);
                    }
                }
                b'G' | b'C' | b'S' | b'R' | b'A' => {
                    let k = r.key();
                    if !by_key.contains_key(&k) {
                        by_key.insert(k, unique.len());
                        unique.push(r);
                    }
                }
                b'P' => plan.push(r),
                b'1' | b'2' => d3d11.push(r),
                _ => {}
            }
        }
    }
    let all: Vec<&Rec> = unique.iter().chain(plan.iter()).chain(d3d11.iter()).collect();
    if all.is_empty() {
        return Vec::new();
    }
    let _ = fs::remove_dir_all(root);
    if fs::create_dir_all(root).is_err() {
        return Vec::new();
    }
    let unique_keys: Vec<String> = unique.iter().map(Rec::key).collect();
    let mut slices = Vec::new();
    for (bi, chunk) in all.chunks(max_items).enumerate() {
        let mut chunk_keys = HashSet::new();
        for r in chunk {
            if matches!(r.tag, b'G' | b'C' | b'S' | b'R' | b'A') {
                chunk_keys.insert(r.key());
            }
        }
        let mut deps: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        let enqueue = |k: String, deps: &mut HashSet<String>, queue: &mut VecDeque<String>| {
            if !chunk_keys.contains(&k) && by_key.contains_key(&k) && deps.insert(k.clone()) {
                queue.push_back(k);
            }
        };
        let rec_deps = |r: &Rec| -> Vec<String> {
            let mut v = Vec::new();
            if r.tag == b'A' && r.payload.len() >= 20 {
                v.push(psodb::hex(&r.payload[..20]));
            }
            if psodb::is_state_object(r.tag) {
                if let Ok(so) = psodb::parse_state_object(r) {
                    v.extend(so.depends);
                }
            }
            v
        };
        for r in chunk {
            if r.tag == b'P' && r.payload.len() >= 20 {
                enqueue(psodb::hex(&r.payload[..20]), &mut deps, &mut queue);
            }
            if matches!(r.tag, b'G' | b'C' | b'S' | b'R' | b'A') {
                for k in rec_deps(r) {
                    enqueue(k, &mut deps, &mut queue);
                }
            }
        }
        while let Some(k) = queue.pop_front() {
            if let Some(&i) = by_key.get(&k) {
                for d in rec_deps(&unique[i]) {
                    enqueue(d, &mut deps, &mut queue);
                }
            }
        }
        let dep_templates: Vec<&Rec> = unique.iter().zip(unique_keys.iter()).filter(|(_, k)| deps.contains(*k)).map(|(r, _)| r).collect();
        let mut non_blob: Vec<&Rec> = dep_templates.clone();
        non_blob.extend(chunk.iter().copied());
        let needed: Vec<String> = match psodb::references(non_blob.iter().copied()) {
            Ok(set) => set.into_iter().collect(),
            Err(_) => blobs.keys().cloned().collect(),
        };
        let dir = root.join(format!("batch_{bi:03}"));
        let _ = fs::create_dir_all(&dir);
        let db = dir.join("shaderless.db");
        let ok = (|| -> std::io::Result<()> {
            let mut w = BufWriter::with_capacity(1 << 20, fs::File::create(&db)?);
            for h in &needed {
                if let Some(b) = blobs.get(h) {
                    psodb::write(&mut w, b'B', b)?;
                }
            }
            for r in &non_blob {
                psodb::write(&mut w, r.tag, &r.payload)?;
            }
            w.flush()
        })();
        if ok.is_err() {
            return Vec::new();
        }
        slices.push(WarmerBatchSlice { batch_db_path: db, skip_prefix_count: dep_templates.len(), executable_item_count: chunk.len() });
    }
    slices
}

/// The file name of the recorder inbox, for messages.
pub fn inbox_name() -> &'static str {
    "shaderless.db"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(h: u8) -> Rec {
        let mut p = vec![h; 20];
        p.extend(b"DXBC");
        Rec::new(b'B', p)
    }
    fn compute(rs: u8, cs: u8) -> Rec {
        let mut p = vec![rs; 20];
        p.extend(vec![cs; 20]);
        p.extend([0u8; 8]);
        Rec::new(b'C', p)
    }

    #[test]
    fn standalone_and_batches_and_filter() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("recording.db");
        psodb::write_compact(&store, vec![Ok(blob(1)), Ok(blob(2)), Ok(compute(1, 2)), Ok(compute(1, 1)), Ok(compute(9, 9))]).unwrap();
        let out = dir.path().join("work/shaderless.db");
        let n = materialize_standalone_recording(&store, &out, &crate::proton::CancelToken::new()).unwrap();
        assert_eq!(n, 2); // compute(9,9) names blobs nobody has
        let gen = dir.path().join("work/shaderless_gen.db");
        assert_eq!(count_executable_items(&out, &gen), 2);
        let slices = split_databases_into_batches(&out, &gen, &dir.path().join("b"), 1);
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].executable_item_count, 1);
        assert_eq!(psodb::read_all(&slices[0].batch_db_path).unwrap().iter().filter(|r| r.tag == b'B').count(), 2);

        let warmed: HashSet<String> = [compute(1, 2).key()].into_iter().collect();
        let (all, unwarmed, already) = filter_databases_to_unwarmed(&out, &gen, &warmed);
        assert_eq!((all.len(), unwarmed, already), (2, 1, 1));
        assert_eq!(count_executable_items(&out, &gen), 1);
    }
}
