//! In-memory mock of the Drive API v3 subset PackSync uses, plus Google's OAuth endpoints, with
//! deterministic fault injection. Multi-threaded (`tiny_http` workers behind a small TCP relay
//! that can cut connections).
//!
//! Emulated (documented Drive behaviour only): `files.list` (`q` subset, `spaces`, `pageSize`,
//! `pageToken`, `fields`), `files.get` (+ `alt=media` with `Range`), `files.create`
//! (metadata-only, multipart, resumable with `308`/`Range`, status queries and session expiry),
//! `files.update` (metadata PATCH, media replace), `files.delete`, `files.generateIds` (IDs must
//! come from it; re-use → `409`), `about.get`, the token endpoint (authorization code with PKCE
//! verification, refresh, `invalid_grant`), revocation and a consent-screen stand-in.
//!
//! **Undocumented behaviour is configurable, not assumed:** list visibility of new objects
//! ([`MockConfig::list_visibility_delay`]), how many bytes a cut-off chunk persists
//! ([`MockConfig::resumable_granularity`]), list page sizes ([`MockConfig::page_size_cap`]).
//! Per-app visibility under `drive.file` is *not* modelled (every object is visible).

mod query;
mod relay;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use md5::{Digest, Md5};
use packsync_core::redact::Secret;
pub use packsync_core::storage_fault::When;
use parking_lot::{Condvar, Mutex};
use serde_json::{Value, json};
use sha2::Sha256;

use self::query::{Subject, eval, parse, parse_fields, select};
use self::relay::Relay;
use crate::config::{
    CHUNK_GRANULARITY, DriveEndpoints, FOLDER_MIME, OAuthClientConfig, OAuthEndpoints, SCOPE_DRIVE_APPDATA,
    SCOPE_DRIVE_FILE,
};
use crate::oauth::UrlOpener;
use crate::util::{form_decode, form_encode, format_rfc3339_ms, now_ms, pct_decode, pkce_s256, random_urlsafe};

const ROOT_ID: &str = "0AMockMyDriveRoot";
const APPDATA_ID: &str = "0AMockAppDataRoot";

/// Endpoints of the mock, for fault matching and request counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Endpoint {
    /// Matches every endpoint (fault rules only).
    Any,
    /// `GET /drive/v3/files`.
    FilesList,
    /// `GET /drive/v3/files/{id}`.
    FilesGet,
    /// `GET /drive/v3/files/{id}?alt=media`.
    FilesGetMedia,
    /// `POST /drive/v3/files` (metadata only).
    FilesCreate,
    /// `POST /upload/drive/v3/files?uploadType=multipart`.
    UploadMultipart,
    /// Resumable session start (`POST`/`PATCH ...?uploadType=resumable`).
    ResumableInit,
    /// A resumable chunk `PUT`.
    ResumableChunk,
    /// A resumable status query (`Content-Range: bytes */N`).
    ResumableStatus,
    /// `PATCH /drive/v3/files/{id}`.
    FilesUpdate,
    /// `PATCH /upload/drive/v3/files/{id}` (multipart/media).
    FilesUpdateMedia,
    /// `DELETE /drive/v3/files/{id}`.
    FilesDelete,
    /// `GET /drive/v3/files/generateIds`.
    GenerateIds,
    /// `GET /drive/v3/about`.
    About,
    /// The consent-screen stand-in (`GET /o/oauth2/v2/auth`).
    Authorize,
    /// `POST /token`.
    Token,
    /// `POST /revoke`.
    Revoke,
    /// Anything else.
    Unknown,
}

/// An injected fault.
#[derive(Debug, Clone)]
pub enum Fault {
    /// Answer with a Drive-style error (no side effects).
    Error {
        /// HTTP status.
        status: u16,
        /// `error.errors[0].reason`.
        reason: String,
        /// Message.
        message: String,
        /// `Retry-After` header value.
        retry_after: Option<String>,
    },
    /// Cut the connection before reading the request body (nothing is applied).
    DropConnection,
    /// Read this many request-body bytes, then cut the connection. For resumable chunks the
    /// received prefix is kept, rounded down to [`MockConfig::resumable_granularity`].
    DropAfterRequestBytes(u64),
    /// Apply the request completely, then cut the connection instead of answering (ambiguous
    /// failure: "timeout after commit").
    ApplyThenDrop,
    /// Apply the request, send the headers and the first `keep` body bytes, then cut.
    TruncateResponse {
        /// Body bytes delivered before the cut.
        keep: usize,
    },
    /// Expire the addressed resumable session before handling the request (→ `404`).
    ExpireSession,
    /// Sleep before handling (slow response).
    Delay(Duration),
    /// Store the uploaded bytes with one byte flipped (the reported MD5 matches what is stored).
    CorruptStoredContent,
    /// Report a wrong `md5Checksum` for the object created/updated by this request.
    WrongMd5,
    /// Replace the `Location` header of a resumable session start (a hostile or broken server).
    BadLocation(String),
}

impl Fault {
    fn err(status: u16, reason: &str, message: &str) -> Fault {
        Fault::Error { status, reason: reason.into(), message: message.into(), retry_after: None }
    }
    /// `403 userRateLimitExceeded` (optionally with `Retry-After`).
    pub fn rate_limit_403(retry_after_secs: Option<u64>) -> Fault {
        Fault::Error {
            status: 403,
            reason: "userRateLimitExceeded".into(),
            message: "User Rate Limit Exceeded".into(),
            retry_after: retry_after_secs.map(|s| s.to_string()),
        }
    }
    /// `429 rateLimitExceeded` (optionally with `Retry-After`).
    pub fn too_many_requests(retry_after_secs: Option<u64>) -> Fault {
        Fault::Error {
            status: 429,
            reason: "rateLimitExceeded".into(),
            message: "Rate Limit Exceeded".into(),
            retry_after: retry_after_secs.map(|s| s.to_string()),
        }
    }
    /// A `5xx` `backendError`.
    pub fn server_error(status: u16) -> Fault {
        Fault::err(status, "backendError", "Backend Error")
    }
    /// `401 authError` regardless of the token.
    pub fn unauthorized() -> Fault {
        Fault::err(401, "authError", "Invalid Credentials")
    }
    /// `403 storageQuotaExceeded`.
    pub fn storage_quota_exceeded() -> Fault {
        Fault::err(403, "storageQuotaExceeded", "The user's Drive storage quota has been exceeded.")
    }
    /// `403 appNotAuthorizedToFile`.
    pub fn app_not_authorized() -> Fault {
        Fault::err(403, "appNotAuthorizedToFile", "The user has not granted the app access to the file.")
    }
}

/// A fault rule: which endpoint, when, what.
#[derive(Debug, Clone)]
pub struct FaultRule {
    /// Endpoint to match ([`Endpoint::Any`] for all).
    pub endpoint: Endpoint,
    /// When to fire (counted over matching requests).
    pub when: When,
    /// What to do.
    pub fault: Fault,
    /// Stop after this many triggers.
    pub max_triggers: Option<u32>,
    /// Only requests whose path contains this.
    pub path_contains: Option<String>,
}

impl FaultRule {
    /// A rule without limits.
    pub fn new(endpoint: Endpoint, when: When, fault: Fault) -> Self {
        Self { endpoint, when, fault, max_triggers: None, path_contains: None }
    }
    /// Fire at most `n` times.
    pub fn max(mut self, n: u32) -> Self {
        self.max_triggers = Some(n);
        self
    }
    /// Only for paths containing `s`.
    pub fn on_path(mut self, s: impl Into<String>) -> Self {
        self.path_contains = Some(s.into());
        self
    }
}

/// What the consent-screen stand-in does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consent {
    /// Grant everything requested.
    Approve,
    /// Grant only these scopes (granular consent).
    GrantScopes(Vec<String>),
    /// The user declines (`error=access_denied`).
    Deny,
    /// Redirect with a different `state` (CSRF attempt).
    WrongState,
}

/// Mock settings.
#[derive(Debug, Clone)]
pub struct MockConfig {
    /// Accepted OAuth client ID.
    pub client_id: String,
    /// Required client secret (`None` = not checked).
    pub client_secret: Option<String>,
    /// Force pages to at most this many entries regardless of `pageSize`.
    pub page_size_cap: Option<u32>,
    /// New objects stay invisible to this many matching `files.list` requests (eventual
    /// consistency; Google documents nothing here). `files.get` always sees them.
    pub list_visibility_delay: u32,
    /// Bytes persisted from a cut-off chunk are rounded down to this.
    pub resumable_granularity: u64,
    /// Access-token lifetime.
    pub access_token_ttl: Duration,
    /// Seed for probabilistic faults.
    pub seed: u64,
    /// Storage limit in bytes (`None` = unlimited).
    pub quota_limit: Option<u64>,
    /// Worker threads.
    pub workers: usize,
    /// Account e-mail reported by `about.get`.
    pub email: String,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            client_id: "mock-client.apps.googleusercontent.com".into(),
            client_secret: Some("mock-client-secret-0123".into()),
            page_size_cap: None,
            list_visibility_delay: 0,
            resumable_granularity: CHUNK_GRANULARITY,
            access_token_ttl: Duration::from_secs(3599),
            seed: 7,
            quota_limit: Some(15 * 1024 * 1024 * 1024),
            workers: 16,
            email: "mock.user@example.com".into(),
        }
    }
}

/// One handled request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// Endpoint.
    pub endpoint: Endpoint,
    /// HTTP method.
    pub method: String,
    /// Path without query.
    pub path: String,
    /// Status sent (`0` when the connection was cut instead).
    pub status: u16,
    /// `Content-Range` request header (uploads).
    pub content_range: Option<String>,
    /// Request body bytes received.
    pub body_len: u64,
    /// Extra detail (`grant_type` of token requests).
    pub detail: Option<String>,
}

/// Snapshot of a stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockObject {
    /// File ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Parent IDs.
    pub parents: Vec<String>,
    /// Whether it is a folder.
    pub is_folder: bool,
    /// Content size.
    pub size: u64,
    /// MD5 of the stored content.
    pub md5: String,
    /// Explicitly trashed.
    pub trashed: bool,
    /// `appProperties`.
    pub app_properties: BTreeMap<String, String>,
    /// Creation time (ms).
    pub created_ms: i64,
}

#[derive(Clone)]
struct MockFile {
    id: String,
    name: String,
    mime: String,
    parents: Vec<String>,
    props: BTreeMap<String, String>,
    content: Arc<Vec<u8>>,
    md5: String,
    sha256: String,
    created_ms: i64,
    modified_ms: i64,
    trashed: bool,
    appdata: bool,
    list_hidden: u32,
    wrong_md5: bool,
}

impl MockFile {
    fn is_folder(&self) -> bool {
        self.mime == FOLDER_MIME
    }
    fn set_content(&mut self, content: Vec<u8>) {
        self.md5 = hex::encode(Md5::digest(&content));
        self.sha256 = hex::encode(Sha256::digest(&content));
        self.content = Arc::new(content);
    }
}

#[derive(Clone)]
enum SessionTarget {
    Create(Value),
    Update { file_id: String, meta: Value },
}

struct Session {
    target: SessionTarget,
    total: Option<u64>,
    data: Vec<u8>,
    expired: bool,
    done: Option<String>,
    fields: Option<String>,
    corrupt: bool,
    wrong_md5: bool,
}

struct AccessTok {
    scopes: Vec<String>,
    expires_ms: i64,
    refresh: Option<String>,
    revoked: bool,
}

struct RefreshTok {
    scopes: Vec<String>,
    revoked: bool,
}

struct AuthCode {
    redirect_uri: String,
    challenge: String,
    scopes: Vec<String>,
    used: bool,
}

struct RuleState {
    rule: FaultRule,
    matches: u32,
    triggers: u32,
}

#[derive(Default)]
struct State {
    files: BTreeMap<String, MockFile>,
    generated: HashMap<String, bool>,
    used_ids: HashSet<String>,
    sessions: HashMap<String, Session>,
    access: HashMap<String, AccessTok>,
    refresh: HashMap<String, RefreshTok>,
    codes: HashMap<String, AuthCode>,
    rules: Vec<RuleState>,
    triggered: usize,
    clock_ms: i64,
    log: Vec<RequestRecord>,
    secrets: Vec<String>,
    consent: Option<Consent>,
}

struct Shared {
    cfg: MockConfig,
    state: Mutex<State>,
    rng: Mutex<fastrand::Rng>,
    relay: Arc<Relay>,
    base: String,
    stop: AtomicBool,
}

/// The running mock server. Dropping it stops the server.
pub struct MockDrive {
    shared: Arc<Shared>,
    server: Arc<tiny_http::Server>,
    workers: Vec<JoinHandle<()>>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn json(status: u16, v: &Value) -> Resp {
        Resp {
            status,
            headers: vec![("Content-Type".into(), "application/json; charset=UTF-8".into())],
            body: serde_json::to_vec(v).unwrap_or_default(),
        }
    }
    fn empty(status: u16) -> Resp {
        Resp { status, headers: vec![], body: vec![] }
    }
    fn header(mut self, k: &str, v: impl Into<String>) -> Resp {
        self.headers.push((k.into(), v.into()));
        self
    }
    fn drive_error(status: u16, reason: &str, message: &str) -> Resp {
        let domain =
            if reason.contains("RateLimit") || reason == "rateLimitExceeded" { "usageLimits" } else { "global" };
        Resp::json(
            status,
            &json!({"error": {"code": status, "message": message, "errors": [{"domain": domain, "reason": reason, "message": message}]}}),
        )
    }
    fn oauth_error(status: u16, code: &str, desc: &str) -> Resp {
        Resp::json(status, &json!({"error": code, "error_description": desc}))
    }
    fn unauthorized() -> Resp {
        Resp::drive_error(401, "authError", "Invalid Credentials")
            .header("WWW-Authenticate", "Bearer realm=\"https://accounts.google.com/\", error=\"invalid_token\"")
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        302 => "Found",
        308 => "Resume Incomplete",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn file_json(st: &State, f: &MockFile) -> Value {
    let mut v = json!({
        "kind": "drive#file",
        "id": f.id,
        "name": f.name,
        "mimeType": f.mime,
        "parents": f.parents,
        "trashed": effective_trashed(st, f),
        "explicitlyTrashed": f.trashed,
        "createdTime": format_rfc3339_ms(f.created_ms),
        "modifiedTime": format_rfc3339_ms(f.modified_ms),
        "appProperties": f.props,
        "spaces": [if f.appdata { "appDataFolder" } else { "drive" }],
    });
    if !f.is_folder() {
        v["size"] = Value::String(f.content.len().to_string());
        v["md5Checksum"] =
            Value::String(if f.wrong_md5 { "00000000000000000000000000000000".to_string() } else { f.md5.clone() });
        v["sha256Checksum"] = Value::String(f.sha256.clone());
    }
    v
}

fn effective_trashed(st: &State, f: &MockFile) -> bool {
    let mut cur = f;
    for _ in 0..64 {
        if cur.trashed {
            return true;
        }
        match cur.parents.first().and_then(|p| st.files.get(p)) {
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}

fn fields_of(params: &HashMap<String, String>, default: &str) -> Result<query::Fields, Resp> {
    let spec = params.get("fields").map(String::as_str).unwrap_or(default);
    parse_fields(spec).map_err(|e| Resp::drive_error(400, "invalidParameter", &format!("Invalid field selection: {e}")))
}

fn parse_multipart(content_type: &str, body: &[u8]) -> Option<(Value, Vec<u8>)> {
    let boundary = content_type.split(';').find_map(|p| p.trim().strip_prefix("boundary="))?.trim_matches('"');
    let delim = format!("--{boundary}");
    let d = delim.as_bytes();
    let find = |hay: &[u8], from: usize| hay[from..].windows(d.len()).position(|w| w == d).map(|p| p + from);
    let first = find(body, 0)?;
    let second = find(body, first + d.len())?;
    let third = find(body, second + d.len())?;
    let part = |a: usize, b: usize| -> Option<&[u8]> {
        let seg = &body[a + d.len()..b];
        let start = seg.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let end = seg.len().checked_sub(2)?;
        (start <= end && &seg[end..] == b"\r\n").then(|| &seg[start..end])
    };
    let meta: Value = serde_json::from_slice(part(first, second)?).ok()?;
    let media = part(second, third)?.to_vec();
    (body.get(third + d.len()..third + d.len() + 2) == Some(b"--")).then_some((meta, media))
}

impl Shared {
    fn rand(&self, prefix: &str) -> String {
        let r = random_urlsafe(24).unwrap_or_else(|_| format!("{:x}", self.rng.lock().u64(..)));
        format!("{prefix}{r}")
    }

    fn new_id(&self) -> String {
        // Drive-like: 33 URL-safe characters starting with "1".
        let mut rng = self.rng.lock();
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::from("1");
        for _ in 0..32 {
            s.push(alphabet[rng.usize(..alphabet.len())] as char);
        }
        s
    }

    fn alias(&self, id: &str) -> String {
        match id {
            "root" => ROOT_ID.into(),
            "appDataFolder" => APPDATA_ID.into(),
            other => other.into(),
        }
    }

    fn pick_faults(&self, st: &mut State, ep: Endpoint, path: &str) -> Vec<Fault> {
        let mut out = Vec::new();
        for rs in st.rules.iter_mut() {
            if rs.rule.endpoint != Endpoint::Any && rs.rule.endpoint != ep {
                continue;
            }
            if rs.rule.path_contains.as_deref().is_some_and(|p| !path.contains(p)) {
                continue;
            }
            rs.matches += 1;
            if rs.rule.max_triggers.is_some_and(|m| rs.triggers >= m) {
                continue;
            }
            let fire = match rs.rule.when {
                When::Nth(n) => rs.matches == n,
                When::EveryNth(n) => n > 0 && rs.matches % n == 0,
                When::FromNth(n) => rs.matches >= n,
                When::Always => true,
                When::Probability(p) => self.rng.lock().f64() < p,
            };
            if fire {
                rs.triggers += 1;
                out.push(rs.rule.fault.clone());
            }
        }
        st.triggered += out.len();
        out
    }

    fn usage(st: &State) -> u64 {
        st.files.values().map(|f| f.content.len() as u64).sum()
    }

    fn check_quota(&self, st: &State, extra: u64) -> Result<(), Resp> {
        match self.cfg.quota_limit {
            Some(limit) if Self::usage(st) + extra > limit => {
                Err(Resp::drive_error(403, "storageQuotaExceeded", "The user's Drive storage quota has been exceeded."))
            }
            _ => Ok(()),
        }
    }

    fn bearer_scopes(&self, st: &State, headers: &HashMap<String, String>) -> Result<Vec<String>, Resp> {
        let tok =
            headers.get("authorization").and_then(|v| v.strip_prefix("Bearer ")).ok_or_else(Resp::unauthorized)?;
        match st.access.get(tok) {
            Some(a) if !a.revoked && a.expires_ms > now_ms() => Ok(a.scopes.clone()),
            _ => Err(Resp::unauthorized()),
        }
    }

    fn scope_ok(scopes: &[String], appdata: bool) -> Result<(), Resp> {
        let need = if appdata { SCOPE_DRIVE_APPDATA } else { SCOPE_DRIVE_FILE };
        if scopes.iter().any(|s| s == need || s == "https://www.googleapis.com/auth/drive") {
            Ok(())
        } else {
            Err(Resp::json(
                403,
                &json!({"error": {"code": 403, "message": "Request had insufficient authentication scopes.",
                    "errors": [{"domain": "global", "reason": "insufficientPermissions", "message": "Insufficient Permission"}],
                    "status": "PERMISSION_DENIED",
                    "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}),
            ))
        }
    }

    /// Validate creation metadata; returns (id, parent, appdata).
    fn prepare_create(&self, st: &mut State, meta: &Value, scopes: &[String]) -> Result<(String, String, bool), Resp> {
        let parent = match meta.get("parents").and_then(|p| p.as_array()) {
            Some(arr) if arr.len() > 1 => {
                return Err(Resp::drive_error(400, "invalid", "A file can only have one parent folder."));
            }
            Some(arr) if !arr.is_empty() => self.alias(arr[0].as_str().unwrap_or("")),
            _ => ROOT_ID.to_string(),
        };
        let appdata = if parent == ROOT_ID {
            false
        } else if parent == APPDATA_ID {
            true
        } else {
            match st.files.get(&parent) {
                Some(p) if p.is_folder() => p.appdata,
                _ => return Err(Resp::drive_error(404, "notFound", &format!("File not found: {parent}."))),
            }
        };
        Self::scope_ok(scopes, appdata)?;
        let id = match meta.get("id").and_then(|v| v.as_str()) {
            Some(id) => {
                if st.used_ids.contains(id) {
                    return Err(Resp::drive_error(409, "conflict", "A file already exists with the provided ID."));
                }
                match st.generated.get(id) {
                    Some(sp) if *sp == appdata => id.to_string(),
                    _ => return Err(Resp::drive_error(400, "invalid", "The provided file ID is not usable.")),
                }
            }
            None => self.new_id(),
        };
        Ok((id, parent, appdata))
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_file(
        &self,
        st: &mut State,
        id: String,
        parent: String,
        appdata: bool,
        meta: &Value,
        content: Vec<u8>,
        wrong_md5: bool,
    ) -> String {
        st.clock_ms += 1;
        let now = st.clock_ms;
        let props: BTreeMap<String, String> = meta
            .get("appProperties")
            .and_then(|p| p.as_object())
            .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_default();
        let mut f = MockFile {
            id: id.clone(),
            name: meta.get("name").and_then(|n| n.as_str()).unwrap_or("Untitled").to_string(),
            mime: meta.get("mimeType").and_then(|m| m.as_str()).unwrap_or("application/octet-stream").to_string(),
            parents: vec![parent],
            props,
            content: Arc::new(Vec::new()),
            md5: String::new(),
            sha256: String::new(),
            created_ms: now,
            modified_ms: now,
            trashed: false,
            appdata,
            list_hidden: self.cfg.list_visibility_delay,
            wrong_md5,
        };
        f.set_content(content);
        st.used_ids.insert(id.clone());
        st.generated.remove(&id);
        st.files.insert(id.clone(), f);
        id
    }

    fn respond_file(&self, st: &State, id: &str, status: u16, params: &HashMap<String, String>) -> Resp {
        let Some(f) = st.files.get(id) else { return Resp::drive_error(404, "notFound", "File not found") };
        match fields_of(params, "kind,id,name,mimeType") {
            Ok(fields) => Resp::json(status, &select(&file_json(st, f), &fields)),
            Err(r) => r,
        }
    }

    fn list(&self, st: &mut State, params: &HashMap<String, String>, scopes: &[String]) -> Resp {
        let expr = match params.get("q").filter(|q| !q.trim().is_empty()) {
            Some(q) => match parse(q) {
                Ok(e) => Some(e),
                Err(e) => return Resp::drive_error(400, "invalid", &format!("Invalid Value: {e}")),
            },
            None => None,
        };
        let spaces = params.get("spaces").map(String::as_str).unwrap_or("drive");
        let want_drive = spaces.split(',').any(|s| s.trim() == "drive");
        let want_appdata = spaces.split(',').any(|s| s.trim() == "appDataFolder");
        if !want_drive && !want_appdata {
            return Resp::drive_error(400, "invalid", "Invalid Value: spaces");
        }
        for (wanted, appdata) in [(want_drive, false), (want_appdata, true)] {
            if wanted && let Err(r) = Self::scope_ok(scopes, appdata) {
                return r;
            }
        }
        let mut page_size = params.get("pageSize").and_then(|p| p.parse::<u32>().ok()).unwrap_or(100).clamp(1, 1000);
        if let Some(cap) = self.cfg.page_size_cap {
            page_size = page_size.min(cap.max(1));
        }
        let offset = match params.get("pageToken") {
            None => 0usize,
            Some(t) => match t.strip_prefix("pt-").and_then(|n| n.parse::<usize>().ok()) {
                Some(n) => n,
                None => return Resp::drive_error(400, "invalid", "Invalid Value: pageToken"),
            },
        };
        let alias = |id: &str| self.alias(id);
        let mut matched: Vec<String> = Vec::new();
        let mut hidden: Vec<String> = Vec::new();
        for f in st.files.values() {
            if !((f.appdata && want_appdata) || (!f.appdata && want_drive)) {
                continue;
            }
            let subject = Subject {
                name: &f.name,
                mime: &f.mime,
                parents: &f.parents,
                trashed: effective_trashed(st, f),
                app_props: &f.props,
            };
            if expr.as_ref().is_none_or(|e| eval(e, &subject, &alias)) {
                if f.list_hidden > 0 { hidden.push(f.id.clone()) } else { matched.push(f.id.clone()) }
            }
        }
        if offset == 0 {
            for id in hidden {
                if let Some(f) = st.files.get_mut(&id) {
                    f.list_hidden -= 1;
                }
            }
        }
        let end = (offset + page_size as usize).min(matched.len());
        let page: Vec<Value> = matched
            .get(offset.min(matched.len())..end)
            .unwrap_or(&[])
            .iter()
            .filter_map(|id| st.files.get(id).map(|f| file_json(st, f)))
            .collect();
        let mut v = json!({"kind": "drive#fileList", "incompleteSearch": false, "files": page});
        if end < matched.len() {
            v["nextPageToken"] = Value::String(format!("pt-{end}"));
        }
        match fields_of(params, "kind,incompleteSearch,nextPageToken,files(kind,id,name,mimeType)") {
            Ok(fields) => Resp::json(200, &select(&v, &fields)),
            Err(r) => r,
        }
    }

    fn get_media(&self, st: &State, id: &str, headers: &HashMap<String, String>) -> Resp {
        let Some(f) = st.files.get(id) else {
            return Resp::drive_error(404, "notFound", &format!("File not found: {id}."));
        };
        if f.is_folder() {
            return Resp::drive_error(403, "fileNotDownloadable", "Only files with binary content can be downloaded.");
        }
        let len = f.content.len() as u64;
        let Some(range) = headers.get("range") else {
            return Resp {
                status: 200,
                headers: vec![("Content-Type".into(), f.mime.clone())],
                body: f.content.to_vec(),
            };
        };
        let parsed = range.strip_prefix("bytes=").and_then(|r| r.split_once('-')).and_then(|(a, b)| {
            let a: u64 = a.trim().parse().ok()?;
            let b: u64 = if b.trim().is_empty() { len.checked_sub(1)? } else { b.trim().parse().ok()? };
            Some((a, b.min(len.saturating_sub(1))))
        });
        match parsed {
            Some((a, b)) if a <= b && a < len => Resp {
                status: 206,
                headers: vec![
                    ("Content-Type".into(), f.mime.clone()),
                    ("Content-Range".into(), format!("bytes {a}-{b}/{len}")),
                ],
                body: f.content[a as usize..=b as usize].to_vec(),
            },
            _ => Resp::empty(416).header("Content-Range", format!("bytes */{len}")),
        }
    }

    fn apply_meta_update(&self, st: &mut State, id: &str, meta: &Value) -> Result<(), Resp> {
        let Some(f) = st.files.get_mut(id) else {
            return Err(Resp::drive_error(404, "notFound", &format!("File not found: {id}.")));
        };
        if let Some(t) = meta.get("trashed").and_then(|t| t.as_bool()) {
            if f.appdata && t {
                return Err(Resp::drive_error(
                    403,
                    "appDataFolderTrash",
                    "Files in the appDataFolder cannot be trashed.",
                ));
            }
            f.trashed = t;
        }
        if let Some(n) = meta.get("name").and_then(|n| n.as_str()) {
            f.name = n.to_string();
        }
        if let Some(props) = meta.get("appProperties").and_then(|p| p.as_object()) {
            for (k, v) in props {
                match v.as_str() {
                    Some(s) => {
                        f.props.insert(k.clone(), s.to_string());
                    }
                    None => {
                        f.props.remove(k);
                    }
                }
            }
        }
        st.clock_ms += 1;
        if let Some(f) = st.files.get_mut(id) {
            f.modified_ms = st.clock_ms;
        }
        Ok(())
    }

    fn replace_content(&self, st: &mut State, id: &str, content: Vec<u8>, wrong_md5: bool) -> Result<(), Resp> {
        let old = st
            .files
            .get(id)
            .map(|f| f.content.len() as u64)
            .ok_or_else(|| Resp::drive_error(404, "notFound", "File not found"))?;
        self.check_quota(st, (content.len() as u64).saturating_sub(old))?;
        st.clock_ms += 1;
        let now = st.clock_ms;
        if let Some(f) = st.files.get_mut(id) {
            f.set_content(content);
            f.modified_ms = now;
            f.wrong_md5 = wrong_md5;
        }
        Ok(())
    }

    fn delete_tree(st: &mut State, id: &str) {
        let children: Vec<String> =
            st.files.values().filter(|f| f.parents.iter().any(|p| p == id)).map(|f| f.id.clone()).collect();
        for c in children {
            Self::delete_tree(st, &c);
        }
        st.files.remove(id);
    }

    fn finish_session(&self, st: &mut State, upload_id: &str, params: &HashMap<String, String>) -> Resp {
        let Some(s) = st.sessions.get(upload_id) else {
            return Resp::drive_error(404, "notFound", "Upload session not found");
        };
        let mut data = s.data.clone();
        if s.corrupt && !data.is_empty() {
            data[0] ^= 0x01;
        }
        let (target, wrong, fields) = (s.target.clone(), s.wrong_md5, s.fields.clone());
        let mut p = params.clone();
        if let Some(f) = fields {
            p.insert("fields".into(), f);
        }
        let (id, status) = match target {
            SessionTarget::Create(meta) => {
                if let Err(r) = self.check_quota(st, data.len() as u64) {
                    return r;
                }
                let id = meta.get("id").and_then(|v| v.as_str()).map(str::to_string);
                if id.as_ref().is_some_and(|i| st.used_ids.contains(i)) {
                    return Resp::drive_error(409, "conflict", "A file already exists with the provided ID.");
                }
                let parent = self.alias(meta["parents"].get(0).and_then(|v| v.as_str()).unwrap_or("root"));
                let appdata = parent == APPDATA_ID || st.files.get(&parent).is_some_and(|f| f.appdata);
                let id = id.unwrap_or_else(|| self.new_id());
                (self.insert_file(st, id, parent, appdata, &meta, data, wrong), 200)
            }
            SessionTarget::Update { file_id, meta } => {
                if let Err(r) = self.replace_content(st, &file_id, data, wrong) {
                    return r;
                }
                if let Err(r) = self.apply_meta_update(st, &file_id, &meta) {
                    return r;
                }
                (file_id, 200)
            }
        };
        if let Some(s) = st.sessions.get_mut(upload_id) {
            s.done = Some(id.clone());
        }
        self.respond_file(st, &id, status, &p)
    }

    fn resumable_put(
        &self,
        st: &mut State,
        params: &HashMap<String, String>,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> Resp {
        let Some(upload_id) = params.get("upload_id").cloned() else {
            return Resp::drive_error(400, "invalid", "Missing upload_id");
        };
        let Some(s) = st.sessions.get_mut(&upload_id).filter(|s| !s.expired) else {
            return Resp::drive_error(404, "notFound", "Upload session expired or not found");
        };
        let cr = headers.get("content-range").cloned().unwrap_or_default();
        let Some(spec) = cr.trim().strip_prefix("bytes ") else {
            return Resp::drive_error(400, "invalid", "Failed to parse Content-Range header.");
        };
        let Some((range, total)) = spec.split_once('/') else {
            return Resp::drive_error(400, "invalid", "Failed to parse Content-Range header.");
        };
        let total: Option<u64> = if total == "*" { None } else { total.parse().ok() };
        if let (Some(t), Some(known)) = (total, s.total)
            && t != known
        {
            return Resp::drive_error(400, "invalid", "The upload size does not match the session.");
        }
        if s.total.is_none() {
            s.total = total;
        }
        let have = s.data.len() as u64;
        if range == "*" {
            if let Some(id) = s.done.clone() {
                let mut p = params.clone();
                if let Some(f) = s.fields.clone() {
                    p.insert("fields".into(), f);
                }
                return self.respond_file(st, &id, 200, &p);
            }
            return if have == 0 {
                Resp::empty(308)
            } else {
                Resp::empty(308).header("Range", format!("bytes=0-{}", have - 1))
            };
        }
        if s.done.is_some() {
            return Resp::drive_error(400, "invalid", "The upload is already complete.");
        }
        let Some((a, b)) =
            range.split_once('-').and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
        else {
            return Resp::drive_error(400, "invalid", "Failed to parse Content-Range header.");
        };
        if b < a || body.len() as u64 != b - a + 1 {
            return Resp::drive_error(400, "invalid", "The chunk size does not match Content-Range.");
        }
        let Some(t) = s.total else { return Resp::drive_error(400, "invalid", "Unknown upload size.") };
        if b >= t {
            return Resp::drive_error(400, "invalid", "The chunk ends beyond the declared size.");
        }
        if b + 1 < t && !(body.len() as u64).is_multiple_of(CHUNK_GRANULARITY) {
            return Resp::drive_error(400, "invalid", "Chunks must be multiples of 256 KiB except the last one.");
        }
        if a > have {
            return Resp::drive_error(400, "invalid", "The chunk starts beyond the received data.");
        }
        let skip = (have - a) as usize;
        if skip < body.len() {
            s.data.extend_from_slice(&body[skip..]);
        }
        if s.data.len() as u64 == t {
            return self.finish_session(st, &upload_id, params);
        }
        let have = s.data.len() as u64;
        Resp::empty(308).header("Range", format!("bytes=0-{}", have - 1))
    }

    fn partial_chunk(
        &self,
        st: &mut State,
        params: &HashMap<String, String>,
        headers: &HashMap<String, String>,
        got: &[u8],
    ) {
        let Some(s) = params.get("upload_id").and_then(|u| st.sessions.get_mut(u)) else { return };
        let start = headers
            .get("content-range")
            .and_then(|c| c.strip_prefix("bytes "))
            .and_then(|c| c.split_once('-'))
            .and_then(|(a, _)| a.parse::<u64>().ok());
        if start != Some(s.data.len() as u64) {
            return;
        }
        let g = self.cfg.resumable_granularity.max(1);
        let keep = (got.len() as u64 / g * g) as usize;
        if s.total.is_none_or(|t| ((s.data.len() + keep) as u64) < t) {
            s.data.extend_from_slice(&got[..keep]);
        }
    }

    fn authorize(&self, st: &mut State, params: &HashMap<String, String>) -> Resp {
        let get = |k: &str| params.get(k).cloned().unwrap_or_default();
        if get("client_id") != self.cfg.client_id {
            return Resp::oauth_error(401, "invalid_client", "The OAuth client was not found.");
        }
        let redirect = get("redirect_uri");
        let loopback = redirect.starts_with("http://127.0.0.1:") || redirect.starts_with("http://[::1]:");
        if get("response_type") != "code"
            || !loopback
            || get("code_challenge_method") != "S256"
            || get("code_challenge").len() < 43
        {
            return Resp::oauth_error(400, "invalid_request", "Invalid authorization request.");
        }
        let requested: Vec<String> = get("scope").split_whitespace().map(str::to_string).collect();
        let state = get("state");
        let consent = st.consent.clone().unwrap_or(Consent::Approve);
        let mut q: Vec<(String, String)> = Vec::new();
        match consent {
            Consent::Deny => {
                q.push(("error".into(), "access_denied".into()));
                q.push(("state".into(), state));
            }
            other => {
                let scopes = match &other {
                    Consent::GrantScopes(s) => s.clone(),
                    _ => requested,
                };
                let code = self.rand("mock-code-");
                st.secrets.push(code.clone());
                st.codes.insert(
                    code.clone(),
                    AuthCode {
                        redirect_uri: redirect.clone(),
                        challenge: get("code_challenge"),
                        scopes: scopes.clone(),
                        used: false,
                    },
                );
                let state = if other == Consent::WrongState { "forged-state".to_string() } else { state };
                q.push(("state".into(), state));
                q.push(("code".into(), code));
                q.push(("scope".into(), scopes.join(" ")));
            }
        }
        let pairs: Vec<(&str, &str)> = q.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        Resp::empty(302).header("Location", format!("{redirect}/?{}", form_encode(&pairs)).replacen("//?", "/?", 1))
    }

    fn issue(&self, st: &mut State, scopes: &[String], refresh: Option<String>) -> String {
        let at = self.rand("mock-at-");
        st.secrets.push(at.clone());
        st.access.insert(
            at.clone(),
            AccessTok {
                scopes: scopes.to_vec(),
                expires_ms: now_ms() + self.cfg.access_token_ttl.as_millis() as i64,
                refresh,
                revoked: false,
            },
        );
        at
    }

    fn token(&self, st: &mut State, body: &[u8]) -> (Resp, Option<String>) {
        let form: HashMap<String, String> = form_decode(&String::from_utf8_lossy(body)).into_iter().collect();
        let get = |k: &str| form.get(k).cloned().unwrap_or_default();
        let grant = get("grant_type");
        if get("client_id") != self.cfg.client_id {
            return (Resp::oauth_error(401, "invalid_client", "The OAuth client was not found."), Some(grant));
        }
        if let Some(secret) = &self.cfg.client_secret
            && form.get("client_secret") != Some(secret)
        {
            return (Resp::oauth_error(401, "invalid_client", "Unauthorized"), Some(grant));
        }
        let ttl = self.cfg.access_token_ttl.as_secs();
        let resp = match grant.as_str() {
            "authorization_code" => {
                let code = get("code");
                let Some(c) = st.codes.get_mut(&code).filter(|c| !c.used) else {
                    return (Resp::oauth_error(400, "invalid_grant", "Malformed auth code."), Some(grant));
                };
                c.used = true;
                if c.redirect_uri != get("redirect_uri") {
                    return (Resp::oauth_error(400, "redirect_uri_mismatch", "Bad Request"), Some(grant));
                }
                let verifier = get("code_verifier");
                if !verifier.is_empty() {
                    st.secrets.push(verifier.clone());
                }
                if verifier.len() < 43 || pkce_s256(&verifier) != c.challenge {
                    return (Resp::oauth_error(400, "invalid_grant", "Invalid code verifier."), Some(grant));
                }
                let scopes = c.scopes.clone();
                let rt = self.rand("mock-rt-");
                st.secrets.push(rt.clone());
                st.refresh.insert(rt.clone(), RefreshTok { scopes: scopes.clone(), revoked: false });
                let at = self.issue(st, &scopes, Some(rt.clone()));
                Resp::json(
                    200,
                    &json!({"access_token": at, "expires_in": ttl, "refresh_token": rt, "scope": scopes.join(" "), "token_type": "Bearer"}),
                )
            }
            "refresh_token" => {
                let rt = get("refresh_token");
                let Some(r) = st.refresh.get(&rt).filter(|r| !r.revoked) else {
                    return (
                        Resp::oauth_error(400, "invalid_grant", "Token has been expired or revoked."),
                        Some(grant),
                    );
                };
                let scopes = r.scopes.clone();
                let at = self.issue(st, &scopes, Some(rt));
                Resp::json(
                    200,
                    &json!({"access_token": at, "expires_in": ttl, "scope": scopes.join(" "), "token_type": "Bearer"}),
                )
            }
            _ => Resp::oauth_error(400, "unsupported_grant_type", "Invalid grant_type."),
        };
        (resp, Some(grant))
    }

    fn revoke(&self, st: &mut State, body: &[u8], params: &HashMap<String, String>) -> Resp {
        let form: HashMap<String, String> = form_decode(&String::from_utf8_lossy(body)).into_iter().collect();
        let Some(tok) = form.get("token").or_else(|| params.get("token")).cloned() else {
            return Resp::oauth_error(400, "invalid_request", "Missing token.");
        };
        if let Some(r) = st.refresh.get_mut(&tok).filter(|r| !r.revoked) {
            r.revoked = true;
            for a in st.access.values_mut().filter(|a| a.refresh.as_deref() == Some(tok.as_str())) {
                a.revoked = true;
            }
            return Resp::json(200, &json!({}));
        }
        if let Some(a) = st.access.get_mut(&tok).filter(|a| !a.revoked) {
            a.revoked = true;
            if let Some(rt) = a.refresh.clone() {
                if let Some(r) = st.refresh.get_mut(&rt) {
                    r.revoked = true;
                }
                for a in st.access.values_mut().filter(|a| a.refresh.as_deref() == Some(rt.as_str())) {
                    a.revoked = true;
                }
            }
            return Resp::json(200, &json!({}));
        }
        Resp::oauth_error(400, "invalid_token", "Token expired or revoked")
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &self,
        st: &mut State,
        ep: Endpoint,
        path: &str,
        params: &HashMap<String, String>,
        headers: &HashMap<String, String>,
        body: &[u8],
        corrupt: bool,
        wrong_md5: bool,
    ) -> (Resp, Option<String>) {
        match ep {
            Endpoint::Authorize => return (self.authorize(st, params), None),
            Endpoint::Token => return self.token(st, body),
            Endpoint::Revoke => return (self.revoke(st, body, params), None),
            Endpoint::Unknown | Endpoint::Any => return (Resp::drive_error(404, "notFound", "Not Found"), None),
            _ => {}
        }
        let scopes = match self.bearer_scopes(st, headers) {
            Ok(s) => s,
            Err(r) => return (r, None),
        };
        let file_id = path.rsplit('/').next().unwrap_or("").to_string();
        let file_scope = |st: &State, id: &str| -> Result<(), Resp> {
            match st.files.get(id) {
                Some(f) => Self::scope_ok(&scopes, f.appdata),
                None => Err(Resp::drive_error(404, "notFound", &format!("File not found: {id}."))),
            }
        };
        let meta_json = || serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
        let resp = match ep {
            Endpoint::FilesList => self.list(st, params, &scopes),
            Endpoint::GenerateIds => {
                let count = params.get("count").and_then(|c| c.parse::<usize>().ok()).unwrap_or(10).clamp(1, 1000);
                let space = params.get("space").map(String::as_str).unwrap_or("drive");
                let appdata = space == "appDataFolder";
                if !appdata && space != "drive" {
                    return (Resp::drive_error(400, "invalid", "Invalid Value: space"), None);
                }
                if let Err(r) = Self::scope_ok(&scopes, appdata) {
                    return (r, None);
                }
                let ids: Vec<String> = (0..count).map(|_| self.new_id()).collect();
                for id in &ids {
                    st.generated.insert(id.clone(), appdata);
                }
                Resp::json(200, &json!({"kind": "drive#generatedIds", "space": space, "ids": ids}))
            }
            Endpoint::FilesGet => match file_scope(st, &file_id) {
                Ok(()) => self.respond_file(st, &file_id, 200, params),
                Err(r) => r,
            },
            Endpoint::FilesGetMedia => match file_scope(st, &file_id) {
                Ok(()) => self.get_media(st, &file_id, headers),
                Err(r) => r,
            },
            Endpoint::FilesCreate => {
                let meta = meta_json();
                match self.prepare_create(st, &meta, &scopes) {
                    Ok((id, parent, appdata)) => {
                        let id = self.insert_file(st, id, parent, appdata, &meta, Vec::new(), false);
                        self.respond_file(st, &id, 200, params)
                    }
                    Err(r) => r,
                }
            }
            Endpoint::UploadMultipart => {
                let ct = headers.get("content-type").cloned().unwrap_or_default();
                let Some((meta, mut media)) = parse_multipart(&ct, body) else {
                    return (Resp::drive_error(400, "badContent", "Malformed multipart body."), None);
                };
                match self.prepare_create(st, &meta, &scopes) {
                    Ok((id, parent, appdata)) => {
                        if let Err(r) = self.check_quota(st, media.len() as u64) {
                            return (r, None);
                        }
                        if corrupt && !media.is_empty() {
                            media[0] ^= 0x01;
                        }
                        let id = self.insert_file(st, id, parent, appdata, &meta, media, wrong_md5);
                        self.respond_file(st, &id, 200, params)
                    }
                    Err(r) => r,
                }
            }
            Endpoint::ResumableInit => {
                let meta = meta_json();
                let total = headers.get("x-upload-content-length").and_then(|v| v.parse::<u64>().ok());
                let target = if path.ends_with("/files") {
                    match self.prepare_create(st, &meta, &scopes) {
                        Ok(_) => SessionTarget::Create(meta),
                        Err(r) => return (r, None),
                    }
                } else {
                    if let Err(r) = file_scope(st, &file_id) {
                        return (r, None);
                    }
                    SessionTarget::Update { file_id: file_id.clone(), meta }
                };
                let upload_id = self.rand("mock-up-");
                st.secrets.push(upload_id.clone());
                st.sessions.insert(
                    upload_id.clone(),
                    Session {
                        target,
                        total,
                        data: Vec::new(),
                        expired: false,
                        done: None,
                        fields: params.get("fields").cloned(),
                        corrupt,
                        wrong_md5,
                    },
                );
                let loc = format!("{}{path}?uploadType=resumable&upload_id={upload_id}", self.base);
                Resp::empty(200).header("Location", loc)
            }
            Endpoint::ResumableChunk | Endpoint::ResumableStatus => {
                if let Some(s) = params.get("upload_id").and_then(|u| st.sessions.get_mut(u)) {
                    s.corrupt |= corrupt;
                    s.wrong_md5 |= wrong_md5;
                }
                self.resumable_put(st, params, headers, body)
            }
            Endpoint::FilesUpdate => match file_scope(st, &file_id) {
                Ok(()) => match self.apply_meta_update(st, &file_id, &meta_json()) {
                    Ok(()) => self.respond_file(st, &file_id, 200, params),
                    Err(r) => r,
                },
                Err(r) => r,
            },
            Endpoint::FilesUpdateMedia => {
                if let Err(r) = file_scope(st, &file_id) {
                    return (r, None);
                }
                let ct = headers.get("content-type").cloned().unwrap_or_default();
                let (meta, mut media) = if params.get("uploadType").map(String::as_str) == Some("multipart") {
                    match parse_multipart(&ct, body) {
                        Some(x) => x,
                        None => return (Resp::drive_error(400, "badContent", "Malformed multipart body."), None),
                    }
                } else {
                    (Value::Null, body.to_vec())
                };
                if corrupt && !media.is_empty() {
                    media[0] ^= 0x01;
                }
                match self
                    .replace_content(st, &file_id, media, wrong_md5)
                    .and_then(|_| self.apply_meta_update(st, &file_id, &meta))
                {
                    Ok(()) => self.respond_file(st, &file_id, 200, params),
                    Err(r) => r,
                }
            }
            Endpoint::FilesDelete => match file_scope(st, &file_id) {
                Ok(()) => {
                    Self::delete_tree(st, &file_id);
                    Resp::empty(204)
                }
                Err(r) => r,
            },
            Endpoint::About => {
                if !params.contains_key("fields") {
                    return (
                        Resp::drive_error(400, "required", "The 'fields' parameter is required for this method."),
                        None,
                    );
                }
                let usage = Self::usage(st);
                let mut quota =
                    json!({"usage": usage.to_string(), "usageInDrive": usage.to_string(), "usageInDriveTrash": "0"});
                if let Some(l) = self.cfg.quota_limit {
                    quota["limit"] = Value::String(l.to_string());
                }
                let v = json!({"kind": "drive#about", "storageQuota": quota,
                    "user": {"kind": "drive#user", "displayName": "Mock User", "emailAddress": self.cfg.email, "permissionId": "01234567890123456789", "me": true}});
                match fields_of(params, "") {
                    Ok(f) => Resp::json(200, &select(&v, &f)),
                    Err(r) => r,
                }
            }
            _ => Resp::drive_error(404, "notFound", "Not Found"),
        };
        (resp, None)
    }
}

fn route(method: &str, path: &str, params: &HashMap<String, String>, headers: &HashMap<String, String>) -> Endpoint {
    let upload_type = params.get("uploadType").map(String::as_str);
    match (method, path) {
        ("GET", "/o/oauth2/v2/auth") => Endpoint::Authorize,
        ("POST", "/token") => Endpoint::Token,
        ("POST", "/revoke") => Endpoint::Revoke,
        ("GET", "/drive/v3/about") => Endpoint::About,
        ("GET", "/drive/v3/files") => Endpoint::FilesList,
        ("GET", "/drive/v3/files/generateIds") => Endpoint::GenerateIds,
        ("POST", "/drive/v3/files") => Endpoint::FilesCreate,
        ("POST", "/upload/drive/v3/files") => match upload_type {
            Some("multipart") => Endpoint::UploadMultipart,
            Some("resumable") => Endpoint::ResumableInit,
            _ => Endpoint::Unknown,
        },
        ("PUT", p) if p.starts_with("/upload/drive/v3/files") && params.contains_key("upload_id") => {
            if headers.get("content-range").is_some_and(|c| c.trim().starts_with("bytes */")) {
                Endpoint::ResumableStatus
            } else {
                Endpoint::ResumableChunk
            }
        }
        ("PATCH", p) if p.starts_with("/upload/drive/v3/files/") => match upload_type {
            Some("resumable") => Endpoint::ResumableInit,
            Some("multipart") | Some("media") => Endpoint::FilesUpdateMedia,
            _ => Endpoint::Unknown,
        },
        (m, p) if p.starts_with("/drive/v3/files/") && p.matches('/').count() == 4 => match m {
            "GET" if params.get("alt").map(String::as_str) == Some("media") => Endpoint::FilesGetMedia,
            "GET" => Endpoint::FilesGet,
            "PATCH" => Endpoint::FilesUpdate,
            "DELETE" => Endpoint::FilesDelete,
            _ => Endpoint::Unknown,
        },
        _ => Endpoint::Unknown,
    }
}

fn send(req: tiny_http::Request, r: Resp) {
    let mut resp = tiny_http::Response::from_data(r.body).with_status_code(r.status);
    for (k, v) in &r.headers {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            resp.add_header(h);
        }
    }
    let _ = req.respond(resp);
}

fn handle(sh: &Shared, mut req: tiny_http::Request) {
    let method = req.method().as_str().to_string();
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let path = pct_decode(path, false);
    let params: HashMap<String, String> = form_decode(query).into_iter().collect();
    let headers: HashMap<String, String> = req
        .headers()
        .iter()
        .map(|h| (h.field.as_str().as_str().to_ascii_lowercase(), h.value.as_str().to_string()))
        .collect();
    let peer: Option<SocketAddr> = req.remote_addr().copied();
    let ep = route(&method, &path, &params, &headers);
    let faults = {
        let mut st = sh.state.lock();
        sh.pick_faults(&mut st, ep, &path)
    };
    let mut record = RequestRecord {
        endpoint: ep,
        method: method.clone(),
        path: path.clone(),
        status: 0,
        content_range: headers.get("content-range").cloned(),
        body_len: 0,
        detail: None,
    };
    let log = |record: RequestRecord| sh.state.lock().log.push(record);
    let cut = |req: tiny_http::Request| {
        if let Some(p) = &peer {
            sh.relay.kill(p);
        }
        drop(req.into_writer());
    };

    for f in &faults {
        if let Fault::Delay(d) = f {
            thread::sleep(*d);
        }
    }
    if faults.iter().any(|f| matches!(f, Fault::DropConnection)) {
        log(record);
        cut(req);
        return;
    }
    let drop_after = faults.iter().find_map(|f| match f {
        Fault::DropAfterRequestBytes(n) => Some(*n),
        _ => None,
    });
    let mut body = Vec::new();
    let _ = req.as_reader().take(drop_after.unwrap_or(512 * 1024 * 1024)).read_to_end(&mut body);
    record.body_len = body.len() as u64;
    if drop_after.is_some() {
        if ep == Endpoint::ResumableChunk {
            let mut st = sh.state.lock();
            sh.partial_chunk(&mut st, &params, &headers, &body);
        }
        log(record);
        cut(req);
        return;
    }
    if let Some(Fault::Error { status, reason, message, retry_after }) =
        faults.iter().find(|f| matches!(f, Fault::Error { .. }))
    {
        let mut r = if *status == 401 { Resp::unauthorized() } else { Resp::drive_error(*status, reason, message) };
        if let Some(ra) = retry_after {
            r = r.header("Retry-After", ra.clone());
        }
        record.status = r.status;
        log(record);
        send(req, r);
        return;
    }
    let corrupt = faults.iter().any(|f| matches!(f, Fault::CorruptStoredContent));
    let wrong_md5 = faults.iter().any(|f| matches!(f, Fault::WrongMd5));
    let (mut resp, detail) = {
        let mut st = sh.state.lock();
        if faults.iter().any(|f| matches!(f, Fault::ExpireSession))
            && let Some(s) = params.get("upload_id").and_then(|u| st.sessions.get_mut(u))
        {
            s.expired = true;
        }
        sh.dispatch(&mut st, ep, &path, &params, &headers, &body, corrupt, wrong_md5)
    };
    record.detail = detail;
    if faults.iter().any(|f| matches!(f, Fault::ApplyThenDrop)) {
        log(record);
        cut(req);
        return;
    }
    if let Some(keep) = faults.iter().find_map(|f| match f {
        Fault::TruncateResponse { keep } => Some(*keep),
        _ => None,
    }) && let Some(p) = &peer
    {
        let mut raw = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n",
            resp.status,
            reason_phrase(resp.status),
            resp.body.len()
        );
        for (k, v) in &resp.headers {
            raw.push_str(&format!("{k}: {v}\r\n"));
        }
        raw.push_str("\r\n");
        let mut bytes = raw.into_bytes();
        bytes.extend_from_slice(&resp.body[..keep.min(resp.body.len())]);
        record.status = resp.status;
        log(record);
        drop(req.into_writer());
        sh.relay.send_raw_and_kill(p, &bytes);
        return;
    }
    if let Some(loc) = faults.iter().find_map(|f| match f {
        Fault::BadLocation(l) => Some(l.clone()),
        _ => None,
    }) {
        resp.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("location"));
        resp.headers.push(("Location".into(), loc));
    }
    record.status = resp.status;
    log(record);
    send(req, resp);
}

impl MockDrive {
    /// Start a mock with default settings.
    pub fn start_default() -> std::io::Result<MockDrive> {
        Self::start(MockConfig::default())
    }

    /// Start a mock server on `127.0.0.1` (ephemeral port).
    pub fn start(cfg: MockConfig) -> std::io::Result<MockDrive> {
        let server =
            Arc::new(tiny_http::Server::http("127.0.0.1:0").map_err(|e| std::io::Error::other(e.to_string()))?);
        let upstream = server.server_addr().to_ip().ok_or_else(|| std::io::Error::other("no TCP address"))?;
        let relay = Relay::start(upstream)?;
        let base = format!("http://{}", relay.addr());
        let mut st = State { clock_ms: 1_790_000_000_000, ..State::default() };
        if let Some(s) = &cfg.client_secret {
            st.secrets.push(s.clone());
        }
        let workers_n = cfg.workers.max(2);
        let shared = Arc::new(Shared {
            rng: Mutex::new(fastrand::Rng::with_seed(cfg.seed)),
            cfg,
            state: Mutex::new(st),
            relay,
            base,
            stop: AtomicBool::new(false),
        });
        let mut workers = Vec::new();
        for i in 0..workers_n {
            let sh = shared.clone();
            let srv = server.clone();
            workers.push(thread::Builder::new().name(format!("mock-drive-{i}")).spawn(move || {
                while !sh.stop.load(Ordering::SeqCst) {
                    match srv.recv_timeout(Duration::from_millis(50)) {
                        Ok(Some(req)) => handle(&sh, req),
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            })?);
        }
        Ok(MockDrive { shared, server, workers })
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> String {
        self.shared.base.clone()
    }

    /// Drive endpoints pointing at the mock.
    pub fn endpoints(&self) -> DriveEndpoints {
        DriveEndpoints::custom(&self.shared.base)
    }

    /// OAuth endpoints pointing at the mock.
    pub fn oauth_endpoints(&self) -> OAuthEndpoints {
        OAuthEndpoints {
            auth_url: format!("{}/o/oauth2/v2/auth", self.shared.base),
            token_url: format!("{}/token", self.shared.base),
            revoke_url: format!("{}/revoke", self.shared.base),
        }
    }

    /// The OAuth client the mock accepts.
    pub fn client_config(&self) -> OAuthClientConfig {
        OAuthClientConfig {
            client_id: self.shared.cfg.client_id.clone(),
            client_secret: self.shared.cfg.client_secret.clone().map(Secret::new),
            endpoints: self.oauth_endpoints(),
            source: crate::config::ClientConfigSource::Explicit,
        }
    }

    /// The settings in use.
    pub fn config(&self) -> &MockConfig {
        &self.shared.cfg
    }

    /// Issue a refresh token directly (skipping the browser flow).
    pub fn issue_refresh_token(&self, scopes: &[&str]) -> String {
        let mut st = self.shared.state.lock();
        let rt = self.shared.rand("mock-rt-");
        st.secrets.push(rt.clone());
        st.refresh
            .insert(rt.clone(), RefreshTok { scopes: scopes.iter().map(|s| s.to_string()).collect(), revoked: false });
        rt
    }

    /// Add a fault rule.
    pub fn add_fault(&self, rule: FaultRule) {
        self.shared.state.lock().rules.push(RuleState { rule, matches: 0, triggers: 0 });
    }

    /// Remove all fault rules.
    pub fn clear_faults(&self) {
        self.shared.state.lock().rules.clear();
    }

    /// Number of faults injected so far.
    pub fn faults_triggered(&self) -> usize {
        self.shared.state.lock().triggered
    }

    /// Every handled request, in order.
    pub fn requests(&self) -> Vec<RequestRecord> {
        self.shared.state.lock().log.clone()
    }

    /// Requests that hit `ep`.
    pub fn count(&self, ep: Endpoint) -> usize {
        self.shared.state.lock().log.iter().filter(|r| ep == Endpoint::Any || r.endpoint == ep).count()
    }

    /// Token-endpoint requests with this `grant_type`.
    pub fn token_requests(&self, grant_type: &str) -> usize {
        self.shared
            .state
            .lock()
            .log
            .iter()
            .filter(|r| r.endpoint == Endpoint::Token && r.detail.as_deref() == Some(grant_type))
            .count()
    }

    /// Clear the request log.
    pub fn clear_log(&self) {
        self.shared.state.lock().log.clear();
    }

    /// Snapshot of all stored objects.
    pub fn objects(&self) -> Vec<MockObject> {
        let st = self.shared.state.lock();
        st.files
            .values()
            .map(|f| MockObject {
                id: f.id.clone(),
                name: f.name.clone(),
                parents: f.parents.clone(),
                is_folder: f.is_folder(),
                size: f.content.len() as u64,
                md5: f.md5.clone(),
                trashed: f.trashed,
                app_properties: f.props.clone(),
                created_ms: f.created_ms,
            })
            .collect()
    }

    /// Stored bytes of an object.
    pub fn content(&self, id: &str) -> Option<Vec<u8>> {
        self.shared.state.lock().files.get(id).map(|f| f.content.to_vec())
    }

    /// ID of the simulated My Drive root.
    pub fn root_id(&self) -> &'static str {
        ROOT_ID
    }

    /// ID of the simulated `appDataFolder`.
    pub fn appdata_id(&self) -> &'static str {
        APPDATA_ID
    }

    /// Create a folder directly (tests: duplicates, pre-existing layouts). `parent` `None` = My
    /// Drive root. `created_ms` `None` = next tick of the mock clock.
    pub fn seed_folder(
        &self,
        parent: Option<&str>,
        name: &str,
        created_ms: Option<i64>,
        props: &[(&str, &str)],
    ) -> String {
        self.seed(parent, name, FOLDER_MIME, Vec::new(), created_ms, props)
    }

    /// Create a file directly (tagged `ps=1`).
    pub fn seed_file(&self, parent: &str, name: &str, content: &[u8], created_ms: Option<i64>) -> String {
        self.seed(Some(parent), name, "application/octet-stream", content.to_vec(), created_ms, &[("ps", "1")])
    }

    fn seed(
        &self,
        parent: Option<&str>,
        name: &str,
        mime: &str,
        content: Vec<u8>,
        created_ms: Option<i64>,
        props: &[(&str, &str)],
    ) -> String {
        let mut st = self.shared.state.lock();
        let id = self.shared.new_id();
        let parent = parent.map(|p| self.shared.alias(p)).unwrap_or_else(|| ROOT_ID.to_string());
        let appdata = parent == APPDATA_ID || st.files.get(&parent).is_some_and(|f| f.appdata);
        let meta = json!({"name": name, "mimeType": mime,
            "appProperties": props.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect::<serde_json::Map<_, _>>()});
        let id = self.shared.insert_file(&mut st, id, parent, appdata, &meta, content, false);
        if let Some(f) = st.files.get_mut(&id) {
            f.list_hidden = 0;
            if let Some(c) = created_ms {
                f.created_ms = c;
            }
        }
        id
    }

    /// Flip the first stored byte of an object but keep the reported checksums (bit rot / a
    /// server-side corruption that only a client-side check can detect).
    pub fn corrupt_stored_bytes(&self, id: &str) {
        if let Some(f) = self.shared.state.lock().files.get_mut(id) {
            let mut c = f.content.to_vec();
            if let Some(b) = c.first_mut() {
                *b ^= 0x01;
            }
            f.content = Arc::new(c);
        }
    }

    /// Mark an object as trashed (as if the user trashed it).
    pub fn trash(&self, id: &str) {
        if let Some(f) = self.shared.state.lock().files.get_mut(id) {
            f.trashed = true;
        }
    }

    /// Revoke every refresh token (and the access tokens issued from them).
    pub fn revoke_all_tokens(&self) {
        let mut st = self.shared.state.lock();
        for r in st.refresh.values_mut() {
            r.revoked = true;
        }
        for a in st.access.values_mut() {
            a.revoked = true;
        }
    }

    /// Let every access token expire now.
    pub fn expire_access_tokens(&self) {
        for a in self.shared.state.lock().access.values_mut() {
            a.expires_ms = 0;
        }
    }

    /// Expire every resumable session.
    pub fn expire_sessions(&self) {
        for s in self.shared.state.lock().sessions.values_mut() {
            s.expired = true;
        }
    }

    /// Set the consent-screen behaviour.
    pub fn set_consent(&self, c: Consent) {
        self.shared.state.lock().consent = Some(c);
    }

    /// Every secret the mock issued or knows: client secret, authorization codes, access and
    /// refresh tokens, upload session IDs (secret-hygiene tests).
    pub fn secrets(&self) -> Vec<String> {
        self.shared.state.lock().secrets.clone()
    }
}

impl Drop for MockDrive {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.relay.shutdown();
        for _ in 0..self.workers.len() {
            self.server.unblock();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

/// A page served to the scripted browser.
#[derive(Debug, Clone)]
pub struct BrowserVisit {
    /// Status of the loopback callback response.
    pub status: u16,
    /// Body of the loopback page.
    pub body: String,
}

/// A scripted "browser" for OAuth tests: on a background thread it opens the authorization URL
/// on the mock's consent stand-in and delivers the redirect to the loopback listener.
#[derive(Clone, Default)]
pub struct MockBrowser {
    visits: Arc<(Mutex<Vec<BrowserVisit>>, Condvar)>,
    forge_first: bool,
}

impl MockBrowser {
    /// Follows the redirect faithfully.
    pub fn new() -> Self {
        Self::default()
    }

    /// First delivers a forged callback (wrong `state`), then the real one.
    pub fn forging_first_callback() -> Self {
        Self { forge_first: true, ..Self::default() }
    }

    /// Wait (event-driven, bounded) until `n` loopback pages were received.
    pub fn wait_visits(&self, n: usize, timeout: Duration) -> Vec<BrowserVisit> {
        let (m, cv) = &*self.visits;
        let mut v = m.lock();
        let deadline = std::time::Instant::now() + timeout;
        while v.len() < n {
            if cv.wait_until(&mut v, deadline).timed_out() {
                break;
            }
        }
        v.clone()
    }
}

impl UrlOpener for MockBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        let url = url.to_string();
        let visits = self.visits.clone();
        let forge = self.forge_first;
        thread::spawn(move || {
            let agent: ureq::Agent =
                ureq::Agent::config_builder().max_redirects(0).http_status_as_error(false).build().into();
            let Ok(resp) = agent.get(&url).call() else { return };
            let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string) else {
                return;
            };
            let visit = |target: &str| {
                if let Ok(mut r) = agent.get(target).call() {
                    let status = r.status().as_u16();
                    let body = r.body_mut().read_to_string().unwrap_or_default();
                    let (m, cv) = &*visits;
                    m.lock().push(BrowserVisit { status, body });
                    cv.notify_all();
                }
            };
            if forge {
                let forged = match loc.split_once('?') {
                    Some((base, q)) => {
                        let pairs: Vec<(String, String)> = form_decode(q)
                            .into_iter()
                            .map(|(k, v)| if k == "state" { (k, "forged".to_string()) } else { (k, v) })
                            .collect();
                        let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                        format!("{base}?{}", form_encode(&refs))
                    }
                    None => loc.clone(),
                };
                visit(&forged);
            }
            visit(&loc);
        });
        Ok(())
    }
}
