//! Shaderless's data folder (~/.local/share/shaderless) and XDG helpers (AppPaths.cs).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const APP_NAME: &str = "StutterFree";

/// The app's version, e.g. "0.2.3-beta".
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version as shown to the user.
pub fn display_version() -> String {
    VERSION.to_string()
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, home_override: Option<&Path>, value_override: Option<&str>, default: &[&str]) -> PathBuf {
    let env = match value_override {
        Some(v) => Some(v.to_string()),
        None => std::env::var(var).ok(),
    };
    if let Some(v) = env {
        if !v.trim().is_empty() {
            return PathBuf::from(v.trim());
        }
    }
    let mut p = home_override.map(Path::to_path_buf).unwrap_or_else(home_dir);
    for d in default {
        p.push(d);
    }
    p
}

pub fn xdg_data_home_with(home: Option<&Path>, over: Option<&str>) -> PathBuf {
    xdg("XDG_DATA_HOME", home, over, &[".local", "share"])
}
pub fn xdg_config_home_with(home: Option<&Path>, over: Option<&str>) -> PathBuf {
    xdg("XDG_CONFIG_HOME", home, over, &[".config"])
}
pub fn xdg_cache_home_with(home: Option<&Path>, over: Option<&str>) -> PathBuf {
    xdg("XDG_CACHE_HOME", home, over, &[".cache"])
}
pub fn xdg_data_home() -> PathBuf {
    xdg_data_home_with(None, None)
}
pub fn xdg_config_home() -> PathBuf {
    xdg_config_home_with(None, None)
}
pub fn xdg_cache_home() -> PathBuf {
    xdg_cache_home_with(None, None)
}

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
/// ~/.local/share/stutterfree. The first use copies Shaderless's data there (~/.local/share/shaderless, or the older
/// scskiller-linux): settings, recordings, compile state. `STUTTERFREE_DATA_DIR` overrides it (tests, portable use).
/// (~/.local/share/scskiller-linux) there. `STUTTERFREE_DATA_DIR` overrides it (tests, portable use).
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            if let Ok(v) = std::env::var("STUTTERFREE_DATA_DIR") {
                if !v.trim().is_empty() {
                    let p = PathBuf::from(v.trim());
                    let _ = std::fs::create_dir_all(&p);
                    return p;
                }
            }
            let share = xdg_data_home();
            let dir = share.join("stutterfree");
            if !dir.is_dir() {
                // first start: bring over Shaderless's settings, recordings and compile state (copied, so Shaderless
                // keeps working); its scratch folders are left, they are rebuilt on demand
                for old in [share.join("shaderless"), share.join("scskiller-linux")] {
                    if old.is_dir() {
                        let _ = copy_dir_except(&old, &dir, &["work", "proton_prefix"]);
                        break;
                    }
                }
            }
            let _ = std::fs::create_dir_all(&dir);
            dir
        })
        .clone()
}

fn copy_dir_except(src: &Path, dst: &Path, skip: &[&str]) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let name = e.file_name();
        if skip.iter().any(|s| name == *s) {
            continue;
        }
        let (from, to) = (e.path(), dst.join(&name));
        let ft = e.file_type()?;
        if ft.is_dir() {
            copy_dir_except(&from, &to, &[])?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// The path with every symlink on the way resolved, without a trailing separator; the input (trimmed) when it can't
/// be resolved. Unlike `fs::canonicalize`, a missing tail is kept as written.
pub fn canonical_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.trim().is_empty() {
        return String::new();
    }
    if let Ok(c) = std::fs::canonicalize(path) {
        return trim_sep(&c.to_string_lossy());
    }
    // resolve the longest existing prefix, keep the rest
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf())
    };
    let mut existing = abs.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(n), Some(p)) => {
                rest.push(n);
                existing = p.to_path_buf();
            }
            _ => return trim_sep(&s),
        }
    }
    let mut out = std::fs::canonicalize(&existing).unwrap_or(existing);
    for n in rest.into_iter().rev() {
        out.push(n);
    }
    trim_sep(&out.to_string_lossy())
}

pub fn trim_sep(s: &str) -> String {
    let t = s.trim_end_matches('/');
    if t.is_empty() && s.starts_with('/') {
        "/".to_string()
    } else {
        t.to_string()
    }
}

/// Folders that may hold the two Windows-side helpers (d3d12.dll recorder, shaderless_warm.exe warmer), in order:
/// `$STUTTERFREE_NATIVE_DIR`, `<exe dir>/native`, `<exe dir>/../share/shaderless/native` (AppImage / install layout),
/// `<exe dir>`, the data folder's `bin`, and `./native` (running from a source checkout).
pub fn native_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(v) = std::env::var("STUTTERFREE_NATIVE_DIR") {
        if !v.trim().is_empty() {
            dirs.push(PathBuf::from(v.trim()));
        }
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("native"));
        dirs.push(exe_dir.join("..").join("share").join("shaderless").join("native"));
        dirs.push(exe_dir.clone());
    }
    dirs.push(data_dir().join("bin"));
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("native"));
    }
    dirs
}

pub fn find_native(name: &str) -> Option<PathBuf> {
    native_dirs().into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}
