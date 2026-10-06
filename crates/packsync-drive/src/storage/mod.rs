//! [`DriveStorage`]: the [`Storage`] contract on top of the Drive API v3.
//!
//! Layout: logical key `a/b/c.pack` → folder `a` → folder `b` → file `c.pack`, all below one root
//! folder (`PackSync`, tagged `psk=root`) in My Drive (`drive.file`) or in `appDataFolder`. Every
//! object PackSync creates carries `appProperties ps=1`.
//!
//! Drive names are not unique. Folder sets and file lookups therefore always work with *all*
//! objects of a name and pick a canonical one deterministically (oldest `createdTime`, then lowest
//! ID); `list` unions the children of duplicate folders and reports extra copies in
//! [`ObjectInfo::duplicates`]. Duplicates are never deleted automatically.

mod api;
mod download;
mod upload;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use packsync_core::progress::{Level, OpCtx, Phase};
use packsync_core::retry::Sleeper;
use packsync_core::storage::*;
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};

pub(crate) use api::Api;

use crate::config::{DRIVE_MAX_FILE_SIZE, DriveConfig, FOLDER_MIME};
use crate::http::{Method, Request};
use crate::model::{
    DriveFile, FILE_FIELDS, KIND_ROOT, PROP_KIND, PROP_MARKER, RawAbout, RawFile, RawFileList, RawGeneratedIds,
    canonical_order, is_reserved_tag, list_fields, sort_canonical,
};
use crate::oauth::AccessTokenSource;
use crate::stats::{RequestKind, RequestStats, StatsHandle};
use crate::util::{is_valid_drive_id, q_string, truncate_utf8, with_query};

/// Upper bound on pages per listing (1000 × 10 000 objects); protects against servers that never
/// stop returning page tokens.
const MAX_PAGES: usize = 10_000;
/// Parents per `files.list` query (`'a' in parents or 'b' in parents ...`).
const PARENTS_PER_QUERY: usize = 20;

/// One object or folder created by this instance (recorded when
/// [`DriveConfig::track_created`] is set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedObject {
    /// Drive file ID.
    pub id: String,
    /// Logical key (`dir/` for folders, `""` for the root folder).
    pub key: String,
    /// Whether it is a folder.
    pub is_folder: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FolderRef {
    id: String,
    #[serde(rename = "c", default)]
    created_ms: Option<i64>,
}

fn sort_folders(v: &mut Vec<FolderRef>) {
    v.sort_by(|a, b| canonical_order(a.created_ms, &a.id, b.created_ms, &b.id));
    v.dedup_by(|a, b| a.id == b.id);
}

#[derive(Debug, Clone)]
struct DirEntry {
    ids: Vec<FolderRef>,
    /// `None` for hints loaded from the persisted state (never verified by this process).
    fetched: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fresh {
    /// Use any cached value (including persisted hints).
    Any,
    /// Re-query cache entries older than `folder_cache_ttl` (and unverified hints).
    Ttl,
}

enum Resolved {
    Found { ids: Vec<FolderRef>, fresh: bool },
    Missing { fresh: bool },
}

#[derive(Debug, Clone)]
struct CachedFile {
    file: DriveFile,
    duplicates: u32,
}

/// Serialises operations on one key inside this process (concurrent identical creates then reuse
/// the first result instead of uploading again).
#[derive(Default)]
struct KeyLocks {
    busy: Mutex<HashSet<String>>,
    cv: Condvar,
}

struct KeyGuard<'a> {
    locks: &'a KeyLocks,
    key: String,
}

impl KeyLocks {
    fn acquire(&self, key: &str, ctx: &OpCtx) -> StorageResult<KeyGuard<'_>> {
        let mut busy = self.busy.lock();
        while busy.contains(key) {
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            self.cv.wait_for(&mut busy, Duration::from_millis(50));
        }
        busy.insert(key.to_string());
        Ok(KeyGuard { locks: self, key: key.to_string() })
    }
}

impl Drop for KeyGuard<'_> {
    fn drop(&mut self) {
        self.locks.busy.lock().remove(&self.key);
        self.locks.cv.notify_all();
    }
}

pub(crate) struct Inner {
    cfg: DriveConfig,
    api: Api,
    state: Arc<dyn TransferStateStore>,
    pid: String,
    dirs: Mutex<HashMap<String, DirEntry>>,
    files: Mutex<HashMap<String, CachedFile>>,
    id_pool: Mutex<Vec<String>>,
    key_locks: KeyLocks,
    folder_lock: Mutex<()>,
    created: Mutex<Vec<CreatedObject>>,
}

/// Google Drive implementation of [`Storage`]. Thread-safe; cheap to share behind an `Arc`.
pub struct DriveStorage {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for DriveStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveStorage").field("provider_id", &self.inner.pid).finish_non_exhaustive()
    }
}

fn split_key(key: &str) -> (&str, &str) {
    match key.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", key),
    }
}

fn object_info(key: &str, f: &DriveFile, duplicates: u32) -> ObjectInfo {
    ObjectInfo {
        key: key.to_string(),
        size: f.size.unwrap_or(0),
        created_ms: f.created_ms,
        modified_ms: f.modified_ms,
        checksums: Checksums { md5: f.md5.clone(), sha256: f.sha256.clone(), blake3: None },
        provider_ref: Some(f.id.clone()),
        is_dir: false,
        duplicates,
        tags: f.user_tags(),
    }
}

fn is_valid_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `appProperties` for an object: the `ps=1` marker plus validated caller tags. Keys must be
/// `[A-Za-z0-9._-]{1,64}` and not reserved; values are truncated (on a UTF-8 boundary) so that
/// key + value fit Drive's 124-byte limit; at most 29 tags (Drive allows 30 per app per file).
pub(crate) fn app_properties(
    tags: &BTreeMap<String, String>,
) -> StorageResult<serde_json::Map<String, serde_json::Value>> {
    if tags.len() > 29 {
        return Err(StorageError::Other(format!(
            "{} tags given; Drive allows at most 29 besides PackSync's marker",
            tags.len()
        )));
    }
    let mut m = serde_json::Map::new();
    m.insert(PROP_MARKER.into(), "1".into());
    for (k, v) in tags {
        if is_reserved_tag(k) {
            return Err(StorageError::Other(format!("tag key {k:?} is reserved by the Drive adapter")));
        }
        if k.is_empty()
            || k.len() > 64
            || !k.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(StorageError::Other(format!(
                "tag key {k:?} is not allowed (use [A-Za-z0-9._-], at most 64 bytes)"
            )));
        }
        m.insert(k.clone(), truncate_utf8(v, 124 - k.len()).into());
    }
    Ok(m)
}

impl DriveStorage {
    /// Create an adapter. `tokens` is normally an [`Authenticator`](crate::oauth::Authenticator);
    /// `state` persists upload sessions and cached IDs across restarts (the engine's database).
    pub fn new(
        cfg: DriveConfig,
        tokens: Arc<dyn AccessTokenSource>,
        state: Arc<dyn TransferStateStore>,
    ) -> StorageResult<Self> {
        Self::with_sleeper(cfg, tokens, state, packsync_core::retry::real_sleeper())
    }

    /// Like [`new`](Self::new) with an injected [`Sleeper`] (tests use `RecordingSleeper`).
    pub fn with_sleeper(
        cfg: DriveConfig,
        tokens: Arc<dyn AccessTokenSource>,
        state: Arc<dyn TransferStateStore>,
        sleeper: Arc<dyn Sleeper>,
    ) -> StorageResult<Self> {
        cfg.validate()?;
        let transport = crate::http::Transport::new(cfg.timeouts, &cfg.user_agent, cfg.stats.clone());
        let account = if cfg.account.is_empty() { "unknown" } else { cfg.account.as_str() };
        let pid = format!("gdrive:{account}:{}:{}", cfg.space.tag(), cfg.root_folder_name);
        let api = Api {
            transport,
            recent: Default::default(),
            tokens,
            sleeper,
            retry: cfg.retry.clone(),
            max_retry_after: cfg.max_retry_after,
        };
        Ok(Self {
            inner: Arc::new(Inner {
                cfg,
                api,
                state,
                pid,
                dirs: Mutex::new(HashMap::new()),
                files: Mutex::new(HashMap::new()),
                id_pool: Mutex::new(Vec::new()),
                key_locks: KeyLocks::default(),
                folder_lock: Mutex::new(()),
                created: Mutex::new(Vec::new()),
            }),
        })
    }

    /// The configuration in use.
    pub fn config(&self) -> &DriveConfig {
        &self.inner.cfg
    }

    /// HTTP request counters.
    pub fn stats(&self) -> RequestStats {
        self.inner.api.stats().snapshot()
    }

    /// The last (up to 16) retry reasons, redacted, oldest first (diagnostics).
    pub fn recent_retries(&self) -> Vec<String> {
        self.inner.api.recent.lock().iter().cloned().collect()
    }

    /// The shared counter handle.
    pub fn stats_handle(&self) -> &StatsHandle {
        self.inner.api.stats()
    }

    /// Update only the `appProperties` of the canonical object for `key` (merge semantics: given
    /// tags are set, others are kept). Creates no new revision; meant for lease heartbeats.
    pub fn update_tags(&self, key: &str, tags: &BTreeMap<String, String>, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        self.inner.update_tags(key, tags, ctx)
    }

    /// Objects and folders created by this instance, in creation order (requires
    /// [`DriveConfig::track_created`]).
    pub fn created_objects(&self) -> Vec<CreatedObject> {
        self.inner.created.lock().clone()
    }

    /// Permanently delete everything this instance created (files first, then folders, newest
    /// first), skipping anything not tagged `ps=1`. Used by the self-test to clean up exactly what
    /// it created. Returns the number of deleted objects.
    pub fn delete_created(&self, ctx: &OpCtx) -> StorageResult<usize> {
        self.inner.delete_created(ctx)
    }
}

impl Inner {
    fn api_space(&self) -> &'static str {
        self.cfg.space.api_space()
    }

    fn state_key(&self, kind: &str, rest: &str) -> String {
        format!("{}|{kind}|{rest}", self.pid)
    }

    fn record_created(&self, id: &str, key: &str, is_folder: bool) {
        if self.cfg.track_created {
            self.created.lock().push(CreatedObject { id: id.into(), key: key.into(), is_folder });
        }
    }

    // ----- folder cache ------------------------------------------------------------------

    fn cached_dir(&self, path: &str) -> Option<DirEntry> {
        if let Some(e) = self.dirs.lock().get(path) {
            return Some(e.clone());
        }
        let raw = self.state.load(&self.state_key("dir", path))?;
        let mut ids: Vec<FolderRef> = serde_json::from_str(&raw).ok()?;
        ids.retain(|f| is_valid_drive_id(&f.id));
        if ids.is_empty() {
            return None;
        }
        sort_folders(&mut ids);
        Some(DirEntry { ids, fetched: None })
    }

    fn store_dir(&self, path: &str, ids: &[FolderRef]) {
        let mut ids = ids.to_vec();
        sort_folders(&mut ids);
        if let Ok(json) = serde_json::to_string(&ids) {
            self.state.save(&self.state_key("dir", path), &json);
        }
        self.dirs.lock().insert(path.to_string(), DirEntry { ids, fetched: Some(Instant::now()) });
    }

    /// Forget `rel` and all its ancestors (a parent vanished or was trashed).
    fn invalidate_dir_chain(&self, rel: &str) {
        let mut paths = vec![String::new()];
        let mut acc = String::new();
        for comp in rel.split('/').filter(|c| !c.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(comp);
            paths.push(acc.clone());
        }
        let mut dirs = self.dirs.lock();
        for p in paths {
            dirs.remove(&p);
            self.state.clear(&self.state_key("dir", &p));
        }
    }

    fn is_fresh(&self, e: &DirEntry, fresh: Fresh) -> bool {
        match fresh {
            Fresh::Any => true,
            Fresh::Ttl => e.fetched.is_some_and(|t| t.elapsed() < self.cfg.folder_cache_ttl),
        }
    }

    fn entry_recent(&self, e: &DirEntry) -> bool {
        e.fetched.is_some_and(|t| t.elapsed() < self.cfg.folder_cache_ttl)
    }

    // ----- queries -----------------------------------------------------------------------

    /// Run a paginated `files.list` query.
    fn list_query(&self, q: &str, ctx: &OpCtx, phase: Phase, what: &str) -> StorageResult<(Vec<DriveFile>, bool)> {
        let fields = list_fields();
        let page_size = self.cfg.page_size.to_string();
        let base = format!("{}/files", self.cfg.endpoints.api_base);
        let mut token: Option<String> = None;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut incomplete = false;
        for _ in 0..MAX_PAGES {
            let mut params: Vec<(&str, &str)> =
                vec![("q", q), ("fields", &fields), ("pageSize", &page_size), ("spaces", self.api_space())];
            if let Some(t) = &token {
                params.push(("pageToken", t));
            }
            let url = with_query(&base, &params);
            let page: RawFileList =
                self.api.json(ctx, phase, what, || Request::new(Method::Get, url.clone(), RequestKind::List))?;
            incomplete |= page.incomplete_search;
            for raw in page.files {
                match DriveFile::from_raw(raw) {
                    Ok(f) => out.push(f),
                    Err(e) => tracing::warn!(target: "packsync_drive", "{what}: skipping a malformed entry ({e})"),
                }
            }
            match page.next_page_token.filter(|t| !t.is_empty()) {
                Some(t) => {
                    if t.len() > 8192 || !seen.insert(t.clone()) {
                        return Err(StorageError::Other(format!("{what}: the server returned an invalid page token")));
                    }
                    token = Some(t);
                }
                None => return Ok((out, incomplete)),
            }
        }
        Err(StorageError::Other(format!("{what}: the listing did not terminate")))
    }

    fn parents_clause(parents: &[FolderRef]) -> String {
        let parts: Vec<String> = parents.iter().map(|p| format!("{} in parents", q_string(&p.id))).collect();
        format!("({})", parts.join(" or "))
    }

    /// Folders called `name` below any of `parents` (canonical order).
    fn query_folders(&self, parents: &[FolderRef], name: &str, ctx: &OpCtx) -> StorageResult<Vec<FolderRef>> {
        let mut out = Vec::new();
        for group in parents.chunks(PARENTS_PER_QUERY) {
            let q = format!(
                "mimeType = {} and name = {} and trashed = false and appProperties has {{ key='{PROP_MARKER}' and value='1' }} and {}",
                q_string(FOLDER_MIME),
                q_string(name),
                Self::parents_clause(group)
            );
            let (files, _) = self.list_query(&q, ctx, Phase::Discovery, "looking up a folder")?;
            out.extend(
                files
                    .into_iter()
                    .filter(|f| f.is_folder && f.name == name && !f.trashed && f.is_ours())
                    .map(|f| FolderRef { id: f.id, created_ms: f.created_ms }),
            );
        }
        sort_folders(&mut out);
        Ok(out)
    }

    fn query_roots(&self, ctx: &OpCtx) -> StorageResult<Vec<FolderRef>> {
        let name = &self.cfg.root_folder_name;
        let q = format!(
            "mimeType = {} and name = {} and trashed = false and appProperties has {{ key='{PROP_KIND}' and value='{KIND_ROOT}' }}",
            q_string(FOLDER_MIME),
            q_string(name)
        );
        let (files, _) = self.list_query(&q, ctx, Phase::Discovery, "looking up the PackSync folder")?;
        let mut out: Vec<FolderRef> = files
            .into_iter()
            .filter(|f| f.is_folder && &f.name == name && !f.trashed && f.is_ours())
            .map(|f| FolderRef { id: f.id, created_ms: f.created_ms })
            .collect();
        sort_folders(&mut out);
        Ok(out)
    }

    /// All live files called `name` in the folder set (canonical order).
    fn find_files(&self, dir: &[FolderRef], name: &str, ctx: &OpCtx, phase: Phase) -> StorageResult<Vec<DriveFile>> {
        let mut out = Vec::new();
        for group in dir.chunks(PARENTS_PER_QUERY) {
            let q = format!(
                "name = {} and trashed = false and mimeType != {} and appProperties has {{ key='{PROP_MARKER}' and value='1' }} and {}",
                q_string(name),
                q_string(FOLDER_MIME),
                Self::parents_clause(group)
            );
            let (files, _) = self.list_query(&q, ctx, phase, "looking up an object")?;
            out.extend(files.into_iter().filter(|f| !f.is_folder && f.name == name && !f.trashed && f.is_ours()));
        }
        sort_canonical(&mut out);
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    /// `files.get` by ID (strongly consistent). `None` when it does not exist (or is not visible).
    fn get_by_id(&self, id: &str, ctx: &OpCtx, phase: Phase) -> StorageResult<Option<DriveFile>> {
        if !is_valid_drive_id(id) {
            return Ok(None);
        }
        let url = with_query(&format!("{}/files/{id}", self.cfg.endpoints.api_base), &[("fields", FILE_FIELDS)]);
        let res = self.api.json::<RawFile>(ctx, phase, "reading object metadata", || {
            Request::new(Method::Get, url.clone(), RequestKind::Get)
        });
        match res {
            Ok(raw) => DriveFile::from_raw(raw).map(Some).map_err(StorageError::Other),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // ----- folders -----------------------------------------------------------------------

    fn lock_folders(&self, ctx: &OpCtx) -> StorageResult<parking_lot::MutexGuard<'_, ()>> {
        loop {
            if let Some(g) = self.folder_lock.try_lock_for(Duration::from_millis(50)) {
                return Ok(g);
            }
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
        }
    }

    /// Create a folder with a pre-generated ID (so a retried create cannot duplicate it).
    fn create_folder(
        &self,
        parent: Option<&str>,
        name: &str,
        key: &str,
        root: bool,
        ctx: &OpCtx,
    ) -> StorageResult<DriveFile> {
        let mut props = serde_json::Map::new();
        props.insert(PROP_MARKER.into(), "1".into());
        if root {
            props.insert(PROP_KIND.into(), KIND_ROOT.into());
        }
        let parents: Vec<&str> = match (parent, self.cfg.space) {
            (Some(p), _) => vec![p],
            (None, crate::config::DriveSpace::AppData) => vec!["appDataFolder"],
            (None, crate::config::DriveSpace::VisibleFolder) => vec![],
        };
        for _ in 0..2 {
            let id = self.next_id(ctx)?;
            let mut meta = serde_json::json!({"id": id, "name": name, "mimeType": FOLDER_MIME, "appProperties": props});
            if !parents.is_empty() {
                meta["parents"] = serde_json::json!(parents);
            }
            let body = serde_json::to_vec(&meta).map_err(|e| StorageError::Other(e.to_string()))?;
            let url = with_query(&format!("{}/files", self.cfg.endpoints.api_base), &[("fields", FILE_FIELDS)]);
            let created = self.api.retrying(ctx, Phase::Upload, "creating a folder", |_| {
                let req = Request::new(Method::Post, url.clone(), RequestKind::Create)
                    .body(&body, "application/json; charset=UTF-8");
                let mut resp = self.api.send(&req, ctx)?;
                match crate::http::status(&resp) {
                    200 | 201 => {
                        let raw: RawFile =
                            crate::http::read_json(&mut resp, 1 << 20, self.api.stats(), "creating a folder")?;
                        DriveFile::from_raw(raw).map(Some).map_err(StorageError::Other)
                    }
                    409 => {
                        crate::http::discard(resp, self.api.stats());
                        Ok(None)
                    }
                    _ => {
                        Err(crate::http::classify(&crate::http::api_error(resp, self.api.stats()), "creating a folder"))
                    }
                }
            })?;
            let folder = match created {
                Some(f) => Some(f),
                // 409: an earlier attempt with this ID already created it.
                None => self.get_by_id(&id, ctx, Phase::Upload)?,
            };
            match folder {
                Some(f) if f.id == id && f.is_folder => {
                    self.record_created(&f.id, key, true);
                    return Ok(f);
                }
                Some(_) => {
                    return Err(StorageError::Other("the server created an unexpected object for a folder".into()));
                }
                None => continue, // the ID is unusable; try a fresh one
            }
        }
        Err(StorageError::Transient("the folder could not be created".into()))
    }

    /// The root folder set, created on demand.
    fn root_set(&self, create: bool, fresh: Fresh, ctx: &OpCtx) -> StorageResult<Resolved> {
        if let Some(e) = self.cached_dir("")
            && self.is_fresh(&e, fresh)
        {
            let recent = self.entry_recent(&e);
            return Ok(Resolved::Found { ids: e.ids, fresh: recent });
        }
        let found = self.query_roots(ctx)?;
        if !found.is_empty() {
            self.store_dir("", &found);
            return Ok(Resolved::Found { ids: found, fresh: true });
        }
        if !create {
            return Ok(Resolved::Missing { fresh: true });
        }
        let _g = self.lock_folders(ctx)?;
        if let Some(e) = self.dirs.lock().get("").filter(|e| self.entry_recent(e)).cloned() {
            return Ok(Resolved::Found { ids: e.ids, fresh: true });
        }
        let mut found = self.query_roots(ctx)?;
        if found.is_empty() {
            let f = self.create_folder(None, &self.cfg.root_folder_name.clone(), "", true, ctx)?;
            tracing::info!(target: "packsync_drive", "created the Drive root folder");
            found = self.query_roots(ctx)?;
            if !found.iter().any(|r| r.id == f.id) {
                found.push(FolderRef { id: f.id, created_ms: f.created_ms });
                sort_folders(&mut found);
            }
        }
        self.store_dir("", &found);
        Ok(Resolved::Found { ids: found, fresh: true })
    }

    /// Resolve the folder set of directory `rel` (`""` = root).
    fn resolve_dir(&self, rel: &str, create: bool, fresh: Fresh, ctx: &OpCtx) -> StorageResult<Resolved> {
        let (mut parents, mut all_fresh) = match self.root_set(create, fresh, ctx)? {
            Resolved::Found { ids, fresh } => (ids, fresh),
            missing => return Ok(missing),
        };
        let mut path = String::new();
        for comp in rel.split('/').filter(|c| !c.is_empty()) {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(comp);
            if let Some(e) = self.cached_dir(&path)
                && self.is_fresh(&e, fresh)
            {
                all_fresh &= self.entry_recent(&e);
                parents = e.ids;
                continue;
            }
            let mut found = self.query_folders(&parents, comp, ctx)?;
            if found.is_empty() {
                if !create {
                    self.dirs.lock().remove(&path);
                    return Ok(Resolved::Missing { fresh: all_fresh });
                }
                let _g = self.lock_folders(ctx)?;
                if let Some(e) = self.dirs.lock().get(&path).filter(|e| self.entry_recent(e)).cloned() {
                    parents = e.ids;
                    continue;
                }
                found = self.query_folders(&parents, comp, ctx)?;
                if found.is_empty() {
                    let f = self.create_folder(Some(&parents[0].id), comp, &format!("{path}/"), false, ctx)?;
                    found = self.query_folders(&parents, comp, ctx)?;
                    if !found.iter().any(|r| r.id == f.id) {
                        found.push(FolderRef { id: f.id, created_ms: f.created_ms });
                        sort_folders(&mut found);
                    }
                }
            }
            self.store_dir(&path, &found);
            parents = found;
        }
        Ok(Resolved::Found { ids: parents, fresh: all_fresh })
    }

    // ----- object lookup -----------------------------------------------------------------

    fn remember_file(&self, key: &str, file: &DriveFile, duplicates: u32) {
        self.files.lock().insert(key.to_string(), CachedFile { file: file.clone(), duplicates });
    }

    fn forget_file(&self, key: &str) {
        self.files.lock().remove(key);
        self.state.clear(&self.state_key("file", key));
    }

    fn alive_match(f: &DriveFile, name: &str) -> bool {
        !f.trashed && !f.is_folder && f.name == name && f.is_ours()
    }

    /// Live objects for `key`, canonical first. Prefers a known file ID (strongly consistent),
    /// falls back to a name query (eventually consistent) in the folder set.
    fn lookup(&self, key: &str, ctx: &OpCtx, phase: Phase) -> StorageResult<(Vec<DriveFile>, u32)> {
        let (dir, name) = split_key(key);
        let cached = self.files.lock().get(key).cloned();
        let known_id = cached
            .as_ref()
            .map(|c| c.file.id.clone())
            .or_else(|| self.state.load(&self.state_key("file", key)).filter(|id| is_valid_drive_id(id)));
        if let Some(id) = known_id {
            match self.get_by_id(&id, ctx, phase)? {
                Some(f) if Self::alive_match(&f, name) => {
                    let dups = cached.map(|c| c.duplicates).unwrap_or(0);
                    self.remember_file(key, &f, dups);
                    return Ok((vec![f], dups));
                }
                _ => self.forget_file(key),
            }
        }
        for attempt in 0..2 {
            let fresh = if attempt == 0 { Fresh::Any } else { Fresh::Ttl };
            match self.resolve_dir(dir, false, fresh, ctx)? {
                Resolved::Found { ids, fresh: was_fresh } => {
                    let files = self.find_files(&ids, name, ctx, phase)?;
                    if let Some(first) = files.first() {
                        let dups = (files.len() - 1) as u32;
                        self.remember_file(key, first, dups);
                        return Ok((files, dups));
                    }
                    if was_fresh {
                        break;
                    }
                }
                Resolved::Missing { fresh: was_fresh } => {
                    if was_fresh {
                        break;
                    }
                }
            }
        }
        Ok((Vec::new(), 0))
    }

    // ----- operations ----------------------------------------------------------------------

    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        validate_key(prefix)?;
        if !prefix.ends_with('/') {
            return Err(StorageError::Other(format!("list prefix {prefix:?} must end with '/'")));
        }
        let rel = prefix.trim_end_matches('/');
        let dir = match self.resolve_dir(rel, false, Fresh::Ttl, ctx)? {
            Resolved::Found { ids, .. } => ids,
            Resolved::Missing { .. } => return Ok(Vec::new()),
        };
        let mut children = Vec::new();
        let mut incomplete = false;
        for group in dir.chunks(PARENTS_PER_QUERY) {
            let q = format!(
                "trashed = false and appProperties has {{ key='{PROP_MARKER}' and value='1' }} and {}",
                Self::parents_clause(group)
            );
            let (files, inc) = self.list_query(&q, ctx, Phase::Discovery, "listing a folder")?;
            incomplete |= inc;
            children.extend(files);
        }
        if incomplete {
            let msg = format!("Drive reported an incomplete listing for {prefix}; some entries may be missing");
            tracing::warn!(target: "packsync_drive", "{msg}");
            ctx.note(Phase::Discovery, Level::Warn, msg);
        }
        let mut folders: BTreeMap<String, Vec<FolderRef>> = BTreeMap::new();
        let mut files: BTreeMap<String, Vec<DriveFile>> = BTreeMap::new();
        let mut seen = HashSet::new();
        for f in children {
            if f.trashed || !f.is_ours() || !seen.insert(f.id.clone()) {
                continue;
            }
            if !is_valid_component(&f.name) || validate_key(&format!("{prefix}{}", f.name)).is_err() {
                tracing::debug!(target: "packsync_drive", "ignoring an entry whose name is not a valid key component");
                continue;
            }
            if f.is_folder {
                folders.entry(f.name.clone()).or_default().push(FolderRef { id: f.id, created_ms: f.created_ms });
            } else {
                files.entry(f.name.clone()).or_default().push(f);
            }
        }
        let mut out = Vec::with_capacity(folders.len() + files.len());
        for (name, mut ids) in folders {
            sort_folders(&mut ids);
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            self.store_dir(&path, &ids);
            let mut info = ObjectInfo::file(format!("{prefix}{name}/"), 0);
            info.is_dir = true;
            info.duplicates = (ids.len() - 1) as u32;
            info.provider_ref = Some(ids[0].id.clone());
            info.created_ms = ids[0].created_ms;
            out.push(info);
        }
        for (name, mut group) in files {
            sort_canonical(&mut group);
            let key = format!("{prefix}{name}");
            let dups = (group.len() - 1) as u32;
            self.remember_file(&key, &group[0], dups);
            out.push(object_info(&key, &group[0], dups));
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        validate_key(key)?;
        if key.ends_with('/') {
            return Ok(None);
        }
        let (files, dups) = self.lookup(key, ctx, Phase::Discovery)?;
        Ok(files.first().map(|f| object_info(key, f, dups)))
    }

    fn get(self: &Arc<Self>, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        validate_key(key)?;
        if key.ends_with('/') {
            return Err(StorageError::Other(format!("cannot read directory key {key:?}")));
        }
        let (files, _) = self.lookup(key, ctx, Phase::StageDownload)?;
        let Some(f) = files.into_iter().next() else {
            return Err(StorageError::NotFound(key.to_string()));
        };
        let size = f.size.ok_or_else(|| StorageError::Corrupt(format!("{key}: the server reports no size")))?;
        let (start, len) = match range {
            None => (0, size),
            Some((s, l)) => {
                if s.checked_add(l).is_none_or(|e| e > size) {
                    return Err(StorageError::Other(format!("range {s}+{l} is outside object {key} of {size} bytes")));
                }
                (s, l)
            }
        };
        if len == 0 {
            return Ok(Box::new(std::io::empty()));
        }
        let expected_md5 = if start == 0 && len == size { f.md5.clone() } else { None };
        match download::DownloadReader::open(self.clone(), f.id.clone(), size, start, start + len, expected_md5, ctx) {
            Ok(r) => Ok(Box::new(r)),
            Err(StorageError::NotFound(m)) => {
                self.forget_file(key);
                Err(StorageError::NotFound(m))
            }
            Err(e) => Err(e),
        }
    }

    fn delete_one(&self, f: &DriveFile, mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        if !f.is_ours() {
            tracing::warn!(target: "packsync_drive", "refusing to delete an object that PackSync did not create");
            return Ok(());
        }
        let url = format!("{}/files/{}", self.cfg.endpoints.api_base, f.id);
        if mode == DeleteMode::Trash && self.cfg.space.supports_trash() {
            let body = br#"{"trashed":true}"#;
            let url = with_query(&url, &[("fields", "id")]);
            let resp = self.api.call(ctx, Phase::Cleanup, "moving an object to the trash", &[200, 404], || {
                Request::new(Method::Patch, url.clone(), RequestKind::Update)
                    .body(body, "application/json; charset=UTF-8")
            })?;
            crate::http::discard(resp, self.api.stats());
        } else {
            let resp = self.api.call(ctx, Phase::Cleanup, "deleting an object", &[200, 204, 404], || {
                Request::new(Method::Delete, url.clone(), RequestKind::Delete)
            })?;
            crate::http::discard(resp, self.api.stats());
        }
        Ok(())
    }

    fn delete(&self, key: &str, mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        validate_key(key)?;
        if key.ends_with('/') {
            return Err(StorageError::Other(format!("cannot delete directory key {key:?}")));
        }
        let _g = self.key_locks.acquire(key, ctx)?;
        let (dir, name) = split_key(key);
        let mut targets: Vec<DriveFile> = Vec::new();
        let known = self.files.lock().get(key).map(|c| c.file.id.clone());
        if let Some(id) = known.or_else(|| self.state.load(&self.state_key("file", key)))
            && let Some(f) = self.get_by_id(&id, ctx, Phase::Cleanup)?
            && Self::alive_match(&f, name)
        {
            targets.push(f);
        }
        if let Resolved::Found { ids, .. } = self.resolve_dir(dir, false, Fresh::Ttl, ctx)? {
            for f in self.find_files(&ids, name, ctx, Phase::Cleanup)? {
                if !targets.iter().any(|t| t.id == f.id) {
                    targets.push(f);
                }
            }
        }
        for f in &targets {
            self.delete_one(f, mode, ctx)?;
        }
        self.forget_file(key);
        Ok(())
    }

    fn quota(&self, ctx: &OpCtx) -> StorageResult<Option<Quota>> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let url = with_query(&format!("{}/about", self.cfg.endpoints.api_base), &[("fields", "storageQuota")]);
        let about: RawAbout = self.api.json(ctx, Phase::Discovery, "reading the storage quota", || {
            Request::new(Method::Get, url.clone(), RequestKind::Other)
        })?;
        let q = about.storage_quota.unwrap_or_default();
        Ok(Some(Quota { used: q.usage.unwrap_or(0), total: q.limit }))
    }

    fn update_tags(&self, key: &str, tags: &BTreeMap<String, String>, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        validate_key(key)?;
        let props = app_properties(tags)?;
        let _g = self.key_locks.acquire(key, ctx)?;
        let (files, dups) = self.lookup(key, ctx, Phase::Upload)?;
        let Some(f) = files.first() else { return Err(StorageError::NotFound(key.to_string())) };
        let body = serde_json::to_vec(&serde_json::json!({ "appProperties": props }))
            .map_err(|e| StorageError::Other(e.to_string()))?;
        let url = with_query(&format!("{}/files/{}", self.cfg.endpoints.api_base, f.id), &[("fields", FILE_FIELDS)]);
        let raw: RawFile = self.api.json(ctx, Phase::Upload, "updating object tags", || {
            Request::new(Method::Patch, url.clone(), RequestKind::Update).body(&body, "application/json; charset=UTF-8")
        })?;
        let updated = DriveFile::from_raw(raw).map_err(StorageError::Other)?;
        if updated.id != f.id {
            return Err(StorageError::Other("the server updated an unexpected object".into()));
        }
        self.remember_file(key, &updated, dups);
        Ok(object_info(key, &updated, dups))
    }

    /// A pre-generated file ID (`files.generateIds`), pooled.
    fn next_id(&self, ctx: &OpCtx) -> StorageResult<String> {
        if let Some(id) = self.id_pool.lock().pop() {
            return Ok(id);
        }
        let url = with_query(
            &format!("{}/files/generateIds", self.cfg.endpoints.api_base),
            &[("count", "10"), ("space", self.api_space()), ("type", "files")],
        );
        let raw: RawGeneratedIds = self.api.json(ctx, Phase::Upload, "generating file IDs", || {
            Request::new(Method::Get, url.clone(), RequestKind::Other)
        })?;
        let ids: Vec<String> = raw.ids.into_iter().filter(|i| is_valid_drive_id(i)).take(1000).collect();
        let mut pool = self.id_pool.lock();
        pool.extend(ids);
        pool.pop().ok_or_else(|| StorageError::Other("the server returned no usable file IDs".into()))
    }

    fn delete_created(&self, ctx: &OpCtx) -> StorageResult<usize> {
        let created = self.created.lock().clone();
        let mut n = 0;
        let order = created.iter().rev().filter(|c| !c.is_folder).chain(created.iter().rev().filter(|c| c.is_folder));
        for c in order {
            if let Some(f) = self.get_by_id(&c.id, ctx, Phase::Cleanup)?
                && f.is_ours()
            {
                self.delete_one(&f, DeleteMode::Permanent, ctx)?;
                n += 1;
            }
        }
        self.created.lock().clear();
        self.dirs.lock().clear();
        self.files.lock().clear();
        Ok(n)
    }
}

impl Storage for DriveStorage {
    fn provider_id(&self) -> String {
        self.inner.pid.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: "google-drive".into(),
            unique_keys: false,
            // Inferred from Drive's revision model (an upload becomes the head revision only when
            // complete); not verified against live Drive.
            atomic_replace: true,
            conditional_write: false,
            range_reads: true,
            resumable_uploads: true,
            // sha256Checksum is documented but only verified when the server returns it.
            server_checksums: vec![ChecksumKind::Md5],
            // Undocumented by Google; treated as eventual (see docs/dev/drive-adapter.md).
            list_consistency: Consistency::Eventual,
            max_object_size: Some(DRIVE_MAX_FILE_SIZE),
            recommended_concurrency: 4,
            recommended_pack_size: 16 * 1024 * 1024,
        }
    }

    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>> {
        self.inner.list(prefix, ctx)
    }

    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        self.inner.stat(key, ctx)
    }

    fn get(&self, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>> {
        self.inner.get(key, range, ctx)
    }

    fn put(&self, key: &str, src: PutSource<'_>, opts: &PutOptions, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        upload::put(&self.inner, key, src, opts, ctx)
    }

    fn delete(&self, key: &str, mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        self.inner.delete(key, mode, ctx)
    }

    fn quota(&self, ctx: &OpCtx) -> StorageResult<Option<Quota>> {
        self.inner.quota(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_splitting_and_components() {
        assert_eq!(split_key("a/b/c.pack"), ("a/b", "c.pack"));
        assert_eq!(split_key("top"), ("", "top"));
        assert!(is_valid_component("x.pack"));
        for bad in ["", ".", "..", "a b", "a/b", "ü", "a'b"] {
            assert!(!is_valid_component(bad), "{bad:?}");
        }
    }

    #[test]
    fn tags_are_validated_and_truncated_deterministically() {
        let mut t = BTreeMap::new();
        t.insert("kind".to_string(), "commit".to_string());
        t.insert("parents".to_string(), "é".repeat(100));
        let p = app_properties(&t).unwrap();
        assert_eq!(p["ps"], "1");
        let v = p["parents"].as_str().unwrap();
        assert!(v.len() + "parents".len() <= 124 && v.len() >= 115, "{}", v.len());
        assert_eq!(app_properties(&t).unwrap(), p, "deterministic");
        t.insert("ps".into(), "0".into());
        assert!(app_properties(&t).is_err(), "reserved key");
        let mut bad = BTreeMap::new();
        bad.insert("a b".to_string(), "x".to_string());
        assert!(app_properties(&bad).is_err());
        let many: BTreeMap<String, String> = (0..30).map(|i| (format!("k{i}"), "v".into())).collect();
        assert!(app_properties(&many).is_err());
    }

    #[test]
    fn folder_refs_sort_canonically() {
        let mut v = vec![
            FolderRef { id: "b".into(), created_ms: Some(2) },
            FolderRef { id: "a".into(), created_ms: Some(2) },
            FolderRef { id: "c".into(), created_ms: Some(1) },
            FolderRef { id: "a".into(), created_ms: Some(2) },
        ];
        sort_folders(&mut v);
        let ids: Vec<_> = v.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b"]);
    }
}
