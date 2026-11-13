//! Multi-device test harness: several simulated computers (each with its own state directory,
//! database, instance folders and fault plan) sharing ONE fake remote directory — exactly the
//! shape of "two PCs and one Google Drive".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;

use crate::engine::{EnableOutcome, EnableRequest, Engine, EngineConfig};
use crate::ids::InstanceId;
use crate::progress::OpCtx;
use crate::retry::RecordingSleeper;
use crate::storage::Storage;
use crate::storage_fault::{FaultPlan, FaultyStorage};
use crate::storage_local::LocalDirStorage;
use crate::storage_meter::{Meter, MeteredStorage};
use crate::testkit::fixtures::{self, FixtureInfo, FixtureSpec};

pub struct TestWorld {
    pub tmp: TempDir,
    pub remote_dir: PathBuf,
    pub shared: Arc<LocalDirStorage>,
    /// Present the shared folder as storage with atomic compare-and-swap (like R2), so the HEAD
    /// pointer fast path is used. In one test process the folder's per-key lock makes that true.
    pub atomic_cas: bool,
}

/// Wraps a storage so it behaves like Cloudflare R2 or the PackSync cloud: atomic compare-and-swap
/// ([`crate::storage::Capabilities::conditional_write`]), a namespace that is PackSync's alone
/// ([`crate::storage::Capabilities::owned_namespace`]), and no object tags anywhere (S3 listings carry
/// none, the PackSync cloud stores none).
pub struct AtomicCas(pub Arc<dyn Storage>);

impl Storage for AtomicCas {
    fn provider_id(&self) -> String {
        self.0.provider_id()
    }
    fn capabilities(&self) -> crate::storage::Capabilities {
        crate::storage::Capabilities { conditional_write: true, owned_namespace: true, ..self.0.capabilities() }
    }
    fn list(&self, prefix: &str, ctx: &OpCtx) -> crate::storage::StorageResult<Vec<crate::storage::ObjectInfo>> {
        Ok(self.0.list(prefix, ctx)?.into_iter().map(untagged).collect())
    }
    fn stat(&self, key: &str, ctx: &OpCtx) -> crate::storage::StorageResult<Option<crate::storage::ObjectInfo>> {
        Ok(self.0.stat(key, ctx)?.map(untagged))
    }
    fn get(
        &self,
        key: &str,
        range: Option<(u64, u64)>,
        ctx: &OpCtx,
    ) -> crate::storage::StorageResult<Box<dyn std::io::Read + Send>> {
        self.0.get(key, range, ctx)
    }
    fn put(
        &self,
        key: &str,
        src: crate::storage::PutSource<'_>,
        opts: &crate::storage::PutOptions,
        ctx: &OpCtx,
    ) -> crate::storage::StorageResult<crate::storage::ObjectInfo> {
        self.0.put(key, src, opts, ctx)
    }
    fn delete(&self, key: &str, mode: crate::storage::DeleteMode, ctx: &OpCtx) -> crate::storage::StorageResult<()> {
        self.0.delete(key, mode, ctx)
    }
    fn quota(&self, ctx: &OpCtx) -> crate::storage::StorageResult<Option<crate::storage::Quota>> {
        self.0.quota(ctx)
    }
}

fn untagged(mut i: crate::storage::ObjectInfo) -> crate::storage::ObjectInfo {
    i.tags.clear();
    i
}

impl TestWorld {
    pub fn new() -> TestWorld {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("cloud");
        let shared = Arc::new(LocalDirStorage::new(&remote_dir).unwrap());
        TestWorld { tmp, remote_dir, shared, atomic_cas: false }
    }

    /// Like [`TestWorld::new`], with a cloud that has atomic compare-and-swap (the R2 fast path).
    pub fn with_atomic_cas() -> TestWorld {
        TestWorld { atomic_cas: true, ..Self::new() }
    }

    /// A new computer connected to the shared cloud.
    pub fn device(&self, name: &str) -> TestDevice {
        self.device_with(name, |_| {})
    }

    /// A new computer with a customised engine configuration.
    pub fn device_with(&self, name: &str, tweak: impl FnOnce(&mut EngineConfig)) -> TestDevice {
        let home = self.tmp.path().join(format!("device-{name}"));
        std::fs::create_dir_all(&home).unwrap();
        TestDevice::build(name, home, self.shared.clone(), self.atomic_cas, tweak)
    }
}

impl Default for TestWorld {
    fn default() -> Self {
        Self::new()
    }
}

pub struct TestDevice {
    pub name: String,
    pub home: PathBuf,
    pub engine: Arc<Engine>,
    pub plan: Arc<FaultPlan>,
    pub meter: Arc<Meter>,
    pub sleeper: Arc<RecordingSleeper>,
    shared: Arc<LocalDirStorage>,
    atomic_cas: bool,
}

impl TestDevice {
    fn build(
        name: &str,
        home: PathBuf,
        shared: Arc<LocalDirStorage>,
        atomic_cas: bool,
        tweak: impl FnOnce(&mut EngineConfig),
    ) -> TestDevice {
        let sleeper = Arc::new(RecordingSleeper::default());
        let mut cfg = EngineConfig::new(home.join("state"));
        cfg.device_name = Some(name.to_string());
        // small packs so tests exercise multi-pack uploads and rollover
        cfg.snapshot.pack_target = 256 * 1024;
        cfg.snapshot.scan.threads = 2;
        cfg.restore.concurrency = 2;
        cfg.upload_concurrency = 2;
        tweak(&mut cfg);
        let engine = Engine::open_with_sleeper(cfg, sleeper.clone()).unwrap();
        let plan = FaultPlan::new(0xC0FFEE);
        let (metered, meter) = MeteredStorage::new(Self::cloud(&shared, atomic_cas));
        let faulty = FaultyStorage::new(metered, plan.clone());
        engine.set_storage(Some(faulty)).unwrap();
        TestDevice { name: name.to_string(), home, engine, plan, meter, sleeper, shared, atomic_cas }
    }

    fn cloud(shared: &Arc<LocalDirStorage>, atomic_cas: bool) -> Arc<dyn Storage> {
        let s = shared.clone() as Arc<dyn Storage>;
        if atomic_cas { Arc::new(AtomicCas(s)) } else { s }
    }

    /// Simulate quitting and restarting the app: a new `Engine` over the same state directory.
    pub fn restart(&mut self) {
        let cfg = self.engine.cfg.clone();
        let sleeper = self.sleeper.clone();
        drop(std::mem::replace(&mut self.engine, Engine::open_with_sleeper(cfg, sleeper).unwrap()));
        let (metered, meter) = MeteredStorage::new(Self::cloud(&self.shared, self.atomic_cas));
        self.meter = meter;
        self.engine.set_storage(Some(FaultyStorage::new(metered, self.plan.clone()))).unwrap();
    }

    pub fn instances_dir(&self) -> PathBuf {
        let d = self.home.join("instances");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn ctx(&self) -> OpCtx {
        OpCtx::detached()
    }

    /// Generate a fixture instance and register it for syncing (the "first computer" journey).
    pub fn new_instance(&self, folder: &str, spec: &FixtureSpec) -> (PathBuf, FixtureInfo, EnableOutcome) {
        let root = self.instances_dir().join(folder);
        let info = fixtures::generate(&root, spec);
        let out = self.enable(&root);
        (root, info, out)
    }

    pub fn enable(&self, root: &Path) -> EnableOutcome {
        self.engine
            .enable_sync(
                EnableRequest {
                    root: root.to_path_buf(),
                    launcher: "generic".into(),
                    name: None,
                    rules_overrides: String::new(),
                    decisions: vec![],
                    launcher_ref: None,
                },
                &self.ctx(),
            )
            .unwrap()
    }

    pub fn upload(&self, inst: InstanceId) -> crate::engine::UploadSummary {
        self.engine.upload_pending(inst, true, &self.ctx()).unwrap()
    }
}

/// The paths the default rules sync, for comparing two devices' trees (`generic` layout).
pub fn is_synced_path(rel: &str) -> bool {
    const EXCLUDE: &[&str] =
        &["logs/", "crash-reports/", "screenshots/", "shaderpacks/", "backups/", ".fabric/", "natives/", ".packsync/"];
    if EXCLUDE.iter().any(|e| rel.starts_with(e)) {
        return false;
    }
    if rel.ends_with("session.lock") || rel.ends_with(".dll") || rel.ends_with(".so") {
        return false;
    }
    true
}
