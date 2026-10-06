use packsync_core::error::{Error, Result};
use packsync_core::redact::redact;
use serde::Serialize;

use super::Cx;
use crate::args::{BridgeArgs, DoctorArgs, GcCmd};
use crate::render::bytes;

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

pub fn gc(_cx: &Cx, _g: GcCmd) -> Result<()> {
    Err(Error::Unsupported("cloud cleanup is not available in this build".into()))
}

pub fn dev_bridge(_cx: &Cx, _b: BridgeArgs) -> Result<()> {
    Err(Error::Unsupported("the dev bridge is not available in this build".into()))
}
