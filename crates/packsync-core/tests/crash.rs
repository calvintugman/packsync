//! Simulated process death at every destructive transaction boundary.
//!
//! An armed failpoint returns `Error::InjectedCrash`, and all cleanup/rollback code deliberately
//! skips work for it, so the test sees exactly the on-disk and in-database state a killed process
//! would leave behind. We then "restart" (a brand-new `Engine` over the same directories), run
//! recovery, and check the invariants. The failpoint registry is process-global, so every test
//! holds a mutex.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use packsync_core::Error;
use packsync_core::db::InstanceState;
use packsync_core::engine::*;
use packsync_core::failpoint::{self, Action};
use packsync_core::hash::Hash;
use packsync_core::ids::InstanceId;
use packsync_core::testkit::fixtures::{self, FixtureSpec};
use packsync_core::testkit::harness::{TestDevice, TestWorld, is_synced_path};

static SERIAL: Mutex<()> = Mutex::new(());

struct Guard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
fn serial() -> Guard {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    failpoint::disarm_all();
    Guard(g)
}
impl Drop for Guard {
    fn drop(&mut self) {
        failpoint::disarm_all();
    }
}

fn digest(root: &std::path::Path) -> Vec<(String, Hash)> {
    let mut v = fixtures::tree_digest(root, is_synced_path);
    let policy = packsync_core::mixed::options_txt_policy("");
    for (p, h) in v.iter_mut() {
        if p == "options.txt" {
            *h = Hash::of(&packsync_core::mixed::project(&policy, &fs::read(root.join(&*p)).unwrap()).unwrap());
        }
    }
    v
}

fn spec() -> FixtureSpec {
    let mut s = FixtureSpec::tiny();
    s.mods = 6;
    s.mod_size = (60_000, 200_000); // several 256 KiB packs
    s.regions_per_world = 3;
    s
}

fn is_crash(r: &Result<impl std::fmt::Debug, Error>) -> bool {
    matches!(r, Err(Error::InjectedCrash(_)))
}

struct Setup {
    world: TestWorld,
    a: TestDevice,
    b: TestDevice,
    inst: InstanceId,
    root_a: PathBuf,
    root_b: PathBuf,
    old: Vec<(String, Hash)>,
    new: Vec<(String, Hash)>,
}

/// A and B in sync at state OLD; A then publishes NEW (edits, deletion, rename, new + removed dirs).
fn two_devices_a_ahead() -> Setup {
    let world = TestWorld::new();
    let a = world.device("desktop");
    let (root_a, info, en) = a.new_instance("pack", &spec());
    a.upload(en.instance);
    let b = world.device("laptop");
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
    let old = digest(&rb.root);
    assert_eq!(old, digest(&root_a));
    // A's next session
    fixtures::edit_region(&info.regions[0], 4, 21);
    fs::write(root_a.join("config/added.toml"), b"added = 1\n").unwrap();
    fs::remove_file(&info.mods[0]).unwrap();
    fs::rename(&info.configs[0], root_a.join("config/renamed.toml")).unwrap();
    fs::create_dir_all(root_a.join("config/new-empty")).unwrap();
    fs::remove_dir_all(root_a.join("config/empty-folder")).unwrap();
    let rep = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    assert!(matches!(rep.result, SnapshotResult::Created { .. }));
    a.upload(en.instance);
    let new = digest(&root_a);
    assert_ne!(old, new);
    Setup { world, a, b, inst: en.instance, root_a, root_b: rb.root, old, new }
}

#[test]
fn crash_anywhere_in_an_in_place_restore_leaves_old_or_new_never_a_mix_and_converges() {
    let _g = serial();
    // (failpoint, nth hit, state after recovery: old=true / new=false)
    let cases: &[(&str, u32, bool)] = &[
        ("restore.after_plan", 1, true),
        ("restore.after_fetch_batch", 1, true),
        ("restore.after_staged", 1, true),
        ("restore.after_batch_begin", 1, true),
        ("restore.mid_batch", 1, true),
        ("restore.mid_batch", 2, true),
        ("restore.mid_batch", 4, true),
        ("restore.mid_batch", 7, true),
        ("restore.after_trash", 1, true),
        ("restore.after_trash", 3, true),
        ("restore.after_place", 1, true),
        ("restore.after_place", 2, true),
        ("restore.after_place", 5, true),
        ("restore.after_batch_done", 1, true),
        ("restore.after_applied", 1, false),
        ("restore.after_db_update", 1, false),
        ("restore.after_finalize_move", 1, false),
    ];
    let mut exercised = 0;
    for (fp, nth, expect_old) in cases {
        let mut s = two_devices_a_ahead();
        failpoint::arm(fp, Action::Error, Some(*nth));
        let r = s.b.engine.reconcile(s.inst, ReconcileOpts::default(), &s.b.ctx());
        if !is_crash(&r) {
            // the boundary may not be reached (fewer ops than N): that is fine, the restore simply succeeded
            failpoint::disarm_all();
            assert_eq!(digest(&s.root_b), s.new, "{fp}@{nth}: completed restore must equal NEW");
            continue;
        }
        exercised += 1;
        failpoint::disarm_all();
        // Between crash and recovery the instance is flagged, and nothing may run on it.
        let st = s.b.engine.instance(s.inst).unwrap().state;
        assert!(matches!(st, InstanceState::Restoring | InstanceState::Ok), "{fp}@{nth}: {st:?}");

        s.b.restart();
        let rec = s.b.engine.recover(&s.b.ctx()).unwrap();
        let after = digest(&s.root_b);
        if *expect_old {
            assert_eq!(
                after, s.old,
                "{fp}@{nth}: crash before the 'applied' record must roll back to EXACTLY the old state"
            );
            if *fp != "restore.after_plan" {
                // (that boundary precedes the creation of the transaction directory: nothing to recover)
                assert!(rec.rolled_back.contains(&s.inst) || rec.discarded_transactions > 0, "{fp}@{nth}: {rec:?}");
            }
        } else {
            assert_eq!(after, s.new, "{fp}@{nth}: crash after 'applied' rolls forward to the new state");
        }
        assert_eq!(s.b.engine.instance(s.inst).unwrap().state, InstanceState::Ok, "{fp}@{nth}");
        // never a leftover transaction after recovery
        assert!(
            packsync_core::journal::pending_txs(&s.root_b).is_empty(),
            "{fp}@{nth}: transaction directory must be gone"
        );

        // resuming converges to NEW, from either side of the crash
        let r = s.b.engine.reconcile(s.inst, ReconcileOpts::default(), &s.b.ctx()).unwrap();
        assert!(
            matches!(r, ReconcileReport::FastForwarded { .. } | ReconcileReport::UpToDate { .. }),
            "{fp}@{nth}: {r:?}"
        );
        assert_eq!(digest(&s.root_b), s.new, "{fp}@{nth}: converged");
        assert!(
            !s.b.engine.pending_changes(s.inst, &s.b.ctx()).unwrap().dirty,
            "{fp}@{nth}: nothing pending after convergence"
        );
        let _ = (&s.world, &s.a, &s.root_a);
    }
    assert!(exercised >= 12, "most boundaries must actually be reached, only {exercised} were");
}

#[test]
fn recovery_itself_can_crash_and_be_repeated() {
    let _g = serial();
    for fp in ["recover.before_rollback", "recover.after_rollback"] {
        let mut s = two_devices_a_ahead();
        failpoint::arm("restore.after_place", Action::Error, Some(3));
        let r = s.b.engine.reconcile(s.inst, ReconcileOpts::default(), &s.b.ctx());
        assert!(is_crash(&r));
        failpoint::disarm_all();
        s.b.restart();
        failpoint::arm(fp, Action::Error, Some(1));
        let r = s.b.engine.recover(&s.b.ctx());
        assert!(is_crash(&r), "{fp}: {r:?}");
        failpoint::disarm_all();
        s.b.restart();
        s.b.engine.recover(&s.b.ctx()).unwrap();
        assert_eq!(digest(&s.root_b), s.old, "{fp}: a second recovery finishes the job without damaging anything");
        let r = s.b.engine.reconcile(s.inst, ReconcileOpts::default(), &s.b.ctx()).unwrap();
        assert!(matches!(r, ReconcileReport::FastForwarded { .. }), "{r:?}");
        assert_eq!(digest(&s.root_b), s.new);
    }
}

#[test]
fn crash_during_new_directory_restore_leaves_no_half_instance() {
    let _g = serial();
    for (fp, nth) in [
        ("restore_new.after_marker", 1),
        ("restore_new.after_file", 1),
        ("restore_new.after_file", 5),
        ("restore_new.before_register", 1),
    ] {
        let world = TestWorld::new();
        let a = world.device("desktop");
        let (_root, _info, en) = a.new_instance("pack", &spec());
        a.upload(en.instance);
        let mut b = world.device("laptop");
        let parent = b.instances_dir();
        let req = || RestoreNewRequest {
            instance: en.instance,
            snapshot: None,
            parent_dir: parent.clone(),
            folder_name: "pack".into(),
            launcher: "generic".into(),
            launcher_ref: None,
            local: Default::default(),
            skip_incompatible: false,
        };
        failpoint::arm(fp, Action::Error, Some(nth));
        let r = b.engine.restore_new(req(), &b.ctx());
        assert!(is_crash(&r), "{fp}@{nth}: {:?}", r.map(|_| ()));
        failpoint::disarm_all();
        // the half-restored folder exists but is hidden from the launcher (dot-folder), and not registered
        assert!(b.engine.instances().unwrap().is_empty());
        b.restart();
        let rec = b.engine.recover(&b.ctx()).unwrap();
        let leftovers: Vec<_> = fs::read_dir(b.instances_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.is_empty(),
            "{fp}@{nth}: recovery removes our own partial restore; left: {leftovers:?} / {rec:?}"
        );
        // a retry now succeeds
        let r = b.engine.restore_new(req(), &b.ctx()).unwrap();
        assert!(r.root.join("mods").is_dir());
        assert_eq!(b.engine.instances().unwrap().len(), 1);
    }
}

#[test]
fn crash_at_every_snapshot_and_upload_boundary_is_resumable_and_never_exposes_a_partial_snapshot() {
    let _g = serial();
    let cases: &[(&str, u32)] = &[
        ("snapshot.after_walk", 1),
        ("snapshot.after_hash", 1),
        ("snapshot.after_stage_move", 1),
        ("snapshot.before_db_commit", 1),
        ("snapshot.after_db_commit", 1),
        ("upload.before_packs", 1),
        ("upload.after_pack", 1),
        ("upload.after_pack", 2),
        ("upload.after_packs", 1),
        ("upload.after_index", 1),
        ("publish.before_manifest", 1),
        ("publish.after_manifest", 1),
        ("publish.before_head", 1),
        ("upload.before_cleanup", 1),
    ];
    let mut crashed = 0;
    for (fp, nth) in cases {
        let world = TestWorld::new();
        let mut a = world.device("desktop");
        let (root_a, info, en) = a.new_instance("pack", &spec());
        a.upload(en.instance);
        let b = world.device("laptop");
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
        let old = digest(&rb.root);
        fixtures::edit_region(&info.regions[0], 6, 77);
        fs::write(root_a.join("config/session.toml"), b"x = 2\n").unwrap();
        fs::remove_file(&info.mods[1]).unwrap();
        let new = digest(&root_a);

        failpoint::arm(fp, Action::Error, Some(*nth));
        let r = a
            .engine
            .snapshot(en.instance, SnapshotOpts::default(), &a.ctx())
            .and_then(|_| a.engine.upload_pending(en.instance, true, &a.ctx()).map(|_| ()));
        if !is_crash(&r) {
            failpoint::disarm_all();
            continue;
        }
        crashed += 1;
        failpoint::disarm_all();

        // INVARIANT 1: another computer never sees a snapshot whose data is incomplete. It either
        // sees the old head (nothing newer is published yet) or the new one, complete.
        let seen = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx());
        match &seen {
            Ok(ReconcileReport::UpToDate { .. }) => assert_eq!(digest(&rb.root), old, "{fp}@{nth}"),
            Ok(ReconcileReport::FastForwarded { .. }) => {
                assert_eq!(digest(&rb.root), new, "{fp}@{nth}: a visible snapshot is always complete")
            }
            other => panic!("{fp}@{nth}: another device must never hit a broken snapshot: {other:?}"),
        }

        // INVARIANT 2: after restart + recovery the work resumes without duplicating or corrupting history.
        a.restart();
        a.engine.recover(&a.ctx()).unwrap();
        assert_eq!(digest(&root_a), new, "{fp}@{nth}: the live instance is untouched by upload crashes");
        let st = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
        assert!(!matches!(st.1, ReconcileReport::Conflict(_)), "{fp}@{nth}: {:?}", st.1);
        let rb_report = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap();
        assert!(
            matches!(rb_report, ReconcileReport::FastForwarded { .. } | ReconcileReport::UpToDate { .. }),
            "{fp}@{nth}: {rb_report:?}"
        );
        assert_eq!(digest(&rb.root), new, "{fp}@{nth}: B converges on A's final state");
        // exactly one head in the cloud, no hidden branch from the interrupted attempt
        let cloud = b.engine.cloud_instances(&b.ctx()).unwrap();
        assert_eq!(cloud[0].heads.len(), 1, "{fp}@{nth}: {cloud:?}");
        // and A reports honest, complete status
        let status = a.engine.status(en.instance, true, &a.ctx()).unwrap();
        assert_eq!(status.phase, SyncPhase::ReadyOnOtherDevices, "{fp}@{nth}: {status:?}");
        assert_eq!(status.pending_snapshots, 0);
        let _ = &b;
    }
    assert!(crashed >= 10, "most boundaries must be reached; got {crashed}");
}

/// A history restore the user asked for must stick even when the process dies after the files were
/// applied: recovery records it as a new snapshot (exactly as the uninterrupted path does), so the next
/// sync uploads it instead of fast-forwarding back to the newer cloud head.
#[test]
fn a_history_restore_interrupted_after_apply_still_sticks() {
    let _g = serial();
    for fp in ["restore.after_applied", "restore.after_db_update", "restore.after_finalize_move"] {
        let mut s = two_devices_a_ahead();
        // B catches up to NEW, so the history has OLD → NEW and B sits on NEW
        let r = s.b.engine.reconcile(s.inst, ReconcileOpts::default(), &s.b.ctx()).unwrap();
        assert!(matches!(r, ReconcileReport::FastForwarded { .. }), "{r:?}");
        assert_eq!(digest(&s.root_b), s.new);
        let old_id =
            s.b.engine
                .history(s.inst, true, &s.b.ctx())
                .unwrap()
                .into_iter()
                .find(|h| h.parents.is_empty())
                .unwrap()
                .id;
        failpoint::arm(fp, Action::Error, Some(1));
        let r = s.b.engine.restore_snapshot(s.inst, old_id, RestoreOpts { skip_incompatible: false }, &s.b.ctx());
        assert!(is_crash(&r), "{fp}: {r:?}");
        failpoint::disarm_all();
        s.b.restart();
        let rec = s.b.engine.recover(&s.b.ctx()).unwrap();
        assert_eq!(rec.rolled_forward, vec![s.inst], "{fp}");
        assert_eq!(digest(&s.root_b), s.old, "{fp}: the restored content is in place");
        // the restore is recorded on top of NEW, so the cloud head is *behind* this computer — not ahead
        let r =
            s.b.engine
                .reconcile(s.inst, ReconcileOpts { apply_fast_forward: true, upload_local: false }, &s.b.ctx())
                .unwrap();
        assert!(matches!(r, ReconcileReport::LocalAhead { .. }), "{fp}: {r:?}");
        assert_eq!(digest(&s.root_b), s.old, "{fp}: the next sync did not undo the restore");
        s.b.upload(s.inst);
        // and the other computer follows it
        let r = s.a.engine.reconcile(s.inst, ReconcileOpts::default(), &s.a.ctx()).unwrap();
        assert!(matches!(r, ReconcileReport::FastForwarded { .. }), "{fp}: {r:?}");
        assert_eq!(digest(&s.root_a), s.old, "{fp}");
        let _ = &s.world;
    }
}
