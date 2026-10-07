//! The launcher hooks as Prism runs them: the command lines PackSync wrote into `instance.cfg` are
//! tokenised with Qt's `splitCommand` rules (ported in `packsync_prism::cmdline`) and executed.
//! Unix only: the user's own pre-launch and post-exit commands it chains are shell scripts.
#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use packsync_prism::cmdline::split_command;
use packsync_prism::hooks;
use packsync_prism::testkit::FakeGame;

/// What Prism does with a hook command: split without a shell, run in the game folder.
fn run_like_prism(cmdline: &str, cwd: &Path) -> std::process::Output {
    let argv = split_command(cmdline);
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]).current_dir(cwd).env("PACKSYNC_LAUNCH_POLL_MS", "40");
    // Prism's own environment knows nothing about the test sandbox: the command line must carry what is needed
    hermetic(&mut c, cwd);
    c.output().unwrap()
}

fn user_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    // the closure may print what it sees on stderr to help diagnose a timeout
    for _ in 0..200 {
        if f() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn hooks_sync_a_launcher_started_game_and_keep_the_users_own_commands() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, root) = a.enabled_instance("Pack One");
    let marker = a.base.join("user-commands.log");
    let pre = user_script(&a.base, "user-pre.sh", &format!("echo pre >> '{}'", marker.display()));
    let post = user_script(&a.base, "user-post.sh", &format!("echo post >> '{}'", marker.display()));
    let cfg_path = root.join("instance.cfg");
    let original = std::fs::read_to_string(&cfg_path).unwrap().replace(
        "[General]\n",
        &format!(
            "[General]\nOverrideCommands=true\nPreLaunchCommand={}\nPostExitCommand={} --done\n",
            pre.display(),
            post.display()
        ),
    );
    std::fs::write(&cfg_path, &original).unwrap();

    // install: the plan is shown first, existing commands are chained, not replaced
    let plan = a.run(&["--json", "hooks", "install", &id, "--cli", BIN, "--plan"]).result();
    assert_eq!(plan["preserved_commands"].as_array().unwrap().len(), 2, "{plan}");
    assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), original, "planning changes nothing");
    a.ok(&["hooks", "install", &id, "--cli", BIN]);
    let st = hooks::state(&root).unwrap();
    assert!(st.installed && st.intact, "{st:?}");
    assert_eq!(a.ok(&["hooks", "status", &id])["installed"], true);

    // Prism launches: pre-launch command first
    let game_dir = root.join(".minecraft");
    let o = run_like_prism(&st.pre_launch.clone().unwrap(), &game_dir);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "pre\n", "the user's own pre-launch command still ran");
    // …the game runs and ends normally…
    a.play(playing("+via-hooks"));
    assert!(
        Command::new(&a.fake.exe)
            .args(["--dir", a.prism.to_str().unwrap(), "--launch", "Pack One"])
            .status()
            .unwrap()
            .success()
    );
    // …then the post-exit command
    let o = run_like_prism(&st.post_exit.clone().unwrap(), &game_dir);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "pre\npost\n");
    // the upload continues in the background after the hook returned
    let mut last = serde_json::Value::Null;
    wait_until("the background upload", || {
        last = a.ok(&["status", &id, "--online"]);
        eprintln!("status: {}", last["instances"][0]["phase"]);
        matches!(last["instances"][0]["phase"].as_str(), Some("ready_on_other_devices" | "cloud_published"))
    });
    let _ = &last;
    let hist = a.ok(&["history", &id]);
    assert_eq!(hist.as_array().unwrap().len(), 2, "{hist}");

    // another computer gets the progress made through the hooks
    let b = w.dev("laptop");
    let broot = std::path::PathBuf::from(b.ok(&["restore-new"])["root"].as_str().unwrap());
    assert!(std::fs::read(game(&broot, STATS)).unwrap().ends_with(b"+via-hooks"));

    // uninstall: byte-for-byte what the user had
    a.ok(&["hooks", "uninstall", &id]);
    assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), original);
    assert!(!hooks::state(&root).unwrap().installed);
}

#[test]
fn a_session_that_never_got_a_post_exit_blocks_the_next_launch_and_skips_the_users_command() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, root) = a.enabled_instance("Pack One");
    let marker = a.base.join("ran.log");
    let pre = user_script(&a.base, "user-pre.sh", &format!("echo pre >> '{}'", marker.display()));
    let cfg_path = root.join("instance.cfg");
    let cfg = std::fs::read_to_string(&cfg_path)
        .unwrap()
        .replace("[General]\n", &format!("[General]\nOverrideCommands=true\nPreLaunchCommand={}\n", pre.display()));
    std::fs::write(&cfg_path, cfg).unwrap();
    a.ok(&["hooks", "install", &id, "--cli", BIN]);
    let st = hooks::state(&root).unwrap();
    let game_dir = root.join(".minecraft");

    assert!(run_like_prism(&st.pre_launch.clone().unwrap(), &game_dir).status.success());
    // the game crashed: Prism does not run the post-exit command (exit status 1, crash report written)
    a.play(FakeGame { exit: 1, crash_report: true, ..playing("+lost") });
    assert!(
        !Command::new(&a.fake.exe)
            .args(["--dir", a.prism.to_str().unwrap(), "--launch", "Pack One"])
            .status()
            .unwrap()
            .success()
    );

    // the next launch is refused by the pre-launch command (non-zero → Prism does not start the game)
    let o = run_like_prism(&st.pre_launch.clone().unwrap(), &game_dir);
    assert_eq!(o.status.code(), Some(19), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("PackSync stopped the launch"));
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "pre\n",
        "the chained command did not run for a refused launch"
    );
    // …and the app tells the user what to decide
    let s = a.ok(&["status", &id]);
    assert_eq!(s["instances"][0]["phase"], "crash_pending");
    let c = a.run(&["crash", "show", &id]);
    assert!(c.stdout.contains("promote") && c.stdout.contains("discard"), "{}", c.stdout);
    assert!(a.ok(&["crash", "promote", &id])["snapshot"].is_string());
    // after the decision the launch goes through again
    assert!(run_like_prism(&st.pre_launch.clone().unwrap(), &game_dir).status.success());
}

#[test]
fn a_broken_packsync_never_stops_the_game_from_starting() {
    let w = World::new();
    let a = w.dev("desktop");
    let (id, root) = a.enabled_instance("Pack One");
    a.ok(&["hooks", "install", &id, "--cli", BIN]);
    let st = hooks::state(&root).unwrap();
    // corrupt PackSync's own settings: its part fails, the launch must still be allowed
    std::fs::write(a.config.join("settings.json"), "{ not json").unwrap();
    let cmd = st.pre_launch.unwrap();
    assert!(
        cmd.contains("--config-dir") && cmd.contains("--state-dir"),
        "non-default directories are carried by the hook itself: {cmd}"
    );
    let o = run_like_prism(&cmd, &root.join(".minecraft"));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("continuing without syncing"));
    // a chained command's own failure is passed through (Prism then blocks the launch itself)
    let failing = format!("{cmd} -- /bin/false");
    assert_eq!(run_like_prism(&failing, &root.join(".minecraft")).status.code(), Some(1));
}
