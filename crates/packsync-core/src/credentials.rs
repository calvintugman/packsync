//! Credential storage contract. Secrets (OAuth refresh tokens) live in the OS credential store.
//! There is deliberately **no plaintext-file implementation** in this crate: if no secure store is
//! available the operation fails with an actionable message rather than silently degrading.

use std::collections::BTreeMap;

use parking_lot::Mutex;
use thiserror::Error;

use crate::redact::Secret;

#[derive(Debug, Error, Clone)]
pub enum CredentialError {
    /// No usable secure store (e.g. no Secret Service on Linux). The message says what to do.
    #[error("no secure credential store is available: {0}")]
    Unavailable(String),
    #[error("access to the credential store was denied: {0}")]
    Denied(String),
    #[error("credential store error: {0}")]
    Other(String),
}

pub trait CredentialStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<Secret>, CredentialError>;
    fn set(&self, account: &str, secret: &Secret) -> Result<(), CredentialError>;
    fn delete(&self, account: &str) -> Result<(), CredentialError>;
    /// Human-readable name for diagnostics (`"OS keychain (Secret Service)"`).
    fn describe(&self) -> String;
}

/// Process-memory store for tests. Never persisted.
#[derive(Default)]
pub struct MemoryCredentialStore(Mutex<BTreeMap<String, String>>);

impl CredentialStore for MemoryCredentialStore {
    fn get(&self, account: &str) -> Result<Option<Secret>, CredentialError> {
        Ok(self.0.lock().get(account).map(|s| Secret::new(s.clone())))
    }
    fn set(&self, account: &str, secret: &Secret) -> Result<(), CredentialError> {
        self.0.lock().insert(account.to_string(), secret.expose().to_string());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<(), CredentialError> {
        self.0.lock().remove(account);
        Ok(())
    }
    fn describe(&self) -> String {
        "in-memory (tests only)".into()
    }
}

/// A store that always fails: used to test "no secure store" behaviour end to end.
pub struct UnavailableCredentialStore(pub String);
impl CredentialStore for UnavailableCredentialStore {
    fn get(&self, _: &str) -> Result<Option<Secret>, CredentialError> {
        Err(CredentialError::Unavailable(self.0.clone()))
    }
    fn set(&self, _: &str, _: &Secret) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(self.0.clone()))
    }
    fn delete(&self, _: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(self.0.clone()))
    }
    fn describe(&self) -> String {
        "unavailable".into()
    }
}
