//! HTTP transport: one blocking `ureq` agent (rustls, proxy settings from `ALL_PROXY` /
//! `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY`, redirects disabled), request accounting, and parsing
//! and classification of Google error responses into [`StorageError`]s.
//!
//! **Secret hygiene:** request URLs may be resumable session URIs (bearer-like secrets) and
//! request headers carry access tokens. Neither is ever formatted into a log line or an error;
//! errors only mention the caller-supplied `what` description. Everything that originates from
//! the network is additionally passed through [`redact`].

use std::io::Read;
use std::time::{Duration, SystemTime};

use packsync_core::redact::{Secret, redact};
use packsync_core::storage::{AuthProblem, StorageError, StorageResult};
use serde::de::DeserializeOwned;

use crate::config::HttpTimeouts;
use crate::stats::{RequestKind, StatsHandle};
use crate::util::{parse_retry_after, truncate_utf8};

/// Upper bound for error bodies we bother to read.
const ERROR_BODY_LIMIT: u64 = 64 * 1024;
/// Default upper bound for JSON responses (a 1000-entry list page is well under 1 MiB).
pub(crate) const JSON_LIMIT: u64 = 16 * 1024 * 1024;

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// One HTTP request. `url` may be a secret (session URI): never log it.
pub(crate) struct Request<'a> {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    /// Body bytes and content type. `None` sends no body (`GET`/`DELETE`) or an empty body.
    pub body: Option<(&'a [u8], &'a str)>,
    pub kind: RequestKind,
    /// Expected response body size, used to scale the receive timeout.
    pub expect_down: u64,
}

impl<'a> Request<'a> {
    pub fn new(method: Method, url: String, kind: RequestKind) -> Self {
        Request { method, url, headers: Vec::new(), body: None, kind, expect_down: 0 }
    }
    pub fn header(mut self, k: &'static str, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
    pub fn body(mut self, data: &'a [u8], content_type: &'a str) -> Self {
        self.body = Some((data, content_type));
        self
    }
    pub fn expect_down(mut self, n: u64) -> Self {
        self.expect_down = n;
        self
    }
}

/// A response with its body not yet consumed.
pub(crate) type Response = ureq::http::Response<ureq::Body>;

/// Network-level failure (no HTTP status was received). The message is already redacted.
#[derive(Debug, Clone)]
pub(crate) struct NetError {
    pub message: String,
}

impl From<NetError> for StorageError {
    fn from(e: NetError) -> Self {
        StorageError::Offline(e.message)
    }
}

/// Shared blocking HTTP client. Cheap to clone (the agent and the counters are shared).
#[derive(Clone)]
pub(crate) struct Transport {
    agent: ureq::Agent,
    timeouts: HttpTimeouts,
    stats: StatsHandle,
}

impl Transport {
    pub fn new(timeouts: HttpTimeouts, user_agent: &str, stats: StatsHandle) -> Self {
        // `Config::default()` (behind `config_builder`) reads the proxy environment variables.
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(user_agent)
            .timeout_resolve(Some(timeouts.connect))
            .timeout_connect(Some(timeouts.connect))
            .timeout_send_request(Some(timeouts.response))
            .timeout_recv_response(Some(timeouts.response))
            .timeout_send_body(Some(timeouts.body_base))
            .timeout_recv_body(Some(timeouts.body_base))
            .max_idle_connections_per_host(16)
            .build();
        Transport { agent: config.into(), timeouts, stats }
    }

    pub fn stats(&self) -> &StatsHandle {
        &self.stats
    }

    /// Send `req` (with an optional bearer token). Any HTTP status is returned as a response;
    /// only network-level failures are errors.
    pub fn send(&self, req: &Request<'_>, bearer: Option<&Secret>) -> Result<Response, NetError> {
        self.stats.request(req.kind);
        let up = req.body.map(|(b, _)| b.len() as u64).unwrap_or(0);
        self.stats.up(up);
        let send_budget = self.timeouts.body_budget(up);
        let recv_budget = self.timeouts.body_budget(req.expect_down);
        let auth = bearer.map(|t| format!("Bearer {}", t.expose()));

        macro_rules! prepare {
            ($b:expr) => {{
                let mut b =
                    $b.config().timeout_send_body(Some(send_budget)).timeout_recv_body(Some(recv_budget)).build();
                for (k, v) in &req.headers {
                    b = b.header(*k, v.as_str());
                }
                if let Some(a) = &auth {
                    b = b.header("Authorization", a.as_str());
                }
                b
            }};
        }

        let url = req.url.as_str();
        let result = match req.method {
            Method::Get => prepare!(self.agent.get(url)).call(),
            Method::Delete => prepare!(self.agent.delete(url)).call(),
            Method::Post | Method::Put | Method::Patch => {
                let b = match req.method {
                    Method::Post => self.agent.post(url),
                    Method::Put => self.agent.put(url),
                    _ => self.agent.patch(url),
                };
                let b = prepare!(b);
                match req.body {
                    Some((data, ct)) => b.content_type(ct).send(data),
                    None => b.send_empty(),
                }
            }
        };
        result.map_err(net_error)
    }
}

/// Map a `ureq` error to a redacted message. Never includes the URL (session URIs are secrets).
fn net_error(e: ureq::Error) -> NetError {
    let message = match &e {
        ureq::Error::Timeout(t) => format!("request timed out ({t})"),
        ureq::Error::HostNotFound => "DNS lookup failed".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::Io(io) => format!("network I/O error: {io}"),
        ureq::Error::Protocol(p) => format!("HTTP protocol error: {p}"),
        ureq::Error::BadUri(_) => "invalid request URL".to_string(),
        ureq::Error::ConnectProxyFailed(_) => "the HTTP proxy refused the connection".to_string(),
        ureq::Error::InvalidProxyUrl => "the proxy URL in the environment is invalid".to_string(),
        other => format!("network error: {other}"),
    };
    NetError { message: redact(&message).into_owned() }
}

/// Status code of a response.
pub(crate) fn status(resp: &Response) -> u16 {
    resp.status().as_u16()
}

/// A response header as UTF-8.
pub(crate) fn header(resp: &Response, name: &str) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// Read the whole body (at most `limit` bytes). Mid-body network failures map to `Offline`.
pub(crate) fn read_body(resp: &mut Response, limit: u64, stats: &StatsHandle) -> StorageResult<Vec<u8>> {
    let mut out = Vec::new();
    let reader = resp.body_mut().with_config().limit(limit.saturating_add(1)).reader();
    let r = reader.take(limit.saturating_add(1)).read_to_end(&mut out);
    stats.down(out.len() as u64);
    match r {
        Ok(_) if out.len() as u64 > limit => {
            Err(StorageError::Other(format!("the server response exceeded {limit} bytes")))
        }
        Ok(_) => Ok(out),
        Err(e) => Err(StorageError::Offline(redact(&format!("reading the response failed: {e}")).into_owned())),
    }
}

/// Read and parse a JSON body. Parse errors never quote the body.
pub(crate) fn read_json<T: DeserializeOwned>(
    resp: &mut Response,
    limit: u64,
    stats: &StatsHandle,
    what: &str,
) -> StorageResult<T> {
    let body = read_body(resp, limit, stats)?;
    serde_json::from_slice(&body).map_err(|e| {
        StorageError::Other(format!(
            "{what}: unexpected response from the server (malformed JSON at line {}, column {})",
            e.line(),
            e.column()
        ))
    })
}

/// Drain and drop the body so the connection can be reused.
pub(crate) fn discard(mut resp: Response, stats: &StatsHandle) {
    let _ = read_body(&mut resp, ERROR_BODY_LIMIT, stats);
}

/// A parsed non-success HTTP response.
#[derive(Debug, Clone, Default)]
pub(crate) struct ApiError {
    pub status: u16,
    /// `error.errors[].reason`, `error.details[].reason` and OAuth `error` codes.
    pub reasons: Vec<String>,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl ApiError {
    pub fn has_reason(&self, r: &str) -> bool {
        self.reasons.iter().any(|x| x.eq_ignore_ascii_case(r))
    }
}

/// Consume an error response and extract the Google error details (both the Drive format
/// `{"error":{"code","message","errors":[{"reason"}],"details":[{"reason"}]}}` and the OAuth format
/// `{"error":"invalid_grant","error_description":"…"}`).
pub(crate) fn api_error(mut resp: Response, stats: &StatsHandle) -> ApiError {
    let status = status(&resp);
    let retry_after = header(&resp, "retry-after").and_then(|v| parse_retry_after(&v, SystemTime::now()));
    let body = read_body(&mut resp, ERROR_BODY_LIMIT, stats).unwrap_or_default();
    let mut e = parse_error_body(&body);
    e.status = status;
    e.retry_after = retry_after;
    e
}

pub(crate) fn parse_error_body(body: &[u8]) -> ApiError {
    let mut out = ApiError::default();
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return out;
    };
    match v.get("error") {
        Some(serde_json::Value::Object(o)) => {
            if let Some(m) = o.get("message").and_then(|m| m.as_str()) {
                out.message = m.to_string();
            }
            for key in ["errors", "details"] {
                if let Some(arr) = o.get(key).and_then(|a| a.as_array()) {
                    for item in arr.iter().take(16) {
                        if let Some(r) = item.get("reason").and_then(|r| r.as_str()) {
                            out.reasons.push(r.to_string());
                        }
                    }
                }
            }
            if let Some(s) = o.get("status").and_then(|s| s.as_str()) {
                out.reasons.push(s.to_string());
            }
        }
        Some(serde_json::Value::String(code)) => {
            out.reasons.push(code.clone());
            out.message = v.get("error_description").and_then(|d| d.as_str()).unwrap_or(code).to_string();
        }
        _ => {}
    }
    out
}

const RATE_LIMIT_REASONS: &[&str] = &[
    "rateLimitExceeded",
    "userRateLimitExceeded",
    "sharingRateLimitExceeded",
    "dailyLimitExceeded",
    "quotaExceeded",
    "RATE_LIMIT_EXCEEDED",
    "RESOURCE_EXHAUSTED",
];
const STORAGE_QUOTA_REASONS: &[&str] = &[
    "storageQuotaExceeded",
    "activeItemCreationLimitExceeded",
    "teamDriveFileLimitExceeded",
    "numChildrenInNonRootLimitExceeded",
];
const SCOPE_REASONS: &[&str] = &["insufficientPermissions", "insufficientScopes", "ACCESS_TOKEN_SCOPE_INSUFFICIENT"];

/// Map a Google API error to a [`StorageError`].
pub(crate) fn classify(e: &ApiError, what: &str) -> StorageError {
    let detail = if e.message.is_empty() { "no details".to_string() } else { e.message.clone() };
    let msg = redact(&format!("{what}: {} (HTTP {})", truncate_utf8(&detail, 300), e.status)).into_owned();
    let any = |list: &[&str]| list.iter().any(|r| e.has_reason(r));
    match e.status {
        401 => StorageError::Auth(AuthProblem::Expired),
        403 if any(STORAGE_QUOTA_REASONS) => StorageError::QuotaExceeded(msg),
        403 if any(RATE_LIMIT_REASONS) => StorageError::RateLimited { retry_after: e.retry_after },
        403 if any(SCOPE_REASONS) => StorageError::Auth(AuthProblem::InsufficientScope),
        403 => StorageError::Permission(msg),
        404 | 410 => StorageError::NotFound(msg),
        409 | 412 => StorageError::Conflict(msg),
        408 => StorageError::Transient(msg),
        429 => StorageError::RateLimited { retry_after: e.retry_after },
        500..=599 => StorageError::Transient(msg),
        _ => StorageError::Other(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(status: u16, body: &str) -> ApiError {
        let mut e = parse_error_body(body.as_bytes());
        e.status = status;
        e
    }

    fn drive(status: u16, reason: &str) -> ApiError {
        err(
            status,
            &format!(
                r#"{{"error":{{"code":{status},"message":"m","errors":[{{"domain":"usageLimits","reason":"{reason}","message":"m"}}]}}}}"#
            ),
        )
    }

    #[test]
    fn classification_table() {
        use StorageError as E;
        assert!(matches!(classify(&drive(401, "authError"), "x"), E::Auth(AuthProblem::Expired)));
        for r in ["rateLimitExceeded", "userRateLimitExceeded", "sharingRateLimitExceeded", "dailyLimitExceeded"] {
            assert!(matches!(classify(&drive(403, r), "x"), E::RateLimited { .. }), "{r}");
        }
        assert!(matches!(classify(&drive(403, "storageQuotaExceeded"), "x"), E::QuotaExceeded(_)));
        assert!(matches!(classify(&drive(403, "activeItemCreationLimitExceeded"), "x"), E::QuotaExceeded(_)));
        for r in ["insufficientFilePermissions", "appNotAuthorizedToFile", "domainPolicy"] {
            assert!(matches!(classify(&drive(403, r), "x"), E::Permission(_)), "{r}");
        }
        assert!(matches!(
            classify(&drive(403, "insufficientPermissions"), "x"),
            E::Auth(AuthProblem::InsufficientScope)
        ));
        assert!(matches!(classify(&drive(404, "notFound"), "x"), E::NotFound(_)));
        assert!(matches!(classify(&drive(429, "rateLimitExceeded"), "x"), E::RateLimited { .. }));
        for s in [500, 502, 503, 504] {
            assert!(matches!(classify(&drive(s, "backendError"), "x"), E::Transient(_)));
        }
        assert!(matches!(classify(&drive(400, "badRequest"), "x"), E::Other(_)));
        assert!(!classify(&drive(403, "storageQuotaExceeded"), "x").is_retryable());
        assert!(classify(&drive(503, "backendError"), "x").is_retryable());
    }

    #[test]
    fn details_and_oauth_formats() {
        let e = err(
            403,
            r#"{"error":{"code":403,"message":"Request had insufficient authentication scopes.","status":"PERMISSION_DENIED",
               "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}"#,
        );
        assert!(matches!(classify(&e, "x"), StorageError::Auth(AuthProblem::InsufficientScope)));
        let o = err(400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#);
        assert!(o.has_reason("invalid_grant"));
        assert_eq!(o.message, "Token has been expired or revoked.");
        let garbage = err(502, "<html>Bad Gateway</html>");
        assert!(garbage.reasons.is_empty());
        assert!(matches!(classify(&garbage, "x"), StorageError::Transient(_)));
    }

    #[test]
    fn messages_are_redacted() {
        let e =
            err(500, r#"{"error":{"code":500,"message":"failed for upload_id=SECRETSESSION access_token=ya29.zzz"}}"#);
        let s = classify(&e, "upload").to_string();
        assert!(!s.contains("SECRETSESSION") && !s.contains("ya29"), "{s}");
    }
}
