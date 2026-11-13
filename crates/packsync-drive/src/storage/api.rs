//! Authenticated requests with bounded retries.

use std::sync::Arc;
use std::time::Duration;

use packsync_core::progress::{Level, OpCtx, Phase};
use packsync_core::redact::redact;
use packsync_core::retry::{RetryPolicy, Sleeper};
use packsync_core::storage::{AuthProblem, StorageError, StorageResult};
use serde::de::DeserializeOwned;

use crate::http::{self, JSON_LIMIT, Request, Response, Transport};
use crate::oauth::AccessTokenSource;
use crate::stats::StatsHandle;

pub(crate) struct Api {
    pub transport: Transport,
    /// The last few (redacted) retry reasons, for diagnostics.
    pub recent: parking_lot::Mutex<std::collections::VecDeque<String>>,
    pub tokens: Arc<dyn AccessTokenSource>,
    pub sleeper: Arc<dyn Sleeper>,
    pub retry: RetryPolicy,
    pub max_retry_after: Duration,
}

impl Api {
    pub fn stats(&self) -> &StatsHandle {
        self.transport.stats()
    }

    /// One exchange with a bearer token. On HTTP 401 the token is dropped and the request is sent
    /// once more with a freshly refreshed token; a second 401 is `Auth(Expired)`. Refresh failures
    /// surface as their own errors (`invalid_grant` → `Auth(Revoked)`, network → `Offline`).
    pub fn send(&self, req: &Request<'_>, ctx: &OpCtx) -> StorageResult<Response> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let token = self.tokens.access_token(ctx)?;
        let started = std::time::Instant::now();
        let resp = self.transport.send(req, Some(&token))?;
        // only the path: the query can hold a resumable session's upload_id (a capability)
        tracing::debug!(
            target: "packsync_drive::request",
            kind = ?req.kind,
            path = req.url.split('?').next().unwrap_or(""),
            // the files.list query (folder IDs and object names: no secrets), to see what a listing was for
            q = req.url.split(['?', '&']).find_map(|p| p.strip_prefix("q=")).unwrap_or(""),
            status = http::status(&resp),
            ms = started.elapsed().as_millis() as u64,
            "request"
        );
        if http::status(&resp) != 401 {
            return Ok(resp);
        }
        http::discard(resp, self.stats());
        self.tokens.invalidate(&token);
        tracing::debug!(target: "packsync_drive", "access token rejected (HTTP 401); refreshing once");
        let token = self.tokens.access_token(ctx)?;
        let resp = self.transport.send(req, Some(&token))?;
        if http::status(&resp) == 401 {
            http::discard(resp, self.stats());
            self.tokens.invalidate(&token);
            return Err(StorageError::Auth(AuthProblem::Expired));
        }
        Ok(resp)
    }

    /// Wait before retry number `failed + 1`. Returns the error to give up with, if any.
    pub fn backoff(
        &self,
        ctx: &OpCtx,
        phase: Phase,
        what: &str,
        failed: u32,
        err: &StorageError,
    ) -> Result<(), StorageError> {
        let retry_after = match err {
            StorageError::RateLimited { retry_after } => *retry_after,
            _ => None,
        };
        if retry_after.is_some_and(|ra| ra > self.max_retry_after) {
            // A long pause is the engine's decision, not ours.
            return Err(err.clone());
        }
        let delay = self.retry.delay_for(err, failed, retry_after);
        self.stats().retry();
        let msg = redact(&format!(
            "{what}: {err}; retrying in {:.1}s (attempt {}/{})",
            delay.as_secs_f32(),
            failed + 2,
            self.retry.max_attempts
        ))
        .into_owned();
        tracing::warn!(target: "packsync_drive", "{msg}");
        {
            let mut r = self.recent.lock();
            if r.len() >= 16 {
                r.pop_front();
            }
            r.push_back(msg.clone());
        }
        ctx.note(phase, Level::Warn, msg);
        if !self.sleeper.sleep(delay, ctx) {
            return Err(StorageError::Cancelled);
        }
        Ok(())
    }

    /// Run `op`, retrying retryable errors with truncated exponential backoff (full jitter,
    /// `Retry-After` as a floor, at most `retry.max_attempts` attempts).
    pub fn retrying<T>(
        &self,
        ctx: &OpCtx,
        phase: Phase,
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
                Err(e) if self.retry.may_retry(&e, attempt) => {
                    self.backoff(ctx, phase, what, attempt, &e)?;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Send with retries; statuses in `ok` are returned, all others are classified.
    pub fn call<'a>(
        &self,
        ctx: &OpCtx,
        phase: Phase,
        what: &str,
        ok: &[u16],
        build: impl Fn() -> Request<'a>,
    ) -> StorageResult<Response> {
        self.retrying(ctx, phase, what, |_| {
            let resp = self.send(&build(), ctx)?;
            if ok.contains(&http::status(&resp)) {
                Ok(resp)
            } else {
                Err(http::classify(&http::api_error(resp, self.stats()), what))
            }
        })
    }

    /// Send with retries and parse a 200 JSON response (body read errors are retried too).
    pub fn json<'a, T: DeserializeOwned>(
        &self,
        ctx: &OpCtx,
        phase: Phase,
        what: &str,
        build: impl Fn() -> Request<'a>,
    ) -> StorageResult<T> {
        self.retrying(ctx, phase, what, |_| {
            let mut resp = self.send(&build(), ctx)?;
            match http::status(&resp) {
                200 | 201 => http::read_json(&mut resp, JSON_LIMIT, self.stats(), what),
                _ => Err(http::classify(&http::api_error(resp, self.stats()), what)),
            }
        })
    }
}
