//! Configuration: which Drive space and root folder to use, endpoints (overridable for tests),
//! HTTP timeouts, upload tuning, and the OAuth client configuration loaders.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use packsync_core::brand;
use packsync_core::redact::Secret;
use packsync_core::retry::RetryPolicy;
use packsync_core::storage::{StorageError, StorageResult};
use serde::Deserialize;
use thiserror::Error;

use crate::stats::StatsHandle;
use crate::util::{UrlRules, url_host};

/// Google's authorization endpoint for installed apps (verified 2026-10-06).
pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's token endpoint.
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Google's token revocation endpoint.
pub const GOOGLE_REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
/// Drive API v3 metadata base URL.
pub const DRIVE_API_BASE: &str = "https://www.googleapis.com/drive/v3";
/// Drive API v3 upload base URL.
pub const DRIVE_UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";
/// Non-sensitive scope: files created or opened by the app.
pub const SCOPE_DRIVE_FILE: &str = "https://www.googleapis.com/auth/drive.file";
/// Non-sensitive scope: the hidden per-app `appDataFolder`.
pub const SCOPE_DRIVE_APPDATA: &str = "https://www.googleapis.com/auth/drive.appdata";
/// MIME type of Drive folders.
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// Where the user-facing OAuth client setup is documented (written by the app layer).
pub const OAUTH_SETUP_DOC: &str = "docs/oauth-setup.md";
/// Drive's documented maximum file size (5,120 GB).
pub const DRIVE_MAX_FILE_SIZE: u64 = 5120 * 1024 * 1024 * 1024;
/// Resumable upload chunks must be multiples of this (except the last one).
pub const CHUNK_GRANULARITY: u64 = 256 * 1024;

/// Where PackSync keeps its objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DriveSpace {
    /// A visible folder in the user's My Drive, scope `drive.file` (default): users can see and
    /// delete what PackSync stores.
    #[default]
    VisibleFolder,
    /// The hidden application data folder, scope `drive.appdata`. Files there cannot be trashed
    /// (only deleted) and the user cannot browse them.
    AppData,
}

impl DriveSpace {
    /// The OAuth scope this space needs.
    pub fn scope(self) -> &'static str {
        match self {
            DriveSpace::VisibleFolder => SCOPE_DRIVE_FILE,
            DriveSpace::AppData => SCOPE_DRIVE_APPDATA,
        }
    }
    /// The Drive API `spaces` / `generateIds?space=` value.
    pub fn api_space(self) -> &'static str {
        match self {
            DriveSpace::VisibleFolder => "drive",
            DriveSpace::AppData => "appDataFolder",
        }
    }
    /// Short stable tag used in [`provider_id`](packsync_core::storage::Storage::provider_id).
    pub fn tag(self) -> &'static str {
        match self {
            DriveSpace::VisibleFolder => "drive.file",
            DriveSpace::AppData => "appdata",
        }
    }
    /// Whether the space supports moving files to the trash.
    pub fn supports_trash(self) -> bool {
        matches!(self, DriveSpace::VisibleFolder)
    }
}

/// Drive API endpoints. Production code uses [`DriveEndpoints::google`]; tests point them at the
/// mock server with [`DriveEndpoints::custom`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveEndpoints {
    /// Metadata API base, e.g. `https://www.googleapis.com/drive/v3`.
    pub api_base: String,
    /// Upload API base, e.g. `https://www.googleapis.com/upload/drive/v3`.
    pub upload_base: String,
    /// Hosts that server-provided URLs (resumable session URIs, download redirects) may use.
    /// `.example.com` matches subdomains.
    pub allowed_hosts: Vec<String>,
    /// Permit plain-HTTP endpoints and server URLs. Only for the local mock.
    pub allow_http: bool,
}

impl DriveEndpoints {
    /// The real Google endpoints. Session URIs must stay on `*.googleapis.com` over HTTPS.
    pub fn google() -> Self {
        Self {
            api_base: DRIVE_API_BASE.into(),
            upload_base: DRIVE_UPLOAD_BASE.into(),
            allowed_hosts: vec![".googleapis.com".into()],
            allow_http: false,
        }
    }
    /// Endpoints of a Drive-compatible server at `base` (`http://127.0.0.1:1234` for the mock):
    /// `{base}/drive/v3` and `{base}/upload/drive/v3`; server URLs must stay on the same host.
    pub fn custom(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            api_base: format!("{base}/drive/v3"),
            upload_base: format!("{base}/upload/drive/v3"),
            allowed_hosts: url_host(base).into_iter().collect(),
            allow_http: base.starts_with("http://"),
        }
    }
    pub(crate) fn url_rules(&self) -> UrlRules {
        UrlRules { hosts: self.allowed_hosts.clone(), allow_http: self.allow_http }
    }
}

impl Default for DriveEndpoints {
    fn default() -> Self {
        Self::google()
    }
}

/// HTTP timeouts. Body timeouts scale with the payload so that large transfers on slow links are
/// not cut off while a stalled connection is still detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpTimeouts {
    /// DNS + TCP + TLS handshake.
    pub connect: Duration,
    /// Sending request headers / waiting for response headers.
    pub response: Duration,
    /// Fixed part of the body send/receive budget.
    pub body_base: Duration,
    /// Slowest throughput still considered alive (bytes per second) for the variable part.
    pub min_bytes_per_sec: u64,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(30),
            response: Duration::from_secs(90),
            body_base: Duration::from_secs(60),
            min_bytes_per_sec: 16 * 1024,
        }
    }
}

impl HttpTimeouts {
    /// Short timeouts for tests against the local mock.
    pub fn for_tests() -> Self {
        Self {
            connect: Duration::from_secs(5),
            response: Duration::from_secs(10),
            body_base: Duration::from_secs(10),
            min_bytes_per_sec: 1024 * 1024,
        }
    }
    /// Budget for transferring a body of `bytes`.
    pub fn body_budget(&self, bytes: u64) -> Duration {
        let per = bytes.checked_div(self.min_bytes_per_sec.max(1)).unwrap_or(0);
        self.body_base.saturating_add(Duration::from_secs(per))
    }
}

/// Configuration of a [`DriveStorage`](crate::DriveStorage).
#[derive(Clone)]
pub struct DriveConfig {
    /// Visible folder (`drive.file`, default) or hidden app data (`drive.appdata`).
    pub space: DriveSpace,
    /// Name of the root folder that holds all logical keys
    /// (default [`brand::DRIVE_ROOT_FOLDER`]; the self-test uses `PackSync-Test`).
    pub root_folder_name: String,
    /// Account identity used in `provider_id()` (normally the Google account e-mail from
    /// [`Authenticator::account_info`](crate::oauth::Authenticator::account_info)).
    pub account: String,
    /// API endpoints (overridable for tests).
    pub endpoints: DriveEndpoints,
    /// `files.list` page size (1–1000, default 1000).
    pub page_size: u32,
    /// HTTP timeouts.
    pub timeouts: HttpTimeouts,
    /// Resumable upload chunk size: a multiple of 256 KiB (default 8 MiB). One chunk is held in
    /// memory at a time.
    pub chunk_size: u64,
    /// Objects up to this size use a single multipart request (default and maximum 5 MiB).
    pub multipart_threshold: u64,
    /// Provider-level retry policy (default [`RetryPolicy::network`]).
    pub retry: RetryPolicy,
    /// Never sleep longer than this because of a `Retry-After` header; a longer request is returned
    /// to the caller as [`StorageError::RateLimited`] instead.
    pub max_retry_after: Duration,
    /// How long `list` trusts cached folder IDs before re-resolving (duplicate folders created by
    /// other devices become visible after at most this long).
    pub folder_cache_ttl: Duration,
    /// How often a single download may be resumed after a dropped connection.
    pub max_download_resumes: u32,
    /// Remember every object/folder created by this instance (used by the self-test to clean up
    /// exactly what it created).
    pub track_created: bool,
    /// HTTP request counters (share one handle with the [`Authenticator`](crate::oauth::Authenticator)
    /// to include token requests).
    pub stats: StatsHandle,
    /// `User-Agent` header value.
    pub user_agent: String,
}

impl fmt::Debug for DriveConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriveConfig")
            .field("space", &self.space)
            .field("root_folder_name", &self.root_folder_name)
            .field("account", &self.account)
            .field("endpoints", &self.endpoints)
            .field("page_size", &self.page_size)
            .field("chunk_size", &self.chunk_size)
            .field("multipart_threshold", &self.multipart_threshold)
            .finish_non_exhaustive()
    }
}

/// Default `User-Agent`. Google only gzip-compresses API responses for clients whose user agent
/// contains `gzip`.
pub fn default_user_agent() -> String {
    format!("{}/{} packsync-drive (gzip)", brand::PRODUCT_NAME, env!("CARGO_PKG_VERSION"))
}

impl Default for DriveConfig {
    fn default() -> Self {
        Self {
            space: DriveSpace::VisibleFolder,
            root_folder_name: brand::DRIVE_ROOT_FOLDER.to_string(),
            account: String::new(),
            endpoints: DriveEndpoints::google(),
            page_size: 1000,
            timeouts: HttpTimeouts::default(),
            chunk_size: 8 * 1024 * 1024,
            multipart_threshold: 5 * 1024 * 1024,
            retry: RetryPolicy::network(),
            max_retry_after: Duration::from_secs(120),
            folder_cache_ttl: Duration::from_secs(60),
            max_download_resumes: 5,
            track_created: false,
            stats: StatsHandle::default(),
            user_agent: default_user_agent(),
        }
    }
}

impl DriveConfig {
    /// Default configuration for `account` in `space`.
    pub fn new(account: impl Into<String>, space: DriveSpace) -> Self {
        Self { account: account.into(), space, ..Self::default() }
    }

    /// Validate the configuration; called by [`DriveStorage::new`](crate::DriveStorage::new).
    pub fn validate(&self) -> StorageResult<()> {
        let bad = |m: String| Err(StorageError::Other(format!("invalid Drive configuration: {m}")));
        let name = self.root_folder_name.trim();
        if name.is_empty() || self.root_folder_name.len() > 255 || name == "." || name == ".." {
            return bad(format!("root folder name {:?} is not usable", self.root_folder_name));
        }
        if self.root_folder_name.chars().any(char::is_control) {
            return bad("the root folder name contains control characters".into());
        }
        if !(1..=1000).contains(&self.page_size) {
            return bad(format!("page size {} is outside 1..=1000", self.page_size));
        }
        if self.chunk_size == 0
            || !self.chunk_size.is_multiple_of(CHUNK_GRANULARITY)
            || self.chunk_size > 256 * 1024 * 1024
        {
            return bad(format!("chunk size {} must be a positive multiple of 256 KiB (max 256 MiB)", self.chunk_size));
        }
        if self.multipart_threshold > 5 * 1024 * 1024 {
            return bad("the multipart threshold cannot exceed 5 MiB".into());
        }
        if self.retry.max_attempts == 0 {
            return bad("retry.max_attempts must be at least 1".into());
        }
        if self.account.chars().any(|c| c.is_control()) {
            return bad("the account name contains control characters".into());
        }
        for (what, url) in [("api_base", &self.endpoints.api_base), ("upload_base", &self.endpoints.upload_base)] {
            let ok = url.starts_with("https://") || (self.endpoints.allow_http && url.starts_with("http://"));
            if !ok || url.parse::<ureq::http::Uri>().is_err() {
                return bad(format!("{what} must be an absolute https URL"));
            }
        }
        Ok(())
    }
}

/// OAuth endpoints (Google's by default; the mock server provides its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthEndpoints {
    /// Authorization endpoint opened in the system browser.
    pub auth_url: String,
    /// Token endpoint (code exchange and refresh).
    pub token_url: String,
    /// Revocation endpoint.
    pub revoke_url: String,
}

impl Default for OAuthEndpoints {
    fn default() -> Self {
        Self {
            auth_url: GOOGLE_AUTH_URL.into(),
            token_url: GOOGLE_TOKEN_URL.into(),
            revoke_url: GOOGLE_REVOKE_URL.into(),
        }
    }
}

/// Where an [`OAuthClientConfig`] came from (diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientConfigSource {
    /// `PACKSYNC_GOOGLE_CLIENT_ID` / `PACKSYNC_GOOGLE_CLIENT_SECRET` at run time.
    Environment,
    /// A `client_secret*.json` file downloaded from the Google Cloud console.
    JsonFile(PathBuf),
    /// Values baked into the binary at build time (release builds).
    Compiled,
    /// Constructed programmatically (tests, embedding applications).
    Explicit,
}

/// An OAuth client (a "Desktop app" client from the Google Cloud console).
#[derive(Clone)]
pub struct OAuthClientConfig {
    /// The client ID (`….apps.googleusercontent.com`).
    pub client_id: String,
    /// Optional client secret. Google issues one to Desktop clients but does not treat it as
    /// confidential; PackSync still never logs it.
    pub client_secret: Option<Secret>,
    /// Endpoints to use.
    pub endpoints: OAuthEndpoints,
    /// Provenance.
    pub source: ClientConfigSource,
}

impl fmt::Debug for OAuthClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClientConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "<redacted>"))
            .field("endpoints", &self.endpoints)
            .field("source", &self.source)
            .finish()
    }
}

/// Problems loading the OAuth client configuration.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ClientConfigError {
    /// Nothing is configured.
    #[error(
        "no Google OAuth client is configured: set PACKSYNC_GOOGLE_CLIENT_ID (and optionally \
         PACKSYNC_GOOGLE_CLIENT_SECRET) or provide the client_secret*.json file downloaded from the \
         Google Cloud console; see {OAUTH_SETUP_DOC}"
    )]
    NotConfigured,
    /// The JSON file could not be read.
    #[error("cannot read the OAuth client file {path}: {detail}")]
    Io {
        /// File path.
        path: String,
        /// OS error text.
        detail: String,
    },
    /// The JSON is not a Google client file.
    #[error("the OAuth client file is not valid: {0}; see {OAUTH_SETUP_DOC}")]
    InvalidJson(String),
    /// The file describes a non-desktop client.
    #[error(
        "the OAuth client file describes a \"{0}\" client; PackSync needs a \"Desktop app\" client \
         (an \"installed\" object); see {OAUTH_SETUP_DOC}"
    )]
    WrongClientType(String),
    /// A value is malformed.
    #[error("invalid OAuth client configuration: {0}; see {OAUTH_SETUP_DOC}")]
    Invalid(String),
}

/// Environment variable with the client ID.
pub const ENV_CLIENT_ID: &str = "PACKSYNC_GOOGLE_CLIENT_ID";
/// Environment variable with the (optional) client secret.
pub const ENV_CLIENT_SECRET: &str = "PACKSYNC_GOOGLE_CLIENT_SECRET";

/// Client ID baked in at build time (`PACKSYNC_GOOGLE_CLIENT_ID=... cargo build --release`).
const COMPILED_CLIENT_ID: Option<&str> = option_env!("PACKSYNC_GOOGLE_CLIENT_ID");
/// Client secret baked in at build time.
const COMPILED_CLIENT_SECRET: Option<&str> = option_env!("PACKSYNC_GOOGLE_CLIENT_SECRET");

#[derive(Deserialize)]
struct GoogleClientFile {
    installed: Option<InstalledClient>,
    web: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct InstalledClient {
    client_id: Option<String>,
    client_secret: Option<String>,
}

fn validate_client_id(id: &str) -> Result<(), ClientConfigError> {
    if id.is_empty() || id.len() > 256 || !id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ClientConfigError::Invalid("the client ID is empty or contains invalid characters".into()));
    }
    Ok(())
}

fn validate_secret(s: &str) -> Result<(), ClientConfigError> {
    if s.len() > 512 || !s.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ClientConfigError::Invalid("the client secret contains invalid characters".into()));
    }
    Ok(())
}

impl OAuthClientConfig {
    /// A client with Google's endpoints.
    pub fn new(client_id: impl Into<String>, client_secret: Option<Secret>) -> Result<Self, ClientConfigError> {
        let client_id = client_id.into();
        validate_client_id(&client_id)?;
        if let Some(s) = &client_secret {
            validate_secret(s.expose())?;
        }
        Ok(Self {
            client_id,
            client_secret,
            endpoints: OAuthEndpoints::default(),
            source: ClientConfigSource::Explicit,
        })
    }

    /// Use different endpoints (tests).
    pub fn with_endpoints(mut self, endpoints: OAuthEndpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Read `PACKSYNC_GOOGLE_CLIENT_ID` / `PACKSYNC_GOOGLE_CLIENT_SECRET` through `get`
    /// (pass `|k| std::env::var(k).ok()` or a test map). `Ok(None)` when the ID is unset.
    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, ClientConfigError> {
        let Some(id) = get(ENV_CLIENT_ID).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let secret = get(ENV_CLIENT_SECRET).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let mut c = Self::new(id, secret.map(Secret::new))?;
        c.source = ClientConfigSource::Environment;
        Ok(Some(c))
    }

    /// Read the process environment.
    pub fn from_env() -> Result<Option<Self>, ClientConfigError> {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    /// Parse the JSON downloaded from the Google Cloud console (an `"installed"` object).
    pub fn from_json_str(json: &str) -> Result<Self, ClientConfigError> {
        let file: GoogleClientFile = serde_json::from_str(json)
            .map_err(|e| ClientConfigError::InvalidJson(format!("not a client JSON document (line {})", e.line())))?;
        let Some(inst) = file.installed else {
            if file.web.is_some() {
                return Err(ClientConfigError::WrongClientType("Web application".into()));
            }
            return Err(ClientConfigError::InvalidJson("the \"installed\" object is missing".into()));
        };
        let id = inst.client_id.unwrap_or_default();
        let secret = inst.client_secret.filter(|s| !s.is_empty()).map(Secret::new);
        Self::new(id, secret)
    }

    /// Read and parse a `client_secret*.json` file.
    pub fn from_json_file(path: &Path) -> Result<Self, ClientConfigError> {
        let meta = std::fs::metadata(path)
            .map_err(|e| ClientConfigError::Io { path: path.display().to_string(), detail: e.to_string() })?;
        if meta.len() > 64 * 1024 {
            return Err(ClientConfigError::InvalidJson("the file is too large to be a client file".into()));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| ClientConfigError::Io { path: path.display().to_string(), detail: e.to_string() })?;
        let mut c = Self::from_json_str(&text)?;
        c.source = ClientConfigSource::JsonFile(path.to_path_buf());
        Ok(c)
    }

    /// The client baked into this build with `option_env!` (release builds), if any.
    pub fn compiled() -> Result<Option<Self>, ClientConfigError> {
        let Some(id) = COMPILED_CLIENT_ID.filter(|s| !s.is_empty()) else { return Ok(None) };
        let mut c = Self::new(id, COMPILED_CLIENT_SECRET.filter(|s| !s.is_empty()).map(Secret::new))?;
        c.source = ClientConfigSource::Compiled;
        Ok(Some(c))
    }

    /// Find a client: an explicitly given JSON file first, then the environment, then the
    /// compiled-in values. Fails with [`ClientConfigError::NotConfigured`] (pointing to
    /// `docs/oauth-setup.md`) when there is none.
    pub fn discover(json_path: Option<&Path>) -> Result<Self, ClientConfigError> {
        if let Some(p) = json_path {
            return Self::from_json_file(p);
        }
        if let Some(c) = Self::from_env()? {
            return Ok(c);
        }
        if let Some(c) = Self::compiled()? {
            return Ok(c);
        }
        Err(ClientConfigError::NotConfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        DriveConfig::default().validate().unwrap();
        let mut c = DriveConfig { chunk_size: 300 * 1024, ..DriveConfig::default() };
        assert!(c.validate().is_err());
        c.chunk_size = 512 * 1024;
        c.page_size = 0;
        assert!(c.validate().is_err());
        c.page_size = 25;
        c.root_folder_name = "  ".into();
        assert!(c.validate().is_err());
        c.root_folder_name = "Pack'Sync \\ ünï".into();
        c.validate().unwrap();
        c.endpoints = DriveEndpoints::custom("http://127.0.0.1:9");
        c.validate().unwrap();
        c.endpoints.allow_http = false;
        assert!(c.validate().is_err(), "plain http requires allow_http");
    }

    #[test]
    fn custom_endpoints_restrict_hosts() {
        let e = DriveEndpoints::custom("http://127.0.0.1:4000/");
        assert_eq!(e.api_base, "http://127.0.0.1:4000/drive/v3");
        assert_eq!(e.upload_base, "http://127.0.0.1:4000/upload/drive/v3");
        assert!(e.url_rules().allows("http://127.0.0.1:4000/upload/drive/v3/files?upload_id=a"));
        assert!(!e.url_rules().allows("http://10.0.0.1:4000/upload"));
        assert!(DriveEndpoints::google().url_rules().allows("https://www.googleapis.com/upload/drive/v3/files"));
    }

    #[test]
    fn client_json_installed_and_web() {
        let ok = r#"{"installed":{"client_id":"123-abc.apps.googleusercontent.com","project_id":"p",
            "auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token",
            "client_secret":"GOCSPX-test","redirect_uris":["http://localhost"]}}"#;
        let c = OAuthClientConfig::from_json_str(ok).unwrap();
        assert_eq!(c.client_id, "123-abc.apps.googleusercontent.com");
        assert_eq!(c.client_secret.as_ref().unwrap().expose(), "GOCSPX-test");
        assert_eq!(c.endpoints, OAuthEndpoints::default(), "file URIs are ignored; verified constants are used");
        assert!(!format!("{c:?}").contains("GOCSPX"), "Debug must not reveal the secret");

        let web = r#"{"web":{"client_id":"x","client_secret":"y"}}"#;
        assert!(matches!(OAuthClientConfig::from_json_str(web), Err(ClientConfigError::WrongClientType(_))));
        assert!(matches!(OAuthClientConfig::from_json_str("{}"), Err(ClientConfigError::InvalidJson(_))));
        let err = OAuthClientConfig::from_json_str(r#"{"installed":{"client_secret":"GOCSPX-zz"}}"#).unwrap_err();
        assert!(!err.to_string().contains("GOCSPX"));
        let err = OAuthClientConfig::from_json_str("not json GOCSPX-leak").unwrap_err();
        assert!(!err.to_string().contains("GOCSPX"), "{err}");
    }

    #[test]
    fn env_loader_and_error_message() {
        let none = OAuthClientConfig::from_env_with(|_| None).unwrap();
        assert!(none.is_none());
        let c = OAuthClientConfig::from_env_with(|k| match k {
            ENV_CLIENT_ID => Some(" id-1 ".into()),
            ENV_CLIENT_SECRET => Some(String::new()),
            _ => None,
        })
        .unwrap()
        .unwrap();
        assert_eq!(c.client_id, "id-1");
        assert!(c.client_secret.is_none());
        assert_eq!(c.source, ClientConfigSource::Environment);
        assert!(ClientConfigError::NotConfigured.to_string().contains(OAUTH_SETUP_DOC));
        assert!(OAuthClientConfig::new("has space", None).is_err());
    }

    #[test]
    fn json_file_loader() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("client_secret_test.json");
        std::fs::write(&p, r#"{"installed":{"client_id":"abc","client_secret":"s"}}"#).unwrap();
        let c = OAuthClientConfig::discover(Some(&p)).unwrap();
        assert_eq!(c.source, ClientConfigSource::JsonFile(p.clone()));
        assert!(matches!(
            OAuthClientConfig::from_json_file(&dir.path().join("missing.json")),
            Err(ClientConfigError::Io { .. })
        ));
    }

    #[test]
    fn body_budget_scales() {
        let t = HttpTimeouts::default();
        assert_eq!(t.body_budget(0), t.body_base);
        assert!(t.body_budget(16 * 1024 * 1024) > t.body_base);
    }
}
