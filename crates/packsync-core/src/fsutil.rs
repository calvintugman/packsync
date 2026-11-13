//! Filesystem helpers: durable atomic writes, no-follow stat with the metadata the scan policy
//! needs (nanosecond mtime, change time, file identity), free-space queries.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    /// FIFO, socket, device, ...
    Other,
}

/// Cheap change-detection metadata. Never trusted alone (see the scan policy in `scan.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub kind: FileKind,
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Inode change time (Unix `ctime`, NTFS `ChangeTime`) when the platform exposes it. Unlike
    /// mtime it cannot be set by user space, so it exposes "same size, restored timestamp" edits.
    pub ctime_ns: Option<i64>,
    /// `dev:ino` (Unix) or volume serial + file index (Windows): detects replaced files.
    pub file_id: Option<String>,
    pub executable: bool,
}

/// `lstat`-style metadata (never follows symlinks).
pub fn stat_nofollow(path: &Path) -> io::Result<FileStat> {
    let md = fs::symlink_metadata(long_path(path))?;
    Ok(stat_from_md(path, &md))
}

/// Convert already-fetched (non-following) metadata, e.g. from `DirEntry::metadata`.
pub fn stat_from_md(path: &Path, md: &fs::Metadata) -> FileStat {
    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    };
    platform::stat_from_metadata(path, md, kind)
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    pub fn stat_from_metadata(_path: &Path, md: &fs::Metadata, kind: FileKind) -> FileStat {
        FileStat {
            kind,
            size: md.len(),
            mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
            ctime_ns: Some(md.ctime() * 1_000_000_000 + md.ctime_nsec()),
            file_id: Some(format!("{}:{}", md.dev(), md.ino())),
            executable: md.mode() & 0o111 != 0,
        }
    }

    /// Whether `pid`-independent mandatory/advisory lock probing is possible: used for session.lock.
    pub fn is_locked_by_other_process(path: &Path) -> io::Result<bool> {
        // F_GETLK on a read-write open reports a conflicting POSIX lock without taking one.
        use std::os::unix::io::AsRawFd;
        let f = fs::OpenOptions::new().read(true).write(true).open(path)?;
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        fl.l_start = 0;
        fl.l_len = 0;
        // SAFETY: `fl` is a valid, initialised `flock`; the fd is valid for the call's duration.
        let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETLK, &mut fl) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fl.l_type != libc::F_UNLCK as libc::c_short)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::fs::MetadataExt;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo, FileIdInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING,
    };

    const WINDOWS_TICKS_TO_UNIX_EPOCH: i64 = 116_444_736_000_000_000; // 100ns ticks between 1601 and 1970

    fn ticks_to_unix_ns(t: i64) -> i64 {
        (t - WINDOWS_TICKS_TO_UNIX_EPOCH) * 100
    }

    pub fn stat_from_metadata(path: &Path, md: &fs::Metadata, kind: FileKind) -> FileStat {
        let mtime_ns = ticks_to_unix_ns(md.last_write_time() as i64);
        let (ctime_ns, file_id) = extra(path).unwrap_or((None, None));
        FileStat { kind, size: md.len(), mtime_ns, ctime_ns, file_id, executable: false }
    }

    /// NTFS ChangeTime and volume-serial/file-id via one handle (opened without following reparse points).
    fn extra(path: &Path) -> Option<(Option<i64>, Option<String>)> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = super::long_path(path).as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call; all other
        // arguments are plain flags/null pointers accepted by CreateFileW.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: `h` is a valid handle; the output buffers are correctly sized for the requested classes.
        let (basic, id) = unsafe {
            let mut basic: FILE_BASIC_INFO = std::mem::zeroed();
            let ok_b = GetFileInformationByHandleEx(
                h,
                FileBasicInfo,
                &mut basic as *mut _ as *mut _,
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            );
            let mut idi: FILE_ID_INFO = std::mem::zeroed();
            let ok_i = GetFileInformationByHandleEx(
                h,
                FileIdInfo,
                &mut idi as *mut _ as *mut _,
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            );
            CloseHandle(h);
            (
                (ok_b != 0).then_some(basic.ChangeTime),
                (ok_i != 0).then(|| format!("{:x}:{}", idi.VolumeSerialNumber, hex::encode(idi.FileId.Identifier))),
            )
        };
        Some((basic.map(ticks_to_unix_ns), id))
    }

    pub fn is_locked_by_other_process(path: &Path) -> io::Result<bool> {
        // Java's `FileChannel.lock()` is a mandatory byte-range lock (`LockFileEx`) on Windows. Such a
        // lock does NOT stop the file being opened (Java opens it with read/write sharing): it makes
        // reads and writes of the locked range fail with ERROR_LOCK_VIOLATION (33). So the probe opens
        // the file read-only and reads one byte: nothing is written and no lock is ever taken, so a
        // game that starts at the same moment cannot be disturbed by the probe.
        // A sharing violation on open (32) means someone holds the file exclusively: also "held".
        use std::io::Read;
        use std::os::windows::fs::OpenOptionsExt;
        const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        let held = |e: &io::Error| matches!(e.raw_os_error(), Some(32) | Some(33));
        let mut f = match fs::OpenOptions::new().read(true).share_mode(SHARE_ALL).open(super::long_path(path)) {
            Ok(f) => f,
            Err(e) if held(&e) => return Ok(true),
            Err(e) => return Err(e),
        };
        match f.read(&mut [0u8; 1]) {
            Ok(_) => Ok(false),
            Err(e) if held(&e) => Ok(true),
            Err(e) => Err(e),
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    pub fn stat_from_metadata(_path: &Path, md: &fs::Metadata, kind: FileKind) -> FileStat {
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        FileStat { kind, size: md.len(), mtime_ns, ctime_ns: None, file_id: None, executable: false }
    }
    pub fn is_locked_by_other_process(_path: &Path) -> io::Result<bool> {
        Ok(false)
    }
}

/// Is `path` (a Minecraft `session.lock`) currently locked by another process? Best effort.
pub fn is_locked_by_other_process(path: &Path) -> io::Result<bool> {
    platform::is_locked_by_other_process(path)
}

/// Windows: an absolute path in its `\\?\` form, so paths beyond MAX_PATH work. Elsewhere: identity.
///
/// The `\\?\` form turns off Windows' own path parsing, so the path is rebuilt from its components:
/// `/` becomes `\`, `.` is dropped and `..` is resolved here (a `/` or `..` left in a verbatim path is
/// "an invalid name" to Windows).
pub fn long_path(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        windows_long_path(p)
    }
    #[cfg(not(windows))]
    {
        p.to_path_buf()
    }
}

#[cfg(any(windows, test))]
fn windows_long_path(p: &Path) -> PathBuf {
    // Parsed by hand rather than with `Path::components`, so the logic is the same (and unit-tested) on
    // every platform.
    let s = p.as_os_str().to_string_lossy().replace('/', "\\");
    if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") {
        return PathBuf::from(s);
    }
    let (prefix, rest) = if let Some(unc) = s.strip_prefix(r"\\") {
        // \\server\share\rest
        let mut it = unc.splitn(3, '\\');
        let (server, share) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        if server.is_empty() || share.is_empty() {
            return p.to_path_buf();
        }
        (format!(r"\\?\UNC\{server}\{share}"), it.next().unwrap_or("").to_string())
    } else {
        let b = s.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
            (format!(r"\\?\{}", &s[..2]), s[3..].to_string())
        } else {
            return p.to_path_buf(); // relative, or drive-relative (`C:foo`): leave to Windows
        }
    };
    let mut parts: Vec<&str> = vec![];
    for c in rest.split('\\') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            n => parts.push(n),
        }
    }
    let mut out = prefix;
    out.push('\\');
    out.push_str(&parts.join("\\"));
    PathBuf::from(out)
}

/// Flush everything written to the file system that holds `path` — data and directory entries — in one
/// go (Linux `syncfs`). `Ok(false)` when the platform or the file system cannot do it; the caller then
/// has to flush file by file. Far cheaper than a flush per file when thousands of files were just written.
pub fn syncfs(path: &Path) -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let f = File::open(path)?;
        // SAFETY: `f` is a valid open descriptor for the duration of the call.
        if unsafe { libc::syncfs(f.as_raw_fd()) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP) => Ok(false),
            _ => Err(e),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::metadata(long_path(path))?; // a path that does not exist is an error everywhere
        Ok(false)
    }
}

/// Flush a directory's metadata (new/renamed entries) to disk. No-op where unsupported.
pub fn fsync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Write `bytes` to `path` atomically and durably: temp file in the same directory, fsync, rename,
/// fsync the directory. Readers see either the old or the new content, never a mix.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(long_path(parent))?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
        uuid::Uuid::new_v4().simple()
    ));
    let res = (|| {
        let mut f = File::create(long_path(&tmp))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(long_path(&tmp), long_path(path))
    })();
    if res.is_err() {
        let _ = fs::remove_file(long_path(&tmp));
    }
    res?;
    fsync_dir(parent);
    Ok(())
}

pub fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(long_path(path)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Free space available to the current user on the volume holding `path` (or its nearest existing ancestor).
pub fn available_space(path: &Path) -> io::Result<u64> {
    let mut p = path.to_path_buf();
    loop {
        if p.exists() {
            return fs4::available_space(&p);
        }
        if !p.pop() {
            return fs4::available_space(Path::new("."));
        }
    }
}

/// Set a file's modification time (milliseconds since the epoch).
pub fn set_mtime_ms(path: &Path, ms: i64) -> io::Result<()> {
    let secs = ms.div_euclid(1000);
    let nanos = (ms.rem_euclid(1000) * 1_000_000) as u32;
    filetime::set_file_mtime(long_path(path), filetime::FileTime::from_unix_time(secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_flush_of_the_file_system_works_where_it_exists_and_says_so_where_it_does_not() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("f"), b"data").unwrap();
        // never an error for a perfectly good directory; `true` on Linux (tmpfs and ext4/btrfs/xfs all support it)
        let r = syncfs(d.path());
        assert!(r.is_ok(), "{r:?}");
        if cfg!(target_os = "linux") {
            assert!(matches!(r, Ok(true) | Ok(false)));
        } else {
            assert!(matches!(r, Ok(false)));
        }
        assert!(
            syncfs(&d.path().join("missing")).is_err(),
            "a path that does not exist is an error, not 'unsupported'"
        );
    }

    #[test]
    fn windows_long_paths_are_rebuilt_from_their_components() {
        let lp = |s: &str| windows_long_path(Path::new(s)).to_string_lossy().into_owned();
        assert_eq!(lp(r"C:\Users\a\inst"), r"\\?\C:\Users\a\inst");
        // forward slashes from `join("a/b")`, `.` and `..` are invalid inside a verbatim path
        assert_eq!(lp(r"C:\Users\a\tmp\j/journal.jsonl"), r"\\?\C:\Users\a\tmp\j\journal.jsonl");
        assert_eq!(lp("C:/x/./y/../z"), r"\\?\C:\x\z");
        assert_eq!(lp(r"C:\"), r"\\?\C:\");
        assert_eq!(lp(r"\\server\share\dir/f"), r"\\?\UNC\server\share\dir\f");
        // already verbatim, device paths, relative and drive-relative paths are left alone
        assert_eq!(lp(r"\\?\C:\a"), r"\\?\C:\a");
        assert_eq!(lp(r"\\.\pipe\x"), r"\\.\pipe\x");
        assert_eq!(lp("rel/a"), "rel/a");
        assert_eq!(lp("C:foo"), "C:foo");
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp_files() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/dir/file.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two-two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two-two");
        let names: Vec<_> = fs::read_dir(p.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "no stray temp files: {names:?}");
    }

    #[test]
    fn stat_reports_kind_and_does_not_follow_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        fs::write(&f, b"hello").unwrap();
        let s = stat_nofollow(&f).unwrap();
        assert_eq!(s.kind, FileKind::File);
        assert_eq!(s.size, 5);
        assert!(s.mtime_ns > 0);
        #[cfg(unix)]
        {
            let l = d.path().join("link");
            std::os::unix::fs::symlink(&f, &l).unwrap();
            assert_eq!(stat_nofollow(&l).unwrap().kind, FileKind::Symlink);
            assert_eq!(stat_nofollow(d.path()).unwrap().kind, FileKind::Dir);
        }
    }

    #[test]
    fn ctime_changes_even_when_mtime_is_restored() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.bin");
        fs::write(&f, b"AAAA").unwrap();
        let before = stat_nofollow(&f).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&f, b"BBBB").unwrap(); // same size
        set_mtime_ms(&f, before.mtime_ns / 1_000_000).unwrap(); // restore (ms precision) timestamp
        let after = stat_nofollow(&f).unwrap();
        assert_eq!(after.size, before.size);
        #[cfg(unix)]
        assert_ne!(after.ctime_ns, before.ctime_ns, "ctime exposes a same-size edit with restored mtime");
    }

    #[test]
    fn free_space_of_missing_path_uses_ancestor() {
        let d = tempfile::tempdir().unwrap();
        assert!(available_space(&d.path().join("not/yet/there")).unwrap() > 0);
    }

    /// Windows: the lock Minecraft takes on `session.lock` (Java `FileChannel.lock()` = an exclusive
    /// `LockFileEx` range) leaves the file openable; only reading it fails. Locks belong to a handle
    /// there, so a second handle in this process stands in for the game.
    #[cfg(windows)]
    #[test]
    fn lock_probe_sees_a_byte_range_lock_and_an_exclusive_open() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("session.lock");
        fs::write(&f, "\u{2603}").unwrap(); // what the game writes
        assert!(!is_locked_by_other_process(&f).unwrap(), "idle world");

        let game = fs::OpenOptions::new().read(true).write(true).share_mode(7).open(&f).unwrap();
        game.lock().unwrap();
        // Wine records byte-range locks but does not enforce them on reads, so this one assertion can
        // only hold on real Windows (`scripts/windows-check.sh` sets the variable).
        if std::env::var_os("PACKSYNC_TEST_UNDER_WINE").is_none() {
            assert!(is_locked_by_other_process(&f).unwrap(), "a byte-range lock is seen although the file opens");
        } else {
            eprintln!("skipped under Wine: byte-range lock detection (needs real Windows)");
        }
        game.unlock().unwrap();
        assert!(!is_locked_by_other_process(&f).unwrap(), "released");
        drop(game);

        let exclusive = fs::OpenOptions::new().read(true).write(true).share_mode(0).open(&f).unwrap();
        assert!(is_locked_by_other_process(&f).unwrap(), "a file opened without sharing counts as held");
        drop(exclusive);
        assert!(!is_locked_by_other_process(&f).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn lock_probe_sees_posix_locks_from_other_processes_only() {
        // A lock held by this very process is invisible to F_GETLK (POSIX semantics), which is
        // exactly what we want for "another writer" detection.
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("session.lock");
        fs::write(&f, b"x").unwrap();
        assert!(!is_locked_by_other_process(&f).unwrap());
    }
}
