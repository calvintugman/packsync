//! Blob codec: raw or Zstandard, chosen per blob by *measurement* (compression is skipped when it
//! does not pay), and decoding that is hardened against decompression bombs.

use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::chunk::MAX_BLOB;
use crate::error::{Error, Result};
use crate::rules::Compress;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Codec {
    Raw = 0,
    Zstd = 1,
}

impl Codec {
    pub fn from_u8(v: u8) -> Result<Codec> {
        match v {
            0 => Ok(Codec::Raw),
            1 => Ok(Codec::Zstd),
            _ => Err(Error::invalid("blob codec", format!("unknown codec id {v}"))),
        }
    }
}

/// Extensions of formats that are already compressed; trying zstd on them wastes CPU.
pub fn is_precompressed_ext(path: &str) -> bool {
    const EXT: &[&str] = &[
        ".jar",
        ".zip",
        ".png",
        ".jpg",
        ".jpeg",
        ".ogg",
        ".mp3",
        ".mp4",
        ".gz",
        ".zst",
        ".xz",
        ".7z",
        ".rar",
        ".bz2",
        ".lz4",
        ".webp",
        ".mca",
        ".mcr",
        ".dat",
        ".litematic",
        ".nbt",
    ];
    let l = path.to_ascii_lowercase();
    EXT.iter().any(|e| l.ends_with(e))
}

/// Keep the compressed form only if it saves at least this fraction.
const MIN_SAVING: f64 = 0.10;

/// Encode one blob. `Compress::Auto` measures: it tries zstd (unless the extension says the data is
/// already compressed) and keeps the result only when it is at least 10 % smaller.
pub fn encode_blob(raw: &[u8], policy: Compress, path_hint: &str, level: i32) -> (Codec, Vec<u8>) {
    let want = match policy {
        Compress::Never => false,
        Compress::Always => true,
        Compress::Auto => !is_precompressed_ext(path_hint) && raw.len() >= 64,
    };
    if !want {
        return (Codec::Raw, raw.to_vec());
    }
    match zstd::bulk::compress(raw, level) {
        Ok(c) if policy == Compress::Always || (c.len() as f64) <= (raw.len() as f64) * (1.0 - MIN_SAVING) => {
            (Codec::Zstd, c)
        }
        _ => (Codec::Raw, raw.to_vec()),
    }
}

/// Decode a stored blob whose raw length is declared as `raw_len`. The output must be *exactly*
/// that long and `raw_len` may not exceed [`MAX_BLOB`]; a zstd frame that expands further is
/// rejected without allocating beyond the cap.
pub fn decode_blob(codec: Codec, stored: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    if raw_len > MAX_BLOB {
        return Err(Error::invalid("blob", format!("declared raw size {raw_len} exceeds the {MAX_BLOB} byte limit")));
    }
    match codec {
        Codec::Raw => {
            if stored.len() != raw_len {
                return Err(Error::Corrupt(format!("raw blob has {} bytes, expected {raw_len}", stored.len())));
            }
            Ok(stored.to_vec())
        }
        Codec::Zstd => {
            let mut dec = zstd::stream::Decoder::new(stored).map_err(|e| Error::Corrupt(format!("zstd: {e}")))?;
            let mut out = Vec::with_capacity(raw_len);
            (&mut dec)
                .take(raw_len as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| Error::Corrupt(format!("zstd: {e}")))?;
            if out.len() != raw_len {
                return Err(Error::Corrupt(format!("zstd blob expanded to {} bytes, declared {raw_len}", out.len())));
            }
            Ok(out)
        }
    }
}

/// Compress a metadata document (manifest, index) — always zstd.
pub fn compress_doc(bytes: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(bytes, 9).expect("in-memory zstd compression cannot fail")
}

/// Decompress a metadata document with a hard output cap (bomb protection).
pub fn decompress_doc(stored: &[u8], max_len: u64) -> Result<Vec<u8>> {
    let mut dec = zstd::stream::Decoder::new(stored).map_err(|e| Error::Corrupt(format!("zstd: {e}")))?;
    let mut out = Vec::new();
    (&mut dec).take(max_len + 1).read_to_end(&mut out).map_err(|e| Error::Corrupt(format!("zstd: {e}")))?;
    if out.len() as u64 > max_len {
        return Err(Error::invalid("document", format!("decompresses beyond the {max_len} byte limit")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_compresses_text_and_skips_random_data() {
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(500);
        let (c, enc) = encode_blob(&text, Compress::Auto, "config/a.json", 3);
        assert_eq!(c, Codec::Zstd);
        assert!(enc.len() < text.len() / 5);
        assert_eq!(decode_blob(c, &enc, text.len()).unwrap(), text);

        let mut rnd = vec![0u8; 50_000];
        fastrand::Rng::with_seed(1).fill(&mut rnd);
        let (c, enc) = encode_blob(&rnd, Compress::Auto, "data/blob.bin", 3);
        assert_eq!(c, Codec::Raw, "incompressible data must be stored raw (measured, not assumed)");
        assert_eq!(enc, rnd);
    }

    #[test]
    fn precompressed_extensions_skip_the_attempt() {
        let text = vec![b'a'; 10_000];
        assert_eq!(encode_blob(&text, Compress::Auto, "mods/x.jar", 3).0, Codec::Raw);
        assert_eq!(encode_blob(&text, Compress::Auto, "saves/w/region/r.0.0.mca", 3).0, Codec::Raw);
        assert_eq!(encode_blob(&text, Compress::Always, "mods/x.jar", 3).0, Codec::Zstd);
        assert_eq!(encode_blob(&text, Compress::Never, "x.txt", 3).0, Codec::Raw);
    }

    #[test]
    fn decode_rejects_bombs_and_mismatches() {
        // 64 MiB of zeros compresses to a few KB: a classic bomb.
        let bomb = zstd::bulk::compress(&vec![0u8; 64 << 20], 19).unwrap();
        assert!(bomb.len() < 100_000);
        assert!(decode_blob(Codec::Zstd, &bomb, 1000).is_err(), "expansion beyond the declared size is rejected");
        assert!(decode_blob(Codec::Zstd, &bomb, MAX_BLOB + 1).is_err(), "declared size above the blob cap is rejected");
        let ok = zstd::bulk::compress(b"hello", 3).unwrap();
        assert!(decode_blob(Codec::Zstd, &ok, 4).is_err());
        assert!(decode_blob(Codec::Zstd, &ok, 6).is_err());
        assert_eq!(decode_blob(Codec::Zstd, &ok, 5).unwrap(), b"hello");
        assert!(decode_blob(Codec::Raw, b"abc", 4).is_err());
        assert!(decode_blob(Codec::Zstd, b"definitely not zstd", 10).is_err());
    }

    #[test]
    fn doc_decompression_is_capped() {
        let big = compress_doc(&vec![b'x'; 10 << 20]);
        assert!(decompress_doc(&big, 1 << 20).is_err());
        assert_eq!(decompress_doc(&big, 10 << 20).unwrap().len(), 10 << 20);
    }
}
