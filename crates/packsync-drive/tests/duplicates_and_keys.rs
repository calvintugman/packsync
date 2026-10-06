//! Duplicate names (Drive names are not unique) and key safety.

mod common;

use common::{digest, read_all};
use packsync_core::progress::OpCtx;
use packsync_core::storage::{DeleteMode, PutOptions, PutSource, Storage};
use packsync_drive::DriveSpace;
use packsync_drive::mock::{Endpoint, MockConfig};

fn ctx() -> OpCtx {
    OpCtx::detached()
}

const ROOT: &[(&str, &str)] = &[("ps", "1"), ("psk", "root")];
const OURS: &[(&str, &str)] = &[("ps", "1")];

#[test]
fn duplicates_are_tolerated_reported_and_never_auto_deleted() {
    let env = common::setup();
    let m = &env.mock;
    // Two devices raced on first use: two root folders, each with its own `packs` folder.
    let root_a = m.seed_folder(None, "PackSync", Some(100), ROOT);
    let root_b = m.seed_folder(None, "PackSync", Some(200), ROOT);
    let packs_a = m.seed_folder(Some(&root_a), "packs", Some(110), OURS);
    let packs_b = m.seed_folder(Some(&root_b), "packs", Some(210), OURS);
    let x_new = m.seed_file(&packs_a, "x.pack", b"same bytes", Some(120));
    let x_old = m.seed_file(&packs_b, "x.pack", b"same bytes", Some(115));
    let y = m.seed_file(&packs_b, "y.pack", b"only in b", Some(220));
    // An object PackSync did not create (no ps=1) must be invisible and untouchable.
    let foreign = m.seed_folder(Some(&packs_a), "x.pack", Some(1), &[]);

    let listing = env.drive.list("packs/", &ctx()).unwrap();
    let keys: Vec<_> = listing.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["packs/x.pack", "packs/y.pack"], "union of both duplicate folders");
    assert_eq!(listing[0].duplicates, 1);
    assert_eq!(listing[0].provider_ref.as_deref(), Some(x_old.as_str()), "oldest createdTime is canonical");
    assert_eq!(listing[1].provider_ref.as_deref(), Some(y.as_str()));
    let root = env.drive.list("packs/../", &ctx());
    assert!(root.is_err(), "traversal in a prefix is rejected");

    let st = env.drive.stat("packs/x.pack", &ctx()).unwrap().unwrap();
    assert_eq!(st.provider_ref.as_deref(), Some(x_old.as_str()));
    assert_eq!(st.duplicates, 1);
    assert_eq!(read_all(env.drive.get("packs/x.pack", None, &ctx()).unwrap()).unwrap(), b"same bytes");

    // A fresh instance makes the same choice.
    let other = env.new_instance(|_| {});
    let st2 = other.stat("packs/x.pack", &ctx()).unwrap().unwrap();
    assert_eq!(st2.provider_ref, st.provider_ref);
    assert_eq!(st2.duplicates, 1);

    // Idempotent CreateOnly on a duplicated key creates nothing.
    let same = b"same bytes";
    env.drive
        .put("packs/x.pack", PutSource::Bytes(same), &PutOptions::create_only(Some(digest(same))), &ctx())
        .unwrap();
    // New objects go into the canonical (oldest) folder.
    let z = env.drive.put("packs/z.pack", PutSource::Bytes(b"z"), &PutOptions::create_only(None), &ctx()).unwrap();
    let objs = m.objects();
    let z_obj = objs.iter().find(|o| Some(o.id.as_str()) == z.provider_ref.as_deref()).unwrap();
    assert_eq!(z_obj.parents, vec![packs_a.clone()]);
    let copies = |name: &str| m.objects().iter().filter(|o| o.name == name && !o.is_folder).count();
    assert_eq!(copies("x.pack"), 2, "duplicates are never deleted automatically");
    assert_eq!(m.objects().iter().filter(|o| o.name == "PackSync").count(), 2, "no third root folder");

    // Explicit delete removes every PackSync copy, nothing else.
    env.drive.delete("packs/x.pack", DeleteMode::Permanent, &ctx()).unwrap();
    assert_eq!(copies("x.pack"), 0);
    assert!(m.objects().iter().any(|o| o.id == foreign), "objects without ps=1 are never deleted");
    assert!(!m.objects().iter().any(|o| o.id == x_new || o.id == x_old));
    assert!(env.drive.stat("packs/x.pack", &ctx()).unwrap().is_none());
}

#[test]
fn identical_creation_times_tie_break_on_the_lowest_id() {
    let env = common::setup();
    let m = &env.mock;
    let root = m.seed_folder(None, "PackSync", Some(100), ROOT);
    let dir = m.seed_folder(Some(&root), "d", Some(100), OURS);
    let a = m.seed_file(&dir, "t.bin", b"t", Some(500));
    let b = m.seed_file(&dir, "t.bin", b"t", Some(500));
    let lowest = a.clone().min(b.clone());
    let st = env.drive.stat("d/t.bin", &ctx()).unwrap().unwrap();
    assert_eq!(st.provider_ref.as_deref(), Some(lowest.as_str()));
    let listed = env.drive.list("d/", &ctx()).unwrap();
    assert_eq!(listed[0].provider_ref.as_deref(), Some(lowest.as_str()));
}

#[test]
fn trash_mode_moves_to_trash_in_my_drive() {
    let env = common::setup();
    env.drive.put("t/a.bin", PutSource::Bytes(b"a"), &PutOptions::create_only(None), &ctx()).unwrap();
    env.drive.delete("t/a.bin", DeleteMode::Trash, &ctx()).unwrap();
    let o = env.mock.objects().into_iter().find(|o| o.name == "a.bin").unwrap();
    assert!(o.trashed, "recoverable from the trash");
    assert!(env.drive.stat("t/a.bin", &ctx()).unwrap().is_none());
    assert!(env.drive.list("t/", &ctx()).unwrap().is_empty());
    // appDataFolder has no trash: Trash falls back to a permanent delete.
    let app = common::setup_with(MockConfig::default(), DriveSpace::AppData, |_| {});
    app.drive.put("t/a.bin", PutSource::Bytes(b"a"), &PutOptions::create_only(None), &ctx()).unwrap();
    app.drive.delete("t/a.bin", DeleteMode::Trash, &ctx()).unwrap();
    assert!(!app.mock.objects().iter().any(|o| o.name == "a.bin"));
}

#[test]
fn a_trashed_parent_folder_is_not_reused() {
    let env = common::setup();
    env.drive.put("p/one.bin", PutSource::Bytes(b"1"), &PutOptions::create_only(None), &ctx()).unwrap();
    let p = env.mock.objects().into_iter().find(|o| o.name == "p" && o.is_folder).unwrap();
    env.mock.trash(&p.id);
    // The cached folder ID points into the trash; the upload must not land there.
    let info = env.drive.put("p/two.bin", PutSource::Bytes(b"2"), &PutOptions::create_only(None), &ctx()).unwrap();
    let two = env.mock.objects().into_iter().find(|o| Some(o.id.as_str()) == info.provider_ref.as_deref()).unwrap();
    assert_ne!(two.parents, vec![p.id.clone()]);
    assert_eq!(read_all(env.drive.get("p/two.bin", None, &ctx()).unwrap()).unwrap(), b"2");
}

#[test]
fn unusual_root_folder_names_are_escaped_in_queries() {
    let name = "Pack'Sync \\ ünï \"q\" ' or name != '";
    let env =
        common::setup_with(MockConfig::default(), DriveSpace::VisibleFolder, |c| c.root_folder_name = name.into());
    env.drive.put("a/b.bin", PutSource::Bytes(b"data"), &PutOptions::create_only(None), &ctx()).unwrap();
    // A fresh instance must find that root by querying its name (no cached IDs in memory).
    let other = env.new_instance(|_| {});
    assert_eq!(read_all(other.get("a/b.bin", None, &ctx()).unwrap()).unwrap(), b"data");
    other.put("a/c.bin", PutSource::Bytes(b"c"), &PutOptions::create_only(None), &ctx()).unwrap();
    let roots: Vec<_> =
        env.mock.objects().into_iter().filter(|o| o.is_folder && o.app_properties.contains_key("psk")).collect();
    assert_eq!(roots.len(), 1, "exactly one root folder");
    assert_eq!(roots[0].name, name);
}

#[test]
fn invalid_keys_never_reach_the_network() {
    let env = common::setup();
    let before = env.mock.count(Endpoint::Any);
    let bad = [
        "",
        "/abs",
        "../escape",
        "a/../../b",
        "a/./b",
        "a//b",
        "a b",
        "a'b",
        "a\\b",
        "ü",
        "a%2Fb",
        "a?b",
        "a#b",
        "x/../",
    ];
    for k in bad {
        assert!(env.drive.put(k, PutSource::Bytes(b"x"), &PutOptions::replace(), &ctx()).is_err(), "{k:?}");
        assert!(env.drive.get(k, None, &ctx()).is_err(), "{k:?}");
        assert!(env.drive.stat(k, &ctx()).is_err(), "{k:?}");
        assert!(env.drive.delete(k, DeleteMode::Permanent, &ctx()).is_err(), "{k:?}");
        assert!(env.drive.list(&format!("{k}/"), &ctx()).is_err() || k.is_empty(), "{k:?}");
    }
    let mut t = std::collections::BTreeMap::new();
    t.insert("ps".to_string(), "0".to_string());
    assert!(
        env.drive
            .put("ok/k.bin", PutSource::Bytes(b"x"), &PutOptions { tags: t, ..PutOptions::replace() }, &ctx())
            .is_err()
    );
    assert_eq!(env.mock.count(Endpoint::Any), before, "no request was sent for invalid input");
}

#[test]
fn provider_id_identifies_account_space_and_root() {
    let env = common::setup();
    assert_eq!(env.drive.provider_id(), "gdrive:mock.user@example.com:drive.file:PackSync");
    let app = env.new_instance(|c| {
        c.space = DriveSpace::AppData;
        c.root_folder_name = "PackSync-Test".into();
    });
    assert_eq!(app.provider_id(), "gdrive:mock.user@example.com:appdata:PackSync-Test");
    let caps = env.drive.capabilities();
    assert!(!caps.unique_keys && !caps.conditional_write && caps.range_reads && caps.resumable_uploads);
    assert_eq!(caps.recommended_concurrency, 4);
    assert_eq!(caps.recommended_pack_size, 16 * 1024 * 1024);
}
