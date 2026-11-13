//! Data models (Models/*.cs, Drivers/DriverInfo.cs, Pipelines/CompilationProgress.cs).
//!
//! JSON shapes match the C# app's: camelCase properties and enums as numbers, so the existing HTML UI and the
//! files the C# build wrote keep working.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum GameSource {
    Steam = 0,
    Heroic = 1,
    Lutris = 2,
    Bottles = 3,
    Manual = 4,
}

impl GameSource {
    pub fn name(self) -> &'static str {
        match self {
            GameSource::Steam => "Steam",
            GameSource::Heroic => "Heroic",
            GameSource::Lutris => "Lutris",
            GameSource::Bottles => "Bottles",
            GameSource::Manual => "Manual",
        }
    }
    pub fn parse(s: &str) -> Option<GameSource> {
        match s.to_ascii_lowercase().as_str() {
            "steam" => Some(GameSource::Steam),
            "heroic" => Some(GameSource::Heroic),
            "lutris" => Some(GameSource::Lutris),
            "bottles" => Some(GameSource::Bottles),
            "manual" => Some(GameSource::Manual),
            _ => None,
        }
    }
}

impl std::fmt::Display for GameSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum GameEngine {
    Unknown = 0,
    UnrealEngine4 = 1,
    UnrealEngine5 = 2,
    Unity = 3,
    RedEngine = 4,
    FromSoftware = 5,
    ReEngine = 6,
    Northlight = 7,
    DirectX12 = 8,
    DirectX11 = 9,
    Dagor = 10,
}

impl GameEngine {
    pub fn name(self) -> &'static str {
        match self {
            GameEngine::Unknown => "Unknown",
            GameEngine::UnrealEngine4 => "UnrealEngine4",
            GameEngine::UnrealEngine5 => "UnrealEngine5",
            GameEngine::Unity => "Unity",
            GameEngine::RedEngine => "RedEngine",
            GameEngine::FromSoftware => "FromSoftware",
            GameEngine::ReEngine => "ReEngine",
            GameEngine::Northlight => "Northlight",
            GameEngine::DirectX12 => "DirectX12",
            GameEngine::DirectX11 => "DirectX11",
            GameEngine::Dagor => "Dagor",
        }
    }
}

impl std::fmt::Display for GameEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum CacheState {
    NeedsCompilation = 0,
    Cached = 1,
    Outdated = 2,
    Compiling = 3,
}

impl CacheState {
    pub fn name(self) -> &'static str {
        match self {
            CacheState::NeedsCompilation => "NeedsCompilation",
            CacheState::Cached => "Cached",
            CacheState::Outdated => "Outdated",
            CacheState::Compiling => "Compiling",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameInfo {
    pub id: String,
    pub name: String,
    pub source: GameSource,
    pub install_path: String,
    pub executable_path: String,
    pub prefix_path: Option<String>,
    pub banner_path: Option<String>,
    pub engine: GameEngine,
    pub state: CacheState,
    pub total_shaders_estimated: i32,
    pub compiled_shaders_count: i32,
    pub last_compiled_at: Option<DateTime<Utc>>,
    pub cache_size_bytes: i64,
    pub cached_files_count: i32,
    pub recorder_installed: bool,
    pub recorded_psos: i32,
    /// Lutris: the folder its __GL_SHADER_DISK_CACHE_PATH points to for this game (the driver adds /GLCache).
    pub gl_cache_path: Option<String>,
    pub is_flatpak: bool,
    pub stale_reason: Option<String>,
    pub new_recorded_psos: i32,
    /// e.g. a DX12 compile made without a recording: "Partial: ... ● Record ... Re-Compile".
    pub compile_note: Option<String>,
    /// what the last compile planned from the game's files, e.g. "38,426 pipelines · Unreal 5.5 D3D12"
    pub plan_label: Option<String>,
    /// Computed (as the C# record's RawId / SourceLabel properties are serialized too).
    pub raw_id: String,
    pub source_label: String,
}

impl GameInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        name: String,
        source: GameSource,
        install_path: String,
        executable_path: String,
        prefix_path: Option<String>,
        banner_path: Option<String>,
        engine: GameEngine,
        total_shaders_estimated: i32,
        is_flatpak: bool,
    ) -> Self {
        let mut g = GameInfo {
            id,
            name,
            source,
            install_path,
            executable_path,
            prefix_path,
            banner_path,
            engine,
            state: CacheState::NeedsCompilation,
            total_shaders_estimated,
            compiled_shaders_count: 0,
            last_compiled_at: None,
            cache_size_bytes: 0,
            cached_files_count: 0,
            recorder_installed: false,
            recorded_psos: 0,
            gl_cache_path: None,
            is_flatpak,
            stale_reason: None,
            new_recorded_psos: 0,
            compile_note: None,
            plan_label: None,
            raw_id: String::new(),
            source_label: String::new(),
        };
        g.refresh_computed();
        g
    }

    pub fn refresh_computed(&mut self) {
        self.raw_id = raw_id_of(&self.id).to_string();
        self.source_label = if self.is_flatpak { format!("{} (Flatpak)", self.source) } else { self.source.to_string() };
    }

    pub fn raw_id(&self) -> &str {
        raw_id_of(&self.id)
    }
}

pub fn raw_id_of(id: &str) -> &str {
    if id.len() >= 8 && id[..8].eq_ignore_ascii_case("flatpak_") {
        &id[8..]
    } else {
        id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckedPathInfo {
    pub path: String,
    pub description: String,
    pub exists: bool,
    pub is_flatpak: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedItemInfo {
    pub name_or_id: String,
    pub path: String,
    pub reason: String,
    pub is_flatpak: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceDiagnostic {
    pub source: GameSource,
    pub checked_paths: Vec<CheckedPathInfo>,
    pub discovered_games: Vec<GameInfo>,
    pub skipped_items: Vec<SkippedItemInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryReport {
    pub generated_at_utc: DateTime<Utc>,
    pub active_driver_summary: String,
    pub default_proton_binary: Option<String>,
    pub sources: Vec<SourceDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum GpuVendor {
    Unknown = 0,
    Nvidia = 1,
    Amd = 2,
    Intel = 3,
}

impl std::fmt::Display for GpuVendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GpuVendor::Unknown => "Unknown",
            GpuVendor::Nvidia => "Nvidia",
            GpuVendor::Amd => "Amd",
            GpuVendor::Intel => "Intel",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum DriverCacheStatus {
    Healthy = 0,
    NeedsEnlargement = 1,
    InvalidatedByUpdate = 2,
    DirectoryMissing = 3,
}

impl std::fmt::Display for DriverCacheStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DriverCacheStatus::Healthy => "Healthy",
            DriverCacheStatus::NeedsEnlargement => "NeedsEnlargement",
            DriverCacheStatus::InvalidatedByUpdate => "InvalidatedByUpdate",
            DriverCacheStatus::DirectoryMissing => "DirectoryMissing",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverInfo {
    pub vendor: GpuVendor,
    pub device_name: String,
    pub driver_name: String,
    pub driver_version: String,
    pub shader_cache_path: String,
    pub cache_size_bytes: i64,
    pub cached_files_count: i32,
    pub steam_shader_cache_size_bytes: i64,
    pub steam_shader_cache_files_count: i32,
    pub total_cache_size_bytes: i64,
    pub status: DriverCacheStatus,
    pub status_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompilationProgress {
    pub game_id: String,
    pub game_name: String,
    pub processed_shaders: i32,
    pub total_shaders: i32,
    pub percentage: f64,
    pub current_action: String,
    pub is_complete: bool,
    pub failed: bool,
    pub error_message: Option<String>,
    pub failed_shaders: i32,
    pub cache_verified: bool,
}

impl CompilationProgress {
    pub fn step(game: &GameInfo, processed: i32, total: i32, pct: f64, action: impl Into<String>) -> Self {
        CompilationProgress {
            game_id: game.id.clone(),
            game_name: game.name.clone(),
            processed_shaders: processed,
            total_shaders: total,
            percentage: pct,
            current_action: action.into(),
            is_complete: false,
            failed: false,
            error_message: None,
            failed_shaders: 0,
            cache_verified: false,
        }
    }

    pub fn fail(game: &GameInfo, total: i32, msg: impl Into<String>) -> Self {
        let m = msg.into();
        CompilationProgress {
            game_id: game.id.clone(),
            game_name: game.name.clone(),
            processed_shaders: 0,
            total_shaders: total,
            percentage: 0.0,
            current_action: m.clone(),
            is_complete: true,
            failed: true,
            error_message: Some(m),
            failed_shaders: 0,
            cache_verified: false,
        }
    }
}

/// One line of the warmer's JSON output (`{"event":"progress","done":..,"total":..}`).
#[derive(Debug, Clone, Default)]
pub struct WarmerEvent {
    pub event: String,
    pub threads: Option<i32>,
    pub done: Option<i32>,
    pub total: Option<i32>,
    pub failed: Option<i32>,
    pub message: Option<String>,
}

impl WarmerEvent {
    pub fn new(event: &str) -> Self {
        WarmerEvent { event: event.to_string(), ..Default::default() }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        WarmerEvent { event: "error".into(), message: Some(msg.into()), ..Default::default() }
    }
}

/// SCSKiller.Core's AntiCheat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntiCheat {
    None,
    EasyAntiCheat,
    BattlEye,
    Other,
}

impl std::fmt::Display for AntiCheat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AntiCheat::None => "None",
            AntiCheat::EasyAntiCheat => "EasyAntiCheat",
            AntiCheat::BattlEye => "BattlEye",
            AntiCheat::Other => "Other",
        })
    }
}

/// SCSKiller.Core's Store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    Steam,
    Epic,
    Xbox,
    Other,
    Ea,
    Manual,
}

/// SCSKiller.Core's Game: what the core helpers (anti-cheat detection, engine readers) take.
#[derive(Debug, Clone)]
pub struct CoreGame {
    pub id: String,
    pub name: String,
    pub store: Store,
    pub install_dir: String,
    pub exe_path: String,
}
