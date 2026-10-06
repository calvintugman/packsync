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
    /// The platform's standard locations, ignoring any environment overrides.
    pub fn platform_defaults() -> AppPaths {
        let dirs = directories::ProjectDirs::from("", "", STATE_DIR_NAME);
        let fallback = std::env::temp_dir().join(STATE_DIR_NAME);
        let config_dir = dirs.as_ref().map(|d| d.config_dir().to_path_buf()).unwrap_or_else(|| fallback.join("config"));
        let state_dir = dirs
            .as_ref()
            .map(|d| d.state_dir().unwrap_or_else(|| d.data_local_dir()).to_path_buf())
            .unwrap_or_else(|| fallback.join("state"));
        AppPaths { logs_dir: state_dir.join("logs"), config_dir, state_dir }
    }

    /// Platform defaults, overridable with `PACKSYNC_CONFIG_DIR` / `PACKSYNC_STATE_DIR`.
    pub fn discover() -> AppPaths {
        let mut p = Self::platform_defaults();
        if let Some(c) = std::env::var_os(env_var("CONFIG_DIR")) {
            p.config_dir = PathBuf::from(c);
        }
        if let Some(s) = std::env::var_os(env_var("STATE_DIR")) {
            p.state_dir = PathBuf::from(s);
            p.logs_dir = p.state_dir.join("logs");
        }
        p
    }

    /// Arguments that make another PackSync process (a launcher hook) use *these* directories, or
    /// nothing when they are the platform defaults that such a process would find on its own.
    pub fn hook_args(&self) -> Vec<String> {
        let d = Self::platform_defaults();
        let mut a = vec![];
        if self.config_dir != d.config_dir {
            a.push("--config-dir".to_string());
            a.push(self.config_dir.to_string_lossy().into_owned());
        }
        if self.state_dir != d.state_dir {
            a.push("--state-dir".to_string());
            a.push(self.state_dir.to_string_lossy().into_owned());
        }
        a
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
