//! DXBC/DXIL container checks for carving game files (SCSKiller.Core Carved/Dxbc.cs). Layout: "DXBC", 16-byte checksum,
//! u32 1, u32 size, u32 chunk count, u32 chunk offsets; each chunk = FourCC, u32 length, data.

pub const MAX_SIZE: usize = 64 << 20;

fn u(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Container size from a 32-byte header, or 0 when the header can't start a container.
pub fn header_size(h: &[u8]) -> usize {
    if h.len() < 32 || &h[..4] != b"DXBC" {
        return 0;
    }
    let (ver, size, n) = (u(h, 20), u(h, 24) as usize, u(h, 28) as usize);
    if ver == 1 && size > 32 && size <= MAX_SIZE && (1..=64).contains(&n) && 32 + 4 * n <= size {
        size
    } else {
        0
    }
}

/// The whole container (exactly its size) is well formed.
pub fn valid(c: &[u8]) -> bool {
    if header_size(c) != c.len() {
        return false;
    }
    for k in 0..u(c, 28) as usize {
        let co = u(c, 32 + 4 * k) as usize;
        if co + 8 > c.len() || !c[co..co + 4].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
            return false;
        }
        if co + 8 + u(c, co + 4) as usize > c.len() {
            return false;
        }
    }
    true
}

/// Every valid container in the bytes, in order; the bytes inside one aren't searched.
pub fn containers(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(k) = b[i..].windows(4).position(|w| w == b"DXBC") {
        let at = i + k;
        let size = header_size(&b[at..]);
        if size > 0 && at + size <= b.len() && valid(&b[at..at + size]) {
            out.push(&b[at..at + size]);
            i = at + size;
        } else {
            i = at + 1;
        }
    }
    out
}

/// Data of the first chunk named `fourcc` (valid containers only), None if there is none.
pub fn part<'a>(c: &'a [u8], fourcc: &[u8; 4]) -> Option<&'a [u8]> {
    (0..u(c, 28) as usize).find_map(|k| {
        let co = u(c, 32 + 4 * k) as usize;
        (&c[co..co + 4] == fourcc).then(|| &c[co + 8..co + 8 + u(c, co + 4) as usize])
    })
}

/// A DXBC (SM 4/5) program's stage in SCSKiller's numbering (1 VS, 2 PS, 3 DS, 4 HS, 5 GS, 6 CS) and its shader model
/// (major, minor), from the SHEX/SHDR version token; None for DXIL or a container without a program.
pub fn d3d11_program(c: &[u8]) -> Option<(u32, u32, u32)> {
    let p = part(c, b"SHEX").or_else(|| part(c, b"SHDR")).filter(|p| p.len() >= 4)?;
    let tok = u(p, 0);
    let stage = match tok >> 16 {
        0 => 2,
        1 => 1,
        2 => 5,
        3 => 4,
        4 => 3,
        5 => 6,
        _ => return None,
    };
    Some((stage, (tok >> 4) & 0xF, tok & 0xF))
}

/// What the D3D11 warm compiles (Planner.IsD3D11): a DXBC program of SM 4_0, 4_1 or 5_0.
pub fn is_d3d11(c: &[u8]) -> Option<u32> {
    let (stage, major, minor) = d3d11_program(c)?;
    matches!((major, minor), (4, 0) | (4, 1) | (5, 0)).then_some(stage)
}

#[cfg(test)]
pub fn test_container(stage_kind: u32, major: u32, minor: u32) -> Vec<u8> {
    // one SHEX chunk holding just the version token
    let mut c = b"DXBC".to_vec();
    c.extend([0u8; 16]);
    c.extend(1u32.to_le_bytes());
    let size = 32 + 4 + 8 + 4;
    c.extend((size as u32).to_le_bytes());
    c.extend(1u32.to_le_bytes());
    c.extend(36u32.to_le_bytes());
    c.extend(b"SHEX");
    c.extend(4u32.to_le_bytes());
    c.extend(((stage_kind << 16) | (major << 4) | minor).to_le_bytes());
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carves_and_reads_stage() {
        let ps = test_container(0, 5, 0);
        let vs = test_container(1, 4, 0);
        let mut blob = b"junk DXBC not a header ".to_vec();
        blob.extend(&ps);
        blob.extend(b"xx");
        blob.extend(&vs);
        let found = containers(&blob);
        assert_eq!(found.len(), 2);
        assert_eq!(is_d3d11(found[0]), Some(2));
        assert_eq!(is_d3d11(found[1]), Some(1));
        assert_eq!(is_d3d11(&test_container(1, 5, 1)), None); // SM 5.1 is D3D12 only
    }
}
