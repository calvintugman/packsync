//! [`KeyringCredentialStore`]: the OS credential store via the `keyring` crate (v4, `v1` mode):
//! Secret Service (pure-Rust zbus client) on Linux/BSD, Keychain on macOS, Credential Manager on
//! Windows. When no secure store is usable the operations fail with
//! [`CredentialError::Unavailable`] and an actionable message; there is **no** plaintext or file
//! fallback.

use packsync_core::brand;
use packsync_core::credentials::{CredentialError, CredentialStore};
use packsync_core::redact::Secret;

/// OS credential store for the OAuth refresh token.
#[derive(Debug, Clone)]
pub struct KeyringCredentialStore {
    service: String,
}

impl Default for KeyringCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

/// What to do when the store cannot be used, per platform.
fn platform_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "PackSync keeps its Google sign-in in the macOS Keychain: unlock the login keychain and allow \
         PackSync to access it, then connect again."
    } else if cfg!(target_os = "windows") {
        "PackSync keeps its Google sign-in in the Windows Credential Manager: make sure it is available \
         for your user account, then connect again."
    } else if cfg!(unix) {
        "PackSync keeps its Google sign-in in the Secret Service: install or start a provider (GNOME \
         Keyring, KDE Wallet 5.97+, or KeePassXC with Secret Service integration), unlock it, then \
         connect again. PackSync never stores tokens in plain files."
    } else {
        "This platform has no supported secure credential store; PackSync never stores tokens in plain files."
    }
}

fn map_err(e: keyring::Error) -> CredentialError {
    use keyring::Error as K;
    let hint = platform_hint();
    match e {
        K::NoStorageAccess(err) => {
            CredentialError::Unavailable(format!("the credential store is locked or access was denied ({err}). {hint}"))
        }
        K::PlatformFailure(err) => CredentialError::Unavailable(format!("the credential store failed ({err}). {hint}")),
        K::NoDefaultStore => {
            let why = match keyring::Entry::store_status() {
                Err(e) => format!(" ({e})"),
                Ok(()) => String::new(),
            };
            CredentialError::Unavailable(format!("no secure credential store could be opened{why}. {hint}"))
        }
        K::NotSupportedByStore(s) => CredentialError::Unavailable(format!("{s}. {hint}")),
        // Never format the raw bytes these variants carry.
        K::BadEncoding(_) | K::BadDataFormat(..) => {
            CredentialError::Other("the stored credential is unreadable; disconnect and reconnect the account".into())
        }
        K::TooLong(name, max) => {
            CredentialError::Other(format!("the credential is too long for the store ({name} > {max})"))
        }
        K::Invalid(attr, reason) => CredentialError::Other(format!("invalid credential attribute {attr}: {reason}")),
        K::Ambiguous(v) => CredentialError::Other(format!(
            "{} matching credentials exist; remove the duplicates in the OS keychain",
            v.len()
        )),
        other => CredentialError::Other(format!("credential store error: {other}")),
    }
}

impl KeyringCredentialStore {
    /// Store under the service name [`brand::KEYRING_SERVICE`].
    pub fn new() -> Self {
        Self { service: brand::KEYRING_SERVICE.to_string() }
    }

    /// Store under a different service name (tests).
    pub fn with_service(service: impl Into<String>) -> Self {
        Self { service: service.into() }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, CredentialError> {
        keyring::Entry::new(&self.service, account).map_err(map_err)
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, account: &str) -> Result<Option<Secret>, CredentialError> {
        match self.entry(account)?.get_password() {
            Ok(p) => Ok(Some(Secret::new(p))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_err(e)),
        }
    }

    fn set(&self, account: &str, secret: &Secret) -> Result<(), CredentialError> {
        self.entry(account)?.set_password(secret.expose()).map_err(map_err)
    }

    fn delete(&self, account: &str) -> Result<(), CredentialError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    }

    fn describe(&self) -> String {
        if cfg!(target_os = "macos") {
            "OS keychain (macOS Keychain)".into()
        } else if cfg!(target_os = "windows") {
            "OS keychain (Windows Credential Manager)".into()
        } else if cfg!(unix) {
            "OS keychain (Secret Service)".into()
        } else {
            "unsupported platform".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Boom;
    impl std::fmt::Display for Boom {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("org.freedesktop.DBus.Error.ServiceUnknown")
        }
    }
    impl std::error::Error for Boom {}

    #[test]
    fn errors_map_to_actionable_unavailable() {
        for e in [keyring::Error::NoStorageAccess(Box::new(Boom)), keyring::Error::PlatformFailure(Box::new(Boom))] {
            match map_err(e) {
                CredentialError::Unavailable(m) => {
                    assert!(m.contains("ServiceUnknown"), "{m}");
                    assert!(m.contains("connect again"), "{m}");
                }
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }
        let leaked = map_err(keyring::Error::BadEncoding(b"1//0refresh-token".to_vec())).to_string();
        assert!(!leaked.contains("refresh-token"), "{leaked}");
        assert!(KeyringCredentialStore::new().describe().starts_with("OS keychain"));
    }

    /// Touches the real OS keychain. Run manually: `cargo test -p packsync-drive -- --ignored live_keyring`.
    #[test]
    #[ignore = "uses the real OS credential store"]
    fn live_keyring_roundtrip() {
        let store = KeyringCredentialStore::with_service("packsync-selftest");
        let account = format!("test:{}", crate::util::random_urlsafe(9).unwrap());
        assert!(store.get(&account).unwrap().is_none());
        store.set(&account, &Secret::new("value-1")).unwrap();
        assert_eq!(store.get(&account).unwrap().unwrap().expose(), "value-1");
        store.delete(&account).unwrap();
        assert!(store.get(&account).unwrap().is_none());
        store.delete(&account).unwrap();
    }
}
