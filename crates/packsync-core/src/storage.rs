//! The storage-provider contract.
//!
//! A [`Storage`] is a *logical key → bytes* object store with deliberately weak guarantees,
//! chosen so that Google Drive (which has no POSIX locks, no cross-file transactions, no unique
//! file names and no compare-and-swap in API v3) can implement it faithfully:
//!
//! * Keys are `/`-separated logical paths (`packs/<id>.pack`). On providers where names are not
//!   unique (Drive) several physical objects may carry one key; [`ObjectInfo::duplicates`] reports it.
//! * **`put` returns `Ok` only after the object is durably stored *and verified* according to the
//!   provider's [`Capabilities`]** (size always; server checksum where the provider reports one).
//! * `PutMode::CreateOnly` is *idempotent*: if the key already exists with identical content the
//!   existing object is returned (a retry after an ambiguous timeout must never fail or duplicate);
//!   if it exists with different content the call fails with [`StorageError::Conflict`].
//! * `PutMode::Replace` is only used for single-writer keys (per-device head/lease records);
//!   there is no conditional/CAS variant because the Drive API provides none.
//! * `list` may be eventually consistent ([`Capabilities::list_consistency`]); `stat`/`get` by key
//!   should be strongly consistent where the provider can offer it. The engine never relies on
//!   `list` alone to prove an object is *absent*.
//! * Implementations must be thread-safe, honour `ctx` cancellation promptly and report transfer
//!   progress through `ctx`.
//!
//! The conformance suite in `crate::testkit::conformance` encodes these rules; every provider
//! (local fake, fault-injecting wrapper, Google Drive) must pass it.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::hash::Hash;
use crate::progress::OpCtx;

pub type StorageResult<T> = Result<T, StorageError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "problem", content = "detail", rename_all = "snake_case")]
pub enum AuthProblem {
    /// No credentials are configured (the user has not connected an account yet).
    NotConnected,
    /// The access token expired and refreshing failed in a retryable way.
    Expired,
    /// The refresh token was revoked/expired (`invalid_grant`): the user must reconnect.
    Revoked,
    /// The granted OAuth scope does not allow the operation.
    InsufficientScope,
    Other(String),
}

impl std::fmt::Display for AuthProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthProblem::NotConnected => write!(f, "no account is connected"),
            AuthProblem::Expired => write!(f, "the access token expired"),
            AuthProblem::Revoked => write!(f, "access was revoked or the sign-in expired; reconnect the account"),
            AuthProblem::InsufficientScope => write!(f, "the granted permissions do not allow this operation"),
            AuthProblem::Other(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Error, Clone)]
pub enum StorageError {
    /// Network unreachable / DNS / timeouts that exhausted the provider's own retry budget.
    #[error("storage is unreachable: {0}")]
    Offline(String),
    #[error("authentication problem: {0}")]
    Auth(AuthProblem),
    /// The account is out of storage space. Not retryable.
    #[error("storage quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("rate limited by the storage provider")]
    RateLimited { retry_after: Option<Duration> },
    /// 5xx and similar. Retryable.
    #[error("temporary storage failure: {0}")]
    Transient(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// `CreateOnly` hit an existing object with different content.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Verification of stored/received bytes failed (size/checksum).
    #[error("integrity check failed: {0}")]
    Corrupt(String),
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("storage I/O error: {0}")]
    Io(String),
    #[error("{0}")]
    Other(String),
}

impl StorageError {
    /// Whether retrying the same operation later can reasonably succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            StorageError::Offline(_)
                | StorageError::RateLimited { .. }
                | StorageError::Transient(_)
                | StorageError::Auth(AuthProblem::Expired)
        )
    }
    pub fn io(e: std::io::Error) -> Self {
        StorageError::Io(e.to_string())
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::io(e)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksums {
    pub md5: Option<String>,
    pub sha256: Option<String>,
    pub blake3: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectInfo {
    /// Logical key. Directory entries end with `/`.
    pub key: String,
    pub size: u64,
    /// Provider-reported times, milliseconds since the Unix epoch.
    pub created_ms: Option<i64>,
    pub modified_ms: Option<i64>,
    pub checksums: Checksums,
    /// Provider's own identity for the object (Drive file ID). Persist this, not just the key.
    pub provider_ref: Option<String>,
    pub is_dir: bool,
    /// Number of *additional* physical objects sharing this key (Drive permits duplicate names).
    pub duplicates: u32,
    /// Provider-side tags that were written with the object (small, optional).
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

impl ObjectInfo {
    pub fn file(key: impl Into<String>, size: u64) -> Self {
        ObjectInfo {
            key: key.into(),
            size,
            created_ms: None,
            modified_ms: None,
            checksums: Checksums::default(),
            provider_ref: None,
            is_dir: false,
            duplicates: 0,
            tags: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    Strong,
    Eventual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumKind {
    Md5,
    Sha256,
    Blake3,
}

/// What the provider can and cannot do. The engine adapts verification and concurrency to it and
/// the docs state these facts explicitly (nothing here is assumed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub name: String,
    /// `false` when several objects can share one key (Drive).
    pub unique_keys: bool,
    /// Readers never observe a half-written replacement.
    pub atomic_replace: bool,
    /// Compare-and-swap writes available. `false` for Drive API v3 (no etag/If-Match).
    pub conditional_write: bool,
    pub range_reads: bool,
    pub resumable_uploads: bool,
    /// Checksums the *server* reports after an upload (used to verify without re-downloading).
    pub server_checksums: Vec<ChecksumKind>,
    pub list_consistency: Consistency,
    pub max_object_size: Option<u64>,
    pub recommended_concurrency: usize,
    /// Suggested pack size to keep request counts low.
    pub recommended_pack_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PutMode {
    /// Idempotent create (see module docs).
    CreateOnly,
    /// Replace the content of a single-writer key (or create it).
    Replace,
}

/// Content digest the caller already knows (so providers can verify without a second pass).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentDigest {
    pub size: u64,
    pub blake3: Hash,
}

#[derive(Debug, Clone)]
pub enum PutSource<'a> {
    Bytes(&'a [u8]),
    /// A local file whose content must not change during the call.
    File {
        path: &'a Path,
        len: u64,
    },
}

impl PutSource<'_> {
    pub fn len(&self) -> u64 {
        match self {
            PutSource::Bytes(b) => b.len() as u64,
            PutSource::File { len, .. } => *len,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone)]
pub struct PutOptions {
    pub mode: PutMode,
    /// Digest of the content as the caller computed it. If present, the provider must refuse to
    /// report success when the stored object does not match.
    pub digest: Option<ContentDigest>,
    /// Small provider-side tags (Drive `appProperties`): `kind`, `instance`, `parents`, ...
    pub tags: BTreeMap<String, String>,
}

impl PutOptions {
    pub fn create_only(digest: Option<ContentDigest>) -> Self {
        Self { mode: PutMode::CreateOnly, digest, tags: BTreeMap::new() }
    }
    pub fn replace() -> Self {
        Self { mode: PutMode::Replace, digest: None, tags: BTreeMap::new() }
    }
    pub fn with_tag(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.tags.insert(k.into(), v.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Move to the provider's trash when it has one (recoverable), else delete.
    Trash,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    pub used: u64,
    /// `None` = unlimited.
    pub total: Option<u64>,
}

pub trait Storage: Send + Sync {
    /// Stable identity of the remote (`local:<path>`, `gdrive:<account>:<space>`). Used to detect
    /// that a configured instance points at a different remote than before.
    fn provider_id(&self) -> String;
    fn capabilities(&self) -> Capabilities;

    /// Non-recursive listing of the "directory" `prefix` (must end with `/`). Files and
    /// sub-directories are returned (`is_dir`); a missing directory lists as empty.
    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>>;
    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>>;
    /// Streaming read of `[start, start+len)` of the object (`None` = whole object).
    fn get(&self, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>>;
    fn put(&self, key: &str, src: PutSource<'_>, opts: &PutOptions, ctx: &OpCtx) -> StorageResult<ObjectInfo>;
    /// Delete every physical object carrying `key`. Only the retention logic calls this.
    fn delete(&self, key: &str, mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()>;
    fn quota(&self, ctx: &OpCtx) -> StorageResult<Option<Quota>>;

    /// Convenience: read a whole (small) object into memory with an upper bound.
    fn get_bytes(&self, key: &str, max: u64, ctx: &OpCtx) -> StorageResult<Vec<u8>> {
        let mut r = self.get(key, None, ctx)?;
        let mut out = Vec::new();
        let mut limited = (&mut r).take(max + 1);
        limited.read_to_end(&mut out).map_err(StorageError::io)?;
        if out.len() as u64 > max {
            return Err(StorageError::Corrupt(format!("object {key} exceeds the {max} byte limit")));
        }
        Ok(out)
    }
}

/// Persisted state for resumable transfers (Drive upload session URIs, byte offsets). The engine's
/// SQLite database implements this so that uploads survive an application restart.
/// Values are opaque to the engine and **may contain secrets** (session URIs): never log them.
pub trait TransferStateStore: Send + Sync {
    fn load(&self, key: &str) -> Option<String>;
    fn save(&self, key: &str, state: &str);
    fn clear(&self, key: &str);
}

/// In-memory implementation (tests).
#[derive(Default)]
pub struct MemoryTransferState(parking_lot::Mutex<BTreeMap<String, String>>);
impl TransferStateStore for MemoryTransferState {
    fn load(&self, key: &str) -> Option<String> {
        self.0.lock().get(key).cloned()
    }
    fn save(&self, key: &str, state: &str) {
        self.0.lock().insert(key.to_string(), state.to_string());
    }
    fn clear(&self, key: &str) {
        self.0.lock().remove(key);
    }
}

/// Validates a logical key: `/`-separated, each component `[A-Za-z0-9._-]+`, no `.`/`..`,
/// at most 8 components. Providers call this before touching anything.
pub fn validate_key(key: &str) -> StorageResult<()> {
    if key.is_empty() || key.len() > 300 || key.starts_with('/') {
        return Err(StorageError::Other(format!("invalid storage key {key:?}")));
    }
    let trimmed = key.strip_suffix('/').unwrap_or(key);
    let mut n = 0;
    for comp in trimmed.split('/') {
        n += 1;
        let ok = !comp.is_empty()
            && comp != "."
            && comp != ".."
            && comp.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !ok || n > 8 {
            return Err(StorageError::Other(format!("invalid storage key {key:?}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation() {
        assert!(validate_key("packs/abc.pack").is_ok());
        assert!(validate_key("instances/0f0f/commits/").is_ok());
        for bad in ["", "/abs", "a//b", "a/../b", "a/./b", "a b", "ü", "a\\b", &"x".repeat(301)] {
            assert!(validate_key(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn retryability() {
        assert!(StorageError::Offline("x".into()).is_retryable());
        assert!(StorageError::RateLimited { retry_after: None }.is_retryable());
        assert!(!StorageError::QuotaExceeded("x".into()).is_retryable());
        assert!(!StorageError::Auth(AuthProblem::Revoked).is_retryable());
        assert!(!StorageError::Corrupt("x".into()).is_retryable());
    }
}
