//! Pack files (remote format v1, `docs/remote-format.md`).
//!
//! A pack groups many small blobs into one remote object so the number of Drive requests stays
//! low (thousands of tiny uploads would be slow and rate-limited). Layout:
//!
//! ```text
//! header   16 B   "PSPACK1\0" + 8 reserved zero bytes
//! blobs    …      stored (raw or zstd) blob bytes, back to back
//! footer   …      zstd(JSON { format, version, entries:[{h,o,s,r,c,k}] })
//! trailer  44 B   u32 LE footer_stored_len | BLAKE3(footer_stored) | "PSPKEND1"
//! ```
//!
//! * A pack's identity is the BLAKE3 hash of its complete bytes; the object name is `<id>.pack`.
//! * The footer makes a pack self-describing (indexes can be rebuilt from packs), the trailer
//!   hash protects the footer, and every blob is verified by its own hash after decoding.
//! * Footers and index entries are **untrusted input**: [`validate_entries`] bounds every offset
//!   and length and rejects overlaps before anything is read.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chunk::MAX_BLOB;
use crate::codec::{Codec, compress_doc, decompress_doc};
use crate::error::{Error, Result};
use crate::hash::{Hash, Hasher};

pub const PACK_MAGIC: &[u8; 8] = b"PSPACK1\0";
pub const PACK_END_MAGIC: &[u8; 8] = b"PSPKEND1";
pub const PACK_HEADER_LEN: u64 = 16;
pub const TRAILER_LEN: u64 = 4 + 32 + 8;
pub const MAX_FOOTER_STORED: u64 = 32 * 1024 * 1024;
pub const MAX_FOOTER_RAW: u64 = 128 * 1024 * 1024;
pub const MAX_PACK_ENTRIES: usize = 1_000_000;
/// Absolute upper bound for a pack object (writers stop at the target size plus one blob).
pub const MAX_PACK_SIZE: u64 = 256 * 1024 * 1024;

pub const KIND_DATA: u8 = 0;
pub const KIND_RECIPE: u8 = 1;

pub const FOOTER_FORMAT: &str = "packsync.pack";
pub const INDEX_FORMAT: &str = "packsync.index";
pub const FORMAT_VERSION: u32 = 1;

/// Where one blob lives inside a pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobEntry {
    #[serde(rename = "h")]
    pub hash: Hash,
    #[serde(rename = "o")]
    pub offset: u64,
    #[serde(rename = "s")]
    pub stored_len: u32,
    #[serde(rename = "r")]
    pub raw_len: u32,
    #[serde(rename = "c")]
    pub codec: u8,
    #[serde(rename = "k", default)]
    pub kind: u8,
}

#[derive(Debug, Serialize, Deserialize)]
struct Footer {
    format: String,
    version: u32,
    entries: Vec<BlobEntry>,
}

pub struct FinishedPack {
    pub id: Hash,
    pub path: PathBuf,
    pub size: u64,
    pub entries: Vec<BlobEntry>,
}

/// Streaming pack writer. Bytes go to a temp file (never visible under the final name) and the
/// pack is atomically renamed to `<id>.pack` by [`PackWriter::finish`].
pub struct PackWriter {
    dir: PathBuf,
    tmp: PathBuf,
    out: BufWriter<File>,
    hasher: Hasher,
    offset: u64,
    entries: Vec<BlobEntry>,
    seen: HashSet<Hash>,
    target: u64,
}

impl PackWriter {
    pub fn create(dir: &Path, target: u64) -> Result<PackWriter> {
        fs::create_dir_all(dir).map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        let tmp = dir.join(format!(".new-{}.pack", uuid::Uuid::new_v4().simple()));
        let f = File::create(&tmp).map_err(|e| Error::io(format!("creating {}", tmp.display()), e))?;
        let mut w = PackWriter {
            dir: dir.to_path_buf(),
            tmp,
            out: BufWriter::with_capacity(1 << 20, f),
            hasher: Hasher::new(),
            offset: 0,
            entries: vec![],
            seen: HashSet::new(),
            target,
        };
        let mut header = [0u8; PACK_HEADER_LEN as usize];
        header[..8].copy_from_slice(PACK_MAGIC);
        w.write_all(&header)?;
        Ok(w)
    }

    fn write_all(&mut self, b: &[u8]) -> Result<()> {
        self.out.write_all(b).map_err(|e| Error::io(format!("writing {}", self.tmp.display()), e))?;
        self.hasher.update(b);
        self.offset += b.len() as u64;
        Ok(())
    }

    /// Append a blob. Returns `false` (and writes nothing) if the same blob is already in this pack.
    pub fn push(&mut self, hash: Hash, codec: Codec, stored: &[u8], raw_len: usize, kind: u8) -> Result<bool> {
        if raw_len > MAX_BLOB || stored.len() > u32::MAX as usize || stored.is_empty() {
            return Err(Error::invalid("blob", "size out of range"));
        }
        if !self.seen.insert(hash) {
            return Ok(false);
        }
        let offset = self.offset;
        self.write_all(stored)?;
        self.entries.push(BlobEntry {
            hash,
            offset,
            stored_len: stored.len() as u32,
            raw_len: raw_len as u32,
            codec: codec as u8,
            kind,
        });
        Ok(true)
    }

    pub fn is_full(&self) -> bool {
        self.offset >= self.target || self.entries.len() >= MAX_PACK_ENTRIES / 2
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn len(&self) -> u64 {
        self.offset
    }
    pub fn contains(&self, h: &Hash) -> bool {
        self.seen.contains(h)
    }

    /// Write footer + trailer, fsync, and rename to `<dir>/<id>.pack`.
    pub fn finish(mut self) -> Result<FinishedPack> {
        let footer = Footer {
            format: FOOTER_FORMAT.into(),
            version: FORMAT_VERSION,
            entries: std::mem::take(&mut self.entries),
        };
        let json = serde_json::to_vec(&footer)?;
        let stored = compress_doc(&json);
        let footer_hash = Hash::of(&stored);
        self.write_all(&stored)?;
        let mut trailer = Vec::with_capacity(TRAILER_LEN as usize);
        trailer.extend((stored.len() as u32).to_le_bytes());
        trailer.extend(footer_hash.0);
        trailer.extend(PACK_END_MAGIC);
        self.write_all(&trailer)?;
        self.out.flush().map_err(|e| Error::io("flushing pack", e))?;
        let file = self.out.into_inner().map_err(|e| Error::io("flushing pack", e.into_error()))?;
        file.sync_all().map_err(|e| Error::io("syncing pack", e))?;
        drop(file);
        let id = self.hasher.finalize();
        let path = self.dir.join(format!("{}.pack", id.to_hex()));
        if path.exists() {
            // identical content already staged: keep the existing file
            let _ = fs::remove_file(&self.tmp);
        } else {
            fs::rename(&self.tmp, &path).map_err(|e| Error::io(format!("renaming pack into {}", path.display()), e))?;
            crate::fsutil::fsync_dir(&self.dir);
        }
        Ok(FinishedPack { id, path, size: self.offset, entries: footer.entries })
    }

    /// Abandon the pack, removing the temp file.
    pub fn discard(self) {
        drop(self.out);
        let _ = fs::remove_file(&self.tmp);
    }
}

/// Reject entries that could read outside the data region or overlap each other.
pub fn validate_entries(entries: &[BlobEntry], pack_size: u64) -> Result<()> {
    if entries.len() > MAX_PACK_ENTRIES {
        return Err(Error::invalid(
            "pack entries",
            format!("{} entries exceed the limit of {MAX_PACK_ENTRIES}", entries.len()),
        ));
    }
    if !(PACK_HEADER_LEN + TRAILER_LEN..=MAX_PACK_SIZE).contains(&pack_size) {
        return Err(Error::invalid("pack", format!("implausible pack size {pack_size}")));
    }
    let data_end = pack_size - TRAILER_LEN;
    let mut spans: Vec<(u64, u64)> = Vec::with_capacity(entries.len());
    let mut seen = HashSet::with_capacity(entries.len());
    for e in entries {
        Codec::from_u8(e.codec)?;
        if e.kind > KIND_RECIPE {
            return Err(Error::invalid("pack entry", format!("unknown blob kind {}", e.kind)));
        }
        if e.stored_len == 0 || e.raw_len == 0 || e.raw_len as usize > MAX_BLOB {
            return Err(Error::invalid("pack entry", format!("blob {} has an invalid size", e.hash.short())));
        }
        if e.codec == Codec::Raw as u8 && e.stored_len != e.raw_len {
            return Err(Error::invalid("pack entry", format!("raw blob {} has stored != raw length", e.hash.short())));
        }
        let end =
            e.offset.checked_add(e.stored_len as u64).ok_or_else(|| Error::invalid("pack entry", "offset overflow"))?;
        if e.offset < PACK_HEADER_LEN || end > data_end {
            return Err(Error::invalid(
                "pack entry",
                format!("blob {} lies outside the pack's data region", e.hash.short()),
            ));
        }
        if !seen.insert(e.hash) {
            return Err(Error::invalid("pack entry", format!("duplicate blob {} in one pack", e.hash.short())));
        }
        spans.push((e.offset, end));
    }
    spans.sort_unstable();
    for w in spans.windows(2) {
        if w[0].1 > w[1].0 {
            return Err(Error::invalid("pack entry", "overlapping blobs"));
        }
    }
    Ok(())
}

/// Read and verify a pack's footer through any ranged reader (a local file or remote `get`
/// calls): `read(offset, len)` must return exactly `len` bytes.
pub fn read_footer(pack_size: u64, mut read: impl FnMut(u64, u64) -> Result<Vec<u8>>) -> Result<Vec<BlobEntry>> {
    if !(PACK_HEADER_LEN + TRAILER_LEN..=MAX_PACK_SIZE).contains(&pack_size) {
        return Err(Error::Corrupt(format!("pack has an implausible size ({pack_size} bytes)")));
    }
    let head = read(0, 8)?;
    if head.as_slice() != PACK_MAGIC {
        return Err(Error::Corrupt("not a PackSync pack (bad magic)".into()));
    }
    let tail = read(pack_size - TRAILER_LEN, TRAILER_LEN)?;
    if tail.len() as u64 != TRAILER_LEN || &tail[36..] != PACK_END_MAGIC {
        return Err(Error::Corrupt("pack trailer is missing or damaged (truncated upload?)".into()));
    }
    let flen = u32::from_le_bytes(tail[..4].try_into().unwrap()) as u64;
    let fhash = Hash(tail[4..36].try_into().unwrap());
    if flen == 0 || flen > MAX_FOOTER_STORED || flen > pack_size - TRAILER_LEN - PACK_HEADER_LEN {
        return Err(Error::Corrupt(format!("pack footer length {flen} is implausible")));
    }
    let stored = read(pack_size - TRAILER_LEN - flen, flen)?;
    if stored.len() as u64 != flen || Hash::of(&stored) != fhash {
        return Err(Error::Corrupt("pack footer failed its checksum".into()));
    }
    let json = decompress_doc(&stored, MAX_FOOTER_RAW)?;
    let footer: Footer = serde_json::from_slice(&json).map_err(|e| Error::Corrupt(format!("pack footer: {e}")))?;
    if footer.format != FOOTER_FORMAT || footer.version != FORMAT_VERSION {
        return Err(Error::Corrupt(format!("unsupported pack footer {} v{}", footer.format, footer.version)));
    }
    // blobs must end before the footer starts
    let mut entries = footer.entries;
    validate_entries(&entries, pack_size)?;
    entries.sort_by_key(|e| e.offset);
    if let Some(last) = entries.last()
        && last.offset + last.stored_len as u64 > pack_size - TRAILER_LEN - flen
    {
        return Err(Error::Corrupt("pack blobs overlap the footer".into()));
    }
    Ok(entries)
}

/// Read a footer from a pack file on disk.
pub fn read_footer_from_file(path: &Path) -> Result<Vec<BlobEntry>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
    let size = f.metadata().map_err(|e| Error::io("stat pack", e))?.len();
    read_footer(size, |off, len| {
        f.seek(SeekFrom::Start(off)).map_err(|e| Error::io("seek", e))?;
        let mut b = vec![0u8; len as usize];
        f.read_exact(&mut b).map_err(|e| Error::Corrupt(format!("pack is truncated: {e}")))?;
        Ok(b)
    })
}

/// A set of packs and where their blobs are: the unit that is uploaded to `index/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexPack {
    pub id: Hash,
    pub size: u64,
    pub blobs: Vec<BlobEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDoc {
    pub format: String,
    pub version: u32,
    pub packs: Vec<IndexPack>,
    /// Index objects whose content is fully contained here (written by consolidation).
    #[serde(default)]
    pub supersedes: Vec<Hash>,
}

pub const MAX_INDEX_RAW: u64 = 512 * 1024 * 1024;

impl IndexDoc {
    pub fn new(packs: Vec<IndexPack>) -> IndexDoc {
        IndexDoc { format: INDEX_FORMAT.into(), version: FORMAT_VERSION, packs, supersedes: vec![] }
    }

    /// Canonical bytes → `(id, stored)`; the id is the BLAKE3 of the uncompressed JSON.
    pub fn encode(&self) -> Result<(Hash, Vec<u8>)> {
        let json = serde_json::to_vec(self)?;
        Ok((Hash::of(&json), compress_doc(&json)))
    }

    pub fn decode(stored: &[u8], expected: Hash) -> Result<IndexDoc> {
        let json = decompress_doc(stored, MAX_INDEX_RAW)?;
        if Hash::of(&json) != expected {
            return Err(Error::Corrupt("index object does not match its name (hash mismatch)".into()));
        }
        let doc: IndexDoc = serde_json::from_slice(&json).map_err(|e| Error::Corrupt(format!("index: {e}")))?;
        if doc.format != INDEX_FORMAT {
            return Err(Error::Corrupt(format!("unexpected index format {:?}", doc.format)));
        }
        if doc.version != FORMAT_VERSION {
            return Err(Error::Unsupported(format!(
                "index format version {} was written by a newer PackSync",
                doc.version
            )));
        }
        for p in &doc.packs {
            validate_entries(&p.blobs, p.size)?;
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_blob, encode_blob};
    use crate::rules::Compress;

    fn blob(seed: u64, len: usize, compressible: bool) -> Vec<u8> {
        if compressible {
            format!("line {seed}\n").into_bytes().into_iter().cycle().take(len).collect()
        } else {
            let mut v = vec![0u8; len];
            fastrand::Rng::with_seed(seed).fill(&mut v);
            v
        }
    }

    fn write_sample(dir: &Path) -> (FinishedPack, Vec<(Hash, Vec<u8>)>) {
        let mut w = PackWriter::create(dir, 1 << 20).unwrap();
        let mut blobs = vec![];
        for i in 0..40u64 {
            let raw = blob(i, 1000 + (i as usize * 313) % 9000, i % 2 == 0);
            let h = Hash::of(&raw);
            let (c, enc) = encode_blob(&raw, Compress::Auto, "x.json", 3);
            assert!(w.push(h, c, &enc, raw.len(), KIND_DATA).unwrap());
            blobs.push((h, raw));
        }
        // a duplicate inside one pack is ignored
        let (h, raw) = &blobs[3];
        assert!(!w.push(*h, Codec::Raw, raw, raw.len(), KIND_DATA).unwrap());
        (w.finish().unwrap(), blobs)
    }

    #[test]
    fn roundtrip_footer_and_every_blob() {
        let d = tempfile::tempdir().unwrap();
        let (p, blobs) = write_sample(d.path());
        assert_eq!(p.path.file_name().unwrap().to_str().unwrap(), format!("{}.pack", p.id));
        let bytes = fs::read(&p.path).unwrap();
        assert_eq!(Hash::of(&bytes), p.id, "pack id is the hash of its bytes");
        assert_eq!(bytes.len() as u64, p.size);

        let entries = read_footer_from_file(&p.path).unwrap();
        assert_eq!(entries.len(), blobs.len());
        for (h, raw) in &blobs {
            let e = entries.iter().find(|e| e.hash == *h).expect("blob listed in footer");
            let stored = &bytes[e.offset as usize..e.offset as usize + e.stored_len as usize];
            let back = decode_blob(Codec::from_u8(e.codec).unwrap(), stored, e.raw_len as usize).unwrap();
            assert_eq!(&back, raw);
            assert_eq!(Hash::of(&back), *h);
        }
    }

    #[test]
    fn corrupted_or_truncated_packs_are_rejected_not_trusted() {
        let d = tempfile::tempdir().unwrap();
        let (p, _) = write_sample(d.path());
        let bytes = fs::read(&p.path).unwrap();
        let try_bytes = |b: &[u8]| {
            let f = d.path().join("probe.pack");
            fs::write(&f, b).unwrap();
            read_footer_from_file(&f)
        };
        assert!(try_bytes(&bytes).is_ok());
        // truncation (an interrupted upload)
        for cut in [1usize, 10, 44, 100, bytes.len() / 2] {
            assert!(try_bytes(&bytes[..bytes.len() - cut]).is_err(), "cut {cut}");
        }
        // flip a footer byte
        let mut b = bytes.clone();
        let i = b.len() - TRAILER_LEN as usize - 5;
        b[i] ^= 0xFF;
        assert!(try_bytes(&b).is_err());
        // bad magic
        let mut b = bytes.clone();
        b[0] ^= 1;
        assert!(try_bytes(&b).is_err());
        // garbage trailer length
        let mut b = bytes.clone();
        let t = b.len() - TRAILER_LEN as usize;
        b[t..t + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(try_bytes(&b).is_err());
    }

    #[test]
    fn malicious_entries_are_rejected() {
        let h = |n: u8| Hash([n; 32]);
        let e = |n: u8, off: u64, len: u32| BlobEntry {
            hash: h(n),
            offset: off,
            stored_len: len,
            raw_len: len,
            codec: 0,
            kind: 0,
        };
        let size = 1000u64;
        assert!(validate_entries(&[e(1, 16, 100), e(2, 116, 100)], size).is_ok());
        assert!(validate_entries(&[e(1, 16, 100), e(2, 100, 100)], size).is_err(), "overlap");
        assert!(validate_entries(&[e(1, 0, 100)], size).is_err(), "inside the header");
        assert!(validate_entries(&[e(1, 900, 100)], size).is_err(), "runs into the trailer");
        assert!(validate_entries(&[e(1, u64::MAX - 5, 100)], size).is_err(), "overflow");
        assert!(validate_entries(&[e(1, 16, 100), e(1, 200, 100)], size).is_err(), "duplicate hash");
        assert!(validate_entries(&[e(1, 16, 0)], size).is_err(), "zero length");
        assert!(validate_entries(&[BlobEntry { raw_len: (MAX_BLOB + 1) as u32, ..e(1, 16, 100) }], size).is_err());
        assert!(validate_entries(&[BlobEntry { codec: 9, ..e(1, 16, 100) }], size).is_err());
        assert!(
            validate_entries(&[BlobEntry { raw_len: 50, ..e(1, 16, 100) }], size).is_err(),
            "raw blob with stored != raw"
        );
    }

    #[test]
    fn index_docs_roundtrip_and_verify_their_name() {
        let d = tempfile::tempdir().unwrap();
        let (p, _) = write_sample(d.path());
        let doc = IndexDoc::new(vec![IndexPack { id: p.id, size: p.size, blobs: p.entries.clone() }]);
        let (id, stored) = doc.encode().unwrap();
        let back = IndexDoc::decode(&stored, id).unwrap();
        assert_eq!(back.packs.len(), 1);
        assert_eq!(back.packs[0].blobs, p.entries);
        assert!(IndexDoc::decode(&stored, Hash::of(b"other")).is_err(), "wrong name");
        let mut tampered = stored.clone();
        let n = tampered.len();
        tampered[n / 2] ^= 0x55;
        assert!(IndexDoc::decode(&tampered, id).is_err());
    }

    #[test]
    fn writer_discard_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let mut w = PackWriter::create(d.path(), 1000).unwrap();
        w.push(Hash::of(b"x"), Codec::Raw, b"x", 1, KIND_DATA).unwrap();
        w.discard();
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0);
    }
}
