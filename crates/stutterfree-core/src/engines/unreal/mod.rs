//! Cooked UE 4.2x / 5.x games (SCSKiller.Core Unreal/UnrealReader.cs + UnrealRhi.cs, without CUE4Parse): every shader of
//! the shader code libraries (ShaderArchive-*.ushaderbytecode) in the game's .pak and IoStore containers, and which
//! graphics API the game runs.

pub mod compress;
pub mod iostore;
pub mod pak;

use super::d3d12;
use crate::dxbc;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The <Project>/Content/Paks folders of an install (the project's, then Engine's own are skipped).
pub fn pak_dirs(install: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for d in crate::util::subdirs(install) {
        let name = crate::util::file_name(&d);
        let paks = d.join("Content").join("Paks");
        if !name.eq_ignore_ascii_case("Engine") && paks.is_dir() {
            out.push((name, paks));
        }
    }
    out
}

pub fn is_unreal(install: &Path) -> bool {
    !pak_dirs(install).is_empty()
}

/// A shader library's file name and bytes (IoStore libraries keep only the header: their code is in group chunks).
struct Library {
    name: String,
    bytes: Vec<u8>,
}

/// Each shader's whole FShaderCode ([resource table][container][optional data]) by shader index, and each shader map's
/// shader indices.
fn library_code(lib: &Library, tocs: &[iostore::Toc]) -> Result<(Vec<Vec<u8>>, Vec<Vec<usize>>), String> {
    let b = &lib.bytes;
    let version = u32::from_le_bytes(b.get(..4).ok_or("empty library")?.try_into().unwrap());
    for width in [20usize, 8] {
        if version == 1 {
            // IoStore: hashes, hashes, group chunk ids, map entries, code entries, group entries, indices; header only
            let Some((arrays, end)) = layout(b, &[width, width, 12, 8, 8, 16, 4]) else { continue };
            if end != b.len() {
                continue;
            }
            let (ids, map_entries, entries, groups, indices) = (arrays[2], arrays[3], arrays[4], arrays[5], arrays[6]);
            let mut out = vec![Vec::new(); entries.1];
            for g in 0..groups.1 {
                let id: [u8; 12] = b[ids.0 + 12 * g..ids.0 + 12 * g + 12].try_into().unwrap();
                let e = groups.0 + 16 * g;
                let (first, n, usize_, csize) = (u32le(b, e) as usize, u32le(b, e + 4) as usize, u32le(b, e + 8) as usize, u32le(b, e + 12) as usize);
                let raw = tocs.iter().find(|t| t.has_chunk(&id)).and_then(|t| t.read_chunk_id(&id)).ok_or("a shader group chunk is missing or encrypted")?;
                let group = compress::decompress_shader(&raw[..csize.min(raw.len())], usize_).ok_or("a shader group doesn't decompress")?;
                // FIoStoreShaderCodeEntry: u64 Frequency:4, ShaderGroupIndex:30, UncompressedOffsetInGroup:30
                let mut members: Vec<(usize, usize)> = Vec::with_capacity(n);
                for k in first..first + n {
                    let s = indices_at(b, indices, k).ok_or("a group's shader index is out of range")?;
                    let packed = u64::from_le_bytes(b.get(entries.0 + 8 * s..entries.0 + 8 * s + 8).ok_or("bad shader index")?.try_into().unwrap());
                    if (packed >> 4) & 0x3FFF_FFFF != g as u64 {
                        return Err("unexpected shader code entry layout".into());
                    }
                    members.push((s, (packed >> 34) as usize));
                }
                members.sort_by_key(|m| m.1);
                for (k, &(s, o)) in members.iter().enumerate() {
                    let end = members.get(k + 1).map(|m| m.1).unwrap_or(group.len());
                    out[s] = group.get(o..end).ok_or("a shader runs past its group")?.to_vec();
                }
            }
            return Ok((out, maps(b, map_entries, 8, indices)?));
        } else if version == 2 {
            // pak era: hashes, hashes, map entries, code entries (u64 offset, u32 size, u32 uncompressed, u8), preloads, indices
            let Some((arrays, header_end)) = layout(b, &[width, width, 16, 17, 16, 4]) else { continue };
            let entries = arrays[3];
            let code_len: usize = (0..entries.1).map(|i| u32le(b, entries.0 + 17 * i + 8) as usize).sum();
            let extra = b.len().saturating_sub(header_end + code_len);
            // UE5 pak-era libraries name their compression format (an FString) between the header and the code
            let code = header_end + if (5..=68).contains(&extra) && u32le(b, header_end) as usize == extra - 4 { extra } else { 0 };
            if code + code_len > b.len() {
                continue;
            }
            let codes = (0..entries.1)
                .map(|i| {
                    let e = entries.0 + 17 * i;
                    let off = u64::from_le_bytes(b[e..e + 8].try_into().unwrap()) as usize;
                    let (size, usize_) = (u32le(b, e + 8) as usize, u32le(b, e + 12) as usize);
                    let src = b.get(code + off..code + off + size).ok_or("a shader runs past the library")?;
                    compress::decompress_shader(src, usize_).ok_or_else(|| "a shader doesn't decompress".to_string())
                })
                .collect::<Result<Vec<_>, String>>()?;
            return Ok((codes, maps(b, arrays[2], 16, arrays[5])?));
        } else {
            return Err(format!("library version {version} not read"));
        }
    }
    Err("not a shader library layout this reads".into())
}

/// Each shader map's shader indices: map entries start with (u32 first index, u32 count) into the index array.
fn maps(b: &[u8], entries: (usize, usize), size: usize, indices: (usize, usize)) -> Result<Vec<Vec<usize>>, String> {
    (0..entries.1)
        .map(|m| {
            let (first, n) = (u32le(b, entries.0 + size * m) as usize, u32le(b, entries.0 + size * m + 4) as usize);
            (first..first + n).map(|k| indices_at(b, indices, k).ok_or_else(|| "a map's shader index is out of range".to_string())).collect()
        })
        .collect()
}

/// The k-th u32 of an index array.
fn indices_at(b: &[u8], indices: (usize, usize), k: usize) -> Option<usize> {
    (k < indices.1).then(|| u32le(b, indices.0 + 4 * k) as usize)
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Each array's (start, count) for arrays of the given element sizes after the u32 version, and where they end.
fn layout(b: &[u8], sizes: &[usize]) -> Option<(Vec<(usize, usize)>, usize)> {
    let mut p = 4;
    let mut out = Vec::new();
    for &s in sizes {
        let n = i32::from_le_bytes(b.get(p..p + 4)?.try_into().ok()?);
        if n < 0 || (n as usize).checked_mul(s)? > b.len() - p - 4 {
            return None;
        }
        out.push((p + 4, n as usize));
        p += 4 + n as usize * s;
    }
    Some((out, p))
}

/// FShaderCode's optional data trailer: [u8 key][i32 size][bytes]..., then the i32 size of the whole trailer.
fn optional_data(code: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let total = if code.len() >= 4 { i32::from_le_bytes(code[code.len() - 4..].try_into().unwrap()) } else { 0 };
    if total < 4 || total as usize > code.len() {
        return out;
    }
    let (mut p, end) = (code.len() - total as usize, code.len() - 4);
    while p + 5 <= end {
        let len = i32::from_le_bytes(code[p + 1..p + 5].try_into().unwrap());
        if len < 0 || p + 5 + len as usize > end {
            break;
        }
        out.push((code[p], &code[p + 5..p + 5 + len as usize]));
        p += 5 + len as usize;
    }
    out
}

/// FShaderCodePackedResourceCounts ('p': usage flags, samplers, SRVs, CBs, UAVs) and, for UE5, the flags its root
/// signatures depend on (ShaderContainer.UeCounts / UeFlags.Of); NV intrinsics count from 5.8 only. Zeros when absent.
/// shortcut: the 10-byte 'p' of some UE4 forks (Respawn's wide SRV count) isn't voted on, the stock order is taken.
pub fn ue_counts(code: &[u8], ue5: bool, nv_extension: bool) -> d3d12::Counts {
    use d3d12::*;
    let d = optional_data(code);
    let get = |k: u8| d.iter().find(|(x, _)| *x == k).map(|(_, v)| *v);
    let Some(pc) = get(b'p').filter(|p| p.len() >= 5) else { return Counts::default() };
    let mut flags = 0;
    if ue5 {
        let usage = pc[0] as u32;
        let x = get(b'x').map(|x| x[0] as u32 | if x.len() > 1 { (x[1] as u32) << 8 } else { 0 }).unwrap_or(0);
        if usage & 2 != 0 || x & 0x20 != 0 {
            flags |= BINDLESS_RESOURCES;
        }
        if usage & 4 != 0 || x & 0x40 != 0 {
            flags |= BINDLESS_SAMPLERS;
        }
        if usage & 8 != 0 {
            flags |= ROOT_CONSTANTS;
        }
        if usage & 0x40 != 0 || x & 0x10 != 0 {
            flags |= DIAGNOSTIC_BUFFER;
        }
        // 'v': i32 count, then {u32 vendor, u16 x3, bool (4 bytes), u8 type} each
        if let Some(v) = get(b'v').filter(|v| v.len() >= 8) {
            let n = i32::from_le_bytes(v[..4].try_into().unwrap()).max(0) as usize;
            let mut o = 4;
            while o + 4 <= v.len() && o < 4 + 15 * n {
                match u32le(v, o) {
                    0x1002 => flags |= AMD_INTRINSICS,
                    0x10DE if nv_extension => flags |= NV_INTRINSICS,
                    _ => {}
                }
                o += 15;
            }
        }
    }
    Counts { flags, sampler: pc[1] as u32, srv: pc[2] as u32, cb: pc[3] as u32, uav: pc[4] as u32 }
}

/// One shader: its container (what the game hands D3D), D3D12 stage (0 = none), D3D11 stage (0 = not SM 4/5.0), counts.
#[derive(Clone)]
pub struct Shader {
    pub container: Vec<u8>,
    pub stage: u32,
    pub stage11: u32,
    pub counts: d3d12::Counts,
}

/// The shaders of one shader map; `pooled`: the Global library's, which pair across their maps.
#[derive(Clone, Default)]
pub struct ShaderMap {
    pub pooled: bool,
    pub shas: Vec<String>,
}

/// Everything read from an Unreal install.
#[derive(Default)]
pub struct UnrealIndex {
    pub project: String,
    pub ue5: bool,
    /// (major, minor) and whether it's a guess (no build string, no container version that tells)
    pub version: (u32, u32),
    pub version_guessed: bool,
    /// platform (PCD3D_SM5 / PCD3D_SM6 / ...) -> sha1 -> shader
    pub shaders: HashMap<String, HashMap<String, Shader>>,
    /// platform -> its shader maps (a material's, or the whole Global library as one, pooled): the shaders that draw together
    pub maps: HashMap<String, Vec<ShaderMap>>,
    pub libraries: Vec<String>,
    pub errors: Vec<String>,
    pub encrypted: bool,
    pub engine_ini: Option<String>,
}

fn platform_of(name: &str) -> String {
    name.trim_end_matches(".ushaderbytecode").rsplit('-').next().unwrap_or("").to_string()
}

/// The engine version (UnrealReader.DetectEngine): the exe's "++UE5+Release-5.x" build string, else the containers'
/// format versions, else 4.27 (guessed). shortcut: the UE5 package-tag and PE-version fallbacks aren't ported.
fn engine_version(exe: &Path, toc_version: u8, pak_version: u32) -> ((u32, u32), bool) {
    if let Some(v) = build_string(exe) {
        return (v, false);
    }
    match toc_version {
        1..=2 => return ((4, 26), false),
        3 => return ((4, 27), false),
        4..=5 => return ((5, 1), false),
        6 => return ((5, 4), false),
        7..=8 => return ((5, 5), false),
        9.. => return ((5, 8), false),
        _ => {}
    }
    match pak_version {
        1..=7 => ((4, 21), false),
        8 => ((4, 24), false),
        9 => ((4, 25), false),
        10 => ((4, 26), false),
        _ => ((4, 27), true),
    }
}

/// The "++UE4+Release-4.26" style build string (UTF-16) in the exe, read in 4 MB pieces.
fn build_string(exe: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut f = std::fs::File::open(exe).ok()?;
    let marker: Vec<u8> = "++UE".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let re = regex::Regex::new(r"^\+\+UE[45]\+[^\x00]{0,40}?([45])\.(\d+)").unwrap();
    let mut buf = vec![0u8; 4 << 20];
    let mut kept = 0;
    loop {
        let n = f.read(&mut buf[kept..]).ok()?;
        if n == 0 {
            return None;
        }
        let len = kept + n;
        let mut i = 0;
        while let Some(k) = buf[i..len].windows(4).position(|w| w == &marker[..]) {
            let at = i + k;
            let units: Vec<u16> = buf[at..len.min(at + 120)].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let s = String::from_utf16_lossy(&units);
            if let Some(m) = re.captures(&s) {
                return Some((m[1].parse().ok()?, m[2].parse().ok()?));
            }
            i = at + 1;
        }
        kept = len.min(120);
        buf.copy_within(len - kept..len, 0);
    }
}

/// Reads the shader libraries (all of them, or the given platform's), the project's DefaultEngine.ini and the version.
pub fn index(install: &Path, exe: &Path, only_platform: Option<&str>) -> UnrealIndex {
    let mut ix = UnrealIndex::default();
    let Some((project, paks)) = pak_dirs(install).into_iter().next() else { return ix };
    ix.project = project.clone();
    let files = crate::util::files_in(&paks);
    let tocs: Vec<iostore::Toc> = files.iter().filter(|f| f.extension().map(|e| e == "utoc").unwrap_or(false)).filter_map(|f| iostore::Toc::open(f)).collect();
    let mut libs: Vec<Library> = Vec::new();
    let want = |name: &str| name.ends_with(".ushaderbytecode") && only_platform.map(|p| platform_of(name).eq_ignore_ascii_case(p)).unwrap_or(true);
    for t in &tocs {
        ix.encrypted |= t.encrypted;
        for (name, entry) in &t.files {
            if want(name) {
                match t.read_entry(*entry) {
                    Some(bytes) => libs.push(Library { name: name.clone(), bytes }),
                    None => ix.errors.push(format!("{name}: unreadable")),
                }
            }
        }
    }
    let mut pak_version = 0;
    for f in files.iter().filter(|f| f.extension().map(|e| e == "pak").unwrap_or(false)) {
        let Some(p) = pak::Pak::open(f) else { continue };
        pak_version = pak_version.max(p.version);
        ix.encrypted |= p.encrypted_index;
        for e in &p.entries {
            let name = e.path.rsplit('/').next().unwrap_or(&e.path).to_string();
            if want(&name) || (ix.engine_ini.is_none() && e.path.to_ascii_lowercase().ends_with(&format!("{}/config/defaultengine.ini", project.to_ascii_lowercase()))) {
                match p.read(e) {
                    Some(bytes) if name.ends_with(".ini") => ix.engine_ini = Some(String::from_utf8_lossy(&bytes).into_owned()),
                    Some(bytes) => libs.push(Library { name, bytes }),
                    None => ix.errors.push(format!("{name}: unreadable{}", if e.encrypted { " (encrypted)" } else { "" })),
                }
            }
        }
    }
    (ix.version, ix.version_guessed) = engine_version(exe, tocs.iter().map(|t| t.version).max().unwrap_or(0), pak_version);
    ix.ue5 = ix.version.0 >= 5;
    let nv_ext = ix.version >= (5, 8);
    for lib in &libs {
        ix.libraries.push(lib.name.clone());
        let plat = platform_of(&lib.name);
        match library_code(lib, &tocs) {
            Ok((codes, lib_maps)) => {
                let set = ix.shaders.entry(plat.clone()).or_default();
                let mut shas: Vec<Option<String>> = Vec::with_capacity(codes.len());
                for code in codes {
                    let Some(c) = dxbc::containers(&code).into_iter().next() else {
                        shas.push(None);
                        continue;
                    };
                    let h = hex::encode(Sha1::digest(c));
                    shas.push(Some(h.clone()));
                    set.entry(h).or_insert_with(|| Shader {
                        container: c.to_vec(),
                        stage: d3d12::stage(c).unwrap_or(0),
                        stage11: dxbc::is_d3d11(c).unwrap_or(0),
                        counts: ue_counts(&code, ix.ue5, nv_ext),
                    });
                }
                let named = |m: &Vec<usize>| m.iter().filter_map(|&i| shas.get(i).cloned().flatten()).collect::<Vec<String>>();
                let out = ix.maps.entry(plat).or_default();
                if lib.name.contains("-Global-") {
                    out.push(ShaderMap { pooled: true, shas: lib_maps.iter().flat_map(named).collect() });
                } else {
                    out.extend(lib_maps.iter().map(|m| ShaderMap { pooled: false, shas: named(m) }));
                }
            }
            Err(e) => ix.errors.push(format!("{}: {e}", lib.name)),
        }
    }
    ix
}

/// The last value of `key` in an ini section (`*` = any section).
fn ini_last(text: &str, section: &str, key: &str) -> Option<String> {
    let mut cur = String::new();
    let mut found = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            cur = line[1..line.len() - 1].to_string();
        } else if let Some((k, v)) = line.split_once('=') {
            let k = k.trim().trim_start_matches(['+', '-', '.', '!']);
            if k.eq_ignore_ascii_case(key) && (section == "*" || cur.eq_ignore_ascii_case(section)) {
                found = Some(v.trim().to_string());
            }
        }
    }
    found
}

fn api_name(v: &str) -> Option<&'static str> {
    let l = v.to_ascii_lowercase();
    if l.contains("12") {
        Some("D3D12")
    } else if l.contains("11") {
        Some("D3D11")
    } else if l.contains("vulkan") {
        Some("Vulkan")
    } else {
        None
    }
}

/// Which API the game runs (UnrealRhi): the user's GameUserSettings.ini, the project's DefaultGraphicsRHI, the last log,
/// else the engine's default (UE5 DX12, UE4 DX11); "D3D11 or D3D12" when the project config can't be read and no log says.
/// shortcut: Steam launch options (-dx11 / -dx12) aren't read yet.
pub fn graphics_api(ix: &UnrealIndex, prefix: Option<&str>) -> String {
    let user_dir = prefix.and_then(|p| {
        crate::util::subdirs(&Path::new(p).join("drive_c/users")).into_iter().map(|u| u.join("AppData/Local").join(&ix.project).join("Saved")).find(|d| d.is_dir())
    });
    let settings = user_dir.as_ref().and_then(|d| {
        ["Windows", "WindowsNoEditor", "WinGDK"].iter().find_map(|w| std::fs::read_to_string(d.join("Config").join(w).join("GameUserSettings.ini")).ok())
    });
    let def = ix.engine_ini.as_deref().and_then(|t| ini_last(t, "/Script/WindowsTargetPlatform.WindowsTargetSettings", "DefaultGraphicsRHI"));
    let by_project = match def.as_deref() {
        Some("DefaultGraphicsRHI_DX11") => "D3D11",
        Some("DefaultGraphicsRHI_DX12") => "D3D12",
        Some("DefaultGraphicsRHI_Vulkan") => "Vulkan",
        _ if ix.ue5 => "D3D12",
        _ => "D3D11",
    };
    let explicit = matches!(def.as_deref(), Some(d) if d != "DefaultGraphicsRHI_Default");
    if let Some(s) = &settings {
        let pref = ini_last(s, "D3DRHIPreference", "PreferredRHI")
            .and_then(|v| api_name(&v))
            .or_else(|| ini_last(s, "*", "PreferredGraphicsAPI").and_then(|v| api_name(&v)))
            .or_else(|| ini_last(s, "D3DRHIPreference", "bUseD3D12InGame").filter(|b| b.eq_ignore_ascii_case("true")).map(|_| "D3D12"));
        // UE4 honours the user's choice only when the project doesn't set its API explicitly
        if let Some(p) = pref.filter(|_| ix.ue5 || !explicit) {
            if p != by_project {
                return format!("{p} (user setting)");
            }
        }
    }
    if ix.engine_ini.is_none() {
        if let Some(log) = user_dir.as_ref().and_then(|d| last_log_api(&d.join("Logs"))) {
            return format!("{log} (last run)");
        }
        return "D3D11 or D3D12".into();
    }
    by_project.into()
}

fn last_log_api(logs: &Path) -> Option<&'static str> {
    let mut files: Vec<PathBuf> = crate::util::files_in(logs).into_iter().filter(|f| f.extension().map(|e| e == "log").unwrap_or(false)).collect();
    files.sort_by_key(|f| std::cmp::Reverse(crate::util::mtime_ticks(f)));
    let text = std::fs::read(files.first()?).ok()?;
    let t = String::from_utf8_lossy(&text[..text.len().min(1 << 20)]).into_owned();
    let (d12, d11, vk) = (t.find("LogD3D12RHI"), t.find("LogD3D11RHI"), t.find("LogVulkanRHI"));
    [(d12, "D3D12"), (d11, "D3D11"), (vk, "Vulkan")].into_iter().filter_map(|(p, a)| p.map(|p| (p, a))).min().map(|(_, a)| a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_pak_era_library() {
        let c = dxbc::test_container(0, 5, 0);
        let comp = lz4_flex::block::compress(&c);
        let mut b = 2u32.to_le_bytes().to_vec();
        for _ in 0..2 {
            b.extend(1u32.to_le_bytes()); // map hashes, shader hashes
            b.extend([0u8; 20]);
        }
        b.extend(1u32.to_le_bytes()); // one map: indices 0..1
        b.extend([0u32, 1, 0, 0].iter().flat_map(|v| v.to_le_bytes()));
        b.extend(1u32.to_le_bytes()); // one code entry
        b.extend(0u64.to_le_bytes());
        b.extend((comp.len() as u32).to_le_bytes());
        b.extend((c.len() as u32).to_le_bytes());
        b.push(1);
        b.extend(0u32.to_le_bytes()); // preloads
        b.extend(1u32.to_le_bytes()); // indices: shader 0
        b.extend(0u32.to_le_bytes());
        b.extend(&comp);
        let (code, maps) = library_code(&Library { name: "ShaderArchive-G-PCD3D_SM5.ushaderbytecode".into(), bytes: b }, &[]).unwrap();
        assert_eq!((code, maps), (vec![c], vec![vec![0]]));
        assert_eq!(platform_of("ShaderArchive-Global-PCD3D_SM6.ushaderbytecode"), "PCD3D_SM6");
    }

    #[test]
    fn ini_values() {
        let ini = "[/Script/WindowsTargetPlatform.WindowsTargetSettings]\nDefaultGraphicsRHI=DefaultGraphicsRHI_DX11\n[D3DRHIPreference]\nbUseD3D12InGame=True\n";
        assert_eq!(ini_last(ini, "/Script/WindowsTargetPlatform.WindowsTargetSettings", "DefaultGraphicsRHI").as_deref(), Some("DefaultGraphicsRHI_DX11"));
        assert_eq!(ini_last(ini, "D3DRHIPreference", "bUseD3D12InGame").as_deref(), Some("True"));
    }
}
