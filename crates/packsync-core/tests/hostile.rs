//! Untrusted cloud data and hostile local conditions: corrupt, missing or malicious objects must
//! be rejected *before* the live instance is touched, and secrets must never leave the machine.

mod common;

use std::fs;

use common::*;
use packsync_core::engine::*;
use packsync_core::error::{Error, ErrorCode};
use packsync_core::hash::Hash;
use packsync_core::manifest::{Entry, EntryType};
#[cfg(unix)]
use packsync_core::paths::FoldMode;
use packsync_core::testkit::harness::TestWorld;

/// A (cloud head with new content, B still at the old head) situation.
fn b_behind() -> (
    TestWorld,
    packsync_core::testkit::harness::TestDevice,
    packsync_core::testkit::harness::TestDevice,
    packsync_core::ids::InstanceId,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root_a, info, en) = a.new_instance("pack", &spec(9));
    a.upload(en.instance);
    let b = w.device("laptop");
    let root_b = restore(&b, en.instance, "pack");
    packsync_core::testkit::fixtures::edit_region(&info.regions[0], 5, 3);
    write(&root_a, "config/new-in-a.toml", b"n = 1\n");
    a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    a.upload(en.instance);
    (w, a, b, en.instance, root_a, root_b)
}

fn no_leftovers(root: &std::path::Path) {
    assert!(packsync_core::journal::pending_txs(root).is_empty(), "no transaction directory may remain");
}

#[test]
fn a_damaged_pack_in_the_cloud_is_rejected_and_the_instance_is_untouched() {
    let (w, a, b, inst, _ra, rb) = b_behind();
    let before = digest(&rb);
    // damage every pack the new snapshot introduced
    let mut damaged = 0;
    for e in fs::read_dir(w.remote_dir.join("packs")).unwrap().flatten() {
        let mut bytes = fs::read(e.path()).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        fs::write(e.path(), &bytes).unwrap();
        damaged += 1;
    }
    assert!(damaged > 0);
    let err = b.engine.reconcile(inst, ReconcileOpts::default(), &b.ctx()).unwrap_err();
    assert!(matches!(err.code(), ErrorCode::CorruptRemote | ErrorCode::Invalid), "{err}");
    assert_eq!(digest(&rb), before, "a rejected restore changes nothing");
    no_leftovers(&rb);
    assert_eq!(b.engine.instance(inst).unwrap().state, packsync_core::db::InstanceState::Ok);
    assert!(!b.engine.pending_changes(inst, &b.ctx()).unwrap().dirty);
    // the verify command names the problem
    let rep = a.engine.verify_remote(inst, None, true, &a.ctx()).unwrap();
    assert!(!rep.damaged_blobs.is_empty(), "{rep:?}");
}

#[test]
fn a_missing_pack_is_detected_at_restore_and_at_publish_time() {
    let (w, a, b, inst, root_a, rb) = b_behind();
    let before = digest(&rb);
    // delete the pack holding data the head needs and B does not have yet (A's new config file)
    let (pack, _) = a.engine.db().blob_lookup(Hash::of(b"n = 1\n")).unwrap().expect("A knows where it put it");
    let victim = w.remote_dir.join("packs").join(format!("{}.pack", pack.to_hex()));
    fs::remove_file(&victim).unwrap();
    let err = b.engine.reconcile(inst, ReconcileOpts::default(), &b.ctx()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::CorruptRemote, "{err}");
    assert!(err.advice().unwrap().contains("verify"));
    assert_eq!(digest(&rb), before);
    no_leftovers(&rb);
    // the owner is told, too: remote verification lists the missing pack…
    let rep = a.engine.verify_remote(inst, None, false, &a.ctx()).unwrap();
    assert!(!rep.missing_packs.is_empty(), "{rep:?}");
    // …and a new snapshot that would depend on the vanished pack refuses to publish
    fs::write(root_a.join("config/more.toml"), b"m = 2\n").unwrap();
    a.engine.snapshot(inst, SnapshotOpts::default(), &a.ctx()).unwrap();
    let e = a.engine.upload_pending(inst, true, &a.ctx());
    match e {
        Err(Error::Corrupt(m)) => assert!(m.contains("missing"), "{m}"),
        Ok(_) => {} // the new snapshot may not depend on the victim pack; then publishing is legitimately fine
        Err(other) => panic!("{other}"),
    }
}

#[test]
fn garbage_bombs_and_foreign_objects_in_the_cloud_are_ignored_not_trusted() {
    let (w, a, b, inst, _ra, rb) = b_behind();
    // an object that is not a manifest, one that hashes to a different name, and a decompression bomb
    plant_raw_commit(&w, inst, &Hash::of(b"a").to_hex(), b"this is not a manifest");
    plant_raw_commit(
        &w,
        inst,
        &Hash::of(b"b").to_hex(),
        &packsync_core::codec::compress_doc(b"{\"format\":\"packsync.snapshot\"}"),
    );
    let bomb = zstd::bulk::compress(&vec![b' '; 300 << 20], 19).unwrap();
    plant_raw_commit(&w, inst, &Hash::of(b"c").to_hex(), &bomb);
    // unknown objects in the cloud are never touched by anything
    fs::write(w.remote_dir.join("packs/not-ours.txt"), b"keep me").unwrap();
    fs::create_dir_all(w.remote_dir.join("instances/not-a-uuid")).unwrap();
    // discovery survives and the real work proceeds
    let r = b.engine.reconcile(inst, ReconcileOpts::default(), &b.ctx()).unwrap();
    assert!(matches!(r, ReconcileReport::FastForwarded { .. }), "{r:?}");
    let events = b.engine.db().events_recent(Some(inst), 50).unwrap();
    assert!(events.iter().any(|e| e.code == "damaged_snapshot"), "damaged objects are reported: {events:?}");
    assert!(w.remote_dir.join("packs/not-ours.txt").exists());
    assert_eq!(digest(&rb), digest(&_ra));
    let _ = a;
}

#[test]
fn malicious_manifests_cannot_write_outside_the_instance_or_pass_without_data() {
    let (w, a, b, inst, _ra, rb) = b_behind();
    let head = a.engine.cloud_instances(&a.ctx()).unwrap()[0].heads[0].id;
    let good = a.engine.manifest(inst, head, &a.ctx()).unwrap();
    let outside = rb.parent().unwrap().join("pwned.txt");

    let child = |f: &dyn Fn(&mut packsync_core::manifest::Manifest)| {
        let mut m = good.clone();
        m.parents = vec![head];
        m.created = "2099-01-01T00:00:00Z".into();
        f(&mut m);
        m
    };
    // 1. path traversal: structurally invalid, rejected by validation before anything else
    let mut m = child(&|m| {
        m.entries.push(Entry {
            p: "../pwned.txt".into(),
            t: EntryType::File,
            s: 1,
            m: 0,
            x: false,
            h: Some(Hash::of(b"x")),
            r: None,
            mx: None,
        });
    });
    m.stats = Default::default();
    let json = serde_json::to_vec(&m).unwrap();
    let id = Hash::of(&json);
    plant_raw_commit(&w, inst, &id.to_hex(), &packsync_core::codec::compress_doc(&json));

    // 2. a valid-looking child whose file data does not exist anywhere (malicious reconstruction mapping)
    let ghost = child(&|m| {
        m.entries.push(Entry {
            p: "config/ghost.toml".into(),
            t: EntryType::File,
            s: 100,
            m: 0,
            x: false,
            h: Some(Hash::of(b"nonexistent")),
            r: None,
            mx: None,
        });
        m.finalize().unwrap();
    });
    plant_manifest(&w, &ghost);

    let before = digest(&rb);
    let r = b.engine.reconcile(inst, ReconcileOpts::default(), &b.ctx());
    // two heads now (A's real head and the ghost child of it): never silently chosen
    assert!(matches!(r, Ok(ReconcileReport::Conflict(_))) || r.is_err(), "{r:?}");
    assert_eq!(digest(&rb), before, "nothing was applied");
    assert!(!outside.exists(), "nothing was written outside the instance");
    no_leftovers(&rb);
    // restoring the ghost explicitly fails before touching the instance
    let e = b.engine.restore_snapshot(
        inst,
        packsync_core::ids::SnapshotId(Hash::of(&ghost.encode().unwrap().1)),
        RestoreOpts { skip_incompatible: false },
        &b.ctx(),
    );
    let _ = e;
    let ghost_id = ghost.encode().unwrap().0;
    let e = b.engine.restore_snapshot(inst, ghost_id, RestoreOpts { skip_incompatible: false }, &b.ctx()).unwrap_err();
    assert!(matches!(e.code(), ErrorCode::CorruptRemote | ErrorCode::Conflict), "{e}");
    assert_eq!(digest(&rb), before);
    assert!(!rb.join("config/ghost.toml").exists());
}

/// A snapshot used to be able to carry `.packsync/tx/<x>/journal.jsonl` (plus the files it names): the next
/// startup recovery then executed the forged journal, with `..` in its paths, outside the instance.
#[test]
fn a_snapshot_cannot_carry_pack_sync_s_own_bookkeeping_and_a_forged_journal_is_never_executed() {
    use packsync_core::ids::{InstanceId, SnapshotId, TxId};
    use packsync_core::journal::{JOp, Rec};

    let (w, a, b, inst, _ra, rb) = b_behind();
    let head = a.engine.cloud_instances(&a.ctx()).unwrap()[0].heads[0].id;
    let good = a.engine.manifest(inst, head, &a.ctx()).unwrap();
    let outside = rb.parent().unwrap().join("escaped/payload.txt");

    // 1. the manifests are refused by validation — also when planted without going through `finalize`
    for planted in [".packsync/tx/evil/journal.jsonl", ".packsync/marker", "minecraft/.PackSync/tx/x", ".PACKSYNC/x"] {
        let mut m = good.clone();
        m.parents = vec![head];
        m.created = "2099-01-01T00:00:00Z".into();
        m.entries.push(Entry {
            p: planted.into(),
            t: EntryType::File,
            s: 1,
            m: 0,
            x: false,
            h: Some(Hash::of(b"x")),
            r: None,
            mx: None,
        });
        m.entries.sort_by(|x, y| x.p.as_bytes().cmp(y.p.as_bytes()));
        let err = m.finalize().unwrap_err();
        assert!(err.to_string().contains("bookkeeping"), "{planted}: {err}");
        m.stats = Default::default();
        let json = serde_json::to_vec(&m).unwrap();
        plant_raw_commit(&w, inst, &Hash::of(&json).to_hex(), &packsync_core::codec::compress_doc(&json));
    }
    let before = digest(&rb);
    let _ = b.engine.reconcile(inst, ReconcileOpts::default(), &b.ctx());
    assert!(!rb.join(".packsync/tx/evil").exists() && !outside.exists());
    no_leftovers(&rb);
    let _ = before;

    // 2. whatever got into the instance folder by other means: a journal with an escaping path is not run
    let tx = TxId::new();
    let txd = rb.join(".packsync/tx/evil");
    std::fs::create_dir_all(txd.join("stage/escaped")).unwrap();
    std::fs::write(txd.join("stage/escaped/payload.txt"), "forged").unwrap();
    let journal: String = [
        Rec::Begin {
            tx,
            target: SnapshotId(Hash::of(b"t")),
            base: None,
            instance: InstanceId::new(),
            created: "2026-01-01T00:00:00Z".into(),
            derive_note: None,
        },
        Rec::Ops { ops: vec![JOp::Write { p: "../escaped/payload.txt".into(), existed: false }] },
        Rec::Staged,
        Rec::BatchBegin { upto: 1 },
    ]
    .iter()
    .map(|r| serde_json::to_string(r).unwrap() + "\n")
    .collect();
    std::fs::write(txd.join("journal.jsonl"), journal).unwrap();
    // …next to a second, ordinary instance whose own (empty) interrupted transaction must still be cleaned up
    let (root2, _info2, en2) = b.new_instance("second", &spec(2));
    std::fs::create_dir_all(root2.join(".packsync/tx/old")).unwrap();

    let report = b.engine.recover(&b.ctx()).expect("one damaged journal does not abort recovery");
    assert!(!outside.exists(), "nothing was written outside the instance");
    assert_eq!(report.could_not_recover.len(), 1, "{report:?}");
    assert!(report.could_not_recover[0].1.contains("unsafe path"), "{report:?}");
    assert!(txd.exists(), "the forged transaction is left alone");
    assert!(!root2.join(".packsync/tx/old").exists(), "the other instance was still recovered ({})", en2.instance);
}

/// A newer version from another computer embeds wider rules (a newer PackSync with other defaults, or a
/// hostile snapshot that includes everything). Following it must not delete what THIS computer does not
/// sync — here its screenshots — which a fast-forward used to move to the recovery area (purged later).
#[test]
fn wider_rules_in_a_cloud_version_never_delete_what_this_computer_does_not_sync() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_ra, _info, en) = a.new_instance("pack", &spec(3));
    a.upload(en.instance);
    let b = w.device("laptop");
    let rb = restore(&b, en.instance, "pack");
    write(&rb, "screenshots/2026-10-07_12.00.00.png", b"the laptop's own screenshot");
    write(&rb, "shaderpacks/MyShaders.zip", b"the laptop's shaders");

    let head = a.engine.cloud_instances(&a.ctx()).unwrap()[0].heads[0].id;
    let mut m = a.engine.manifest(en.instance, head, &a.ctx()).unwrap();
    m.parents = vec![head];
    m.created = "2099-01-01T00:00:00Z".into();
    m.rules.text.push_str(
        "\n[[rule]]\nid = \"everything\"\nmatch = [\"**\"]\naction = \"include\"\nreason = \"sync everything\"\n",
    );
    m.rules.hash = Hash::of(m.rules.text.as_bytes());
    m.finalize().unwrap();
    plant_manifest(&w, &m);

    let r = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap();
    assert!(matches!(r, ReconcileReport::FastForwarded { .. }), "{r:?}");
    assert_eq!(fs::read(rb.join("screenshots/2026-10-07_12.00.00.png")).unwrap(), b"the laptop's own screenshot");
    assert_eq!(fs::read(rb.join("shaderpacks/MyShaders.zip")).unwrap(), b"the laptop's shaders");
}

#[test]
fn an_absurd_declared_size_fails_the_disk_space_check_before_anything_changes() {
    let (w, a, b, inst, _ra, rb) = b_behind();
    let head = a.engine.cloud_instances(&a.ctx()).unwrap()[0].heads[0].id;
    let mut m = a.engine.manifest(inst, head, &a.ctx()).unwrap();
    m.parents = vec![head];
    m.created = "2099-01-01T00:00:00Z".into();
    // a recipe-backed file claiming to be 40 TiB
    m.entries.push(Entry {
        p: "mods/huge.jar".into(),
        t: EntryType::File,
        s: 40 << 40,
        m: 0,
        x: false,
        h: Some(Hash::of(b"huge")),
        r: Some(Hash::of(b"huge-recipe")),
        mx: None,
    });
    m.finalize().unwrap();
    let id = plant_manifest(&w, &m);
    let before = digest(&rb);
    let e = b.engine.restore_snapshot(inst, id, RestoreOpts { skip_incompatible: false }, &b.ctx()).unwrap_err();
    assert!(matches!(e, Error::NoSpace { .. } | Error::Conflict(_)), "{e}");
    assert_eq!(digest(&rb), before);
    no_leftovers(&rb);
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_followed_and_a_symlinked_folder_is_never_recorded_as_deleted() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &spec(4));
    a.upload(en.instance);
    let outside = w.tmp.path().join("shared-saves");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("precious.dat"), b"outside the instance").unwrap();
    // the user replaces `saves` with a link to a shared folder
    fs::rename(root.join("saves"), w.tmp.path().join("saves-moved")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("saves")).unwrap();
    // snapshots and change detection refuse instead of publishing "all worlds deleted"
    let e = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap_err();
    assert!(matches!(e, Error::SafetyGuard(_)), "{e}");
    assert!(e.to_string().contains("symbolic link"));
    let e = a.engine.pending_changes(en.instance, &a.ctx()).unwrap_err();
    assert!(matches!(e, Error::SafetyGuard(_)), "{e}");
    assert_eq!(fs::read(outside.join("precious.dat")).unwrap(), b"outside the instance");

    // …and a restore never writes through such a link
    fs::remove_file(root.join("saves")).unwrap();
    fs::rename(w.tmp.path().join("saves-moved"), root.join("saves")).unwrap();
    let b = w.device("laptop");
    let rb = restore(&b, en.instance, "pack");
    write(&root, "saves/World 0/level-extra.dat", b"new world data");
    a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    a.upload(en.instance);
    fs::rename(rb.join("saves"), w.tmp.path().join("b-saves-moved")).unwrap();
    std::os::unix::fs::symlink(&outside, rb.join("saves")).unwrap();
    let r = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx());
    assert!(r.is_err(), "restoring through a symlinked folder must be refused: {r:?}");
    assert!(
        !outside.join("World 0").exists() && fs::read_dir(&outside).unwrap().count() == 1,
        "the link target was not written to"
    );
}

#[test]
fn credentials_never_enter_a_snapshot_even_when_the_whole_folder_is_selected() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let root = a.instances_dir().join("pack");
    packsync_core::testkit::fixtures::generate(&root, &spec(2));
    for (rel, body) in [
        ("accounts.json", "{\"token\":\"SECRET-MARKER-ACCOUNTS\"}"),
        ("launcher_accounts.json", "SECRET-MARKER-LAUNCHER"),
        ("config/discord/.env", "TOKEN=SECRET-MARKER-ENV"),
        ("keys/server.pem", "-----BEGIN PRIVATE KEY----- SECRET-MARKER-PEM"),
        (".ssh/id_ed25519", "SECRET-MARKER-SSH"),
        ("prismlauncher.cfg", "FlameKeyOverride=SECRET-MARKER-CFG"),
    ] {
        write(&root, rel, body.as_bytes());
    }
    // the user tries to sync everything, including unrecognised files
    let rules = "version = 1\nunmatched = \"include\"\n[[rule]]\nid = \"all\"\nmatch = [\"**\"]\naction = \"include\"\nreason = \"everything\"\n";
    let en = a
        .engine
        .enable_sync(
            EnableRequest {
                root: root.clone(),
                launcher: "generic".into(),
                name: None,
                rules_overrides: rules.into(),
                launcher_ref: None,
            },
            &a.ctx(),
        )
        .unwrap();
    a.upload(en.instance);
    let m = a.engine.manifest(en.instance, en.snapshot, &a.ctx()).unwrap();
    for e in &m.entries {
        for needle in ["accounts.json", ".env", ".pem", "id_ed25519", "prismlauncher.cfg", ".ssh"] {
            assert!(!e.p.contains(needle), "{} must not be in the snapshot", e.p);
        }
    }
    // and no byte of the secrets reached the cloud, in any object
    for ent in walkdir::WalkDir::new(&w.remote_dir).into_iter().flatten().filter(|e| e.file_type().is_file()) {
        let b = fs::read(ent.path()).unwrap();
        assert!(!b.windows(13).any(|w| w == b"SECRET-MARKER"), "{} contains a secret", ent.path().display());
    }
    // the preview explains why
    let p = a
        .engine
        .preview(
            &PreviewRequest { root, launcher: "generic".into(), rules_overrides: rules.into(), estimate_upload: false },
            &a.ctx(),
        )
        .unwrap();
    assert!(
        p.excluded.groups.keys().any(|k| k.starts_with("sealed:")),
        "{:?}",
        p.excluded.groups.keys().collect::<Vec<_>>()
    );
}

// The source computer has to be able to hold `CON.txt` and two names that differ only in case, so this
// runs where those can exist; the Windows-like target is simulated by configuration.
#[cfg(unix)]
#[test]
fn names_that_cannot_exist_on_the_target_platform_are_found_before_anything_is_touched() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let root = a.instances_dir().join("pack");
    packsync_core::testkit::fixtures::generate(&root, &spec(6));
    write(&root, "config/CON.txt", b"reserved on windows");
    write(&root, "config/Case.cfg", b"1");
    write(&root, "config/case.cfg", b"2");
    write(&root, "config/ok.cfg", b"ok");
    let en = a.enable(&root);
    a.upload(en.instance);
    // the source computer is warned at snapshot time
    let m = a.engine.manifest(en.instance, en.snapshot, &a.ctx()).unwrap();
    assert!(m.entries.iter().any(|e| e.p == "config/CON.txt"));
    assert!(
        en.issues.iter().any(|i| matches!(i.kind, packsync_core::scan::IssueKind::NonPortableName)),
        "{:?}",
        en.issues
    );
    assert!(en.issues.iter().any(|i| matches!(i.kind, packsync_core::scan::IssueKind::CaseCollision)));

    // a Windows-like, case-insensitive target refuses without writing anything
    let win = w.device_with("win-laptop", |c| {
        c.restore.target_is_windows = true;
        c.restore.fold_override = Some(FoldMode::CaseInsensitive);
    });
    let err = win.engine.restore_new(restore_req(&win, en.instance, "pack"), &win.ctx()).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
    assert!(err.to_string().contains("CON.txt") || err.to_string().contains("overwrite each other"), "{err}");
    let leftovers: Vec<_> = fs::read_dir(win.instances_dir()).unwrap().flatten().collect();
    assert!(leftovers.is_empty(), "no folder was created: {leftovers:?}");
    assert!(win.engine.instances().unwrap().is_empty());

    // the user may opt in to skipping what cannot exist there; the skipped files are reported
    let mut req = restore_req(&win, en.instance, "pack");
    req.skip_incompatible = true;
    let r = win.engine.restore_new(req, &win.ctx()).unwrap();
    assert!(!r.root.join("config/CON.txt").exists());
    assert!(r.root.join("config/ok.cfg").exists());
    assert!(!r.report.skipped.is_empty());
}
