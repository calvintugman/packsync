//! `LocalDirStorage`: a provider backed by a directory. It is the credential-free "fake remote"
//! used by tests, benchmarks and the desktop app's "local folder (testing)" mode, but it has
//! *real* semantics: writes are temp-file + fsync + atomic rename, `CreateOnly` is atomic
//! (hard-link) and idempotent, reads support ranges.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::hash::{Hash, Hasher};
use crate::progress::OpCtx;
use crate::storage::*;

const TMP_DIR: &str = ".tmp-packsync";
const TAGS_DIR: &str = ".tags-packsync";
/// Per-key lock files for `PutMode::IfVersion`.
const LOCKS_DIR: &str = ".locks-packsync";

/// An object's version: changes whenever the file is replaced (a rename gives a new inode and time).
fn version_of(meta: &fs::Metadata) -> String {
    let ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let ino = 0u64;
    format!("{}-{ns}-{ino}", meta.len())
}

pub struct LocalDirStorage {
    root: PathBuf,
    id: String,
}

impl LocalDirStorage {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root: PathBuf = root.into();
        fs::create_dir_all(&root)?;
        let canon = dunce::canonicalize(&root)?;
        let id = format!("local:{}", canon.display());
        Ok(Self { root: canon, id })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Filesystem path backing `key` (tests use it to tamper with stored objects).
    pub fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key.trim_end_matches('/'))
    }

    fn tags_path(&self, key: &str) -> PathBuf {
        self.root.join(TAGS_DIR).join(format!("{}.json", key.replace('/', "__")))
    }

    fn load_tags(&self, key: &str) -> std::collections::BTreeMap<String, String> {
        fs::read(self.tags_path(key)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn info_for(&self, key: &str, path: &Path, meta: &fs::Metadata) -> ObjectInfo {
        let ms = |t: io::Result<std::time::SystemTime>| {
            t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64)
        };
        let _ = path;
        let mut info = ObjectInfo::file(key, meta.len());
        // Birth time is not portable (and cannot be set); objects are immutable, so mtime is their age.
        info.created_ms = ms(meta.modified());
        info.modified_ms = ms(meta.modified());
        info.tags = self.load_tags(key);
        info.version = Some(version_of(meta));
        info
    }

    fn file_blake3(path: &Path) -> io::Result<Hash> {
        let mut f = fs::File::open(path)?;
        let mut h = Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(h.finalize())
    }
}

fn fsync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

impl Storage for LocalDirStorage {
    fn provider_id(&self) -> String {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: "local-dir".into(),
            unique_keys: true,
            atomic_replace: true,
            // atomic on one machine (a lock file per key); a folder on a network share may not honour locks
            conditional_write: false,
            owned_namespace: false,
            range_reads: true,
            resumable_uploads: false,
            server_checksums: vec![ChecksumKind::Blake3],
            list_consistency: Consistency::Strong,
            max_object_size: None,
            recommended_concurrency: 4,
            recommended_pack_size: 16 * 1024 * 1024,
        }
    }

    fn list(&self, prefix: &str, ctx: &OpCtx) -> StorageResult<Vec<ObjectInfo>> {
        ctx.check().map_err(|_| StorageError::Cancelled)?;
        validate_key(prefix)?;
        if !prefix.ends_with('/') {
            return Err(StorageError::Other(format!("list prefix {prefix:?} must end with '/'")));
        }
        let dir = self.path_for(prefix);
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for ent in rd {
            let ent = ent?;
            let name = ent.file_name().to_string_lossy().into_owned();
            if name == TMP_DIR || name == TAGS_DIR || name == LOCKS_DIR || name.starts_with(".tmp-") {
                continue;
            }
            let meta = ent.metadata()?;
            if meta.is_dir() {
                let mut i = ObjectInfo::file(format!("{prefix}{name}/"), 0);
                i.is_dir = true;
                out.push(i);
            } else if meta.is_file() {
                out.push(self.info_for(&format!("{prefix}{name}"), &ent.path(), &meta));
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn stat(&self, key: &str, ctx: &OpCtx) -> StorageResult<Option<ObjectInfo>> {
        ctx.check().map_err(|_| StorageError::Cancelled)?;
        validate_key(key)?;
        let p = self.path_for(key);
        match fs::metadata(&p) {
            Ok(m) if m.is_file() => Ok(Some(self.info_for(key, &p, &m))),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn get(&self, key: &str, range: Option<(u64, u64)>, ctx: &OpCtx) -> StorageResult<Box<dyn Read + Send>> {
        ctx.check().map_err(|_| StorageError::Cancelled)?;
        validate_key(key)?;
        let p = self.path_for(key);
        let mut f = match fs::File::open(&p) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(StorageError::NotFound(key.to_string())),
            Err(e) => return Err(e.into()),
        };
        let size = f.metadata()?.len();
        match range {
            None => Ok(Box::new(f)),
            Some((start, len)) => {
                if start.checked_add(len).is_none_or(|end| end > size) {
                    return Err(StorageError::Other(format!(
                        "range {start}+{len} is outside object {key} of {size} bytes"
                    )));
                }
                f.seek(SeekFrom::Start(start))?;
                Ok(Box::new(f.take(len)))
            }
        }
    }

    fn put(&self, key: &str, src: PutSource<'_>, opts: &PutOptions, ctx: &OpCtx) -> StorageResult<ObjectInfo> {
        ctx.check().map_err(|_| StorageError::Cancelled)?;
        validate_key(key)?;
        if key.ends_with('/') {
            return Err(StorageError::Other("cannot put a directory key".into()));
        }
        if let Some(d) = &opts.digest
            && d.size != src.len()
        {
            return Err(StorageError::Corrupt(format!("declared size {} != source size {}", d.size, src.len())));
        }
        let dest = self.path_for(key);
        let parent = dest.parent().expect("key has a parent");
        fs::create_dir_all(parent)?;
        let tmp_dir = self.root.join(TMP_DIR);
        fs::create_dir_all(&tmp_dir)?;
        let tmp = tmp_dir.join(format!("{}.part", uuid::Uuid::new_v4()));

        // Stream into a temp file while hashing; the object is never visible half-written.
        let mut hasher = Hasher::new();
        let mut written = 0u64;
        let write_result: StorageResult<()> = (|| {
            let mut out = fs::File::create(&tmp)?;
            let mut buf = vec![0u8; 256 * 1024];
            let mut reader: Box<dyn Read> = match &src {
                PutSource::Bytes(b) => Box::new(*b),
                PutSource::File { path, .. } => Box::new(fs::File::open(path)?),
            };
            loop {
                if ctx.is_cancelled() {
                    return Err(StorageError::Cancelled);
                }
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])?;
                written += n as u64;
                ctx.add_up(n as u64);
            }
            out.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        let digest = hasher.finalize();
        if written != src.len() {
            let _ = fs::remove_file(&tmp);
            return Err(StorageError::Corrupt(format!(
                "source changed while uploading: expected {} bytes, read {}",
                src.len(),
                written
            )));
        }
        if let Some(d) = &opts.digest
            && d.blake3 != digest
        {
            let _ = fs::remove_file(&tmp);
            return Err(StorageError::Corrupt(format!("{key}: content does not match the declared digest")));
        }

        let finish = |meta_of: &Path| -> StorageResult<ObjectInfo> {
            // the tags belong to this version of the object: replace them, or remove stale ones
            let tp = self.tags_path(key);
            if opts.tags.is_empty() {
                let _ = fs::remove_file(&tp);
            } else {
                if let Some(dir) = tp.parent() {
                    fs::create_dir_all(dir)?;
                }
                fs::write(&tp, serde_json::to_vec(&opts.tags).map_err(|e| StorageError::Other(e.to_string()))?)?;
            }
            let m = fs::metadata(meta_of)?;
            let mut info = self.info_for(key, meta_of, &m);
            info.checksums.blake3 = Some(digest.to_hex());
            Ok(info)
        };

        match &opts.mode {
            PutMode::IfVersion(expected) => {
                let locks = self.root.join(LOCKS_DIR);
                fs::create_dir_all(&locks)?;
                let lock = fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(locks.join(format!("{}.lock", key.replace('/', "__"))))?;
                lock.lock()?; // released when `lock` is dropped
                let current = match fs::metadata(&dest) {
                    Ok(m) => Some(version_of(&m)),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => {
                        let _ = fs::remove_file(&tmp);
                        return Err(e.into());
                    }
                };
                if &current != expected {
                    let _ = fs::remove_file(&tmp);
                    return Err(StorageError::Conflict(format!(
                        "{key} changed (version {current:?}, expected {expected:?})"
                    )));
                }
                fs::rename(&tmp, &dest).inspect_err(|_| {
                    let _ = fs::remove_file(&tmp);
                })?;
                fsync_dir(parent);
                finish(&dest)
            }
            PutMode::Replace => {
                fs::rename(&tmp, &dest).inspect_err(|_| {
                    let _ = fs::remove_file(&tmp);
                })?;
                fsync_dir(parent);
                finish(&dest)
            }
            PutMode::CreateOnly => {
                // Atomic create-if-absent via hard link; fall back to check+rename on filesystems
                // without hard links.
                let linked = fs::hard_link(&tmp, &dest);
                match linked {
                    Ok(()) => {
                        let _ = fs::remove_file(&tmp);
                        fsync_dir(parent);
                        finish(&dest)
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        let _ = fs::remove_file(&tmp);
                        let existing_size = fs::metadata(&dest)?.len();
                        let same = existing_size == written && Self::file_blake3(&dest)? == digest;
                        if same {
                            finish(&dest)
                        } else {
                            Err(StorageError::Conflict(format!("{key} already exists with different content")))
                        }
                    }
                    Err(_) => {
                        if dest.exists() {
                            let _ = fs::remove_file(&tmp);
                            let same = fs::metadata(&dest)?.len() == written && Self::file_blake3(&dest)? == digest;
                            return if same {
                                finish(&dest)
                            } else {
                                Err(StorageError::Conflict(format!("{key} already exists with different content")))
                            };
                        }
                        fs::rename(&tmp, &dest).inspect_err(|_| {
                            let _ = fs::remove_file(&tmp);
                        })?;
                        fsync_dir(parent);
                        finish(&dest)
                    }
                }
            }
        }
    }

    fn delete(&self, key: &str, _mode: DeleteMode, ctx: &OpCtx) -> StorageResult<()> {
        ctx.check().map_err(|_| StorageError::Cancelled)?;
        validate_key(key)?;
        let _ = fs::remove_file(self.tags_path(key));
        match fs::remove_file(self.path_for(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn quota(&self, _ctx: &OpCtx) -> StorageResult<Option<Quota>> {
        let mut used = 0u64;
        for e in walkdir::WalkDir::new(&self.root).into_iter().flatten() {
            if e.file_type().is_file() {
                used += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        Ok(Some(Quota { used, total: None }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tags describe one version of an object: replacing it without tags must not leave the old ones visible.
    #[test]
    fn replacing_an_object_replaces_its_tags_too() {
        let d = tempfile::tempdir().unwrap();
        let s = LocalDirStorage::new(d.path()).unwrap();
        let ctx = OpCtx::detached();
        s.put("a/x", PutSource::Bytes(b"one"), &PutOptions::replace().with_tag("kind", "crash"), &ctx).unwrap();
        assert_eq!(s.stat("a/x", &ctx).unwrap().unwrap().tags.get("kind").map(String::as_str), Some("crash"));
        s.put("a/x", PutSource::Bytes(b"two"), &PutOptions::replace(), &ctx).unwrap();
        assert!(s.stat("a/x", &ctx).unwrap().unwrap().tags.is_empty(), "stale tags from the previous version");
    }
}
