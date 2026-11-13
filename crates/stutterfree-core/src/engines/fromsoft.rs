//! FromSoftware's engine (SCSKiller.Core FromSoft/FromSoftReader.cs, Souls.cs, SoulsKeys.cs): Dark Souls III and Dark
//! Souls: Remastered (DirectX 11). Shaders live in `shader/*.shaderbnd.dcx` (and `*.shaderbdlebnd.dcx` bundles):
//! DCX-compressed BND4 binders holding DXBC containers. In DS3 they sit inside the BHD5/BDT archives, whose headers are
//! RSA-"encrypted" with the game's private key: the public key decrypts them. Read-only and in memory.
//! Elden Ring and Nightreign: DXIL shaders carrying their root signatures (RTS0), planned by plan12; their archives use
//! 64-bit name hashes and Oodle Kraken, their exe carries the archive keys in the clear.
//! shortcut: their ray tracing libraries (gxraytracing, *_[rt] bundles) aren't read.

use crate::dxbc;
use num_bigint::BigUint;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// A title this reader knows: its exe, name and the archives next to it (none: loose files in `shader/`).
pub struct Title {
    pub exe: &'static str,
    pub name: &'static str,
    pub api: &'static str,
    archives: &'static [&'static str],
    /// 64-bit archive name hashes and bucket table (Elden Ring and later)
    hash64: bool,
    /// the section of UXM's published keys holding its archive keys (its exe doesn't carry them in the clear)
    uxm: Option<&'static str>,
}

pub const TITLES: &[Title] = &[
    Title { exe: "DarkSoulsIII.exe", name: "Dark Souls III", api: "D3D11", archives: &["Data1", "Data2", "Data3", "Data4", "Data5", "DLC1", "DLC2"], hash64: false, uxm: Some("DarkSouls3Keys") },
    Title { exe: "DarkSoulsRemastered.exe", name: "Dark Souls: Remastered", api: "D3D11", archives: &[], hash64: false, uxm: None },
    Title { exe: "eldenring.exe", name: "Elden Ring", api: "D3D12", archives: &["Data0", "Data1", "Data2", "Data3", "DLC"], hash64: true, uxm: None },
    Title { exe: "nightreign.exe", name: "Elden Ring Nightreign", api: "D3D12", archives: &["Data0", "Data1", "Data2", "Data3"], hash64: true, uxm: None },
];

/// UXM Selective Unpack's published archive keys, pinned to a commit so an upstream edit can't change what is fetched.
const UXM: &str = "https://raw.githubusercontent.com/Nordgaren/UXM-Selective-Unpack/9501be87e272b6dae55e60a13c3f4753ca6fb3bb/UXM/ArchiveKeys.cs";

/// Shader files by name (the archives only know their hashes), as in the community's archive dictionaries.
fn shader_files() -> Vec<String> {
    let bnd = ["debugfont", "guishader", "gxdecal", "gxffxshader", "gxflvershader", "gxgui", "gxposteffect", "gxrenderershader", "gxshader", "grass", "title"];
    let bdle = ["shaderbdle", "shaderbdle_dlc01", "shaderbdle_dlc02", "speedtree"];
    bnd.iter()
        .map(|n| format!("/shader/{n}.shaderbnd.dcx"))
        .chain(bdle.iter().flat_map(|n| [format!("/shader/{n}.shaderbdlebnd.dcx"), format!("/shader/{n}.shaderbdlebnd.devpatch.dcx")]))
        .collect()
}

pub fn title_of(exe: &Path) -> Option<&'static Title> {
    let name = exe.file_name()?.to_str()?;
    TITLES.iter().find(|t| t.exe.eq_ignore_ascii_case(name))
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        acc = acc << 6 | val(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// (n, e) of a PKCS#1 RSAPublicKey (DER: SEQUENCE { INTEGER n, INTEGER e }).
fn rsa_key(der: &[u8]) -> Option<(BigUint, BigUint)> {
    fn tlv(b: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
        if *b.first()? != tag {
            return None;
        }
        let (len, at) = match *b.get(1)? {
            l if l < 0x80 => (l as usize, 2),
            l => {
                let n = (l & 0x7F) as usize;
                (b.get(2..2 + n)?.iter().fold(0usize, |a, &x| a << 8 | x as usize), 2 + n)
            }
        };
        Some((b.get(at..at + len)?, &b[at + len..]))
    }
    let (seq, _) = tlv(der, 0x30)?;
    let (n, rest) = tlv(seq, 0x02)?;
    let (e, _) = tlv(rest, 0x02)?;
    Some((BigUint::from_bytes_be(n), BigUint::from_bytes_be(e)))
}

/// Every PEM "RSA PUBLIC KEY" body in the bytes that parses, distinct.
fn pem_keys(b: &[u8]) -> Vec<String> {
    const BEGIN: &[u8] = b"-----BEGIN RSA PUBLIC KEY-----";
    const END: &[u8] = b"-----END RSA PUBLIC KEY-----";
    let mut keys = Vec::new();
    let mut i = 0;
    while let Some(k) = b[i..].windows(BEGIN.len()).position(|w| w == BEGIN) {
        let body = &b[i + k + BEGIN.len()..];
        if let Some(e) = body.windows(END.len()).take(4096).position(|w| w == END) {
            let key: String = String::from_utf8_lossy(&body[..e]).chars().filter(|c| !c.is_whitespace()).collect();
            if !keys.contains(&key) && base64(&key).and_then(|d| rsa_key(&d)).is_some() {
                keys.push(key);
            }
        }
        i += k + BEGIN.len();
    }
    keys
}

/// The key's block size: its modulus' bytes.
fn block_size(n: &BigUint) -> usize {
    (n.bits() as usize).div_ceil(8)
}

/// Raw RSA with the public key: c^e mod n per modulus-sized block, one byte less out (left-padded).
fn decrypt_bhd(file: &[u8], key: &str) -> Option<Vec<u8>> {
    let (n, e) = rsa_key(&base64(key)?)?;
    let in_size = block_size(&n);
    let out_size = in_size - 1;
    let mut out = vec![0u8; file.len() / in_size * out_size];
    for (k, block) in file.chunks_exact(in_size).enumerate() {
        let m = BigUint::from_bytes_be(block).modpow(&e, &n).to_bytes_be();
        if m.len() > out_size {
            return None; // not this archive's key
        }
        out[k * out_size + out_size - m.len()..(k + 1) * out_size].copy_from_slice(&m);
    }
    Some(out)
}

/// The key decrypts the header's first block to the BHD5 magic.
fn opens(bhd: &Path, key: &str) -> bool {
    let Some((n, _)) = base64(key).and_then(|d| rsa_key(&d)) else { return false };
    let mut block = vec![0u8; block_size(&n)];
    File::open(bhd).and_then(|f| f.read_exact_at(&mut block, 0)).is_ok() && decrypt_bhd(&block, key).map(|d| d.starts_with(b"BHD5")).unwrap_or(false)
}

struct Entry {
    hash: u64,
    offset: u64,
    size: u64,
    aes: Option<([u8; 16], Vec<(u64, u64)>)>,
}

fn i32le(b: &[u8], o: usize) -> Option<i64> {
    Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as i64)
}

fn i64le(b: &[u8], o: usize) -> Option<i64> {
    Some(i64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Every file header of a decrypted BHD5 (DS3 and later: salt, SHA and AES records); `hash64`: Elden Ring and later
/// (64-bit name hashes; a 64-bit bucket table when its count's high half is zero).
fn parse_bhd(b: &[u8], hash64: bool) -> Option<Vec<Entry>> {
    if !b.starts_with(b"BHD5") || b[4] != 0xFF {
        return None;
    }
    let wide = b.len() > 0x28 && i32le(b, 0x14)? == 0 && i32le(b, 0x1C)? == 0;
    let (count, mut at) = if wide { (i64le(b, 0x10)?, i64le(b, 0x18)? as usize) } else { (i32le(b, 0x10)?, i32le(b, 0x14)? as usize) };
    let mut out = Vec::new();
    for _ in 0..count {
        let (n, mut h) = (i32le(b, at)?, if wide { i64le(b, at + 8)? } else { i32le(b, at + 4)? } as usize);
        for _ in 0..n {
            let (hash, mut size, off, aes_at) = if hash64 {
                (u64::from_le_bytes(b.get(h..h + 8)?.try_into().ok()?), i32le(b, h + 12)?, i64le(b, h + 16)?, i64le(b, h + 32)?)
            } else {
                (u32::from_le_bytes(b.get(h..h + 4)?.try_into().ok()?) as u64, i64le(b, h + 32)?, i64le(b, h + 8)?, i64le(b, h + 24)?)
            };
            if size <= 0 {
                size = i32le(b, h + if hash64 { 8 } else { 4 })?; // unpadded size unknown: the padded one
            }
            let aes = if aes_at > 0 {
                let a = aes_at as usize;
                let key: [u8; 16] = b.get(a..a + 16)?.try_into().ok()?;
                // a range of -1s marks a part left in the clear
                let ranges = (0..i32le(b, a + 16)? as usize)
                    .map(|r| Some((i64le(b, a + 20 + 16 * r)?, i64le(b, a + 28 + 16 * r)?)))
                    .collect::<Option<Vec<_>>>()?
                    .into_iter()
                    .filter(|&(s, e)| s >= 0 && e > s)
                    .map(|(s, e)| (s as u64, e as u64))
                    .collect();
                Some((key, ranges))
            } else {
                None
            };
            out.push(Entry { hash, offset: off as u64, size: size as u64, aes });
            h += 40;
        }
        at += if wide { 16 } else { 8 };
    }
    Some(out)
}

/// The archive name hash: lower case, '/' separators, a leading '/'; h = h * 37 + c (32-bit) or h * 133 + c (64-bit).
fn name_hash(path: &str, hash64: bool) -> u64 {
    let s = path.trim().replace('\\', "/").to_lowercase();
    let s = if s.starts_with('/') { s } else { format!("/{s}") };
    if hash64 {
        return s.bytes().fold(0u64, |h, c| h.wrapping_mul(133).wrapping_add(c as u64));
    }
    s.bytes().fold(0u32, |h, c| h.wrapping_mul(37).wrapping_add(c as u32)) as u64
}

/// An entry's bytes from its .bdt, AES-128-ECB ranges decrypted.
fn read_entry(bdt: &File, e: &Entry) -> Option<Vec<u8>> {
    use aes::cipher::{BlockDecrypt, KeyInit};
    let padded = e.aes.as_ref().and_then(|(_, r)| r.iter().map(|r| r.1).max()).unwrap_or(0).max(e.size);
    if padded > 1 << 31 {
        return None;
    }
    let mut b = vec![0u8; padded as usize];
    bdt.read_exact_at(&mut b, e.offset).ok()?;
    if let Some((key, ranges)) = &e.aes {
        let cipher = aes::Aes128::new(key.into());
        for &(s, end) in ranges {
            for blk in b.get_mut(s as usize..end as usize)?.chunks_exact_mut(16) {
                cipher.decrypt_block(blk.into());
            }
        }
    }
    b.truncate(e.size as usize);
    Some(b)
}

/// A DCX file's content (DFLT = zlib, KRAK = Oodle Kraken). shortcut: ZSTD (newer titles) isn't read.
fn dcx(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 0x4C || !b.starts_with(b"DCX\0") || &b[0x18..0x1C] != b"DCS\0" || &b[0x44..0x48] != b"DCA\0" {
        return None;
    }
    let be = |o: usize| u32::from_be_bytes(b[o..o + 4].try_into().unwrap()) as usize;
    let (size, packed) = (be(0x1C), be(0x20));
    let data = &b[0x4C..(0x4C + packed).min(b.len())];
    match &b[0x28..0x2C] {
        b"DFLT" if size <= 1032 * data.len() => super::unreal::compress::decompress("zlib", data, size),
        b"KRAK" => super::unreal::compress::decompress("oodle", data, size),
        _ => None,
    }
}

/// The files of a little-endian BND4 binder; a per-file DCX-compressed one decompressed.
fn bnd4(b: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    if b.len() < 0x40 || !b.starts_with(b"BND4") || b[9] != 0 {
        return None;
    }
    let reverse = |x: u8| x.reverse_bits();
    let bit_big_endian = b[10] == 0;
    let unicode = b[0x30] != 0;
    let raw = b[0x31];
    let format = if bit_big_endian || (raw & 1 != 0 && raw & 0x80 == 0) { raw } else { reverse(raw) };
    let (ids, names, long_offsets, compression) = (format & 0x02 != 0, format & 0x0C != 0, format & 0x10 != 0, format & 0x20 != 0);
    let (count, header) = (i32le(b, 0x0C)? as usize, i64le(b, 0x20)? as usize);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let h = 0x40 + i * header;
        let flags = if bit_big_endian { *b.get(h)? } else { reverse(*b.get(h)?) };
        let size = i64le(b, h + 8)? as usize;
        let mut p = h + 16 + if compression { 8 } else { 0 };
        let offset = if long_offsets { i64le(b, p)? as usize } else { u32::from_le_bytes(b.get(p..p + 4)?.try_into().ok()?) as usize };
        p += if long_offsets { 8 } else { 4 } + if ids { 4 } else { 0 };
        let name = if names {
            let at = u32::from_le_bytes(b.get(p..p + 4)?.try_into().ok()?) as usize;
            if unicode {
                let units: Vec<u16> = b.get(at..)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
                String::from_utf16_lossy(&units)
            } else {
                String::from_utf8_lossy(b.get(at..)?.split(|&x| x == 0).next()?).into_owned()
            }
        } else {
            format!("#{i}")
        };
        let data = b.get(offset..offset + size)?;
        out.push((name, if flags & 1 != 0 { dcx(data)? } else { data.to_vec() }));
    }
    Some(out)
}

/// DCX -> its content, BND4 -> each file ("binder|file"), anything else: the containers carved from it, with where.
fn visit(at: &str, b: &[u8], found: &mut dyn FnMut(&str, &[u8]), depth: u32) {
    if depth > 8 {
        return;
    }
    if b.starts_with(b"DCX\0") {
        if let Some(d) = dcx(b) {
            visit(at, &d, found, depth + 1);
        }
    } else if b.starts_with(b"BND4") {
        for (file, d) in bnd4(b).unwrap_or_default() {
            visit(&format!("{at}|{file}"), &d, found, depth + 1);
        }
    } else {
        dxbc::containers(b).into_iter().for_each(|c| found(at, c));
    }
}

/// The pool a container's shaders pair in: the binder holding its file (a material's .shaderbdle, or a whole .shaderbnd).
fn pool(at: &str) -> &str {
    at.rsplit_once('|').map(|(p, _)| p).unwrap_or(at)
}

/// The key of each archive: the ones kept in `key_file`, else PEM keys in the exe, else (DS3) UXM's published ones,
/// each checked to open its archive; the ones found are kept for next time (local only). Err names the archives none opens.
fn archive_keys(title: &Title, exe: &Path, archives: &[(String, PathBuf)], key_file: &Path) -> Result<HashMap<String, String>, String> {
    let mut stored: HashMap<String, String> = std::fs::read_to_string(key_file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(a, k)| (a.to_string(), k.trim().to_string())))
        .collect();
    let mut keys: HashMap<String, String> = archives.iter().filter_map(|(a, bhd)| stored.get(a).filter(|k| opens(bhd, k)).map(|k| (a.clone(), k.clone()))).collect();
    let missing = |keys: &HashMap<String, String>| archives.iter().filter(|(a, _)| !keys.contains_key(a)).cloned().collect::<Vec<_>>();
    let mut added = false;
    let mut try_keys = |found: Vec<String>, keys: &mut HashMap<String, String>| {
        for (a, bhd) in missing(keys) {
            if let Some(k) = found.iter().find(|k| opens(&bhd, k)) {
                keys.insert(a.clone(), k.clone());
                stored.insert(a, k.clone());
                added = true;
            }
        }
    };
    if !missing(&keys).is_empty() {
        try_keys(std::fs::read(exe).map(|b| pem_keys(&b)).unwrap_or_default(), &mut keys);
    }
    if let (false, Some(section)) = (missing(&keys).is_empty(), title.uxm) {
        // UXM's C# source: one dictionary per game, from its name to the initializer's end
        if let Some((0, src)) = crate::util::run_capture("curl", &["-sSfL", "--max-time", "20", "-A", "stutterfree", UXM]) {
            if let Some(at) = src.find(&format!("{section} =")) {
                let end = src[at..].find("};").map(|e| at + e).unwrap_or(src.len());
                try_keys(pem_keys(src[at..end].as_bytes()), &mut keys);
            }
        }
    }
    let lost = missing(&keys);
    if added {
        let mut lines: Vec<String> = stored.iter().map(|(a, k)| format!("{a} {k}")).collect();
        lines.sort();
        let _ = std::fs::create_dir_all(key_file.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(key_file, lines.join("\n") + "\n");
    }
    if lost.is_empty() {
        Ok(keys)
    } else {
        Err(format!(
            "no key opens its archives {}: put one \"<archive> <key>\" line per archive (the body of its \"RSA PUBLIC KEY\" PEM) in {}",
            lost.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>().join(", "),
            key_file.display()
        ))
    }
}

/// What a FromSoftware title's shader files hold: every container (sha1 -> bytes), the shaders of each binder (the
/// ones that draw together), and how many shader files were read.
pub struct Index {
    pub shaders: HashMap<String, Vec<u8>>,
    pub maps: Vec<Vec<String>>,
    pub files: usize,
}

pub fn index(title: &Title, exe: &Path, key_file: &Path, cancel: &crate::proton::CancelToken) -> Result<Index, String> {
    let dir = exe.parent().ok_or("no game folder")?;
    let mut shaders = HashMap::new();
    let mut files = 0;
    let mut pools: HashMap<String, Vec<String>> = HashMap::new();
    let mut add = |name: &str, b: &[u8]| {
        files += 1;
        visit(name, b, &mut |at, c| {
            let h = hex::encode(Sha1::digest(c));
            shaders.entry(h.clone()).or_insert_with(|| c.to_vec());
            let list = pools.entry(pool(at).to_string()).or_default();
            if !list.contains(&h) {
                list.push(h);
            }
        }, 0);
    };
    if title.archives.is_empty() {
        for f in crate::util::files_in(&dir.join("shader")).into_iter().filter(|f| f.extension().map(|e| e.eq_ignore_ascii_case("dcx")).unwrap_or(false)) {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            add(&crate::util::file_name(&f), &std::fs::read(&f).map_err(|e| e.to_string())?);
        }
        return Ok(Index { shaders, maps: pools.into_values().collect(), files });
    }
    let archives: Vec<(String, PathBuf)> = title.archives.iter().map(|a| (a.to_string(), dir.join(format!("{a}.bhd")))).filter(|(_, b)| b.is_file() && b.with_extension("bdt").is_file()).collect();
    let keys = archive_keys(title, exe, &archives, key_file)?;
    let mut by_hash: HashMap<u64, String> = shader_files().into_iter().map(|n| (name_hash(&n, title.hash64), n)).collect();
    for (a, bhd) in &archives {
        if by_hash.is_empty() {
            break;
        }
        let header = std::fs::read(bhd).map_err(|e| e.to_string())?;
        let entries = decrypt_bhd(&header, &keys[a]).and_then(|d| parse_bhd(&d, title.hash64)).ok_or_else(|| format!("{a}.bhd doesn't parse"))?;
        let bdt = File::open(bhd.with_extension("bdt")).map_err(|e| e.to_string())?;
        for e in &entries {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            let Some(name) = by_hash.remove(&e.hash) else {
                continue; // not a shader file, or found already: a name lives in one archive
            };
            add(&name, &read_entry(&bdt, e).ok_or_else(|| format!("a shader file of {a}.bdt doesn't read"))?);
        }
    }
    Ok(Index { shaders, maps: pools.into_values().collect(), files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_hash_and_base64() {
        // h = h * 37 + c over "/a"
        assert_eq!(name_hash("A", false), (b'/' as u64) * 37 + b'a' as u64);
        assert_eq!(name_hash("A", true), (b'/' as u64) * 133 + b'a' as u64);
        assert_eq!(base64("TWFu").unwrap(), b"Man");
        assert_eq!(base64("TWE=").unwrap(), b"Ma");
    }

    #[test]
    fn raw_rsa_round_trip() {
        // a toy key: n = 61 * 53 = 3233 (2 bytes), e = 17, d = 2753; "encrypt" with d like FromSoftware's private key
        let der = [0x30, 0x07, 0x02, 0x02, 0x0C, 0xA1, 0x02, 0x01, 0x11];
        let key: String = {
            let t = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            der.chunks(3)
                .flat_map(|c| {
                    let v = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
                    (0..4).take(c.len() + 1).map(move |i| t[(v >> (18 - 6 * i) & 63) as usize] as char)
                })
                .collect()
        };
        let m = BigUint::from(65u32);
        let c = m.modpow(&BigUint::from(2753u32), &BigUint::from(3233u32)).to_bytes_be();
        let block = [vec![0u8; 2 - c.len()], c].concat();
        assert_eq!(decrypt_bhd(&block, &key).unwrap(), vec![65]);
    }
}
