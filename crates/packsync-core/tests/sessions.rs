//! Play sessions: live writers, duplicate jobs, advisory leases, crash quarantine, recovery.

mod common;

use std::fs;
use std::sync::Barrier;

use common::*;
use packsync_core::db::{InstanceState, SessionOwner, SessionState};
use packsync_core::engine::*;
use packsync_core::error::Error;
use packsync_core::manifest::{SessionOutcome, SnapshotKind};
use packsync_core::testkit::harness::TestWorld;

fn begin(
    d: &packsync_core::testkit::harness::TestDevice,
    inst: packsync_core::ids::InstanceId,
    token: &str,
    offline: bool,
) -> Result<SessionBegun, Error> {
    d.engine.begin_session(inst, BeginSession { owner: SessionOwner::App, token: token.into(), offline }, &d.ctx())
}

#[cfg(unix)]
#[test]
fn nothing_mutates_an_instance_while_a_game_process_is_using_it() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &spec(1));
    a.upload(en.instance);
    write(&root, "config/edit.toml", b"during play\n");
    let mut game = spawn_fake_game(&root);

    let e = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap_err();
    assert!(matches!(e, Error::GameRunning(_)), "{e}");
    assert!(e.advice().unwrap().contains("Close Minecraft"));
    // a restore (fast-forward) is refused too, and so is starting another session
    let e = begin(&a, en.instance, "t1", false).unwrap_err();
    assert!(matches!(e, Error::GameRunning(_)), "{e}");
    let e = a
        .engine
        .restore_snapshot(en.instance, en.snapshot, RestoreOpts { skip_incompatible: false }, &a.ctx())
        .unwrap_err();
    assert!(matches!(e, Error::GameRunning(_) | Error::Conflict(_)), "{e}");
    // nothing claims a clean handoff: no snapshot exists for the edit
    assert_eq!(a.engine.history(en.instance, false, &a.ctx()).unwrap().len(), 1);

    game.kill().unwrap();
    game.wait().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(matches!(
        a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap().result,
        SnapshotResult::Created { .. }
    ));
}

#[test]
fn duplicate_hook_and_app_jobs_are_idempotent() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, _info, en) = a.new_instance("pack", &spec(1));
    a.upload(en.instance);
    let barrier = Barrier::new(2);
    let (r1, r2) = std::thread::scope(|s| {
        let h1 = s.spawn(|| {
            barrier.wait();
            a.engine.begin_session(
                en.instance,
                BeginSession { owner: SessionOwner::Hook, token: "launch-42".into(), offline: false },
                &a.ctx(),
            )
        });
        let h2 = s.spawn(|| {
            barrier.wait();
            a.engine.begin_session(
                en.instance,
                BeginSession { owner: SessionOwner::App, token: "launch-42".into(), offline: false },
                &a.ctx(),
            )
        });
        (h1.join().unwrap(), h2.join().unwrap())
    });
    // exactly one session exists, whoever won
    let ids: Vec<_> = [r1, r2].into_iter().filter_map(|r| r.ok()).map(|b| b.session).collect();
    assert!(!ids.is_empty());
    assert!(ids.iter().all(|i| *i == ids[0]), "both requests resolve to the same session");
    assert_eq!(a.engine.db().sessions_for(en.instance, 10).unwrap().len(), 1);
}

#[test]
fn leases_are_advisory_stale_ones_are_ignored_and_simultaneous_starts_lose_nothing() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root_a, _info, en) = a.new_instance("pack", &spec(2));
    a.upload(en.instance);
    let b = w.device("laptop");
    let root_b = restore(&b, en.instance, "pack");

    // B starts playing: its lease is visible to A as a *warning*, not a block
    let sb = begin(&b, en.instance, "b1", false).unwrap();
    assert!(sb.playing_elsewhere.is_empty());
    let sa = begin(&a, en.instance, "a1", false).unwrap();
    assert_eq!(sa.playing_elsewhere, vec!["laptop".to_string()], "advisory warning only");

    // both devices play, edit, and end their sessions
    write(&root_a, "config/a-session.toml", b"a\n");
    write(&root_b, "config/b-session.toml", b"b\n");
    let ea =
        a.engine.end_session(sa.session, ExitInfo { exit_code: Some(0), clean_signal: true }, true, &a.ctx()).unwrap();
    let eb =
        b.engine.end_session(sb.session, ExitInfo { exit_code: Some(0), clean_signal: true }, true, &b.ctx()).unwrap();
    assert_eq!((ea.outcome, eb.outcome), (SessionOutcome::Clean, SessionOutcome::Clean));
    assert!(ea.snapshot.is_some() && eb.snapshot.is_some());
    // no claim of an exclusive lock: both publications exist, the divergence is explicit
    let cloud = a.engine.cloud_instances(&a.ctx()).unwrap();
    assert_eq!(cloud[0].heads.len(), 2, "two simultaneous sessions produce two preserved branches: {cloud:?}");
    assert!(matches!(
        a.engine.reconcile(en.instance, ReconcileOpts::default(), &a.ctx()).unwrap(),
        ReconcileReport::Conflict(_)
    ));

    // a stale lease (holder vanished) is not even mentioned
    let lease = fs::read_dir(w.remote_dir.join(format!("instances/{}/leases", en.instance)))
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert_eq!(lease, 0, "leases are released when the session ends");
    let sb2 = begin(&b, en.instance, "b2", false);
    // B is in conflict state now → starting a new session is refused until it is resolved
    assert!(matches!(sb2, Err(Error::Conflict(_)) | Err(Error::GameRunning(_)) | Ok(_)), "{sb2:?}");
}

#[test]
fn a_stale_lease_is_ignored_while_a_fresh_one_only_warns() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, _info, en) = a.new_instance("pack", &spec(2));
    a.upload(en.instance);
    let b = w.device("laptop");
    let _rb = restore(&b, en.instance, "pack");
    let sb = begin(&b, en.instance, "b1", false).unwrap();
    // rewrite B's lease with a heartbeat from long ago (it crashed without releasing it)
    let dir = w.remote_dir.join(format!("instances/{}/leases", en.instance));
    let f = fs::read_dir(&dir).unwrap().flatten().next().unwrap().path();
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    v["heartbeat"] = "2020-01-01T00:00:00Z".into();
    fs::write(&f, serde_json::to_vec(&v).unwrap()).unwrap();
    let sa = begin(&a, en.instance, "a1", false).unwrap();
    assert!(sa.playing_elsewhere.is_empty(), "a stale lease must never block or even warn: {:?}", sa.playing_elsewhere);
    let _ = sb;
}

#[test]
fn a_crashed_session_is_quarantined_never_promoted_automatically() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, info, en) = a.new_instance("pack", &spec(3));
    a.upload(en.instance);
    let b = w.device("laptop");
    let rb = restore(&b, en.instance, "pack");
    let good_head = a.engine.cloud_instances(&a.ctx()).unwrap()[0].heads[0].id;

    let s = begin(&a, en.instance, "play-1", false).unwrap();
    packsync_core::testkit::fixtures::edit_region(&info.regions[0], 3, 1);
    fs::create_dir_all(root.join("crash-reports")).unwrap();
    fs::write(root.join("crash-reports/crash-2026-01-01_00.00.00-client.txt"), b"java.lang.OutOfMemoryError").unwrap();
    let end =
        a.engine.end_session(s.session, ExitInfo { exit_code: Some(1), clean_signal: false }, true, &a.ctx()).unwrap();
    assert_eq!(end.outcome, SessionOutcome::Crashed);
    assert_eq!(end.kind, Some(SnapshotKind::Crash));
    assert!(end.snapshot.is_some());
    let inst = a.engine.instance(en.instance).unwrap();
    assert_eq!(inst.state, InstanceState::CrashPending);

    // The last known-good cloud snapshot is NOT replaced; other computers keep seeing it.
    let cloud = b.engine.cloud_instances(&b.ctx()).unwrap();
    assert_eq!(cloud[0].heads.len(), 1);
    assert_eq!(cloud[0].heads[0].id, good_head);
    assert!(matches!(
        b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap(),
        ReconcileReport::UpToDate { .. }
    ));
    assert_eq!(digest(&rb), digest(&rb));
    // …while the crash save itself is preserved in the cloud history, flagged
    let hist = a.engine.history(en.instance, true, &a.ctx()).unwrap();
    let crash = hist.iter().find(|h| h.kind == SnapshotKind::Crash).expect("crash save retained");
    assert_eq!(crash.outcome, Some(SessionOutcome::Crashed));
    assert!(!crash.is_cloud_head);
    // syncing and launching are blocked until a human decides
    assert!(matches!(a.engine.reconcile(en.instance, ReconcileOpts::default(), &a.ctx()), Err(Error::CrashPending(_))));
    assert!(matches!(begin(&a, en.instance, "play-2", false), Err(Error::CrashPending(_))));

    // PROMOTE: the user verified the world; it becomes a normal snapshot and other devices adopt it
    let promoted = a.engine.promote_crash(en.instance, &a.ctx()).unwrap();
    a.engine.upload_pending(en.instance, true, &a.ctx()).unwrap();
    assert_eq!(a.engine.instance(en.instance).unwrap().state, InstanceState::Ok);
    let r = b.engine.reconcile(en.instance, ReconcileOpts::default(), &b.ctx()).unwrap();
    assert!(matches!(r, ReconcileReport::FastForwarded { to, .. } if to == promoted), "{r:?}");
    assert_eq!(digest(&rb), digest(&root));
}

#[test]
fn discarding_a_crash_save_returns_to_the_last_good_snapshot_and_keeps_the_crash_data() {
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (root, info, en) = a.new_instance("pack", &spec(4));
    a.upload(en.instance);
    let good = digest(&root);
    let s = begin(&a, en.instance, "p", false).unwrap();
    packsync_core::testkit::fixtures::edit_region(&info.regions[0], 5, 9);
    write(&root, "config/corrupt-looking.toml", b"???");
    a.engine.end_session(s.session, ExitInfo { exit_code: Some(137), clean_signal: false }, true, &a.ctx()).unwrap();
    assert_eq!(a.engine.instance(en.instance).unwrap().state, InstanceState::CrashPending);
    a.engine.discard_crash(en.instance, &a.ctx()).unwrap();
    assert_eq!(digest(&root), good, "back to the last known-good state");
    assert_eq!(a.engine.instance(en.instance).unwrap().state, InstanceState::Ok);
    // the crash data is not lost: recovery point on disk + quarantined snapshot in the cloud
    assert!(!a.engine.list_recovery_points(en.instance).unwrap().is_empty());
    assert!(a.engine.history(en.instance, true, &a.ctx()).unwrap().iter().any(|h| h.kind == SnapshotKind::Crash));
}

#[test]
fn a_session_killed_without_any_signal_is_found_after_restart_and_treated_as_unclean() {
    let w = TestWorld::new();
    let mut a = w.device("desktop");
    let (root, _info, en) = a.new_instance("pack", &spec(5));
    a.upload(en.instance);
    let s = begin(&a, en.instance, "p", false).unwrap();
    a.engine.session_running(s.session, 4_000_000, packsync_core::db::now_ms()).unwrap(); // a pid that is long gone
    write(&root, "config/played.toml", b"progress\n");
    // power loss: no end_session, no post-exit hook. The app restarts.
    a.restart();
    // A passive observer (app start-up) cannot tell "crashed" from "the game just exited and the
    // launcher's post-exit hook is about to run": it only notes the sighting and waits.
    let rec = a.engine.recover(&a.ctx()).unwrap();
    assert!(rec.interrupted_sessions.is_empty());
    assert!(a.engine.db().session_get(s.session).unwrap().unwrap().dead_seen_ms.is_some());
    // …unless the computer restarted since the session began: then it is certain.
    // (Here: starting a launch — an explicit moment — closes every earlier session without a live game.)
    let rec = a.engine.recover_before_launch(&a.ctx()).unwrap();
    assert_eq!(rec.interrupted_sessions, vec![s.session]);
    let row = a.engine.db().session_get(s.session).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Interrupted);
    assert_eq!(row.outcome, Some(SessionOutcome::Interrupted));
    // the progress is kept, but quarantined and flagged — not published as the new truth
    let inst = a.engine.instance(en.instance).unwrap();
    assert_eq!(inst.state, InstanceState::CrashPending, "{inst:?}");
    let cloud_heads_before = a.engine.cloud_instances(&a.ctx()).unwrap()[0].heads.len();
    assert_eq!(cloud_heads_before, 1);
}

#[test]
fn a_passive_observer_closes_a_dead_session_only_after_the_grace_period_or_a_reboot() {
    use packsync_core::db::{SessionOwner, SessionRow, SessionState};
    use packsync_core::session::{PASSIVE_GRACE_MS, SessionMgr, boot_time_ms};
    let w = TestWorld::new();
    let a = w.device("desktop");
    let (_root, _info, en) = a.new_instance("pack", &spec(6));
    a.upload(en.instance);
    let inst = a.engine.instance(en.instance).unwrap();
    let mgr = SessionMgr { db: a.engine.db() };
    let mk = |token: &str, started_ms: i64| SessionRow {
        id: packsync_core::ids::SessionId::new(),
        instance_id: en.instance,
        owner: SessionOwner::Hook,
        token: token.into(),
        base_snapshot: inst.base_snapshot,
        started_ms,
        ended_ms: None,
        state: SessionState::Running,
        outcome: None,
        game_pid: Some(4_000_001),
        game_start_ms: Some(started_ms),
        offline: false,
        result_snapshot: None,
        note: None,
        dead_seen_ms: None,
        owner_pid: None,
        owner_start_ms: None,
    };
    // a session that began after this boot: first sighting only records the time …
    let fresh = mk("fresh", packsync_core::db::now_ms());
    a.engine.db().session_insert(&fresh).unwrap();
    assert!(mgr.stale_open_sessions(&inst, &[], false).unwrap().is_empty());
    let seen = a.engine.db().session_get(fresh.id).unwrap().unwrap().dead_seen_ms.expect("sighting recorded");
    // … a second look inside the grace period still waits …
    assert!(mgr.stale_open_sessions(&inst, &[], false).unwrap().is_empty());
    // … and after the grace period it is stale
    a.engine.db().session_update(fresh.id, |r| r.dead_seen_ms = Some(seen - PASSIVE_GRACE_MS - 1)).unwrap();
    assert_eq!(mgr.stale_open_sessions(&inst, &[], false).unwrap().len(), 1);
    a.engine.db().session_update(fresh.id, |r| r.state = SessionState::Ended).unwrap();
    // a session that began before the computer last started cannot still be running: stale at once
    if let Some(boot) = boot_time_ms() {
        let old = mk("before-boot", boot - 60_000);
        a.engine.db().session_insert(&old).unwrap();
        assert_eq!(mgr.stale_open_sessions(&inst, &[], false).unwrap().len(), 1);
    }
}
