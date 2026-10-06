//! Where PackSync keeps its files.

use std::path::{Path, PathBuf};

use packsync_core::brand::{STATE_DIR_NAME, env_var};

#[derive(Debug, Clone)]
pub struct AppPaths {
    /// `settings.json`, `google-oauth.json` (user-editable, small).
    pub config_dir: PathBuf,
    /// Database, staging, cached manifests, locks.
    pub state_dir: PathBuf,
    /// Rotating diagnostic logs (secrets redacted).
    pub logs_dir: PathBuf,
}

impl AppPaths {
    /// Platform defaults, overridable with `PACKSYNC_CONFIG_DIR` / `PACKSYNC_STATE_DIR`.
    pub fn discover() -> AppPaths {
        let dirs = directories::ProjectDirs::from("", "", STATE_DIR_NAME);
        let config = std::env::var_os(env_var("CONFIG_DIR"))
            .map(PathBuf::from)
            .or_else(|| dirs.as_ref().map(|d| d.config_dir().to_path_buf()));
        let state = std::env::var_os(env_var("STATE_DIR"))
            .map(PathBuf::from)
            .or_else(|| dirs.as_ref().map(|d| d.state_dir().unwrap_or_else(|| d.data_local_dir()).to_path_buf()));
        let fallback = std::env::temp_dir().join(STATE_DIR_NAME);
        let config_dir = config.unwrap_or_else(|| fallback.join("config"));
        let state_dir = state.unwrap_or_else(|| fallback.join("state"));
        AppPaths { logs_dir: state_dir.join("logs"), config_dir, state_dir }
    }

    /// Everything below one root directory (tests, portable setups).
    pub fn under(root: &Path) -> AppPaths {
        AppPaths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            logs_dir: root.join("state").join("logs"),
        }
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }
    pub fn google_client_file(&self) -> PathBuf {
        self.config_dir.join("google-oauth.json")
    }
}
