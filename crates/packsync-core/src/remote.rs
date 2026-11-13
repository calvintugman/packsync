//! The remote object layer: a typed view of the cloud on top of [`Storage`].
//!
//! ```text
//! packs/<pack-id>.pack                         immutable, content-addressed data packs
//! index/<index-id>.idx                         immutable pack indexes (blob → pack/offset)
//! instances/<instance-uuid>/commits/<snap>.snap immutable snapshot manifests (the commit graph)
//! instances/<instance-uuid>/heads/<device>.json single-writer per-device head record (hint)
//! instances/<instance-uuid>/leases/<device>.json single-writer advisory play lease
//! ```
//!
//! Nothing here is a shared mutable pointer: every cross-device fact is an immutable object with
//! a unique name, and the two kinds of mutable record are written by exactly one device each, so
//! there is no write contention and no compare-and-swap is needed (Drive has none). Everything
//! read back is verified (names are hashes) and treated as untrusted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::codec::{Codec, decode_blob};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::ids::{DeviceId, InstanceId, SnapshotId};
use crate::manifest::{MAX_MANIFEST_STORED, Manifest, SessionOutcome, SnapshotKind, parse_rfc3339};
use crate::pack::{BlobEntry, IndexDoc, IndexPack};
use crate::progress::OpCtx;
use crate::retry::{RetryPolicy, Sleeper};
use crate::storage::*;

pub const HEAD_FORMAT: &str = "packsync.head";
pub const LEASE_FORMAT: &str = "packsync.lease";
pub const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_INDEX_STORED: u64 = 256 * 1024 * 1024;
/// How many recent commits a head record carries (a hint window so a new device can see recent
/// history without downloading every manifest).
pub const HEAD_WINDOW: usize = 50;

pub fn pack_key(id: Hash) -> String {
    format!("packs/{}.pack", id.to_hex())
}
pub fn index_key(id: Hash) -> String {
    format!("index/{}.idx", id.to_hex())
}
pub fn instance_prefix(inst: InstanceId) -> String {
    format!("instances/{inst}/")
}
pub fn commits_prefix(inst: InstanceId) -> String {
    format!("instances/{inst}/commits/")
}
pub fn commit_key(inst: InstanceId, id: SnapshotId) -> String {
    format!("instances/{inst}/commits/{}.snap", id.to_hex())
}
pub fn heads_prefix(inst: InstanceId) -> String {
    format!("instances/{inst}/heads/")
}
pub fn head_key(inst: InstanceId, dev: DeviceId) -> String {
    format!("instances/{inst}/heads/{dev}.json")
}
pub fn leases_prefix(inst: InstanceId) -> String {
    format!("instances/{inst}/leases/")
}
pub fn lease_key(inst: InstanceId, dev: DeviceId) -> String {
    format!("instances/{inst}/leases/{dev}.json")
}
/// The instance's HEAD pointer: which snapshot is current, kept by compare-and-swap (see [`HeadPointer`]).
pub fn head_pointer_key(inst: InstanceId) -> String {
    format!("instances/{inst}/HEAD.json")
}
pub fn expired_prefix(inst: InstanceId) -> String {
    format!("instances/{inst}/expired/")
}
pub fn expired_key(inst: InstanceId, id: SnapshotId) -> String {
    format!("instances/{inst}/expired/{}.json", id.to_hex())
}

fn parse_name<'a>(key: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    key.strip_prefix(prefix)?.strip_suffix(suffix)
}

pub const HEAD_POINTER_FORMAT: &str = "packsync.headptr";

/// `instances/<id>/HEAD.json`: the snapshot this instance is at, moved only by compare-and-swap.
///
/// It lets a computer whose last known HEAD is still current skip every listing: "nothing changed in
/// the cloud" is one read, and publishing is the new packs, the manifest and one conditional write.
/// It is kept only on storage whose conditional writes are atomic (R2/S3); the history graph built
/// from `commits/` stays the authority — HEAD is a fast path, and every disagreement (a failed swap, a
/// HEAD this computer did not expect) sends the computer down the full path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadPointer {
    pub format: String,
    pub version: u32,
    pub instance: InstanceId,
    pub snapshot: SnapshotId,
    /// The snapshot the previous HEAD pointed at.
    pub parent: Option<SnapshotId>,
    pub device_id: DeviceId,
    pub device_name: String,
    /// RFC 3339.
    pub updated: String,
    /// The index(es) uploaded with this snapshot's new packs, when the writer knows them. A computer at
    /// `parent` reads just these instead of listing every pack and index to catch up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<Hash>,
    /// Versions published from an older HEAD that lost the race to move it (two computers played from
    /// the same version). While any is listed, no computer may trust HEAD alone: the full path sees the
    /// conflict. A version that resolves it (a descendant of all of them) clears the list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forks: Vec<SnapshotId>,
}

/// One commit as seen in a listing (before its manifest has been read).
#[derive(Debug, Clone)]
pub struct CommitListing {
    pub id: SnapshotId,
    pub info: ObjectInfo,
    /// Parents, kind etc. from provider tags, when present. **A hint**: never used for decisions
    /// that modify local files without the verified manifest.
    pub hint: Option<CommitHint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitHint {
    pub parents: Vec<SnapshotId>,
    pub kind: SnapshotKind,
    pub device: Option<DeviceId>,
    pub created: Option<String>,
}

pub fn commit_tags(m: &Manifest) -> BTreeMap<String, String> {
    let mut t = BTreeMap::new();
    t.insert("ps".into(), "1".into());
    t.insert("kind".into(), if m.kind == SnapshotKind::Crash { "crash" } else { "normal" }.into());
    for (i, p) in m.parents.iter().take(4).enumerate() {
        t.insert(format!("p{i}"), p.to_hex());
    }
    // how many parents there really are: tags hold at most four, so a reader can tell a truncated list
    t.insert("np".into(), m.parents.len().to_string());
    t.insert("dev".into(), m.device.id.to_string());
    t.insert("ts".into(), m.created.clone());
    t
}

fn hint_from_tags(tags: &BTreeMap<String, String>) -> Option<CommitHint> {
    let kind = match tags.get("kind").map(|s| s.as_str()) {
        Some("crash") => SnapshotKind::Crash,
        Some("normal") => SnapshotKind::Normal,
        _ => return None,
    };
    let mut parents = vec![];
    for i in 0..4 {
        match tags.get(&format!("p{i}")) {
            Some(p) => parents.push(p.parse::<SnapshotId>().ok()?),
            None => break,
        }
    }
    // A merge with more than four parents loses information in tags; treat as unknown (the manifest is
    // read instead). `np` is the real count; `p4` never appears but is kept for safety.
    if tags.contains_key("p4")
        || tags.get("np").and_then(|n| n.parse::<usize>().ok()).is_some_and(|n| n != parents.len())
    {
        return None;
    }
    Some(CommitHint {
        parents,
        kind,
        device: tags.get("dev").and_then(|d| DeviceId::parse(d).ok()),
        created: tags.get("ts").cloned(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentCommit {
    pub id: SnapshotId,
    pub parents: Vec<SnapshotId>,
    pub kind: SnapshotKind,
    pub created: String,
    pub device: String,
    pub device_id: DeviceId,
    pub files: u64,
    pub bytes: u64,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub outcome: Option<SessionOutcome>,
}

/// Single-writer record: "this device's latest snapshot of the instance is X" plus a window of
/// recent commits. An optimisation and a UI aid; the commit listing remains the discovery source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadRecord {
    pub format: String,
    pub version: u32,
    pub device_id: DeviceId,
    pub device_name: String,
    pub updated: String,
    pub head: SnapshotId,
    pub recent: Vec<RecentCommit>,
}

/// Advisory "someone is playing" marker. Never relied on for exclusion (see `docs/architecture.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub format: String,
    pub version: u32,
    pub device_id: DeviceId,
    pub device_name: String,
    pub session: crate::ids::SessionId,
    pub started: String,
    pub heartbeat: String,
    /// The holder promises to refresh within this many seconds; afterwards the lease is stale.
    pub ttl_secs: u32,
    pub base: Option<SnapshotId>,
}

impl LeaseRecord {
    pub fn is_stale(&self, now: time::OffsetDateTime) -> bool {
        match parse_rfc3339(&self.heartbeat) {
            Some(hb) => now - hb > time::Duration::seconds(self.ttl_secs as i64 * 2),
            None => true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct IndexSyncStats {
    pub packs_listed: usize,
    pub indexes_fetched: usize,
    pub blobs_added: u64,
    /// Indexes that mention packs the listing does not show (yet): retried next time.
    pub indexes_deferred: usize,
    /// Packs the local cache knew that are no longer in the cloud (forgotten now).
    pub packs_vanished: usize,
    /// Cached packs this listing did not show but that were seen too recently to be declared gone (the
    /// listing may lag). A publisher checks these with a direct (strongly consistent) lookup.
    pub unlisted_recent: Vec<Hash>,
    /// Every pack this (fresh) listing showed, with its size.
    pub listed: HashMap<Hash, u64>,
}

/// How long a pack must have been absent from listings before the cache forgets it. Listings can lag
/// behind uploads (Google Drive is eventually consistent), so a pack this computer uploaded or saw
/// recently is never declared gone because one listing did not show it.
pub const PACK_VANISH_GRACE_MS: i64 = 15 * 60 * 1000;

pub struct Remote {
    pub storage: Arc<dyn Storage>,
    pub db: Arc<Db>,
    pub sleeper: Arc<dyn Sleeper>,
    pub retry: RetryPolicy,
    pub vanish_grace_ms: i64,
}

impl Remote {
    pub fn new(storage: Arc<dyn Storage>, db: Arc<Db>, sleeper: Arc<dyn Sleeper>) -> Remote {
        Remote { storage, db, sleeper, retry: RetryPolicy::quick(), vanish_grace_ms: PACK_VANISH_GRACE_MS }
    }

    pub fn with_vanish_grace(mut self, ms: i64) -> Remote {
        self.vanish_grace_ms = ms;
        self
    }

    fn call<T>(&self, ctx: &OpCtx, what: &str, f: impl FnMut(u32) -> StorageResult<T>) -> Result<T> {
        self.retry.run(&*self.sleeper, ctx, what, f).map_err(Error::from)
    }

    pub fn provider_id(&self) -> String {
        self.storage.provider_id()
    }

    // ------------------------------------------------------------------ listings
    pub fn list_instances(&self, ctx: &OpCtx) -> Result<Vec<InstanceId>> {
        let items = self.call(ctx, "list instances", |_| self.storage.list("instances/", ctx))?;
        let mut out: Vec<InstanceId> = items
            .iter()
            .filter(|i| i.is_dir)
            .filter_map(|i| i.key.strip_prefix("instances/")?.strip_suffix('/')?.parse().ok())
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    pub fn list_commits(&self, inst: InstanceId, ctx: &OpCtx) -> Result<Vec<CommitListing>> {
        let prefix = commits_prefix(inst);
        let items = self.call(ctx, "list snapshots", |_| self.storage.list(&prefix, ctx))?;
        let mut out = vec![];
        for i in items {
            if i.is_dir {
                continue;
            }
            let Some(hex) = parse_name(&i.key, &prefix, ".snap") else { continue };
            let Ok(id) = hex.parse::<SnapshotId>() else { continue };
            let hint = hint_from_tags(&i.tags);
            out.push(CommitListing { id, info: i, hint });
        }
        out.sort_by_key(|c| c.id);
        out.dedup_by_key(|c| c.id);
        Ok(out)
    }

    /// Fetch, verify and parse a commit. Fails if the object is missing, does not hash to its name,
    /// is malformed, or belongs to another instance.
    pub fn get_commit(&self, inst: InstanceId, id: SnapshotId, ctx: &OpCtx) -> Result<Manifest> {
        self.get_commit_with_bytes(inst, id, ctx).map(|(m, _)| m)
    }

    /// Like [`Remote::get_commit`] but also returns the verified stored bytes (for local caching).
    pub fn get_commit_with_bytes(&self, inst: InstanceId, id: SnapshotId, ctx: &OpCtx) -> Result<(Manifest, Vec<u8>)> {
        let key = commit_key(inst, id);
        let stored = self.call(ctx, "read snapshot", |_| self.storage.get_bytes(&key, MAX_MANIFEST_STORED, ctx))?;
        ctx.add_down(stored.len() as u64);
        let (m, got) = Manifest::decode(&stored, Some(id))?;
        debug_assert_eq!(got, id);
        if m.instance.id != inst {
            return Err(Error::Corrupt(format!(
                "snapshot {} is filed under instance {inst} but belongs to {}",
                id.short(),
                m.instance.id
            )));
        }
        Ok((m, stored))
    }

    pub fn put_commit(&self, m: &Manifest, id: SnapshotId, stored: &[u8], ctx: &OpCtx) -> Result<ObjectInfo> {
        let key = commit_key(m.instance.id, id);
        let digest = ContentDigest { size: stored.len() as u64, blake3: Hash::of(stored) };
        let mut opts = PutOptions::create_only(Some(digest));
        opts.tags = commit_tags(m);
        let info =
            self.call(ctx, "publish snapshot", |_| self.storage.put(&key, PutSource::Bytes(stored), &opts, ctx))?;
        ctx.add_up(stored.len() as u64);
        Ok(info)
    }

    pub fn commit_exists(&self, inst: InstanceId, id: SnapshotId, ctx: &OpCtx) -> Result<bool> {
        let key = commit_key(inst, id);
        Ok(self.call(ctx, "check snapshot", |_| self.storage.stat(&key, ctx))?.is_some())
    }

    // ------------------------------------------------------------------ packs & indexes
    pub fn list_packs(&self, ctx: &OpCtx) -> Result<HashMap<Hash, ObjectInfo>> {
        let items = self.call(ctx, "list packs", |_| self.storage.list("packs/", ctx))?;
        let mut out = HashMap::new();
        for i in items {
            if i.is_dir {
                continue;
            }
            let Some(hex) = parse_name(&i.key, "packs/", ".pack") else { continue };
            if let Ok(h) = Hash::from_hex(hex) {
                out.entry(h).or_insert(i);
            }
        }
        Ok(out)
    }

    /// A pack's own table of contents (its footer), read with two small ranged reads and verified. Packs
    /// are self-describing: this works when no index object describes the pack.
    pub fn read_pack_footer(&self, id: Hash, size: u64, ctx: &OpCtx) -> Result<Vec<crate::pack::BlobEntry>> {
        let key = pack_key(id);
        crate::pack::read_footer(size, |off, len| {
            self.call(ctx, "read pack footer", |_| {
                let r = self.storage.get(&key, Some((off, len)), ctx)?;
                let mut buf = Vec::with_capacity(len as usize);
                r.take(len).read_to_end(&mut buf).map_err(StorageError::io)?;
                Ok(buf)
            })
        })
    }

    pub fn stat_pack(&self, id: Hash, ctx: &OpCtx) -> Result<Option<ObjectInfo>> {
        let key = pack_key(id);
        self.call(ctx, "check pack", |_| self.storage.stat(&key, ctx))
    }

    /// Upload a staged pack file. `id` must equal the BLAKE3 of the file (it is the object's name).
    pub fn put_pack(&self, path: &Path, id: Hash, size: u64, ctx: &OpCtx) -> Result<ObjectInfo> {
        let key = pack_key(id);
        let opts = PutOptions::create_only(Some(ContentDigest { size, blake3: id }))
            .with_tag("ps", "1")
            .with_tag("kind", "pack");
        let info =
            self.call(ctx, "upload pack", |_| self.storage.put(&key, PutSource::File { path, len: size }, &opts, ctx))?;
        Ok(info)
    }

    pub fn put_index(&self, doc: &IndexDoc, ctx: &OpCtx) -> Result<Hash> {
        let (id, stored) = doc.encode()?;
        let key = index_key(id);
        let opts =
            PutOptions::create_only(Some(ContentDigest { size: stored.len() as u64, blake3: Hash::of(&stored) }))
                .with_tag("ps", "1")
                .with_tag("kind", "index");
        self.call(ctx, "upload index", |_| self.storage.put(&key, PutSource::Bytes(&stored), &opts, ctx))?;
        ctx.add_up(stored.len() as u64);
        Ok(id)
    }

    /// Index objects with their provider metadata (age, tags) for cleanup decisions.
    pub fn list_index_infos(&self, ctx: &OpCtx) -> Result<Vec<(Hash, ObjectInfo)>> {
        let items = self.call(ctx, "list indexes", |_| self.storage.list("index/", ctx))?;
        let mut out: Vec<(Hash, ObjectInfo)> = items
            .into_iter()
            .filter(|i| !i.is_dir)
            .filter_map(|i| Some((Hash::from_hex(parse_name(&i.key, "index/", ".idx")?).ok()?, i)))
            .collect();
        out.sort_by_key(|(h, _)| *h);
        out.dedup_by_key(|(h, _)| *h);
        Ok(out)
    }

    /// Keys directly under `prefix` (`packs/`, `index/`) that are not objects PackSync names itself.
    pub fn list_unrecognised(&self, prefix: &str, suffix: &str, ctx: &OpCtx) -> Result<Vec<String>> {
        let items = self.call(ctx, "list objects", |_| self.storage.list(prefix, ctx))?;
        Ok(items
            .into_iter()
            .filter(|i| !i.is_dir)
            .filter(|i| parse_name(&i.key, prefix, suffix).and_then(|h| Hash::from_hex(h).ok()).is_none())
            .map(|i| i.key)
            .collect())
    }

    /// Delete one object PackSync created (the provider's trash is used when it has one).
    pub fn delete_object(&self, key: &str, ctx: &OpCtx) -> Result<()> {
        self.call(ctx, "delete object", |_| self.storage.delete(key, DeleteMode::Trash, ctx))
    }

    pub fn put_expired(&self, rec: &crate::gc::ExpiredRecord, ctx: &OpCtx) -> Result<()> {
        let key = expired_key(rec.instance, rec.snapshot);
        let bytes = serde_json::to_vec(rec)?;
        let opts = PutOptions::replace().with_tag("ps", "1").with_tag("kind", "expired");
        self.call(ctx, "mark snapshot as released", |_| self.storage.put(&key, PutSource::Bytes(&bytes), &opts, ctx))?;
        Ok(())
    }

    /// Snapshots of `inst` whose content was released by a cleanup.
    pub fn list_expired(&self, inst: InstanceId, ctx: &OpCtx) -> Result<HashSet<SnapshotId>> {
        let prefix = expired_prefix(inst);
        let items = self.call(ctx, "list released snapshots", |_| self.storage.list(&prefix, ctx))?;
        Ok(items
            .iter()
            .filter(|i| !i.is_dir)
            .filter_map(|i| parse_name(&i.key, &prefix, ".json")?.parse::<SnapshotId>().ok())
            .collect())
    }

    pub fn list_index_ids(&self, ctx: &OpCtx) -> Result<Vec<Hash>> {
        let items = self.call(ctx, "list indexes", |_| self.storage.list("index/", ctx))?;
        let mut out: Vec<Hash> = items
            .iter()
            .filter(|i| !i.is_dir)
            .filter_map(|i| Hash::from_hex(parse_name(&i.key, "index/", ".idx")?).ok())
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    pub fn get_index(&self, id: Hash, ctx: &OpCtx) -> Result<IndexDoc> {
        let key = index_key(id);
        let stored = self.call(ctx, "read index", |_| self.storage.get_bytes(&key, MAX_INDEX_STORED, ctx))?;
        ctx.add_down(stored.len() as u64);
        IndexDoc::decode(&stored, id)
    }

    /// Bring the local blob-location cache up to date with the cloud: list packs and indexes, fetch
    /// the indexes we have not seen and merge the blobs of packs that exist with the recorded size.
    pub fn sync_index_cache(&self, ctx: &OpCtx) -> Result<IndexSyncStats> {
        let mut stats = IndexSyncStats::default();
        let packs = self.list_packs(ctx)?;
        stats.packs_listed = packs.len();
        stats.listed = packs.iter().map(|(id, i)| (*id, i.size)).collect();
        ctx.mark("index cache: record listed packs");
        self.db.packs_put_many(packs.iter().map(|(id, i)| (*id, i.size, i.provider_ref.as_deref(), i.created_ms)))?;
        ctx.mark("index cache: list and read new indexes");
        // Packs the cache relies on that the cloud no longer has (a cleanup on another computer, files
        // removed from Drive): forget them, so nothing new deduplicates against them, and have every
        // instance re-read its files and check its next publication in full.
        let cutoff = crate::db::now_ms() - self.vanish_grace_ms;
        for (id, seen_ms) in self.db.packs_seen()? {
            if packs.contains_key(&id) {
                continue;
            }
            if seen_ms <= cutoff {
                self.db.blobs_forget_pack(id)?;
                stats.packs_vanished += 1;
            } else {
                stats.unlisted_recent.push(id);
            }
        }
        if stats.packs_vanished > 0 {
            self.db.mark_cloud_data_lost()?;
            self.db.event(
                None,
                "warn",
                "cloud_data_lost",
                &format!(
                    "{} data pack(s) this computer knew about are no longer in the cloud (a cleanup on another computer?); \
                     the next save re-reads every file and re-uploads what is missing",
                    stats.packs_vanished
                ),
            );
        }
        let mut superseded: HashSet<Hash> = HashSet::new();
        let ids = self.list_index_ids(ctx)?;
        for id in ids {
            ctx.check()?;
            if self.db.index_seen(id)? {
                continue;
            }
            let doc = match self.get_index(id, ctx) {
                Ok(d) => d,
                Err(Error::Storage(StorageError::NotFound(_))) => continue, // listed, not readable yet
                Err(e @ (Error::Corrupt(_) | Error::Invalid { .. })) => {
                    // A damaged index must not break syncing; its blobs are simply not trusted.
                    self.db.event(None, "warn", "index_damaged", &format!("ignoring index {}: {e}", id.short()));
                    continue;
                }
                Err(e) => return Err(e),
            };
            stats.indexes_fetched += 1;
            let mut complete = true;
            for p in &doc.packs {
                match packs.get(&p.id) {
                    Some(info) if info.size == p.size => {
                        stats.blobs_added += self.db.blobs_put(p.id, &p.blobs)?;
                    }
                    _ => complete = false,
                }
            }
            if complete {
                self.db.index_mark_seen(id)?;
                superseded.extend(doc.supersedes.iter().copied());
            } else {
                stats.indexes_deferred += 1;
            }
        }
        for s in superseded {
            self.db.index_mark_seen(s)?;
        }
        Ok(stats)
    }

    /// Look up, directly, the cached packs a listing did not show ([`IndexSyncStats::unlisted_recent`]) and
    /// forget those that are really gone. Returns how many were gone.
    pub fn confirm_unlisted(&self, stats: &IndexSyncStats, ctx: &OpCtx) -> Result<usize> {
        let mut gone = 0;
        for p in &stats.unlisted_recent {
            if self.stat_pack(*p, ctx)?.is_none() {
                self.db.blobs_forget_pack(*p)?;
                gone += 1;
            }
        }
        if gone > 0 {
            self.db.mark_cloud_data_lost()?;
        }
        Ok(gone)
    }

    /// Record that this computer wrote index `id` itself: every pack it lists was just uploaded (or found
    /// present with the right size) and its blobs are already in the local cache through
    /// [`Self::note_uploaded_pack`], so the next `sync_index_cache` must not download it again. Only call
    /// this once all the index's packs are in the cloud.
    pub fn note_uploaded_index(&self, id: Hash) -> Result<()> {
        self.db.index_mark_seen(id)
    }

    /// Record a pack we just uploaded and verified (no need to wait for a listing to show it).
    pub fn note_uploaded_pack(&self, p: &IndexPack, info: &ObjectInfo) -> Result<()> {
        self.db.pack_put(p.id, p.size, info.provider_ref.as_deref(), info.created_ms)?;
        self.db.blobs_put(p.id, &p.blobs).map(|_| ())
    }

    // ------------------------------------------------------------------ blobs
    /// Read one blob through the cached index, verifying size, codec and hash.
    pub fn read_blob(&self, h: Hash, ctx: &OpCtx) -> Result<Vec<u8>> {
        let (pack, e) = self
            .db
            .blob_lookup(h)?
            .ok_or_else(|| Error::Corrupt(format!("blob {} is not listed in any pack index", h.short())))?;
        let key = pack_key(pack);
        let stored = self
            .call(ctx, "read blob", |_| {
                let r = self.storage.get(&key, Some((e.offset, e.stored_len as u64)), ctx)?;
                let mut buf = Vec::with_capacity(e.stored_len as usize);
                r.take(e.stored_len as u64).read_to_end(&mut buf).map_err(StorageError::io)?;
                if buf.len() != e.stored_len as usize {
                    return Err(StorageError::Transient(format!("short read from {key}")));
                }
                Ok(buf)
            })
            .map_err(|e| self.missing_pack_is_corruption(pack, e))?;
        ctx.add_down(stored.len() as u64);
        verify_blob(&e, &stored)
    }

    fn missing_pack_is_corruption(&self, pack: Hash, e: Error) -> Error {
        if matches!(e, Error::Storage(StorageError::NotFound(_))) {
            let _ = self.db.blobs_forget_pack(pack);
            let _ = self.db.mark_cloud_data_lost();
            return Error::Corrupt(format!(
                "pack {} is missing from the cloud (deleted outside PackSync?)",
                pack.short()
            ));
        }
        e
    }

    /// Where a blob lives (its pack and offset in it), if the cached index knows it. Used to download
    /// files in the order their data is stored.
    pub fn blob_location(&self, h: Hash) -> Result<Option<(Hash, u64)>> {
        Ok(self.db.blob_lookup(h)?.map(|(pack, e)| (pack, e.offset)))
    }

    /// Fetch `wanted` blobs into `dir/<hash>` (verified, atomic), grouped by pack with coalesced
    /// ranged reads. Existing store files of the right size are reused (resumability).
    pub fn fetch_blobs(&self, wanted: &[Hash], dir: &Path, concurrency: usize, ctx: &OpCtx) -> Result<FetchStats> {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        let mut by_pack: BTreeMap<Hash, Vec<BlobEntry>> = BTreeMap::new();
        let mut missing = vec![];
        let mut stats = FetchStats::default();
        let uniq: HashSet<Hash> = wanted.iter().copied().collect();
        for h in uniq {
            match self.db.blob_lookup(h)? {
                Some((pack, e)) => {
                    let have = dir.join(h.to_hex());
                    if std::fs::metadata(&have).map(|m| m.len() == e.raw_len as u64).unwrap_or(false) {
                        stats.reused += 1;
                        continue;
                    }
                    by_pack.entry(pack).or_default().push(e);
                }
                None => missing.push(h),
            }
        }
        if !missing.is_empty() {
            // The data may have been uploaded with another instance, a crash save or an index that a
            // cleanup merged — none of which this computer has read. Read every index once before
            // calling the snapshot incomplete.
            self.sync_index_cache(ctx)?;
            let mut still = vec![];
            for h in missing {
                match self.db.blob_lookup(h)? {
                    Some((pack, e)) => by_pack.entry(pack).or_default().push(e),
                    None => still.push(h),
                }
            }
            if !still.is_empty() {
                return Err(Error::Corrupt(format!(
                    "{} blob(s) are not listed in any pack index (first: {}); the snapshot is incomplete",
                    still.len(),
                    still[0].short()
                )));
            }
        }
        let packs_sizes = self.db.packs_known()?;
        let jobs: Vec<(Hash, Vec<BlobEntry>)> = by_pack.into_iter().collect();
        let (tx, rx) = crossbeam_channel::unbounded();
        for j in jobs {
            tx.send(j).ok();
        }
        drop(tx);
        let first_err: parking_lot::Mutex<Option<Error>> = parking_lot::Mutex::new(None);
        let fetched = std::sync::atomic::AtomicU64::new(0);
        let nblobs = std::sync::atomic::AtomicU64::new(0);
        let (first_err_r, fetched_r, nblobs_r, sizes_r) = (&first_err, &fetched, &nblobs, &packs_sizes);
        std::thread::scope(|s| {
            for _ in 0..concurrency.max(1) {
                let rx = rx.clone();
                s.spawn(move || {
                    while let Ok((pack, mut entries)) = rx.recv() {
                        if first_err_r.lock().is_some() || ctx.is_cancelled() {
                            return;
                        }
                        entries.sort_by_key(|e| e.offset);
                        let size = sizes_r.get(&pack).copied();
                        match self.fetch_pack_blobs(pack, size, &entries, dir, ctx) {
                            Ok((b, n)) => {
                                fetched_r.fetch_add(b, std::sync::atomic::Ordering::Relaxed);
                                nblobs_r.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(e) => {
                                first_err_r.lock().get_or_insert(e);
                                return;
                            }
                        }
                    }
                });
            }
        });
        if let Some(e) = first_err.into_inner() {
            return Err(e);
        }
        ctx.check()?;
        stats.fetched_blobs = nblobs.load(std::sync::atomic::Ordering::Relaxed);
        stats.fetched_bytes = fetched.load(std::sync::atomic::Ordering::Relaxed);
        Ok(stats)
    }

    fn fetch_pack_blobs(
        &self,
        pack: Hash,
        pack_size: Option<u64>,
        entries: &[BlobEntry],
        dir: &Path,
        ctx: &OpCtx,
    ) -> Result<(u64, u64)> {
        const GAP: u64 = 64 * 1024;
        const MAX_RANGE: u64 = 16 * 1024 * 1024;
        // Coalesce into ranges of at most MAX_RANGE: every range is read into memory whole, and several
        // workers read at once. Small gaps are read through; a mostly-needed pack is read through entirely
        // (fewer requests), but still in MAX_RANGE pieces.
        let coalesce = |gap: u64| {
            let mut ranges: Vec<(u64, u64, Vec<&BlobEntry>)> = vec![];
            for e in entries {
                let end = e.offset + e.stored_len as u64;
                match ranges.last_mut() {
                    Some((s, r_end, v)) if e.offset <= r_end.saturating_add(gap) && end - *s <= MAX_RANGE => {
                        *r_end = (*r_end).max(end);
                        v.push(e);
                    }
                    _ => ranges.push((e.offset, end, vec![e])),
                }
            }
            ranges
        };
        let mut ranges = coalesce(GAP);
        let needed: u64 = entries.iter().map(|e| e.stored_len as u64).sum();
        if let Some(sz) = pack_size
            && ranges.len() > 1
            && needed * 10 >= sz * 6
        {
            ranges = coalesce(u64::MAX);
        }
        let key = pack_key(pack);
        let (mut bytes, mut n) = (0u64, 0u64);
        for (start, end, es) in ranges {
            ctx.check()?;
            let len = end - start;
            let mut produced: Vec<(Hash, Vec<u8>)> = vec![];
            self.call(ctx, "download blobs", |_| {
                produced.clear();
                let mut r = self.storage.get(&key, Some((start, len)), ctx)?;
                let mut pos = start;
                for e in &es {
                    if e.offset > pos {
                        let skip = e.offset - pos;
                        std::io::copy(&mut (&mut r).take(skip), &mut std::io::sink()).map_err(StorageError::io)?;
                    }
                    let mut buf = Vec::with_capacity(e.stored_len as usize);
                    (&mut r).take(e.stored_len as u64).read_to_end(&mut buf).map_err(StorageError::io)?;
                    if buf.len() != e.stored_len as usize {
                        return Err(StorageError::Transient(format!("short read from {key}")));
                    }
                    pos = e.offset + e.stored_len as u64;
                    ctx.add_down(buf.len() as u64);
                    // Verification failures are corruption, not transient: surface them distinctly.
                    match verify_blob(e, &buf) {
                        Ok(raw) => produced.push((e.hash, raw)),
                        Err(err) => return Err(StorageError::Corrupt(err.to_string())),
                    }
                }
                Ok(())
            })
            .map_err(|e| self.missing_pack_is_corruption(pack, e))?;
            for (h, raw) in produced.drain(..) {
                write_store_file(dir, h, &raw)?;
                bytes += raw.len() as u64;
                n += 1;
            }
        }
        Ok((bytes, n))
    }

    // ------------------------------------------------------------------ heads and leases
    /// The HEAD pointer and its version (`None` if there is none yet).
    pub fn get_head_pointer(&self, inst: InstanceId, ctx: &OpCtx) -> Result<Option<(HeadPointer, String)>> {
        let key = head_pointer_key(inst);
        let Some((bytes, info)) =
            self.call(ctx, "read HEAD", |_| self.storage.get_with_info(&key, MAX_RECORD_BYTES, ctx))?
        else {
            return Ok(None);
        };
        let p: HeadPointer =
            serde_json::from_slice(&bytes).map_err(|e| Error::invalid("HEAD pointer", e.to_string()))?;
        if p.format != HEAD_POINTER_FORMAT || p.version != 1 || p.instance != inst {
            return Err(Error::invalid("HEAD pointer", "not a version 1 PackSync HEAD of this instance"));
        }
        let v = info.version.ok_or_else(|| Error::invalid("HEAD pointer", "the storage reports no version"))?;
        Ok(Some((p, v)))
    }

    /// Move the HEAD pointer only if it is still at `expected` (`None`: only if there is none). Returns
    /// the new version; `Ok(None)` when someone else moved it first (nothing was changed).
    pub fn swap_head_pointer(&self, p: &HeadPointer, expected: Option<String>, ctx: &OpCtx) -> Result<Option<String>> {
        let bytes = serde_json::to_vec(p)?;
        let key = head_pointer_key(p.instance);
        let opts = PutOptions::if_version(expected).with_tag("ps", "1").with_tag("kind", "headptr");
        match self.storage.put(&key, PutSource::Bytes(&bytes), &opts, ctx) {
            Ok(info) => {
                Ok(Some(info.version.ok_or_else(|| Error::invalid("HEAD pointer", "the storage reports no version"))?))
            }
            Err(StorageError::Conflict(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn put_head(&self, inst: InstanceId, rec: &HeadRecord, ctx: &OpCtx) -> Result<()> {
        let bytes = serde_json::to_vec(rec)?;
        let key = head_key(inst, rec.device_id);
        self.call(ctx, "update head record", |_| {
            self.storage.put(&key, PutSource::Bytes(&bytes), &PutOptions::replace().with_tag("ps", "1"), ctx)
        })?;
        Ok(())
    }

    pub fn list_heads(&self, inst: InstanceId, ctx: &OpCtx) -> Result<Vec<HeadRecord>> {
        let prefix = heads_prefix(inst);
        let items = self.call(ctx, "list head records", |_| self.storage.list(&prefix, ctx))?;
        let mut out = vec![];
        for i in items.into_iter().filter(|i| !i.is_dir && i.key.ends_with(".json")) {
            match self.call(ctx, "read head record", |_| self.storage.get_bytes(&i.key, MAX_RECORD_BYTES, ctx)) {
                Ok(b) => match serde_json::from_slice::<HeadRecord>(&b) {
                    Ok(r) if r.format == HEAD_FORMAT && r.version == 1 && r.recent.len() <= HEAD_WINDOW * 2 => {
                        out.push(r)
                    }
                    _ => self.db.event(
                        Some(inst),
                        "warn",
                        "head_record_ignored",
                        &format!("ignoring malformed head record {}", i.key),
                    ),
                },
                Err(Error::Storage(StorageError::NotFound(_))) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    pub fn put_lease(&self, inst: InstanceId, rec: &LeaseRecord, ctx: &OpCtx) -> Result<()> {
        let bytes = serde_json::to_vec(rec)?;
        let key = lease_key(inst, rec.device_id);
        self.call(ctx, "update lease", |_| {
            self.storage.put(&key, PutSource::Bytes(&bytes), &PutOptions::replace().with_tag("ps", "1"), ctx)
        })?;
        Ok(())
    }

    pub fn delete_lease(&self, inst: InstanceId, dev: DeviceId, ctx: &OpCtx) -> Result<()> {
        let key = lease_key(inst, dev);
        self.call(ctx, "release lease", |_| self.storage.delete(&key, DeleteMode::Permanent, ctx))
    }

    pub fn list_leases(&self, inst: InstanceId, ctx: &OpCtx) -> Result<Vec<LeaseRecord>> {
        let prefix = leases_prefix(inst);
        let items = self.call(ctx, "list leases", |_| self.storage.list(&prefix, ctx))?;
        let mut out = vec![];
        for i in items.into_iter().filter(|i| !i.is_dir && i.key.ends_with(".json")) {
            if let Ok(b) = self.call(ctx, "read lease", |_| self.storage.get_bytes(&i.key, MAX_RECORD_BYTES, ctx))
                && let Ok(r) = serde_json::from_slice::<LeaseRecord>(&b)
                && r.format == LEASE_FORMAT
                && r.version == 1
            {
                out.push(r);
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Default, Clone)]
pub struct FetchStats {
    pub fetched_blobs: u64,
    pub fetched_bytes: u64,
    pub reused: u64,
}

/// Decode a stored blob and verify it hashes to its name.
pub fn verify_blob(e: &BlobEntry, stored: &[u8]) -> Result<Vec<u8>> {
    let raw = decode_blob(Codec::from_u8(e.codec)?, stored, e.raw_len as usize)?;
    if Hash::of(&raw) != e.hash {
        return Err(Error::Corrupt(format!(
            "blob {} failed its content hash (damaged in storage or in transit)",
            e.hash.short()
        )));
    }
    Ok(raw)
}

pub fn write_store_file(dir: &Path, h: Hash, raw: &[u8]) -> Result<PathBuf> {
    let dest = dir.join(h.to_hex());
    let tmp = dir.join(format!(".{}.part", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, raw).map_err(|e| Error::io(format!("writing {}", tmp.display()), e))?;
    std::fs::rename(&tmp, &dest).map_err(|e| Error::io(format!("renaming into {}", dest.display()), e))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tags hold at most four parents. A merge with more (up to eight are allowed) must not be read from
    /// its tags as if four were all of them: the hint is refused and the manifest is read instead.
    #[test]
    fn a_merge_with_more_parents_than_tags_hold_is_not_trusted_from_its_tags() {
        let mut m = crate::manifest::testutil::sample();
        m.parents = (0..5u8).map(|i| SnapshotId(Hash::of(&[i]))).collect();
        assert!(hint_from_tags(&commit_tags(&m)).is_none());
        m.parents.truncate(2);
        assert_eq!(hint_from_tags(&commit_tags(&m)).unwrap().parents, m.parents);
        m.parents.clear();
        assert!(hint_from_tags(&commit_tags(&m)).unwrap().parents.is_empty());
    }
    use crate::codec::encode_blob;
    use crate::pack::{KIND_DATA, PackWriter, read_footer_from_file};
    use crate::retry::RecordingSleeper;
    use crate::rules::Compress;
    use crate::storage_fault::{Fault, FaultPlan, FaultRule, FaultyStorage, OpKind, When};
    use crate::storage_local::LocalDirStorage;

    struct Env {
        _dir: tempfile::TempDir,
        local: Arc<LocalDirStorage>,
        remote: Remote,
        ctx: OpCtx,
    }

    fn env_with(wrap: impl FnOnce(Arc<dyn Storage>) -> Arc<dyn Storage>) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalDirStorage::new(dir.path().join("remote")).unwrap());
        let storage = wrap(local.clone());
        let remote = Remote::new(storage, Arc::new(Db::open_memory().unwrap()), Arc::new(RecordingSleeper::default()));
        Env { _dir: dir, local, remote, ctx: OpCtx::detached() }
    }
    fn env() -> Env {
        env_with(|s| s)
    }

    fn make_pack(dir: &Path, n: u64) -> (crate::pack::FinishedPack, Vec<(Hash, Vec<u8>)>) {
        let mut w = PackWriter::create(dir, 1 << 20).unwrap();
        let mut blobs = vec![];
        for i in 0..n {
            let raw: Vec<u8> = (0..3000 + i * 100).map(|x| (x * (i + 3)) as u8).collect();
            let h = Hash::of(&raw);
            let (c, enc) = encode_blob(&raw, Compress::Auto, "x.json", 3);
            w.push(h, c, &enc, raw.len(), KIND_DATA).unwrap();
            blobs.push((h, raw));
        }
        (w.finish().unwrap(), blobs)
    }

    fn upload(e: &Env, pack: &crate::pack::FinishedPack) -> Hash {
        let info = e.remote.put_pack(&pack.path, pack.id, pack.size, &e.ctx).unwrap();
        let ip = IndexPack { id: pack.id, size: pack.size, blobs: pack.entries.clone() };
        e.remote.note_uploaded_pack(&ip, &info).unwrap();
        e.remote.put_index(&IndexDoc::new(vec![ip]), &e.ctx).unwrap()
    }

    #[test]
    fn pack_index_blob_roundtrip_through_a_fresh_cache() {
        let e = env();
        let d = tempfile::tempdir().unwrap();
        let (pack, blobs) = make_pack(d.path(), 20);
        upload(&e, &pack);

        // a second device: empty DB, learns everything from the cloud
        let other =
            Remote::new(e.local.clone(), Arc::new(Db::open_memory().unwrap()), Arc::new(RecordingSleeper::default()));
        let st = other.sync_index_cache(&e.ctx).unwrap();
        assert_eq!((st.packs_listed, st.indexes_fetched, st.blobs_added), (1, 1, 20));
        for (h, raw) in &blobs {
            assert_eq!(&other.read_blob(*h, &e.ctx).unwrap(), raw);
        }
        // idempotent
        assert_eq!(other.sync_index_cache(&e.ctx).unwrap().indexes_fetched, 0);
    }

    #[test]
    fn corrupt_blob_in_storage_is_detected_on_read() {
        let e = env();
        let d = tempfile::tempdir().unwrap();
        let (pack, blobs) = make_pack(d.path(), 5);
        upload(&e, &pack);
        // flip a byte in the middle of the stored pack
        let p = e.local.path_for(&pack_key(pack.id));
        let mut b = std::fs::read(&p).unwrap();
        b[pack.entries[2].offset as usize + 10] ^= 0xFF;
        std::fs::write(&p, b).unwrap();
        let err = e.remote.read_blob(blobs[2].0, &e.ctx).unwrap_err();
        assert!(matches!(err, Error::Corrupt(_) | Error::Invalid { .. }), "{err}");
        assert!(e.remote.read_blob(blobs[0].0, &e.ctx).is_ok(), "undamaged blobs stay readable");
    }

    #[test]
    fn missing_pack_is_reported_as_corruption_and_forgotten() {
        let e = env();
        let d = tempfile::tempdir().unwrap();
        let (pack, blobs) = make_pack(d.path(), 3);
        upload(&e, &pack);
        std::fs::remove_file(e.local.path_for(&pack_key(pack.id))).unwrap();
        let err = e.remote.read_blob(blobs[0].0, &e.ctx).unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)), "{err}");
        assert!(
            !e.remote.db.blob_known(blobs[0].0).unwrap(),
            "stale cache entries are dropped so the blob is re-uploaded"
        );
    }

    #[test]
    fn fetch_blobs_coalesces_verifies_and_resumes() {
        let e = env();
        let d = tempfile::tempdir().unwrap();
        let (pack, blobs) = make_pack(d.path(), 40);
        upload(&e, &pack);
        let store = d.path().join("store");
        let want: Vec<Hash> = blobs.iter().step_by(3).map(|b| b.0).collect();
        let st = e.remote.fetch_blobs(&want, &store, 3, &e.ctx).unwrap();
        assert_eq!(st.fetched_blobs, want.len() as u64);
        for (h, raw) in blobs.iter().step_by(3) {
            assert_eq!(&std::fs::read(store.join(h.to_hex())).unwrap(), raw);
        }
        // second call reuses everything
        let st2 = e.remote.fetch_blobs(&want, &store, 3, &e.ctx).unwrap();
        assert_eq!((st2.fetched_blobs, st2.reused), (0, want.len() as u64));
        // unknown blob → clear error
        let err = e.remote.fetch_blobs(&[Hash::of(b"nope")], &store, 1, &e.ctx).unwrap_err();
        assert!(err.to_string().contains("not listed in any pack index"), "{err}");
    }

    /// Every coalesced read is held in memory whole, by several workers at once, so none may exceed 16 MiB —
    /// also when a pack is mostly needed and read through (it used to become one read of the whole pack,
    /// up to 256 MiB).
    #[test]
    fn a_mostly_needed_big_pack_is_still_read_in_bounded_pieces() {
        let meter = std::sync::Mutex::new(None);
        let e = env_with(|s| {
            let (m, meter_handle) = crate::storage_meter::MeteredStorage::new(s);
            *meter.lock().unwrap() = Some(meter_handle);
            m
        });
        let meter = meter.into_inner().unwrap().unwrap();
        let d = tempfile::tempdir().unwrap();
        let mut w = PackWriter::create(d.path(), 1 << 30).unwrap();
        let mut blobs = vec![];
        let mut rng = fastrand::Rng::with_seed(7);
        for _ in 0..24 {
            let mut raw = vec![0u8; 1 << 20]; // 1 MiB, incompressible
            rng.fill(&mut raw);
            let h = Hash::of(&raw);
            let (c, enc) = encode_blob(&raw, Compress::Never, "x.bin", 3);
            w.push(h, c, &enc, raw.len(), KIND_DATA).unwrap();
            blobs.push(h);
        }
        let pack = w.finish().unwrap();
        upload(&e, &pack);
        let want: Vec<Hash> = blobs.iter().copied().filter(|h| h.0[0] % 5 != 0).collect(); // most of it
        let before = meter.snapshot();
        e.remote.fetch_blobs(&want, &d.path().join("store"), 1, &e.ctx).unwrap();
        let used = meter.snapshot().since(&before);
        let reads = used.by_prefix.get("packs").map(|p| p.gets).unwrap_or(0);
        assert!(reads >= 2, "a 24 MiB pack is not read in one piece: {reads} read(s)");
        assert!(used.bytes_down <= pack.size, "and nothing is read twice");
    }

    #[test]
    fn fetch_detects_damage_inside_a_range() {
        let e = env();
        let d = tempfile::tempdir().unwrap();
        let (pack, blobs) = make_pack(d.path(), 10);
        upload(&e, &pack);
        let p = e.local.path_for(&pack_key(pack.id));
        let mut b = std::fs::read(&p).unwrap();
        b[pack.entries[4].offset as usize + 5] ^= 0x01;
        std::fs::write(&p, b).unwrap();
        let all: Vec<Hash> = blobs.iter().map(|b| b.0).collect();
        let err = e.remote.fetch_blobs(&all, &d.path().join("store"), 2, &e.ctx).unwrap_err();
        assert!(err.to_string().contains("integrity") || matches!(err, Error::Corrupt(_)), "{err}");
    }

    #[test]
    fn transient_storage_errors_are_retried_without_real_sleeping() {
        let plan = FaultPlan::new(1);
        plan.add(FaultRule::new(OpKind::Put, When::Nth(1), Fault::Error(StorageError::Transient("503".into()))));
        plan.add(FaultRule::new(
            OpKind::Get,
            When::Nth(1),
            Fault::Error(StorageError::RateLimited { retry_after: None }),
        ));
        let e = env_with(|s| FaultyStorage::new(s, plan.clone()));
        let d = tempfile::tempdir().unwrap();
        let (pack, blobs) = make_pack(d.path(), 3);
        upload(&e, &pack);
        assert_eq!(&e.remote.read_blob(blobs[1].0, &e.ctx).unwrap(), &blobs[1].1);
        assert_eq!(plan.triggered(), 2);
    }

    #[test]
    fn ambiguous_put_is_idempotent_on_retry() {
        // The pack is stored but the client sees an error: the retry must succeed, not conflict.
        let plan = FaultPlan::new(2);
        plan.add(FaultRule::new(
            OpKind::Put,
            When::Nth(1),
            Fault::ErrorAfterApply(StorageError::Offline("timeout".into())),
        ));
        let e = env_with(|s| FaultyStorage::new(s, plan.clone()));
        let d = tempfile::tempdir().unwrap();
        let (pack, _) = make_pack(d.path(), 3);
        e.remote.put_pack(&pack.path, pack.id, pack.size, &e.ctx).unwrap();
        assert_eq!(e.remote.list_packs(&e.ctx).unwrap().len(), 1);
    }

    #[test]
    fn index_for_a_pack_that_is_not_listed_yet_is_deferred_not_lost() {
        let e = env();
        let d = tempfile::tempdir().unwrap();
        let (pack, blobs) = make_pack(d.path(), 3);
        let ip = IndexPack { id: pack.id, size: pack.size, blobs: pack.entries.clone() };
        e.remote.put_index(&IndexDoc::new(vec![ip]), &e.ctx).unwrap(); // index first (pack not there)
        let st = e.remote.sync_index_cache(&e.ctx).unwrap();
        assert_eq!((st.indexes_deferred, st.blobs_added), (1, 0));
        e.remote.put_pack(&pack.path, pack.id, pack.size, &e.ctx).unwrap();
        let st = e.remote.sync_index_cache(&e.ctx).unwrap();
        assert_eq!(st.blobs_added, 3, "the deferred index is picked up once its pack shows up");
        assert!(e.remote.read_blob(blobs[0].0, &e.ctx).is_ok());
    }

    #[test]
    fn damaged_index_does_not_break_syncing() {
        let e = env();
        let id = Hash::of(b"fake");
        e.local.put(&index_key(id), PutSource::Bytes(b"not an index"), &PutOptions::replace(), &e.ctx).unwrap();
        assert!(e.remote.sync_index_cache(&e.ctx).is_ok());
    }

    #[test]
    fn commits_head_and_lease_records_roundtrip_and_reject_misfiled_manifests() {
        use crate::manifest::testutil::sample;
        let e = env();
        let m = sample();
        let (id, stored) = m.encode().unwrap();
        e.remote.put_commit(&m, id, &stored, &e.ctx).unwrap();
        e.remote.put_commit(&m, id, &stored, &e.ctx).unwrap(); // idempotent
        let l = e.remote.list_commits(m.instance.id, &e.ctx).unwrap();
        assert_eq!(l.len(), 1);
        let hint = l[0].hint.clone().expect("tags travel with the object");
        assert_eq!(hint.parents, m.parents);
        assert_eq!(hint.kind, SnapshotKind::Normal);
        assert_eq!(e.remote.get_commit(m.instance.id, id, &e.ctx).unwrap(), m);
        assert_eq!(e.remote.list_instances(&e.ctx).unwrap(), vec![m.instance.id]);

        // the same manifest filed under another instance is rejected
        let other = InstanceId::new();
        e.local.put(&commit_key(other, id), PutSource::Bytes(&stored), &PutOptions::replace(), &e.ctx).unwrap();
        assert!(matches!(e.remote.get_commit(other, id, &e.ctx), Err(Error::Corrupt(_))));

        // head + lease
        let rec = HeadRecord {
            format: HEAD_FORMAT.into(),
            version: 1,
            device_id: m.device.id,
            device_name: "d".into(),
            updated: crate::manifest::now_rfc3339(),
            head: id,
            recent: vec![],
        };
        e.remote.put_head(m.instance.id, &rec, &e.ctx).unwrap();
        e.remote.put_head(m.instance.id, &rec, &e.ctx).unwrap(); // Replace is repeatable
        assert_eq!(e.remote.list_heads(m.instance.id, &e.ctx).unwrap().len(), 1);
        let lease = LeaseRecord {
            format: LEASE_FORMAT.into(),
            version: 1,
            device_id: m.device.id,
            device_name: "d".into(),
            session: crate::ids::SessionId::new(),
            started: crate::manifest::now_rfc3339(),
            heartbeat: crate::manifest::now_rfc3339(),
            ttl_secs: 60,
            base: None,
        };
        e.remote.put_lease(m.instance.id, &lease, &e.ctx).unwrap();
        let leases = e.remote.list_leases(m.instance.id, &e.ctx).unwrap();
        assert_eq!(leases.len(), 1);
        assert!(!leases[0].is_stale(time::OffsetDateTime::now_utc()));
        assert!(leases[0].is_stale(time::OffsetDateTime::now_utc() + time::Duration::seconds(500)));
        e.remote.delete_lease(m.instance.id, m.device.id, &e.ctx).unwrap();
        assert!(e.remote.list_leases(m.instance.id, &e.ctx).unwrap().is_empty());
    }

    #[test]
    fn footer_of_uploaded_pack_matches_index() {
        let e = env();
        let d = tempfile::tempdir().unwrap();
        let (pack, _) = make_pack(d.path(), 6);
        upload(&e, &pack);
        let local_footer = read_footer_from_file(&e.local.path_for(&pack_key(pack.id))).unwrap();
        let mut a = pack.entries.clone();
        a.sort_by_key(|e| e.offset);
        assert_eq!(local_footer, a);
    }
}
