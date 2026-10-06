//! Process-safe locks. One advisory OS file lock per instance serialises every mutating
//! operation (snapshot, apply, upload bookkeeping) across threads **and processes** (app, CLI,
//! launcher hooks). OS file locks are released by the kernel when a process dies, so a crashed
//! holder can never wedge an instance.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::InstanceId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub operation: String,
    pub started_ms: i64,
}

/// Held while an operation mutates an instance. Dropping it releases the lock.
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Try to take the lock without waiting; `Err(Busy)` names the holder when it can be read.
    pub fn try_acquire(dir: &Path, instance: InstanceId, operation: &str) -> Result<InstanceLock> {
        Self::try_acquire_kind(dir, instance, "", operation)
    }

    /// Like [`try_acquire`](Self::try_acquire) but for an independent lock *kind* (`"upload"`): locks of
    /// different kinds never block each other, so an operation holding the instance lock can still
    /// upload.
    pub fn try_acquire_kind(dir: &Path, instance: InstanceId, kind: &str, operation: &str) -> Result<InstanceLock> {
        fs::create_dir_all(dir).map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        let path = if kind.is_empty() {
            dir.join(format!("{instance}.lock"))
        } else {
            dir.join(format!("{instance}.{kind}.lock"))
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let holder = fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<LockInfo>(&s).ok())
                    .map(|i| format!("{} (process {})", i.operation, i.pid))
                    .unwrap_or_else(|| "another PackSync operation".to_string());
                return Err(Error::Busy(format!("this instance is busy: {holder}")));
            }
            Err(TryLockError::Error(e)) => return Err(Error::io(format!("locking {}", path.display()), e)),
        }
        let info =
            LockInfo { pid: std::process::id(), operation: operation.to_string(), started_ms: crate::db::now_ms() };
        let json = serde_json::to_vec(&info)?;
        let _ = file
            .set_len(0)
            .and_then(|_| file.seek(SeekFrom::Start(0)))
            .and_then(|_| file.write_all(&json))
            .and_then(|_| file.flush());
        Ok(InstanceLock { file, path })
    }

    /// Whether the lock is currently held by someone (without keeping it).
    pub fn is_held(dir: &Path, instance: InstanceId) -> bool {
        match Self::try_acquire(dir, instance, "probe") {
            Ok(l) => {
                drop(l);
                false
            }
            Err(Error::Busy(_)) => true,
            Err(_) => false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquirer_is_refused_with_a_holder_description() {
        let d = tempfile::tempdir().unwrap();
        let id = InstanceId::new();
        let a = InstanceLock::try_acquire(d.path(), id, "snapshot").unwrap();
        let err = InstanceLock::try_acquire(d.path(), id, "restore").err().unwrap();
        assert!(matches!(err, Error::Busy(_)));
        assert!(err.to_string().contains("snapshot"), "{err}");
        assert!(InstanceLock::is_held(d.path(), id));
        drop(a);
        assert!(!InstanceLock::is_held(d.path(), id));
        InstanceLock::try_acquire(d.path(), id, "restore").unwrap();
    }

    #[test]
    fn different_instances_do_not_block_each_other() {
        let d = tempfile::tempdir().unwrap();
        let _a = InstanceLock::try_acquire(d.path(), InstanceId::new(), "x").unwrap();
        let _b = InstanceLock::try_acquire(d.path(), InstanceId::new(), "y").unwrap();
    }

    #[test]
    fn exactly_one_of_many_racing_threads_wins() {
        let d = tempfile::tempdir().unwrap();
        let id = InstanceId::new();
        let wins = std::sync::atomic::AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    if let Ok(l) = InstanceLock::try_acquire(d.path(), id, "race") {
                        wins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        drop(l);
                    }
                });
            }
        });
        assert_eq!(wins.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
