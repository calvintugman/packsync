//! The real sync engine on top of the real Drive adapter (against the mock server): the journeys that
//! the engine's own tests run on a local directory, now through `DriveStorage` — folder resolution,
//! resumable uploads, tags, listings, trash. Two "computers", one Drive.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::*;
use packsync_core::engine::*;
use packsync_core::gc::GcParams;
use packsync_core::hash::Hash;
use packsync_core::manifest::{SessionOutcome, SnapshotKind};
use packsync_core::progress::OpCtx;
use packsync_core::storage::{PutOptions, PutSource, Storage, TransferStateStore};
use packsync_core::testkit::fixtures::{self, FixtureSpec};
use packsync_core::testkit::harness::is_synced_path;
use packsync_drive::mock::{MockConfig, MockDrive};
use packsync_drive::{DriveSpace, DriveStorage, StatsHandle};

struct Computer {
    engine: Arc<Engine>,
    home: PathBuf,
    drive: Arc<DriveStorage>,
}

fn computer(mock: &MockDrive, home: &Path, name: &str) -> Computer {
    fs::create_dir_all(home).unwrap();
    let mut cfg = EngineConfig::new(home.join("state"));
    cfg.device_name = Some(name.into());
    cfg.snapshot.pack_target = 256 * 1024; // several packs, so uploads are multi-object
    cfg.upload_concurrency = 3;
    let engine = Engine::open(cfg).unwrap();
    let stats = StatsHandle::default();
    let (auth, _creds) = authenticator(mock, DriveSpace::VisibleFolder, &stats);
    let mut dcfg = drive_config(mock, DriveSpace::VisibleFolder, &stats);
    dcfg.multipart_threshold = 64 * 1024; // packs go through resumable sessions persisted in the engine's database
    let state: Arc<dyn TransferStateStore> = engine.db().clone();
    let drive = Arc::new(DriveStorage::new(dcfg, auth, state).unwrap());
    engine.set_storage(Some(drive.clone())).unwrap();
    Computer { engine, home: home.to_path_buf(), drive }
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

fn restore(
    c: &Computer,
    inst: packsync_core::ids::InstanceId,
    folder: &str,
    snapshot: Option<packsync_core::ids::SnapshotId>,
) -> PathBuf {
    let parent = c.home.join("instances");
    fs::create_dir_all(&parent).unwrap();
    c.engine
        .restore_new(
            RestoreNewRequest {
                instance: inst,
                snapshot,
                parent_dir: parent,
                folder_name: folder.into(),
                launcher: "generic".into(),
                launcher_ref: None,
                local: Default::default(),
                skip_incompatible: false,
            },
            &OpCtx::detached(),
        )
        .unwrap()
        .root
}

fn spec() -> FixtureSpec {
    let mut s = FixtureSpec::tiny();
    s.mods = 5;
    s.mod_size = (80_000, 200_000);
    s.regions_per_world = 3;
    s
}

#[test]
fn the_sync_journeys_work_through_the_drive_adapter() {
    let mock = MockDrive::start(MockConfig::default()).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let a = computer(&mock, &tmp.path().join("desktop"), "desktop");
    let b = computer(&mock, &tmp.path().join("laptop"), "laptop");
    let ctx = OpCtx::detached();

    // desktop: first snapshot and upload
    let root_a = a.home.join("instances").join("pack");
    let info = fixtures::generate(&root_a, &spec());
    let en = a
        .engine
        .enable_sync(
            EnableRequest {
                root: root_a.clone(),
                launcher: "generic".into(),
                name: None,
                rules_overrides: String::new(),
                decisions: vec![],
                launcher_ref: None,
            },
            &ctx,
        )
        .unwrap();
    let inst = en.instance;
    let up = a.engine.upload_pending(inst, true, &ctx).unwrap();
    assert_eq!(up.remaining, 0, "{up:?}");
    assert!(
        mock.requests().iter().any(|r| r.endpoint == packsync_drive::mock::Endpoint::ResumableChunk),
        "packs went through resumable sessions"
    );
    let status = a.engine.status(inst, true, &ctx).unwrap();
    assert!(matches!(status.phase, SyncPhase::ReadyOnOtherDevices | SyncPhase::CloudPublished), "{:?}", status.phase);

    // laptop: discovers it and restores byte for byte
    let cloud = b.engine.cloud_instances(&ctx).unwrap();
    assert_eq!(cloud.len(), 1);
    assert_eq!(cloud[0].id, inst);
    assert_eq!(cloud[0].heads.len(), 1);
    let root_b = restore(&b, inst, "pack", None);
    assert_eq!(tree(&root_b), tree(&root_a));
    let v = a.engine.verify_remote(inst, None, true, &ctx).unwrap();
    assert!(v.missing_packs.is_empty() && v.damaged_blobs.is_empty(), "{v:?}");

    // a play session on the desktop, the laptop catches up with only the changed blocks
    fixtures::edit_region(&info.regions[0], 3, 5);
    fs::write(root_a.join("config/session.toml"), b"level = 7\n").unwrap();
    a.engine.snapshot(inst, SnapshotOpts::default(), &ctx).unwrap();
    a.engine.upload_pending(inst, true, &ctx).unwrap();
    let rec = b.engine.reconcile(inst, ReconcileOpts::default(), &ctx).unwrap();
    assert!(matches!(rec, ReconcileReport::FastForwarded { .. }), "{rec:?}");
    assert_eq!(tree(&root_b), tree(&root_a));

    // both play while apart: nothing is merged, the user chooses, both versions survive
    fs::write(root_a.join("config/mine.toml"), b"from = 'desktop'\n").unwrap();
    fs::write(root_b.join("config/mine.toml"), b"from = 'laptop'\n").unwrap();
    a.engine.snapshot(inst, SnapshotOpts::default(), &ctx).unwrap();
    a.engine.upload_pending(inst, true, &ctx).unwrap();
    let (_, rec) = b.engine.sync_now(inst, &ctx).unwrap();
    assert!(matches!(rec, ReconcileReport::Conflict(_)), "{rec:?}");
    assert_eq!(
        fs::read_to_string(root_b.join("config/mine.toml")).unwrap(),
        "from = 'laptop'\n",
        "nothing was overwritten"
    );
    let before = b.engine.history(inst, true, &ctx).unwrap().len();
    b.engine.resolve_conflict(inst, Resolution::KeepCloud { head: None }, &ctx).unwrap();
    assert_eq!(tree(&root_b), tree(&root_a));
    assert!(b.engine.history(inst, true, &ctx).unwrap().len() > before, "the laptop's version was preserved first");

    // a crashed session on the desktop is kept apart in the cloud and is never a head
    let s = a
        .engine
        .begin_session(
            inst,
            BeginSession { owner: packsync_core::db::SessionOwner::App, token: "t1".into(), offline: false },
            &ctx,
        )
        .unwrap();
    fs::write(root_a.join("config/half-written.toml"), b"oops\n").unwrap();
    fs::create_dir_all(root_a.join("crash-reports")).unwrap();
    fs::write(root_a.join("crash-reports/crash-2026-01-01_00.00.00-client.txt"), b"java.lang.OutOfMemoryError")
        .unwrap();
    let end =
        a.engine.end_session(s.session, ExitInfo { exit_code: Some(1), clean_signal: false }, true, &ctx).unwrap();
    assert_eq!((end.outcome, end.kind), (SessionOutcome::Crashed, Some(SnapshotKind::Crash)));
    assert!(end.uploaded, "{end:?}");
    assert!(
        matches!(b.engine.reconcile(inst, ReconcileOpts::default(), &ctx).unwrap(), ReconcileReport::UpToDate { .. }),
        "the crash save is not a head"
    );
    let hist = b.engine.history(inst, true, &ctx).unwrap();
    assert!(hist.iter().any(|h| h.kind == SnapshotKind::Crash && !h.is_cloud_head));
}

#[test]
fn cleanup_works_on_drive_objects_tags_ages_and_trash() {
    let mock = MockDrive::start(MockConfig::default()).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let a = computer(&mock, &tmp.path().join("desktop"), "desktop");
    let ctx = OpCtx::detached();
    let root = a.home.join("instances").join("pack");
    fixtures::generate(&root, &spec());
    let inst = a
        .engine
        .enable_sync(
            EnableRequest {
                root: root.clone(),
                launcher: "generic".into(),
                name: None,
                rules_overrides: String::new(),
                decisions: vec![],
                launcher_ref: None,
            },
            &ctx,
        )
        .unwrap()
        .instance;
    a.engine.upload_pending(inst, true, &ctx).unwrap();
    // an abandoned upload's pack, and a file somebody else (not PackSync) put next to the packs
    let junk = b"left behind by an abandoned upload".to_vec();
    let key = format!("packs/{}.pack", Hash::of(&junk).to_hex());
    a.drive
        .put(
            &key,
            PutSource::Bytes(&junk),
            &PutOptions::create_only(None).with_tag("ps", "1").with_tag("kind", "pack"),
            &ctx,
        )
        .unwrap();
    let packs_folder =
        mock.objects().iter().find(|o| o.is_folder && o.name == "packs").map(|o| o.id.clone()).expect("packs folder");
    mock.seed_folder(Some(&packs_folder), "not-ours", None, &[]); // untagged: never listed, never touched
    mock.age_files(100);
    let plan = a.engine.gc_plan(&GcParams::default(), &ctx).unwrap();
    let keys: Vec<_> = plan.delete.iter().map(|d| d.key.clone()).collect();
    assert_eq!(keys, vec![key.clone()], "only the orphan, recognised as PackSync's by its tag: {plan:#?}");
    let rep = a.engine.gc_run(&plan.id, &GcParams::default(), &ctx).unwrap();
    assert_eq!(rep.deleted_packs, 1, "{rep:?}");
    assert!(a.drive.stat(&key, &ctx).unwrap().is_none());
    // everything the instance needs is intact; a second computer restores it
    let v = a.engine.verify_remote(inst, None, true, &ctx).unwrap();
    assert!(v.missing_packs.is_empty() && v.damaged_blobs.is_empty(), "{v:?}");
    let b = computer(&mock, &tmp.path().join("laptop"), "laptop");
    let rb = restore(&b, inst, "pack", None);
    assert_eq!(tree(&rb), tree(&root));
}
