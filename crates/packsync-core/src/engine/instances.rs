//! Instance-level operations: preview (dry run), enable sync, status, history, cloud listing.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::*;
use crate::brand::LOCAL_MARKER_DIR;
use crate::db::{InstanceState, SnapshotState, now_ms};
use crate::graph::Graph;
use crate::launcher::DiscoveredInstance;
use crate::manifest::SnapshotKind;
use crate::rules::{self};
use crate::scan::{self, ScanMode, ScanOptions, hash_phase, walk};
use crate::stage::CountingSink;

pub(crate) fn marker_path(root: &Path) -> PathBuf {
    root.join(LOCAL_MARKER_DIR).join("marker")
}

/// Database key prefix: unknown content found by the last save that is worth asking about.
const UNKNOWN: &str = "unknown_content:";

impl Engine {
    pub fn instances(&self) -> Result<Vec<InstanceRow>> {
        self.db.instances()
    }

    pub fn find_instance_by_root(&self, root: &Path) -> Result<Option<InstanceRow>> {
        let canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        self.db.instance_by_root(&canon)
    }

    /// Dry run: what would be synced, what would not and why. Reads metadata only (unless an upload
    /// estimate is requested). Nothing is written, in the instance or anywhere else.
    pub fn preview(&self, req: &PreviewRequest, ctx: &OpCtx) -> Result<PreviewReport> {
        let launcher = self.launcher(&req.launcher);
        let desc = launcher.describe(&req.root)?;
        let text = rules::effective_rules_text(launcher.rules_fragment(), &req.rules_overrides)?;
        let rs = RuleSet::compile(&text, &desc.game_root)?;
        let opts = ScanOptions { mode: ScanMode::Quick, ..self.cfg.snapshot.scan.now() };
        let w = walk(&req.root, &rs, &HashMap::new(), &opts, ctx)?;
        let mut by_cat: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut samples = vec![];
        let mut per_cat_samples: HashMap<String, usize> = HashMap::new();
        for f in &w.files {
            let e = by_cat.entry(f.decision.category.clone()).or_default();
            e.0 += 1;
            e.1 += f.stat.size;
            let n = per_cat_samples.entry(f.decision.category.clone()).or_default();
            if *n < 3 {
                *n += 1;
                samples.push((f.decision.category.clone(), f.path.clone(), f.stat.size));
            }
        }
        let mut estimated = None;
        if req.estimate_upload {
            let remote = self.remote().ok();
            if let Some(r) = &remote {
                let _ = r.sync_index_cache(ctx);
            }
            let sink = CountingSink::new(self.db.clone());
            let cands: Vec<&scan::Candidate> = w.files.iter().collect();
            let policies = self.policies_for(&InstanceRow {
                game_root: desc.game_root.clone(),
                launcher: req.launcher.clone(),
                ..self.placeholder_row(&req.root, &desc)
            });
            hash_phase(&req.root, &cands, &sink, &self.cfg.snapshot.chunking, &policies, opts.threads, ctx)?;
            estimated = Some(sink.new_bytes.load(std::sync::atomic::Ordering::Relaxed));
        }
        Ok(PreviewReport {
            name: desc.name.clone(),
            game_root: desc.game_root.clone(),
            pack: desc.identity.clone(),
            worlds: desc.worlds.clone(),
            included_files: w.files.len() as u64,
            included_bytes: w.included_bytes(),
            included_by_category: by_cat
                .into_iter()
                .map(|(category, (files, bytes))| IncludedGroup { category, files, bytes })
                .collect(),
            empty_dirs: w.empty_dirs.len() as u64,
            issues: w.issues,
            estimated_upload_bytes: estimated,
            suggestions: rules::suggest::suggest(&w.excluded.unmatched_groups, &desc.game_root),
            excluded: w.excluded,
            rules_text: text,
            samples,
        })
    }

    fn placeholder_row(&self, root: &Path, desc: &DiscoveredInstance) -> InstanceRow {
        InstanceRow {
            id: InstanceId::new(),
            name: desc.name.clone(),
            root: root.to_path_buf(),
            launcher: desc.launcher_ref.launcher.clone(),
            launcher_ref: None,
            game_root: desc.game_root.clone(),
            sync_enabled: false,
            base_snapshot: None,
            pending_rules: None,
            rule_decisions: None,
            marker: String::new(),
            root_dev: None,
            provider_id: None,
            state: InstanceState::Ok,
            state_detail: None,
            created_ms: now_ms(),
            last_scan_ms: None,
            last_full_verify_ms: None,
            last_synced_ms: None,
            needs_full_scan: true,
        }
    }

    /// Register an instance for syncing and create its first (local) snapshot. The caller then
    /// uploads it ([`Engine::upload_pending`]); until that completes the instance is **not** synced.
    pub fn enable_sync(&self, req: EnableRequest, ctx: &OpCtx) -> Result<EnableOutcome> {
        let root = dunce::canonicalize(&req.root)
            .map_err(|e| Error::InstanceUnavailable(format!("{}: {e}", req.root.display())))?;
        if self.db.instance_by_root(&root)?.is_some() {
            return Err(Error::Invalid {
                what: "instance",
                detail: format!("{} is already registered", root.display()),
            });
        }
        let launcher = self.launcher(&req.launcher);
        let desc = launcher.describe(&root)?;
        let text = rules::suggest::apply_decisions(
            &rules::effective_rules_text(launcher.rules_fragment(), &req.rules_overrides)?,
            &req.decisions,
        )?;
        let rs = RuleSet::compile(&text, &desc.game_root)?;
        let id = InstanceId::new();
        let mut row = self.placeholder_row(&root, &desc);
        row.id = id;
        row.name = req.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or(desc.name.clone());
        row.launcher = req.launcher.clone();
        row.launcher_ref = serde_json::to_string(&req.launcher_ref.clone().unwrap_or(desc.launcher_ref.clone())).ok();
        row.sync_enabled = true;
        row.pending_rules = Some(text);
        row.marker = uuid::Uuid::new_v4().simple().to_string();
        row.provider_id = self.storage_id();
        for pid in rs.mixed_ids() {
            if !self.policies_for(&row).contains_key(&pid) {
                return Err(Error::Config(format!("the rules refer to an unknown mixed-file policy {pid:?}")));
            }
        }
        self.ensure_idle(&row)?;
        // Register first, atomically with the "is this folder registered already?" check, and only then
        // write the folder's marker: two requests enabling the same folder at once (a double click, the CLI
        // and the app) cannot both register it, and the loser never overwrites the winner's marker.
        if !self.db.instance_insert_new_root(&row)? {
            return Err(Error::Invalid {
                what: "instance",
                detail: format!("{} is already registered", root.display()),
            });
        }
        if let Err(e) = crate::fsutil::write_atomic(&marker_path(&root), row.marker.as_bytes()) {
            let _ = self.db.instance_remove(id);
            return Err(Error::io("writing the instance marker", e));
        }
        let undo = |e: Error| -> Error {
            if !crate::failpoint::is_crash(&e) {
                let _ = self.db.instance_remove(id);
                let _ = std::fs::remove_file(marker_path(&root));
            }
            e
        };
        let rep = self
            .snapshot(
                id,
                SnapshotOpts { force: true, note: Some("initial snapshot".into()), ..Default::default() },
                ctx,
            )
            .map_err(undo)?;
        let snap = match rep.result {
            SnapshotResult::Created { id } => id,
            SnapshotResult::Unchanged { base } => base,
        };
        Ok(EnableOutcome { instance: id, snapshot: snap, stats: rep.stats, issues: rep.issues })
    }

    /// Stop syncing on this computer: forget the registration and local bookkeeping. **Cloud data
    /// and local files are untouched.**
    pub fn disable_sync(&self, id: InstanceId) -> Result<()> {
        let inst = self.instance(id)?;
        let _lock = self.lock(id, "disable")?;
        let _ = std::fs::remove_file(marker_path(&inst.root));
        self.db.instance_remove(id)
    }

    pub fn status(&self, id: InstanceId, online: bool, ctx: &OpCtx) -> Result<InstanceStatus> {
        let inst = self.instance(id)?;
        let rows = self.db.snapshots(id)?;
        let base_row = inst.base_snapshot.and_then(|b| rows.iter().find(|r| r.id == b));
        let unpublished: Vec<_> = rows
            .iter()
            .filter(|r| matches!(r.state, SnapshotState::LocalComplete | SnapshotState::Uploading))
            .collect();
        let (mut pack_bytes, mut next_retry, mut last_error) = (0u64, None::<i64>, None::<String>);
        for s in &unpublished {
            for p in self.db.staged_for(s.id)?.into_iter().filter(|p| !p.uploaded) {
                pack_bytes += p.size;
                if let Some(t) = p.next_attempt_ms {
                    next_retry = Some(next_retry.map_or(t, |c| c.min(t)));
                }
                if p.last_error.is_some() {
                    last_error = p.last_error.clone();
                }
            }
        }
        // a publication refused because data it builds on is missing from the cloud
        if last_error.is_none() && !unpublished.is_empty() {
            last_error = self.db.publish_blocked(id)?;
        }
        let mut phase = if !inst.sync_enabled {
            SyncPhase::NotEnabled
        } else {
            match inst.state {
                InstanceState::Conflict => SyncPhase::Conflict,
                InstanceState::CrashPending => SyncPhase::CrashPending,
                InstanceState::Restoring => SyncPhase::Restoring,
                InstanceState::Unavailable => SyncPhase::Unavailable,
                InstanceState::Ok => match base_row.map(|r| r.state) {
                    None => SyncPhase::NotEnabled,
                    Some(SnapshotState::LocalComplete) if last_error.is_none() && next_retry.is_none() => {
                        SyncPhase::LocalSnapshotComplete
                    }
                    Some(SnapshotState::LocalComplete) | Some(SnapshotState::Uploading) => SyncPhase::UploadPending,
                    Some(SnapshotState::Published) => SyncPhase::CloudPublished,
                    Some(SnapshotState::Visible) | Some(SnapshotState::Remote) => SyncPhase::ReadyOnOtherDevices,
                },
            }
        };
        let mut playing_elsewhere = vec![];
        if online
            && inst.sync_enabled
            && self.has_storage()
            && let Ok(view) = self.discover(id, ctx)
        {
            let graph = Graph::load(&self.db, id)?;
            if phase == SyncPhase::ReadyOnOtherDevices {
                if let crate::graph::Decision::FastForward { .. } =
                    crate::graph::decide(&graph, inst.base_snapshot, false)
                {
                    phase = SyncPhase::CloudAhead;
                } else if let crate::graph::Decision::Conflict { .. } =
                    crate::graph::decide(&graph, inst.base_snapshot, false)
                {
                    phase = SyncPhase::Conflict;
                }
            }
            let now = time::OffsetDateTime::now_utc();
            playing_elsewhere = view
                .leases
                .iter()
                .filter(|l| l.device_id != self.device().0 && !l.is_stale(now))
                .map(|l| l.device_name.clone())
                .collect();
        }
        let manifest = inst.base_snapshot.and_then(|b| self.manifest(id, b, ctx).ok());
        Ok(InstanceStatus {
            id,
            name: inst.name.clone(),
            root: inst.root.clone(),
            launcher: inst.launcher.clone(),
            sync_enabled: inst.sync_enabled,
            state: inst.state,
            state_detail: inst.state_detail.clone(),
            phase,
            base: inst.base_snapshot,
            base_state: base_row.map(|r| r.state),
            last_synced_ms: inst.last_synced_ms,
            last_synced_by: rows
                .iter()
                .filter(|r| r.state.is_in_cloud())
                .max_by_key(|r| r.created.clone())
                .and_then(|r| r.device_name.clone()),
            pending_snapshots: unpublished.len() as u32,
            pending_pack_bytes: pack_bytes,
            next_retry_ms: next_retry,
            last_error,
            worlds: manifest.as_ref().map(|m| m.worlds.clone()).unwrap_or_default(),
            pack: manifest.as_ref().map(|m| m.pack.clone()).unwrap_or_default(),
            total_files: manifest.as_ref().map(|m| m.stats.files).unwrap_or(0),
            total_bytes: manifest.as_ref().map(|m| m.stats.bytes).unwrap_or(0),
            playing_elsewhere,
            provider_id: inst.provider_id.clone(),
            unknown: self.unknown_content(id)?,
        })
    }

    /// Content of `id` no rule covers and nobody has decided about, as found by the last save.
    pub fn unknown_content(&self, id: InstanceId) -> Result<Vec<rules::suggest::Suggestion>> {
        Ok(self.db.meta_get(&format!("{UNKNOWN}{id}"))?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
    }

    /// Record what the last scan found that no rule covers (see [`Engine::unknown_content`]). Groups
    /// PackSync is sure are caches or tool folders are not worth asking about and are left out.
    pub(crate) fn record_unknown(
        &self,
        inst: &InstanceRow,
        groups: &std::collections::BTreeMap<String, (u64, u64)>,
    ) -> Result<()> {
        let ask: Vec<_> =
            rules::suggest::suggest(groups, &inst.game_root).into_iter().filter(|s| s.include || !s.sure).collect();
        self.db.meta_set(&format!("{UNKNOWN}{}", inst.id), &serde_json::to_string(&ask)?)
    }

    /// Apply the person's answers about unknown content: they become rules of the instance, used by
    /// the next save (which stores them in the snapshot, so every computer follows them).
    pub fn decide_unknown(&self, id: InstanceId, decisions: &[rules::suggest::RuleDecision]) -> Result<()> {
        if decisions.is_empty() {
            return Ok(());
        }
        let _lock = self.lock(id, "decide")?;
        let inst = self.instance(id)?;
        // later answers about the same path replace earlier ones
        let mut all = super::rule_decisions(&inst);
        for d in decisions {
            all.retain(|e| e.path != d.path);
            all.push(d.clone());
        }
        let json = serde_json::to_string(&all)?;
        // must compile together with the instance's rules
        let mut probe = inst.clone();
        probe.rule_decisions = Some(json.clone());
        self.rules_for(&probe, &OpCtx::detached())?;
        self.db.instance_update(id, |r| r.rule_decisions = Some(json.clone()))?;
        let decided: std::collections::HashSet<&str> = decisions.iter().map(|d| d.path.as_str()).collect();
        let left: Vec<_> =
            self.unknown_content(id)?.into_iter().filter(|s| !decided.contains(s.path.as_str())).collect();
        self.db.meta_set(&format!("{UNKNOWN}{id}"), &serde_json::to_string(&left)?)
    }

    pub fn history(&self, id: InstanceId, online: bool, ctx: &OpCtx) -> Result<Vec<HistoryEntry>> {
        let inst = self.instance(id)?;
        if online && self.has_storage() {
            let _ = self.discover(id, ctx);
        }
        let rows = self.db.snapshots(id)?;
        let graph = Graph::from_rows(&rows);
        let heads = graph.cloud_heads();
        let expired = if online && self.has_storage() {
            self.remote().and_then(|r| r.list_expired(id, ctx)).unwrap_or_default()
        } else {
            Default::default()
        };
        Ok(rows
            .into_iter()
            .map(|r| HistoryEntry {
                id: r.id,
                kind: r.kind,
                created: r.created,
                device_id: r.device_id,
                device: r.device_name,
                files: r.files,
                bytes: r.bytes,
                note: r.note,
                outcome: r.session_outcome,
                parents: r.parents,
                state: r.state,
                is_base: inst.base_snapshot == Some(r.id),
                is_cloud_head: heads.contains(&r.id),
                expired: expired.contains(&r.id),
            })
            .collect())
    }

    pub(crate) fn summarize(&self, inst: InstanceId, id: SnapshotId, ctx: &OpCtx) -> SnapshotSummary {
        let row = self.db.snapshot_get(inst, id).ok().flatten();
        let m = self.manifest(inst, id, ctx).ok();
        SnapshotSummary {
            id,
            kind: row.as_ref().map(|r| r.kind).unwrap_or(SnapshotKind::Normal),
            created: row.as_ref().and_then(|r| r.created.clone()).or_else(|| m.as_ref().map(|m| m.created.clone())),
            device: row
                .as_ref()
                .and_then(|r| r.device_name.clone())
                .or_else(|| m.as_ref().map(|m| m.device.name.clone())),
            files: m.as_ref().map(|m| m.stats.files).or(row.as_ref().map(|r| r.files)).unwrap_or(0),
            bytes: m.as_ref().map(|m| m.stats.bytes).or(row.as_ref().map(|r| r.bytes)).unwrap_or(0),
            worlds: m.as_ref().map(|m| m.worlds.clone()).unwrap_or_default(),
            note: row.and_then(|r| r.note),
        }
    }

    /// Instances stored in the cloud (for "restore on this computer").
    pub fn cloud_instances(&self, ctx: &OpCtx) -> Result<Vec<CloudInstance>> {
        let remote = self.remote()?;
        let mut out = vec![];
        for inst in remote.list_instances(ctx)? {
            let view = match self.discover(inst, ctx) {
                Ok(v) => v,
                Err(Error::Storage(s)) if !s.is_retryable() => continue,
                Err(e) => return Err(e),
            };
            if view.heads.is_empty() && view.crash_saves.is_empty() {
                continue;
            }
            let heads: Vec<SnapshotSummary> = view.heads.iter().map(|h| self.summarize(inst, *h, ctx)).collect();
            let newest = heads.iter().max_by_key(|h| h.created.clone());
            let manifest = newest.and_then(|h| self.manifest(inst, h.id, ctx).ok());
            out.push(CloudInstance {
                id: inst,
                name: manifest.as_ref().map(|m| m.instance.name.clone()).unwrap_or_else(|| inst.to_string()),
                pack: manifest.as_ref().map(|m| m.pack.clone()).unwrap_or_default(),
                worlds: manifest.as_ref().map(|m| m.worlds.clone()).unwrap_or_default(),
                files: newest.map(|h| h.files).unwrap_or(0),
                bytes: newest.map(|h| h.bytes).unwrap_or(0),
                last_updated: newest.and_then(|h| h.created.clone()),
                last_device: newest.and_then(|h| h.device.clone()),
                heads,
                local_root: self.db.instance_get(inst)?.map(|r| r.root),
            });
        }
        out.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
        Ok(out)
    }
}
