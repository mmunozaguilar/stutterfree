//! Small helpers shared by the modules (the .NET BCL bits the C# code leaned on).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// .NET DateTime ticks (100 ns since 0001-01-01) of a time: stamps written by the C# build stay comparable.
pub fn ticks(t: SystemTime) -> i64 {
    const EPOCH_TICKS: i64 = 621_355_968_000_000_000;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => EPOCH_TICKS + (d.as_nanos() / 100) as i64,
        Err(e) => EPOCH_TICKS - (e.duration().as_nanos() / 100) as i64,
    }
}

pub fn ticks_to_utc(t: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    const EPOCH_TICKS: i64 = 621_355_968_000_000_000;
    let since = t.checked_sub(EPOCH_TICKS)?;
    let secs = since.div_euclid(10_000_000);
    let nanos = (since.rem_euclid(10_000_000) * 100) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
}

pub fn mtime_ticks(path: &Path) -> i64 {
    fs::metadata(path).and_then(|m| m.modified()).map(ticks).unwrap_or(0)
}

pub fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| if m.is_file() { m.len() } else { 0 }).unwrap_or(0)
}

/// Exists and is a non-empty regular file.
pub fn non_empty(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_file() && m.len() > 0).unwrap_or(false)
}

/// "1.5 GB" as the C# FormatBytes ({0:0.##}).
pub fn format_bytes(bytes: i64) -> String {
    const SUFFIXES: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut d = bytes as f64;
    let mut i = 0;
    while d >= 1024.0 && i < SUFFIXES.len() - 1 {
        d /= 1024.0;
        i += 1;
    }
    let s = format!("{d:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s} {}", SUFFIXES[i])
}

/// "12,345" ({0:N0}, invariant culture).
pub fn n0(v: i64) -> String {
    let neg = v < 0;
    let digits = v.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique "<path>.<32 hex>.tmp" next to `path` (per writer: the app and the CLI may write at once).
pub fn tmp_path(path: &Path) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tag = format!("{:016x}{:08x}{:08x}", nanos as u64, std::process::id(), n as u32);
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".{tag}.tmp"));
    PathBuf::from(s)
}

/// Writes through a temp file, then renames over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = tmp_path(path);
    let r = fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Every regular file under `dir` (recursive, not following directory symlinks), inaccessible entries skipped.
pub fn files_recursive(dir: &Path) -> Vec<PathBuf> {
    files_recursive_depth(dir, usize::MAX)
}

/// As [`files_recursive`], at most `max_depth` levels below `dir` (0 = only `dir` itself).
pub fn files_recursive_depth(dir: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                if depth < max_depth {
                    stack.push((p, depth + 1));
                }
            } else if ft.is_file() || (ft.is_symlink() && p.is_file()) {
                out.push(p);
            }
        }
    }
    out
}

/// Calls `f` with every file under `dir` (as [`files_recursive`]), its metadata (a symlink's: its target's) and whether
/// it's a symlink, without keeping a list: a driver cache can hold millions of files.
pub fn for_each_file(dir: &Path, f: &mut dyn FnMut(&Path, &fs::Metadata, bool)) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() || ft.is_symlink() {
                if let Ok(m) = if ft.is_symlink() { fs::metadata(&p) } else { e.metadata() } {
                    if m.is_file() {
                        f(&p, &m, ft.is_symlink());
                    }
                }
            }
        }
    }
}

/// The entries of `dir` that are directories (following symlinks, as Directory.GetDirectories does).
pub fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    v.sort();
    v
}

/// The regular files directly in `dir`.
pub fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    v.sort();
    v
}

/// Simple glob for a file name: `*` matches any run, the rest compares case-insensitively when `ci`.
pub fn name_matches(name: &str, pattern: &str, ci: bool) -> bool {
    let (n, p) = if ci { (name.to_ascii_lowercase(), pattern.to_ascii_lowercase()) } else { (name.to_string(), pattern.to_string()) };
    glob(n.as_bytes(), p.as_bytes())
}

fn glob(n: &[u8], p: &[u8]) -> bool {
    let (mut ni, mut pi) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            ni += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Files under `dir` (recursive) whose name matches `pattern` (case-insensitive).
pub fn find_files(dir: &Path, pattern: &str) -> Vec<PathBuf> {
    files_recursive(dir)
        .into_iter()
        .filter(|f| f.file_name().map(|n| name_matches(&n.to_string_lossy(), pattern, true)).unwrap_or(false))
        .collect()
}

/// Any file under `dir` (recursive) whose name matches `pattern`, stopping at the first.
pub fn any_file(dir: &Path, pattern: &str) -> bool {
    let pat = pattern.to_ascii_lowercase();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if name_matches(&e.file_name().to_string_lossy().to_ascii_lowercase(), &pat, false) {
                return true;
            }
        }
    }
    false
}

pub fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn parent_str(p: &str) -> Option<String> {
    Path::new(p).parent().map(|x| x.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
}

pub fn ends_with_ci(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len() && s.is_char_boundary(s.len() - suffix.len()) && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

pub fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

pub fn contains_ci(s: &str, needle: &str) -> bool {
    s.to_lowercase().contains(&needle.to_lowercase())
}

/// Path.ChangeExtension(p, ext) for a file name with an extension.
pub fn with_ext(p: &Path, ext: &str) -> PathBuf {
    p.with_extension(ext)
}

pub fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)?.flatten() {
        let p = e.path();
        let t = dst.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &t)?;
        } else {
            fs::copy(&p, &t)?;
        }
    }
    Ok(())
}

/// Runs a program and returns its stdout (None when it can't start).
pub fn run_capture(program: &str, args: &[&str]) -> Option<(i32, String)> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    Some((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Path.GetRelativePath for `path` under `base` (falls back to the file name).
pub fn relative(base: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(base).map(Path::to_path_buf).unwrap_or_else(|_| PathBuf::from(path.file_name().unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_csharp() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1 GB");
        assert_eq!(n0(1234567), "1,234,567");
        assert_eq!(n0(12), "12");
    }

    #[test]
    fn globbing() {
        assert!(name_matches("re_chunk_000.pak", "re_chunk_*.pak", true));
        assert!(name_matches("Game_BE.exe", "*_be.exe", true));
        assert!(!name_matches("game.exe", "*.pak", true));
    }

    #[test]
    fn ticks_roundtrip() {
        let t = SystemTime::now();
        let tk = ticks(t);
        assert!(ticks_to_utc(tk).is_some());
    }
}
