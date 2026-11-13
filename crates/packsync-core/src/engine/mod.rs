//! The engine: the single entry point the CLI and the desktop app use. It owns the local state
//! database and composes scan → snapshot → upload → reconcile → restore, sessions and recovery.
//!
//! Every mutating operation takes the per-instance process-safe lock; operations refuse to touch
//! an instance while a game (or another recognised writer) is using it, while a conflict or crash
//! decision is pending, or while a restore transaction needs recovery.

mod conflict;
mod gc;
mod instances;
mod lifecycle;
mod repair;
mod sync;
mod types;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::db::{Db, InstanceRow, InstanceState};
use crate::error::{Error, Result};
use crate::generic_launcher::GenericLauncher;
use crate::ids::{DeviceId, InstanceId, SnapshotId};
use crate::launcher::Launcher;
use crate::lock::InstanceLock;
use crate::manifest::Manifest;
use crate::mixed::{MixedPolicy, options_txt_policy};
use crate::progress::OpCtx;
use crate::remote::Remote;
use crate::restore::RestoreConfig;
use crate::retry::{RealSleeper, Sleeper};
use crate::rules::RuleSet;
use crate::snapshot::{SnapshotConfig, manifest_path, manifests_dir};
use crate::storage::Storage;

pub use repair::{RepairOpts, RepairReport};
pub use types::*;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Holds the database, staging area, cached manifests and locks. Per user and per machine.
    pub state_dir: PathBuf,
    pub snapshot: SnapshotConfig,
    pub restore: RestoreConfig,
    pub upload_concurrency: usize,
    pub lease_ttl_secs: u32,
    /// Name this computer is known by (persisted in the database). `None` keeps the stored/default name.
    pub device_name: Option<String>,
    /// How long a cached pack must be missing from listings before it counts as gone
    /// ([`crate::remote::PACK_VANISH_GRACE_MS`]; tests shorten it).
    pub pack_vanish_grace_ms: i64,
}

impl EngineConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> EngineConfig {
        EngineConfig {
            state_dir: state_dir.into(),
            snapshot: SnapshotConfig::default(),
            restore: RestoreConfig::default(),
            upload_concurrency: 4,
            lease_ttl_secs: 120,
            device_name: None,
            pack_vanish_grace_ms: crate::remote::PACK_VANISH_GRACE_MS,
        }
    }
}

/// How many decoded manifests [`Engine::manifest_shared`] keeps.
const DECODED_MANIFESTS: usize = 4;

pub struct Engine {
    pub cfg: EngineConfig,
    pub(crate) db: Arc<Db>,
    device: RwLock<(DeviceId, String)>,
    storage: RwLock<Option<Arc<dyn Storage>>>,
    launchers: RwLock<HashMap<String, Arc<dyn Launcher>>>,
    sleeper: Arc<dyn Sleeper>,
    /// Session start and end leave the advisory lease to a background process (launch hooks: a cloud
    /// write costs ~1 s on R2, and the person waits for the hook).
    defer_leases: std::sync::atomic::AtomicBool,
    /// Leases read ahead of a session start ([`Engine::prefetch_leases`]), with when they were read.
    manifests: parking_lot::Mutex<Vec<(SnapshotId, Arc<Manifest>)>>,
    lease_cache: parking_lot::Mutex<HashMap<InstanceId, (std::time::Instant, Vec<crate::remote::LeaseRecord>)>>,
}

impl Engine {
    pub fn open(cfg: EngineConfig) -> Result<Arc<Engine>> {
        Self::open_with_sleeper(cfg, Arc::new(RealSleeper))
    }

    /// Open with an injected sleeper (tests never really sleep).
    pub fn open_with_sleeper(cfg: EngineConfig, sleeper: Arc<dyn Sleeper>) -> Result<Arc<Engine>> {
        std::fs::create_dir_all(&cfg.state_dir)
            .map_err(|e| Error::io(format!("creating {}", cfg.state_dir.display()), e))?;
        let db = Arc::new(Db::open(&cfg.state_dir.join("state.db"))?);
        db.jobs_mark_interrupted()?;
        if let Some(n) = &cfg.device_name {
            db.set_device_name(n)?;
        }
        let device = RwLock::new(db.device()?);
        let e = Engine {
            cfg,
            db,
            device,
            storage: RwLock::new(None),
            launchers: RwLock::new(HashMap::new()),
            sleeper,
            defer_leases: std::sync::atomic::AtomicBool::new(false),
            manifests: Default::default(),
            lease_cache: Default::default(),
        };
        e.register_launcher(Arc::new(GenericLauncher));
        Ok(Arc::new(e))
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }
    pub fn device(&self) -> (DeviceId, String) {
        self.device.read().clone()
    }

    /// Rename this computer (shown as the "source device" in other devices' history).
    pub fn rename_device(&self, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 || name.chars().any(|c| c.is_control()) {
            return Err(Error::invalid("device name", "must be 1-100 characters without control characters"));
        }
        self.db.set_device_name(name)?;
        self.device.write().1 = name.to_string();
        Ok(())
    }
    pub fn state_dir(&self) -> &Path {
        &self.cfg.state_dir
    }
    pub(crate) fn locks_dir(&self) -> PathBuf {
        self.cfg.state_dir.join("locks")
    }

    pub fn register_launcher(&self, l: Arc<dyn Launcher>) {
        self.launchers.write().insert(l.kind().to_string(), l);
    }

    pub fn launcher(&self, kind: &str) -> Arc<dyn Launcher> {
        let g = self.launchers.read();
        g.get(kind)
            .or_else(|| g.get(GenericLauncher::KIND))
            .cloned()
            .expect("the generic launcher is always registered")
    }

    /// Configure (or clear) the storage provider. Caches of remote objects belong to one remote and
    /// are dropped when it changes.
    pub fn set_storage(&self, s: Option<Arc<dyn Storage>>) -> Result<()> {
        if let Some(st) = &s {
            let id = st.provider_id();
            if self.db.meta_get("cache_provider_id")?.as_deref() != Some(id.as_str()) {
                self.db.remote_cache_clear()?;
                self.db.meta_set("cache_provider_id", &id)?;
            }
        }
        *self.storage.write() = s;
        Ok(())
    }

    pub fn storage_id(&self) -> Option<String> {
        self.storage.read().as_ref().map(|s| s.provider_id())
    }

    pub fn has_storage(&self) -> bool {
        self.storage.read().is_some()
    }

    pub fn storage(&self) -> Result<Arc<dyn Storage>> {
        self.storage.read().clone().ok_or_else(|| {
            Error::Config(
                "no cloud storage is connected yet (connect Google Drive or choose a storage folder first)".into(),
            )
        })
    }

    pub(crate) fn remote(&self) -> Result<Remote> {
        Ok(Remote::new(self.storage()?, self.db.clone(), self.sleeper.clone())
            .with_vanish_grace(self.cfg.pack_vanish_grace_ms))
    }

    // ------------------------------------------------------------------------------- guards
    pub fn instance(&self, id: InstanceId) -> Result<InstanceRow> {
        self.db
            .instance_get(id)?
            .ok_or_else(|| Error::NotFound(format!("instance {id} is not registered on this computer")))
    }

    pub(crate) fn lock(&self, id: InstanceId, op: &str) -> Result<InstanceLock> {
        InstanceLock::try_acquire(&self.locks_dir(), id, op)
    }

    /// Refuse to mutate an instance that has an unresolved problem.
    pub(crate) fn require_state(&self, inst: &InstanceRow, allowed: &[InstanceState]) -> Result<()> {
        if allowed.contains(&inst.state) {
            return Ok(());
        }
        Err(match inst.state {
            InstanceState::Conflict => Error::Conflict(
                "this instance has two divergent versions; choose which to continue before syncing".into(),
            ),
            InstanceState::CrashPending => Error::CrashPending(
                inst.state_detail
                    .clone()
                    .unwrap_or_else(|| "the last session ended abnormally; resolve it before syncing".into()),
            ),
            InstanceState::Restoring => Error::Busy("a restore was interrupted; run recovery first".into()),
            InstanceState::Unavailable => Error::InstanceUnavailable(inst.root.display().to_string()),
            InstanceState::Ok => Error::other("unexpected instance state"),
        })
    }

    /// No game or recognised writer may be using the instance.
    pub(crate) fn ensure_idle(&self, inst: &InstanceRow) -> Result<()> {
        let launcher = self.launcher(&inst.launcher);
        let patterns = launcher.game_process_patterns(&inst.root);
        let game_dir = if inst.game_root.is_empty() { inst.root.clone() } else { inst.root.join(&inst.game_root) };
        let mut found: Vec<String> = crate::proc::find_writers(&inst.root, &patterns, &[])
            .into_iter()
            .map(|w| format!("{} (pid {}): {}", w.name, w.pid, w.reason))
            .collect();
        found.extend(
            crate::proc::world_locks_held(&game_dir)
                .into_iter()
                .map(|p| format!("world {} is open (session.lock held)", p.display())),
        );
        let mgr = crate::session::SessionMgr { db: &self.db };
        for s in self.db.sessions_open(inst.id)? {
            if mgr.is_alive(&s, &inst.root, &patterns) {
                found.push(format!("a play session started by {} is still running", s.owner.as_str()));
            }
        }
        if found.is_empty() { Ok(()) } else { Err(Error::GameRunning(found.join("; "))) }
    }

    pub(crate) fn policies_for(&self, inst: &InstanceRow) -> HashMap<String, MixedPolicy> {
        let mut m = HashMap::new();
        let p = options_txt_policy(&inst.game_root);
        m.insert(p.id.clone(), p);
        for p in self.launcher(&inst.launcher).mixed_policies() {
            m.insert(p.id.clone(), p);
        }
        m
    }

    /// A verified manifest, from the local cache or the cloud (then cached and recorded).
    pub fn manifest(&self, inst: InstanceId, id: SnapshotId, ctx: &OpCtx) -> Result<Manifest> {
        Ok((*self.manifest_shared(inst, id, ctx)?).clone())
    }

    /// [`Self::manifest`] without a copy. The last few stay decoded: one sync reads its base several
    /// times (rules, the save, the change check), and decoding a large modpack's costs tens of ms.
    /// A manifest never changes (its ID is its hash), so a decoded one stays valid; it is used only while
    /// the copy on disk it was decoded from is still there (that copy is what is cached).
    pub fn manifest_shared(&self, inst: InstanceId, id: SnapshotId, ctx: &OpCtx) -> Result<Arc<Manifest>> {
        let cached = self.manifests.lock().iter().find(|(i, _)| *i == id).map(|(_, m)| m.clone());
        if let Some(m) = cached
            && manifest_path(&self.cfg.state_dir, id).is_file()
        {
            return Ok(m);
        }
        let m = Arc::new(self.load_manifest(inst, id, ctx)?);
        let mut cache = self.manifests.lock();
        cache.retain(|(i, _)| *i != id);
        cache.insert(0, (id, m.clone()));
        cache.truncate(DECODED_MANIFESTS);
        Ok(m)
    }

    fn load_manifest(&self, inst: InstanceId, id: SnapshotId, ctx: &OpCtx) -> Result<Manifest> {
        let path = manifest_path(&self.cfg.state_dir, id);
        if let Ok(stored) = std::fs::read(&path) {
            if let Ok((m, _)) = Manifest::decode(&stored, Some(id)) {
                return Ok(m);
            }
            let _ = std::fs::remove_file(&path); // damaged cache copy: refetch
        }
        let remote = self.remote()?;
        let (m, stored) = remote.get_commit_with_bytes(inst, id, ctx)?;
        std::fs::create_dir_all(manifests_dir(&self.cfg.state_dir))
            .map_err(|e| Error::io("creating manifest cache", e))?;
        crate::fsutil::write_atomic(&path, &stored).map_err(|e| Error::io("caching manifest", e))?;
        self.record_remote_manifest(inst, id, &m)?;
        Ok(m)
    }

    pub(crate) fn record_remote_manifest(&self, inst: InstanceId, id: SnapshotId, m: &Manifest) -> Result<()> {
        use crate::db::{Origin, SnapshotRow, SnapshotState};
        let existing = self.db.snapshot_get(inst, id)?;
        let state = existing.as_ref().map(|r| r.state).unwrap_or(SnapshotState::Remote);
        let origin = existing.as_ref().map(|r| r.origin).unwrap_or(Origin::Remote);
        self.db.snapshot_upsert(&SnapshotRow {
            id,
            instance_id: inst,
            kind: m.kind,
            parents: m.parents.clone(),
            parents_known: true,
            device_id: Some(m.device.id),
            device_name: Some(m.device.name.clone()),
            created: Some(m.created.clone()),
            origin,
            state,
            files: m.stats.files,
            bytes: m.stats.bytes,
            note: m.note.clone(),
            session_outcome: m.session.as_ref().and_then(|s| s.outcome),
            published_ms: existing.as_ref().and_then(|r| r.published_ms),
            visible_ms: existing.as_ref().and_then(|r| r.visible_ms),
            has_manifest: true,
        })
    }

    /// The effective rules of an instance: its base snapshot's rules, or the seed rules before the
    /// first snapshot. Returns `(compiled, text)`.
    /// What restoring `target` over `inst` starts from: the mixed-file policies and the
    /// live files scanned under the target's rules — with the files this instance itself does not sync marked
    /// as never to be deleted (the target's rules may be wider). Shared by every in-place restore.
    pub(crate) fn restore_scan(
        &self,
        inst: &InstanceRow,
        target: &Manifest,
        ctx: &OpCtx,
    ) -> Result<(HashMap<String, MixedPolicy>, crate::restore::LocalState)> {
        let rules = RuleSet::compile(&target.rules.text, &inst.game_root)?;
        let policies = self.policies_for(inst);
        let own_rules = self.rules_for(inst, ctx).ok().map(|r| r.0);
        let local = crate::restore::scan_local_state(
            &self.db,
            inst,
            &rules,
            &policies,
            &self.cfg.snapshot.chunking,
            &self.cfg.snapshot.scan,
            ctx,
            own_rules.as_ref(),
        )?;
        Ok((policies, local))
    }

    /// The rules the next save uses: the last snapshot's, followed by decisions made since (or, before
    /// the first snapshot, the rules chosen when syncing was turned on).
    pub(crate) fn rules_for(&self, inst: &InstanceRow, ctx: &OpCtx) -> Result<(RuleSet, String)> {
        let text = match (inst.base_snapshot, &inst.pending_rules) {
            (Some(b), _) => self.manifest_shared(inst.id, b, ctx)?.rules.text.clone(),
            (None, Some(p)) => p.clone(),
            (None, None) => return Err(Error::Config("the instance has no rules yet".into())),
        };
        let text = crate::rules::suggest::apply_decisions(&text, &rule_decisions(inst))?;
        let rs = RuleSet::compile(&text, &inst.game_root)?;
        Ok((rs, text))
    }

    /// The rules the last snapshot carried, without decisions made since: what the other computers follow.
    pub(crate) fn snapshot_rules(&self, inst: &InstanceRow, ctx: &OpCtx) -> Result<Option<RuleSet>> {
        inst.base_snapshot
            .map(|b| RuleSet::compile(&self.manifest_shared(inst.id, b, ctx)?.rules.text, &inst.game_root))
            .transpose()
    }

    pub(crate) fn guard_provider(&self, inst: &InstanceRow) -> Result<()> {
        if let (Some(have), Some(want)) = (self.storage_id(), inst.provider_id.as_ref())
            && &have != want
        {
            return Err(Error::Config(format!(
                "this instance was synced with a different storage ({want}); the connected storage is {have}. Reconnect the original storage or remove the instance's sync registration and enable it again"
            )));
        }
        Ok(())
    }
}

/// The person's answers about unknown content not yet stored in a snapshot.
pub(crate) fn rule_decisions(inst: &InstanceRow) -> Vec<crate::rules::suggest::RuleDecision> {
    inst.rule_decisions.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default()
}
