//! Local snapshot creation: scan → hash/chunk changed content → stage new blobs → manifest.
//!
//! A snapshot is created **after all writers have stopped** (the caller checks and holds the
//! instance lock; a stable-read check on every file is the last line of defence). Everything that
//! will have to be uploaded is copied into a private staging directory, so the upload never reads
//! from the mutable live instance. The database commit at the end of [`create`] is the single
//! commit point of "local snapshot complete".

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::chunk::ChunkingConfig;
use crate::db::{Db, IndexRow, InstanceRow, Origin, SnapshotRow, SnapshotState};
use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::ids::{DeviceId, InstanceId, SnapshotId};
use crate::launcher::{PackIdentity, WorldInfo};
use crate::manifest::{
    DeviceRef, Entry, EntryType, InstanceRef, LayoutInfo, Manifest, RulesInfo, SNAPSHOT_VERSION, SessionSummary,
    SnapshotKind, diff_entries,
};
use crate::mixed::MixedPolicy;
use crate::pack::{IndexDoc, IndexPack, read_footer_from_file};
use crate::progress::{Level, OpCtx, Phase};
use crate::rules::RuleSet;
use crate::scan::{
    Candidate, ExcludedSummary, HashedFile, IssueKind, RehashReason, ScanIssue, ScanMode, ScanOptions, WalkResult,
    hash_phase, now_ns, walk,
};
use crate::stage::{StageStats, StagingSink};

/// Tunables for snapshot creation.
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    pub chunking: ChunkingConfig,
    pub pack_target: u64,
    pub zstd_level: i32,
    pub scan: ScanOptions,
    /// Refuse a snapshot that would delete more than this fraction of the base's files…
    pub mass_delete_fraction: f64,
    /// …when at least this many files vanish (small instances are exempt from the fraction rule).
    pub mass_delete_min_files: usize,
    /// Proceed even if directories could not be read (their content is then carried from the base).
    pub allow_unreadable: bool,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            chunking: ChunkingConfig::default(),
            pack_target: 16 * 1024 * 1024,
            zstd_level: 3,
            scan: ScanOptions::default(),
            mass_delete_fraction: 0.5,
            mass_delete_min_files: 25,
            allow_unreadable: false,
        }
    }
}

pub struct SnapshotRequest<'a> {
    pub instance: &'a InstanceRow,
    pub rules: &'a RuleSet,
    pub rules_text: &'a str,
    pub policies: &'a HashMap<String, MixedPolicy>,
    /// The manifest of the instance's base snapshot (what the working tree last matched).
    pub base: Option<(&'a Manifest, SnapshotId)>,
    pub parents: Vec<SnapshotId>,
    pub kind: SnapshotKind,
    pub session: Option<SessionSummary>,
    pub note: Option<String>,
    pub pack: PackIdentity,
    pub worlds: Vec<WorldInfo>,
    pub device: (DeviceId, String),
    pub launcher: String,
    /// Create a snapshot even when the content equals the base (merge commits, promotions).
    pub force: bool,
    /// The user explicitly confirmed a large deletion / missing top-level directories.
    pub confirm_mass_delete: bool,
    pub fork_of: Option<crate::manifest::ForkOrigin>,
    pub new_instance_id: Option<InstanceId>,
}

/// One file's share of a snapshot's work (for `--explain`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathCost {
    pub path: String,
    /// Bytes read and hashed (0 when the cached hash could be reused).
    pub hashed: u64,
    /// New data this file contributed, before and after compression.
    pub new_raw: u64,
    pub new_stored: u64,
}

/// How many files `SnapshotStats::costliest` lists.
pub const COSTLIEST_FILES: usize = 15;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub hashed_files: u64,
    pub reused_files: u64,
    pub hashed_bytes: u64,
    pub new_blobs: u64,
    pub reused_blobs: u64,
    pub new_raw_bytes: u64,
    pub new_stored_bytes: u64,
    /// The snapshot's manifest as stored (compressed) and as JSON.
    #[serde(default)]
    pub manifest_bytes: u64,
    #[serde(default)]
    pub manifest_raw_bytes: u64,
    /// The files that cost the most: new data first, then bytes hashed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub costliest: Vec<PathCost>,
    pub packs: u64,
    pub added: u64,
    pub removed: u64,
    pub modified: u64,
}

pub enum SnapshotOutcome {
    /// The working tree equals the base: nothing was created, nothing needs uploading.
    Unchanged {
        base: SnapshotId,
        stats: SnapshotStats,
        walk: Box<WalkSummary>,
    },
    Created {
        id: SnapshotId,
        manifest: Box<Manifest>,
        stats: SnapshotStats,
        walk: Box<WalkSummary>,
    },
}

/// What the walk learned, minus the bulky per-file data.
#[derive(Debug, Clone, Default)]
pub struct WalkSummary {
    pub issues: Vec<ScanIssue>,
    pub excluded: ExcludedSummary,
    pub files_seen: u64,
}

fn walk_summary(w: &WalkResult) -> WalkSummary {
    WalkSummary { issues: w.issues.clone(), excluded: w.excluded.clone(), files_seen: w.files_seen }
}

pub fn staging_root(state_dir: &Path) -> PathBuf {
    state_dir.join("staging")
}
pub fn snapshot_staging_dir(state_dir: &Path, id: SnapshotId) -> PathBuf {
    staging_root(state_dir).join(id.to_hex())
}
pub fn manifests_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("manifests")
}
pub fn manifest_path(state_dir: &Path, id: SnapshotId) -> PathBuf {
    manifests_dir(state_dir).join(format!("{}.snap", id.to_hex()))
}

/// Everything about a staged snapshot that the uploader needs, stored next to the packs.
#[derive(Debug, Serialize, Deserialize)]
pub struct StagedMeta {
    pub snapshot: SnapshotId,
    pub instance: InstanceId,
    pub packs: Vec<IndexPack>,
    /// Remote packs the *reused* blobs live in (existence is verified before publishing).
    pub reused_packs: Vec<Hash>,
    /// Packs of the blobs referenced by entries that changed relative to the first parent
    /// (verified present in the cloud at publish time).
    pub required_packs: Vec<Hash>,
    /// Built from another snapshot's tree (merge, fork, promotion, rollback) rather than from a scan: its
    /// whole tree is checked against the cloud before it is published.
    #[serde(default)]
    pub derived: bool,
}

/// Blobs staged by earlier local snapshots of this device that are not published yet **and that the new
/// snapshot builds on** (`parents` and their ancestors). A snapshot that is not an ancestor — a crash
/// save that was discarded, a branch that was replaced — may never be published, so depending on its
/// packs would leave a published snapshot with data that is nowhere in the cloud.
pub fn load_known_local_blobs(
    db: &Db,
    state_dir: &Path,
    instance: InstanceId,
    parents: &[SnapshotId],
) -> Result<HashSet<Hash>> {
    Ok(LocalBlobs::of_lineage(db, state_dir, instance, parents)?.map.into_keys().collect())
}

/// Blobs that are staged on this computer and not uploaded yet, with where to read them: the packs of
/// unpublished snapshots, read locally (for example a chunk list that is not in the cloud yet).
#[derive(Default)]
pub struct LocalBlobs {
    map: HashMap<Hash, (PathBuf, crate::pack::BlobEntry)>,
}

impl LocalBlobs {
    /// The staged packs of `parents` and their ancestors that are not published.
    pub fn of_lineage(db: &Db, state_dir: &Path, instance: InstanceId, parents: &[SnapshotId]) -> Result<LocalBlobs> {
        let graph = crate::graph::Graph::load(db, instance)?;
        let mut lineage: HashSet<SnapshotId> = parents.iter().copied().collect();
        for p in parents {
            lineage.extend(graph.ancestors(*p));
        }
        let mut out = LocalBlobs::default();
        for s in db
            .snapshots_in_state(instance, &[SnapshotState::LocalComplete, SnapshotState::Uploading])?
            .into_iter()
            .filter(|s| lineage.contains(&s.id))
        {
            let meta = snapshot_staging_dir(state_dir, s.id).join("staged.json");
            if let Ok(b) = fs::read(&meta)
                && let Ok(m) = serde_json::from_slice::<StagedMeta>(&b)
            {
                out.add(state_dir, s.id, &m.packs);
            }
        }
        Ok(out)
    }

    /// The packs staged with snapshot `id`.
    pub fn of_snapshot(state_dir: &Path, id: SnapshotId, packs: &[IndexPack]) -> LocalBlobs {
        let mut out = LocalBlobs::default();
        out.add(state_dir, id, packs);
        out
    }

    fn add(&mut self, state_dir: &Path, id: SnapshotId, packs: &[IndexPack]) {
        for p in packs {
            let path = snapshot_staging_dir(state_dir, id).join(format!("{}.pack", p.id.to_hex()));
            for e in &p.blobs {
                self.map.insert(e.hash, (path.clone(), e.clone()));
            }
        }
    }

    pub fn contains(&self, h: &Hash) -> bool {
        self.map.contains_key(h)
    }

    pub fn hashes(&self) -> HashSet<Hash> {
        self.map.keys().copied().collect()
    }

    /// Read and verify one staged blob.
    pub fn read(&self, h: &Hash) -> Result<Option<Vec<u8>>> {
        use std::io::{Read, Seek, SeekFrom};
        let Some((path, e)) = self.map.get(h) else { return Ok(None) };
        let mut f = fs::File::open(path).map_err(|err| Error::io(format!("reading {}", path.display()), err))?;
        f.seek(SeekFrom::Start(e.offset)).map_err(|err| Error::io(format!("reading {}", path.display()), err))?;
        let mut buf = vec![0u8; e.stored_len as usize];
        f.read_exact(&mut buf).map_err(|err| Error::io(format!("reading {}", path.display()), err))?;
        crate::remote::verify_blob(e, &buf).map(Some)
    }
}

fn entry_from(h: &HashedFile) -> Entry {
    Entry {
        p: h.path.clone(),
        t: EntryType::File,
        s: h.content.size,
        m: h.stat.mtime_ns.div_euclid(1_000_000),
        x: h.stat.executable,
        h: Some(h.content.hash),
        r: h.content.recipe,
        mx: h.mixed.clone(),
    }
}

fn entry_from_row(path: &str, mtime_ns: i64, exec: bool, row: &IndexRow) -> Entry {
    Entry {
        p: path.to_string(),
        t: EntryType::File,
        s: row.content_size,
        m: mtime_ns.div_euclid(1_000_000),
        x: exec,
        h: Some(row.hash),
        r: row.recipe,
        mx: row.mixed.clone(),
    }
}

/// A path that was synced before but is now a symbolic link (or special file) is **unknown**, not
/// deleted: links are never followed, so the content behind one cannot be seen. Recording it as a
/// deletion would delete the data on every other computer.
pub(crate) fn symlink_guard(issues: &[ScanIssue], base: Option<&Manifest>) -> Result<()> {
    let Some(base) = base else { return Ok(()) };
    for iss in issues.iter().filter(|i| matches!(i.kind, IssueKind::Symlink | IssueKind::SpecialFile)) {
        let prefix = format!("{}/", iss.path);
        let at = base.entries.partition_point(|e| e.p.as_bytes() < prefix.as_bytes());
        let beneath = base.entries.get(at).is_some_and(|e| e.p.starts_with(&prefix));
        if base.entry(&iss.path).is_some() || beneath {
            return Err(Error::SafetyGuard(format!(
                "{} was synced before but is now a symbolic link or special file. PackSync never follows links, so it cannot see what is inside \
                 and will not record it as deleted. Restore the real folder, or add an exclude rule if you meant to stop syncing it",
                iss.path
            )));
        }
    }
    Ok(())
}

/// Guard against turning an unavailable drive or a mis-selected folder into a mass deletion.
fn check_instance_available(
    inst: &InstanceRow,
    walk: &WalkResult,
    base: Option<&Manifest>,
    cfg: &SnapshotConfig,
    confirm: bool,
) -> Result<()> {
    let marker = inst.root.join(crate::brand::LOCAL_MARKER_DIR).join("marker");
    if inst.sync_enabled {
        match fs::read_to_string(&marker) {
            Ok(m) if m.trim() == inst.marker => {}
            Ok(_) => {
                return Err(Error::InstanceUnavailable(format!(
                    "{} contains a different instance's marker: the folder was replaced or moved",
                    inst.root.display()
                )));
            }
            Err(_) => {
                return Err(Error::InstanceUnavailable(format!(
                    "{} is missing PackSync's marker file. If it is on a removable or network drive, make sure the drive is mounted; \
                     nothing was changed in the cloud",
                    inst.root.display()
                )));
            }
        }
    }
    symlink_guard(&walk.issues, base)?;
    if !walk.unreadable_dirs.is_empty() && !cfg.allow_unreadable {
        return Err(Error::InstanceUnavailable(format!(
            "{} directories could not be read (first: {}); their content is unknown, so no snapshot was taken",
            walk.unreadable_dirs.len(),
            walk.unreadable_dirs[0]
        )));
    }
    if let Some(base) = base {
        let now: HashSet<&str> = walk.files.iter().map(|f| f.path.as_str()).collect();
        let base_files: Vec<&Entry> = base.entries.iter().filter(|e| e.is_file()).collect();
        let vanished = base_files.iter().filter(|e| !now.contains(e.p.as_str())).count();
        // Top-level directories that existed in the base and are completely gone.
        let top = |p: &str| {
            p.split('/').take(if base.layout.game_root.is_empty() { 1 } else { 2 }).collect::<Vec<_>>().join("/")
        };
        let base_tops: HashSet<String> = base_files.iter().map(|e| top(&e.p)).collect();
        let now_tops: HashSet<String> =
            walk.files.iter().map(|f| top(&f.path)).chain(walk.empty_dirs.iter().map(|d| top(d))).collect();
        let gone_tops: Vec<&String> = base_tops.iter().filter(|t| !now_tops.contains(*t)).collect();
        let frac = if base_files.is_empty() { 0.0 } else { vanished as f64 / base_files.len() as f64 };
        let mass = vanished >= cfg.mass_delete_min_files && frac > cfg.mass_delete_fraction;
        let tops_gone = !gone_tops.is_empty()
            && base_files.len() >= cfg.mass_delete_min_files
            && gone_tops
                .iter()
                .any(|t| base_files.iter().filter(|e| top(&e.p) == **t).count() >= cfg.mass_delete_min_files / 2);
        if (mass || tops_gone) && !confirm {
            return Err(Error::SafetyGuard(format!(
                "{vanished} of {} synced files are missing{}. If the drive or folder is unavailable nothing should be deleted; \
                 if you really removed them, confirm explicitly to record the deletion",
                base_files.len(),
                if gone_tops.is_empty() {
                    String::new()
                } else {
                    format!(" (including whole folders such as {})", gone_tops[0])
                }
            )));
        }
    }
    Ok(())
}

/// Create a local snapshot. The caller holds the instance lock and has verified no game is running.
pub fn create(
    db: &Arc<Db>,
    state_dir: &Path,
    req: &SnapshotRequest<'_>,
    cfg: &SnapshotConfig,
    ctx: &OpCtx,
) -> Result<SnapshotOutcome> {
    let inst = req.instance;
    ctx.phase(Phase::Scan, format!("scanning {}", inst.name));
    let index = if inst.needs_full_scan { HashMap::new() } else { db.index_load(inst.id)? };
    let mut scan_opts = cfg.scan.now();
    if inst.needs_full_scan {
        scan_opts.mode = ScanMode::Full;
    }
    let started_ns = now_ns();
    let w = walk(&inst.root, req.rules, &index, &scan_opts, ctx)?;
    check_instance_available(inst, &w, req.base.map(|b| b.0), cfg, req.confirm_mass_delete)?;
    crate::failpoint!("snapshot.after_walk")?;

    // ---- hash/chunk what changed -------------------------------------------------------------
    ctx.phase(Phase::Snapshot, "hashing changed files");
    let known_local = load_known_local_blobs(db, state_dir, inst.id, &req.parents)?;
    let known_local_copy = known_local.clone();
    let tmp_dir = staging_root(state_dir).join(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let sink = StagingSink::new(db.clone(), tmp_dir.clone(), cfg.pack_target, cfg.zstd_level, known_local)?;
    let to_hash: Vec<&Candidate> = w.files.iter().filter(|c| c.reason != RehashReason::Reuse).collect();
    let phase = match hash_phase(&inst.root, &to_hash, &sink, &cfg.chunking, req.policies, scan_opts.threads, ctx) {
        Ok(p) => p,
        Err(e) => {
            if !crate::failpoint::is_crash(&e) {
                sink.discard();
            }
            return Err(e);
        }
    };
    crate::failpoint!("snapshot.after_hash")?;

    // ---- assemble entries ----------------------------------------------------------------------
    let hashed: HashMap<&str, &HashedFile> = phase.hashed.iter().map(|h| (h.path.as_str(), h)).collect();
    let mut entries: Vec<Entry> = Vec::with_capacity(w.files.len() + w.empty_dirs.len());
    let mut new_index: Vec<IndexRow> = Vec::with_capacity(w.files.len());
    let verified_now = now_ns();
    for c in &w.files {
        if let Some(row) = &c.reuse {
            entries.push(entry_from_row(&c.path, c.stat.mtime_ns, c.stat.executable, row));
            let mut r = row.clone();
            r.mtime_ns = c.stat.mtime_ns;
            new_index.push(r);
        } else if let Some(h) = hashed.get(c.path.as_str()) {
            entries.push(entry_from(h));
            new_index.push(IndexRow {
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
                verified_at_ns: verified_now,
            });
        }
        // else: vanished between walk and read → deleted, simply absent
    }
    for d in &w.empty_dirs {
        entries.push(Entry::dir(d.clone()));
    }

    let staged = match sink.finish() {
        Ok(s) => s,
        Err(e) => {
            if !crate::failpoint::is_crash(&e) {
                let _ = fs::remove_dir_all(&tmp_dir);
            }
            return Err(e);
        }
    };

    let mut m = Manifest::new(
        InstanceRef { id: req.new_instance_id.unwrap_or(inst.id), name: inst.name.clone() },
        DeviceRef { id: req.device.0, name: req.device.1.clone() },
        LayoutInfo { launcher: req.launcher.clone(), game_root: inst.game_root.clone() },
        RulesInfo { version: 1, hash: Hash::of(req.rules_text.as_bytes()), text: req.rules_text.to_string() },
    );
    m.version = SNAPSHOT_VERSION;
    m.parents = req.parents.clone();
    m.kind = req.kind;
    m.session = req.session.clone();
    m.note = req.note.clone();
    m.pack = req.pack.clone();
    m.worlds = req.worlds.clone();
    m.fork_of = req.fork_of.clone();
    if !cfg!(unix) {
        // this computer cannot see the executable bit: keep what the base recorded
        if let Some((b, _)) = req.base {
            for e in entries.iter_mut().filter(|e| e.is_file()) {
                if let Some(be) = b.entry(&e.p) {
                    e.x = be.x;
                }
            }
        }
    }
    m.entries = entries;
    m.finalize()?;

    // ---- changed at all? --------------------------------------------------------------------------
    let diff = req.base.map(|(b, _)| diff_entries(&b.entries, &m.entries)).unwrap_or_default();
    let summary = crate::manifest::summarize(&diff);
    let identical_to_base = req.base.is_some_and(|(b, _)| {
        diff.is_empty() && b.pack == m.pack && b.rules.hash == m.rules.hash && b.layout == m.layout
    });
    let mut stats = SnapshotStats {
        files: m.stats.files,
        dirs: m.stats.dirs,
        bytes: m.stats.bytes,
        hashed_files: phase.hashed.len() as u64,
        reused_files: (w.files.len() - phase.hashed.len() - phase.vanished.len()) as u64,
        hashed_bytes: phase.bytes_read,
        new_blobs: staged.stats.blobs_new,
        reused_blobs: staged.stats.blobs_reused,
        new_raw_bytes: staged.stats.raw_bytes_new,
        new_stored_bytes: staged.stats.stored_bytes_new,
        manifest_bytes: 0,
        manifest_raw_bytes: 0,
        costliest: costliest(&phase.hashed, &staged.stats.new_by_path),
        packs: staged.packs.len() as u64,
        added: summary.added,
        removed: summary.removed,
        modified: summary.modified,
    };
    let _ = StageStats::default();
    let wsum = Box::new(walk_summary(&w));

    // Index bookkeeping happens even when nothing changed (fresh stats, verification times).
    let commit_index = |db: &Db| -> Result<()> {
        // by difference, unless this was a full scan (which starts from an empty index)
        if inst.needs_full_scan {
            db.index_replace(inst.id, &new_index)?;
        } else {
            db.index_update(inst.id, &new_index, &index)?;
        }
        db.instance_update(inst.id, |r| {
            r.last_scan_ms = Some(crate::db::now_ms());
            if scan_opts.mode == ScanMode::Full {
                r.last_full_verify_ms = Some(crate::db::now_ms());
            }
            r.needs_full_scan = false;
        })?;
        Ok(())
    };

    if identical_to_base && !req.force {
        // Nothing to publish: drop anything staged (blobs we only "newly" staged because the cloud
        // did not know them yet are not needed by any snapshot).
        let _ = fs::remove_dir_all(&tmp_dir);
        commit_index(db)?;
        let base = req.base.expect("identical_to_base implies a base").1;
        stats.packs = 0;
        stats.new_blobs = 0;
        return Ok(SnapshotOutcome::Unchanged { base, stats, walk: wsum });
    }

    ctx.mark("snapshot: encode the manifest");
    let (id, stored, raw_len) = m.encode_sized()?;
    stats.manifest_bytes = stored.len() as u64;
    stats.manifest_raw_bytes = raw_len as u64;

    // ---- required packs for publish-time verification -------------------------------------------
    let required =
        required_packs(db, &m, req.base.map(|b| b.0), &staged.packs, &known_local_copy).inspect_err(|_| {
            // do not leave gigabytes of staged packs behind for the hourly orphan cleanup
            let _ = fs::remove_dir_all(&tmp_dir);
        })?;

    // ---- make it durable: staging dir, manifest file, then the single DB commit -------------------
    ctx.phase(Phase::Snapshot, "saving local snapshot");
    let final_dir = snapshot_staging_dir(state_dir, id);
    let meta = StagedMeta {
        snapshot: id,
        instance: m.instance.id,
        packs: staged.packs.iter().map(|p| IndexPack { id: p.id, size: p.size, blobs: p.entries.clone() }).collect(),
        reused_packs: staged.reused_packs.iter().copied().collect(),
        required_packs: required,
        derived: false,
    };
    fs::write(tmp_dir.join("staged.json"), serde_json::to_vec(&meta)?)
        .map_err(|e| Error::io("writing staged metadata", e))?;
    if final_dir.exists() {
        // The identical snapshot was staged before (idempotent re-run).
        let _ = fs::remove_dir_all(&tmp_dir);
    } else {
        fs::rename(&tmp_dir, &final_dir)
            .map_err(|e| Error::io(format!("moving staging into {}", final_dir.display()), e))?;
        crate::fsutil::fsync_dir(&staging_root(state_dir));
    }
    crate::failpoint!("snapshot.after_stage_move")?;
    crate::fsutil::write_atomic(&manifest_path(state_dir, id), &stored)
        .map_err(|e| Error::io("writing manifest", e))?;
    crate::failpoint!("snapshot.before_db_commit")?;
    ctx.mark("snapshot: record it in the database");

    let row = SnapshotRow {
        id,
        instance_id: m.instance.id,
        kind: m.kind,
        parents: m.parents.clone(),
        parents_known: true,
        device_id: Some(m.device.id),
        device_name: Some(m.device.name.clone()),
        created: Some(m.created.clone()),
        origin: Origin::Local,
        state: SnapshotState::LocalComplete,
        files: m.stats.files,
        bytes: m.stats.bytes,
        note: m.note.clone(),
        session_outcome: m.session.as_ref().and_then(|s| s.outcome),
        published_ms: None,
        visible_ms: None,
        has_manifest: true,
    };
    let staged_rows: Vec<(Hash, PathBuf, u64)> = staged
        .packs
        .iter()
        .map(|p| (p.id, final_dir.join(p.path.file_name().expect("pack file name")), p.size))
        .collect();
    // Only snapshots of the instance being synced move its base pointer and file index; forks pass their own.
    let moves_base = m.instance.id == inst.id;
    db.commit_local_snapshot(&crate::db::LocalSnapshotCommit {
        row: &row,
        staged: &staged_rows,
        index: moves_base.then_some((inst.id, &new_index[..])),
        // a full scan starts from an empty index: replace it as a whole
        index_before: (moves_base && !inst.needs_full_scan).then_some(&index),
        full_scan: scan_opts.mode == ScanMode::Full,
    })?;
    crate::failpoint!("snapshot.after_db_commit")?;
    ctx.note(
        Phase::Snapshot,
        Level::Info,
        format!(
            "local snapshot {} saved: {} files, {} new blobs ({} KiB stored)",
            id.short(),
            m.stats.files,
            stats.new_blobs,
            stats.new_stored_bytes / 1024
        ),
    );
    let _ = started_ns;
    Ok(SnapshotOutcome::Created { id, manifest: Box::new(m), stats, walk: wsum })
}

fn costliest(hashed: &[crate::scan::HashedFile], new_by_path: &HashMap<String, (u64, u64)>) -> Vec<PathCost> {
    let mut by: HashMap<&str, PathCost> = HashMap::new();
    for h in hashed {
        let e = by.entry(h.path.as_str()).or_insert_with(|| PathCost { path: h.path.clone(), ..Default::default() });
        e.hashed = h.content.size;
    }
    for (p, (raw, stored)) in new_by_path {
        let e = by.entry(p.as_str()).or_insert_with(|| PathCost { path: p.clone(), ..Default::default() });
        e.new_raw = *raw;
        e.new_stored = *stored;
    }
    let mut v: Vec<PathCost> = by.into_values().collect();
    v.sort_by(|a, b| b.new_stored.cmp(&a.new_stored).then(b.hashed.cmp(&a.hashed)).then(a.path.cmp(&b.path)));
    v.truncate(COSTLIEST_FILES);
    v
}

/// Packs holding the blobs referenced by entries that are new or changed relative to the base.
/// Unchanged entries inherit the base's guarantees (it was published and verified).
fn required_packs(
    db: &Db,
    m: &Manifest,
    base: Option<&Manifest>,
    staged: &[crate::pack::FinishedPack],
    known_local: &HashSet<Hash>,
) -> Result<Vec<Hash>> {
    let staged_blobs: HashSet<Hash> = staged.iter().flat_map(|p| p.entries.iter().map(|e| e.hash)).collect();
    // An empty file has no stored data at all (its hash is that of nothing), so there is nothing to find.
    let has_data = |e: &&Entry| e.is_file() && e.s > 0;
    let changed: Vec<&Entry> = match base {
        Some(b) => {
            let bm: HashMap<&str, &Entry> = b.entries.iter().map(|e| (e.p.as_str(), e)).collect();
            m.entries
                .iter()
                .filter(has_data)
                .filter(|e| bm.get(e.p.as_str()).is_none_or(|be| !be.same_content(e) || be.r != e.r))
                .collect()
        }
        None => m.entries.iter().filter(has_data).collect(),
    };
    let mut out: HashSet<Hash> = HashSet::new();
    for e in changed {
        for h in [e.r, if e.r.is_none() { e.h } else { None }].into_iter().flatten() {
            if staged_blobs.contains(&h) || known_local.contains(&h) {
                continue; // in this snapshot's own packs, or in those of an unpublished ancestor (uploaded first)
            }
            match db.blob_lookup(h)? {
                Some((pack, _)) => {
                    out.insert(pack);
                }
                // A blob this snapshot refers to that is neither staged, nor in an ancestor's packs, nor known
                // to be in the cloud would be published as a hole. It must not be possible; refuse loudly.
                None => {
                    return Err(Error::other(format!(
                        "internal error: the data of {:?} (blob {}) was not saved with this snapshot; \
                         nothing was published. Please report this",
                        e.p,
                        h.short()
                    )));
                }
            }
        }
    }
    let mut v: Vec<Hash> = out.into_iter().collect();
    v.sort();
    Ok(v)
}

/// Arguments for a snapshot that is derived from an existing manifest instead of a scan: merge
/// commits (conflict resolution), promotions of crash saves, forks and rollback commits. The tree is
/// the source's tree, so no content has to be staged and nothing is read from the instance.
pub struct DeriveRequest<'a> {
    pub source: &'a Manifest,
    /// Override the instance identity (forking into a separate instance).
    pub instance: Option<(InstanceId, String)>,
    pub parents: Vec<SnapshotId>,
    pub kind: SnapshotKind,
    pub device: (DeviceId, String),
    pub note: Option<String>,
    pub session: Option<SessionSummary>,
    pub fork_of: Option<crate::manifest::ForkOrigin>,
}

pub fn create_derived(db: &Db, state_dir: &Path, req: &DeriveRequest<'_>) -> Result<(SnapshotId, Manifest)> {
    let mut m = req.source.clone();
    if let Some((id, name)) = &req.instance {
        m.instance = InstanceRef { id: *id, name: name.clone() };
    }
    m.device = DeviceRef { id: req.device.0, name: req.device.1.clone() };
    m.created = crate::manifest::now_rfc3339();
    m.parents = req.parents.clone();
    m.kind = req.kind;
    m.note = req.note.clone();
    m.session = req.session.clone();
    m.fork_of = req.fork_of.clone();
    m.finalize()?;
    let (id, stored) = m.encode()?;
    let dir = snapshot_staging_dir(state_dir, id);
    fs::create_dir_all(&dir).map_err(|e| Error::io("creating staging directory", e))?;
    let meta = StagedMeta {
        snapshot: id,
        instance: m.instance.id,
        packs: vec![],
        reused_packs: vec![],
        required_packs: vec![],
        derived: true,
    };
    fs::write(dir.join("staged.json"), serde_json::to_vec(&meta)?)
        .map_err(|e| Error::io("writing staged metadata", e))?;
    crate::fsutil::write_atomic(&manifest_path(state_dir, id), &stored)
        .map_err(|e| Error::io("writing manifest", e))?;
    let row = SnapshotRow {
        id,
        instance_id: m.instance.id,
        kind: m.kind,
        parents: m.parents.clone(),
        parents_known: true,
        device_id: Some(m.device.id),
        device_name: Some(m.device.name.clone()),
        created: Some(m.created.clone()),
        origin: Origin::Local,
        state: SnapshotState::LocalComplete,
        files: m.stats.files,
        bytes: m.stats.bytes,
        note: m.note.clone(),
        session_outcome: m.session.as_ref().and_then(|s| s.outcome),
        published_ms: None,
        visible_ms: None,
        has_manifest: true,
    };
    db.commit_local_snapshot(&crate::db::LocalSnapshotCommit {
        row: &row,
        staged: &[],
        index: None,
        index_before: None,
        full_scan: false,
    })?;
    Ok((id, m))
}

/// Re-read `index` documents of staged packs from disk (used by the uploader).
pub fn load_staged_meta(state_dir: &Path, id: SnapshotId) -> Result<StagedMeta> {
    let p = snapshot_staging_dir(state_dir, id).join("staged.json");
    let b = fs::read(&p).map_err(|e| Error::io(format!("reading {}", p.display()), e))?;
    let m: StagedMeta = serde_json::from_slice(&b)?;
    // Defence in depth: the staged packs on disk must still match what was recorded.
    for p in &m.packs {
        let path = snapshot_staging_dir(state_dir, id).join(format!("{}.pack", p.id.to_hex()));
        let len =
            fs::metadata(&path).map_err(|e| Error::io(format!("staged pack {} is missing", p.id.short()), e))?.len();
        if len != p.size {
            return Err(Error::Corrupt(format!("staged pack {} changed size on disk", p.id.short())));
        }
    }
    Ok(m)
}

/// Build the index document for a staged snapshot.
pub fn staged_index_doc(meta: &StagedMeta) -> IndexDoc {
    IndexDoc::new(meta.packs.clone())
}

pub fn verify_staged_pack_footer(state_dir: &Path, id: SnapshotId, pack: Hash) -> Result<()> {
    let path = snapshot_staging_dir(state_dir, id).join(format!("{}.pack", pack.to_hex()));
    read_footer_from_file(&path).map(|_| ())
}

/// Treat unreadable-directory style issues as hard errors for callers that don't allow them.
pub fn has_blocking_issues(w: &WalkSummary) -> bool {
    w.issues.iter().any(|i| matches!(i.kind, IssueKind::Unreadable))
}

#[cfg(test)]
mod tests {
    // Behavioural tests live in `crate::engine` (they need the full harness).
}
