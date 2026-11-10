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

/// Transfer counters at one moment, as reported by the storage provider (HTTP requests and body bytes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferCounts {
    pub requests: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

/// One step of an operation, as `--explain` reports it: how long it took and what it sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceStep {
    pub step: String,
    pub ms: u64,
    pub requests: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

/// Records named steps with their duration and the requests made during each (for `sync --explain`).
/// A step lasts from its mark to the next mark (or [`Trace::finish`]).
pub struct Trace {
    counts: Box<dyn Fn() -> TransferCounts + Send + Sync>,
    marks: Mutex<Vec<(String, std::time::Instant, TransferCounts)>>,
}

impl Trace {
    /// `counts` reads the provider's cumulative counters (it is called at every mark).
    pub fn new(counts: impl Fn() -> TransferCounts + Send + Sync + 'static) -> Arc<Trace> {
        let t = Trace { counts: Box::new(counts), marks: Mutex::new(Vec::new()) };
        t.mark("start");
        Arc::new(t)
    }
    pub fn mark(&self, step: impl Into<String>) {
        let c = (self.counts)();
        self.marks.lock().push((step.into(), std::time::Instant::now(), c));
    }
    /// The steps so far, each with its duration and the requests and bytes it caused.
    pub fn finish(&self) -> Vec<TraceStep> {
        let end = ((self.counts)(), std::time::Instant::now());
        let marks = self.marks.lock();
        let mut out = Vec::with_capacity(marks.len());
        for (i, (step, at, c)) in marks.iter().enumerate() {
            let (next_at, next_c) = marks.get(i + 1).map(|(_, a, c)| (*a, *c)).unwrap_or((end.1, end.0));
            out.push(TraceStep {
                step: step.clone(),
                ms: next_at.duration_since(*at).as_millis() as u64,
                requests: next_c.requests.saturating_sub(c.requests),
                bytes_up: next_c.bytes_up.saturating_sub(c.bytes_up),
                bytes_down: next_c.bytes_down.saturating_sub(c.bytes_down),
            });
        }
        out
    }
}

/// Everything a long-running operation needs besides its own arguments.
#[derive(Clone)]
pub struct OpCtx {
    pub cancel: CancelToken,
    pub sink: Arc<dyn ProgressSink>,
    pub job: Option<JobId>,
    pub instance: Option<InstanceId>,
    pub counters: Arc<Counters>,
    /// Set by `--explain`: every phase change and [`OpCtx::mark`] is recorded with timings.
    pub trace: Option<Arc<Trace>>,
}

impl OpCtx {
    pub fn new(sink: Arc<dyn ProgressSink>, cancel: CancelToken) -> Self {
        Self { cancel, sink, job: None, instance: None, counters: Arc::new(Counters::default()), trace: None }
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
    pub fn with_trace(mut self, trace: Arc<Trace>) -> Self {
        self.trace = Some(trace);
        self
    }
    /// Start a named step of the trace (no-op without `--explain`). Phase changes are steps too.
    pub fn mark(&self, step: impl Into<String>) {
        if let Some(t) = &self.trace {
            t.mark(step);
        }
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
        let message = message.into();
        self.mark(message.clone());
        self.sink.emit(self.base(phase, EventKind::Phase, message));
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
