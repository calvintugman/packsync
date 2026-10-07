//! Play sessions: begin (recording the base snapshot), liveness, end, and crash classification.
//!
//! A *clean* end and a *crash* are different things and are kept apart: a crashed or interrupted
//! session produces a quarantined snapshot (`SnapshotKind::Crash`) that is never an automatic
//! head or fast-forward target. Prism only runs its post-exit hook after an exit code of 0, so a
//! session that was begun but never ended by a hook, with no live game process, is *interrupted*.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::db::{Db, InstanceRow, SessionOwner, SessionRow, SessionState, now_ms};
use crate::error::{Error, Result};
use crate::ids::{SessionId, SnapshotId};
use crate::manifest::SessionOutcome;
use crate::proc::{find_writers, pid_alive};

/// How long a passive observer waits after first seeing that a session's game is gone before it
/// closes the session as interrupted.
pub const PASSIVE_GRACE_MS: i64 = 10 * 60_000;

/// When the computer last started (ms since the epoch), if the platform says.
pub fn boot_time_ms() -> Option<i64> {
    let secs = sysinfo::System::boot_time();
    (secs > 0).then_some(secs as i64 * 1000)
}

pub struct SessionMgr<'a> {
    pub db: &'a Db,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Begin {
    /// A new session row was created.
    Created,
    /// The same launch token was already registered: idempotent no-op (duplicate hook/app job).
    AlreadyOwned,
}

/// Tolerance when comparing a process's start time (whole seconds) with a session's start.
const START_SLACK_MS: i64 = 2_000;

impl SessionMgr<'_> {
    /// Register the start of a play session. Idempotent per `token`; refuses when another session
    /// is genuinely active (its game process is alive or detectable).
    pub fn begin(
        &self,
        inst: &InstanceRow,
        owner: SessionOwner,
        token: &str,
        offline: bool,
        patterns: &[String],
    ) -> Result<(SessionRow, Begin)> {
        for s in self.db.sessions_open(inst.id)? {
            if s.token == token {
                return Ok((s, Begin::AlreadyOwned));
            }
            if self.is_alive(&s, &inst.root, patterns) {
                return Err(Error::Busy(format!(
                    "another play session of this instance is active (started by {})",
                    s.owner.as_str()
                )));
            }
            // a stale open session from a process that died: close it as interrupted
            self.db.session_update(s.id, |r| {
                r.state = SessionState::Interrupted;
                r.ended_ms = Some(now_ms());
                r.outcome = Some(SessionOutcome::Interrupted);
            })?;
        }
        let row = SessionRow {
            id: SessionId::new(),
            instance_id: inst.id,
            owner,
            token: token.to_string(),
            base_snapshot: inst.base_snapshot,
            started_ms: now_ms(),
            ended_ms: None,
            state: SessionState::Starting,
            outcome: None,
            game_pid: None,
            game_start_ms: None,
            offline,
            result_snapshot: None,
            note: None,
            dead_seen_ms: None,
            owner_pid: (owner == SessionOwner::App).then(std::process::id),
            owner_start_ms: (owner == SessionOwner::App)
                .then(|| crate::proc::process_start_ms(std::process::id()))
                .flatten(),
        };
        if !self.db.session_insert_if_absent(&row)? {
            // a concurrent request for the same launch got there first: that is the session
            if let Some(existing) = self.db.sessions_open(inst.id)?.into_iter().find(|s| s.token == token) {
                return Ok((existing, Begin::AlreadyOwned));
            }
            return Err(Error::Busy("another play session of this instance was just started".into()));
        }
        Ok((row, Begin::Created))
    }

    pub fn mark_running(&self, id: SessionId, pid: u32, start_ms: i64) -> Result<()> {
        self.db.session_update(id, |r| {
            r.state = SessionState::Running;
            r.game_pid = Some(pid);
            r.game_start_ms = Some(start_ms);
        })?;
        Ok(())
    }

    pub fn is_alive(&self, s: &SessionRow, root: &Path, patterns: &[String]) -> bool {
        if let Some(pid) = s.game_pid
            && pid_alive(pid, s.game_start_ms)
        {
            return true;
        }
        if s.game_pid.is_some() {
            return false; // its game is gone
        }
        // No game process recorded: a session started from Prism through the hooks never records one (it
        // stays `starting` for the whole game), and an app launch records it only once the game is found.
        // Such a session owns a game of this instance only if that game started after the session began —
        // never one that was already running, nor one another open session has recorded as its own.
        let others: Vec<u32> = self
            .db
            .sessions_open(s.instance_id)
            .map(|v| v.into_iter().filter(|o| o.id != s.id).filter_map(|o| o.game_pid).collect())
            .unwrap_or_default();
        find_writers(root, patterns, &others).iter().any(|w| match crate::proc::process_start_ms(w.pid) {
            // process start times have one-second granularity
            Some(t) => t + START_SLACK_MS >= s.started_ms,
            None => true, // unknown: assume it is ours rather than declare a live game's session over
        })
    }

    pub fn finish(
        &self,
        id: SessionId,
        outcome: SessionOutcome,
        result: Option<SnapshotId>,
        note: Option<String>,
    ) -> Result<SessionRow> {
        self.db.session_update(id, |r| {
            r.state =
                if outcome == SessionOutcome::Interrupted { SessionState::Interrupted } else { SessionState::Ended };
            r.ended_ms = Some(now_ms());
            r.outcome = Some(outcome);
            r.result_snapshot = result;
            r.note = note;
        })
    }

    /// Open sessions whose game is gone, i.e. that did not end through `end_session`.
    ///
    /// * `explicit == true` (a new launch is about to start, or the user asked): every open session
    ///   without a live game is over, no waiting.
    /// * `explicit == false` (a passive observer such as an app start or a hook's own start-up): the game
    ///   may have *just* exited and the launcher's post-exit hook may be about to end the session
    ///   cleanly. The first sighting is only recorded; the session becomes stale after
    ///   [`PASSIVE_GRACE_MS`] — or at once when the computer has restarted since it began (power loss).
    pub fn stale_open_sessions(
        &self,
        inst: &InstanceRow,
        patterns: &[String],
        explicit: bool,
    ) -> Result<Vec<SessionRow>> {
        let now = now_ms();
        let boot_ms = boot_time_ms();
        let mut out = vec![];
        for s in self.db.sessions_open(inst.id)? {
            if self.is_alive(&s, &inst.root, patterns) {
                if s.dead_seen_ms.is_some() {
                    self.db.session_update(s.id, |r| r.dead_seen_ms = None)?;
                }
                continue;
            }
            if explicit || boot_ms.is_some_and(|b| s.started_ms < b) {
                out.push(s);
                continue;
            }
            match s.dead_seen_ms {
                None => {
                    self.db.session_update(s.id, |r| r.dead_seen_ms = Some(now))?;
                }
                Some(t) if now - t >= PASSIVE_GRACE_MS => out.push(s),
                Some(_) => {}
            }
        }
        Ok(out)
    }
}

/// Decide how a session ended. `clean_signal` is true when the launcher told us the game exited
/// normally (Prism's post-exit hook, which only runs after exit code 0).
pub fn classify_exit(
    root: &Path,
    game_root: &str,
    started_ms: i64,
    exit_code: Option<i32>,
    clean_signal: bool,
) -> SessionOutcome {
    match exit_code {
        Some(0) => {
            if crash_artifacts_since(root, game_root, started_ms).is_empty() {
                return SessionOutcome::Clean;
            }
            return SessionOutcome::Crashed;
        }
        Some(_) => return SessionOutcome::Crashed,
        None => {}
    }
    if !crash_artifacts_since(root, game_root, started_ms).is_empty() {
        return SessionOutcome::Crashed;
    }
    if clean_signal { SessionOutcome::Clean } else { SessionOutcome::Interrupted }
}

/// Crash reports and JVM crash logs written since `started_ms`.
pub fn crash_artifacts_since(root: &Path, game_root: &str, started_ms: i64) -> Vec<String> {
    let since = SystemTime::UNIX_EPOCH + Duration::from_millis(started_ms.max(0) as u64);
    let base = if game_root.is_empty() { root.to_path_buf() } else { root.join(game_root) };
    let mut out = vec![];
    let newer = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).map(|t| t >= since).unwrap_or(false);
    if let Ok(rd) = std::fs::read_dir(base.join("crash-reports")) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("crash-") && newer(&e.path()) {
                out.push(format!("crash-reports/{}", e.file_name().to_string_lossy()));
            }
        }
    }
    for dir in [&base, &root.to_path_buf()] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("hs_err_pid") && n.ends_with(".log") && newer(&e.path()) {
                    out.push(n);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, InstanceState};
    use crate::ids::InstanceId;

    fn inst(root: &Path) -> InstanceRow {
        InstanceRow {
            id: InstanceId::new(),
            name: "t".into(),
            root: root.to_path_buf(),
            launcher: "generic".into(),
            launcher_ref: None,
            game_root: ".minecraft".into(),
            sync_enabled: true,
            base_snapshot: None,
            pending_rules: None,
            marker: "m".into(),
            root_dev: None,
            provider_id: None,
            state: InstanceState::Ok,
            state_detail: None,
            created_ms: now_ms(),
            last_scan_ms: None,
            last_full_verify_ms: None,
            last_synced_ms: None,
            needs_full_scan: false,
        }
    }

    #[test]
    fn begin_is_idempotent_per_token_and_refuses_a_second_live_session() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let i = inst(d.path());
        db.instance_insert(&i).unwrap();
        let m = SessionMgr { db: &db };
        let (a, how) = m.begin(&i, SessionOwner::Hook, "tok-1", false, &[]).unwrap();
        assert_eq!(how, Begin::Created);
        let (b, how) = m.begin(&i, SessionOwner::App, "tok-1", false, &[]).unwrap();
        assert_eq!((how, b.id), (Begin::AlreadyOwned, a.id), "duplicate hook/app job is a no-op");

        // a running session whose process is alive blocks a different token
        m.mark_running(a.id, std::process::id(), now_ms()).unwrap();
        assert!(matches!(m.begin(&i, SessionOwner::Hook, "tok-2", false, &[]), Err(Error::Busy(_))));
    }

    #[test]
    fn a_dead_open_session_is_closed_as_interrupted_when_a_new_one_starts() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let i = inst(d.path());
        db.instance_insert(&i).unwrap();
        let m = SessionMgr { db: &db };
        let (a, _) = m.begin(&i, SessionOwner::Hook, "t1", false, &[]).unwrap();
        m.mark_running(a.id, 4_000_000, now_ms()).unwrap(); // a pid that does not exist
        assert_eq!(m.stale_open_sessions(&i, &[], true).unwrap().len(), 1);
        let (b, _) = m.begin(&i, SessionOwner::Hook, "t2", false, &[]).unwrap();
        assert_ne!(a.id, b.id);
        let old = db.session_get(a.id).unwrap().unwrap();
        assert_eq!(old.state, SessionState::Interrupted);
        assert_eq!(old.outcome, Some(SessionOutcome::Interrupted));
    }

    #[test]
    fn crash_classification() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let start = now_ms() - 5_000;
        assert_eq!(classify_exit(root, ".minecraft", start, Some(0), true), SessionOutcome::Clean);
        assert_eq!(classify_exit(root, ".minecraft", start, Some(1), false), SessionOutcome::Crashed);
        assert_eq!(classify_exit(root, ".minecraft", start, None, true), SessionOutcome::Clean);
        assert_eq!(
            classify_exit(root, ".minecraft", start, None, false),
            SessionOutcome::Interrupted,
            "no clean signal and no exit code = unknown/unclean"
        );
        std::fs::create_dir_all(root.join(".minecraft/crash-reports")).unwrap();
        std::fs::write(root.join(".minecraft/crash-reports/crash-2026-01-01_00.00.00-client.txt"), b"boom").unwrap();
        assert_eq!(
            classify_exit(root, ".minecraft", start, Some(0), true),
            SessionOutcome::Crashed,
            "a fresh crash report overrides a 0 exit"
        );
        assert_eq!(
            classify_exit(root, ".minecraft", now_ms() + 60_000, None, true),
            SessionOutcome::Clean,
            "reports from before the session are ignored"
        );
        std::fs::write(root.join(".minecraft/hs_err_pid42.log"), b"jvm").unwrap();
        assert_eq!(crash_artifacts_since(root, ".minecraft", start).len(), 2);
    }
}
