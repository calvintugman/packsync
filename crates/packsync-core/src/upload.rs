//! Upload and publish.
//!
//! Order of operations for one snapshot — every step idempotent, so a crash, restart or retry at
//! any point is safe and *never* exposes an incomplete snapshot:
//!
//! 1. parents must already be in the cloud;
//! 2. verify packs the snapshot depends on but did not create still exist (cache ≠ authority), and that
//!    every file of the tree has its data either in this snapshot's packs or in the cloud — in full
//!    (chunk lists included) for derived snapshots and after cloud data was found missing;
//! 3. upload the staged packs (skipping ones already stored with the right size);
//! 4. upload the pack index;
//! 5. re-verify every pack of the snapshot;
//! 6. **only now** upload the manifest — publishing is this single immutable object;
//! 7. write this device's head record (a hint), record `published`;
//! 8. re-list as another device would; only then is the snapshot `visible` ("ready elsewhere").
//!
//! There is no shared mutable pointer to overwrite, so concurrent publications from the same
//! parent produce two commits (two heads), never a lost update.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::db::{Db, SnapshotState, now_ms};
use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::ids::{DeviceId, InstanceId, SnapshotId};
use crate::manifest::{Manifest, now_rfc3339};
use crate::pack::IndexPack;
use crate::progress::{Level, OpCtx, Phase};
use crate::remote::{HEAD_FORMAT, HEAD_WINDOW, HeadRecord, RecentCommit, Remote};
use crate::snapshot::{LocalBlobs, load_staged_meta, manifest_path, snapshot_staging_dir};
use crate::storage::StorageError;

/// Every this-many publications of an instance, all of its dependency packs are looked up directly
/// instead of trusting the listing (see the dependency check in [`Uploader`]).
pub const FULL_CHECK_EVERY: u64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadReport {
    pub snapshot: SnapshotId,
    pub packs_uploaded: u32,
    pub packs_already_present: u32,
    pub bytes_uploaded: u64,
    pub state: SnapshotState,
    /// Set when a persisted backoff delayed the attempt: nothing was tried.
    pub deferred_until_ms: Option<i64>,
    /// The index uploaded with this snapshot's packs (fast path), named in the HEAD pointer so that a
    /// computer one version behind can catch up without listing the cloud.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<Hash>,
}

pub struct Uploader<'a> {
    pub remote: &'a Remote,
    pub db: &'a Arc<Db>,
    pub state_dir: &'a Path,
    pub device: (DeviceId, String),
    pub concurrency: usize,
}

/// Files of a snapshot whose data is neither in the snapshot's own packs nor in the cloud.
#[derive(Debug, Default, Clone)]
pub struct Gaps {
    pub paths: BTreeSet<String>,
    pub blobs: usize,
}

impl Gaps {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
    pub fn describe(&self) -> String {
        let first: Vec<&str> = self.paths.iter().take(3).map(String::as_str).collect();
        format!(
            "{} file(s) of this version have data that is no longer in the cloud (for example {}{})",
            self.paths.len(),
            first.join(", "),
            if self.paths.len() > first.len() { ", …" } else { "" }
        )
    }
}

/// Which files of `m` have data that is neither in `own` (blobs staged on this computer that are uploaded
/// with the snapshot or before it) nor known to be in the cloud. With `deep`, every chunked file's chunk
/// list is read (from `own` or the cloud) and each chunk is checked too; without it only each file's
/// top-level blob is.
pub fn completeness_gaps(remote: &Remote, m: &Manifest, own: &LocalBlobs, deep: bool, ctx: &OpCtx) -> Result<Gaps> {
    let db = &remote.db;
    let have = |h: Hash| -> Result<bool> { Ok(own.contains(&h) || db.blob_lookup(h)?.is_some()) };
    let mut g = Gaps::default();
    for e in m.entries.iter().filter(|e| e.is_file() && e.s > 0) {
        ctx.check()?;
        let top = match (e.r, e.h) {
            (Some(r), _) => r,
            (None, Some(h)) => h,
            _ => continue,
        };
        if !have(top)? {
            g.blobs += 1;
            g.paths.insert(e.p.clone());
            continue;
        }
        let Some(r) = e.r.filter(|_| deep) else { continue };
        let recipe = match own.read(&r)? {
            Some(b) => Ok(b),
            None => remote.read_blob(r, ctx),
        };
        match recipe.and_then(|b| crate::manifest::Recipe::decode(&b, e.s)) {
            Ok(rc) => {
                for (c, _) in rc.chunks {
                    if !have(c)? {
                        g.blobs += 1;
                        g.paths.insert(e.p.clone());
                    }
                }
            }
            // the recipe's pack is gone (read_blob forgets it) or the recipe is damaged
            Err(err)
                if matches!(err.code(), crate::error::ErrorCode::CorruptRemote | crate::error::ErrorCode::Invalid) =>
            {
                g.blobs += 1;
                g.paths.insert(e.p.clone());
            }
            Err(err) => return Err(err),
        }
    }
    Ok(g)
}

/// Persisted retry delay after `attempts` failures: 30 s doubling to one hour, with jitter.
pub fn backoff_ms(attempts: u32) -> i64 {
    let base = 30_000i64.saturating_mul(1i64 << attempts.min(7));
    let capped = base.min(3_600_000);
    (capped as f64 * (0.75 + 0.5 * fastrand::f64())) as i64
}

impl Uploader<'_> {
    /// Publish one snapshot. See the module docs for the protocol.
    /// The fast path of a publication, for a computer that knows the cloud has not moved since its base
    /// (its HEAD pointer is where it left it): no listings and no lookups. The new packs and their index
    /// (side by side), then the manifest. The caller then moves the HEAD pointer by compare-and-swap; if
    /// that fails, someone else published meanwhile and the caller takes the full path, which sees both
    /// versions. Returns `None` when this snapshot cannot take the fast path (the caller uses `publish`).
    /// `no_cleanup` runs alongside the upload; the manifest is stored only if it reports no cleanup since this
    /// computer last looked (a cleanup may have deleted data this snapshot relies on).
    pub fn publish_fast(
        &self,
        inst: InstanceId,
        id: SnapshotId,
        no_cleanup: &(dyn Fn(&OpCtx) -> Result<bool> + Sync),
        ctx: &OpCtx,
    ) -> Result<Option<UploadReport>> {
        let db = self.db;
        let Some(row) = db.snapshot_get(inst, id)? else { return Ok(None) };
        if !matches!(row.state, SnapshotState::LocalComplete) {
            return Ok(None);
        }
        let staged_rows = db.staged_for(id)?;
        if staged_rows.iter().any(|p| p.next_attempt_ms.is_some()) {
            return Ok(None); // an earlier attempt failed: the full path honours its backoff
        }
        let stored = std::fs::read(manifest_path(self.state_dir, id))
            .map_err(|e| Error::io("reading local snapshot manifest", e))?;
        let (m, _) = Manifest::decode(&stored, Some(id))?;
        let meta = load_staged_meta(self.state_dir, id)?;
        if meta.derived {
            return Ok(None);
        }
        // No completeness check here: the fast path only publishes a snapshot created moments ago by the
        // same operation, and creating it already refused any file whose data is neither in its own packs
        // nor known to be in the cloud (`required_packs` in snapshot.rs); cloud data reported lost turns
        // the fast path off. The full path checks again because time passes between saving and uploading.
        db.snapshot_set_state(inst, id, SnapshotState::Uploading)?;
        let mut report = UploadReport {
            snapshot: id,
            packs_uploaded: 0,
            packs_already_present: 0,
            bytes_uploaded: 0,
            state: SnapshotState::Uploading,
            deferred_until_ms: None,
            index: None,
        };
        ctx.mark(format!("fast publish: {} new packs and their index", meta.packs.len()));
        let none = HashMap::new();
        let (packs_res, index_res, unmoved) = std::thread::scope(|s| {
            let index = (!meta.packs.is_empty())
                .then(|| s.spawn(|| self.remote.put_index(&crate::snapshot::staged_index_doc(&meta), ctx)));
            let unmoved = s.spawn(|| no_cleanup(ctx));
            let packs = self.upload_packs(id, &meta.packs, &none, &mut report, ctx);
            let index = index.map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)));
            (packs, index, unmoved.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
        });
        let verified = packs_res?;
        if !unmoved? {
            // the full path publishes it, looking every dependency up
            db.snapshot_set_state(inst, id, SnapshotState::LocalComplete)?;
            return Ok(None);
        }
        crate::failpoint!("upload.after_packs")?;
        if let Some(index_id) = index_res.transpose()? {
            self.remote.note_uploaded_index(index_id)?;
            report.index = Some(index_id);
        }
        crate::failpoint!("upload.after_index")?;
        if let Some(p) = meta.packs.iter().find(|p| verified.get(&p.id) != Some(&p.size)) {
            return Err(Error::Corrupt(format!("pack {} did not verify after upload", p.id.short())));
        }
        crate::failpoint!("publish.before_manifest")?;
        ctx.mark(format!("fast publish: manifest ({} bytes)", stored.len()));
        self.remote.put_commit(&m, id, &stored, ctx)?;
        crate::failpoint!("publish.after_manifest")?;
        db.snapshot_set_state(inst, id, SnapshotState::Published)?;
        db.set_publish_blocked(inst, None)?;
        report.state = SnapshotState::Published;
        Ok(Some(report))
    }

    pub fn publish(&self, inst: InstanceId, id: SnapshotId, force: bool, ctx: &OpCtx) -> Result<UploadReport> {
        let db = self.db;
        let row = db.snapshot_get(inst, id)?.ok_or_else(|| Error::NotFound(format!("snapshot {}", id.short())))?;
        let mut report = UploadReport {
            snapshot: id,
            packs_uploaded: 0,
            packs_already_present: 0,
            bytes_uploaded: 0,
            state: row.state,
            deferred_until_ms: None,
            index: None,
        };
        if matches!(row.state, SnapshotState::Visible) {
            return Ok(report);
        }
        if matches!(row.state, SnapshotState::Remote) {
            return Ok(report);
        }

        // ---- 0. honour persisted backoff ----------------------------------------------------------
        let staged_rows = db.staged_for(id)?;
        if !force {
            let now = now_ms();
            if let Some(until) =
                staged_rows.iter().filter(|p| !p.uploaded).filter_map(|p| p.next_attempt_ms).filter(|t| *t > now).max()
            {
                report.deferred_until_ms = Some(until);
                return Ok(report);
            }
        }

        // ---- 1. manifest (verified against its id) and parents ----------------------------------------
        let stored = std::fs::read(manifest_path(self.state_dir, id))
            .map_err(|e| Error::io("reading local snapshot manifest", e))?;
        let (m, decoded_id) = Manifest::decode(&stored, Some(id))?;
        debug_assert_eq!(decoded_id, id);
        if row.state != SnapshotState::Published {
            ctx.mark("upload: check parents are in the cloud");
            self.require_parents_in_cloud(&m, inst, ctx)?;
            db.snapshot_set_state(inst, id, SnapshotState::Uploading)?;
            report.state = SnapshotState::Uploading;
            ctx.phase(Phase::Upload, format!("uploading snapshot {}", id.short()));

            // ---- 2. dependencies exist ------------------------------------------------------------------
            let meta = load_staged_meta(self.state_dir, id)?;
            ctx.mark("upload: refresh the cloud index cache");
            let sync = self.remote.sync_index_cache(ctx)?;
            // cached packs the listing did not show may have just been removed: a direct lookup decides
            let mut lost_dep = self.remote.confirm_unlisted(&sync, ctx)? > 0;
            let deps: BTreeSet<Hash> = meta.required_packs.iter().chain(meta.reused_packs.iter()).copied().collect();
            // The listing just made shows the packs the cloud has. Only a pack it did not show needs a direct
            // (strongly consistent) lookup — unless a full check is due: after cloud data was lost, and on
            // every FULL_CHECK_EVERY-th publication, every dependency is looked up directly. (A listing can
            // lag a deletion by seconds; a cleanup only deletes packs unused for its grace period, and the
            // periodic and `verify --remote` checks catch what that leaves.)
            let periodic = db.count_publication(inst, FULL_CHECK_EVERY)?;
            let full = periodic || meta.derived || lost_dep || self.db.deep_check_due(inst)?.is_some();
            let deps: Vec<Hash> = deps.into_iter().filter(|p| full || !sync.listed.contains_key(p)).collect();
            ctx.mark(format!(
                "upload: look up {} dependency packs{}",
                deps.len(),
                if full { " (full check)" } else { " the listing did not show" }
            ));
            for (p, info) in deps.iter().zip(self.stat_packs(&deps, ctx)?) {
                if info.is_none() {
                    // gone (a cleanup on another computer, or removed outside PackSync): forget it and let the
                    // full check below decide whether this version still has all its data somewhere
                    self.db.blobs_forget_pack(*p)?;
                    self.db.mark_cloud_data_lost()?;
                    lost_dep = true;
                }
            }
            // The whole tree, not just what changed: unchanged files inherit their data from older versions,
            // and that data may have been removed since (a cleanup elsewhere while this computer was away).
            let deep_due = self.db.deep_check_due(inst)?;
            let deep = meta.derived || lost_dep || deep_due.is_some();
            ctx.mark(if deep { "upload: completeness check (deep)" } else { "upload: completeness check" });
            let own = LocalBlobs::of_snapshot(self.state_dir, id, &meta.packs);
            let gaps = completeness_gaps(self.remote, &m, &own, deep, ctx)?;
            if !gaps.is_empty() {
                return Err(self.blocked(inst, format!("{}; nothing was published", gaps.describe())));
            }
            if let Some(t) = deep_due {
                self.db.set_deep_checked(inst, t)?;
            }
            crate::failpoint!("upload.before_packs")?;

            // ---- 3 + 4. packs, and the index that describes them, side by side --------------------------------
            // Their order does not matter: a reader only uses an index entry for a pack the cloud lists with
            // the recorded size, so an index that arrives first is simply not used yet. Both come before the
            // manifest, which is what makes them visible.
            ctx.mark(format!("upload: {} new packs and their index", meta.packs.len()));
            let (packs_res, index_res) = std::thread::scope(|s| {
                let index = (!meta.packs.is_empty())
                    .then(|| s.spawn(|| self.remote.put_index(&crate::snapshot::staged_index_doc(&meta), ctx)));
                let packs = self.upload_packs(id, &meta.packs, &sync.listed, &mut report, ctx);
                (packs, index.map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))))
            });
            let verified = packs_res?;
            crate::failpoint!("upload.after_packs")?;
            if let Some(index_id) = index_res.transpose()? {
                // every pack it lists was uploaded or found present: nothing to learn from it
                self.remote.note_uploaded_index(index_id)?;
            }
            crate::failpoint!("upload.after_index")?;

            // ---- 5. verify -----------------------------------------------------------------------------------
            // A pack whose upload response (or lookup) reported the expected size was verified by the provider
            // (Drive checks the checksum of what it stored); any other pack is looked up again.
            let unverified: Vec<&IndexPack> =
                meta.packs.iter().filter(|p| verified.get(&p.id) != Some(&p.size)).collect();
            ctx.mark(format!("upload: verify {} packs", unverified.len()));
            let uploaded_ids: Vec<Hash> = unverified.iter().map(|p| p.id).collect();
            for (p, info) in unverified.iter().zip(self.stat_packs(&uploaded_ids, ctx)?) {
                match info {
                    Some(info) if info.size == p.size => {}
                    other => {
                        return Err(Error::Corrupt(format!(
                            "pack {} did not verify after upload (cloud reports {:?} bytes, expected {})",
                            p.id.short(),
                            other.map(|i| i.size),
                            p.size
                        )));
                    }
                }
            }
            crate::failpoint!("publish.before_manifest")?;

            // ---- 6. publish: the manifest, last ----------------------------------------------------------------
            ctx.mark(format!("upload: manifest ({} bytes)", stored.len()));
            self.remote.put_commit(&m, id, &stored, ctx)?;
            crate::failpoint!("publish.after_manifest")?;

            // ---- 7. record -----------------------------------------------------------------------------------------
            db.snapshot_set_state(inst, id, SnapshotState::Published)?;
            db.set_publish_blocked(inst, None)?;
            report.state = SnapshotState::Published;
            db.instance_update(inst, |r| {
                r.last_synced_ms = Some(now_ms());
                r.provider_id = Some(self.remote.provider_id());
            })?;
        }
        crate::failpoint!("publish.before_head")?;
        ctx.mark("upload: head record");
        // The head record is a hint: failing to write it must not undo a successful publication.
        if let Err(e) = self.write_head(inst, id, ctx) {
            if crate::failpoint::is_crash(&e) {
                return Err(e);
            }
            ctx.note(
                Phase::Publish,
                Level::Warn,
                format!("snapshot published, but the head record could not be updated ({e}); it will be retried"),
            );
        }

        // ---- 8. visibility, as another device would discover it -------------------------------------------------------
        ctx.mark("upload: confirm it is listed");
        match self.remote.list_commits(inst, ctx) {
            Ok(list) if list.iter().any(|c| c.id == id) => {
                db.snapshot_set_state(inst, id, SnapshotState::Visible)?;
                report.state = SnapshotState::Visible;
            }
            Ok(_) => ctx.note(
                Phase::Publish,
                Level::Info,
                "published; waiting for the listing to show the snapshot (eventual consistency)",
            ),
            Err(e) if e.exit_code() == crate::error::exit::OFFLINE => {}
            Err(e) => return Err(e),
        }

        // ---- cleanup -----------------------------------------------------------------------------------------------------------
        crate::failpoint!("upload.before_cleanup")?;
        let dir = snapshot_staging_dir(self.state_dir, id);
        let _ = std::fs::remove_dir_all(dir);
        db.staged_remove_snapshot(id)?;
        Ok(report)
    }

    /// Remember why publishing is blocked (the status shows it) and build the error.
    fn blocked(&self, inst: InstanceId, why: String) -> Error {
        let _ = self.db.set_publish_blocked(inst, Some(&why));
        Error::CloudDataMissing(why)
    }

    fn require_parents_in_cloud(&self, m: &Manifest, inst: InstanceId, ctx: &OpCtx) -> Result<()> {
        for p in &m.parents {
            match self.db.snapshot_get(inst, *p)? {
                Some(r) if r.state.is_in_cloud() => {}
                Some(r) if r.state == SnapshotState::Remote => {}
                Some(_) => {
                    return Err(Error::other(format!(
                        "parent snapshot {} must be uploaded before {}",
                        p.short(),
                        m.created
                    )));
                }
                None => {
                    if !self.remote.commit_exists(inst, *p, ctx)? {
                        return Err(Error::Corrupt(format!("parent snapshot {} is not in the cloud", p.short())));
                    }
                }
            }
        }
        Ok(())
    }

    /// Look up many packs in the cloud, a few requests at a time; results are in the order of `ids`.
    ///
    /// Each lookup is an independent, strongly consistent request. One after another, the few hundred
    /// packs a large instance depends on took about 20 seconds on Google Drive — longer than a small
    /// upload itself. With `concurrency` 1 the lookups stay strictly sequential.
    fn stat_packs(&self, ids: &[Hash], ctx: &OpCtx) -> Result<Vec<Option<crate::storage::ObjectInfo>>> {
        const LOOKUPS_IN_FLIGHT: usize = 16;
        let workers = if self.concurrency <= 1 { 1 } else { LOOKUPS_IN_FLIGHT }.min(ids.len());
        let next = AtomicU64::new(0);
        let failed = std::sync::atomic::AtomicBool::new(false);
        let slots: Vec<parking_lot::Mutex<Option<Result<Option<crate::storage::ObjectInfo>>>>> =
            ids.iter().map(|_| parking_lot::Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    while !failed.load(Ordering::Relaxed) && !ctx.is_cancelled() {
                        let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                        let Some(id) = ids.get(i) else { return };
                        let r = self.remote.stat_pack(*id, ctx);
                        if r.is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                        *slots[i].lock() = Some(r);
                    }
                });
            }
        });
        let mut results: Vec<Option<Result<Option<crate::storage::ObjectInfo>>>> =
            slots.into_iter().map(|m| m.into_inner()).collect();
        // the first failure in pack order, so the error does not depend on thread timing
        if let Some(i) = results.iter().position(|r| matches!(r, Some(Err(_)))) {
            return Err(results.swap_remove(i).expect("just matched").expect_err("just matched"));
        }
        ctx.check()?;
        results
            .into_iter()
            .map(|r| r.ok_or_else(|| Error::other("a pack lookup did not run")).and_then(|r| r))
            .collect()
    }

    fn upload_packs(
        &self,
        id: SnapshotId,
        packs: &[IndexPack],
        listed: &HashMap<Hash, u64>,
        report: &mut UploadReport,
        ctx: &OpCtx,
    ) -> Result<HashMap<Hash, u64>> {
        let dir = snapshot_staging_dir(self.state_dir, id);
        let total: u64 = packs.iter().map(|p| p.size).sum();
        let done = AtomicU64::new(0);
        let uploaded = AtomicU64::new(0);
        let present = AtomicU64::new(0);
        let bytes = AtomicU64::new(0);
        let (tx, rx) = crossbeam_channel::unbounded();
        for p in packs {
            tx.send(p).ok();
        }
        drop(tx);
        let first_err: parking_lot::Mutex<Option<Error>> = parking_lot::Mutex::new(None);
        // packs whose size the provider confirmed in this run (upload response or lookup)
        let verified: parking_lot::Mutex<HashMap<Hash, u64>> = parking_lot::Mutex::new(HashMap::new());
        let refs = (&done, &uploaded, &present, &bytes, &first_err, &dir, &verified);
        std::thread::scope(|s| {
            for _ in 0..self.concurrency.max(1) {
                let rx = rx.clone();
                s.spawn(move || {
                    let (done, uploaded, present, bytes, first_err, dir, verified) = refs;
                    while let Ok(p) = rx.recv() {
                        if first_err.lock().is_some() || ctx.is_cancelled() {
                            return;
                        }
                        let res: Result<bool> = (|| {
                            // Already there with the right size (a retry after a crash, a dedup'd twin)? The
                            // listing just made says so. A pack it does not show is simply put: a put is
                            // create-only by content, and an interrupted upload of it is resumed or found by the
                            // provider (Drive keeps the session and the pre-generated file ID), so looking it
                            // up first would only cost a request per new pack.
                            if listed.get(&p.id) == Some(&p.size)
                                && let Some(info) = self.remote.stat_pack(p.id, ctx)?
                                && info.size == p.size
                            {
                                self.remote.note_uploaded_pack(p, &info)?;
                                verified.lock().insert(p.id, info.size);
                                return Ok(false);
                            }
                            let path = dir.join(format!("{}.pack", p.id.to_hex()));
                            let info = self.remote.put_pack(&path, p.id, p.size, ctx)?;
                            self.remote.note_uploaded_pack(p, &info)?;
                            verified.lock().insert(p.id, info.size);
                            Ok(true)
                        })();
                        match res {
                            Ok(did) => {
                                let _ = self.db.staged_mark_uploaded(p.id);
                                if did {
                                    uploaded.fetch_add(1, Ordering::Relaxed);
                                    bytes.fetch_add(p.size, Ordering::Relaxed);
                                } else {
                                    present.fetch_add(1, Ordering::Relaxed);
                                }
                                let d = done.fetch_add(p.size, Ordering::Relaxed) + p.size;
                                ctx.progress(
                                    Phase::Upload,
                                    (
                                        uploaded.load(Ordering::Relaxed) + present.load(Ordering::Relaxed),
                                        packs.len() as u64,
                                    ),
                                    (d, total),
                                    "uploading data",
                                );
                                if let Err(e) = crate::failpoint!("upload.after_pack") {
                                    first_err.lock().get_or_insert(e);
                                    return;
                                }
                            }
                            Err(e) => {
                                if !crate::failpoint::is_crash(&e) && !e.is_cancelled() {
                                    let attempts = self
                                        .db
                                        .staged_for(id)
                                        .ok()
                                        .and_then(|v| v.iter().find(|s| s.id == p.id).map(|s| s.attempts))
                                        .unwrap_or(0);
                                    let _ = self.db.staged_record_failure(
                                        p.id,
                                        e.code().as_str(),
                                        &crate::redact::redact(&e.to_string()),
                                        now_ms() + backoff_ms(attempts),
                                    );
                                }
                                first_err.lock().get_or_insert(e);
                                return;
                            }
                        }
                    }
                });
            }
        });
        report.packs_uploaded += uploaded.load(Ordering::Relaxed) as u32;
        report.packs_already_present += present.load(Ordering::Relaxed) as u32;
        report.bytes_uploaded += bytes.load(Ordering::Relaxed);
        if let Some(e) = first_err.into_inner() {
            return Err(e);
        }
        ctx.check()?;
        Ok(verified.into_inner())
    }

    /// Update this device's single-writer head record with a window of recent cloud-side history.
    pub fn write_head(&self, inst: InstanceId, head: SnapshotId, ctx: &OpCtx) -> Result<()> {
        let mut rows = self.db.snapshots(inst)?;
        rows.retain(|r| r.state.is_in_cloud() || r.id == head);
        rows.truncate(HEAD_WINDOW);
        let recent: Vec<RecentCommit> = rows
            .iter()
            .filter_map(|r| {
                Some(RecentCommit {
                    id: r.id,
                    parents: r.parents.clone(),
                    kind: r.kind,
                    created: r.created.clone()?,
                    device: r.device_name.clone().unwrap_or_default(),
                    device_id: r.device_id?,
                    files: r.files,
                    bytes: r.bytes,
                    note: r.note.clone(),
                    outcome: r.session_outcome,
                })
            })
            .collect();
        let rec = HeadRecord {
            format: HEAD_FORMAT.into(),
            version: 1,
            device_id: self.device.0,
            device_name: self.device.1.clone(),
            updated: now_rfc3339(),
            head,
            recent,
        };
        self.remote.put_head(inst, &rec, ctx)
    }

    /// Re-check snapshots that were published but not yet seen in a listing.
    pub fn confirm_visibility(&self, inst: InstanceId, ctx: &OpCtx) -> Result<usize> {
        let pending = self.db.snapshots_in_state(inst, &[SnapshotState::Published])?;
        if pending.is_empty() {
            return Ok(0);
        }
        let list = self.remote.list_commits(inst, ctx)?;
        let mut n = 0;
        for s in pending {
            if list.iter().any(|c| c.id == s.id) {
                self.db.snapshot_set_state(inst, s.id, SnapshotState::Visible)?;
                n += 1;
            }
        }
        Ok(n)
    }
}

/// Classify an upload error for the UI/engine: (retryable-later, message).
pub fn describe_failure(e: &Error) -> (bool, String) {
    let retryable =
        matches!(e, Error::Storage(s) if s.is_retryable()) || matches!(e, Error::Storage(StorageError::Offline(_)));
    (retryable, crate::redact::redact(&e.to_string()).into_owned())
}
