//! The HEAD pointer fast path (storage with atomic compare-and-swap, i.e. R2): a computer whose last
//! known HEAD is still the cloud's needs one read when nothing changed and no listing to publish, and
//! every disagreement sends it down the full path without losing a version.

use std::fs;

use packsync_core::engine::*;
use packsync_core::testkit::fixtures::{self, FixtureSpec};
use packsync_core::testkit::harness::{TestDevice, TestWorld, is_synced_path};

/// Synced content, with `options.txt` (a mixed file) compared through its portable projection.
fn tree(root: &std::path::Path) -> Vec<(String, packsync_core::hash::Hash)> {
    let mut v = fixtures::tree_digest(root, is_synced_path);
    let policy = packsync_core::mixed::options_txt_policy("");
    for (p, h) in v.iter_mut() {
        if p == "options.txt" {
            *h = packsync_core::hash::Hash::of(
                &packsync_core::mixed::project(&policy, &fs::read(root.join(&*p)).unwrap()).unwrap(),
            );
        }
    }
    v
}

fn head_snapshot(w: &TestWorld, inst: packsync_core::ids::InstanceId) -> String {
    let p = w.remote_dir.join(format!("instances/{inst}/HEAD.json"));
    let v: serde_json::Value = serde_json::from_slice(&fs::read(p).expect("HEAD.json exists")).unwrap();
    v["snapshot"].as_str().unwrap().to_string()
}

fn restore_on(b: &TestDevice, inst: packsync_core::ids::InstanceId) -> RestoredInstance {
    b.engine
        .restore_new(
            RestoreNewRequest {
                instance: inst,
                snapshot: None,
                parent_dir: b.instances_dir(),
                folder_name: "pack".into(),
                launcher: "generic".into(),
                launcher_ref: None,
                local: Default::default(),
                skip_incompatible: false,
            },
            &b.ctx(),
        )
        .unwrap()
}

#[test]
fn an_unchanged_cloud_costs_one_read_and_a_publication_needs_no_listing() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    // the first full sync creates the HEAD pointer
    let (_, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::UpToDate { .. }), "{rec:?}");

    let before = a.meter.snapshot();
    let (_, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let d = a.meter.snapshot().since(&before);
    assert!(matches!(rec, ReconcileReport::UpToDate { .. }), "{rec:?}");
    assert_eq!(d.lists, 0, "nothing changed: no listing ({d:?})");
    assert!(d.requests <= 2, "nothing changed: one read of HEAD (two on storage without a combined read): {d:?}");

    fs::write(&info.configs[0], b"changed = true\n").unwrap();
    fixtures::edit_region(&info.regions[0], 2, 7);
    let before = a.meter.snapshot();
    let (snap, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let d = a.meter.snapshot().since(&before);
    let SnapshotResult::Created { id } = snap.unwrap().result else { panic!("a snapshot was expected") };
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");
    assert_eq!(d.lists, 0, "a publication on the fast path lists nothing: {d:?}");
    assert_eq!((d.stats, d.gets), (0, 0), "…and reads nothing: HEAD is swapped from the version seen last: {d:?}");
    assert_eq!(d.requests, 4, "one pack, its index, the manifest and the HEAD swap: {d:?}");
    assert_eq!(head_snapshot(&w, en.instance), id.to_hex());
}

#[test]
fn a_computer_whose_head_is_stale_takes_the_full_path_and_catches_up() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (root_a, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let b = w.device("laptop");
    let rb = restore_on(&b, en.instance);
    b.engine.sync_now(en.instance, &b.ctx()).unwrap(); // B now knows HEAD too

    fs::write(&info.configs[1], b"from the desktop\n").unwrap();
    let (_, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");

    // B: its HEAD is stale, so "nothing changed here" is not "up to date": it catches up, reading the
    // one index HEAD names instead of listing every pack and index
    let before = b.meter.snapshot();
    let (_, rec) = b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::FastForwarded { .. }), "{rec:?}");
    let d = b.meter.snapshot().since(&before);
    let lists = |prefix: &str| d.by_prefix.get(prefix).map_or(0, |p| p.lists);
    assert_eq!((lists("packs"), lists("index")), (0, 0), "{d:?}");
    assert_eq!(tree(&root_a), tree(&rb.root));
    // and is back on the fast path afterwards
    let before = b.meter.snapshot();
    let (_, rec) = b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::UpToDate { .. }), "{rec:?}");
    assert_eq!(b.meter.snapshot().since(&before).lists, 0);
}

#[test]
fn two_computers_publishing_from_the_same_head_end_in_a_conflict_not_a_lost_version() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (_root_a, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let b = w.device("laptop");
    let rb = restore_on(&b, en.instance);
    b.engine.sync_now(en.instance, &b.ctx()).unwrap();

    fs::write(&info.configs[0], b"desktop = 1\n").unwrap();
    let (snap_a, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");
    let SnapshotResult::Created { id: a1 } = snap_a.unwrap().result else { panic!() };

    // B changed something too, from the same HEAD: its swap fails and the full path reports the conflict
    let rel = info.configs[1].strip_prefix(&info.root).unwrap();
    fs::write(rb.root.join(rel), b"laptop = 1\n").unwrap();
    let (snap_b, rec) = b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    let SnapshotResult::Created { id: b1 } = snap_b.unwrap().result else { panic!() };
    assert!(matches!(rec, ReconcileReport::Conflict(_)), "{rec:?}");
    assert_eq!(head_snapshot(&w, en.instance), a1.to_hex(), "HEAD stays with the first publication");
    // both versions are in the cloud
    let commits = w.remote_dir.join(format!("instances/{}/commits", en.instance));
    let names: Vec<String> =
        fs::read_dir(commits).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    for id in [a1, b1] {
        assert!(names.iter().any(|n| n.starts_with(&id.to_hex())), "{id:?} missing from {names:?}");
    }
}

/// What Prism's hooks do: before launch, "is the cloud where I left it?" in one read; after exit, the
/// saved snapshot is uploaded on the fast path (the post-exit hook runs `packsync upload` in the
/// background); a computer whose HEAD is stale gets `false` and takes the full path.
#[test]
fn the_launch_hooks_check_the_cloud_in_one_read_and_upload_on_the_fast_path() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance); // the first upload also creates the HEAD pointer
    assert!(w.remote_dir.join(format!("instances/{}/HEAD.json", en.instance)).exists());
    // HEAD is only trusted after a full look at the cloud (the first sync or launch after enabling)
    assert!(!a.engine.cloud_unchanged(en.instance, &a.ctx()).unwrap());
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();

    let before = a.meter.snapshot();
    assert!(a.engine.cloud_unchanged(en.instance, &a.ctx()).unwrap());
    let d = a.meter.snapshot().since(&before);
    assert_eq!(d.lists, 0, "{d:?}");
    assert!(d.requests <= 2, "{d:?}");

    // the game exits: a snapshot, then the upload
    fs::write(&info.configs[0], b"played = true\n").unwrap();
    let SnapshotResult::Created { id } =
        a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap().result
    else {
        panic!("a snapshot was expected")
    };
    let before = a.meter.snapshot();
    let up = a.engine.upload_pending(en.instance, false, &a.ctx()).unwrap();
    let d = a.meter.snapshot().since(&before);
    assert_eq!((up.remaining, d.lists, d.stats), (0, 0, 0), "{up:?} {d:?}");
    assert_eq!(head_snapshot(&w, en.instance), id.to_hex());

    // another computer that last saw an older HEAD is told "unknown", not "unchanged"
    let b = w.device("laptop");
    restore_on(&b, en.instance);
    fs::write(&info.configs[1], b"later = true\n").unwrap();
    a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    a.engine.upload_pending(en.instance, false, &a.ctx()).unwrap();
    assert!(!b.engine.cloud_unchanged(en.instance, &b.ctx()).unwrap());
}

/// Found by review: the computer that won the race to move HEAD kept trusting HEAD and reported
/// "up to date" although the other computer had published a competing version. The loser now records
/// its version in HEAD, and nobody trusts a HEAD with a recorded fork.
#[test]
fn the_computer_that_won_the_race_also_sees_the_conflict() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let b = w.device("laptop");
    let rb = restore_on(&b, en.instance);
    b.engine.sync_now(en.instance, &b.ctx()).unwrap();

    fs::write(&info.configs[0], b"desktop=1\n").unwrap();
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let rel = info.configs[1].strip_prefix(&info.root).unwrap();
    fs::write(rb.root.join(rel), b"laptop=1\n").unwrap();
    let (_, loser) = b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    assert!(matches!(loser, ReconcileReport::Conflict(_)), "{loser:?}");
    let (_, winner) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    assert!(matches!(winner, ReconcileReport::Conflict(_)), "the winner must see both versions: {winner:?}");
    assert!(!a.engine.cloud_unchanged(en.instance, &a.ctx()).unwrap(), "nor may a launch trust HEAD");

    // resolving it publishes a version that descends from both: HEAD is trusted again
    a.engine.resolve_conflict(en.instance, Resolution::KeepLocal, &a.ctx()).unwrap();
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&fs::read(w.remote_dir.join(format!("instances/{}/HEAD.json", en.instance))).unwrap())
            .unwrap();
    assert!(v.get("forks").is_none(), "the fork is resolved: {v}");
    assert!(a.engine.cloud_unchanged(en.instance, &a.ctx()).unwrap());
}

/// If the computer that lost the race crashed before it could record itself in HEAD, the winner still
/// finds the other version: HEAD alone is trusted for at most 30 minutes after a full look at the cloud.
#[test]
fn an_unrecorded_competing_version_is_found_by_the_periodic_full_look() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let b = w.device("laptop");
    let rb = restore_on(&b, en.instance);
    b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    fs::write(&info.configs[0], b"desktop=1\n").unwrap();
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    // the laptop publishes a competing version but never touches HEAD (as if it crashed right after)
    let rel = info.configs[1].strip_prefix(&info.root).unwrap();
    fs::write(rb.root.join(rel), b"laptop=1\n").unwrap();
    let head = w.remote_dir.join(format!("instances/{}/HEAD.json", en.instance));
    let head_before = fs::read(&head).unwrap();
    b.engine.snapshot(en.instance, SnapshotOpts::default(), &b.ctx()).unwrap();
    b.engine.upload_pending(en.instance, true, &b.ctx()).unwrap();
    // …and HEAD as if the laptop had died before recording its version there
    fs::write(&head, &head_before).unwrap();

    // within the trust window the desktop cannot know
    assert!(a.engine.cloud_unchanged(en.instance, &a.ctx()).unwrap());
    // 31 minutes later it looks at the listing again and finds the other version
    let old = packsync_core::db::now_ms() - 31 * 60 * 1000;
    a.engine.db().meta_set(&format!("last_discovery_ms:{}", en.instance), &old.to_string()).unwrap();
    let (_, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::Conflict(_)), "{rec:?}");
}
