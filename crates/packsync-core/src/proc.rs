//! Detecting other writers of an instance (a running game, or a process that looks like one).
//!
//! The launcher process is *not* the game: Prism may forward a launch to an already-running
//! launcher and exit at once, and the game is a separate `java` child. So we look at the process
//! table: a Java-like process whose working directory is inside the instance, or whose command
//! line mentions the instance root (or an adapter-supplied pattern such as
//! `-Djava.library.path=<root>/natives`), is a writer. A second, independent signal is the
//! `session.lock` of each world (see [`world_locks_held`]). Neither is a proof of absence; the
//! snapshot's stable-read check is the third line of defence.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, RefreshKind, System, UpdateKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Writer {
    pub pid: u32,
    pub name: String,
    pub reason: String,
}

fn looks_like_game(name: &str, cmd: &[String]) -> bool {
    let n = name.to_ascii_lowercase();
    if n.contains("java") || n.contains("minecraft") {
        return true;
    }
    // wrappers (gamemoderun, mangohud, flatpak/steam runtimes) hide the java binary's name
    cmd.iter().any(|a| {
        let a = a.to_ascii_lowercase();
        a.contains("net.minecraft")
            || a.contains("org.prismlauncher.entrypoint")
            || a.contains("cpw.mods.bootstraplauncher")
    })
}

fn norm(p: &str) -> String {
    let s = p.replace('\\', "/");
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// The process table. Threads are left out: on Linux each one is listed as a process of its own (a
/// running game has hundreds), and reading their command lines and folders was most of the cost.
/// Always read fresh: a game that started a moment ago must be seen.
fn process_table() -> System {
    System::new_with_specifics(
        RefreshKind::nothing().with_processes(
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cmd(UpdateKind::Always)
                .with_cwd(UpdateKind::Always)
                .with_exe(UpdateKind::OnlyIfNotSet),
        ),
    )
}

/// Processes (other than `ignore_pids` and ourselves) that appear to be using `root`.
pub fn find_writers(root: &Path, extra_patterns: &[String], ignore_pids: &[u32]) -> Vec<Writer> {
    writers_in(&process_table(), root, extra_patterns, ignore_pids)
}

fn writers_in(sys: &System, root: &Path, extra_patterns: &[String], ignore_pids: &[u32]) -> Vec<Writer> {
    let me = std::process::id();
    let mut roots = vec![norm(&root.to_string_lossy())];
    if let Ok(c) = dunce::canonicalize(root) {
        let c = norm(&c.to_string_lossy());
        if !roots.contains(&c) {
            roots.push(c);
        }
    }
    let patterns: Vec<String> = extra_patterns.iter().map(|p| norm(p)).collect();
    let mut out = vec![];
    for (pid, p) in sys.processes() {
        let pid = pid.as_u32();
        // On Linux every thread of a JVM is listed as a process of its own; only the process counts.
        if pid == me || ignore_pids.contains(&pid) || p.thread_kind().is_some() {
            continue;
        }
        let name = p.name().to_string_lossy().into_owned();
        let cmd: Vec<String> = p.cmd().iter().map(|s| s.to_string_lossy().into_owned()).collect();
        if !looks_like_game(&name, &cmd) {
            continue;
        }
        let mut reason = None;
        if let Some(cwd) = p.cwd() {
            let c = norm(&cwd.to_string_lossy());
            if roots.iter().any(|r| c == *r || c.starts_with(&format!("{r}/"))) {
                reason = Some(format!("working directory is inside the instance ({})", cwd.display()));
            }
        }
        if reason.is_none() {
            let joined = norm(&cmd.join(" "));
            if let Some(r) = roots.iter().find(|r| joined.contains(r.as_str())) {
                reason = Some(format!("command line mentions the instance ({r})"));
            } else if let Some(pat) = patterns.iter().find(|pat| joined.contains(pat.as_str())) {
                reason = Some(format!("command line matches launcher pattern ({pat})"));
            }
        }
        if let Some(reason) = reason {
            out.push(Writer { pid, name, reason });
        }
    }
    out.sort_by_key(|w| w.pid);
    out
}

/// Is a process with this pid alive and still looking like the same game (same start time)?
pub fn pid_alive(pid: u32, started_ms: Option<i64>) -> bool {
    let sys = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing().without_tasks()),
    );
    let Some(p) = sys.process(sysinfo::Pid::from_u32(pid)) else { return false };
    match started_ms {
        // Guard against pid reuse: a different process started long after the recorded start.
        Some(ms) => (p.start_time() as i64 * 1000 - ms).abs() < 120_000,
        None => true,
    }
}

/// When a process started (milliseconds since the epoch), if it is running.
pub fn process_start_ms(pid: u32) -> Option<i64> {
    let sys = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing().without_tasks()),
    );
    sys.process(sysinfo::Pid::from_u32(pid)).map(|p| p.start_time() as i64 * 1000)
}

/// World folders whose `session.lock` is locked by another process (Minecraft holds it while a
/// world is open). Missing locks and probe errors count as "not held".
pub fn world_locks_held(game_root: &Path) -> Vec<PathBuf> {
    let mut held = vec![];
    let saves = game_root.join("saves");
    let Ok(rd) = std::fs::read_dir(crate::fsutil::long_path(&saves)) else { return held };
    for e in rd.flatten() {
        let lock = e.path().join("session.lock");
        if lock.is_file() && crate::fsutil::is_locked_by_other_process(&lock).unwrap_or(false) {
            held.push(saves.join(e.file_name())); // the caller's spelling, not the `\\?\` form
        }
    }
    held
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_report_ordinary_processes_or_ourselves() {
        let d = tempfile::tempdir().unwrap();
        // The test binary's cwd is the crate dir, not inside `d`; nothing should match.
        assert!(find_writers(d.path(), &[], &[]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detects_a_java_like_process_by_cwd_and_by_cmdline() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // a fake "java" (named so) that just sleeps
        let fake = bin.join("java");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap(); // no `exec`: the process must keep the name "java"
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let game_dir = d.path().join("inst/.minecraft");
        std::fs::create_dir_all(&game_dir).unwrap();

        let mut child = std::process::Command::new(&fake).current_dir(&game_dir).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let w = find_writers(&d.path().join("inst"), &[], &[]);
        assert!(w.iter().any(|w| w.pid == child.id()), "cwd match: {w:?}");
        assert!(find_writers(&d.path().join("inst"), &[], &[child.id()]).is_empty(), "ignored pid");
        assert!(find_writers(&d.path().join("elsewhere"), &[], &[]).is_empty());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(find_writers(&d.path().join("inst"), &[], &[]).iter().all(|w| w.pid != child.id()));
    }

    #[test]
    fn lock_probe_finds_nothing_in_idle_worlds() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path().join("saves/W");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(w.join("session.lock"), b"x").unwrap();
        assert!(world_locks_held(d.path()).is_empty());
    }
}
