//! Scanning: walking an instance, applying the rules, deciding what must be (re)hashed, and
//! hashing/chunking changed content into the staging sink.
//!
//! ## Correctness policy for the persistent file index (documented in `docs/consistency.md`)
//!
//! The index is an *optimisation*, never authority. A file's cached hash is reused only when ALL of
//! these hold (see [`classify`]):
//!
//! 1. an index entry exists and the scan is not a *full* scan;
//! 2. size, mtime (ns) and — where the platform exposes it — **change time** and **file identity**
//!    are unchanged (change time cannot be set from user space, so it exposes same-size edits whose
//!    mtime was restored; identity exposes replaced files);
//! 3. the entry is not **racy**: the file's mtime/ctime is at least `racy_window` older than the
//!    moment the entry was hashed (otherwise a later same-tick modification would be invisible —
//!    the "racy git" problem);
//! 4. the entry has not outlived its **verification period** (default 14 days, staggered per path so
//!    the work is spread out) — this bounds how long a modification that evaded 2–3 (e.g. on a file
//!    system without change times) can stay undetected.
//!
//! Full hashing/reconciliation happens: on the first scan, after a restore, watcher overflow, an
//! unclean session, a changed volume/instance path, on `verify --full`, and per (4).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::chunk::{ChunkingConfig, chunk_stream, resolve_strategy};
use crate::db::IndexRow;
use crate::error::{Error, Result};
use crate::fsutil::{FileKind, FileStat, stat_from_md, stat_nofollow};
use crate::hash::{Hash, Hasher};
use crate::manifest::Recipe;
use crate::mixed::{self, MixedPolicy};
use crate::pack::{KIND_DATA, KIND_RECIPE};
use crate::paths::{FoldMode, find_collisions, validate_relpath, windows_issues};
use crate::progress::{OpCtx, Phase};
use crate::rules::{Decision, DecisionSource, RuleSet, Strategy};
use crate::stage::BlobSink;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    /// Trust the index per the policy above.
    Quick,
    /// Re-hash every included file.
    Full,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub mode: ScanMode,
    pub trust_ctime: bool,
    pub racy_window_ns: i64,
    pub verify_period_ns: i64,
    pub now_ns: i64,
    pub threads: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            mode: ScanMode::Quick,
            trust_ctime: true,
            racy_window_ns: 2_000_000_000, // FAT has 2 s granularity; ext4/NTFS are finer
            verify_period_ns: 14 * 86_400 * 1_000_000_000,
            now_ns: now_ns(),
            threads: std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4),
        }
    }
}

pub fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehashReason {
    Reuse,
    New,
    SizeChanged,
    MtimeChanged,
    CtimeChanged,
    IdentityChanged,
    Racy,
    Periodic,
    FullScan,
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Decide whether a file's cached hash may be reused. Pure and deterministic: the heart of the
/// documented scan policy.
pub fn classify(path: &str, stat: &FileStat, row: Option<&IndexRow>, opts: &ScanOptions) -> RehashReason {
    if opts.mode == ScanMode::Full {
        return RehashReason::FullScan;
    }
    let Some(row) = row else { return RehashReason::New };
    if row.size != stat.size {
        return RehashReason::SizeChanged;
    }
    if row.mtime_ns != stat.mtime_ns {
        return RehashReason::MtimeChanged;
    }
    if opts.trust_ctime && stat.ctime_ns.is_some() && row.ctime_ns != stat.ctime_ns {
        return RehashReason::CtimeChanged;
    }
    if let (Some(a), Some(b)) = (&row.file_id, &stat.file_id)
        && a != b
    {
        return RehashReason::IdentityChanged;
    }
    let newest = stat.mtime_ns.max(if opts.trust_ctime { stat.ctime_ns.unwrap_or(i64::MIN) } else { i64::MIN });
    if newest.saturating_add(opts.racy_window_ns) >= row.hashed_at_ns {
        return RehashReason::Racy;
    }
    // Staggered verification: due between 50 % and 100 % of the period, per path.
    let jitter = 0.5 + 0.5 * ((fnv1a(path) % 1000) as f64 / 1000.0);
    let due_after = (opts.verify_period_ns as f64 * jitter) as i64;
    if opts.now_ns.saturating_sub(row.verified_at_ns) >= due_after {
        return RehashReason::Periodic;
    }
    RehashReason::Reuse
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    Symlink,
    SpecialFile,
    UnsupportedName,
    Unreadable,
    NonPortableName,
    CaseCollision,
    Unstable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanIssue {
    pub path: String,
    pub kind: IssueKind,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExcludedGroup {
    pub category: String,
    pub reason: String,
    pub files: u64,
    pub bytes: u64,
    pub samples: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExcludedSummary {
    /// Keyed by rule id (`sealed:<id>` for the hard-coded layer, `unmatched` for unrecognised paths).
    pub groups: BTreeMap<String, ExcludedGroup>,
    /// Directories skipped without descending: (path, rule id, reason).
    pub pruned_dirs: Vec<(String, String, String)>,
    /// Unrecognised content by top-level directory: (files, bytes).
    pub unmatched_by_dir: BTreeMap<String, (u64, u64)>,
}

impl ExcludedSummary {
    fn add_file(&mut self, rel: &str, size: u64, d: &Decision, game_root: &str) {
        let key = match &d.source {
            DecisionSource::Sealed { id } => format!("sealed:{id}"),
            DecisionSource::Rule { id, .. } => id.clone(),
            DecisionSource::Unmatched => "unmatched".to_string(),
        };
        let g = self.groups.entry(key).or_insert_with(|| ExcludedGroup {
            category: d.category.clone(),
            reason: d.reason.clone(),
            ..Default::default()
        });
        g.files += 1;
        g.bytes += size;
        if g.samples.len() < 5 {
            g.samples.push(rel.to_string());
        }
        if matches!(d.source, DecisionSource::Unmatched) {
            let under =
                if game_root.is_empty() { rel } else { rel.strip_prefix(&format!("{game_root}/")).unwrap_or(rel) };
            let top: String = under.split('/').next().unwrap_or(under).to_string();
            let e = self.unmatched_by_dir.entry(top).or_default();
            e.0 += 1;
            e.1 += size;
        }
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: String,
    pub stat: FileStat,
    pub decision: Decision,
    pub reason: RehashReason,
    /// The cached row, kept only when the policy allows reusing it.
    pub reuse: Option<IndexRow>,
}

#[derive(Debug, Default)]
pub struct WalkResult {
    pub files: Vec<Candidate>,
    pub empty_dirs: Vec<String>,
    pub excluded: ExcludedSummary,
    pub issues: Vec<ScanIssue>,
    /// Directories that could not be read: their content is *unknown*, never "deleted".
    pub unreadable_dirs: Vec<String>,
    pub files_seen: u64,
    pub dirs_seen: u64,
}

impl WalkResult {
    pub fn included_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.stat.size).sum()
    }
    pub fn rehash_count(&self) -> usize {
        self.files.iter().filter(|f| f.reason != RehashReason::Reuse).count()
    }
}

struct WalkCtx<'a> {
    root: &'a Path,
    rules: &'a RuleSet,
    index: &'a HashMap<String, IndexRow>,
    opts: &'a ScanOptions,
    ctx: &'a OpCtx,
    out: WalkResult,
}

pub const MAX_WALK_FILES: usize = crate::manifest::MAX_ENTRIES;

/// Walk the instance (stat only, no hashing). Symlinks are never followed; unreadable directories
/// are reported (never treated as empty).
pub fn walk(
    root: &Path,
    rules: &RuleSet,
    index: &HashMap<String, IndexRow>,
    opts: &ScanOptions,
    ctx: &OpCtx,
) -> Result<WalkResult> {
    let meta = fs::metadata(crate::fsutil::long_path(root))
        .map_err(|e| Error::InstanceUnavailable(format!("{}: {e}", root.display())))?;
    if !meta.is_dir() {
        return Err(Error::InstanceUnavailable(format!("{} is not a directory", root.display())));
    }
    let mut w = WalkCtx { root, rules, index, opts, ctx, out: WalkResult::default() };
    walk_dir(&mut w, root, "")?;
    let mut out = w.out;

    // Portability: names that cannot exist on Windows, and case/Unicode collisions.
    let paths: Vec<(&str, bool)> = out
        .files
        .iter()
        .map(|f| (f.path.as_str(), false))
        .chain(out.empty_dirs.iter().map(|d| (d.as_str(), true)))
        .collect();
    let collisions = find_collisions(paths.iter().copied(), FoldMode::PORTABLE);
    for c in collisions {
        out.issues.push(ScanIssue {
            path: c[0].clone(),
            kind: IssueKind::CaseCollision,
            detail: format!("these paths collide on case-insensitive or normalising file systems: {}", c.join(", ")),
        });
    }
    for f in &out.files {
        let wi = windows_issues(&f.path);
        if !wi.is_empty() {
            out.issues.push(ScanIssue {
                path: f.path.clone(),
                kind: IssueKind::NonPortableName,
                detail: wi.iter().map(|i| i.to_string()).collect::<Vec<_>>().join("; "),
            });
        }
    }
    out.files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    out.empty_dirs.sort();
    Ok(out)
}

fn walk_dir(w: &mut WalkCtx<'_>, abs: &Path, rel: &str) -> Result<u64> {
    w.ctx.check()?;
    w.out.dirs_seen += 1;
    let rd = match fs::read_dir(crate::fsutil::long_path(abs)) {
        Ok(rd) => rd,
        Err(e) if rel.is_empty() => return Err(Error::InstanceUnavailable(format!("{}: {e}", abs.display()))),
        Err(e) => {
            w.out.unreadable_dirs.push(rel.to_string());
            w.out.issues.push(ScanIssue { path: rel.to_string(), kind: IssueKind::Unreadable, detail: e.to_string() });
            return Ok(0);
        }
    };
    let mut entries: Vec<fs::DirEntry> = Vec::new();
    for e in rd {
        match e {
            Ok(e) => entries.push(e),
            Err(e) => {
                w.out.unreadable_dirs.push(rel.to_string());
                w.out.issues.push(ScanIssue {
                    path: rel.to_string(),
                    kind: IssueKind::Unreadable,
                    detail: e.to_string(),
                });
            }
        }
    }
    entries.sort_by_key(|e| e.file_name());
    let mut included_below = 0u64;

    for e in entries {
        let Some(name) = e.file_name().to_str().map(|s| s.to_string()) else {
            w.out.issues.push(ScanIssue {
                path: format!("{rel}/{}", e.file_name().to_string_lossy()),
                kind: IssueKind::UnsupportedName,
                detail: "file name is not valid UTF-8 and cannot be synced".into(),
            });
            continue;
        };
        let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let child_abs = abs.join(&name);
        let md = match e.metadata() {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue, // vanished while scanning
            Err(err) => {
                w.out.issues.push(ScanIssue { path: child_rel, kind: IssueKind::Unreadable, detail: err.to_string() });
                continue;
            }
        };
        let stat = stat_from_md(&child_abs, &md);
        match stat.kind {
            FileKind::Dir => {
                if let Some(d) = w.rules.prune_dir(&child_rel) {
                    let id = match &d.source {
                        DecisionSource::Sealed { id } => format!("sealed:{id}"),
                        DecisionSource::Rule { id, .. } => id.clone(),
                        DecisionSource::Unmatched => "unmatched".into(),
                    };
                    w.out.excluded.pruned_dirs.push((child_rel, id, d.reason));
                    continue;
                }
                if let Err(issue) = validate_relpath(&child_rel) {
                    w.out.issues.push(ScanIssue {
                        path: child_rel,
                        kind: IssueKind::UnsupportedName,
                        detail: issue.to_string(),
                    });
                    continue;
                }
                let unreadable_before = w.out.unreadable_dirs.len();
                let n = walk_dir(w, &child_abs, &child_rel)?;
                // An unreadable directory's content is UNKNOWN: it is neither empty nor deleted.
                let became_unreadable = w.out.unreadable_dirs.len() > unreadable_before;
                if n == 0 && !became_unreadable && w.rules.decide(&format!("{child_rel}/\u{1}x")).include {
                    w.out.empty_dirs.push(child_rel);
                    included_below += 1;
                } else {
                    included_below += n;
                }
            }
            FileKind::File => {
                w.out.files_seen += 1;
                if let Err(issue) = validate_relpath(&child_rel) {
                    w.out.issues.push(ScanIssue {
                        path: child_rel,
                        kind: IssueKind::UnsupportedName,
                        detail: issue.to_string(),
                    });
                    continue;
                }
                let d = w.rules.decide(&child_rel);
                if !d.include {
                    w.out.excluded.add_file(&child_rel, stat.size, &d, w.rules.game_root());
                    continue;
                }
                if w.out.files.len() >= MAX_WALK_FILES {
                    return Err(Error::invalid(
                        "instance",
                        format!("more than {MAX_WALK_FILES} files would be synced"),
                    ));
                }
                let row = w.index.get(&child_rel);
                let reason = classify(&child_rel, &stat, row, w.opts);
                let reuse = if reason == RehashReason::Reuse { row.cloned() } else { None };
                w.out.files.push(Candidate { path: child_rel, stat, decision: d, reason, reuse });
                included_below += 1;
            }
            FileKind::Symlink | FileKind::Other => {
                let d = w.rules.decide(&child_rel);
                // A link can stand for a whole folder (`saves` → elsewhere): judge it by whether
                // anything *inside* that path would be synced, not only by the link's own name.
                let in_included_area = d.include || w.rules.decide(&format!("{child_rel}/\u{1}x")).include;
                if in_included_area {
                    let (kind, detail) = if stat.kind == FileKind::Symlink {
                        (IssueKind::Symlink, "symbolic links are never followed or synced; the link was skipped")
                    } else {
                        (IssueKind::SpecialFile, "not a regular file; skipped")
                    };
                    w.out.issues.push(ScanIssue { path: child_rel, kind, detail: detail.into() });
                } else {
                    w.out.excluded.add_file(&child_rel, 0, &d, w.rules.game_root());
                }
            }
        }
    }
    let _ = w.root;
    Ok(included_below)
}

/// Content identity of one file as stored in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentInfo {
    /// Size of the stored content (the projection's size for mixed files).
    pub size: u64,
    pub hash: Hash,
    pub recipe: Option<Hash>,
    pub chunks: u32,
}

#[derive(Debug, Clone)]
pub struct HashedFile {
    pub path: String,
    /// Metadata observed *before* reading (stored in the index with `hashed_at_ns`).
    pub stat: FileStat,
    pub content: ContentInfo,
    pub mixed: Option<String>,
    pub hashed_at_ns: i64,
}

pub const MAX_MIXED_FILE: u64 = 1024 * 1024;

fn stable(a: &FileStat, b: &FileStat) -> bool {
    a.size == b.size && a.mtime_ns == b.mtime_ns && a.ctime_ns == b.ctime_ns && a.file_id == b.file_id
}

/// Read, chunk and hash one file, offering new blobs to `sink`. The file is stat-ed before and
/// after reading; if it changed meanwhile the read is retried and finally reported as unstable
/// (a writer is active — snapshots must be taken after the game exits).
pub fn hash_candidate(
    root: &Path,
    c: &Candidate,
    sink: &dyn BlobSink,
    cfg: &ChunkingConfig,
    policies: &HashMap<String, MixedPolicy>,
) -> Result<Option<HashedFile>> {
    let abs = crate::paths::join_safe(root, &c.path)?;
    let mut last_err = None;
    for attempt in 0..3u32 {
        let hashed_at = now_ns();
        let before = match stat_nofollow(&abs) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None), // deleted while scanning
            Err(e) => return Err(Error::io(format!("reading {}", abs.display()), e)),
        };
        if before.kind != FileKind::File {
            return Err(Error::Busy(format!("{} changed type while being scanned", c.path)));
        }
        let (content, read_bytes) = match read_content(&abs, c, &before, sink, cfg, policies) {
            Ok(v) => v,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let after = match stat_nofollow(&abs) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(format!("reading {}", abs.display()), e)),
        };
        if stable(&before, &after) && read_bytes == before.size {
            return Ok(Some(HashedFile {
                path: c.path.clone(),
                stat: before,
                content,
                mixed: c.decision.mixed.clone(),
                hashed_at_ns: hashed_at,
            }));
        }
        last_err = Some(format!("{} changed while it was being read (attempt {})", c.path, attempt + 1));
        std::thread::sleep(Duration::from_millis(40 * (attempt as u64 + 1)));
    }
    Err(Error::GameRunning(last_err.unwrap_or_else(|| format!("{} keeps changing", c.path))))
}

fn read_content(
    abs: &Path,
    c: &Candidate,
    before: &FileStat,
    sink: &dyn BlobSink,
    cfg: &ChunkingConfig,
    policies: &HashMap<String, MixedPolicy>,
) -> Result<(ContentInfo, u64)> {
    let io_err = |e: std::io::Error| Error::io(format!("reading {}", abs.display()), e);
    // Mixed files: hash the portable projection (a small in-memory blob).
    if let Some(id) = &c.decision.mixed {
        let policy = policies
            .get(id)
            .ok_or_else(|| Error::Config(format!("rule refers to unknown mixed-file policy {id:?}")))?;
        if before.size > MAX_MIXED_FILE {
            return Err(Error::invalid(
                "mixed settings file",
                format!("{} is larger than {MAX_MIXED_FILE} bytes", c.path),
            ));
        }
        let mut raw = Vec::with_capacity(before.size as usize);
        fs::File::open(crate::fsutil::long_path(abs)).map_err(io_err)?.read_to_end(&mut raw).map_err(io_err)?;
        let read = raw.len() as u64;
        let projected = mixed::project(policy, &raw)?;
        return Ok((single_blob(&projected, &c.path, c, sink)?, read));
    }

    let strategy = resolve_strategy(c.decision.strategy, &c.path, before.size, cfg);
    let file = fs::File::open(crate::fsutil::long_path(abs)).map_err(io_err)?;
    let mut whole = Hasher::new();
    let mut chunks: Vec<(Hash, u32)> = Vec::new();
    let mut total = 0u64;
    for ch in chunk_stream(file, strategy) {
        let ch = ch.map_err(io_err)?;
        whole.update(&ch.data);
        total += ch.data.len() as u64;
        chunks.push((ch.hash, ch.data.len() as u32));
        sink.offer(ch.hash, &ch.data, KIND_DATA, &c.path, c.decision.compress)?;
    }
    let fh = whole.finalize();
    let info = match chunks.len() {
        0 => ContentInfo { size: 0, hash: Hash::of(b""), recipe: None, chunks: 0 },
        1 => ContentInfo { size: total, hash: chunks[0].0, recipe: None, chunks: 1 },
        n => {
            let recipe = Recipe { chunks };
            let bytes = recipe.encode();
            let rh = Hash::of(&bytes);
            sink.offer(rh, &bytes, KIND_RECIPE, &c.path, crate::rules::Compress::Auto)?;
            ContentInfo { size: total, hash: fh, recipe: Some(rh), chunks: n as u32 }
        }
    };
    let _ = Strategy::Auto;
    Ok((info, total))
}

fn single_blob(bytes: &[u8], path: &str, c: &Candidate, sink: &dyn BlobSink) -> Result<ContentInfo> {
    if bytes.is_empty() {
        return Ok(ContentInfo { size: 0, hash: Hash::of(b""), recipe: None, chunks: 0 });
    }
    let h = Hash::of(bytes);
    sink.offer(h, bytes, KIND_DATA, path, c.decision.compress)?;
    Ok(ContentInfo { size: bytes.len() as u64, hash: h, recipe: None, chunks: 1 })
}

pub struct HashPhase {
    pub hashed: Vec<HashedFile>,
    /// Files that disappeared between the walk and the read.
    pub vanished: Vec<String>,
    pub bytes_read: u64,
}

/// Hash every candidate whose cached entry cannot be reused, in parallel with bounded memory
/// (one chunk per worker), cancellation and progress.
pub fn hash_phase(
    root: &Path,
    candidates: &[&Candidate],
    sink: &dyn BlobSink,
    cfg: &ChunkingConfig,
    policies: &HashMap<String, MixedPolicy>,
    threads: usize,
    ctx: &OpCtx,
) -> Result<HashPhase> {
    let total_bytes: u64 = candidates.iter().map(|c| c.stat.size).sum();
    let total_files = candidates.len() as u64;
    let done_files = AtomicU64::new(0);
    let done_bytes = AtomicU64::new(0);
    let last_report = AtomicU64::new(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|i| format!("packsync-hash-{i}"))
        .build()
        .map_err(|e| Error::other(format!("thread pool: {e}")))?;
    let results: Vec<Result<(Option<HashedFile>, u64)>> = pool.install(|| {
        candidates
            .par_iter()
            .map(|c| {
                ctx.check()?;
                let r = hash_candidate(root, c, sink, cfg, policies)?;
                let n = done_files.fetch_add(1, Ordering::Relaxed) + 1;
                let b = done_bytes.fetch_add(c.stat.size, Ordering::Relaxed) + c.stat.size;
                let now = (now_ns() / 1_000_000) as u64;
                let prev = last_report.load(Ordering::Relaxed);
                if now.saturating_sub(prev) >= 200
                    && last_report.compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
                {
                    ctx.progress(Phase::Snapshot, (n, total_files), (b, total_bytes), format!("hashing {}", c.path));
                }
                Ok((r, c.stat.size))
            })
            .collect()
    });
    let mut hashed = Vec::with_capacity(results.len());
    let mut vanished = vec![];
    let mut bytes = 0;
    for (r, c) in results.into_iter().zip(candidates) {
        let (h, sz) = r?;
        bytes += sz;
        match h {
            Some(h) => hashed.push(h),
            None => vanished.push(c.path.clone()),
        }
    }
    ctx.progress(Phase::Snapshot, (total_files, total_files), (total_bytes, total_bytes), "hashing complete");
    Ok(HashPhase { hashed, vanished, bytes_read: bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::set_mtime_ms;
    use crate::rules::{DEFAULT_RULES_TOML, RuleSet};
    use crate::stage::CountingSink;
    use std::sync::Arc;

    fn rules() -> RuleSet {
        RuleSet::compile(DEFAULT_RULES_TOML, ".minecraft").unwrap()
    }

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    fn stat(size: u64, mtime: i64, ctime: Option<i64>, id: Option<&str>) -> FileStat {
        FileStat {
            kind: FileKind::File,
            size,
            mtime_ns: mtime,
            ctime_ns: ctime,
            file_id: id.map(|s| s.into()),
            executable: false,
        }
    }

    fn row(size: u64, mtime: i64, ctime: Option<i64>, id: Option<&str>, hashed_at: i64, verified_at: i64) -> IndexRow {
        IndexRow {
            path: "p".into(),
            size,
            mtime_ns: mtime,
            ctime_ns: ctime,
            file_id: id.map(|s| s.into()),
            hash: Hash::of(b"x"),
            recipe: None,
            mixed: None,
            content_size: size,
            hashed_at_ns: hashed_at,
            verified_at_ns: verified_at,
        }
    }

    const SEC: i64 = 1_000_000_000;

    #[test]
    fn classify_covers_every_documented_reason() {
        let now = 1_000_000 * SEC;
        let o = ScanOptions { now_ns: now, ..Default::default() };
        let old = now - 100 * SEC; // file last modified long before it was hashed
        let hashed = now - 50 * SEC;
        let base = row(10, old, Some(old), Some("1:2"), hashed, hashed);
        let ok_stat = stat(10, old, Some(old), Some("1:2"));
        assert_eq!(classify("p", &ok_stat, Some(&base), &o), RehashReason::Reuse);
        assert_eq!(classify("p", &ok_stat, None, &o), RehashReason::New);
        assert_eq!(classify("p", &stat(11, old, Some(old), Some("1:2")), Some(&base), &o), RehashReason::SizeChanged);
        assert_eq!(
            classify("p", &stat(10, old + 1, Some(old), Some("1:2")), Some(&base), &o),
            RehashReason::MtimeChanged
        );
        // same size, restored mtime, but the inode change time moved → still found
        assert_eq!(
            classify("p", &stat(10, old, Some(old + 5 * SEC), Some("1:2")), Some(&base), &o),
            RehashReason::CtimeChanged
        );
        // file replaced (different inode) with identical metadata
        assert_eq!(
            classify("p", &stat(10, old, Some(old), Some("1:99")), Some(&base), &o),
            RehashReason::IdentityChanged
        );
        // racy: modified within the window before it was hashed
        let racy_row = row(10, hashed - SEC, Some(hashed - SEC), Some("1:2"), hashed, hashed);
        assert_eq!(
            classify("p", &stat(10, hashed - SEC, Some(hashed - SEC), Some("1:2")), Some(&racy_row), &o),
            RehashReason::Racy
        );
        // periodic: verification overdue
        let stale = row(10, old, Some(old), Some("1:2"), hashed, now - 15 * 86_400 * SEC);
        assert_eq!(classify("p", &ok_stat, Some(&stale), &o), RehashReason::Periodic);
        // full scans rehash everything
        let full = ScanOptions { mode: ScanMode::Full, ..o.clone() };
        assert_eq!(classify("p", &ok_stat, Some(&base), &full), RehashReason::FullScan);
    }

    #[test]
    fn without_ctime_support_the_documented_gap_is_closed_by_periodic_verification() {
        // On file systems with no usable change time, a same-size edit with a restored mtime is
        // invisible to metadata... until the staggered verification period elapses.
        let now = 10_000_000 * SEC;
        let o = ScanOptions { now_ns: now, trust_ctime: false, ..Default::default() };
        let old = now - 1_000 * SEC;
        let r = row(10, old, None, None, old + 10 * SEC, now - 10 * SEC);
        let s = stat(10, old, None, None);
        assert_eq!(classify("p", &s, Some(&r), &o), RehashReason::Reuse, "documented limitation within the period");
        let later = ScanOptions { now_ns: now + 15 * 86_400 * SEC, ..o };
        assert_eq!(
            classify("p", &s, Some(&r), &later),
            RehashReason::Periodic,
            "found at the latest after the verification period"
        );
    }

    #[test]
    fn periodic_verification_is_staggered_across_paths() {
        let now = 10_000_000 * SEC;
        let o = ScanOptions { now_ns: now + 10 * 86_400 * SEC, ..Default::default() };
        let old = now - 1_000 * SEC;
        let due = (0..200)
            .filter(|i| {
                let p = format!("saves/w/region/r.{i}.0.mca");
                let r = row(10, old, Some(old), Some("1:2"), old + 10 * SEC, now);
                classify(&p, &stat(10, old, Some(old), Some("1:2")), Some(&r), &o) == RehashReason::Periodic
            })
            .count();
        assert!(
            due > 20 && due < 180,
            "verification must be spread out, not all-at-once: {due}/200 due after 10 of 14 days"
        );
    }

    #[test]
    fn walk_applies_rules_prunes_and_reports_issues() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, ".minecraft/mods/a.jar", b"jar");
        write(r, ".minecraft/saves/W/level.dat", b"lvl");
        write(r, ".minecraft/saves/W/session.lock", b"x");
        write(r, ".minecraft/logs/latest.log", b"log");
        write(r, ".minecraft/some_new_mod/data.bin", b"12345");
        write(r, ".minecraft/saves/W/datapacks/pack.zip", b"zip");
        fs::create_dir_all(r.join(".minecraft/saves/EmptyWorld/region")).unwrap();
        fs::create_dir_all(r.join(".minecraft/config/empty-cfg-dir")).unwrap();
        write(r, "accounts.json", b"{\"token\":\"x\"}");
        write(r, ".minecraft/mods/CON.txt", b"windows-hostile");
        write(r, ".minecraft/config/A.cfg", b"1");
        write(r, ".minecraft/config/a.cfg", b"2");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", r.join(".minecraft/saves/linked-saves")).unwrap();
            std::os::unix::fs::symlink("/etc/hostname", r.join(".minecraft/mods/link.jar")).unwrap();
        }
        let w = walk(r, &rules(), &HashMap::new(), &ScanOptions::default(), &OpCtx::detached()).unwrap();
        let paths: Vec<&str> = w.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&".minecraft/mods/a.jar"));
        assert!(paths.contains(&".minecraft/saves/W/level.dat"));
        assert!(paths.contains(&".minecraft/saves/W/datapacks/pack.zip"));
        assert!(
            !paths
                .iter()
                .any(|p| p.contains("session.lock") || p.contains("latest.log") || p.contains("accounts.json"))
        );
        assert!(!paths.iter().any(|p| p.contains("some_new_mod")), "unrecognised content is not synced");
        assert!(w.excluded.unmatched_by_dir.contains_key("some_new_mod"));
        assert!(w.excluded.groups.contains_key("sealed:launcher-accounts"));
        assert!(w.excluded.pruned_dirs.iter().any(|(p, ..)| p == ".minecraft/logs"));
        // empty directories inside included areas are kept; the excluded areas' are not
        assert!(w.empty_dirs.contains(&".minecraft/config/empty-cfg-dir".to_string()), "{:?}", w.empty_dirs);
        assert!(w.empty_dirs.contains(&".minecraft/saves/EmptyWorld/region".to_string()));
        let kinds: Vec<IssueKind> = w.issues.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&IssueKind::NonPortableName));
        assert!(kinds.contains(&IssueKind::CaseCollision));
        #[cfg(unix)]
        {
            assert!(kinds.contains(&IssueKind::Symlink), "{:?}", w.issues);
            assert!(!paths.iter().any(|p| p.contains("linked-saves")), "symlinked dirs are never followed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directories_are_reported_not_treated_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, ".minecraft/saves/W/level.dat", b"x");
        write(r, ".minecraft/saves/Locked/level.dat", b"y");
        fs::set_permissions(r.join(".minecraft/saves/Locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let w = walk(r, &rules(), &HashMap::new(), &ScanOptions::default(), &OpCtx::detached()).unwrap();
        fs::set_permissions(r.join(".minecraft/saves/Locked"), fs::Permissions::from_mode(0o755)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(w.unreadable_dirs, vec![".minecraft/saves/Locked".to_string()]);
            assert!(!w.empty_dirs.contains(&".minecraft/saves/Locked".to_string()));
        }
    }

    #[test]
    fn missing_root_is_instance_unavailable() {
        let d = tempfile::tempdir().unwrap();
        let err = walk(&d.path().join("gone"), &rules(), &HashMap::new(), &ScanOptions::default(), &OpCtx::detached())
            .unwrap_err();
        assert!(matches!(err, Error::InstanceUnavailable(_)), "{err}");
    }

    #[test]
    fn hashing_chunks_files_and_records_recipes() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let mut region = vec![0u8; 100_000];
        fastrand::Rng::with_seed(9).fill(&mut region);
        write(r, ".minecraft/saves/W/region/r.0.0.mca", &region);
        write(r, ".minecraft/config/small.json", b"{\"a\":1}");
        write(r, ".minecraft/config/empty.cfg", b"");
        let w = walk(r, &rules(), &HashMap::new(), &ScanOptions::default(), &OpCtx::detached()).unwrap();
        let db = Arc::new(Db::open_memory().unwrap());
        let sink = CountingSink::new(db);
        let cands: Vec<&Candidate> = w.files.iter().collect();
        let out =
            hash_phase(r, &cands, &sink, &ChunkingConfig::default(), &HashMap::new(), 2, &OpCtx::detached()).unwrap();
        let by: HashMap<&str, &HashedFile> = out.hashed.iter().map(|h| (h.path.as_str(), h)).collect();
        let mca = by[".minecraft/saves/W/region/r.0.0.mca"];
        assert_eq!(mca.content.size, 100_000);
        assert_eq!(mca.content.hash, Hash::of(&region), "file hash is the whole-content BLAKE3");
        assert!(mca.content.recipe.is_some());
        assert_eq!(mca.content.chunks, (100_000u32).div_ceil(16384));
        let small = by[".minecraft/config/small.json"];
        assert_eq!(small.content.hash, Hash::of(b"{\"a\":1}"));
        assert!(small.content.recipe.is_none());
        let empty = by[".minecraft/config/empty.cfg"];
        assert_eq!((empty.content.size, empty.content.hash, empty.content.chunks), (0, Hash::of(b""), 0));
    }

    #[test]
    fn a_modification_during_the_read_is_reported_as_unstable() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, ".minecraft/config/busy.json", &vec![b'a'; 3_000_000]);
        let w = walk(r, &rules(), &HashMap::new(), &ScanOptions::default(), &OpCtx::detached()).unwrap();
        let path = r.join(".minecraft/config/busy.json");
        // A sink that rewrites the file mid-read, as an active writer would.
        struct Interfering<'a>(&'a Path, std::sync::atomic::AtomicBool);
        impl BlobSink for Interfering<'_> {
            fn offer(&self, _: Hash, _: &[u8], _: u8, _: &str, _: crate::rules::Compress) -> Result<bool> {
                if !self.1.swap(true, Ordering::SeqCst) || true {
                    let mut data = vec![b'b'; 3_000_001];
                    data[0] = fastrand::u8(..);
                    fs::write(self.0, data).unwrap();
                }
                Ok(true)
            }
        }
        let sink = Interfering(&path, Default::default());
        let c = &w.files[0];
        let err = hash_candidate(r, c, &sink, &ChunkingConfig::default(), &HashMap::new()).unwrap_err();
        assert!(matches!(err, Error::GameRunning(_)), "{err}");
    }

    #[test]
    fn same_size_same_mtime_edit_is_found_by_ctime_during_a_real_scan() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, ".minecraft/config/a.json", b"AAAAAAAA");
        let opts = ScanOptions::default();
        let w1 = walk(r, &rules(), &HashMap::new(), &opts, &OpCtx::detached()).unwrap();
        let db = Arc::new(Db::open_memory().unwrap());
        let cands: Vec<&Candidate> = w1.files.iter().collect();
        let h = hash_phase(
            r,
            &cands,
            &CountingSink::new(db),
            &ChunkingConfig::default(),
            &HashMap::new(),
            1,
            &OpCtx::detached(),
        )
        .unwrap();
        let f = &h.hashed[0];
        // index as the engine would store it, but pretend it was hashed long ago so it is not "racy"
        let mut idx = HashMap::new();
        idx.insert(
            f.path.clone(),
            IndexRow {
                path: f.path.clone(),
                size: f.stat.size,
                mtime_ns: f.stat.mtime_ns,
                ctime_ns: f.stat.ctime_ns,
                file_id: f.stat.file_id.clone(),
                hash: f.content.hash,
                recipe: None,
                mixed: None,
                content_size: f.content.size,
                hashed_at_ns: f.stat.mtime_ns + 100 * SEC,
                verified_at_ns: now_ns(),
            },
        );
        let quiet = ScanOptions { now_ns: f.stat.mtime_ns + 200 * SEC, ..Default::default() };
        let unchanged = walk(r, &rules(), &idx, &quiet, &OpCtx::detached()).unwrap();
        assert_eq!(unchanged.files[0].reason, RehashReason::Reuse);
        // same-size edit, mtime restored to the original value
        std::thread::sleep(Duration::from_millis(20));
        fs::write(r.join(".minecraft/config/a.json"), b"BBBBBBBB").unwrap();
        set_mtime_ms(&r.join(".minecraft/config/a.json"), f.stat.mtime_ns / 1_000_000).unwrap();
        let after = walk(r, &rules(), &idx, &quiet, &OpCtx::detached()).unwrap();
        #[cfg(unix)]
        assert!(
            matches!(after.files[0].reason, RehashReason::CtimeChanged | RehashReason::MtimeChanged),
            "{:?}",
            after.files[0].reason
        );
    }

    use crate::db::Db;
}
