//! PackSync core: the provider- and UI-independent sync engine.
//!
//! See `docs/architecture.md` for the module map and `docs/consistency.md` for the invariants.

pub mod brand;
pub mod credentials;
pub mod error;
pub mod fsutil;
pub mod hash;
pub mod ids;
pub mod launcher;
pub mod mixed;
pub mod paths;
pub mod progress;
pub mod redact;
pub mod retry;
pub mod rules;
pub mod storage;
pub mod storage_fault;
pub mod storage_local;
pub mod storage_meter;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use error::{Error, ErrorCode, Result};
pub use hash::Hash;
pub use ids::{DeviceId, InstanceId, JobId, SessionId, SnapshotId, TxId};
pub use progress::{CancelToken, OpCtx};
