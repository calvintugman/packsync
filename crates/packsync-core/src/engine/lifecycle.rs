//! Play sessions, advisory leases and startup recovery.
//!
//! Leases are **advisory**: they warn that another device appears to be playing, nothing more.
//! Drive offers no way to make acquisition exclusive (no compare-and-swap, non-unique names), so we
//! never claim a distributed lock. Two devices that start at the same moment both play; the result
//! is two snapshots from one parent — an explicit, preserved divergence — never a lost update.

use super::*;
use crate::db::{InstanceState, SessionRow, SessionState, SnapshotState};
use crate::journal::{self, RecoveryAction};
use crate::manifest::{SessionOutcome, SessionSummary, SnapshotKind, now_rfc3339};
use crate::progress::{Level, Phase};
use crate::remote::{LEASE_FORMAT, LeaseRecord};
use crate::restore;
use crate::session::{SessionMgr, classify_exit, crash_artifacts_since};
use crate::snapshot::staging_root;

fn rfc3339_from_ms(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(now_rfc3339)
}

/// How long leases read ahead of a launch stand in for a fresh read.
const LEASES_READ_AHEAD_FOR: std::time::Duration = std::time::Duration::from_secs(120);

impl Engine {
    /// Read who is playing `id` elsewhere now, for the next [`Engine::begin_session`]: a launch does this
    /// while it checks the cloud, instead of after. Best effort: without an answer the session start reads.
    pub fn prefetch_leases(&self, id: InstanceId, ctx: &OpCtx) {
        if !self.has_storage() {
            return;
        }
        if let Ok(remote) = self.remote()
            && let Ok(leases) = remote.list_leases(id, ctx)
        {
            self.lease_cache.lock().insert(id, (std::time::Instant::now(), leases));
        }
    }

    /// Register the start of a play session. Idempotent per token. Returns advisory warnings about
    /// other devices; **never blocks** on them.
    pub fn begin_session(&self, id: InstanceId, req: BeginSession, ctx: &OpCtx) -> Result<SessionBegun> {
        let inst = self.instance(id)?;
        self.require_state(&inst, &[InstanceState::Ok])?;
        self.check_marker(&inst)?;
        let launcher = self.launcher(&inst.launcher);
        let patterns = launcher.game_process_patterns(&inst.root);
        let mgr = SessionMgr { db: &self.db };
        // A new session may only start when nothing is using the instance (a retry with a known
        // token is the idempotent path and is not re-checked).
        if !self.db.sessions_open(id)?.iter().any(|s| s.token == req.token) {
            self.ensure_idle(&inst)?;
        }
        let (row, how) = mgr.begin(&inst, req.owner, &req.token, req.offline, &patterns)?;
        let mut playing = vec![];
        if how == crate::session::Begin::Created
            && !req.offline
            && self.has_storage()
            && let Ok(remote) = self.remote()
        {
            let now = time::OffsetDateTime::now_utc();
            let ahead = self.lease_cache.lock().remove(&id).filter(|(t, _)| t.elapsed() < LEASES_READ_AHEAD_FOR);
            if let Ok(leases) = ahead.map(|(_, l)| Ok(l)).unwrap_or_else(|| remote.list_leases(id, ctx)) {
                playing = leases
                    .iter()
                    .filter(|l| l.device_id != self.device().0 && !l.is_stale(now))
                    .map(|l| l.device_name.clone())
                    .collect();
            }
            if !self.leases_deferred() {
                let _ = self.write_lease(&inst, &row, ctx); // best effort (offline is fine)
            }
        }
        Ok(SessionBegun {
            session: row.id,
            already_owned: how == crate::session::Begin::AlreadyOwned,
            base: row.base_snapshot,
            playing_elsewhere: playing,
        })
    }

    fn write_lease(&self, inst: &InstanceRow, s: &SessionRow, ctx: &OpCtx) -> Result<()> {
        let remote = self.remote()?;
        remote.put_lease(
            inst.id,
            &LeaseRecord {
                format: LEASE_FORMAT.into(),
                version: 1,
                device_id: self.device().0,
                device_name: self.device().1.clone(),
                session: s.id,
                started: rfc3339_from_ms(s.started_ms),
                heartbeat: now_rfc3339(),
                ttl_secs: self.cfg.lease_ttl_secs,
                base: s.base_snapshot,
            },
            ctx,
        )
    }

    /// Refresh the advisory lease. Metadata-only traffic (one tiny object), never content.
    /// See [`Engine::lease_for_open_session`] and [`Engine::release_lease`]: session start and end do not
    /// touch the cloud lease themselves (the launch hooks do it from a background process).
    pub fn defer_leases(&self, defer: bool) {
        self.defer_leases.store(defer, std::sync::atomic::Ordering::Relaxed);
    }

    fn leases_deferred(&self) -> bool {
        self.defer_leases.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Write the advisory lease of this computer's open session of `id` (after a deferred start).
    pub fn lease_for_open_session(&self, id: InstanceId, ctx: &OpCtx) -> Result<()> {
        let inst = self.instance(id)?;
        match self.db.sessions_open(id)?.into_iter().next() {
            Some(s) => self.write_lease(&inst, &s, ctx),
            None => Ok(()),
        }
    }

    /// Keep this computer's lease of `id` fresh while its hook session lasts (the background `hook lease`
    /// step): refresh every `every`, stop when the session has ended, or when no game process of the
    /// instance has been seen once `startup` has passed (the game never started, or died without the
    /// post-exit hook — Prism runs that only after a clean exit). Returns how many refreshes were written.
    pub fn keep_lease_fresh(
        &self,
        id: InstanceId,
        every: std::time::Duration,
        startup: std::time::Duration,
        ctx: &OpCtx,
    ) -> Result<u32> {
        let started = std::time::Instant::now();
        let mut written = 0;
        loop {
            let Some(s) = self.db.sessions_open(id)?.into_iter().next() else { return Ok(written) };
            if started.elapsed() > startup && !self.game_process_running(id)? {
                return Ok(written);
            }
            let inst = self.instance(id)?;
            if self.write_lease(&inst, &s, ctx).is_ok() {
                written += 1;
            }
            if !self.sleeper.sleep(every, ctx) {
                return Ok(written); // cancelled
            }
        }
    }

    /// Is a game process of this instance running (found by its command line or working directory, or
    /// holding a world's lock)?
    pub fn game_process_running(&self, id: InstanceId) -> Result<bool> {
        let inst = self.instance(id)?;
        let patterns = self.launcher(&inst.launcher).game_process_patterns(&inst.root);
        let game_dir = if inst.game_root.is_empty() { inst.root.clone() } else { inst.root.join(&inst.game_root) };
        Ok(!crate::proc::find_writers(&inst.root, &patterns, &[]).is_empty()
            || !crate::proc::world_locks_held(&game_dir).is_empty())
    }

    /// Remove this computer's advisory lease of `id` unless a session is open again (after a deferred end).
    pub fn release_lease(&self, id: InstanceId, ctx: &OpCtx) -> Result<()> {
        if !self.db.sessions_open(id)?.is_empty() || !self.has_storage() {
            return Ok(());
        }
        self.remote()?.delete_lease(id, self.device().0, ctx)
    }

    pub fn lease_heartbeat(&self, session: crate::ids::SessionId, ctx: &OpCtx) -> Result<()> {
        let s = self.db.session_get(session)?.ok_or_else(|| Error::NotFound(format!("session {session}")))?;
        let inst = self.instance(s.instance_id)?;
        self.write_lease(&inst, &s, ctx)
    }

    pub fn session_running(&self, session: crate::ids::SessionId, pid: u32, start_ms: i64) -> Result<()> {
        SessionMgr { db: &self.db }.mark_running(session, pid, start_ms)
    }

    /// The game has stopped. Classify how it ended, create the end-of-session snapshot (quarantined
    /// when the session crashed) and optionally upload it.
    pub fn end_session(
        &self,
        session: crate::ids::SessionId,
        exit: ExitInfo,
        upload: bool,
        ctx: &OpCtx,
    ) -> Result<EndReport> {
        let s = self.db.session_get(session)?.ok_or_else(|| Error::NotFound(format!("session {session}")))?;
        if s.state == SessionState::Ended {
            return Ok(EndReport {
                outcome: s.outcome.unwrap_or(SessionOutcome::Clean),
                snapshot: s.result_snapshot,
                kind: None,
                crash_artifacts: vec![],
                uploaded: false,
                upload_error: None,
            });
        }
        let inst = self.instance(s.instance_id)?;
        let ctx = &ctx.clone().with_instance(inst.id);
        let _lock = self.lock(inst.id, "end-session")?;
        // The JVM may still be flushing the world: give writers a moment to disappear.
        let mut waited = 0;
        while let Err(e) = self.ensure_idle_ignoring_session(&inst, s.id) {
            waited += 1;
            if waited > 40 {
                return Err(e);
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let outcome = classify_exit(&inst.root, &inst.game_root, s.started_ms, exit.exit_code, exit.clean_signal);
        let artifacts = crash_artifacts_since(&inst.root, &inst.game_root, s.started_ms);
        let quarantine = outcome != SessionOutcome::Clean || inst.state == InstanceState::CrashPending;
        let kind = if quarantine { SnapshotKind::Crash } else { SnapshotKind::Normal };
        let summary = SessionSummary {
            id: s.id,
            started: rfc3339_from_ms(s.started_ms),
            ended: Some(now_rfc3339()),
            outcome: Some(outcome),
            base: s.base_snapshot,
            offline: s.offline,
        };
        let note = match outcome {
            SessionOutcome::Clean if quarantine => Some("session ended normally but started from an unverified crash save".to_string()),
            SessionOutcome::Clean => None,
            SessionOutcome::Crashed => Some(format!("the game crashed ({}); kept apart from the last known-good snapshot", if artifacts.is_empty() { "nonzero exit".into() } else { artifacts.join(", ") })),
            SessionOutcome::Interrupted => Some("the game ended without a clean exit signal (killed or power loss?); kept apart from the last known-good snapshot".to_string()),
        };
        let rep = self.snapshot_locked(
            &inst,
            SnapshotOpts { kind, session: Some(summary), note, allow_running: true, ..Default::default() },
            ctx,
        );
        let (snap, rep) = match rep {
            Ok(r) => (
                match r.result {
                    SnapshotResult::Created { id } => Some(id),
                    SnapshotResult::Unchanged { .. } => None,
                },
                Some(r),
            ),
            Err(e) => {
                // keep the session open so it can be retried; never lose track of it
                return Err(e);
            }
        };
        let _ = rep;
        if let (Some(_), true) = (snap, quarantine) {
            self.db.instance_update(inst.id, |r| {
                r.state = InstanceState::CrashPending;
                r.state_detail = Some(match outcome {
                    SessionOutcome::Crashed => format!(
                        "the last session crashed{}",
                        if artifacts.is_empty() { String::new() } else { format!(" ({})", artifacts.join(", ")) }
                    ),
                    _ => "the last session ended abnormally".to_string(),
                });
            })?;
        }
        SessionMgr { db: &self.db }.finish(s.id, outcome, snap, None)?;
        if !self.leases_deferred()
            && self.has_storage()
            && let Ok(remote) = self.remote()
        {
            let _ = remote.delete_lease(inst.id, self.device().0, ctx);
        }
        let (mut uploaded, mut upload_error) = (false, None);
        if upload && snap.is_some() {
            match self.upload_pending(inst.id, false, ctx) {
                Ok(sum) => uploaded = sum.deferred_until_ms.is_none(),
                Err(e) => upload_error = Some(e.to_string()),
            }
        }
        Ok(EndReport {
            outcome,
            snapshot: snap,
            kind: snap.map(|_| kind),
            crash_artifacts: artifacts,
            uploaded,
            upload_error,
        })
    }

    fn ensure_idle_ignoring_session(&self, inst: &InstanceRow, _session: crate::ids::SessionId) -> Result<()> {
        // `ensure_idle` also consults open sessions; for ending a session we only care about processes.
        let launcher = self.launcher(&inst.launcher);
        let patterns = launcher.game_process_patterns(&inst.root);
        let game_dir = if inst.game_root.is_empty() { inst.root.clone() } else { inst.root.join(&inst.game_root) };
        let mut found: Vec<String> = crate::proc::find_writers(&inst.root, &patterns, &[])
            .into_iter()
            .map(|w| format!("{} (pid {})", w.name, w.pid))
            .collect();
        found.extend(
            crate::proc::world_locks_held(&game_dir).into_iter().map(|p| format!("world {} is open", p.display())),
        );
        if found.is_empty() { Ok(()) } else { Err(Error::GameRunning(found.join("; "))) }
    }

    // ------------------------------------------------------------------------------ recovery

    /// Startup recovery: unfinished restore transactions, half-restored new instances, sessions
    /// that died, orphaned staging. Idempotent; safe to run on every start. Passive: a session whose
    /// game has only just disappeared is left for its post-exit hook (see [`Engine::recover_before_launch`]).
    pub fn recover(&self, ctx: &OpCtx) -> Result<RecoveryReport> {
        self.recover_with(ctx, false)
    }

    /// Like [`Engine::recover`] for the moment a new launch begins: every earlier session without a
    /// live game is over, so it is closed now.
    pub fn recover_before_launch(&self, ctx: &OpCtx) -> Result<RecoveryReport> {
        self.recover_with(ctx, true)
    }

    fn recover_with(&self, ctx: &OpCtx, explicit: bool) -> Result<RecoveryReport> {
        let mut rep = RecoveryReport::default();
        ctx.phase(Phase::Recover, "checking for interrupted operations");
        for inst in self.db.instances()? {
            if !inst.root.is_dir() {
                continue;
            }
            // Another process may be working on this very instance right now: never recover under it.
            let Ok(busy_guard) = self.lock(inst.id, "recover") else { continue };
            for txd in journal::pending_txs(&inst.root) {
                let action = match journal::recover_tx(&inst.root, &txd) {
                    Ok(a) => a,
                    Err(e) if crate::failpoint::is_crash(&e) => return Err(e), // the process "died" here
                    Err(e) => {
                        // a damaged or forged journal: leave it, say so, and carry on with everything else
                        let msg = format!("an interrupted restore could not be recovered ({e}); nothing was changed");
                        self.db.event(Some(inst.id), "error", "restore_recovery_failed", &msg);
                        rep.could_not_recover.push((inst.id, msg));
                        continue;
                    }
                };
                match action {
                    RecoveryAction::RolledBack => {
                        rep.rolled_back.push(inst.id);
                        self.db.event(
                            Some(inst.id),
                            "warn",
                            "restore_rolled_back",
                            "an interrupted restore was rolled back; the instance is exactly as it was before",
                        );
                    }
                    RecoveryAction::Discarded | RecoveryAction::Nothing => rep.discarded_transactions += 1,
                    RecoveryAction::RollForward { target, tx, base, derive_note } => {
                        match self.manifest(inst.id, target, ctx) {
                            Ok(m) => {
                                restore::roll_forward(&self.db, &inst, &m, target, tx)?;
                                // A restore the user asked for must stick: record it on top of what was
                                // there before, exactly as the uninterrupted path does. Otherwise the
                                // next sync would see an older base behind the cloud head and quietly
                                // fast-forward away from the restored state.
                                if let Some(note) = derive_note
                                    && base != Some(target)
                                {
                                    let n = self.derive(
                                        &m,
                                        base.into_iter().collect(),
                                        SnapshotKind::Normal,
                                        note,
                                        None,
                                        None,
                                    )?;
                                    self.db.instance_update(inst.id, |r| r.base_snapshot = Some(n))?;
                                }
                                rep.rolled_forward.push(inst.id);
                                self.db.event(
                                    Some(inst.id),
                                    "info",
                                    "restore_completed",
                                    "an interrupted restore had applied everything; it was completed",
                                );
                            }
                            Err(e) => {
                                // we cannot confirm the target: keep the tx for the next attempt
                                self.db.event(
                                    Some(inst.id),
                                    "warn",
                                    "restore_pending",
                                    &format!("could not finish an interrupted restore yet: {e}"),
                                );
                                continue;
                            }
                        }
                    }
                }
            }
            let fresh = self.instance(inst.id)?;
            if fresh.state == InstanceState::Restoring && journal::pending_txs(&fresh.root).is_empty() {
                self.db.instance_update(inst.id, |r| {
                    r.state = InstanceState::Ok;
                    r.state_detail = None;
                })?;
            }
            // Closing abandoned sessions takes the instance lock itself: release ours first.
            drop(busy_guard);
            // sessions whose game is gone and that were never closed
            let launcher = self.launcher(&inst.launcher);
            let patterns = launcher.game_process_patterns(&inst.root);
            let mgr = SessionMgr { db: &self.db };
            for s in mgr.stale_open_sessions(&inst, &patterns, explicit)? {
                rep.interrupted_sessions.push(s.id);
                match self.end_session(s.id, ExitInfo { exit_code: None, clean_signal: false }, false, ctx) {
                    Ok(_) => {}
                    Err(e) => self.db.event(
                        Some(inst.id),
                        "warn",
                        "stale_session",
                        &format!("could not close an abandoned session: {e}"),
                    ),
                }
            }
        }
        let locks = self.locks_dir();
        let in_progress = |i: InstanceId| {
            crate::lock::InstanceLock::try_acquire_kind(&locks, i, "restore-new", "recovery check").is_err()
        };
        for (path, removed) in restore::recover_pending_restores(&self.db, &in_progress) {
            if removed {
                rep.removed_partial_restores.push(path);
            }
        }
        rep.orphan_staging_removed = self.clean_orphan_staging()?;
        ctx.note(Phase::Recover, Level::Info, "recovery check complete");
        Ok(rep)
    }

    fn clean_orphan_staging(&self) -> Result<u32> {
        let root = staging_root(&self.cfg.state_dir);
        let Ok(rd) = std::fs::read_dir(&root) else { return Ok(0) };
        let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
        for inst in self.db.instances()? {
            for s in self.db.snapshots(inst.id)? {
                if matches!(s.state, SnapshotState::LocalComplete | SnapshotState::Uploading) {
                    known.insert(s.id.to_hex());
                }
            }
        }
        let mut n = 0;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|d| d.as_secs() > 3600).unwrap_or(false))
                .unwrap_or(false);
            let orphan = if name.starts_with("tmp-") { old } else { !known.contains(&name) && old };
            if orphan && std::fs::remove_dir_all(e.path()).is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }
}
