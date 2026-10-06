//! The restore transaction journal.
//!
//! Applying a snapshot touches many files in many directories, which no file system can do
//! atomically. We therefore never claim atomicity; we guarantee **recoverability**:
//!
//! * everything that will be written is first *staged* and verified outside the live paths;
//! * every replaced or deleted file is *moved* (atomic rename) into a per-transaction trash
//!   directory, never unlinked;
//! * the journal records the ordered operation list up front and the batch boundaries as the
//!   apply progresses; each operation is a sequence of atomic renames whose effect can be read
//!   back from the file system, so recovery is idempotent even for the batch that was in flight;
//! * until an `applied` record exists, recovery **rolls back** (the trash and the stage let it put
//!   every file back); after `applied` it rolls forward (finalising bookkeeping only).
//!
//! Layout inside the instance root (never synced, see the sealed `.packsync/**` rule):
//! `.packsync/tx/<tx>/{journal.jsonl, stage/, trash/, blobs/}`; successful transactions move the
//! trash to `.packsync/recovery/<tx>/` so the previous state stays restorable for a while.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::brand::LOCAL_MARKER_DIR;
use crate::error::{Error, IoContext, Result};
use crate::fsutil::{fsync_dir, long_path};
use crate::ids::{InstanceId, SnapshotId, TxId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum JOp {
    /// Move a local file to the trash.
    Delete { p: String },
    /// Remove a directory that the deletions emptied (attempt; skipped when not empty).
    RmDir { p: String },
    /// Create a directory that did not exist.
    MkDir { p: String },
    /// Place the staged file at `p`; when `existed`, the current file is trashed first.
    Write { p: String, existed: bool },
}

impl JOp {
    pub fn path(&self) -> &str {
        match self {
            JOp::Delete { p } | JOp::RmDir { p } | JOp::MkDir { p } | JOp::Write { p, .. } => p,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Rec {
    Begin {
        tx: TxId,
        target: SnapshotId,
        base: Option<SnapshotId>,
        instance: InstanceId,
        created: String,
        /// When set, the restore is one the user asked for (a history restore): after it is applied it
        /// must be recorded as a new snapshot on top of `base` so that it sticks. Recovery uses this to
        /// finish the job when the process died after applying the files.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        derive_note: Option<String>,
    },
    Ops {
        ops: Vec<JOp>,
    },
    /// All new content is staged and verified; nothing in the live tree has been touched yet.
    Staged,
    BatchBegin {
        upto: usize,
    },
    BatchDone {
        upto: usize,
    },
    /// Every operation ran. From here recovery rolls forward.
    Applied,
    /// Bookkeeping (database) updated.
    Committed,
    Finalized,
    RolledBack,
}

pub struct Journal {
    file: File,
}

impl Journal {
    pub fn create(path: &Path) -> Result<Journal> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(long_path(p)).io_ctx(|| format!("creating {}", p.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(long_path(path))
            .io_ctx(|| format!("opening journal {}", path.display()))?;
        Ok(Journal { file })
    }

    pub fn append(&mut self, rec: &Rec, durable: bool) -> Result<()> {
        let mut line = serde_json::to_vec(rec)?;
        line.push(b'\n');
        self.file.write_all(&line).io_ctx(|| "writing journal".into())?;
        if durable {
            self.file.sync_data().io_ctx(|| "syncing journal".into())?;
        }
        Ok(())
    }

    /// Read every well-formed record. A torn final line (crash mid-write) is ignored: it can only be
    /// a record whose effects had not been relied upon yet.
    pub fn read_all(path: &Path) -> Result<Vec<Rec>> {
        let f = File::open(long_path(path)).io_ctx(|| format!("opening journal {}", path.display()))?;
        let mut out = vec![];
        let mut lines = BufReader::new(f).lines().peekable();
        while let Some(line) = lines.next() {
            let line = line.io_ctx(|| "reading journal".into())?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Rec>(&line) {
                Ok(r) => out.push(r),
                Err(_) if lines.peek().is_none() => break, // torn tail
                Err(e) => return Err(Error::Corrupt(format!("journal {} is damaged: {e}", path.display()))),
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Default, Clone)]
pub struct JournalState {
    pub begin: Option<Rec>,
    pub ops: Vec<JOp>,
    pub staged: bool,
    pub begun_upto: usize,
    pub done_upto: usize,
    pub applied: bool,
    pub committed: bool,
    pub finalized: bool,
    pub rolled_back: bool,
}

impl JournalState {
    pub fn from_records(recs: &[Rec]) -> JournalState {
        let mut s = JournalState::default();
        for r in recs {
            match r {
                Rec::Begin { .. } => s.begin = Some(r.clone()),
                Rec::Ops { ops } => s.ops = ops.clone(),
                Rec::Staged => s.staged = true,
                Rec::BatchBegin { upto } => s.begun_upto = s.begun_upto.max(*upto),
                Rec::BatchDone { upto } => s.done_upto = s.done_upto.max(*upto),
                Rec::Applied => s.applied = true,
                Rec::Committed => s.committed = true,
                Rec::Finalized => s.finalized = true,
                Rec::RolledBack => s.rolled_back = true,
            }
        }
        s
    }
    pub fn target(&self) -> Option<SnapshotId> {
        match &self.begin {
            Some(Rec::Begin { target, .. }) => Some(*target),
            _ => None,
        }
    }
    pub fn tx(&self) -> Option<TxId> {
        match &self.begin {
            Some(Rec::Begin { tx, .. }) => Some(*tx),
            _ => None,
        }
    }
    pub fn base(&self) -> Option<SnapshotId> {
        match &self.begin {
            Some(Rec::Begin { base, .. }) => *base,
            _ => None,
        }
    }
    pub fn derive_note(&self) -> Option<String> {
        match &self.begin {
            Some(Rec::Begin { derive_note, .. }) => derive_note.clone(),
            _ => None,
        }
    }
}

pub fn tx_root(root: &Path) -> PathBuf {
    root.join(LOCAL_MARKER_DIR).join("tx")
}
pub fn tx_dir(root: &Path, tx: TxId) -> PathBuf {
    tx_root(root).join(tx.to_string())
}
pub fn recovery_root(root: &Path) -> PathBuf {
    root.join(LOCAL_MARKER_DIR).join("recovery")
}

/// Join a snapshot-relative path below `base` (the caller validated the path already).
fn under(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    p
}

pub fn live_path(root: &Path, rel: &str) -> PathBuf {
    under(root, rel)
}
pub fn stage_path(txd: &Path, rel: &str) -> PathBuf {
    under(&txd.join("stage"), rel)
}
pub fn trash_path(txd: &Path, rel: &str) -> PathBuf {
    under(&txd.join("trash"), rel)
}

fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(long_path(from), long_path(to))
}

fn ensure_parent(p: &Path) -> std::io::Result<()> {
    match p.parent() {
        Some(d) => fs::create_dir_all(long_path(d)),
        None => Ok(()),
    }
}

/// Undo operations `ops[..begun_upto]` in reverse order using the file system's own state, so it is
/// safe to run any number of times and after a crash at any point inside an operation.
pub fn rollback_ops(root: &Path, txd: &Path, ops: &[JOp], begun_upto: usize) -> Result<()> {
    for op in ops[..begun_upto.min(ops.len())].iter().rev() {
        let live = live_path(root, op.path());
        let stage = stage_path(txd, op.path());
        let trash = trash_path(txd, op.path());
        match op {
            JOp::Write { existed, .. } => {
                // Placed already? Then the live file is our own staged content: move it BACK to the
                // stage rather than deleting it. Rollback must never destroy information, otherwise
                // a second run (recovery interrupted and repeated) would see "stage missing, live
                // present" and delete the original we just restored.
                if !stage.exists() && live.exists() {
                    ensure_parent(&stage).io_ctx(|| "preparing stage directory".into())?;
                    rename(&live, &stage).io_ctx(|| format!("un-placing {}", live.display()))?;
                }
                if *existed && trash.exists() && !live.exists() {
                    ensure_parent(&live).io_ctx(|| "restoring directories".into())?;
                    rename(&trash, &live).io_ctx(|| format!("restoring {}", live.display()))?;
                }
            }
            JOp::Delete { .. } => {
                if trash.exists() && !live.exists() {
                    ensure_parent(&live).io_ctx(|| "restoring directories".into())?;
                    rename(&trash, &live).io_ctx(|| format!("restoring {}", live.display()))?;
                }
            }
            JOp::MkDir { .. } => {
                let _ = fs::remove_dir(long_path(&live)); // only succeeds when empty
            }
            JOp::RmDir { .. } => {
                if !live.exists() {
                    fs::create_dir_all(long_path(&live)).io_ctx(|| format!("restoring {}", live.display()))?;
                }
            }
        }
    }
    Ok(())
}

/// Move the trash of a finished transaction to the recovery area and drop the rest of the tx dir.
pub fn finalize_tx(root: &Path, tx: TxId, note: &RecoveryNote) -> Result<()> {
    let txd = tx_dir(root, tx);
    let trash = txd.join("trash");
    let rec = recovery_root(root).join(tx.to_string());
    if trash.exists() && fs::read_dir(long_path(&trash)).map(|mut d| d.next().is_some()).unwrap_or(false) {
        fs::create_dir_all(long_path(&rec)).io_ctx(|| "creating recovery directory".into())?;
        rename(&trash, &rec.join("files")).io_ctx(|| "moving trash to the recovery area".into())?;
        crate::failpoint!("restore.after_finalize_move")?;
        let _ = crate::fsutil::write_atomic(&rec.join("recovery.json"), &serde_json::to_vec_pretty(note)?);
    }
    let _ = fs::remove_dir_all(long_path(&txd));
    fsync_dir(&tx_root(root));
    Ok(())
}

/// What a recovery point contains and why it exists (shown in the UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryNote {
    pub tx: TxId,
    pub created: String,
    pub from: Option<SnapshotId>,
    pub to: SnapshotId,
    pub files: u64,
    pub bytes: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Nothing had touched the live tree; the transaction directory was removed.
    Discarded,
    /// The live tree was put back exactly as it was before the transaction.
    RolledBack,
    /// The transaction had applied everything: finish bookkeeping for `target`.
    RollForward { target: SnapshotId, tx: TxId, base: Option<SnapshotId>, derive_note: Option<String> },
    /// Already complete.
    Nothing,
}

/// Recover one transaction directory found under `root`. Idempotent.
pub fn recover_tx(root: &Path, txd: &Path) -> Result<RecoveryAction> {
    let jp = txd.join("journal.jsonl");
    if !jp.exists() {
        let _ = fs::remove_dir_all(long_path(txd));
        return Ok(RecoveryAction::Discarded);
    }
    let st = JournalState::from_records(&Journal::read_all(&jp)?);
    if st.finalized || st.rolled_back {
        let _ = fs::remove_dir_all(long_path(txd));
        return Ok(RecoveryAction::Nothing);
    }
    if st.applied {
        return match (st.target(), st.tx()) {
            (Some(target), Some(tx)) => {
                Ok(RecoveryAction::RollForward { target, tx, base: st.base(), derive_note: st.derive_note() })
            }
            _ => Err(Error::Corrupt("journal has an applied marker but no begin record".into())),
        };
    }
    crate::failpoint!("recover.before_rollback")?;
    rollback_ops(root, txd, &st.ops, st.begun_upto)?;
    crate::failpoint!("recover.after_rollback")?;
    let touched = st.begun_upto > 0;
    if let Ok(mut j) = Journal::create(&jp) {
        let _ = j.append(&Rec::RolledBack, true);
    }
    let _ = fs::remove_dir_all(long_path(txd));
    Ok(if touched { RecoveryAction::RolledBack } else { RecoveryAction::Discarded })
}

/// All transaction directories currently present under `root`.
pub fn pending_txs(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(long_path(&tx_root(root))) else { return vec![] };
    let mut v: Vec<PathBuf> = rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect();
    v.sort();
    v
}

/// Delete recovery points older than `keep_days` (never the most recent one).
pub fn prune_recovery(root: &Path, keep_days: u32) -> Result<usize> {
    let dir = recovery_root(root);
    let Ok(rd) = fs::read_dir(long_path(&dir)) else { return Ok(0) };
    let mut items: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    items.sort();
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(keep_days as u64 * 86_400);
    let mut removed = 0;
    let n = items.len();
    for (i, (t, p)) in items.into_iter().enumerate() {
        if i + 1 < n && t < cutoff && fs::remove_dir_all(long_path(&p)).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;

    fn rec_begin() -> Rec {
        Rec::Begin {
            tx: TxId::new(),
            target: SnapshotId(Hash::of(b"t")),
            base: None,
            instance: InstanceId::new(),
            created: "2026-01-01T00:00:00Z".into(),
            derive_note: None,
        }
    }

    #[test]
    fn journal_roundtrip_and_torn_tail_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("j/journal.jsonl");
        let mut j = Journal::create(&p).unwrap();
        j.append(&rec_begin(), true).unwrap();
        j.append(&Rec::Ops { ops: vec![JOp::Write { p: "a/b".into(), existed: true }] }, true).unwrap();
        j.append(&Rec::Staged, true).unwrap();
        drop(j);
        // simulate a crash mid-write of the next record
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"t\":\"batch_be").unwrap();
        drop(f);
        let recs = Journal::read_all(&p).unwrap();
        assert_eq!(recs.len(), 3);
        let st = JournalState::from_records(&recs);
        assert!(st.staged && st.begun_upto == 0 && !st.applied);
        // a damaged record in the MIDDLE is corruption, not a torn tail
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"\n{\"t\":\"staged\"}\n").unwrap();
        assert!(Journal::read_all(&p).is_err());
    }

    /// Build a live tree, a trash/stage as the apply loop would have left them, then check rollback.
    #[test]
    fn rollback_restores_every_file_at_every_possible_interruption_point() {
        // ops: delete d.txt; rmdir olddir; mkdir newdir; write w1 (existed); write w2 (new)
        let ops = vec![
            JOp::Delete { p: "d.txt".into() },
            JOp::RmDir { p: "olddir".into() },
            JOp::MkDir { p: "newdir".into() },
            JOp::Write { p: "w1.txt".into(), existed: true },
            JOp::Write { p: "newdir/w2.txt".into(), existed: false },
        ];
        // For every prefix of "micro-steps" executed, rollback must restore the original tree.
        // Micro-steps in apply order:
        // 0 trash d.txt; 1 rmdir olddir; 2 mkdir newdir; 3 trash w1; 4 place w1; 5 place w2
        for crash_after in 0..=6 {
            let d = tempfile::tempdir().unwrap();
            let root = d.path().join("inst");
            let txd = root.join(".packsync/tx/t1");
            fs::create_dir_all(root.join("olddir")).unwrap();
            fs::write(root.join("d.txt"), b"delete me").unwrap();
            fs::write(root.join("w1.txt"), b"old w1").unwrap();
            fs::write(root.join("untouched.txt"), b"keep").unwrap();
            fs::create_dir_all(txd.join("stage/newdir")).unwrap();
            fs::write(txd.join("stage/w1.txt"), b"new w1").unwrap();
            fs::write(txd.join("stage/newdir/w2.txt"), b"new w2").unwrap();
            let snapshot = |r: &Path| -> Vec<(String, Vec<u8>)> {
                let mut v: Vec<(String, Vec<u8>)> = walkdir::WalkDir::new(r)
                    .into_iter()
                    .flatten()
                    .filter(|e| !e.path().starts_with(r.join(".packsync")))
                    .map(|e| {
                        (
                            e.path().strip_prefix(r).unwrap().to_string_lossy().into_owned(),
                            if e.file_type().is_file() { fs::read(e.path()).unwrap() } else { b"<dir>".to_vec() },
                        )
                    })
                    .collect();
                v.sort();
                v
            };
            let before = snapshot(&root);

            let step = |n: usize| crash_after > n; // has micro-step n run?
            if step(0) {
                fs::create_dir_all(txd.join("trash")).unwrap();
                fs::rename(root.join("d.txt"), txd.join("trash/d.txt")).unwrap();
            }
            if step(1) {
                fs::remove_dir(root.join("olddir")).unwrap();
            }
            if step(2) {
                fs::create_dir(root.join("newdir")).unwrap();
            }
            if step(3) {
                fs::create_dir_all(txd.join("trash")).unwrap();
                fs::rename(root.join("w1.txt"), txd.join("trash/w1.txt")).unwrap();
            }
            if step(4) {
                fs::rename(txd.join("stage/w1.txt"), root.join("w1.txt")).unwrap();
            }
            if step(5) {
                fs::rename(txd.join("stage/newdir/w2.txt"), root.join("newdir/w2.txt")).unwrap();
            }
            // rollback with the whole list marked as begun (the worst case: crash inside the batch)
            rollback_ops(&root, &txd, &ops, ops.len()).unwrap();
            assert_eq!(snapshot(&root), before, "crash after micro-step {crash_after} must roll back to the original");
            // idempotent
            rollback_ops(&root, &txd, &ops, ops.len()).unwrap();
            assert_eq!(snapshot(&root), before);
        }
    }

    #[test]
    fn recover_tx_decides_between_discard_rollback_and_roll_forward() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let mk = |name: &str, recs: &[Rec]| {
            let txd = tx_root(root).join(name);
            let mut j = Journal::create(&txd.join("journal.jsonl")).unwrap();
            for r in recs {
                j.append(r, true).unwrap();
            }
            txd
        };
        let t1 = mk("a", &[rec_begin(), Rec::Ops { ops: vec![] }]);
        assert_eq!(recover_tx(root, &t1).unwrap(), RecoveryAction::Discarded);
        assert!(!t1.exists());

        let t2 = mk(
            "b",
            &[
                rec_begin(),
                Rec::Ops { ops: vec![JOp::MkDir { p: "x".into() }] },
                Rec::Staged,
                Rec::BatchBegin { upto: 1 },
            ],
        );
        fs::create_dir_all(root.join("x")).unwrap();
        assert_eq!(recover_tx(root, &t2).unwrap(), RecoveryAction::RolledBack);
        assert!(!root.join("x").exists(), "directory created by the interrupted batch is removed");

        let t3 = mk(
            "c",
            &[
                rec_begin(),
                Rec::Ops { ops: vec![] },
                Rec::Staged,
                Rec::BatchBegin { upto: 0 },
                Rec::BatchDone { upto: 0 },
                Rec::Applied,
            ],
        );
        assert!(matches!(recover_tx(root, &t3).unwrap(), RecoveryAction::RollForward { .. }));
        assert!(t3.exists(), "roll-forward keeps the tx until the caller finalises it");
    }

    #[test]
    fn finalize_moves_trash_to_the_recovery_area() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let tx = TxId::new();
        let txd = tx_dir(root, tx);
        fs::create_dir_all(txd.join("trash/sub")).unwrap();
        fs::write(txd.join("trash/sub/old.dat"), b"old").unwrap();
        fs::create_dir_all(txd.join("stage")).unwrap();
        let note = RecoveryNote {
            tx,
            created: "now".into(),
            from: None,
            to: SnapshotId(Hash::of(b"x")),
            files: 1,
            bytes: 3,
            reason: "test".into(),
        };
        finalize_tx(root, tx, &note).unwrap();
        assert!(!txd.exists());
        assert_eq!(fs::read(recovery_root(root).join(tx.to_string()).join("files/sub/old.dat")).unwrap(), b"old");
        assert!(recovery_root(root).join(tx.to_string()).join("recovery.json").exists());
        assert_eq!(prune_recovery(root, 7).unwrap(), 0, "fresh recovery points are kept");
    }
}
