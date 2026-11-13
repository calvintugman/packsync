//! PackSync core: the provider- and UI-independent sync engine.
//!
//! See `docs/architecture.md` for the module map and the invariants.

pub mod brand;
pub mod chunk;
pub mod codec;
pub mod credentials;
pub mod db;
pub mod engine;
pub mod error;
pub mod failpoint;
pub mod fsutil;
pub mod gc;
pub mod generic_launcher;
pub mod graph;
pub mod hash;
pub mod ids;
pub mod journal;
pub mod launcher;
pub mod lock;
pub mod manifest;
pub mod mixed;
pub mod pack;
pub mod paths;
pub mod proc;
pub mod progress;
pub mod redact;
pub mod remote;
pub mod restore;
pub mod retry;
pub mod rules;
pub mod scan;
pub mod session;
pub mod snapshot;
pub mod stage;
pub mod storage;
pub mod storage_fault;
pub mod storage_local;
pub mod storage_meter;
pub mod upload;
pub mod worlds;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use error::{Error, ErrorCode, Result};
pub use hash::Hash;
pub use ids::{DeviceId, InstanceId, JobId, SessionId, SnapshotId, TxId};
pub use progress::{CancelToken, OpCtx};
