//! Events the application layer publishes to its front ends (desktop UI, dev bridge, CLI progress).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use packsync_core::error::Error;
use packsync_core::ids::{InstanceId, JobId};
use packsync_core::progress::{EventKind, Level, ProgressEvent, ProgressSink};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// An error in a form front ends can render and tests can match on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorView {
    /// Stable machine-readable code (`offline`, `conflict`, `crash_pending`, ...).
    pub code: String,
    /// What happened, in one or two sentences. Secrets are redacted.
    pub message: String,
    /// What the user can do about it.
    pub advice: Option<String>,
    /// The exit code the CLI uses for this error.
    pub exit_code: i32,
}

impl From<&Error> for ErrorView {
    fn from(e: &Error) -> Self {
        ErrorView {
            code: e.code().as_str().to_string(),
            message: packsync_core::redact::redact(&e.to_string()).into_owned(),
            advice: e.advice().map(str::to_string),
            exit_code: e.exit_code(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: JobId,
    /// The API method that runs in this job (`instance.sync`, `launch.start`, ...).
    pub method: String,
    pub instance: Option<InstanceId>,
    pub state: JobState,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    pub error: Option<ErrorView>,
    pub result: Option<serde_json::Value>,
    pub progress: Option<ProgressEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppEvent {
    Progress(ProgressEvent),
    Job(JobInfo),
    /// Something about this instance changed (status, history, state); refetch it.
    InstanceChanged {
        instance: InstanceId,
    },
    /// The storage connection or its settings changed.
    StorageChanged,
    Notice {
        level: String,
        message: String,
    },
}

/// Fan-out to any number of subscribers. A subscriber that disappeared is dropped on the next send;
/// nothing ever blocks on a slow consumer (channels are unbounded, progress is throttled upstream).
#[derive(Default, Clone)]
pub struct EventBus {
    subs: Arc<Mutex<Vec<Sender<AppEvent>>>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn subscribe(&self) -> Receiver<AppEvent> {
        let (tx, rx) = unbounded();
        self.subs.lock().push(tx);
        rx
    }
    pub fn emit(&self, ev: AppEvent) {
        self.subs.lock().retain(|s| s.send(ev.clone()).is_ok());
    }
    pub fn notice(&self, level: Level, message: impl Into<String>) {
        let level = match level {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        };
        self.emit(AppEvent::Notice { level: level.into(), message: message.into() });
    }
}

/// Progress sink that publishes to the bus. Plain `Progress` ticks are throttled (a scan can produce
/// thousands per second); phase changes and messages always pass, and the last tick of a burst is
/// delivered by the next event.
pub struct BusSink {
    bus: EventBus,
    last: Mutex<Instant>,
    min_gap: Duration,
}

impl BusSink {
    pub fn new(bus: EventBus) -> Arc<Self> {
        Arc::new(Self {
            bus,
            last: Mutex::new(Instant::now() - Duration::from_secs(1)),
            min_gap: Duration::from_millis(100),
        })
    }
}

impl ProgressSink for BusSink {
    fn emit(&self, ev: ProgressEvent) {
        if matches!(ev.kind, EventKind::Progress) {
            let mut last = self.last.lock();
            let done = ev.items_total > 0 && ev.items_done >= ev.items_total
                || ev.bytes_total > 0 && ev.bytes_done >= ev.bytes_total;
            if last.elapsed() < self.min_gap && !done {
                return;
            }
            *last = Instant::now();
        }
        self.bus.emit(AppEvent::Progress(ev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use packsync_core::progress::Phase;

    fn tick(done: u64) -> ProgressEvent {
        ProgressEvent {
            job: None,
            instance: None,
            phase: Phase::Scan,
            kind: EventKind::Progress,
            message: String::new(),
            bytes_done: 0,
            bytes_total: 0,
            items_done: done,
            items_total: 1000,
        }
    }

    #[test]
    fn bus_fans_out_drops_dead_subscribers_and_throttles_ticks() {
        let bus = EventBus::new();
        let a = bus.subscribe();
        let b = bus.subscribe();
        drop(b);
        bus.emit(AppEvent::StorageChanged);
        assert!(matches!(a.try_recv().unwrap(), AppEvent::StorageChanged));
        assert_eq!(bus.subs.lock().len(), 1, "the dead subscriber was pruned");

        let sink = BusSink::new(bus.clone());
        for i in 0..500 {
            sink.emit(tick(i));
        }
        sink.emit(tick(1000)); // completion always passes
        let got: Vec<_> = a.try_iter().collect();
        assert!(got.len() < 20, "throttled: {}", got.len());
        assert!(matches!(got.last().unwrap(), AppEvent::Progress(p) if p.items_done == 1000));
    }

    #[test]
    fn error_view_redacts_and_carries_advice() {
        let e = Error::Config("refresh_token=1//0abcdef failed".into());
        let v = ErrorView::from(&e);
        assert!(!v.message.contains("1//0abcdef"), "{}", v.message);
        let c = ErrorView::from(&Error::Conflict("x".into()));
        assert_eq!((c.code.as_str(), c.exit_code), ("conflict", packsync_core::error::exit::CONFLICT));
        assert!(c.advice.is_some());
    }
}
