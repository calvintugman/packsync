//! Command implementations. Each returns a value that is both printed for humans and serialised for
//! `--json`; the exit code comes from the error taxonomy (`packsync_core::error::exit`).

mod admin;
mod conflict;
mod hooks;
mod instances;
mod restore;
mod storage;
mod sync;

use std::sync::Arc;

use packsync_app::{App, AppOptions, AppPaths};
use packsync_core::error::{Error, Result};

use crate::args::{Cli, Command};
use crate::ui::Out;

pub struct Cx {
    pub app: Arc<App>,
    pub out: Out,
}

pub fn paths_from(cli: &Cli) -> AppPaths {
    let mut p = AppPaths::discover();
    if let Some(c) = &cli.config_dir {
        p.config_dir = c.clone();
    }
    if let Some(s) = &cli.state_dir {
        p.logs_dir = s.join("logs");
        p.state_dir = s.clone();
    }
    p
}

pub fn open_app(cli: &Cli) -> Result<Arc<App>> {
    App::open(paths_from(cli), AppOptions::default())
}

pub fn run(cli: Cli) -> Result<i32> {
    let out = Out { json: cli.json, quiet: cli.quiet, yes: cli.yes };
    // The hook entry points must work even when PackSync itself is broken: they have their own error policy.
    if let Command::Hook(h) = &cli.command {
        return hooks::run_hook(&cli, h, &out);
    }
    let app = open_app(&cli)?;
    let cx = Cx { app, out };
    match cli.command {
        Command::Status(a) => instances::status(&cx, a)?,
        Command::Instances => instances::list(&cx)?,
        Command::Preview(a) => instances::preview(&cx, a)?,
        Command::Enable(a) => instances::enable(&cx, a)?,
        Command::Disable(a) => instances::disable(&cx, a)?,
        Command::Changes(a) => sync::changes(&cx, a)?,
        Command::Snapshot(a) => sync::snapshot(&cx, a)?,
        Command::Upload(a) => return sync::upload(&cx, a),
        Command::Sync(a) => return sync::sync_now(&cx, a),
        Command::Reconcile(a) => return sync::reconcile(&cx, a),
        Command::History(a) => sync::history(&cx, a)?,
        Command::Verify(a) => return sync::verify(&cx, a),
        Command::Restore(a) => restore::restore(&cx, a)?,
        Command::RestoreNew(a) => restore::restore_new(&cx, a)?,
        Command::Cloud => restore::cloud(&cx)?,
        Command::Recovery(a) => restore::recovery(&cx, a)?,
        Command::Conflicts(a) => return conflict::conflicts(&cx, a),
        Command::Resolve(a) => conflict::resolve(&cx, a)?,
        Command::Crash(c) => conflict::crash(&cx, c)?,
        Command::Rules(r) => instances::rules(&cx, r)?,
        Command::Hooks(h) => hooks::manage(&cx, h)?,
        Command::Launch(a) => return hooks::launch(&cx, a),
        Command::Storage(s) => storage::storage(&cx, s)?,
        Command::Auth(a) => storage::auth(&cx, a)?,
        Command::Settings(s) => storage::settings(&cx, s)?,
        Command::Device(d) => storage::device(&cx, d)?,
        Command::Doctor(d) => admin::doctor(&cx, d)?,
        Command::Gc(g) => admin::gc(&cx, g)?,
        Command::DevBridge(b) => admin::dev_bridge(&cx, b)?,
        Command::Hook(_) => unreachable!("handled above"),
    }
    Ok(packsync_core::error::exit::OK)
}

/// Print an error the way a person (or a script reading `--json`) needs it, and return its exit code.
pub fn report_error(e: &Error, json: bool) -> i32 {
    let view = packsync_app::ErrorView::from(e);
    if json {
        let mut doc = serde_json::json!({ "ok": false, "error": view });
        // what the command found before it failed (for example `verify --remote`'s report)
        if let Some(result) = crate::ui::take_pending_result() {
            doc["result"] = result;
        }
        crate::ui::print_json(&doc);
    } else {
        eprintln!("error: {}", view.message);
        if let Some(a) = &view.advice {
            eprintln!("  {a}");
        }
    }
    view.exit_code
}
