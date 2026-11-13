//! UE5 IoStore containers: a .utoc table of contents (chunk ids, offsets, compression blocks, an optional directory index)
//! over .ucas data (split in partitions: X.ucas, X_s1.ucas, ...). Layout of FIoStoreTocHeader (144 bytes) and what
//! follows as in UE 5.x (TOC versions 2-8).

use super::compress;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 16] = b"-==--==--==--==-";
const FLAG_ENCRYPTED: u8 = 2;
const FLAG_SIGNED: u8 = 4;
const FLAG_INDEXED: u8 = 8;

struct BlockEntry {
    offset: u64,
    csize: u32,
    usize_: u32,
    method: u8,
}

pub struct Toc {
    pub path: PathBuf,
    pub version: u8,
    pub encrypted: bool,
    chunk_ids: Vec<[u8; 12]>,
    by_id: HashMap<[u8; 12], usize>,
    offlen: Vec<(u64, u64)>,
    blocks: Vec<BlockEntry>,
    methods: Vec<String>,
    block_size: u64,
    partition_size: u64,
    /// file name -> toc entry, from the directory index
    pub files: Vec<(String, usize)>,
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn be40(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |a, &x| (a << 8) | x as u64)
}

fn le_n(b: &[u8]) -> u64 {
    b.iter().rev().fold(0u64, |a, &x| (a << 8) | x as u64)
}

/// An FString: i32 length, then ASCII with its null (or UTF-16 for a negative length).
pub fn fstring(b: &[u8], pos: &mut usize) -> Option<String> {
    let n = i32::from_le_bytes(b.get(*pos..*pos + 4)?.try_into().ok()?);
    *pos += 4;
    if n == 0 {
        return Some(String::new());
    }
    if n > 0 {
        let s = b.get(*pos..*pos + n as usize)?;
        *pos += n as usize;
        return Some(String::from_utf8_lossy(&s[..s.len() - 1]).into_owned());
    }
    let len = (-n) as usize * 2;
    let s = b.get(*pos..*pos + len)?;
    *pos += len;
    let units: Vec<u16> = s.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(String::from_utf16_lossy(&units[..units.len().saturating_sub(1)]))
}

impl Toc {
    pub fn open(path: &Path) -> Option<Toc> {
        let b = std::fs::read(path).ok()?;
        if b.len() < 144 || &b[..16] != MAGIC {
            return None;
        }
        let version = b[16];
        let (entries, nblocks, block_entry_size) = (u32le(&b, 24) as usize, u32le(&b, 28) as usize, u32le(&b, 32) as usize);
        let (nmethods, method_len, block_size) = (u32le(&b, 36) as usize, u32le(&b, 40) as usize, u32le(&b, 44) as u64);
        let dir_size = u32le(&b, 48) as usize;
        let flags = b[80];
        let seeds = if version >= 4 { u32le(&b, 84) as usize } else { 0 };
        let partition_size = if version >= 3 { u64::from_le_bytes(b[88..96].try_into().unwrap()) } else { u64::MAX };
        let without_hash = if version >= 5 { u32le(&b, 96) as usize } else { 0 };
        if block_entry_size != 12 || block_size == 0 {
            return None;
        }
        let mut p = u32le(&b, 20) as usize; // header size
        let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
            let s = b.get(*p..*p + n)?;
            *p += n;
            Some(s)
        };
        let ids = take(&mut p, entries * 12)?;
        let chunk_ids: Vec<[u8; 12]> = ids.chunks(12).map(|c| c.try_into().unwrap()).collect();
        let ol = take(&mut p, entries * 10)?;
        let offlen = ol.chunks(10).map(|c| (be40(&c[..5]), be40(&c[5..]))).collect();
        take(&mut p, seeds * 4)?;
        take(&mut p, without_hash * 4)?;
        let bl = take(&mut p, nblocks * 12)?;
        let blocks = bl
            .chunks(12)
            .map(|c| BlockEntry { offset: le_n(&c[..5]), csize: le_n(&c[5..8]) as u32, usize_: le_n(&c[8..11]) as u32, method: c[11] })
            .collect();
        let names = take(&mut p, nmethods * method_len)?;
        let methods = names.chunks(method_len.max(1)).map(|c| String::from_utf8_lossy(c.split(|&x| x == 0).next().unwrap_or(&[])).into_owned()).collect();
        if flags & FLAG_SIGNED != 0 {
            let hs = u32le(b.get(p..p + 4)?, 0) as usize;
            p += 4 + 2 * hs + nblocks * 20;
        }
        let mut files = Vec::new();
        let encrypted = flags & FLAG_ENCRYPTED != 0;
        if flags & FLAG_INDEXED != 0 && dir_size > 0 && !encrypted {
            // shortcut: an encrypted directory index (AES) isn't read; such games need their key, which StutterFree doesn't take yet
            let d = b.get(p..p + dir_size)?;
            files = directory_files(d).unwrap_or_default();
        }
        let by_id = chunk_ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        Some(Toc { path: path.to_path_buf(), version, encrypted, chunk_ids, by_id, offlen, blocks, methods, block_size, partition_size, files })
    }

    pub fn has_chunk(&self, id: &[u8; 12]) -> bool {
        self.by_id.contains_key(id)
    }

    pub fn read_chunk_id(&self, id: &[u8; 12]) -> Option<Vec<u8>> {
        self.read_entry(*self.by_id.get(id)?)
    }

    /// The uncompressed bytes of a TOC entry, block by block from the .ucas partitions.
    pub fn read_entry(&self, i: usize) -> Option<Vec<u8>> {
        if self.encrypted {
            return None;
        }
        let (off, len) = *self.offlen.get(i)?;
        let first = (off / self.block_size) as usize;
        let last = ((off + len).saturating_sub(1) / self.block_size) as usize;
        let mut out = Vec::with_capacity(len as usize);
        let mut files: HashMap<u64, File> = HashMap::new();
        for bi in first..=last {
            let blk = self.blocks.get(bi)?;
            let part = if self.partition_size == u64::MAX || self.partition_size == 0 { 0 } else { blk.offset / self.partition_size };
            let file = match files.entry(part) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    let stem = self.path.with_extension("");
                    let name = if part == 0 { format!("{}.ucas", stem.display()) } else { format!("{}_s{part}.ucas", stem.display()) };
                    v.insert(File::open(name).ok()?)
                }
            };
            let mut raw = vec![0u8; blk.csize as usize];
            let at = if part == 0 { blk.offset } else { blk.offset % self.partition_size };
            file.read_exact_at(&mut raw, at).ok()?;
            let method = if blk.method == 0 { "" } else { self.methods.get(blk.method as usize - 1)?.as_str() };
            let data = compress::decompress(method, &raw, blk.usize_ as usize)?;
            let start = if bi == first { (off % self.block_size) as usize } else { 0 };
            let need = (len as usize - out.len()).min(data.len() - start);
            out.extend_from_slice(&data[start..start + need]);
        }
        (out.len() == len as usize).then_some(out)
    }

    pub fn chunk_count(&self) -> usize {
        self.chunk_ids.len()
    }
}

/// The file names of an FIoDirectoryIndexResource, each with the TOC entry it names (UserData).
fn directory_files(d: &[u8]) -> Option<Vec<(String, usize)>> {
    let mut p = 0;
    fstring(d, &mut p)?; // mount point
    let ndirs = u32le(d.get(p..p + 4)?, 0) as usize;
    p += 4 + ndirs * 16;
    let nfiles = u32le(d.get(p..p + 4)?, 0) as usize;
    let files_at = p + 4;
    p = files_at + nfiles * 12;
    let nstrings = u32le(d.get(p..p + 4)?, 0) as usize;
    p += 4;
    let mut strings = Vec::with_capacity(nstrings);
    for _ in 0..nstrings {
        strings.push(fstring(d, &mut p)?);
    }
    (0..nfiles)
        .map(|i| {
            let e = files_at + i * 12;
            let name = strings.get(u32le(d, e) as usize)?.clone();
            Some((name, u32le(d, e + 8) as usize))
        })
        .collect()
}
