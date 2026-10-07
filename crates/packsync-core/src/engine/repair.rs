//! Putting missing data back into the cloud.
//!
//! Blobs are content-addressed and shared by every snapshot, so a version whose data was removed from the
//! cloud (a cleanup on another computer released it, or files were deleted from Drive) is repaired by
//! uploading the missing blobs again, from any computer whose files still contain them. Nothing is
//! rewritten: no manifest changes, history stays as it is. What this computer cannot supply (the file was
//! changed or deleted since) is reported, never guessed.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::Engine;
use crate::db::SnapshotState;
use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::ids::{InstanceId, SnapshotId};
use crate::pack::{IndexDoc, IndexPack};
use crate::progress::{Level, OpCtx, Phase};
use crate::remote::Remote;
use crate::scan::{Candidate, ScanMode, hash_phase, walk};
use crate::snapshot::{LocalBlobs, staging_root};
use crate::stage::StagingSink;
use crate::upload::completeness_gaps;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepairOpts {
    /// Also check this version (for example the current one, already published).
    pub also: Option<SnapshotId>,
    /// When some data cannot be supplied by this computer, drop this computer's versions that are not
    /// uploaded yet (they can never be published) and go back to the last uploaded one, so the next
    /// sync saves the files as they are now. The files themselves are not touched.
    pub discard_unrepairable: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepairReport {
    /// Versions whose whole tree was checked against the cloud.
    pub checked: Vec<SnapshotId>,
    /// Files whose data was missing from the cloud before the repair.
    pub files_missing: u32,
    /// Blobs read from this computer's files and uploaded again.
    pub blobs_uploaded: u64,
    pub bytes_uploaded: u64,
    /// Files whose data is still missing: this computer no longer has that content.
    pub unrepairable: Vec<String>,
    /// Versions that were only on this computer and were dropped (`discard_unrepairable`).
    pub discarded: Vec<SnapshotId>,
}

impl RepairReport {
    pub fn complete(&self) -> bool {
        self.unrepairable.is_empty()
    }
}

impl Engine {
    /// Check this instance's versions that are not in the cloud yet — and `also` (for example the current
    /// version, already published) — against the cloud in full, and upload again whatever data is missing
    /// and still present in this computer's files. Read-only for the instance folder; safe to run at any time.
    pub fn repair_cloud_data(&self, id: InstanceId, opts: RepairOpts, ctx: &OpCtx) -> Result<RepairReport> {
        let inst = self.instance(id)?;
        self.guard_provider(&inst)?;
        let ctx = &ctx.clone().with_instance(id);
        let remote = self.remote()?;
        let mut rep = self.repair_with(&remote, id, opts.also, ctx)?;
        if !rep.complete() && opts.discard_unrepairable {
            rep.discarded = self.discard_unpublished(id)?;
        }
        Ok(rep)
    }

    /// Drop every version of `id` that exists only on this computer and point the instance back at the
    /// newest uploaded one it builds on. The instance's files are not touched: the next snapshot records
    /// them as they are (read in full).
    fn discard_unpublished(&self, id: InstanceId) -> Result<Vec<SnapshotId>> {
        let inst = self.instance(id)?;
        self.require_state(&inst, &[crate::db::InstanceState::Ok])?;
        let _lock = self.lock(id, "discard-unpublished")?;
        // and no upload may be publishing one of the versions being dropped
        let _upload =
            crate::lock::InstanceLock::try_acquire_kind(&self.locks_dir(), id, "upload", "discard-unpublished")
                .map_err(|e| match e {
                    Error::Busy(_) => {
                        Error::Busy("an upload of this instance is running; try again when it has finished".into())
                    }
                    other => other,
                })?;
        let rows = self.db.snapshots_in_state(id, &[SnapshotState::LocalComplete, SnapshotState::Uploading])?;
        let gone: std::collections::HashSet<SnapshotId> = rows.iter().map(|r| r.id).collect();
        let graph = Graph::load(&self.db, id)?;
        let mut base = inst.base_snapshot;
        while let Some(b) = base.filter(|b| gone.contains(b)) {
            base = graph.get(b).and_then(|n| n.parents.first().copied());
        }
        for r in &rows {
            // packs of these versions that already went up have no index in the cloud: never let anything
            // new deduplicate against them
            for p in self.db.staged_for(r.id)?.into_iter().filter(|p| p.uploaded) {
                self.db.blobs_forget_pack(p.id)?;
            }
            self.db.staged_remove_snapshot(r.id)?;
            self.db.snapshot_remove(id, r.id)?;
            let _ = std::fs::remove_dir_all(crate::snapshot::snapshot_staging_dir(&self.cfg.state_dir, r.id));
            let _ = std::fs::remove_file(crate::snapshot::manifest_path(&self.cfg.state_dir, r.id));
        }
        self.db.instance_update(id, |r| {
            r.base_snapshot = base;
            r.needs_full_scan = true;
        })?;
        self.db.set_publish_blocked(id, None)?;
        self.db.event(
            Some(id),
            "warn",
            "unpublished_discarded",
            &format!(
                "{} version(s) that could not be uploaded (their data is gone) were dropped; the files were not touched",
                rows.len()
            ),
        );
        Ok(rows.iter().map(|r| r.id).collect())
    }

    pub(crate) fn repair_with(
        &self,
        remote: &Remote,
        id: InstanceId,
        also: Option<SnapshotId>,
        ctx: &OpCtx,
    ) -> Result<RepairReport> {
        let inst = self.instance(id)?;
        ctx.phase(Phase::Verify, "checking that the cloud has all the data of this instance");
        let sync = remote.sync_index_cache(ctx)?;
        remote.confirm_unlisted(&sync, ctx)?;

        // What will reach the cloud anyway: the staged packs of versions that are not uploaded yet.
        let graph = Graph::load(&self.db, id)?;
        let mut targets = graph.unpublished_in_order();
        let pending_local = LocalBlobs::of_lineage(&self.db, &self.cfg.state_dir, id, &targets)?;
        if let Some(s) = also
            && !targets.contains(&s)
        {
            targets.push(s);
        }
        let mut rep = RepairReport::default();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let mut manifests = vec![];
        for s in &targets {
            let m = self.manifest(id, *s, ctx)?;
            missing.extend(completeness_gaps(remote, &m, &pending_local, true, ctx)?.paths);
            rep.checked.push(*s);
            manifests.push(m);
        }
        rep.files_missing = missing.len() as u32;
        if missing.is_empty() {
            self.db.set_publish_blocked(id, None)?;
            return Ok(rep);
        }

        // Re-read exactly those files and stage what the cloud does not have.
        ctx.phase(Phase::Snapshot, format!("reading {} file(s) to upload their data again", missing.len()));
        let (rules, _) = self.rules_for(&inst, ctx)?;
        let mut scan = self.cfg.snapshot.scan.clone();
        scan.mode = ScanMode::Full;
        let w = walk(&inst.root, &rules, &HashMap::new(), &scan, ctx)?;
        let wanted: Vec<&Candidate> = w.files.iter().filter(|c| missing.contains(&c.path)).collect();
        let tmp = staging_root(&self.cfg.state_dir).join(format!("tmp-repair-{}", uuid::Uuid::new_v4().simple()));
        let sink = StagingSink::new(
            self.db.clone(),
            tmp.clone(),
            self.cfg.snapshot.pack_target,
            self.cfg.snapshot.zstd_level,
            pending_local.hashes(),
        )?;
        let policies = self.policies_for(&inst);
        if let Err(e) =
            hash_phase(&inst.root, &wanted, &sink, &self.cfg.snapshot.chunking, &policies, scan.threads, ctx)
        {
            sink.discard();
            return Err(e);
        }
        let staged = sink.finish()?;
        let uploaded = upload_repair_packs(remote, &staged.packs, ctx);
        let _ = std::fs::remove_dir_all(&tmp);
        let (blobs, bytes) = uploaded?;
        rep.blobs_uploaded = blobs;
        rep.bytes_uploaded = bytes;

        // Check again: whatever is still missing could not be supplied by this computer.
        let mut still: BTreeSet<String> = BTreeSet::new();
        for m in &manifests {
            still.extend(completeness_gaps(remote, m, &pending_local, true, ctx)?.paths);
        }
        rep.unrepairable = still.into_iter().collect();
        if rep.complete() {
            self.db.set_publish_blocked(id, None)?;
            if let Some(t) = self.db.cloud_data_lost_ms()? {
                self.db.set_deep_checked(id, t)?;
            }
            ctx.note(
                Phase::Verify,
                Level::Info,
                format!(
                    "uploaded the missing data of {} file(s) again ({} bytes)",
                    rep.files_missing, rep.bytes_uploaded
                ),
            );
            self.db.event(
                Some(id),
                "info",
                "cloud_data_repaired",
                &format!(
                    "the missing cloud data of {} file(s) was uploaded again from this computer",
                    rep.files_missing
                ),
            );
        } else {
            let msg = format!(
                "{} file(s) of a version that is not uploaded yet have data that is neither in the cloud nor on this computer \
                 any more (first: {}); that version cannot be published",
                rep.unrepairable.len(),
                rep.unrepairable[0]
            );
            self.db.set_publish_blocked(id, Some(&msg))?;
            self.db.event(Some(id), "error", "cloud_data_unrepairable", &msg);
        }
        Ok(rep)
    }
}

/// Upload repair packs: packs first, then their index, and only then teach the local cache about them —
/// so a crash in between leaves at worst unreferenced packs (removed by a cleanup), never a cache that
/// believes in data no index in the cloud describes.
fn upload_repair_packs(remote: &Remote, packs: &[crate::pack::FinishedPack], ctx: &OpCtx) -> Result<(u64, u64)> {
    if packs.is_empty() {
        return Ok((0, 0));
    }
    let mut infos = Vec::with_capacity(packs.len());
    let mut bytes = 0;
    for p in packs {
        let info = match remote.stat_pack(p.id, ctx)? {
            Some(info) if info.size == p.size => info,
            _ => {
                bytes += p.size;
                remote.put_pack(&p.path, p.id, p.size, ctx)?
            }
        };
        infos.push(info);
    }
    let doc =
        IndexDoc::new(packs.iter().map(|p| IndexPack { id: p.id, size: p.size, blobs: p.entries.clone() }).collect());
    let index_id = remote.put_index(&doc, ctx)?;
    for (p, info) in doc.packs.iter().zip(&infos) {
        match remote.stat_pack(p.id, ctx)? {
            Some(i) if i.size == p.size => remote.note_uploaded_pack(p, info)?,
            other => {
                return Err(Error::Corrupt(format!(
                    "repair pack {} did not verify after upload (cloud reports {:?} bytes, expected {})",
                    p.id.short(),
                    other.map(|i| i.size),
                    p.size
                )));
            }
        }
    }
    remote.note_uploaded_index(index_id)?;
    let blobs: u64 = packs.iter().map(|p| p.entries.len() as u64).sum();
    Ok((blobs, bytes))
}
