//! Microsoft / Minecraft device-code sign-in for the PackSync cloud service.
//!
//! Flow:
//! 1. [`start_device_login`] fetches `GET <endpoint>/v1/auth/config`, verifies that the Microsoft
//!    endpoints point to `https://login.microsoftonline.com/` (or a loopback test server when the
//!    service endpoint itself is loopback), and starts a device authorization request (`POST
//!    <device_code_url>`).
//! 2. [`PendingDeviceLogin::wait`] polls `POST <token_url>` until the user completes sign-in in
//!    their browser (honouring `authorization_pending`, `slow_down`, expiration, and cancellation),
//!    then exchanges the returned Microsoft `id_token` at `POST <endpoint>/v1/auth/microsoft` for
//!    the PackSync account ID, per-device session token, and server-wrapped [`MasterKey`].

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use packsync_core::progress::OpCtx;
use packsync_core::redact::Secret;
use packsync_core::retry::{Sleeper, real_sleeper};
use packsync_core::storage::{AuthProblem, StorageError, StorageResult};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::client::{AccountCredentials, Http, Reply, check_endpoint, classify, net_error};
use crate::crypto::MasterKey;
use crate::{Credentials, b64};

const MAX_AUTH_JSON: u64 = 256 * 1024;
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

/// What the user needs to enter in their browser to sign in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLoginPrompt {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub privacy_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_url: Option<String>,
}

/// Result of a completed Microsoft device-code sign-in against the PackSync service.
#[derive(Clone, Debug)]
pub struct MsLoginOutcome {
    pub credentials: Credentials,
    pub display_name: Option<String>,
    pub created: bool,
    pub quota: u64,
}

/// An in-flight device-code authorization waiting for the user to sign in.
pub struct PendingDeviceLogin {
    prompt: DeviceLoginPrompt,
    endpoint: String,
    client_id: String,
    token_url: String,
    device_code: String,
    interval: Duration,
    expires_at: Instant,
    http: Http,
    sleeper: Arc<dyn Sleeper>,
}

impl Drop for PendingDeviceLogin {
    fn drop(&mut self) {
        self.device_code.zeroize();
    }
}

impl std::fmt::Debug for PendingDeviceLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingDeviceLogin")
            .field("endpoint", &self.endpoint)
            .field("user_code", &self.prompt.user_code)
            .field("verification_uri", &self.prompt.verification_uri)
            .finish_non_exhaustive()
    }
}

impl PendingDeviceLogin {
    pub fn prompt(&self) -> &DeviceLoginPrompt {
        &self.prompt
    }

    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// Poll Microsoft until sign-in finishes, fails, expires, or is cancelled, then exchange the
    /// `id_token` with the PackSync service for account credentials and the account's [`MasterKey`].
    pub fn wait(mut self, ctx: &OpCtx) -> StorageResult<MsLoginOutcome> {
        let id_token = self.poll_token(ctx)?;
        self.exchange_microsoft_token(id_token.expose(), ctx)
    }

    fn poll_token(&mut self, ctx: &OpCtx) -> StorageResult<Secret> {
        let mut transient_failures = 0u32;
        loop {
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let now = Instant::now();
            if now >= self.expires_at {
                return Err(StorageError::Auth(AuthProblem::Other(
                    "the sign-in code expired before sign-in finished; start sign-in again".into(),
                )));
            }

            let mut form = form_encode(&[
                ("grant_type", DEVICE_CODE_GRANT),
                ("client_id", &self.client_id),
                ("device_code", &self.device_code),
            ]);
            let resp = self
                .http
                .agent()
                .post(&self.token_url)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("accept", "application/json")
                .send(form.as_bytes());
            form.zeroize();

            match resp {
                Ok(mut raw_resp) => {
                    let status = raw_resp.status().as_u16();
                    let retry_after = raw_resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let mut body = read_bounded_body(raw_resp.body_mut(), MAX_AUTH_JSON)?;

                    match status {
                        200 => {
                            let parsed: Result<TokenSuccess, _> = serde_json::from_slice(&body);
                            body.zeroize();
                            let mut tok = parsed.map_err(|_| {
                                StorageError::Auth(AuthProblem::Other(
                                    "Microsoft returned an unreadable sign-in response".into(),
                                ))
                            })?;
                            let id = tok.id_token.take().filter(|s| !s.is_empty()).ok_or_else(|| {
                                StorageError::Auth(AuthProblem::Other(
                                    "Microsoft sign-in did not include an identity token".into(),
                                ))
                            })?;
                            return Ok(Secret::new(id));
                        }
                        400 => {
                            let parsed: TokenError = serde_json::from_slice(&body).unwrap_or_default();
                            body.zeroize();
                            match parsed.error.as_str() {
                                "authorization_pending" => {
                                    transient_failures = 0;
                                    if !self.sleep_bounded(self.interval, ctx) {
                                        return Err(StorageError::Cancelled);
                                    }
                                }
                                "slow_down" => {
                                    transient_failures = 0;
                                    self.interval =
                                        (self.interval + Duration::from_secs(5)).min(Duration::from_secs(60));
                                    if !self.sleep_bounded(self.interval, ctx) {
                                        return Err(StorageError::Cancelled);
                                    }
                                }
                                "expired_token" => {
                                    return Err(StorageError::Auth(AuthProblem::Other(
                                        "the sign-in code expired before sign-in finished; start sign-in again".into(),
                                    )));
                                }
                                "access_denied" | "authorization_declined" => {
                                    return Err(StorageError::Auth(AuthProblem::Other(
                                        "Microsoft sign-in was cancelled or declined".into(),
                                    )));
                                }
                                "bad_verification_code" => {
                                    return Err(StorageError::Auth(AuthProblem::Other(
                                        "the device sign-in code was rejected; start sign-in again".into(),
                                    )));
                                }
                                other => {
                                    let code = sanitize_code(other);
                                    return Err(StorageError::Auth(AuthProblem::Other(format!(
                                        "Microsoft sign-in failed ({code})"
                                    ))));
                                }
                            }
                        }
                        429 | 500..=599 => {
                            body.zeroize();
                            transient_failures += 1;
                            if transient_failures >= 6 {
                                return Err(if status == 429 {
                                    StorageError::RateLimited { retry_after }
                                } else {
                                    StorageError::Transient(format!("Microsoft sign-in service returned HTTP {status}"))
                                });
                            }
                            let wait = retry_after.unwrap_or(self.interval).max(self.interval);
                            if !self.sleep_bounded(wait, ctx) {
                                return Err(StorageError::Cancelled);
                            }
                        }
                        _ => {
                            body.zeroize();
                            return Err(StorageError::Auth(AuthProblem::Other(format!(
                                "unexpected response from Microsoft sign-in (HTTP {status})"
                            ))));
                        }
                    }
                }
                Err(e) => {
                    transient_failures += 1;
                    if transient_failures >= 3 {
                        return Err(net_error(e));
                    }
                    let wait = self.interval.min(Duration::from_secs(2));
                    if !self.sleep_bounded(wait, ctx) {
                        return Err(StorageError::Cancelled);
                    }
                }
            }
        }
    }

    fn sleep_bounded(&self, d: Duration, ctx: &OpCtx) -> bool {
        let remaining = self.expires_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return !ctx.is_cancelled();
        }
        self.sleeper.sleep(d.min(remaining), ctx)
    }

    fn exchange_microsoft_token(&self, id_token: &str, ctx: &OpCtx) -> StorageResult<MsLoginOutcome> {
        let mut req_json = serde_json::json!({ "id_token": id_token }).to_string();
        let headers = [("content-type", "application/json".to_string())];
        let reply_res = self.http.retry().run(&*self.sleeper, ctx, "signing in to PackSync cloud", |_| {
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let r =
                self.http.exchange("POST", "/v1/auth/microsoft", &headers, Some(req_json.as_bytes()), MAX_AUTH_JSON)?;
            match r.status {
                503 if r.error_code().as_deref() == Some("auth_not_configured") => {
                    Err(StorageError::Other(format!("signing in to PackSync cloud: {}", r.message())))
                }
                500..=599 => Err(classify(&r, "signing in to PackSync cloud")),
                429 => {
                    if matches!(r.error_code().as_deref(), Some("too_many_accounts" | "too_many_attempts")) {
                        Err(StorageError::Other(format!("signing in to PackSync cloud: {}", r.message())))
                    } else {
                        Err(classify(&r, "signing in to PackSync cloud"))
                    }
                }
                _ => Ok(r),
            }
        });
        req_json.zeroize();
        let mut reply = reply_res?;

        if !matches!(reply.status, 200 | 201) {
            let err = match reply.status {
                401 => StorageError::Auth(AuthProblem::Other(format!(
                    "PackSync did not accept the Microsoft sign-in ({})",
                    reply.message()
                ))),
                _ => classify(&reply, "signing in to PackSync cloud"),
            };
            reply.body.zeroize();
            return Err(err);
        }

        let parsed: Result<AuthMicrosoftResponse, _> = serde_json::from_slice(&reply.body);
        reply.body.zeroize();
        let mut auth = parsed
            .map_err(|e| StorageError::Other(format!("unexpected sign-in answer from the PackSync service: {e}")))?;
        if auth.account.is_empty() || auth.token.is_empty() {
            return Err(StorageError::Other(
                "unexpected sign-in answer from the PackSync service: missing account or token".into(),
            ));
        }

        let mut decoded = b64()
            .decode(auth.master_key.trim())
            .map_err(|_| StorageError::Other("the PackSync service returned an invalid encryption key".into()))?;
        let mut key_bytes: [u8; 32] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| StorageError::Other("the PackSync service returned a key with the wrong length".into()))?;
        decoded.zeroize();
        let key = MasterKey::from_bytes(key_bytes);
        key_bytes.zeroize();

        let creds = Credentials {
            account: AccountCredentials {
                endpoint: self.endpoint.clone(),
                account: std::mem::take(&mut auth.account),
                token: Secret::new(std::mem::take(&mut auth.token)),
            },
            key,
        };
        Ok(MsLoginOutcome {
            credentials: creds,
            display_name: auth.display_name.take().filter(|s| !s.trim().is_empty()),
            created: auth.created,
            quota: auth.quota,
        })
    }
}

/// Start Microsoft device-code sign-in against the PackSync hosted service at `endpoint`.
pub fn start_device_login(endpoint: &str, ctx: &OpCtx) -> StorageResult<PendingDeviceLogin> {
    start_device_login_with_sleeper(endpoint, real_sleeper(), ctx)
}

/// Like [`start_device_login`], with a custom [`Sleeper`] (for deterministic tests).
pub fn start_device_login_with_sleeper(
    endpoint: &str,
    sleeper: Arc<dyn Sleeper>,
    ctx: &OpCtx,
) -> StorageResult<PendingDeviceLogin> {
    if ctx.is_cancelled() {
        return Err(StorageError::Cancelled);
    }
    let endpoint = check_endpoint(endpoint)?;
    let endpoint_is_loopback = endpoint.starts_with("http://");
    let http = Http::with_sleeper(&endpoint, sleeper.clone())?;

    let cfg_reply = http.retry().run(&*sleeper, ctx, "reading sign-in configuration", |_| {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let r = http.exchange("GET", "/v1/auth/config", &[], None, MAX_AUTH_JSON)?;
        match r.status {
            503 if r.error_code().as_deref() == Some("auth_not_configured") => {
                Err(StorageError::Other(format!("reading sign-in configuration: {}", r.message())))
            }
            500..=599 | 429 => Err(classify(&r, "reading sign-in configuration")),
            _ => Ok(r),
        }
    })?;
    if cfg_reply.status != 200 {
        return Err(classify(&cfg_reply, "reading sign-in configuration"));
    }
    let cfg: AuthConfigResponse = serde_json::from_slice(&cfg_reply.body)
        .map_err(|e| StorageError::Other(format!("unexpected sign-in configuration from the service: {e}")))?;
    if cfg.microsoft_client_id.trim().is_empty() || cfg.scope.trim().is_empty() {
        return Err(StorageError::Other("the service returned an incomplete sign-in configuration".into()));
    }
    validate_auth_url(&cfg.device_code_url, endpoint_is_loopback)?;
    validate_auth_url(&cfg.token_url, endpoint_is_loopback)?;
    validate_info_url(&cfg.privacy_url, endpoint_is_loopback)?;
    if let Some(terms) = &cfg.terms_url {
        validate_info_url(terms, endpoint_is_loopback)?;
    }

    let form = form_encode(&[("client_id", cfg.microsoft_client_id.trim()), ("scope", cfg.scope.trim())]);
    let dc_reply = http.retry().run(&*sleeper, ctx, "starting Microsoft device-code sign-in", |_| {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let mut resp = http
            .agent()
            .post(&cfg.device_code_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .send(form.as_bytes())
            .map_err(net_error)?;
        let status = resp.status().as_u16();
        let headers = extract_headers(&resp);
        let body = read_bounded_body(resp.body_mut(), MAX_AUTH_JSON)?;
        let r = Reply { status, headers, body };
        match r.status {
            500..=599 | 429 => Err(classify(&r, "starting Microsoft device-code sign-in")),
            _ => Ok(r),
        }
    })?;

    if dc_reply.status != 200 {
        let mut body = dc_reply.body;
        body.zeroize();
        return Err(StorageError::Auth(AuthProblem::Other(format!(
            "could not start Microsoft sign-in (HTTP {})",
            dc_reply.status
        ))));
    }
    let mut dc_body = dc_reply.body;
    let parsed: Result<DeviceCodeResponse, _> = serde_json::from_slice(&dc_body);
    dc_body.zeroize();
    let mut dc = parsed.map_err(|e| StorageError::Other(format!("unexpected answer from Microsoft: {e}")))?;
    if dc.device_code.is_empty() || dc.user_code.is_empty() {
        return Err(StorageError::Other("Microsoft returned an incomplete device sign-in response".into()));
    }
    validate_info_url(&dc.verification_uri, endpoint_is_loopback)?;

    let expires_in = dc.expires_in.clamp(1, 3600);
    let interval = Duration::from_secs(dc.interval.clamp(1, 60));
    let prompt = DeviceLoginPrompt {
        user_code: std::mem::take(&mut dc.user_code),
        verification_uri: std::mem::take(&mut dc.verification_uri),
        expires_in,
        privacy_url: cfg.privacy_url,
        terms_url: cfg.terms_url,
    };
    Ok(PendingDeviceLogin {
        prompt,
        endpoint,
        client_id: cfg.microsoft_client_id,
        token_url: cfg.token_url,
        device_code: std::mem::take(&mut dc.device_code),
        interval,
        expires_at: Instant::now() + Duration::from_secs(expires_in),
        http,
        sleeper,
    })
}

fn validate_auth_url(url: &str, endpoint_is_loopback: bool) -> StorageResult<()> {
    let uri = url
        .parse::<ureq::http::Uri>()
        .map_err(|_| StorageError::Other(format!("invalid sign-in endpoint URL {url:?}")))?;
    let Some(auth) = uri.authority() else {
        return Err(StorageError::Other(format!("sign-in endpoint URL {url:?} has no host")));
    };
    if auth.as_str().contains('@') {
        return Err(StorageError::Other(format!("sign-in endpoint URL {url:?} must not contain userinfo")));
    }
    let host = auth.host().to_ascii_lowercase();
    match uri.scheme_str() {
        Some("https") if host == "login.microsoftonline.com" => Ok(()),
        Some("http") if endpoint_is_loopback && is_loopback_host(&host) => Ok(()),
        _ => Err(StorageError::Other(format!(
            "refusing untrusted sign-in endpoint {url:?} (expected https://login.microsoftonline.com/)"
        ))),
    }
}

fn validate_info_url(url: &str, endpoint_is_loopback: bool) -> StorageResult<()> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(StorageError::Other(format!("invalid URL {url:?}")));
    }
    let uri = url.parse::<ureq::http::Uri>().map_err(|_| StorageError::Other(format!("invalid URL {url:?}")))?;
    let Some(auth) = uri.authority() else {
        return Err(StorageError::Other(format!("URL {url:?} has no host")));
    };
    if auth.as_str().contains('@') {
        return Err(StorageError::Other(format!("URL {url:?} must not contain userinfo")));
    }
    let host = auth.host().to_ascii_lowercase();
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") if endpoint_is_loopback && is_loopback_host(&host) => Ok(()),
        _ => Err(StorageError::Other(format!("refusing non-HTTPS URL {url:?}"))),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "localhost" || host == "[::1]" || host == "::1"
}

fn extract_headers(resp: &ureq::http::Response<ureq::Body>) -> BTreeMap<String, String> {
    let mut hs = BTreeMap::new();
    for (k, v) in resp.headers() {
        if let Ok(v) = v.to_str() {
            hs.insert(k.as_str().to_ascii_lowercase(), v.to_string());
        }
    }
    hs
}

fn read_bounded_body(body: &mut ureq::Body, limit: u64) -> StorageResult<Vec<u8>> {
    let mut out = Vec::new();
    body.with_config()
        .limit(limit.saturating_add(1))
        .reader()
        .take(limit.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| StorageError::Offline(format!("reading the response failed: {e}")))?;
    if out.len() as u64 > limit {
        out.zeroize();
        return Err(StorageError::Other(format!("the server response exceeded {limit} bytes")));
    }
    Ok(out)
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 2);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX_UPPER[(b >> 4) as usize] as char);
            out.push(HEX_UPPER[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&pct_encode(k));
        out.push('=');
        out.push_str(&pct_encode(v));
    }
    out
}

fn sanitize_code(s: &str) -> String {
    let c: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(64).collect();
    if c.is_empty() { "unknown_error".into() } else { c }
}

#[derive(Deserialize)]
struct AuthConfigResponse {
    microsoft_client_id: String,
    device_code_url: String,
    token_url: String,
    scope: String,
    privacy_url: String,
    #[serde(default)]
    terms_url: Option<String>,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_url")]
    verification_uri: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

impl Drop for DeviceCodeResponse {
    fn drop(&mut self) {
        self.device_code.zeroize();
        self.user_code.zeroize();
    }
}

fn default_expires_in() -> u64 {
    900
}
fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct TokenSuccess {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

impl Drop for TokenSuccess {
    fn drop(&mut self) {
        if let Some(s) = &mut self.id_token {
            s.zeroize();
        }
        if let Some(s) = &mut self.access_token {
            s.zeroize();
        }
        if let Some(s) = &mut self.refresh_token {
            s.zeroize();
        }
    }
}

#[derive(Deserialize, Default)]
struct TokenError {
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
struct AuthMicrosoftResponse {
    account: String,
    token: String,
    master_key: String,
    #[serde(default)]
    quota: u64,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    created: bool,
}

impl Drop for AuthMicrosoftResponse {
    fn drop(&mut self) {
        self.account.zeroize();
        self.token.zeroize();
        self.master_key.zeroize();
        if let Some(s) = &mut self.display_name {
            s.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use packsync_core::retry::RecordingSleeper;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn host_validation_allows_microsoft_and_loopback_only_when_appropriate() {
        assert!(validate_auth_url("https://login.microsoftonline.com/common/oauth2/v2.0/devicecode", false).is_ok());
        assert!(validate_auth_url("https://login.microsoftonline.com/common/oauth2/v2.0/token", false).is_ok());
        assert!(validate_auth_url("http://127.0.0.1:8787/mock-ms/devicecode", true).is_ok());
        assert!(validate_auth_url("http://localhost:8787/mock-ms/token", true).is_ok());
        assert!(validate_auth_url("http://127.0.0.1:8787/mock-ms/devicecode", false).is_err());
        assert!(validate_auth_url("http://localhost:8787/mock-ms/token", false).is_err());
        assert!(validate_auth_url("http://login.microsoftonline.com/common/oauth2/v2.0/token", false).is_err());
        assert!(validate_auth_url("https://login.microsoftonline.com.evil.example/token", false).is_err());
        assert!(validate_auth_url("https://user@login.microsoftonline.com/token", false).is_err());
        assert!(validate_auth_url("https://evil.example/token", true).is_err());

        assert!(validate_info_url("https://microsoft.com/link", false).is_ok());
        assert!(validate_info_url("http://127.0.0.1:8787/privacy", true).is_ok());
        assert!(validate_info_url("http://127.0.0.1:8787/privacy", false).is_err());
        assert!(validate_info_url("javascript:alert(1)", true).is_err());
        assert!(validate_info_url("file:///etc/passwd", true).is_err());
    }

    #[test]
    fn handles_pending_slow_down_and_zeroizes_device_code_on_drop() {
        let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = srv.server_addr().to_ip().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let base_for_srv = base.clone();
        let token_calls = Arc::new(AtomicUsize::new(0));
        let token_calls_srv = token_calls.clone();

        let handle = std::thread::spawn(move || {
            for _ in 0..6 {
                let mut req = srv.recv().unwrap();
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                let (status, json) = match (req.method().as_str(), req.url()) {
                    ("GET", "/v1/auth/config") => (
                        200,
                        serde_json::json!({
                            "microsoft_client_id": "cid-1",
                            "device_code_url": format!("{base_for_srv}/ms/devicecode"),
                            "token_url": format!("{base_for_srv}/ms/token"),
                            "scope": "openid profile offline_access",
                            "privacy_url": format!("{base_for_srv}/privacy"),
                            "terms_url": format!("{base_for_srv}/terms"),
                        }),
                    ),
                    ("POST", "/ms/devicecode") => (
                        200,
                        serde_json::json!({
                            "device_code": "secret-device-code",
                            "user_code": "USER-1234",
                            "verification_uri": "https://microsoft.com/link",
                            "expires_in": 900,
                            "interval": 5,
                        }),
                    ),
                    ("POST", "/ms/token") => {
                        let n = token_calls_srv.fetch_add(1, Ordering::SeqCst);
                        match n {
                            0 => (400, serde_json::json!({ "error": "authorization_pending" })),
                            1 => (400, serde_json::json!({ "error": "slow_down" })),
                            _ => (200, serde_json::json!({ "id_token": "hdr.payload.sig" })),
                        }
                    }
                    ("POST", "/v1/auth/microsoft") => {
                        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(v["id_token"], "hdr.payload.sig");
                        (
                            201,
                            serde_json::json!({
                                "account": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
                                "token": "T".repeat(43),
                                "master_key": b64().encode([7u8; 32]),
                                "quota": 1000,
                                "display_name": "Steve",
                                "created": true,
                            }),
                        )
                    }
                    other => panic!("unexpected request {other:?}"),
                };
                let mut resp = tiny_http::Response::from_string(json.to_string()).with_status_code(status);
                resp.add_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap());
                let _ = req.respond(resp);
            }
        });

        let sleeper = Arc::new(RecordingSleeper::default());
        let pending = start_device_login_with_sleeper(&base, sleeper.clone(), &OpCtx::detached()).unwrap();
        assert_eq!(pending.prompt().user_code, "USER-1234");
        assert!(!format!("{pending:?}").contains("secret-device-code"));

        let outcome = pending.wait(&OpCtx::detached()).unwrap();
        assert!(outcome.created);
        assert_eq!(outcome.display_name.as_deref(), Some("Steve"));
        assert_eq!(outcome.credentials.account.account, "aaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(outcome.credentials.key.as_bytes(), &[7u8; 32]);
        assert_eq!(token_calls.load(Ordering::SeqCst), 3);
        assert_eq!(*sleeper.slept.lock(), vec![Duration::from_secs(5), Duration::from_secs(10)]);
        handle.join().unwrap();
    }

    #[test]
    fn wait_cancels_promptly_while_polling() {
        let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = srv.server_addr().to_ip().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let base_for_srv = base.clone();

        let handle = std::thread::spawn(move || {
            for _ in 0..3 {
                let mut req = srv.recv().unwrap();
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                let (status, json) = match (req.method().as_str(), req.url()) {
                    ("GET", "/v1/auth/config") => (
                        200,
                        serde_json::json!({
                            "microsoft_client_id": "cid-1",
                            "device_code_url": format!("{base_for_srv}/ms/devicecode"),
                            "token_url": format!("{base_for_srv}/ms/token"),
                            "scope": "openid",
                            "privacy_url": format!("{base_for_srv}/privacy"),
                        }),
                    ),
                    ("POST", "/ms/devicecode") => (
                        200,
                        serde_json::json!({
                            "device_code": "dc-to-wipe",
                            "user_code": "CODE-99",
                            "verification_uri": "https://microsoft.com/link",
                            "expires_in": 60,
                            "interval": 5,
                        }),
                    ),
                    ("POST", "/ms/token") => (400, serde_json::json!({ "error": "authorization_pending" })),
                    other => panic!("unexpected {other:?}"),
                };
                let _ = req.respond(tiny_http::Response::from_string(json.to_string()).with_status_code(status));
            }
        });

        let pending = start_device_login(&base, &OpCtx::detached()).unwrap();
        let ctx = OpCtx::detached();
        let cancel = ctx.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            cancel.cancel();
        });
        let started = Instant::now();
        let err = pending.wait(&ctx).unwrap_err();
        assert!(matches!(err, StorageError::Cancelled), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
        handle.join().unwrap();
    }
}
