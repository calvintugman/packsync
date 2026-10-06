//! Streaming downloads (`files.get?alt=media`, optionally with `Range`). The reader delivers
//! exactly the requested bytes, resumes after dropped connections from the current position
//! (bounded), honours cancellation, and verifies the server's MD5 at the end of whole-object reads.

use std::io::{self, Read};
use std::sync::Arc;

use md5::{Digest, Md5};
use packsync_core::progress::{OpCtx, Phase};
use packsync_core::storage::{StorageError, StorageResult};

use super::Inner;
use crate::http::{self, Method, Request};
use crate::stats::RequestKind;
use crate::util::with_query;

pub(super) struct DownloadReader {
    inner: Arc<Inner>,
    id: String,
    size: u64,
    pos: u64,
    end: u64,
    body: Option<Box<dyn Read + Send>>,
    md5: Option<(Md5, String)>,
    resumes: u32,
    ctx: OpCtx,
}

/// Parse `Content-Range: bytes a-b/total` (total may be `*`).
fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let total = if total.trim() == "*" { None } else { Some(total.trim().parse().ok()?) };
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, total))
}

impl Inner {
    /// Open `[start, end)` of file `id` (of `size` bytes) and return a body positioned at `start`.
    pub(super) fn open_media(
        &self,
        id: &str,
        size: u64,
        start: u64,
        end: u64,
        ctx: &OpCtx,
    ) -> StorageResult<Box<dyn Read + Send>> {
        let ranged = start > 0 || end < size;
        let base = format!("{}/files/{id}", self.cfg.endpoints.api_base);
        let mut acknowledge_abuse = false;
        self.api.retrying(ctx, Phase::StageDownload, "downloading an object", |_| {
            loop {
                let mut params = vec![("alt", "media")];
                if acknowledge_abuse {
                    params.push(("acknowledgeAbuse", "true"));
                }
                let mut req = Request::new(Method::Get, with_query(&base, &params), RequestKind::Get)
                    .header("Accept-Encoding", "identity")
                    .expect_down(end - start);
                if ranged {
                    req = req.header("Range", format!("bytes={start}-{}", end - 1));
                }
                let resp = self.api.send(&req, ctx)?;
                let content_length = http::header(&resp, "content-length").and_then(|v| v.trim().parse::<u64>().ok());
                match http::status(&resp) {
                    206 => {
                        let cr = http::header(&resp, "content-range").and_then(|v| parse_content_range(&v));
                        match cr {
                            Some((a, b, total))
                                if a == start
                                    && b + 1 == end
                                    && total.is_none_or(|t| t == size)
                                    && content_length.is_none_or(|l| l == end - start) => {}
                            _ => {
                                return Err(StorageError::Corrupt(
                                    "the server answered a range request with a different range".into(),
                                ));
                            }
                        }
                        return Ok(Box::new(resp.into_body().into_reader()) as Box<dyn Read + Send>);
                    }
                    200 => {
                        if content_length.is_some_and(|l| l != size) {
                            return Err(StorageError::Corrupt(format!(
                                "the server sent {} bytes for an object of {size} bytes",
                                content_length.unwrap_or(0)
                            )));
                        }
                        let mut body: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
                        if start > 0 {
                            // The server ignored `Range`: skip to the requested offset.
                            let skipped = io::copy(&mut (&mut body).take(start), &mut io::sink())
                                .map_err(|e| StorageError::Offline(format!("download interrupted: {e}")))?;
                            if skipped != start {
                                return Err(StorageError::Offline("download interrupted".into()));
                            }
                        }
                        return Ok(body);
                    }
                    416 => {
                        http::discard(resp, self.api.stats());
                        return Err(StorageError::Corrupt(
                            "the stored object is shorter than its metadata says".into(),
                        ));
                    }
                    _ => {
                        let e = http::api_error(resp, self.api.stats());
                        if e.status == 403 && e.has_reason("cannotDownloadAbusiveFile") && !acknowledge_abuse {
                            // PackSync verifies every object by hash; Drive's malware flag on our own
                            // pack must not make it unreadable.
                            acknowledge_abuse = true;
                            continue;
                        }
                        return Err(http::classify(&e, "downloading an object"));
                    }
                }
            }
        })
    }
}

impl DownloadReader {
    pub(super) fn open(
        inner: Arc<Inner>,
        id: String,
        size: u64,
        start: u64,
        end: u64,
        expected_md5: Option<String>,
        ctx: &OpCtx,
    ) -> StorageResult<Self> {
        let body = inner.open_media(&id, size, start, end, ctx)?;
        Ok(DownloadReader {
            inner,
            id,
            size,
            pos: start,
            end,
            body: Some(body),
            md5: expected_md5.map(|m| (Md5::new(), m)),
            resumes: 0,
            ctx: ctx.clone(),
        })
    }

    fn finish(&mut self) -> io::Result<usize> {
        if let Some((h, expected)) = self.md5.take() {
            let got = hex::encode(h.finalize());
            if got != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the downloaded content does not match the server's MD5 checksum",
                ));
            }
        }
        Ok(0)
    }

    fn resume_after(&mut self, err: io::Error) -> io::Result<()> {
        self.body = None;
        self.resumes += 1;
        if self.resumes > self.inner.cfg.max_download_resumes {
            return Err(err);
        }
        let msg = format!("download interrupted ({err})");
        self.inner
            .api
            .backoff(&self.ctx, Phase::StageDownload, &msg, self.resumes - 1, &StorageError::Offline(err.to_string()))
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

impl Read for DownloadReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.ctx.is_cancelled() {
                return Err(io::Error::other("operation cancelled"));
            }
            if self.pos >= self.end {
                return self.finish();
            }
            let body = match self.body.as_mut() {
                Some(b) => b,
                None => {
                    let b = self
                        .inner
                        .open_media(&self.id, self.size, self.pos, self.end, &self.ctx)
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    self.body.insert(b)
                }
            };
            let want = (buf.len() as u64).min(self.end - self.pos) as usize;
            match body.read(&mut buf[..want]) {
                Ok(0) => {
                    self.resume_after(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection closed early"))?;
                }
                Ok(n) => {
                    self.pos += n as u64;
                    if let Some((h, _)) = self.md5.as_mut() {
                        h.update(&buf[..n]);
                    }
                    self.ctx.add_down(n as u64);
                    self.inner.api.stats().down(n as u64);
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => self.resume_after(e)?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 0-9/100"), Some((0, 9, Some(100))));
        assert_eq!(parse_content_range("bytes 5-5/*"), Some((5, 5, None)));
        assert_eq!(parse_content_range("bytes */100"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }
}
