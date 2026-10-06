//! Cloud cleanup: retention, mark-and-sweep, grace period, never touching what PackSync did not
//! create, plan-id protection, index consolidation. Manual operations only.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use packsync_core::Error;
use packsync_core::engine::*;
use packsync_core::failpoint::{self, Action};
use packsync_core::gc::{GcParams, RetentionPolicy};
use packsync_core::hash::Hash;
use packsync_core::ids::{InstanceId, SnapshotId};
use packsync_core::storage::{PutOptions, PutSource, Storage};
use packsync_core::testkit::fixtures::{self, FixtureSpec};
use packsync_core::testkit::harness::{TestDevice, TestWorld, is_synced_path};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    failpoint::disarm_all();
    g
}

fn spec() -> FixtureSpec {
    let mut s = FixtureSpec::tiny();
    s.mods = 5;
    s.mod_size = (60_000, 150_000);
    s.regions_per_world = 3;
    s
}

fn tree(root: &Path) -> Vec<(String, Hash)> {
    let mut v = fixtures::tree_digest(root, is_synced_path);
    let policy = packsync_core::mixed::options_txt_policy("");
    for (p, h) in v.iter_mut() {
        if p == "options.txt" {
            *h = Hash::of(&packsync_core::mixed::project(&policy, &fs::read(root.join(&*p)).unwrap()).unwrap());
        }
    }
    v
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(files_under(&p)) } else { out.push(p) }
        }
    }
    out.sort();
    out
}

/// Make every object under `packs/` and `index/` look `hours` old.
fn age_everything(w: &TestWorld, hours: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(hours * 3600);
    for sub in ["packs", "index"] {
        for f in files_under(&w.remote_dir.join(sub)) {
            filetime::set_file_mtime(&f, filetime::FileTime::from_system_time(t)).unwrap();
        }
    }
}

fn plant(w: &TestWorld, prefix: &str, name_hex: Option<Hash>, ext: &str, bytes: &[u8], tagged: bool) -> String {
    let key = match name_hex {
        Some(h) => format!("{prefix}{}.{ext}", h.to_hex()),
        None => format!("{prefix}{ext}"),
    };
    let mut opts = PutOptions::create_only(None);
    if tagged {
        opts = opts.with_tag("ps", "1").with_tag("kind", "pack");
    }
    w.shared.put(&key, PutSource::Bytes(bytes), &opts, &packsync_core::progress::OpCtx::detached()).unwrap();
    key
}

fn exists(w: &TestWorld, key: &str) -> bool {
    w.remote_dir.join(key).exists()
}

fn session(a: &TestDevice, root: &Path, inst: InstanceId, n: u32) -> SnapshotId {
    fs::write(
        root.join("config/session.toml"),
        format!("session = {n}\npayload = \"{}\"\n", "x".repeat(300 + n as usize)),
    )
    .unwrap();
    let rep = a.engine.snapshot(inst, SnapshotOpts::default(), &a.ctx()).unwrap();
    let SnapshotResult::Created { id } = rep.result else { panic!("{rep:?}") };
    a.upload(inst);
    id
}

#[test]
fn only_old_unreferenced_objects_made_by_packsync_are_removed() {
    let _g = serial();
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &spec());
    a.upload(en.instance);
    let before = tree(&root);
    age_everything(&w, 100);

    let junk = b"not a real pack, left behind by an abandoned upload".to_vec();
    let old_orphan = plant(&w, "packs/", Some(Hash::of(&junk)), "pack", &junk, true);
    let young_junk = b"an upload that is still in progress".to_vec();
    let young = plant(&w, "packs/", Some(Hash::of(&young_junk)), "pack", &young_junk, true);
    let untagged_bytes = b"somebody else's file with a pack-like name".to_vec();
    let untagged = plant(&w, "packs/", Some(Hash::of(&untagged_bytes)), "pack", &untagged_bytes, false);
    let stray = plant(&w, "packs/", None, "README.txt", b"hello", false);
    let stray_index = plant(&w, "index/", None, "notes.txt", b"mine", false);
    // the young one really is young; the others are old
    for k in [&old_orphan, &untagged, &stray, &stray_index] {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 3600);
        filetime::set_file_mtime(w.remote_dir.join(k), filetime::FileTime::from_system_time(t)).unwrap();
    }

    let plan = a.engine.gc_plan(&GcParams::default(), &a.ctx()).unwrap();
    let keys: Vec<_> = plan.delete.iter().map(|d| d.key.clone()).collect();
    assert_eq!(keys, vec![old_orphan.clone()], "{plan:#?}");
    assert!(plan.kept_unknown.iter().any(|k| k.contains(&untagged)), "{:?}", plan.kept_unknown);
    assert!(plan.kept_unknown.iter().any(|k| k.contains("README.txt")));
    assert!(plan.kept_unknown.iter().any(|k| k.contains("notes.txt")));
    assert!(plan.notes.iter().any(|n| n.contains("grace")), "{:?}", plan.notes);
    // a preview deletes nothing
    assert!(exists(&w, &old_orphan));

    let rep = a.engine.gc_run(&plan.id, &GcParams::default(), &a.ctx()).unwrap();
    assert_eq!((rep.deleted_packs, rep.bytes_freed), (1, junk.len() as u64), "{rep:?}");
    assert!(!exists(&w, &old_orphan));
    for k in [&young, &untagged, &stray, &stray_index] {
        assert!(exists(&w, k), "{k} must survive");
    }
    // everything that belongs to the instance is intact: a new computer restores identical bytes
    let b = w.device("laptop");
    let rb = b
        .engine
        .restore_new(
            RestoreNewRequest {
                instance: en.instance,
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
        .unwrap();
    assert_eq!(tree(&rb.root), before);
    // nothing left to do now
    assert!(a.engine.gc_plan(&GcParams::default(), &a.ctx()).unwrap().delete.is_empty());
}

#[test]
fn a_stale_preview_cannot_be_run() {
    let _g = serial();
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, _info, en) = a.new_instance("pack", &spec());
    a.upload(en.instance);
    age_everything(&w, 100);
    let junk = b"orphan one".to_vec();
    let one = plant(&w, "packs/", Some(Hash::of(&junk)), "pack", &junk, true);
    filetime::set_file_mtime(w.remote_dir.join(&one), filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
    let plan = a.engine.gc_plan(&GcParams::default(), &a.ctx()).unwrap();
    assert_eq!(plan.delete.len(), 1);
    // the cloud changes between preview and run
    let junk2 = b"orphan two appeared meanwhile".to_vec();
    let two = plant(&w, "packs/", Some(Hash::of(&junk2)), "pack", &junk2, true);
    filetime::set_file_mtime(w.remote_dir.join(&two), filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
    let err = a.engine.gc_run(&plan.id, &GcParams::default(), &a.ctx()).unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err}");
    assert!(exists(&w, &one) && exists(&w, &two), "nothing was deleted");
    // a made-up id is refused too
    assert!(a.engine.gc_run("0000000000000000", &GcParams::default(), &a.ctx()).is_err());
}

#[test]
fn an_upload_that_was_interrupted_keeps_its_packs_until_it_is_finished_or_abandoned() {
    let _g = serial();
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &spec());
    a.upload(en.instance);
    // a new snapshot whose upload dies after the packs and the index are in the cloud but before the manifest
    fs::write(root.join("config/progress.toml"), b"level = 99\n").unwrap();
    a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    failpoint::arm("upload.after_index", Action::Error, Some(1));
    let r = a.engine.upload_pending(en.instance, true, &a.ctx());
    assert!(matches!(r, Err(Error::InjectedCrash(_))), "{r:?}");
    failpoint::disarm_all();
    // even if everything looks old, the local unpublished snapshot still needs those blobs
    age_everything(&w, 100);
    let plan = a.engine.gc_plan(&GcParams::default(), &a.ctx()).unwrap();
    let listing: Vec<String> = plan.delete.iter().map(|d| format!("{} {} ({})", d.kind, d.key, d.reason)).collect();
    assert!(
        plan.delete.iter().all(|d| d.kind == "index"),
        "packs needed by an unpublished local snapshot are kept: {listing:?}"
    );
    // finishing the upload still works
    let up = a.engine.upload_pending(en.instance, true, &a.ctx()).unwrap();
    assert_eq!(up.remaining, 0);
}

#[test]
fn retention_releases_old_snapshots_but_never_heads_and_a_dormant_computer_can_still_catch_up() {
    let _g = serial();
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &spec());
    a.upload(en.instance);
    // a computer that restored the first snapshot and then was left in a drawer
    let dormant = w.device("dormant");
    let rd = dormant
        .engine
        .restore_new(
            RestoreNewRequest {
                instance: en.instance,
                snapshot: None,
                parent_dir: dormant.instances_dir(),
                folder_name: "pack".into(),
                launcher: "generic".into(),
                launcher_ref: None,
                local: Default::default(),
                skip_incompatible: false,
            },
            &dormant.ctx(),
        )
        .unwrap();
    let ids: Vec<SnapshotId> = (1..=6).map(|n| session(&a, &root, en.instance, n)).collect();
    let head = *ids.last().unwrap();
    let at_head = tree(&root);
    age_everything(&w, 100);

    let params = GcParams {
        retention: Some(RetentionPolicy { keep_snapshots: 2, keep_days: 0, keep_crash_days: 90 }),
        ..Default::default()
    };
    let plan = a.engine.gc_plan(&params, &a.ctx()).unwrap();
    let expired: Vec<SnapshotId> = plan.expire.iter().map(|e| e.snapshot).collect();
    assert_eq!(expired.len(), 5, "the first snapshot and sessions 1-4: {plan:#?}");
    assert!(!expired.contains(&head) && !expired.contains(&ids[4]), "the newest two are kept");
    assert!(
        plan.delete.iter().filter(|d| d.kind == "pack").count() >= 4,
        "the packs only expired snapshots needed: {plan:#?}"
    );
    let rep = a.engine.gc_run(&plan.id, &params, &a.ctx()).unwrap();
    assert_eq!(rep.expired, 5);
    assert!(rep.deleted_packs >= 4 && rep.bytes_freed > 0, "{rep:?}");

    // history says which snapshots are gone; the graph itself is untouched
    let hist = a.engine.history(en.instance, true, &a.ctx()).unwrap();
    assert_eq!(hist.len(), 7);
    assert_eq!(hist.iter().filter(|h| h.expired).count(), 5);
    assert!(hist.iter().find(|h| h.id == head).is_some_and(|h| !h.expired && h.is_cloud_head));
    // an expired snapshot cannot be restored, with a clear reason; the head can
    let err = a.engine.restore_preview(en.instance, expired[0], &a.ctx()).unwrap_err();
    assert!(matches!(&err, Error::Unsupported(m) if m.contains("released")), "{err}");
    let err = a
        .engine
        .restore_snapshot(en.instance, expired[0], RestoreOpts { skip_incompatible: false }, &a.ctx())
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)));
    assert!(a.engine.restore_preview(en.instance, head, &a.ctx()).is_ok());
    // everything the head needs is still in the cloud, byte for byte
    let v = a.engine.verify_remote(en.instance, Some(head), true, &a.ctx()).unwrap();
    assert!(v.missing_packs.is_empty() && v.damaged_blobs.is_empty(), "{v:?}");
    // a brand-new computer restores the head
    let b = w.device("laptop");
    let rb = b
        .engine
        .restore_new(
            RestoreNewRequest {
                instance: en.instance,
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
        .unwrap();
    assert_eq!(tree(&rb.root), at_head);
    // the dormant computer's base was released, yet ancestry still resolves: it simply fast-forwards
    let rec = dormant.engine.reconcile(en.instance, ReconcileOpts::default(), &dormant.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::FastForwarded { to, .. } if to == head), "{rec:?}");
    assert_eq!(tree(&rd.root), at_head);
    // running the same cleanup again has nothing left to do
    let again = a.engine.gc_plan(&params, &a.ctx()).unwrap();
    assert!(again.expire.is_empty() && again.delete.is_empty(), "{again:#?}");
}

#[test]
fn every_branch_head_and_crash_save_keeps_its_data() {
    let _g = serial();
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root_a, _info, en) = a.new_instance("pack", &spec());
    a.upload(en.instance);
    for n in 1..=3 {
        session(&a, &root_a, en.instance, n);
    }
    // the laptop restores, and both computers play on independently: two heads
    let b = w.device("laptop");
    let rb = b
        .engine
        .restore_new(
            RestoreNewRequest {
                instance: en.instance,
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
        .unwrap();
    let head_b = session(&b, &rb.root, en.instance, 100);
    let head_a = session(&a, &root_a, en.instance, 200);
    let want_a = tree(&root_a);
    let want_b = tree(&rb.root);
    assert_ne!(want_a, want_b);
    // …and a session on the desktop that crashed: its save is kept apart from the history, but it is kept
    let sess = a
        .engine
        .begin_session(
            en.instance,
            BeginSession { owner: packsync_core::db::SessionOwner::App, token: "t".into(), offline: false },
            &a.ctx(),
        )
        .unwrap();
    fs::write(root_a.join("config/crashy.toml"), b"half-written world state\n").unwrap();
    fs::create_dir_all(root_a.join("crash-reports")).unwrap();
    fs::write(root_a.join("crash-reports/crash-2026-01-01_00.00.00-client.txt"), b"java.lang.OutOfMemoryError")
        .unwrap();
    let end = a
        .engine
        .end_session(sess.session, ExitInfo { exit_code: Some(1), clean_signal: false }, true, &a.ctx())
        .unwrap();
    let crash_id = end.snapshot.expect("the crash save was made");
    assert_eq!(end.kind, Some(packsync_core::manifest::SnapshotKind::Crash));
    age_everything(&w, 100);

    let params = GcParams {
        retention: Some(RetentionPolicy { keep_snapshots: 0, keep_days: 0, keep_crash_days: 90 }),
        ..Default::default()
    };
    let plan = a.engine.gc_plan(&params, &a.ctx()).unwrap();
    let expired: Vec<SnapshotId> = plan.expire.iter().map(|e| e.snapshot).collect();
    assert!(!expired.contains(&head_a) && !expired.contains(&head_b), "both branch heads are kept");
    assert!(!expired.contains(&crash_id), "a recent crash save is kept");
    a.engine.gc_run(&plan.id, &params, &a.ctx()).unwrap();
    let v = a.engine.verify_remote(en.instance, Some(crash_id), true, &a.ctx()).unwrap();
    assert!(
        v.missing_packs.is_empty() && v.damaged_blobs.is_empty() && v.unlisted_blobs.is_empty(),
        "crash save: {v:?}"
    );
    for (head, want) in [(head_a, &want_a), (head_b, &want_b)] {
        let v = a.engine.verify_remote(en.instance, Some(head), true, &a.ctx()).unwrap();
        assert!(v.missing_packs.is_empty() && v.damaged_blobs.is_empty() && v.unlisted_blobs.is_empty(), "{v:?}");
        let c = w.device(&format!("check-{}", head.short()));
        let r = c
            .engine
            .restore_new(
                RestoreNewRequest {
                    instance: en.instance,
                    snapshot: Some(head),
                    parent_dir: c.instances_dir(),
                    folder_name: "pack".into(),
                    launcher: "generic".into(),
                    launcher_ref: None,
                    local: Default::default(),
                    skip_incompatible: false,
                },
                &c.ctx(),
            )
            .unwrap();
        assert_eq!(&tree(&r.root), want);
    }
}

#[test]
fn many_small_index_objects_are_merged_and_everyone_keeps_working() {
    let _g = serial();
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &spec());
    a.upload(en.instance);
    for n in 1..=6 {
        session(&a, &root, en.instance, n);
    }
    let count_indexes = || files_under(&w.remote_dir.join("index")).len();
    assert!(count_indexes() >= 7);
    age_everything(&w, 100);
    let plan = a.engine.gc_plan(&GcParams::default(), &a.ctx()).unwrap();
    let c = plan.consolidate.clone().expect("indexes are merged");
    assert!(c.merge.len() >= 7, "{c:?}");
    assert!(plan.delete.iter().all(|d| d.kind == "index"), "no pack is unreferenced: {plan:#?}");
    let rep = a.engine.gc_run(&plan.id, &GcParams::default(), &a.ctx()).unwrap();
    assert!(rep.consolidated_into.is_some() && rep.deleted_indexes as usize >= 7, "{rep:?}");
    assert_eq!(count_indexes(), 1, "one merged index remains");
    // the computer that did the cleanup and a brand-new one both work from the merged index
    assert!(matches!(
        a.engine.reconcile(en.instance, ReconcileOpts::default(), &a.ctx()).unwrap(),
        ReconcileReport::UpToDate { .. }
    ));
    let b = w.device("laptop");
    let rb = b
        .engine
        .restore_new(
            RestoreNewRequest {
                instance: en.instance,
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
        .unwrap();
    assert_eq!(tree(&rb.root), tree(&root));
    // a new upload after the merge adds a (small) index again and everything still verifies
    session(&a, &root, en.instance, 7);
    let v = a.engine.verify_remote(en.instance, None, true, &a.ctx()).unwrap();
    assert!(v.missing_packs.is_empty() && v.damaged_blobs.is_empty(), "{v:?}");
    // merging is idempotent: a second cleanup right after finds nothing new to merge beyond the one index + the new one
    let again = a.engine.gc_plan(&GcParams::default(), &a.ctx()).unwrap();
    assert!(again.delete.iter().all(|d| d.kind == "index"));
}

#[test]
fn an_unreadable_snapshot_stops_the_cleanup_before_anything_is_deleted() {
    let _g = serial();
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &spec());
    a.upload(en.instance);
    session(&a, &root, en.instance, 1);
    age_everything(&w, 100);
    let junk = b"orphan".to_vec();
    let orphan = plant(&w, "packs/", Some(Hash::of(&junk)), "pack", &junk, true);
    filetime::set_file_mtime(w.remote_dir.join(&orphan), filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
    // damage one snapshot object in the cloud (and forget our cached copy of it)
    let commits = files_under(&w.remote_dir.join("instances").join(en.instance.to_string()).join("commits"));
    let victim = commits.iter().find(|p| p.extension().is_some_and(|e| e == "snap")).unwrap().clone();
    fs::write(&victim, b"garbage").unwrap();
    for f in files_under(&a.engine.state_dir().join("manifests")) {
        let _ = fs::remove_file(f);
    }
    let err = a.engine.gc_plan(&GcParams::default(), &a.ctx()).unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err}");
    assert!(exists(&w, &orphan), "an incomplete picture of the history never leads to deletions");
}

#[test]
fn a_cleanup_killed_at_any_step_leaves_everything_restorable_and_can_simply_be_run_again() {
    // Termination during cleanup (required scenario 9): after the release markers, after the merged index,
    // before the first deletion, and in the middle of the deletions. Whatever the moment, every snapshot that
    // was to be kept still restores byte for byte, and running the cleanup again finishes the job.
    let _g = serial();
    let params = GcParams {
        retention: Some(RetentionPolicy { keep_snapshots: 2, keep_days: 0, keep_crash_days: 90 }),
        ..Default::default()
    };
    let steps: [(&str, u32); 6] = [
        ("gc.after_expire_markers", 1),
        ("gc.after_merged_index", 1),
        ("gc.before_deletes", 1),
        ("gc.after_delete", 1),
        ("gc.after_delete", 3),
        ("gc.after_delete", 5),
    ];
    for (point, nth) in steps {
        let w = TestWorld::new();
        let a = w.device("desktop");
        let (root, _info, en) = a.new_instance("pack", &spec());
        a.upload(en.instance);
        let ids: Vec<SnapshotId> = (1..=6).map(|n| session(&a, &root, en.instance, n)).collect();
        let (head, previous) = (ids[5], ids[4]);
        let at_head = tree(&root);
        age_everything(&w, 100);
        let junk = format!("an abandoned upload {point}").into_bytes();
        let orphan = plant(&w, "packs/", Some(Hash::of(&junk)), "pack", &junk, true);
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 3600);
        filetime::set_file_mtime(w.remote_dir.join(&orphan), filetime::FileTime::from_system_time(t)).unwrap();

        let plan = a.engine.gc_plan(&params, &a.ctx()).unwrap();
        assert!(!plan.delete.is_empty() && !plan.expire.is_empty(), "{point}: nothing to do: {plan:#?}");
        failpoint::arm(point, Action::Error, Some(nth));
        let r = a.engine.gc_run(&plan.id, &params, &a.ctx());
        failpoint::disarm_all();
        assert!(matches!(r, Err(Error::InjectedCrash(_))), "{point}@{nth}: {r:?}");

        // whatever state the cloud was left in, the snapshots that are kept are intact...
        let v = a.engine.verify_remote(en.instance, Some(head), true, &a.ctx()).unwrap();
        assert!(v.missing_packs.is_empty() && v.damaged_blobs.is_empty(), "{point}@{nth}: {v:?}");
        let v = a.engine.verify_remote(en.instance, Some(previous), true, &a.ctx()).unwrap();
        assert!(v.missing_packs.is_empty() && v.damaged_blobs.is_empty(), "{point}@{nth}: {v:?}");
        // ...and a brand-new computer restores the head from what is there
        let b = w.device("laptop");
        let rb = b
            .engine
            .restore_new(
                RestoreNewRequest {
                    instance: en.instance,
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
            .unwrap_or_else(|e| panic!("{point}@{nth}: the head must restore after an interrupted cleanup: {e}"));
        assert_eq!(tree(&rb.root), at_head, "{point}@{nth}");

        // running the cleanup again (a fresh preview, as a person would) completes it
        let plan2 = a.engine.gc_plan(&params, &a.ctx()).unwrap();
        let rep = a.engine.gc_run(&plan2.id, &params, &a.ctx()).unwrap_or_else(|e| panic!("{point}@{nth}: {e}"));
        let _ = rep;
        let left = a.engine.gc_plan(&params, &a.ctx()).unwrap();
        assert!(left.expire.is_empty() && left.delete.is_empty(), "{point}@{nth}: not finished: {left:#?}");
        assert!(!exists(&w, &orphan), "{point}@{nth}: the orphan is gone");
        let v = a.engine.verify_remote(en.instance, Some(head), true, &a.ctx()).unwrap();
        assert!(v.missing_packs.is_empty() && v.damaged_blobs.is_empty(), "{point}@{nth}: {v:?}");
    }
}

/// When the index objects are lost (a Drive hiccup, someone tidying up the folder), every pack looks like
/// an abandoned upload to a computer whose cache is empty. Cleanup used to plan to delete the packs that
/// hold the only copy of the head's data; it now reads each such pack's own table of contents first.
#[test]
fn packs_whose_index_was_lost_are_kept_when_a_kept_snapshot_needs_them() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let mut spec = packsync_core::testkit::fixtures::FixtureSpec::tiny();
    spec.worlds = 0; // only whole-file blobs: no chunk lists that would make the cleanup stop on its own
    let (_root, _info, en) = a.new_instance("pack", &spec);
    a.upload(en.instance);
    for e in walkdir::WalkDir::new(w.remote_dir.join("index")).into_iter().flatten().filter(|e| e.file_type().is_file())
    {
        fs::remove_file(e.path()).unwrap();
    }
    age_everything(&w, 100);
    let b = w.device("laptop"); // an empty cache: it knows the packs only from the listing
    let plan = b.engine.gc_plan(&GcParams::default(), &b.ctx()).unwrap();
    let doomed: Vec<_> = plan.delete.iter().filter(|d| d.kind == "pack").collect();
    assert!(doomed.is_empty(), "{doomed:#?}");
    assert!(plan.notes.iter().any(|n| n.contains("described by no index")), "{:?}", plan.notes);
    let rep = b.engine.gc_run(&plan.id, &GcParams::default(), &b.ctx()).unwrap();
    assert_eq!(rep.deleted_packs, 0);
}
