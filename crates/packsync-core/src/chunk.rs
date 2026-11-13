//! Streaming chunkers. A file is split into content-addressed *blobs*; unchanged blobs are never
//! uploaded again. Two mature strategies are offered and the defaults were chosen by the
//! benchmarks in `docs/benchmarks.md`:
//!
//! * **Fixed blocks** — predictable, and ideal for Minecraft region files whose chunks live in
//!   4 KiB sectors that are rewritten in place (an edit dirties the blocks covering the touched
//!   sectors and nothing else).
//! * **FastCDC** (content-defined) — for large mutable files where data can shift.
//!
//! Memory is bounded: at most one chunk (≤ [`MAX_BLOB`]) is alive per file being processed.

use std::io::{self, Read};

use crate::hash::Hash;
use crate::rules::Strategy;

/// Hard upper bound for any blob. Files up to this size are stored as a single blob.
pub const MAX_BLOB: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ChunkingConfig {
    /// Block size for `*.mca`/`*.mcr` region files.
    pub region_block: u32,
    /// Content-defined parameters for other large mutable files.
    pub cdc: (u32, u32, u32),
    /// Files larger than this that are not region/mutable are split in `MAX_BLOB` blocks.
    pub cdc_threshold: u64,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        // Chosen by the benchmark (see docs/benchmarks.md); override in rules with `strategy`.
        Self { region_block: 16 * 1024, cdc: (16 * 1024, 64 * 1024, 256 * 1024), cdc_threshold: 4 * 1024 * 1024 }
    }
}

/// Resolve `Strategy::Auto` for a concrete file; explicit strategies pass through (clamped so a
/// blob can never exceed [`MAX_BLOB`]).
pub fn resolve_strategy(requested: Strategy, path: &str, size: u64, cfg: &ChunkingConfig) -> Strategy {
    let s = match requested {
        Strategy::Auto => {
            let lower = path.to_ascii_lowercase();
            if lower.ends_with(".mca") || lower.ends_with(".mcr") {
                Strategy::Fixed(cfg.region_block)
            } else if size > cfg.cdc_threshold && is_mutable_data(&lower) {
                Strategy::Cdc { min: cfg.cdc.0, avg: cfg.cdc.1, max: cfg.cdc.2 }
            } else {
                Strategy::Whole
            }
        }
        other => other,
    };
    match s {
        Strategy::Fixed(n) => Strategy::Fixed(n.min(MAX_BLOB as u32)),
        Strategy::Cdc { min, avg, max } => Strategy::Cdc { min, avg, max: max.min(MAX_BLOB as u32) },
        other => other,
    }
}

/// Large files that are rewritten in place by games/mods (databases, big NBT, map data), as opposed
/// to archives that are replaced as a whole.
fn is_mutable_data(lower_path: &str) -> bool {
    const IMMUTABLE: &[&str] = &[
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
        ".nbt",
        ".litematic",
    ];
    !IMMUTABLE.iter().any(|e| lower_path.ends_with(e))
}

#[derive(Debug)]
pub struct Chunk {
    pub hash: Hash,
    pub data: Vec<u8>,
}

/// Split `reader` according to `strategy` (which must already be resolved: not `Auto`).
pub fn chunk_stream<'a, R: Read + 'a>(
    reader: R,
    strategy: Strategy,
) -> Box<dyn Iterator<Item = io::Result<Chunk>> + 'a> {
    match strategy {
        Strategy::Auto | Strategy::Whole => Box::new(FixedChunker::new(reader, MAX_BLOB)),
        Strategy::Fixed(n) => Box::new(FixedChunker::new(reader, n as usize)),
        Strategy::Cdc { min, avg, max } => Box::new(
            fastcdc::v2020::StreamCDC::new(reader, min as usize, avg as usize, max as usize).map(|r| match r {
                Ok(c) => Ok(Chunk { hash: Hash::of(&c.data), data: c.data }),
                Err(fastcdc::v2020::Error::IoError(e)) => Err(e),
                Err(e) => Err(io::Error::other(e.to_string())),
            }),
        ),
    }
}

struct FixedChunker<R> {
    r: R,
    size: usize,
    done: bool,
}

impl<R: Read> FixedChunker<R> {
    fn new(r: R, size: usize) -> Self {
        assert!(size > 0 && size <= MAX_BLOB);
        Self { r, size, done: false }
    }
}

impl<R: Read> Iterator for FixedChunker<R> {
    type Item = io::Result<Chunk>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let mut buf = vec![0u8; self.size];
        let mut filled = 0;
        while filled < self.size {
            match self.r.read(&mut buf[filled..]) {
                Ok(0) => {
                    self.done = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        if filled == 0 {
            return None;
        }
        buf.truncate(filled);
        Some(Ok(Chunk { hash: Hash::of(&buf), data: buf }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(len: usize, seed: u64) -> Vec<u8> {
        let mut rng = fastrand::Rng::with_seed(seed);
        let mut v = vec![0u8; len];
        rng.fill(&mut v);
        v
    }

    fn collect(d: &[u8], s: Strategy) -> Vec<Chunk> {
        chunk_stream(d, s).map(|c| c.unwrap()).collect()
    }

    #[test]
    fn chunks_always_reassemble_to_the_input() {
        for len in [0usize, 1, 4095, 4096, 4097, 100_000, 1_048_577] {
            let d = data(len, len as u64);
            for s in [
                Strategy::Whole,
                Strategy::Fixed(4096),
                Strategy::Fixed(16384),
                Strategy::Cdc { min: 2048, avg: 8192, max: 32768 },
            ] {
                let chunks = collect(&d, s);
                let joined: Vec<u8> = chunks.iter().flat_map(|c| c.data.clone()).collect();
                assert_eq!(joined, d, "{s} len {len}");
                for c in &chunks {
                    assert_eq!(c.hash, Hash::of(&c.data));
                    assert!(c.data.len() <= MAX_BLOB);
                    assert!(!c.data.is_empty());
                }
            }
        }
    }

    #[test]
    fn fixed_blocks_localise_in_place_edits() {
        let mut d = data(1 << 20, 1);
        let before = collect(&d, Strategy::Fixed(16384));
        d[5 * 16384 + 100] ^= 0xFF; // one byte in block 5
        d[40 * 16384] ^= 0x01; // and one in block 40
        let after = collect(&d, Strategy::Fixed(16384));
        let changed: Vec<usize> = (0..before.len()).filter(|&i| before[i].hash != after[i].hash).collect();
        assert_eq!(changed, vec![5, 40]);
    }

    #[test]
    fn cdc_resynchronises_after_an_insertion() {
        let d = data(2 << 20, 2);
        let mut e = d.clone();
        e.splice(500_000..500_000, std::iter::repeat_n(0xAB, 1000));
        let s = Strategy::Cdc { min: 4096, avg: 16384, max: 65536 };
        let a: std::collections::HashSet<Hash> = collect(&d, s).iter().map(|c| c.hash).collect();
        let b = collect(&e, s);
        let reused = b.iter().filter(|c| a.contains(&c.hash)).count();
        assert!(
            reused as f64 > 0.9 * b.len() as f64,
            "CDC should reuse >90% of chunks after an insertion, got {reused}/{}",
            b.len()
        );
        // fixed blocks do NOT survive an insertion: this is why CDC exists
        let fa: std::collections::HashSet<Hash> = collect(&d, Strategy::Fixed(16384)).iter().map(|c| c.hash).collect();
        let fb = collect(&e, Strategy::Fixed(16384));
        let freused = fb.iter().filter(|c| fa.contains(&c.hash)).count();
        assert!(freused < reused);
    }

    #[test]
    fn cdc_respects_size_bounds() {
        let d = data(3 << 20, 3);
        let chunks = collect(&d, Strategy::Cdc { min: 4096, avg: 16384, max: 65536 });
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.data.len() <= 65536);
            if i + 1 < chunks.len() {
                assert!(c.data.len() >= 4096);
            }
        }
    }

    #[test]
    fn auto_strategy_selection() {
        let cfg = ChunkingConfig::default();
        assert_eq!(resolve_strategy(Strategy::Auto, "s/W/region/r.0.0.mca", 4 << 20, &cfg), Strategy::Fixed(16384));
        assert_eq!(resolve_strategy(Strategy::Auto, "s/W/REGION/R.0.0.MCA", 100, &cfg), Strategy::Fixed(16384));
        assert_eq!(resolve_strategy(Strategy::Auto, "mods/a.jar", 50 << 20, &cfg), Strategy::Whole);
        assert_eq!(resolve_strategy(Strategy::Auto, "config/a.json", 1000, &cfg), Strategy::Whole);
        assert!(matches!(resolve_strategy(Strategy::Auto, "data/big.sqlite", 50 << 20, &cfg), Strategy::Cdc { .. }));
        assert_eq!(resolve_strategy(Strategy::Fixed(u32::MAX), "x", 1, &cfg), Strategy::Fixed(MAX_BLOB as u32));
    }

    #[test]
    fn whole_strategy_splits_only_above_the_blob_cap() {
        let d = data(MAX_BLOB + 10, 4);
        let c = collect(&d, Strategy::Whole);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].data.len(), MAX_BLOB);
        assert_eq!(c[1].data.len(), 10);
    }
}
