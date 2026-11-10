//! Snapshot, upload, discovery, reconcile and verification.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::db::{InstanceState, Origin, SnapshotRow, SnapshotState};
use crate::graph::{self, Decision, Graph};
use crate::manifest::SnapshotKind;
use crate::progress::{Level, Phase};
use crate::remote::LeaseRecord;
use crate::restore::{self, RestoreEnv};
use crate::scan::{ScanMode, hash_phase, walk};
use crate::snapshot::{self, SnapshotOutcome, SnapshotRequest};
use crate::stage::CountingSink;
use crate::storage::StorageError;
use crate::upload::Uploader;

#[derive(Debug, Clone, Default)]
pub struct CloudView {
    pub heads: Vec<SnapshotId>,
    pub crash_saves: Vec<SnapshotId>,
    pub leases: Vec<LeaseRecord>,
}

fn is_offline(e: &Error) -> bool {
    matches!(
        e,
        Error::Storage(StorageError::Offline(_) | StorageError::RateLimited { .. } | StorageError::Transient(_))
    )
}

impl Engine {
    /// Look at the cloud the way another device would: list commits (+ hints), learn their
    /// ancestry, and compute the heads. Unknown ancestry is resolved from *verified* manifests;
    /// damaged commits are set aside, never trusted.
    pub(crate) fn discover(&self, inst: InstanceId, ctx: &OpCtx) -> Result<CloudView> {
        let remote = self.remote()?;
        ctx.phase(Phase::Discovery, "looking at the cloud");
        let listing = remote.list_commits(inst, ctx)?;
        let head_records = remote.list_heads(inst, ctx).unwrap_or_default();
        let mut recent: HashMap<SnapshotId, crate::remote::RecentCommit> = HashMap::new();
        for h in &head_records {
            for r in &h.recent {
                recent.entry(r.id).or_insert_with(|| r.clone());
            }
        }
        let listed: HashSet<SnapshotId> = listing.iter().map(|c| c.id).collect();
        let base = self.db.instance_get(inst)?.and_then(|r| r.base_snapshot);
        for c in &listing {
            match self.db.snapshot_get(inst, c.id)? {
                Some(row) => {
                    if matches!(
                        row.state,
                        SnapshotState::LocalComplete | SnapshotState::Uploading | SnapshotState::Published
                    ) {
                        // The fresh listing shows a snapshot we produced: it is published and discoverable
                        // (this also repairs "crashed after the manifest upload, before the DB update").
                        self.db.snapshot_set_state(inst, c.id, SnapshotState::Visible)?;
                        let _ = std::fs::remove_dir_all(snapshot::snapshot_staging_dir(&self.cfg.state_dir, c.id));
                        self.db.staged_remove_snapshot(c.id)?;
                    }
                }
                None => {
                    let row = if let Some(r) = recent.get(&c.id) {
                        SnapshotRow {
                            id: c.id,
                            instance_id: inst,
                            kind: r.kind,
                            parents: r.parents.clone(),
                            parents_known: true,
                            device_id: Some(r.device_id),
                            device_name: Some(r.device.clone()),
                            created: Some(r.created.clone()),
                            origin: Origin::Remote,
                            state: SnapshotState::Remote,
                            files: r.files,
                            bytes: r.bytes,
                            note: r.note.clone(),
                            session_outcome: r.outcome,
                            published_ms: None,
                            visible_ms: None,
                            has_manifest: false,
                        }
                    } else if let Some(h) = &c.hint {
                        SnapshotRow {
                            id: c.id,
                            instance_id: inst,
                            kind: h.kind,
                            parents: h.parents.clone(),
                            parents_known: true,
                            device_id: h.device,
                            device_name: None,
                            created: h.created.clone(),
                            origin: Origin::Remote,
                            state: SnapshotState::Remote,
                            files: 0,
                            bytes: 0,
                            note: None,
                            session_outcome: None,
                            published_ms: None,
                            visible_ms: None,
                            has_manifest: false,
                        }
                    } else {
                        SnapshotRow {
                            id: c.id,
                            instance_id: inst,
                            kind: SnapshotKind::Normal,
                            parents: vec![],
                            parents_known: false,
                            device_id: None,
                            device_name: None,
                            created: None,
                            origin: Origin::Remote,
                            state: SnapshotState::Remote,
                            files: 0,
                            bytes: 0,
                            note: None,
                            session_outcome: None,
                            published_ms: None,
                            visible_ms: None,
                            has_manifest: false,
                        }
                    };
                    self.db.snapshot_upsert(&row)?;
                }
            }
        }
        // Forget cached commits that vanished from the cloud (pruned); keep our own and the base.
        for r in self.db.snapshots(inst)? {
            if r.origin == Origin::Remote && !listed.contains(&r.id) && Some(r.id) != base {
                self.db.snapshot_remove(inst, r.id)?;
            }
        }
        // Resolve unknown ancestry from verified manifests.
        for r in self.db.snapshots(inst)?.into_iter().filter(|r| r.state.is_in_cloud() && !r.parents_known) {
            match self.manifest(inst, r.id, ctx) {
                Ok(_) => {}
                Err(e @ Error::Unsupported(_)) => return Err(e),
                Err(Error::Corrupt(m)) | Err(Error::Invalid { detail: m, .. }) => {
                    self.db.event(
                        Some(inst),
                        "warn",
                        "damaged_snapshot",
                        &format!("ignoring damaged snapshot {}: {m}", r.id.short()),
                    );
                    self.db.snapshot_remove(inst, r.id)?;
                }
                Err(Error::Storage(StorageError::NotFound(_))) => self.db.snapshot_remove(inst, r.id)?,
                Err(e) => return Err(e),
            }
        }
        let graph = Graph::load(&self.db, inst)?;
        let leases = remote.list_leases(inst, ctx).unwrap_or_default();
        Ok(CloudView { heads: graph.cloud_heads(), crash_saves: graph.cloud_crash_saves(), leases })
    }

    /// Is `anc` an ancestor of `desc` according to **verified manifests** (hints are not enough to
    /// overwrite local files)?
    pub(crate) fn verified_ancestor(
        &self,
        inst: InstanceId,
        anc: SnapshotId,
        desc: SnapshotId,
        ctx: &OpCtx,
    ) -> Result<bool> {
        if anc == desc {
            return Ok(true);
        }
        let mut seen = HashSet::new();
        let mut stack = vec![desc];
        let mut steps = 0;
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            steps += 1;
            if steps > 20_000 {
                return Ok(false);
            }
            let m = match self.manifest(inst, n, ctx) {
                Ok(m) => m,
                Err(Error::Storage(StorageError::NotFound(_))) | Err(Error::Corrupt(_)) => continue, // pruned history: cannot prove
                Err(e) => return Err(e),
            };
            for p in &m.parents {
                if *p == anc {
                    return Ok(true);
                }
                stack.push(*p);
            }
        }
        Ok(false)
    }

    /// Exact set of local changes relative to the base snapshot (hashes only what the scan policy
    /// cannot vouch for).
    pub fn pending_changes(&self, id: InstanceId, ctx: &OpCtx) -> Result<PendingChanges> {
        let inst = self.instance(id)?;
        self.check_marker(&inst)?;
        let (rules, _) = self.rules_for(&inst, ctx)?;
        let base = inst.base_snapshot.map(|b| self.manifest(id, b, ctx)).transpose()?;
        let index = if inst.needs_full_scan { HashMap::new() } else { self.db.index_load(id)? };
        let mut opts = self.cfg.snapshot.scan.clone();
        if inst.needs_full_scan {
            opts.mode = ScanMode::Full;
        }
        let w = walk(&inst.root, &rules, &index, &opts, ctx)?;
        snapshot::symlink_guard(&w.issues, base.as_ref())?;
        if !w.unreadable_dirs.is_empty() {
            return Err(Error::InstanceUnavailable(format!(
                "{} could not be read; its content is unknown, so no changes can be computed",
                w.unreadable_dirs[0]
            )));
        }
        let to_hash: Vec<&crate::scan::Candidate> = w.files.iter().filter(|c| c.reuse.is_none()).collect();
        let sink = CountingSink::new(self.db.clone());
        let hp = hash_phase(
            &inst.root,
            &to_hash,
            &sink,
            &self.cfg.snapshot.chunking,
            &self.policies_for(&inst),
            opts.threads,
            ctx,
        )?;
        let hashed: HashMap<&str, &crate::scan::HashedFile> = hp.hashed.iter().map(|h| (h.path.as_str(), h)).collect();
        let (mut added, mut modified, mut removed) = (vec![], vec![], vec![]);
        let base_files: HashMap<&str, &crate::manifest::Entry> =
            base.iter().flat_map(|b| b.entries.iter()).filter(|e| e.is_file()).map(|e| (e.p.as_str(), e)).collect();
        let mut present: HashSet<&str> = HashSet::new();
        for c in &w.files {
            if hp.vanished.contains(&c.path) {
                continue;
            }
            present.insert(c.path.as_str());
            let (hash, size, mixed) = match (&c.reuse, hashed.get(c.path.as_str())) {
                (Some(r), _) => (r.hash, r.content_size, r.mixed.clone()),
                (None, Some(h)) => (h.content.hash, h.content.size, h.mixed.clone()),
                _ => continue,
            };
            match base_files.get(c.path.as_str()) {
                None => added.push(c.path.clone()),
                Some(be)
                    if be.h != Some(hash)
                        || be.s != size
                        || be.mx != mixed
                        || (cfg!(unix) && be.x != c.stat.executable) =>
                {
                    modified.push(c.path.clone())
                }
                Some(_) => {}
            }
        }
        for p in base_files.keys() {
            if !present.contains(p) {
                removed.push((*p).to_string());
            }
        }
        // empty directories
        let base_dirs: HashSet<&str> =
            base.iter().flat_map(|b| b.entries.iter()).filter(|e| !e.is_file()).map(|e| e.p.as_str()).collect();
        let now_dirs: HashSet<&str> = w.empty_dirs.iter().map(|s| s.as_str()).collect();
        added.extend(now_dirs.difference(&base_dirs).map(|d| format!("{d}/")));
        removed.extend(
            base_dirs
                .difference(&now_dirs)
                .filter(|d| !present.iter().any(|p| p.starts_with(&format!("{d}/"))))
                .map(|d| format!("{d}/")),
        );
        added.sort();
        removed.sort();
        modified.sort();
        let dirty = !(added.is_empty() && removed.is_empty() && modified.is_empty());
        Ok(PendingChanges { dirty, added, removed, modified, issues: w.issues, exact: true })
    }

    /// The instance folder is still the one registered (its marker matches); one check for every operation.
    pub(crate) fn check_marker(&self, inst: &InstanceRow) -> Result<()> {
        if !inst.sync_enabled {
            return Ok(());
        }
        match std::fs::read_to_string(super::instances::marker_path(&inst.root)) {
            Ok(m) if m.trim() == inst.marker => Ok(()),
            Ok(_) => {
                Err(Error::InstanceUnavailable(format!("{} belongs to a different instance now", inst.root.display())))
            }
            Err(_) => Err(Error::InstanceUnavailable(format!(
                "{} is missing or on an unmounted drive (PackSync's marker file is not there); nothing was changed in the cloud",
                inst.root.display()
            ))),
        }
    }

    /// Create a local snapshot of the instance. The instance must be idle.
    pub fn snapshot(&self, id: InstanceId, opts: SnapshotOpts, ctx: &OpCtx) -> Result<SnapshotReport> {
        let inst = self.instance(id)?;
        self.require_state(&inst, &[InstanceState::Ok, InstanceState::Conflict, InstanceState::CrashPending])?;
        let _lock = self.lock(id, "snapshot")?;
        if !opts.allow_running {
            self.ensure_idle(&inst)?;
        }
        self.snapshot_locked(&inst, opts, ctx)
    }

    pub(crate) fn snapshot_locked(
        &self,
        inst: &InstanceRow,
        opts: SnapshotOpts,
        ctx: &OpCtx,
    ) -> Result<SnapshotReport> {
        let ctx = &ctx.clone().with_instance(inst.id);
        let (rules, rules_text) = self.rules_for(inst, ctx)?;
        let base = inst.base_snapshot.map(|b| self.manifest(inst.id, b, ctx).map(|m| (m, b))).transpose()?;
        let launcher = self.launcher(&inst.launcher);
        let desc = launcher.describe(&inst.root).ok();
        let game_dir = if inst.game_root.is_empty() { inst.root.clone() } else { inst.root.join(&inst.game_root) };
        let pack = desc
            .as_ref()
            .map(|d| d.identity.clone())
            .unwrap_or_else(|| base.as_ref().map(|b| b.0.pack.clone()).unwrap_or_default());
        let worlds = desc.as_ref().map(|d| d.worlds.clone()).unwrap_or_else(|| crate::worlds::list_worlds(&game_dir));
        let policies = self.policies_for(inst);
        let req = SnapshotRequest {
            instance: inst,
            rules: &rules,
            rules_text: &rules_text,
            policies: &policies,
            base: base.as_ref().map(|(m, id)| (m, *id)),
            parents: inst.base_snapshot.into_iter().collect(),
            kind: opts.kind,
            session: opts.session.clone(),
            note: opts.note.clone(),
            pack,
            worlds,
            device: self.device(),
            launcher: inst.launcher.clone(),
            force: opts.force,
            confirm_mass_delete: opts.confirm_mass_delete,
            fork_of: None,
            new_instance_id: None,
        };
        let outcome = snapshot::create(&self.db, &self.cfg.state_dir, &req, &self.cfg.snapshot, ctx)?;
        Ok(match outcome {
            SnapshotOutcome::Unchanged { base, stats, walk } => {
                SnapshotReport { result: SnapshotResult::Unchanged { base }, stats, issues: walk.issues }
            }
            SnapshotOutcome::Created { id, stats, walk, .. } => {
                SnapshotReport { result: SnapshotResult::Created { id }, stats, issues: walk.issues }
            }
        })
    }

    /// Upload every local snapshot that the cloud does not have yet, parents first. Safe to call
    /// repeatedly; resumes after crashes and honours persisted retry backoff unless `force`.
    pub fn upload_pending(&self, id: InstanceId, force: bool, ctx: &OpCtx) -> Result<UploadSummary> {
        let inst = self.instance(id)?;
        self.guard_provider(&inst)?;
        let remote = self.remote()?;
        let _ul = crate::lock::InstanceLock::try_acquire_kind(&self.locks_dir(), id, "upload", "upload").map_err(
            |e| match e {
                Error::Busy(_) => Error::Busy("an upload of this instance is already running".into()),
                other => other,
            },
        )?;
        let ctx = &ctx.clone().with_instance(id);
        let graph = Graph::load(&self.db, id)?;
        let order = graph.unpublished_in_order();
        let up = Uploader {
            remote: &remote,
            db: &self.db,
            state_dir: &self.cfg.state_dir,
            device: self.device(),
            concurrency: self.cfg.upload_concurrency,
        };
        let mut summary = UploadSummary::default();
        let mut repaired = false;
        for (i, sid) in order.iter().enumerate() {
            let rep = match up.publish(id, *sid, force, ctx) {
                // Data this version builds on is no longer in the cloud: upload it again from this computer's
                // files (once), then try again. Nothing is published with a hole.
                Err(Error::CloudDataMissing(why)) if !repaired => {
                    repaired = true;
                    ctx.note(Phase::Upload, Level::Warn, format!("{why}; uploading the missing data again"));
                    let r = self.repair_with(&remote, id, None, ctx)?;
                    if !r.complete() {
                        return Err(Error::CloudDataMissing(format!(
                            "{why}. This computer no longer has the data of {} of those file(s) (first: {}), so it cannot \
                             put it back; the version stays on this computer only",
                            r.unrepairable.len(),
                            r.unrepairable[0]
                        )));
                    }
                    up.publish(id, *sid, force, ctx)?
                }
                other => other?,
            };
            summary.bytes_uploaded += rep.bytes_uploaded;
            if let Some(t) = rep.deferred_until_ms {
                summary.deferred_until_ms = Some(t);
                summary.remaining = (order.len() - i) as u32;
                summary.snapshots.push(rep);
                return Ok(summary);
            }
            summary.snapshots.push(rep);
        }
        up.confirm_visibility(id, ctx)?;
        Ok(summary)
    }

    pub fn reconcile(&self, id: InstanceId, opts: ReconcileOpts, ctx: &OpCtx) -> Result<ReconcileReport> {
        let inst = self.instance(id)?;
        self.guard_provider(&inst)?;
        self.require_state(&inst, &[InstanceState::Ok, InstanceState::Conflict])?;
        self.check_marker(&inst)?;
        let ctx = &ctx.clone().with_instance(id);
        ctx.phase(Phase::Reconcile, format!("reconciling {}", inst.name));
        let _view = match self.discover(id, ctx) {
            Ok(v) => v,
            Err(e) if is_offline(&e) => {
                return Ok(ReconcileReport::Offline { base: inst.base_snapshot, reason: e.to_string() });
            }
            Err(e) => return Err(e),
        };
        let pending = self.pending_changes(id, ctx)?;
        let mut guard = 0;
        let decision = loop {
            let graph = Graph::load(&self.db, id)?;
            match graph::decide(&graph, inst.base_snapshot, pending.dirty) {
                Decision::NeedMoreHistory { of } => {
                    guard += 1;
                    if guard > 5_000 {
                        return Err(Error::Corrupt("snapshot history is too deep or inconsistent to reconcile".into()));
                    }
                    self.manifest(id, of, ctx)?; // verified read fills in the parents
                    // a node whose manifest we just read now has known parents; if not, drop it
                    if let Some(r) = self.db.snapshot_get(id, of)?
                        && !r.parents_known
                    {
                        self.db.snapshot_remove(id, of)?;
                    }
                }
                d => break d,
            }
        };
        match decision {
            Decision::UpToDate { head } => {
                if inst.state == InstanceState::Conflict {
                    self.db.instance_update(id, |r| {
                        r.state = InstanceState::Ok;
                        r.state_detail = None;
                    })?;
                }
                Ok(ReconcileReport::UpToDate { head })
            }
            Decision::CloudEmpty => Ok(ReconcileReport::CloudEmpty),
            Decision::LocalAhead => {
                let graph = Graph::load(&self.db, id)?;
                let snaps = graph.unpublished_in_order();
                if !opts.upload_local || snaps.is_empty() {
                    return Ok(ReconcileReport::LocalAhead { snapshots: snaps, uploaded: false, upload_error: None });
                }
                match self.upload_pending(id, false, ctx) {
                    Ok(s) if s.deferred_until_ms.is_none() => {
                        Ok(ReconcileReport::LocalAhead { snapshots: snaps, uploaded: true, upload_error: None })
                    }
                    Ok(_) => Ok(ReconcileReport::LocalAhead {
                        snapshots: snaps,
                        uploaded: false,
                        upload_error: Some("upload postponed after earlier failures".into()),
                    }),
                    Err(e) if is_offline(&e) => Ok(ReconcileReport::LocalAhead {
                        snapshots: snaps,
                        uploaded: false,
                        upload_error: Some(e.to_string()),
                    }),
                    Err(e) => Err(e),
                }
            }
            Decision::FastForward { to } => {
                let base = inst.base_snapshot;
                let proven = match base {
                    Some(b) => self.verified_ancestor(id, b, to, ctx)?,
                    None => false,
                };
                if !proven {
                    return self.report_conflict(
                        &inst,
                        crate::graph::ConflictKind::Diverged,
                        base,
                        pending.dirty,
                        vec![to],
                        ctx,
                    );
                }
                if !opts.apply_fast_forward {
                    return Ok(ReconcileReport::CloudAhead { head: to });
                }
                self.apply_fast_forward(&inst, to, ctx)
            }
            Decision::Conflict { kind, local, local_dirty, cloud_heads } => {
                self.report_conflict(&inst, kind, local, local_dirty, cloud_heads, ctx)
            }
            Decision::NeedMoreHistory { .. } => unreachable!("resolved in the loop above"),
        }
    }

    fn report_conflict(
        &self,
        inst: &InstanceRow,
        kind: crate::graph::ConflictKind,
        local: Option<SnapshotId>,
        local_dirty: bool,
        cloud: Vec<SnapshotId>,
        ctx: &OpCtx,
    ) -> Result<ReconcileReport> {
        self.db.instance_update(inst.id, |r| {
            r.state = InstanceState::Conflict;
            r.state_detail = Some("this computer and the cloud have independent changes".into());
        })?;
        self.db.event(Some(inst.id), "warn", "conflict", "divergent versions detected; both are preserved");
        Ok(ReconcileReport::Conflict(ConflictReport {
            kind,
            local: local.map(|l| self.summarize(inst.id, l, ctx)),
            local_dirty,
            cloud: cloud.into_iter().map(|c| self.summarize(inst.id, c, ctx)).collect(),
        }))
    }

    fn apply_fast_forward(&self, inst: &InstanceRow, to: SnapshotId, ctx: &OpCtx) -> Result<ReconcileReport> {
        let remote = self.remote()?;
        let target = self.manifest(inst.id, to, ctx)?;
        remote.sync_index_cache(ctx)?;
        let _lock = self.lock(inst.id, "fast-forward")?;
        self.ensure_idle(inst)?;
        // Under the lock, prove again that nothing changed locally (a session may have ended meanwhile).
        let pending = self.pending_changes(inst.id, ctx)?;
        if pending.dirty {
            return self.report_conflict(
                inst,
                crate::graph::ConflictKind::Diverged,
                inst.base_snapshot,
                true,
                vec![to],
                ctx,
            );
        }
        let fresh = self.instance(inst.id)?;
        let (policies, local) = self.restore_scan(&fresh, &target, ctx)?;
        let idle = || self.ensure_idle(&fresh);
        let env = RestoreEnv {
            db: &self.db,
            remote: &remote,
            policies: &policies,
            cfg: &self.cfg.restore,
            writers: Some(&idle),
        };
        let reason = format!("fast-forward to snapshot {}", to.short());
        let rep = restore::restore_in_place(
            &env,
            &fresh,
            &restore::RestoreRequest {
                target: &target,
                target_id: to,
                base: fresh.base_snapshot,
                reason: &reason,
                derive_note: None,
            },
            &local,
            ctx,
        )?;
        self.db.instance_update(inst.id, |r| r.last_synced_ms = Some(crate::db::now_ms()))?;
        ctx.note(Phase::Reconcile, Level::Info, format!("updated to snapshot {} from the cloud", to.short()));
        Ok(ReconcileReport::FastForwarded {
            from: inst.base_snapshot,
            to,
            files_written: rep.written,
            files_deleted: rep.deleted,
        })
    }

    /// Snapshot (when idle and changed) → reconcile → upload. The "Sync now" button.
    pub fn sync_now(&self, id: InstanceId, ctx: &OpCtx) -> Result<(Option<SnapshotReport>, ReconcileReport)> {
        let inst = self.instance(id)?;
        let mut snap = None;
        if inst.state == InstanceState::Ok {
            match self.snapshot(id, SnapshotOpts::default(), ctx) {
                Ok(r) => snap = Some(r),
                // playing: the end-of-session snapshot will cover it — but say so, or "nothing to upload"
                // looks like "nothing changed"
                Err(Error::GameRunning(why)) => ctx.note(
                    Phase::Snapshot,
                    Level::Warn,
                    format!("not saving now, the instance is in use: {why}. It is saved when the game closes."),
                ),
                Err(e) => return Err(e),
            }
        }
        let rep = self.reconcile(id, ReconcileOpts::default(), ctx)?;
        Ok((snap, rep))
    }

    /// Re-hash the whole instance against its base snapshot (a *full* scan, ignoring the index).
    pub fn verify_local(&self, id: InstanceId, ctx: &OpCtx) -> Result<VerifyReport> {
        let inst = self.instance(id)?;
        // it records what it hashes as the trusted file index: never while a restore is half applied
        self.require_state(&inst, &[InstanceState::Ok, InstanceState::Conflict, InstanceState::CrashPending])?;
        self.check_marker(&inst)?;
        let _lock = self.lock(id, "verify")?;
        let (rules, _) = self.rules_for(&inst, ctx)?;
        let base = inst.base_snapshot.map(|b| self.manifest(id, b, ctx)).transpose()?;
        let mut opts = self.cfg.snapshot.scan.clone();
        opts.mode = ScanMode::Full;
        let w = walk(&inst.root, &rules, &HashMap::new(), &opts, ctx)?;
        let cands: Vec<&crate::scan::Candidate> = w.files.iter().collect();
        let sink = CountingSink::new(self.db.clone());
        let hp = hash_phase(
            &inst.root,
            &cands,
            &sink,
            &self.cfg.snapshot.chunking,
            &self.policies_for(&inst),
            opts.threads,
            ctx,
        )?;
        let hashed: HashMap<&str, &crate::scan::HashedFile> = hp.hashed.iter().map(|h| (h.path.as_str(), h)).collect();
        let (mut changed, mut missing, mut extra) = (vec![], vec![], vec![]);
        let mut seen = HashSet::new();
        if let Some(b) = &base {
            for e in b.entries.iter().filter(|e| e.is_file()) {
                match hashed.get(e.p.as_str()) {
                    None => missing.push(e.p.clone()),
                    Some(h) => {
                        seen.insert(e.p.as_str());
                        if e.h != Some(h.content.hash) || e.s != h.content.size {
                            changed.push(e.p.clone());
                        }
                    }
                }
            }
        }
        for p in hashed.keys() {
            if !seen.contains(p) {
                extra.push((*p).to_string());
            }
        }
        changed.sort();
        missing.sort();
        extra.sort();
        // The observation is exact: refresh the index and the verification time.
        let now = crate::scan::now_ns();
        let rows: Vec<crate::db::IndexRow> = hp
            .hashed
            .iter()
            .map(|h| crate::db::IndexRow {
                path: h.path.clone(),
                size: h.stat.size,
                mtime_ns: h.stat.mtime_ns,
                ctime_ns: h.stat.ctime_ns,
                file_id: h.stat.file_id.clone(),
                hash: h.content.hash,
                recipe: h.content.recipe,
                mixed: h.mixed.clone(),
                content_size: h.content.size,
                hashed_at_ns: h.hashed_at_ns,
                verified_at_ns: now,
            })
            .collect();
        self.db.index_replace(id, &rows)?;
        self.db.instance_update(id, |r| {
            r.last_full_verify_ms = Some(crate::db::now_ms());
            r.needs_full_scan = false;
        })?;
        Ok(VerifyReport { files_checked: hp.hashed.len() as u64, changed, missing, extra })
    }

    /// Mark the file index untrusted: the next snapshot hashes everything.
    pub fn rescan(&self, id: InstanceId) -> Result<()> {
        self.db.instance_update(id, |r| r.needs_full_scan = true)?;
        Ok(())
    }

    /// Check that everything a snapshot needs is present in the cloud. `deep` also downloads and
    /// hash-verifies every blob.
    pub fn verify_remote(
        &self,
        id: InstanceId,
        snapshot: Option<SnapshotId>,
        deep: bool,
        ctx: &OpCtx,
    ) -> Result<RemoteVerifyReport> {
        let inst = self.instance(id)?;
        let remote = self.remote()?;
        let sid = snapshot.or(inst.base_snapshot).ok_or_else(|| Error::NotFound("no snapshot to verify".into()))?;
        let m = self.manifest(id, sid, ctx)?;
        remote.sync_index_cache(ctx)?;
        let mut rep = RemoteVerifyReport { snapshot: Some(sid), ..Default::default() };
        let mut missing_packs: HashSet<crate::hash::Hash> = HashSet::new();
        let mut checked_packs: HashSet<crate::hash::Hash> = HashSet::new();
        let known_sizes = self.db.packs_known()?;
        // Existence of the packs behind a set of blobs (strongly consistent `stat`, not a listing).
        let check_packs = |blobs: &HashSet<crate::hash::Hash>,
                           rep: &mut RemoteVerifyReport,
                           missing: &mut HashSet<crate::hash::Hash>,
                           checked: &mut HashSet<crate::hash::Hash>|
         -> Result<HashSet<crate::hash::Hash>> {
            let mut packs = HashSet::new();
            for b in blobs {
                match self.db.blob_lookup(*b)? {
                    Some((p, _)) => {
                        packs.insert(p);
                    }
                    None => rep.unlisted_blobs.push(b.to_hex()),
                }
            }
            for p in &packs {
                if !checked.insert(*p) {
                    continue;
                }
                match remote.stat_pack(*p, ctx)? {
                    Some(info) if known_sizes.get(p).is_none_or(|s| *s == info.size) => {}
                    _ => {
                        missing.insert(*p);
                    }
                }
            }
            Ok(packs)
        };
        // 1. the blobs named by the manifest itself (single blobs and recipes)
        let mut blobs: HashSet<crate::hash::Hash> = HashSet::new();
        let mut recipes: Vec<(&crate::manifest::Entry, crate::hash::Hash)> = vec![];
        for e in m.entries.iter().filter(|e| e.is_file() && e.s > 0) {
            match (e.r, e.h) {
                (Some(r), _) => {
                    blobs.insert(r);
                    recipes.push((e, r));
                }
                (None, Some(h)) => {
                    blobs.insert(h);
                }
                _ => {}
            }
        }
        let root_packs = check_packs(&blobs, &mut rep, &mut missing_packs, &mut checked_packs)?;
        // 2. chunk lists, read only from packs that exist
        let mut chunk_blobs: HashSet<crate::hash::Hash> = HashSet::new();
        for (e, r) in recipes {
            match self.db.blob_lookup(r)? {
                Some((p, _)) if !missing_packs.contains(&p) => {
                    match remote.read_blob(r, ctx).and_then(|b| crate::manifest::Recipe::decode(&b, e.s)) {
                        Ok(rc) => chunk_blobs.extend(rc.chunks.iter().map(|c| c.0)),
                        Err(err) => rep.damaged_blobs.push(format!("recipe of {}: {err}", e.p)),
                    }
                }
                _ => rep.damaged_blobs.push(format!("recipe of {} cannot be read (its pack is missing)", e.p)),
            }
        }
        check_packs(&chunk_blobs, &mut rep, &mut missing_packs, &mut checked_packs)?;
        blobs.extend(chunk_blobs);
        let _ = root_packs;
        rep.blobs_checked = blobs.len() as u64;
        rep.packs_checked = checked_packs.len() as u64;
        rep.missing_packs = missing_packs.iter().map(|p| p.to_hex()).collect();
        rep.missing_packs.sort();
        if deep && rep.missing_packs.is_empty() {
            let dir = tempfile::tempdir().map_err(|e| Error::io("creating temp dir", e))?;
            let all: Vec<_> = blobs.iter().copied().collect();
            if let Err(e) = remote.fetch_blobs(&all, dir.path(), self.cfg.restore.concurrency, ctx) {
                rep.damaged_blobs.push(e.to_string());
            }
        }
        Ok(rep)
    }
}
