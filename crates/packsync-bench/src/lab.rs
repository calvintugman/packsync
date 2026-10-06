//! A little laboratory: engines ("computers") sharing one cloud, with every operation measured.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use packsync_core::credentials::MemoryCredentialStore;
use packsync_core::engine::{Engine, EngineConfig};
use packsync_core::progress::OpCtx;
use packsync_core::redact::Secret;
use packsync_core::storage::{MemoryTransferState, Storage};
use packsync_core::storage_local::LocalDirStorage;
use packsync_core::storage_meter::{Meter, MeterSnapshot, MeteredStorage};
use packsync_drive::mock::{MockConfig, MockDrive};
use packsync_drive::oauth::{AuthConfig, Authenticator};
use packsync_drive::{DriveConfig, DriveSpace, DriveStorage, RequestStats};
use serde::Serialize;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// A directory on disk standing in for the cloud (no network model).
    Local,
    /// The in-process Google Drive mock: counts real Drive HTTP requests (no latency model).
    MockDrive,
}

impl Backend {
    pub fn parse(s: &str) -> Result<Backend, String> {
        match s {
            "local" => Ok(Backend::Local),
            "mock-drive" => Ok(Backend::MockDrive),
            other => Err(format!("unknown storage backend {other:?}: use local or mock-drive")),
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Local => "local-directory",
            Backend::MockDrive => "mock-drive",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Metrics {
    pub secs: f64,
    pub peak_rss_mb: f64,
    /// Logical storage operations as the engine issued them.
    pub storage: MeterSnapshot,
    /// Drive HTTP requests (mock-drive backend only).
    pub http: Option<RequestStats>,
}

pub struct Lab {
    pub root: PathBuf,
    pub backend: Backend,
    shared: Arc<dyn Storage>,
    mock: Option<MockDrive>,
}

pub struct Device {
    pub engine: Arc<Engine>,
    pub meter: Arc<Meter>,
    drive: Option<Arc<DriveStorage>>,
}

fn diff_stats(a: &RequestStats, b: &RequestStats) -> RequestStats {
    RequestStats {
        list: b.list - a.list,
        get: b.get - a.get,
        create: b.create - a.create,
        resumable_init: b.resumable_init - a.resumable_init,
        chunk: b.chunk - a.chunk,
        resumable_status: b.resumable_status - a.resumable_status,
        update: b.update - a.update,
        delete: b.delete - a.delete,
        token: b.token - a.token,
        other: b.other - a.other,
        bytes_up: b.bytes_up - a.bytes_up,
        bytes_down: b.bytes_down - a.bytes_down,
        retries: b.retries - a.retries,
    }
}

/// Samples this process's resident memory while an operation runs.
struct RssSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RssSampler {
    fn start() -> RssSampler {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let (s, p) = (stop.clone(), peak.clone());
        let handle = std::thread::spawn(move || {
            let pid = sysinfo::Pid::from_u32(std::process::id());
            let mut sys = System::new();
            while !s.load(Ordering::Relaxed) {
                sys.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing().with_memory(),
                );
                if let Some(proc) = sys.process(pid) {
                    p.fetch_max(proc.memory(), Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        });
        RssSampler { stop, peak, handle: Some(handle) }
    }
    fn finish(mut self) -> f64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.peak.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0)
    }
}

impl Lab {
    pub fn new(root: &Path, backend: Backend) -> Lab {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root).unwrap();
        match backend {
            Backend::Local => {
                let st = LocalDirStorage::new(root.join("cloud")).unwrap();
                Lab { root: root.to_path_buf(), backend, shared: Arc::new(st), mock: None }
            }
            Backend::MockDrive => {
                let mock = MockDrive::start(MockConfig::default()).unwrap();
                Lab {
                    root: root.to_path_buf(),
                    backend,
                    shared: Arc::new(LocalDirStorage::new(root.join("unused")).unwrap()),
                    mock: Some(mock),
                }
            }
        }
    }

    /// Bytes stored in the cloud (local backend: files on disk; Drive mock: object contents).
    pub fn cloud_bytes(&self) -> u64 {
        match (&self.mock, self.backend) {
            (Some(m), _) => m.objects().iter().map(|o| o.size).sum(),
            (None, _) => walkdir::WalkDir::new(self.root.join("cloud"))
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .filter(|e| !e.path().to_string_lossy().contains(".tags"))
                .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                .sum(),
        }
    }

    pub fn device(&self, name: &str, tweak: impl FnOnce(&mut EngineConfig)) -> Device {
        let mut cfg = EngineConfig::new(self.root.join(format!("state-{name}")));
        cfg.device_name = Some(name.to_string());
        tweak(&mut cfg);
        let engine = Engine::open(cfg).unwrap();
        let (storage, drive): (Arc<dyn Storage>, Option<Arc<DriveStorage>>) = match &self.mock {
            None => (self.shared.clone(), None),
            Some(mock) => {
                let space = DriveSpace::VisibleFolder;
                let creds = Arc::new(MemoryCredentialStore::default());
                let auth_cfg = AuthConfig::for_endpoints(&mock.endpoints());
                let auth = Arc::new(Authenticator::new(mock.client_config(), space, auth_cfg, creds));
                let rt = mock.issue_refresh_token(&[space.scope()]);
                auth.store_refresh_token(Secret::new(rt), vec![space.scope().to_string()], None).unwrap();
                let cfg = DriveConfig {
                    account: mock.config().email.clone(),
                    space,
                    endpoints: mock.endpoints(),
                    ..DriveConfig::default()
                };
                let drive = Arc::new(DriveStorage::new(cfg, auth, Arc::new(MemoryTransferState::default())).unwrap());
                (drive.clone() as Arc<dyn Storage>, Some(drive))
            }
        };
        let (metered, meter) = MeteredStorage::new(storage);
        engine.set_storage(Some(metered)).unwrap();
        Device { engine, meter, drive }
    }
}

impl Device {
    pub fn ctx(&self) -> OpCtx {
        OpCtx::detached()
    }

    /// Run `f`, returning its result with time, peak memory and request counts.
    pub fn measure<T>(&self, f: impl FnOnce() -> T) -> (T, Metrics) {
        let before = self.meter.snapshot();
        let http_before = self.drive.as_ref().map(|d| d.stats());
        let sampler = RssSampler::start();
        let t = Instant::now();
        let out = f();
        let secs = t.elapsed().as_secs_f64();
        let peak = sampler.finish();
        let after = self.meter.snapshot();
        let http = self.drive.as_ref().zip(http_before).map(|(d, b)| diff_stats(&b, &d.stats()));
        (out, Metrics { secs, peak_rss_mb: peak, storage: after.since(&before), http })
    }
}
