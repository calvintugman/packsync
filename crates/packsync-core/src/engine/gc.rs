//! Cloud cleanup (see `crate::gc` for the model). Manual: a plan is computed and shown first; running
//! it requires the plan's id, and the plan is recomputed against the live cloud right before anything
//! is deleted, so a stale preview can never act.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::*;
use crate::db::SnapshotState;
use crate::gc::*;
use crate::hash::Hash;
use crate::manifest::{Recipe, parse_rfc3339};
use crate::pack::IndexDoc;
use crate::progress::{Level, Phase};
use crate::remote::index_key;
use crate::storage::ObjectInfo;

/// Everything a plan decided, including the objects `gc_run` has to write.
struct Prepared {
    plan: GcPlan,
    expire_records: Vec<ExpiredRecord>,
    merged_index: Option<IndexDoc>,
}

/// Blobs that must stay: everything the snapshots we keep (or may still upload) refer to.
struct Live {
    blobs: HashSet<Hash>,
    recipes: HashMap<Hash, Vec<Hash>>,
}

impl Engine {
    /// Age of the youngest timestamp the provider reports: an object that was touched recently counts as young.
    fn age_hours(info: &ObjectInfo, now_ms: i64) -> Option<i64> {
        let t = match (info.created_ms, info.modified_ms) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return None,
        };
        Some(((now_ms - t).max(0)) / 3_600_000)
    }

    fn is_ours(info: &ObjectInfo) -> bool {
        info.tags.get("ps").map(|v| v == "1").unwrap_or(false)
    }

    fn mark(&self, remote: &Remote, m: &Manifest, live: &mut Live, ctx: &OpCtx) -> Result<()> {
        for e in m.entries.iter().filter(|e| e.is_file()) {
            match (e.r, e.h) {
                (Some(r), _) => {
                    live.blobs.insert(r);
                    if !live.recipes.contains_key(&r) {
                        let bytes = remote.read_blob(r, ctx).map_err(|err| {
                            Error::Corrupt(format!(
                                "cleanup refused: the recipe of {} (needed by a snapshot that is kept) cannot be read: {err}. Run `packsync verify --remote` first",
                                e.p
                            ))
                        })?;
                        let recipe = Recipe::decode(&bytes, e.s)?;
                        live.recipes.insert(r, recipe.chunks.iter().map(|(h, _)| *h).collect());
                    }
                    live.blobs.extend(live.recipes[&r].iter().copied());
                }
                (None, Some(h)) => {
                    live.blobs.insert(h);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn gc_prepare(&self, params: &GcParams, ctx: &OpCtx) -> Result<Prepared> {
        let remote = self.remote()?;
        let now = crate::db::now_ms();
        let grace_ms = params.effective_grace_ms();
        let mut notes = vec![];
        ctx.phase(Phase::Verify, "reading the cloud's snapshots");
        remote.sync_index_cache(ctx)?;

        // ---- snapshots, retention and the set of blobs that must stay
        let mut live = Live { blobs: HashSet::new(), recipes: HashMap::new() };
        let mut expire: Vec<ExpireItem> = vec![];
        let mut expire_records: Vec<ExpiredRecord> = vec![];
        let (dev_id, dev_name) = self.device();
        let instances = remote.list_instances(ctx)?;
        for (i, inst) in instances.iter().enumerate() {
            ctx.check()?;
            ctx.progress(Phase::Verify, (i as u64, instances.len() as u64), (0, 0), format!("instance {inst}"));
            let listing = remote.list_commits(*inst, ctx)?;
            let already: HashSet<SnapshotId> = remote.list_expired(*inst, ctx)?;
            let mut manifests: Vec<(SnapshotId, Manifest)> = vec![];
            for c in &listing {
                let m = self.manifest(*inst, c.id, ctx).map_err(|e| {
                    Error::Corrupt(format!(
                        "cleanup refused: snapshot {} of instance {inst} cannot be read ({e}); nothing is deleted while the history is incomplete",
                        c.id.short()
                    ))
                })?;
                manifests.push((c.id, m));
            }
            let mut newly: HashSet<SnapshotId> = HashSet::new();
            if let (Some(policy), true) = (&params.retention, params.instance.is_none_or(|p| p == *inst)) {
                let infos: Vec<CommitInfo> = manifests
                    .iter()
                    .map(|(id, m)| CommitInfo {
                        id: *id,
                        parents: m.parents.clone(),
                        kind: m.kind,
                        created_ms: parse_rfc3339(&m.created).map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64),
                    })
                    .collect();
                for id in expirable(&infos, policy, now) {
                    if !already.contains(&id) {
                        newly.insert(id);
                        let m = &manifests.iter().find(|(i, _)| *i == id).expect("listed").1;
                        expire.push(ExpireItem { instance: *inst, snapshot: id, created: Some(m.created.clone()) });
                        expire_records.push(ExpiredRecord {
                            format: EXPIRED_FORMAT.into(),
                            version: 1,
                            instance: *inst,
                            snapshot: id,
                            expired_at: crate::manifest::now_rfc3339(),
                            device: dev_name.clone(),
                        });
                    }
                }
            }
            for (id, m) in &manifests {
                if already.contains(id) || newly.contains(id) {
                    continue;
                }
                self.mark(&remote, m, &mut live, ctx)?;
            }
            // snapshots this computer made and has not published yet may reuse blobs that are in the cloud
            if self.db.instance_get(*inst)?.is_some() {
                for s in self.db.snapshots(*inst)? {
                    if matches!(s.state, SnapshotState::LocalComplete | SnapshotState::Uploading)
                        && let Ok(m) = self.manifest(*inst, s.id, ctx)
                    {
                        self.mark(&remote, &m, &mut live, ctx)?;
                    }
                }
            }
        }
        let _ = dev_id;
        expire.sort_by_key(|e| (e.instance, e.snapshot));

        // ---- packs
        ctx.phase(Phase::Verify, "looking for objects nothing needs");
        let packs = remote.list_packs(ctx)?;
        let mut kept_unknown: Vec<String> = remote.list_unrecognised("packs/", ".pack", ctx)?;
        let mut delete_packs: Vec<GcObject> = vec![];
        let mut partial = PartialSummary::default();
        let mut deleted_pack_ids: HashSet<Hash> = HashSet::new();
        let mut young = 0u64;
        for (id, info) in &packs {
            let blobs = self.db.pack_blobs(*id)?;
            let (mut live_n, mut dead_bytes) = (0u64, 0u64);
            for (h, len) in &blobs {
                if live.blobs.contains(h) {
                    live_n += 1;
                } else {
                    dead_bytes += *len as u64;
                }
            }
            if live_n > 0 {
                if dead_bytes > 0 {
                    partial.packs += 1;
                    partial.reclaimable_bytes += dead_bytes;
                }
                continue;
            }
            if !Self::is_ours(info) {
                kept_unknown.push(format!("{} (not created by PackSync: left alone)", info.key));
                continue;
            }
            let Some(age) = Self::age_hours(info, now) else {
                kept_unknown.push(format!("{} (age unknown: left alone)", info.key));
                continue;
            };
            if age * 3_600_000 < grace_ms {
                young += 1;
                continue;
            }
            deleted_pack_ids.insert(*id);
            delete_packs.push(GcObject {
                key: info.key.clone(),
                kind: "pack".into(),
                size: info.size,
                age_hours: age,
                reason: if blobs.is_empty() {
                    "no index describes it (abandoned upload)".into()
                } else {
                    "no kept snapshot needs any of its content".into()
                },
            });
        }
        if young > 0 {
            notes.push(format!("{young} unused object(s) are younger than the {}-hour grace period and were left alone (they may belong to an upload in progress).", grace_ms / 3_600_000));
        }

        // ---- indexes
        let infos = remote.list_index_infos(ctx)?;
        kept_unknown.extend(remote.list_unrecognised("index/", ".idx", ctx)?);
        let mut delete_indexes: Vec<GcObject> = vec![];
        let mut eligible: Vec<(Hash, ObjectInfo, IndexDoc)> = vec![];
        for (id, info) in infos {
            ctx.check()?;
            if !Self::is_ours(&info) {
                kept_unknown.push(format!("{} (not created by PackSync: left alone)", info.key));
                continue;
            }
            let age = Self::age_hours(&info, now);
            if age.is_none_or(|a| a * 3_600_000 < grace_ms) {
                continue;
            }
            let doc = match remote.get_index(id, ctx) {
                Ok(d) => d,
                Err(Error::Corrupt(_) | Error::Invalid { .. } | Error::Unsupported(_)) => {
                    kept_unknown.push(format!("{} (cannot be read: left alone)", info.key));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let needed = doc.packs.iter().any(|p| packs.contains_key(&p.id) && !deleted_pack_ids.contains(&p.id));
            if !needed {
                delete_indexes.push(GcObject {
                    key: info.key.clone(),
                    kind: "index".into(),
                    size: info.size,
                    age_hours: age.unwrap_or(0),
                    reason: "describes only packs that are gone or being removed".into(),
                });
            } else {
                eligible.push((id, info, doc));
            }
        }

        // ---- consolidation of the indexes that stay
        let mut consolidate = None;
        let mut merged_index = None;
        if params.consolidate_indexes && eligible.len() >= 2 {
            let mut by_pack: BTreeMap<Hash, crate::pack::IndexPack> = BTreeMap::new();
            let mut supersedes: Vec<Hash> = vec![];
            for (id, _, doc) in &eligible {
                supersedes.push(*id);
                supersedes.extend(doc.supersedes.iter().copied());
                for p in &doc.packs {
                    if packs.get(&p.id).is_some_and(|i| i.size == p.size) && !deleted_pack_ids.contains(&p.id) {
                        by_pack.entry(p.id).or_insert_with(|| p.clone());
                    }
                }
            }
            let blobs: u64 = by_pack.values().map(|p| p.blobs.len() as u64).sum();
            const MAX_CONSOLIDATED_BLOBS: u64 = 400_000;
            if blobs > MAX_CONSOLIDATED_BLOBS {
                notes.push(format!("{} index objects were not merged: together they describe {blobs} blobs, more than one merged object may hold.", eligible.len()));
            } else {
                supersedes.sort();
                supersedes.dedup();
                let mut doc = IndexDoc::new(by_pack.into_values().collect());
                doc.supersedes = supersedes;
                let (new_id, _) = doc.encode()?;
                let merge: Vec<String> = eligible.iter().map(|(id, _, _)| index_key(*id)).collect();
                // already merged? (a previous cleanup wrote exactly this object)
                if !(merge.len() == 1 && merge[0] == index_key(new_id)) {
                    consolidate = Some(Consolidation { merge: merge.clone(), packs: doc.packs.len() as u64, blobs });
                    for (id, info, _) in &eligible {
                        if *id == new_id {
                            continue;
                        }
                        delete_indexes.push(GcObject {
                            key: info.key.clone(),
                            kind: "index".into(),
                            size: info.size,
                            age_hours: Self::age_hours(info, now).unwrap_or(0),
                            reason: format!("merged into index {}", new_id.short()),
                        });
                    }
                    notes.push(format!(
                        "{} index objects will be merged into one ({}).",
                        eligible.len(),
                        new_id.short()
                    ));
                    merged_index = Some(doc);
                }
            }
        }

        // ---- the plan
        let mut delete: Vec<GcObject> = delete_indexes;
        delete.sort_by(|a, b| a.key.cmp(&b.key));
        delete_packs.sort_by(|a, b| a.key.cmp(&b.key));
        delete.extend(delete_packs);
        kept_unknown.sort();
        kept_unknown.dedup();
        let total: u64 = delete.iter().map(|d| d.size).sum();
        let provider = remote.provider_id();
        let id = plan_id(&provider, &expire, &delete, &consolidate);
        Ok(Prepared {
            plan: GcPlan {
                id,
                params: params.clone(),
                generated_ms: now,
                provider,
                expire,
                delete,
                consolidate,
                partially_live: partial,
                kept_unknown,
                total_delete_bytes: total,
                notes,
            },
            expire_records,
            merged_index,
        })
    }

    /// What a cleanup would do. Reads the cloud, changes nothing.
    pub fn gc_plan(&self, params: &GcParams, ctx: &OpCtx) -> Result<GcPlan> {
        Ok(self.gc_prepare(params, ctx)?.plan)
    }

    /// Carry out the plan with id `plan_id`. The plan is recomputed from the live cloud first; if it
    /// no longer matches the one that was previewed, nothing happens.
    pub fn gc_run(&self, plan_id: &str, params: &GcParams, ctx: &OpCtx) -> Result<GcReport> {
        let remote = self.remote()?;
        let prepared = self.gc_prepare(params, ctx)?;
        if prepared.plan.id != plan_id {
            return Err(Error::Conflict(format!(
                "the cloud changed since the preview (plan {plan_id} is now {}); preview again before cleaning up",
                prepared.plan.id
            )));
        }
        let mut rep = GcReport::default();
        // 1. mark released snapshots (idempotent markers)
        for r in &prepared.expire_records {
            ctx.check()?;
            remote.put_expired(r, ctx)?;
            rep.expired += 1;
        }
        // 2. merged index: written and read back before any old index disappears
        if let Some(doc) = &prepared.merged_index {
            ctx.phase(Phase::Cleanup, "merging index objects");
            let new_id = remote.put_index(doc, ctx)?;
            let back = remote.get_index(new_id, ctx)?;
            if back.packs.len() != doc.packs.len() {
                return Err(Error::Corrupt("the merged index did not read back intact; nothing was deleted".into()));
            }
            rep.consolidated_into = Some(index_key(new_id));
        }
        // 3. deletions: indexes first, then packs (an index pointing at a missing pack is harmless, the reverse is not)
        let total = prepared.plan.delete.len() as u64;
        for (i, o) in prepared.plan.delete.iter().enumerate() {
            ctx.check()?;
            ctx.progress(Phase::Cleanup, (i as u64, total), (0, 0), format!("removing {}", o.key));
            // the object must still be the one the plan looked at
            match self.storage()?.stat(&o.key, ctx).map_err(Error::from)? {
                Some(info) if info.size == o.size && Self::is_ours(&info) => {}
                Some(_) => {
                    rep.notes.push(format!("{} changed since the plan; left alone", o.key));
                    continue;
                }
                None => continue,
            }
            remote.delete_object(&o.key, ctx)?;
            rep.bytes_freed += o.size;
            if o.kind == "pack" {
                rep.deleted_packs += 1;
            } else {
                rep.deleted_indexes += 1;
            }
        }
        // the local view of the cloud is rebuilt on the next sync
        self.db.remote_cache_clear()?;
        ctx.note(
            Phase::Cleanup,
            Level::Info,
            format!("cleanup finished: {} object(s) removed", rep.deleted_packs + rep.deleted_indexes),
        );
        self.db.event(
            None,
            "info",
            "gc_run",
            &format!(
                "removed {} packs, {} indexes, released {} snapshots",
                rep.deleted_packs, rep.deleted_indexes, rep.expired
            ),
        );
        Ok(rep)
    }
}
