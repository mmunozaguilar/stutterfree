//! Unreal .pak files, versions 3-11: the footer, the legacy index (< 10) or the encoded entries + full directory index
//! (10+), and entry reads with block compression. Encrypted indexes / entries need the game's AES key: not read.

use super::compress;
use super::iostore::fstring;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

const MAGIC: [u8; 4] = [0xE1, 0x12, 0x6F, 0x5A];

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    offset: u64,
    size: u64,
    uncompressed: u64,
    method: u32,
    blocks: Vec<(u64, u64)>,
    block_size: u32,
    pub encrypted: bool,
}

pub struct Pak {
    file: File,
    pub version: u32,
    methods: Vec<String>,
    pub entries: Vec<Entry>,
    pub encrypted_index: bool,
}

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl Cur<'_> {
    fn n<const N: usize>(&mut self) -> Option<[u8; N]> {
        let s = self.b.get(self.p..self.p + N)?.try_into().ok()?;
        self.p += N;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.n::<1>()?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.n()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.n()?))
    }
    fn s(&mut self) -> Option<String> {
        fstring(self.b, &mut self.p)
    }
}

impl Pak {
    pub fn open(path: &Path) -> Option<Pak> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let tail_len = len.min(1024) as usize;
        let mut tail = vec![0u8; tail_len];
        file.read_exact_at(&mut tail, len - tail_len as u64).ok()?;
        let m = tail.windows(4).rposition(|w| w == MAGIC)?;
        let mut c = Cur { b: &tail, p: m + 4 };
        let version = c.u32()?;
        let (index_offset, index_size) = (c.u64()?, c.u64()?);
        c.n::<20>()?;
        if version == 9 {
            c.u8()?; // frozen index
        }
        let encrypted_index = m >= 1 && tail[m - 1] != 0;
        let methods: Vec<String> = if version >= 8 {
            tail[c.p..].chunks_exact(32).map(|n| String::from_utf8_lossy(n.split(|&x| x == 0).next().unwrap_or(&[])).into_owned()).collect()
        } else {
            vec!["Zlib".into(), "Gzip".into(), String::new(), String::new(), "Oodle".into()] // pre-8: flags (1 zlib, 2 gzip, 16 custom)
        };
        let mut pak = Pak { file, version, methods, entries: Vec::new(), encrypted_index };
        if encrypted_index || index_size > 512 << 20 || index_offset + index_size > len {
            return Some(pak); // shortcut: an encrypted index needs the AES key (aes.key), not taken yet
        }
        let mut idx = vec![0u8; index_size as usize];
        pak.file.read_exact_at(&mut idx, index_offset).ok()?;
        pak.entries = if version >= 10 { pak.encoded_index(&idx, len)? } else { pak.legacy_index(&idx)? };
        Some(pak)
    }

    fn method_code(&self, c: &mut Cur) -> Option<u32> {
        // v8 paks from before 4.22 store it as a byte; their footer lists 4 methods instead of 5
        if self.version == 8 && self.methods.len() == 4 { Some(c.u8()? as u32) } else { c.u32() }
    }

    /// The FPakEntry layout of the legacy index and of every entry's header in the data.
    fn serialized_entry(&self, c: &mut Cur, path: String) -> Option<Entry> {
        let (offset, size, uncompressed) = (c.u64()?, c.u64()?, c.u64()?);
        let method = self.method_code(c)?;
        if self.version <= 1 {
            c.u64()?; // timestamp
        }
        c.n::<20>()?;
        let mut blocks = Vec::new();
        let (mut encrypted, mut block_size) = (false, 0);
        if self.version >= 3 {
            if method != 0 {
                for _ in 0..c.u32()? {
                    blocks.push((c.u64()?, c.u64()?));
                }
            }
            encrypted = c.u8()? & 1 != 0;
            block_size = c.u32()?;
        }
        Some(Entry { path, offset, size, uncompressed, method, blocks, block_size, encrypted })
    }

    fn legacy_index(&self, idx: &[u8]) -> Option<Vec<Entry>> {
        let mut c = Cur { b: idx, p: 0 };
        let mount = c.s()?;
        let n = c.u32()?;
        (0..n).map(|_| { let name = c.s()?; self.serialized_entry(&mut c, format!("{mount}{name}")) }).collect()
    }

    fn encoded_index(&self, idx: &[u8], len: u64) -> Option<Vec<Entry>> {
        let mut c = Cur { b: idx, p: 0 };
        let mount = c.s()?;
        c.u32()?; // entry count
        c.u64()?; // path hash seed
        if c.u32()? != 0 {
            c.u64()?;
            c.u64()?;
            c.n::<20>()?;
        }
        let full = if c.u32()? != 0 {
            let (o, s) = (c.u64()?, c.u64()?);
            c.n::<20>()?;
            Some((o, s))
        } else {
            None
        };
        let enc_len = c.u32()? as usize;
        let encoded = idx.get(c.p..c.p + enc_len)?.to_vec();
        c.p += enc_len;
        let mut plain = Vec::new();
        for _ in 0..c.u32()? {
            plain.push(self.serialized_entry(&mut c, String::new())?);
        }
        let (fo, fs) = full?; // shortcut: paks with only a path-hash index (no names) aren't listed
        if fo + fs > len || fs > 512 << 20 {
            return None;
        }
        let mut dir = vec![0u8; fs as usize];
        self.file.read_exact_at(&mut dir, fo).ok()?;
        let mut d = Cur { b: &dir, p: 0 };
        let mut out = Vec::new();
        for _ in 0..d.u32()? {
            let dname = d.s()?;
            for _ in 0..d.u32()? {
                let fname = d.s()?;
                let at = d.u32()? as i32;
                let path = format!("{mount}{dname}{fname}");
                let e = if at >= 0 { self.decode_entry(&encoded, at as usize, path) } else { plain.get((-at - 1) as usize).cloned().map(|e| Entry { path, ..e }) };
                if let Some(e) = e {
                    out.push(e);
                }
            }
        }
        Some(out)
    }

    /// An encoded FPakEntry (UE 4.25+): bit-packed sizes, method, block count and flags.
    fn decode_entry(&self, b: &[u8], at: usize, path: String) -> Option<Entry> {
        let mut c = Cur { b, p: at };
        let bits = c.u32()?;
        let block_size = if bits & 0x3F == 0x3F { c.u32()? } else { (bits & 0x3F) << 11 };
        let method = (bits >> 23) & 0x3F;
        let encrypted = (bits >> 22) & 1 != 0;
        let nblocks = (bits >> 6) & 0xFFFF;
        let rd = |c: &mut Cur, small: bool| if small { c.u32().map(u64::from) } else { c.u64() };
        let offset = rd(&mut c, bits & (1 << 31) != 0)?;
        let uncompressed = rd(&mut c, bits & (1 << 30) != 0)?;
        let size = if method != 0 { rd(&mut c, bits & (1 << 29) != 0)? } else { uncompressed };
        // the entry's header in the data comes before its blocks
        let header = 8 + 8 + 8 + 4 + 20 + if method != 0 { 4 + 16 * nblocks as u64 } else { 0 } + 1 + 4;
        let align = |x: u64| if encrypted { (x + 15) & !15 } else { x };
        let mut blocks = Vec::new();
        if nblocks == 1 && !encrypted {
            blocks.push((header, header + size));
        } else {
            let mut start = header;
            for _ in 0..nblocks {
                let n = c.u32()? as u64;
                blocks.push((start, start + n));
                start += align(n);
            }
        }
        Some(Entry { path, offset, size, uncompressed, method, blocks, block_size, encrypted })
    }

    /// An entry's uncompressed bytes; None for an encrypted entry or a method this can't decode.
    pub fn read(&self, e: &Entry) -> Option<Vec<u8>> {
        if e.encrypted || e.uncompressed > 1 << 31 {
            return None;
        }
        if e.method == 0 {
            // the entry repeats its header in the data
            let mut h = vec![0u8; 128.min(e.size as usize + 128)];
            let _ = self.file.read_at(&mut h, e.offset);
            let hdr = self.serialized_entry(&mut Cur { b: &h, p: 0 }, String::new())?;
            let skip = 8 + 8 + 8 + if self.version == 8 && self.methods.len() == 4 { 1 } else { 4 } + 20 + if self.version >= 3 { 5 } else { 0 } + if self.version <= 1 { 8 } else { 0 };
            let mut out = vec![0u8; hdr.size as usize];
            self.file.read_exact_at(&mut out, e.offset + skip as u64).ok()?;
            return Some(out);
        }
        let name = if self.version >= 8 { self.methods.get(e.method as usize - 1)?.clone() } else { self.methods.get(e.method.trailing_zeros() as usize)?.clone() };
        let mut out = Vec::with_capacity(e.uncompressed as usize);
        for &(s, t) in &e.blocks {
            let base = if self.version >= 5 { e.offset } else { 0 };
            let mut raw = vec![0u8; (t - s) as usize];
            self.file.read_exact_at(&mut raw, base + s).ok()?;
            // a block size of 0 means one block holding the whole entry
            let bs = if e.block_size == 0 { e.uncompressed } else { e.block_size as u64 };
            let want = (e.uncompressed - out.len() as u64).min(bs) as usize;
            out.extend(compress::decompress(&name, &raw, want)?);
        }
        (out.len() as u64 == e.uncompressed).then_some(out)
    }
}
