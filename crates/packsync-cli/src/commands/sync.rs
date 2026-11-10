use std::io::Write;

use packsync_core::engine::{ReconcileOpts, ReconcileReport, SnapshotOpts, SnapshotResult, UploadSummary};
use packsync_core::error::{Error, Result, exit};

use super::Cx;
use crate::args::{HistoryArgs, InstanceArg, ReconcileArgs, SnapshotArgs, SyncArgs, UploadArgs, VerifyArgs};
use crate::render::{ago, bytes, short, when};
use crate::resolve;

pub fn changes(cx: &Cx, a: InstanceArg) -> Result<()> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let c = cx.app.engine().pending_changes(id, &cx.out.ctx())?;
    cx.out.emit(&c, |w| {
        if !c.dirty {
            return writeln!(w, "No local changes since the last snapshot.");
        }
        writeln!(w, "Changes since the last snapshot{}:", if c.exact { "" } else { " (estimate)" })?;
        for (label, list) in [("added", &c.added), ("modified", &c.modified), ("removed", &c.removed)] {
            for p in list.iter().take(50) {
                writeln!(w, "  {label:<9} {p}")?;
            }
            if list.len() > 50 {
                writeln!(w, "  … and {} more {label}", list.len() - 50)?;
            }
        }
        for i in &c.issues {
            writeln!(w, "  note: {:?} {} — {}", i.kind, i.path, i.detail)?;
        }
        Ok(())
    })
}

pub fn snapshot(cx: &Cx, a: SnapshotArgs) -> Result<()> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let rep = cx.app.engine().snapshot(
        id,
        SnapshotOpts { force: a.force, note: a.note, confirm_mass_delete: a.confirm_mass_delete, ..Default::default() },
        &cx.out.ctx(),
    )?;
    cx.out.emit(&rep, |w| {
        match &rep.result {
            SnapshotResult::Unchanged { base } => writeln!(w, "Nothing changed since snapshot {}.", short(base))?,
            SnapshotResult::Created { id } => writeln!(
                w,
                "Saved snapshot {}: {} files, {} ({} new, {} reused). Not uploaded yet — `packsync upload`.",
                short(id),
                rep.stats.files,
                bytes(rep.stats.bytes),
                bytes(rep.stats.new_stored_bytes),
                bytes(rep.stats.bytes.saturating_sub(rep.stats.new_raw_bytes))
            )?,
        }
        for i in &rep.issues {
            writeln!(w, "  note: {:?} {} — {}", i.kind, i.path, i.detail)?;
        }
        Ok(())
    })
}

fn print_upload(w: &mut dyn Write, s: &UploadSummary) -> std::io::Result<()> {
    if s.snapshots.is_empty() && s.deferred_until_ms.is_none() {
        return writeln!(w, "Nothing to upload.");
    }
    for r in &s.snapshots {
        writeln!(
            w,
            "Snapshot {}: {:?} ({} uploaded, {} already in the cloud)",
            short(&r.snapshot),
            r.state,
            bytes(r.bytes_uploaded),
            r.packs_already_present
        )?;
    }
    if let Some(ms) = s.deferred_until_ms {
        writeln!(
            w,
            "Waiting to retry (backoff until {}). Use --force to try now.",
            ago(-ms + 2 * packsync_core::db::now_ms())
        )?;
    }
    if s.remaining > 0 {
        writeln!(w, "{} snapshot(s) still waiting to upload.", s.remaining)?;
    }
    Ok(())
}

pub fn upload(cx: &Cx, a: UploadArgs) -> Result<i32> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let s = cx.app.engine().upload_pending(id, a.force, &cx.out.ctx())?;
    cx.out.emit(&s, |w| print_upload(w, &s))?;
    Ok(if s.remaining > 0 || s.deferred_until_ms.is_some() { exit::UPLOAD_PENDING } else { exit::OK })
}

fn print_reconcile(w: &mut dyn Write, r: &ReconcileReport) -> std::io::Result<()> {
    match r {
        ReconcileReport::UpToDate { head } => writeln!(w, "Up to date with the cloud (snapshot {}).", short(head)),
        ReconcileReport::CloudEmpty => writeln!(w, "The cloud has nothing for this instance yet."),
        ReconcileReport::LocalAhead { snapshots, uploaded, upload_error } => {
            writeln!(
                w,
                "{} local snapshot(s) are ahead of the cloud; {}.",
                snapshots.len(),
                if *uploaded { "uploaded" } else { "not uploaded yet" }
            )?;
            if let Some(e) = upload_error {
                writeln!(w, "  upload problem: {e}")?;
            }
            Ok(())
        }
        ReconcileReport::CloudAhead { head } => {
            writeln!(w, "The cloud has a newer snapshot ({}). Run `packsync reconcile` to apply it.", short(head))
        }
        ReconcileReport::FastForwarded { to, files_written, files_deleted, .. } => {
            writeln!(
                w,
                "Updated to snapshot {} from the cloud ({files_written} files written, {files_deleted} removed).",
                short(to)
            )
        }
        ReconcileReport::Conflict(c) => {
            writeln!(
                w,
                "CONFLICT ({:?}): this computer and the cloud both changed. Nothing was overwritten. See `packsync conflicts`.",
                c.kind
            )
        }
        ReconcileReport::Offline { reason, .. } => writeln!(w, "The cloud could not be reached: {reason}"),
    }
}

fn reconcile_code(r: &ReconcileReport) -> i32 {
    match r {
        ReconcileReport::Conflict(_) => exit::CONFLICT,
        ReconcileReport::Offline { .. } => exit::OFFLINE,
        ReconcileReport::LocalAhead { uploaded: false, .. } => exit::UPLOAD_PENDING,
        _ => exit::OK,
    }
}

pub fn sync_now(cx: &Cx, a: SyncArgs) -> Result<i32> {
    let started = std::time::Instant::now();
    let id = resolve::instance(&cx.app, &a.instance)?;
    let mut ctx = cx.out.ctx();
    let trace = a.explain.then(|| packsync_core::progress::Trace::new(packsync_app::provider::transfer_counts));
    if let Some(t) = &trace {
        ctx = ctx.with_trace(t.clone());
    }
    let (snap, rec) = cx.app.engine().sync_now(id, &ctx)?;
    let explain = trace.map(|t| Explain {
        total_ms: started.elapsed().as_millis() as u64,
        steps: t.finish(),
        requests_by_kind: packsync_app::provider::drive_request_stats(),
        snapshot: snap.as_ref().map(|s| s.stats.clone()),
    });
    cx.out.emit(&serde_json::json!({ "snapshot": snap, "reconcile": rec, "explain": explain }), |w| {
        if let Some(s) = &snap {
            match &s.result {
                SnapshotResult::Created { id } => {
                    writeln!(w, "Saved snapshot {} ({} files).", short(id), s.stats.files)?
                }
                SnapshotResult::Unchanged { .. } => writeln!(w, "No local changes.")?,
            }
        }
        print_reconcile(w, &rec)?;
        if let Some(e) = &explain {
            print_explain(w, e)?;
        }
        Ok(())
    })?;
    Ok(reconcile_code(&rec))
}

/// What `sync --explain` reports.
#[derive(serde::Serialize)]
struct Explain {
    total_ms: u64,
    steps: Vec<packsync_core::progress::TraceStep>,
    requests_by_kind: Option<packsync_drive::RequestStats>,
    snapshot: Option<packsync_core::snapshot::SnapshotStats>,
}

fn print_explain(w: &mut dyn Write, e: &Explain) -> std::io::Result<()> {
    let total_req: u64 = e.steps.iter().map(|s| s.requests).sum();
    let up: u64 = e.steps.iter().map(|s| s.bytes_up).sum();
    let down: u64 = e.steps.iter().map(|s| s.bytes_down).sum();
    writeln!(w)?;
    writeln!(
        w,
        "Explain: {:.2} s, {total_req} HTTP requests, {} sent, {} received",
        e.total_ms as f64 / 1000.0,
        bytes(up),
        bytes(down)
    )?;
    writeln!(w, "  {:<52} {:>9} {:>9} {:>10} {:>10}", "step", "time", "requests", "sent", "received")?;
    for s in e.steps.iter().filter(|s| s.ms > 0 || s.requests > 0) {
        let name: String = s.step.chars().take(52).collect();
        writeln!(
            w,
            "  {:<52} {:>7.2} s {:>9} {:>10} {:>10}",
            name,
            s.ms as f64 / 1000.0,
            s.requests,
            bytes(s.bytes_up),
            bytes(s.bytes_down)
        )?;
    }
    if let Some(k) = &e.requests_by_kind {
        writeln!(
            w,
            "  by kind: list {}, get {}, create {}, upload sessions {}, upload chunks {}, update {}, delete {}, \
             token {}, other {}; retries {}",
            k.list, k.get, k.create, k.resumable_init, k.chunk, k.update, k.delete, k.token, k.other, k.retries
        )?;
    }
    if let Some(s) = &e.snapshot {
        writeln!(
            w,
            "Snapshot: {} files hashed ({}), {} reused from the cache; {} new chunks, {} new data ({} stored) in {} packs",
            s.hashed_files,
            bytes(s.hashed_bytes),
            s.reused_files,
            s.new_blobs,
            bytes(s.new_raw_bytes),
            bytes(s.new_stored_bytes),
            s.packs
        )?;
        if s.manifest_bytes > 0 {
            writeln!(w, "Manifest: {} stored ({} as JSON)", bytes(s.manifest_bytes), bytes(s.manifest_raw_bytes))?;
        }
        if !s.costliest.is_empty() {
            writeln!(w, "Costliest files:  {:>10} {:>10} {:>10}  path", "hashed", "new", "stored")?;
            for c in &s.costliest {
                writeln!(
                    w,
                    "                  {:>10} {:>10} {:>10}  {}",
                    bytes(c.hashed),
                    bytes(c.new_raw),
                    bytes(c.new_stored),
                    c.path
                )?;
            }
        }
    }
    Ok(())
}

pub fn reconcile(cx: &Cx, a: ReconcileArgs) -> Result<i32> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let r = cx.app.engine().reconcile(
        id,
        ReconcileOpts { apply_fast_forward: !a.no_fast_forward, upload_local: !a.no_upload },
        &cx.out.ctx(),
    )?;
    cx.out.emit(&r, |w| print_reconcile(w, &r))?;
    Ok(reconcile_code(&r))
}

pub fn history(cx: &Cx, a: HistoryArgs) -> Result<()> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let h = cx.app.engine().history(id, !a.offline, &cx.out.ctx())?;
    cx.out.emit(&h, |w| {
        if h.is_empty() {
            return writeln!(w, "No snapshots yet.");
        }
        for e in &h {
            let flags = [e.is_base.then_some("this computer"), e.is_cloud_head.then_some("cloud head")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                w,
                "{}  {:<7} {}  {:<14} {:>6} files {:>9}  {:?}{}{}",
                short(&e.id),
                format!("{:?}", e.kind).to_lowercase(),
                e.created.as_deref().map(when).unwrap_or_else(|| "?".into()),
                e.device.as_deref().unwrap_or("?"),
                e.files,
                bytes(e.bytes),
                e.state,
                if flags.is_empty() { String::new() } else { format!("  [{flags}]") },
                e.note.as_ref().map(|n| format!("  — {n}")).unwrap_or_default()
            )?;
        }
        Ok(())
    })
}

pub fn verify(cx: &Cx, a: VerifyArgs) -> Result<i32> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let ctx = cx.out.ctx();
    let local = cx.app.engine().verify_local(id, &ctx)?;
    let snap = match (&a.snapshot, a.remote) {
        (Some(t), true) => Some(resolve::snapshot(&cx.app, id, t, true, &ctx)?),
        _ => None,
    };
    let repair = if a.repair {
        let also = snap.or(cx.app.engine().instance(id)?.base_snapshot);
        Some(cx.app.engine().repair_cloud_data(
            id,
            packsync_core::engine::RepairOpts { also, discard_unrepairable: a.discard_unrepairable },
            &ctx,
        )?)
    } else {
        None
    };
    let remote = if a.remote { Some(cx.app.engine().verify_remote(id, snap, a.deep, &ctx)?) } else { None };
    let local_ok = local.changed.is_empty() && local.missing.is_empty() && local.extra.is_empty();
    let remote_ok = remote
        .as_ref()
        .map(|r| r.missing_packs.is_empty() && r.damaged_blobs.is_empty() && r.unlisted_blobs.is_empty())
        .unwrap_or(true);
    let repaired_ok = repair.as_ref().is_none_or(|r| r.complete() || !r.discarded.is_empty());
    cx.out.emit(&serde_json::json!({ "local": local, "remote": remote, "repair": repair }), |w| {
        writeln!(
            w,
            "Local: {} files checked; {} changed, {} missing, {} not in the snapshot.",
            local.files_checked,
            local.changed.len(),
            local.missing.len(),
            local.extra.len()
        )?;
        for (label, list) in [("changed", &local.changed), ("missing", &local.missing), ("extra", &local.extra)] {
            for p in list.iter().take(20) {
                writeln!(w, "  {label}: {p}")?;
            }
        }
        if let Some(r) = &remote {
            writeln!(
                w,
                "Cloud: {} pack(s), {} object(s) checked; {} pack(s) missing, {} damaged object(s).",
                r.packs_checked,
                r.blobs_checked,
                r.missing_packs.len(),
                r.damaged_blobs.len()
            )?;
            for p in r.missing_packs.iter().take(20) {
                writeln!(w, "  missing pack: {p}")?;
            }
            if !r.unlisted_blobs.is_empty() {
                writeln!(w, "  {} object(s) are not described by any index in the cloud", r.unlisted_blobs.len())?;
            }
        }
        if let Some(r) = &repair {
            if r.files_missing == 0 {
                writeln!(w, "Repair: nothing was missing.")?;
            } else {
                writeln!(
                    w,
                    "Repair: {} file(s) had data missing from the cloud; {} object(s) ({}) uploaded again from this computer.",
                    r.files_missing,
                    r.blobs_uploaded,
                    bytes(r.bytes_uploaded)
                )?;
            }
            for p in r.unrepairable.iter().take(20) {
                writeln!(w, "  not on this computer any more: {p}")?;
            }
            if !r.discarded.is_empty() {
                writeln!(
                    w,
                    "  {} version(s) that could never be uploaded were dropped; run `packsync sync` to save the files as they are now.",
                    r.discarded.len()
                )?;
            } else if !r.unrepairable.is_empty() {
                writeln!(
                    w,
                    "  Another computer that still has these files can repair them, or use --discard-unrepairable to drop \
                     the versions that only exist here."
                )?;
            }
        }
        Ok(())
    })?;
    if !repaired_ok {
        return Err(Error::CloudDataMissing(
            "some data is neither in the cloud nor on this computer; see the list above".into(),
        ));
    }
    if !remote_ok {
        return Err(Error::Corrupt("the cloud is missing or has damaged data that this snapshot needs".into()));
    }
    // local differences are normal after playing; they are reported, not an error
    let _ = local_ok;
    Ok(exit::OK)
}
