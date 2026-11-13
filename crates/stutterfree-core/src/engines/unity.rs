//! Unity games (UnityPlayer.dll + a *_Data folder), from SCSKiller.Core Unity/UnityReader.cs + UnityFiles.cs.
//!
//! Shaders live in serialized Shader objects (class 48) as a per-platform LZ4 blob, in loose files and in UnityFS bundles
//! anywhere under *_Data; ComputeShader objects (72) keep their DXBC raw. The d3d11 platform blob holds DXBC containers
//! both the DX11 and the DX12 player run. Unity ships no root signatures, so only DX11 plans from the files; DX12 needs a
//! recording.

use crate::dxbc;
use sha1::{Digest, Sha1};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const SHADER_CLASS: i32 = 48;
pub const COMPUTE_SHADER_CLASS: i32 = 72;
pub const BUILD_SETTINGS_CLASS: i32 = 141;
/// ShaderCompilerPlatform "d3d11": the DXBC both the DX11 and DX12 players run.
pub const D3D11_PLATFORM: u32 = 4;

// ---- byte reader ------------------------------------------------------------------------------------------------------

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    be: bool,
}

type R<T> = Result<T, ()>;

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos.checked_add(n).ok_or(())?).ok_or(())?;
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> R<u16> {
        let s = self.take(2)?.try_into().unwrap();
        Ok(if self.be { u16::from_be_bytes(s) } else { u16::from_le_bytes(s) })
    }
    fn u32(&mut self) -> R<u32> {
        let s = self.take(4)?.try_into().unwrap();
        Ok(if self.be { u32::from_be_bytes(s) } else { u32::from_le_bytes(s) })
    }
    fn i32(&mut self) -> R<i32> {
        Ok(self.u32()? as i32)
    }
    fn i64(&mut self) -> R<i64> {
        let s = self.take(8)?.try_into().unwrap();
        Ok(if self.be { i64::from_be_bytes(s) } else { i64::from_le_bytes(s) })
    }
    fn count(&mut self, max: usize) -> R<usize> {
        let n = self.i32()?;
        if n >= 0 && n as usize <= max {
            Ok(n as usize)
        } else {
            Err(())
        }
    }
    fn skip(&mut self, n: usize) -> R<()> {
        self.take(n).map(|_| ())
    }
    fn align(&mut self, a: usize) {
        self.pos = ((self.pos + a - 1) / a * a).min(self.b.len());
    }
    fn cstring(&mut self) -> R<String> {
        let n = self.b.get(self.pos..).ok_or(())?.iter().position(|&c| c == 0).ok_or(())?;
        let s = String::from_utf8_lossy(self.take(n)?).into_owned();
        self.pos += 1;
        Ok(s)
    }
}

fn le(s: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(s[o..o + 4].try_into().unwrap())
}

// ---- serialized files -------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Obj {
    pub start: u64,
    pub size: u64,
    pub class_id: i32,
}

#[derive(Debug)]
pub struct SerializedFile {
    pub unity_version: String,
    pub objects: Vec<Obj>,
}

/// The object table of a SerializedFile (formats 9-22 and up), or None when the bytes aren't one. No type tree needed.
pub fn read_serialized(read: &dyn Fn(u64, usize) -> Vec<u8>, length: u64) -> Option<SerializedFile> {
    let h = read(0, 48);
    if h.len() < 48 {
        return None;
    }
    let be = |o: usize| u32::from_be_bytes(h[o..o + 4].try_into().unwrap()) as u64;
    let (mut meta_size, mut file_size, mut data_offset) = (be(0), be(4), be(12));
    let format = be(8) as i64;
    let mut meta_at = 20u64;
    if !(9..=64).contains(&format) {
        return None;
    }
    if format >= 22 {
        meta_size = be(20);
        file_size = u64::from_be_bytes(h[24..32].try_into().unwrap());
        data_offset = u64::from_be_bytes(h[32..40].try_into().unwrap());
        meta_at = 48;
    }
    if file_size != length || data_offset > length || meta_size == 0 || meta_size > 256 << 20 || meta_at + meta_size > length {
        return None;
    }
    let meta = read(meta_at, meta_size as usize);
    let mut r = Reader { b: &meta, pos: 0, be: h[16] != 0 };
    (|| -> R<SerializedFile> {
        let unity = r.cstring()?;
        r.i32()?; // target platform
        let type_tree = format < 13 || r.u8()? != 0;
        let mut types = vec![0i32; r.count(1 << 20)?];
        for t in types.iter_mut() {
            let cls = r.i32()?;
            *t = cls;
            if format >= 16 {
                r.u8()?; // stripped
            }
            if format >= 17 {
                r.u16()?; // script type index
            }
            if format >= 13 {
                r.skip(if (format < 16 && cls < 0) || (format >= 16 && cls == 114) { 32 } else { 16 })?;
            }
            if !type_tree {
                continue;
            }
            if format < 12 && format != 10 {
                return Err(()); // old recursive trees: Unity 4 and older
            }
            let (nodes, strings) = (r.count(1 << 20)?, r.count(64 << 20)?);
            r.skip(nodes * if format >= 19 { 32 } else { 24 } + strings)?;
            if format >= 21 {
                let n = r.count(1 << 20)?;
                r.skip(4 * n)?;
            }
        }
        if (7..14).contains(&format) {
            r.i32()?; // big id enabled
        }
        let n = r.count(1 << 24)?;
        let mut objects = Vec::with_capacity(n);
        for _ in 0..n {
            if format >= 14 {
                r.align(4);
            }
            if format >= 14 {
                r.i64()?;
            } else {
                r.i32()?;
            }
            let start = if format >= 22 { r.i64()? as u64 } else { r.u32()? as u64 };
            let size = r.u32()? as u64;
            let t = r.i32()?;
            let cls = if format >= 16 { types.get(t as usize).copied().unwrap_or(-1) } else { t };
            if format < 16 {
                r.u16()?;
            }
            if format < 11 {
                r.u16()?;
            }
            if (11..17).contains(&format) {
                r.u16()?;
            }
            if (15..17).contains(&format) {
                r.u8()?;
            }
            objects.push(Obj { start: data_offset + start, size, class_id: cls });
        }
        Ok(SerializedFile { unity_version: unity, objects })
    })()
    .ok()
}

// ---- UnityFS bundles --------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Node {
    pub offset: u64,
    pub size: u64,
    pub path: String,
}

struct Block {
    uoff: u64,
    usize: usize,
    coff: u64,
    csize: usize,
    comp: u32,
}

/// A UnityFS bundle: node table and random access to its uncompressed data, blocks decompressed on demand (the last 16 kept).
pub struct Bundle {
    file: File,
    blocks: Vec<Block>,
    pub nodes: Vec<Node>,
    cache: RefCell<Vec<(usize, Rc<Vec<u8>>)>>,
}

/// UnityFS compression: 0 none, 1 LZMA (5 property bytes, then the raw stream), 2 LZ4, 3 LZ4HC.
pub fn decompress(src: &[u8], size: usize, comp: u32) -> Option<Vec<u8>> {
    match comp {
        0 => Some(src.to_vec()),
        2 | 3 => {
            let mut dst = vec![0u8; size];
            (lz4_flex::block::decompress_into(src, &mut dst).ok()? == size).then_some(dst)
        }
        1 => {
            let mut out = Vec::with_capacity(size);
            let opts = lzma_rs::decompress::Options {
                unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(Some(size as u64)),
                ..Default::default()
            };
            lzma_rs::lzma_decompress_with_options(&mut std::io::Cursor::new(src), &mut out, &opts).ok()?;
            (out.len() == size).then_some(out)
        }
        _ => None,
    }
}

impl Bundle {
    pub fn open(path: &Path) -> Option<Bundle> {
        let file = File::open(path).ok()?;
        let length = file.metadata().ok()?.len();
        let mut head = vec![0u8; length.min(512) as usize];
        file.read_exact_at(&mut head, 0).ok()?;
        if !head.starts_with(b"UnityFS\0") {
            return None;
        }
        let mut r = Reader { b: &head, pos: 8, be: true };
        let (blocks, nodes) = (|| -> R<(Vec<Block>, Vec<Node>)> {
            let ver = r.u32()?;
            r.cstring()?; // unity version
            r.cstring()?; // revision
            r.i64()?; // total size
            let (cbi, ubi, flags) = (r.u32()? as usize, r.u32()? as usize, r.u32()?);
            if cbi > 64 << 20 || ubi > 64 << 20 || cbi as u64 > length {
                return Err(());
            }
            if ver >= 7 {
                r.align(16);
            }
            let mut at = r.pos as u64;
            let mut info = vec![0u8; cbi];
            file.read_exact_at(&mut info, if flags & 0x80 != 0 { length - cbi as u64 } else { at }).map_err(|_| ())?;
            if flags & 0x80 == 0 {
                at += cbi as u64;
            }
            if flags & 0x200 != 0 {
                at = (at + 15) & !15;
            }
            let info = decompress(&info, ubi, flags & 0x3F).ok_or(())?;
            let mut bi = Reader { b: &info, pos: 16, be: true };
            let mut blocks = Vec::new();
            let (mut uoff, mut coff) = (0u64, at);
            for _ in 0..bi.count(1 << 24)? {
                let (us, cs, bf) = (bi.u32()? as usize, bi.u32()? as usize, bi.u16()? as u32);
                if coff + cs as u64 > length || ((bf & 0x3F) == 2 || (bf & 0x3F) == 3) && us > 256 * cs + 64 {
                    return Err(());
                }
                blocks.push(Block { uoff, usize: us, coff, csize: cs, comp: bf & 0x3F });
                uoff += us as u64;
                coff += cs as u64;
            }
            let mut nodes = Vec::new();
            for _ in 0..bi.count(1 << 20)? {
                let (offset, size) = (bi.i64()? as u64, bi.i64()? as u64);
                bi.u32()?; // flags
                nodes.push(Node { offset, size, path: bi.cstring()? });
            }
            Ok((blocks, nodes))
        })()
        .ok()?;
        Some(Bundle { file, blocks, nodes, cache: RefCell::new(Vec::new()) })
    }

    fn block(&self, i: usize) -> Option<Rc<Vec<u8>>> {
        let mut cache = self.cache.borrow_mut();
        if let Some(at) = cache.iter().position(|(k, _)| *k == i) {
            let hit = cache.remove(at);
            cache.push(hit.clone());
            return Some(hit.1);
        }
        let b = &self.blocks[i];
        let mut src = vec![0u8; b.csize];
        self.file.read_exact_at(&mut src, b.coff).ok()?;
        let data = Rc::new(decompress(&src, b.usize, b.comp)?);
        if cache.len() == 16 {
            cache.remove(0);
        }
        cache.push((i, data.clone()));
        Some(data)
    }

    /// Uncompressed bytes [offset, offset + count) of the bundle's data (short when a block can't be read).
    pub fn read(&self, offset: u64, count: usize) -> Vec<u8> {
        let mut dst = Vec::with_capacity(count);
        let mut i = match self.blocks.partition_point(|b| b.uoff <= offset) {
            0 => return dst,
            n => n - 1,
        };
        while dst.len() < count && i < self.blocks.len() {
            let Some(b) = self.block(i) else { break };
            let from = (offset + dst.len() as u64 - self.blocks[i].uoff) as usize;
            if from >= b.len() {
                break;
            }
            let n = (count - dst.len()).min(b.len() - from);
            dst.extend_from_slice(&b[from..from + n]);
            i += 1;
        }
        dst
    }
}

// ---- shader blobs -----------------------------------------------------------------------------------------------------

/// The d3d11 segments of a Shader object's compressed blob, decompressed; None = no d3d11 code. Found by its shape
/// (platforms, offsets, compressedLengths, decompressedLengths, then the blob); the first candidate whose d3d11 segments
/// all LZ4-decode to their stated size wins.
/// shortcut: a shape scan instead of the per-version SerializedShader layout, as upstream; parse m_ParsedForm per Unity
/// version if a game ever breaks it.
pub fn shader_blob(o: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut p = 0;
    while p + 16 <= o.len() {
        let n = le(o, p) as usize;
        if (1..=32).contains(&n) {
            for nested in [true, false] {
                if let Some((segs, blob_at)) = shape(o, p, n, nested) {
                    if let Some(d) = decode(o, &segs, blob_at) {
                        return Some(d);
                    }
                }
            }
        }
        p += 4;
    }
    None
}

fn u_array(o: &[u8], q: &mut usize, max: usize) -> Option<Vec<u32>> {
    if *q + 4 > o.len() {
        return None;
    }
    let n = le(o, *q) as usize;
    if n > max || *q + 4 + 4 * n > o.len() {
        return None;
    }
    let a = (0..n).map(|i| le(o, *q + 4 + 4 * i)).collect();
    *q += 4 + 4 * n;
    Some(a)
}

fn shape(o: &[u8], p: usize, n: usize, nested: bool) -> Option<(Vec<(usize, usize, usize)>, usize)> {
    let mut q = p + 4;
    if q + 4 * n > o.len() {
        return None;
    }
    let plats: Vec<u32> = (0..n).map(|i| le(o, q + 4 * i)).collect();
    if plats.iter().any(|&x| x > 64) {
        return None;
    }
    q += 4 * n;
    let mut arrays: Vec<Vec<Vec<u32>>> = Vec::new();
    for a in 0..3 {
        let mut cur = Vec::new();
        if nested {
            if q + 4 > o.len() || le(o, q) as usize != n {
                return None;
            }
            q += 4;
            for i in 0..n {
                let x = u_array(o, &mut q, 16)?;
                if x.is_empty() || (a > 0 && x.len() != arrays[0][i].len()) {
                    return None;
                }
                cur.push(x);
            }
        } else {
            let x = u_array(o, &mut q, n)?;
            if x.len() != n {
                return None;
            }
            cur.extend(x.into_iter().map(|v| vec![v]));
        }
        arrays.push(cur);
    }
    if q + 4 > o.len() {
        return None;
    }
    let len = le(o, q) as usize;
    q += 4;
    if len == 0 || q + len > o.len() {
        return None;
    }
    let mut segs = Vec::new();
    for i in 0..n {
        for j in 0..arrays[0][i].len() {
            let (off, cl, dl) = (arrays[0][i][j] as usize, arrays[1][i][j] as usize, arrays[2][i][j] as usize);
            if off + cl > len || dl > 256 << 20 {
                return None;
            }
            if plats[i] == D3D11_PLATFORM && cl > 0 {
                segs.push((off, cl, dl));
            }
        }
    }
    Some((segs, q))
}

fn decode(o: &[u8], segs: &[(usize, usize, usize)], blob_at: usize) -> Option<Vec<Vec<u8>>> {
    if segs.is_empty() {
        return None;
    }
    segs.iter()
        .map(|&(off, cl, dl)| {
            let src = &o[blob_at + off..blob_at + off + cl];
            if cl == dl {
                return Some(src.to_vec());
            }
            let mut dst = vec![0u8; dl];
            (lz4_flex::block::decompress_into(src, &mut dst).ok()? == dl).then_some(dst)
        })
        .collect()
}

/// BuildSettings' last field m_GraphicsAPIs (2 D3D11, 17 OpenGL core, 18 D3D12, 21 Vulkan), the list the player tries.
pub fn graphics_apis(bs: &[u8]) -> Option<Vec<u32>> {
    (1..=8usize).find_map(|n| {
        if 4 * (n + 1) > bs.len() {
            return None;
        }
        let at = bs.len() - 4 * (n + 1);
        if le(bs, at) as usize != n {
            return None;
        }
        let apis: Vec<u32> = (0..n).map(|i| le(bs, at + 4 + 4 * i)).collect();
        apis.iter().all(|a| matches!(a, 2 | 17 | 18 | 21)).then_some(apis)
    })
}

// ---- sources ----------------------------------------------------------------------------------------------------------

/// One serialized file, loose or a node of a bundle.
pub struct Source {
    pub path: PathBuf,
    pub file: SerializedFile,
    read: Box<dyn Fn(u64, usize) -> Vec<u8>>,
}

impl Source {
    pub fn read(&self, off: u64, count: usize) -> Vec<u8> {
        (self.read)(off, count)
    }
}

const MANAGERS: [&str; 3] = ["globalgamemanagers", "globalgamemanagers.assets", "mainData"];

fn is_resource(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".ress") || l.ends_with(".resource")
}

fn loose(path: &Path) -> Option<Source> {
    let f = Rc::new(File::open(path).ok()?);
    let len = f.metadata().ok()?.len();
    let fr = f.clone();
    let read: Box<dyn Fn(u64, usize) -> Vec<u8>> = Box::new(move |off, count| {
        let n = (count as u64).min(len.saturating_sub(off)) as usize;
        let mut b = vec![0u8; n];
        if fr.read_exact_at(&mut b, off).is_err() {
            b.clear();
        }
        b
    });
    let file = read_serialized(&*read, len)?;
    Some(Source { path: path.to_path_buf(), file, read })
}

/// Calls `f` with every serialized file under the data folder, loose or inside a UnityFS bundle (the file's first bytes
/// decide, not its name). `only_managers`: globalgamemanagers(.assets) / mainData only (loose or in data.unity3d).
pub fn for_each_source(data: &Path, only_managers: bool, f: &mut dyn FnMut(&Source)) {
    let mut paths: Vec<PathBuf> = if only_managers {
        crate::util::files_in(data)
    } else {
        crate::util::files_recursive_depth(data, 8)
    };
    paths.retain(|p| !is_resource(&crate::util::file_name(p)));
    if only_managers {
        paths.retain(|p| {
            let n = crate::util::file_name(p);
            MANAGERS.iter().chain(["data.unity3d"].iter()).any(|m| m.eq_ignore_ascii_case(&n))
        });
    }
    paths.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    for path in paths {
        if let Some(s) = loose(&path) {
            f(&s);
            continue;
        }
        let Some(b) = Bundle::open(&path) else { continue };
        let b = Rc::new(b);
        for n in b.nodes.clone() {
            if is_resource(&n.path) || (only_managers && !MANAGERS.iter().any(|m| m.eq_ignore_ascii_case(&n.path))) {
                continue;
            }
            let (bb, node) = (b.clone(), n.clone());
            let read: Box<dyn Fn(u64, usize) -> Vec<u8>> =
                Box::new(move |off, count| bb.read(node.offset + off, (count as u64).min(node.size.saturating_sub(off)) as usize));
            if let Some(file) = read_serialized(&*read, n.size) {
                f(&Source { path: path.clone(), file, read });
            }
        }
    }
}

/// The folder holding UnityPlayer.dll and its *_Data folder: the exe's folder, the install folder, or one level below.
pub fn find(install: &str, exe: &str) -> Option<(PathBuf, PathBuf)> {
    let mut dirs: Vec<PathBuf> = vec![Path::new(exe).parent().map(Path::to_path_buf).unwrap_or_default(), PathBuf::from(install)];
    dirs.extend(crate::util::subdirs(Path::new(install)));
    let own = format!("{}_Data", Path::new(exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    for dir in dirs.into_iter().filter(|d| d.join("UnityPlayer.dll").is_file()) {
        let mut datas: Vec<PathBuf> = crate::util::subdirs(&dir).into_iter().filter(|d| crate::util::file_name(d).ends_with("_Data")).collect();
        datas.sort_by_key(|d| !crate::util::file_name(d).eq_ignore_ascii_case(&own));
        if let Some(data) = datas.into_iter().find(|d| d.join("globalgamemanagers").is_file() || d.join("data.unity3d").is_file()) {
            return Some((dir, data));
        }
    }
    None
}

/// The API the last run used (Player.log in the Proton prefix), else the first of the build's list, else ambiguous.
pub fn graphics_api(data: &Path, prefix: Option<&str>) -> String {
    if let Some(api) = last_run(data, prefix) {
        return format!("{api} (last run)");
    }
    let mut apis = None;
    for_each_source(data, true, &mut |s| {
        if apis.is_none() {
            if let Some(o) = s.file.objects.iter().find(|o| o.class_id == BUILD_SETTINGS_CLASS) {
                apis = graphics_apis(&s.read(o.start, o.size as usize));
            }
        }
    });
    match apis.as_deref() {
        Some([2, ..]) => "D3D11".into(),
        Some([18, ..]) => "D3D12".into(),
        Some([21, ..]) => "Vulkan".into(),
        Some(_) => "OpenGL".into(),
        None => "D3D11 or D3D12".into(),
    }
}

/// <prefix>/drive_c/users/steamuser/AppData/LocalLow/<company>/<product>/Player.log (company and product from app.info).
fn last_run(data: &Path, prefix: Option<&str>) -> Option<&'static str> {
    let info = std::fs::read_to_string(data.join("app.info")).ok()?;
    let mut lines = info.lines();
    let (company, product) = (lines.next()?, lines.next()?);
    let pfx = PathBuf::from(prefix?);
    let users = pfx.join("drive_c/users");
    let log = crate::util::subdirs(&users).into_iter().map(|u| u.join("AppData/LocalLow").join(company).join(product).join("Player.log")).find(|p| p.is_file())?;
    let mut head = vec![0u8; 256 << 10];
    let n = std::io::Read::read(&mut File::open(log).ok()?, &mut head).ok()?;
    let text = String::from_utf8_lossy(&head[..n]);
    let re = regex::Regex::new(r"(?m)^\s*Version:\s+(Direct3D 11|Direct3D 12|Vulkan)").unwrap();
    Some(match re.captures(&text)?.get(1)?.as_str() {
        "Direct3D 11" => "D3D11",
        "Direct3D 12" => "D3D12",
        _ => "Vulkan",
    })
}

/// The DXBC containers of a Shader (its d3d11 blob, decompressed) or ComputeShader (raw) object.
fn programs(obj: &[u8], class: i32) -> Vec<Vec<u8>> {
    if class == COMPUTE_SHADER_CLASS {
        return dxbc::containers(obj).into_iter().map(<[u8]>::to_vec).collect();
    }
    shader_blob(obj).unwrap_or_default().iter().flat_map(|b| dxbc::containers(b).into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>()).collect()
}

/// What an index of the game's D3D11 shaders found.
#[derive(Default, Debug)]
pub struct D3D11Index {
    /// sha1 -> (stage, container bytes)
    pub shaders: HashMap<String, (u32, Vec<u8>)>,
    pub files: usize,
    pub shader_objects: usize,
}

/// Every SM 4/5 DXBC program of the game's Shader and ComputeShader objects.
pub fn index_d3d11(data: &Path, cancel: &crate::proton::CancelToken) -> D3D11Index {
    let mut ix = D3D11Index::default();
    for_each_source(data, false, &mut |s| {
        if cancel.is_cancelled() {
            return;
        }
        ix.files += 1;
        for o in s.file.objects.iter().filter(|o| o.class_id == SHADER_CLASS || o.class_id == COMPUTE_SHADER_CLASS) {
            ix.shader_objects += 1;
            if o.size > 512 << 20 {
                continue;
            }
            for c in programs(&s.read(o.start, o.size as usize), o.class_id) {
                if let Some(stage) = dxbc::is_d3d11(&c) {
                    let sha = hex::encode(Sha1::digest(&c));
                    ix.shaders.entry(sha).or_insert((stage, c));
                }
            }
        }
    });
    ix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_lz4_shader_blob_by_shape() {
        let ps = dxbc::test_container(0, 5, 0);
        let mut payload = ps.clone();
        payload.extend(dxbc::test_container(1, 5, 0));
        let comp = lz4_flex::block::compress(&payload);
        // flat (pre-2019.3) shape: platforms [d3d11], offsets [0], compressed [len], decompressed [len], blob
        let mut obj = vec![0xAAu8; 12];
        for v in [1u32, D3D11_PLATFORM, 1, 0, 1, comp.len() as u32, 1, payload.len() as u32, comp.len() as u32] {
            obj.extend(v.to_le_bytes());
        }
        obj.extend(&comp);
        let segs = shader_blob(&obj).expect("blob found");
        assert_eq!(segs, vec![payload.clone()]);
        let progs = programs(&obj, SHADER_CLASS);
        assert_eq!(progs.len(), 2);
        assert_eq!(dxbc::is_d3d11(&progs[1]), Some(1));
    }

    #[test]
    fn decompresses_lz4_and_lzma() {
        let data = b"hello unity hello unity hello unity".repeat(20);
        let lz4 = lz4_flex::block::compress(&data);
        assert_eq!(decompress(&lz4, data.len(), 2).unwrap(), data);
        let mut lzma = Vec::new();
        lzma_rs::lzma_compress(&mut std::io::Cursor::new(&data), &mut lzma).unwrap();
        // Unity keeps the 5 property bytes and drops the 8-byte size of the .lzma header
        let raw: Vec<u8> = lzma[..5].iter().chain(&lzma[13..]).copied().collect();
        assert_eq!(decompress(&raw, data.len(), 1).unwrap(), data);
    }

    #[test]
    fn graphics_api_list() {
        let mut bs = vec![0u8; 40];
        for v in [2u32, 2, 18] {
            bs.extend(v.to_le_bytes()); // count 2, then D3D11, D3D12
        }
        assert_eq!(graphics_apis(&bs), Some(vec![2, 18]));
    }
}
