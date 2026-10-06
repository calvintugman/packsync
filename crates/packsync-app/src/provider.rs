//! Turning settings into a [`Storage`]: a local folder (tests, demos, a NAS or a folder another
//! sync client mirrors) or Google Drive with the refresh token in the OS credential store.

use std::path::Path;
use std::sync::Arc;

use packsync_core::credentials::CredentialStore;
use packsync_core::db::Db;
use packsync_core::error::{Error, Result};
use packsync_core::progress::OpCtx;
use packsync_core::storage::{Storage, TransferStateStore};
use packsync_core::storage_local::LocalDirStorage;
use packsync_drive::oauth::{AccountInfo, AuthConfig, AuthStatus, Authenticator, LoginOptions, UrlOpener};
use packsync_drive::{
    ClientConfigError, DriveConfig, DriveSpace, DriveStorage, KeyringCredentialStore, OAuthClientConfig,
};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;
use crate::settings::{Settings, StorageSetting};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageStatus {
    /// `none`, `local` or `google`.
    pub kind: String,
    pub connected: bool,
    /// One line for the UI: the folder, or the account and where its files live.
    pub detail: String,
    pub account: Option<String>,
    pub space: Option<String>,
    /// The saved sign-in no longer works (revoked / expired): reconnect.
    pub needs_reconnect: bool,
    /// Google only: is an OAuth client configured? When not, `setup_hint` says how.
    pub oauth_client_configured: bool,
    pub oauth_client_source: Option<String>,
    pub credential_store: Option<String>,
    pub problem: Option<String>,
    pub setup_hint: Option<String>,
}

pub fn space_from(s: &str) -> DriveSpace {
    if s == "app_data" { DriveSpace::AppData } else { DriveSpace::VisibleFolder }
}

/// Google's downloaded client JSON: the configured path, else `<config>/google-oauth.json` if present.
pub fn client_config(
    settings: &Settings,
    paths: &AppPaths,
) -> std::result::Result<OAuthClientConfig, ClientConfigError> {
    let explicit =
        settings.google_client_file.clone().or_else(|| Some(paths.google_client_file()).filter(|p| p.is_file()));
    OAuthClientConfig::discover(explicit.as_deref())
}

pub fn client_error_to_error(e: ClientConfigError) -> Error {
    Error::Config(e.to_string())
}

pub struct Provider {
    pub storage: Option<Arc<dyn Storage>>,
    pub auth: Option<Arc<Authenticator>>,
}

/// Build the storage described by `settings`. Never touches the network; Google calls fetch tokens
/// lazily and fail with an authentication error until the user has signed in.
pub fn build(settings: &Settings, paths: &AppPaths, db: &Arc<Db>, creds: Arc<dyn CredentialStore>) -> Result<Provider> {
    match &settings.storage {
        StorageSetting::None => Ok(Provider { storage: None, auth: None }),
        StorageSetting::Local { path } => {
            let st = LocalDirStorage::new(path)
                .map_err(|e| Error::io(format!("opening the storage folder {}", path.display()), e))?;
            Ok(Provider { storage: Some(Arc::new(st)), auth: None })
        }
        StorageSetting::Google { space, account } => {
            let client = client_config(settings, paths).map_err(client_error_to_error)?;
            let space = space_from(space);
            let auth = Arc::new(Authenticator::new(client, space, AuthConfig::default(), creds));
            let cfg = DriveConfig::new(account.clone().unwrap_or_default(), space);
            let state: Arc<dyn TransferStateStore> = db.clone();
            let drive = DriveStorage::new(cfg, auth.clone(), state).map_err(Error::from)?;
            Ok(Provider { storage: Some(Arc::new(drive)), auth: Some(auth) })
        }
    }
}

pub fn default_credentials() -> Arc<dyn CredentialStore> {
    Arc::new(KeyringCredentialStore::new())
}

pub fn status(settings: &Settings, paths: &AppPaths, auth: Option<&Authenticator>) -> StorageStatus {
    match &settings.storage {
        StorageSetting::None => StorageStatus {
            kind: "none".into(),
            detail: "Nothing is connected yet.".into(),
            setup_hint: Some(
                "Connect Google Drive, or choose a local folder to use as the cloud (for testing).".into(),
            ),
            ..Default::default()
        },
        StorageSetting::Local { path } => {
            let ok = path.is_dir();
            StorageStatus {
                kind: "local".into(),
                connected: ok,
                detail: format!("Local folder: {}", path.display()),
                problem: (!ok).then(|| format!("{} is not available (is the drive mounted?)", path.display())),
                ..Default::default()
            }
        }
        StorageSetting::Google { space, account } => {
            let client = client_config(settings, paths);
            let (configured, source) = match &client {
                Ok(c) => (true, Some(format!("{:?}", c.source))),
                Err(_) => (false, None),
            };
            let mut st = StorageStatus {
                kind: "google".into(),
                space: Some(space.clone()),
                account: account.clone(),
                oauth_client_configured: configured,
                oauth_client_source: source,
                detail: format!(
                    "Google Drive{} ({})",
                    account.as_ref().map(|a| format!(" — {a}")).unwrap_or_default(),
                    if space == "app_data" { "hidden app data" } else { "visible PackSync folder" }
                ),
                ..Default::default()
            };
            if let Err(e) = &client {
                st.problem = Some(e.to_string());
                st.setup_hint = Some(
                    "Create a Google OAuth client and point PackSync at its JSON file; see docs/oauth-setup.md.".into(),
                );
            }
            if let Some(a) = auth {
                match a.status() {
                    AuthStatus::Connected { has_required_scope, .. } => {
                        st.connected = has_required_scope;
                        if !has_required_scope {
                            st.needs_reconnect = true;
                            st.problem =
                                Some("The granted Google permission is too narrow; reconnect and allow access.".into());
                        }
                    }
                    AuthStatus::NotConnected => st.needs_reconnect = true,
                    AuthStatus::Revoked => {
                        st.needs_reconnect = true;
                        st.problem = Some("Google no longer accepts the saved sign-in. Reconnect your account; cloud data is unaffected.".into());
                    }
                    AuthStatus::CredentialStoreUnavailable(m) => st.problem = Some(m),
                }
            }
            st
        }
    }
}

pub fn login(auth: &Authenticator, opener: &dyn UrlOpener, ctx: &OpCtx) -> Result<AccountInfo> {
    auth.login(opener, &LoginOptions::default(), ctx).map_err(|e| Error::from(e.to_storage_error()))
}

/// Does `dir` look usable as a storage folder (exists or can be created, writable)?
pub fn check_local_folder(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    let probe = dir.join(format!(".packsync-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"x").map_err(|e| Error::io(format!("writing to {}", dir.display()), e))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}
