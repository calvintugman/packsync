//! Local staging of new blobs. While a snapshot is being created, new content is compressed (when
//! it pays) and appended to bounded pack files in a private staging directory. Together with the
//! manifest they form the **local snapshot**: an isolated, stable copy of everything that has to
//! be uploaded, independent of the live (mutable) instance.
//!
//! Blobs the cloud already has (per the cached remote index) or that an earlier unpublished local
//! snapshot already staged are *not* staged again: that is how unchanged mod JARs, packs and world
//! data are deduplicated across snapshots **and instances**.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::ThreadId;

use parking_lot::Mutex;

use crate::codec::{Codec, encode_blob};
use crate::db::Db;
use crate::error::Result;
use crate::hash::Hash;
use crate::pack::{FinishedPack, PackWriter};
use crate::rules::Compress;

#[derive(Debug, Default, Clone)]
pub struct StageStats {
    pub blobs_new: u64,
    pub blobs_reused: u64,
    pub raw_bytes_new: u64,
    pub stored_bytes_new: u64,
    pub compressed_blobs: u64,
    /// New (not yet stored anywhere) data per file: path → (raw bytes, stored bytes).
    pub new_by_path: HashMap<String, (u64, u64)>,
}

pub trait BlobSink: Sync {
    /// Offer one blob. Returns `true` when it was newly staged, `false` when it is already known
    /// (cloud, an earlier local snapshot, or earlier in this run).
    fn offer(&self, hash: Hash, raw: &[u8], kind: u8, path_hint: &str, compress: Compress) -> Result<bool>;

    /// The calling thread has offered every blob of one file (or given up on it). A sink that can keeps
    /// the blobs of one file next to each other in its packs: files are hashed in parallel, and without
    /// this their blocks would be interleaved, so reading one file back would take a request per block.
    fn end_of_file(&self) -> Result<()> {
        Ok(())
    }
}

/// A file's blobs are written to the pack in one piece up to this many stored bytes; a bigger file is
/// written in pieces of this size. It bounds the memory held per hashing thread.
const RUN_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// One staged blob waiting to be appended to the open pack.
struct Pending {
    hash: Hash,
    codec: Codec,
    stored: Vec<u8>,
    raw_len: usize,
    kind: u8,
}

/// The blobs one thread has produced for the file it is working on.
#[derive(Default)]
struct Run {
    blobs: Vec<Pending>,
    bytes: u64,
}

pub struct StagingSink {
    db: Arc<Db>,
    dir: PathBuf,
    target: u64,
    level: i32,
    /// Blobs staged by earlier, still unpublished local snapshots of this device.
    known_local: HashSet<Hash>,
    seen: Mutex<HashSet<Hash>>,
    writer: Mutex<Option<PackWriter>>,
    /// Per hashing thread: the blobs of the file in progress, not yet in the pack.
    runs: Mutex<HashMap<ThreadId, Run>>,
    finished: Mutex<Vec<FinishedPack>>,
    reused_packs: Mutex<HashSet<Hash>>,
    blobs_new: AtomicU64,
    blobs_reused: AtomicU64,
    raw_new: AtomicU64,
    stored_new: AtomicU64,
    compressed: AtomicU64,
    new_by_path: Mutex<HashMap<String, (u64, u64)>>,
}

pub struct StagedOutput {
    pub packs: Vec<FinishedPack>,
    /// Remote packs that this snapshot's *reused* blobs live in (verified to still exist at upload time).
    pub reused_packs: HashSet<Hash>,
    pub stats: StageStats,
}

impl StagingSink {
    pub fn new(db: Arc<Db>, dir: PathBuf, target: u64, level: i32, known_local: HashSet<Hash>) -> Result<StagingSink> {
        std::fs::create_dir_all(&dir).map_err(|e| crate::error::Error::io(format!("creating {}", dir.display()), e))?;
        Ok(StagingSink {
            db,
            dir,
            target,
            level,
            known_local,
            seen: Mutex::new(HashSet::new()),
            writer: Mutex::new(None),
            runs: Mutex::new(HashMap::new()),
            finished: Mutex::new(vec![]),
            reused_packs: Mutex::new(HashSet::new()),
            blobs_new: AtomicU64::new(0),
            blobs_reused: AtomicU64::new(0),
            raw_new: AtomicU64::new(0),
            stored_new: AtomicU64::new(0),
            compressed: AtomicU64::new(0),
            new_by_path: Mutex::new(HashMap::new()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append a run's blobs to the open pack, back to back, starting a new pack whenever one is full.
    fn append(&self, run: Run) -> Result<()> {
        if run.blobs.is_empty() {
            return Ok(());
        }
        let mut g = self.writer.lock();
        for p in run.blobs {
            if g.is_none() {
                *g = Some(PackWriter::create(&self.dir, self.target)?);
            }
            let w = g.as_mut().expect("writer was just created");
            w.push(p.hash, p.codec, &p.stored, p.raw_len, p.kind)?;
            if w.is_full() {
                let full = g.take().expect("writer present");
                self.finished.lock().push(full.finish()?);
            }
        }
        Ok(())
    }

    /// Flush the open pack and return everything staged.
    pub fn finish(self) -> Result<StagedOutput> {
        // blobs of files whose thread never said it was done (it cannot happen through the scanner, but
        // dropping them would leave blobs that were marked as staged out of every pack)
        let leftovers: Vec<Run> = std::mem::take(&mut *self.runs.lock()).into_values().collect();
        for run in leftovers {
            self.append(run)?;
        }
        if let Some(w) = self.writer.lock().take() {
            if w.is_empty() {
                w.discard();
            } else {
                self.finished.lock().push(w.finish()?);
            }
        }
        Ok(StagedOutput {
            packs: std::mem::take(&mut *self.finished.lock()),
            reused_packs: std::mem::take(&mut *self.reused_packs.lock()),
            stats: StageStats {
                blobs_new: self.blobs_new.load(Ordering::Relaxed),
                blobs_reused: self.blobs_reused.load(Ordering::Relaxed),
                raw_bytes_new: self.raw_new.load(Ordering::Relaxed),
                stored_bytes_new: self.stored_new.load(Ordering::Relaxed),
                compressed_blobs: self.compressed.load(Ordering::Relaxed),
                new_by_path: std::mem::take(&mut *self.new_by_path.lock()),
            },
        })
    }

    /// Abandon everything staged so far (failed or cancelled snapshot).
    pub fn discard(self) {
        self.runs.lock().clear();
        if let Some(w) = self.writer.lock().take() {
            w.discard();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl BlobSink for StagingSink {
    fn offer(&self, hash: Hash, raw: &[u8], kind: u8, path_hint: &str, compress: Compress) -> Result<bool> {
        if !self.seen.lock().insert(hash) {
            self.blobs_reused.fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }
        if self.known_local.contains(&hash) {
            self.blobs_reused.fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }
        if let Some((pack, _)) = self.db.blob_lookup(hash)? {
            self.reused_packs.lock().insert(pack);
            self.blobs_reused.fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }
        // Compression is CPU-heavy: do it outside any lock.
        let (codec, stored) = encode_blob(raw, compress, path_hint, self.level);
        self.blobs_new.fetch_add(1, Ordering::Relaxed);
        self.raw_new.fetch_add(raw.len() as u64, Ordering::Relaxed);
        self.stored_new.fetch_add(stored.len() as u64, Ordering::Relaxed);
        if codec == Codec::Zstd {
            self.compressed.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut by = self.new_by_path.lock();
            let e = by.entry(path_hint.to_string()).or_default();
            e.0 += raw.len() as u64;
            e.1 += stored.len() as u64;
        }
        // Queue it with the other blobs of this thread's file; the run goes into the pack in one piece.
        let full_run = {
            let mut runs = self.runs.lock();
            let run = runs.entry(std::thread::current().id()).or_default();
            run.bytes += stored.len() as u64;
            run.blobs.push(Pending { hash, codec, stored, raw_len: raw.len(), kind });
            (run.bytes >= RUN_MAX_BYTES).then(|| std::mem::take(run))
        };
        if let Some(run) = full_run {
            self.append(run)?;
        }
        Ok(true)
    }

    fn end_of_file(&self) -> Result<()> {
        let run = self.runs.lock().remove(&std::thread::current().id());
        match run {
            Some(run) => self.append(run),
            None => Ok(()),
        }
    }
}

/// A sink that stages nothing and only counts what *would* be new (dry-run previews).
pub struct CountingSink {
    pub db: Arc<Db>,
    pub seen: Mutex<HashSet<Hash>>,
    pub new_blobs: AtomicU64,
    pub new_bytes: AtomicU64,
}

impl CountingSink {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db, seen: Mutex::new(HashSet::new()), new_blobs: AtomicU64::new(0), new_bytes: AtomicU64::new(0) }
    }
}

impl BlobSink for CountingSink {
    fn offer(&self, hash: Hash, raw: &[u8], _kind: u8, _path_hint: &str, _compress: Compress) -> Result<bool> {
        if !self.seen.lock().insert(hash) || self.db.blob_known(hash)? {
            return Ok(false);
        }
        self.new_blobs.fetch_add(1, Ordering::Relaxed);
        self.new_bytes.fetch_add(raw.len() as u64, Ordering::Relaxed);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{KIND_DATA, read_footer_from_file};

    fn blob(i: u64, len: usize) -> (Hash, Vec<u8>) {
        let mut v = vec![0u8; len];
        fastrand::Rng::with_seed(i).fill(&mut v);
        (Hash::of(&v), v)
    }

    #[test]
    fn dedups_within_run_against_cloud_and_local_and_bounds_packs() {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open_memory().unwrap());
        let (cloud_h, cloud_b) = blob(1, 5000);
        db.blobs_put(
            Hash::of(b"cloud-pack"),
            &[crate::pack::BlobEntry { hash: cloud_h, offset: 16, stored_len: 5000, raw_len: 5000, codec: 0, kind: 0 }],
        )
        .unwrap();
        let (local_h, local_b) = blob(2, 5000);
        let sink = StagingSink::new(db, d.path().join("s"), 64 * 1024, 3, HashSet::from([local_h])).unwrap();

        assert!(!sink.offer(cloud_h, &cloud_b, KIND_DATA, "a.bin", Compress::Auto).unwrap(), "cloud already has it");
        assert!(
            !sink.offer(local_h, &local_b, KIND_DATA, "a.bin", Compress::Auto).unwrap(),
            "earlier local snapshot staged it"
        );
        let mut new = vec![];
        for i in 10..40 {
            let (h, b) = blob(i, 10_000);
            assert!(sink.offer(h, &b, KIND_DATA, "x.bin", Compress::Auto).unwrap());
            assert!(!sink.offer(h, &b, KIND_DATA, "x.bin", Compress::Auto).unwrap(), "duplicate in the same run");
            new.push(h);
        }
        let out = sink.finish().unwrap();
        assert_eq!(out.stats.blobs_new, 30);
        assert!(out.reused_packs.contains(&Hash::of(b"cloud-pack")));
        assert!(out.packs.len() >= 4, "30 x 10 KB with a 64 KiB target must roll over: got {}", out.packs.len());
        let mut found = 0;
        for p in &out.packs {
            assert!(p.size < 64 * 1024 + 20_000, "packs stay bounded (target + one blob): {}", p.size);
            let entries = read_footer_from_file(&p.path).unwrap();
            found += entries.iter().filter(|e| new.contains(&e.hash)).count();
        }
        assert_eq!(found, 30);
    }

    #[test]
    fn discard_leaves_no_files() {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open_memory().unwrap());
        let sink = StagingSink::new(db, d.path().join("s"), 1 << 20, 3, HashSet::new()).unwrap();
        let (h, b) = blob(5, 1000);
        sink.offer(h, &b, KIND_DATA, "a", Compress::Never).unwrap();
        sink.discard();
        assert!(!d.path().join("s").exists());
    }
}
