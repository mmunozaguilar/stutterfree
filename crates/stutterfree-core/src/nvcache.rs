//! NVIDIA GLCache packing and vkd3d-proton cache merging (the cache-file half of ProtonRunner.cs).
//!
//! NVIDIA writes per-process writable buckets (steamapp_shader_cache*.bin/.toc). They are packed into deduplicated,
//! size-capped read-only files (steamapp_merged_shader_cache, shaderless_part1..N), each well below NVIDIA's 2 GiB
//! signed 32-bit offset limit, which the driver reads through __GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME.

use crate::util::{file_len, file_name, with_ext};
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_NVIDIA_PART_BYTES: i64 = 1_850_000_000;

pub fn try_parse_shaderless_part_index(path_or_name: &str) -> i32 {
    if path_or_name.is_empty() {
        return -1;
    }
    let mut name = file_name(Path::new(path_or_name));
    if crate::util::ends_with_ci(&name, ".bin") || crate::util::ends_with_ci(&name, ".toc") {
        name.truncate(name.len() - 4);
    }
    const PREFIX: &str = "shaderless_part";
    if !name.starts_with(PREFIX) || name.len() <= PREFIX.len() {
        return -1;
    }
    let digits = &name[PREFIX.len()..];
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return -1;
    }
    match digits.parse::<i32>() {
        Ok(i) if i > 0 => i,
        _ => -1,
    }
}

pub fn is_nvidia_read_only_cache_file_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if matches!(name, "steam_shader_cache.bin" | "steam_shader_cache.toc" | "steamapp_merged_shader_cache.bin" | "steamapp_merged_shader_cache.toc") {
        return true;
    }
    (name.ends_with(".bin") || name.ends_with(".toc")) && try_parse_shaderless_part_index(name) > 0
}

/// The read-only app names the driver should open: steam_shader_cache, steamapp_merged_shader_cache and every
/// complete shaderless_partN under `gl_cache_root`.
pub fn build_nvidia_read_only_app_names(gl_cache_root: Option<&Path>, existing: Option<&str>) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut add = |n: &str| {
        let n = n.trim();
        if !n.is_empty() && !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    };
    if let Some(e) = existing {
        for tok in e.split(';') {
            if try_parse_shaderless_part_index(tok.trim()) < 0 {
                add(tok);
            }
        }
    }
    add("steam_shader_cache");
    add("steamapp_merged_shader_cache");
    if let Some(root) = gl_cache_root.filter(|r| r.is_dir()) {
        let mut idx = BTreeSet::new();
        for f in crate::util::files_recursive(root) {
            let n = file_name(&f);
            if n.starts_with("shaderless_part") && n.ends_with(".bin") {
                let i = try_parse_shaderless_part_index(&n);
                if i > 0 && file_len(&f) > 64 && with_ext(&f, "toc").is_file() {
                    idx.insert(i);
                }
            }
        }
        for i in idx {
            add(&format!("shaderless_part{i}"));
        }
    }
    names.join(";")
}

pub fn is_nvdc_header(toc: &[u8]) -> bool {
    toc.len() >= 32 && (toc.len() - 32) % 24 == 0 && (&toc[..4] == b"CDVN" || &toc[..4] == b"NVDC")
}

fn try_read_valid_nvdc_header(toc_path: &Path) -> Option<[u8; 32]> {
    let len = fs::metadata(toc_path).ok()?.len();
    if len < 32 || (len - 32) % 24 != 0 {
        return None;
    }
    let mut f = File::open(toc_path).ok()?;
    let mut hdr = [0u8; 32];
    f.read_exact(&mut hdr).ok()?;
    is_nvdc_header_head(&hdr).then_some(hdr)
}

fn is_nvdc_header_head(h: &[u8; 32]) -> bool {
    &h[..4] == b"CDVN" || &h[..4] == b"NVDC"
}

fn toc_keys(toc: &[u8], keys: &mut HashSet<(u64, u64)>) {
    let mut pos = 32;
    while pos + 24 <= toc.len() {
        keys.insert((u64::from_le_bytes(toc[pos..pos + 8].try_into().unwrap()), u64::from_le_bytes(toc[pos + 8..pos + 16].try_into().unwrap())));
        pos += 24;
    }
}

/// (device, inode) and size of a file.
pub fn file_identity(path: &Path) -> Option<((u64, u64), u64)> {
    let m = fs::metadata(path).ok()?;
    (m.ino() != 0).then(|| ((m.dev(), m.ino()), m.len()))
}

pub fn are_same_file_on_disk(a: &Path, b: &Path) -> bool {
    match (file_identity(a), file_identity(b)) {
        (Some((ia, _)), Some((ib, _))) => ia == ib,
        _ => false,
    }
}

/// Sums file sizes, counting hard-linked files that share an inode once.
pub fn sum_unique_file_bytes<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> (i64, i32) {
    let mut total = 0i64;
    let mut count = 0i32;
    let mut seen = HashSet::new();
    for p in paths {
        if let Some((id, size)) = file_identity(p) {
            if seen.insert(id) {
                total += size as i64;
                count += 1;
            }
        }
    }
    (total, count)
}

/// As [`sum_unique_file_bytes`] over every file under `dirs` that `keep` takes, streamed: no path list is kept (a Mesa
/// cache can hold millions of files); only files that may be counted twice (hard links, symlinks) are remembered.
pub fn sum_tree_bytes(dirs: &[PathBuf], keep: impl Fn(&Path) -> bool) -> (i64, i32) {
    let (mut total, mut count) = (0i64, 0i32);
    let mut seen = HashSet::new();
    let roots: Vec<PathBuf> = dirs.iter().filter_map(|d| d.canonicalize().ok()).collect();
    for d in dirs {
        crate::util::for_each_file(d, &mut |p, m, link| {
            // a symlink to a file the walk also meets is that file: counted there
            if link && p.canonicalize().map(|t| roots.iter().any(|r| t.starts_with(r))).unwrap_or(false) {
                return;
            }
            if keep(p) && ((m.nlink() < 2 && !link) || seen.insert((m.dev(), m.ino()))) {
                total += m.len() as i64;
                count += 1;
            }
        });
    }
    (total, count)
}

/// Hard link (no extra disk space), else a copy.
pub fn link_or_copy(source: &Path, dest: &Path) -> io::Result<()> {
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d)?;
    }
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    if fs::hard_link(source, dest).is_err() {
        fs::copy(source, dest)?;
    }
    Ok(())
}

fn rename_over(a: &Path, b: &Path) -> io::Result<()> {
    fs::rename(a, b)
}

/// A unique "<dir>/.repack_<tag>" base name (the caller adds .bin / .toc).
fn tmp_tag(dir: &Path) -> String {
    let t = crate::util::tmp_path(&dir.join(".repack"));
    let s = t.to_string_lossy();
    s.trim_end_matches(".tmp").replace("/.repack.", "/.repack_")
}

/// Packs the writable buckets and any oversized or duplicate legacy read-only files of `leaf_dir` into deduplicated,
/// size-capped read-only slots, every .bin at most `max_part_bytes`.
pub fn pack_nvidia_gl_cache_directory(leaf_dir: &Path, max_part_bytes: i64) {
    let _ = pack_inner(leaf_dir, max_part_bytes.max(256) as u64);
}

fn pack_inner(leaf: &Path, max_part: u64) -> io::Result<()> {
    if !leaf.is_dir() {
        return Ok(());
    }
    let mut writable: Vec<(PathBuf, u64)> = crate::util::files_in(leaf)
        .into_iter()
        .filter(|p| {
            let n = file_name(p);
            n.starts_with("steamapp_shader_cache") && n.ends_with(".bin")
        })
        .map(|p| {
            let l = file_len(&p);
            (p, l)
        })
        .collect();
    writable.sort_by(|a, b| b.1.cmp(&a.1));

    // 1. empty 32-byte stub writable buckets
    writable.retain(|(p, l)| {
        if *l <= 64 {
            let _ = fs::remove_file(p);
            let _ = fs::remove_file(with_ext(p, "toc"));
            false
        } else {
            true
        }
    });

    let merged_bin = leaf.join("steamapp_merged_shader_cache.bin");
    let merged_toc = leaf.join("steamapp_merged_shader_cache.toc");
    let steam_bin = leaf.join("steam_shader_cache.bin");
    let steam_toc = leaf.join("steam_shader_cache.toc");

    // 2. legacy redundant hard link between steam_shader_cache and steamapp_merged_shader_cache
    if merged_bin.is_file() && steam_bin.is_file() && are_same_file_on_disk(&merged_bin, &steam_bin) {
        let _ = fs::remove_file(&steam_bin);
        let _ = fs::remove_file(&steam_toc);
    }

    // 3. non-NVDC opaque writable buckets vs real NVDC buckets
    let mut nvdc: Vec<(PathBuf, u64)> = Vec::new();
    let mut non_nvdc: Vec<(PathBuf, u64)> = Vec::new();
    for (wb, l) in writable {
        let wtoc = with_ext(&wb, "toc");
        if try_read_valid_nvdc_header(&wtoc).is_some() {
            nvdc.push((wb, l));
        } else if wtoc.is_file() {
            non_nvdc.push((wb, l));
        } else {
            let _ = fs::remove_file(&wb);
        }
    }
    if !non_nvdc.is_empty() {
        let existing_merged = file_len(&merged_bin);
        let (primary, plen) = &non_nvdc[0];
        if *plen >= existing_merged {
            let _ = rename_over(primary, &merged_bin).and_then(|_| rename_over(&with_ext(primary, "toc"), &merged_toc));
        }
        if non_nvdc.len() > 1 && nvdc.is_empty() {
            let (sec, slen) = &non_nvdc[1];
            if *slen >= file_len(&steam_bin) {
                let _ = rename_over(sec, &steam_bin).and_then(|_| rename_over(&with_ext(sec, "toc"), &steam_toc));
            }
        }
        for (rem, _) in &non_nvdc {
            let _ = fs::remove_file(rem);
            let _ = fs::remove_file(with_ext(rem, "toc"));
        }
    }

    // 4. canonical 32-byte NVDC header
    let mut canonical: Option<[u8; 32]> = nvdc.iter().find_map(|(wb, _)| try_read_valid_nvdc_header(&with_ext(wb, "toc")));
    if canonical.is_none() {
        canonical = try_read_valid_nvdc_header(&merged_toc);
    }
    let mut parts_tocs: Vec<PathBuf> = crate::util::files_in(leaf)
        .into_iter()
        .filter(|p| {
            let n = file_name(p);
            n.starts_with("shaderless_part") && n.ends_with(".toc")
        })
        .collect();
    parts_tocs.sort_by_key(|p| try_parse_shaderless_part_index(&p.to_string_lossy()));
    if canonical.is_none() {
        canonical = parts_tocs.iter().find_map(|t| try_read_valid_nvdc_header(t));
    }
    if canonical.is_none() {
        canonical = try_read_valid_nvdc_header(&steam_toc);
    }
    let Some(header) = canonical else { return Ok(()) };

    let merged_is_non_nvdc = merged_bin.is_file() && file_len(&merged_bin) > 64 && try_read_valid_nvdc_header(&merged_toc).is_none();

    // 5. purge parts of an older driver (mismatched header), stage oversized read-only files for a re-split
    let mut sources: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut keys: HashSet<(u64, u64)> = HashSet::new();
    let stage_for_repack = |bin: &Path, toc: &Path, sources: &mut Vec<(PathBuf, PathBuf)>| -> io::Result<()> {
        let tag = tmp_tag(leaf);
        let tb = PathBuf::from(format!("{tag}.bin"));
        let tt = PathBuf::from(format!("{tag}.toc"));
        rename_over(bin, &tb)?;
        rename_over(toc, &tt)?;
        sources.push((tb, tt));
        Ok(())
    };

    if !merged_is_non_nvdc && merged_bin.is_file() && merged_toc.is_file() {
        let mh = try_read_valid_nvdc_header(&merged_toc);
        let ml = file_len(&merged_bin);
        let mtl = file_len(&merged_toc);
        if mh.is_none() || ml <= 64 || mh != Some(header) {
            let _ = fs::remove_file(&merged_bin);
            let _ = fs::remove_file(&merged_toc);
        } else if ml > max_part && mtl >= 32 + 48 {
            stage_for_repack(&merged_bin, &merged_toc, &mut sources)?;
        } else {
            toc_keys(&fs::read(&merged_toc)?, &mut keys);
        }
    }

    let mut parts: Vec<(PathBuf, PathBuf, i32)> = crate::util::files_in(leaf)
        .into_iter()
        .filter(|p| {
            let n = file_name(p);
            n.starts_with("shaderless_part") && n.ends_with(".bin")
        })
        .map(|b| {
            let i = try_parse_shaderless_part_index(&b.to_string_lossy());
            (b.clone(), with_ext(&b, "toc"), i)
        })
        .filter(|p| p.2 > 0)
        .collect();
    parts.sort_by_key(|p| p.2);
    for (b, t, _) in &parts {
        let ph = try_read_valid_nvdc_header(t);
        let pl = file_len(b);
        let ptl = file_len(t);
        if ph.is_none() || pl <= 64 || ph != Some(header) {
            let _ = fs::remove_file(b);
            let _ = fs::remove_file(t);
        } else if pl > max_part && ptl >= 32 + 48 {
            stage_for_repack(b, t, &mut sources)?;
        } else {
            toc_keys(&fs::read(t)?, &mut keys);
        }
    }

    // steam_shader_cache: oversized or >= 50% duplicate -> repack; else keep and register its keys
    let mut steam_ro_primary = false;
    if steam_bin.is_file() && steam_toc.is_file() {
        let rh = try_read_valid_nvdc_header(&steam_toc);
        let rl = file_len(&steam_bin);
        if rh.is_some() && rl > 64 {
            if rh != Some(header) {
                let _ = fs::remove_file(&steam_bin);
                let _ = fs::remove_file(&steam_toc);
            } else {
                let rt = fs::read(&steam_toc)?;
                let total = (rt.len() - 32) / 24;
                let mut overlap = 0;
                if !keys.is_empty() {
                    let mut pos = 32;
                    while pos + 24 <= rt.len() {
                        let k = (u64::from_le_bytes(rt[pos..pos + 8].try_into().unwrap()), u64::from_le_bytes(rt[pos + 8..pos + 16].try_into().unwrap()));
                        if keys.contains(&k) {
                            overlap += 1;
                        }
                        pos += 24;
                    }
                }
                if (rl > max_part && rt.len() >= 32 + 48) || (!keys.is_empty() && total > 0 && overlap * 2 >= total) {
                    stage_for_repack(&steam_bin, &steam_toc, &mut sources)?;
                } else {
                    toc_keys(&rt, &mut keys);
                    if merged_is_non_nvdc {
                        steam_ro_primary = true;
                    }
                }
            }
        }
    }

    for (wb, _) in &nvdc {
        sources.push((wb.clone(), with_ext(wb, "toc")));
    }
    if sources.is_empty() {
        return Ok(());
    }

    let slot_name = |slot: usize| -> String {
        if slot == 0 {
            return if steam_ro_primary {
                "steam_shader_cache".into()
            } else if merged_is_non_nvdc {
                "shaderless_part1".into()
            } else {
                "steamapp_merged_shader_cache".into()
            };
        }
        let n = if steam_ro_primary || !merged_is_non_nvdc { slot } else { slot + 1 };
        format!("shaderless_part{n}")
    };

    // 6. fast move when slot 0 is empty, no parts exist yet, and the first source fits
    let slot0_bin = leaf.join(format!("{}.bin", slot_name(0)));
    let slot0_toc = leaf.join(format!("{}.toc", slot_name(0)));
    if keys.is_empty() && !slot0_bin.exists() && file_len(&sources[0].0) <= max_part {
        let (b, t) = sources.remove(0);
        rename_over(&b, &slot0_bin)?;
        rename_over(&t, &slot0_toc)?;
        toc_keys(&fs::read(&slot0_toc)?, &mut keys);
        if sources.is_empty() {
            return Ok(());
        }
    }

    // 7. stream every remaining unique record across size-capped slots
    struct Slot {
        bin: File,
        toc: File,
        len: u64,
        new: bool,
        bin_path: PathBuf,
        toc_path: PathBuf,
    }
    let close = |s: Option<Slot>| {
        if let Some(s) = s {
            let (new, len, bp, tp) = (s.new, s.len, s.bin_path.clone(), s.toc_path.clone());
            drop(s);
            if new && len <= 32 {
                let _ = fs::remove_file(bp);
                let _ = fs::remove_file(tp);
            }
        }
    };
    let mut slot_idx = 0usize;
    let mut cur: Option<Slot> = None;
    let result = (|| -> io::Result<()> {
        for (src_bin, src_toc) in &sources {
            if !src_bin.is_file() || !src_toc.is_file() {
                continue;
            }
            let st = fs::read(src_toc)?;
            let src_len = file_len(src_bin);
            if is_nvdc_header(&st) && st[..32] == header {
                let mut sb = File::open(src_bin)?;
                let mut pos = 32;
                while pos + 24 <= st.len() {
                    let k0 = u64::from_le_bytes(st[pos..pos + 8].try_into().unwrap());
                    let k1 = u64::from_le_bytes(st[pos + 8..pos + 16].try_into().unwrap());
                    let entry = pos;
                    pos += 24;
                    if keys.contains(&(k0, k1)) {
                        continue;
                    }
                    let off = u32::from_le_bytes(st[entry + 16..entry + 20].try_into().unwrap()) as u64;
                    let plen = u32::from_le_bytes(st[entry + 20..entry + 24].try_into().unwrap());
                    let rlen = 32 + plen as u64;
                    if off < 32 || off + rlen > src_len || rlen > 64 * 1024 * 1024 {
                        continue;
                    }
                    let mut rec = vec![0u8; rlen as usize];
                    sb.seek(SeekFrom::Start(off))?;
                    if sb.read_exact(&mut rec).is_err() {
                        continue;
                    }
                    // record magic 0x9846a19d and payload length at offset 28
                    if rec[..4] != [0x9d, 0xa1, 0x46, 0x98] || u32::from_le_bytes(rec[28..32].try_into().unwrap()) != plen {
                        continue;
                    }
                    // ensure a writable slot
                    let needs_new = match &cur {
                        Some(s) => !(s.len <= 32 || s.len + rlen <= max_part),
                        None => true,
                    };
                    if needs_new {
                        if cur.is_some() {
                            close(cur.take());
                            slot_idx += 1;
                        }
                        loop {
                            let base = slot_name(slot_idx);
                            let bp = leaf.join(format!("{base}.bin"));
                            let tp = leaf.join(format!("{base}.toc"));
                            if bp.is_file() && tp.is_file() && file_len(&bp) >= 32 {
                                let el = file_len(&bp);
                                if el > 32 && el + rlen > max_part {
                                    slot_idx += 1;
                                    continue;
                                }
                                let bin = OpenOptions::new().append(true).open(&bp)?;
                                let toc = OpenOptions::new().append(true).open(&tp)?;
                                cur = Some(Slot { bin, toc, len: el, new: false, bin_path: bp, toc_path: tp });
                                break;
                            }
                            let mut bin = File::create(&bp)?;
                            let mut toc = File::create(&tp)?;
                            bin.write_all(&header)?;
                            toc.write_all(&header)?;
                            cur = Some(Slot { bin, toc, len: 32, new: true, bin_path: bp, toc_path: tp });
                            break;
                        }
                    }
                    let s = cur.as_mut().unwrap();
                    let new_off = s.len as u32;
                    s.bin.write_all(&rec)?;
                    s.len += rlen;
                    let mut te = [0u8; 24];
                    te[..16].copy_from_slice(&st[entry..entry + 16]);
                    te[16..20].copy_from_slice(&new_off.to_le_bytes());
                    te[20..24].copy_from_slice(&plen.to_le_bytes());
                    s.toc.write_all(&te)?;
                    keys.insert((k0, k1));
                }
            }
            let _ = fs::remove_file(src_bin);
            let _ = fs::remove_file(src_toc);
        }
        Ok(())
    })();
    close(cur.take());
    result
}

/// Merges a vkd3d-proton `.cache.write` into its `.cache` (same 48-byte header: append the new stream chunks; another
/// header, i.e. a new runtime/driver: replace).
pub fn merge_or_replace_vkd3d_cache(write_file: &Path, final_file: &Path) -> io::Result<()> {
    if !final_file.is_file() {
        fs::copy(write_file, final_file)?;
        return Ok(());
    }
    let w = fs::read(write_file)?;
    let f = fs::read(final_file)?;
    if w.len() <= 48 {
        return Ok(());
    }
    if f.len() <= 48 || w[..48] != f[..48] {
        fs::write(final_file, &w)?;
        return Ok(());
    }
    if w == f {
        return Ok(());
    }
    if w.len() > f.len() && w[..f.len()] == f[..] {
        let mut out = OpenOptions::new().append(true).open(final_file)?;
        out.write_all(&w[f.len()..])?;
        return Ok(());
    }
    let payload = w.len() - 48;
    if f.len() >= 48 + payload && f[f.len() - payload..] == w[48..] {
        return Ok(());
    }
    let mut out = OpenOptions::new().append(true).open(final_file)?;
    out.write_all(&w[48..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_bytes_count_linked_files_once() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("a/b")).unwrap();
        fs::write(d.path().join("a/one"), [0u8; 100]).unwrap();
        fs::write(d.path().join("a/b/two"), [0u8; 10]).unwrap();
        fs::write(d.path().join("skip.tmp"), [0u8; 1000]).unwrap();
        fs::hard_link(d.path().join("a/one"), d.path().join("a/b/one-link")).unwrap();
        std::os::unix::fs::symlink(d.path().join("a/b/two"), d.path().join("two-sym")).unwrap();
        let (bytes, files) = sum_tree_bytes(&[d.path().to_path_buf()], |p| p.extension().map(|e| e != "tmp").unwrap_or(true));
        assert_eq!((bytes, files), (110, 2));
    }

    fn bucket(dir: &Path, base: &str, header: &[u8; 32], records: &[(u64, u64, Vec<u8>)]) {
        let mut bin = header.to_vec();
        let mut toc = header.to_vec();
        for (k0, k1, payload) in records {
            let off = bin.len() as u32;
            let mut rec = vec![0x9d, 0xa1, 0x46, 0x98];
            rec.extend([0u8; 24]);
            rec.extend((payload.len() as u32).to_le_bytes());
            rec.extend(payload);
            bin.extend(&rec);
            toc.extend(k0.to_le_bytes());
            toc.extend(k1.to_le_bytes());
            toc.extend(off.to_le_bytes());
            toc.extend((payload.len() as u32).to_le_bytes());
        }
        fs::write(dir.join(format!("{base}.bin")), bin).unwrap();
        fs::write(dir.join(format!("{base}.toc")), toc).unwrap();
    }

    #[test]
    fn packs_and_dedups_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = [0u8; 32];
        h[..4].copy_from_slice(b"NVDC");
        bucket(dir.path(), "steamapp_shader_cache0", &h, &[(1, 1, vec![1; 100]), (2, 2, vec![2; 100])]);
        bucket(dir.path(), "steamapp_shader_cache1", &h, &[(2, 2, vec![2; 100]), (3, 3, vec![3; 100])]);
        pack_nvidia_gl_cache_directory(dir.path(), DEFAULT_MAX_NVIDIA_PART_BYTES);
        let toc = fs::read(dir.path().join("steamapp_merged_shader_cache.toc")).unwrap();
        assert_eq!((toc.len() - 32) / 24, 3);
        assert!(!dir.path().join("steamapp_shader_cache0.bin").exists());
        assert!(!dir.path().join("steamapp_shader_cache1.bin").exists());

        // tiny cap: records spill into shaderless_partN
        bucket(dir.path(), "steamapp_shader_cache2", &h, &[(4, 4, vec![4; 100]), (5, 5, vec![5; 100])]);
        pack_nvidia_gl_cache_directory(dir.path(), 300);
        assert!(dir.path().join("shaderless_part1.bin").is_file());
        let names = build_nvidia_read_only_app_names(Some(dir.path()), None);
        assert!(names.starts_with("steam_shader_cache;steamapp_merged_shader_cache;shaderless_part1"));
    }

    #[test]
    fn part_names() {
        assert_eq!(try_parse_shaderless_part_index("shaderless_part3.bin"), 3);
        assert_eq!(try_parse_shaderless_part_index("shaderless_part0.bin"), -1);
        assert!(is_nvidia_read_only_cache_file_name("shaderless_part2.toc"));
        assert!(!is_nvidia_read_only_cache_file_name("steamapp_shader_cache0.bin"));
    }

    #[test]
    fn vkd3d_merge() {
        let dir = tempfile::tempdir().unwrap();
        let (w, f) = (dir.path().join("a.cache.write"), dir.path().join("a.cache"));
        let mut base = vec![9u8; 48];
        base.extend([1, 2, 3]);
        fs::write(&f, &base).unwrap();
        let mut more = base.clone();
        more.extend([4, 5]);
        fs::write(&w, &more).unwrap();
        merge_or_replace_vkd3d_cache(&w, &f).unwrap();
        assert_eq!(fs::read(&f).unwrap(), more);
    }
}
