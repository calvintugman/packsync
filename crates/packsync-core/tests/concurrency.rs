//! Genuinely simultaneous publications and uploads: nothing may be lost to a race.

mod common;

use std::sync::Barrier;

use common::*;
use packsync_core::engine::*;
use packsync_core::graph::ConflictKind;
use packsync_core::testkit::harness::TestWorld;

#[test]
fn simultaneous_publications_from_one_parent_keep_both_branches() {
    for round in 0..8 {
        let world = TestWorld::new();
        let a = world.device("desktop");
        let (root_a, _info, en) = a.new_instance("pack", &spec(round));
        a.upload(en.instance);
        let b = world.device("laptop");
        let root_b = restore(&b, en.instance, "pack");
        write(&root_a, "config/from-a.toml", format!("a = {round}\n").as_bytes());
        write(&root_b, "config/from-b.toml", format!("b = {round}\n").as_bytes());
        let sa = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
        let sb = b.engine.snapshot(en.instance, SnapshotOpts::default(), &b.ctx()).unwrap();
        let (SnapshotResult::Created { id: ida }, SnapshotResult::Created { id: idb }) = (sa.result, sb.result) else {
            panic!()
        };

        // Both publish at the same instant, from the same parent.
        let barrier = Barrier::new(2);
        std::thread::scope(|s| {
            let ha = s.spawn(|| {
                barrier.wait();
                a.engine.upload_pending(en.instance, true, &a.ctx())
            });
            let hb = s.spawn(|| {
                barrier.wait();
                b.engine.upload_pending(en.instance, true, &b.ctx())
            });
            ha.join().unwrap().expect("A's publication must succeed");
            hb.join().unwrap().expect("B's publication must succeed");
        });

        // Neither branch is silently lost: the cloud shows two heads and each device sees the conflict.
        let cloud = a.engine.cloud_instances(&a.ctx()).unwrap();
        assert_eq!(cloud[0].heads.len(), 2, "round {round}: both commits must be retained as heads");
        let mut heads: Vec<_> = cloud[0].heads.iter().map(|h| h.id).collect();
        heads.sort();
        let mut expect = vec![ida, idb];
        expect.sort();
        assert_eq!(heads, expect, "round {round}");
        for d in [&a, &b] {
            match d.engine.reconcile(en.instance, ReconcileOpts::default(), &d.ctx()).unwrap() {
                ReconcileReport::Conflict(c) => {
                    assert!(matches!(c.kind, ConflictKind::MultipleCloudHeads | ConflictKind::Diverged), "{c:?}")
                }
                other => panic!("round {round}: expected a conflict, got {other:?}"),
            }
        }
        // Resolve on A by continuing its own version: B then converges, and both branches stay in history.
        a.engine.resolve_conflict(en.instance, Resolution::KeepLocal, &a.ctx()).unwrap();
        let r = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap();
        assert!(matches!(r, ReconcileReport::Conflict(_) | ReconcileReport::FastForwarded { .. }), "{r:?}");
        let hist = a.engine.history(en.instance, true, &a.ctx()).unwrap();
        assert!(
            hist.iter().any(|h| h.id == ida) && hist.iter().any(|h| h.id == idb),
            "round {round}: both snapshots remain in history"
        );
        assert_eq!(a.engine.cloud_instances(&a.ctx()).unwrap()[0].heads.len(), 1);
    }
}

#[test]
fn unrelated_instances_uploaded_at_the_same_time_all_survive() {
    for round in 0..6 {
        let world = TestWorld::new();
        let a = world.device("a");
        let b = world.device("b");
        let c = world.device("c");
        let (root_a, _, en_a) = a.new_instance("pack-a", &spec(100 + round));
        let (root_b, _, en_b) = b.new_instance("pack-b", &spec(200 + round));
        let (root_c, _, en_c) = c.new_instance("pack-c", &spec(300 + round));
        let barrier = Barrier::new(3);
        std::thread::scope(|s| {
            for (d, i) in [(&a, en_a.instance), (&b, en_b.instance), (&c, en_c.instance)] {
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    d.engine.upload_pending(i, true, &d.ctx()).expect("upload");
                });
            }
        });
        // a fourth computer, with an empty cache, sees all three and restores them byte for byte
        let z = world.device("z");
        let cloud = z.engine.cloud_instances(&z.ctx()).unwrap();
        assert_eq!(cloud.len(), 3, "round {round}: no instance may vanish because of an index/catalog race");
        for (src, id, name) in
            [(&root_a, en_a.instance, "ra"), (&root_b, en_b.instance, "rb"), (&root_c, en_c.instance, "rc")]
        {
            let r = restore(&z, id, name);
            assert_eq!(digest(src), digest(&r), "round {round}: {name}");
        }
    }
}

#[test]
fn identical_content_is_deduplicated_across_instances() {
    let world = TestWorld::new();
    let a = world.device("a");
    let (_r1, info, en1) = a.new_instance("first", &spec(5));
    let up1 = a.upload(en1.instance);
    assert!(up1.bytes_uploaded > 100_000);
    // a second instance with identical mods/worlds (same seed): every blob is already in the cloud
    let before = a.meter.snapshot();
    let (_r2, _, en2) = a.new_instance("second", &spec(5));
    let up2 = a.upload(en2.instance);
    let d = a.meter.snapshot().since(&before);
    assert_eq!(up2.bytes_uploaded, 0, "no payload for content the cloud already holds: {d:?}");
    assert_eq!(d.payload_puts(), 0);
    // yet it is fully restorable elsewhere
    let b = world.device("b");
    let r = restore(&b, en2.instance, "second");
    assert_eq!(digest(&r), digest(&_r2));
    let _ = info;
}
