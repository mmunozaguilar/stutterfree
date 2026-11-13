//! Watches known games' processes (Games/GameExitMonitor.cs). When a game exits and no other game is running, its
//! caches are finalized, the recording is imported, and (with "Compile automatically after a game closes") it compiles.

use crate::models::{CacheState, GameInfo};
use crate::{proton, recorder, settings};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Updates `running` with the games running now; returns those that just exited, unless another game still runs
/// (never compile in the background next to a game).
pub fn detect_exited_games(games: &[GameInfo], running: &mut HashSet<String>, is_running: &dyn Fn(&GameInfo) -> bool) -> Vec<GameInfo> {
    let mut exited = Vec::new();
    let mut any = false;
    for g in games {
        if is_running(g) {
            running.insert(g.id.clone());
            any = true;
        } else if running.remove(&g.id) {
            exited.push(g.clone());
        }
    }
    if any {
        Vec::new()
    } else {
        exited
    }
}

pub fn should_auto_compile_after_exit(g: &GameInfo, newly_imported: usize) -> bool {
    if newly_imported > 0 || g.state != CacheState::Cached {
        return true;
    }
    if recorder::unwarmed_recorded_psos(g) > 0 {
        return true;
    }
    recorder::load_warmed_keys(&g.id, None, None).is_empty() && recorder::recorded_psos(g) > 0
}

pub struct GameExitMonitor {
    stop: Arc<AtomicBool>,
}

impl GameExitMonitor {
    /// Polls every 3 s. `known` gives the current game list; `on_ready_to_compile` / `on_exited` run on the monitor thread.
    pub fn start(
        known: impl Fn() -> Vec<GameInfo> + Send + 'static,
        on_ready_to_compile: impl Fn(GameInfo) + Send + 'static,
        on_exited: impl Fn(GameInfo) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        std::thread::spawn(move || {
            let mut running = HashSet::new();
            let sleep = |d: u64| {
                for _ in 0..(d * 10) {
                    if s.load(Ordering::SeqCst) {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                true
            };
            loop {
                if !sleep(3) {
                    return;
                }
                let games = known();
                if games.is_empty() {
                    continue;
                }
                let exited = detect_exited_games(&games, &mut running, &proton::is_game_running);
                if exited.is_empty() {
                    continue;
                }
                // let Wine / the launcher flush shaderless.db and release cache locks
                if !sleep(3) {
                    return;
                }
                if known().iter().any(proton::is_game_running) {
                    continue;
                }
                let auto = settings::load().auto_compile_on_exit;
                for g in exited {
                    if s.load(Ordering::SeqCst) {
                        return;
                    }
                    if proton::is_game_running(&g) {
                        continue;
                    }
                    proton::finalize_game_driver_caches(&g);
                    let added = recorder::import(&g, None).unwrap_or(0);
                    if auto && should_auto_compile_after_exit(&g, added) {
                        on_ready_to_compile(g);
                    } else {
                        on_exited(g);
                    }
                }
            }
        });
        GameExitMonitor { stop }
    }
}

impl Drop for GameExitMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{GameEngine, GameSource};

    #[test]
    fn exits_only_when_nothing_runs() {
        let g = |id: &str| GameInfo::new(id.into(), id.into(), GameSource::Steam, "/x".into(), "/x/a.exe".into(), None, None, GameEngine::Unknown, 0, false);
        let games = vec![g("a"), g("b")];
        let mut running: HashSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        let only_b = |x: &GameInfo| x.id == "b";
        assert!(detect_exited_games(&games, &mut running, &only_b).is_empty());
        let none = |_: &GameInfo| false;
        let ex = detect_exited_games(&games, &mut running, &none);
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].id, "b");
    }
}
