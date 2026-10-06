//! OAuth 2.0 for installed applications (Google's "Desktop app" clients).
//!
//! * [`Authenticator::login`] runs the interactive flow in the **system browser** with a loopback
//!   redirect (`http://127.0.0.1:<ephemeral>`), PKCE S256 and a random `state`
//!   (<https://developers.google.com/identity/protocols/oauth2/native-app>).
//! * The refresh token lives only in a [`CredentialStore`] (the OS keychain in production). There is
//!   no plaintext fallback.
//! * [`Authenticator`] is thread-safe: the access token is cached until shortly before it expires
//!   and refreshed on demand, single-flight (concurrent callers wait for one refresh).
//! * [`Authenticator::disconnect`] forgets local tokens only; [`Authenticator::revoke`] additionally
//!   revokes the grant at Google. Neither ever touches cloud data.
//!
//! The flow is implemented directly with `sha2` + `base64` rather than the `oauth2` crate: v5 of
//! that crate pins `ureq` 2, `base64` < 0.23 and `sha2` 0.10, and its parse errors carry raw
//! response bodies (which may contain tokens). See `docs/dev/drive-adapter.md`.

mod flow;

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use packsync_core::credentials::{CredentialError, CredentialStore};
use packsync_core::progress::OpCtx;
use packsync_core::redact::{Secret, redact};
use packsync_core::storage::{AuthProblem, StorageError, StorageResult};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use flow::{FnOpener, LoginOptions, PrintUrl, SystemBrowser, UrlOpener};

use crate::config::{DRIVE_API_BASE, DriveEndpoints, DriveSpace, HttpTimeouts, OAuthClientConfig, default_user_agent};
use crate::http::{self, JSON_LIMIT, Method, Request, Transport};
use crate::model::RawAbout;
use crate::stats::{RequestKind, StatsHandle};
use crate::util::{form_encode, now_ms_i64, truncate_utf8, with_query};

/// Supplies OAuth access tokens to [`DriveStorage`](crate::DriveStorage).
pub trait AccessTokenSource: Send + Sync {
    /// A currently valid access token, refreshing it first when needed.
    fn access_token(&self, ctx: &OpCtx) -> StorageResult<Secret>;
    /// The server rejected `token` (HTTP 401): drop it so the next call refreshes.
    fn invalidate(&self, token: &Secret);
}

/// A fixed token (tests, or applications that manage tokens themselves).
pub struct StaticTokenSource(pub Secret);

impl AccessTokenSource for StaticTokenSource {
    fn access_token(&self, ctx: &OpCtx) -> StorageResult<Secret> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        Ok(self.0.clone())
    }
    fn invalidate(&self, _token: &Secret) {}
}

/// Settings of an [`Authenticator`] besides the client itself.
#[derive(Clone)]
pub struct AuthConfig {
    /// Drive API base used for [`Authenticator::account_info`] (`about.get`).
    pub drive_api_base: String,
    /// HTTP timeouts for token requests.
    pub timeouts: HttpTimeouts,
    /// `User-Agent` header value.
    pub user_agent: String,
    /// Request counters (share with [`DriveConfig::stats`](crate::DriveConfig::stats)).
    pub stats: StatsHandle,
    /// Refresh the access token this long before it expires.
    pub expiry_skew: Duration,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field("drive_api_base", &self.drive_api_base)
            .field("expiry_skew", &self.expiry_skew)
            .finish_non_exhaustive()
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            drive_api_base: DRIVE_API_BASE.into(),
            timeouts: HttpTimeouts::default(),
            user_agent: default_user_agent(),
            stats: StatsHandle::default(),
            expiry_skew: Duration::from_secs(120),
        }
    }
}

impl AuthConfig {
    /// Settings matching a Drive endpoint set (tests point both at the mock).
    pub fn for_endpoints(endpoints: &DriveEndpoints) -> Self {
        Self { drive_api_base: endpoints.api_base.clone(), ..Self::default() }
    }
}

/// The Google account behind a grant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    /// E-mail address (from `about.get`).
    pub email: Option<String>,
    /// Display name.
    pub display_name: Option<String>,
    /// Stable per-user permission ID.
    pub permission_id: Option<String>,
}

/// Local connection state (no network involved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    /// No refresh token is stored.
    NotConnected,
    /// A refresh token is stored.
    Connected {
        /// Account e-mail recorded at sign-in, if known.
        email: Option<String>,
        /// Scopes Google granted.
        scopes: Vec<String>,
        /// Whether the granted scopes include the one this configuration needs.
        has_required_scope: bool,
    },
    /// Google rejected the stored refresh token (`invalid_grant`): the user must reconnect.
    /// Cloud data is unaffected.
    Revoked,
    /// The OS credential store cannot be used (message says what to do).
    CredentialStoreUnavailable(String),
}

/// Errors of interactive sign-in, revocation and account management.
#[derive(Debug, Error, Clone)]
pub enum AuthError {
    /// Cancelled through the [`OpCtx`].
    #[error("sign-in was cancelled")]
    Cancelled,
    /// No usable callback arrived in time (the user may have closed the browser tab).
    #[error("sign-in timed out: no answer arrived from the browser within {}s{}", .waited.as_secs(), if *.rejected_callbacks > 0 { " (callbacks with a wrong state value were rejected)" } else { "" })]
    TimedOut {
        /// How long we waited.
        waited: Duration,
        /// Callbacks rejected because their `state` did not match.
        rejected_callbacks: u32,
    },
    /// The user declined consent (`error=access_denied`) or Google reported another error.
    #[error("access was not granted in the browser ({0})")]
    Denied(String),
    /// The URL could not be opened.
    #[error("could not open the sign-in page in a browser: {0}")]
    Browser(String),
    /// The loopback listener could not be started.
    #[error("could not start the local sign-in listener: {0}")]
    Listener(String),
    /// The user unchecked the Drive permission on Google's consent screen.
    #[error("Google did not grant the Drive permission PackSync needs ({needed}); sign in again and keep that permission checked")]
    InsufficientScope {
        /// The scope that is required.
        needed: String,
        /// The scopes that were granted.
        granted: Vec<String>,
    },
    /// The token response had no refresh token.
    #[error("Google did not return a refresh token; remove PackSync's access at https://myaccount.google.com/permissions and sign in again")]
    NoRefreshToken,
    /// The token endpoint rejected the code exchange.
    #[error("the authorization could not be completed: {0}")]
    Exchange(String),
    /// The OS credential store failed (no plaintext fallback exists).
    #[error(transparent)]
    CredentialStore(#[from] CredentialError),
    /// Network / server problems.
    #[error(transparent)]
    Storage(StorageError),
}

impl AuthError {
    /// The closest [`StorageError`] (for callers that only deal with storage errors).
    pub fn to_storage_error(&self) -> StorageError {
        match self {
            AuthError::Cancelled => StorageError::Cancelled,
            AuthError::Storage(e) => e.clone(),
            AuthError::InsufficientScope { .. } => StorageError::Auth(AuthProblem::InsufficientScope),
            other => StorageError::Auth(AuthProblem::Other(other.to_string())),
        }
    }
}

impl From<StorageError> for AuthError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::Cancelled => AuthError::Cancelled,
            other => AuthError::Storage(other),
        }
    }
}

/// What the credential store holds (JSON inside the secret).
#[derive(Clone, Serialize, Deserialize)]
struct StoredGrant {
    v: u32,
    refresh_token: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    connected_ms: i64,
}

impl fmt::Debug for StoredGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredGrant").field("scopes", &self.scopes).field("email", &self.email).finish_non_exhaustive()
    }
}

struct CachedToken {
    token: Secret,
    refresh_at: Instant,
}

#[derive(Default)]
struct AuthState {
    access: Option<CachedToken>,
    grant: Option<StoredGrant>,
    loaded: bool,
    revoked: bool,
}

/// Successful token endpoint answer.
pub(crate) struct TokenGrant {
    pub access_token: Secret,
    pub expires_in: u64,
    pub refresh_token: Option<Secret>,
    pub scopes: Option<Vec<String>>,
}

/// Why a token request failed.
pub(crate) enum TokenFailure {
    InvalidGrant(String),
    Other(StorageError),
}

#[derive(Deserialize)]
struct RawToken {
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<serde_json::Value>,
    refresh_token: Option<String>,
    scope: Option<String>,
    token_type: Option<String>,
}

/// A token travels in an HTTP header: refuse anything that could break header framing.
fn plausible_token(t: &str) -> bool {
    !t.is_empty() && t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_graphic())
}

/// Thread-safe OAuth session for one client and one account.
pub struct Authenticator {
    client: OAuthClientConfig,
    scope: String,
    cfg: AuthConfig,
    creds: Arc<dyn CredentialStore>,
    cred_account: String,
    transport: Transport,
    state: Mutex<AuthState>,
    refresh_lock: Mutex<()>,
}

impl fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authenticator")
            .field("client_id", &self.client.client_id)
            .field("scope", &self.scope)
            .field("credential_store", &self.creds.describe())
            .finish_non_exhaustive()
    }
}

impl Authenticator {
    /// An authenticator for `client` that needs the scope of `space`. The refresh token is kept
    /// in `creds` under [`credential_account_for`].
    pub fn new(client: OAuthClientConfig, space: DriveSpace, cfg: AuthConfig, creds: Arc<dyn CredentialStore>) -> Self {
        let transport = Transport::new(cfg.timeouts, &cfg.user_agent, cfg.stats.clone());
        Self {
            cred_account: credential_account_for(&client.client_id),
            scope: space.scope().to_string(),
            client,
            cfg,
            creds,
            transport,
            state: Mutex::new(AuthState::default()),
            refresh_lock: Mutex::new(()),
        }
    }

    /// The OAuth client in use.
    pub fn client(&self) -> &OAuthClientConfig {
        &self.client
    }

    /// The scope this authenticator requests.
    pub fn required_scope(&self) -> &str {
        &self.scope
    }

    /// The credential-store account name the refresh token is stored under.
    pub fn credential_account(&self) -> &str {
        &self.cred_account
    }

    /// Request counters (token requests are counted as `token`).
    pub fn stats(&self) -> &StatsHandle {
        self.transport.stats()
    }

    /// Local connection status; reads the credential store, never the network.
    pub fn status(&self) -> AuthStatus {
        match self.grant() {
            Err(e) => AuthStatus::CredentialStoreUnavailable(e.to_string()),
            Ok(None) => AuthStatus::NotConnected,
            Ok(Some(g)) => {
                if self.state.lock().revoked {
                    return AuthStatus::Revoked;
                }
                let has = g.scopes.is_empty() || g.scopes.iter().any(|s| s == &self.scope);
                AuthStatus::Connected { email: g.email.clone(), scopes: g.scopes.clone(), has_required_scope: has }
            }
        }
    }

    /// Run the interactive sign-in (system browser + loopback listener + PKCE). On success the
    /// refresh token is stored in the credential store and the account is returned.
    pub fn login(&self, opener: &dyn UrlOpener, opts: &LoginOptions, ctx: &OpCtx) -> Result<AccountInfo, AuthError> {
        flow::run_login(self, opener, opts, ctx)
    }

    /// Store a refresh token obtained elsewhere (migration, headless setups, tests).
    pub fn store_refresh_token(
        &self,
        refresh_token: Secret,
        scopes: Vec<String>,
        account: Option<AccountInfo>,
    ) -> Result<(), AuthError> {
        if !plausible_token(refresh_token.expose()) {
            return Err(AuthError::Exchange("the refresh token is malformed".into()));
        }
        let account = account.unwrap_or_default();
        let grant = StoredGrant {
            v: 1,
            refresh_token: refresh_token.expose().to_string(),
            scopes,
            email: account.email,
            display_name: account.display_name,
            connected_ms: now_ms_i64(),
        };
        self.persist_grant(grant, None)
    }

    /// Forget the local tokens (credential store entry and memory). Cloud data and the grant at
    /// Google are untouched; use [`revoke`](Self::revoke) to also revoke the grant.
    pub fn disconnect(&self) -> Result<(), AuthError> {
        self.creds.delete(&self.cred_account)?;
        let mut st = self.state.lock();
        *st = AuthState { loaded: true, ..AuthState::default() };
        tracing::info!(target: "packsync_drive::oauth", "local Google credentials removed");
        Ok(())
    }

    /// Revoke the grant at Google, then forget local tokens. If Google cannot be reached the
    /// local tokens are kept (so the user can retry) and the error is returned.
    pub fn revoke(&self, ctx: &OpCtx) -> Result<(), AuthError> {
        if ctx.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let token = match self.grant()? {
            Some(g) => Some(Secret::new(g.refresh_token)),
            None => {
                let access = self.state.lock().access.as_ref().map(|c| c.token.clone());
                access
            }
        };
        let Some(token) = token else { return self.disconnect() };
        let body = form_encode(&[("token", token.expose())]);
        let req = Request::new(Method::Post, self.client.endpoints.revoke_url.clone(), RequestKind::Token)
            .header("Accept", "application/json")
            .body(body.as_bytes(), "application/x-www-form-urlencoded");
        let resp = self.transport.send(&req, None).map_err(StorageError::from)?;
        let status = http::status(&resp);
        if status == 200 {
            http::discard(resp, self.transport.stats());
        } else {
            let e = http::api_error(resp, self.transport.stats());
            // Already invalid (expired or revoked elsewhere): nothing left to revoke.
            if !(status == 400 && (e.has_reason("invalid_token") || e.has_reason("invalid_grant"))) {
                return Err(AuthError::Storage(http::classify(&e, "revoking the Google grant")));
            }
        }
        tracing::info!(target: "packsync_drive::oauth", "Google grant revoked");
        self.disconnect()
    }

    /// The signed-in account (`about.get?fields=user(...)`).
    pub fn account_info(&self, ctx: &OpCtx) -> Result<AccountInfo, AuthError> {
        let token = self.access_token(ctx)?;
        self.fetch_account(&token, ctx).map_err(AuthError::from)
    }

    fn fetch_account(&self, token: &Secret, ctx: &OpCtx) -> StorageResult<AccountInfo> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let url = with_query(
            &format!("{}/about", self.cfg.drive_api_base),
            &[("fields", "user(displayName,emailAddress,permissionId)")],
        );
        let req = Request::new(Method::Get, url, RequestKind::Other);
        let mut resp = self.transport.send(&req, Some(token))?;
        if http::status(&resp) != 200 {
            let e = http::api_error(resp, self.transport.stats());
            if e.status == 401 {
                self.invalidate(token);
            }
            return Err(http::classify(&e, "reading the account"));
        }
        let about: RawAbout = http::read_json(&mut resp, JSON_LIMIT, self.transport.stats(), "about.get")?;
        let user = about.user.unwrap_or_default();
        let clean = |s: Option<String>| s.filter(|v| v.len() <= 512 && !v.chars().any(char::is_control));
        Ok(AccountInfo {
            email: clean(user.email_address),
            display_name: clean(user.display_name),
            permission_id: clean(user.permission_id),
        })
    }

    fn grant(&self) -> Result<Option<StoredGrant>, CredentialError> {
        {
            let st = self.state.lock();
            if st.loaded {
                return Ok(st.grant.clone());
            }
        }
        let loaded = self.load_grant_from_store()?;
        let mut st = self.state.lock();
        if !st.loaded {
            st.grant = loaded;
            st.loaded = true;
        }
        Ok(st.grant.clone())
    }

    fn load_grant_from_store(&self) -> Result<Option<StoredGrant>, CredentialError> {
        let Some(secret) = self.creds.get(&self.cred_account)? else { return Ok(None) };
        match serde_json::from_str::<StoredGrant>(secret.expose()) {
            Ok(g) if plausible_token(&g.refresh_token) => Ok(Some(g)),
            _ => {
                tracing::warn!(target: "packsync_drive::oauth", "the stored Google credential is unreadable; treating the account as disconnected");
                Ok(None)
            }
        }
    }

    pub(crate) fn persist_grant(&self, grant: StoredGrant, access: Option<(Secret, u64)>) -> Result<(), AuthError> {
        let json = serde_json::to_string(&grant)
            .map_err(|_| AuthError::Exchange("could not serialise the credential".into()))?;
        self.creds.set(&self.cred_account, &Secret::new(json))?;
        let mut st = self.state.lock();
        st.grant = Some(grant);
        st.loaded = true;
        st.revoked = false;
        st.access = access.map(|(token, expires_in)| CachedToken { token, refresh_at: self.refresh_deadline(expires_in) });
        Ok(())
    }

    fn refresh_deadline(&self, expires_in: u64) -> Instant {
        let lifetime = Duration::from_secs(expires_in.clamp(1, 86_400));
        let skew = self.cfg.expiry_skew.min(lifetime / 2);
        Instant::now() + lifetime.saturating_sub(skew)
    }

    fn cached_valid(&self) -> Option<Secret> {
        let st = self.state.lock();
        st.access.as_ref().filter(|c| Instant::now() < c.refresh_at).map(|c| c.token.clone())
    }

    /// POST a form to the token endpoint.
    pub(crate) fn token_request(&self, form: &[(&str, &str)], ctx: &OpCtx) -> Result<TokenGrant, TokenFailure> {
        if ctx.is_cancelled() {
            return Err(TokenFailure::Other(StorageError::Cancelled));
        }
        let mut pairs: Vec<(&str, &str)> = form.to_vec();
        pairs.push(("client_id", &self.client.client_id));
        if let Some(s) = &self.client.client_secret {
            pairs.push(("client_secret", s.expose()));
        }
        let body = form_encode(&pairs);
        let req = Request::new(Method::Post, self.client.endpoints.token_url.clone(), RequestKind::Token)
            .header("Accept", "application/json")
            .body(body.as_bytes(), "application/x-www-form-urlencoded");
        let mut resp = self.transport.send(&req, None).map_err(|e| TokenFailure::Other(e.into()))?;
        let status = http::status(&resp);
        if status != 200 {
            let e = http::api_error(resp, self.transport.stats());
            let desc = redact(truncate_utf8(&e.message, 200)).into_owned();
            if e.has_reason("invalid_grant") {
                return Err(TokenFailure::InvalidGrant(desc));
            }
            let err = if e.has_reason("invalid_client") || e.has_reason("unauthorized_client") {
                StorageError::Auth(AuthProblem::Other(format!(
                    "Google rejected the OAuth client ({desc}); check the client ID/secret, see {}",
                    crate::config::OAUTH_SETUP_DOC
                )))
            } else if status == 429 || status >= 500 {
                http::classify(&e, "the Google token endpoint")
            } else {
                StorageError::Auth(AuthProblem::Other(format!("the Google token endpoint refused the request ({desc})")))
            };
            return Err(TokenFailure::Other(err));
        }
        // Never quote the body in errors: it contains tokens.
        let raw: RawToken = http::read_json(&mut resp, 64 * 1024, self.transport.stats(), "token response").map_err(
            |e| match e {
                StorageError::Offline(m) => TokenFailure::Other(StorageError::Offline(m)),
                _ => TokenFailure::Other(StorageError::Auth(AuthProblem::Other(
                    "the token endpoint returned a malformed response".into(),
                ))),
            },
        )?;
        let malformed =
            || TokenFailure::Other(StorageError::Auth(AuthProblem::Other("the token endpoint returned an unusable token".into())));
        let access = raw.access_token.filter(|t| plausible_token(t)).ok_or_else(malformed)?;
        if let Some(tt) = &raw.token_type
            && !tt.eq_ignore_ascii_case("bearer")
        {
            return Err(malformed());
        }
        let refresh = match raw.refresh_token {
            Some(r) if plausible_token(&r) => Some(Secret::new(r)),
            Some(_) => return Err(malformed()),
            None => None,
        };
        let expires_in = match raw.expires_in {
            Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(3600),
            Some(serde_json::Value::String(s)) => s.parse().unwrap_or(3600),
            _ => 3600,
        };
        let scopes = raw.scope.map(|s| s.split_whitespace().map(str::to_string).collect());
        Ok(TokenGrant { access_token: Secret::new(access), expires_in, refresh_token: refresh, scopes })
    }

    fn refresh(&self, ctx: &OpCtx) -> StorageResult<Secret> {
        let grant = match self.grant() {
            Ok(Some(g)) => g,
            Ok(None) => return Err(StorageError::Auth(AuthProblem::NotConnected)),
            Err(e) => return Err(credential_problem(&e)),
        };
        let mut grant = grant;
        let mut reloaded = false;
        loop {
            let rt = Secret::new(grant.refresh_token.clone());
            match self.token_request(&[("grant_type", "refresh_token"), ("refresh_token", rt.expose())], ctx) {
                Ok(t) => {
                    let mut st = self.state.lock();
                    st.revoked = false;
                    st.access = Some(CachedToken { token: t.access_token.clone(), refresh_at: self.refresh_deadline(t.expires_in) });
                    tracing::debug!(target: "packsync_drive::oauth", expires_in = t.expires_in, "access token refreshed");
                    return Ok(t.access_token);
                }
                Err(TokenFailure::InvalidGrant(desc)) => {
                    // Another process (CLI vs app) may have reconnected meanwhile: re-read once.
                    if !reloaded
                        && let Ok(Some(fresh)) = self.load_grant_from_store()
                        && fresh.refresh_token != grant.refresh_token
                    {
                        reloaded = true;
                        self.state.lock().grant = Some(fresh.clone());
                        grant = fresh;
                        continue;
                    }
                    self.state.lock().revoked = true;
                    tracing::warn!(target: "packsync_drive::oauth", detail = %desc, "Google rejected the refresh token (invalid_grant); the account must be reconnected");
                    return Err(StorageError::Auth(AuthProblem::Revoked));
                }
                Err(TokenFailure::Other(e)) => return Err(e),
            }
        }
    }
}

/// Map a credential-store failure to a storage error.
fn credential_problem(e: &CredentialError) -> StorageError {
    StorageError::Auth(AuthProblem::Other(format!("cannot read the stored Google credential: {e}")))
}

impl AccessTokenSource for Authenticator {
    fn access_token(&self, ctx: &OpCtx) -> StorageResult<Secret> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        if let Some(t) = self.cached_valid() {
            return Ok(t);
        }
        // Single flight: one thread refreshes, the others wait and reuse its token.
        let _guard = loop {
            if let Some(g) = self.refresh_lock.try_lock_for(Duration::from_millis(100)) {
                break g;
            }
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
        };
        if let Some(t) = self.cached_valid() {
            return Ok(t);
        }
        if self.state.lock().revoked {
            // Do not hammer the token endpoint with a refresh token Google already rejected,
            // unless another process stored a new one.
            match self.load_grant_from_store() {
                Ok(Some(g)) if self.state.lock().grant.as_ref().map(|c| &c.refresh_token) != Some(&g.refresh_token) => {
                    let mut st = self.state.lock();
                    st.grant = Some(g);
                    st.revoked = false;
                }
                _ => return Err(StorageError::Auth(AuthProblem::Revoked)),
            }
        }
        self.refresh(ctx)
    }

    fn invalidate(&self, token: &Secret) {
        let mut st = self.state.lock();
        if st.access.as_ref().is_some_and(|c| c.token.expose() == token.expose()) {
            st.access = None;
        }
    }
}

/// Credential-store account name for a client: `google-drive:<first 12 hex of BLAKE3(client_id)>`.
pub fn credential_account_for(client_id: &str) -> String {
    let h = packsync_core::hash::Hash::of(client_id.as_bytes()).to_hex();
    format!("google-drive:{}", &h[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_name_is_stable_and_opaque() {
        let a = credential_account_for("123-abc.apps.googleusercontent.com");
        assert_eq!(a, credential_account_for("123-abc.apps.googleusercontent.com"));
        assert!(a.starts_with("google-drive:"));
        assert_eq!(a.len(), "google-drive:".len() + 12);
        assert!(!a.contains("123-abc"));
        assert_ne!(a, credential_account_for("other"));
    }

    #[test]
    fn token_plausibility() {
        assert!(plausible_token("ya29.a0Af-_~/+="));
        assert!(!plausible_token(""));
        assert!(!plausible_token("a b"));
        assert!(!plausible_token("a\r\nX-Injected: 1"));
    }

    #[test]
    fn grant_debug_hides_the_refresh_token() {
        let g = StoredGrant {
            v: 1,
            refresh_token: "1//0secret".into(),
            scopes: vec![],
            email: None,
            display_name: None,
            connected_ms: 0,
        };
        assert!(!format!("{g:?}").contains("secret"));
    }
}
