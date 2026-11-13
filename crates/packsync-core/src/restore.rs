//! Restoring a snapshot onto disk: plan → pre-flight → stage & verify → journaled apply.
//!
//! Safety properties (tested in `tests/` with fault injection at every boundary):
//! * pre-flight failures (incompatible names, collisions, symlinks in the way, no disk space,
//!   missing/corrupt cloud data) happen **before the live instance is touched**;
//! * all new content is downloaded, hash-verified (per blob *and* per file) and staged first;
//! * replaced/deleted files are moved to a trash, never unlinked; the whole transaction is
//!   recoverable (see [`crate::journal`]) and the previous state is kept as a recovery point;
//! * cancellation or any error during apply rolls the instance back.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::chunk::ChunkingConfig;
use crate::db::{Db, IndexRow, InstanceRow, InstanceState};
use crate::error::{Error, IoContext, Result};
use crate::fsutil::{FileKind, FileStat, available_space, long_path, set_mtime_ms, stat_nofollow};
use crate::hash::{Hash, Hasher};
use crate::ids::{InstanceId, SnapshotId, TxId};
use crate::journal::*;
use crate::manifest::{Entry, EntryType, Manifest, Recipe, now_rfc3339};
use crate::mixed::{self, MixedPolicy};
use crate::paths::{FoldMode, ensure_no_symlink_ancestors, find_collisions, is_case_insensitive_fs, windows_issues};
use crate::progress::{Level, OpCtx, Phase};
use crate::remote::Remote;
use crate::rules::RuleSet;
use crate::scan::{ContentInfo, ScanOptions, hash_phase, walk};
use crate::stage::CountingSink;

#[derive(Debug, Clone)]
pub struct RestoreConfig {
    /// File bytes fetched and staged per batch (bounds temporary disk use beyond the staged files).
    pub batch_bytes: u64,
    pub concurrency: usize,
    /// Files assembled (hashed, written, flushed to disk) at the same time. Flushing a file to disk waits
    /// for the drive, so doing it for many files at once is what makes a restore of thousands of files fast.
    pub assemble_threads: usize,
    /// Skip entries whose names cannot exist on this platform instead of refusing the restore.
    pub skip_incompatible: bool,
    pub space_margin: u64,
    pub apply_batch: usize,
    pub keep_recovery_days: u32,
    /// Treat the target as Windows when checking names (default: the platform we are running on).
    /// Tests use it to exercise the cross-platform refusal on any OS.
    pub target_is_windows: bool,
    /// Force a name-folding mode instead of probing the target file system.
    pub fold_override: Option<FoldMode>,
}

impl Default for RestoreConfig {
    fn default() -> Self {
        Self {
            batch_bytes: 512 * 1024 * 1024,
            concurrency: 4,
            assemble_threads: 8,
            skip_incompatible: false,
            space_margin: 64 * 1024 * 1024,
            apply_batch: 64,
            keep_recovery_days: 14,
            target_is_windows: cfg!(windows),
            fold_override: None,
        }
    }
}

pub struct RestoreEnv<'a> {
    pub db: &'a Arc<Db>,
    pub remote: &'a Remote,
    pub policies: &'a HashMap<String, MixedPolicy>,
    pub cfg: &'a RestoreConfig,
    /// Re-checked immediately before the first live file is touched (staging can take minutes and a game
    /// may have been started meanwhile): fails when the game or another writer uses the instance.
    pub writers: Option<&'a dyn Fn() -> Result<()>>,
}

#[derive(Debug, Clone)]
pub struct LocalFile {
    pub content: ContentInfo,
    pub stat: FileStat,
    pub mixed: Option<String>,
}

/// What is on disk right now (restricted to paths the *target's* rules include).
#[derive(Debug, Default)]
pub struct LocalState {
    pub files: HashMap<String, LocalFile>,
    pub empty_dirs: HashSet<String>,
    pub rows: HashMap<String, IndexRow>,
    pub excluded_present: u64,
    /// Present here and included by the target's rules, but not synced by this instance's own rules (for
    /// example screenshots, when the other computer chose to sync everything). Overwritten only with a
    /// copy in the recovery area, and never deleted by a restore.
    pub not_synced_here: HashSet<String>,
}

/// Scan the live instance under `rules` (the target's rules) without staging anything.
#[allow(clippy::too_many_arguments)]
pub fn scan_local_state(
    db: &Arc<Db>,
    inst: &InstanceRow,
    rules: &RuleSet,
    policies: &HashMap<String, MixedPolicy>,
    chunking: &ChunkingConfig,
    scan: &ScanOptions,
    ctx: &OpCtx,
    own_rules: Option<&RuleSet>,
) -> Result<LocalState> {
    let index = if inst.needs_full_scan { HashMap::new() } else { db.index_load(inst.id)? };
    let mut opts = scan.now();
    if inst.needs_full_scan {
        opts.mode = crate::scan::ScanMode::Full;
    }
    let w = walk(&inst.root, rules, &index, &opts, ctx)?;
    if !w.unreadable_dirs.is_empty() {
        return Err(Error::InstanceUnavailable(format!(
            "{} could not be read; refusing to restore over unknown content",
            w.unreadable_dirs[0]
        )));
    }
    let to_hash: Vec<&crate::scan::Candidate> = w.files.iter().filter(|c| c.reuse.is_none()).collect();
    let sink = CountingSink::new(db.clone());
    let hp = hash_phase(&inst.root, &to_hash, &sink, chunking, policies, scan.threads, ctx)?;
    let hashed: HashMap<&str, &crate::scan::HashedFile> = hp.hashed.iter().map(|h| (h.path.as_str(), h)).collect();
    let mut st =
        LocalState { excluded_present: w.excluded.groups.values().map(|g| g.files).sum(), ..Default::default() };
    let now = crate::scan::now_ns();
    for c in &w.files {
        if let Some(row) = &c.reuse {
            st.files.insert(
                c.path.clone(),
                LocalFile {
                    content: ContentInfo { size: row.content_size, hash: row.hash, recipe: row.recipe, chunks: 0 },
                    stat: c.stat.clone(),
                    mixed: row.mixed.clone(),
                },
            );
            st.rows.insert(c.path.clone(), row.clone());
        } else if let Some(h) = hashed.get(c.path.as_str()) {
            st.files.insert(
                c.path.clone(),
                LocalFile { content: h.content.clone(), stat: h.stat.clone(), mixed: h.mixed.clone() },
            );
            st.rows.insert(
                c.path.clone(),
                IndexRow {
                    path: c.path.clone(),
                    size: h.stat.size,
                    mtime_ns: h.stat.mtime_ns,
                    ctime_ns: h.stat.ctime_ns,
                    file_id: h.stat.file_id.clone(),
                    hash: h.content.hash,
                    recipe: h.content.recipe,
                    mixed: h.mixed.clone(),
                    content_size: h.content.size,
                    hashed_at_ns: h.hashed_at_ns,
                    verified_at_ns: now,
                },
            );
        }
    }
    st.empty_dirs = w.empty_dirs.iter().cloned().collect();
    if let Some(own) = own_rules {
        st.not_synced_here = st.files.keys().filter(|p| !own.decide(p).include).cloned().collect();
    }
    Ok(st)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanIssue {
    pub path: String,
    pub problem: String,
}

#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub target: SnapshotId,
    pub ops: Vec<JOp>,
    /// Entries to materialise, keyed by path (the `Write` ops refer to these).
    pub writes: Vec<Entry>,
    pub unchanged: u64,
    pub deleted: u64,
    pub bytes_to_write: u64,
    pub preserved_local_only: u64,
    /// Entries left out because they cannot exist on this platform (`skip_incompatible`).
    pub skipped: Vec<PlanIssue>,
}

impl RestorePlan {
    pub fn is_noop(&self) -> bool {
        self.ops.is_empty()
    }
}

fn ancestors(path: &str) -> Vec<&str> {
    let mut v = vec![];
    let mut idx = 0;
    while let Some(p) = path[idx..].find('/') {
        idx += p;
        v.push(&path[..idx]);
        idx += 1;
    }
    v
}

/// Compute the operations that make the included paths of `root` equal to `target`.
/// Pure with respect to the cloud; reads the live tree only to see what exists.
pub fn plan(
    target: &Manifest,
    target_id: SnapshotId,
    local: &LocalState,
    root: &Path,
    cfg: &RestoreConfig,
) -> Result<RestorePlan> {
    let on_windows = cfg.target_is_windows;
    let mut skipped: Vec<PlanIssue> = vec![];
    let mut entries: Vec<&Entry> = vec![];
    for e in &target.entries {
        if on_windows {
            let wi = windows_issues(&e.p);
            if !wi.is_empty() {
                let problem = wi.iter().map(|i| i.to_string()).collect::<Vec<_>>().join("; ");
                if cfg.skip_incompatible {
                    skipped.push(PlanIssue { path: e.p.clone(), problem });
                    continue;
                }
                return Err(Error::Unsupported(format!(
                    "this snapshot contains a name that cannot exist on Windows: {} ({problem}). Nothing was changed; restore with 'skip incompatible files' or rename it on the source computer",
                    e.p
                )));
            }
        }
        entries.push(e);
    }
    // Names that collide on this file system (case / Unicode normalisation).
    let probe_dir = if root.is_dir() {
        root.to_path_buf()
    } else {
        root.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
    };
    let mode = cfg.fold_override.unwrap_or(if cfg!(target_os = "macos") {
        FoldMode::CaseAndNormalizationInsensitive
    } else if on_windows || is_case_insensitive_fs(&probe_dir) {
        FoldMode::CaseInsensitive
    } else {
        FoldMode::Sensitive
    });
    let coll = find_collisions(entries.iter().map(|e| (e.p.as_str(), e.t == EntryType::Dir)), mode);
    if !coll.is_empty() {
        if !cfg.skip_incompatible {
            let shown: Vec<String> = coll.iter().take(3).map(|c| c.join(" / ")).collect();
            return Err(Error::Unsupported(format!(
                "{} group(s) of snapshot paths would overwrite each other on this file system ({}{}). Nothing was changed",
                coll.len(),
                shown.join("; "),
                if coll.len() > shown.len() { "; …" } else { "" }
            )));
        }
        // every group keeps its first path; all the others are skipped (and reported)
        let drop: HashSet<&str> = coll.iter().flat_map(|c| c.iter().skip(1)).map(|s| s.as_str()).collect();
        entries.retain(|e| {
            if drop.contains(e.p.as_str()) {
                skipped.push(PlanIssue {
                    path: e.p.clone(),
                    problem: "collides with another path on this file system".into(),
                });
                false
            } else {
                true
            }
        });
    }

    let target_files: HashMap<&str, &Entry> =
        entries.iter().filter(|e| e.is_file()).map(|e| (e.p.as_str(), *e)).collect();
    let mut needed_dirs: BTreeSet<String> = BTreeSet::new();
    for e in &entries {
        if e.t == EntryType::Dir {
            needed_dirs.insert(e.p.clone());
        }
        for a in ancestors(&e.p) {
            needed_dirs.insert(a.to_string());
        }
    }

    let mut writes: Vec<Entry> = vec![];
    let mut unchanged = 0u64;
    let mut bytes_to_write = 0u64;
    let mut write_ops = vec![];
    for e in entries.iter().filter(|e| e.is_file()) {
        let same = local.files.get(&e.p).is_some_and(|lf| {
            lf.content.hash == e.h.unwrap_or(Hash::ZERO) && lf.content.size == e.s && lf.mixed == e.mx
        });
        if same {
            unchanged += 1;
        } else {
            let existed =
                root.join(Path::new(&e.p.replace('/', std::path::MAIN_SEPARATOR_STR))).symlink_metadata().is_ok();
            write_ops.push(JOp::Write { p: e.p.clone(), existed });
            bytes_to_write += e.s;
            writes.push((*e).clone());
        }
    }
    let mut delete_paths: Vec<&str> = local
        .files
        .keys()
        .map(|s| s.as_str())
        .filter(|p| !target_files.contains_key(p) && !local.not_synced_here.contains(*p))
        .collect();
    delete_paths.sort_unstable();
    let delete_set: HashSet<&str> = delete_paths.iter().copied().collect();
    let mut ops: Vec<JOp> = delete_paths.iter().map(|p| JOp::Delete { p: p.to_string() }).collect();

    // Directories emptied by the deletions (attempted, deepest first) and directories that stand where
    // a file must go.
    let mut rm: BTreeSet<String> = BTreeSet::new();
    for p in &delete_paths {
        for a in ancestors(p) {
            if !needed_dirs.contains(a) {
                rm.insert(a.to_string());
            }
        }
    }
    // Empty directories that exist locally but are not part of the target any more.
    for d in &local.empty_dirs {
        if !needed_dirs.contains(d) && !target_files.contains_key(d.as_str()) {
            rm.insert(d.clone());
        }
    }
    for e in &writes {
        let live = root.join(Path::new(&e.p.replace('/', std::path::MAIN_SEPARATOR_STR)));
        if live.is_dir() {
            // a directory where the snapshot has a file: allowed only if everything inside is being deleted
            for ent in walkdir::WalkDir::new(&live).into_iter().flatten().filter(|x| x.file_type().is_file()) {
                let rel = crate::paths::to_relpath(root, ent.path()).unwrap_or_default();
                if !delete_set.contains(rel.as_str()) {
                    return Err(Error::Unsupported(format!(
                        "{} is a folder on this computer but a file in the snapshot, and it holds local-only content ({}); move it away first. Nothing was changed",
                        e.p, rel
                    )));
                }
            }
            rm.insert(e.p.clone());
            for ent in walkdir::WalkDir::new(&live).into_iter().flatten().filter(|x| x.file_type().is_dir()) {
                if let Some(rel) = crate::paths::to_relpath(root, ent.path()) {
                    rm.insert(rel);
                }
            }
        }
    }
    let mut rm_sorted: Vec<String> = rm.into_iter().collect();
    rm_sorted.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    ops.extend(rm_sorted.into_iter().map(|p| JOp::RmDir { p }));

    // Directories to create (missing ones only), shallow first.
    let mut mk: Vec<String> = vec![];
    for d in &needed_dirs {
        let live = root.join(Path::new(&d.replace('/', std::path::MAIN_SEPARATOR_STR)));
        match live.symlink_metadata() {
            Ok(md) if md.is_dir() => {}
            Ok(_) if delete_set.contains(d.as_str()) => mk.push(d.clone()), // a file being deleted stands here
            Ok(_) => {
                return Err(Error::Unsupported(format!(
                    "{d} is a local file that is not part of the sync, but the snapshot needs a folder there. Nothing was changed"
                )));
            }
            Err(_) => mk.push(d.clone()),
        }
    }
    mk.sort_by_key(|p| p.matches('/').count());
    ops.extend(mk.into_iter().map(|p| JOp::MkDir { p }));
    ops.extend(write_ops);

    Ok(RestorePlan {
        target: target_id,
        deleted: delete_paths.len() as u64,
        ops,
        writes,
        unchanged,
        bytes_to_write,
        preserved_local_only: local.excluded_present,
        skipped,
    })
}

/// Checks that need the file system but not the cloud. Fails before anything is touched.
pub fn preflight(plan: &RestorePlan, root: &Path, cfg: &RestoreConfig) -> Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    for op in &plan.ops {
        let p = op.path();
        // check each distinct parent chain once
        let parent = p.rsplit_once('/').map(|x| x.0).unwrap_or("");
        let probe = if matches!(op, JOp::MkDir { .. } | JOp::RmDir { .. }) { p } else { parent };
        if !probe.is_empty() && seen.insert(probe.to_string()) {
            ensure_no_symlink_ancestors(root, probe)?;
        }
        if matches!(op, JOp::Write { .. } | JOp::Delete { .. }) {
            let live = root.join(Path::new(&p.replace('/', std::path::MAIN_SEPARATOR_STR)));
            if let Ok(md) = live.symlink_metadata()
                && md.file_type().is_symlink()
            {
                return Err(Error::Unsupported(format!(
                    "{p} is a symbolic link; PackSync never replaces links inside an instance"
                )));
            }
        }
    }
    let needed =
        plan.bytes_to_write.saturating_add(cfg.batch_bytes.min(plan.bytes_to_write)).saturating_add(cfg.space_margin);
    let have = available_space(root).unwrap_or(u64::MAX);
    if have < needed {
        return Err(Error::NoSpace { path: root.to_path_buf(), needed, available: have });
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreReport {
    pub target: Option<SnapshotId>,
    pub written: u64,
    pub unchanged: u64,
    pub deleted: u64,
    pub bytes_written: u64,
    pub bytes_downloaded: u64,
    /// Content of replaced files that was already on disk (unchanged blocks) and so was not downloaded.
    #[serde(default)]
    pub bytes_reused_locally: u64,
    pub recovery_point: Option<String>,
    pub skipped: Vec<PlanIssue>,
}

fn lp(p: &Path) -> PathBuf {
    long_path(p)
}

fn local_path(root: &Path, rel: &str) -> PathBuf {
    live_path(root, rel)
}

/// What one batch needed from the cloud.
struct Fetched {
    recipes: HashMap<Hash, Recipe>,
    downloaded: u64,
    /// Bytes served from blocks that were already on disk.
    reused: u64,
}

/// Chunks of the new version of a file that sit, unchanged, at the same offset of the old local file
/// (the game rewrites a few chunks of a region file in place): they need not be downloaded. Every block
/// taken from disk is verified against its hash, so a stale or modified local file can only cost a
/// download, never produce wrong content.
fn reuse_local_blocks(root: &Path, e: &Entry, rec: &Recipe, blob_dir: &Path) -> Result<(HashSet<Hash>, u64)> {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = fs::File::open(lp(&local_path(root, &e.p))) else { return Ok((HashSet::new(), 0)) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut have = HashSet::new();
    let (mut reused, mut off) = (0u64, 0u64);
    for (h, l) in &rec.chunks {
        let l64 = *l as u64;
        if off + l64 > len {
            break;
        }
        if blob_dir.join(h.to_hex()).exists() {
            have.insert(*h);
        } else {
            f.seek(SeekFrom::Start(off)).io_ctx(|| format!("reading {}", e.p))?;
            let mut buf = vec![0u8; *l as usize];
            if f.read_exact(&mut buf).is_err() {
                break;
            }
            if Hash::of(&buf) == *h {
                fs::create_dir_all(lp(blob_dir)).io_ctx(|| "creating the blob store".to_string())?;
                crate::remote::write_store_file(blob_dir, *h, &buf)?;
                have.insert(*h);
                reused += l64;
            }
        }
        off += l64;
    }
    Ok((have, reused))
}

/// Gather blobs for one batch of entries into `blob_dir` (recipes first, then chunks). With `local_root`,
/// unchanged blocks of files that are being replaced are taken from disk instead of the cloud.
fn fetch_batch(
    env: &RestoreEnv<'_>,
    entries: &[&Entry],
    blob_dir: &Path,
    local_root: Option<&Path>,
    ctx: &OpCtx,
) -> Result<Fetched> {
    let rec_hashes: Vec<Hash> = entries.iter().filter_map(|e| e.r).collect();
    let mut downloaded = 0;
    let mut reused = 0;
    if !rec_hashes.is_empty() {
        downloaded += env.remote.fetch_blobs(&rec_hashes, blob_dir, env.cfg.concurrency, ctx)?.fetched_bytes;
    }
    let mut recipes = HashMap::new();
    let mut data: Vec<Hash> = vec![];
    for e in entries {
        match e.r {
            Some(r) => {
                let bytes = fs::read(blob_dir.join(r.to_hex())).io_ctx(|| format!("reading recipe for {}", e.p))?;
                let rec = Recipe::decode(&bytes, e.s)?;
                let have = match local_root {
                    Some(root) => {
                        let (have, n) = reuse_local_blocks(root, e, &rec, blob_dir)?;
                        reused += n;
                        have
                    }
                    None => HashSet::new(),
                };
                data.extend(rec.chunks.iter().map(|c| c.0).filter(|h| !have.contains(h)));
                recipes.insert(r, rec);
            }
            None if e.s > 0 => {
                data.push(e.h.ok_or_else(|| Error::invalid("snapshot", format!("{} has no content hash", e.p)))?)
            }
            None => {}
        }
    }
    downloaded += env.remote.fetch_blobs(&data, blob_dir, env.cfg.concurrency, ctx)?.fetched_bytes;
    Ok(Fetched { recipes, downloaded, reused })
}

/// Build the file for `e` at `out` from the blob store and verify its whole-content hash.
/// Returns the final on-disk size.
fn assemble(
    e: &Entry,
    recipe: Option<&Recipe>,
    blob_dir: &Path,
    out: &Path,
    policy: Option<&MixedPolicy>,
    local_current: Option<Vec<u8>>,
    flush_file: bool,
) -> Result<u64> {
    if let Some(parent) = out.parent() {
        fs::create_dir_all(lp(parent)).io_ctx(|| format!("creating {}", parent.display()))?;
    }
    let want = e.h.ok_or_else(|| Error::invalid("snapshot", format!("{} has no content hash", e.p)))?;
    let part = out.with_file_name(format!(".{}.part", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<u64> {
        let mut hasher = Hasher::new();
        let mut size = 0u64;
        // The portable content of the file, for mixed files held in memory.
        let mut mixed_buf: Option<Vec<u8>> = policy.map(|_| Vec::new());
        let mut file = if mixed_buf.is_none() {
            Some(fs::File::create(lp(&part)).io_ctx(|| format!("creating {}", part.display()))?)
        } else {
            None
        };
        let mut emit = |bytes: &[u8]| -> Result<()> {
            hasher.update(bytes);
            size += bytes.len() as u64;
            if let Some(b) = mixed_buf.as_mut() {
                b.extend_from_slice(bytes);
                if b.len() as u64 > crate::scan::MAX_MIXED_FILE {
                    return Err(Error::invalid("mixed settings file", format!("{} is too large", e.p)));
                }
            } else if let Some(f) = file.as_mut() {
                f.write_all(bytes).io_ctx(|| format!("writing {}", part.display()))?;
            }
            Ok(())
        };
        if e.s > 0 {
            match recipe {
                Some(r) => {
                    for (h, len) in &r.chunks {
                        let b = fs::read(blob_dir.join(h.to_hex()))
                            .io_ctx(|| format!("reading staged blob {}", h.short()))?;
                        if b.len() != *len as usize || Hash::of(&b) != *h {
                            return Err(Error::Corrupt(format!("blob {} of {} failed verification", h.short(), e.p)));
                        }
                        emit(&b)?;
                    }
                }
                None => {
                    let b = fs::read(blob_dir.join(want.to_hex()))
                        .io_ctx(|| format!("reading staged blob {}", want.short()))?;
                    emit(&b)?;
                }
            }
        }
        if size != e.s || hasher.finalize() != want {
            return Err(Error::Corrupt(format!(
                "{} does not match the snapshot's recorded content (hash/size mismatch)",
                e.p
            )));
        }
        if let (Some(pol), Some(projection)) = (policy, mixed_buf.take()) {
            let merged = mixed::merge(pol, &projection, local_current.as_deref())?;
            fs::write(lp(&part), &merged).io_ctx(|| format!("writing {}", part.display()))?;
            if flush_file {
                fs::OpenOptions::new()
                    .write(true)
                    .open(lp(&part))
                    .and_then(|f| f.sync_all())
                    .io_ctx(|| format!("syncing {}", part.display()))?;
            }
            size = merged.len() as u64;
        } else if let Some(f) = file.take()
            && flush_file
        {
            f.sync_all().io_ctx(|| format!("syncing {}", part.display()))?;
        }
        Ok(size)
    })();
    match result {
        Ok(size) => {
            fs::rename(lp(&part), lp(out)).io_ctx(|| format!("finishing {}", out.display()))?;
            Ok(size)
        }
        Err(e) => {
            let _ = fs::remove_file(lp(&part));
            Err(e)
        }
    }
}

fn apply_metadata(path: &Path, e: &Entry) {
    if e.m != 0 {
        let _ = set_mtime_ms(path, e.m);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = fs::metadata(path) {
            let mut mode = md.permissions().mode();
            let want = if e.x { mode | 0o111 } else { mode & !0o111 };
            if want != mode {
                mode = want;
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
            }
        }
    }
}

/// Run `f` for every item on up to `threads` threads. The first error wins: no new item is started after it.
/// Cancellation is checked before each item.
fn for_each_parallel<T: Sync>(
    items: &[T],
    threads: usize,
    ctx: &OpCtx,
    f: impl Fn(&T) -> Result<()> + Sync,
) -> Result<()> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let first_err: parking_lot::Mutex<Option<Error>> = parking_lot::Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, items.len().max(1)) {
            s.spawn(|| {
                loop {
                    if first_err.lock().is_some() {
                        return;
                    }
                    let Some(item) = items.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) else {
                        return;
                    };
                    if let Err(e) = ctx.check().and_then(|_| f(item)) {
                        first_err.lock().get_or_insert(e);
                        return;
                    }
                }
            });
        }
    });
    match first_err.into_inner() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The files of a restore in the order their data is stored in the cloud (by pack, then offset), so that
/// each batch of files reads whole stretches of a few packs instead of scattered pieces of many.
fn download_order<'a>(env: &RestoreEnv<'_>, writes: &'a [Entry]) -> Vec<&'a Entry> {
    let mut keyed: Vec<(Option<(Hash, u64)>, &Entry)> = writes
        .iter()
        .map(|e| {
            let at = e.r.or(e.h).filter(|_| e.s > 0).and_then(|h| env.remote.blob_location(h).ok().flatten());
            (at, e)
        })
        .collect();
    keyed.sort_by_key(|k| k.0); // stable: files without a location keep their order, first
    keyed.into_iter().map(|(_, e)| e).collect()
}

/// Download, verify and stage every file in `plan.writes` under `stage_dir`.
/// Where restored files go and what this computer can contribute: the one difference between restoring
/// over an existing instance and into a new folder.
struct Materialise<'a> {
    /// The finished file for a snapshot path.
    out: &'a (dyn Fn(&str) -> PathBuf + Sync),
    /// The live instance (in place): its unchanged blocks are reused and its settings files merged.
    local_root: Option<&'a Path>,
    /// Flushed once per batch where the platform allows it (the folder the files are written to).
    flush_dir: &'a Path,
    blob_dir: PathBuf,
    /// Hit after each file (crash tests).
    after_file: Option<&'static str>,
}

/// Download (in storage order, batch by batch), assemble and verify every file of `writes`, and make it
/// durable before returning. Returns (bytes downloaded, bytes reused from local files).
fn materialise(
    env: &RestoreEnv<'_>,
    writes: &[Entry],
    total: u64,
    m: &Materialise<'_>,
    ctx: &OpCtx,
) -> Result<(u64, u64)> {
    let mut downloaded = 0;
    let mut reused = 0;
    let done_bytes = std::sync::atomic::AtomicU64::new(0);
    let done_files = std::sync::atomic::AtomicU64::new(0);
    let order = download_order(env, writes);
    // Files must be on disk before any of them is renamed over a live file or the folder becomes an
    // instance. One flush of the whole file system per batch is far cheaper than one per file; where that
    // is not available each file flushes itself.
    let one_flush = crate::fsutil::syncfs(m.flush_dir).unwrap_or(false);
    let flush_each = !one_flush;
    let mut i = 0;
    while i < order.len() {
        ctx.check()?;
        // one batch
        let mut batch: Vec<&Entry> = vec![];
        let mut bytes = 0u64;
        while i < order.len() && (batch.is_empty() || bytes.saturating_add(order[i].s) <= env.cfg.batch_bytes) {
            bytes = bytes.saturating_add(order[i].s);
            batch.push(order[i]);
            i += 1;
        }
        ctx.phase(Phase::StageDownload, format!("downloading {} files", batch.len()));
        let fetched = fetch_batch(env, &batch, &m.blob_dir, m.local_root, ctx)?;
        downloaded += fetched.downloaded;
        reused += fetched.reused;
        let recipes = fetched.recipes;
        crate::failpoint!("restore.after_fetch_batch")?;
        for_each_parallel(&batch, env.cfg.assemble_threads, ctx, |e| {
            let out = (m.out)(&e.p);
            let policy =
                e.mx.as_ref()
                    .map(|id| {
                        env.policies
                            .get(id)
                            .ok_or_else(|| Error::Config(format!("snapshot uses unknown mixed-file policy {id:?}")))
                    })
                    .transpose()?;
            let local_current = match (policy, m.local_root) {
                (Some(_), Some(root)) => match fs::read(lp(&local_path(root, &e.p))) {
                    // an oversized local copy must not be treated as absent: that would drop every local-only
                    // value (Java path, window size …) without a word. Stop instead; nothing was changed yet.
                    Ok(b) if b.len() as u64 > crate::scan::MAX_MIXED_FILE => {
                        return Err(Error::invalid(
                            "settings file",
                            format!(
                                "{} is larger than {} bytes, so its local settings cannot be kept; nothing was changed",
                                e.p,
                                crate::scan::MAX_MIXED_FILE
                            ),
                        ));
                    }
                    Ok(b) => Some(b),
                    Err(_) => None,
                },
                _ => None,
            };
            assemble(e, e.r.and_then(|r| recipes.get(&r)), &m.blob_dir, &out, policy, local_current, flush_each)?;
            apply_metadata(&out, e);
            if let Some(fp) = m.after_file {
                crate::failpoint!(fp)?;
            }
            let files = done_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let bytes = done_bytes.fetch_add(e.s, std::sync::atomic::Ordering::Relaxed) + e.s;
            ctx.progress(Phase::Validate, (files, writes.len() as u64), (bytes, total), format!("verified {}", e.p));
            Ok(())
        })?;
        // the downloaded blocks are no longer needed: drop them first so the flush below does not have to
        // write them to disk as well (only the assembled files matter)
        let _ = fs::remove_dir_all(lp(&m.blob_dir));
        if one_flush {
            crate::fsutil::syncfs(m.flush_dir).io_ctx(|| "flushing the restored files to disk".to_string())?;
        }
    }
    Ok((downloaded, reused))
}

fn stage_all(env: &RestoreEnv<'_>, plan: &RestorePlan, root: &Path, txd: &Path, ctx: &OpCtx) -> Result<(u64, u64)> {
    let out = |p: &str| stage_path(txd, p);
    let m = Materialise {
        out: &out,
        local_root: Some(root),
        flush_dir: txd,
        blob_dir: txd.join("blobs"),
        after_file: None,
    };
    materialise(env, &plan.writes, plan.bytes_to_write, &m, ctx)
}

fn new_tx_dir(root: &Path, tx: TxId) -> Result<PathBuf> {
    let d = tx_dir(root, tx);
    fs::create_dir_all(lp(&d)).io_ctx(|| format!("creating {}", d.display()))?;
    Ok(d)
}

/// Apply the plan to the live tree under the journal. On error (other than a simulated crash) the
/// operations that ran are rolled back.
fn apply(
    root: &Path,
    txd: &Path,
    journal: &mut Journal,
    plan: &RestorePlan,
    cfg: &RestoreConfig,
    ctx: &OpCtx,
) -> Result<()> {
    let n = plan.ops.len();
    let mut done = 0;
    let result: Result<()> = (|| {
        while done < n {
            ctx.check()?;
            let upto = (done + cfg.apply_batch.max(1)).min(n);
            journal.append(&Rec::BatchBegin { upto }, true)?;
            crate::failpoint!("restore.after_batch_begin")?;
            for (k, op) in plan.ops[done..upto].iter().enumerate() {
                run_op(root, txd, op)?;
                if k % 8 == 7 {
                    ctx.progress(
                        Phase::Apply,
                        ((done + k + 1) as u64, n as u64),
                        (0, 0),
                        format!("applying {}", op.path()),
                    );
                }
                crate::failpoint!("restore.mid_batch")?;
            }
            // The renames must be on disk before the journal says they happened: otherwise, after a power
            // cut, a durable "done"/"applied" could describe files that are still the old ones, and roll-forward
            // would adopt the target as the base while the old files sit there (the next snapshot would then
            // quietly revert the other computer's progress).
            flush_renames(root, &plan.ops[done..upto])?;
            journal.append(&Rec::BatchDone { upto }, true)?;
            crate::failpoint!("restore.after_batch_done")?;
            done = upto;
        }
        Ok(())
    })();
    if let Err(e) = result {
        if crate::failpoint::is_crash(&e) {
            return Err(e);
        }
        // roll back everything that may have started
        let begun = n.min(done + cfg.apply_batch.max(1));
        if let Err(re) = rollback_ops(root, txd, &plan.ops, begun) {
            return Err(Error::Corrupt(format!(
                "restore failed ({e}) and rollback also failed ({re}); recovery will retry on next start"
            )));
        }
        return Err(e);
    }
    Ok(())
}

/// Make the directory entries changed by `ops` durable: one file-system flush where the platform has it
/// (Linux `syncfs`), otherwise each touched directory (no-op on Windows, where NTFS journals renames).
fn flush_renames(root: &Path, ops: &[JOp]) -> Result<()> {
    if crate::fsutil::syncfs(root).map_err(|e| Error::io(format!("flushing {}", root.display()), e))? {
        return Ok(());
    }
    let dirs: std::collections::BTreeSet<PathBuf> =
        ops.iter().filter_map(|op| local_path(root, op.path()).parent().map(Path::to_path_buf)).collect();
    for d in dirs {
        crate::fsutil::fsync_dir(&d);
    }
    Ok(())
}

fn run_op(root: &Path, txd: &Path, op: &JOp) -> Result<()> {
    let live = local_path(root, op.path());
    match op {
        JOp::Delete { p } => {
            let trash = trash_path(txd, p);
            if let Some(d) = trash.parent() {
                fs::create_dir_all(lp(d)).io_ctx(|| "creating trash".into())?;
            }
            match fs::rename(lp(&live), lp(&trash)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // already gone
                Err(e) => return Err(map_rename_err(e, &live)),
            }
        }
        JOp::RmDir { .. } => {
            let _ = fs::remove_dir(lp(&live)); // fails (and is skipped) when something is still inside
        }
        JOp::MkDir { .. } => match fs::create_dir(lp(&live)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && live.is_dir() => {}
            Err(e) => return Err(Error::io(format!("creating {}", live.display()), e)),
        },
        JOp::Write { p, existed } => {
            if *existed && fs::symlink_metadata(lp(&live)).is_ok() {
                let trash = trash_path(txd, p);
                if let Some(d) = trash.parent() {
                    fs::create_dir_all(lp(d)).io_ctx(|| "creating trash".into())?;
                }
                fs::rename(lp(&live), lp(&trash)).map_err(|e| map_rename_err(e, &live))?;
                crate::failpoint!("restore.after_trash")?;
            }
            if !*existed && fs::symlink_metadata(lp(&live)).is_ok() {
                // Something created this file after the restore was planned (the game, another program):
                // never overwrite it silently. Stop; everything done so far is rolled back.
                return Err(Error::GameRunning(format!(
                    "{p} appeared while the restore was running (is the game or another program using the instance?); \
                     nothing was changed"
                )));
            }
            let stage = stage_path(txd, p);
            if let Some(d) = live.parent() {
                fs::create_dir_all(lp(d)).io_ctx(|| format!("creating {}", d.display()))?;
            }
            fs::rename(lp(&stage), lp(&live)).map_err(|e| map_rename_err(e, &live))?;
            crate::failpoint!("restore.after_place")?;
        }
    }
    Ok(())
}

fn map_rename_err(e: std::io::Error, path: &Path) -> Error {
    // Windows: sharing violation (32) / lock violation (33): another process has the file open.
    if matches!(e.raw_os_error(), Some(32) | Some(33)) || e.kind() == std::io::ErrorKind::PermissionDenied {
        return Error::GameRunning(format!("{} is in use by another program ({e})", path.display()));
    }
    if e.raw_os_error() == Some(18) {
        return Error::Unsupported(format!(
            "{} is on a different file system than the instance folder; PackSync cannot replace it atomically",
            path.display()
        ));
    }
    Error::io(format!("replacing {}", path.display()), e)
}

/// What to restore and why.
#[derive(Clone, Copy)]
pub struct RestoreRequest<'a> {
    pub target: &'a Manifest,
    pub target_id: SnapshotId,
    /// The snapshot the live tree currently corresponds to (recorded in the recovery point).
    pub base: Option<SnapshotId>,
    pub reason: &'a str,
    /// A history restore the user asked for: when set, the result is recorded as a new snapshot on top
    /// of `base` with this note (by the caller on success, by recovery if the process dies after the
    /// files were applied).
    pub derive_note: Option<&'a str>,
}

/// Restore `target` over an existing instance. The caller holds the instance lock and has verified
/// that no game is running.
pub fn restore_in_place(
    env: &RestoreEnv<'_>,
    inst: &InstanceRow,
    req: &RestoreRequest<'_>,
    local: &LocalState,
    ctx: &OpCtx,
) -> Result<RestoreReport> {
    let RestoreRequest { target, target_id, base, reason, derive_note } = *req;
    let root = inst.root.as_path();
    ctx.phase(Phase::Validate, "planning restore");
    let plan = plan(target, target_id, local, root, env.cfg)?;
    preflight(&plan, root, env.cfg)?;
    crate::failpoint!("restore.after_plan")?;
    let mut report = RestoreReport {
        target: Some(target_id),
        unchanged: plan.unchanged,
        deleted: plan.deleted,
        skipped: plan.skipped.clone(),
        ..Default::default()
    };
    if plan.is_noop() {
        finish_in_place(env.db, inst, target, target_id, local, &plan, &HashMap::new())?;
        return Ok(report);
    }

    let tx = TxId::new();
    let txd = new_tx_dir(root, tx)?;
    let mut journal = Journal::create(&txd.join("journal.jsonl"))?;
    journal.append(
        &Rec::Begin {
            tx,
            target: target_id,
            base,
            instance: inst.id,
            created: now_rfc3339(),
            derive_note: derive_note.map(str::to_string),
        },
        true,
    )?;
    journal.append(&Rec::Ops { ops: plan.ops.clone() }, true)?;
    env.db.instance_update(inst.id, |r| {
        r.state = InstanceState::Restoring;
        r.state_detail = Some(format!("restoring snapshot {}", target_id.short()));
    })?;

    let cleanup = |txd: &Path, e: &Error| {
        if !crate::failpoint::is_crash(e) {
            let _ = fs::remove_dir_all(lp(txd));
            let _ = env.db.instance_update(inst.id, |r| {
                r.state = InstanceState::Ok;
                r.state_detail = None;
            });
        }
    };

    let (downloaded, reused) = match stage_all(env, &plan, root, &txd, ctx) {
        Ok(d) => d,
        Err(e) => {
            cleanup(&txd, &e);
            return Err(e);
        }
    };
    report.bytes_downloaded = downloaded;
    report.bytes_reused_locally = reused;
    if let Err(e) = journal.append(&Rec::Staged, true).and_then(|_| crate::failpoint!("restore.after_staged")) {
        cleanup(&txd, &e);
        return Err(e);
    }
    ctx.phase(Phase::Apply, format!("applying {} changes", plan.ops.len()));
    if let Some(check) = env.writers
        && let Err(e) = check()
    {
        let _ = journal.append(&Rec::RolledBack, true);
        cleanup(&txd, &e);
        return Err(e);
    }
    if let Err(e) = apply(root, &txd, &mut journal, &plan, env.cfg, ctx) {
        if !crate::failpoint::is_crash(&e) {
            let _ = journal.append(&Rec::RolledBack, true);
        }
        cleanup(&txd, &e);
        return Err(e);
    }
    journal.append(&Rec::Applied, true)?;
    crate::failpoint!("restore.after_applied")?;

    // bookkeeping, then the commit record, then move the trash to the recovery area
    finish_in_place(env.db, inst, target, target_id, local, &plan, &HashMap::new())?;
    crate::failpoint!("restore.after_db_update")?;
    journal.append(&Rec::Committed, true)?;
    let bytes_replaced = plan.bytes_to_write;
    finalize_tx(
        root,
        tx,
        &RecoveryNote {
            tx,
            created: now_rfc3339(),
            from: base,
            to: target_id,
            files: (plan.writes.len() as u64) + plan.deleted,
            bytes: bytes_replaced,
            reason: reason.to_string(),
        },
    )?;
    let _ = prune_recovery(root, env.cfg.keep_recovery_days);
    report.written = plan.writes.len() as u64;
    report.bytes_written = plan.bytes_to_write;
    report.recovery_point = Some(tx.to_string());
    ctx.note(
        Phase::Apply,
        Level::Info,
        format!(
            "restored snapshot {} ({} files written, {} removed)",
            target_id.short(),
            report.written,
            report.deleted
        ),
    );
    Ok(report)
}

/// Database bookkeeping after a successful apply: new base, a file index that reflects what we just
/// wrote (fresh stats; hashes known from the manifest), instance state back to `ok`.
pub fn finish_in_place(
    db: &Arc<Db>,
    inst: &InstanceRow,
    target: &Manifest,
    target_id: SnapshotId,
    local: &LocalState,
    plan: &RestorePlan,
    _extra: &HashMap<String, String>,
) -> Result<()> {
    let root = inst.root.as_path();
    let written: HashSet<&str> = plan.writes.iter().map(|e| e.p.as_str()).collect();
    let target_files: HashSet<&str> = target.entries.iter().filter(|e| e.is_file()).map(|e| e.p.as_str()).collect();
    let now = crate::scan::now_ns();
    let mut rows: Vec<IndexRow> = vec![];
    for e in target.entries.iter().filter(|e| e.is_file()) {
        if written.contains(e.p.as_str()) {
            let abs = local_path(root, &e.p);
            if let Ok(st) = stat_nofollow(&abs)
                && st.kind == FileKind::File
            {
                rows.push(IndexRow {
                    path: e.p.clone(),
                    size: st.size,
                    mtime_ns: st.mtime_ns,
                    ctime_ns: st.ctime_ns,
                    file_id: st.file_id,
                    hash: e.h.unwrap_or(Hash::ZERO),
                    recipe: e.r,
                    mixed: e.mx.clone(),
                    content_size: e.s,
                    hashed_at_ns: now,
                    verified_at_ns: now,
                });
            }
        } else if let Some(r) = local.rows.get(&e.p) {
            rows.push(r.clone());
        }
    }
    let _ = target_files;
    db.index_replace(inst.id, &rows)?;
    db.instance_update(inst.id, |r| {
        r.base_snapshot = Some(target_id);
        r.state = InstanceState::Ok;
        r.state_detail = None;
        r.needs_full_scan = false;
        r.last_scan_ms = Some(crate::db::now_ms());
    })?;
    Ok(())
}

/// Finish a transaction that had applied everything before a crash.
pub fn roll_forward(
    db: &Arc<Db>,
    inst: &InstanceRow,
    target: &Manifest,
    target_id: SnapshotId,
    tx: TxId,
) -> Result<()> {
    // The files on disk equal the target by construction; re-derive the index lazily with a full scan.
    db.index_clear(inst.id)?;
    db.instance_update(inst.id, |r| {
        r.base_snapshot = Some(target_id);
        r.state = InstanceState::Ok;
        r.state_detail = None;
        r.needs_full_scan = true;
    })?;
    let txd = tx_dir(&inst.root, tx);
    let note = RecoveryNote {
        tx,
        created: now_rfc3339(),
        from: inst.base_snapshot,
        to: target_id,
        files: 0,
        bytes: 0,
        reason: "completed after an interruption".into(),
    };
    let _ = target;
    finalize_tx(&inst.root, tx, &note)?;
    let _ = fs::remove_dir_all(lp(&txd));
    Ok(())
}

// -----------------------------------------------------------------------------------------------
// New-directory restore (second computer / "restore as a new instance")
// -----------------------------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct RestoreMarker {
    pub tx: TxId,
    pub instance: InstanceId,
    pub target: SnapshotId,
    pub created: String,
}

pub fn restore_marker_path(dir: &Path) -> PathBuf {
    dir.join(crate::brand::LOCAL_MARKER_DIR).join("restore.json")
}

/// Is `dir` a half-finished restore we created (and may therefore delete)?
pub fn is_our_restore_dir(dir: &Path) -> bool {
    fs::read(restore_marker_path(dir)).ok().and_then(|b| serde_json::from_slice::<RestoreMarker>(&b).ok()).is_some()
}

/// Materialise `target` into the new, non-existing directory `dir` (a hidden staging folder the
/// launcher ignores). On failure or cancellation the directory is removed again; the caller then
/// registers/renames it.
pub fn restore_new(
    env: &RestoreEnv<'_>,
    dir: &Path,
    instance: InstanceId,
    target: &Manifest,
    target_id: SnapshotId,
    ctx: &OpCtx,
) -> Result<RestoreReport> {
    if dir.exists() {
        return Err(Error::Invalid { what: "restore location", detail: format!("{} already exists", dir.display()) });
    }
    // Pre-flight with an empty local state: same checks as an in-place restore.
    let empty = LocalState::default();
    let parent = dir.parent().unwrap_or(Path::new("."));
    let plan = plan(target, target_id, &empty, dir, env.cfg)?;
    // sizes come from the (untrusted) manifest: never let them wrap around
    let needed = plan
        .bytes_to_write
        .saturating_add(env.cfg.batch_bytes.min(plan.bytes_to_write))
        .saturating_add(env.cfg.space_margin);
    let have = available_space(parent).unwrap_or(u64::MAX);
    if have < needed {
        return Err(Error::NoSpace { path: parent.to_path_buf(), needed, available: have });
    }
    let tx = TxId::new();
    fs::create_dir_all(lp(dir)).io_ctx(|| format!("creating {}", dir.display()))?;
    let marker = RestoreMarker { tx, instance, target: target_id, created: now_rfc3339() };
    crate::fsutil::write_atomic(&restore_marker_path(dir), &serde_json::to_vec(&marker)?)
        .io_ctx(|| "writing restore marker".into())?;
    let added = dir.to_string_lossy().into_owned();
    update_pending(env.db, |v| v.push(added))?;
    crate::failpoint!("restore_new.after_marker")?;

    let result = (|| -> Result<RestoreReport> {
        let blob_dir = dir.join(crate::brand::LOCAL_MARKER_DIR).join("tx").join(tx.to_string()).join("blobs");
        for op in &plan.ops {
            if let JOp::MkDir { p } = op {
                fs::create_dir_all(lp(&local_path(dir, p))).io_ctx(|| format!("creating {p}"))?;
            }
        }
        let out = |p: &str| local_path(dir, p);
        let m = Materialise {
            out: &out,
            local_root: None,
            flush_dir: dir,
            blob_dir: blob_dir.clone(),
            after_file: Some("restore_new.after_file"),
        };
        let (downloaded, _) = materialise(env, &plan.writes, plan.bytes_to_write, &m, ctx)?;
        let _ = fs::remove_dir_all(lp(&dir.join(crate::brand::LOCAL_MARKER_DIR).join("tx")));
        Ok(RestoreReport {
            target: Some(target_id),
            written: plan.writes.len() as u64,
            unchanged: 0,
            deleted: 0,
            bytes_written: plan.bytes_to_write,
            bytes_downloaded: downloaded,
            bytes_reused_locally: 0,
            recovery_point: None,
            skipped: plan.skipped.clone(),
        })
    })();
    match result {
        // On success the ownership marker and the pending-restore record are KEPT until the caller has
        // registered the instance ([`finish_new_restore`]): a crash in between must still be able to
        // recognise (and remove) this folder as ours.
        Ok(r) => Ok(r),
        Err(e) => {
            if !crate::failpoint::is_crash(&e) {
                remove_restore_dir(dir);
                forget_pending(env.db, dir);
            }
            Err(e)
        }
    }
}

/// The restored folder has been registered and renamed into place: it is a normal instance now.
pub fn finish_new_restore(db: &Db, staged: &Path, final_dir: &Path) {
    let _ = fs::remove_file(restore_marker_path(final_dir));
    forget_pending(db, staged);
}

/// Remove a restore directory, but only if it carries our restore marker.
pub fn remove_restore_dir(dir: &Path) -> bool {
    if is_our_restore_dir(dir) { fs::remove_dir_all(lp(dir)).is_ok() } else { false }
}

/// The list of new-directory restores in progress (to clean up after a crash), changed atomically: restores
/// of different instances run in parallel, in this process and in others.
fn update_pending(db: &Db, f: impl FnOnce(&mut Vec<String>)) -> Result<()> {
    db.meta_update("pending_restores", |cur| {
        let mut v: Vec<String> = cur.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        f(&mut v);
        Some(serde_json::to_string(&v).unwrap_or_else(|_| "[]".into()))
    })
}

fn forget_pending(db: &Db, dir: &Path) {
    let gone = dir.to_string_lossy().into_owned();
    let _ = update_pending(db, |v| v.retain(|p| *p != gone));
}

fn restore_marker_of(dir: &Path) -> Option<RestoreMarker> {
    fs::read(restore_marker_path(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// Startup recovery for half-finished new-directory restores. `in_progress(instance)` says whether a
/// restore of that instance is still running (in this or another process): its folder is left alone.
pub fn recover_pending_restores(db: &Db, in_progress: &dyn Fn(InstanceId) -> bool) -> Vec<(PathBuf, bool)> {
    let Ok(Some(s)) = db.meta_get("pending_restores") else { return vec![] };
    let listed: Vec<String> = serde_json::from_str(&s).unwrap_or_default();
    let mut out = vec![];
    let mut done: Vec<String> = vec![];
    for p in listed {
        let path = PathBuf::from(&p);
        if restore_marker_of(&path).is_some_and(|m| in_progress(m.instance)) {
            continue; // still being written by a running restore
        }
        let removed = if path.exists() { remove_restore_dir(&path) } else { true };
        out.push((path, removed));
        done.push(p);
    }
    let _ = update_pending(db, |v| v.retain(|p| !done.contains(p)));
    out
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<RestoreConfig>();
}

#[allow(unused_imports)]
use Read as _;

#[cfg(test)]
mod tests {
    use super::*;

    /// Startup recovery removes the folders of new-directory restores that died, never the folder of one
    /// that is still running (in another process: the CLI while the app starts, for instance).
    #[test]
    fn recovery_leaves_a_running_new_directory_restore_alone() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let mk = |name: &str| {
            let dir = d.path().join(name);
            fs::create_dir_all(dir.join(crate::brand::LOCAL_MARKER_DIR)).unwrap();
            let m = RestoreMarker {
                tx: TxId::new(),
                instance: InstanceId::new(),
                target: SnapshotId(Hash::of(name.as_bytes())),
                created: now_rfc3339(),
            };
            fs::write(restore_marker_path(&dir), serde_json::to_vec(&m).unwrap()).unwrap();
            let p = dir.to_string_lossy().into_owned();
            update_pending(&db, |v| v.push(p)).unwrap();
            (dir, m.instance)
        };
        let (running, running_id) = mk("running");
        let (dead, _) = mk("dead");
        let out = recover_pending_restores(&db, &|i| i == running_id);
        assert!(running.exists(), "a running restore's folder is never removed");
        assert!(!dead.exists(), "a dead restore's folder is cleaned up");
        assert_eq!(out.len(), 1);
        let left: Vec<String> = serde_json::from_str(&db.meta_get("pending_restores").unwrap().unwrap()).unwrap();
        assert_eq!(left, vec![running.to_string_lossy().into_owned()], "still listed, for when it ends");
    }

    /// With "skip what cannot exist here", EVERY group of names that collide on a case-insensitive file
    /// system is reduced to one path — the second group used to be written twice to the same real file.
    #[test]
    fn every_colliding_group_is_skipped_not_just_the_first() {
        let mut m = crate::manifest::testutil::sample();
        let file = |p: &str| Entry {
            p: p.into(),
            t: EntryType::File,
            s: 1,
            m: 0,
            x: false,
            h: Some(Hash::of(p.as_bytes())),
            r: None,
            mx: None,
        };
        m.entries = vec![
            file(".minecraft/config/A.cfg"),
            file(".minecraft/config/a.cfg"),
            file(".minecraft/mods/X.jar"),
            file(".minecraft/mods/x.jar"),
            file(".minecraft/ok.txt"),
        ];
        m.entries.sort_by(|a, b| a.p.as_bytes().cmp(b.p.as_bytes()));
        let d = tempfile::tempdir().unwrap();
        let mut cfg = RestoreConfig { fold_override: Some(FoldMode::CaseInsensitive), ..Default::default() };
        let err = plan(&m, SnapshotId(Hash::of(b"t")), &LocalState::default(), d.path(), &cfg).unwrap_err();
        assert!(err.to_string().contains("2 group(s)"), "{err}");
        cfg.skip_incompatible = true;
        let p = plan(&m, SnapshotId(Hash::of(b"t")), &LocalState::default(), d.path(), &cfg).unwrap();
        let written: Vec<String> = p.writes.iter().map(|e| e.p.to_lowercase()).collect();
        let unique: HashSet<&String> = written.iter().collect();
        assert_eq!(written.len(), unique.len(), "no two writes land on the same file: {written:?}");
        assert_eq!(written.len(), 3, "{written:?}");
        assert_eq!(p.skipped.len(), 2, "{:?}", p.skipped);
    }

    /// A file the plan expected to be new but that appeared meanwhile (the game started writing, another
    /// program) is never overwritten: the operation fails and nothing of the person's file changes.
    #[test]
    fn a_file_that_appeared_after_planning_is_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let (root, txd) = (d.path().join("inst"), d.path().join("tx"));
        fs::create_dir_all(root.join("saves/W")).unwrap();
        fs::create_dir_all(txd.join("stage/saves/W")).unwrap();
        fs::write(txd.join("stage/saves/W/new.dat"), b"from the cloud").unwrap();
        fs::write(root.join("saves/W/new.dat"), b"written by the game just now").unwrap();
        let err = run_op(&root, &txd, &JOp::Write { p: "saves/W/new.dat".into(), existed: false }).unwrap_err();
        assert!(matches!(err, Error::GameRunning(_)), "{err}");
        assert_eq!(fs::read(root.join("saves/W/new.dat")).unwrap(), b"written by the game just now");
        assert!(txd.join("stage/saves/W/new.dat").exists(), "the staged copy stays for the rollback");
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parallel_work_runs_every_item_exactly_once() {
        let items: Vec<usize> = (0..200).collect();
        let seen = AtomicUsize::new(0);
        let sum = AtomicUsize::new(0);
        for_each_parallel(&items, 8, &OpCtx::detached(), |i| {
            seen.fetch_add(1, Ordering::Relaxed);
            sum.fetch_add(*i, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        assert_eq!((seen.load(Ordering::Relaxed), sum.load(Ordering::Relaxed)), (200, 199 * 200 / 2));
        // nothing to do is fine
        for_each_parallel::<usize>(&[], 8, &OpCtx::detached(), |_| unreachable!()).unwrap();
    }

    #[test]
    fn the_first_error_wins_and_stops_new_work() {
        let items: Vec<usize> = (0..100).collect();
        let started = AtomicUsize::new(0);
        let r = for_each_parallel(&items, 1, &OpCtx::detached(), |i| {
            started.fetch_add(1, Ordering::Relaxed);
            if *i == 10 { Err(Error::other("boom at ten")) } else { Ok(()) }
        });
        assert!(matches!(r, Err(e) if e.to_string().contains("boom at ten")));
        assert_eq!(started.load(Ordering::Relaxed), 11, "no item after the failing one was started");
        // with several threads at least the failure is reported and work stops well short of everything
        let started = AtomicUsize::new(0);
        let r = for_each_parallel(&items, 4, &OpCtx::detached(), |i| {
            started.fetch_add(1, Ordering::Relaxed);
            if *i == 3 { Err(Error::other("early failure")) } else { Ok(()) }
        });
        assert!(r.is_err());
        assert!(started.load(Ordering::Relaxed) < 100);
    }

    #[test]
    fn cancellation_is_honoured_between_items() {
        let items: Vec<usize> = (0..50).collect();
        let token = crate::progress::CancelToken::new();
        let ctx = OpCtx::new(std::sync::Arc::new(crate::progress::NullSink), token.clone());
        let done = AtomicUsize::new(0);
        let r = for_each_parallel(&items, 1, &ctx, |_| {
            if done.fetch_add(1, Ordering::Relaxed) == 4 {
                token.cancel();
            }
            Ok(())
        });
        assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
        assert_eq!(done.load(Ordering::Relaxed), 5);
    }
}
