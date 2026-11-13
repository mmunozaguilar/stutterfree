//! Persistent user preferences in ~/.local/share/shaderless/settings.json (AppSettings.cs + SettingsManager.cs).
//! All automation options default to OFF; the max shader cache size defaults to 10 GB.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_MAX_CACHE_SIZE_GB: i32 = 10;
pub const MIN_MAX_CACHE_SIZE_GB: i32 = 5;
pub const MAX_MAX_CACHE_SIZE_GB: i32 = 500;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub auto_record_all_games: bool,
    pub auto_compile_on_exit: bool,
    pub disabled_auto_record_game_ids: Option<Vec<String>>,
    pub max_cache_size_gb: i32,
    /// Rescan the launchers when the app opens (the last scan shows at once either way).
    pub scan_on_startup: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            auto_record_all_games: false,
            auto_compile_on_exit: false,
            disabled_auto_record_game_ids: None,
            max_cache_size_gb: DEFAULT_MAX_CACHE_SIZE_GB,
            scan_on_startup: true,
        }
    }
}

pub fn clamp_max_cache_size_gb(value: i32) -> i32 {
    if value <= 0 {
        DEFAULT_MAX_CACHE_SIZE_GB
    } else {
        value.clamp(MIN_MAX_CACHE_SIZE_GB, MAX_MAX_CACHE_SIZE_GB)
    }
}

impl AppSettings {
    pub fn effective_max_cache_size_gb(&self) -> i32 {
        clamp_max_cache_size_gb(self.max_cache_size_gb)
    }
    pub fn max_cache_size_bytes(&self) -> i64 {
        self.effective_max_cache_size_gb() as i64 * 1024 * 1024 * 1024
    }
    pub fn mesa_max_cache_size(&self) -> String {
        format!("{}G", self.effective_max_cache_size_gb())
    }
    pub fn is_auto_record_disabled_for_game(&self, game_id: &str) -> bool {
        self.disabled_auto_record_game_ids.as_ref().map(|v| v.iter().any(|x| x == game_id)).unwrap_or(false)
    }
}

/// `$STUTTERFREE_SETTINGS_PATH`, else <data dir>/settings.json.
pub fn default_settings_file_path() -> PathBuf {
    if let Ok(p) = std::env::var("STUTTERFREE_SETTINGS_PATH") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    crate::paths::data_dir().join("settings.json")
}

pub fn settings_path_overridden() -> bool {
    std::env::var("STUTTERFREE_SETTINGS_PATH").map(|p| !p.is_empty()).unwrap_or(false)
}

pub fn load() -> AppSettings {
    load_from(&default_settings_file_path())
}

pub fn load_from(path: &std::path::Path) -> AppSettings {
    let Ok(text) = std::fs::read_to_string(path) else { return AppSettings::default() };
    match parse_lenient(&text) {
        Some(mut s) => {
            s.max_cache_size_gb = clamp_max_cache_size_gb(s.max_cache_size_gb);
            s
        }
        None => AppSettings::default(),
    }
}

/// The C# reader matched property names case-insensitively: accept PascalCase files too.
fn parse_lenient(text: &str) -> Option<AppSettings> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;
    let mut lowered = serde_json::Map::new();
    for (k, val) in obj {
        let key = match k.to_ascii_lowercase().as_str() {
            "autorecordallgames" => "autoRecordAllGames",
            "autocompileonexit" => "autoCompileOnExit",
            "disabledautorecordgameids" => "disabledAutoRecordGameIds",
            "maxcachesizegb" => "maxCacheSizeGb",
            "scanonstartup" => "scanOnStartup",
            _ => continue,
        };
        lowered.insert(key.to_string(), val.clone());
    }
    serde_json::from_value(serde_json::Value::Object(lowered)).ok()
}

pub fn save(settings: &AppSettings) {
    save_to(settings, &default_settings_file_path());
}

pub fn save_to(settings: &AppSettings, path: &std::path::Path) {
    let mut normalized = settings.clone();
    normalized.max_cache_size_gb = clamp_max_cache_size_gb(settings.max_cache_size_gb);
    if let Ok(json) = serde_json::to_string_pretty(&normalized) {
        let _ = crate::util::write_atomic(path, json.as_bytes());
    }
}

pub fn set_game_auto_record_opt_out(game_id: &str, opt_out: bool) -> AppSettings {
    let mut current = load();
    let mut set: std::collections::BTreeSet<String> =
        current.disabled_auto_record_game_ids.clone().unwrap_or_default().into_iter().collect();
    if opt_out {
        set.insert(game_id.to_string());
    } else {
        set.remove(game_id);
    }
    current.disabled_auto_record_game_ids = Some(set.into_iter().collect());
    save(&current);
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps() {
        assert_eq!(clamp_max_cache_size_gb(0), 10);
        assert_eq!(clamp_max_cache_size_gb(1), 5);
        assert_eq!(clamp_max_cache_size_gb(9999), 500);
    }

    #[test]
    fn roundtrip_and_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(&p, r#"{"AutoRecordAllGames":true,"MaxCacheSizeGb":2}"#).unwrap();
        let s = load_from(&p);
        assert!(s.auto_record_all_games);
        assert_eq!(s.max_cache_size_gb, 5);
        assert!(s.scan_on_startup, "files without the key keep scanning on startup");
        save_to(&s, &p);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"autoRecordAllGames\": true"));
    }
}
