use std::io::Write;

use packsync_core::engine::{PreviewRequest, SyncPhase};
use packsync_core::error::{Error, Result};

use super::Cx;
use crate::args::{EnableArgs, InstanceArg, PreviewArgs, RulesCmd, StatusArgs};
use crate::render::{ago, bytes, phase_label, short};
use crate::resolve;

fn read_rules(path: &Option<std::path::PathBuf>) -> Result<String> {
    match path {
        Some(p) => std::fs::read_to_string(p).map_err(|e| Error::io(format!("reading {}", p.display()), e)),
        None => Ok(String::new()),
    }
}

pub fn status(cx: &Cx, a: StatusArgs) -> Result<()> {
    let ctx = cx.out.ctx();
    let list = match &a.instance {
        Some(t) => {
            let id = resolve::instance(&cx.app, t)?;
            vec![cx.app.engine().status(id, a.online, &ctx)?]
        }
        None => cx.app.instance_statuses(a.online, &ctx)?,
    };
    let storage = cx.app.storage_status();
    cx.out.emit(&serde_json::json!({ "storage": storage, "instances": list }), |w| {
        writeln!(w, "Storage: {}{}", storage.detail, if storage.connected { "" } else { "  (not connected)" })?;
        if let Some(p) = &storage.problem {
            writeln!(w, "  problem: {p}")?;
        }
        if list.is_empty() {
            writeln!(
                w,
                "\nNo instances are synced on this computer. Try `packsync instances` and `packsync enable <folder>`."
            )?;
        }
        for s in &list {
            writeln!(w, "\n{}  [{}]", s.name, &s.id.to_string()[..8])?;
            writeln!(w, "  {}", phase_label(s.phase))?;
            writeln!(w, "  folder: {}", s.root.display())?;
            if let Some(ms) = s.last_synced_ms {
                writeln!(
                    w,
                    "  last synced: {}{}",
                    ago(ms),
                    s.last_synced_by.as_ref().map(|d| format!(" by {d}")).unwrap_or_default()
                )?;
            }
            if s.pending_snapshots > 0 {
                writeln!(
                    w,
                    "  {} snapshot(s) waiting to upload ({})",
                    s.pending_snapshots,
                    bytes(s.pending_pack_bytes)
                )?;
            }
            if let Some(e) = &s.last_error {
                writeln!(w, "  last problem: {e}")?;
            }
            if !s.playing_elsewhere.is_empty() {
                writeln!(w, "  appears to be playing on: {}", s.playing_elsewhere.join(", "))?;
            }
            if let Some(d) = &s.state_detail {
                writeln!(w, "  note: {d}")?;
            }
            for wd in &s.worlds {
                writeln!(w, "  world: {}", wd.name.as_deref().unwrap_or(&wd.folder))?;
            }
            if s.phase != SyncPhase::NotEnabled {
                writeln!(w, "  {} files, {}", s.total_files, bytes(s.total_bytes))?;
            }
        }
        Ok(())
    })
}

pub fn list(cx: &Cx) -> Result<()> {
    let found = cx.app.discover()?;
    cx.out.emit(&found, |w| {
        if found.is_empty() {
            writeln!(w, "No Prism Launcher instances were found. Select an instance folder manually with `packsync preview <folder>`.")?;
        }
        for d in &found {
            writeln!(
                w,
                "{}  {}{}",
                d.instance.name,
                d.instance.root.display(),
                match (d.registered, d.sync_enabled) {
                    (Some(id), true) => format!("   [syncing: {}]", &id.to_string()[..8]),
                    (Some(id), false) => format!("   [registered, sync off: {}]", &id.to_string()[..8]),
                    _ => String::new(),
                }
            )?;
            let mc = d.instance.identity.minecraft_version.as_deref().unwrap_or("?");
            let loader = d.instance.identity.loader.as_ref().map(|l| format!("{} {}", l.kind, l.version)).unwrap_or_else(|| "vanilla".into());
            writeln!(w, "    Minecraft {mc}, {loader}, {} world(s)", d.instance.worlds.len())?;
        }
        Ok(())
    })
}

fn print_preview(w: &mut dyn Write, r: &packsync_core::engine::PreviewReport) -> std::io::Result<()> {
    writeln!(
        w,
        "{}  (Minecraft {}, {})",
        r.name,
        r.pack.minecraft_version.as_deref().unwrap_or("?"),
        r.pack.loader.as_ref().map(|l| format!("{} {}", l.kind, l.version)).unwrap_or_else(|| "vanilla".into())
    )?;
    writeln!(w, "Would sync {} files ({}):", r.included_files, bytes(r.included_bytes))?;
    for g in &r.included_by_category {
        writeln!(w, "  {:<28} {:>7} files  {:>10}", g.category, g.files, bytes(g.bytes))?;
    }
    if !r.worlds.is_empty() {
        writeln!(w, "Worlds:")?;
        for wd in &r.worlds {
            writeln!(w, "  {}", wd.name.as_deref().unwrap_or(&wd.folder))?;
        }
    }
    if !r.excluded.groups.is_empty() || !r.excluded.pruned_dirs.is_empty() {
        writeln!(w, "Not synced (on purpose):")?;
        for (id, g) in &r.excluded.groups {
            writeln!(w, "  {:<28} {:>7} files  {:>10}  — {}", id, g.files, bytes(g.bytes), g.reason)?;
        }
        for (dir, _rule, reason) in &r.excluded.pruned_dirs {
            writeln!(w, "  {dir}/  — {reason}")?;
        }
    }
    for (dir, (files, size)) in &r.excluded.unmatched_by_dir {
        writeln!(w, "  not recognised: {dir}/ ({files} files, {})", bytes(*size))?;
    }
    if let Some(u) = r.estimated_upload_bytes {
        writeln!(w, "Estimated first upload: {}", bytes(u))?;
    }
    for i in &r.issues {
        writeln!(w, "  {:?}: {} — {}", i.kind, i.path, i.detail)?;
    }
    Ok(())
}

pub fn preview(cx: &Cx, a: PreviewArgs) -> Result<()> {
    let ctx = cx.out.ctx();
    let rules = read_rules(&a.rules_file)?;
    let d = cx.app.describe(&a.launcher, &a.path)?;
    let r = cx.app.engine().preview(
        &PreviewRequest {
            root: d.root,
            launcher: a.launcher,
            rules_overrides: rules,
            estimate_upload: a.estimate_upload,
        },
        &ctx,
    )?;
    cx.out.emit(&r, |w| print_preview(w, &r))
}

pub fn enable(cx: &Cx, a: EnableArgs) -> Result<()> {
    let ctx = cx.out.ctx();
    let rules = read_rules(&a.rules_file)?;
    let d = cx.app.describe(&a.launcher, &a.path)?;
    if !cx.out.json && !cx.out.quiet {
        let r = cx.app.engine().preview(
            &PreviewRequest {
                root: d.root.clone(),
                launcher: a.launcher.clone(),
                rules_overrides: rules.clone(),
                estimate_upload: false,
            },
            &ctx,
        )?;
        print_preview(&mut std::io::stderr(), &r).map_err(|e| Error::io("printing the preview", e))?;
    }
    if !cx.out.confirm(&format!("Start syncing {}?", d.name))? {
        return Err(Error::Cancelled);
    }
    let en = cx.app.enable_sync(&a.launcher, &d.root, rules, &ctx)?;
    let mut uploaded = None;
    if !a.no_upload && cx.app.engine().has_storage() {
        match cx.app.engine().upload_pending(en.instance, false, &ctx) {
            Ok(s) => uploaded = Some(s),
            Err(e) => cx.out.warn(format!("the first snapshot is saved locally but could not be uploaded yet: {e}")),
        }
    } else if !a.no_upload {
        cx.out.warn(
            "no storage is connected, so nothing was uploaded; run `packsync storage ...` and then `packsync upload`",
        );
    }
    cx.out.emit(&serde_json::json!({ "outcome": en, "upload": uploaded }), |w| {
        writeln!(w, "Syncing {}  [{}]", d.name, en.instance)?;
        writeln!(w, "  first snapshot {}: {} files, {}", short(&en.snapshot), en.stats.files, bytes(en.stats.bytes))?;
        match &uploaded {
            Some(s) => writeln!(w, "  uploaded {} (remaining: {})", bytes(s.bytes_uploaded), s.remaining)?,
            None => writeln!(w, "  not uploaded yet — run `packsync upload {}`", &en.instance.to_string()[..8])?,
        }
        for i in &en.issues {
            writeln!(w, "  note: {:?} {} — {}", i.kind, i.path, i.detail)?;
        }
        Ok(())
    })
}

pub fn disable(cx: &Cx, a: InstanceArg) -> Result<()> {
    let id = resolve::instance(&cx.app, &a.instance)?;
    let inst = cx.app.engine().instance(id)?;
    if !cx
        .out
        .confirm(&format!("Stop syncing {} on this computer? Files and cloud data stay as they are.", inst.name))?
    {
        return Err(Error::Cancelled);
    }
    cx.app.engine().disable_sync(id)?;
    cx.out.emit(&serde_json::json!({ "instance": id, "sync_enabled": false }), |w| {
        writeln!(w, "Sync is off for {}.", inst.name)
    })
}

pub fn rules(cx: &Cx, c: RulesCmd) -> Result<()> {
    let ctx = cx.out.ctx();
    match c {
        RulesCmd::Show(a) => {
            let id = resolve::instance(&cx.app, &a.instance)?;
            let inst = cx.app.engine().instance(id)?;
            let text = match inst.base_snapshot {
                Some(b) => cx.app.engine().manifest(id, b, &ctx)?.rules.text,
                None => inst.pending_rules.clone().unwrap_or_default(),
            };
            cx.out.emit(&serde_json::json!({ "rules": text }), |w| writeln!(w, "{text}"))
        }
        RulesCmd::Explain { instance, path } => {
            let id = resolve::instance(&cx.app, &instance)?;
            let inst = cx.app.engine().instance(id)?;
            let text = match inst.base_snapshot {
                Some(b) => cx.app.engine().manifest(id, b, &ctx)?.rules.text,
                None => inst.pending_rules.clone().unwrap_or_default(),
            };
            let rs = packsync_core::rules::RuleSet::compile(&text, &inst.game_root)?;
            let d = rs.decide(path.trim_start_matches('/'));
            let explanation = d.explain(&path);
            cx.out.emit(&serde_json::json!({ "path": path, "synced": d.include, "explanation": explanation }), |w| {
                writeln!(w, "{explanation}")
            })
        }
    }
}
