//! Runs the real `packsync` binary against disposable state, a local "cloud" folder and a fake
//! `prismlauncher` (shell scripts on Unix, the `packsync-fake-prism` program elsewhere).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use packsync_core::testkit::fixtures::FixtureSpec;
use packsync_prism::testkit::{FakeGame, FakePrism, make_instance};
use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_packsync");

pub struct World {
    pub dir: tempfile::TempDir,
    pub cloud: PathBuf,
}

pub struct Dev {
    pub name: String,
    pub base: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub prism: PathBuf,
    pub fake: FakePrism,
}

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}\nstderr:\n{}", self.stdout, self.stderr))
    }
    pub fn result(&self) -> Value {
        let v = self.json();
        assert_eq!(v["ok"], true, "{v}\nstderr:\n{}", self.stderr);
        v["result"].clone()
    }
    pub fn error_code(&self) -> String {
        let v = self.json();
        assert_eq!(v["ok"], false, "{v}");
        v["error"]["code"].as_str().unwrap().to_string()
    }
}

pub fn spec() -> FixtureSpec {
    let mut s = FixtureSpec::tiny();
    s.mods = 4;
    s.mod_size = (30_000, 90_000);
    s.regions_per_world = 2;
    s
}

impl World {
    pub fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        World { dir, cloud }
    }

    pub fn dev(&self, name: &str) -> Dev {
        let base = self.dir.path().join(name);
        let (config, state, prism) = (base.join("config"), base.join("state"), base.join("prism"));
        std::fs::create_dir_all(prism.join("instances")).unwrap();
        let fake = FakePrism::install(&base.join("bin"), &prism);
        let d = Dev { name: name.into(), base, config, state, prism, fake };
        for args in [
            vec!["settings", "set", "device_name", name],
            vec!["settings", "set", "prism_data_dir", d.prism.to_str().unwrap()],
            vec!["settings", "set", "prism_executable", d.fake.exe.to_str().unwrap()],
            vec!["storage", "use-local", self.cloud.to_str().unwrap()],
        ] {
            let r = d.run(&args);
            assert_eq!(r.code, 0, "{args:?}: {}", r.stderr);
        }
        d
    }
}

impl Dev {
    pub fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.args(["--config-dir", self.config.to_str().unwrap(), "--state-dir", self.state.to_str().unwrap()]);
        c.env_remove("PACKSYNC_FAILPOINTS").env("PACKSYNC_LAUNCH_POLL_MS", "40");
        hermetic(&mut c, &self.base);
        c
    }

    pub fn run(&self, args: &[&str]) -> Run {
        let o = self.cmd().args(args).output().unwrap();
        Run {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    /// `--json --yes` run that must succeed; returns `result`.
    pub fn ok(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json", "--yes"];
        a.extend_from_slice(args);
        let r = self.run(&a);
        assert_eq!(r.code, 0, "{args:?} failed ({}):\n{}\n{}", r.code, r.stdout, r.stderr);
        r.result()
    }

    pub fn with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut c = self.cmd();
        c.args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        let o = c.output().unwrap();
        Run {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    pub fn new_instance(&self, folder: &str) -> PathBuf {
        make_instance(&self.prism, folder, &spec())
    }

    /// Create an instance and start syncing it; returns (id, root).
    pub fn enabled_instance(&self, folder: &str) -> (String, PathBuf) {
        let root = self.new_instance(folder);
        let r = self.ok(&["enable", root.to_str().unwrap()]);
        (r["outcome"]["instance"].as_str().unwrap().to_string(), root)
    }

    pub fn play(&self, g: FakeGame) {
        self.fake.set(&g);
    }
}

/// Point every "home" style directory at the sandbox so that a test can never touch the real user's files.
pub fn hermetic(c: &mut Command, base: &Path) {
    let home = base.join("home");
    c.env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("PACKSYNC_STATE_DIR")
        .env_remove("PACKSYNC_CONFIG_DIR");
}

pub fn game(root: &Path, rel: &str) -> PathBuf {
    root.join(".minecraft").join(rel)
}

pub const STATS: &str = "saves/World 0/stats/p1.json";

pub fn playing(payload: &str) -> FakeGame {
    FakeGame { write: Some((STATS.into(), payload.into())), ..FakeGame::default() }
}
