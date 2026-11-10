use std::io::Write;
use std::process::{Command, Stdio};

use packsync_app::{HookOutcome, LaunchOptions};
use packsync_core::error::{Error, Result, exit};
use packsync_core::ids::InstanceId;
use packsync_core::launcher::HookPlan;
use packsync_core::manifest::SessionOutcome;

use super::{Cx, open_app};
use crate::args::{Cli, HookArgs, HookCmd, HooksCmd, LaunchArgs};
use crate::render::short;
use crate::resolve;
use crate::ui::Out;

fn print_plan(w: &mut dyn Write, plan: &HookPlan, title: &str) -> std::io::Result<()> {
    writeln!(w, "{title}")?;
    if plan.changes.is_empty() {
        writeln!(w, "  nothing to change")?;
    }
    for c in &plan.changes {
        writeln!(w, "  {}: {}", c.file.display(), c.key)?;
        writeln!(w, "      before: {}", c.before.as_deref().unwrap_or("(not set)"))?;
        writeln!(w, "      after:  {}", c.after.as_deref().unwrap_or("(removed)"))?;
    }
    for p in &plan.preserved_commands {
        writeln!(w, "  your command keeps running (chained): {p}")?;
    }
    for b in &plan.blockers {
        writeln!(w, "  CANNOT APPLY: {b}")?;
    }
    Ok(())
}

pub fn manage(cx: &Cx, h: HooksCmd) -> Result<()> {
    match h {
        HooksCmd::Status(a) => {
            let id = resolve::instance(&cx.app, &a.instance)?;
            let st = cx.app.hooks_state(id)?;
            cx.out.emit(&st, |w| {
                writeln!(
                    w,
                    "Hooks installed: {}{}",
                    if st.installed { "yes" } else { "no" },
                    if st.installed && !st.intact { " (but changed since — reinstall to repair)" } else { "" }
                )?;
                if let Some(p) = &st.pre_launch {
                    writeln!(w, "  pre-launch: {p}")?;
                }
                if let Some(p) = &st.post_exit {
                    writeln!(w, "  post-exit:  {p}")?;
                }
                Ok(())
            })
        }
        HooksCmd::Install { instance, cli, plan: only_plan } => {
            let id = resolve::instance(&cx.app, &instance)?;
            let exe = match cli {
                Some(p) => dunce::canonicalize(&p).map_err(|e| Error::io(format!("{}", p.display()), e))?,
                None => cx.app.resolve_cli_path()?,
            };
            let plan = cx.app.hooks_plan_install(id, &exe)?;
            cx.out.emit(&plan, |w| print_plan(w, &plan, "Installing the hooks would change:"))?;
            if only_plan {
                return Ok(());
            }
            if !plan.blockers.is_empty() {
                return Err(Error::Busy(plan.blockers.join("; ")));
            }
            if plan.changes.is_empty() {
                return Ok(());
            }
            if !cx.out.confirm("Apply these changes to the launcher configuration?")? {
                return Err(Error::Cancelled);
            }
            cx.app.hooks_apply(id, &plan)?;
            cx.out.note("Hooks installed. Prism now syncs this instance when you launch it from Prism.");
            Ok(())
        }
        HooksCmd::Uninstall { instance, plan: only_plan } => {
            let id = resolve::instance(&cx.app, &instance)?;
            let plan = cx.app.hooks_plan_remove(id)?;
            cx.out.emit(&plan, |w| print_plan(w, &plan, "Removing the hooks would change:"))?;
            if only_plan || plan.changes.is_empty() {
                return Ok(());
            }
            if !plan.blockers.is_empty() {
                return Err(Error::Busy(plan.blockers.join("; ")));
            }
            if !cx.out.confirm("Apply these changes to the launcher configuration?")? {
                return Err(Error::Cancelled);
            }
            cx.app.hooks_apply(id, &plan)?;
            cx.out.note("Hooks removed; your original commands are back.");
            Ok(())
        }
    }
}

fn run_chained(argv: &[String]) -> i32 {
    let Some((prog, rest)) = argv.split_first() else { return exit::OK };
    match Command::new(prog).args(rest).status() {
        Ok(s) => s.code().unwrap_or(exit::FAILURE),
        Err(e) => {
            eprintln!("PackSync: could not run your chained command {prog:?}: {e}");
            127
        }
    }
}

fn spawn_background_upload(id: InstanceId, cli: &Cli) {
    let Ok(me) = std::env::current_exe() else { return };
    let mut cmd = Command::new(me);
    if let Some(d) = &cli.config_dir {
        cmd.arg("--config-dir").arg(d);
    }
    if let Some(d) = &cli.state_dir {
        cmd.arg("--state-dir").arg(d);
    }
    cmd.args(["--quiet", "upload"])
        .arg(id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0800_0000); // DETACHED_PROCESS | CREATE_NO_WINDOW
    }
    let _ = cmd.spawn();
}

/// What Prism runs. Our part never makes the launch fail because PackSync itself is broken: only a
/// deliberate decision (conflict, crash waiting for you, strict offline policy) returns non-zero.
/// Then the user's own chained command runs and its status is passed through.
pub fn run_hook(cli: &Cli, h: &HookCmd, out: &Out) -> Result<i32> {
    let (args, pre): (&HookArgs, bool) = match h {
        HookCmd::PreLaunch(a) => (a, true),
        HookCmd::PostExit(a) => (a, false),
    };
    let ours = (|| -> Result<HookOutcome> {
        let id = InstanceId::parse(&args.instance)
            .map_err(|_| Error::invalid("hook", "--instance must be a PackSync instance ID"))?;
        let app = open_app(cli)?;
        let ctx = out.ctx();
        Ok(match if pre { app.hook_pre_launch(id, &ctx) } else { app.hook_post_exit(id, &ctx) } {
            HookOutcome::Ask { question, mut messages } => {
                let answer = crate::ask::yes_no("PackSync", &question, "Play anyway", "Don't play");
                messages.push(format!(
                    "{question} — {}",
                    match answer {
                        Some(true) => "answered: play anyway",
                        Some(false) => "answered: don't play",
                        None => "no dialog could be shown; playing (the warning is advisory)",
                    }
                ));
                if answer == Some(false) {
                    let _ = app.hook_post_exit(id, &ctx); // close the session this launch opened
                    HookOutcome::Block { message: "the launch was cancelled".into(), exit_code: 1 }
                } else {
                    HookOutcome::Proceed { messages, upload: None }
                }
            }
            other => other,
        })
    })();
    match ours {
        Ok(HookOutcome::Block { message, exit_code }) => {
            eprintln!("PackSync stopped the launch: {message}");
            return Ok(exit_code.max(1));
        }
        Ok(HookOutcome::Proceed { messages, upload }) => {
            for m in messages {
                eprintln!("PackSync: {m}");
            }
            if let (Some(id), false) = (upload, pre) {
                spawn_background_upload(id, cli);
            }
        }
        Ok(HookOutcome::Ask { .. }) => unreachable!("answered above"),
        Err(e) => eprintln!("PackSync could not run its part of the hook ({e}); continuing without syncing"),
    }
    Ok(run_chained(&args.chained))
}

pub fn launch(cx: &Cx, a: LaunchArgs) -> Result<i32> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let ctx = cx.out.ctx();
    let out = cx.app.launch(
        id,
        LaunchOptions {
            offline_player: a.offline_player,
            profile: a.profile,
            skip_sync: a.skip_sync,
            no_upload: a.no_upload,
            ..Default::default()
        },
        &ctx,
    )?;
    cx.out.emit(&out, |w| {
        for m in &out.warnings {
            writeln!(w, "note: {m}")?;
        }
        if !out.game_started {
            writeln!(w, "The game did not start.")?;
        }
        if let Some(e) = &out.end {
            match (e.outcome, e.snapshot) {
                (SessionOutcome::Clean, Some(s)) => writeln!(w, "Saved snapshot {}{}.", short(&s), if e.uploaded { " and uploaded it" } else { " (not uploaded yet)" })?,
                (SessionOutcome::Clean, None) => writeln!(w, "No changes to save.")?,
                (o, _) => writeln!(w, "The session ended abnormally ({o:?}). Its files were kept apart from your last good version: `packsync crash show`.")?,
            }
        }
        Ok(())
    })?;
    Ok(match &out.end {
        _ if !out.game_started => exit::LAUNCH_FAILED,
        Some(e) if e.outcome != SessionOutcome::Clean => exit::SAFETY_GUARD,
        Some(e) if e.snapshot.is_some() && !e.uploaded && !out.offline => exit::UPLOAD_PENDING,
        _ => exit::OK,
    })
}
