//! The real binary is killed (SIGABRT from a compiled-in failpoint) at transaction boundaries; the
//! next invocation must find a consistent state and finish the work. Build with
//! `cargo test -p packsync-cli --features failpoints`.
#![cfg(all(unix, feature = "failpoints"))]

mod common;

use std::path::Path;

use common::*;
use packsync_core::hash::Hash;
use packsync_core::testkit::fixtures::tree_digest;
use packsync_core::testkit::harness::is_synced_path;

/// Digest of what is synced; `options.txt` through its portable projection (it mixes machine-local keys).
fn digest(root: &Path) -> Vec<(String, Hash)> {
    let mut v = tree_digest(root, |p| is_synced_path(p.strip_prefix(".minecraft/").unwrap_or(p)));
    let policy = packsync_core::mixed::options_txt_policy(".minecraft");
    let cfg_policy = packsync_prism::rules::instance_cfg_policy();
    for (p, h) in v.iter_mut() {
        if p.ends_with("options.txt") {
            *h = Hash::of(&packsync_core::mixed::project(&policy, &std::fs::read(root.join(&*p)).unwrap()).unwrap());
        } else if p == "instance.cfg" {
            // portable keys only: Java path, memory and the like are machine-local by design
            *h =
                Hash::of(&packsync_core::mixed::project(&cfg_policy, &std::fs::read(root.join(&*p)).unwrap()).unwrap());
        }
    }
    v
}

/// Assert two digests are equal, printing only the differing paths.
fn same(a: &[(String, Hash)], b: &[(String, Hash)], what: &str) {
    let ma: std::collections::BTreeMap<_, _> = a.iter().cloned().collect();
    let mb: std::collections::BTreeMap<_, _> = b.iter().cloned().collect();
    let mut diff = vec![];
    for k in ma.keys().chain(mb.keys()).collect::<std::collections::BTreeSet<_>>() {
        if ma.get(k) != mb.get(k) {
            diff.push(format!("{k}: {:?} vs {:?}", ma.get(k), mb.get(k)));
        }
    }
    assert!(diff.is_empty(), "{what}: trees differ:\n{}", diff.join("\n"));
}

fn cloud_manifests(w: &World) -> usize {
    walk(&w.cloud).iter().filter(|p| p.extension().is_some_and(|e| e == "snap")).count()
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
        }
    }
    out
}

#[test]
fn a_killed_upload_leaves_no_visible_snapshot_and_resumes() {
    let w = World::new();
    let a = w.dev("desktop");
    let root = a.new_instance("Pack One");
    let id =
        a.ok(&["enable", root.to_str().unwrap(), "--no-upload"])["outcome"]["instance"].as_str().unwrap().to_string();
    assert_eq!(cloud_manifests(&w), 0);
    for fp in ["upload.after_pack=abort@1", "upload.after_packs=abort", "upload.after_index=abort"] {
        let r = a.with_env(&["--json", "upload", &id], &[("PACKSYNC_FAILPOINTS", fp)]);
        assert_eq!(r.code, -1, "{fp}: the process was killed: {} {}", r.code, r.stderr);
        assert_eq!(cloud_manifests(&w), 0, "{fp}: a manifest must be the last thing published");
        let s = a.ok(&["status", &id]);
        assert!(
            matches!(s["instances"][0]["phase"].as_str(), Some("local_snapshot_complete" | "upload_pending")),
            "{fp}: {s}"
        );
    }
    // the next run finishes the job; the cloud copy is complete and restorable on another computer
    let up = a.ok(&["upload", &id]);
    assert_eq!(up["remaining"], 0);
    assert_eq!(cloud_manifests(&w), 1);
    assert_eq!(a.ok(&["verify", &id, "--remote", "--deep"])["remote"]["damaged_blobs"].as_array().unwrap().len(), 0);
    let b = w.dev("laptop");
    let broot = std::path::PathBuf::from(b.ok(&["restore-new"])["root"].as_str().unwrap());
    same(&digest(&broot), &digest(&root), "restored on the laptop vs the desktop");
}

#[test]
fn a_killed_in_place_restore_is_rolled_back_or_completed_never_mixed() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, root) = a.enabled_instance("Pack One");
    let first = a.ok(&["history", &id])[0]["id"].as_str().unwrap().to_string();
    let at_first = digest(&root);
    // change the instance (edit, delete and add files), snapshot and upload it, then restore the first
    // snapshot over it: the restore has to replace, re-create and remove files
    std::fs::write(game(&root, "config/mod0/settings-0.toml"), "edited = true\n").unwrap();
    std::fs::remove_file(game(&root, "config/mod1/settings-1.json")).unwrap();
    std::fs::write(game(&root, "config/mine.toml"), "second = true\n").unwrap();
    let second = a.ok(&["snapshot", &id])["result"]["id"].as_str().unwrap().to_string();
    a.ok(&["upload", &id]); // a snapshot can only be restored from the cloud once it is there
    let at_second = digest(&root);
    assert_ne!(at_first, at_second);
    for fp in [
        "restore.after_plan=abort",
        "restore.after_staged=abort",
        "restore.after_trash=abort@1",
        "restore.after_place=abort@2",
        "restore.after_applied=abort",
        "restore.after_db_update=abort",
    ] {
        let r = a.with_env(&["--json", "--yes", "restore", &id, &first[..12]], &[("PACKSYNC_FAILPOINTS", fp)]);
        assert_eq!(r.code, -1, "{fp}: {}", r.stderr);
        // any later invocation recovers first; the instance must be exactly one of the two versions
        let s = a.run(&["--json", "status", &id]);
        assert_eq!(s.code, 0, "{fp}: {}", s.stderr);
        let now = digest(&root);
        assert!(now == at_first || now == at_second, "{fp}: the instance is a mix of two versions");
        // and a fresh restore always converges on the target
        a.ok(&["restore", &id, &first[..12]]);
        same(&digest(&root), &at_first, &format!("{fp}: after restoring the first snapshot"));
        // go back to the second version for the next round
        a.ok(&["restore", &id, &second[..12]]);
        same(&digest(&root), &at_second, &format!("{fp}: after restoring the second snapshot"));
    }
}

#[test]
fn a_killed_new_instance_restore_leaves_no_half_instance_for_the_launcher_to_find() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, _root) = a.enabled_instance("Pack One");
    let b = w.dev("laptop");
    for fp in ["restore_new.after_marker=abort", "restore_new.after_file=abort@5", "restore_new.before_register=abort"]
    {
        let r = b.with_env(&["--json", "--yes", "restore-new"], &[("PACKSYNC_FAILPOINTS", fp)]);
        assert_eq!(r.code, -1, "{fp}: {}", r.stderr);
        // Prism lists instances by folder: hidden staging folders are ignored, and any command cleans them up
        let found = b.ok(&["instances"]);
        assert_eq!(found.as_array().unwrap().len(), 0, "{fp}: {found}");
        let _ = b.run(&["--json", "status"]);
        let leftovers: Vec<_> = std::fs::read_dir(b.prism.join("instances"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "{fp}: {leftovers:?}");
    }
    let r = b.ok(&["restore-new"]);
    assert_eq!(r["instance"], id.as_str());
}

#[test]
fn a_killed_snapshot_is_either_not_there_or_complete() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, root) = a.enabled_instance("Pack One");
    for fp in [
        "snapshot.after_walk=abort",
        "snapshot.after_hash=abort",
        "snapshot.before_db_commit=abort",
        "snapshot.after_db_commit=abort",
    ] {
        let base = a.ok(&["history", &id, "--offline"]).as_array().unwrap().len();
        std::fs::write(game(&root, "config/mine.toml"), format!("{fp}\n")).unwrap();
        let r = a.with_env(&["--json", "snapshot", &id], &[("PACKSYNC_FAILPOINTS", fp)]);
        assert_eq!(r.code, -1, "{fp}: {}", r.stderr);
        let n = a.ok(&["history", &id, "--offline"]).as_array().unwrap().len();
        assert!(n == base || n == base + 1, "{fp}: {n}");
        // finish the work and move on
        a.ok(&["snapshot", &id]);
        a.ok(&["upload", &id]);
        assert_eq!(a.ok(&["verify", &id, "--remote"])["remote"]["missing_packs"].as_array().unwrap().len(), 0, "{fp}");
        std::fs::write(game(&root, "config/mine.toml"), "reset\n").unwrap();
        a.ok(&["snapshot", &id]);
        a.ok(&["upload", &id]);
    }
}
