//! Cancellation and job-progress plumbing shared by every long-running operation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::ids::{InstanceId, JobId};

/// Cooperative cancellation flag. Cheap to clone; all clones share state.
#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Coarse phases of the sync state machine (see `docs/state-machine.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Discovery,
    Scan,
    Snapshot,
    Upload,
    Publish,
    Reconcile,
    StageDownload,
    Validate,
    Apply,
    Recover,
    Verify,
    Cleanup,
    Conflict,
    Launch,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// Entered a phase.
    Phase,
    /// Numeric progress within the current phase.
    Progress,
    /// Human-readable note (also used for retry/backoff notices).
    Message { level: Level },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub job: Option<JobId>,
    pub instance: Option<InstanceId>,
    pub phase: Phase,
    pub kind: EventKind,
    pub message: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub items_done: u64,
    pub items_total: u64,
}

pub trait ProgressSink: Send + Sync {
    fn emit(&self, ev: ProgressEvent);
}

/// Discards everything.
pub struct NullSink;
impl ProgressSink for NullSink {
    fn emit(&self, _ev: ProgressEvent) {}
}

/// Records events in memory (tests, CLI summaries).
#[derive(Default)]
pub struct CollectingSink {
    events: Mutex<Vec<ProgressEvent>>,
}
impl CollectingSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn events(&self) -> Vec<ProgressEvent> {
        self.events.lock().clone()
    }
    pub fn messages(&self) -> Vec<String> {
        self.events
            .lock()
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Message { .. }))
            .map(|e| e.message.clone())
            .collect()
    }
}
impl ProgressSink for CollectingSink {
    fn emit(&self, ev: ProgressEvent) {
        self.events.lock().push(ev);
    }
}

/// Byte counters for one operation context (e.g. a job). Used for transfer summaries.
#[derive(Default, Debug)]
pub struct Counters {
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
}

/// Everything a long-running operation needs besides its own arguments.
#[derive(Clone)]
pub struct OpCtx {
    pub cancel: CancelToken,
    pub sink: Arc<dyn ProgressSink>,
    pub job: Option<JobId>,
    pub instance: Option<InstanceId>,
    pub counters: Arc<Counters>,
}

impl OpCtx {
    pub fn new(sink: Arc<dyn ProgressSink>, cancel: CancelToken) -> Self {
        Self { cancel, sink, job: None, instance: None, counters: Arc::new(Counters::default()) }
    }
    /// A context that reports nowhere and is never cancelled (tests, internal helpers).
    pub fn detached() -> Self {
        Self::new(Arc::new(NullSink), CancelToken::new())
    }
    pub fn with_job(mut self, job: JobId) -> Self {
        self.job = Some(job);
        self
    }
    pub fn with_instance(mut self, id: InstanceId) -> Self {
        self.instance = Some(id);
        self
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
    /// `Err(Error::Cancelled)` if cancellation was requested.
    pub fn check(&self) -> Result<(), Error> {
        if self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }

    fn base(&self, phase: Phase, kind: EventKind, message: String) -> ProgressEvent {
        ProgressEvent {
            job: self.job,
            instance: self.instance,
            phase,
            kind,
            message,
            bytes_done: 0,
            bytes_total: 0,
            items_done: 0,
            items_total: 0,
        }
    }
    pub fn phase(&self, phase: Phase, message: impl Into<String>) {
        self.sink.emit(self.base(phase, EventKind::Phase, message.into()));
    }
    pub fn progress(&self, phase: Phase, items: (u64, u64), bytes: (u64, u64), message: impl Into<String>) {
        let mut ev = self.base(phase, EventKind::Progress, message.into());
        ev.items_done = items.0;
        ev.items_total = items.1;
        ev.bytes_done = bytes.0;
        ev.bytes_total = bytes.1;
        self.sink.emit(ev);
    }
    pub fn note(&self, phase: Phase, level: Level, message: impl Into<String>) {
        self.sink.emit(self.base(phase, EventKind::Message { level }, message.into()));
    }
    pub fn add_up(&self, n: u64) {
        self.counters.bytes_up.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_down(&self, n: u64) {
        self.counters.bytes_down.fetch_add(n, Ordering::Relaxed);
    }
}
