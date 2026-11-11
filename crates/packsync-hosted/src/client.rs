//! Client of the hosted service (`services/hosted`): the object API as a [`Storage`] (unencrypted; wrap
//! it in [`crate::crypto::EncryptedStorage`]), and the account and pairing calls.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use md5::{Digest as _, Md5};
use packsync_core::hash::Hash;
use packsync_core::progress::OpCtx;
use packsync_core::redact::Secret;
use packsync_core::retry::{RetryPolicy, Sleeper, real_sleeper};
use packsync_core::storage::*;
use serde::Deserialize;
use sha2::Sha256;

/// Largest upload: the service accepts 90,000,000 bytes (Workers take request bodies up to 100 MB).
pub const MAX_PUT: u64 = 90_000_000;
const MAX_JSON: u64 = 16 * 1024 * 1024;

/// An account on the hosted service. The token is a secret.
#[derive(Clone)]
pub struct AccountCredentials {
    pub endpoint: String,
    pub account: String,
    pub token: Secret,
}

impl std::fmt::Debug for AccountCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountCredentials")
            .field("endpoint", &self.endpoint)
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub struct HostedStats {
    pub requests: AtomicU64,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
    pub retries: AtomicU64,
}

impl HostedStats {
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes_up.load(Ordering::Relaxed),
            self.bytes_down.load(Ordering::Relaxed),
            self.retries.load(Ordering::Relaxed),
        )
    }
}

pub(crate) struct Reply {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
    fn message(&self) -> String {
        #[derive(Deserialize)]
        struct E {
            #[serde(default)]
            error: String,
            #[serde(default)]
            message: String,
        }
        match serde_json::from_slice::<E>(&self.body) {
            Ok(e) if !e.message.is_empty() => format!("{} ({})", e.message, e.error),
            _ => format!("HTTP {}", self.status),
        }
    }
}

fn check_endpoint(endpoint: &str) -> StorageResult<String> {
    let e = endpoint.trim().trim_end_matches('/');
    let rest = e
        .strip_prefix("https://")
        .or_else(|| e.strip_prefix("http://"))
        .ok_or_else(|| StorageError::Other(format!("the service address {endpoint:?} must start with https://")))?;
    if rest.is_empty() || rest.contains('/') {
        return Err(StorageError::Other(format!("the service address {endpoint:?} must be a scheme and a host")));
    }
    Ok(e.to_string())
}

/// The HTTP side shared by the storage and the account calls.
pub struct Http {
    endpoint: String,
    agent: ureq::Agent,
    retry: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
    stats: Arc<HostedStats>,
}

impl Http {
    pub fn new(endpoint: &str) -> StorageResult<Http> {
        Self::with_sleeper(endpoint, real_sleeper())
    }

    pub fn with_sleeper(endpoint: &str, sleeper: Arc<dyn Sleeper>) -> StorageResult<Http> {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(format!("{}/{} packsync-hosted", packsync_core::brand::PRODUCT_NAME, env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_resolve(Some(Duration::from_secs(15)))
            .timeout_send_request(Some(Duration::from_secs(60)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .max_idle_connections_per_host(16)
            .build();
        Ok(Http {
            endpoint: check_endpoint(endpoint)?,
            agent: config.into(),
            retry: RetryPolicy::network(),
            sleeper,
            stats: Arc::new(HostedStats::default()),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn budget(bytes: u64) -> Duration {
        Duration::from_secs(60 + bytes / (64 * 1024))
    }

    /// One request, no retries.
    pub(crate) fn exchange(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Option<&[u8]>,
        max_body: u64,
    ) -> StorageResult<Reply> {
        let url = format!("{}{}", self.endpoint, path);
        let up = body.map(|b| b.len() as u64).unwrap_or(0);
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_up.fetch_add(up, Ordering::Relaxed);
        macro_rules! prep {
            ($b:expr) => {{
                let mut b = $b
                    .config()
                    .timeout_send_body(Some(Self::budget(up)))
                    .timeout_recv_body(Some(Self::budget(max_body.min(64 << 20))))
                    .build();
                for (k, v) in headers {
                    b = b.header(*k, v.as_str());
                }
                b
            }};
        }
        let res = match (method, body) {
            ("GET", _) => prep!(self.agent.get(&url)).call(),
            ("HEAD", _) => prep!(self.agent.head(&url)).call(),
            ("DELETE", _) => prep!(self.agent.delete(&url)).call(),
            ("PUT", Some(b)) => prep!(self.agent.put(&url)).send(b),
            ("PUT", None) => prep!(self.agent.put(&url)).send_empty(),
            ("POST", Some(b)) => prep!(self.agent.post(&url)).send(b),
            ("POST", None) => prep!(self.agent.post(&url)).send_empty(),
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
            resp.body_mut()
                .with_config()
                .limit(limit.saturating_add(1))
                .reader()
                .take(limit.saturating_add(1))
                .read_to_end(&mut out)
                .map_err(|e| StorageError::Offline(format!("reading the response failed: {e}")))?;
            if out.len() as u64 > limit {
                return Err(StorageError::Other(format!("the server response exceeded {limit} bytes")));
            }
        }
        self.stats.bytes_down.fetch_add(out.len() as u64, Ordering::Relaxed);
        tracing::debug!(target: "packsync_hosted::request", method, path, status, "request");
        Ok(Reply { status, headers: hs, body: out })
    }

    /// [`Http::exchange`] with retries of network failures, 5xx and 429.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn call(
        &self,
        ctx: &OpCtx,
        what: &str,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Option<&[u8]>,
        max_body: u64,
    ) -> StorageResult<Reply> {
        self.retry.run(&*self.sleeper, ctx, what, |attempt| {
            if attempt > 0 {
                self.stats.retries.fetch_add(1, Ordering::Relaxed);
            }
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let r = self.exchange(method, path, headers, body, max_body)?;
            match r.status {
                500..=599 | 429 => Err(classify(&r, what)),
                _ => Ok(r),
            }
        })
    }

    /// Create a new account. The service limits how many one network address may create per day.
    pub fn register(&self, ctx: &OpCtx) -> StorageResult<AccountCredentials> {
        #[derive(Deserialize)]
        struct Created {
            account: String,
            token: String,
        }
        let r = self.call(ctx, "creating an account", "POST", "/v1/accounts", &[], None, MAX_JSON)?;
        if r.status != 201 {
            return Err(classify(&r, "creating an account"));
        }
        let c: Created = serde_json::from_slice(&r.body)
            .map_err(|e| StorageError::Other(format!("unexpected answer from the service: {e}")))?;
        Ok(AccountCredentials { endpoint: self.endpoint.clone(), account: c.account, token: Secret::new(c.token) })
    }
}

/// Map a non-success reply to a storage error.
pub(crate) fn classify(r: &Reply, what: &str) -> StorageError {
    let detail = format!("{what}: {}", r.message());
    let retry_after = r.header("retry-after").and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs);
    match r.status {
        429 => StorageError::RateLimited { retry_after },
        503 => StorageError::RateLimited { retry_after },
        500..=599 if r.status != 507 => StorageError::Transient(detail),
        507 => StorageError::QuotaExceeded(r.message()),
        401 => StorageError::Auth(AuthProblem::Other(format!(
            "the PackSync service did not accept this computer's sign-in ({}); pair it again",
            r.message()
        ))),
        403 => StorageError::Permission(detail),
        404 => StorageError::NotFound(detail),
        409 | 412 => StorageError::Conflict(detail),
        413 => StorageError::Other(detail),
        400 if r.message().contains("bad_digest") => StorageError::Corrupt(detail),
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

/// Percent-encode a key for the URL path (keys are `[A-Za-z0-9._-/]`, so this is a safeguard).
fn path_of(key: &str) -> String {
    let mut s = String::from("/v1/objects/");
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/') {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn query_escape(v: &str) -> String {
    let mut s = String::new();
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~') {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn etag_clean(s: &str) -> String {
    s.trim().trim_start_matches("W/").trim_matches('"').to_string()
}

/// The object API of one account as a [`Storage`] (unencrypted bytes).
pub struct HostedStorage {
    http: Http,
    creds: AccountCredentials,
}

impl std::fmt::Debug for HostedStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostedStorage").field("creds", &self.creds).finish_non_exhaustive()
    }
}

impl HostedStorage {
    pub fn new(creds: AccountCredentials) -> StorageResult<HostedStorage> {
        Self::with_sleeper(creds, real_sleeper())
    }

    pub fn with_sleeper(creds: AccountCredentials, sleeper: Arc<dyn Sleeper>) -> StorageResult<HostedStorage> {
        Ok(HostedStorage { http: Http::with_sleeper(&creds.endpoint, sleeper)?, creds })
    }

    pub fn stats(&self) -> Arc<HostedStats> {
        self.http.stats.clone()
    }

    pub fn credentials(&self) -> &AccountCredentials {
        &self.creds
    }

    pub(crate) fn auth(&self) -> (&'static str, String) {
        ("authorization", format!("Bearer {}.{}", self.creds.account, self.creds.token.expose()))
    }

    pub(crate) fn http(&self) -> &Http {
        &self.http
    }

    fn info_from_headers(key: &str, r: &Reply, size: u64) -> ObjectInfo {
        let mut info = ObjectInfo::file(key, size);
        let ms = r.header("x-uploaded-ms").and_then(|v| v.parse().ok());
        info.created_ms = ms;
        info.modified_ms = ms;
        if let Some(e) = r.header("etag") {
            let e = etag_clean(e);
            if e.len() == 32 && e.bytes().all(|b| b.is_ascii_hexdigit()) {
                info.checksums.md5 = Some(e.to_ascii_lowercase());
            }
            info.version = Some(e);
        }
        info
    }

    /// Space used and allowed for this account.
    pub fn usage(&self, ctx: &OpCtx) -> StorageResult<Quota> {
        #[derive(Deserialize)]
        struct U {
            used: u64,
            quota: u64,
        }
        let r = self.http.call(ctx, "reading the space used", "GET", "/v1/usage", &[self.auth()], None, MAX_JSON)?;
        if r.status != 200 {
            return Err(classify(&r, "reading the space used"));
        }
        let u: U = serde_json::from_slice(&r.body).map_err(|e| StorageError::Other(e.to_string()))?;
        Ok(Quota { used: u.used, total: Some(u.quota) })
    }

    /// Delete the account and everything stored in it.
    pub fn delete_account(&self, ctx: &OpCtx) -> StorageResult<()> {
        let r =
            self.http.call(ctx, "deleting the account", "DELETE", "/v1/accounts", &[self.auth()], None, MAX_JSON)?;
        match r.status {
            204 | 200 => Ok(()),
            _ => Err(classify(&r, "deleting the account")),
        }
    }
}

impl Storage for HostedStorage {
    fn provider_id(&self) -> String {
        let host = self.creds.endpoint.split("://").nth(1).unwrap_or(&self.creds.endpoint);
        format!("hosted:{host}:{}", self.creds.account)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: "packsync-hosted".into(),
            unique_keys: true,
            atomic_replace: true,
            conditional_write: true,
            owned_namespace: true,
            range_reads: true,
            resumable_uploads: false,
            server_checksums: vec![ChecksumKind::Md5],
            list_consistency: Consistency::Strong,
            max_object_size: Some(MAX_PUT),
            recommended_concurrency: 8,
            recommended_pack_size: 16 * 1024 * 1024,
        }
    }

    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>> {
        validate_key(prefix)?;
        if !prefix.ends_with('/') {
            return Err(StorageError::Other(format!("list prefix {prefix:?} must end with '/'")));
        }
        #[derive(Deserialize)]
        struct Obj {
            key: String,
            size: u64,
            etag: String,
            uploaded_ms: i64,
        }
        #[derive(Deserialize)]
        struct Page {
            objects: Vec<Obj>,
            prefixes: Vec<String>,
            cursor: Option<String>,
        }
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..100_000 {
            let mut path = format!("/v1/objects?prefix={}", query_escape(prefix));
            if let Some(c) = &cursor {
                path.push_str(&format!("&cursor={}", query_escape(c)));
            }
            let r = self.http.call(ctx, "listing objects", "GET", &path, &[self.auth()], None, MAX_JSON)?;
            if r.status != 200 {
                return Err(classify(&r, "listing objects"));
            }
            let page: Page = serde_json::from_slice(&r.body)
                .map_err(|e| StorageError::Other(format!("unexpected listing from the service: {e}")))?;
            for o in page.objects {
                if validate_key(&o.key).is_err() || o.key.ends_with('/') {
                    continue;
                }
                let mut info = ObjectInfo::file(o.key, o.size);
                info.created_ms = Some(o.uploaded_ms);
                info.modified_ms = Some(o.uploaded_ms);
                let e = etag_clean(&o.etag);
                if e.len() == 32 && e.bytes().all(|b| b.is_ascii_hexdigit()) {
                    info.checksums.md5 = Some(e.to_ascii_lowercase());
                }
                info.version = Some(e);
                out.push(info);
            }
            for p in page.prefixes {
                if validate_key(&p).is_ok() {
                    let mut d = ObjectInfo::file(p, 0);
                    d.is_dir = true;
                    out.push(d);
                }
            }
            match page.cursor {
                Some(c) => cursor = Some(c),
                None => return Ok(out),
            }
        }
        Err(StorageError::Other("listing did not end".into()))
    }

    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        validate_key(key)?;
        if key.ends_with('/') {
            return Ok(None);
        }
        let r = self.http.call(ctx, "checking an object", "HEAD", &path_of(key), &[self.auth()], None, 0)?;
        match r.status {
            200 => {
                let size = r
                    .header("x-object-size")
                    .or_else(|| r.header("content-length"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                Ok(Some(Self::info_from_headers(key, &r, size)))
            }
            404 => Ok(None),
            _ => Err(classify(&r, "checking an object")),
        }
    }

    fn get(&self, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>> {
        validate_key(key)?;
        let mut headers = vec![self.auth()];
        let expect = match range {
            Some((_, 0)) => return Ok(Box::new(std::io::empty())),
            Some((s, l)) => {
                headers.push(("range", format!("bytes={s}-{}", s + l - 1)));
                Some(l)
            }
            None => None,
        };
        let r = self.http.call(ctx, "downloading", "GET", &path_of(key), &headers, None, expect.unwrap_or(MAX_PUT))?;
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

    fn get_with_info(&self, key: &str, max: u64, ctx: &OpCtx) -> StorageResult<Option<(Vec<u8>, ObjectInfo)>> {
        validate_key(key)?;
        let r = self.http.call(ctx, "downloading", "GET", &path_of(key), &[self.auth()], None, max)?;
        match r.status {
            200 => {
                let info = Self::info_from_headers(key, &r, r.body.len() as u64);
                ctx.add_down(r.body.len() as u64);
                Ok(Some((r.body, info)))
            }
            404 => Ok(None),
            _ => Err(classify(&r, "downloading")),
        }
    }

    fn put(&self, key: &str, src: PutSource<'_>, opts: &PutOptions, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        validate_key(key)?;
        if key.ends_with('/') {
            return Err(StorageError::Other(format!("cannot put directory key {key:?}")));
        }
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        if src.len() > MAX_PUT {
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
        let md5_hex = hex::encode(Md5::digest(&data));
        let mut headers = vec![
            self.auth(),
            ("x-content-sha256", hex::encode(Sha256::digest(&data))),
            ("content-type", "application/octet-stream".to_string()),
        ];
        match &opts.mode {
            PutMode::CreateOnly | PutMode::IfVersion(None) => headers.push(("if-none-match", "*".into())),
            PutMode::IfVersion(Some(v)) => headers.push(("if-match", format!("\"{v}\""))),
            PutMode::Replace => {}
        }
        let r = self.http.call(ctx, "uploading", "PUT", &path_of(key), &headers, Some(&data), MAX_JSON)?;
        match r.status {
            200 => {
                let etag = r.header("etag").map(etag_clean).unwrap_or_default();
                if !etag.eq_ignore_ascii_case(&md5_hex) {
                    return Err(StorageError::Corrupt(format!(
                        "{key}: the service stored something else (ETag {etag}, expected {md5_hex})"
                    )));
                }
                ctx.add_up(data.len() as u64);
                let mut info = Self::info_from_headers(key, &r, data.len() as u64);
                info.checksums.blake3 = opts.digest.map(|d| d.blake3.to_hex());
                info.tags = opts.tags.clone();
                Ok(info)
            }
            412 | 409 if opts.mode == PutMode::CreateOnly => match self.stat(key, ctx)? {
                Some(info)
                    if info.size == data.len() as u64 && info.checksums.md5.as_deref() == Some(md5_hex.as_str()) =>
                {
                    Ok(info)
                }
                Some(_) => Err(StorageError::Conflict(format!("{key} already exists with different content"))),
                None => Err(StorageError::Transient(format!(
                    "{key}: the precondition failed but the object is gone; retry"
                ))),
            },
            _ => Err(classify(&r, "uploading")),
        }
    }

    fn delete(&self, key: &str, _mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        validate_key(key)?;
        let r = self.http.call(ctx, "deleting", "DELETE", &path_of(key), &[self.auth()], None, MAX_JSON)?;
        match r.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(classify(&r, "deleting")),
        }
    }

    fn quota(&self, ctx: &OpCtx) -> StorageResult<Option<Quota>> {
        self.usage(ctx).map(Some)
    }
}
