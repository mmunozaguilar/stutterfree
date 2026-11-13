//! GPU driver backends (Drivers/DriverDetector.cs, NvidiaBackend.cs, MesaBackend.cs): which driver caches shaders,
//! where, how big, and clearing them.

use crate::models::{DriverCacheStatus, DriverInfo, GameSource, GpuVendor};
use crate::nvcache::{sum_tree_bytes, sum_unique_file_bytes};
use crate::paths::{home_dir, xdg_cache_home};
use crate::proton;
use crate::util::{files_recursive, run_capture};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct DriverBackend {
    pub vendor: GpuVendor,
}

/// NVIDIA when nvidia-smi answers, else what `vulkaninfo --summary` names; Mesa (AMD) by default.
pub fn detect_active_backend() -> DriverBackend {
    if let Some((0, out)) = run_capture("nvidia-smi", &["--query-gpu=gpu_name", "--format=csv,noheader"]) {
        if !out.trim().is_empty() {
            return DriverBackend { vendor: GpuVendor::Nvidia };
        }
    }
    if let Some((_, out)) = run_capture("vulkaninfo", &["--summary"]) {
        let l = out.to_lowercase();
        if l.contains("nvidia") {
            return DriverBackend { vendor: GpuVendor::Nvidia };
        }
        if l.contains("amd") || l.contains("radeon") {
            return DriverBackend { vendor: GpuVendor::Amd };
        }
        if l.contains("intel") {
            return DriverBackend { vendor: GpuVendor::Intel };
        }
    }
    DriverBackend { vendor: GpuVendor::Amd }
}

fn steam_cache_totals() -> (i64, i32) {
    sum_tree_bytes(&proton::all_steam_shader_cache_roots(), proton::is_compiled_shader_cache_file)
}

fn clear_dir_contents(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() && !p.is_symlink() {
            let _ = fs::remove_dir_all(&p);
        } else {
            let _ = fs::remove_file(&p);
        }
    }
}

impl DriverBackend {
    pub fn driver_info(&self) -> DriverInfo {
        match self.vendor {
            GpuVendor::Nvidia => self.nvidia_info(),
            _ => self.mesa_info(),
        }
    }

    fn nvidia_info(&self) -> DriverInfo {
        let (mut device, mut version) = ("NVIDIA GeForce GPU".to_string(), "Unknown".to_string());
        if let Some((_, out)) = run_capture("nvidia-smi", &["--query-gpu=gpu_name,driver_version", "--format=csv,noheader"]) {
            let parts: Vec<&str> = out.trim().lines().next().unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            if parts.len() >= 2 {
                device = parts[0].to_string();
                version = parts[1].to_string();
            }
        }
        let home = home_dir();
        let mut global = xdg_cache_home().join("nvidia/GLCache");
        if !global.is_dir() {
            let fallback = home.join(".cache/nvidia/GLCache");
            let legacy = home.join(".nv/GLCache");
            if fallback.is_dir() {
                global = fallback;
            } else if legacy.is_dir() {
                global = legacy;
            }
        }
        let (gsize, gfiles) = sum_tree_bytes(std::slice::from_ref(&global), |_| true);
        let (ssize, sfiles) = steam_cache_totals();
        let total_files = gfiles + sfiles;
        let (status, msg) = if total_files == 0 {
            (DriverCacheStatus::InvalidatedByUpdate, Some("Cache is currently empty or recently cleared after a driver update.".to_string()))
        } else {
            (DriverCacheStatus::Healthy, None)
        };
        DriverInfo {
            vendor: GpuVendor::Nvidia,
            device_name: device,
            driver_name: "NVIDIA Proprietary Driver".into(),
            driver_version: version,
            shader_cache_path: global.to_string_lossy().into_owned(),
            cache_size_bytes: gsize,
            cached_files_count: gfiles,
            steam_shader_cache_size_bytes: ssize,
            steam_shader_cache_files_count: sfiles,
            total_cache_size_bytes: gsize + ssize,
            status,
            status_message: msg,
        }
    }

    fn mesa_cache_dirs() -> Vec<PathBuf> {
        let home = home_dir();
        let xc = xdg_cache_home();
        let mut v = Vec::new();
        if let Ok(env) = std::env::var("MESA_SHADER_CACHE_DIR") {
            if !env.trim().is_empty() {
                let e = PathBuf::from(env.trim());
                v.push(e.join("mesa_shader_cache_db"));
                v.push(e.join("mesa_shader_cache_sf"));
                v.push(e);
            }
        }
        for base in [xc, home.join(".cache")] {
            for n in ["mesa_shader_cache_db", "mesa_shader_cache_sf", "mesa_shader_cache"] {
                v.push(base.join(n));
            }
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for p in v {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    fn mesa_info(&self) -> DriverInfo {
        let amd = self.vendor == GpuVendor::Amd;
        let (generic_dev, mut device, mut version) =
            if amd { ("AMD Radeon GPU", "AMD Radeon GPU".to_string(), "Mesa".to_string()) } else { ("Intel Graphics", "Intel Graphics".to_string(), "Mesa".to_string()) };
        if let Some((_, out)) = run_capture("vulkaninfo", &["--summary"]) {
            for line in out.lines() {
                if let Some((_, v)) = line.split_once('=') {
                    let v = v.trim();
                    if line.to_lowercase().contains("devicename") {
                        if !v.to_lowercase().contains("llvmpipe") {
                            device = v.to_string();
                        }
                    } else if line.to_lowercase().contains("driverinfo") && !v.is_empty() {
                        version = v.to_string();
                    }
                }
            }
        }
        if device == generic_dev {
            if let Some((_, out)) = run_capture("glxinfo", &["-B"]) {
                for line in out.lines() {
                    if let Some((_, v)) = line.split_once(':') {
                        let l = line.to_lowercase();
                        if l.contains("opengl renderer string:") {
                            device = v.trim().to_string();
                        } else if l.contains("opengl version string:") {
                            version = v.trim().to_string();
                        }
                    }
                }
            }
        }
        let dirs = Self::mesa_cache_dirs();
        let cache_path = dirs.iter().find(|d| d.is_dir()).cloned().unwrap_or_else(|| xdg_cache_home().join("mesa_shader_cache_db"));
        let (gsize, gfiles) = sum_tree_bytes(&dirs, |_| true);
        let (ssize, sfiles) = steam_cache_totals();
        let total = gsize + ssize;
        let ok = cache_path.is_dir() || total > 0;
        DriverInfo {
            vendor: self.vendor,
            device_name: device,
            driver_name: if amd { "Mesa RADV (ACO)".into() } else { "Mesa ANV".into() },
            driver_version: version,
            shader_cache_path: cache_path.to_string_lossy().into_owned(),
            cache_size_bytes: gsize,
            cached_files_count: gfiles,
            steam_shader_cache_size_bytes: ssize,
            steam_shader_cache_files_count: sfiles,
            total_cache_size_bytes: total,
            status: if ok { DriverCacheStatus::Healthy } else { DriverCacheStatus::DirectoryMissing },
            status_message: if ok { None } else { Some("Mesa shader cache directory not yet created.".into()) },
        }
    }

    /// Bytes, files and folder of the compiled cache of one game.
    pub fn game_cache_stats(&self, game_id: &str, source: GameSource, prefix: Option<&str>) -> (i64, i32, String) {
        let target = if source == GameSource::Steam {
            proton::steam_shader_cache_path(game_id, None, prefix)
        } else {
            proton::non_steam_shader_cache_path(game_id, prefix)
        };
        if target.is_dir() {
            let files: Vec<PathBuf> = files_recursive(&target).into_iter().filter(|f| proton::is_compiled_shader_cache_file(f)).collect();
            let (b, n) = sum_unique_file_bytes(&files);
            return (b, n, target.to_string_lossy().into_owned());
        }
        (0, 0, String::new())
    }

    /// Empties the global driver cache folder.
    pub fn clear_cache(&self) {
        let info = self.driver_info();
        clear_dir_contents(Path::new(&info.shader_cache_path));
    }

    pub fn clear_game_cache(&self, game_id: &str, source: GameSource, prefix: Option<&str>) {
        if source == GameSource::Steam {
            let target = proton::steam_shader_cache_path(game_id, None, prefix);
            if target.is_dir() {
                if let Ok(text) = fs::read_to_string(target.join("installed-files.txt")) {
                    for l in text.lines().filter(|l| !l.is_empty()) {
                        let _ = fs::remove_file(l);
                    }
                }
                let Ok(rd) = fs::read_dir(&target) else { return };
                for e in rd.flatten() {
                    let p = e.path();
                    let n = e.file_name().to_string_lossy().to_lowercase();
                    if p.is_dir() {
                        // Steam's own Fossilize downloads stay
                        if !(n.starts_with("fozpipelines") || n.starts_with("fozmedia")) {
                            let _ = fs::remove_dir_all(&p);
                        }
                    } else {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        } else {
            let effective = prefix
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| crate::paths::data_dir().join("prefixes").join(game_id));
            proton::remove_installed_caches(&effective, game_id);
            let target = proton::non_steam_shader_cache_path(game_id, prefix);
            let _ = fs::remove_dir_all(target);
        }
        crate::recorder::clear_warmed_keys(game_id, None);
    }

    /// The global driver cache and every Steam per-game shader cache.
    pub fn clear_all_caches(&self) {
        self.clear_cache();
        for root in proton::all_steam_shader_cache_roots() {
            for d in crate::util::subdirs(&root) {
                let _ = fs::remove_dir_all(d);
            }
        }
    }

    pub fn recommended_environment_variables(&self) -> String {
        let s = crate::settings::load();
        match self.vendor {
            GpuVendor::Nvidia => format!(
                "__GL_SHADER_DISK_CACHE=1\n__GL_SHADER_DISK_CACHE_SKIP_CLEANUP=1\n__GL_SHADER_DISK_CACHE_SIZE={}",
                s.max_cache_size_bytes()
            ),
            _ => format!("MESA_SHADER_CACHE_MAX_SIZE={}\nRADV_PERFTEST=aco", s.mesa_max_cache_size()),
        }
    }
}

/// Where fossilize_replay lives, if anywhere (shown by `status`).
pub fn find_fossilize_replay() -> Option<PathBuf> {
    let home = home_dir();
    let cands = [
        PathBuf::from("/usr/bin/fossilize-replay"),
        PathBuf::from("/usr/local/bin/fossilize-replay"),
        PathBuf::from("/usr/bin/fossilize_replay"),
        home.join(".local/share/Steam/ubuntu12_64/fossilize_replay"),
        home.join(".local/share/Steam/steamrt64/fossilize_replay"),
        home.join(".steam/steam/ubuntu12_64/fossilize_replay"),
        home.join(".steam/steam/steamrt64/fossilize_replay"),
        home.join(".var/app/com.valvesoftware.Steam/data/Steam/ubuntu12_64/fossilize_replay"),
    ];
    if let Some(c) = cands.into_iter().find(|c| c.is_file()) {
        return Some(c);
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        for n in ["fossilize-replay", "fossilize_replay"] {
            let t = Path::new(dir).join(n);
            if t.is_file() {
                return Some(t);
            }
        }
    }
    None
}
