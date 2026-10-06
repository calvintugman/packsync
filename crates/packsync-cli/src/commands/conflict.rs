use std::io::Write;

use packsync_core::db::InstanceState;
use packsync_core::engine::{ReconcileOpts, ReconcileReport, Resolution, SnapshotSummary};
use packsync_core::error::{Error, Result, exit};
use packsync_core::manifest::SnapshotKind;

use super::Cx;
use crate::args::{CrashCmd, InstanceArg, ResolveArgs};
use crate::render::{bytes, short, when};
use crate::resolve;

fn print_version(w: &mut dyn Write, label: &str, s: &SnapshotSummary) -> std::io::Result<()> {
    writeln!(
        w,
        "  {label}: snapshot {} from {} ({}), {} files, {}",
        short(&s.id),
        s.device.as_deref().unwrap_or("?"),
        s.created.as_deref().map(when).unwrap_or_else(|| "?".into()),
        s.files,
        bytes(s.bytes)
    )?;
    for wd in &s.worlds {
        writeln!(w, "      world {}", wd.name.as_deref().unwrap_or(&wd.folder))?;
    }
    if let Some(n) = &s.note {
        writeln!(w, "      {n}")?;
    }
    Ok(())
}

pub fn conflicts(cx: &Cx, a: InstanceArg) -> Result<i32> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let rep = cx.app.engine().reconcile(
        id,
        ReconcileOpts { apply_fast_forward: false, upload_local: false },
        &cx.out.ctx(),
    )?;
    match &rep {
        ReconcileReport::Conflict(c) => {
            cx.out.emit(&rep, |w| {
                writeln!(w, "This instance has diverged ({:?}). Both versions are safe; nothing was overwritten.", c.kind)?;
                if let Some(l) = &c.local {
                    print_version(w, if c.local_dirty { "This computer (with unsaved changes)" } else { "This computer" }, l)?;
                }
                for (i, s) in c.cloud.iter().enumerate() {
                    print_version(w, &format!("Cloud version {}", i + 1), s)?;
                }
                writeln!(w, "\nChoose with `packsync resolve <instance> keep-local | keep-cloud [--head ID] | split --name NEW`.")?;
                writeln!(w, "  keep-local  continue with this computer's version (the cloud's stays in history)")?;
                writeln!(w, "  keep-cloud  continue with the cloud's version (this computer's is saved first)")?;
                writeln!(w, "  split       keep this computer's version as a separate new instance")
            })?;
            Ok(exit::CONFLICT)
        }
        other => {
            cx.out.emit(other, |w| {
                writeln!(
                    w,
                    "No conflict: {}",
                    match other {
                        ReconcileReport::UpToDate { .. } => "up to date",
                        ReconcileReport::CloudAhead { .. } => "the cloud is ahead (a normal update)",
                        ReconcileReport::LocalAhead { .. } => "this computer is ahead (a normal upload)",
                        _ => "nothing to decide",
                    }
                )
            })?;
            Ok(exit::OK)
        }
    }
}

pub fn resolve(cx: &Cx, a: ResolveArgs) -> Result<()> {
    let ctx = cx.out.ctx();
    let id = resolve::instance(&cx.app, &a.instance)?;
    let resolution = match a.choice.as_str() {
        "keep-local" => Resolution::KeepLocal,
        "keep-cloud" => Resolution::KeepCloud {
            head: match &a.head {
                Some(t) => Some(resolve::snapshot(&cx.app, id, t, true, &ctx)?),
                None => None,
            },
        },
        "split" => Resolution::SplitLocal {
            new_name: a
                .name
                .clone()
                .ok_or_else(|| Error::invalid("resolve", "split needs --name for the new instance"))?,
        },
        other => {
            return Err(Error::invalid(
                "resolve",
                format!("unknown choice {other:?}; use keep-local, keep-cloud or split"),
            ));
        }
    };
    let what = match &resolution {
        Resolution::KeepLocal => "Continue with this computer's version?",
        Resolution::KeepCloud { .. } => "Continue with the cloud's version? This computer's version is saved first.",
        Resolution::SplitLocal { .. } => "Keep this computer's version as a separate new instance?",
    };
    if !cx.out.confirm(what)? {
        return Err(Error::Cancelled);
    }
    let rep = cx.app.engine().resolve_conflict(id, resolution, &ctx)?;
    cx.out.emit(&rep, |w| {
        writeln!(
            w,
            "Resolved ({}). Current snapshot: {}{}",
            rep.resolution,
            short(&rep.head),
            if rep.uploaded { ", uploaded" } else { ", not uploaded yet" }
        )?;
        if let Some(n) = &rep.new_instance {
            writeln!(w, "New instance: {n}")?;
        }
        if let Some(p) = &rep.preserved_local {
            writeln!(w, "This computer's version is preserved as snapshot {}.", short(p))?;
        }
        Ok(())
    })
}

pub fn crash(cx: &Cx, c: CrashCmd) -> Result<()> {
    let ctx = cx.out.ctx();
    match c {
        CrashCmd::Show(a) => {
            let id = resolve::instance(&cx.app, &a.instance)?;
            let inst = cx.app.engine().instance(id)?;
            let hist = cx.app.engine().history(id, false, &ctx)?;
            let crash = hist.iter().find(|h| h.is_base && h.kind == SnapshotKind::Crash);
            cx.out.emit(&serde_json::json!({ "state": inst.state, "detail": inst.state_detail, "crash_snapshot": crash }), |w| {
                if inst.state != InstanceState::CrashPending {
                    return writeln!(w, "No crash is waiting for a decision.");
                }
                writeln!(w, "{}", inst.state_detail.as_deref().unwrap_or("The last session ended abnormally."))?;
                if let Some(c) = crash {
                    writeln!(w, "Its files were saved separately as snapshot {} ({} files, {}); other computers still see the last good version.", short(&c.id), c.files, bytes(c.bytes))?;
                }
                writeln!(w, "  `packsync crash promote <instance>`  the world looks fine: make it the current version")?;
                writeln!(w, "  `packsync crash discard <instance>`  go back to the last good snapshot (the crash data is kept)")
            })
        }
        CrashCmd::Promote(a) => {
            let id = resolve::instance(&cx.app, &a.instance)?;
            if !cx.out.confirm("Trust this computer's files and make them the current version?")? {
                return Err(Error::Cancelled);
            }
            let n = cx.app.engine().promote_crash(id, &ctx)?;
            let up = cx.app.engine().upload_pending(id, false, &ctx);
            if let Err(e) = &up {
                cx.out.warn(format!("promoted locally; the upload will be retried: {e}"));
            }
            cx.out.emit(&serde_json::json!({ "snapshot": n, "uploaded": up.is_ok() }), |w| {
                writeln!(w, "Promoted: snapshot {} is now the current version.", short(&n))
            })
        }
        CrashCmd::Discard(a) => {
            let id = resolve::instance(&cx.app, &a.instance)?;
            if !cx.out.confirm(
                "Go back to the last good snapshot? The files from the crashed session are kept as a recovery point.",
            )? {
                return Err(Error::Cancelled);
            }
            let r = cx.app.engine().discard_crash(id, &ctx)?;
            cx.out.emit(&r, |w| {
                writeln!(w, "Restored the last good snapshot ({} files written, {} removed).", r.written, r.deleted)?;
                if let Some(p) = &r.recovery_point {
                    writeln!(w, "The crashed session's files are in recovery point {p}.")?;
                }
                Ok(())
            })
        }
    }
}
