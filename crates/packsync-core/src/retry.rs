//! Bounded exponential backoff with full jitter. Time is injected so tests never really sleep.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::progress::{Level, OpCtx, Phase};
use crate::storage::{StorageError, StorageResult};

pub trait Sleeper: Send + Sync {
    /// Sleep for `d`, returning early (with `false`) when `ctx` is cancelled.
    fn sleep(&self, d: Duration, ctx: &OpCtx) -> bool;
}

/// Real wall-clock sleeping in short slices so cancellation stays responsive.
pub struct RealSleeper;
impl Sleeper for RealSleeper {
    fn sleep(&self, d: Duration, ctx: &OpCtx) -> bool {
        let slice = Duration::from_millis(100);
        let mut left = d;
        while left > Duration::ZERO {
            if ctx.is_cancelled() {
                return false;
            }
            let step = left.min(slice);
            std::thread::sleep(step);
            left -= step;
        }
        !ctx.is_cancelled()
    }
}

/// Records requested sleeps instead of sleeping (tests, deterministic fault injection).
#[derive(Default)]
pub struct RecordingSleeper {
    pub slept: Mutex<Vec<Duration>>,
}
impl Sleeper for RecordingSleeper {
    fn sleep(&self, d: Duration, ctx: &OpCtx) -> bool {
        self.slept.lock().push(d);
        !ctx.is_cancelled()
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first one.
    pub max_attempts: u32,
    pub base: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Provider-level policy: a handful of quick retries (Google recommends truncated exponential
    /// backoff with a 32–64 s cap).
    pub fn network() -> Self {
        Self { max_attempts: 6, base: Duration::from_millis(500), max_delay: Duration::from_secs(32) }
    }
    pub fn quick() -> Self {
        Self { max_attempts: 3, base: Duration::from_millis(100), max_delay: Duration::from_secs(2) }
    }
    pub fn none() -> Self {
        Self { max_attempts: 1, base: Duration::ZERO, max_delay: Duration::ZERO }
    }

    /// Full-jitter delay for the given (0-based) failed attempt: uniform in `[0, min(max, base·2^n)]`,
    /// but never below `retry_after` when the server asked for a pause.
    pub fn delay(&self, failed_attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exp = self.base.saturating_mul(1u32.checked_shl(failed_attempt.min(20)).unwrap_or(u32::MAX));
        let cap = exp.min(self.max_delay);
        let jittered = Duration::from_secs_f64(cap.as_secs_f64() * fastrand::f64());
        match retry_after {
            Some(ra) => jittered.max(ra).min(self.max_delay.max(ra)),
            None => jittered,
        }
    }

    /// Run `op`, retrying retryable [`StorageError`]s. Non-retryable errors and exhaustion are
    /// returned unchanged so callers keep honest status.
    pub fn run<T>(
        &self,
        sleeper: &dyn Sleeper,
        ctx: &OpCtx,
        what: &str,
        mut op: impl FnMut(u32) -> StorageResult<T>,
    ) -> StorageResult<T> {
        let mut attempt = 0u32;
        loop {
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            match op(attempt) {
                Ok(v) => return Ok(v),
                Err(e) if e.is_retryable() && attempt + 1 < self.max_attempts => {
                    let retry_after = match &e {
                        StorageError::RateLimited { retry_after } => *retry_after,
                        _ => None,
                    };
                    let d = self.delay(attempt, retry_after);
                    ctx.note(
                        Phase::Upload,
                        Level::Warn,
                        format!(
                            "{what}: {e}; retrying in {:.1}s (attempt {}/{})",
                            d.as_secs_f32(),
                            attempt + 2,
                            self.max_attempts
                        ),
                    );
                    if !sleeper.sleep(d, ctx) {
                        return Err(StorageError::Cancelled);
                    }
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

pub fn real_sleeper() -> Arc<dyn Sleeper> {
    Arc::new(RealSleeper)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn retries_then_succeeds_without_sleeping() {
        let policy = RetryPolicy::network();
        let sleeper = RecordingSleeper::default();
        let calls = AtomicU32::new(0);
        let r = policy.run(&sleeper, &OpCtx::detached(), "t", |_| {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 { Err(StorageError::Transient("503".into())) } else { Ok(7) }
        });
        assert_eq!(r.unwrap(), 7);
        assert_eq!(sleeper.slept.lock().len(), 2);
    }

    #[test]
    fn non_retryable_fails_immediately() {
        let sleeper = RecordingSleeper::default();
        let calls = AtomicU32::new(0);
        let r: StorageResult<()> = RetryPolicy::network().run(&sleeper, &OpCtx::detached(), "t", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(StorageError::QuotaExceeded("full".into()))
        });
        assert!(matches!(r, Err(StorageError::QuotaExceeded(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(sleeper.slept.lock().is_empty());
    }

    #[test]
    fn exhaustion_returns_last_error_and_is_bounded() {
        let sleeper = RecordingSleeper::default();
        let calls = AtomicU32::new(0);
        let r: StorageResult<()> = RetryPolicy::network().run(&sleeper, &OpCtx::detached(), "t", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(StorageError::RateLimited { retry_after: Some(Duration::from_secs(3)) })
        });
        assert!(matches!(r, Err(StorageError::RateLimited { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        assert!(sleeper.slept.lock().iter().all(|d| *d >= Duration::from_secs(3)), "retry_after is a floor");
    }

    #[test]
    fn delay_is_capped_and_jittered() {
        let p = RetryPolicy::network();
        for n in 0..40 {
            assert!(p.delay(n, None) <= p.max_delay);
        }
    }

    #[test]
    fn cancellation_stops_retrying() {
        let ctx = OpCtx::detached();
        ctx.cancel.cancel();
        let r: StorageResult<()> = RetryPolicy::network().run(&RecordingSleeper::default(), &ctx, "t", |_| Ok(()));
        assert!(matches!(r, Err(StorageError::Cancelled)));
    }
}
