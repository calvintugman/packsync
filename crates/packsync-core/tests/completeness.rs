//! A published snapshot is complete: every blob it refers to is in the cloud, whatever path led to it —
//! splitting off a branch that was never uploaded, promoting a crash save, a cleanup on another computer
//! that removed data a dormant computer still relies on, a pack that vanished behind the local cache.
//!
//! These scenarios came out of an independent review; each one used to publish a head that another
//! computer could not restore.

mod common;

use std::fs;

use common::*;
use packsync_core::db::SessionOwner;
use packsync_core::engine::*;
use packsync_core::ids::InstanceId;
use packsync_core::storage::StorageError;
use packsync_core::storage_fault::{Fault, FaultRule, OpKind, When};
use packsync_core::testkit::fixtures::FixtureSpec;
use packsync_core::testkit::harness::{TestDevice, TestWorld};

/// Incompressible-looking text of a given size: content that exists in no other pack.
fn unique_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            b'a' + (s % 26) as u8
        })
        .collect()
}

/// Everything another computer needs to restore `inst` is there: the engine's own remote check is clean
/// and a brand-new device can actually restore the head.
fn assert_restorable_from_scratch(w: &TestWorld, owner: &TestDevice, who: &str, inst: InstanceId) {
    let v = owner.engine.verify_remote(inst, None, false, &owner.ctx()).unwrap();
    assert!(v.missing_packs.is_empty() && v.unlisted_blobs.is_empty(), "the cloud copy is incomplete: {v:?}");
    let c = w.device(who);
    c.engine
        .restore_new(restore_req(&c, inst, "restored"), &c.ctx())
        .unwrap_or_else(|e| panic!("a new computer cannot restore the published head: {e}"));
}

fn two_computers_with_diverged_offline_work() -> (TestWorld, TestDevice, TestDevice, InstanceId, std::path::PathBuf) {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root_a, _info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    let b = w.device("laptop");
    let rb = restore(&b, en.instance, "pack");

    // both play offline; the laptop's work contains content that exists nowhere else
    fs::write(root_a.join("config/from-desktop.toml"), b"desktop = true\n").unwrap();
    fs::write(rb.join("config/from-laptop.toml"), unique_bytes(7, 300_000)).unwrap();
    b.plan.set_offline(true);
    let (_, r) = b.engine.sync_now(en.instance, &b.ctx()).unwrap();
    assert!(matches!(r, ReconcileReport::Offline { .. }), "{r:?}");
    b.plan.set_offline(false);
    // the desktop publishes first
    a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    // the laptop finds the conflict — which does not upload its local branch
    let r = b
        .engine
        .reconcile(en.instance, ReconcileOpts { apply_fast_forward: true, upload_local: false }, &b.ctx())
        .unwrap();
    assert!(matches!(r, ReconcileReport::Conflict(_)), "{r:?}");
    (w, a, b, en.instance, rb)
}

/// "Keep both as separate instances" when this computer's branch was never uploaded — the ordinary case.
/// The fork used to be published without the laptop's unique content, and said it was uploaded.
#[test]
fn splitting_off_a_branch_that_was_never_uploaded_gives_a_complete_fork() {
    let (w, _a, b, inst, rb) = two_computers_with_diverged_offline_work();
    let rep =
        b.engine.resolve_conflict(inst, Resolution::SplitLocal { new_name: "Laptop Fork".into() }, &b.ctx()).unwrap();
    let fork = rep.new_instance.unwrap();
    assert!(rep.uploaded);
    assert_restorable_from_scratch(&w, &b, "tablet", fork);
    // and the laptop's unique file really is in the restored fork
    let t = w.device("tablet2");
    let restored = restore(&t, fork, "fork");
    assert_eq!(
        fs::read(restored.join("config/from-laptop.toml")).unwrap(),
        fs::read(rb.join("config/from-laptop.toml")).unwrap()
    );
    // the original instance continues with the desktop's version, and nothing was lost from its history
    let t2 = w.device("tablet3");
    let original = restore(&t2, inst, "original");
    assert!(original.join("config/from-desktop.toml").exists() && !original.join("config/from-laptop.toml").exists());
}

/// If the upload cannot happen, the split does not either: nothing is renamed, nothing half-published.
#[test]
fn splitting_off_a_branch_stops_when_it_cannot_be_uploaded() {
    let (_w, _a, b, inst, _rb) = two_computers_with_diverged_offline_work();
    b.plan.set_offline(true);
    let err = b
        .engine
        .resolve_conflict(inst, Resolution::SplitLocal { new_name: "Laptop Fork".into() }, &b.ctx())
        .unwrap_err();
    assert!(matches!(err.code(), packsync_core::error::ErrorCode::Offline), "{err}");
    let still = b.engine.instance(inst).expect("the instance was not renamed");
    assert_eq!(still.id, inst);
    b.plan.set_offline(false);
    // and it can simply be tried again
    let rep =
        b.engine.resolve_conflict(inst, Resolution::SplitLocal { new_name: "Laptop Fork".into() }, &b.ctx()).unwrap();
    assert!(rep.uploaded);
}

/// An offline session that crashed; the user checks the world and promotes the crash save. Returns the
/// crash snapshot and the promoted one.
fn crash_then_promote(
    a: &TestDevice,
    root: &std::path::Path,
    inst: InstanceId,
    seed: u64,
) -> (packsync_core::ids::SnapshotId, packsync_core::ids::SnapshotId) {
    a.plan.set_offline(true);
    let begun = a
        .engine
        .begin_session(
            inst,
            BeginSession { owner: SessionOwner::App, token: format!("t{seed}"), offline: true },
            &a.ctx(),
        )
        .unwrap();
    fs::write(root.join("config/crashed-world.toml"), unique_bytes(100 + seed, 200_000)).unwrap();
    let end = a
        .engine
        .end_session(begun.session, ExitInfo { exit_code: Some(1), clean_signal: false }, false, &a.ctx())
        .unwrap();
    let crash = end.snapshot.expect("a crash snapshot");
    let promoted = a.engine.promote_crash(inst, &a.ctx()).unwrap();
    a.plan.set_offline(false);
    (crash, promoted)
}

/// The promoted snapshot has the crash save's tree. It used to have no dependency on the crash save, so
/// (about half the time, by snapshot id) it was published first — and stayed published when the crash
/// save's data could not be uploaded, leaving a head that another computer could not restore.
#[test]
fn a_promoted_crash_save_is_never_published_before_its_content() {
    for attempt in 0..6 {
        let w = TestWorld::new();
        let a = w.device("desktop");
        let (root, _info, en) = a.new_instance("pack", &FixtureSpec::tiny());
        a.upload(en.instance);
        let (crash, promoted) = crash_then_promote(&a, &root, en.instance, attempt);

        let g = packsync_core::graph::Graph::load(a.engine.db(), en.instance).unwrap();
        let order = g.unpublished_in_order();
        let pos = |id| order.iter().position(|x| *x == id).unwrap();
        assert!(pos(crash) < pos(promoted), "attempt {attempt}: the crash save is uploaded first: {order:?}");

        // its data cannot be stored: nothing built on it may be published
        a.plan.add(
            FaultRule::new(OpKind::Put, When::Always, Fault::Error(StorageError::Offline("link drops".into())))
                .on_key("packs/"),
        );
        assert!(a.engine.upload_pending(en.instance, true, &a.ctx()).is_err());
        a.plan.clear();
        let st = a.engine.status(en.instance, false, &a.ctx()).unwrap();
        assert_ne!(st.phase, SyncPhase::ReadyOnOtherDevices, "{st:?}");
        // …and when the link is back everything goes out, in order, and is restorable
        a.engine.upload_pending(en.instance, true, &a.ctx()).unwrap();
        assert_restorable_from_scratch(&w, &a, "laptop", en.instance);
    }
}

/// After a crash save is discarded, new snapshots must not lean on the discarded crash save's staged
/// data (it is not their ancestor and may never be published): content that happens to be identical is
/// saved again with the new snapshot.
#[test]
fn identical_content_is_saved_again_after_a_crash_save_was_discarded() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    a.plan.set_offline(true);
    let begun = a
        .engine
        .begin_session(
            en.instance,
            BeginSession { owner: SessionOwner::App, token: "t".into(), offline: true },
            &a.ctx(),
        )
        .unwrap();
    let content = unique_bytes(5, 150_000);
    fs::write(root.join("config/world-state.toml"), &content).unwrap();
    a.engine.end_session(begun.session, ExitInfo { exit_code: Some(1), clean_signal: false }, false, &a.ctx()).unwrap();
    a.plan.set_offline(false); // putting the crash state away needs the cloud (it restores the last good version)
    a.engine.discard_crash(en.instance, &a.ctx()).unwrap();
    assert!(!root.join("config/world-state.toml").exists(), "the crash state was put away");
    // the same bytes come back by another route
    fs::write(root.join("config/world-state.toml"), &content).unwrap();
    let rep = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    assert!(
        rep.stats.new_blobs > 0,
        "the content is staged with this snapshot, not borrowed from the crash save: {rep:?}"
    );
    a.engine.upload_pending(en.instance, true, &a.ctx()).unwrap();
    assert_restorable_from_scratch(&w, &a, "laptop", en.instance);
}
