//! Google Drive storage adapter for PackSync.
//!
//! * [`DriveStorage`] implements [`packsync_core::storage::Storage`] on the Drive API v3 with
//!   blocking `ureq` + rustls (no async runtime, no OpenSSL; proxy settings come from the usual
//!   environment variables).
//! * [`oauth`] implements Google's installed-application OAuth flow (system browser, loopback
//!   redirect, PKCE S256) and the thread-safe [`oauth::Authenticator`].
//! * [`KeyringCredentialStore`] keeps the refresh token in the OS credential store; there is no
//!   plaintext fallback.
//! * `mock` (feature `mock`) is an in-memory Drive/OAuth server with fault injection for tests and
//!   benchmarks.
//!
//! Design and verified facts: `docs/dev/drive-adapter.md` and `docs/dev/google-drive-verification.md`.
//!
//! ```no_run
//! use std::sync::Arc;
//! use packsync_core::credentials::CredentialStore;
//! use packsync_core::progress::OpCtx;
//! use packsync_core::storage::{MemoryTransferState, Storage};
//! use packsync_drive::oauth::{AuthConfig, Authenticator, LoginOptions, SystemBrowser};
//! use packsync_drive::{DriveConfig, DriveSpace, DriveStorage, KeyringCredentialStore, OAuthClientConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let ctx = OpCtx::detached();
//! let client = OAuthClientConfig::discover(None)?;
//! let creds: Arc<dyn CredentialStore> = Arc::new(KeyringCredentialStore::new());
//! let auth = Arc::new(Authenticator::new(client, DriveSpace::VisibleFolder, AuthConfig::default(), creds));
//! let account = auth.login(&SystemBrowser { print_fallback: true }, &LoginOptions::default(), &ctx)?;
//! let cfg = DriveConfig::new(account.email.unwrap_or_default(), DriveSpace::VisibleFolder);
//! let drive = DriveStorage::new(cfg, auth, Arc::new(MemoryTransferState::default()))?;
//! println!("{:?}", drive.quota(&ctx)?);
//! # Ok(()) }
//! ```

pub mod config;
mod credentials;
mod http;
#[cfg(feature = "mock")]
pub mod mock;
mod model;
pub mod oauth;
mod stats;
mod storage;
mod util;

pub use config::{
    ClientConfigError, ClientConfigSource, DriveConfig, DriveEndpoints, DriveSpace, HttpTimeouts, OAuthClientConfig,
    OAuthEndpoints,
};
pub use credentials::KeyringCredentialStore;
pub use stats::{RequestKind, RequestStats, StatsHandle};
pub use storage::{CreatedObject, DriveStorage};
