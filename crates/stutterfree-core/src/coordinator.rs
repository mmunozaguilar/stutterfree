//! One game's compile, start to finish (Pipelines/ShaderCompilerCoordinator.cs): plan (from a recording, or the game's
//! files once the readers are ported), skip what is already compiled, run the warmer, verify the cache landed.

use crate::drivers::DriverBackend;
use crate::game_manager::GameManager;
use crate::models::{AntiCheat, CompilationProgress, GameInfo, GpuVendor};
use crate::proton::{self, CancelToken};
use crate::util::{format_bytes, n0, non_empty};
use crate::{engines, paths, recorder, settings};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub fn should_preserve_existing_cache(incremental: bool, rebuild_reason: Option<&str>, existing: i64, max: i64) -> bool {
    if incremental || rebuild_reason.map(|r| !r.is_empty()).unwrap_or(false) || existing <= 0 {
        return false;
    }
    max <= 0 || existing <= max / 4 * 3
}

pub fn partial_compile_note(partial: bool, ac: AntiCheat) -> Option<String> {
    if !partial {
        return None;
    }
    Some(if ac != AntiCheat::None {
        "Partial: compiled from the game's files only (anti-cheat blocks recording), so some stutter may remain.".into()
    } else {
        "Partial: compiled from the game's files only. Turn on ● Record, play once, then Re-Compile to improve it.".into()
    })
}

pub fn format_completion_message(done: i32, failed: i32, bytes: i64, verified: bool, detail: Option<&str>, previously: i32) -> String {
    let compiled = (done - failed).max(0);
    let counts = match (previously > 0, failed > 0) {
        (true, true) => format!("{} new compiled, {} failed ({} total)", n0(compiled as i64), n0(failed as i64), n0((previously + compiled) as i64)),
        (true, false) => format!("{} new compiled ({} total)", n0(compiled as i64), n0((previously + compiled) as i64)),
        (false, true) => format!("{} compiled, {} failed", n0(compiled as i64), n0(failed as i64)),
        (false, false) => format!("{} compiled", n0(compiled as i64)),
    };
    if verified {
        return if bytes > 0 {
            format!("Cache ready: {counts} ({} on disk, cache verified).", format_bytes(bytes))
        } else {
            format!("Cache ready: {counts} (cache verified).")
        };
    }
    let reason = detail.filter(|d| !d.trim().is_empty()).map(|d| format!(": {d}")).unwrap_or_default();
    format!("Compiled ({counts}), but couldn't confirm cache files in game folder{reason}.")
}

pub struct Coordinator<'a> {
    pub manager: &'a GameManager,
    pub backend: DriverBackend,
}

impl<'a> Coordinator<'a> {
    pub fn new(manager: &'a GameManager, backend: DriverBackend) -> Self {
        Coordinator { manager, backend }
    }

    /// Compiles one game, reporting every step through `on_progress`; the last report has `is_complete` set.
    pub fn compile(&self, game: &GameInfo, cancel: &CancelToken, on_progress: &mut dyn FnMut(CompilationProgress)) {
        let mut game = game.clone();
        let mut estimated = if game.total_shaders_estimated > 0 { game.total_shaders_estimated } else { 5000 };
        on_progress(CompilationProgress::step(&game, 0, estimated, 0.0, "Preparing shader extraction and D3D12/D3D11 pipeline environment..."));

        let ac = recorder::detect_anti_cheat(&game);
        if ac != AntiCheat::None {
            on_progress(CompilationProgress::step(&game, 0, estimated, 1.0, format!("{ac} detected: compiling outside the game, nothing is added to the game's folder")));
        }
        let data = paths::data_dir();
        let work = data.join("work").join(&game.id);
        let _ = fs::create_dir_all(&work);

        // the real game / Shipping executable
        let raw_exe = if !game.executable_path.is_empty() {
            game.executable_path.clone()
        } else {
            crate::engine_detector::find_executable(&game.install_path).unwrap_or_default()
        };
        if !raw_exe.is_empty() && Path::new(&game.install_path).is_dir() {
            let r = crate::gamefiles::game_exe(&game.install_path, &raw_exe);
            if Path::new(&r).is_file() {
                game.executable_path = r;
            }
        }

        if proton::is_game_running(&game) {
            on_progress(CompilationProgress::fail(
                &game,
                estimated,
                format!("{} is currently running. Please close the game before compiling so its cache files aren't locked.", game.name),
            ));
            return;
        }

        let driver = self.backend.driver_info();
        recorder::migrate_legacy_files(Some(&work));
        recorder::migrate_legacy_files(Some(&recorder::game_dir(&game.id)));
        if let Some(d) = recorder::exe_dir(&game) {
            recorder::migrate_legacy_files(Some(Path::new(&d)));
        }
        let gen_db = work.join("shaderless_gen.db");
        let rec_db = work.join("shaderless.db");
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).max(2);
        let cur_driver = driver.driver_version.clone();
        let cur_proton = proton::proton_stamp(&game);
        let cur_build = proton::game_build_stamp(&game);
        let runtime_stamp = format!("{cur_driver}|{cur_proton}");

        let (existing_bytes, _, _) = self.backend.game_cache_stats(&game.id, game.source, game.prefix_path.as_deref());
        let (existing_verified, _) = proton::verify_cache_installed(&game);
        let truncated = driver.vendor == GpuVendor::Nvidia && proton::is_nvidia_cache_truncated(&game);
        let rebuild_reason: Option<String> = match game.stale_reason.as_deref() {
            Some(r @ ("driver updated" | "Proton updated" | "cache truncated")) => Some(r.to_string()),
            _ => recorder::warmed_stamp_mismatch_reason(&game.id, Some(&cur_driver), Some(&cur_proton))
                .or_else(|| proton::runtime_cache_stamp_mismatch_reason(&game))
                .or_else(|| truncated.then(|| "cache truncated".to_string())),
        };
        let can_incremental = existing_bytes > 0 && existing_verified && rebuild_reason.is_none() && proton::is_runtime_cache_stamp_valid(&game);
        let existing_warmed = if can_incremental { recorder::load_warmed_keys(&game.id, Some(&runtime_stamp), Some(&work)) } else { HashSet::new() };

        on_progress(CompilationProgress::step(&game, 0, estimated, 2.0, "Scanning game archives across all supported engines and building compilation plan..."));

        let mut extract_error: Option<String> = None;
        let mut partial = false;
        let mut plan_label: Option<String> = None; // what the plan from the game's files holds, for the card
        let resolved = crate::engine_detector::resolve_real_game_executable(&game.install_path, Some(&game.executable_path)).unwrap_or_else(|| game.executable_path.clone());
        if Path::new(&game.install_path).is_dir() && !resolved.is_empty() && Path::new(&resolved).is_file() && crate::util::ends_with_ci(&resolved, ".exe") {
            let _ = fs::remove_file(&rec_db);
            let _ = fs::remove_file(&gen_db);
            // any finished recording session in the game folder first
            let _ = recorder::import(&game, None);
            let mut store = recorder::recording_path(&game.id);
            if !non_empty(&store) && game.id != game.raw_id() {
                let raw_store = recorder::recording_path(game.raw_id());
                if non_empty(&raw_store) {
                    store = raw_store;
                }
            }
            let recording = non_empty(&store).then_some(store.as_path());
            let mut planned = 0usize;
            match engines::plan_from_game_files(&game, &work, recording, cancel) {
                Ok(o) => {
                    partial = o.partial;
                    planned = o.planned_items;
                    if planned > 0 {
                        estimated = planned.min(i32::MAX as usize) as i32;
                    }
                    if planned > 0 && !o.label.is_empty() {
                        plan_label = Some(format!("{} pipelines · {}", n0(planned as i64), o.label));
                    }
                    on_progress(CompilationProgress::step(&game, 0, estimated, 3.0, format!("Planned from the game files: {}", o.summary)));
                }
                Err(e) => extract_error = Some(e),
            }
            // what was recorded replays too (DX12 pipelines next to a DX11 plan, or everything when the files can't be read)
            if non_empty(&store) {
                match recorder::materialize_standalone_recording(&store, &rec_db, cancel) {
                    Ok(n) if n > 0 => {
                        extract_error = None;
                        partial = false; // exactly what was recorded: nothing guessed
                        estimated = (planned + n).min(i32::MAX as usize) as i32;
                    }
                    Ok(_) => {}
                    Err(e) => extract_error = Some(e.to_string()),
                }
            }
        }
        if cancel.is_cancelled() {
            on_progress(CompilationProgress::fail(&game, estimated, "Compile cancelled."));
            return;
        }

        let has_db = non_empty(&gen_db) || non_empty(&rec_db);
        if !has_db {
            let msg = match &extract_error {
                Some(e) if ac != AntiCheat::None => format!("Failed to extract game shaders: {e}"),
                Some(e) => format!("No recording yet, and {e}. Enable ● Record, play the game once (DirectX 12, about 5-10 minutes), close it, then click Compile."),
                None if ac != AntiCheat::None => "Nothing to compile from the game's files for this engine yet (its shaders can't be paired into pipelines without a recording, and anti-cheat blocks recording).".into(),
                None => "Nothing to compile yet: turn on ● Record, play about 5 minutes (DirectX 12), then Compile.".into(),
            };
            recorder::save_compile_note(&game.id, None);
            recorder::save_plan_label(&game.id, None);
            on_progress(CompilationProgress::fail(&game, if extract_error.is_some() { estimated } else { 0 }, msg));
            return;
        }

        let snapshot = recorder::build_warmed_snapshot(&[rec_db.clone(), gen_db.clone()]);
        let (all_keys, unwarmed, already) = recorder::filter_databases_to_unwarmed(&rec_db, &gen_db, &existing_warmed);
        let note = partial_compile_note(partial, ac);

        // nothing new and the cache on disk verified: done
        if can_incremental && already > 0 && unwarmed == 0 {
            let _ = fs::remove_file(&gen_db);
            proton::finalize_game_driver_caches(&game);
            recorder::save_warmed_keys(&game, &all_keys, &runtime_stamp, None, snapshot);
            recorder::save_compile_note(&game.id, note.as_deref());
            recorder::save_plan_label(&game.id, plan_label.as_deref());
            self.manager.save_game_compiled(&game.id, all_keys.len() as i32, Some(&cur_driver), Some(&cur_proton), Some(&cur_build));
            let msg = format!(
                "Cache ready: 0 new pipelines ({} already compiled, {} on disk, cache verified).{}",
                n0(already as i64),
                format_bytes(existing_bytes),
                note.as_ref().map(|n| format!(" {n}")).unwrap_or_default()
            );
            let mut p = CompilationProgress::step(&game, already as i32, already as i32, 100.0, msg);
            p.is_complete = true;
            p.cache_verified = true;
            on_progress(p);
            return;
        }
        if unwarmed > 0 {
            estimated = unwarmed as i32;
        }
        let incremental = already > 0;
        let rebuild_prefix: Option<&str> = if !incremental {
            rebuild_reason.as_deref().map(|r| {
                if r.eq_ignore_ascii_case("driver updated") {
                    "Driver updated"
                } else if r.eq_ignore_ascii_case("cache truncated") {
                    "Legacy >2 GB cache truncated"
                } else {
                    "Proton updated"
                }
            })
        } else {
            None
        };
        let recorded_suffix = if non_empty(&rec_db) { " + recorded pipelines" } else { "" };
        let max_bytes = settings::load().max_cache_size_bytes();
        let preserve = should_preserve_existing_cache(incremental, rebuild_reason.as_deref(), existing_bytes, max_bytes);
        let header = if incremental {
            format!("Warming {} new pipelines (skipping {} already compiled) across {threads} CPU threads...", n0(unwarmed as i64), n0(already as i64))
        } else if let Some(pfx) = rebuild_prefix {
            format!("{pfx} — rebuilding full cache{recorded_suffix} across {threads} CPU threads...")
        } else if preserve {
            format!("Warming GPU driver pipeline cache via Proton across {threads} CPU threads (keeping the existing {} cache)...", format_bytes(existing_bytes))
        } else {
            format!("Warming GPU driver pipeline cache via Proton across {threads} CPU threads...")
        };
        on_progress(CompilationProgress::step(&game, 0, estimated, 5.0, header));

        let (mut processed, mut succeeded, mut failed) = (0i32, 0i32, 0i32);
        let mut total = estimated;
        let mut last_error: Option<String> = None;
        let pipe_label = if incremental { "new pipelines" } else { "pipelines" };
        let success = {
            let g = game.clone();
            let mut on_event = |ev: crate::models::WarmerEvent| {
                if let Some(t) = ev.total.filter(|t| *t > 0) {
                    total = t;
                }
                if let Some(f) = ev.failed {
                    failed = f;
                }
                if let Some(d) = ev.done {
                    if ev.event == "done" {
                        succeeded = d; // "done" reports succeeded, "progress" processed
                        processed = succeeded + failed;
                    } else {
                        processed = d;
                        succeeded = (processed - failed).max(0);
                    }
                }
                if ev.message.is_some() {
                    last_error = ev.message.clone();
                }
                let pct = if total > 0 { (processed as f64 / total as f64 * 100.0).min(99.0) } else { 0.0 };
                let fail_suffix = if failed > 0 { format!(", {} failed", n0(failed as i64)) } else { String::new() };
                let th = ev.threads.map(|t| t as usize).unwrap_or(threads);
                let action = match (ev.event.as_str(), rebuild_prefix) {
                    ("init", Some(p)) => format!("{p} — initializing D3D12/D3D11 device across {th} CPU threads..."),
                    ("init", None) => format!("Initializing D3D12/D3D11 device across {th} CPU threads..."),
                    ("start", Some(p)) => format!("{p} — rebuilding full cache ({} pipelines)...", n0(total as i64)),
                    ("start", None) => format!("Starting compilation of {} {pipe_label}...", n0(total as i64)),
                    ("done", _) => format!("Completed compilation: {} compiled{fail_suffix}.", n0(succeeded as i64)),
                    ("error", _) => format!("Warmer error: {}", ev.message.clone().unwrap_or_default()),
                    (_, Some(p)) => format!("{p} — rebuilding full cache ({}/{}{fail_suffix})...", n0(processed as i64), n0(total as i64)),
                    ("progress", None) => format!("Compiling {pipe_label} into driver cache ({}/{}{fail_suffix})...", n0(processed as i64), n0(total as i64)),
                    (_, None) => format!("Compiling {pipe_label} ({}/{}{fail_suffix})...", n0(processed as i64), n0(total as i64)),
                };
                let mut p = CompilationProgress::step(&g, processed, total, (pct * 10.0).round() / 10.0, action);
                p.failed = ev.event == "error";
                p.error_message = ev.message.clone();
                p.failed_shaders = failed;
                on_progress(p);
            };
            proton::run_warmer(
                &game,
                &work,
                &mut on_event,
                cancel,
                incremental,
                proton::MAX_PIPELINES_PER_WARMER_BATCH,
                crate::nvcache::DEFAULT_MAX_NVIDIA_PART_BYTES,
                preserve,
            )
        };

        let completed_all = total > 0 && processed >= total;
        if !all_keys.is_empty() && (success || completed_all) {
            recorder::save_warmed_keys(&game, &all_keys, &runtime_stamp, Some(&[rec_db.clone(), gen_db.clone()]), snapshot);
        }
        let _ = fs::remove_file(&gen_db);

        if cancel.is_cancelled() {
            on_progress(CompilationProgress::fail(&game, total, "Compile cancelled."));
            return;
        }

        let (bytes, _, _) = self.backend.game_cache_stats(&game.id, game.source, game.prefix_path.as_deref());
        let (on_disk, detail) = proton::verify_cache_installed(&game);
        let verified = on_disk && bytes > 0;
        let total_processed = if processed > 0 { processed } else { total };
        let compiled = if succeeded > 0 { succeeded } else { (total_processed - failed).max(0) };
        let cumulative = if incremental && !all_keys.is_empty() {
            all_keys.len() as i32 - failed
        } else if compiled > 0 {
            compiled
        } else {
            total_processed
        };
        let worked = success || processed > 0;
        if worked {
            self.manager.save_game_compiled(&game.id, cumulative, Some(&cur_driver), Some(&cur_proton), Some(&cur_build));
            recorder::save_compile_note(&game.id, note.as_deref());
            recorder::save_plan_label(&game.id, plan_label.as_deref());
        }
        let mut msg = format_completion_message(total_processed, failed, bytes, verified, Some(&detail), already as i32);
        if let (Some(n), true) = (&note, worked) {
            msg.push(' ');
            msg.push_str(n);
        }
        let mut p = CompilationProgress::step(&game, total, total, 100.0, msg);
        p.is_complete = true;
        p.failed = !worked;
        p.error_message = (!worked).then(|| last_error.unwrap_or_else(|| "Warmer process exited unexpectedly".into()));
        p.failed_shaders = failed;
        p.cache_verified = verified;
        on_progress(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages() {
        assert_eq!(format_completion_message(10, 2, 2048, true, None, 0), "Cache ready: 8 compiled, 2 failed (2 KB on disk, cache verified).");
        assert_eq!(format_completion_message(5, 0, 0, false, Some("x"), 3), "Compiled (5 new compiled (8 total)), but couldn't confirm cache files in game folder: x.");
        assert!(should_preserve_existing_cache(false, None, 10, 100));
        assert!(!should_preserve_existing_cache(false, None, 90, 100));
        assert!(!should_preserve_existing_cache(true, None, 10, 100));
    }
}
