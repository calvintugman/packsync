//! The application object: owns the engine, the settings, the storage connection and the job runner.
//! The CLI, the desktop shell and the dev bridge are thin front ends over this type.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use packsync_core::credentials::CredentialStore;
use packsync_core::engine::{
    EnableOutcome, EnableRequest, Engine, EngineConfig, InstanceStatus, RestoreNewRequest, RestoredInstance,
};
use packsync_core::error::{Error, Result};
use packsync_core::ids::{InstanceId, SnapshotId};
use packsync_core::launcher::{DiscoveredInstance, LauncherRef, LocalSettings};
use packsync_core::progress::{CancelToken, OpCtx};
use packsync_core::retry::Sleeper;
use packsync_core::storage::Storage;
use packsync_drive::oauth::{AccountInfo, Authenticator, UrlOpener};
use packsync_prism::PrismLauncher;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::events::{AppEvent, BusSink, EventBus};
use crate::jobs::JobRunner;
use crate::paths::AppPaths;
use crate::provider::{self, StorageStatus};
use crate::settings::{Settings, StorageSetting};

/// Wraps whatever storage the settings describe (fault injection, metering) before the engine uses it.
pub type StorageWrapper = Arc<dyn Fn(Arc<dyn Storage>) -> Arc<dyn Storage> + Send + Sync>;

/// Injection points for tests and special setups. `Default` is what the real application uses.
#[derive(Default)]
pub struct AppOptions {
    /// Credential store for the Google refresh token (default: the OS keychain).
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub sleeper: Option<Arc<dyn Sleeper>>,
    /// Replace the Prism adapter (tests point it at a disposable data directory).
    pub prism: Option<PrismLauncher>,
    /// Skip startup recovery (tests that exercise it explicitly).
    pub skip_recovery: bool,
    /// Wrap the configured storage, so a test can make "the cloud" fail on demand.
    pub storage_wrapper: Option<StorageWrapper>,
}

pub struct App {
    pub paths: AppPaths,
    settings: RwLock<Settings>,
    settings_lock: parking_lot::Mutex<()>,
    engine: Arc<Engine>,
    auth: RwLock<Option<Arc<Authenticator>>>,
    creds: Arc<dyn CredentialStore>,
    prism: RwLock<Arc<PrismLauncher>>,
    prism_fixed: bool,
    storage_wrapper: Option<StorageWrapper>,
    /// A pairing code shown and waiting for the other computer ([`App::hosted_pair_start`]).
    pairing: parking_lot::Mutex<Option<packsync_hosted::pairing::PairingOffer>>,
    /// An in-flight Microsoft device-code sign-in waiting to complete ([`App::hosted_login_start`]).
    pending_login: parking_lot::Mutex<Option<packsync_hosted::msauth::PendingDeviceLogin>>,
    bus: EventBus,
    jobs: JobRunner,
}

/// Result of [`App::update_settings`]. The settings are saved whenever this is returned; a storage that
/// could not be set up (missing OAuth client file, unreachable folder) is reported as a problem, not
/// as a failure, so the user can fix it without losing the rest of their choices.
#[derive(Debug, Clone)]
pub struct SettingsUpdate {
    pub settings: Settings,
    pub storage_problem: Option<String>,
}

/// A launcher instance as the UI lists it, with its PackSync registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredView {
    #[serde(flatten)]
    pub instance: DiscoveredInstance,
    pub registered: Option<InstanceId>,
    pub sync_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RestoreCloudRequest {
    pub instance: Option<InstanceId>,
    pub snapshot: Option<SnapshotId>,
    /// Which Prism installation to restore into (default: the first found).
    pub data_dir: Option<PathBuf>,
    /// Folder name in Prism's instances folder (default: derived from the pack's name; made unique).
    pub folder_name: Option<String>,
    #[serde(default)]
    pub local: LocalSettings,
    #[serde(default)]
    pub skip_incompatible: bool,
}

impl App {
    pub fn open(paths: AppPaths, opts: AppOptions) -> Result<Arc<App>> {
        for d in [&paths.config_dir, &paths.state_dir, &paths.logs_dir] {
            std::fs::create_dir_all(d).map_err(|e| Error::io(format!("creating {}", d.display()), e))?;
        }
        let settings = Settings::load(&paths.settings_file())?;
        let engine = {
            let mut cfg = EngineConfig::new(&paths.state_dir);
            apply_engine_settings(&mut cfg, &settings);
            match opts.sleeper {
                Some(s) => Engine::open_with_sleeper(cfg, s)?,
                None => Engine::open(cfg)?,
            }
        };
        let prism_fixed = opts.prism.is_some();
        let prism = Arc::new(opts.prism.unwrap_or_else(|| prism_from_settings(&settings)));
        engine.register_launcher(prism.clone());
        let bus = EventBus::new();
        let jobs = JobRunner::new(bus.clone(), engine.db().clone());
        let app = Arc::new(App {
            paths,
            settings: RwLock::new(settings),
            settings_lock: parking_lot::Mutex::new(()),
            engine,
            auth: RwLock::new(None),
            creds: opts.credentials.unwrap_or_else(provider::default_credentials),
            prism: RwLock::new(prism),
            prism_fixed,
            storage_wrapper: opts.storage_wrapper,
            pairing: parking_lot::Mutex::new(None),
            pending_login: parking_lot::Mutex::new(None),
            bus,
            jobs,
        });
        // A storage that cannot be built (missing OAuth client file, ...) must not stop the app from
        // starting: the status says what is wrong and local features keep working.
        if let Err(e) = app.apply_storage() {
            app.engine.db().event(None, "warn", "storage_unavailable", &e.to_string());
        }
        if !opts.skip_recovery {
            app.close_signalled_sessions(&OpCtx::detached());
            if let Err(e) = app.engine.recover(&OpCtx::detached()) {
                app.engine.db().event(None, "warn", "recovery_failed", &e.to_string());
            }
        }
        Ok(app)
    }

    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }
    pub fn jobs(&self) -> &JobRunner {
        &self.jobs
    }
    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }
    pub fn credential_store_description(&self) -> String {
        self.creds.describe()
    }
    pub fn prism(&self) -> Arc<PrismLauncher> {
        self.prism.read().clone()
    }

    /// While the app is open, uploads that wait for their retry time are started again (the CLI does not
    /// run this: its next sync or launch uploads). They run as ordinary `instance.upload` jobs, so they
    /// show in Activity and can be cancelled.
    pub fn start_upload_retries(self: &Arc<Self>, every: std::time::Duration) {
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("upload-retries".into()).spawn(move || {
            loop {
                std::thread::sleep(every);
                let Some(app) = weak.upgrade() else { return };
                app.retry_uploads_once();
            }
        });
    }

    /// One pass of the retry scheduler; returns the instances an upload was started for. Skips instances
    /// with a job running, with an unresolved problem, waiting for their backoff, or whose publication
    /// needs the person's decision (data missing everywhere).
    pub fn retry_uploads_once(self: &Arc<Self>) -> Vec<InstanceId> {
        let mut started = vec![];
        if !self.engine.has_storage() {
            return started;
        }
        let busy: std::collections::HashSet<InstanceId> = self
            .jobs
            .list()
            .into_iter()
            .filter(|j| j.state == crate::events::JobState::Running)
            .filter_map(|j| j.instance)
            .collect();
        let now = packsync_core::db::now_ms();
        let Ok(instances) = self.engine.instances() else { return started };
        for inst in instances {
            if !inst.sync_enabled || inst.state != packsync_core::db::InstanceState::Ok || busy.contains(&inst.id) {
                continue;
            }
            if self.engine.db().publish_blocked(inst.id).ok().flatten().is_some() {
                continue;
            }
            let Ok(st) = self.engine.status(inst.id, false, &OpCtx::detached()) else { continue };
            if st.pending_snapshots == 0 || st.next_retry_ms.is_some_and(|t| t > now) {
                continue;
            }
            let quota_blocked = self.engine.db().snapshots(inst.id).ok().is_some_and(|snaps| {
                snaps.into_iter().filter(|s| !s.state.is_in_cloud()).any(|s| {
                    self.engine.db().staged_for(s.id).ok().is_some_and(|ps| {
                        ps.iter().any(|p| !p.uploaded && p.last_error_code.as_deref() == Some("quota_exceeded"))
                    })
                })
            });
            if quota_blocked {
                continue;
            }
            let (app, id) = (self.clone(), inst.id);
            let job = self.jobs.start("instance.upload", Some(id), move |ctx| {
                let r = app.engine.upload_pending(id, false, ctx);
                app.bus().emit(AppEvent::InstanceChanged { instance: id });
                Ok(serde_json::to_value(r?)?)
            });
            if job.is_ok() {
                started.push(id);
            }
        }
        started
    }

    /// A context whose progress goes to the event bus (what the UI shows), with its own cancel token.
    pub fn ctx(&self) -> OpCtx {
        OpCtx::new(BusSink::new(self.bus.clone()), CancelToken::new())
    }

    // ------------------------------------------------------------------------------ settings
    /// Change settings: validate, save (atomically), then apply (storage, launcher, device name).
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Result<SettingsUpdate> {
        let _guard = self.settings_lock.lock();
        let prev = self.settings.read().clone();
        let mut next = prev.clone();
        f(&mut next);
        next.validate()?;
        next.save(&self.paths.settings_file())?;
        let storage_changed = next.storage != prev.storage || next.google_client_file != prev.google_client_file;
        let device_changed = next.device_name != prev.device_name;
        *self.settings.write() = next.clone();
        self.engine.update_runtime_options(next.upload_concurrency, next.lease_ttl_secs, next.recovery_point_days);
        if device_changed && let Some(n) = &next.device_name {
            self.engine.rename_device(n)?;
        }
        if !self.prism_fixed {
            let p = Arc::new(prism_from_settings(&next));
            self.engine.register_launcher(p.clone());
            *self.prism.write() = p;
        }
        let mut storage_problem = None;
        if storage_changed {
            if let Err(e) = self.apply_storage() {
                self.engine.db().event(None, "warn", "storage_unavailable", &e.to_string());
                storage_problem = Some(e.to_string());
            }
            self.bus.emit(AppEvent::StorageChanged);
        }
        Ok(SettingsUpdate { settings: next, storage_problem })
    }

    fn wrap_storage(&self, s: Arc<dyn Storage>) -> Arc<dyn Storage> {
        match &self.storage_wrapper {
            Some(wrap) => wrap(s),
            None => s,
        }
    }

    fn apply_storage(&self) -> Result<()> {
        let settings = self.settings();
        // Detach first: a failure to build the new storage must not leave the old one silently active.
        self.engine.set_storage(None)?;
        *self.auth.write() = None;
        let p = provider::build(&settings, &self.paths, self.engine.db(), self.creds.clone())?;
        *self.auth.write() = p.auth;
        let storage = p.storage.map(|s| self.wrap_storage(s));
        self.engine.set_storage(storage)
    }

    // ------------------------------------------------------------------------------ storage
    pub fn storage_status(&self) -> StorageStatus {
        provider::status(&self.settings.read(), &self.paths, self.auth.read().as_deref())
    }

    pub fn use_local_storage(&self, dir: &Path) -> Result<()> {
        provider::check_local_folder(dir)?;
        let dir = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let up = self.update_settings(|s| s.storage = StorageSetting::Local { path: dir.clone() })?;
        match up.storage_problem {
            Some(p) => Err(Error::Config(p)),
            None => Ok(()),
        }
    }

    /// Select Google Drive (does not sign in; see [`App::google_login`]). The selection is saved even
    /// when the OAuth client is not configured yet; the returned status says what is missing.
    pub fn use_google(&self, space: &str) -> Result<StorageStatus> {
        let account = match self.settings().storage {
            StorageSetting::Google { space: ref s, ref account } if s == space => account.clone(),
            _ => None,
        };
        self.update_settings(|s| s.storage = StorageSetting::Google { space: space.to_string(), account })?;
        Ok(self.storage_status())
    }

    /// Select Cloudflare R2. The secret access key goes to the OS credential store (never to the
    /// settings file); the connection is then checked with one listing.
    pub fn use_r2(
        &self,
        account_id: &str,
        bucket: &str,
        prefix: &str,
        access_key_id: &str,
        secret: &packsync_core::redact::Secret,
        ctx: &OpCtx,
    ) -> Result<StorageStatus> {
        let (account_id, bucket, prefix, access_key_id) =
            (account_id.trim(), bucket.trim(), prefix.trim().trim_matches('/'), access_key_id.trim());
        let setting = StorageSetting::R2 {
            account_id: account_id.to_string(),
            bucket: bucket.to_string(),
            prefix: prefix.to_string(),
            access_key_id: access_key_id.to_string(),
        };
        let mut probe = self.settings();
        probe.storage = setting.clone();
        probe.validate()?;
        if secret.is_empty() {
            return Err(Error::Config("the secret access key is empty".into()));
        }
        // Verify connectivity before overwriting existing credentials or settings.
        let (tmp, _stats) = provider::build_r2(account_id, bucket, prefix, access_key_id, secret.clone())?;
        self.wrap_storage(tmp).list("instances/", ctx)?;

        self.creds
            .set(&StorageSetting::r2_secret_account(account_id, access_key_id), secret)
            .map_err(|e| Error::Config(e.to_string()))?;
        let up = self.update_settings(|s| s.storage = setting)?;
        if let Some(p) = up.storage_problem {
            return Err(Error::Config(p));
        }
        Ok(self.storage_status())
    }

    // ------------------------------------------------------------------------ PackSync cloud

    /// The service to use: `explicit`, else the one already configured, else the one built in.
    fn hosted_endpoint(&self, explicit: Option<&str>) -> Result<String> {
        if let Some(e) = explicit.map(str::trim).filter(|e| !e.is_empty()) {
            return Ok(e.trim_end_matches('/').to_string());
        }
        if let StorageSetting::Hosted { endpoint, .. } = self.settings().storage {
            return Ok(endpoint);
        }
        packsync_hosted::default_endpoint().ok_or_else(|| {
            Error::Config(
                "this build of PackSync has no cloud service address; give one with --service \
                 (or the PACKSYNC_HOSTED_URL environment variable)"
                    .into(),
            )
        })
    }

    /// Use `c` from now on: check the connection first so a bad configuration never clobbers the
    /// existing storage, then write the secret to the credential store and update settings.
    fn save_hosted(&self, c: &packsync_hosted::Credentials, ctx: &OpCtx) -> Result<StorageStatus> {
        self.save_hosted_with_name(c, None, ctx)
    }

    fn save_hosted_with_name(
        &self,
        c: &packsync_hosted::Credentials,
        display_name: Option<String>,
        ctx: &OpCtx,
    ) -> Result<StorageStatus> {
        let setting = StorageSetting::Hosted {
            endpoint: c.account.endpoint.clone(),
            account: c.account.account.clone(),
            display_name,
        };
        let mut probe = self.settings();
        probe.storage = setting.clone();
        probe.validate()?;
        let (tmp, _stats) = packsync_hosted::open_storage(c).map_err(Error::from)?;
        self.wrap_storage(tmp).list("instances/", ctx)?;

        self.creds
            .set(&StorageSetting::hosted_secret_account(&c.account.account), &c.secret())
            .map_err(|e| Error::Config(e.to_string()))?;
        let up = self.update_settings(|s| s.storage = setting)?;
        if let Some(p) = up.storage_problem {
            return Err(Error::Config(p));
        }
        Ok(self.storage_status())
    }

    /// Start Microsoft / Minecraft device-code sign-in for the PackSync cloud.
    pub fn hosted_login_start(
        &self,
        endpoint: Option<&str>,
        opener: Option<&dyn UrlOpener>,
        ctx: &OpCtx,
    ) -> Result<packsync_hosted::msauth::DeviceLoginPrompt> {
        let ep = self.hosted_endpoint(endpoint)?;
        let pending = packsync_hosted::msauth::start_device_login(&ep, ctx)?;
        let prompt = pending.prompt().clone();
        *self.pending_login.lock() = Some(pending);
        if let Some(open) = opener
            && let Err(e) = open.open(&prompt.verification_uri)
        {
            tracing::warn!(target: "packsync_app", error = %e, "could not open verification URL in browser");
        }
        Ok(prompt)
    }

    /// Wait for the Microsoft device-code sign-in started by [`App::hosted_login_start`] and
    /// connect this computer to the PackSync cloud account.
    pub fn hosted_login_wait(&self, ctx: &OpCtx) -> Result<StorageStatus> {
        let pending = self
            .pending_login
            .lock()
            .take()
            .ok_or_else(|| Error::Config("no Microsoft sign-in is in progress; start sign-in first".into()))?;
        let outcome = pending.wait(ctx)?;
        self.save_hosted_with_name(&outcome.credentials, outcome.display_name, ctx)
    }

    /// Permanently delete the connected PackSync cloud account, all cloud data, and local credentials.
    /// Also removes local sync registrations bound to the deleted account without touching local files.
    pub fn hosted_delete_account(&self, ctx: &OpCtx) -> Result<StorageStatus> {
        let c = self.hosted_credentials()?;
        let hosted = packsync_hosted::HostedStorage::new(c.account.clone())?;
        let raw_id = packsync_core::storage::Storage::provider_id(&hosted);
        let enc_id = format!("{raw_id}+e2e");
        let active_id = self.engine.storage_id();
        hosted.delete_account(ctx)?;
        if let Ok(rows) = self.engine.instances() {
            for row in rows {
                let matches_deleted = row
                    .provider_id
                    .as_deref()
                    .is_some_and(|p| p == raw_id || p == enc_id || active_id.as_deref() == Some(p));
                if matches_deleted {
                    if let Ok(plan) = self.hooks_plan_remove(row.id)
                        && !plan.changes.is_empty()
                        && plan.blockers.is_empty()
                    {
                        let _ = self.hooks_apply(row.id, &plan);
                    }
                    if self.engine.disable_sync(row.id).is_ok() {
                        self.bus.emit(AppEvent::InstanceChanged { instance: row.id });
                    }
                }
            }
        }
        let _ = self.creds.delete(&StorageSetting::hosted_secret_account(&c.account.account));
        *self.pairing.lock() = None;
        *self.pending_login.lock() = None;
        self.update_settings(|s| s.storage = StorageSetting::None)?;
        Ok(self.storage_status())
    }

    /// First computer: create an account on the PackSync cloud (with a new encryption key) and use it.
    pub fn hosted_start(&self, endpoint: Option<&str>, ctx: &OpCtx) -> Result<StorageStatus> {
        if matches!(self.settings().storage, StorageSetting::Hosted { .. }) {
            return Err(Error::Config(
                "this computer already uses the PackSync cloud; to add another computer, run `packsync storage pair` here".into(),
            ));
        }
        let ep = self.hosted_endpoint(endpoint)?;
        let c = packsync_hosted::create_account(&ep, ctx)?;
        self.save_hosted(&c, ctx)
    }

    /// Another computer: join the account of the computer showing `code`.
    pub fn hosted_join(&self, code: &str, endpoint: Option<&str>, ctx: &OpCtx) -> Result<StorageStatus> {
        let ep = self.hosted_endpoint(endpoint)?;
        let c = packsync_hosted::pairing::join(&ep, code, ctx)?;
        self.save_hosted(&c, ctx)
    }

    /// Any computer: use the account a recovery key describes.
    pub fn hosted_recover(&self, key: &packsync_core::redact::Secret, ctx: &OpCtx) -> Result<StorageStatus> {
        let c = packsync_hosted::Credentials::from_recovery_key(key.expose())?;
        self.save_hosted(&c, ctx)
    }

    fn hosted_credentials(&self) -> Result<packsync_hosted::Credentials> {
        match self.settings().storage {
            StorageSetting::Hosted { endpoint, account, .. } => {
                provider::hosted_credentials(&*self.creds, &endpoint, &account)
            }
            _ => Err(Error::Config("this computer does not use the PackSync cloud".into())),
        }
    }

    /// Show a pairing code for another computer; then call [`App::hosted_pair_wait`].
    pub fn hosted_pair_start(&self, ctx: &OpCtx) -> Result<String> {
        let c = self.hosted_credentials()?;
        let hosted = Arc::new(packsync_hosted::HostedStorage::new(c.account.clone())?);
        let offer = packsync_hosted::pairing::PairingOffer::start(hosted, &c, ctx)?;
        let code = offer.code().to_string();
        *self.pairing.lock() = Some(offer);
        Ok(code)
    }

    /// Wait (up to 10 minutes) until another computer joined with the code shown last.
    pub fn hosted_pair_wait(&self, ctx: &OpCtx) -> Result<()> {
        let offer = self
            .pairing
            .lock()
            .take()
            .ok_or_else(|| Error::Config("no pairing code is being shown; make one first".into()))?;
        offer.wait(ctx).map_err(Error::from)
    }

    /// The text that restores access without pairing. A secret: show it only when asked.
    pub fn hosted_recovery_key(&self) -> Result<packsync_core::redact::Secret> {
        Ok(self.hosted_credentials()?.recovery_key())
    }

    /// Interactive sign-in (opens the system browser). Stores the refresh token in the OS credential
    /// store and remembers the account name in the settings.
    pub fn google_login(&self, opener: &dyn UrlOpener, ctx: &OpCtx) -> Result<AccountInfo> {
        let auth = self.auth.read().clone().ok_or_else(|| {
            Error::Config(match provider::client_config(&self.settings.read(), &self.paths) {
                Err(e) => e.to_string(),
                Ok(_) => "Google Drive is not selected as the storage yet".into(),
            })
        })?;
        let info = provider::login(&auth, opener, ctx)?;
        let (space, account) = match self.settings().storage {
            StorageSetting::Google { space, .. } => (space, info.email.clone()),
            _ => return Err(Error::Config("Google Drive is not selected as the storage".into())),
        };
        self.update_settings(|s| s.storage = StorageSetting::Google { space, account })?;
        Ok(info)
    }

    /// Forget the connection. With `revoke`, Google is also asked to invalidate the grant. Cloud data
    /// is never touched.
    pub fn disconnect_storage(&self, revoke: bool, ctx: &OpCtx) -> Result<()> {
        if let Some(auth) = self.auth.read().clone() {
            if revoke {
                auth.revoke(ctx).map_err(|e| Error::from(e.to_storage_error()))?;
            }
            auth.disconnect().map_err(|e| Error::from(e.to_storage_error()))?;
        }
        match self.settings().storage {
            StorageSetting::R2 { account_id, access_key_id, .. } => {
                let _ = self.creds.delete(&StorageSetting::r2_secret_account(&account_id, &access_key_id));
            }
            StorageSetting::Hosted { account, .. } => {
                let _ = self.creds.delete(&StorageSetting::hosted_secret_account(&account));
            }
            _ => {}
        }
        self.update_settings(|s| s.storage = StorageSetting::None)?;
        Ok(())
    }

    /// Does the connection work right now? Returns how many bytes are free in the cloud, if known.
    pub fn test_storage(&self, ctx: &OpCtx) -> Result<serde_json::Value> {
        let st = self.engine.storage()?;
        let q = st.quota(ctx)?;
        let t = std::time::Instant::now();
        let n = st.list("instances/", ctx)?.len();
        let list_ms = t.elapsed().as_millis() as u64;
        Ok(serde_json::json!({ "provider": st.provider_id(), "quota": q, "instances_in_cloud": n, "list_ms": list_ms }))
    }

    // ------------------------------------------------------------------------------ instances
    pub fn discover(&self) -> Result<Vec<DiscoveredView>> {
        let launcher = self.engine.launcher("prism");
        let mut out = vec![];
        for d in launcher.discover()? {
            let row = self.engine.find_instance_by_root(&d.root)?;
            out.push(DiscoveredView {
                registered: row.as_ref().map(|r| r.id),
                sync_enabled: row.map(|r| r.sync_enabled).unwrap_or(false),
                instance: d,
            });
        }
        Ok(out)
    }

    pub fn describe(&self, launcher: &str, root: &Path) -> Result<DiscoveredInstance> {
        self.engine.launcher(launcher).describe(root)
    }

    /// Start syncing an instance folder the user selected (or discovery found).
    pub fn enable_sync(
        &self,
        launcher: &str,
        root: &Path,
        rules_overrides: String,
        ctx: &OpCtx,
    ) -> Result<EnableOutcome> {
        self.enable_sync_with(launcher, root, rules_overrides, vec![], ctx)
    }

    /// [`App::enable_sync`] with the person's answers about content no rule covers (from the preview).
    pub fn enable_sync_with(
        &self,
        launcher: &str,
        root: &Path,
        rules_overrides: String,
        decisions: Vec<packsync_core::rules::suggest::RuleDecision>,
        ctx: &OpCtx,
    ) -> Result<EnableOutcome> {
        let d = self.engine.launcher(launcher).describe(root)?;
        let out = self.engine.enable_sync(
            EnableRequest {
                root: d.root,
                launcher: launcher.to_string(),
                name: Some(d.name),
                rules_overrides,
                decisions,
                launcher_ref: Some(d.launcher_ref),
            },
            ctx,
        )?;
        self.bus.emit(AppEvent::InstanceChanged { instance: out.instance });
        Ok(out)
    }

    /// Status of every registered instance (local facts only unless `online`).
    pub fn instance_statuses(&self, online: bool, ctx: &OpCtx) -> Result<Vec<InstanceStatus>> {
        let mut out = vec![];
        for row in self.engine.instances()? {
            match self.engine.status(row.id, online, ctx) {
                Ok(s) => out.push(s),
                Err(e) => self.engine.db().event(Some(row.id), "warn", "status_failed", &e.to_string()),
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------------ restore
    /// Bring a cloud instance to this computer as a new Prism instance.
    pub fn restore_cloud_instance(&self, req: RestoreCloudRequest, ctx: &OpCtx) -> Result<RestoredInstance> {
        let prism = self.prism();
        let data_dir = match req.data_dir.clone() {
            Some(d) => d,
            None => prism.data_dirs().into_iter().next().ok_or_else(|| {
                Error::Config("Prism Launcher's data folder was not found. Start Prism once, or set its location in Settings → Launcher".into())
            })?,
        };
        let cloud = self.engine.cloud_instances(ctx)?;
        let which = match req.instance {
            Some(id) => cloud.iter().find(|c| c.id == id),
            None if cloud.len() == 1 => cloud.first(),
            None => return Err(Error::invalid("restore request", "several cloud instances exist; choose one")),
        }
        .ok_or_else(|| Error::NotFound("that instance was not found in the cloud".into()))?;
        let r = self.engine.restore_new(
            RestoreNewRequest {
                instance: which.id,
                snapshot: req.snapshot,
                parent_dir: packsync_prism::discovery::instances_dir(&data_dir),
                folder_name: req.folder_name.clone().unwrap_or_else(|| which.name.clone()),
                launcher: packsync_prism::KIND.into(),
                launcher_ref: Some(LauncherRef {
                    launcher: packsync_prism::KIND.into(),
                    data_dir: Some(data_dir),
                    instance_id: None,
                    executable: None,
                }),
                local: req.local,
                skip_incompatible: req.skip_incompatible,
            },
            ctx,
        )?;
        self.bus.emit(AppEvent::InstanceChanged { instance: r.instance });
        Ok(r)
    }
}

fn apply_engine_settings(cfg: &mut EngineConfig, s: &Settings) {
    cfg.device_name = s.device_name.clone();
    cfg.upload_concurrency = s.upload_concurrency;
    cfg.lease_ttl_secs = s.lease_ttl_secs;
    cfg.restore.keep_recovery_days = s.recovery_point_days;
}

fn prism_from_settings(s: &Settings) -> PrismLauncher {
    let mut p = PrismLauncher::new();
    if !s.prism_data_dirs.is_empty() {
        p = p.with_data_dirs(s.prism_data_dirs.clone());
    }
    if let Some(e) = &s.prism_executable {
        p = p.with_executable(e.clone());
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use packsync_core::credentials::MemoryCredentialStore;

    pub(crate) fn open_in(root: &Path, prism_data: &Path) -> Arc<App> {
        let prism = PrismLauncher::new()
            .with_data_dirs(vec![prism_data.to_path_buf()])
            .with_executable(PathBuf::from("/nonexistent/prismlauncher"));
        App::open(
            AppPaths::under(root),
            AppOptions {
                credentials: Some(Arc::new(MemoryCredentialStore::default())),
                prism: Some(prism),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn opens_with_no_storage_and_switches_to_a_local_folder() {
        let d = tempfile::tempdir().unwrap();
        let prism = d.path().join("prism");
        std::fs::create_dir_all(prism.join("instances")).unwrap();
        let app = open_in(&d.path().join("app"), &prism);
        let st = app.storage_status();
        assert_eq!((st.kind.as_str(), st.connected), ("none", false));
        assert!(app.engine().storage().is_err(), "no storage until one is chosen");
        let cloud = d.path().join("cloud");
        app.use_local_storage(&cloud).unwrap();
        assert!(app.storage_status().connected);
        assert!(app.test_storage(&app.ctx()).is_ok());
        // the choice survives a restart
        drop(app);
        let app = open_in(&d.path().join("app"), &prism);
        assert_eq!(app.storage_status().kind, "local");
        assert!(app.engine().storage().is_ok());
    }

    #[test]
    fn google_without_an_oauth_client_explains_how_to_set_one_up_and_local_features_keep_working() {
        let d = tempfile::tempdir().unwrap();
        let prism = d.path().join("prism");
        std::fs::create_dir_all(&prism).unwrap();
        let app = open_in(&d.path().join("app"), &prism);
        let st = app.use_google("visible_folder").unwrap();
        assert!(!st.oauth_client_configured && !st.connected);
        assert!(st.problem.as_deref().unwrap().contains("Desktop app"), "{st:?}");
        assert!(st.setup_hint.is_some());
        // the selection is saved, but no storage is active, and everything local still works
        assert!(app.engine().storage().is_err());
        assert!(app.discover().is_ok());
        // an explicitly configured client file that does not exist is reported, not silently ignored
        let up = app.update_settings(|s| s.google_client_file = Some("/definitely/not/here.json".into())).unwrap();
        assert!(up.storage_problem.unwrap().contains("not/here.json"));
        assert!(app.storage_status().problem.unwrap().contains("not/here.json"));
        // a valid client file (Google's "installed" JSON shape) makes the storage buildable; sign-in is still needed
        let client = d.path().join("client.json");
        std::fs::write(&client, r#"{"installed":{"client_id":"123-abc.apps.googleusercontent.com","client_secret":"GOCSPX-testsecret","auth_uri":"https://accounts.google.com/o/oauth2/v2/auth","token_uri":"https://oauth2.googleapis.com/token"}}"#).unwrap();
        let up = app.update_settings(|s| s.google_client_file = Some(client.clone())).unwrap();
        assert!(up.storage_problem.is_none(), "{:?}", up.storage_problem);
        let st = app.storage_status();
        assert!(st.oauth_client_configured && st.needs_reconnect && !st.connected, "{st:?}");
        assert!(app.engine().storage().is_ok());
        // using the not-yet-signed-in storage fails with an authentication error the UI can act on
        let e = app.engine().storage().unwrap().list("instances/", &app.ctx()).unwrap_err();
        assert!(matches!(e, packsync_core::storage::StorageError::Auth(_)), "{e:?}");
        // settings validation protects the stored file
        assert!(app.update_settings(|s| s.upload_concurrency = 0).is_err());
        assert_eq!(app.settings().upload_concurrency, 4);
    }

    #[test]
    fn failed_preflight_keeps_existing_storage_and_credentials_intact() {
        let d = tempfile::tempdir().unwrap();
        let prism = d.path().join("prism");
        std::fs::create_dir_all(&prism).unwrap();
        let creds = Arc::new(MemoryCredentialStore::default());
        let reject_cloud: StorageWrapper = Arc::new(|s| {
            if s.provider_id().starts_with("local:") {
                return s;
            }
            struct Rejecting(Arc<dyn Storage>);
            impl Storage for Rejecting {
                fn provider_id(&self) -> String {
                    self.0.provider_id()
                }
                fn capabilities(&self) -> packsync_core::storage::Capabilities {
                    self.0.capabilities()
                }
                fn list(
                    &self,
                    _: &str,
                    _: &OpCtx,
                ) -> packsync_core::storage::StorageResult<Vec<packsync_core::storage::ObjectInfo>> {
                    Err(packsync_core::storage::StorageError::Auth(packsync_core::storage::AuthProblem::Revoked))
                }
                fn stat(
                    &self,
                    k: &str,
                    c: &OpCtx,
                ) -> packsync_core::storage::StorageResult<Option<packsync_core::storage::ObjectInfo>> {
                    self.0.stat(k, c)
                }
                fn get(
                    &self,
                    k: &str,
                    r: Option<(u64, u64)>,
                    c: &OpCtx,
                ) -> packsync_core::storage::StorageResult<Box<dyn std::io::Read + Send>> {
                    self.0.get(k, r, c)
                }
                fn get_with_info(
                    &self,
                    k: &str,
                    m: u64,
                    c: &OpCtx,
                ) -> packsync_core::storage::StorageResult<Option<(Vec<u8>, packsync_core::storage::ObjectInfo)>>
                {
                    self.0.get_with_info(k, m, c)
                }
                fn put(
                    &self,
                    k: &str,
                    s: packsync_core::storage::PutSource<'_>,
                    o: &packsync_core::storage::PutOptions,
                    c: &OpCtx,
                ) -> packsync_core::storage::StorageResult<packsync_core::storage::ObjectInfo> {
                    self.0.put(k, s, o, c)
                }
                fn delete(
                    &self,
                    k: &str,
                    m: packsync_core::storage::DeleteMode,
                    c: &OpCtx,
                ) -> packsync_core::storage::StorageResult<()> {
                    self.0.delete(k, m, c)
                }
                fn quota(
                    &self,
                    c: &OpCtx,
                ) -> packsync_core::storage::StorageResult<Option<packsync_core::storage::Quota>> {
                    self.0.quota(c)
                }
            }
            Arc::new(Rejecting(s))
        });
        let app = App::open(
            AppPaths::under(&d.path().join("app")),
            AppOptions {
                credentials: Some(creds.clone()),
                prism: Some(PrismLauncher::new().with_data_dirs(vec![prism])),
                storage_wrapper: Some(reject_cloud),
                ..Default::default()
            },
        )
        .unwrap();
        let cloud = d.path().join("cloud");
        app.use_local_storage(&cloud).unwrap();

        let err = app
            .use_r2("acct123", "bucket", "prefix", "key123", &packsync_core::redact::Secret::new("secret"), &app.ctx())
            .unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert_eq!(app.storage_status().kind, "local");
        assert!(creds.get(&StorageSetting::r2_secret_account("acct123", "key123")).unwrap().is_none());

        let hosted = packsync_hosted::Credentials::from_secret(
            "http://127.0.0.1:9",
            "abcdefghijklmnopqrstuvwxyz",
            &packsync_core::redact::Secret::new("tok.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        )
        .unwrap();
        let rec = hosted.recovery_key();
        let err = app.hosted_recover(&rec, &app.ctx()).unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert_eq!(app.storage_status().kind, "local");
        assert!(creds.get(&StorageSetting::hosted_secret_account(&hosted.account.account)).unwrap().is_none());
    }
}
