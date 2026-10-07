use packsync_core::error::{Error, Result};
use packsync_core::redact::redact;

use super::Cx;
use crate::args::{BridgeArgs, DoctorArgs, GcCmd, GcOpts};
use crate::render::{bytes, short};
use packsync_core::gc::{GcParams, RetentionPolicy};

pub fn doctor(cx: &Cx, a: DoctorArgs) -> Result<()> {
    let report = packsync_app::diagnostics::report(&cx.app, a.online, &cx.out.ctx())?;
    if let Some(path) = &a.out {
        let text = serde_json::to_string_pretty(&report).unwrap_or_default();
        std::fs::write(path, redact(&text).as_bytes())
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))?;
        cx.out.note(format!(
            "Report written to {} (secrets are redacted; please skim it before sharing).",
            path.display()
        ));
    }
    cx.out.emit(&report, |w| {
        for c in &report.checks {
            writeln!(w, "{:<4} {:<22} {}", if c.ok { "ok" } else { "WARN" }, c.name, c.detail)?;
        }
        if !report.recent_events.is_empty() {
            writeln!(w, "\nRecent events:")?;
            for r in &report.recent_events {
                writeln!(w, "  {r}")?;
            }
        }
        Ok(())
    })
}

fn gc_params(cx: &Cx, o: &GcOpts) -> Result<GcParams> {
    let instance = match (&o.instance, o.retain) {
        (Some(t), true) => Some(match crate::resolve::instance(&cx.app, t) {
            Ok(id) => id,
            // an instance that is only in the cloud (not registered here): accept its ID
            Err(_) => packsync_core::ids::InstanceId::parse(t)
                .map_err(|_| Error::NotFound(format!("no instance matches {t:?}")))?,
        }),
        (Some(_), false) => return Err(Error::invalid("gc", "--instance only makes sense together with --retain")),
        _ => None,
    };
    Ok(GcParams {
        grace_hours: o.grace_hours,
        retention: o.retain.then_some(RetentionPolicy {
            keep_snapshots: o.keep_snapshots,
            keep_days: o.keep_days,
            keep_crash_days: o.keep_crash_days,
        }),
        instance,
        consolidate_indexes: !o.no_consolidate,
    })
}

fn print_gc_plan(w: &mut dyn std::io::Write, p: &packsync_core::gc::GcPlan) -> std::io::Result<()> {
    if p.is_empty() {
        writeln!(w, "Nothing to clean up.")?;
    }
    if !p.expire.is_empty() {
        writeln!(
            w,
            "Release the content of {} old snapshot(s) (they stay in the history but can no longer be restored):",
            p.expire.len()
        )?;
        for e in p.expire.iter().take(10) {
            writeln!(w, "  {}  {}", short(&e.snapshot), e.created.as_deref().unwrap_or("?"))?;
        }
        if p.expire.len() > 10 {
            writeln!(w, "  … and {} more", p.expire.len() - 10)?;
        }
    }
    let packs = p.delete.iter().filter(|d| d.kind == "pack").count();
    let idx = p.delete.iter().filter(|d| d.kind == "index").count();
    if !p.delete.is_empty() {
        writeln!(w, "Delete {packs} pack(s) and {idx} index object(s), freeing {}:", bytes(p.total_delete_bytes))?;
        for d in p.delete.iter().take(12) {
            writeln!(w, "  {:<6} {}  {}  — {}", d.kind, d.key, bytes(d.size), d.reason)?;
        }
        if p.delete.len() > 12 {
            writeln!(w, "  … and {} more", p.delete.len() - 12)?;
        }
    }
    if p.partially_live.packs > 0 {
        writeln!(
            w,
            "{} pack(s) are only partly needed; {} inside them is unused but can only be reclaimed by repacking (not done by this version).",
            p.partially_live.packs,
            bytes(p.partially_live.reclaimable_bytes)
        )?;
    }
    for n in &p.notes {
        writeln!(w, "note: {n}")?;
    }
    if !p.kept_unknown.is_empty() {
        writeln!(w, "Left alone (not created by PackSync or not understood): {}", p.kept_unknown.len())?;
        for k in p.kept_unknown.iter().take(5) {
            writeln!(w, "  {k}")?;
        }
    }
    Ok(())
}

pub fn gc(cx: &Cx, g: GcCmd) -> Result<()> {
    let ctx = cx.out.ctx();
    match g {
        GcCmd::Plan(o) => {
            let params = gc_params(cx, &o)?;
            let plan = cx.app.engine().gc_plan(&params, &ctx)?;
            cx.out.emit(&plan, |w| {
                print_gc_plan(w, &plan)?;
                if !plan.is_empty() {
                    writeln!(w, "\nTo carry this out exactly: packsync gc run {} <the same options>", plan.id)?;
                }
                Ok(())
            })
        }
        GcCmd::Run { plan: plan_id, opts } => {
            let params = gc_params(cx, &opts)?;
            let plan = cx.app.engine().gc_plan(&params, &ctx)?;
            if plan.id != plan_id {
                return Err(Error::Conflict(format!(
                    "the cloud (or the options) changed since the preview: plan {plan_id} is now {}. Run `packsync gc plan` again",
                    plan.id
                )));
            }
            if !cx.out.json && !cx.out.quiet {
                print_gc_plan(&mut std::io::stderr(), &plan).map_err(|e| Error::io("printing the plan", e))?;
            }
            if plan.is_empty() {
                return cx.out.emit(&plan, |w| writeln!(w, "Nothing to clean up."));
            }
            if !cx.out.confirm(
                "Delete these objects from your cloud storage now? (Google Drive keeps them in its trash for a while.)",
            )? {
                return Err(Error::Cancelled);
            }
            let rep = cx.app.engine().gc_run(&plan_id, &params, &ctx)?;
            cx.out.emit(&rep, |w| {
                writeln!(
                    w,
                    "Removed {} pack(s) and {} index object(s), freed {}; released {} snapshot(s).",
                    rep.deleted_packs,
                    rep.deleted_indexes,
                    bytes(rep.bytes_freed),
                    rep.expired
                )?;
                for n in &rep.notes {
                    writeln!(w, "note: {n}")?;
                }
                Ok(())
            })
        }
    }
}

pub fn dev_bridge(cx: &Cx, b: BridgeArgs) -> Result<()> {
    cx.app.start_upload_retries(std::time::Duration::from_secs(60));
    let bridge = packsync_app::bridge::start(
        cx.app.clone(),
        packsync_app::bridge::BridgeConfig { port: b.port, ui_dir: b.ui_dir, allow_origin: b.allow_origin },
    )?;
    // one line for scripts and test harnesses to read
    println!(
        "{}",
        serde_json::json!({ "port": bridge.addr.port(), "token": bridge.token, "url": format!("http://127.0.0.1:{}/#token={}", bridge.addr.port(), bridge.token) })
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    cx.out.note("dev bridge running (127.0.0.1 only); press Ctrl-C to stop");
    loop {
        std::thread::park();
    }
}
