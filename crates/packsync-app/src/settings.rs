//! User settings (`settings.json`). Versioned; unknown fields are ignored so older builds can read
//! files written by newer ones; invalid values are rejected with a message instead of guessed.

use std::path::{Path, PathBuf};

use packsync_core::error::{Error, Result};
use serde::{Deserialize, Serialize};

pub const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StorageSetting {
    /// Nothing connected yet.
    #[default]
    None,
    /// A local folder acting as the cloud. For testing and offline demos only.
    Local { path: PathBuf },
    Google {
        /// `visible_folder` (scope `drive.file`, default) or `app_data` (scope `drive.appdata`).
        #[serde(default = "default_space")]
        space: String,
        /// The account that signed in (shown in the UI, part of the storage identity). Not a secret.
        #[serde(default)]
        account: Option<String>,
    },
    /// Cloudflare R2 (S3 API). The secret access key is kept in the OS credential store under
    /// [`StorageSetting::r2_secret_account`]; nothing here is secret.
    R2 {
        account_id: String,
        bucket: String,
        /// Every object is stored under `<prefix>/`.
        #[serde(default = "default_r2_prefix")]
        prefix: String,
        access_key_id: String,
    },
    /// The PackSync service: nothing to set up, end-to-end encrypted. The account's token and
    /// encryption key are kept in the OS credential store under [`StorageSetting::hosted_secret_account`].
    Hosted {
        /// The service's address (`https://…`).
        endpoint: String,
        /// The account ID (not a secret).
        account: String,
    },
}

fn default_r2_prefix() -> String {
    "packsync".into()
}

impl StorageSetting {
    /// Credential-store entry of an R2 secret access key.
    pub fn r2_secret_account(account_id: &str, access_key_id: &str) -> String {
        format!("r2:{account_id}:{access_key_id}")
    }

    /// Credential-store entry of a hosted account's token and encryption key.
    pub fn hosted_secret_account(account: &str) -> String {
        format!("hosted:{account}")
    }
}

fn default_space() -> String {
    "visible_folder".into()
}

/// What a *launcher hook* does when the cloud cannot be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflinePolicy {
    /// Let the game start from the local copy; the base snapshot is recorded (default).
    PlayLocal,
    /// Refuse to start the game.
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub device_name: Option<String>,
    pub storage: StorageSetting,
    /// Path to Google's downloaded client JSON (default: `<config>/google-oauth.json`).
    pub google_client_file: Option<PathBuf>,
    pub prism_executable: Option<PathBuf>,
    pub prism_data_dirs: Vec<PathBuf>,
    /// The `packsync` command-line executable that launcher hooks call (default: found next to the app).
    pub cli_path: Option<PathBuf>,
    pub hook_offline_policy: OfflinePolicy,
    /// Hooks refuse to start the game on a conflict or an unresolved crash (default `true`).
    pub hook_block_on_problem: bool,
    pub upload_concurrency: usize,
    pub recovery_point_days: u32,
    /// How long another device's "playing now" note stays fresh after its last heartbeat.
    pub lease_ttl_secs: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            device_name: None,
            storage: StorageSetting::None,
            google_client_file: None,
            prism_executable: None,
            prism_data_dirs: vec![],
            cli_path: None,
            hook_offline_policy: OfflinePolicy::PlayLocal,
            hook_block_on_problem: true,
            upload_concurrency: 4,
            recovery_point_days: 14,
            lease_ttl_secs: 120,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Settings> {
        match std::fs::read(path) {
            Ok(b) => {
                let s: Settings = serde_json::from_slice(&b)
                    .map_err(|e| Error::Config(format!("{} is not valid settings JSON: {e}", path.display())))?;
                if s.version > SETTINGS_VERSION {
                    return Err(Error::Unsupported(format!(
                        "{} was written by a newer PackSync (settings version {})",
                        path.display(),
                        s.version
                    )));
                }
                s.validate()?;
                Ok(s)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(Error::io(format!("reading {}", path.display()), e)),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=32).contains(&self.upload_concurrency) {
            return Err(Error::Config("upload_concurrency must be between 1 and 32".into()));
        }
        if !(30..=3600).contains(&self.lease_ttl_secs) {
            return Err(Error::Config("lease_ttl_secs must be between 30 and 3600".into()));
        }
        if let StorageSetting::R2 { account_id, bucket, prefix, access_key_id } = &self.storage {
            let id_ok = |v: &str| !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_alphanumeric());
            if !id_ok(account_id) {
                return Err(Error::Config(
                    "the R2 account ID is 32 letters and digits (Cloudflare dashboard → R2)".into(),
                ));
            }
            if !id_ok(access_key_id) {
                return Err(Error::Config("the R2 access key ID is letters and digits".into()));
            }
            if bucket.is_empty() || !bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
                return Err(Error::Config(format!(
                    "{bucket:?} is not a valid bucket name (lowercase letters, digits, -)"
                )));
            }
            if prefix.is_empty()
                || !prefix.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            {
                return Err(Error::Config(format!("{prefix:?} is not a valid prefix (letters, digits, . _ -)")));
            }
        }
        if let StorageSetting::Hosted { endpoint, account } = &self.storage {
            if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
                return Err(Error::Config(format!("the service address {endpoint:?} must start with https://")));
            }
            if account.is_empty() || !account.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
                return Err(Error::Config(format!("{account:?} is not a PackSync account ID")));
            }
        }
        if let StorageSetting::Google { space, .. } = &self.storage
            && space != "visible_folder"
            && space != "app_data"
        {
            return Err(Error::Config(format!(
                "unknown Google Drive space {space:?} (use visible_folder or app_data)"
            )));
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        packsync_core::fsutil::write_atomic(path, &serde_json::to_vec_pretty(self)?)
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_defaults_and_unknown_fields() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s/settings.json");
        assert_eq!(Settings::load(&p).unwrap().storage, StorageSetting::None);
        let s = Settings { storage: StorageSetting::Local { path: "/tmp/cloud".into() }, ..Settings::default() };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p).unwrap().storage, s.storage);
        std::fs::write(&p, r#"{"version":1,"future_field":42,"hook_block_on_problem":false}"#).unwrap();
        assert!(!Settings::load(&p).unwrap().hook_block_on_problem);
        std::fs::write(&p, r#"{"version":9}"#).unwrap();
        assert!(matches!(Settings::load(&p), Err(Error::Unsupported(_))));
        std::fs::write(&p, r#"{"upload_concurrency":0}"#).unwrap();
        assert!(Settings::load(&p).is_err());
        std::fs::write(&p, "not json").unwrap();
        assert!(Settings::load(&p).is_err());
    }
}
