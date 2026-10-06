//! Upload and publish (state machine in `docs/state-machine.md`, protocol in `docs/consistency.md`).
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

use std::collections::{BTreeSet, HashSet};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadReport {
    pub snapshot: SnapshotId,
    pub packs_uploaded: u32,
    pub packs_already_present: u32,
    pub bytes_uploaded: u64,
    pub state: SnapshotState,
    /// Set when a persisted backoff delayed the attempt: nothing was tried.
    pub deferred_until_ms: Option<i64>,
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
            self.require_parents_in_cloud(&m, inst, ctx)?;
            db.snapshot_set_state(inst, id, SnapshotState::Uploading)?;
            report.state = SnapshotState::Uploading;
            ctx.phase(Phase::Upload, format!("uploading snapshot {}", id.short()));

            // ---- 2. dependencies exist ------------------------------------------------------------------
            let meta = load_staged_meta(self.state_dir, id)?;
            let sync = self.remote.sync_index_cache(ctx)?;
            // cached packs the listing did not show may have just been removed: a direct lookup decides
            let mut lost_dep = self.remote.confirm_unlisted(&sync, ctx)? > 0;
            let deps: HashSet<Hash> = meta.required_packs.iter().chain(meta.reused_packs.iter()).copied().collect();
            for p in deps {
                if self.remote.stat_pack(p, ctx)?.is_none() {
                    // gone (a cleanup on another computer, or removed outside PackSync): forget it and let the
                    // full check below decide whether this version still has all its data somewhere
                    self.db.blobs_forget_pack(p)?;
                    self.db.mark_cloud_data_lost()?;
                    lost_dep = true;
                }
            }
            // The whole tree, not just what changed: unchanged files inherit their data from older versions,
            // and that data may have been removed since (a cleanup elsewhere while this computer was away).
            let deep_due = self.db.deep_check_due(inst)?;
            let deep = meta.derived || lost_dep || deep_due.is_some();
            let own = LocalBlobs::of_snapshot(self.state_dir, id, &meta.packs);
            let gaps = completeness_gaps(self.remote, &m, &own, deep, ctx)?;
            if !gaps.is_empty() {
                return Err(self.blocked(inst, format!("{}; nothing was published", gaps.describe())));
            }
            if let Some(t) = deep_due {
                self.db.set_deep_checked(inst, t)?;
            }
            crate::failpoint!("upload.before_packs")?;

            // ---- 3. packs ---------------------------------------------------------------------------------
            self.upload_packs(id, &meta.packs, &mut report, ctx)?;
            crate::failpoint!("upload.after_packs")?;

            // ---- 4. index -----------------------------------------------------------------------------------
            if !meta.packs.is_empty() {
                let doc = crate::snapshot::staged_index_doc(&meta);
                let index_id = self.remote.put_index(&doc, ctx)?;
                // every pack it lists was uploaded or found present above: nothing to learn from it
                self.remote.note_uploaded_index(index_id)?;
            }
            crate::failpoint!("upload.after_index")?;

            // ---- 5. verify -----------------------------------------------------------------------------------
            for p in &meta.packs {
                match self.remote.stat_pack(p.id, ctx)? {
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

    fn upload_packs(&self, id: SnapshotId, packs: &[IndexPack], report: &mut UploadReport, ctx: &OpCtx) -> Result<()> {
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
        let refs = (&done, &uploaded, &present, &bytes, &first_err, &dir);
        std::thread::scope(|s| {
            for _ in 0..self.concurrency.max(1) {
                let rx = rx.clone();
                s.spawn(move || {
                    let (done, uploaded, present, bytes, first_err, dir) = refs;
                    while let Ok(p) = rx.recv() {
                        if first_err.lock().is_some() || ctx.is_cancelled() {
                            return;
                        }
                        let res: Result<bool> = (|| {
                            // already there with the right size? (a retry after a crash, or a dedup'd twin)
                            if let Some(info) = self.remote.stat_pack(p.id, ctx)?
                                && info.size == p.size
                            {
                                self.remote.note_uploaded_pack(p, &info)?;
                                return Ok(false);
                            }
                            let path = dir.join(format!("{}.pack", p.id.to_hex()));
                            let info = self.remote.put_pack(&path, p.id, p.size, ctx)?;
                            self.remote.note_uploaded_pack(p, &info)?;
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
        Ok(())
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
