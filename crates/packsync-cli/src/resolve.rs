//! Turning what the user typed into an instance or snapshot.

use std::path::Path;

use packsync_app::App;
use packsync_core::db::InstanceRow;
use packsync_core::error::{Error, Result};
use packsync_core::ids::{InstanceId, SnapshotId};
use packsync_core::progress::OpCtx;

/// An instance given as a UUID, a unique UUID prefix, its name (case-insensitive) or its folder.
pub fn instance(app: &App, text: &str) -> Result<InstanceId> {
    let all = app.engine().instances()?;
    if let Ok(id) = InstanceId::parse(text) {
        return all
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.id)
            .ok_or_else(|| Error::NotFound(format!("instance {text} is not registered on this computer")));
    }
    let p = Path::new(text);
    if p.is_dir() {
        let canon = dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if let Some(r) = all.iter().find(|r| dunce::canonicalize(&r.root).unwrap_or_else(|_| r.root.clone()) == canon) {
            return Ok(r.id);
        }
        return Err(Error::NotFound(format!(
            "{text} is not an instance PackSync knows. Use `packsync enable {text}` to start syncing it"
        )));
    }
    let lower = text.to_lowercase();
    let mut hits: Vec<&InstanceRow> = all.iter().filter(|r| r.name.to_lowercase() == lower).collect();
    if hits.is_empty() && text.len() >= 4 {
        hits = all.iter().filter(|r| r.id.to_string().starts_with(&lower)).collect();
    }
    match hits.as_slice() {
        [one] => Ok(one.id),
        [] => Err(Error::NotFound(format!(
            "no instance matches {text:?}. Registered: {}",
            if all.is_empty() {
                "(none yet; use `packsync enable <folder>`)".to_string()
            } else {
                all.iter().map(|r| r.name.clone()).collect::<Vec<_>>().join(", ")
            }
        ))),
        many => Err(Error::invalid(
            "instance",
            format!(
                "{text:?} matches several instances ({}); use the ID",
                many.iter().map(|r| format!("{} [{}]", r.name, &r.id.to_string()[..8])).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

/// A snapshot of `inst` by full ID or unique prefix (from `packsync history`).
pub fn snapshot(app: &App, inst: InstanceId, text: &str, online: bool, ctx: &OpCtx) -> Result<SnapshotId> {
    if text.len() < 6 {
        return Err(Error::invalid(
            "snapshot",
            "give at least 6 characters of the snapshot ID (see `packsync history`)",
        ));
    }
    let hist = app.engine().history(inst, online, ctx)?;
    let hits: Vec<_> = hist.iter().filter(|h| h.id.to_hex().starts_with(&text.to_lowercase())).collect();
    match hits.as_slice() {
        [one] => Ok(one.id),
        [] => Err(Error::NotFound(format!("no snapshot starts with {text}"))),
        _ => Err(Error::invalid("snapshot", format!("{text} matches several snapshots; give more characters"))),
    }
}
