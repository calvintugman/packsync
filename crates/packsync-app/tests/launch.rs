//! App-level launch orchestration and launcher hooks, against a fake `prismlauncher` and a fake game.
//! The fakes are shell scripts on Unix and the `packsync-fake-prism` program elsewhere.

mod common;

use common::*;
use packsync_app::{HookOutcome, LaunchOptions};
use packsync_core::db::{InstanceState, SessionOwner};
use packsync_core::engine::{ReconcileOpts, ReconcileReport, SyncPhase};
use packsync_core::manifest::{SessionOutcome, SnapshotKind};
use packsync_prism::testkit::FakeGame;
use std::io::Write as _;

fn stats_of(d: &Device, root: &std::path::Path) -> Vec<u8> {
    std::fs::read(d.game_file(root, STATS)).unwrap()
}

fn with(before: &[u8], appended: &str) -> Vec<u8> {
    [before, appended.as_bytes()].concat()
}

/// A process source that reports a Prism using `data_dir` only while the flag is up.
struct Toggle(std::sync::Arc<std::sync::atomic::AtomicBool>, std::path::PathBuf);
impl packsync_prism::process::ProcessSource for Toggle {
    fn prism_processes(&self) -> Vec<packsync_prism::process::ProcInfo> {
        if !self.0.load(std::sync::atomic::Ordering::SeqCst) {
            return vec![];
        }
        vec![packsync_prism::process::ProcInfo {
            pid: 1,
            name: "prismlauncher".into(),
            cmd: vec!["prismlauncher".into(), "--dir".into(), self.1.to_string_lossy().into_owned()],
            ..Default::default()
        }]
    }
}

#[test]
fn app_launch_saves_uploads_and_a_second_computer_receives_the_progress() {
    let w = World::new();
    let a = w.device("desktop");
    let (inst, root) = a.new_synced_instance("Pack One");
    let before = stats_of(&a, &root);
    a.play(playing("+played-on-desktop"));

    let out = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap();
    assert!(out.game_started && !out.offline, "{out:?}");
    assert_eq!(out.launcher_exit_code, Some(0));
    let end = out.end.unwrap();
    assert_eq!(end.outcome, SessionOutcome::Clean);
    assert_eq!(end.kind, Some(SnapshotKind::Normal));
    assert!(end.snapshot.is_some() && end.uploaded, "{end:?}");
    assert_eq!(stats_of(&a, &root), with(&before, "+played-on-desktop"));
    let st = a.app.engine().status(inst, true, &a.ctx()).unwrap();
    assert!(matches!(st.phase, SyncPhase::ReadyOnOtherDevices | SyncPhase::CloudPublished), "{:?}", st.phase);

    // a different computer restores the pack from the cloud, then plays: its launch first picks up
    // everything the desktop saved, and ends with its own progress on top
    let b = w.device("laptop");
    let restored = b.app.restore_cloud_instance(Default::default(), &b.ctx()).unwrap();
    assert_eq!(restored.instance, inst);
    assert_eq!(stats_of(&b, &restored.root), with(&before, "+played-on-desktop"));
    // machine-local settings of the desktop did not travel
    let cfg = std::fs::read_to_string(restored.root.join("instance.cfg")).unwrap();
    assert!(!cfg.contains("lastLaunchTime"), "{cfg}");
    b.play(playing("+laptop"));
    let out_b = b.app.launch(inst, launch_opts(), &b.ctx()).unwrap();
    assert_eq!(out_b.end.as_ref().unwrap().outcome, SessionOutcome::Clean);
    assert!(out_b.end.unwrap().uploaded);

    // back on the desktop: opening the game fast-forwards to the laptop's progress first
    a.play(FakeGame::default());
    let out_a = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap();
    assert!(matches!(out_a.pre_sync, Some(ReconcileReport::FastForwarded { .. })), "{:?}", out_a.pre_sync);
    assert_eq!(stats_of(&a, &root), with(&before, "+played-on-desktop+laptop"));
}

#[test]
fn a_crash_is_kept_apart_until_the_user_decides() {
    let w = World::new();
    let a = w.device("desktop");
    let (inst, root) = a.new_synced_instance("Pack One");
    let cloud_head = |d: &Device| {
        d.app.engine().cloud_instances(&d.ctx()).unwrap()[0].heads.iter().map(|h| h.id).collect::<Vec<_>>()
    };
    let good = cloud_head(&a);
    assert_eq!(good.len(), 1);
    a.play(FakeGame {
        exit: 1,
        crash_report: true,
        write: Some((STATS.into(), "+half-written".into())),
        ..Default::default()
    });

    let out = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap();
    let end = out.end.unwrap();
    assert_eq!(end.outcome, SessionOutcome::Crashed);
    assert_eq!(end.kind, Some(SnapshotKind::Crash));
    assert!(!end.crash_artifacts.is_empty());
    let row = a.app.engine().instance(inst).unwrap();
    assert_eq!(row.state, InstanceState::CrashPending);
    assert_eq!(cloud_head(&a), good, "other computers keep seeing the last known-good snapshot");
    // the next launch is refused with a clear reason, and nothing is overwritten
    let err = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap_err();
    assert_eq!(err.code().as_str(), "crash_pending", "{err}");
    assert!(stats_of(&a, &root).ends_with(b"+half-written"));

    // the user trusts the local state: it becomes a normal snapshot and play resumes
    let promoted = a.app.engine().promote_crash(inst, &a.ctx()).unwrap();
    a.app.engine().upload_pending(inst, false, &a.ctx()).unwrap();
    assert_ne!(vec![promoted], good);
    a.play(FakeGame::default());
    let out = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap();
    assert_eq!(out.end.unwrap().outcome, SessionOutcome::Clean);
}

#[test]
fn a_failed_launch_that_never_started_the_game_is_not_blamed_on_a_crash() {
    let w = World::new();
    let a = w.device("desktop");
    let (inst, _root) = a.new_synced_instance("Pack One");
    a.play(FakeGame { fail_early: true, ..Default::default() });
    let out = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap();
    assert!(!out.game_started);
    assert_eq!(out.launcher_exit_code, Some(1));
    let end = out.end.unwrap();
    assert_eq!(end.outcome, SessionOutcome::Clean);
    assert!(end.snapshot.is_none());
    assert_eq!(a.app.engine().instance(inst).unwrap().state, InstanceState::Ok);
}

#[test]
fn when_prism_was_already_running_the_exit_status_is_unknown_and_that_is_treated_with_care() {
    let w = World::new();
    let dir = w.dir.path().join("desktop/prism");
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let a = w.device_with("desktop", |p| p.with_process_source(std::sync::Arc::new(Toggle(running.clone(), dir))));
    let (inst, _root) = a.new_synced_instance("Pack One");
    a.play(FakeGame { forward: true, ..playing("+forwarded") });
    let out = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap();
    assert!(out.game_started, "the game is found in the process table although the child exited at once: {out:?}");
    assert_eq!(out.launcher_exit_code, None);
    assert!(out.warnings.iter().any(|w| w.contains("already running")), "{:?}", out.warnings);
    let end = out.end.unwrap();
    // without the post-exit hook there is no proof of a clean exit: the save is quarantined, not trusted
    assert_eq!(end.outcome, SessionOutcome::Interrupted);
    assert_eq!(end.kind, Some(SnapshotKind::Crash));
    assert_eq!(a.app.engine().instance(inst).unwrap().state, InstanceState::CrashPending);
}

#[test]
fn with_hooks_installed_the_post_exit_signal_proves_a_clean_exit_even_when_forwarded() {
    let w = World::new();
    let dir = w.dir.path().join("desktop/prism");
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a = w.device_with("desktop", |p| p.with_process_source(std::sync::Arc::new(Toggle(running.clone(), dir))));
    let (inst, _root) = a.new_synced_instance("Pack One");
    // install the hooks while Prism is closed (the only time it is allowed) …
    let cli = std::path::Path::new(if cfg!(windows) {
        r"C:\Program Files\PackSync\packsync.exe"
    } else {
        "/opt/packsync/packsync"
    });
    let plan = a.app.hooks_plan_install(inst, cli).unwrap();
    assert!(plan.blockers.is_empty(), "{plan:?}");
    a.app.hooks_apply(inst, &plan).unwrap();
    assert!(a.app.hooks_state(inst).unwrap().intact);
    // … then Prism is running, and the app asks it to launch the game
    running.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(!a.app.hooks_plan_install(inst, cli).unwrap().blockers.is_empty(), "no edits while Prism runs");
    a.play(FakeGame { forward: true, secs: 1.0, ..playing("+with-hooks") });
    let app = a.app.clone();
    // Prism runs the post-exit command right after the game ends: emulate it from a thread
    let hook = std::thread::spawn(move || {
        // wait for the game to start and end, as Prism does, then run the command
        let started = loop {
            let open = app.engine().db().sessions_open(inst).unwrap();
            if let Some((pid, ms)) = open.iter().find_map(|s| s.game_pid.zip(s.game_start_ms)) {
                break (pid, ms);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        while packsync_core::proc::pid_alive(started.0, Some(started.1)) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        app.hook_post_exit(inst, &packsync_core::progress::OpCtx::detached())
    });
    let mut opts = launch_opts();
    opts.timing.signal_wait = std::time::Duration::from_secs(5);
    let out = a.app.launch(inst, opts, &a.ctx()).unwrap();
    let hook_outcome = hook.join().unwrap();
    assert!(matches!(hook_outcome, HookOutcome::Proceed { .. }));
    assert!(out.clean_signal, "{out:?}");
    assert_eq!(out.end.unwrap().outcome, SessionOutcome::Clean);
    assert_eq!(a.app.engine().instance(inst).unwrap().state, InstanceState::Ok);
}

#[test]
fn hooks_own_the_sync_when_the_game_is_started_from_the_launcher() {
    let w = World::new();
    let a = w.device("desktop");
    let (inst, root) = a.new_synced_instance("Pack One");

    // Prism runs the pre-launch command
    let pre = a.app.hook_pre_launch(inst, &a.ctx());
    assert!(matches!(pre, HookOutcome::Proceed { .. }), "{pre:?}");
    let open = a.app.engine().db().sessions_open(inst).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].owner, SessionOwner::Hook);
    // …the game runs and exits normally (Prism's own exit status is not visible to hooks)…
    a.play(playing("+from-launcher"));
    let status = std::process::Command::new(&a.fake.exe)
        .args(["--dir", a.data_dir.to_str().unwrap(), "--launch", "Pack One"])
        .status()
        .unwrap();
    assert!(status.success());
    // …and Prism runs the post-exit command
    let post = a.app.hook_post_exit(inst, &a.ctx());
    let HookOutcome::Proceed { messages, upload } = post else { panic!("{post:?}") };
    assert_eq!(upload, Some(inst), "{messages:?}");
    assert!(a.app.engine().db().sessions_open(inst).unwrap().is_empty());
    let row = a.app.engine().instance(inst).unwrap();
    assert_eq!(row.state, InstanceState::Ok);
    // the caller (the CLI) then uploads in the background
    a.app.engine().upload_pending(inst, false, &a.ctx()).unwrap();
    assert!(stats_of(&a, &root).ends_with(b"+from-launcher"));
    let st = a.app.engine().status(inst, true, &a.ctx()).unwrap();
    assert!(matches!(st.phase, SyncPhase::ReadyOnOtherDevices | SyncPhase::CloudPublished));
}

#[test]
fn a_launch_without_a_post_exit_hook_is_an_interrupted_session_and_blocks_the_next_launch() {
    let w = World::new();
    let a = w.device("desktop");
    let (inst, _root) = a.new_synced_instance("Pack One");
    assert!(matches!(a.app.hook_pre_launch(inst, &a.ctx()), HookOutcome::Proceed { .. }));
    // the game crashed: Prism never runs the post-exit command. Some time later the user starts it again.
    a.play(FakeGame { exit: 1, crash_report: true, ..playing("+lost-session") });
    let _ = std::process::Command::new(&a.fake.exe)
        .args(["--dir", a.data_dir.to_str().unwrap(), "--launch", "Pack One"])
        .status()
        .unwrap();
    let next = a.app.hook_pre_launch(inst, &a.ctx());
    let HookOutcome::Block { message, exit_code } = next else { panic!("{next:?}") };
    assert!(
        message.to_lowercase().contains("crash") || message.to_lowercase().contains("ended abnormally"),
        "{message}"
    );
    assert_eq!(exit_code, packsync_core::error::exit::SAFETY_GUARD);
    assert_eq!(a.app.engine().instance(inst).unwrap().state, InstanceState::CrashPending);
    // a user who prefers to keep playing can turn blocking off; the problem is reported, nothing is synced
    a.app.update_settings(|s| s.hook_block_on_problem = false).unwrap();
    let again = a.app.hook_pre_launch(inst, &a.ctx());
    let HookOutcome::Proceed { messages, .. } = again else { panic!("{again:?}") };
    assert!(messages.iter().any(|m| m.contains("did not sync")), "{messages:?}");
}

#[test]
fn the_app_owns_the_session_and_hooks_stay_out_of_the_way() {
    let w = World::new();
    let a = w.device("desktop");
    let (inst, _root) = a.new_synced_instance("Pack One");
    use packsync_core::engine::BeginSession;
    let begun = a
        .app
        .engine()
        .begin_session(inst, BeginSession { owner: SessionOwner::App, token: "app-1".into(), offline: false }, &a.ctx())
        .unwrap();
    let pre = a.app.hook_pre_launch(inst, &a.ctx());
    let HookOutcome::Proceed { messages, .. } = pre else { panic!("{pre:?}") };
    assert!(messages.iter().any(|m| m.contains("PackSync started this game")), "{messages:?}");
    assert_eq!(a.app.engine().db().sessions_open(inst).unwrap().len(), 1, "no second session");
    let post = a.app.hook_post_exit(inst, &a.ctx());
    assert!(matches!(post, HookOutcome::Proceed { upload: None, .. }));
    assert_eq!(a.app.engine().db().sessions_open(inst).unwrap()[0].id, begun.session, "still open: the app closes it");
}

#[test]
fn offline_launches_follow_the_policy_and_leave_the_snapshot_pending() {
    use packsync_core::storage::Storage;
    use packsync_core::storage_fault::{FaultPlan, FaultyStorage};
    use packsync_core::storage_local::LocalDirStorage;
    let w = World::new();
    let a = w.device("desktop");
    let (inst, root) = a.new_synced_instance("Pack One");
    // the network drops: wrap the storage in a switchable fault injector
    let plan = FaultPlan::new(1);
    let inner: std::sync::Arc<dyn Storage> = std::sync::Arc::new(LocalDirStorage::new(&w.cloud).unwrap());
    a.app.engine().set_storage(Some(FaultyStorage::new(inner, plan.clone()))).unwrap();
    plan.set_offline(true);

    // default policy: play the local copy
    a.play(playing("+offline-play"));
    let out = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap();
    assert!(out.offline && out.game_started, "{out:?}");
    assert!(out.warnings.iter().any(|w| w.contains("local copy")), "{:?}", out.warnings);
    let end = out.end.unwrap();
    assert_eq!(end.outcome, SessionOutcome::Clean);
    assert!(end.snapshot.is_some() && !end.uploaded);
    assert!(stats_of(&a, &root).ends_with(b"+offline-play"));
    let st = a.app.engine().status(inst, false, &a.ctx()).unwrap();
    assert!(matches!(st.phase, SyncPhase::LocalSnapshotComplete | SyncPhase::UploadPending), "{:?}", st.phase);

    // stricter policy: refuse to start without the cloud
    a.app.update_settings(|s| s.hook_offline_policy = packsync_app::OfflinePolicy::Block).unwrap();
    let err = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap_err();
    assert_eq!(err.code().as_str(), "offline", "{err}");

    // the network is back: the pending snapshot uploads and becomes visible
    plan.set_offline(false);
    let up = a.app.engine().upload_pending(inst, true, &a.ctx()).unwrap();
    assert_eq!(up.remaining, 0, "{up:?}");
    let rec = a.app.engine().reconcile(inst, ReconcileOpts::default(), &a.ctx()).unwrap();
    assert!(matches!(rec, ReconcileReport::UpToDate { .. }), "{rec:?}");
}

#[test]
fn an_instance_without_sync_is_just_started() {
    let w = World::new();
    let a = w.device("desktop");
    let root = packsync_prism::testkit::make_instance(&a.data_dir, "Plain", &spec());
    // register without enabling sync: preview only, so there is no registration at all
    let err = a.app.launch(packsync_core::ids::InstanceId::new(), LaunchOptions::default(), &a.ctx()).unwrap_err();
    assert_eq!(err.code().as_str(), "not_found");
    assert!(root.join("instance.cfg").exists());
}

#[test]
fn a_clean_exit_signal_nobody_collected_closes_the_session_as_clean_later() {
    // The app was closed (or crashed) while the game ran; Prism's post-exit hook still reported a clean
    // exit. The next start of PackSync must not call that a crash.
    use packsync_core::engine::BeginSession;
    let w = World::new();
    let a = w.device("desktop");
    let (inst, root) = a.new_synced_instance("Pack One");
    let begun = a
        .app
        .engine()
        .begin_session(
            inst,
            BeginSession { owner: SessionOwner::App, token: "app-dead".into(), offline: false },
            &a.ctx(),
        )
        .unwrap();
    std::fs::OpenOptions::new().append(true).open(a.game_file(&root, STATS)).unwrap().write_all(b"+played").unwrap();
    // the post-exit hook runs: the session belongs to the app, so it only leaves the signal
    let _ = a.app.hook_post_exit(inst, &a.ctx());
    let signal = a.base.join("app/state/signals").join(format!("{}.clean", begun.session));
    assert!(signal.exists());
    // too fresh: a live app might still be about to collect it
    assert_eq!(a.app.close_signalled_sessions(&a.ctx()), 0);
    assert_eq!(a.app.engine().db().sessions_open(inst).unwrap().len(), 1);
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    filetime::set_file_mtime(&signal, filetime::FileTime::from_system_time(old)).unwrap();
    assert_eq!(a.app.close_signalled_sessions(&a.ctx()), 1);
    assert!(a.app.engine().db().sessions_open(inst).unwrap().is_empty());
    let st = a.app.engine().instance(inst).unwrap();
    assert_eq!(st.state, InstanceState::Ok, "closed as a normal session, not quarantined");
    assert!(!signal.exists());
}

#[test]
fn a_session_whose_app_was_killed_no_longer_makes_the_hooks_stand_aside() {
    // The app started a session and was killed (or crashed) before the game ran. Treating that session as
    // "owned" for half an hour would leave the next launch from the launcher without any syncing.
    use packsync_core::engine::BeginSession;
    let w = World::new();
    let a = w.device("desktop");
    let (inst, _root) = a.new_synced_instance("Pack One");
    let begun = a
        .app
        .engine()
        .begin_session(
            inst,
            BeginSession { owner: SessionOwner::App, token: "app-killed".into(), offline: false },
            &a.ctx(),
        )
        .unwrap();
    // while its owner (this test process) lives, the hooks stay out of the way
    let pre = a.app.hook_pre_launch(inst, &a.ctx());
    let HookOutcome::Proceed { messages, .. } = pre else { panic!("{pre:?}") };
    assert!(messages.iter().any(|m| m.contains("PackSync started this game")), "{messages:?}");
    // the owner dies
    a.app.engine().db().session_update(begun.session, |r| r.owner_pid = Some(4_000_000)).unwrap();
    let pre = a.app.hook_pre_launch(inst, &a.ctx());
    let HookOutcome::Proceed { messages, .. } = pre else { panic!("{pre:?}") };
    assert!(!messages.iter().any(|m| m.contains("PackSync started this game")), "{messages:?}");
    let open = a.app.engine().db().sessions_open(inst).unwrap();
    assert_eq!(open.len(), 1, "the abandoned session was closed and the hook's own session is the only open one");
    assert_eq!(open[0].owner, SessionOwner::Hook);
    assert_ne!(open[0].id, begun.session);
}

/// What the cards promise ("It will retry …"): while the app is open, a version that was saved while
/// offline is uploaded once the cloud is back and its retry time has passed — without anyone clicking.
#[test]
fn while_the_app_is_open_a_pending_upload_is_retried_on_its_own() {
    use packsync_core::storage::Storage;
    use packsync_core::storage_fault::{FaultPlan, FaultyStorage};
    use packsync_core::storage_local::LocalDirStorage;
    let w = World::new();
    let a = w.device("desktop");
    let (inst, _root) = a.new_synced_instance("Pack One");
    let plan = FaultPlan::new(1);
    let inner: std::sync::Arc<dyn Storage> = std::sync::Arc::new(LocalDirStorage::new(&w.cloud).unwrap());
    a.app.engine().set_storage(Some(FaultyStorage::new(inner, plan.clone()))).unwrap();
    plan.set_offline(true);
    a.play(playing("+offline-play"));
    let out = a.app.launch(inst, launch_opts(), &a.ctx()).unwrap();
    assert!(!out.end.unwrap().uploaded);

    // still offline: whatever the scheduler tries fails, and a failure waits for its retry time
    for id in a.app.retry_uploads_once() {
        assert_eq!(id, inst);
    }
    while a.app.jobs().running() > 0 {
        tick();
    }
    plan.set_offline(false);
    let st = a.app.engine().status(inst, false, &a.ctx()).unwrap();
    // an outage is found before any data moves, so it leaves no backoff behind
    assert_eq!(st.next_retry_ms, None, "{st:?}");
    // the cloud is back: the next pass uploads it
    assert_eq!(a.app.retry_uploads_once(), vec![inst]);
    while a.app.jobs().running() > 0 {
        tick();
    }
    let st = a.app.engine().status(inst, false, &a.ctx()).unwrap();
    assert_eq!(st.pending_snapshots, 0, "{st:?}");
    assert!(a.app.retry_uploads_once().is_empty(), "nothing left to do");
}
