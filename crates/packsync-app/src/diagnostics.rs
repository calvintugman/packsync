//! Installation checks and a redacted diagnostics report. Never contains tokens or client secrets.

use packsync_core::error::Result;
use packsync_core::progress::OpCtx;
use packsync_core::redact::redact;
use serde::{Deserialize, Serialize};

use crate::app::App;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostics {
    pub checks: Vec<Check>,
    pub recent_events: Vec<String>,
}

fn check(name: &str, ok: bool, detail: impl Into<String>) -> Check {
    Check { name: name.into(), ok, detail: redact(&detail.into()).into_owned() }
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (n as f64, 0);
    while v >= 1000.0 && u < U.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[u]) }
}

pub fn report(app: &App, online: bool, ctx: &OpCtx) -> Result<Diagnostics> {
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
        format!("{} ({} free)", app.paths.state_dir.display(), free.map(human_bytes).unwrap_or_else(|| "?".into())),
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
    checks.push(check("storage", st.connected, st.detail.clone()));
    if let Some(p) = &st.problem {
        checks.push(check("storage problem", false, p.clone()));
    }
    if st.kind == "google" {
        checks.push(check(
            "Google OAuth client",
            st.oauth_client_configured,
            st.oauth_client_source.clone().unwrap_or_else(|| "not configured".into()),
        ));
        let store =
            packsync_core::credentials::CredentialStore::describe(&packsync_drive::KeyringCredentialStore::new());
        checks.push(check("credential store", !store.contains("unsupported"), store));
    }
    checks.push(check("synced instances", true, format!("{} registered", app.engine().instances()?.len())));
    if online && st.connected {
        match app.test_storage(ctx) {
            Ok(v) => checks.push(check("cloud connection", true, v.to_string())),
            Err(e) => checks.push(check("cloud connection", false, e.to_string())),
        }
    }
    let recent_events = app
        .engine()
        .db()
        .events_recent(None, 15)?
        .into_iter()
        .map(|e| redact(&format!("{} [{}] {} {}", e.ts_ms, e.level, e.code, e.message)).into_owned())
        .collect();
    Ok(Diagnostics { checks, recent_events })
}
