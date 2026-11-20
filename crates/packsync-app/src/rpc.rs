//! The JSON API the desktop UI calls (through Tauri commands or the dev bridge).
//!
//! * [`call`] runs one method synchronously and returns its JSON result;
//! * methods marked `long` are meant to run as jobs ([`start_job`]): they report progress on the
//!   event bus and can be cancelled.
//!
//! Every method takes and returns plain JSON, so the UI never sees Rust types, and the whole surface
//! can be exercised in tests (and by the E2E suite through the bridge) exactly as the UI does.

use std::sync::Arc;

use packsync_core::engine::{PreviewRequest, ReconcileOpts, ReconcileReport, Resolution, RestoreOpts, SnapshotOpts};
use packsync_core::error::{Error, Result};
use packsync_core::gc::GcParams;
use packsync_core::ids::{InstanceId, JobId, SnapshotId};
use packsync_core::launcher::HookPlan;
use packsync_core::progress::OpCtx;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::app::{App, RestoreCloudRequest};
use crate::launch::LaunchOptions;
use crate::settings::Settings;

pub struct MethodInfo {
    pub name: &'static str,
    /// Runs for a long time or changes data: call it through [`start_job`].
    pub long: bool,
    pub doc: &'static str,
}

const fn m(name: &'static str, long: bool, doc: &'static str) -> MethodInfo {
    MethodInfo { name, long, doc }
}

pub const METHODS: &[MethodInfo] = &[
    m("app.info", false, "Version, paths, this computer, storage and launcher summary."),
    m("app.open_url", false, "{url}: open a trusted https:// or loopback URL in the system browser."),
    m("settings.get", false, "Current settings."),
    m("settings.update", false, "Change settings: {patch: {…}} (unknown keys are refused)."),
    m("device.rename", false, "{name}"),
    m("storage.status", false, "Connection state of the storage."),
    m("storage.use_local", false, "{path}: use a folder as the cloud (testing)."),
    m("storage.use_google", false, "{space}: select Google Drive (sign in separately)."),
    m("storage.google_login", true, "Sign in with Google in the system browser."),
    m(
        "storage.use_r2",
        true,
        "{account_id, bucket, prefix?, access_key_id, secret_access_key}: configure Cloudflare R2.",
    ),
    m("storage.disconnect", false, "{revoke}: forget the connection (cloud data is not touched)."),
    m("storage.test", true, "Check that the connection works."),
    m("storage.usage", false, "How much of the account's storage is used (one cheap request)."),
    m(
        "instance.decide_unknown",
        false,
        "{instance, decisions:[{path, include}]}: answers about content no rule covers; the next save applies them.",
    ),
    m("storage.hosted_start", true, "{service?}: create a PackSync cloud account and use it (first computer)."),
    m("storage.hosted_join", true, "{code, service?}: join the PackSync cloud of the computer showing the code."),
    m("storage.hosted_recover", true, "{key}: use the PackSync cloud with a recovery key."),
    m("storage.hosted_pair_start", false, "Make a pairing code for another computer: {code}."),
    m("storage.hosted_pair_wait", true, "Wait (up to 10 minutes) until another computer joined with the code."),
    m(
        "storage.hosted_login_start",
        false,
        "{service?, open_browser?}: start Microsoft device-code sign-in for the PackSync cloud.",
    ),
    m("storage.hosted_login_wait", true, "Wait until Microsoft device-code sign-in completes."),
    m("storage.hosted_delete_account", true, "Permanently delete the PackSync cloud account and all cloud data."),
    m("storage.hosted_recovery_key", false, "The recovery key (a secret: show it only when asked): {key}."),
    m("launcher.discover", false, "Prism instances found on this computer."),
    m("launcher.describe", false, "{launcher, path}: validate a manually selected folder."),
    m("instances.list", false, "{online?}: status of every synced instance."),
    m("instance.status", false, "{instance, online?}"),
    m("instances.preview", true, "PreviewRequest: what would be synced."),
    m("instances.enable", true, "{path, launcher, rules_overrides?, upload?}: start syncing."),
    m("instances.disable", false, "{instance}"),
    m("instance.changes", true, "{instance}: local changes not in a snapshot yet."),
    m("instance.history", false, "{instance, online?}"),
    m("instance.snapshot", true, "{instance, force?, note?}"),
    m("instance.upload", true, "{instance, force?}"),
    m("instance.sync", true, "{instance}: snapshot, then reconcile."),
    m("instance.reconcile", true, "{instance, apply_fast_forward?, upload_local?}"),
    m("instance.verify_local", true, "{instance}"),
    m("instance.verify_remote", true, "{instance, snapshot?, deep?}"),
    m("cloud.instances", true, "Instances stored in the cloud."),
    m("restore.preview", true, "{instance, snapshot}"),
    m("restore.apply", true, "{instance, snapshot, skip_incompatible?}"),
    m("restore.new", true, "RestoreCloudRequest: bring a cloud instance to this computer."),
    m("restore.points", false, "{instance}: recovery points."),
    m("conflict.show", true, "{instance}: the diverged versions."),
    m("conflict.resolve", true, "{instance, resolution}"),
    m("crash.promote", true, "{instance}"),
    m("crash.discard", true, "{instance}"),
    m("launch.start", true, "{instance, options?}: sync, play, save, upload."),
    m("hooks.state", false, "{instance}"),
    m("hooks.plan_install", false, "{instance, cli?}: what installing would change."),
    m("hooks.plan_remove", false, "{instance}"),
    m("hooks.apply", false, "{instance, plan}"),
    m("rules.show", true, "{instance}"),
    m("rules.explain", true, "{instance, path}"),
    m("gc.plan", true, "GcParams: what a cleanup would do."),
    m("gc.run", true, "{plan_id, params}: carry out exactly that plan."),
    m("jobs.list", false, "Background jobs."),
    m("jobs.cancel", false, "{job}"),
    m("events.recent", false, "{instance?, limit?}: recent log entries."),
    m("events.clear", false, "Clear local detailed log entries."),
    m("events.export", false, "{path, limit?}: write recent redacted log entries to a JSON file."),
    m("diagnostics.report", true, "{online?}: installation checks (redacted)."),
];

pub fn is_long(method: &str) -> bool {
    METHODS.iter().any(|x| x.name == method && x.long)
}

pub fn is_known(method: &str) -> bool {
    METHODS.iter().any(|x| x.name == method)
}

fn p<T: DeserializeOwned>(params: &Value) -> Result<T> {
    serde_json::from_value(params.clone()).map_err(|e| Error::invalid("request", e.to_string()))
}

fn ser<T: serde::Serialize>(v: &T) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}

#[derive(Deserialize)]
struct Inst {
    instance: InstanceId,
}
#[derive(Deserialize)]
struct InstOnline {
    instance: InstanceId,
    #[serde(default)]
    online: bool,
}
#[derive(Deserialize)]
struct InstSnap {
    instance: InstanceId,
    snapshot: SnapshotId,
}

/// The instance a request is about, if any (used to deduplicate jobs).
fn instance_of(params: &Value) -> Option<InstanceId> {
    params.get("instance")?.as_str()?.parse().ok()
}

fn app_info(app: &App) -> Value {
    let (id, name) = app.engine().device();
    let prism = app.prism();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "paths": { "config": app.paths.config_dir, "state": app.paths.state_dir },
        "device": { "id": id, "name": name },
        "storage": app.storage_status(),
        "prism": {
            "executable": match prism.find_exe() {
                Some(packsync_prism::Exe::Native(p)) => Some(p.display().to_string()),
                Some(packsync_prism::Exe::Flatpak) => Some("flatpak run org.prismlauncher.PrismLauncher".into()),
                None => None,
            },
            "data_dirs": prism.data_dirs(),
        },
        "minecraft_profile": prism.active_minecraft_profile(),
        "running_jobs": app.jobs().running(),
    })
}

fn settings_patch(app: &App, patch: &serde_json::Map<String, Value>) -> Result<Value> {
    let mut cur = serde_json::to_value(app.settings())?;
    let obj = cur.as_object_mut().expect("settings are an object");
    for (k, v) in patch {
        if !obj.contains_key(k) {
            return Err(Error::invalid("settings", format!("unknown setting {k:?}")));
        }
        obj.insert(k.clone(), v.clone());
    }
    let next: Settings = serde_json::from_value(cur).map_err(|e| Error::invalid("settings", e.to_string()))?;
    let up = app.update_settings(|s| *s = next)?;
    Ok(json!({ "settings": up.settings, "storage_problem": up.storage_problem }))
}

fn validate_external_url(url: &str) -> Result<()> {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::invalid("url", "URL cannot be empty or contain whitespace"));
    }
    let (is_http, rest) = if let Some(r) = url.strip_prefix("https://") {
        (false, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (true, r)
    } else {
        return Err(Error::invalid("url", "only https:// or localhost http:// URLs are allowed"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.contains('\\') {
        return Err(Error::invalid("url", "URL has an invalid authority"));
    }
    let (host, port_opt) = if let Some(after_open) = authority.strip_prefix('[') {
        let Some((h, tail)) = after_open.split_once(']') else {
            return Err(Error::invalid("url", "URL has an invalid IPv6 host"));
        };
        if let Some(p) = tail.strip_prefix(':') {
            (h, Some(p))
        } else if tail.is_empty() {
            (h, None)
        } else {
            return Err(Error::invalid("url", "URL has an invalid authority"));
        }
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err(Error::invalid("url", "URL host cannot be empty"));
    }
    if let Some(port) = port_opt
        && (port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port.parse::<u16>().ok().is_none_or(|n| n == 0))
    {
        return Err(Error::invalid("url", "URL has an invalid port"));
    }
    if is_http && !(host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost")) {
        return Err(Error::invalid("url", "plain http:// URLs are only allowed for 127.0.0.1 or localhost"));
    }
    Ok(())
}

/// Run one method. Long-running methods block until done; prefer [`start_job`] for those.
pub fn call(app: &Arc<App>, method: &str, params: Value, ctx: &OpCtx) -> Result<Value> {
    let e = app.engine();
    match method {
        "app.info" => Ok(app_info(app)),
        "app.open_url" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct A {
                url: String,
            }
            let a: A = p(&params)?;
            validate_external_url(&a.url)?;
            use packsync_drive::oauth::UrlOpener as _;
            let opened = packsync_drive::oauth::SystemBrowser { print_fallback: false }.open(&a.url).is_ok();
            Ok(json!({ "opened": opened }))
        }
        "settings.get" => ser(&app.settings()),
        "settings.update" => {
            #[derive(Deserialize)]
            struct A {
                patch: serde_json::Map<String, Value>,
            }
            let a: A = p(&params)?;
            settings_patch(app, &a.patch)
        }
        "device.rename" => {
            #[derive(Deserialize)]
            struct A {
                name: String,
            }
            let a: A = p(&params)?;
            e.rename_device(&a.name)?;
            app.update_settings(|s| s.device_name = Some(a.name.trim().to_string()))?;
            let (id, name) = e.device();
            Ok(json!({ "id": id, "name": name }))
        }
        "storage.status" => ser(&app.storage_status()),
        "storage.use_local" => {
            #[derive(Deserialize)]
            struct A {
                path: std::path::PathBuf,
            }
            let a: A = p(&params)?;
            app.use_local_storage(&a.path)?;
            ser(&app.storage_status())
        }
        "storage.use_google" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(default = "default_space")]
                space: String,
            }
            fn default_space() -> String {
                "visible_folder".into()
            }
            let a: A = p(&params)?;
            ser(&app.use_google(&a.space)?)
        }
        "storage.google_login" => {
            let opener = packsync_drive::oauth::SystemBrowser { print_fallback: false };
            let info = app.google_login(&opener, ctx)?;
            Ok(json!({ "account": info, "status": app.storage_status() }))
        }
        "storage.use_r2" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct A {
                account_id: String,
                bucket: String,
                #[serde(default = "default_r2_prefix")]
                prefix: String,
                access_key_id: String,
                secret_access_key: String,
            }
            fn default_r2_prefix() -> String {
                "packsync".into()
            }
            let a: A = p(&params)?;
            let secret = packsync_core::redact::Secret::new(a.secret_access_key);
            ser(&app.use_r2(&a.account_id, &a.bucket, &a.prefix, &a.access_key_id, &secret, ctx)?)
        }
        "storage.disconnect" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(default)]
                revoke: bool,
            }
            let a: A = p(&params)?;
            app.disconnect_storage(a.revoke, ctx)?;
            ser(&app.storage_status())
        }
        "storage.hosted_start" => {
            #[derive(Deserialize, Default)]
            struct A {
                #[serde(default)]
                service: Option<String>,
            }
            // the desktop app sends no parameters (null)
            let a: A = if params.is_null() { A::default() } else { p(&params)? };
            ser(&app.hosted_start(a.service.as_deref(), ctx)?)
        }
        "storage.hosted_join" => {
            #[derive(Deserialize)]
            struct A {
                code: String,
                #[serde(default)]
                service: Option<String>,
            }
            let a: A = p(&params)?;
            ser(&app.hosted_join(&a.code, a.service.as_deref(), ctx)?)
        }
        "storage.hosted_recover" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
            }
            let a: A = p(&params)?;
            ser(&app.hosted_recover(&packsync_core::redact::Secret::new(a.key.trim().to_string()), ctx)?)
        }
        "storage.hosted_pair_start" => Ok(json!({ "code": app.hosted_pair_start(ctx)? })),
        "storage.hosted_pair_wait" => {
            app.hosted_pair_wait(ctx)?;
            Ok(json!({ "joined": true }))
        }
        "storage.hosted_login_start" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct A {
                #[serde(default)]
                service: Option<String>,
                #[serde(default = "default_open_browser")]
                open_browser: bool,
            }
            impl Default for A {
                fn default() -> Self {
                    Self { service: None, open_browser: true }
                }
            }
            fn default_open_browser() -> bool {
                true
            }
            let a: A = if params.is_null() { A::default() } else { p(&params)? };
            let opener = packsync_drive::oauth::SystemBrowser { print_fallback: false };
            let opener_opt: Option<&dyn packsync_drive::oauth::UrlOpener> =
                if a.open_browser { Some(&opener) } else { None };
            let prompt = app.hosted_login_start(a.service.as_deref(), opener_opt, ctx)?;
            Ok(json!({
                "user_code": prompt.user_code,
                "verification_uri": prompt.verification_uri,
                "expires_in": prompt.expires_in,
                "privacy_url": prompt.privacy_url,
            }))
        }
        "storage.hosted_login_wait" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Empty {}
            if !params.is_null() {
                let _: Empty = p(&params)?;
            }
            ser(&app.hosted_login_wait(ctx)?)
        }
        "storage.hosted_delete_account" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Empty {}
            if !params.is_null() {
                let _: Empty = p(&params)?;
            }
            ser(&app.hosted_delete_account(ctx)?)
        }
        "storage.hosted_recovery_key" => Ok(json!({ "key": app.hosted_recovery_key()?.expose() })),
        "storage.test" => app.test_storage(ctx),
        "storage.usage" => {
            let st = e.storage()?;
            Ok(json!({ "provider": st.provider_id(), "quota": st.quota(ctx)? }))
        }
        "launcher.discover" => ser(&app.discover()?),
        "launcher.describe" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(default = "prism")]
                launcher: String,
                path: std::path::PathBuf,
            }
            fn prism() -> String {
                "prism".into()
            }
            let a: A = p(&params)?;
            ser(&app.describe(&a.launcher, &a.path)?)
        }
        "instances.list" => {
            #[derive(Deserialize, Default)]
            struct A {
                #[serde(default)]
                online: bool,
            }
            let a: A = if params.is_null() { A::default() } else { p(&params)? };
            ser(&app.instance_statuses(a.online, ctx)?)
        }
        "instance.status" => {
            let a: InstOnline = p(&params)?;
            ser(&e.status(a.instance, a.online, ctx)?)
        }
        "instances.preview" => {
            let r: PreviewRequest = p(&params)?;
            ser(&e.preview(&r, ctx)?)
        }
        "instances.enable" => {
            #[derive(Deserialize)]
            struct A {
                path: std::path::PathBuf,
                #[serde(default = "prism")]
                launcher: String,
                #[serde(default)]
                rules_overrides: String,
                #[serde(default)]
                decisions: Vec<packsync_core::rules::suggest::RuleDecision>,
                #[serde(default = "yes")]
                upload: bool,
            }
            fn prism() -> String {
                "prism".into()
            }
            fn yes() -> bool {
                true
            }
            let a: A = p(&params)?;
            let outcome = app.enable_sync_with(&a.launcher, &a.path, a.rules_overrides, a.decisions, ctx)?;
            let mut upload = Value::Null;
            let mut upload_error = Value::Null;
            if a.upload && e.has_storage() {
                match e.upload_pending(outcome.instance, false, ctx) {
                    Ok(u) => upload = ser(&u)?,
                    Err(err) => upload_error = json!(crate::events::ErrorView::from(&err)),
                }
            }
            Ok(json!({ "outcome": outcome, "upload": upload, "upload_error": upload_error }))
        }
        "instances.disable" => {
            let a: Inst = p(&params)?;
            e.disable_sync(a.instance)?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            Ok(json!({ "instance": a.instance, "sync_enabled": false }))
        }
        "instance.decide_unknown" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                decisions: Vec<packsync_core::rules::suggest::RuleDecision>,
            }
            let a: A = p(&params)?;
            e.decide_unknown(a.instance, &a.decisions)?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            Ok(json!({ "decided": a.decisions.len() }))
        }
        "instance.changes" => {
            let a: Inst = p(&params)?;
            ser(&e.pending_changes(a.instance, ctx)?)
        }
        "instance.history" => {
            let a: InstOnline = p(&params)?;
            ser(&e.history(a.instance, a.online, ctx)?)
        }
        "instance.snapshot" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                #[serde(default)]
                force: bool,
                note: Option<String>,
            }
            let a: A = p(&params)?;
            let r = e.snapshot(a.instance, SnapshotOpts { force: a.force, note: a.note, ..Default::default() }, ctx)?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            ser(&r)
        }
        "instance.upload" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                #[serde(default)]
                force: bool,
            }
            let a: A = p(&params)?;
            let r = e.upload_pending(a.instance, a.force, ctx)?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            ser(&r)
        }
        "instance.sync" => {
            let a: Inst = p(&params)?;
            let (snap, rec) = e.sync_now(a.instance, ctx)?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            Ok(json!({ "snapshot": snap, "reconcile": rec }))
        }
        "instance.reconcile" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                #[serde(default = "yes")]
                apply_fast_forward: bool,
                #[serde(default = "yes")]
                upload_local: bool,
            }
            fn yes() -> bool {
                true
            }
            let a: A = p(&params)?;
            let r = e.reconcile(
                a.instance,
                ReconcileOpts { apply_fast_forward: a.apply_fast_forward, upload_local: a.upload_local },
                ctx,
            )?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            ser(&r)
        }
        "instance.verify_local" => {
            let a: Inst = p(&params)?;
            ser(&e.verify_local(a.instance, ctx)?)
        }
        "instance.verify_remote" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                snapshot: Option<SnapshotId>,
                #[serde(default)]
                deep: bool,
            }
            let a: A = p(&params)?;
            ser(&e.verify_remote(a.instance, a.snapshot, a.deep, ctx)?)
        }
        "cloud.instances" => ser(&e.cloud_instances(ctx)?),
        "restore.preview" => {
            let a: InstSnap = p(&params)?;
            ser(&e.restore_preview(a.instance, a.snapshot, ctx)?)
        }
        "restore.apply" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                snapshot: SnapshotId,
                #[serde(default)]
                skip_incompatible: bool,
            }
            let a: A = p(&params)?;
            let r = e.restore_snapshot(
                a.instance,
                a.snapshot,
                RestoreOpts { skip_incompatible: a.skip_incompatible },
                ctx,
            )?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            ser(&r)
        }
        "restore.new" => {
            let r: RestoreCloudRequest = p(&params)?;
            ser(&app.restore_cloud_instance(r, ctx)?)
        }
        "restore.points" => {
            let a: Inst = p(&params)?;
            ser(&e.list_recovery_points(a.instance)?)
        }
        "conflict.show" => {
            let a: Inst = p(&params)?;
            let r = e.reconcile(a.instance, ReconcileOpts { apply_fast_forward: false, upload_local: false }, ctx)?;
            let is_conflict = matches!(r, ReconcileReport::Conflict(_));
            Ok(json!({ "conflict": is_conflict, "report": r }))
        }
        "conflict.resolve" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                resolution: Resolution,
            }
            let a: A = p(&params)?;
            let r = e.resolve_conflict(a.instance, a.resolution, ctx)?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            ser(&r)
        }
        "crash.promote" => {
            let a: Inst = p(&params)?;
            let n = e.promote_crash(a.instance, ctx)?;
            let upload = e
                .upload_pending(a.instance, false, ctx)
                .map(|u| json!(u))
                .unwrap_or_else(|err| json!({ "error": crate::events::ErrorView::from(&err) }));
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            Ok(json!({ "snapshot": n, "upload": upload }))
        }
        "crash.discard" => {
            let a: Inst = p(&params)?;
            let r = e.discard_crash(a.instance, ctx)?;
            app.bus().emit(crate::events::AppEvent::InstanceChanged { instance: a.instance });
            ser(&r)
        }
        "launch.start" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                #[serde(default)]
                options: LaunchOptions,
            }
            let a: A = p(&params)?;
            ser(&app.launch(a.instance, a.options, ctx)?)
        }
        "hooks.state" => {
            let a: Inst = p(&params)?;
            ser(&app.hooks_state(a.instance)?)
        }
        "hooks.plan_install" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                cli: Option<std::path::PathBuf>,
            }
            let a: A = p(&params)?;
            let cli = match a.cli {
                Some(c) => c,
                None => app.resolve_cli_path()?,
            };
            ser(&app.hooks_plan_install(a.instance, &cli)?)
        }
        "hooks.plan_remove" => {
            let a: Inst = p(&params)?;
            ser(&app.hooks_plan_remove(a.instance)?)
        }
        "hooks.apply" => {
            #[derive(Deserialize)]
            struct A {
                instance: InstanceId,
                plan: HookPlan,
            }
            let a: A = p(&params)?;
            app.hooks_apply(a.instance, &a.plan)?;
            ser(&app.hooks_state(a.instance)?)
        }
        "rules.show" | "rules.explain" => {
            let a: Inst = p(&params)?;
            let inst = e.instance(a.instance)?;
            let (rs, text) = e.rules_for(&inst, ctx)?;
            if method == "rules.show" {
                return Ok(json!({ "rules": text }));
            }
            let path = params
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::invalid("request", "path is required"))?;
            let d = rs.decide(path.trim_start_matches('/'));
            Ok(json!({ "path": path, "synced": d.include, "explanation": d.explain(path) }))
        }
        "gc.plan" => {
            let g: GcParams = if params.is_null() { GcParams::default() } else { p(&params)? };
            ser(&e.gc_plan(&g, ctx)?)
        }
        "gc.run" => {
            #[derive(Deserialize)]
            struct A {
                plan_id: String,
                #[serde(default)]
                params: Option<GcParams>,
            }
            let a: A = p(&params)?;
            ser(&e.gc_run(&a.plan_id, &a.params.unwrap_or_default(), ctx)?)
        }
        "jobs.list" => ser(&app.jobs().list()),
        "jobs.cancel" => {
            #[derive(Deserialize)]
            struct A {
                job: JobId,
            }
            let a: A = p(&params)?;
            Ok(json!({ "cancelled": app.jobs().cancel(a.job) }))
        }
        "events.recent" => {
            #[derive(Deserialize, Default)]
            struct A {
                instance: Option<InstanceId>,
                limit: Option<usize>,
            }
            let a: A = if params.is_null() { A::default() } else { p(&params)? };
            let rows = e.db().events_recent(a.instance, a.limit.unwrap_or(50).min(500))?;
            Ok(json!(rows
                .into_iter()
                .map(|r| json!({ "ts_ms": r.ts_ms, "instance": r.instance_id, "level": r.level, "code": r.code, "message": r.message }))
                .collect::<Vec<_>>()))
        }
        "events.clear" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Empty {}
            if !params.is_null() {
                let _: Empty = p(&params)?;
            }
            let cleared = e.db().events_clear()?;
            Ok(json!({ "cleared": cleared }))
        }
        "events.export" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct A {
                path: std::path::PathBuf,
                #[serde(default)]
                limit: Option<usize>,
            }
            let a: A = p(&params)?;
            if a.path.as_os_str().is_empty() || a.path.is_dir() {
                return Err(Error::invalid("path", "choose a JSON file path"));
            }
            let limit = a.limit.unwrap_or(500).min(5000);
            let rows = e.db().events_recent(None, limit)?;
            let count = rows.len();
            let (dev_id, dev_name) = e.device();
            let exported_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let payload = json!({
                "exported_at_ms": exported_at_ms,
                "version": env!("CARGO_PKG_VERSION"),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "device": { "id": dev_id, "name": dev_name },
                "count": count,
                "events": rows
                    .into_iter()
                    .map(|r| json!({
                        "ts_ms": r.ts_ms,
                        "instance": r.instance_id,
                        "level": r.level,
                        "code": r.code,
                        "message": r.message,
                    }))
                    .collect::<Vec<_>>(),
            });
            let mut bytes = serde_json::to_vec_pretty(&payload)?;
            bytes.push(b'\n');
            packsync_core::fsutil::write_atomic(&a.path, &bytes)
                .map_err(|err| Error::io(format!("writing {}", a.path.display()), err))?;
            Ok(json!({ "path": a.path, "count": count }))
        }
        "diagnostics.report" => {
            #[derive(Deserialize, Default)]
            struct A {
                #[serde(default)]
                online: bool,
            }
            let a: A = if params.is_null() { A::default() } else { p(&params)? };
            ser(&crate::diagnostics::report(app, a.online, ctx)?)
        }
        other => Err(Error::NotFound(format!("unknown method {other:?}"))),
    }
}

/// Run a long method as a background job (progress on the event bus, cancellable).
pub fn start_job(app: &Arc<App>, method: &str, params: Value) -> Result<JobId> {
    if !is_known(method) {
        return Err(Error::NotFound(format!("unknown method {method:?}")));
    }
    let inst = instance_of(&params);
    let (a, name) = (app.clone(), method.to_string());
    let key_params = params.clone();
    app.jobs().start_with_params(method, inst, key_params, move |ctx| call(&a, &name, params, ctx))
}

#[cfg(test)]
mod tests {
    use super::validate_external_url;

    #[test]
    fn external_url_allows_only_https_and_loopback_http() {
        assert!(validate_external_url("https://microsoft.com/link").is_ok());
        assert!(validate_external_url("https://packsync.example.workers.dev/privacy").is_ok());
        assert!(validate_external_url("http://127.0.0.1:8787/privacy").is_ok());
        assert!(validate_external_url("http://localhost:8787/terms").is_ok());

        for bad in [
            "",
            "http://example.com/phish",
            "http://127.0.0.1.evil.com/",
            "http://localhost.evil.com/",
            "https://user:pass@evil.com/",
            "https://evil.com\\path",
            "https://example.com/a\nb",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<script>alert(1)</script>",
        ] {
            assert!(validate_external_url(bad).is_err(), "{bad:?} must be rejected");
        }
    }
}
