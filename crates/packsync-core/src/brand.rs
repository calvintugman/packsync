//! Branding constants. Change these (and `apps/desktop/src-tauri/tauri.conf.json`,
//! `apps/desktop/src/brand.ts`) to rebrand the product. See `docs/architecture.md`.
//!
//! Remote *format* identifiers (see [`FORMAT_NAMESPACE`]) are protocol names, not branding:
//! they must NOT change when rebranding, or existing cloud data stops being recognised.

/// Human-facing product name (window titles, README, Drive folder default).
pub const PRODUCT_NAME: &str = "PackSync";
/// Executable name of the command-line interface.
pub const CLI_NAME: &str = "packsync";
/// Reverse-DNS application identifier (placeholder: this identifier is not registered by anyone).
pub const APP_ID: &str = "org.example.packsync";
/// Service name under which credentials are stored in the OS credential store.
pub const KEYRING_SERVICE: &str = "packsync";
/// Name of the application-created root folder in the user's Google Drive.
pub const DRIVE_ROOT_FOLDER: &str = "PackSync";
/// Directory name used under the platform config/data/state directories.
pub const STATE_DIR_NAME: &str = "packsync";
/// Directory created inside every synced instance for local-only bookkeeping (never synced).
pub const LOCAL_MARKER_DIR: &str = ".packsync";
/// Prefix of environment variables read by the CLI/app (`PACKSYNC_STATE_DIR`, ...).
pub const ENV_PREFIX: &str = "PACKSYNC";
/// Marker embedded in launcher hook command lines so they can be recognised and removed safely.
pub const HOOK_MARKER: &str = "packsync-hook";

/// Namespace used in remote file formats (`packsync.snapshot`, ...). Protocol constant.
pub const FORMAT_NAMESPACE: &str = "packsync";

/// Environment variable name helper, e.g. `env_var("STATE_DIR")` → `PACKSYNC_STATE_DIR`.
pub fn env_var(suffix: &str) -> String {
    format!("{ENV_PREFIX}_{suffix}")
}
