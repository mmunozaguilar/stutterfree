//! Game sources: Steam, Heroic and Lutris, native and Flatpak (Games/*LinuxSource.cs).

pub mod heroic;
pub mod lutris;
pub mod steam;

use crate::models::{CheckedPathInfo, GameInfo, GameSource, SkippedItemInfo, SourceDiagnostic};
use std::path::PathBuf;

pub trait GameSourceScanner: Send + Sync {
    fn source_type(&self) -> GameSource;
    fn diagnose(&self) -> SourceDiagnostic;
    fn discover(&self) -> Vec<GameInfo> {
        self.diagnose().discovered_games
    }
}

/// Where a scan looks: the user's home and XDG overrides (tests point them elsewhere).
#[derive(Debug, Clone, Default)]
pub struct ScanRoots {
    pub home: Option<PathBuf>,
    pub xdg_data: Option<String>,
    pub xdg_config: Option<String>,
}

impl ScanRoots {
    pub fn home(&self) -> PathBuf {
        self.home.clone().unwrap_or_else(crate::paths::home_dir)
    }
}

pub(crate) struct Diag {
    pub checked: Vec<CheckedPathInfo>,
    pub found: Vec<GameInfo>,
    pub skipped: Vec<SkippedItemInfo>,
}

impl Diag {
    pub fn new() -> Self {
        Diag { checked: Vec::new(), found: Vec::new(), skipped: Vec::new() }
    }
    pub fn check(&mut self, path: &std::path::Path, desc: impl Into<String>, exists: bool, fp: bool) {
        self.checked.push(CheckedPathInfo { path: path.to_string_lossy().into_owned(), description: desc.into(), exists, is_flatpak: fp });
    }
    pub fn skip(&mut self, name: impl Into<String>, path: impl Into<String>, reason: impl Into<String>, fp: bool) {
        self.skipped.push(SkippedItemInfo { name_or_id: name.into(), path: path.into(), reason: reason.into(), is_flatpak: fp });
    }
    pub fn finish(self, source: GameSource, error: Option<String>) -> SourceDiagnostic {
        SourceDiagnostic { source, checked_paths: self.checked, discovered_games: self.found, skipped_items: self.skipped, error }
    }
}

pub fn default_sources() -> Vec<Box<dyn GameSourceScanner>> {
    vec![
        Box::new(steam::SteamSource::new(ScanRoots::default())),
        Box::new(heroic::HeroicSource::new(ScanRoots::default())),
        Box::new(lutris::LutrisSource::new(ScanRoots::default())),
    ]
}
