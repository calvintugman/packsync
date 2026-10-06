use packsync_core::error::{Error, Result};
use packsync_core::redact::redact;
use serde::Serialize;

use super::Cx;
use crate::args::{BridgeArgs, DoctorArgs, GcCmd, GcOpts};
use crate::render::{bytes, short};
use packsync_core::gc::{GcParams, RetentionPolicy};

#[derive(Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check { name, ok, detail: detail.into() }
}

pub fn doctor(cx: &Cx, a: DoctorArgs) -> Result<()> {
    let app = &cx.app;
    let mut checks = vec![];
    checks.push(check(
        "version",
        true,
        format!("packsync {} ({} {})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH),
    ));
    checks.push(check("config directory", app.paths.config_dir.is_dir(), app.paths.config_dir.display().to_string()));
    let free = packsync_core::fsutil::available_space(&app.paths.state_dir).ok();
    checks.push(check(
        "state directory",
        free.is_some_and(|f| f > 1 << 30),
        format!("{} ({} free)", app.paths.state_dir.display(), free.map(bytes).unwrap_or_else(|| "?".into())),
    ));
    let (dev_id, dev_name) = app.engine().device();
    checks.push(check("this computer", true, format!("{dev_name} ({dev_id})")));
    let prism = app.prism();
    let exe = prism.find_exe();
    checks.push(check(
        "Prism Launcher",
        exe.is_some(),
        match &exe {
            Some(packsync_prism::Exe::Native(p)) => p.display().to_string(),
            Some(packsync_prism::Exe::Flatpak) => "Flatpak (org.prismlauncher.PrismLauncher)".into(),
            None => "not found: set it with `packsync settings set prism_executable <path>`".into(),
        },
    ));
    let dirs = prism.data_dirs();
    checks.push(check(
        "Prism data folders",
        !dirs.is_empty(),
        if dirs.is_empty() {
            "none found".into()
        } else {
            dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
        },
    ));
    let st = app.storage_status();
    checks.push(check("storage", st.connected, redact(&st.detail).into_owned()));
    if let Some(p) = &st.problem {
        checks.push(check("storage problem", false, redact(p).into_owned()));
    }
    if st.kind == "google" {
        checks.push(check(
            "Google OAuth client",
            st.oauth_client_configured,
            st.oauth_client_source.clone().unwrap_or_else(|| "not configured (docs/oauth-setup.md)".into()),
        ));
        let store =
            packsync_core::credentials::CredentialStore::describe(&packsync_drive::KeyringCredentialStore::new());
        checks.push(check("credential store", !store.contains("unsupported"), store));
    }
    let n = app.engine().instances()?.len();
    checks.push(check("synced instances", true, format!("{n} registered")));
    if a.online && st.connected {
        match app.test_storage(&cx.out.ctx()) {
            Ok(v) => checks.push(check("cloud connection", true, v.to_string())),
            Err(e) => checks.push(check("cloud connection", false, redact(&e.to_string()).into_owned())),
        }
    }
    let recent: Vec<String> = app
        .engine()
        .db()
        .events_recent(None, 15)?
        .into_iter()
        .map(|e| format!("{} [{}] {} {}", e.ts_ms, e.level, e.code, e.message))
        .collect();
    let report = serde_json::json!({ "checks": checks, "recent_events": recent });
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
        for c in &checks {
            writeln!(w, "{:<4} {:<22} {}", if c.ok { "ok" } else { "WARN" }, c.name, c.detail)?;
        }
        if !recent.is_empty() {
            writeln!(w, "\nRecent events:")?;
            for r in &recent {
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

pub fn dev_bridge(_cx: &Cx, _b: BridgeArgs) -> Result<()> {
    Err(Error::Unsupported("the dev bridge is not available in this build".into()))
}
