//! A little world for app-level tests: a shared "cloud" folder, and devices that each have their own
//! app state, Prism data directory and fake launcher.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use packsync_app::{App, AppOptions, AppPaths, LaunchOptions, LaunchTiming};
use packsync_core::credentials::MemoryCredentialStore;
use packsync_core::ids::InstanceId;
use packsync_core::progress::OpCtx;
use packsync_core::testkit::fixtures::FixtureSpec;
use packsync_prism::PrismLauncher;
use packsync_prism::testkit::{FakeGame, FakePrism, make_instance};

pub struct World {
    pub dir: tempfile::TempDir,
    pub cloud: PathBuf,
}

pub struct Device {
    pub name: String,
    pub app: Arc<App>,
    pub data_dir: PathBuf,
    pub fake: FakePrism,
    pub base: PathBuf,
}

pub fn spec() -> FixtureSpec {
    let mut s = FixtureSpec::tiny();
    s.mods = 4;
    s.mod_size = (30_000, 90_000);
    s.regions_per_world = 2;
    s
}

pub fn fast_timing() -> LaunchTiming {
    LaunchTiming {
        poll: Duration::from_millis(40),
        start_timeout: Duration::from_secs(20),
        exit_grace: Duration::from_secs(5),
        signal_wait: Duration::from_millis(800),
    }
}

pub fn launch_opts() -> LaunchOptions {
    LaunchOptions { timing: fast_timing(), ..Default::default() }
}

impl World {
    pub fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        World { dir, cloud }
    }

    /// A computer with its own app state and Prism data directory, connected to the shared cloud.
    pub fn device(&self, name: &str) -> Device {
        self.device_with(name, |p| p)
    }

    pub fn device_with(&self, name: &str, tweak: impl FnOnce(PrismLauncher) -> PrismLauncher) -> Device {
        let base = self.dir.path().join(name);
        let data_dir = base.join("prism");
        std::fs::create_dir_all(data_dir.join("instances")).unwrap();
        let fake = FakePrism::install(&base.join("bin"), &data_dir);
        let prism =
            tweak(PrismLauncher::new().with_data_dirs(vec![data_dir.clone()]).with_executable(fake.exe.clone()));
        let app = App::open(
            AppPaths::under(&base.join("app")),
            AppOptions {
                credentials: Some(Arc::new(MemoryCredentialStore::default())),
                prism: Some(prism),
                ..Default::default()
            },
        )
        .unwrap();
        app.update_settings(|s| s.device_name = Some(name.to_string())).unwrap();
        app.use_local_storage(&self.cloud).unwrap();
        Device { name: name.into(), app, data_dir, fake, base }
    }
}

impl Device {
    pub fn ctx(&self) -> OpCtx {
        OpCtx::detached()
    }

    /// Create a fixture instance in this device's Prism and start syncing it.
    pub fn new_synced_instance(&self, folder: &str) -> (InstanceId, PathBuf) {
        let root = make_instance(&self.data_dir, folder, &spec());
        let out = self.app.enable_sync("prism", &root, String::new(), &self.ctx()).unwrap();
        self.app.engine().upload_pending(out.instance, false, &self.ctx()).unwrap();
        (out.instance, root)
    }

    pub fn game_file(&self, root: &Path, rel: &str) -> PathBuf {
        root.join(".minecraft").join(rel)
    }

    pub fn play(&self, g: FakeGame) {
        self.fake.set(&g);
    }
}

pub const STATS: &str = "saves/World 0/stats/p1.json";

pub fn playing(payload: &str) -> FakeGame {
    FakeGame { write: Some((STATS.into(), payload.into())), ..FakeGame::default() }
}

/// Pause so that file mtimes differ between edits on coarse-timestamp filesystems.
pub fn tick() {
    std::thread::sleep(Duration::from_millis(30));
}
