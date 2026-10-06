//! Shared helpers for the integration tests: a mock Drive, an `Authenticator` holding a refresh
//! token issued by the mock (so every test exercises the real refresh path), and a
//! `DriveStorage` with a `RecordingSleeper` (no real backoff sleeping).
#![allow(dead_code)]

use std::sync::Arc;

use packsync_core::credentials::MemoryCredentialStore;
use packsync_core::redact::Secret;
use packsync_core::retry::{RecordingSleeper, RetryPolicy};
use packsync_core::storage::MemoryTransferState;
use packsync_drive::mock::{MockConfig, MockDrive};
use packsync_drive::oauth::{AuthConfig, Authenticator};
use packsync_drive::{DriveConfig, DriveSpace, DriveStorage, HttpTimeouts, StatsHandle};

pub struct Env {
    pub mock: MockDrive,
    pub auth: Arc<Authenticator>,
    pub creds: Arc<MemoryCredentialStore>,
    pub state: Arc<MemoryTransferState>,
    pub sleeper: Arc<RecordingSleeper>,
    pub stats: StatsHandle,
    pub cfg: DriveConfig,
    pub drive: DriveStorage,
}

pub fn auth_config(mock: &MockDrive, stats: &StatsHandle) -> AuthConfig {
    AuthConfig {
        timeouts: HttpTimeouts::for_tests(),
        stats: stats.clone(),
        ..AuthConfig::for_endpoints(&mock.endpoints())
    }
}

pub fn authenticator(
    mock: &MockDrive,
    space: DriveSpace,
    stats: &StatsHandle,
) -> (Arc<Authenticator>, Arc<MemoryCredentialStore>) {
    let creds = Arc::new(MemoryCredentialStore::default());
    let auth = Arc::new(Authenticator::new(mock.client_config(), space, auth_config(mock, stats), creds.clone()));
    let rt = mock.issue_refresh_token(&[space.scope()]);
    auth.store_refresh_token(Secret::new(rt), vec![space.scope().to_string()], None).unwrap();
    (auth, creds)
}

pub fn drive_config(mock: &MockDrive, space: DriveSpace, stats: &StatsHandle) -> DriveConfig {
    DriveConfig {
        account: mock.config().email.clone(),
        space,
        endpoints: mock.endpoints(),
        timeouts: HttpTimeouts::for_tests(),
        chunk_size: 1024 * 1024,
        retry: RetryPolicy::network(),
        stats: stats.clone(),
        ..DriveConfig::default()
    }
}

pub fn setup_with(mock_cfg: MockConfig, space: DriveSpace, tweak: impl FnOnce(&mut DriveConfig)) -> Env {
    let mock = MockDrive::start(mock_cfg).unwrap();
    let stats = StatsHandle::new();
    let (auth, creds) = authenticator(&mock, space, &stats);
    let mut cfg = drive_config(&mock, space, &stats);
    tweak(&mut cfg);
    let state = Arc::new(MemoryTransferState::default());
    let sleeper = Arc::new(RecordingSleeper::default());
    let drive = DriveStorage::with_sleeper(cfg.clone(), auth.clone(), state.clone(), sleeper.clone()).unwrap();
    Env { mock, auth, creds, state, sleeper, stats, cfg, drive }
}

pub fn setup() -> Env {
    setup_with(MockConfig::default(), DriveSpace::VisibleFolder, |_| {})
}

impl Env {
    /// A second, independent adapter instance sharing the transfer state and the authenticator
    /// (models a restarted process).
    pub fn new_instance(&self, tweak: impl FnOnce(&mut DriveConfig)) -> DriveStorage {
        let mut cfg = self.cfg.clone();
        tweak(&mut cfg);
        DriveStorage::with_sleeper(cfg, self.auth.clone(), self.state.clone(), self.sleeper.clone()).unwrap()
    }

    pub fn sleeps(&self) -> Vec<std::time::Duration> {
        self.sleeper.slept.lock().clone()
    }
}

pub fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut v = vec![0u8; len];
    rng.fill(&mut v);
    v
}

pub fn digest(b: &[u8]) -> packsync_core::storage::ContentDigest {
    packsync_core::storage::ContentDigest { size: b.len() as u64, blake3: packsync_core::Hash::of(b) }
}

pub fn read_all(r: Box<dyn std::io::Read + Send>) -> std::io::Result<Vec<u8>> {
    let mut v = Vec::new();
    let mut r = r;
    r.read_to_end(&mut v)?;
    Ok(v)
}
