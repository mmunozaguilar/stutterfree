//! The recorder's / warmer's db format (SCSKiller.Core Planning/PsoDb.cs): records of u8 tag, u32 len, payload.
//!
//! 'B' blob = sha1[20] + bytes (shader container or root signature). 'G' / 'C' / 'S' = canonical PSO payloads
//! (graphics desc, compute desc, pipeline stream); sha1(tag + payload) is the PSO key. 'P' plan item = template key[20],
//! root sig[20], u32 n, n x (u32 stage, sha1[20]), then a layout. '1' D3D11 item = u32 stage + sha1[20]. '2' D3D11
//! tessellation pair = HS sha1[20] + DS sha1[20]. 'R' / 'A' = ray tracing state objects. 'N' = NVAPI state of another
//! record. 'W' = a create a layer wrapping the device changed. Hashes are lowercase hex; all-zero = none.
//!
//! A *compact* recording is "\0SCSKREC", u8 version (1), i64 length of the raw db it holds, then that db Brotli-compressed.

use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const ZERO: &str = "0000000000000000000000000000000000000000";

pub fn hex(b: &[u8]) -> String {
    hex::encode(b)
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rec {
    pub tag: u8,
    pub payload: Vec<u8>,
}

impl Rec {
    pub fn new(tag: u8, payload: Vec<u8>) -> Self {
        Rec { tag, payload }
    }

    /// sha1(tag + payload), lowercase hex: the record's key.
    pub fn key(&self) -> String {
        let mut h = Sha1::new();
        h.update([self.tag]);
        h.update(&self.payload);
        hex(&h.finalize())
    }

    pub fn tag_char(&self) -> char {
        self.tag as char
    }

    /// A 'B' record's blob hash (its first 20 bytes).
    pub fn blob_hash(&self) -> Option<String> {
        (self.payload.len() >= 20).then(|| hex(&self.payload[..20]))
    }
}

/// Pipeline / state-object records the warmer creates.
pub fn is_item(tag: u8) -> bool {
    matches!(tag, b'G' | b'C' | b'S' | b'R' | b'A' | b'P' | b'1' | b'2')
}

pub fn is_state_object(tag: u8) -> bool {
    matches!(tag, b'R' | b'A')
}

const COMPACT_MAGIC: &[u8; 8] = b"\0SCSKREC";
const COMPACT_VERSION: u8 = 1;
const COMPACT_HEAD: usize = 8 + 1 + 8;

/// Streams the records of a raw db: stops quietly at a torn tail, as the recorder does.
pub struct RawReader<R: Read> {
    r: R,
    remaining: Option<u64>,
    failed: bool,
}

impl<R: Read> RawReader<R> {
    pub fn new(r: R, length: Option<u64>) -> Self {
        RawReader { r, remaining: length, failed: false }
    }
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

impl<R: Read> Iterator for RawReader<R> {
    type Item = io::Result<Rec>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let mut head = [0u8; 5];
        match read_full(&mut self.r, &mut head) {
            Ok(5) => {}
            Ok(_) => return None,
            Err(e) => {
                self.failed = true;
                return Some(Err(e));
            }
        }
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as u64;
        if head[0] == b'B' && len < 20 {
            self.failed = true;
            return Some(Err(invalid(format!("a 'B' record of {len} bytes, short of its SHA-1"))));
        }
        if let Some(rem) = self.remaining.as_mut() {
            *rem = rem.saturating_sub(5);
            if len > *rem {
                return None; // a torn tail, or a stray file's "length": nothing allocated
            }
            *rem -= len;
        }
        // a stream (a compact recording's Brotli) can't tell its length: grow only with bytes that arrive
        let mut body = Vec::with_capacity(len.min(1 << 20) as usize);
        match (&mut self.r).take(len).read_to_end(&mut body) {
            Ok(n) if n as u64 == len => Some(Ok(Rec::new(head[0], body))),
            Ok(_) => None,
            Err(e) => {
                self.failed = true;
                Some(Err(e))
            }
        }
    }
}

/// Every record of a raw or compact db, in file order.
pub struct DbReader {
    inner: Box<dyn Iterator<Item = io::Result<Rec>> + Send>,
    expected: Option<u64>,
    got: u64,
    path: String,
    done: bool,
}

impl Iterator for DbReader {
    type Item = io::Result<Rec>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.inner.next() {
            Some(Ok(r)) => {
                self.got += 5 + r.payload.len() as u64;
                Some(Ok(r))
            }
            Some(Err(e)) => {
                self.done = true;
                Some(Err(e))
            }
            None => {
                self.done = true;
                match self.expected {
                    Some(raw) if raw != self.got => Some(Err(invalid(format!(
                        "{}: a compact recording that ends short ({} of {} bytes)",
                        self.path, self.got, raw
                    )))),
                    _ => None,
                }
            }
        }
    }
}

pub fn read(path: &Path) -> io::Result<DbReader> {
    let mut f = File::open(path)?;
    let mut head = [0u8; COMPACT_HEAD];
    let n = read_full(&mut f, &mut head)?;
    let path_s = path.to_string_lossy().into_owned();
    if !(n >= COMPACT_MAGIC.len() && &head[..8] == COMPACT_MAGIC) {
        f.seek(SeekFrom::Start(0))?;
        let len = f.metadata()?.len();
        return Ok(DbReader {
            inner: Box::new(RawReader::new(BufReader::with_capacity(1 << 20, f), Some(len))),
            expected: None,
            got: 0,
            path: path_s,
            done: false,
        });
    }
    if n < COMPACT_HEAD || head[8] != COMPACT_VERSION {
        return Err(invalid(format!("{path_s}: a compact recording of an unknown version")));
    }
    let raw = i64::from_le_bytes(head[9..17].try_into().unwrap()) as u64;
    let dec = brotli::Decompressor::new(BufReader::with_capacity(1 << 20, f), 1 << 16);
    Ok(DbReader { inner: Box::new(RawReader::new(dec, None)), expected: Some(raw), got: 0, path: path_s, done: false })
}

/// All records (errors included: a malformed compact file fails the whole read).
pub fn read_all(path: &Path) -> io::Result<Vec<Rec>> {
    read(path)?.collect()
}

/// The records that read before the first error (as the C# `foreach` over a throwing enumerable inside a try did).
pub fn read_lenient(path: &Path) -> Vec<Rec> {
    match read(path) {
        Ok(it) => it.map_while(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn is_compact(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let mut head = [0u8; 8];
    matches!(read_full(&mut f, &mut head), Ok(8)) && &head == COMPACT_MAGIC
}

pub fn write<W: Write>(w: &mut W, tag: u8, payload: &[u8]) -> io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = tag;
    head[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)
}

pub fn write_blob<W: Write>(w: &mut W, sha1_hex: &str, bytes: &[u8]) -> io::Result<()> {
    let h = hex::decode(sha1_hex).map_err(|e| invalid(e.to_string()))?;
    let mut head = Vec::with_capacity(25);
    head.push(b'B');
    head.extend_from_slice(&((20 + bytes.len()) as u32).to_le_bytes());
    head.extend_from_slice(&h);
    w.write_all(&head)?;
    w.write_all(bytes)
}

/// Writes records as a raw db at `path` through a temp file.
pub fn write_raw_file<'a>(path: &Path, records: impl IntoIterator<Item = &'a Rec>) -> io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = crate::util::tmp_path(path);
    let r = (|| {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        for r in records {
            write(&mut w, r.tag, &r.payload)?;
        }
        w.flush()?;
        drop(w);
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

fn framed_hash(h: &mut sha2::Sha256, r: &Rec) {
    use sha2::Digest as _;
    let mut head = [0u8; 5];
    head[0] = r.tag;
    head[1..].copy_from_slice(&(r.payload.len() as u32).to_le_bytes());
    h.update(head);
    h.update(&r.payload);
}

/// Writes records as a compact recording at `path`: into a temp file that is read back and compared before it
/// replaces `path`, so an interrupted write leaves the old file.
pub fn write_compact(path: &Path, records: impl IntoIterator<Item = io::Result<Rec>>) -> io::Result<()> {
    use sha2::Digest as _;
    let tmp = crate::util::tmp_path(path);
    let result = (|| -> io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(&[0u8; COMPACT_HEAD])?;
        let mut h = sha2::Sha256::new();
        let mut raw: u64 = 0;
        {
            // Brotli 6: 20x on recordings' pipeline records in well under a second
            let mut b = BufWriter::with_capacity(1 << 20, brotli::CompressorWriter::new(&mut f, 1 << 16, 6, 22));
            for r in records {
                let r = r?;
                write(&mut b, r.tag, &r.payload)?;
                framed_hash(&mut h, &r);
                raw += 5 + r.payload.len() as u64;
            }
            b.flush()?;
            let enc = b.into_inner().map_err(|e| e.into_error())?;
            drop(enc); // finishes the Brotli stream
        }
        let sum = h.finalize();
        f.seek(SeekFrom::Start(0))?;
        f.write_all(COMPACT_MAGIC)?;
        f.write_all(&[COMPACT_VERSION])?;
        f.write_all(&(raw as i64).to_le_bytes())?;
        f.sync_all().ok();
        drop(f);
        let mut h2 = sha2::Sha256::new();
        for r in read(&tmp)? {
            framed_hash(&mut h2, &r?);
        }
        if h2.finalize() != sum {
            return Err(invalid(format!("{} doesn't read back as written", tmp.display())));
        }
        std::fs::rename(&tmp, path)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

// ---- parsing -------------------------------------------------------------------------------------------------------

fn u32_at(p: &[u8], o: usize) -> io::Result<u32> {
    p.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| invalid("record truncated"))
}

fn h_at(p: &[u8], o: usize) -> io::Result<String> {
    p.get(o..o + 20).map(hex).ok_or_else(|| invalid("record truncated"))
}

/// Stage numbers (SCSKiller.Core's Stage).
pub mod stage {
    pub const VERTEX: u32 = 1;
    pub const PIXEL: u32 = 2;
    pub const DOMAIN: u32 = 3;
    pub const HULL: u32 = 4;
    pub const GEOMETRY: u32 = 5;
    pub const COMPUTE: u32 = 6;
    pub const AMPLIFICATION: u32 = 24;
    pub const MESH: u32 = 25;
}

/// A PSO record's (root sig, stage -> shader) tuple.
#[derive(Debug, Clone)]
pub struct Pso {
    pub rs: String,
    pub stages: BTreeMap<u32, String>,
    pub has_layout: bool,
    pub topology: u32,
}

const GFX_LAYOUT_AT: usize = 120 + 328 + 4 + 44 + 52; // 'G': rs + 5 shaders, blend, sample mask, rasterizer, depth-stencil
pub const SO_DECL: u32 = 0x10007;

fn sub_pod(t: u32) -> Option<usize> {
    Some(match t {
        8 => 328,
        9 => 4,
        10 => 44,
        11 => 52,
        13 => 4,
        14 => 4,
        15 => 36,
        16 => 4,
        17 => 8,
        18 => 4,
        20 => 4,
        21 => 56,
        26 => 60,
        27 => 44,
        28 => 40,
        _ => return None,
    })
}

fn is_shader(t: u32) -> bool {
    (1..=6).contains(&t) || t == 24 || t == 25
}

fn layout_size(p: &[u8], pos: usize) -> io::Result<usize> {
    let mut o = pos + 4;
    for _ in 0..u32_at(p, pos)? {
        let n = u32_at(p, o)? as usize;
        o = o.checked_add(4 + n + 24).ok_or_else(|| invalid("layout overflow"))?;
        if o > p.len() {
            return Err(invalid("layout truncated"));
        }
    }
    Ok(o - pos - 4)
}

fn so_size(p: &[u8], pos: usize) -> io::Result<usize> {
    let mut o = pos + 4;
    for _ in 0..u32_at(p, pos)? {
        let n = u32_at(p, o + 4)?;
        let s = if n != u32::MAX { n as usize } else { 0 };
        o = o.checked_add(8 + s + 16).ok_or_else(|| invalid("stream output overflow"))?;
        if o > p.len() {
            return Err(invalid("stream output truncated"));
        }
    }
    let strides = u32_at(p, o)? as usize;
    Ok(o + 4 + 4 * strides + 4 - pos)
}

/// An 'S' payload's subobjects in order: type, where its body starts and where it ends.
fn subobjects(p: &[u8]) -> io::Result<Vec<(u32, usize, usize)>> {
    let mut out = Vec::new();
    let mut pos = 4;
    let count = u32_at(p, 0)?;
    if count > 64 {
        return Err(invalid("stream of too many subobjects"));
    }
    for _ in 0..count {
        let t = u32_at(p, pos)?;
        pos += 4;
        let size = if t == 0 || is_shader(t) {
            20
        } else if t == 12 {
            4 + layout_size(p, pos)?
        } else if t == 22 {
            4 + 8 * u32_at(p, pos)? as usize + 4
        } else if t == 7 {
            0
        } else if t == SO_DECL {
            so_size(p, pos)?
        } else {
            sub_pod(t).ok_or_else(|| invalid(format!("unknown stream subobject {t}")))?
        };
        if pos + size > p.len() {
            return Err(invalid("stream subobject truncated"));
        }
        out.push((t, pos, pos + size));
        pos += size;
    }
    Ok(out)
}

pub fn parse(r: &Rec) -> io::Result<Pso> {
    let p = &r.payload;
    match r.tag {
        b'C' => {
            let mut st = BTreeMap::new();
            st.insert(stage::COMPUTE, h_at(p, 20)?);
            Ok(Pso { rs: h_at(p, 0)?, stages: st, has_layout: false, topology: 0 })
        }
        b'G' => {
            let order = [1u32, 2, 3, 4, 5]; // VS, PS, DS, HS, GS
            let mut st = BTreeMap::new();
            for (i, s) in order.iter().enumerate() {
                let h = h_at(p, 20 + 20 * i)?;
                if h != ZERO {
                    st.insert(*s, h);
                }
            }
            let has_layout = u32_at(p, GFX_LAYOUT_AT)? > 0;
            let topo = u32_at(p, GFX_LAYOUT_AT + 4 + layout_size(p, GFX_LAYOUT_AT)? + 4)?;
            Ok(Pso { rs: h_at(p, 0)?, stages: st, has_layout, topology: topo })
        }
        b'S' => {
            let mut rs = ZERO.to_string();
            let mut st = BTreeMap::new();
            let mut layout = false;
            let mut topo = 0;
            let mut end = 4;
            for (t, pos, next) in subobjects(p)? {
                if t == 0 {
                    rs = h_at(p, pos)?;
                } else if is_shader(t) {
                    let h = h_at(p, pos)?;
                    if h != ZERO {
                        st.insert(t, h);
                    }
                } else if t == 12 {
                    layout = u32_at(p, pos)? > 0;
                } else if t == 14 {
                    topo = u32_at(p, pos)?;
                }
                end = next;
            }
            if end != p.len() {
                return Err(invalid(format!("stream record parse mismatch ({end} != {})", p.len())));
            }
            Ok(Pso { rs, stages: st, has_layout: layout, topology: topo })
        }
        t => Err(invalid(format!("not a PSO record: '{}'", t as char))),
    }
}

/// A ray tracing state object record: 'R' (CreateStateObject) or 'A' (AddToStateObject: `base`, the key of the record
/// it grows). `depends`: the records it builds on (its base, the collections it links).
#[derive(Debug, Clone)]
pub struct StateObject {
    pub kind: u32,
    pub base: Option<String>,
    pub libraries: Vec<String>,
    pub root_signatures: Vec<String>,
    pub depends: Vec<String>,
}

pub const MAX_SUBOBJECTS: u32 = 8192;

pub fn parse_state_object(r: &Rec) -> io::Result<StateObject> {
    if !is_state_object(r.tag) {
        return Err(invalid(format!("'{}' is not a state object record", r.tag as char)));
    }
    let p = &r.payload;
    let mut pos = 0usize;
    let next = |pos: &mut usize| -> io::Result<u32> {
        let v = u32_at(p, *pos).map_err(|_| invalid("state object record truncated"))?;
        *pos += 4;
        Ok(v)
    };
    let hash = |pos: &mut usize| -> io::Result<String> {
        let h = h_at(p, *pos).map_err(|_| invalid("state object record truncated"))?;
        *pos += 20;
        Ok(h)
    };
    let str_ = |pos: &mut usize| -> io::Result<()> {
        let n = next(pos)?;
        if n != u32::MAX {
            if 2 * n as u64 > (p.len() - *pos) as u64 {
                return Err(invalid("malformed string"));
            }
            *pos += 2 * n as usize;
        }
        Ok(())
    };
    let base = if r.tag == b'A' { Some(hash(&mut pos)?) } else { None };
    let kind = next(&mut pos)?;
    let mut so = StateObject {
        kind,
        base: base.clone(),
        libraries: Vec::new(),
        root_signatures: Vec::new(),
        depends: base.into_iter().collect(),
    };
    let count = next(&mut pos)?;
    if count > MAX_SUBOBJECTS {
        return Err(invalid(format!("state object of {count} subobjects, over {MAX_SUBOBJECTS}")));
    }
    let exports = |pos: &mut usize| -> io::Result<()> {
        for _ in 0..next(pos)? {
            str_(pos)?;
            str_(pos)?;
            next(pos)?;
        }
        Ok(())
    };
    let strs = |pos: &mut usize| -> io::Result<()> {
        for _ in 0..next(pos)? {
            str_(pos)?;
        }
        Ok(())
    };
    for _ in 0..count {
        match next(&mut pos)? {
            0 | 3 | 10 => {
                next(&mut pos)?;
            }
            9 | 12 => {
                next(&mut pos)?;
                next(&mut pos)?;
            }
            1 | 2 => {
                let rs = hash(&mut pos)?;
                if rs != ZERO {
                    so.root_signatures.push(rs);
                }
            }
            5 => {
                so.libraries.push(hash(&mut pos)?);
                exports(&mut pos)?;
            }
            6 => {
                so.depends.push(hash(&mut pos)?);
                exports(&mut pos)?;
            }
            7 => {
                next(&mut pos)?;
                strs(&mut pos)?;
            }
            8 => {
                str_(&mut pos)?;
                strs(&mut pos)?;
            }
            11 => {
                str_(&mut pos)?;
                next(&mut pos)?;
                str_(&mut pos)?;
                str_(&mut pos)?;
                str_(&mut pos)?;
            }
            t => return Err(invalid(format!("unknown state subobject type {t}"))),
        }
    }
    if pos != p.len() {
        return Err(invalid(format!("state object record parse mismatch ({pos} != {})", p.len())));
    }
    Ok(so)
}

/// An 'N' record: the NVAPI state the record with key `target` was created with.
#[derive(Debug, Clone)]
pub struct NvState {
    pub target: String,
    pub slot: u32,
    pub space: u32,
    pub scope: u32,
    pub options: u32,
}

pub fn parse_nv_state(r: &Rec) -> io::Result<NvState> {
    if r.tag != b'N' || r.payload.len() != 36 {
        return Err(invalid(format!("not an 'N' record ('{}', {} bytes)", r.tag as char, r.payload.len())));
    }
    let p = &r.payload;
    Ok(NvState { target: hex(&p[..20]), slot: u32_at(p, 20)?, space: u32_at(p, 24)?, scope: u32_at(p, 28)?, options: u32_at(p, 32)? })
}

/// A 'P' plan item: (template, root sig, stages).
pub fn parse_item(p: &[u8]) -> io::Result<(String, String, BTreeMap<u32, String>)> {
    let mut st = BTreeMap::new();
    let n = u32_at(p, 40)? as usize;
    for i in 0..n {
        st.insert(u32_at(p, 44 + 24 * i)?, h_at(p, 48 + 24 * i)?);
    }
    Ok((h_at(p, 0)?, h_at(p, 20)?, st))
}

/// Whether a graphics PSO record has a vertex shader (Planner.HasDraws).
pub fn has_vertex_stage(r: &Rec) -> bool {
    matches!(r.tag, b'G' | b'S') && parse(r).map(|p| p.stages.contains_key(&stage::VERTEX)).unwrap_or(false)
}

/// Every non-zero blob hash the records reference (Rehydrate.References): stages + root signatures of 'G'/'C'/'S',
/// 'P' items, '1' / '2' D3D11 items, DXIL libraries + root signatures of 'R'/'A' state objects. ('Y' / 'H' are
/// plan-only records of the planner, which recordings never hold.)
pub fn references<'a>(records: impl IntoIterator<Item = &'a Rec>) -> io::Result<HashSet<String>> {
    let mut refs = HashSet::new();
    for r in records {
        match r.tag {
            b'G' | b'C' | b'S' => {
                let p = parse(r)?;
                refs.extend(p.stages.into_values());
                refs.insert(p.rs);
            }
            b'P' => {
                let (_, rs, st) = parse_item(&r.payload)?;
                refs.extend(st.into_values());
                refs.insert(rs);
            }
            b'1' => {
                refs.insert(h_at(&r.payload, 4)?);
            }
            b'2' => {
                refs.insert(h_at(&r.payload, 0)?);
                refs.insert(h_at(&r.payload, 20)?);
            }
            b'R' | b'A' => {
                let so = parse_state_object(r)?;
                refs.extend(so.libraries);
                refs.extend(so.root_signatures);
            }
            _ => {}
        }
    }
    refs.remove(ZERO);
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compute(rs: u8, cs: u8) -> Rec {
        let mut p = vec![rs; 20];
        p.extend(vec![cs; 20]);
        p.extend([0u8; 8]);
        Rec::new(b'C', p)
    }

    #[test]
    fn raw_and_compact_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("a.db");
        let mut blob = vec![7u8; 20];
        blob.extend(b"DXBCbytes");
        let recs = vec![Rec::new(b'B', blob), compute(1, 2), compute(3, 4)];
        write_raw_file(&raw, &recs).unwrap();
        assert_eq!(read_all(&raw).unwrap(), recs);
        assert!(!is_compact(&raw));

        let compact = dir.path().join("b.db");
        write_compact(&compact, recs.clone().into_iter().map(Ok)).unwrap();
        assert!(is_compact(&compact));
        assert_eq!(read_all(&compact).unwrap(), recs);
    }

    #[test]
    fn torn_tail_stops_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("a.db");
        let mut bytes = Vec::new();
        write(&mut bytes, b'C', &compute(1, 2).payload).unwrap();
        bytes.extend([b'C', 200, 0, 0, 0, 1, 2]); // claims 200 bytes, has 2
        std::fs::write(&raw, bytes).unwrap();
        assert_eq!(read_all(&raw).unwrap().len(), 1);
    }

    #[test]
    fn references_of_compute() {
        let r = compute(1, 2);
        let refs = references([&r]).unwrap();
        assert_eq!(refs.len(), 2);
        let p = parse(&r).unwrap();
        assert_eq!(p.stages.get(&stage::COMPUTE).unwrap(), &hex(&[2u8; 20]));
    }
}
