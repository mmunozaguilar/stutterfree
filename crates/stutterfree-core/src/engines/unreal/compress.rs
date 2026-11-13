//! Unreal's compression methods (by the name a container lists): Oodle (open-source decoder), Zlib, Gzip, LZ4.

use std::io::Read;

pub fn decompress(method: &str, src: &[u8], size: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; size];
    let ok = match method.to_ascii_lowercase().as_str() {
        "" | "none" => return (src.len() == size).then(|| src.to_vec()),
        "oodle" => oozextract::Extractor::new().read_from_slice(src, &mut out).ok() == Some(size),
        "zlib" => flate2::read::ZlibDecoder::new(src).read_exact(&mut out).is_ok(),
        "gzip" => flate2::read::GzDecoder::new(src).read_exact(&mut out).is_ok(),
        "lz4" => lz4_flex::block::decompress_into(src, &mut out).ok() == Some(size),
        _ => false,
    };
    ok.then_some(out)
}

/// A shader library's per-shader / per-group code, whose format the file doesn't name: Oodle (UE5's default), else LZ4
/// (UE4's), else zlib.
pub fn decompress_shader(src: &[u8], size: usize) -> Option<Vec<u8>> {
    if src.len() == size {
        return Some(src.to_vec());
    }
    ["Oodle", "LZ4", "Zlib"].iter().find_map(|m| decompress(m, src, size))
}

#[cfg(test)]
mod tests {
    #[test]
    fn lz4_and_zlib() {
        let data = b"unreal shader code ".repeat(40);
        assert_eq!(super::decompress_shader(&lz4_flex::block::compress(&data), data.len()).unwrap(), data);
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, &data).unwrap();
        assert_eq!(super::decompress("Zlib", &z.finish().unwrap(), data.len()).unwrap(), data);
    }
}
