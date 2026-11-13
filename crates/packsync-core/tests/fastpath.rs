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
    assert!(
        d.requests <= 6,
        "one pack, its index, the manifest, the HEAD swap, and alongside them one read of HEAD (two on storage without a combined read) to see whether a cleanup ran: {d:?}"
    );
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
    assert_eq!((up.remaining, d.lists), (0, 0), "{up:?} {d:?}");
    assert!(d.requests <= 6, "{d:?}");
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

/// Found by review: HEAD names the index uploaded with its snapshot, and a supported cleanup may merge
/// that index into another one and delete it. Catching up must then fall back to the listing.
#[test]
fn catching_up_survives_a_cleanup_that_merged_the_named_index() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let b = w.device("laptop");
    restore_on(&b, en.instance);
    b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    fs::write(&info.configs[0], b"new_save=1\n").unwrap();
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();

    // make every index old enough for the cleanup to consolidate them, and run it
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 3600);
    for entry in fs::read_dir(w.remote_dir.join("index")).unwrap() {
        filetime::set_file_mtime(entry.unwrap().path(), filetime::FileTime::from_system_time(old)).unwrap();
    }
    let params = packsync_core::gc::GcParams::default();
    let plan = a.engine.gc_plan(&params, &a.ctx()).unwrap();
    assert!(plan.consolidate.is_some(), "the test needs a consolidation: {plan:?}");
    let cleaned = a.engine.gc_run(&plan.id, &params, &a.ctx()).unwrap();
    assert!(cleaned.deleted_indexes >= 2, "{cleaned:?}");

    let (_, rec) = b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::FastForwarded { .. }), "{rec:?}");
}

/// Found by review: catching up reads only the index the cloud's HEAD names. When the new version
/// reuses data first uploaded with another instance (the same mod jar in two packs) after this computer
/// last looked, that index does not list it, and catching up failed on every attempt with "not listed in
/// any pack index".
#[test]
fn catching_up_finds_data_first_uploaded_with_another_instance() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (_rx, info_x, x) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(x.instance);
    a.engine.sync_now(x.instance, &a.ctx()).unwrap();
    let mut spec_y = FixtureSpec::tiny();
    spec_y.seed += 1000;
    let (_ry, info_y, y) = a.new_instance("other-pack", &spec_y);
    a.upload(y.instance);
    a.engine.sync_now(y.instance, &a.ctx()).unwrap();
    let b = w.device("laptop");
    let rb = restore_on(&b, x.instance);
    b.engine.sync_now(x.instance, &b.ctx()).unwrap();

    // a jar arrives in instance Y first (its data is uploaded with Y)…
    let shared: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    fs::write(info_y.mods[0].with_file_name("shared-lib.jar"), &shared).unwrap();
    a.engine.sync_now(y.instance, &a.ctx()).unwrap();
    // …then the same jar in X, with something new so X's version has an index of its own
    let target = info_x.mods[0].with_file_name("shared-lib.jar");
    fs::write(&target, &shared).unwrap();
    fs::write(&info_x.configs[0], b"brand_new = 1\n").unwrap();
    a.engine.sync_now(x.instance, &a.ctx()).unwrap();

    let (_, rec) = b.engine.sync_now(x.instance, &b.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::FastForwarded { .. }), "{rec:?}");
    let rel = target.strip_prefix(&info_x.root).unwrap();
    assert_eq!(fs::read(rb.root.join(rel)).unwrap(), shared);
}

/// Found by review: a cleanup on one computer deleted a pack that the other computer's cache still
/// listed. That computer then put old content back (a mod downgrade), its save reused the deleted pack,
/// and the fast path published a version with missing data and reported it uploaded. A cleanup now
/// records itself in HEAD before deleting: the other computer forgets its cache and checks everything.
#[test]
fn a_cleanup_elsewhere_never_lets_the_fast_path_publish_missing_data() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (root_a, _info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let v = |n: u32| -> Vec<u8> { (0..300_000u32).map(|x| (x.wrapping_mul(2654435761 + n) >> 11) as u8).collect() };
    for i in 1..=4 {
        fs::write(root_a.join("mods").join("big.jar"), v(i)).unwrap();
        a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    }
    let b = w.device("laptop");
    let rb = restore_on(&b, en.instance);
    b.engine.sync_now(en.instance, &b.ctx()).unwrap();

    // the desktop cleans up, keeping only the newest version: older versions of big.jar are deleted
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 3600);
    for d in ["index", "packs"] {
        for entry in fs::read_dir(w.remote_dir.join(d)).unwrap() {
            filetime::set_file_mtime(entry.unwrap().path(), filetime::FileTime::from_system_time(old)).unwrap();
        }
    }
    let retention = packsync_core::gc::RetentionPolicy { keep_snapshots: 1, keep_days: 0, keep_crash_days: 0 };
    let params = packsync_core::gc::GcParams { retention: Some(retention), ..Default::default() };
    let plan = a.engine.gc_plan(&params, &a.ctx()).unwrap();
    let rep = a.engine.gc_run(&plan.id, &params, &a.ctx()).unwrap();
    assert!(rep.deleted_packs > 0, "the test needs a deleted pack: {rep:?}");

    // the laptop puts version 1 back and syncs: whatever path it takes, the cloud must hold all the data
    fs::write(rb.root.join("mods").join("big.jar"), v(1)).unwrap();
    let (_, rec) = b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");
    let ver = b.engine.verify_remote(en.instance, None, false, &b.ctx()).unwrap();
    assert!(ver.missing_packs.is_empty() && ver.unlisted_blobs.is_empty(), "{ver:?}");
    let (_, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::FastForwarded { .. }), "{rec:?}");
    assert_eq!(fs::read(root_a.join("mods").join("big.jar")).unwrap(), v(1));
}

/// Found by review: HEAD was moved onto a quarantined crash save. After the crash was discarded, HEAD
/// kept naming it, every later sync took the full path and appended itself to HEAD's forks. HEAD now
/// never names a crash save, and a full look that finds a single version re-anchors it there.
#[test]
fn head_never_names_a_crash_save_and_recovers_when_it_did() {
    let w = TestWorld::with_atomic_cas();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let good = head_snapshot(&w, en.instance);

    let s = a
        .engine
        .begin_session(
            en.instance,
            BeginSession { owner: packsync_core::db::SessionOwner::App, token: "p".into(), offline: false },
            &a.ctx(),
        )
        .unwrap();
    fs::write(&info.configs[0], b"crashy = true\n").unwrap();
    let end = a
        .engine
        .end_session(s.session, ExitInfo { exit_code: Some(137), clean_signal: false }, true, &a.ctx())
        .unwrap();
    let crash = end.snapshot.expect("a crash save").to_hex();
    assert_eq!(head_snapshot(&w, en.instance), good, "HEAD stays on the last good version");

    // an older version did move HEAD there: write that state, then discard the crash and sync
    let path = w.remote_dir.join(format!("instances/{}/HEAD.json", en.instance));
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    v["snapshot"] = crash.clone().into();
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    a.engine.discard_crash(en.instance, &a.ctx()).unwrap();
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    assert_ne!(head_snapshot(&w, en.instance), crash, "re-anchored");
    assert!(a.engine.cloud_unchanged(en.instance, &a.ctx()).unwrap(), "and trusted again");

    fs::write(&info.configs[1], b"after = 1\n").unwrap();
    let before = a.meter.snapshot();
    let (_, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let d = a.meter.snapshot().since(&before);
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");
    assert_eq!(d.lists, 0, "the next save takes the fast path again: {d:?}");
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(v.get("forks").is_none(), "{v}");
}
