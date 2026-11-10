//! State shared by the several processes that open one database (the desktop app, the CLI, the launch
//! hooks' background steps), and state of a long-lived process.

use packsync_core::engine::*;
use packsync_core::ids::JobId;
use packsync_core::testkit::fixtures::FixtureSpec;
use packsync_core::testkit::harness::TestWorld;

/// Found by review: every engine start marked all running jobs interrupted, so a CLI command or a hook
/// started while the app uploaded made the app's upload look interrupted.
#[test]
fn opening_another_engine_keeps_a_live_process_job_running_and_interrupts_a_dead_one() {
    let dir = tempfile::tempdir().unwrap();
    let first = Engine::open(EngineConfig::new(dir.path().to_path_buf())).unwrap();
    let live = JobId::new();
    first.db().job_register(live, "instance.upload", None, None).unwrap();
    // a job left behind by a process that no longer exists
    let dead = JobId::new();
    first.db().job_register(dead, "instance.upload", None, None).unwrap();
    first
        .db()
        .with(|c| {
            c.execute("UPDATE jobs SET owner_pid=4000000 WHERE id=?1", [dead.to_string()])?;
            Ok(())
        })
        .unwrap();

    let _second = Engine::open(EngineConfig::new(dir.path().to_path_buf())).unwrap();
    let state = |id: JobId| first.db().jobs_recent(10).unwrap().into_iter().find(|r| r.id == id).unwrap().state;
    assert_eq!(state(live), "running", "its owner process is still alive");
    assert_eq!(state(dead), "interrupted", "its owner is gone");
}

/// Found by review: the clock that decides when files are due for periodic verification was read once,
/// when the engine was configured, so a long-lived process (the desktop app) never verified again.
#[test]
fn a_long_lived_engine_verifies_files_once_their_period_has_passed() {
    let w = TestWorld::new();
    let a = w.device_with("desktop", |cfg| {
        cfg.snapshot.scan.verify_period_ns = 1_000_000_000;
        cfg.snapshot.scan.racy_window_ns = 0;
    });
    let (_, _, en) = a.new_instance("pack", &FixtureSpec::tiny());
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let report = a.engine.snapshot(en.instance, SnapshotOpts::default(), &a.ctx()).unwrap();
    assert!(report.stats.hashed_files > 0, "the period passed, so files are verified: {:?}", report.stats);
}
