//! End-to-end tests of the `packsync` binary. Unix only (the fake launcher is a shell script).
#![cfg(unix)]

mod common;

use common::*;
use packsync_prism::testkit::FakeGame;

#[test]
fn help_and_version_work_without_any_setup() {
    let d = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(BIN).arg("--version").env("PACKSYNC_STATE_DIR", d.path()).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("packsync "));
    let out = std::process::Command::new(BIN).arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for cmd in ["status", "enable", "restore-new", "resolve", "launch", "storage", "doctor"] {
        assert!(text.contains(cmd), "help lists {cmd}");
    }
    assert!(!text.contains("dev-bridge") && !text.contains("pre-launch"), "internal commands are hidden");
}

#[test]
fn two_computers_continue_each_others_progress() {
    let w = World::new();
    let a = w.dev("desktop");
    let root = a.new_instance("Pack One");

    // discovery sees the instance; nothing is synced until the user says so
    let found = a.ok(&["instances"]);
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["name"], "Pack One");
    assert!(found[0]["registered"].is_null());
    let pv = a.ok(&["preview", root.to_str().unwrap()]);
    assert!(pv["included_files"].as_u64().unwrap() > 10);
    assert!(
        pv["excluded"]["pruned_dirs"].as_array().unwrap().len() >= 3,
        "logs, caches etc. are excluded and explained: {}",
        pv["excluded"]
    );
    let text = a.run(&["preview", root.to_str().unwrap()]).stdout;
    assert!(text.contains("Not synced") && text.contains("logs/") && text.contains("screenshots/"), "{text}");

    let en = a.ok(&["enable", root.to_str().unwrap()]);
    let id = en["outcome"]["instance"].as_str().unwrap().to_string();
    assert_eq!(en["upload"]["remaining"], 0);
    let st = a.ok(&["status", &id, "--online"]);
    let phase = st["instances"][0]["phase"].as_str().unwrap().to_string();
    assert!(matches!(phase.as_str(), "ready_on_other_devices" | "cloud_published"), "{phase}");

    // play on the desktop
    a.play(playing("+desktop"));
    let l = a.ok(&["launch", &id]);
    assert_eq!(l["end"]["outcome"], "clean", "{l}");
    assert_eq!(l["end"]["uploaded"], true);

    // the laptop sees it in the cloud, brings it over, and continues
    let b = w.dev("laptop");
    let cloud = b.ok(&["cloud"]);
    assert_eq!(cloud[0]["id"], id.as_str());
    let r = b.ok(&["restore-new"]);
    let broot = std::path::PathBuf::from(r["root"].as_str().unwrap());
    assert_eq!(std::fs::read(game(&broot, STATS)).unwrap(), std::fs::read(game(&root, STATS)).unwrap());
    b.play(playing("+laptop"));
    assert_eq!(b.ok(&["launch", &id])["end"]["outcome"], "clean");

    // back on the desktop the next launch starts from the laptop's progress
    a.play(FakeGame::default());
    let l = a.ok(&["launch", &id]);
    assert_eq!(l["pre_sync"]["outcome"], "fast_forwarded", "{l}");
    assert!(std::fs::read(game(&root, STATS)).unwrap().ends_with(b"+desktop+laptop"));
    let hist = a.ok(&["history", &id]);
    assert_eq!(hist.as_array().unwrap().len(), 3, "first snapshot + one per session that changed something: {hist}");
}

#[test]
fn diverged_computers_are_never_merged_and_the_user_chooses() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, aroot) = a.enabled_instance("Pack One");
    let b = w.dev("laptop");
    let broot = std::path::PathBuf::from(b.ok(&["restore-new"])["root"].as_str().unwrap());

    // both change the same world while apart
    std::fs::write(game(&aroot, "config/mine.toml"), "from = 'desktop'\n").unwrap();
    std::fs::write(game(&broot, "config/mine.toml"), "from = 'laptop'\n").unwrap();
    a.ok(&["sync", &id]);
    let r = b.run(&["--json", "sync", &id]);
    assert_eq!(r.code, 12, "a conflict has its own exit code: {}", r.stderr);
    assert_eq!(r.result()["reconcile"]["outcome"], "conflict");
    // nothing was overwritten
    assert_eq!(std::fs::read_to_string(game(&broot, "config/mine.toml")).unwrap(), "from = 'laptop'\n");
    let c = b.run(&["--json", "conflicts", &id]);
    assert_eq!(c.code, 12);
    let human = b.run(&["conflicts", &id]);
    assert!(human.stdout.contains("keep-cloud") && human.stdout.contains("split"), "{}", human.stdout);
    // everything else is blocked until a decision
    assert_eq!(b.run(&["--json", "launch", &id]).error_code(), "conflict");

    // choose: keep the cloud's (desktop) version here; the laptop's is preserved in history first
    let before = b.ok(&["history", &id]).as_array().unwrap().len();
    b.ok(&["resolve", &id, "keep-cloud"]);
    assert_eq!(std::fs::read_to_string(game(&broot, "config/mine.toml")).unwrap(), "from = 'desktop'\n");
    assert!(b.ok(&["history", &id]).as_array().unwrap().len() > before);
    assert_eq!(b.run(&["--json", "conflicts", &id]).code, 0);
}

#[test]
fn splitting_keeps_both_versions_as_two_instances() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, aroot) = a.enabled_instance("Pack One");
    let b = w.dev("laptop");
    let broot = std::path::PathBuf::from(b.ok(&["restore-new"])["root"].as_str().unwrap());
    std::fs::write(game(&aroot, "config/mine.toml"), "from = 'desktop'\n").unwrap();
    std::fs::write(game(&broot, "config/mine.toml"), "from = 'laptop'\n").unwrap();
    a.ok(&["sync", &id]);
    assert_eq!(b.run(&["--json", "sync", &id]).code, 12);
    let r = b.ok(&["resolve", &id, "split", "--name", "Pack One (laptop)"]);
    assert!(r["new_instance"].is_string());
    let cloud = a.ok(&["cloud"]);
    assert_eq!(cloud.as_array().unwrap().len(), 2, "{cloud}");
}

#[test]
fn errors_are_machine_readable_with_stable_codes_and_exit_codes() {
    let w = World::new();
    let a = w.dev("desktop");
    let r = a.run(&["--json", "status", "does-not-exist"]);
    assert_eq!(r.code, 3);
    assert_eq!(r.error_code(), "not_found");
    let v = r.json();
    assert!(v["error"]["message"].as_str().unwrap().contains("does-not-exist"));
    assert!(v["error"]["exit_code"] == 3);
    // an unknown subcommand is a usage error
    assert_eq!(a.run(&["frobnicate"]).code, 2);
    // destructive commands without --yes and without a terminal refuse instead of guessing
    let (id, _root) = a.enabled_instance("Pack One");
    let r = a.run(&["--json", "disable", &id]);
    assert_ne!(r.code, 0);
    assert_eq!(r.error_code(), "invalid");
    assert!(a.run(&["--json", "status", &id]).stdout.contains("\"sync_enabled\": true"));
    // no storage connected
    let b = w.dev("laptop");
    let r = b.run(&["--json", "storage", "disconnect", "--yes"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = b.run(&["--json", "cloud"]);
    assert_eq!(r.error_code(), "config");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("no cloud storage"));
}

#[test]
fn secrets_never_appear_in_json_output_or_diagnostics() {
    let w = World::new();
    let a = w.dev("desktop");
    let client = a.base.join("client.json");
    std::fs::write(&client, r#"{"installed":{"client_id":"123-abc.apps.googleusercontent.com","client_secret":"GOCSPX-supersecretvalue","auth_uri":"https://accounts.google.com/o/oauth2/v2/auth","token_uri":"https://oauth2.googleapis.com/token"}}"#).unwrap();
    a.ok(&["settings", "set", "google_client_file", client.to_str().unwrap()]);
    let r = a.run(&["--json", "storage", "use-google"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    for args in [
        vec!["--json", "storage", "status"],
        vec!["--json", "doctor"],
        vec!["--json", "settings", "show"],
        vec!["--json", "status"],
    ] {
        let r = a.run(&args);
        for text in [&r.stdout, &r.stderr] {
            assert!(!text.contains("GOCSPX-supersecretvalue"), "{args:?} leaked the client secret:\n{text}");
        }
    }
    let report = a.base.join("report.json");
    assert_eq!(a.run(&["doctor", "--out", report.to_str().unwrap()]).code, 0);
    assert!(!std::fs::read_to_string(&report).unwrap().contains("GOCSPX"));
    // and none of it was written into the settings file
    let settings = std::fs::read_to_string(a.config.join("settings.json")).unwrap();
    assert!(!settings.contains("GOCSPX"), "{settings}");
}

#[test]
fn cleanup_shows_a_plan_first_and_only_runs_exactly_that_plan() {
    use packsync_core::hash::Hash;
    use packsync_core::storage::{PutOptions, PutSource, Storage};
    let w = World::new();
    let a = w.dev("desktop");
    let (id, _root) = a.enabled_instance("Pack One");
    // an abandoned upload's pack, old enough; and a file somebody else put into the same folder
    let junk = b"left behind by an abandoned upload".to_vec();
    let key = format!("packs/{}.pack", Hash::of(&junk).to_hex());
    let st = packsync_core::storage_local::LocalDirStorage::new(&w.cloud).unwrap();
    let ctx = packsync_core::progress::OpCtx::detached();
    st.put(
        &key,
        PutSource::Bytes(&junk),
        &PutOptions::create_only(None).with_tag("ps", "1").with_tag("kind", "pack"),
        &ctx,
    )
    .unwrap();
    st.put("packs/mine.txt", PutSource::Bytes(b"not PackSync's"), &PutOptions::create_only(None), &ctx).unwrap();
    for f in [key.as_str(), "packs/mine.txt"] {
        filetime::set_file_mtime(w.cloud.join(f), filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
    }
    // plan: informative and harmless
    let plan = a.ok(&["gc", "plan"]);
    assert_eq!(plan["delete"].as_array().unwrap().len(), 1, "{plan}");
    assert!(plan["kept_unknown"].as_array().unwrap().iter().any(|k| k.as_str().unwrap().contains("mine.txt")));
    assert!(w.cloud.join(&key).exists());
    let human = a.run(&["gc", "plan"]);
    assert!(human.stdout.contains("gc run"), "{}", human.stdout);
    // a plan id that does not match is refused (exit 12) and nothing happens
    let bad = a.run(&["--json", "--yes", "gc", "run", "0000000000000000"]);
    assert_eq!(bad.code, 12, "{}", bad.stderr);
    assert!(w.cloud.join(&key).exists());
    // without --yes and without a terminal it does not guess
    let id_str = plan["id"].as_str().unwrap().to_string();
    let no_yes = a.run(&["--json", "gc", "run", &id_str]);
    assert_ne!(no_yes.code, 0);
    assert!(w.cloud.join(&key).exists());
    // the real run deletes exactly that and nothing else
    let rep = a.ok(&["gc", "run", &id_str]);
    assert_eq!(rep["deleted_packs"], 1);
    assert!(!w.cloud.join(&key).exists() && w.cloud.join("packs/mine.txt").exists());
    assert_eq!(a.ok(&["gc", "plan"])["delete"].as_array().unwrap().len(), 0);
    // options are validated
    assert_ne!(a.run(&["--json", "gc", "plan", "--instance", &id]).code, 0, "--instance needs --retain");
    // the instance still verifies against the cloud
    assert_eq!(a.ok(&["verify", &id, "--remote", "--deep"])["remote"]["damaged_blobs"].as_array().unwrap().len(), 0);
}

#[test]
fn verify_finds_data_missing_from_the_cloud_and_repair_uploads_it_again() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, _root) = a.enabled_instance("Pack One");
    let v = a.ok(&["verify", &id, "--remote", "--repair"]);
    assert_eq!(v["repair"]["files_missing"], 0, "{v}");

    // the biggest data pack disappears from the cloud folder (removed outside PackSync)
    let mut packs: Vec<_> = std::fs::read_dir(w.cloud.join("packs"))
        .unwrap()
        .flatten()
        .map(|e| (e.metadata().unwrap().len(), e.path()))
        .collect();
    packs.sort();
    std::fs::remove_file(&packs.last().unwrap().1).unwrap();

    let r = a.run(&["--json", "verify", &id, "--remote"]);
    assert_eq!((r.code, r.error_code()), (16, "corrupt_remote".to_string()), "{}", r.stdout);
    // one JSON document: the error, and the report that explains it
    assert!(!r.json()["result"]["remote"]["missing_packs"].as_array().unwrap().is_empty(), "{}", r.stdout);
    let v = a.ok(&["verify", &id, "--remote", "--repair"]);
    assert!(v["repair"]["files_missing"].as_u64().unwrap() > 0, "{v}");
    assert_eq!(v["repair"]["unrepairable"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(
        v["remote"]["missing_packs"].as_array().unwrap().len()
            + v["remote"]["unlisted_blobs"].as_array().unwrap().len(),
        0,
        "{v}"
    );
    // another computer can restore it again
    let b = w.dev("laptop");
    b.ok(&["restore-new"]);
}
