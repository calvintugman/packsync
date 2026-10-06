//! `put`: hashing pre-pass, `CreateOnly` / `Replace` semantics, multipart uploads (small objects)
//! and resumable uploads with persisted sessions, verification against the server's size / MD5 /
//! SHA-256, and removal of objects that fail verification.
//!
//! Idempotency: every new object gets an ID from `files.generateIds` *before* the first request,
//! persisted with the upload state. A retry after an ambiguous failure re-uses it; Drive answers
//! `409` when that ID already exists, and the existing object is then verified instead of
//! creating a duplicate.

use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use md5::{Digest, Md5};
use packsync_core::hash::{Hash, Hasher};
use packsync_core::progress::{OpCtx, Phase};
use packsync_core::redact::Secret;
use packsync_core::storage::*;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::{Fresh, Inner, Resolved, app_properties, object_info, split_key};
use crate::config::DRIVE_MAX_FILE_SIZE;
use crate::http::{self, Method, Request};
use crate::model::{DriveFile, FILE_FIELDS, RawFile};
use crate::stats::RequestKind;
use crate::util::{is_valid_drive_id, random_urlsafe, with_query};

const OCTET: &str = "application/octet-stream";
type Props = serde_json::Map<String, serde_json::Value>;

/// Content hashes from the pre-pass.
struct Hashes {
    md5: String,
    sha256: String,
    blake3: Hash,
}

/// Upload source with random access (multipart bodies and resumable chunks).
enum Source<'a> {
    Mem(Cow<'a, [u8]>),
    File { file: File, len: u64 },
}

fn source_changed() -> StorageError {
    StorageError::Corrupt("the source changed while it was being uploaded".into())
}

impl<'a> Source<'a> {
    /// Small files are read into memory once (they are sent in one multipart request anyway).
    fn open(src: &PutSource<'a>, small: u64) -> StorageResult<Source<'a>> {
        match src {
            PutSource::Bytes(b) => Ok(Source::Mem(Cow::Borrowed(b))),
            PutSource::File { path, len } if *len <= small => {
                let f = File::open(path)?;
                let mut v = Vec::with_capacity(*len as usize);
                f.take(len + 1).read_to_end(&mut v)?;
                if v.len() as u64 != *len {
                    return Err(StorageError::Corrupt(format!(
                        "the source file has {} bytes, {len} were declared",
                        v.len()
                    )));
                }
                Ok(Source::Mem(Cow::Owned(v)))
            }
            PutSource::File { path, len } => Ok(Source::File { file: File::open(path)?, len: *len }),
        }
    }

    fn len(&self) -> u64 {
        match self {
            Source::Mem(b) => b.len() as u64,
            Source::File { len, .. } => *len,
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Source::Mem(b) => Some(b),
            Source::File { .. } => None,
        }
    }

    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> StorageResult<()> {
        match self {
            Source::Mem(b) => {
                let start = off as usize;
                let end = start.checked_add(buf.len()).filter(|e| *e <= b.len()).ok_or_else(source_changed)?;
                buf.copy_from_slice(&b[start..end]);
                Ok(())
            }
            Source::File { file, .. } => {
                file.seek(SeekFrom::Start(off))?;
                file.read_exact(buf).map_err(|e| match e.kind() {
                    std::io::ErrorKind::UnexpectedEof => source_changed(),
                    _ => StorageError::io(e),
                })
            }
        }
    }

    /// MD5 + SHA-256 + BLAKE3 over the whole source in bounded blocks.
    fn hash_all(&mut self, ctx: &OpCtx) -> StorageResult<Hashes> {
        let mut md5 = Md5::new();
        let mut sha = Sha256::new();
        let mut b3 = Hasher::new();
        let mut feed = |d: &[u8]| {
            md5.update(d);
            sha.update(d);
            b3.update(d);
        };
        match self {
            Source::Mem(b) => {
                for block in b.chunks(1 << 20) {
                    if ctx.is_cancelled() {
                        return Err(StorageError::Cancelled);
                    }
                    feed(block);
                }
            }
            Source::File { file, len } => {
                file.seek(SeekFrom::Start(0))?;
                let mut buf = vec![0u8; 1 << 20];
                let mut total = 0u64;
                loop {
                    if ctx.is_cancelled() {
                        return Err(StorageError::Cancelled);
                    }
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    total += n as u64;
                    if total > *len {
                        return Err(StorageError::Corrupt(format!(
                            "the source file is larger than the declared {len} bytes"
                        )));
                    }
                    feed(&buf[..n]);
                }
                if total != *len {
                    return Err(StorageError::Corrupt(format!(
                        "the source file has {total} bytes, {len} were declared"
                    )));
                }
            }
        }
        Ok(Hashes { md5: hex::encode(md5.finalize()), sha256: hex::encode(sha.finalize()), blake3: b3.finalize() })
    }
}

/// MD5 of the bytes actually sent, valid only for a single in-order pass.
struct StreamMd5 {
    h: Md5,
    upto: u64,
    valid: bool,
}

impl StreamMd5 {
    fn new() -> Self {
        Self { h: Md5::new(), upto: 0, valid: true }
    }
    fn feed(&mut self, off: u64, data: &[u8]) {
        if !self.valid {
            return;
        }
        if off != self.upto {
            self.valid = false;
            return;
        }
        self.h.update(data);
        self.upto += data.len() as u64;
    }
    fn finish(self, total: u64) -> Option<String> {
        (self.valid && self.upto == total).then(|| hex::encode(self.h.finalize()))
    }
}

/// Persisted state of an in-flight upload. Contains the session URI, which is a secret: this
/// type deliberately has no `Debug` implementation and is never logged.
#[derive(Serialize, Deserialize, Clone)]
struct UploadState {
    v: u32,
    mode: String,
    file_id: String,
    size: u64,
    md5: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    offset: u64,
}

impl UploadState {
    fn new(mode: &str, file_id: String, size: u64, md5: &str) -> Self {
        Self { v: 1, mode: mode.into(), file_id, size, md5: md5.into(), session: None, offset: 0 }
    }
}

fn load_state(inner: &Inner, skey: &str) -> Option<UploadState> {
    let raw = inner.state.load(skey)?;
    let mut st: UploadState = serde_json::from_str(&raw).ok()?;
    if st.v != 1 || !is_valid_drive_id(&st.file_id) {
        return None;
    }
    if st.session.as_deref().is_some_and(|s| s.len() > 8192 || !inner.cfg.endpoints.url_rules().allows(s)) {
        st.session = None;
        st.offset = 0;
    }
    Some(st)
}

fn save_state(inner: &Inner, skey: &str, st: &UploadState) {
    if let Ok(json) = serde_json::to_string(st) {
        inner.state.save(skey, &json);
    }
}

enum Outcome {
    /// The object as the server reports it, plus the MD5 of what this process streamed (if the
    /// upload was a single in-order pass).
    Done(Box<DriveFile>, Option<String>),
    /// `409`: an object with the pre-generated ID already exists.
    AlreadyExists,
}

enum Target<'a> {
    Create { parent: &'a str, name: &'a str, props: &'a Props },
    Update { id: &'a str, props: &'a Props },
}

enum PutErr {
    /// Recoverable once by re-resolving folders / IDs.
    Retry(String),
    Fail(StorageError),
}

impl From<StorageError> for PutErr {
    fn from(e: StorageError) -> Self {
        PutErr::Fail(e)
    }
}

enum VerifyErr {
    ParentTrashed,
    Bad(String),
    Failed(StorageError),
}

pub(super) fn put(
    inner: &Arc<Inner>,
    key: &str,
    src: PutSource<'_>,
    opts: &PutOptions,
    ctx: &OpCtx,
) -> StorageResult<ObjectInfo> {
    if ctx.is_cancelled() {
        return Err(StorageError::Cancelled);
    }
    validate_key(key)?;
    if key.ends_with('/') {
        return Err(StorageError::Other(format!("cannot put directory key {key:?}")));
    }
    let len = src.len();
    if let Some(d) = &opts.digest
        && d.size != len
    {
        return Err(StorageError::Corrupt(format!("{key}: declared size {} != source size {len}", d.size)));
    }
    if len > DRIVE_MAX_FILE_SIZE {
        return Err(StorageError::Other(format!("{key}: {len} bytes exceed Drive's maximum file size")));
    }
    let props = app_properties(&opts.tags)?;
    let mut source = Source::open(&src, inner.cfg.multipart_threshold)?;
    let hashes = source.hash_all(ctx)?;
    if let Some(d) = &opts.digest
        && d.blake3 != hashes.blake3
    {
        return Err(StorageError::Corrupt(format!("{key}: content does not match the declared digest")));
    }
    let _guard = inner.key_locks.acquire(key, ctx)?;
    let mut recovered = false;
    loop {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        match put_once(inner, key, &mut source, &hashes, &props, opts.mode, ctx) {
            Ok((file, dups)) => {
                let mut info = object_info(key, &file, dups);
                info.checksums.blake3 = Some(hashes.blake3.to_hex());
                return Ok(info);
            }
            Err(PutErr::Retry(reason)) if !recovered => {
                tracing::info!(target: "packsync_drive", "{key}: {reason}; retrying once with fresh folder data");
                recovered = true;
            }
            Err(PutErr::Retry(reason)) => return Err(StorageError::Transient(format!("{key}: {reason}"))),
            Err(PutErr::Fail(e)) => return Err(e),
        }
    }
}

fn put_once(
    inner: &Arc<Inner>,
    key: &str,
    source: &mut Source<'_>,
    h: &Hashes,
    props: &Props,
    mode: PutMode,
    ctx: &OpCtx,
) -> Result<(DriveFile, u32), PutErr> {
    let (dir, name) = split_key(key);
    let parent = match inner.resolve_dir(dir, true, Fresh::Any, ctx)? {
        Resolved::Found { ids, .. } => ids[0].id.clone(),
        Resolved::Missing { .. } => return Err(PutErr::Retry("the folder could not be resolved".into())),
    };
    let (existing, dups) = inner.lookup(key, ctx, Phase::Upload)?;
    match (mode, existing.first()) {
        (PutMode::CreateOnly, Some(f)) => {
            inner.state.clear(&inner.state_key("up", key));
            if same_content(inner, f, h, source.len(), ctx)? {
                Ok((f.clone(), dups))
            } else {
                Err(PutErr::Fail(StorageError::Conflict(format!("{key} already exists with different content"))))
            }
        }
        (PutMode::Replace, Some(f)) => update_existing(inner, key, name, f, dups, source, h, props, ctx),
        (_, None) => create_new(inner, key, &parent, dir, name, source, h, props, mode, ctx).map(|f| (f, 0)),
    }
}

fn same_content(inner: &Arc<Inner>, f: &DriveFile, h: &Hashes, len: u64, ctx: &OpCtx) -> StorageResult<bool> {
    if f.size != Some(len) {
        return Ok(false);
    }
    if f.sha256.as_ref().is_some_and(|s| s != &h.sha256) {
        return Ok(false);
    }
    match &f.md5 {
        Some(m) => Ok(m == &h.md5),
        None => Ok(download_md5(inner, &f.id, len, ctx)? == h.md5),
    }
}

/// Read an object back and hash it (used only when the server reports no MD5).
fn download_md5(inner: &Arc<Inner>, id: &str, len: u64, ctx: &OpCtx) -> StorageResult<String> {
    let mut md5 = Md5::new();
    if len > 0 {
        let mut body = inner.open_media(id, len, 0, len, ctx)?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut got = 0u64;
        loop {
            if ctx.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let n = body
                .read(&mut buf)
                .map_err(|e| StorageError::Offline(format!("reading back an object failed: {e}")))?;
            if n == 0 {
                break;
            }
            got += n as u64;
            if got > len {
                break;
            }
            md5.update(&buf[..n]);
            inner.api.stats().down(n as u64);
        }
        if got != len {
            return Err(StorageError::Corrupt(format!("the stored object has {got} bytes, {len} expected")));
        }
    }
    Ok(hex::encode(md5.finalize()))
}

fn verify(
    inner: &Arc<Inner>,
    file: &DriveFile,
    expect_id: &str,
    name: &str,
    h: &Hashes,
    len: u64,
    ctx: &OpCtx,
) -> Result<(), VerifyErr> {
    if file.id != expect_id {
        return Err(VerifyErr::Bad("the server stored the object under an unexpected ID".into()));
    }
    if file.is_folder || file.name != name {
        return Err(VerifyErr::Bad("the server stored unexpected metadata".into()));
    }
    if file.trashed {
        return Err(VerifyErr::ParentTrashed);
    }
    if file.size != Some(len) {
        return Err(VerifyErr::Bad(format!("size mismatch (sent {len} bytes, the server reports {:?})", file.size)));
    }
    if file.sha256.as_ref().is_some_and(|s| s != &h.sha256) {
        return Err(VerifyErr::Bad("SHA-256 mismatch between the sent bytes and the server's checksum".into()));
    }
    let md5 = match &file.md5 {
        Some(m) => m.clone(),
        // Not reported in the response: ask once more, then verify by reading the bytes back.
        None => match inner.get_by_id(&file.id, ctx, Phase::Verify) {
            Ok(Some(DriveFile { md5: Some(m), .. })) => m,
            Ok(_) => download_md5(inner, &file.id, len, ctx).map_err(VerifyErr::Failed)?,
            Err(e) => return Err(VerifyErr::Failed(e)),
        },
    };
    if md5 != h.md5 {
        return Err(VerifyErr::Bad("MD5 mismatch between the sent bytes and the server's checksum".into()));
    }
    Ok(())
}

/// Permanently delete an object this call just created and that failed verification.
fn delete_created_object(inner: &Arc<Inner>, id: &str, ctx: &OpCtx) -> bool {
    let url = format!("{}/files/{id}", inner.cfg.endpoints.api_base);
    let res =
        inner.api.call(ctx, Phase::Cleanup, "removing an object that failed verification", &[200, 204, 404], || {
            Request::new(Method::Delete, url.clone(), RequestKind::Delete)
        });
    match res {
        Ok(resp) => {
            http::discard(resp, inner.api.stats());
            true
        }
        Err(e) => {
            tracing::warn!(target: "packsync_drive", "could not remove an object that failed verification: {e}");
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create_new(
    inner: &Arc<Inner>,
    key: &str,
    parent: &str,
    dir: &str,
    name: &str,
    source: &mut Source<'_>,
    h: &Hashes,
    props: &Props,
    mode: PutMode,
    ctx: &OpCtx,
) -> Result<DriveFile, PutErr> {
    let len = source.len();
    let skey = inner.state_key("up", key);
    let existing = load_state(inner, &skey).filter(|s| s.mode == "create" && s.size == len && s.md5 == h.md5);
    let mut st = match existing {
        Some(s) => s,
        None => {
            inner.state.clear(&skey);
            let s = UploadState::new("create", inner.next_id(ctx)?, len, &h.md5);
            save_state(inner, &skey, &s);
            s
        }
    };
    if mode == PutMode::Replace {
        // Single-writer key: remember the ID before the create so that a create whose response
        // was lost is found (by ID, strongly consistent) instead of duplicated.
        inner.state.save(&inner.state_key("file", key), &st.file_id);
    }
    let outcome = match source.bytes() {
        Some(data) if len <= inner.cfg.multipart_threshold && st.session.is_none() => {
            multipart_create(inner, &st.file_id, parent, name, props, data, ctx)
        }
        _ => resumable(inner, key, &skey, &mut st, Target::Create { parent, name, props }, source, ctx),
    };
    let (file, streamed) = match outcome {
        Ok(Outcome::Done(f, streamed)) => (*f, streamed),
        Ok(Outcome::AlreadyExists) => match inner.get_by_id(&st.file_id, ctx, Phase::Upload)? {
            Some(f) => (f, None),
            None => {
                inner.state.clear(&skey);
                return Err(PutErr::Retry("a pre-generated ID could not be used".into()));
            }
        },
        Err(StorageError::NotFound(_)) => {
            inner.state.clear(&skey);
            inner.invalidate_dir_chain(dir);
            return Err(PutErr::Retry("the parent folder no longer exists".into()));
        }
        Err(e) => {
            if matches!(e, StorageError::Corrupt(_) | StorageError::Conflict(_) | StorageError::Other(_)) {
                inner.state.clear(&skey);
            }
            return Err(PutErr::Fail(e));
        }
    };
    let verdict = match streamed {
        Some(m) if m != h.md5 => Err(VerifyErr::Bad("the source changed while it was being uploaded".into())),
        _ => verify(inner, &file, &st.file_id, name, h, len, ctx),
    };
    match verdict {
        Ok(()) => {}
        Err(VerifyErr::ParentTrashed) => {
            delete_created_object(inner, &file.id, ctx);
            inner.state.clear(&skey);
            inner.invalidate_dir_chain(dir);
            return Err(PutErr::Retry("the parent folder is in the trash".into()));
        }
        Err(VerifyErr::Bad(msg)) => {
            let removed = delete_created_object(inner, &file.id, ctx);
            inner.state.clear(&skey);
            if mode == PutMode::Replace {
                inner.forget_file(key);
            }
            let tail = if removed { "the uploaded object was removed" } else { "removing the uploaded object failed" };
            return Err(PutErr::Fail(StorageError::Corrupt(format!("{key}: {msg}; {tail}"))));
        }
        Err(VerifyErr::Failed(e)) => return Err(PutErr::Fail(e)),
    }
    inner.state.clear(&skey);
    inner.record_created(&file.id, key, false);
    inner.remember_file(key, &file, 0);
    Ok(file)
}

#[allow(clippy::too_many_arguments)]
fn update_existing(
    inner: &Arc<Inner>,
    key: &str,
    name: &str,
    existing: &DriveFile,
    dups: u32,
    source: &mut Source<'_>,
    h: &Hashes,
    props: &Props,
    ctx: &OpCtx,
) -> Result<(DriveFile, u32), PutErr> {
    let len = source.len();
    let skey = inner.state_key("up", key);
    let outcome = match source.bytes() {
        Some(data) if len <= inner.cfg.multipart_threshold => multipart_update(inner, &existing.id, props, data, ctx),
        _ => {
            let mut st = load_state(inner, &skey)
                .filter(|s| s.mode == "update" && s.file_id == existing.id && s.size == len && s.md5 == h.md5)
                .unwrap_or_else(|| UploadState::new("update", existing.id.clone(), len, &h.md5));
            save_state(inner, &skey, &st);
            resumable(inner, key, &skey, &mut st, Target::Update { id: &existing.id, props }, source, ctx)
        }
    };
    let (file, streamed) = match outcome {
        Ok(Outcome::Done(f, s)) => (*f, s),
        Ok(Outcome::AlreadyExists) => {
            return Err(PutErr::Fail(StorageError::Other("unexpected conflict while updating".into())));
        }
        Err(StorageError::NotFound(_)) => {
            inner.state.clear(&skey);
            inner.forget_file(key);
            return Err(PutErr::Retry("the object disappeared while it was being replaced".into()));
        }
        Err(e) => return Err(PutErr::Fail(e)),
    };
    let verdict = match streamed {
        Some(m) if m != h.md5 => Err(VerifyErr::Bad("the source changed while it was being uploaded".into())),
        _ => verify(inner, &file, &existing.id, name, h, len, ctx),
    };
    match verdict {
        Ok(()) => {}
        Err(VerifyErr::Failed(e)) => return Err(PutErr::Fail(e)),
        Err(VerifyErr::ParentTrashed) => {
            inner.forget_file(key);
            return Err(PutErr::Retry("the object is in the trash".into()));
        }
        Err(VerifyErr::Bad(msg)) => {
            // The object existed before this call, so it is not deleted; a later Replace fixes it.
            inner.state.clear(&skey);
            return Err(PutErr::Fail(StorageError::Corrupt(format!("{key}: {msg}"))));
        }
    }
    inner.state.clear(&skey);
    inner.state.save(&inner.state_key("file", key), &file.id);
    inner.remember_file(key, &file, dups);
    Ok((file, dups))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `multipart/related` body: JSON metadata part + media part.
fn multipart_body(meta: &serde_json::Value, data: &[u8]) -> StorageResult<(Vec<u8>, String)> {
    let meta = serde_json::to_vec(meta).map_err(|e| StorageError::Other(e.to_string()))?;
    for _ in 0..8 {
        let boundary = format!("packsync_{}", random_urlsafe(24).map_err(StorageError::Other)?);
        let delim = format!("--{boundary}");
        if contains(data, delim.as_bytes()) || contains(&meta, delim.as_bytes()) {
            continue;
        }
        let mut body = Vec::with_capacity(data.len() + meta.len() + 256);
        body.extend_from_slice(format!("{delim}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n").as_bytes());
        body.extend_from_slice(&meta);
        body.extend_from_slice(format!("\r\n{delim}\r\nContent-Type: {OCTET}\r\n\r\n").as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n{delim}--\r\n").as_bytes());
        return Ok((body, format!("multipart/related; boundary={boundary}")));
    }
    Err(StorageError::Other("could not choose a multipart boundary".into()))
}

fn parse_file(inner: &Inner, resp: &mut http::Response, what: &str) -> StorageResult<DriveFile> {
    let raw: RawFile = http::read_json(resp, 1 << 20, inner.api.stats(), what)?;
    DriveFile::from_raw(raw).map_err(StorageError::Other)
}

fn multipart_create(
    inner: &Arc<Inner>,
    id: &str,
    parent: &str,
    name: &str,
    props: &Props,
    data: &[u8],
    ctx: &OpCtx,
) -> StorageResult<Outcome> {
    let meta =
        serde_json::json!({"id": id, "name": name, "parents": [parent], "mimeType": OCTET, "appProperties": props});
    let (body, ct) = multipart_body(&meta, data)?;
    let url = with_query(
        &format!("{}/files", inner.cfg.endpoints.upload_base),
        &[("uploadType", "multipart"), ("fields", FILE_FIELDS)],
    );
    inner.api.retrying(ctx, Phase::Upload, "uploading an object", |_| {
        let req = Request::new(Method::Post, url.clone(), RequestKind::Create).body(&body, &ct);
        let res = inner.api.send(&req, ctx);
        ctx.add_up(data.len() as u64);
        let mut resp = res?;
        match http::status(&resp) {
            200 | 201 => Ok(Outcome::Done(Box::new(parse_file(inner, &mut resp, "uploading an object")?), None)),
            409 => {
                http::discard(resp, inner.api.stats());
                Ok(Outcome::AlreadyExists)
            }
            _ => Err(http::classify(&http::api_error(resp, inner.api.stats()), "uploading an object")),
        }
    })
}

fn multipart_update(inner: &Arc<Inner>, id: &str, props: &Props, data: &[u8], ctx: &OpCtx) -> StorageResult<Outcome> {
    let meta = serde_json::json!({ "appProperties": props });
    let (body, ct) = multipart_body(&meta, data)?;
    let url = with_query(
        &format!("{}/files/{id}", inner.cfg.endpoints.upload_base),
        &[("uploadType", "multipart"), ("fields", FILE_FIELDS)],
    );
    inner.api.retrying(ctx, Phase::Upload, "replacing an object", |_| {
        let req = Request::new(Method::Patch, url.clone(), RequestKind::Update).body(&body, &ct);
        let res = inner.api.send(&req, ctx);
        ctx.add_up(data.len() as u64);
        let mut resp = res?;
        match http::status(&resp) {
            200 | 201 => Ok(Outcome::Done(Box::new(parse_file(inner, &mut resp, "replacing an object")?), None)),
            _ => Err(http::classify(&http::api_error(resp, inner.api.stats()), "replacing an object")),
        }
    })
}

enum Next {
    Offset(u64),
    Done(Outcome),
    Expired,
}

enum Step {
    Init,
    Status,
    Send(u64),
}

/// Interpret a resumable-session response (`308` + `Range`, `200/201`, `404/410`).
fn session_response(inner: &Inner, mut resp: http::Response, total: u64, sent_end: Option<u64>) -> StorageResult<Next> {
    match http::status(&resp) {
        308 => {
            let range = http::header(&resp, "range");
            http::discard(resp, inner.api.stats());
            let Some(r) = range else { return Ok(Next::Offset(0)) };
            let confirmed = r
                .trim()
                .strip_prefix("bytes=0-")
                .and_then(|n| n.trim().parse::<u64>().ok())
                .and_then(|n| n.checked_add(1))
                .ok_or_else(|| StorageError::Other("the server sent an unreadable upload Range header".into()))?;
            if confirmed > total || sent_end.is_some_and(|e| confirmed > e) {
                return Err(StorageError::Other("the server confirmed bytes that were never sent".into()));
            }
            Ok(Next::Offset(confirmed))
        }
        200 | 201 => {
            Ok(Next::Done(Outcome::Done(Box::new(parse_file(inner, &mut resp, "finishing an upload")?), None)))
        }
        404 | 410 => {
            http::discard(resp, inner.api.stats());
            Ok(Next::Expired)
        }
        _ => Err(http::classify(&http::api_error(resp, inner.api.stats()), "uploading an object")),
    }
}

/// Start a session. `Ok(None)` = `409` (the pre-generated ID already exists).
fn init_session(
    inner: &Inner,
    st: &UploadState,
    target: &Target<'_>,
    total: u64,
    ctx: &OpCtx,
) -> StorageResult<Option<String>> {
    let (method, url, meta) = match target {
        Target::Create { parent, name, props } => (
            Method::Post,
            with_query(
                &format!("{}/files", inner.cfg.endpoints.upload_base),
                &[("uploadType", "resumable"), ("fields", FILE_FIELDS)],
            ),
            serde_json::json!({"id": st.file_id, "name": name, "parents": [parent], "mimeType": OCTET, "appProperties": props}),
        ),
        Target::Update { id, props } => (
            Method::Patch,
            with_query(
                &format!("{}/files/{id}", inner.cfg.endpoints.upload_base),
                &[("uploadType", "resumable"), ("fields", FILE_FIELDS)],
            ),
            serde_json::json!({ "appProperties": props }),
        ),
    };
    let body = serde_json::to_vec(&meta).map_err(|e| StorageError::Other(e.to_string()))?;
    let req = Request::new(method, url, RequestKind::ResumableInit)
        .header("X-Upload-Content-Type", OCTET)
        .header("X-Upload-Content-Length", total.to_string())
        .body(&body, "application/json; charset=UTF-8");
    let resp = inner.api.send(&req, ctx)?;
    match http::status(&resp) {
        200 | 201 => {
            let loc = http::header(&resp, "location");
            http::discard(resp, inner.api.stats());
            match loc {
                Some(l) if l.len() <= 8192 && inner.cfg.endpoints.url_rules().allows(&l) => Ok(Some(l)),
                _ => Err(StorageError::Other(
                    "the server returned an unusable upload session address (missing, or not on an allowed host)"
                        .into(),
                )),
            }
        }
        409 if matches!(target, Target::Create { .. }) => {
            http::discard(resp, inner.api.stats());
            Ok(None)
        }
        _ => Err(http::classify(&http::api_error(resp, inner.api.stats()), "starting an upload")),
    }
}

/// Resumable upload with persisted session. Bounded: consecutive failures use the retry policy,
/// sessions are restarted at most twice, and a server that stops confirming progress is abandoned.
fn resumable(
    inner: &Arc<Inner>,
    key: &str,
    skey: &str,
    st: &mut UploadState,
    target: Target<'_>,
    source: &mut Source<'_>,
    ctx: &OpCtx,
) -> StorageResult<Outcome> {
    let total = source.len();
    let chunk = inner.cfg.chunk_size;
    let mut buf = vec![0u8; chunk.min(total.max(1)) as usize];
    let mut stream = StreamMd5::new();
    let mut step = if st.session.is_some() { Step::Status } else { Step::Init };
    let mut confirmed = st.offset;
    let (mut failures, mut restarts, mut stalls) = (0u32, 0u32, 0u32);
    loop {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let session = st.session.clone().map(Secret::new);
        let res: StorageResult<Next> = match (&step, &session) {
            (Step::Init, _) | (_, None) => init_session(inner, st, &target, total, ctx).map(|s| match s {
                Some(uri) => {
                    st.session = Some(uri);
                    st.offset = 0;
                    save_state(inner, skey, st);
                    Next::Offset(0)
                }
                None => Next::Done(Outcome::AlreadyExists),
            }),
            (Step::Status, Some(s)) => {
                let req = Request::new(Method::Put, s.expose().to_string(), RequestKind::ResumableStatus)
                    .header("Content-Range", format!("bytes */{total}"))
                    .body(&[], OCTET);
                inner.api.send(&req, ctx).and_then(|resp| session_response(inner, resp, total, None))
            }
            (Step::Send(off), Some(s)) => {
                let off = *off;
                let end = (off + chunk).min(total);
                let n = (end - off) as usize;
                source.read_at(off, &mut buf[..n])?;
                stream.feed(off, &buf[..n]);
                let req = Request::new(Method::Put, s.expose().to_string(), RequestKind::Chunk)
                    .header("Content-Range", format!("bytes {off}-{}/{total}", end - 1))
                    .body(&buf[..n], OCTET);
                let res = inner.api.send(&req, ctx);
                ctx.add_up(n as u64);
                res.and_then(|resp| session_response(inner, resp, total, Some(end)))
            }
        };
        match res {
            Ok(Next::Done(Outcome::Done(f, _))) => {
                let streamed = std::mem::replace(&mut stream, StreamMd5::new()).finish(total);
                return Ok(Outcome::Done(f, streamed));
            }
            Ok(Next::Done(Outcome::AlreadyExists)) => return Ok(Outcome::AlreadyExists),
            Ok(Next::Offset(o)) => {
                if o > confirmed {
                    stalls = 0;
                    failures = 0;
                } else {
                    stalls += 1;
                    if stalls > 4 {
                        return Err(StorageError::Transient(format!("{key}: the upload is not making progress")));
                    }
                }
                confirmed = o;
                st.offset = o;
                save_state(inner, skey, st);
                ctx.progress(Phase::Upload, (0, 0), (o, total), format!("uploading {key}"));
                step = if o >= total { Step::Status } else { Step::Send(o) };
            }
            Ok(Next::Expired) => {
                restarts += 1;
                if restarts > 2 {
                    return Err(StorageError::Transient(format!("{key}: the upload session keeps expiring")));
                }
                tracing::info!(target: "packsync_drive", "{key}: the upload session expired; starting a new one");
                st.session = None;
                st.offset = 0;
                confirmed = 0;
                stream = StreamMd5::new();
                save_state(inner, skey, st);
                step = Step::Init;
            }
            Err(e) if e.is_retryable() => {
                if failures + 1 >= inner.cfg.retry.max_attempts {
                    return Err(e);
                }
                inner.api.backoff(ctx, Phase::Upload, &format!("uploading {key}"), failures, &e)?;
                failures += 1;
                step = if st.session.is_some() { Step::Status } else { Step::Init };
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_layout() {
        let meta = serde_json::json!({"name": "x"});
        let (body, ct) = multipart_body(&meta, b"DATA").unwrap();
        let boundary = ct.strip_prefix("multipart/related; boundary=").unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\nContent-Type: application/json")));
        assert!(text.contains("\r\n\r\nDATA\r\n"));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn stream_md5_only_for_single_pass() {
        let mut s = StreamMd5::new();
        s.feed(0, b"ab");
        s.feed(2, b"c");
        assert_eq!(s.finish(3).unwrap(), hex::encode(Md5::digest(b"abc")));
        let mut s = StreamMd5::new();
        s.feed(0, b"ab");
        s.feed(1, b"bc");
        assert!(s.finish(3).is_none());
    }
}
