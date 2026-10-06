//! HTTP request accounting for benchmarks and diagnostics.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Kind of HTTP request sent to Google.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// `files.list` (folder resolution, name lookups, listings).
    List,
    /// `files.get` (metadata or `alt=media` content).
    Get,
    /// `files.create` (folders, multipart uploads).
    Create,
    /// Starting a resumable upload session.
    ResumableInit,
    /// One resumable upload chunk.
    Chunk,
    /// A resumable session status query (`Content-Range: bytes */N`).
    ResumableStatus,
    /// `files.update` (metadata PATCH, media replace, trash).
    Update,
    /// `files.delete`.
    Delete,
    /// OAuth token endpoint (code exchange, refresh) and revocation.
    Token,
    /// Everything else (`generateIds`, `about.get`).
    Other,
}

/// Snapshot of the request counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RequestStats {
    /// `files.list` requests.
    pub list: u64,
    /// `files.get` requests (metadata and media).
    pub get: u64,
    /// `files.create` requests (folders and multipart uploads).
    pub create: u64,
    /// Resumable session starts.
    pub resumable_init: u64,
    /// Resumable chunk uploads.
    pub chunk: u64,
    /// Resumable status queries.
    pub resumable_status: u64,
    /// `files.update` requests.
    pub update: u64,
    /// `files.delete` requests.
    pub delete: u64,
    /// Token endpoint and revocation requests.
    pub token: u64,
    /// Other requests (`generateIds`, `about.get`).
    pub other: u64,
    /// Request body bytes handed to the network (including retransmissions).
    pub bytes_up: u64,
    /// Response body bytes received.
    pub bytes_down: u64,
    /// Retries after a retryable failure (each one was preceded by a backoff sleep).
    pub retries: u64,
}

impl RequestStats {
    /// Total number of HTTP requests.
    pub fn total(&self) -> u64 {
        self.list
            + self.get
            + self.create
            + self.resumable_init
            + self.chunk
            + self.resumable_status
            + self.update
            + self.delete
            + self.token
            + self.other
    }

    /// Difference `self - earlier` (counters are monotonic).
    pub fn since(&self, earlier: &RequestStats) -> RequestStats {
        RequestStats {
            list: self.list.saturating_sub(earlier.list),
            get: self.get.saturating_sub(earlier.get),
            create: self.create.saturating_sub(earlier.create),
            resumable_init: self.resumable_init.saturating_sub(earlier.resumable_init),
            chunk: self.chunk.saturating_sub(earlier.chunk),
            resumable_status: self.resumable_status.saturating_sub(earlier.resumable_status),
            update: self.update.saturating_sub(earlier.update),
            delete: self.delete.saturating_sub(earlier.delete),
            token: self.token.saturating_sub(earlier.token),
            other: self.other.saturating_sub(earlier.other),
            bytes_up: self.bytes_up.saturating_sub(earlier.bytes_up),
            bytes_down: self.bytes_down.saturating_sub(earlier.bytes_down),
            retries: self.retries.saturating_sub(earlier.retries),
        }
    }
}

#[derive(Default)]
struct Counters {
    by_kind: [AtomicU64; 10],
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
    retries: AtomicU64,
}

/// Shared, thread-safe request counters. Cloning shares the counters, so one handle can be given
/// to both the [`DriveStorage`](crate::DriveStorage) and the
/// [`Authenticator`](crate::oauth::Authenticator).
#[derive(Clone, Default)]
pub struct StatsHandle(Arc<Counters>);

impl fmt::Debug for StatsHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.snapshot(), f)
    }
}

fn index(kind: RequestKind) -> usize {
    match kind {
        RequestKind::List => 0,
        RequestKind::Get => 1,
        RequestKind::Create => 2,
        RequestKind::ResumableInit => 3,
        RequestKind::Chunk => 4,
        RequestKind::ResumableStatus => 5,
        RequestKind::Update => 6,
        RequestKind::Delete => 7,
        RequestKind::Token => 8,
        RequestKind::Other => 9,
    }
}

impl StatsHandle {
    /// New, zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current values.
    pub fn snapshot(&self) -> RequestStats {
        let c = &self.0;
        let k = |i: usize| c.by_kind[i].load(Ordering::Relaxed);
        RequestStats {
            list: k(0),
            get: k(1),
            create: k(2),
            resumable_init: k(3),
            chunk: k(4),
            resumable_status: k(5),
            update: k(6),
            delete: k(7),
            token: k(8),
            other: k(9),
            bytes_up: c.bytes_up.load(Ordering::Relaxed),
            bytes_down: c.bytes_down.load(Ordering::Relaxed),
            retries: c.retries.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn request(&self, kind: RequestKind) {
        self.0.by_kind[index(kind)].fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn up(&self, n: u64) {
        self.0.bytes_up.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn down(&self, n: u64) {
        self.0.bytes_down.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn retry(&self) {
        self.0.retries.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counting_and_since() {
        let h = StatsHandle::new();
        let shared = h.clone();
        h.request(RequestKind::List);
        shared.request(RequestKind::Chunk);
        shared.request(RequestKind::Chunk);
        h.up(10);
        h.down(5);
        h.retry();
        let s = h.snapshot();
        assert_eq!((s.list, s.chunk, s.bytes_up, s.bytes_down, s.retries, s.total()), (1, 2, 10, 5, 1, 3));
        let before = s.clone();
        h.request(RequestKind::Token);
        let d = h.snapshot().since(&before);
        assert_eq!((d.token, d.total()), (1, 1));
    }
}
