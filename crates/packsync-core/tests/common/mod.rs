//! Helpers shared by the integration test suites.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use packsync_core::engine::*;
use packsync_core::hash::Hash;
use packsync_core::ids::InstanceId;
use packsync_core::testkit::fixtures::{self, FixtureSpec};
use packsync_core::testkit::harness::{TestDevice, is_synced_path};

/// Digest of the synced content; `options.txt` through its portable projection (mixed file).
pub fn digest(root: &Path) -> Vec<(String, Hash)> {
    let mut v = fixtures::tree_digest(root, is_synced_path);
    let policy = packsync_core::mixed::options_txt_policy("");
    for (p, h) in v.iter_mut() {
        if p == "options.txt" {
            *h = Hash::of(&packsync_core::mixed::project(&policy, &fs::read(root.join(&*p)).unwrap()).unwrap());
        }
    }
    v
}

/// Several 256 KiB packs worth of data, quick to build.
pub fn spec(seed: u64) -> FixtureSpec {
    let mut s = FixtureSpec::tiny();
    s.seed = seed;
    s.mods = 6;
    s.mod_size = (60_000, 200_000);
    s.regions_per_world = 3;
    s
}

pub fn restore_req(d: &TestDevice, inst: InstanceId, folder: &str) -> RestoreNewRequest {
    RestoreNewRequest {
        instance: inst,
        snapshot: None,
        parent_dir: d.instances_dir(),
        folder_name: folder.into(),
        launcher: "generic".into(),
        launcher_ref: None,
        local: Default::default(),
        skip_incompatible: false,
    }
}

pub fn restore(d: &TestDevice, inst: InstanceId, folder: &str) -> PathBuf {
    d.engine.restore_new(restore_req(d, inst, folder), &d.ctx()).unwrap().root
}

pub fn write(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

/// A process that looks like the game: named `java`, working directory inside the instance.
#[cfg(unix)]
pub fn spawn_fake_game(workdir: &Path) -> std::process::Child {
    use std::os::unix::fs::PermissionsExt;
    let bin = workdir.parent().unwrap().join(format!("fakejava-{}", std::process::id()));
    fs::create_dir_all(&bin).unwrap();
    let java = bin.join("java");
    fs::write(&java, "#!/bin/sh\nsleep 60\n").unwrap();
    fs::set_permissions(&java, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(workdir).unwrap();
    let c = std::process::Command::new(&java).current_dir(workdir).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    c
}

pub use std::time::Duration;

/// Put an arbitrary (already-built) manifest into the cloud under its true content hash.
pub fn plant_manifest(
    world: &packsync_core::testkit::harness::TestWorld,
    m: &packsync_core::manifest::Manifest,
) -> packsync_core::ids::SnapshotId {
    use packsync_core::storage::*;
    let (id, stored) = m.encode().unwrap();
    let key = packsync_core::remote::commit_key(m.instance.id, id);
    world
        .shared
        .put(&key, PutSource::Bytes(&stored), &PutOptions::replace(), &packsync_core::progress::OpCtx::detached())
        .unwrap();
    id
}

/// Put raw bytes at a commit key (the name need not match the content).
pub fn plant_raw_commit(
    world: &packsync_core::testkit::harness::TestWorld,
    inst: InstanceId,
    name_hash: &str,
    bytes: &[u8],
) {
    use packsync_core::storage::*;
    let key = format!("instances/{inst}/commits/{name_hash}.snap");
    world
        .shared
        .put(&key, PutSource::Bytes(bytes), &PutOptions::replace(), &packsync_core::progress::OpCtx::detached())
        .unwrap();
}
