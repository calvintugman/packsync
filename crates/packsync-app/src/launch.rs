//! "Sync → play → save → upload" for one instance, and the launcher-hook entry points.
//!
//! **One component owns the sync of a launch.** When PackSync itself starts the game it owns the
//! session; installed launcher hooks then only report the clean-exit signal. When the game is started
//! from the launcher (hooks installed), the hooks own it. This is decided by the open session in the
//! local database, never by environment variables (a running Prism forwards `--launch` and the
//! variables never reach the hooks).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use packsync_core::db::{InstanceRow, SessionOwner, SessionRow, SessionState, now_ms};
use packsync_core::engine::{BeginSession, EndReport, ExitInfo, ReconcileOpts, ReconcileReport};
use packsync_core::error::{Error, ErrorCode, Result};
use packsync_core::ids::{InstanceId, SessionId};
use packsync_core::launcher::{LaunchRequest, LauncherRef};
use packsync_core::proc::{find_writers, pid_alive, process_start_ms};
use packsync_core::progress::{Level, OpCtx, Phase};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::events::AppEvent;
use crate::settings::OfflinePolicy;

#[derive(Debug, Clone)]
pub struct LaunchTiming {
    /// How often the process table is checked.
    pub poll: Duration,
    /// How long to wait for the game process to appear (downloads, sign-in prompts, ...).
    pub start_timeout: Duration,
    /// How long the launcher process may linger after the game ended before its status is ignored.
    pub exit_grace: Duration,
    /// How long to wait for the post-exit hook's clean-exit signal after the game ended.
    pub signal_wait: Duration,
}

impl Default for LaunchTiming {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(500),
            start_timeout: Duration::from_secs(15 * 60),
            exit_grace: Duration::from_secs(60),
            signal_wait: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchOptions {
    pub offline_player: Option<String>,
    pub profile: Option<String>,
    /// Do not contact the cloud before the game starts (play the local copy).
    #[serde(default)]
    pub skip_sync: bool,
    /// Do not upload after the game ends (the snapshot stays local until the next upload).
    #[serde(default)]
    pub no_upload: bool,
    #[serde(skip)]
    pub timing: LaunchTiming,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchOutcome {
    pub session: Option<SessionId>,
    pub pre_sync: Option<ReconcileReport>,
    /// The game started without confirming the cloud state (offline, or the cloud was unreachable).
    pub offline: bool,
    pub playing_elsewhere: Vec<String>,
    pub game_started: bool,
    /// Prism's exit status when PackSync started it itself (`None` when the launch was forwarded).
    pub launcher_exit_code: Option<i32>,
    pub clean_signal: bool,
    pub end: Option<EndReport>,
    pub warnings: Vec<String>,
}

/// What a hook should do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HookOutcome {
    /// Let the launch continue. `messages` are for the user's log.
    Proceed {
        messages: Vec<String>,
        /// Upload this instance's pending snapshots in the background (post-exit).
        upload: Option<InstanceId>,
    },
    /// Refuse to start the game (non-zero exit from the pre-launch command).
    Block { message: String, exit_code: i32 },
}

/// Errors that mean "the cloud is not reachable right now" rather than "something is wrong with the
/// local data": playing the local copy is allowed (policy permitting).
pub fn is_connectivity_error(e: &Error) -> bool {
    matches!(
        e.code(),
        ErrorCode::Offline
            | ErrorCode::StorageUnavailable
            | ErrorCode::RateLimited
            | ErrorCode::AuthRequired
            | ErrorCode::AuthRevoked
            | ErrorCode::AuthScope
            | ErrorCode::QuotaExceeded
            | ErrorCode::StoragePermission
    ) || matches!(e, Error::Config(m) if m.contains("no cloud storage"))
}

fn signals_dir(app: &App) -> std::path::PathBuf {
    app.paths.state_dir.join("signals")
}

fn signal_path(app: &App, s: SessionId) -> std::path::PathBuf {
    signals_dir(app).join(format!("{s}.clean"))
}

impl App {
    fn write_clean_signal(&self, s: SessionId) {
        let _ = std::fs::create_dir_all(signals_dir(self));
        let _ = std::fs::write(signal_path(self, s), b"clean");
    }

    fn take_clean_signal(&self, s: SessionId, wait: Duration, poll: Duration) -> bool {
        let p = signal_path(self, s);
        let end = Instant::now() + wait;
        loop {
            if p.exists() {
                let _ = std::fs::remove_file(&p);
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(poll.min(Duration::from_millis(200)));
        }
    }

    /// An open session started by the app, whether or not its game is still running: the app is
    /// responsible for closing it, the post-exit hook only reports the clean exit.
    fn open_app_session(&self, inst: &InstanceRow) -> Result<Option<SessionRow>> {
        Ok(self.engine().db().sessions_open(inst.id)?.into_iter().find(|s| s.owner == SessionOwner::App))
    }

    /// Sessions whose clean-exit signal arrived but whose owner never collected it (the app was closed
    /// or crashed while the game was running): close them as clean, once the signal is old enough that
    /// a live app would have taken it. Also forgets old signal files.
    pub fn close_signalled_sessions(&self, ctx: &OpCtx) -> usize {
        let Ok(rd) = std::fs::read_dir(signals_dir(self)) else { return 0 };
        let mut closed = 0;
        for e in rd.flatten() {
            let path = e.path();
            let age = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).unwrap_or_default();
            let Some(session) = path.file_stem().and_then(|n| n.to_str()).and_then(|n| SessionId::parse(n).ok()) else {
                continue;
            };
            if age > Duration::from_secs(24 * 3600) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if age < Duration::from_secs(60) {
                continue;
            }
            let Ok(Some(row)) = self.engine().db().session_get(session) else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            if !matches!(row.state, SessionState::Starting | SessionState::Running) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if row.game_pid.is_some_and(|p| pid_alive(p, row.game_start_ms)) {
                continue;
            }
            if self.engine().end_session(session, ExitInfo { exit_code: None, clean_signal: true }, false, ctx).is_ok()
            {
                closed += 1;
                let _ = std::fs::remove_file(&path);
            }
        }
        closed
    }

    /// An open session started by the app that still looks active: the hooks must stay out of the way.
    fn app_owned_session(&self, inst: &InstanceRow) -> Result<Option<SessionRow>> {
        let patterns = self.engine().launcher(&inst.launcher).game_process_patterns(&inst.root);
        let mgr = packsync_core::session::SessionMgr { db: self.engine().db() };
        Ok(self.engine().db().sessions_open(inst.id)?.into_iter().find(|s| {
            s.owner == SessionOwner::App
                && (mgr.is_alive(s, &inst.root, &patterns)
                    || (s.state == SessionState::Starting && s.started_ms > now_ms() - 30 * 60_000))
        }))
    }

    /// Reconcile before the game starts, applying the offline policy. Returns the report (when the
    /// cloud answered) and whether the game will start without a confirmed cloud state.
    fn pre_launch_sync(
        &self,
        id: InstanceId,
        ctx: &OpCtx,
        warnings: &mut Vec<String>,
    ) -> Result<(Option<ReconcileReport>, bool)> {
        let policy = self.settings().hook_offline_policy;
        let opts = ReconcileOpts { apply_fast_forward: true, upload_local: false };
        match self.engine().reconcile(id, opts, ctx) {
            Ok(ReconcileReport::Conflict(c)) => Err(Error::Conflict(format!(
                "this instance has diverged ({:?}): {} version(s) in the cloud and a different one on this computer. Choose which to continue before playing",
                c.kind,
                c.cloud.len()
            ))),
            Ok(ReconcileReport::Offline { reason, base }) => {
                if policy == OfflinePolicy::Block {
                    return Err(Error::Storage(packsync_core::storage::StorageError::Offline(reason)));
                }
                warnings.push(format!(
                    "The cloud could not be reached ({reason}). Playing the local copy; it is based on snapshot {}.",
                    base.map(|b| b.short()).unwrap_or_else(|| "(none)".into())
                ));
                Ok((None, true))
            }
            Ok(r) => Ok((Some(r), false)),
            Err(e) if is_connectivity_error(&e) => {
                if policy == OfflinePolicy::Block {
                    return Err(e);
                }
                warnings.push(format!("The cloud is not available ({e}). Playing the local copy."));
                Ok((None, true))
            }
            Err(e) => Err(e),
        }
    }

    /// Start the game for a synced instance: sync first, track the session, save and upload after.
    /// Blocks until the game has ended. Run it as a job from a UI.
    pub fn launch(self: &Arc<Self>, id: InstanceId, opts: LaunchOptions, ctx: &OpCtx) -> Result<LaunchOutcome> {
        let engine = self.engine();
        let inst = engine.instance(id)?;
        let launcher = engine.launcher(&inst.launcher);
        let launcher_ref: LauncherRef =
            inst.launcher_ref.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(LauncherRef {
                launcher: inst.launcher.clone(),
                data_dir: None,
                instance_id: None,
                executable: None,
            });
        let mut out = LaunchOutcome {
            session: None,
            pre_sync: None,
            offline: false,
            playing_elsewhere: vec![],
            game_started: false,
            launcher_exit_code: None,
            clean_signal: false,
            end: None,
            warnings: vec![],
        };
        let req = |token: &str| LaunchRequest {
            launcher_ref: launcher_ref.clone(),
            root: inst.root.clone(),
            offline_name: opts.offline_player.clone(),
            profile: opts.profile.clone(),
            env: vec![("PACKSYNC_SESSION".into(), token.to_string())],
        };
        if !inst.sync_enabled {
            // Not synced: just start it. There is nothing to protect or save.
            let mut h = launcher.launch(&req("unsynced"))?;
            if let Some(c) = h.child.as_mut() {
                let _ = c.try_wait();
            }
            out.game_started = true;
            out.warnings
                .push("Sync is not enabled for this instance; it was started without PackSync tracking.".into());
            return Ok(out);
        }

        // 1. bring this computer up to date (or decide to play offline)
        if !opts.skip_sync {
            ctx.phase(Phase::Launch, "checking the cloud before starting");
            let (rep, offline) = self.pre_launch_sync(id, ctx, &mut out.warnings)?;
            out.pre_sync = rep;
            out.offline = offline;
        } else {
            out.offline = true;
            out.warnings.push("Cloud check skipped: playing the local copy.".into());
        }

        // 2. open the session (records the base snapshot; refuses when the instance is in use)
        let token = format!("app-{}-{}", id, now_ms());
        let begun = engine.begin_session(
            id,
            BeginSession { owner: SessionOwner::App, token: token.clone(), offline: out.offline },
            ctx,
        )?;
        out.session = Some(begun.session);
        out.playing_elsewhere = begun.playing_elsewhere.clone();
        if !begun.playing_elsewhere.is_empty() {
            out.warnings.push(format!("{} appears to be playing this instance right now. If you both play, you will have to choose one version afterwards; nothing is lost.", begun.playing_elsewhere.join(", ")));
        }
        self.bus().emit(AppEvent::InstanceChanged { instance: id });

        // 3. lease heartbeats (advisory) while the session lives
        let stop = Arc::new(AtomicBool::new(false));
        let hb = if out.offline {
            None
        } else {
            let (app, stop, session) = (self.clone(), stop.clone(), begun.session);
            let every = Duration::from_secs((self.settings().lease_ttl_secs / 3).max(10) as u64);
            Some(std::thread::spawn(move || {
                let ctx = OpCtx::detached();
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                    if last.elapsed() >= every {
                        let _ = app.engine().lease_heartbeat(session, &ctx);
                        last = Instant::now();
                    }
                }
            }))
        };
        let finish = |stop: &Arc<AtomicBool>, hb: Option<std::thread::JoinHandle<()>>| {
            stop.store(true, Ordering::Relaxed);
            if let Some(h) = hb {
                let _ = h.join();
            }
        };

        // 4. start the launcher
        ctx.phase(Phase::Launch, "starting the launcher");
        let mut handle = match launcher.launch(&req(&token)) {
            Ok(h) => h,
            Err(e) => {
                finish(&stop, hb);
                // nothing started: close the session without a result
                let _ =
                    engine.end_session(begun.session, ExitInfo { exit_code: Some(0), clean_signal: true }, false, ctx);
                return Err(e);
            }
        };
        if handle.forwarded_to_running_launcher {
            out.warnings.push("Prism was already running, so it started the game itself. PackSync cannot read the game's exit status in this case; install the launcher hooks (or close Prism first) for exact crash detection.".into());
        }

        // 5. wait for the game process, then for it to end
        let patterns = launcher.game_process_patterns(&inst.root);
        let t = &opts.timing;
        let started = Instant::now();
        let forwarded = handle.forwarded_to_running_launcher;
        let mut game: Option<(u32, i64)> = None;
        let mut launcher_status: Option<i32> = None;
        // When the launcher exits cleanly before any game appears, a forwarded launch is the likely
        // explanation: keep looking, but not for the full start timeout unless we know it was forwarded.
        let mut give_up_after = t.start_timeout;
        let mut cancelled = false;
        while game.is_none() {
            if ctx.is_cancelled() {
                cancelled = true;
                break;
            }
            let mut found: Vec<(i64, u32)> = find_writers(&inst.root, &patterns, &[])
                .into_iter()
                .filter_map(|w| process_start_ms(w.pid).map(|ms| (ms, w.pid)))
                .collect();
            found.sort();
            if let Some((ms, pid)) = found.first().copied() {
                game = Some((pid, ms));
                break;
            }
            if let Some(c) = handle.child.as_mut()
                && let Ok(Some(status)) = c.try_wait()
            {
                handle.child = None;
                let code = status.code().unwrap_or(1);
                if !forwarded {
                    launcher_status = Some(code);
                    if code != 0 {
                        break; // the launcher failed before the game appeared
                    }
                    give_up_after = give_up_after.min(t.exit_grace);
                }
            }
            if started.elapsed() > give_up_after {
                out.warnings.push("The game did not start within the time limit; PackSync stopped waiting.".into());
                break;
            }
            std::thread::sleep(t.poll);
        }
        if let Some((pid, ms)) = game {
            out.game_started = true;
            let _ = engine.session_running(begun.session, pid, ms);
            ctx.phase(Phase::Launch, "the game is running");
            while pid_alive(pid, Some(ms)) {
                std::thread::sleep(t.poll);
            }
        }
        if let Some(mut c) = handle.child.take() {
            let end = Instant::now() + t.exit_grace;
            loop {
                match c.try_wait() {
                    Ok(Some(st)) => {
                        if !forwarded {
                            launcher_status = Some(st.code().unwrap_or(1));
                        }
                        break;
                    }
                    Ok(None) if Instant::now() < end => std::thread::sleep(t.poll.min(Duration::from_millis(100))),
                    _ => break,
                }
            }
        }
        out.launcher_exit_code = launcher_status;
        // The post-exit hook (when installed) reports a clean exit through a signal file.
        let hooks_installed = launcher.hook_state(&inst.root).map(|h| h.installed).unwrap_or(false);
        out.clean_signal = if hooks_installed && out.game_started {
            self.take_clean_signal(begun.session, t.signal_wait, t.poll)
        } else {
            false
        };

        // 6. save, and upload in the same call (the caller decides whether that is a background job)
        finish(&stop, hb);
        ctx.phase(Phase::Snapshot, "saving the world state");
        let exit = if game.is_none() {
            // The game never ran: nothing can have changed in a way that matters; do not blame it on a crash.
            ExitInfo { exit_code: Some(0), clean_signal: true }
        } else {
            ExitInfo { exit_code: launcher_status, clean_signal: out.clean_signal }
        };
        let rep = engine.end_session(begun.session, exit, !opts.no_upload && !out.offline, ctx)?;
        if let Some(e) = &rep.upload_error {
            out.warnings.push(format!("The snapshot is saved on this computer but not uploaded yet: {e}"));
        }
        out.end = Some(rep);
        self.bus().emit(AppEvent::InstanceChanged { instance: id });
        if cancelled {
            return Err(Error::Cancelled);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------------ hooks
    /// The launcher is about to start the game (Prism's pre-launch command).
    pub fn hook_pre_launch(&self, id: InstanceId, ctx: &OpCtx) -> HookOutcome {
        let mut messages = vec![];
        let engine = self.engine();
        let inst = match engine.instance(id) {
            Ok(i) if i.sync_enabled => i,
            Ok(_) => {
                return HookOutcome::Proceed {
                    messages: vec!["sync is not enabled for this instance".into()],
                    upload: None,
                };
            }
            Err(_) => {
                return HookOutcome::Proceed {
                    messages: vec!["this instance is not registered with PackSync on this computer".into()],
                    upload: None,
                };
            }
        };
        match self.app_owned_session(&inst) {
            Ok(Some(_)) => {
                return HookOutcome::Proceed {
                    messages: vec!["PackSync started this game; nothing to do in the hook".into()],
                    upload: None,
                };
            }
            Ok(None) => {}
            Err(e) => messages.push(format!("could not check for a running PackSync session: {e}")),
        }
        let block = self.settings().hook_block_on_problem;
        let blocked = |e: &Error, messages: Vec<String>| -> HookOutcome {
            if block {
                HookOutcome::Block {
                    message: format!("{e}{}", e.advice().map(|a| format!(" — {a}")).unwrap_or_default()),
                    exit_code: e.exit_code(),
                }
            } else {
                let mut m = messages;
                m.push(format!("PackSync did not sync this launch: {e}"));
                HookOutcome::Proceed { messages: m, upload: None }
            }
        };
        // finish whatever a previous run left half done (a stale session becomes a crash save here)
        self.close_signalled_sessions(ctx);
        if let Err(e) = engine.recover(ctx) {
            messages.push(format!("recovery check failed: {e}"));
        }
        let mut warnings = vec![];
        let (offline, _rep) = match self.pre_launch_sync(id, ctx, &mut warnings) {
            Ok((r, off)) => (off, r),
            Err(e) => return blocked(&e, messages),
        };
        messages.extend(warnings);
        let token = format!("hook-{}-{}", id, now_ms());
        match engine.begin_session(id, BeginSession { owner: SessionOwner::Hook, token, offline }, ctx) {
            Ok(b) => {
                if !b.playing_elsewhere.is_empty() {
                    messages.push(format!(
                        "{} appears to be playing this instance right now (advisory only).",
                        b.playing_elsewhere.join(", ")
                    ));
                }
                HookOutcome::Proceed { messages, upload: None }
            }
            Err(e) => blocked(&e, messages),
        }
    }

    /// The game ended normally (Prism's post-exit command, which Prism only runs after exit code 0).
    pub fn hook_post_exit(&self, id: InstanceId, ctx: &OpCtx) -> HookOutcome {
        let engine = self.engine();
        let Ok(inst) = engine.instance(id) else {
            return HookOutcome::Proceed {
                messages: vec!["this instance is not registered with PackSync".into()],
                upload: None,
            };
        };
        let mut messages = vec![];
        if let Ok(Some(s)) = self.open_app_session(&inst) {
            self.write_clean_signal(s.id);
            return HookOutcome::Proceed {
                messages: vec!["PackSync started this game and will save it".into()],
                upload: None,
            };
        }
        let sessions: Vec<SessionRow> = engine
            .db()
            .sessions_open(id)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.owner == SessionOwner::Hook)
            .collect();
        if sessions.is_empty() {
            messages.push("no PackSync session was open for this launch".into());
        }
        let mut upload = None;
        for s in sessions {
            match engine.end_session(s.id, ExitInfo { exit_code: None, clean_signal: true }, false, ctx) {
                Ok(rep) => {
                    messages.push(match rep.snapshot {
                        Some(sn) => format!("saved snapshot {} ({:?})", sn.short(), rep.outcome),
                        None => "nothing changed".into(),
                    });
                    if rep.snapshot.is_some() && !s.offline {
                        upload = Some(id);
                    }
                }
                Err(e) => messages.push(format!("could not save this session yet: {e}")),
            }
        }
        self.bus().emit(AppEvent::InstanceChanged { instance: id });
        let _ = ctx.note(Phase::Launch, Level::Info, "post-exit hook finished");
        HookOutcome::Proceed { messages, upload }
    }
}
