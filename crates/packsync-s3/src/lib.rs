//! PackSync storage provider for S3-compatible object stores: **Cloudflare R2** (the target), MinIO
//! and AWS S3.
//!
//! What S3 gives that Google Drive does not, and how it is used:
//! * unique keys and atomic replacement;
//! * **conditional writes** (`If-None-Match: *` / `If-Match: <etag>`): create-only is enforced by the
//!   server, and `PutMode::IfVersion` is a real compare-and-swap ([`Capabilities::conditional_write`]);
//! * strongly consistent listings (R2, AWS since 2020, MinIO).
//!
//! What it lacks: listings carry no metadata, so tags are only seen by `stat`; the provider therefore
//! requires a bucket or prefix of its own ([`Capabilities::owned_namespace`]) and everything under it
//! is PackSync's. Integrity: every upload carries `Content-MD5` (the server rejects a damaged body) and
//! the returned ETag (the MD5 of a single-part object) is checked.
//!
//! Requests are signed with AWS Signature Version 4 ([`sigv4`]); the payload is not hashed into the
//! signature (`UNSIGNED-PAYLOAD`, allowed over TLS).

pub mod sigv4;
mod xml;

#[cfg(feature = "mock")]
pub mod mock;

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use md5::{Digest as _, Md5};
use packsync_core::hash::Hash;
use packsync_core::progress::OpCtx;
use packsync_core::redact::Secret;
use packsync_core::retry::{RetryPolicy, Sleeper, real_sleeper};
use packsync_core::storage::*;

/// Largest object this provider uploads in one request (packs are at most 256 MiB).
const MAX_SINGLE_PUT: u64 = 512 * 1024 * 1024;
/// Largest S3 XML response read into memory (a page of 1000 keys is ~300 KB).
const MAX_XML: u64 = 16 * 1024 * 1024;

/// Where the objects live and with which key.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// `https://<account_id>.r2.cloudflarestorage.com` for R2 (no trailing slash).
    pub endpoint: String,
    pub bucket: String,
    /// Every key is stored under `<prefix>/` (default `packsync`); the prefix belongs to PackSync.
    pub prefix: String,
    /// `auto` for R2.
    pub region: String,
    pub access_key_id: String,
    /// How it is shown and identified (`r2:<account>`); part of the provider ID.
    pub label: String,
    pub retry: RetryPolicy,
    pub connect_timeout: Duration,
    /// Per-request time budget besides the body (which gets [`S3Config::min_rate`] on top).
    pub request_timeout: Duration,
    /// Slowest acceptable transfer rate, bytes per second, used to size body timeouts.
    pub min_rate: u64,
}

impl S3Config {
    /// Cloudflare R2: `https://<account_id>.r2.cloudflarestorage.com`, region `auto`.
    pub fn r2(account_id: &str, bucket: &str, prefix: &str, access_key_id: &str) -> Self {
        Self {
            endpoint: format!("https://{account_id}.r2.cloudflarestorage.com"),
            bucket: bucket.into(),
            prefix: prefix.trim_matches('/').into(),
            region: "auto".into(),
            access_key_id: access_key_id.into(),
            label: format!("r2:{account_id}"),
            retry: RetryPolicy::network(),
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(60),
            min_rate: 64 * 1024,
        }
    }
}

/// HTTP request counters (for `--explain` and benchmarks).
#[derive(Debug, Default)]
pub struct S3Stats {
    pub requests: AtomicU64,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
    pub retries: AtomicU64,
}

impl S3Stats {
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes_up.load(Ordering::Relaxed),
            self.bytes_down.load(Ordering::Relaxed),
            self.retries.load(Ordering::Relaxed),
        )
    }
}

pub struct S3Storage {
    cfg: S3Config,
    secret: Secret,
    agent: ureq::Agent,
    host: String,
    sleeper: Arc<dyn Sleeper>,
    stats: Arc<S3Stats>,
}

impl std::fmt::Debug for S3Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Storage")
            .field("endpoint", &self.cfg.endpoint)
            .field("bucket", &self.cfg.bucket)
            .finish_non_exhaustive()
    }
}

/// One HTTP exchange's outcome: status, headers we care about, body (bounded).
struct Reply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

fn md5_of(data: &[u8]) -> [u8; 16] {
    Md5::digest(data).into()
}

fn etag_clean(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

impl S3Storage {
    pub fn new(cfg: S3Config, secret: Secret) -> StorageResult<Self> {
        Self::with_sleeper(cfg, secret, real_sleeper())
    }

    pub fn with_sleeper(cfg: S3Config, secret: Secret, sleeper: Arc<dyn Sleeper>) -> StorageResult<Self> {
        let host = cfg
            .endpoint
            .strip_prefix("https://")
            .or_else(|| cfg.endpoint.strip_prefix("http://"))
            .ok_or_else(|| StorageError::Other(format!("endpoint {:?} must start with https://", cfg.endpoint)))?
            .trim_end_matches('/')
            .to_string();
        if host.is_empty() || host.contains('/') {
            return Err(StorageError::Other(format!("endpoint {:?} must be a scheme and a host only", cfg.endpoint)));
        }
        if cfg.bucket.is_empty()
            || !cfg.bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        {
            return Err(StorageError::Other(format!("{:?} is not a valid bucket name", cfg.bucket)));
        }
        if cfg.prefix.is_empty() || validate_key(&format!("{}/", cfg.prefix)).is_err() {
            return Err(StorageError::Other(format!(
                "{:?} is not a valid prefix (letters, digits, . _ -)",
                cfg.prefix
            )));
        }
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(format!("{}/{} packsync-s3", packsync_core::brand::PRODUCT_NAME, env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(cfg.connect_timeout))
            .timeout_resolve(Some(cfg.connect_timeout))
            .timeout_send_request(Some(cfg.request_timeout))
            .timeout_recv_response(Some(cfg.request_timeout))
            .max_idle_connections_per_host(16)
            .build();
        Ok(S3Storage { agent: config.into(), host, secret, sleeper, stats: Arc::new(S3Stats::default()), cfg })
    }

    pub fn stats(&self) -> Arc<S3Stats> {
        self.stats.clone()
    }

    fn object_path(&self, key: &str) -> String {
        format!("/{}/{}/{}", self.cfg.bucket, self.cfg.prefix, key)
    }

    fn body_budget(&self, bytes: u64) -> Duration {
        self.cfg.request_timeout + Duration::from_secs(bytes / self.cfg.min_rate.max(1))
    }

    /// One signed request (no retries). `path` is unencoded (`/bucket/prefix/key`).
    fn exchange(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        extra: &[(String, String)],
        body: Option<&[u8]>,
        max_body: u64,
    ) -> StorageResult<Reply> {
        let amz_date = sigv4::amz_date_now();
        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), self.host.clone()),
            ("x-amz-date".into(), amz_date.clone()),
            (
                "x-amz-content-sha256".into(),
                if body.is_some() { sigv4::UNSIGNED_PAYLOAD.into() } else { sigv4::EMPTY_SHA256.into() },
            ),
        ];
        headers.extend(extra.iter().cloned());
        let canonical_uri = sigv4::uri_encode(path, true);
        let auth = sigv4::authorization(&sigv4::SignInput {
            access_key_id: &self.cfg.access_key_id,
            secret: self.secret.expose(),
            region: &self.cfg.region,
            service: "s3",
            amz_date: &amz_date,
            method,
            canonical_uri: &canonical_uri,
            query,
            headers: &headers,
            payload_hash: &headers[2].1,
        });
        let qs = sigv4::canonical_query(query);
        let url = format!(
            "{}{}{}",
            self.cfg.endpoint.trim_end_matches('/'),
            canonical_uri,
            if qs.is_empty() { String::new() } else { format!("?{qs}") }
        );
        let up = body.map(|b| b.len() as u64).unwrap_or(0);
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_up.fetch_add(up, Ordering::Relaxed);
        let started = std::time::Instant::now();
        macro_rules! prep {
            ($b:expr) => {{
                let mut b = $b
                    .config()
                    .timeout_send_body(Some(self.body_budget(up)))
                    .timeout_recv_body(Some(self.body_budget(max_body.min(64 << 20))))
                    .build();
                for (k, v) in &headers {
                    if k != "host" {
                        b = b.header(k.as_str(), v.as_str());
                    }
                }
                b.header("authorization", auth.as_str())
            }};
        }
        let res = match (method, body) {
            ("GET", _) => prep!(self.agent.get(&url)).call(),
            ("HEAD", _) => prep!(self.agent.head(&url)).call(),
            ("DELETE", _) => prep!(self.agent.delete(&url)).call(),
            ("PUT", Some(b)) => prep!(self.agent.put(&url)).send(b),
            ("PUT", None) => prep!(self.agent.put(&url)).send_empty(),
            _ => return Err(StorageError::Other(format!("unsupported method {method}"))),
        };
        let mut resp = res.map_err(net_error)?;
        let status = resp.status().as_u16();
        let mut hs = BTreeMap::new();
        for (k, v) in resp.headers() {
            if let Ok(v) = v.to_str() {
                hs.insert(k.as_str().to_ascii_lowercase(), v.to_string());
            }
        }
        let mut out = Vec::new();
        if method != "HEAD" {
            let limit = if (200..300).contains(&status) { max_body } else { 64 * 1024 };
            let r = resp
                .body_mut()
                .with_config()
                .limit(limit.saturating_add(1))
                .reader()
                .take(limit.saturating_add(1))
                .read_to_end(&mut out);
            if let Err(e) = r {
                return Err(StorageError::Offline(format!("reading the response failed: {e}")));
            }
            if out.len() as u64 > limit {
                return Err(StorageError::Other(format!("the server response exceeded {limit} bytes")));
            }
        }
        self.stats.bytes_down.fetch_add(out.len() as u64, Ordering::Relaxed);
        tracing::debug!(target: "packsync_s3::request", method, path, status, ms = started.elapsed().as_millis() as u64, "request");
        Ok(Reply { status, headers: hs, body: out })
    }

    /// `exchange` with retries for retryable failures (network, 5xx, 429/503 SlowDown).
    #[allow(clippy::too_many_arguments)]
    fn call(
        &self,
        ctx: &OpCtx,
        what: &str,
        method: &str,
        path: &str,
        query: &[(String, String)],
        extra: &[(String, String)],
        body: Option<&[u8]>,
        max_body: u64,
    ) -> StorageResult<Reply> {
        self.cfg.retry.run(&*self.sleeper, ctx, what, |attempt| {
            if attempt > 0 {
                self.stats.retries.fetch_add(1, Ordering::Relaxed);
            }
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let r = self.exchange(method, path, query, extra, body, max_body)?;
            match r.status {
                500..=599 | 429 => Err(classify(&r, what)),
                _ => Ok(r),
            }
        })
    }

    fn info_from_head(&self, key: &str, r: &Reply) -> ObjectInfo {
        let size = r.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut info = ObjectInfo::file(key, size);
        let ms = r
            .header("last-modified")
            .and_then(|d| httpdate::parse_http_date(d).ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        info.created_ms = ms;
        info.modified_ms = ms;
        if let Some(e) = r.header("etag") {
            let e = etag_clean(e);
            if e.len() == 32 && e.bytes().all(|b| b.is_ascii_hexdigit()) {
                info.checksums.md5 = Some(e.to_ascii_lowercase());
            }
            info.version = Some(e);
        }
        for (k, v) in &r.headers {
            if let Some(t) = k.strip_prefix("x-amz-meta-") {
                info.tags.insert(t.to_string(), v.clone());
            }
        }
        info
    }

    fn head(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        let r = self.call(ctx, "checking an object", "HEAD", &self.object_path(key), &[], &[], None, 0)?;
        match r.status {
            200 => Ok(Some(self.info_from_head(key, &r))),
            404 => Ok(None),
            _ => Err(classify(&r, "checking an object")),
        }
    }
}

/// Map a non-success reply to a storage error. The XML error body gives the S3 error code.
fn classify(r: &Reply, what: &str) -> StorageError {
    let text = String::from_utf8_lossy(&r.body);
    let code = xml::text(&text, "Code").unwrap_or_default();
    let message = xml::text(&text, "Message").unwrap_or_default();
    let detail = format!("{what}: HTTP {} {code} {message}", r.status).trim().to_string();
    let retry_after = r.header("retry-after").and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs);
    match (r.status, code.as_str()) {
        (_, "SlowDown") | (429, _) => StorageError::RateLimited { retry_after },
        (503, _) => StorageError::RateLimited { retry_after },
        (500..=599, _) => StorageError::Transient(detail),
        (
            _,
            "InvalidAccessKeyId"
            | "SignatureDoesNotMatch"
            | "InvalidToken"
            | "ExpiredToken"
            | "AuthorizationHeaderMalformed",
        ) => StorageError::Auth(AuthProblem::Other(format!(
            "the access key was not accepted ({code}); store the right key with `packsync storage use-r2`"
        ))),
        (403, _) => StorageError::Permission(detail),
        (404, "NoSuchBucket") => StorageError::Other(format!("the bucket does not exist ({detail})")),
        (404, _) => StorageError::NotFound(detail),
        (412, _) | (409, _) => StorageError::Conflict(detail),
        (400, "EntityTooLarge") => StorageError::Other(detail),
        (400, "BadDigest" | "InvalidDigest") => StorageError::Corrupt(detail),
        _ => StorageError::Other(detail),
    }
}

fn net_error(e: ureq::Error) -> StorageError {
    match e {
        ureq::Error::Timeout(t) => StorageError::Offline(format!("request timed out ({t})")),
        ureq::Error::HostNotFound => StorageError::Offline("DNS lookup failed".into()),
        ureq::Error::ConnectionFailed => StorageError::Offline("connection failed".into()),
        ureq::Error::Io(io) => StorageError::Offline(format!("network I/O error: {io}")),
        other => StorageError::Transient(format!("network error: {other}")),
    }
}

impl Storage for S3Storage {
    fn provider_id(&self) -> String {
        format!("{}:{}/{}", self.cfg.label, self.cfg.bucket, self.cfg.prefix)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: "s3".into(),
            unique_keys: true,
            atomic_replace: true,
            conditional_write: true,
            owned_namespace: true,
            range_reads: true,
            resumable_uploads: false,
            server_checksums: vec![ChecksumKind::Md5],
            list_consistency: Consistency::Strong,
            max_object_size: Some(MAX_SINGLE_PUT),
            recommended_concurrency: 8,
            recommended_pack_size: 16 * 1024 * 1024,
        }
    }

    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>> {
        validate_key(prefix)?;
        if !prefix.ends_with('/') {
            return Err(StorageError::Other(format!("list prefix {prefix:?} must end with '/'")));
        }
        let root = format!("{}/", self.cfg.prefix);
        let full = format!("{root}{prefix}");
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..100_000 {
            let mut q = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), full.clone()),
                ("delimiter".to_string(), "/".to_string()),
                ("max-keys".to_string(), "1000".to_string()),
            ];
            if let Some(t) = &token {
                q.push(("continuation-token".to_string(), t.clone()));
            }
            let r =
                self.call(ctx, "listing objects", "GET", &format!("/{}", self.cfg.bucket), &q, &[], None, MAX_XML)?;
            if r.status != 200 {
                return Err(classify(&r, "listing objects"));
            }
            let page = xml::parse_list(&String::from_utf8_lossy(&r.body)).map_err(StorageError::Other)?;
            for f in page.files {
                let Some(key) = f.key.strip_prefix(&root) else { continue };
                if validate_key(key).is_err() || key.ends_with('/') {
                    continue; // something PackSync would never have written
                }
                let mut info = ObjectInfo::file(key, f.size);
                let ms = time::OffsetDateTime::parse(&f.last_modified, &time::format_description::well_known::Rfc3339)
                    .ok()
                    .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64);
                info.created_ms = ms;
                info.modified_ms = ms;
                let e = etag_clean(&f.etag);
                if e.len() == 32 && e.bytes().all(|b| b.is_ascii_hexdigit()) {
                    info.checksums.md5 = Some(e.to_ascii_lowercase());
                }
                info.version = Some(e);
                out.push(info);
            }
            for p in page.prefixes {
                if let Some(key) = p.strip_prefix(&root)
                    && validate_key(key).is_ok()
                {
                    let mut d = ObjectInfo::file(key, 0);
                    d.is_dir = true;
                    out.push(d);
                }
            }
            match (page.truncated, page.next_token) {
                (true, Some(t)) => token = Some(t),
                (true, None) => {
                    return Err(StorageError::Transient(
                        "the listing was cut short without a continuation token".into(),
                    ));
                }
                _ => return Ok(out),
            }
        }
        Err(StorageError::Other("listing did not end".into()))
    }

    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        validate_key(key)?;
        if key.ends_with('/') {
            return Ok(None);
        }
        self.head(key, ctx)
    }

    fn get(&self, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>> {
        validate_key(key)?;
        let (extra, expect) = match range {
            Some((_, 0)) => return Ok(Box::new(std::io::empty())),
            Some((s, l)) => (vec![("range".to_string(), format!("bytes={s}-{}", s + l - 1))], Some(l)),
            None => (vec![], None),
        };
        let max = expect.unwrap_or(MAX_SINGLE_PUT);
        let r = self.call(ctx, "downloading", "GET", &self.object_path(key), &[], &extra, None, max)?;
        match (r.status, expect) {
            (206, Some(l)) | (200, Some(l)) if r.body.len() as u64 == l => {}
            (200, None) => {
                if let Some(cl) = r.header("content-length").and_then(|v| v.parse::<u64>().ok())
                    && cl != r.body.len() as u64
                {
                    return Err(StorageError::Offline(format!(
                        "{key}: the download ended after {} of {cl} bytes",
                        r.body.len()
                    )));
                }
            }
            (200 | 206, Some(l)) => {
                return Err(StorageError::Corrupt(format!("{key}: asked for {l} bytes, received {}", r.body.len())));
            }
            (416, _) => return Err(StorageError::Other(format!("range outside object {key}"))),
            _ => return Err(classify(&r, "downloading")),
        }
        ctx.add_down(r.body.len() as u64);
        Ok(Box::new(std::io::Cursor::new(r.body)))
    }

    fn put(&self, key: &str, src: PutSource<'_>, opts: &PutOptions, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        validate_key(key)?;
        if key.ends_with('/') {
            return Err(StorageError::Other(format!("cannot put directory key {key:?}")));
        }
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        if src.len() > MAX_SINGLE_PUT {
            return Err(StorageError::Other(format!("{key}: {} bytes exceed the single-upload limit", src.len())));
        }
        let data: Vec<u8> = match &src {
            PutSource::Bytes(b) => b.to_vec(),
            PutSource::File { path, len } => {
                let mut v = Vec::with_capacity(*len as usize);
                std::fs::File::open(path)?.take(len + 1).read_to_end(&mut v)?;
                if v.len() as u64 != *len {
                    return Err(StorageError::Corrupt(format!(
                        "source changed while uploading: expected {len} bytes, read {}",
                        v.len()
                    )));
                }
                v
            }
        };
        if let Some(d) = &opts.digest
            && (d.size != data.len() as u64 || d.blake3 != Hash::of(&data))
        {
            return Err(StorageError::Corrupt(format!("{key}: content does not match the declared digest")));
        }
        let md5 = md5_of(&data);
        let md5_hex = hex::encode(md5);
        let mut extra: Vec<(String, String)> = vec![
            ("content-md5".into(), base64::engine::general_purpose::STANDARD.encode(md5)),
            ("content-type".into(), "application/octet-stream".into()),
        ];
        for (k, v) in &opts.tags {
            if !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !v.bytes().all(|b| (0x20..0x7f).contains(&b))
            {
                return Err(StorageError::Other(format!("tag {k:?} cannot be stored as object metadata")));
            }
            extra.push((format!("x-amz-meta-{}", k.to_ascii_lowercase()), v.clone()));
        }
        match &opts.mode {
            PutMode::CreateOnly | PutMode::IfVersion(None) => extra.push(("if-none-match".into(), "*".into())),
            PutMode::IfVersion(Some(v)) => extra.push(("if-match".into(), format!("\"{v}\""))),
            PutMode::Replace => {}
        }
        let r = self.call(ctx, "uploading", "PUT", &self.object_path(key), &[], &extra, Some(&data), 64 * 1024)?;
        match r.status {
            200 => {
                let etag = r.header("etag").map(etag_clean).unwrap_or_default();
                if !etag.eq_ignore_ascii_case(&md5_hex) {
                    return Err(StorageError::Corrupt(format!(
                        "{key}: the server stored something else (ETag {etag}, expected {md5_hex})"
                    )));
                }
                ctx.add_up(data.len() as u64);
                let mut info = ObjectInfo::file(key, data.len() as u64);
                info.checksums.md5 = Some(md5_hex);
                info.checksums.blake3 = opts.digest.map(|d| d.blake3.to_hex());
                info.version = Some(etag);
                info.tags = opts.tags.clone();
                info.modified_ms = Some(now_ms());
                info.created_ms = info.modified_ms;
                Ok(info)
            }
            412 | 409 if opts.mode == PutMode::CreateOnly => {
                // It exists: the same content makes the create a success (idempotent), anything else a conflict.
                match self.head(key, ctx)? {
                    Some(info)
                        if info.size == data.len() as u64
                            && info.checksums.md5.as_deref() == Some(md5_hex.as_str()) =>
                    {
                        Ok(info)
                    }
                    Some(_) => Err(StorageError::Conflict(format!("{key} already exists with different content"))),
                    None => Err(StorageError::Transient(format!(
                        "{key}: the precondition failed but the object is gone; retry"
                    ))),
                }
            }
            _ => Err(classify(&r, "uploading")),
        }
    }

    fn delete(&self, key: &str, _mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        validate_key(key)?;
        let r = self.call(ctx, "deleting", "DELETE", &self.object_path(key), &[], &[], None, 64 * 1024)?;
        match r.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(classify(&r, "deleting")),
        }
    }

    fn quota(&self, _ctx: &OpCtx) -> StorageResult<Option<Quota>> {
        Ok(None) // S3 and R2 have no per-account quota to ask for
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
