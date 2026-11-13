//! Error taxonomy. Every error maps to a stable machine-readable [`ErrorCode`], a CLI exit code and
//! an actionable, user-facing hint.

use std::fmt;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::storage::{AuthProblem, StorageError};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("local database error: {0}")]
    Db(String),
    /// Untrusted or malformed input (manifests, indexes, packs, paths, rules).
    #[error("{what} is invalid: {detail}")]
    Invalid { what: &'static str, detail: String },
    /// Remote data is missing, truncated or does not verify.
    #[error("remote data is corrupt or incomplete: {0}")]
    Corrupt(String),
    #[error("operation cancelled")]
    Cancelled,
    /// A snapshot refers to data that is not in the cloud (any more): publishing it would leave a version
    /// that other computers cannot restore. Nothing was published.
    #[error("{0}")]
    CloudDataMissing(String),
    /// Another job holds the instance lock / an equivalent job is already running.
    #[error("busy: {0}")]
    Busy(String),
    #[error("Minecraft (or another writer) is using this instance: {0}")]
    GameRunning(String),
    /// Divergent histories that need a human decision.
    #[error("{0}")]
    Conflict(String),
    #[error("not enough free disk space on {path}: need {needed} bytes, {available} available")]
    NoSpace { path: PathBuf, needed: u64, available: u64 },
    #[error("the instance location is unavailable: {0}")]
    InstanceUnavailable(String),
    /// A guard refused a risky-looking operation (mass deletion, ...); needs explicit confirmation.
    #[error("safety guard: {0}")]
    SafetyGuard(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("configuration problem: {0}")]
    Config(String),
    /// A previous session ended abnormally and its result has not been resolved yet.
    #[error("{0}")]
    CrashPending(String),
    /// Deterministic crash injection (tests only).
    #[error("injected crash at failpoint '{0}'")]
    InjectedCrash(String),
    #[error("{0}")]
    Other(String),
}

/// Stable machine-readable error identifiers (CLI `--json`, UI, hooks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Io,
    Offline,
    AuthRequired,
    AuthRevoked,
    AuthScope,
    QuotaExceeded,
    RateLimited,
    StorageUnavailable,
    StorageNotFound,
    StorageConflict,
    StoragePermission,
    Database,
    Invalid,
    CorruptRemote,
    CloudDataMissing,
    Cancelled,
    Busy,
    GameRunning,
    Conflict,
    NoSpace,
    InstanceUnavailable,
    SafetyGuard,
    NotFound,
    Unsupported,
    Config,
    CrashPending,
    Internal,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        // serde_json would also work, but a static match avoids allocation and typos.
        match self {
            ErrorCode::Io => "io",
            ErrorCode::Offline => "offline",
            ErrorCode::AuthRequired => "auth_required",
            ErrorCode::AuthRevoked => "auth_revoked",
            ErrorCode::AuthScope => "auth_scope",
            ErrorCode::QuotaExceeded => "quota_exceeded",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::StorageUnavailable => "storage_unavailable",
            ErrorCode::StorageNotFound => "storage_not_found",
            ErrorCode::StorageConflict => "storage_conflict",
            ErrorCode::StoragePermission => "storage_permission",
            ErrorCode::Database => "database",
            ErrorCode::Invalid => "invalid",
            ErrorCode::CorruptRemote => "corrupt_remote",
            ErrorCode::CloudDataMissing => "cloud_data_missing",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Busy => "busy",
            ErrorCode::GameRunning => "game_running",
            ErrorCode::Conflict => "conflict",
            ErrorCode::NoSpace => "no_space",
            ErrorCode::InstanceUnavailable => "instance_unavailable",
            ErrorCode::SafetyGuard => "safety_guard",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Config => "config",
            ErrorCode::CrashPending => "crash_pending",
            ErrorCode::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// CLI exit codes.
pub mod exit {
    pub const OK: i32 = 0;
    pub const FAILURE: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const NOT_FOUND: i32 = 3;
    pub const OFFLINE: i32 = 10;
    pub const AUTH: i32 = 11;
    pub const CONFLICT: i32 = 12;
    pub const BUSY: i32 = 13;
    pub const GAME_RUNNING: i32 = 14;
    /// Local snapshot saved but the cloud copy is not published yet.
    pub const UPLOAD_PENDING: i32 = 15;
    pub const CORRUPT: i32 = 16;
    pub const NO_SPACE: i32 = 17;
    pub const CANCELLED: i32 = 18;
    pub const SAFETY_GUARD: i32 = 19;
    pub const QUOTA: i32 = 20;
    pub const LAUNCH_FAILED: i32 = 30;
}

impl Error {
    pub fn invalid(what: &'static str, detail: impl Into<String>) -> Self {
        Error::Invalid { what, detail: detail.into() }
    }
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Error::Io { context: context.into(), source }
    }
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    /// The operation stopped because the person cancelled it. A cancel can surface from the engine
    /// (`Error::Cancelled`) or from inside a storage call (`StorageError::Cancelled`); both mean the same.
    pub fn is_cancelled(&self) -> bool {
        self.code() == ErrorCode::Cancelled
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            Error::Io { .. } => ErrorCode::Io,
            Error::Storage(s) => match s {
                StorageError::Offline(_) => ErrorCode::Offline,
                StorageError::Auth(AuthProblem::Revoked) => ErrorCode::AuthRevoked,
                StorageError::Auth(AuthProblem::InsufficientScope) => ErrorCode::AuthScope,
                StorageError::Auth(_) => ErrorCode::AuthRequired,
                StorageError::QuotaExceeded(_) => ErrorCode::QuotaExceeded,
                StorageError::RateLimited { .. } => ErrorCode::RateLimited,
                StorageError::Transient(_) => ErrorCode::StorageUnavailable,
                StorageError::NotFound(_) => ErrorCode::StorageNotFound,
                StorageError::Conflict(_) => ErrorCode::StorageConflict,
                StorageError::Corrupt(_) => ErrorCode::CorruptRemote,
                StorageError::Permission(_) => ErrorCode::StoragePermission,
                StorageError::Cancelled => ErrorCode::Cancelled,
                StorageError::Io(_) | StorageError::Other(_) => ErrorCode::StorageUnavailable,
            },
            Error::Db(_) => ErrorCode::Database,
            Error::Invalid { .. } => ErrorCode::Invalid,
            Error::Corrupt(_) => ErrorCode::CorruptRemote,
            Error::Cancelled => ErrorCode::Cancelled,
            Error::CloudDataMissing(_) => ErrorCode::CloudDataMissing,
            Error::Busy(_) => ErrorCode::Busy,
            Error::GameRunning(_) => ErrorCode::GameRunning,
            Error::Conflict(_) => ErrorCode::Conflict,
            Error::NoSpace { .. } => ErrorCode::NoSpace,
            Error::InstanceUnavailable(_) => ErrorCode::InstanceUnavailable,
            Error::SafetyGuard(_) => ErrorCode::SafetyGuard,
            Error::NotFound(_) => ErrorCode::NotFound,
            Error::Unsupported(_) => ErrorCode::Unsupported,
            Error::Config(_) => ErrorCode::Config,
            Error::CrashPending(_) => ErrorCode::CrashPending,
            Error::InjectedCrash(_) | Error::Other(_) => ErrorCode::Internal,
        }
    }

    pub fn exit_code(&self) -> i32 {
        use ErrorCode::*;
        match self.code() {
            Offline | StorageUnavailable | RateLimited => exit::OFFLINE,
            AuthRequired | AuthRevoked | AuthScope | StoragePermission => exit::AUTH,
            QuotaExceeded => exit::QUOTA,
            Conflict => exit::CONFLICT,
            Busy => exit::BUSY,
            GameRunning => exit::GAME_RUNNING,
            CorruptRemote | CloudDataMissing => exit::CORRUPT,
            NoSpace => exit::NO_SPACE,
            Cancelled => exit::CANCELLED,
            SafetyGuard | InstanceUnavailable | CrashPending => exit::SAFETY_GUARD,
            NotFound | StorageNotFound => exit::NOT_FOUND,
            _ => exit::FAILURE,
        }
    }

    /// An actionable hint for the user, or `None` when the message already says what to do.
    pub fn advice(&self) -> Option<&'static str> {
        use ErrorCode::*;
        Some(match self.code() {
            Offline => {
                "Check your internet connection. Your local copy is safe; PackSync will retry and nothing was marked as synced."
            }
            AuthRequired => "Connect your Google account again (Settings → Storage → Connect), then retry.",
            AuthRevoked => "Google access was revoked or expired. Reconnect your account; cloud data was not touched.",
            AuthScope => {
                "The granted Google permission is too narrow for this operation. Disconnect and reconnect the account."
            }
            QuotaExceeded => {
                "Your Google Drive is full. Free up space or buy more storage; local data is unaffected and the upload will resume."
            }
            RateLimited => {
                "Google is rate-limiting requests. PackSync backs off automatically; try again in a few minutes."
            }
            StorageUnavailable => {
                "The storage service had a temporary problem. The operation will be retried; local data is unaffected."
            }
            CorruptRemote => {
                "Some cloud data is missing or damaged. Your instance was not modified. Run `packsync verify --remote` to see what is affected."
            }
            CloudDataMissing => {
                "Data this version builds on is no longer in the cloud (a cleanup on another computer, or files removed from Drive). \
                 Nothing was published. PackSync re-uploads it from this computer's files when they still match; \
                 `packsync verify <instance> --remote --repair` does the same on request and says what could not be repaired."
            }
            GameRunning => "Close Minecraft (and any tool using the instance), then try again.",
            Busy => "Another PackSync operation is already working on this instance. Wait for it to finish.",
            Conflict => {
                "Two devices changed this instance independently. Both versions are preserved: open the conflict screen and choose which to continue."
            }
            NoSpace => "Free disk space or choose another location; the instance was not modified.",
            InstanceUnavailable => {
                "The instance folder is missing or on an unmounted drive. Reconnect the drive; nothing was deleted from the cloud."
            }
            SafetyGuard => {
                "This looks risky (for example many files vanished). Verify the folder is intact, then confirm explicitly if the change is intended."
            }
            CrashPending => {
                "The last session ended abnormally. Review it in the app: promote the local state or restore the last good cloud snapshot."
            }
            Cancelled => return None,
            _ => return None,
        })
    }
}

/// Attach context to `io::Result`s.
pub trait IoContext<T> {
    fn io_ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}
impl<T> IoContext<T> for io::Result<T> {
    fn io_ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| Error::Io { context: context(), source: e })
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Invalid { what: "JSON document", detail: e.to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_exit_codes_are_stable() {
        let e: Error = StorageError::Auth(AuthProblem::Revoked).into();
        assert_eq!(e.code().as_str(), "auth_revoked");
        assert_eq!(e.exit_code(), exit::AUTH);
        let e: Error = StorageError::QuotaExceeded("full".into()).into();
        assert_eq!(e.exit_code(), exit::QUOTA);
        assert!(e.advice().unwrap().contains("full"));
        assert_eq!(Error::Cancelled.exit_code(), exit::CANCELLED);
        assert!(Error::Cancelled.is_cancelled());
        assert!(Error::from(StorageError::Cancelled).is_cancelled(), "a cancel from inside a storage call");
        assert!(!Error::from(StorageError::Transient("x".into())).is_cancelled());
        assert_eq!(Error::Conflict("x".into()).exit_code(), exit::CONFLICT);
    }
}
