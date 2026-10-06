//! Background jobs for front ends that must not block (the desktop UI). The CLI runs the same
//! operations synchronously. Each job has a cancel token, publishes progress on the event bus and is
//! recorded in the local database so interrupted work is visible after a restart.

use std::collections::HashMap;
use std::sync::Arc;

use packsync_core::db::now_ms;
use packsync_core::error::{Error, Result};
use packsync_core::ids::{InstanceId, JobId};
use packsync_core::progress::{CancelToken, OpCtx};
use parking_lot::Mutex;

use crate::events::{AppEvent, BusSink, ErrorView, EventBus, JobInfo, JobState};

struct Entry {
    info: JobInfo,
    cancel: CancelToken,
}

#[derive(Clone)]
pub struct JobRunner {
    inner: Arc<Mutex<HashMap<JobId, Entry>>>,
    bus: EventBus,
    db: Arc<packsync_core::db::Db>,
}

impl JobRunner {
    pub fn new(bus: EventBus, db: Arc<packsync_core::db::Db>) -> Self {
        Self { inner: Default::default(), bus, db }
    }

    /// Start `work` on a new thread. If an identical job (same method, same instance) is already
    /// running its id is returned instead: a double click must not start two uploads.
    pub fn start<F>(&self, method: &str, instance: Option<InstanceId>, work: F) -> Result<JobId>
    where
        F: FnOnce(&OpCtx) -> Result<serde_json::Value> + Send + 'static,
    {
        let id = JobId::new();
        {
            let mut g = self.inner.lock();
            if let Some((existing, _)) = g.iter().find(|(_, e)| {
                e.info.state == JobState::Running && e.info.method == method && e.info.instance == instance
            }) {
                return Ok(*existing);
            }
            let info = JobInfo {
                id,
                method: method.to_string(),
                instance,
                state: JobState::Running,
                started_ms: now_ms(),
                finished_ms: None,
                error: None,
                result: None,
                progress: None,
            };
            g.insert(id, Entry { info: info.clone(), cancel: CancelToken::new() });
            self.bus.emit(AppEvent::Job(info));
        }
        let _ = self.db.job_register(id, method, instance, None);
        let cancel = self.inner.lock().get(&id).map(|e| e.cancel.clone()).unwrap_or_default();
        let me = self.clone();
        std::thread::Builder::new()
            .name(format!("job-{method}"))
            .spawn(move || {
                let mut ctx = OpCtx::new(BusSink::new(me.bus.clone()), cancel).with_job(id);
                if let Some(i) = instance {
                    ctx = ctx.with_instance(i);
                }
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&ctx))).unwrap_or_else(|_| {
                        Err(Error::other("the operation stopped unexpectedly (internal error); see the log"))
                    });
                me.finish(id, outcome);
            })
            .map_err(|e| Error::io("starting a background job", e))?;
        Ok(id)
    }

    fn finish(&self, id: JobId, outcome: Result<serde_json::Value>) {
        let (state, error, result) = match &outcome {
            Ok(v) => (JobState::Succeeded, None, Some(v.clone())),
            Err(e) if e.is_cancelled() => (JobState::Cancelled, None, None),
            Err(e) => (JobState::Failed, Some(ErrorView::from(e)), None),
        };
        let db_state = match state {
            JobState::Succeeded => "succeeded",
            JobState::Cancelled => "cancelled",
            _ => "failed",
        };
        let _ = self.db.job_finish(id, db_state, error.as_ref().map(|e| e.message.as_str()));
        let mut g = self.inner.lock();
        if let Some(e) = g.get_mut(&id) {
            e.info.state = state;
            e.info.finished_ms = Some(now_ms());
            e.info.error = error;
            e.info.result = result;
            self.bus.emit(AppEvent::Job(e.info.clone()));
        }
        // keep the most recent finished jobs for the UI, forget the rest
        let mut done: Vec<(i64, JobId)> = g
            .iter()
            .filter(|(_, e)| e.info.state != JobState::Running)
            .map(|(k, e)| (e.info.finished_ms.unwrap_or(0), *k))
            .collect();
        if done.len() > 50 {
            done.sort();
            for (_, k) in done.iter().take(done.len() - 50) {
                g.remove(k);
            }
        }
    }

    pub fn cancel(&self, id: JobId) -> bool {
        match self.inner.lock().get(&id) {
            Some(e) if e.info.state == JobState::Running => {
                e.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    pub fn get(&self, id: JobId) -> Option<JobInfo> {
        self.inner.lock().get(&id).map(|e| e.info.clone())
    }

    pub fn list(&self) -> Vec<JobInfo> {
        let mut v: Vec<JobInfo> = self.inner.lock().values().map(|e| e.info.clone()).collect();
        v.sort_by_key(|j| std::cmp::Reverse(j.started_ms));
        v
    }

    pub fn running(&self) -> usize {
        self.inner.lock().values().filter(|e| e.info.state == JobState::Running).count()
    }

    /// Wait until a job has finished (tests, CLI).
    pub fn wait(&self, id: JobId, timeout: std::time::Duration) -> Option<JobInfo> {
        let end = std::time::Instant::now() + timeout;
        loop {
            match self.get(id) {
                Some(j) if j.state != JobState::Running => return Some(j),
                None => return None,
                _ => {}
            }
            if std::time::Instant::now() >= end {
                return self.get(id);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn runner() -> (tempfile::TempDir, JobRunner, crossbeam_channel::Receiver<AppEvent>) {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(packsync_core::db::Db::open(&d.path().join("s.db")).unwrap());
        let bus = EventBus::new();
        let rx = bus.subscribe();
        (d, JobRunner::new(bus, db), rx)
    }

    #[test]
    fn success_failure_cancel_and_duplicate_coalescing() {
        let (_d, jobs, rx) = runner();
        let ok = jobs.start("t.ok", None, |_| Ok(serde_json::json!({"n": 1}))).unwrap();
        let j = jobs.wait(ok, Duration::from_secs(5)).unwrap();
        assert_eq!((j.state, j.result), (JobState::Succeeded, Some(serde_json::json!({"n": 1}))));

        let bad = jobs.start("t.bad", None, |_| Err(Error::Busy("x".into()))).unwrap();
        let j = jobs.wait(bad, Duration::from_secs(5)).unwrap();
        assert_eq!(j.state, JobState::Failed);
        assert_eq!(j.error.unwrap().code, "busy");

        let slow = jobs
            .start("t.slow", None, |ctx| {
                for _ in 0..500 {
                    ctx.check()?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(serde_json::Value::Null)
            })
            .unwrap();
        // the same request while it runs returns the same job
        assert_eq!(jobs.start("t.slow", None, |_| Ok(serde_json::Value::Null)).unwrap(), slow);
        assert!(jobs.cancel(slow));
        assert_eq!(jobs.wait(slow, Duration::from_secs(5)).unwrap().state, JobState::Cancelled);
        assert!(!jobs.cancel(slow), "finished jobs cannot be cancelled");

        // a cancel that surfaces from inside a storage call is still "cancelled", not "failed"
        let in_storage = jobs
            .start("t.storage-cancel", None, |_| Err(packsync_core::storage::StorageError::Cancelled.into()))
            .unwrap();
        let j = jobs.wait(in_storage, Duration::from_secs(5)).unwrap();
        assert_eq!((j.state, j.error.is_none()), (JobState::Cancelled, true), "{j:?}");

        let panicky = jobs.start("t.panic", None, |_| panic!("boom")).unwrap();
        let j = jobs.wait(panicky, Duration::from_secs(5)).unwrap();
        assert_eq!(j.state, JobState::Failed, "a panic is reported as a failed job, not a dead app");
        assert!(rx.try_iter().filter(|e| matches!(e, AppEvent::Job(_))).count() >= 8);
        assert_eq!(jobs.running(), 0);
    }
}
