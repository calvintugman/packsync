use packsync_app::RestoreCloudRequest;
use packsync_core::engine::RestoreOpts;
use packsync_core::error::{Error, Result};
use packsync_core::launcher::LocalSettings;

use super::Cx;
use crate::args::{InstanceArg, RestoreArgs, RestoreNewArgs};
use crate::render::{bytes, short, when};
use crate::resolve;

fn print_preview(w: &mut dyn std::io::Write, pv: &packsync_core::engine::RestorePreview) -> std::io::Result<()> {
    writeln!(
        w,
        "Restoring snapshot {} ({}):",
        short(&pv.target.id),
        pv.target.created.as_deref().map(when).unwrap_or_default()
    )?;
    writeln!(
        w,
        "  {} files would be written ({}), {} unchanged, {} removed, {} local-only files kept",
        pv.files_to_write,
        bytes(pv.bytes_to_write),
        pv.files_unchanged,
        pv.files_to_delete,
        pv.local_only_preserved
    )?;
    for p in pv.sample_writes.iter().take(8) {
        writeln!(w, "    write  {p}")?;
    }
    for p in pv.sample_deletes.iter().take(8) {
        writeln!(w, "    remove {p}")?;
    }
    if pv.local_changes {
        writeln!(
            w,
            "  Your current changes are saved as their own snapshot first, and the replaced files are kept as a recovery point."
        )?;
    }
    for s in &pv.skipped {
        writeln!(w, "  cannot restore {}: {}", s.path, s.problem)?;
    }
    for p in &pv.problems {
        writeln!(w, "  problem: {p}")?;
    }
    Ok(())
}

pub fn restore(cx: &Cx, a: RestoreArgs) -> Result<()> {
    let ctx = cx.out.ctx();
    let id = resolve::instance(&cx.app, &a.instance)?;
    let snap = resolve::snapshot(&cx.app, id, &a.snapshot, true, &ctx)?;
    let pv = cx.app.engine().restore_preview(id, snap, &ctx)?;
    if a.preview {
        return cx.out.emit(&pv, |w| print_preview(w, &pv));
    }
    // one JSON document per command: the preview is shown to the person (stderr) before asking
    if !cx.out.json && !cx.out.quiet {
        print_preview(&mut std::io::stderr(), &pv).map_err(|e| Error::io("printing the preview", e))?;
    }
    if !pv.problems.is_empty() {
        return Err(Error::Conflict(pv.problems.join("; ")));
    }
    if !cx.out.confirm("Apply this restore?")? {
        return Err(Error::Cancelled);
    }
    let rep =
        cx.app.engine().restore_snapshot(id, snap, RestoreOpts { skip_incompatible: a.skip_incompatible }, &ctx)?;
    cx.out.emit(&serde_json::json!({ "preview": pv, "report": rep }), |w| {
        writeln!(w, "Restored: {} written, {} unchanged, {} removed.", rep.written, rep.unchanged, rep.deleted)?;
        if let Some(r) = &rep.recovery_point {
            writeln!(w, "Previous files kept as recovery point {r} (`packsync recovery`).")?;
        }
        Ok(())
    })
}

pub fn restore_new(cx: &Cx, a: RestoreNewArgs) -> Result<()> {
    let ctx = cx.out.ctx();
    let instance = match &a.instance {
        Some(t) => {
            let cloud = cx.app.engine().cloud_instances(&ctx)?;
            let lower = t.to_lowercase();
            let hits: Vec<_> = cloud
                .iter()
                .filter(|c| c.id.to_string().starts_with(&lower) || c.name.to_lowercase() == lower)
                .collect();
            match hits.as_slice() {
                [one] => Some(one.id),
                [] => {
                    return Err(Error::NotFound(format!(
                        "no instance in the cloud matches {t:?} (see `packsync cloud`)"
                    )));
                }
                _ => {
                    return Err(Error::invalid(
                        "instance",
                        format!("{t:?} matches several cloud instances; use the ID"),
                    ));
                }
            }
        }
        None => None,
    };
    let snapshot = match (&a.snapshot, instance) {
        (Some(t), Some(i)) => Some(resolve::snapshot(&cx.app, i, t, true, &ctx)?),
        (Some(_), None) => return Err(Error::invalid("restore-new", "--snapshot needs an instance")),
        _ => None,
    };
    if !cx.out.confirm("Create this instance on this computer?")? {
        return Err(Error::Cancelled);
    }
    let r = cx.app.restore_cloud_instance(
        RestoreCloudRequest {
            instance,
            snapshot,
            data_dir: a.data_dir,
            folder_name: a.name,
            local: LocalSettings {
                java_path: a.java,
                min_memory_mb: a.min_memory_mb,
                max_memory_mb: a.max_memory_mb,
                jvm_args: None,
            },
            skip_incompatible: a.skip_incompatible,
        },
        &ctx,
    )?;
    cx.out.emit(&r, |w| {
        writeln!(w, "Restored to {}", r.root.display())?;
        writeln!(w, "  {} files ({} downloaded)", r.report.written, bytes(r.report.bytes_downloaded))?;
        for n in &r.notes {
            writeln!(w, "  note: {n}")?;
        }
        Ok(())
    })
}

pub fn cloud(cx: &Cx) -> Result<()> {
    let list = cx.app.engine().cloud_instances(&cx.out.ctx())?;
    cx.out.emit(&list, |w| {
        if list.is_empty() {
            return writeln!(w, "The cloud has no instances yet.");
        }
        for c in &list {
            writeln!(w, "{}  [{}]", c.name, c.id)?;
            writeln!(
                w,
                "  {} files, {}; last updated {} by {}",
                c.files,
                bytes(c.bytes),
                c.last_updated.as_deref().map(when).unwrap_or_else(|| "?".into()),
                c.last_device.as_deref().unwrap_or("?")
            )?;
            if c.heads.len() > 1 {
                writeln!(w, "  {} diverged versions in the cloud", c.heads.len())?;
            }
            match &c.local_root {
                Some(p) => writeln!(w, "  on this computer: {}", p.display())?,
                None => writeln!(w, "  not on this computer — `packsync restore-new {}`", &c.id.to_string()[..8])?,
            }
        }
        Ok(())
    })
}

pub fn recovery(cx: &Cx, a: InstanceArg) -> Result<()> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let pts = cx.app.engine().list_recovery_points(id)?;
    cx.out.emit(&pts, |w| {
        if pts.is_empty() {
            return writeln!(w, "No recovery points.");
        }
        for p in &pts {
            writeln!(
                w,
                "{}  {}  {} files, {}  — {}\n    {}",
                p.tx,
                when(&p.created),
                p.files,
                bytes(p.bytes),
                p.reason,
                p.path.display()
            )?;
        }
        Ok(())
    })
}
