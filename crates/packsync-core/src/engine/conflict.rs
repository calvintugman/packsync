//! Restore flows, conflict resolution and crash-save handling.
//!
//! Conflict policy: never merge. Divergent versions are both preserved as complete snapshots, and a
//! human picks which one to continue — or splits them into separate instances. Resolving writes a
//! *merge commit* (a derived snapshot with several parents and the chosen tree), so the graph shows
//! exactly what was decided and every other device converges on it by fast-forward.

use std::path::PathBuf;

use super::*;
use crate::db::{InstanceState, SnapshotState};
use crate::launcher::RegisterRequest;
use crate::manifest::{ForkOrigin, SnapshotKind};
use crate::progress::{Level, Phase};
use crate::restore::{self, RestoreEnv, RestoreReport};
use crate::snapshot::{self, DeriveRequest};

impl Engine {
    pub(super) fn derive(
        &self,
        source: &Manifest,
        parents: Vec<SnapshotId>,
        kind: SnapshotKind,
        note: String,
        instance: Option<(InstanceId, String)>,
        fork_of: Option<ForkOrigin>,
    ) -> Result<SnapshotId> {
        let (id, _) = snapshot::create_derived(
            &self.db,
            &self.cfg.state_dir,
            &DeriveRequest {
                source,
                instance,
                parents,
                kind,
                device: self.device(),
                note: Some(note),
                session: None,
                fork_of,
            },
        )?;
        Ok(id)
    }

    /// A snapshot that exists only on this computer has its content in local staging, not in the
    /// cloud's pack index: restoring it would fail halfway with a confusing "incomplete" error.
    fn require_uploaded(&self, id: InstanceId, snapshot: SnapshotId) -> Result<()> {
        if let Some(row) = self.db.snapshot_get(id, snapshot)?
            && matches!(row.state, SnapshotState::LocalComplete | SnapshotState::Uploading)
        {
            return Err(Error::Unsupported(format!(
                "snapshot {} has not been uploaded yet, so it cannot be restored from the cloud. Upload it first (`packsync upload`), then restore",
                snapshot.short()
            )));
        }
        Ok(())
    }

    /// What restoring `snapshot` over this instance would change. Reads the cloud only for the
    /// manifest; touches nothing.
    /// Content released by a cloud cleanup (retention) cannot be restored any more.
    fn require_not_expired(&self, id: InstanceId, snapshot: SnapshotId, ctx: &OpCtx) -> Result<()> {
        if let Ok(remote) = self.remote()
            && let Ok(expired) = remote.list_expired(id, ctx)
            && expired.contains(&snapshot)
        {
            return Err(Error::Unsupported(format!(
                "the data of snapshot {} was released by a cloud cleanup (retention), so it can no longer be restored. Newer snapshots are not affected",
                snapshot.short()
            )));
        }
        Ok(())
    }

    pub fn restore_preview(&self, id: InstanceId, snapshot: SnapshotId, ctx: &OpCtx) -> Result<RestorePreview> {
        let inst = self.instance(id)?;
        self.require_uploaded(id, snapshot)?;
        self.require_not_expired(id, snapshot, ctx)?;
        let target = self.manifest(id, snapshot, ctx)?;
        let (_policies, local) = self.restore_scan(&inst, &target, ctx)?;
        let mut problems = vec![];
        let local_changes = self.pending_changes(id, ctx).map(|p| p.dirty).unwrap_or(false);
        let summary = self.summarize(id, snapshot, ctx);
        match restore::plan(&target, snapshot, &local, &inst.root, &self.cfg.restore) {
            Ok(plan) => {
                if let Err(e) = restore::preflight(&plan, &inst.root, &self.cfg.restore) {
                    problems.push(e.to_string());
                }
                let sample = |ops: &[crate::journal::JOp], writes: bool| -> Vec<String> {
                    ops.iter()
                        .filter(|o| {
                            if writes {
                                matches!(o, crate::journal::JOp::Write { .. })
                            } else {
                                matches!(o, crate::journal::JOp::Delete { .. })
                            }
                        })
                        .take(20)
                        .map(|o| o.path().to_string())
                        .collect()
                };
                Ok(RestorePreview {
                    target: summary,
                    files_to_write: plan.writes.len() as u64,
                    files_unchanged: plan.unchanged,
                    files_to_delete: plan.deleted,
                    bytes_to_write: plan.bytes_to_write,
                    local_only_preserved: plan.preserved_local_only,
                    sample_writes: sample(&plan.ops, true),
                    sample_deletes: sample(&plan.ops, false),
                    skipped: plan.skipped,
                    local_changes,
                    problems,
                })
            }
            Err(e) => Ok(RestorePreview {
                target: summary,
                files_to_write: 0,
                files_unchanged: 0,
                files_to_delete: 0,
                bytes_to_write: 0,
                local_only_preserved: 0,
                sample_writes: vec![],
                sample_deletes: vec![],
                skipped: vec![],
                local_changes,
                problems: vec![e.to_string()],
            }),
        }
    }

    /// Restore a snapshot over the existing instance (history restore). Local changes are first
    /// preserved as their own snapshot; the previous files are kept as a recovery point; the result is
    /// recorded as a new snapshot so every device converges on it.
    pub fn restore_snapshot(
        &self,
        id: InstanceId,
        snapshot: SnapshotId,
        opts: RestoreOpts,
        ctx: &OpCtx,
    ) -> Result<RestoreReport> {
        let inst = self.instance(id)?;
        self.guard_provider(&inst)?;
        self.require_state(&inst, &[InstanceState::Ok])?;
        self.require_uploaded(id, snapshot)?;
        self.require_not_expired(id, snapshot, ctx)?;
        let ctx = &ctx.clone().with_instance(id);
        let remote = self.remote()?;
        // The history must not have moved on without us.
        match self.discover(id, ctx) {
            Ok(view) => {
                let ok = match (view.heads.as_slice(), inst.base_snapshot) {
                    ([], _) => true,
                    ([h], Some(b)) => *h == b || self.verified_ancestor(id, *h, b, ctx)?,
                    _ => false,
                };
                if !ok {
                    return Err(Error::Conflict(
                        "the cloud has changes this computer has not applied yet; sync first, then restore".into(),
                    ));
                }
            }
            Err(Error::Storage(_)) => {} // offline: restoring from a cached manifest is still allowed
            Err(e) => return Err(e),
        }
        let target = self.manifest(id, snapshot, ctx)?;
        if target.instance.id != id {
            return Err(Error::Invalid { what: "snapshot", detail: "belongs to a different instance".into() });
        }
        let _ = remote.sync_index_cache(ctx);
        let _lock = self.lock(id, "restore")?;
        self.ensure_idle(&inst)?;
        // 1. recovery point: preserve local changes as a snapshot of their own
        let pending = self.pending_changes(id, ctx)?;
        if pending.dirty {
            self.snapshot_locked(
                &inst,
                SnapshotOpts {
                    note: Some(format!("local state before restoring snapshot {}", snapshot.short())),
                    ..Default::default()
                },
                ctx,
            )?;
        }
        let fresh = self.instance(id)?;
        let before = fresh.base_snapshot;
        // 2. apply
        let (policies, local) = self.restore_scan(&fresh, &target, ctx)?;
        let mut cfg = self.cfg.restore.clone();
        cfg.skip_incompatible = opts.skip_incompatible;
        let idle = || self.ensure_idle(&fresh);
        let env = RestoreEnv { db: &self.db, remote: &remote, policies: &policies, cfg: &cfg, writers: Some(&idle) };
        let reason = format!("restore of snapshot {}", snapshot.short());
        let derive_note = format!("restored from snapshot {}", snapshot.short());
        let rep = restore::restore_in_place(
            &env,
            &fresh,
            &restore::RestoreRequest {
                target: &target,
                target_id: snapshot,
                base: before,
                reason: &reason,
                derive_note: Some(&derive_note),
            },
            &local,
            ctx,
        )?;
        // 3. record the restore as a new snapshot (unless it is simply the current base)
        if before != Some(snapshot) {
            let new_id =
                self.derive(&target, before.into_iter().collect(), SnapshotKind::Normal, derive_note, None, None)?;
            self.db.instance_update(id, |r| r.base_snapshot = Some(new_id))?;
        }
        ctx.note(Phase::Apply, Level::Info, format!("restored snapshot {}", snapshot.short()));
        Ok(rep)
    }

    /// Restore a cloud instance onto this computer as a new folder, then register it with the
    /// launcher. The folder is staged under a hidden name and only renamed when complete and verified.
    pub fn restore_new(&self, req: RestoreNewRequest, ctx: &OpCtx) -> Result<RestoredInstance> {
        if let Some(existing) = self.db.instance_get(req.instance)? {
            return Err(Error::Invalid {
                what: "instance",
                detail: format!(
                    "this computer already has the instance at {}; restore a snapshot there instead",
                    existing.root.display()
                ),
            });
        }
        // held until the new instance is registered: startup recovery in another process leaves its folder alone
        let _restoring =
            crate::lock::InstanceLock::try_acquire_kind(&self.locks_dir(), req.instance, "restore-new", "restore")
                .map_err(|e| match e {
                    Error::Busy(_) => Error::Busy("this instance is already being restored on this computer".into()),
                    other => other,
                })?;
        let remote = self.remote()?;
        let ctx = &ctx.clone().with_instance(req.instance);
        let view = self.discover(req.instance, ctx)?;
        let sid =
            match req.snapshot {
                Some(s) => s,
                None => match view.heads.as_slice() {
                    [h] => *h,
                    [] => return Err(Error::NotFound("this instance has no published snapshot in the cloud".into())),
                    _ => return Err(Error::Conflict(
                        "the cloud has several independent versions of this instance; choose which snapshot to restore"
                            .into(),
                    )),
                },
            };
        let target = self.manifest(req.instance, sid, ctx)?;
        if target.instance.id != req.instance {
            return Err(Error::Invalid { what: "snapshot", detail: "belongs to a different instance".into() });
        }
        remote.sync_index_cache(ctx)?;
        let launcher = self.launcher(&req.launcher);
        std::fs::create_dir_all(&req.parent_dir)
            .map_err(|e| Error::io(format!("creating {}", req.parent_dir.display()), e))?;
        let staged: PathBuf =
            req.parent_dir.join(format!(".packsync-restore-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
        let mut cfg = self.cfg.restore.clone();
        cfg.skip_incompatible = req.skip_incompatible;
        let policies = {
            let mut m = std::collections::HashMap::new();
            let p = crate::mixed::options_txt_policy(&target.layout.game_root);
            m.insert(p.id.clone(), p);
            for p in launcher.mixed_policies() {
                m.insert(p.id.clone(), p);
            }
            m
        };
        let env = RestoreEnv { db: &self.db, remote: &remote, policies: &policies, cfg: &cfg, writers: None };
        let report = restore::restore_new(&env, &staged, req.instance, &target, sid, ctx)?;
        crate::failpoint!("restore_new.before_register")?;
        let reg = match launcher.register_restored(&RegisterRequest {
            staged_root: staged.clone(),
            target: req.launcher_ref.clone(),
            display_name: target.instance.name.clone(),
            preferred_folder: req.folder_name.clone(),
            local: req.local.clone(),
            instance_id: req.instance,
        }) {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staged);
                return Err(e);
            }
        };
        let marker = uuid::Uuid::new_v4().simple().to_string();
        crate::fsutil::write_atomic(&instances::marker_path(&reg.root), marker.as_bytes())
            .map_err(|e| Error::io("writing the instance marker", e))?;
        let now = crate::db::now_ms();
        self.db.instance_insert(&InstanceRow {
            id: req.instance,
            name: target.instance.name.clone(),
            root: dunce::canonicalize(&reg.root).unwrap_or(reg.root.clone()),
            launcher: req.launcher.clone(),
            launcher_ref: serde_json::to_string(&reg.launcher_ref).ok(),
            game_root: reg.game_root.clone(),
            sync_enabled: true,
            base_snapshot: Some(sid),
            pending_rules: None,
            marker,
            root_dev: None,
            provider_id: self.storage_id(),
            state: InstanceState::Ok,
            state_detail: None,
            created_ms: now,
            last_scan_ms: None,
            last_full_verify_ms: None,
            last_synced_ms: Some(now),
            needs_full_scan: true,
        })?;
        restore::finish_new_restore(&self.db, &staged, &reg.root);
        Ok(RestoredInstance { instance: req.instance, root: reg.root, snapshot: sid, notes: reg.notes, report })
    }

    pub fn list_recovery_points(&self, id: InstanceId) -> Result<Vec<RecoveryPointInfo>> {
        let inst = self.instance(id)?;
        let mut out = vec![];
        if let Ok(rd) = std::fs::read_dir(crate::journal::recovery_root(&inst.root)) {
            for e in rd.flatten() {
                if let Ok(b) = std::fs::read(e.path().join("recovery.json"))
                    && let Ok(n) = serde_json::from_slice::<crate::journal::RecoveryNote>(&b)
                {
                    out.push(RecoveryPointInfo {
                        tx: n.tx.to_string(),
                        created: n.created,
                        from: n.from,
                        to: n.to,
                        files: n.files,
                        bytes: n.bytes,
                        reason: n.reason,
                        path: e.path(),
                    });
                }
            }
        }
        out.sort_by(|a, b| b.created.cmp(&a.created));
        Ok(out)
    }

    // ------------------------------------------------------------------------------ conflicts

    pub fn resolve_conflict(&self, id: InstanceId, res: Resolution, ctx: &OpCtx) -> Result<ResolveReport> {
        let inst = self.instance(id)?;
        self.guard_provider(&inst)?;
        self.require_state(&inst, &[InstanceState::Conflict, InstanceState::Ok])?;
        let remote = self.remote()?;
        let ctx = &ctx.clone().with_instance(id);
        let _lock = self.lock(id, "resolve-conflict")?;
        self.ensure_idle(&inst)?;
        ctx.phase(Phase::Conflict, "resolving a conflict (both versions are kept)");
        // The cloud must be reachable: a merge commit is only meaningful relative to what it saw.
        self.discover(id, ctx)?;
        // Preserve everything on this computer as a complete snapshot first.
        let pending = self.pending_changes(id, ctx)?;
        if pending.dirty {
            self.snapshot_locked(
                &inst,
                SnapshotOpts {
                    note: Some("local changes preserved while resolving a conflict".into()),
                    ..Default::default()
                },
                ctx,
            )?;
        }
        let inst = self.instance(id)?;
        let local = inst.base_snapshot.ok_or_else(|| Error::NotFound("this instance has no local snapshot".into()))?;
        match res {
            Resolution::KeepLocal => {
                self.upload_pending(id, true, ctx)?;
                let view = self.discover(id, ctx)?;
                let lm = self.manifest(id, local, ctx)?;
                let mut parents = vec![local];
                parents.extend(view.heads.iter().copied().filter(|h| *h != local));
                let m = self.derive(
                    &lm,
                    parents,
                    SnapshotKind::Normal,
                    "resolved conflict: continued this computer's version".into(),
                    None,
                    None,
                )?;
                self.db.instance_update(id, |r| {
                    r.base_snapshot = Some(m);
                    r.state = InstanceState::Ok;
                    r.state_detail = None;
                })?;
                self.upload_pending(id, true, ctx)?;
                Ok(ResolveReport {
                    resolution: "keep_local".into(),
                    head: m,
                    new_instance: None,
                    preserved_local: Some(local),
                    uploaded: true,
                })
            }
            Resolution::KeepCloud { head } => {
                // retain this computer's version in the cloud as a branch before replacing it
                self.upload_pending(id, true, ctx)?;
                let view = self.discover(id, ctx)?;
                let cloud: Vec<SnapshotId> = view.heads.iter().copied().filter(|h| *h != local).collect();
                let target = match head {
                    Some(h) if view.heads.contains(&h) => h,
                    Some(_) => {
                        return Err(Error::Invalid {
                            what: "snapshot",
                            detail: "is not a current cloud version".into(),
                        });
                    }
                    None if cloud.len() == 1 => cloud[0],
                    None if view.heads.len() == 1 => view.heads[0],
                    None => {
                        return Err(Error::Conflict(
                            "the cloud has several versions; say which one to continue".into(),
                        ));
                    }
                };
                let tm = self.manifest(id, target, ctx)?;
                remote.sync_index_cache(ctx)?;
                let (policies, st) = self.restore_scan(&inst, &tm, ctx)?;
                let idle = || self.ensure_idle(&inst);
                let env = RestoreEnv {
                    db: &self.db,
                    remote: &remote,
                    policies: &policies,
                    cfg: &self.cfg.restore,
                    writers: Some(&idle),
                };
                restore::restore_in_place(
                    &env,
                    &inst,
                    &restore::RestoreRequest {
                        target: &tm,
                        target_id: target,
                        base: Some(local),
                        reason: "conflict resolved: continuing the cloud version",
                        derive_note: None,
                    },
                    &st,
                    ctx,
                )?;
                let mut parents = vec![target];
                if local != target {
                    parents.push(local);
                }
                parents.extend(view.heads.iter().copied().filter(|h| *h != target && *h != local));
                let m = self.derive(
                    &tm,
                    parents,
                    SnapshotKind::Normal,
                    format!("resolved conflict: continued the cloud version (this computer's version {} is kept in history)", local.short()),
                    None,
                    None,
                )?;
                self.db.instance_update(id, |r| {
                    r.base_snapshot = Some(m);
                    r.state = InstanceState::Ok;
                    r.state_detail = None;
                })?;
                self.upload_pending(id, true, ctx)?;
                Ok(ResolveReport {
                    resolution: "keep_cloud".into(),
                    head: m,
                    new_instance: None,
                    preserved_local: Some(local),
                    uploaded: true,
                })
            }
            Resolution::SplitLocal { new_name } => {
                let name = new_name.trim().to_string();
                if name.is_empty() {
                    return Err(Error::Invalid { what: "name", detail: "give the separate instance a name".into() });
                }
                // This computer's version must be in the cloud before anything refers to it: the fork below is
                // derived from it (its root snapshot lists blobs that exist nowhere else yet), and the
                // original instance keeps it as a retired branch. KeepLocal and KeepCloud upload first too.
                // A failed upload stops here, before the instance is renamed or anything is published.
                self.upload_pending(id, true, ctx)?;
                let view = self.discover(id, ctx)?;
                // Retire this computer's branch in the original instance (it is in the cloud now).
                let others: Vec<SnapshotId> = view.heads.iter().copied().filter(|h| *h != local).collect();
                let local_in_cloud = self.db.snapshot_get(id, local)?.is_some_and(|r| r.state.is_in_cloud());
                if local_in_cloud && others.len() == 1 {
                    let hm = self.manifest(id, others[0], ctx)?;
                    self.derive(
                        &hm,
                        vec![others[0], local],
                        SnapshotKind::Normal,
                        format!("conflict resolved: this version was split off into the separate instance \"{name}\""),
                        None,
                        None,
                    )?;
                    self.upload_pending(id, true, ctx)?;
                } else if local_in_cloud && others.len() > 1 {
                    return Err(Error::Conflict(
                        "the cloud has several independent versions already; resolve those first".into(),
                    ));
                }
                // The fork: a brand-new instance whose root snapshot is this computer's current state.
                let new_id = InstanceId::new();
                let lm = self.manifest(id, local, ctx)?;
                let root_snap = self.derive(
                    &lm,
                    vec![],
                    SnapshotKind::Normal,
                    format!("split off from \"{}\" (snapshot {}) during conflict resolution", inst.name, local.short()),
                    Some((new_id, name.clone())),
                    Some(ForkOrigin { instance: id, snapshot: local }),
                )?;
                let marker = uuid::Uuid::new_v4().simple().to_string();
                crate::fsutil::write_atomic(&instances::marker_path(&inst.root), marker.as_bytes())
                    .map_err(|e| Error::io("updating the instance marker", e))?;
                self.db.instance_rekey(id, new_id, &name, root_snap, &marker)?;
                self.upload_pending(new_id, true, ctx)?;
                let uploaded = self.db.snapshot_get(new_id, root_snap)?.is_some_and(|r| r.state.is_in_cloud());
                Ok(ResolveReport {
                    resolution: "split_local".into(),
                    head: root_snap,
                    new_instance: Some(new_id),
                    preserved_local: Some(local),
                    uploaded,
                })
            }
        }
    }

    // ---------------------------------------------------------------------------- crash saves

    /// The user checked the world after a crash and wants to continue from the post-crash state:
    /// publish it as an ordinary snapshot on top of the last known-good one.
    pub fn promote_crash(&self, id: InstanceId, ctx: &OpCtx) -> Result<SnapshotId> {
        let inst = self.instance(id)?;
        self.require_state(&inst, &[InstanceState::CrashPending])?;
        let _lock = self.lock(id, "promote-crash")?;
        let ctx = &ctx.clone().with_instance(id);
        let crash = inst.base_snapshot.ok_or_else(|| Error::NotFound("no crash snapshot".into()))?;
        let cm = self.manifest(id, crash, ctx)?;
        if cm.kind != SnapshotKind::Crash {
            return Err(Error::Invalid { what: "snapshot", detail: "the current base is not a crash save".into() });
        }
        // The promoted snapshot has the crash save's own tree, so it must also have the crash save as a
        // parent: that makes the uploader publish the crash save (and its data) first. With only the
        // crash save's parents there was no dependency, and the promoted head could be published while
        // the crash save's content was still waiting to upload.
        let mut parents = cm.parents.clone();
        parents.push(crash);
        let n = self.derive(
            &cm,
            parents,
            SnapshotKind::Normal,
            format!("promoted crash save {} after checking the world", crash.short()),
            None,
            None,
        )?;
        self.db.instance_update(id, |r| {
            r.base_snapshot = Some(n);
            r.state = InstanceState::Ok;
            r.state_detail = None;
        })?;
        Ok(n)
    }

    /// Go back to the last known-good snapshot. The crash save stays in the cloud (quarantined)
    /// and the replaced files stay in a recovery point.
    pub fn discard_crash(&self, id: InstanceId, ctx: &OpCtx) -> Result<RestoreReport> {
        let inst = self.instance(id)?;
        self.require_state(&inst, &[InstanceState::CrashPending])?;
        let _lock = self.lock(id, "discard-crash")?;
        self.ensure_idle(&inst)?;
        let ctx = &ctx.clone().with_instance(id);
        let crash = inst.base_snapshot.ok_or_else(|| Error::NotFound("no crash snapshot".into()))?;
        let cm = self.manifest(id, crash, ctx)?;
        let good =
            *cm.parents.first().ok_or_else(|| Error::NotFound("the crash save has no known-good parent".into()))?;
        let gm = self.manifest(id, good, ctx)?;
        let remote = self.remote()?;
        remote.sync_index_cache(ctx)?;
        let (policies, st) = self.restore_scan(&inst, &gm, ctx)?;
        let idle = || self.ensure_idle(&inst);
        let env = RestoreEnv {
            db: &self.db,
            remote: &remote,
            policies: &policies,
            cfg: &self.cfg.restore,
            writers: Some(&idle),
        };
        let rep = restore::restore_in_place(
            &env,
            &inst,
            &restore::RestoreRequest {
                target: &gm,
                target_id: good,
                base: Some(crash),
                reason: "discarded a crash save",
                derive_note: None,
            },
            &st,
            ctx,
        )?;
        self.db.instance_update(id, |r| {
            r.state = InstanceState::Ok;
            r.state_detail = None;
        })?;
        Ok(rep)
    }
}
