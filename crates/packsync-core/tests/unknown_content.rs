//! Content no rule covers (a mod's own data folder): found, suggested, decided once, and the decision
//! travels with the snapshots so every computer syncs the same.

use std::fs;

use packsync_core::engine::*;
use packsync_core::rules::suggest::RuleDecision;
use packsync_core::testkit::fixtures::FixtureSpec;
use packsync_core::testkit::harness::{TestDevice, TestWorld};

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

/// The game folder as a path prefix from the instance root (`.minecraft/`, or nothing).
fn prefix(info: &packsync_core::testkit::fixtures::FixtureInfo) -> String {
    let g = info.game_dir.strip_prefix(&info.root).unwrap().to_string_lossy().replace('\\', "/");
    if g.is_empty() { g } else { format!("{g}/") }
}

#[test]
fn a_new_mods_data_is_found_decided_once_and_synced_everywhere() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    let id = en.instance;
    a.upload(id);
    let b = w.device("laptop");
    let rb = restore_on(&b, id);

    // a newly installed mod writes its own data folder, and something writes a cache
    let vp = info.game_dir.join("serverutilities");
    fs::create_dir_all(&vp).unwrap();
    fs::write(vp.join("claims.dat"), b"my claims").unwrap();
    let cache = info.game_dir.join("coretweaks");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("classes.bin"), b"cache").unwrap();

    a.engine.snapshot(id, SnapshotOpts::default(), &a.ctx()).unwrap();
    let unknown = a.engine.status(id, false, &a.ctx()).unwrap().unknown;
    let game = prefix(&info);
    let vp_path = format!("{game}serverutilities/");
    let found = unknown.iter().find(|s| s.path == vp_path).unwrap_or_else(|| panic!("{unknown:?}"));
    assert!(found.include && found.sure, "{found:?}");
    assert!(!unknown.iter().any(|s| s.path.contains("coretweaks")), "a known cache is not worth asking about");

    // the person agrees: the next save includes it, and it is no longer asked about
    a.engine.decide_unknown(id, &[RuleDecision { path: vp_path.clone(), include: true }]).unwrap();
    assert!(a.engine.status(id, false, &a.ctx()).unwrap().unknown.iter().all(|s| s.path != vp_path));
    let (_, rec) = a.engine.sync_now(id, &a.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");

    // the laptop catches up: the files and the rule arrive
    let (_, rb_rec) = b.engine.sync_now(id, &b.ctx()).unwrap();
    assert!(matches!(rb_rec, ReconcileReport::FastForwarded { .. }), "{rb_rec:?}");
    let on_b = rb.root.join(&game).join("serverutilities/claims.dat");
    assert_eq!(fs::read(&on_b).unwrap(), b"my claims");
    assert!(!rb.root.join(&game).join("coretweaks").exists());
    // and the laptop's own changes there are synced back without asking
    fs::write(&on_b, b"more claims").unwrap();
    b.engine.sync_now(id, &b.ctx()).unwrap();
    a.engine.sync_now(id, &a.ctx()).unwrap();
    assert_eq!(fs::read(vp.join("claims.dat")).unwrap(), b"more claims");
    assert!(b.engine.status(id, false, &b.ctx()).unwrap().unknown.iter().all(|s| s.path != vp_path));
}

#[test]
fn answers_given_before_the_first_sync_are_part_of_the_first_snapshot() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let root = a.instances_dir().join("fresh");
    let info = packsync_core::testkit::fixtures::generate(&root, &FixtureSpec::tiny());
    let game = prefix(&info);
    fs::create_dir_all(info.game_dir.join("serverutilities")).unwrap();
    fs::write(info.game_dir.join("serverutilities/claims.dat"), b"claims").unwrap();

    let preview = a
        .engine
        .preview(
            &PreviewRequest {
                root: root.clone(),
                launcher: "generic".into(),
                rules_overrides: String::new(),
                estimate_upload: false,
            },
            &a.ctx(),
        )
        .unwrap();
    let s = preview.suggestions.iter().find(|s| s.path == format!("{game}serverutilities/")).expect("suggested");
    assert!(s.include && s.sure);

    let en = a
        .engine
        .enable_sync(
            EnableRequest {
                root: root.clone(),
                launcher: "generic".into(),
                name: None,
                rules_overrides: String::new(),
                decisions: vec![RuleDecision { path: s.path.clone(), include: true }],
                launcher_ref: None,
            },
            &a.ctx(),
        )
        .unwrap();
    let files = a.engine.history(en.instance, false, &a.ctx()).unwrap();
    assert_eq!(files.len(), 1);
    let m = a.engine.manifest(en.instance, en.snapshot, &a.ctx()).unwrap();
    assert!(m.entries.iter().any(|e| e.p.ends_with("serverutilities/claims.dat")), "included from the start");
}

/// A modpack update on one computer (a mod updated to a new jar, one added, one removed, a config
/// changed) arrives on the other exactly, and the catch-up says what changed in those terms.
#[test]
fn a_modpack_update_carries_over_and_is_described() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    let id = en.instance;
    a.upload(id);
    let b = w.device("laptop");
    let rb = restore_on(&b, id);
    let mods = info.game_dir.join("mods");
    let mut jars: Vec<_> = fs::read_dir(&mods).unwrap().map(|e| e.unwrap().path()).collect();
    jars.sort();
    assert!(jars.len() >= 2, "the fixture has mods: {jars:?}");

    // update: jei 1.0 → jei 1.1 (another file name), add a mod, remove one, change a config
    fs::write(mods.join("jei-1.20.1-15.0.0.jar"), b"old jei").unwrap();
    a.engine.sync_now(id, &a.ctx()).unwrap();
    b.engine.sync_now(id, &b.ctx()).unwrap();
    fs::remove_file(mods.join("jei-1.20.1-15.0.0.jar")).unwrap();
    fs::write(mods.join("jei-1.20.1-15.2.0.jar"), b"new jei").unwrap();
    fs::write(mods.join("create-0.5.1.jar"), b"create").unwrap();
    fs::remove_file(&jars[0]).unwrap();
    fs::write(&info.configs[0], b"changed=1\n").unwrap();
    a.engine.sync_now(id, &a.ctx()).unwrap();

    let (_, rec) = b.engine.sync_now(id, &b.ctx()).unwrap();
    let ReconcileReport::FastForwarded { changes, .. } = rec else { panic!("{rec:?}") };
    assert_eq!(changes.mods_updated, vec!["jei-1.20.1-15.2.0.jar".to_string()], "{changes:?}");
    assert_eq!(changes.mods_added, vec!["create-0.5.1.jar".to_string()]);
    assert_eq!(changes.mods_removed, vec![jars[0].file_name().unwrap().to_string_lossy().to_string()]);
    assert!(changes.config_files >= 1);

    // the laptop's mods folder is now exactly the desktop's
    let names = |d: &std::path::Path| {
        // native libraries are never carried between computers (another OS's binaries)
        let mut v: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into())
            .filter(|n: &String| n.ends_with(".jar"))
            .collect();
        v.sort();
        v
    };
    let rel = info.game_dir.strip_prefix(&info.root).unwrap();
    assert_eq!(names(&rb.root.join(rel).join("mods")), names(&mods));
}

/// Found by review: older versions left the enable-time rules in `pending_rules` after the first
/// snapshot. Reading them as extra rules duplicated every rule id and broke every save and sync.
#[test]
fn an_instance_enabled_by_an_older_version_still_saves_and_syncs() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    let id = en.instance;
    let full = a.engine.manifest(id, en.snapshot, &a.ctx()).unwrap().rules.text;
    a.engine.db().instance_update(id, |r| r.pending_rules = Some(full.clone())).unwrap();
    fs::write(&info.configs[0], b"edited=1\n").unwrap();
    let (snap, rec) = a.engine.sync_now(id, &a.ctx()).unwrap();
    assert!(snap.is_some(), "saved");
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "{rec:?}");
}

/// Found by review: answering "sync this folder" while another computer's newer version waits in the
/// cloud made the newly included files look like edits of this computer — two versions to choose
/// between, although nothing was edited. The computer now catches up first, then adds the folder.
#[test]
fn deciding_while_behind_the_cloud_catches_up_instead_of_conflicting() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, info, en) = a.new_instance("pack", &FixtureSpec::tiny());
    let id = en.instance;
    a.upload(id);
    let b = w.device("laptop");
    let rb = restore_on(&b, id);
    // the desktop has an unknown folder; meanwhile the laptop plays and uploads
    let su = info.game_dir.join("serverutilities");
    fs::create_dir_all(&su).unwrap();
    fs::write(su.join("claims.dat"), b"claims").unwrap();
    a.engine.snapshot(id, SnapshotOpts::default(), &a.ctx()).unwrap();
    let rel = info.configs[1].strip_prefix(&info.root).unwrap();
    fs::write(rb.root.join(rel), b"laptop=1\n").unwrap();
    b.engine.sync_now(id, &b.ctx()).unwrap();

    let game = prefix(&info);
    a.engine.decide_unknown(id, &[RuleDecision { path: format!("{game}serverutilities/"), include: true }]).unwrap();
    let (_, rec) = a.engine.sync_now(id, &a.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::LocalAhead { uploaded: true, .. }), "saved on top of the laptop's: {rec:?}");
    assert_eq!(fs::read(&info.configs[1]).unwrap(), b"laptop=1\n", "the laptop's change arrived");
    let (_, on_b) = b.engine.sync_now(id, &b.ctx()).unwrap();
    assert!(matches!(on_b, ReconcileReport::FastForwarded { .. }), "{on_b:?}");
    assert_eq!(fs::read(rb.root.join(&game).join("serverutilities/claims.dat")).unwrap(), b"claims");
}
