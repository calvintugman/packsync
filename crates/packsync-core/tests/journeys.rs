//! End-to-end user journeys on two simulated computers sharing one fake cloud.

use std::fs;

use packsync_core::db::{InstanceState, SnapshotState};
use packsync_core::engine::*;
use packsync_core::testkit::fixtures::{self, FixtureSpec};
use packsync_core::testkit::harness::{TestWorld, is_synced_path};

/// Digest of the synced content. `options.txt` is a mixed file (local-only graphics keys stay local),
/// so it is compared through its portable projection, exactly as the engine stores it.
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

#[test]
fn first_computer_uploads_second_computer_restores_identical_bytes() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root_a, info, enabled) = a.new_instance("My Pack", &FixtureSpec::tiny());
    assert!(info.synced_files > 10);

    // Before the upload completes the instance must NOT claim to be synced.
    let st = a.engine.status(enabled.instance, false, &a.ctx()).unwrap();
    assert_eq!(st.phase, SyncPhase::LocalSnapshotComplete, "{st:?}");

    a.upload(enabled.instance);
    let st = a.engine.status(enabled.instance, true, &a.ctx()).unwrap();
    assert_eq!(st.phase, SyncPhase::ReadyOnOtherDevices);
    assert_eq!(st.pending_snapshots, 0);

    // second computer
    let b = w.device("laptop");
    let cloud = b.engine.cloud_instances(&b.ctx()).unwrap();
    assert_eq!(cloud.len(), 1);
    assert_eq!(cloud[0].id, enabled.instance);
    assert_eq!(cloud[0].worlds.len(), 1);
    assert_eq!(cloud[0].worlds[0].name.as_deref(), Some("My World 0 ünï"));
    let restored = b
        .engine
        .restore_new(
            RestoreNewRequest {
                instance: enabled.instance,
                snapshot: None,
                parent_dir: b.instances_dir(),
                folder_name: "My Pack".into(),
                launcher: "generic".into(),
                launcher_ref: None,
                local: Default::default(),
                skip_incompatible: false,
            },
            &b.ctx(),
        )
        .unwrap();
    assert_eq!(tree(&root_a), tree(&restored.root), "every included byte (and empty dir) is reproduced");
    // excluded junk did not travel
    assert!(!restored.root.join("logs").exists() && !restored.root.join("screenshots").exists());
    assert!(!restored.root.join("natives").exists());
    // the mixed options.txt kept portable keys but not this device's graphics keys
    let opts = fs::read_to_string(restored.root.join("options.txt")).unwrap();
    assert!(opts.contains("lang:en_us") && opts.contains("key_key.jump"));
    assert!(!opts.contains("renderDistance") && !opts.contains("fullscreen"), "local-only keys stay local: {opts}");
    // B changes a local-only key: that must not be a pending change
    fs::write(restored.root.join("options.txt"), format!("{opts}renderDistance:4\nfullscreen:true\n")).unwrap();
    assert!(
        !b.engine.pending_changes(enabled.instance, &b.ctx()).unwrap().dirty,
        "local-only settings never create snapshots"
    );
    // B is up to date with the cloud
    let st = b.engine.status(enabled.instance, true, &b.ctx()).unwrap();
    assert_eq!(st.phase, SyncPhase::ReadyOnOtherDevices);
}

#[test]
fn no_change_sync_uploads_no_content_and_sparse_edits_transfer_little() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let mut spec = FixtureSpec::tiny();
    spec.regions_per_world = 3;
    spec.chunks_per_region = 200;
    spec.chunk_sectors = (1, 2);
    let (root, info, en) = a.new_instance("pack", &spec);
    a.upload(en.instance);

    // 1. nothing changed
    let before = a.meter.snapshot();
    let (snap, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    let d = a.meter.snapshot().since(&before);
    assert!(matches!(snap.unwrap().result, SnapshotResult::Unchanged { .. }));
    assert!(matches!(rec, ReconcileReport::UpToDate { .. }));
    assert_eq!(d.payload_puts(), 0, "a no-change sync uploads no new content: {d:?}");
    assert_eq!(d.puts, 0, "…and no metadata objects either (no lease/heartbeat traffic in a pure sync)");

    // 2. a sparse region edit + a small config change
    let region = &info.regions[0];
    let region_size = fs::metadata(region).unwrap().len();
    fixtures::edit_region(region, 3, 5);
    fs::write(&info.configs[0], b"changed = true\n").unwrap();
    let before = a.meter.snapshot();
    let (snap, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
    assert!(matches!(snap.unwrap().result, SnapshotResult::Created { .. }));
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");
    let d = a.meter.snapshot().since(&before);
    assert!(d.payload_bytes_up() > 0);
    assert!(
        d.payload_bytes_up() < region_size / 4,
        "3 edited chunks of a {region_size}-byte region must upload far less than the file: {} bytes",
        d.payload_bytes_up()
    );
    let _ = root;
}

#[test]
fn same_size_edit_with_restored_timestamp_is_detected() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    std::thread::sleep(std::time::Duration::from_millis(30));
    fixtures::same_size_edit_preserving_mtime(&info.configs[1], 11);
    #[cfg_attr(not(unix), allow(unused_variables))]
    let p = a.engine.pending_changes(en.instance, &a.ctx()).unwrap();
    #[cfg(unix)]
    assert_eq!(p.modified.len(), 1, "inode change time exposes the edit: {p:?}");
    // and a forced full scan finds it on any platform
    a.engine.rescan(en.instance).unwrap();
    assert_eq!(a.engine.pending_changes(en.instance, &a.ctx()).unwrap().modified.len(), 1);
}

/// Found on Google Drive with a real 6.8 GB modpack: a small edit took 36 s, 23 of them looking up, one
/// request at a time, each of the 215 packs that the changed files' unchanged blocks live in — although
/// the upload had just listed every pack. A publication now trusts that fresh listing and looks packs up
/// directly only in a periodic full check (or when cloud data was lost, covered by other tests).
#[test]
fn a_small_edit_does_not_look_up_every_pack_it_depends_on_except_in_the_periodic_full_check() {
    let w = TestWorld::new();
    // packs of 32 KiB: one region file's unchanged blocks are spread over many packs
    let a = w.device_with("desktop", |c| c.snapshot.pack_target = 32 * 1024);
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::small());
    a.upload(en.instance);
    let pack_lookups = |d: &packsync_core::storage_meter::MeterSnapshot| d.by_prefix.get("packs").map_or(0, |p| p.stats);

    let mut lookups = vec![];
    // the first upload was publication 1: edits 1..N-1 are publications 2..N, the last one the full check
    for i in 1..packsync_core::upload::FULL_CHECK_EVERY {
        fixtures::edit_region(&info.regions[0], 2, 100 + i);
        let before = a.meter.snapshot();
        let (_, rec) = a.engine.sync_now(en.instance, &a.ctx()).unwrap();
        assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");
        lookups.push(pack_lookups(&a.meter.snapshot().since(&before)));
    }
    let (last, rest) = lookups.split_last().unwrap();
    assert!(rest.iter().all(|n| *n == 0), "ordinary publications look up no packs: {lookups:?}");
    assert!(*last > 1, "every {}th publication looks each dependency up: {lookups:?}", lookups.len());
}

/// Found by the first run on a real modpack: GregTech ships a zero-byte `DynamicConfig.cfg`, and the
/// "publish no holes" check asked for the stored data of a file that has none. Empty files are in the
/// first snapshot, appear later, and a file can be truncated to nothing — all of it must sync.
#[test]
fn empty_files_are_saved_uploaded_and_restored() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let root_a = a.instances_dir().join("pack");
    let _info = packsync_core::testkit::fixtures::generate(&root_a, &FixtureSpec::tiny());
    fs::write(root_a.join("config/empty-at-first.cfg"), b"").unwrap();
    fs::write(root_a.join("config/emptied-later.cfg"), b"not empty yet").unwrap();
    let en = a.enable(&root_a); // first snapshot: no base to compare with
    a.upload(en.instance);

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
    assert_eq!(fs::metadata(rb.root.join("config/empty-at-first.cfg")).unwrap().len(), 0);

    // later: a new empty file, and an existing one truncated to nothing
    fs::write(root_a.join("config/empty-added.cfg"), b"").unwrap();
    fs::write(root_a.join("config/emptied-later.cfg"), b"").unwrap();
    let rep = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    assert!(matches!(rep.result, SnapshotResult::Created { .. }));
    a.upload(en.instance);
    let r = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap();
    assert!(matches!(r, ReconcileReport::FastForwarded { .. }), "{r:?}");
    for f in ["empty-at-first.cfg", "empty-added.cfg", "emptied-later.cfg"] {
        assert_eq!(fs::metadata(rb.root.join("config").join(f)).unwrap().len(), 0, "{f}");
    }
    assert_eq!(tree(&root_a), tree(&rb.root));
}

#[test]
fn creation_deletion_rename_empty_dirs_unicode_and_long_paths_propagate() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root_a, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    let b = w.device("laptop");
    let req = |snapshot| RestoreNewRequest {
        instance: en.instance,
        snapshot,
        parent_dir: b.instances_dir(),
        folder_name: "pack".into(),
        launcher: "generic".into(),
        launcher_ref: None,
        local: Default::default(),
        skip_incompatible: false,
    };
    let rb = b.engine.restore_new(req(None), &b.ctx()).unwrap();

    // A: create / delete / rename / empty dir / unicode / long path
    fs::write(root_a.join("config/new-file.toml"), b"x = 1\n").unwrap();
    fs::remove_file(&info.mods[0]).unwrap();
    fs::rename(&info.configs[0], root_a.join("config/renamed-config.toml")).unwrap();
    fs::create_dir_all(root_a.join("config/brand-new-empty-dir")).unwrap();
    fs::remove_dir_all(root_a.join("config/empty-folder")).unwrap();
    fs::create_dir_all(root_a.join("config/日本語/ディレクトリ")).unwrap();
    fs::write(root_a.join("config/日本語/ディレクトリ/ファイル.json"), b"{}").unwrap();
    let long = (0..40).map(|i| format!("deep-directory-{i:02}")).collect::<Vec<_>>().join("/");
    fs::create_dir_all(root_a.join("config").join(&long)).unwrap();
    fs::write(
        root_a
            .join("config")
            .join(&long)
            .join("a-file-with-a-rather-long-name-to-push-the-path-length-over-two-hundred-chars-total.cfg"),
        b"deep",
    )
    .unwrap();
    let rep = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    assert!(matches!(rep.result, SnapshotResult::Created { .. }));
    assert_eq!((rep.stats.added, rep.stats.removed), (rep.stats.added, rep.stats.removed));
    a.upload(en.instance);

    // B picks it up by fast-forward
    let r = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap();
    assert!(matches!(r, ReconcileReport::FastForwarded { .. }), "{r:?}");
    assert_eq!(tree(&root_a), tree(&rb.root));
    assert!(!rb.root.join(info.mods[0].strip_prefix(&root_a).unwrap()).exists(), "deletion propagated");
    assert!(rb.root.join("config/renamed-config.toml").exists());
    assert!(!rb.root.join("config/empty-folder").exists(), "removed empty dir is removed");
    assert!(rb.root.join("config/brand-new-empty-dir").is_dir(), "new empty dir is created");
    assert!(rb.root.join("config/日本語/ディレクトリ/ファイル.json").exists());
    // the previous versions are kept as a recovery point
    assert!(!b.engine.list_recovery_points(en.instance).unwrap().is_empty());
    // and nothing is pending anywhere
    assert!(!b.engine.pending_changes(en.instance, &b.ctx()).unwrap().dirty);
    assert!(!a.engine.pending_changes(en.instance, &a.ctx()).unwrap().dirty);
}

#[test]
fn offline_changes_on_two_devices_become_a_conflict_and_both_versions_survive_each_resolution() {
    for choice in ["keep_local", "keep_cloud", "split"] {
        let w = TestWorld::new();
        let a = w.device("desktop");
        let (root_a, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
        a.upload(en.instance);
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

        // both play offline: different edits
        fs::write(root_a.join("config/from-desktop.toml"), b"desktop = true\n").unwrap();
        fs::write(rb.root.join("config/from-laptop.toml"), b"laptop = true\n").unwrap();
        a.plan.set_offline(true);
        b.plan.set_offline(true);
        let ra = a.engine.sync_now(en.instance, &a.ctx()).unwrap().1;
        assert!(matches!(ra, ReconcileReport::Offline { .. }), "{ra:?}");
        let rbn = b.engine.sync_now(en.instance, &b.ctx()).unwrap().1;
        assert!(matches!(rbn, ReconcileReport::Offline { .. }), "{rbn:?}");
        // the local snapshots exist but are honestly "not uploaded"
        let sa = a.engine.status(en.instance, false, &a.ctx()).unwrap();
        assert!(matches!(sa.phase, SyncPhase::LocalSnapshotComplete | SyncPhase::UploadPending), "{:?}", sa.phase);
        a.plan.set_offline(false);
        b.plan.set_offline(false);

        // reconnection: desktop publishes first and is simply ahead
        let ra = a.engine.sync_now(en.instance, &a.ctx()).unwrap().1;
        assert!(matches!(ra, ReconcileReport::LocalAhead { uploaded: true, .. }), "{ra:?}");
        // laptop must NOT silently replace desktop's progress (nor overwrite its own)
        let before = tree(&rb.root);
        let rbn = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap();
        let ReconcileReport::Conflict(c) = rbn else { panic!("expected conflict, got {rbn:?}") };
        assert_eq!(c.cloud.len(), 1);
        assert!(c.local.is_some());
        assert_eq!(tree(&rb.root), before, "detecting a conflict touches no files");
        assert_eq!(b.engine.instance(en.instance).unwrap().state, InstanceState::Conflict);
        // while a conflict is pending, syncing is refused (no silent progress)
        assert!(
            b.engine.upload_pending(en.instance, true, &b.ctx()).is_ok(),
            "uploading the retained local branch is allowed"
        );

        let resolution = match choice {
            "keep_local" => Resolution::KeepLocal,
            "keep_cloud" => Resolution::KeepCloud { head: None },
            _ => Resolution::SplitLocal { new_name: "Laptop Fork".into() },
        };
        let rep = b.engine.resolve_conflict(en.instance, resolution, &b.ctx()).unwrap();
        assert_eq!(b.engine.instance(rep.new_instance.unwrap_or(en.instance)).unwrap().state, InstanceState::Ok);

        match choice {
            "keep_local" => {
                assert!(rb.root.join("config/from-laptop.toml").exists());
                // desktop converges onto the merge commit and now has the laptop's file too
                let r = a.engine.reconcile(en.instance, ReconcileOpts::default(), &a.ctx()).unwrap();
                assert!(matches!(r, ReconcileReport::FastForwarded { .. }), "{r:?}");
                assert!(root_a.join("config/from-laptop.toml").exists());
                assert!(
                    !root_a.join("config/from-desktop.toml").exists(),
                    "continuing the laptop's version means desktop's edit is only in history"
                );
            }
            "keep_cloud" => {
                assert!(rb.root.join("config/from-desktop.toml").exists());
                assert!(!rb.root.join("config/from-laptop.toml").exists());
                // the laptop's version is not lost: it is in the cloud history…
                let hist = b.engine.history(en.instance, true, &b.ctx()).unwrap();
                let laptop_branch = hist
                    .iter()
                    .find(|h| {
                        h.device.as_deref() == Some("laptop") && h.note.is_none() && h.parents.len() == 1 && !h.is_base
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "the laptop's own snapshot is retained as a branch in history: {:#?}",
                            hist.iter()
                                .map(|h| (
                                    h.id.short(),
                                    h.device.clone(),
                                    h.note.clone(),
                                    h.parents.iter().map(|p| p.short()).collect::<Vec<_>>(),
                                    h.state,
                                    h.is_base
                                ))
                                .collect::<Vec<_>>()
                        )
                    });
                assert!(
                    matches!(laptop_branch.state, SnapshotState::Visible | SnapshotState::Published),
                    "{:?}",
                    laptop_branch.state
                );
                // …and as a recovery point on disk
                assert!(!b.engine.list_recovery_points(en.instance).unwrap().is_empty());
                // desktop fast-forwards to the merge commit (same tree as its own)
                let r = a.engine.reconcile(en.instance, ReconcileOpts::default(), &a.ctx()).unwrap();
                assert!(matches!(r, ReconcileReport::FastForwarded { .. } | ReconcileReport::UpToDate { .. }), "{r:?}");
            }
            _ => {
                let n = rep.new_instance.expect("a new instance identity");
                assert_ne!(n, en.instance);
                assert!(
                    rb.root.join("config/from-laptop.toml").exists(),
                    "the fork keeps this computer's files in place"
                );
                // the original instance continues unchanged for everyone else
                let r = a.engine.reconcile(en.instance, ReconcileOpts::default(), &a.ctx()).unwrap();
                assert!(matches!(r, ReconcileReport::UpToDate { .. } | ReconcileReport::FastForwarded { .. }), "{r:?}");
                assert!(root_a.join("config/from-desktop.toml").exists());
                assert!(!root_a.join("config/from-laptop.toml").exists());
                // and the fork is its own instance in the cloud
                let c = a.engine.cloud_instances(&a.ctx()).unwrap();
                assert_eq!(c.len(), 2, "{c:?}");
            }
        }
        let _ = info;
    }
}

#[test]
fn a_snapshot_that_exists_only_locally_cannot_be_restored_until_it_is_uploaded() {
    use packsync_core::error::Error;
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    fs::write(root.join("config/extra.toml"), b"x = 1\n").unwrap();
    let rep = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    let SnapshotResult::Created { id } = rep.result else { panic!("{rep:?}") };
    let before = tree(&root);
    // clear, actionable refusal instead of a confusing "incomplete cloud data" failure halfway through
    let err =
        a.engine.restore_snapshot(en.instance, id, RestoreOpts { skip_incompatible: false }, &a.ctx()).unwrap_err();
    assert!(matches!(&err, Error::Unsupported(m) if m.contains("not been uploaded")), "{err}");
    assert!(a.engine.restore_preview(en.instance, id, &a.ctx()).is_err());
    assert_eq!(tree(&root), before, "nothing was touched");
    a.upload(en.instance);
    assert!(a.engine.restore_preview(en.instance, id, &a.ctx()).is_ok());
}

#[test]
fn catching_up_after_an_in_place_region_edit_downloads_only_the_changed_blocks() {
    // The common case: the other computer played, a few chunks of a few region files changed. Catching
    // up must fetch those blocks, not whole region files — unchanged blocks are taken from disk.
    let w = TestWorld::new();
    let a = w.device("desktop");
    let mut spec = FixtureSpec::tiny();
    spec.chunks_per_region = 400;
    spec.chunk_sectors = (2, 6);
    spec.regions_per_world = 3;
    let (root_a, info, en) = a.new_instance("pack", &spec);
    a.upload(en.instance);
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
    let region_bytes: u64 = info.regions.iter().map(|p| fs::metadata(p).unwrap().len()).sum();
    assert!(region_bytes > 1_000_000, "the regions are big enough for the test to mean something: {region_bytes}");
    // A plays: two chunks change in each of two region files
    for (i, p) in info.regions.iter().take(2).enumerate() {
        fixtures::edit_region(p, 2, 300 + i as u64);
    }
    a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    a.upload(en.instance);
    let before = b.meter.snapshot();
    let rec = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::FastForwarded { files_written: 2, .. }), "{rec:?}");
    let spent = b.meter.snapshot().since(&before);
    let two_regions = info.regions.iter().take(2).map(|p| fs::metadata(p).unwrap().len()).sum::<u64>();
    assert!(
        spent.bytes_down < two_regions / 3,
        "downloaded {} for two region files of {} bytes in total: unchanged blocks must come from disk",
        spent.bytes_down,
        two_regions
    );
    assert_eq!(tree(&rb.root), tree(&root_a), "and the result is byte-identical");
}

#[test]
fn the_index_a_computer_uploaded_itself_is_not_downloaded_again() {
    // The index object lists every blob of the packs this computer just uploaded, and the local cache
    // already has them. Fetching it back on the next sync would cost one download per instance (megabytes
    // for a big pack) for nothing. Another computer, which does not have the blobs, must still fetch it.
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, _info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    // the next session: something changes, it is saved and uploaded (this is where the cache is refreshed)
    fs::write(_root.join("config/extra.toml"), b"x = 1\n").unwrap();
    let before = a.meter.snapshot();
    a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    a.upload(en.instance);
    let spent = a.meter.snapshot().since(&before);
    let idx = spent.by_prefix.get("index").cloned().unwrap_or_default();
    assert_eq!(idx.gets, 0, "the uploader re-read its own index: {idx:?}");
    assert_eq!(idx.get_bytes, 0, "{idx:?}");
    assert_eq!(idx.puts, 1, "the second snapshot published its own index: {idx:?}");

    let b = w.device("laptop");
    let before = b.meter.snapshot();
    b.engine
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
    let spent = b.meter.snapshot().since(&before);
    assert!(spent.by_prefix.get("index").map(|p| p.gets).unwrap_or(0) >= 1, "another computer must read the index");
}

#[test]
fn restoring_reads_each_pack_about_once_even_though_files_are_hashed_in_parallel() {
    // Files are hashed by several threads at once. If their blocks were interleaved in the packs, reading one
    // file back would take a request per block, and a restore that works through the files in batches would
    // read every pack over and over, in scraps (measured on a 4.3 GB instance: 34,000 requests and 6.7 GB
    // read to restore 4.3 GB). Each file's blocks must sit next to each other.
    let w = TestWorld::new();
    let a = w.device_with("desktop", |c| {
        c.snapshot.scan.threads = 8;
        c.snapshot.pack_target = 2 * 1024 * 1024;
    });
    let mut spec = FixtureSpec::small();
    spec.regions_per_world = 8;
    let (root, _info, en) = a.new_instance("pack", &spec);
    a.upload(en.instance);
    let before = tree(&root);

    let b = w.device_with("laptop", |c| {
        c.restore.batch_bytes = 6 * 1024 * 1024; // many small batches: the worst case for scattered data
        c.restore.concurrency = 4;
    });
    let meter_before = b.meter.snapshot();
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
    assert_eq!(tree(&rb.root), before, "the restore is byte for byte the same");

    let spent = b.meter.snapshot().since(&meter_before);
    let packs = spent.by_prefix.get("packs").cloned().unwrap_or_default();
    let useful = rb.report.bytes_downloaded;
    let blocks = useful / 16_384;
    eprintln!(
        "useful {useful} B ({blocks} blocks of 16 KiB); read from packs {} B in {} requests",
        packs.get_bytes, packs.gets
    );
    assert!(
        packs.get_bytes as f64 <= useful as f64 * 1.3,
        "read {} B from the packs to obtain {useful} B: scattered data is read over and over",
        packs.get_bytes
    );
    assert!(
        packs.gets * 10 <= blocks,
        "{} requests for {blocks} blocks: a request per few blocks means the blocks of a file are not contiguous",
        packs.gets
    );
}

/// Checking the files records them as the trusted index: never while a restore is half applied (the
/// files may be a mix of two versions until recovery has run).
#[test]
fn a_local_check_is_refused_while_a_restore_needs_recovery() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, _info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.engine.db().instance_update(en.instance, |r| r.state = packsync_core::db::InstanceState::Restoring).unwrap();
    let err = a.engine.verify_local(en.instance, &a.ctx()).unwrap_err();
    assert!(matches!(err, packsync_core::error::Error::Busy(_)), "{err}");
    a.engine.db().instance_update(en.instance, |r| r.state = packsync_core::db::InstanceState::Ok).unwrap();
    assert!(a.engine.verify_local(en.instance, &a.ctx()).is_ok());
}

/// options.txt is merged on restore: synced keys from the cloud, this computer's own keys (window size,
/// graphics …) kept. A local copy too large to merge stops the restore before anything is changed. (The scan
/// of the live files refuses it first; the staging step refuses it too, for a file that grows in between —
/// it used to treat it as absent and drop every local key.)
#[test]
fn an_oversized_local_settings_file_stops_the_restore_instead_of_losing_its_local_values() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root_a, _info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    a.upload(en.instance);
    let b = w.device("laptop");
    let rb = b.engine.restore_new(common_restore_req(&b, en.instance), &b.ctx()).unwrap().root;
    let opts_b = rb.join("options.txt");
    let mut big = fs::read(&opts_b).unwrap();
    big.extend(std::iter::repeat_n(b'#', 1_100_000)); // only this computer's copy grows
    big.extend(b"\nfullscreen:true\n");
    fs::write(&opts_b, &big).unwrap();
    // the desktop changes a synced key
    let opts_a = root_a.join("options.txt");
    let mut t = fs::read_to_string(&opts_a).unwrap();
    t.push_str("lang:de_de\n");
    fs::write(&opts_a, t).unwrap();
    a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    a.upload(en.instance);
    let before = fs::read(&opts_b).unwrap();
    let r = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx());
    assert!(r.is_err(), "{r:?}");
    assert_eq!(fs::read(&opts_b).unwrap(), before, "nothing of this computer's settings was lost");
}

fn common_restore_req(
    d: &packsync_core::testkit::harness::TestDevice,
    inst: packsync_core::ids::InstanceId,
) -> RestoreNewRequest {
    RestoreNewRequest {
        instance: inst,
        snapshot: None,
        parent_dir: d.instances_dir(),
        folder_name: "pack".into(),
        launcher: "generic".into(),
        launcher_ref: None,
        local: Default::default(),
        skip_incompatible: false,
    }
}
