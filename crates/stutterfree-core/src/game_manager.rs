//! Scans every launcher and adds each game's cache state (Games/GameManager.cs). On-disk bytes are the only source of
//! truth: a game with no compiled cache on disk is never "Cached", whatever the state file says.

use crate::drivers::{self, DriverBackend};
use crate::models::{CacheState, DiscoveryReport, GameInfo, GameSource, GpuVendor, SourceDiagnostic};
use crate::sources::{default_sources, GameSourceScanner};
use crate::{proton, recorder, settings};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GameStateRecord {
    pub state: CacheState,
    pub compiled_count: i32,
    pub compiled_at: DateTime<Utc>,
    #[serde(default)]
    pub driver_version: Option<String>,
    #[serde(default)]
    pub proton_stamp: Option<String>,
    #[serde(default)]
    pub game_build_stamp: Option<String>,
}

pub struct GameManager {
    sources: Vec<Box<dyn GameSourceScanner>>,
    state_file: PathBuf,
    state_lock: Mutex<()>,
}

impl Default for GameManager {
    fn default() -> Self {
        Self::new(default_sources())
    }
}

impl GameManager {
    pub fn new(sources: Vec<Box<dyn GameSourceScanner>>) -> Self {
        let dir = crate::paths::data_dir();
        let _ = std::fs::create_dir_all(&dir);
        GameManager { sources, state_file: dir.join("games-cache-state.json"), state_lock: Mutex::new(()) }
    }

    fn diagnose_sources(&self) -> Vec<SourceDiagnostic> {
        // each launcher on its own thread; a panicking one never hides the others
        std::thread::scope(|s| {
            let handles: Vec<_> = self.sources.iter().map(|src| (src.source_type(), s.spawn(|| src.diagnose()))).collect();
            handles
                .into_iter()
                .map(|(t, h)| {
                    h.join().unwrap_or_else(|_| SourceDiagnostic {
                        source: t,
                        checked_paths: vec![],
                        discovered_games: vec![],
                        skipped_items: vec![],
                        error: Some("scan failed".into()),
                    })
                })
                .collect()
        })
    }

    pub fn scan_all_games(&self, backend: Option<&DriverBackend>) -> Vec<GameInfo> {
        let all: Vec<GameInfo> = self.diagnose_sources().into_iter().flat_map(|d| d.discovered_games).collect();
        let b = backend.copied().unwrap_or_else(drivers::detect_active_backend);
        self.enrich(all, &b)
    }

    pub fn diagnose(&self, backend: Option<&DriverBackend>) -> DiscoveryReport {
        let b = backend.copied().unwrap_or_else(drivers::detect_active_backend);
        let info = b.driver_info();
        let sources = self
            .diagnose_sources()
            .into_iter()
            .map(|mut d| {
                d.discovered_games = self.enrich(std::mem::take(&mut d.discovered_games), &b);
                d
            })
            .collect();
        DiscoveryReport {
            generated_at_utc: Utc::now(),
            active_driver_summary: format!("{} • {} (v{})", info.device_name, info.driver_name, info.driver_version),
            default_proton_binary: proton::find_proton_binary(None),
            sources,
        }
    }

    fn load_states(&self) -> BTreeMap<String, GameStateRecord> {
        std::fs::read_to_string(&self.state_file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save_states(&self, s: &BTreeMap<String, GameStateRecord>) {
        if let Ok(j) = serde_json::to_string_pretty(s) {
            let _ = crate::util::write_atomic(&self.state_file, j.as_bytes());
        }
    }

    fn enrich(&self, games: Vec<GameInfo>, backend: &DriverBackend) -> Vec<GameInfo> {
        let _g = self.state_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut states = self.load_states();
        let mut modified = false;
        let s = settings::load();
        let driver = Some(backend.driver_info().driver_version);
        let mut out = Vec::with_capacity(games.len());
        for mut g in games {
            let (bytes, files, _) = backend.game_cache_stats(&g.id, g.source, g.prefix_path.as_deref());
            let mut stale: Option<String> = None;
            let (state, count, at);
            if bytes > 0 && proton::has_shaderless_compiled_artifacts(&g) {
                let cur_proton = proton::proton_stamp(&g);
                let cur_build = proton::game_build_stamp(&g);
                let extra = |g: &GameInfo| {
                    recorder::warmed_stamp_mismatch_reason(&g.id, driver.as_deref(), Some(&cur_proton))
                        .or_else(|| proton::runtime_cache_stamp_mismatch_reason(g))
                        .or_else(|| (backend.vendor == GpuVendor::Nvidia && proton::is_nvidia_cache_truncated(g)).then(|| "cache truncated".to_string()))
                };
                if let Some(rec) = states.get(&g.id).cloned() {
                    count = if rec.compiled_count > 0 { rec.compiled_count } else { g.total_shaders_estimated };
                    at = Some(rec.compiled_at);
                    stale = evaluate_stale_reason(
                        rec.driver_version.as_deref(),
                        rec.proton_stamp.as_deref(),
                        rec.game_build_stamp.as_deref(),
                        driver.as_deref(),
                        Some(&cur_proton),
                        Some(&cur_build),
                        Some(rec.compiled_at),
                    )
                    .or_else(|| extra(&g));
                    state = if stale.is_some() { CacheState::Outdated } else { CacheState::Cached };
                    // caches compiled before stamp tracking adopt the current stamps
                    if stale.is_none() && (rec.driver_version.is_none() || rec.proton_stamp.is_none() || rec.game_build_stamp.is_none()) {
                        let mut r = rec;
                        r.driver_version = r.driver_version.or_else(|| driver.clone());
                        r.proton_stamp = r.proton_stamp.or_else(|| Some(cur_proton.clone()));
                        r.game_build_stamp = r.game_build_stamp.or_else(|| Some(cur_build.clone()));
                        states.insert(g.id.clone(), r);
                        modified = true;
                    }
                } else {
                    stale = extra(&g);
                    state = if stale.is_some() { CacheState::Outdated } else { CacheState::Cached };
                    count = g.total_shaders_estimated;
                    let now = Utc::now();
                    at = Some(now);
                    if stale.is_none() {
                        states.insert(
                            g.id.clone(),
                            GameStateRecord {
                                state: CacheState::Cached,
                                compiled_count: count,
                                compiled_at: now,
                                driver_version: driver.clone(),
                                proton_stamp: Some(cur_proton.clone()),
                                game_build_stamp: Some(cur_build.clone()),
                            },
                        );
                        modified = true;
                    }
                }
            } else {
                state = CacheState::NeedsCompilation;
                count = 0;
                at = None;
                if states.remove(&g.id).is_some() {
                    modified = true;
                }
            }

            let running = proton::is_game_running(&g);
            let mut rec_installed = recorder::is_installed(&g);
            if !running && !rec_installed && s.auto_record_all_games && !s.is_auto_record_disabled_for_game(&g.id) {
                rec_installed = recorder::try_auto_install(&g);
            }
            if !running {
                let _ = recorder::import(&g, None); // a finished session's inbox, never while the game runs
            }
            g.recorded_psos = recorder::recorded_psos(&g);
            g.new_recorded_psos = recorder::unwarmed_recorded_psos(&g);
            g.state = state;
            g.compiled_shaders_count = count;
            g.last_compiled_at = at;
            g.cache_size_bytes = bytes;
            g.cached_files_count = files;
            g.recorder_installed = rec_installed;
            g.stale_reason = stale;
            let has_cache = matches!(state, CacheState::Cached | CacheState::Outdated);
            g.compile_note = if has_cache { recorder::load_compile_note(&g.id) } else { None };
            g.plan_label = if has_cache { recorder::load_plan_label(&g.id) } else { None };
            out.push(g);
        }
        if modified {
            self.save_states(&states);
        }
        out
    }

    pub fn save_game_compiled(&self, game_id: &str, count: i32, driver: Option<&str>, proton_stamp: Option<&str>, build: Option<&str>) {
        let _g = self.state_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut states = self.load_states();
        states.insert(
            game_id.to_string(),
            GameStateRecord {
                state: CacheState::Cached,
                compiled_count: count,
                compiled_at: Utc::now(),
                driver_version: driver.map(str::to_string),
                proton_stamp: proton_stamp.map(str::to_string),
                game_build_stamp: build.map(str::to_string),
            },
        );
        self.save_states(&states);
    }
}

fn differs(saved: Option<&str>, current: Option<&str>, unknown: &str) -> bool {
    match (saved.filter(|s| !s.is_empty()), current.filter(|s| !s.is_empty())) {
        (Some(s), Some(c)) => !s.eq_ignore_ascii_case(unknown) && !c.eq_ignore_ascii_case(unknown) && s != c,
        _ => false,
    }
}

pub fn evaluate_stale_reason(
    saved_driver: Option<&str>,
    saved_proton: Option<&str>,
    saved_build: Option<&str>,
    cur_driver: Option<&str>,
    cur_proton: Option<&str>,
    cur_build: Option<&str>,
    compiled_at: Option<DateTime<Utc>>,
) -> Option<String> {
    if differs(saved_driver, cur_driver, "Unknown") {
        return Some("driver updated".into());
    }
    if differs(saved_proton, cur_proton, "unknown") {
        return Some("Proton updated".into());
    }
    if differs(saved_build, cur_build, "unknown") {
        return Some("game updated".into());
    }
    if let (Some(at), Some(latest)) = (compiled_at, proton::latest_game_content_write_utc(cur_build)) {
        if latest > at + chrono::Duration::minutes(2) {
            return Some("game updated".into());
        }
    }
    None
}

/// The games matching a launcher, a search text (name, id, engine, launcher, "flatpak") and/or needing a compile.
pub fn filter_games(games: &[GameInfo], source: Option<GameSource>, query: Option<&str>, only_needs_compile: bool) -> Vec<GameInfo> {
    let q = query.map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty());
    games
        .iter()
        .filter(|g| {
            if source.map(|s| s != g.source).unwrap_or(false) {
                return false;
            }
            if only_needs_compile && !(g.state != CacheState::Cached || g.new_recorded_psos > 0) {
                return false;
            }
            if let Some(q) = &q {
                let hit = g.name.to_lowercase().contains(q)
                    || g.id.to_lowercase().contains(q)
                    || g.engine.name().to_lowercase().contains(q)
                    || g.source.name().to_lowercase().contains(q)
                    || (g.is_flatpak && "flatpak".contains(q.as_str()));
                if !hit {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_reasons() {
        assert_eq!(evaluate_stale_reason(Some("550"), None, None, Some("560"), None, None, None).as_deref(), Some("driver updated"));
        assert_eq!(evaluate_stale_reason(Some("Unknown"), None, None, Some("560"), None, None, None), None);
        assert_eq!(evaluate_stale_reason(None, Some("a"), None, None, Some("b"), None, None).as_deref(), Some("Proton updated"));
        assert_eq!(evaluate_stale_reason(None, None, Some("1:1|0:0:0"), None, None, Some("2:1|0:0:0"), None).as_deref(), Some("game updated"));
    }

    #[test]
    fn state_record_is_csharp_compatible() {
        let j = r#"{"x":{"State":1,"CompiledCount":5,"CompiledAt":"2026-01-02T03:04:05.1234567Z","DriverVersion":null,"ProtonStamp":"p","GameBuildStamp":null}}"#;
        let m: BTreeMap<String, GameStateRecord> = serde_json::from_str(j).unwrap();
        assert_eq!(m["x"].state, CacheState::Cached);
        assert_eq!(m["x"].proton_stamp.as_deref(), Some("p"));
    }
}
