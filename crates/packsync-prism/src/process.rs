//! Process inspection: is a Prism Launcher running, and which data directory does it use?
//!
//! Needed because (1) Prism rewrites `instance.cfg` from memory, so hooks may only be installed
//! while it is closed, and (2) a second `prismlauncher --launch` is *forwarded* to a running Prism
//! with the same data directory and exits immediately.
//!
//! Only the minimum is retained from other processes: name, executable, command line, working
//! directory and the value of `PRISMLAUNCHER_DATA_DIR` for processes that look like Prism.

use std::path::{Path, PathBuf};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

use crate::env::{DATA_DIR_ENV, Os, PrismEnv, portable_data_dir, same_dir};

#[derive(Debug, Clone, Default)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Value of `PRISMLAUNCHER_DATA_DIR` in the process environment (only collected for Prism).
    pub data_dir_env: Option<PathBuf>,
}

/// Source of process information; tests substitute a fixed list.
pub trait ProcessSource: Send + Sync {
    fn prism_processes(&self) -> Vec<ProcInfo>;
}

/// Does this executable/process name look like Prism Launcher?
pub fn looks_like_prism(name: &str, exe: Option<&Path>, cmd0: Option<&str>) -> bool {
    fn stem_matches(s: &str) -> bool {
        let base = s.rsplit(['/', '\\']).next().unwrap_or(s).to_ascii_lowercase();
        let base = base.strip_suffix(".exe").unwrap_or(&base);
        base == "prismlauncher" || base == "prism launcher"
    }
    stem_matches(name)
        || exe.and_then(|e| e.file_name()).and_then(|n| n.to_str()).map(stem_matches).unwrap_or(false)
        || cmd0.map(stem_matches).unwrap_or(false)
}

/// The real thing.
pub struct SystemProcesses;

impl ProcessSource for SystemProcesses {
    fn prism_processes(&self) -> Vec<ProcInfo> {
        let me = std::process::id();
        let sys = System::new_with_specifics(
            RefreshKind::nothing().with_processes(
                ProcessRefreshKind::nothing()
                    .with_cmd(UpdateKind::Always)
                    .with_cwd(UpdateKind::Always)
                    .with_exe(UpdateKind::OnlyIfNotSet),
            ),
        );
        let mut found: Vec<ProcInfo> = Vec::new();
        for (pid, p) in sys.processes() {
            if pid.as_u32() == me {
                continue;
            }
            let name = p.name().to_string_lossy().into_owned();
            let cmd: Vec<String> = p.cmd().iter().map(|s| s.to_string_lossy().into_owned()).collect();
            if !looks_like_prism(&name, p.exe(), cmd.first().map(|s| s.as_str())) {
                continue;
            }
            found.push(ProcInfo {
                pid: pid.as_u32(),
                name,
                exe: p.exe().map(|e| e.to_path_buf()),
                cmd,
                cwd: p.cwd().map(|c| c.to_path_buf()),
                data_dir_env: None,
            });
        }
        if found.is_empty() {
            return found;
        }
        // Second pass: the environment of the Prism processes only.
        let pids: Vec<sysinfo::Pid> = found.iter().map(|p| sysinfo::Pid::from_u32(p.pid)).collect();
        let mut sys2 = System::new();
        sys2.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
        );
        let prefix = format!("{DATA_DIR_ENV}=");
        for f in &mut found {
            if let Some(p) = sys2.process(sysinfo::Pid::from_u32(f.pid)) {
                f.data_dir_env = p
                    .environ()
                    .iter()
                    .filter_map(|kv| kv.to_str())
                    .find_map(|kv| kv.strip_prefix(prefix.as_str()))
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from);
            }
        }
        found
    }
}

/// The `--dir` value of a Prism command line (`-d DIR`, `-dDIR`, `--dir DIR`, `--dir=DIR`), made
/// absolute against the process's working directory.
pub fn dir_argument(cmd: &[String], cwd: Option<&Path>) -> Option<PathBuf> {
    let mut value: Option<String> = None;
    let mut i = 1;
    while i < cmd.len() {
        let a = &cmd[i];
        if a == "--dir" || a == "-d" {
            value = cmd.get(i + 1).cloned();
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--dir=") {
            value = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("-d")
            && !v.is_empty()
            && !a.starts_with("--")
        {
            value = Some(v.to_string());
        }
        i += 1;
    }
    let v = PathBuf::from(value?);
    Some(if v.is_absolute() { v } else { cwd?.join(v) })
}

/// The data directory a running Prism uses (the order of `Application.cpp`: `--dir`, environment
/// variable, portable mode, platform default).
pub fn process_data_dir(p: &ProcInfo, env: &PrismEnv) -> Option<PathBuf> {
    if let Some(d) = dir_argument(&p.cmd, p.cwd.as_deref()) {
        return Some(d);
    }
    if let Some(d) = &p.data_dir_env {
        return Some(d.clone());
    }
    if let Some(exe) = &p.exe
        && let Some(d) = portable_data_dir(env.os, exe)
    {
        return Some(d);
    }
    // A Flatpak Prism runs from /app inside its sandbox; its data lives in the Flatpak directory.
    if env.os == Os::Linux
        && p.cmd.first().map(|c| c.starts_with("/app/")).unwrap_or(false)
        && let Some(d) = env.flatpak_data_dir()
    {
        return Some(d);
    }
    env.default_data_dir()
}

/// Prism processes using `data_dir` (or any Prism when `data_dir` is `None`).
pub fn processes_for(source: &dyn ProcessSource, env: &PrismEnv, data_dir: Option<&Path>) -> Vec<ProcInfo> {
    source
        .prism_processes()
        .into_iter()
        .filter(|p| match data_dir {
            None => true,
            Some(want) => match process_data_dir(p, env) {
                Some(have) => same_dir(&have, want),
                // Cannot tell: be conservative.
                None => true,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(cmd: &[&str]) -> ProcInfo {
        ProcInfo {
            pid: 1,
            name: "prismlauncher".into(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            cwd: Some(PathBuf::from("/work")),
            ..ProcInfo::default()
        }
    }

    #[test]
    fn recognises_prism_executables_on_every_platform() {
        assert!(looks_like_prism("prismlauncher", None, None));
        assert!(looks_like_prism("PrismLauncher.exe", None, None));
        assert!(looks_like_prism("x", Some(Path::new("/usr/bin/prismlauncher")), None));
        assert!(looks_like_prism("x", None, Some("/tmp/.mount_PrismL1/usr/bin/prismlauncher")));
        assert!(looks_like_prism("prismlauncher", Some(Path::new(r"C:\Prism\prismlauncher.exe")), None));
        assert!(!looks_like_prism("java", Some(Path::new("/usr/bin/java")), Some("java")));
        assert!(!looks_like_prism("prismlauncher-helper", None, None));
        assert!(!looks_like_prism("packsync", Some(Path::new("/opt/packsync")), None));
    }

    #[test]
    fn parses_every_spelling_of_the_dir_option() {
        let cwd = Some(Path::new("/work"));
        assert_eq!(dir_argument(&p(&["prismlauncher", "--dir", "/d/x y"]).cmd, cwd), Some(PathBuf::from("/d/x y")));
        assert_eq!(dir_argument(&p(&["prismlauncher", "-d", "rel/dir"]).cmd, cwd), Some(PathBuf::from("/work/rel/dir")));
        assert_eq!(dir_argument(&p(&["prismlauncher", "--dir=/a b"]).cmd, cwd), Some(PathBuf::from("/a b")));
        assert_eq!(dir_argument(&p(&["prismlauncher", "-d/glued"]).cmd, cwd), Some(PathBuf::from("/glued")));
        assert_eq!(dir_argument(&p(&["prismlauncher", "--launch", "x"]).cmd, cwd), None);
        assert_eq!(dir_argument(&p(&["prismlauncher", "--dir"]).cmd, cwd), None);
        assert_eq!(dir_argument(&p(&["prismlauncher", "-d", "rel"]).cmd, None), None, "relative without a cwd is unknowable");
    }

    #[test]
    fn data_dir_resolution_order() {
        let mut env = PrismEnv::empty(Os::Linux);
        env.home = Some(PathBuf::from("/home/u"));
        // --dir wins over the environment variable
        let mut a = p(&["prismlauncher", "--dir", "/by/arg"]);
        a.data_dir_env = Some(PathBuf::from("/by/env"));
        assert_eq!(process_data_dir(&a, &env), Some(PathBuf::from("/by/arg")));
        // then the process environment
        let mut b = p(&["prismlauncher"]);
        b.data_dir_env = Some(PathBuf::from("/by/env"));
        assert_eq!(process_data_dir(&b, &env), Some(PathBuf::from("/by/env")));
        // then the platform default
        assert_eq!(
            process_data_dir(&p(&["prismlauncher"]), &env),
            Some(PathBuf::from("/home/u/.local/share/PrismLauncher"))
        );
        // Flatpak
        let f = p(&["/app/bin/prismlauncher"]);
        assert_eq!(
            process_data_dir(&f, &env),
            Some(PathBuf::from("/home/u/.var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher"))
        );
    }

    struct Fake(Vec<ProcInfo>);
    impl ProcessSource for Fake {
        fn prism_processes(&self) -> Vec<ProcInfo> {
            self.0.clone()
        }
    }

    #[test]
    fn filters_by_data_dir() {
        let mut env = PrismEnv::empty(Os::Linux);
        env.home = Some(PathBuf::from("/home/u"));
        let src = Fake(vec![p(&["prismlauncher", "--dir", "/data/one"]), p(&["prismlauncher", "--dir", "/data/two"])]);
        assert_eq!(processes_for(&src, &env, Some(Path::new("/data/one"))).len(), 1);
        assert_eq!(processes_for(&src, &env, Some(Path::new("/data/three"))).len(), 0);
        assert_eq!(processes_for(&src, &env, None).len(), 2);
        // the default data dir is matched by processes that pass no --dir
        let src = Fake(vec![p(&["prismlauncher"])]);
        assert_eq!(processes_for(&src, &env, Some(Path::new("/home/u/.local/share/PrismLauncher"))).len(), 1);
    }

    #[test]
    fn the_real_process_table_scan_does_not_panic() {
        let _ = SystemProcesses.prism_processes();
    }
}
