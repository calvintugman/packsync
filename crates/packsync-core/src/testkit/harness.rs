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
}

impl TestWorld {
    pub fn new() -> TestWorld {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("cloud");
        let shared = Arc::new(LocalDirStorage::new(&remote_dir).unwrap());
        TestWorld { tmp, remote_dir, shared }
    }

    /// A new computer connected to the shared cloud.
    pub fn device(&self, name: &str) -> TestDevice {
        let home = self.tmp.path().join(format!("device-{name}"));
        std::fs::create_dir_all(&home).unwrap();
        TestDevice::build(name, home, self.shared.clone())
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
}

impl TestDevice {
    fn build(name: &str, home: PathBuf, shared: Arc<LocalDirStorage>) -> TestDevice {
        let sleeper = Arc::new(RecordingSleeper::default());
        let mut cfg = EngineConfig::new(home.join("state"));
        cfg.device_name = Some(name.to_string());
        // small packs so tests exercise multi-pack uploads and rollover
        cfg.snapshot.pack_target = 256 * 1024;
        cfg.snapshot.scan.threads = 2;
        cfg.restore.concurrency = 2;
        cfg.upload_concurrency = 2;
        let engine = Engine::open_with_sleeper(cfg, sleeper.clone()).unwrap();
        let plan = FaultPlan::new(0xC0FFEE);
        let (metered, meter) = MeteredStorage::new(shared.clone() as Arc<dyn Storage>);
        let faulty = FaultyStorage::new(metered, plan.clone());
        engine.set_storage(Some(faulty)).unwrap();
        TestDevice { name: name.to_string(), home, engine, plan, meter, sleeper, shared }
    }

    /// Simulate quitting and restarting the app: a new `Engine` over the same state directory.
    pub fn restart(&mut self) {
        let cfg = self.engine.cfg.clone();
        let sleeper = self.sleeper.clone();
        drop(std::mem::replace(&mut self.engine, Engine::open_with_sleeper(cfg, sleeper).unwrap()));
        let (metered, meter) = MeteredStorage::new(self.shared.clone() as Arc<dyn Storage>);
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
