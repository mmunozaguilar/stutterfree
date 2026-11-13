//! D3D12 planning pieces (SCSKiller.Core Planning/RootSig.cs + PsoDb.cs): a shader's stage, Unreal's stock root
//! signatures rebuilt from per-stage resource counts, serialized as the runtime does (RTS0 in a checksummed DXBC
//! container), and the proxy db's compute record.

use crate::dxbc;

/// SCSKiller's stage numbering (as in the proxy's records).
pub const VS: u32 = 1;
pub const PS: u32 = 2;
pub const DS: u32 = 3;
pub const HS: u32 = 4;
pub const GS: u32 = 5;
pub const CS: u32 = 6;
pub const AS: u32 = 7;
pub const MS: u32 = 8;

/// A DXBC or DXIL program's stage (above numbering); None for libraries and containers without a program.
pub fn stage(c: &[u8]) -> Option<u32> {
    let p = dxbc::part(c, b"DXIL").or_else(|| dxbc::part(c, b"SHEX")).or_else(|| dxbc::part(c, b"SHDR")).filter(|p| p.len() >= 4)?;
    match u32::from_le_bytes(p[..4].try_into().unwrap()) >> 16 {
        0 => Some(PS),
        1 => Some(VS),
        2 => Some(GS),
        3 => Some(HS),
        4 => Some(DS),
        5 => Some(CS),
        13 => Some(MS),
        14 => Some(AS),
        _ => None,
    }
}

/// Unreal's per-shader resource counts (FShaderCodePackedResourceCounts) and the UE5 flags its root signatures depend on.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Counts {
    pub sampler: u32,
    pub srv: u32,
    pub cb: u32,
    pub uav: u32,
    pub flags: u32,
}

pub const BINDLESS_RESOURCES: u32 = 1;
pub const BINDLESS_SAMPLERS: u32 = 2;
pub const ROOT_CONSTANTS: u32 = 4;
pub const DIAGNOSTIC_BUFFER: u32 = 8;
pub const AMD_INTRINSICS: u32 = 16;
pub const NV_INTRINSICS: u32 = 32;

/// One construction rule per range of Unreal versions (RootSig.Rule; the forks' rules aren't ported).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Rule {
    Ue420,
    Ue421,
    Ue422,
    Ue425,
    Ue426,
    Ue50,
    Ue51,
    Ue54,
    Ue55,
    Ue58,
}

pub fn rule_for(major: u32, minor: u32) -> Option<Rule> {
    Some(match (major, minor) {
        (4, 20) => Rule::Ue420,
        (4, 21) => Rule::Ue421,
        (4, 22..=24) => Rule::Ue422,
        (4, 25) => Rule::Ue425,
        (4, 26 | 27) => Rule::Ue426,
        (5, 0) => Rule::Ue50,
        (5, 1..=3) => Rule::Ue51,
        (5, 4) => Rule::Ue54,
        (5, 5..=7) => Rule::Ue55,
        (5, _) | (6, 0) => Rule::Ue58,
        _ => return None,
    })
}

/// A 1.1 root signature: flags + rows. A row is a table (0, vis, then per range: type, count, base, space, flags), root
/// constants (1, vis, register, space, count) or a root CBV/UAV (2/4, vis, register, space, flags). A table's single range
/// sits at OFFSET_APPEND unless the row carries an explicit offset as an 8th value.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Desc {
    pub flags: u32,
    pub rows: Vec<Vec<u32>>,
}

fn vis(s: u32) -> u32 {
    match s {
        VS => 1,
        HS => 2,
        DS => 3,
        GS => 4,
        PS => 5,
        AS => 6,
        MS => 7,
        _ => 0,
    }
}

fn deny_bit(s: u32) -> u32 {
    match s {
        VS => 0x2,
        HS => 0x4,
        DS => 0x8,
        GS => 0x10,
        PS => 0x20,
        AS => 0x100,
        _ => 0x200,
    }
}

/// D3D12RHI.h's MAX_SRVS.
fn max_srvs(r: Rule) -> u32 {
    match r {
        Rule::Ue420 => 32,
        Rule::Ue421 | Rule::Ue422 => 48,
        _ => 64,
    }
}

/// The MAX_SRVS a game was built with when its shaders need more than the rule's (RootSig.MaxSrvsFor); 0 = the rule's.
/// shortcut: Avalanche's 4.27 fork (128, spotted by bindless SRV spaces) isn't detected: shader bindings aren't read yet.
pub fn max_srvs_for(r: Rule, counts: impl Iterator<Item = Counts>) -> u32 {
    if r > Rule::Ue422 {
        return 0;
    }
    let need = counts.map(|c| c.srv).max().unwrap_or(0);
    if need <= max_srvs(r) { 0 } else if need <= 48 { 48 } else { 64 }
}

/// InitShaderRegisterCounts at tier 3 (RootSig.Quantize): (srv, sampler, uav, cb) table sizes.
fn quantize(r: Rule, s: u32, c: Counts, max_srv: u32) -> (u32, u32, u32, u32) {
    let uavs = s == PS || s == CS || (s == VS && r >= Rule::Ue55) || ((s == MS || s == AS) && r >= Rule::Ue58);
    let srv = if max_srv > 0 { max_srv } else { max_srvs(r) };
    let sampler = if r >= Rule::Ue54 { 32 } else { 16 };
    let cb = c.cb.min(16);
    if r <= Rule::Ue50 {
        (srv, sampler, if uavs { 16 } else { 0 }, cb)
    } else {
        (if c.srv > 0 { srv } else { 0 }, if c.sampler > 0 { sampler } else { 0 }, if c.uav > 0 && uavs { 16 } else { 0 }, cb)
    }
}

/// Stock UE's FD3D12RootSignatureDesc (RootSig.BuildStock): per stage an SRV, sampler and UAV table, then each stage's
/// root CBVs, then (5.x) the AGS UAV, root constants, NV extension table and diagnostic UAV; flags: bindless heaps,
/// input layout, a deny for every stage with nothing bound. `stages`: (stage, counts); `vs_reads_vertices` matters
/// to 4.20/4.21 only. shortcut: the UE4 forks' bindless SRV tables aren't built (they need the shaders' bindings).
pub fn build_stock(r: Rule, stages: &[(u32, Counts)], mesh_tier: bool, max_srv: u32, vs_reads_vertices: bool) -> Desc {
    let all: &[u32] = if r >= Rule::Ue50 { &[PS, VS, GS, MS, AS] } else { &[PS, VS, GS, HS, DS] };
    let get = |s: u32| stages.iter().find(|(x, _)| *x == s).map(|(_, c)| *c);
    let order: Vec<u32> = if get(CS).is_some() { vec![CS] } else { all.iter().copied().filter(|s| get(*s).is_some()).collect() };
    let q: Vec<(u32, (u32, u32, u32, u32))> = order.iter().map(|&s| (s, quantize(r, s, get(s).unwrap(), max_srv))).collect();
    let mut rows: Vec<Vec<u32>> = Vec::new();
    for &(s, (srv, sampler, uav, _)) in &q {
        if srv > 0 {
            rows.push(vec![0, vis(s), 0, srv, 0, 0, 5]); // DESCRIPTORS_VOLATILE | DATA_STATIC_WHILE_SET_AT_EXECUTE
        }
        if sampler > 0 {
            rows.push(vec![0, vis(s), 3, sampler, 0, 0, 1]); // DESCRIPTORS_VOLATILE
        }
        if uav > 0 {
            rows.push(vec![0, vis(s), 1, uav, 0, 0, 3]); // DESCRIPTORS_VOLATILE | DATA_VOLATILE
        }
    }
    for &(s, (_, _, _, cb)) in &q {
        for reg in 0..cb {
            rows.push(vec![2, vis(s), reg, 0, 8]); // DATA_STATIC
        }
    }
    let used = stages.iter().fold(0, |f, (_, c)| f | c.flags);
    let mut flags = match get(VS) {
        None => 0,
        Some(_) if matches!(r, Rule::Ue420 | Rule::Ue421) && !vs_reads_vertices => 0,
        Some(_) => 1, // ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    if r >= Rule::Ue50 {
        if used & AMD_INTRINSICS != 0 {
            rows.push(vec![4, 0, 0, 0x7FFF0ADE, 2]);
        }
        if r >= Rule::Ue54 && used & ROOT_CONSTANTS != 0 {
            rows.push(vec![1, 0, 0, 3, 4]);
        }
        if r >= Rule::Ue58 && used & NV_INTRINSICS != 0 {
            rows.push(vec![0, 0, 1, 1, 0, 1001, 0, 0]);
        }
        if used & DIAGNOSTIC_BUFFER != 0 {
            rows.push(vec![4, 0, 0, 999, 2]);
        }
        if used & BINDLESS_RESOURCES != 0 {
            flags |= 0x400;
        }
        if used & BINDLESS_SAMPLERS != 0 {
            flags |= 0x800;
        }
    }
    for &s in all {
        let empty = q.iter().find(|(x, _)| *x == s).map(|(_, t)| *t == (0, 0, 0, 0)).unwrap_or(true);
        if empty && (mesh_tier || (s != MS && s != AS)) {
            flags |= deny_bit(s);
        }
    }
    Desc { flags, rows }
}

/// UE's six static samplers (point/bilinear/trilinear x wrap/clamp) from register `reg` in `space`, 52 bytes each.
pub fn ue_samplers(r: Rule) -> Vec<u8> {
    let (mut reg, space) = match r {
        Rule::Ue420 | Rule::Ue421 | Rule::Ue422 => return Vec::new(),
        Rule::Ue425 => (1000, 0),
        _ => (0, 1000),
    };
    let mut out = Vec::new();
    for filter in [0u32, 0x14, 0x15] {
        for addr in [1u32, 3] {
            for v in [filter, addr, addr, addr, 0, 1, 1, 0, 0, f32::MAX.to_bits(), reg, space, 0] {
                out.extend(v.to_le_bytes());
            }
            reg += 1;
        }
    }
    out
}

/// The serialized root signature: a version 1.1 RTS0 (header, parameters, their tables / descriptors, static samplers)
/// in a one-part DXBC container with its checksum.
pub fn serialize(d: &Desc, samplers: &[u8]) -> Vec<u8> {
    let at = 24 + 12 * d.rows.len();
    let mut params: Vec<u32> = Vec::new();
    let mut payload: Vec<u32> = Vec::new();
    for r in &d.rows {
        params.extend([r[0], r[1], (at + 4 * payload.len()) as u32]);
        match r[0] {
            0 => {
                let n = (r.len() - 2) / 5;
                let ranges_at = (at + 4 * payload.len() + 8) as u32;
                payload.extend([n as u32, ranges_at]);
                for j in 0..n {
                    let g = &r[2 + 5 * j..7 + 5 * j];
                    let offset = if n == 1 { r.get(7).copied().unwrap_or(u32::MAX) } else { 0 };
                    payload.extend([g[0], g[1], g[2], g[3], g[4], offset]);
                }
            }
            _ => payload.extend([r[2], r[3], r[4]]),
        }
    }
    let samplers_at = at + 4 * payload.len();
    let mut rts0: Vec<u8> = Vec::new();
    for v in [2u32, d.rows.len() as u32, 24, (samplers.len() / 52) as u32, samplers_at as u32, d.flags].iter().chain(&params).chain(&payload) {
        rts0.extend(v.to_le_bytes());
    }
    rts0.extend(samplers);
    wrap_dxbc(b"RTS0", &rts0)
}

/// The root signature a FromSoftware game creates from the 1.1 (or 1.2) one its shader carries (RTS0): the same
/// parameters re-serialized at version 1.0 (RootSig.AsVersion10): range and descriptor flags dropped, OFFSET_APPEND
/// resolved to explicit offsets. None without an RTS0 part, or when it has no 1.0 form (a 1.2 static sampler with flags).
pub fn as_version10(container: &[u8]) -> Option<Vec<u8>> {
    let b = dxbc::part(container, b"RTS0")?;
    let w = |o: u32| u(b, o as usize);
    let (ver, n, at, ns, so, flags) = (w(0)?, w(4)?, w(8)?, w(12)?, w(16)?, w(20)?);
    let range_size = if ver >= 2 { 24 } else { 20 };
    let params_at = 24 + 12 * n as usize;
    let (mut params, mut payload): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new());
    for i in 0..n {
        let (ty, vis, p) = (w(at + 12 * i)?, w(at + 12 * i + 4)?, w(at + 12 * i + 8)?);
        params.extend([ty, vis, (params_at + 4 * payload.len()) as u32]);
        match ty {
            0 => {
                let count = w(p)?;
                payload.extend([count, (params_at + 4 * payload.len() + 8) as u32]);
                let mut offset = 0u32;
                for r in 0..count {
                    let q = w(p + 4)? + range_size * r;
                    let (num, own) = (w(q + 4)?, w(q + range_size - 4)?);
                    let start = if own == u32::MAX { offset } else { own };
                    payload.extend([w(q)?, num, w(q + 8)?, w(q + 12)?, start]);
                    offset = if num == u32::MAX || start == u32::MAX { u32::MAX } else { start + num };
                }
            }
            1 => payload.extend([w(p)?, w(p + 4)?, w(p + 8)?]),
            _ => payload.extend([w(p)?, w(p + 4)?]), // a root descriptor: register, space (1.1 flags dropped)
        }
    }
    let mut samplers: Vec<u8> = Vec::new();
    let sampler_size = if ver == 3 { 56 } else { 52 };
    for k in 0..ns as usize {
        let q = so as usize + sampler_size * k;
        if ver == 3 && u(b, q + 52)? != 0 {
            return None; // a 1.2 static sampler with flags has no 1.0 form
        }
        samplers.extend(b.get(q..q + 52)?);
    }
    let mut rts0 = Vec::new();
    for v in [1u32, n, 24, ns, (params_at + 4 * payload.len()) as u32, flags].iter().chain(&params).chain(&payload) {
        rts0.extend(v.to_le_bytes());
    }
    rts0.extend(samplers);
    Some(wrap_dxbc(b"RTS0", &rts0))
}

/// A one-part DXBC container with its checksum.
pub fn wrap_dxbc(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let total = 32 + 4 + 8 + data.len();
    let mut b = Vec::with_capacity(total);
    b.extend(b"DXBC");
    b.extend([0u8; 16]);
    for v in [1u32, total as u32, 1, 36] {
        b.extend(v.to_le_bytes());
    }
    b.extend(fourcc);
    b.extend((data.len() as u32).to_le_bytes());
    b.extend(data);
    let sum = checksum(&b);
    b[4..20].copy_from_slice(&sum);
    b
}

/// The DXBC container checksum: MD5 over everything after it, with the length padding moved (vkd3d's
/// vkd3d_compute_dxbc_checksum, d3dcompiler's).
pub fn checksum(c: &[u8]) -> [u8; 16] {
    let data = &c[20..];
    let bits = (data.len() as u32).wrapping_mul(8);
    let mut st = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    let full = data.len() / 64 * 64;
    for blk in data[..full].chunks_exact(64) {
        md5_block(&mut st, blk.try_into().unwrap());
    }
    let rest = &data[full..];
    let mut last = [0u8; 64];
    if rest.len() >= 56 {
        let mut blk = [0u8; 64];
        blk[..rest.len()].copy_from_slice(rest);
        blk[rest.len()] = 0x80;
        md5_block(&mut st, &blk);
    } else {
        last[4..4 + rest.len()].copy_from_slice(rest);
        last[4 + rest.len()] = 0x80;
    }
    last[..4].copy_from_slice(&bits.to_le_bytes());
    last[60..].copy_from_slice(&((bits >> 2) | 1).to_le_bytes());
    md5_block(&mut st, &last);
    let mut out = [0u8; 16];
    for (i, w) in st.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

fn md5_block(st: &mut [u32; 4], blk: &[u8; 64]) {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let m: Vec<u32> = blk.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let [mut a, mut b, mut c, mut d] = *st;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let k = ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32;
        let t = d;
        d = c;
        c = b;
        b = b.wrapping_add(a.wrapping_add(f).wrapping_add(k).wrapping_add(m[g]).rotate_left(S[(i / 16) * 4 + i % 4]));
        a = t;
    }
    for (s, v) in st.iter_mut().zip([a, b, c, d]) {
        *s = s.wrapping_add(v);
    }
}

/// One signature element (ShaderContainer.Parse): semantic (upper case), index, register, component mask, system value,
/// component type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigElem {
    pub name: String,
    pub index: u32,
    pub register: i32,
    pub mask: u8,
    pub sys: u32,
    pub comp: u32,
}

/// A resource a shader binds: class (0 SRV, 1 UAV, 2 CBV, 3 sampler, as root signature range types), space, first
/// register, count (-1 = unbounded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub class: u32,
    pub space: u32,
    pub lower: u32,
    pub count: i64,
}

/// What pairing and root signature checks read from a container.
#[derive(Clone, Debug, Default)]
pub struct Reflection {
    pub inputs: Vec<SigElem>,
    pub outputs: Vec<SigElem>,
    pub bindings: Vec<Binding>,
    /// a geometry shader's input primitive (D3D_PRIMITIVE), 0 when unknown or not a GS
    pub gs_input: u32,
}

fn u(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn signature(d: &[u8], size: usize) -> Option<Vec<SigElem>> {
    let mut out = Vec::new();
    for e in 0..u(d, 0)? as usize {
        let o = u(d, 4)? as usize + e * size;
        let b = if size == 24 { o.checked_sub(4)? } else { o }; // 24-byte elements have no leading stream field
        let name_at = u(d, b + 4)? as usize;
        let name = d.get(name_at..)?.split(|&x| x == 0).next()?;
        out.push(SigElem {
            name: String::from_utf8_lossy(name).to_ascii_uppercase(),
            index: u(d, b + 8)?,
            sys: u(d, b + 12)?,
            comp: u(d, b + 16)?,
            register: u(d, b + 20)? as i32,
            mask: *d.get(b + 24)?,
        });
    }
    Some(out)
}

/// Signatures (ISGN/OSGN/OSG5/ISG1/OSG1) and, for DXIL, bindings (PSV0). shortcut: DXBC bindings (SHEX dcl_*) aren't
/// read, so a DXBC shader under D3D12 skips the coverage check; mesh shaders' per-primitive outputs (PSG1) aren't read.
pub fn reflect(c: &[u8]) -> Reflection {
    let mut r = Reflection::default();
    for (k, size, input) in [(b"ISGN", 24, true), (b"ISG1", 32, true), (b"OSGN", 24, false), (b"OSG5", 28, false), (b"OSG1", 32, false)] {
        if let Some(sig) = dxbc::part(c, k).and_then(|d| signature(d, size)) {
            if input { r.inputs = sig } else { r.outputs = sig }
        }
    }
    if let Some(psv) = dxbc::part(c, b"PSV0") {
        // RuntimeInfo starts with the stage union: GSInfo.InputPrimitive first
        if dxbc::part(c, b"DXIL").and_then(|d| u(d, 0)).map(|v| v >> 16 == 2).unwrap_or(false) && u(psv, 0).unwrap_or(0) >= 4 {
            r.gs_input = u(psv, 4).unwrap_or(0);
        }
        // u32 RuntimeInfoSize, RuntimeInfo, u32 ResourceCount, u32 BindInfoSize, PSVResourceBindInfo[] (type, space, lower, upper)
        let o = 4 + u(psv, 0).unwrap_or(0) as usize;
        let n = u(psv, o).unwrap_or(0) as usize;
        let stride = if n > 0 { u(psv, o + 4).unwrap_or(0) as usize } else { 0 };
        for i in 0..if stride >= 16 { n } else { 0 } {
            let at = o + 8 + i * stride;
            let (Some(t), Some(space), Some(lo), Some(hi)) = (u(psv, at), u(psv, at + 4), u(psv, at + 8), u(psv, at + 12)) else { break };
            let class = match t {
                1 => 3,
                2 => 2,
                3..=5 => 0,
                6..=9 => 1,
                _ => continue,
            };
            r.bindings.push(Binding { class, space, lower: lo, count: if hi == u32::MAX { -1 } else { hi as i64 - lo as i64 + 1 } });
        }
    }
    r
}

/// Every binding of the stage's shader is served by the root signature (RootSig.Uncovered): a range of its type and space
/// visible to the stage holds it, and the stage isn't denied. The runtime rejects a pipeline that fails this.
pub fn covers(d: &Desc, samplers: &[u8], stage: u32, bindings: &[Binding]) -> bool {
    if !bindings.is_empty() && d.flags & deny_bit(stage) != 0 && stage != CS {
        return false;
    }
    // (vis, type, base, count, space, table)
    let mut slots: Vec<(u32, u32, u32, u32, u32, bool)> = Vec::new();
    for r in &d.rows {
        match r[0] {
            0 => {
                for g in r[2..2 + (r.len() - 2) / 5 * 5].chunks(5) {
                    slots.push((r[1], g[0], g[2], g[1], g[3], true));
                }
            }
            1 | 2 => slots.push((r[1], 2, r[2], 1, r[3], false)),
            3 => slots.push((r[1], 0, r[2], 1, r[3], false)),
            _ => slots.push((r[1], 1, r[2], 1, r[3], false)),
        }
    }
    for s in samplers.chunks_exact(52) {
        let w = |o: usize| u32::from_le_bytes(s[o..o + 4].try_into().unwrap());
        slots.push((w(48), 3, w(40), 1, w(44), false));
    }
    bindings.iter().all(|b| {
        slots.iter().any(|&(v, t, base, count, space, table)| {
            (v == 0 || v == vis(stage))
                && t == b.class
                && space == b.space
                && base <= b.lower
                && (count == u32::MAX
                    || if b.count >= 0 { b.lower as u64 + b.count as u64 <= base as u64 + count as u64 } else { table && (b.lower as u64) < base as u64 + count as u64 })
        })
    })
}

/// A DXIL shader's required wave lane range ([WaveSize], PSV0 RuntimeInfo0's expected lane counts); None = any.
pub fn wave_lanes(c: &[u8]) -> Option<(u32, u32)> {
    let psv = dxbc::part(c, b"PSV0")?;
    if u(psv, 0)? < 24 {
        return None;
    }
    let (min, max) = (u(psv, 20)?, u(psv, 24)?);
    (!(min == 0 && max == u32::MAX)).then_some((min, max))
}

/// D3D12_PRIMITIVE_TOPOLOGY_TYPE of a GS input primitive (adjacency included); 0 = unknown.
pub fn topology_type(gs_input: u32) -> u32 {
    match gs_input {
        1 => 1,
        2 | 6 => 2,
        3 | 7 => 3,
        _ => 0,
    }
}

/// The last stage before the rasterizer writes SV_Position (or no signature was read): it can draw without a GS.
pub fn rasterizable(r: &Reflection) -> bool {
    r.outputs.is_empty() || r.outputs.iter().any(|o| o.sys == 1)
}

/// Signature identity (Planner.Sig): (semantic, index, register, component type), system values dropped unless asked.
pub fn sig_key(elems: &[SigElem], drop_sys: bool) -> String {
    let mut v: Vec<(&str, u32, i32, u32)> = elems.iter().filter(|e| !drop_sys || e.sys == 0).map(|e| (e.name.as_str(), e.index, e.register, e.comp)).collect();
    v.sort();
    v.iter().map(|(s, i, r, c)| format!("{s}/{i}/{r}/{c}")).collect::<Vec<_>>().join(";")
}

/// `src`'s outputs feed `dst`'s inputs (Planner.Links): every input is written with a superset mask, system values from
/// the previous stage included; a DXBC input reading SV_Position from it must sit in the same register.
pub fn links(src: &Reflection, dst: &Reflection, dst_dxbc: bool) -> bool {
    dst.inputs.iter().all(|i| {
        let o = src.outputs.iter().find(|o| o.name == i.name && o.index == i.index);
        if dst_dxbc && i.sys == 1 && o.map(|o| o.register != i.register).unwrap_or(false) {
            return false;
        }
        if i.sys != 0 && !(2..=5).contains(&i.sys) {
            return true;
        }
        o.map(|o| i.mask & !o.mask == 0).unwrap_or(false)
    })
}

pub const R16G16B16A16_FLOAT: u32 = 10;
pub const D32_FLOAT: u32 = 40;

/// Render targets per pixel shader output (float -> RGBA16F, uint/sint -> RGBA32 UINT/SINT) and D32 when it writes depth.
pub fn targets(ps: &Reflection) -> (Vec<u32>, u32) {
    let t: Vec<&SigElem> = ps.outputs.iter().filter(|o| o.sys == 64 || o.name == "SV_TARGET").collect();
    let mut rt = vec![0u32; t.iter().map(|o| o.index as usize + 1).max().unwrap_or(0)];
    for o in t {
        rt[o.index as usize] = match o.comp {
            1 => 3,
            2 => 4,
            _ => R16G16B16A16_FLOAT,
        };
    }
    (rt, if ps.outputs.iter().any(|o| matches!(o.sys, 65 | 67 | 68)) { D32_FLOAT } else { 0 })
}

/// An input layout element: semantic, index, DXGI format, byte offset (slot 0, per vertex).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutElem {
    pub name: String,
    pub index: u32,
    pub format: u32,
    pub offset: u32,
}

/// The VS's own vertex inputs packed one after another in slot 0 (Planner.VsLayout).
pub fn vs_layout(vs: &Reflection) -> Vec<LayoutElem> {
    let mut ins: Vec<&SigElem> = vs.inputs.iter().filter(|e| e.sys == 0).collect();
    ins.sort_by_key(|e| e.register);
    let mut off = 0;
    ins.into_iter()
        .map(|e| {
            let n = (32 - (e.mask as u32).leading_zeros()).clamp(1, 4) as usize; // components up to the highest mask bit
            let fmts: [u32; 4] = match e.comp {
                1 => [42, 17, 7, 3],
                2 => [43, 18, 8, 4],
                _ => [41, 16, 6, 2],
            };
            let l = LayoutElem { name: e.name.clone(), index: e.index, format: fmts[n - 1], offset: off };
            off += 4 * n as u32;
            l
        })
        .collect()
}

fn put_layout(w: &mut Vec<u8>, layout: &[LayoutElem]) {
    w.extend((layout.len() as u32).to_le_bytes());
    for e in layout {
        w.extend((e.name.len() as u32).to_le_bytes());
        w.extend(e.name.as_bytes());
        for v in [e.index, e.format, 0, e.offset, 0, 0] {
            w.extend(v.to_le_bytes()); // index, format, slot, offset, class, step rate
        }
    }
}

/// A synthesized 'S' stream (PsoDb.Stream): UE's subobject order with neutral state (no blend, cull none, depth only with
/// a DSV, one sample). `stages`: (stage, sha1) of VS / GS / PS.
pub fn stream(rs: &str, stages: &[(u32, &str)], layout: &[LayoutElem], topology: u32, rt: &[u32], dsv: u32) -> Vec<u8> {
    let zero = "0".repeat(40);
    let sh = |s: u32| stages.iter().find(|(x, _)| *x == s).map(|(_, h)| h.to_string()).unwrap_or_else(|| zero.clone());
    let mut w: Vec<u8> = vec![0; 4];
    let mut n = 0u32;
    let mut sub = |w: &mut Vec<u8>, t: u32| {
        n += 1;
        w.extend(t.to_le_bytes());
    };
    let words = |w: &mut Vec<u8>, v: &[u32]| v.iter().for_each(|x| w.extend(x.to_le_bytes()));
    sub(&mut w, 18);
    words(&mut w, &[1]); // node mask
    sub(&mut w, 0);
    w.extend(hex::decode(rs).unwrap());
    sub(&mut w, 12);
    put_layout(&mut w, layout);
    sub(&mut w, 13);
    words(&mut w, &[0]); // IB strip cut off
    sub(&mut w, 14);
    words(&mut w, &[topology]);
    for s in [VS, GS, HS, DS] {
        sub(&mut w, s);
        w.extend(hex::decode(sh(s)).unwrap());
    }
    sub(&mut w, 2);
    w.extend(hex::decode(sh(PS)).unwrap());
    sub(&mut w, 8);
    words(&mut w, &[0, 0]); // no alpha to coverage, no independent blend
    for _ in 0..8 {
        words(&mut w, &[0, 0, 2, 1, 1, 2, 1, 1, 4, 0xF]); // blend off, ONE/ZERO/ADD, logic op off (NOOP), write all
    }
    let depth = (dsv != 0) as u32;
    sub(&mut w, 21);
    words(&mut w, &[depth, depth, 8, 0, 0xFFFF, 1, 1, 1, 8, 1, 1, 1, 8, 0]); // func ALWAYS, no stencil, no depth bounds
    sub(&mut w, 16);
    words(&mut w, &[dsv]);
    sub(&mut w, 10);
    words(&mut w, &[3, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0]); // solid, cull none, depth clip on
    sub(&mut w, 15);
    words(&mut w, &[rt.len() as u32]);
    words(&mut w, &(0..8).map(|i| rt.get(i).copied().unwrap_or(0)).collect::<Vec<_>>());
    sub(&mut w, 17);
    words(&mut w, &[1, 0]); // one sample
    sub(&mut w, 9);
    words(&mut w, &[u32::MAX]); // sample mask
    w[..4].copy_from_slice(&n.to_le_bytes());
    w
}

/// A 'P' item: a template's PSO with another root signature, stages and layout.
pub fn item(template: &str, rs: &str, stages: &[(u32, &str)], layout: &[LayoutElem]) -> Vec<u8> {
    let mut w = hex::decode(template).unwrap();
    w.extend(hex::decode(rs).unwrap());
    let mut st = stages.to_vec();
    st.sort();
    w.extend((st.len() as u32).to_le_bytes());
    for (s, h) in st {
        w.extend(s.to_le_bytes());
        w.extend(hex::decode(h).unwrap());
    }
    put_layout(&mut w, layout);
    w
}

/// A 'C' payload, D3D12_COMPUTE_PIPELINE_STATE_DESC canonical: root sig[20], CS[20], u32 NodeMask, u32 Flags.
pub fn compute_record(rs: &str, cs: &str) -> Vec<u8> {
    let mut p = hex::decode(rs).unwrap();
    p.extend(hex::decode(cs).unwrap());
    p.extend([0u8; 8]);
    p
}

/// A DXBC container with an OSGN / ISGN part of the given elements (name, index, register, mask, sys value) and a
/// SHEX version token of `kind`.
#[cfg(test)]
pub fn test_shader(kind: u32, isgn: &[(&str, u32, u32, u8, u32)], osgn: &[(&str, u32, u32, u8, u32)]) -> Vec<u8> {
    test_shader_with(kind, isgn, osgn, None)
}

/// test_shader with an RTS0 part (a root signature the shader carries).
#[cfg(test)]
pub fn test_shader_with(kind: u32, isgn: &[(&str, u32, u32, u8, u32)], osgn: &[(&str, u32, u32, u8, u32)], rts0: Option<&[u8]>) -> Vec<u8> {
    fn sig(e: &[(&str, u32, u32, u8, u32)]) -> Vec<u8> {
        let mut d: Vec<u8> = Vec::new();
        d.extend((e.len() as u32).to_le_bytes());
        d.extend(8u32.to_le_bytes());
        let names_at = 8 + 24 * e.len();
        let mut names: Vec<u8> = Vec::new();
        for (name, idx, reg, mask, sys) in e {
            for v in [(names_at + names.len()) as u32, *idx, *sys, 3, *reg] {
                d.extend(v.to_le_bytes());
            }
            d.extend([*mask, *mask, 0, 0]);
            names.extend(name.as_bytes());
            names.push(0);
        }
        d.extend(names);
        d
    }
    let mut parts: Vec<(&[u8; 4], Vec<u8>)> = vec![(b"ISGN", sig(isgn)), (b"OSGN", sig(osgn)), (b"SHEX", (kind << 16 | 0x50).to_le_bytes().to_vec())];
    if let Some(r) = rts0 {
        parts.push((b"RTS0", r.to_vec()));
    }
    let head = 32 + 4 * parts.len();
    let mut body: Vec<u8> = Vec::new();
    let mut offs = Vec::new();
    for (k, d) in &parts {
        offs.push((head + body.len()) as u32);
        body.extend(*k);
        body.extend((d.len() as u32).to_le_bytes());
        body.extend(d);
    }
    let mut c = b"DXBC".to_vec();
    c.extend([0u8; 16]);
    for v in [1u32, (head + body.len()) as u32, parts.len() as u32] {
        c.extend(v.to_le_bytes());
    }
    offs.iter().for_each(|o| c.extend(o.to_le_bytes()));
    c.extend(body);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_a_real_container() {
        // the checksum matched FXC's and DXC's on all 24,264 shaders of an Unreal 5.5 game; this pins it
        let w = wrap_dxbc(b"RTS0", &[1, 2, 3]);
        assert!(dxbc::valid(&w) && dxbc::part(&w, b"RTS0") == Some(&[1u8, 2, 3][..]));
        assert_eq!(hex::encode(&w[4..20]), "a9e238e6badd7b43e7285b626866b6b0");
    }

    #[test]
    fn version10_resolves_append_offsets_and_drops_flags() {
        // a 1.1 RTS0: a table of two OFFSET_APPEND ranges (SRV x4, UAV x2), a root CBV b3 with flags, one static sampler
        let m = u32::MAX;
        let words: Vec<u32> = [
            vec![2, 2, 24, 1, 116, 1],                          // version 1.1, 2 params at 24, 1 sampler at 116, flags
            vec![0, 0, 48, 2, 1, 104],                          // table (all stages) at 48; root CBV (VS) at 104
            vec![2, 56],                                        // 2 ranges at 56
            vec![0, 4, 0, 0, 5, m, 1, 2, 0, 0, 3, m],           // SRV x4, UAV x2, both appended
            vec![3, 0, 8],                                      // b3 space 0, DATA_STATIC
        ]
        .concat();
        let mut rts0: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
        rts0.extend(&ue_samplers(Rule::Ue426)[..52]);
        let v10 = as_version10(&wrap_dxbc(b"RTS0", &rts0)).unwrap();
        let b = dxbc::part(&v10, b"RTS0").unwrap();
        let w = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        assert_eq!((w(0), w(4), w(12), w(20)), (1, 2, 1, 1)); // version 1.0, 2 params, 1 sampler, flags kept
        let ranges = w(w(32) as usize + 4) as usize; // param 0's payload: count, ranges offset
        assert_eq!((w(ranges + 16), w(ranges + 20 + 16)), (0, 4)); // 20-byte ranges: SRVs at 0, UAVs right after them
        let cbv = w(44) as usize;
        assert_eq!((w(cbv), w(cbv + 4), w(w(16) as usize + 40)), (3, 0, 0)); // b3 space 0 (no flags word), sampler s0 kept
        assert_eq!(w(16) as usize + 52, b.len()); // nothing after the sampler
        assert!(as_version10(&wrap_dxbc(b"SHEX", &[0; 4])).is_none());
    }

    #[test]
    fn vs_layout_keeps_each_input_width() {
        let vs = reflect(&test_shader(1, &[("POSITION", 0, 0, 0x7, 0), ("TEXCOORD", 0, 1, 0x3, 0), ("SV_VERTEXID", 0, 2, 0x1, 6)], &[]));
        let l = vs_layout(&vs);
        // float3 at 0, float2 at 12; the system value isn't vertex data
        assert_eq!(l.iter().map(|e| (e.format, e.offset)).collect::<Vec<_>>(), vec![(6, 0), (16, 12)]);
    }

    #[test]
    fn wave_size_from_psv0() {
        let psv = |min: u32, max: u32| [24u32, 0, 0, 0, 0, min, max].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        assert_eq!(wave_lanes(&wrap_dxbc(b"PSV0", &psv(16, 16))), Some((16, 16)));
        assert_eq!(wave_lanes(&wrap_dxbc(b"PSV0", &psv(0, u32::MAX))), None); // no [WaveSize]
    }

    #[test]
    fn ue55_compute_root_signature() {
        let c = Counts { srv: 3, uav: 1, cb: 2, sampler: 0, flags: BINDLESS_RESOURCES };
        let d = build_stock(Rule::Ue55, &[(CS, c)], true, 0, false);
        assert_eq!(d.rows, vec![vec![0, 0, 0, 64, 0, 0, 5], vec![0, 0, 1, 16, 0, 0, 3], vec![2, 0, 0, 0, 8], vec![2, 0, 1, 0, 8]]);
        assert_eq!(d.flags, 0x400 | 0x20 | 0x2 | 0x10 | 0x200 | 0x100); // bindless; PS, VS, GS, MS, AS denied
        let b = serialize(&d, &ue_samplers(Rule::Ue55));
        let rts0 = dxbc::part(&b, b"RTS0").unwrap();
        assert_eq!(u32::from_le_bytes(rts0[12..16].try_into().unwrap()), 6); // six static samplers
        assert_eq!(compute_record(&"ab".repeat(20), &"cd".repeat(20)).len(), 48);
    }
}
