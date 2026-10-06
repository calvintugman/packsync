//! The machine environment as far as Prism discovery cares. Everything goes through [`PrismEnv`]
//! so tests (and the smoke test) never depend on, or touch, the real user profile.

use std::path::{Path, PathBuf};

/// Prism's data directory environment variable (`<LAUNCHER_NAME upper>_DATA_DIR`, read by
/// `Application.cpp` when `--dir` is absent).
pub const DATA_DIR_ENV: &str = "PRISMLAUNCHER_DATA_DIR";
/// Flatpak application id; its data lives under `~/.var/app/<id>/data`.
pub const FLATPAK_APP_ID: &str = "org.prismlauncher.PrismLauncher";
/// Folder name Prism uses below the platform data root.
pub const DATA_DIR_NAME: &str = "PrismLauncher";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
    MacOs,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Clone)]
pub struct PrismEnv {
    pub os: Os,
    pub home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    /// Windows `%APPDATA%`.
    pub appdata: Option<PathBuf>,
    /// Windows `%LOCALAPPDATA%`.
    pub localappdata: Option<PathBuf>,
    /// `PRISMLAUNCHER_DATA_DIR`.
    pub data_dir_env: Option<PathBuf>,
    /// Entries of `PATH` (used to find the `prismlauncher` executable).
    pub path_dirs: Vec<PathBuf>,
}

fn env_path(name: &str) -> Option<PathBuf> {
    let v = std::env::var_os(name)?;
    if v.is_empty() { None } else { Some(PathBuf::from(v)) }
}

impl PrismEnv {
    pub fn from_process() -> PrismEnv {
        let home = env_path("HOME").or_else(|| env_path("USERPROFILE"));
        PrismEnv {
            os: Os::current(),
            home,
            xdg_data_home: env_path("XDG_DATA_HOME"),
            appdata: env_path("APPDATA"),
            localappdata: env_path("LOCALAPPDATA"),
            data_dir_env: env_path(DATA_DIR_ENV),
            path_dirs: std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default(),
        }
    }

    /// An environment with nothing set (tests build on this).
    pub fn empty(os: Os) -> PrismEnv {
        PrismEnv {
            os,
            home: None,
            xdg_data_home: None,
            appdata: None,
            localappdata: None,
            data_dir_env: None,
            path_dirs: vec![],
        }
    }

    /// Where Prism keeps its data when neither `--dir`, the environment variable nor portable mode
    /// applies (`QStandardPaths::AppDataLocation/..`).
    pub fn default_data_dir(&self) -> Option<PathBuf> {
        match self.os {
            Os::Linux => {
                let base = match &self.xdg_data_home {
                    Some(x) if x.is_absolute() => x.clone(),
                    _ => self.home.as_ref()?.join(".local").join("share"),
                };
                Some(base.join(DATA_DIR_NAME))
            }
            Os::Windows => Some(self.appdata.clone()?.join(DATA_DIR_NAME)),
            Os::MacOs => Some(self.home.as_ref()?.join("Library").join("Application Support").join(DATA_DIR_NAME)),
        }
    }

    /// The Flatpak data directory (Linux only).
    pub fn flatpak_data_dir(&self) -> Option<PathBuf> {
        if self.os != Os::Linux {
            return None;
        }
        Some(self.home.as_ref()?.join(".var").join("app").join(FLATPAK_APP_ID).join("data").join(DATA_DIR_NAME))
    }

    /// All data directories a Prism on this machine may be using, most specific first, whether or
    /// not they exist. `executable` enables portable-mode detection (`UserData/` or `portable.txt`
    /// next to the executable, not supported by Prism on macOS).
    pub fn data_dir_candidates(&self, executable: Option<&Path>) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut push = |p: PathBuf| {
            if !out.contains(&p) {
                out.push(p);
            }
        };
        if let Some(d) = &self.data_dir_env {
            push(d.clone());
        }
        if let Some(exe) = executable
            && let Some(p) = portable_data_dir(self.os, exe)
        {
            push(p);
        }
        if let Some(d) = self.default_data_dir() {
            push(d);
        }
        if let Some(d) = self.flatpak_data_dir() {
            push(d);
        }
        out
    }
}

/// Portable mode as implemented in `Application.cpp`: "root path" is `<exe dir>/..` on Linux and the
/// executable's directory on Windows; a `UserData` folder there, or a `portable.txt` file, makes the
/// root itself the data directory. Not supported on macOS.
pub fn portable_data_dir(os: Os, executable: &Path) -> Option<PathBuf> {
    let exe_dir = executable.parent()?;
    let root = match os {
        Os::Linux => exe_dir.parent()?.to_path_buf(),
        Os::Windows => exe_dir.to_path_buf(),
        Os::MacOs => return None,
    };
    let user_data = root.join("UserData");
    if user_data.is_dir() {
        return Some(user_data);
    }
    if root.join("portable.txt").is_file() {
        return Some(root);
    }
    None
}

/// Does `a` name the same directory as `b`? Canonicalises when possible, otherwise compares the
/// paths textually after normalising separators (and case on Windows).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (dunce::canonicalize(a), dunce::canonicalize(b)) {
        return ca == cb;
    }
    norm(a) == norm(b)
}

fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(os: Os) -> PrismEnv {
        let mut e = PrismEnv::empty(os);
        e.home = Some(PathBuf::from(if os == Os::Windows { r"C:\Users\Sam" } else { "/home/sam" }));
        e
    }

    #[test]
    fn linux_defaults_flatpak_and_env_override() {
        let mut e = env(Os::Linux);
        assert_eq!(
            e.data_dir_candidates(None),
            vec![
                PathBuf::from("/home/sam/.local/share/PrismLauncher"),
                PathBuf::from("/home/sam/.var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher"),
            ]
        );
        e.xdg_data_home = Some(PathBuf::from("/data/xdg"));
        e.data_dir_env = Some(PathBuf::from("/opt/prism-data"));
        assert_eq!(
            e.data_dir_candidates(None),
            vec![
                PathBuf::from("/opt/prism-data"),
                PathBuf::from("/data/xdg/PrismLauncher"),
                PathBuf::from("/home/sam/.var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher"),
            ]
        );
        e.xdg_data_home = Some(PathBuf::from("relative/is/ignored"));
        assert_eq!(e.default_data_dir().unwrap(), PathBuf::from("/home/sam/.local/share/PrismLauncher"));
    }

    #[test]
    fn windows_and_macos_defaults() {
        let mut w = env(Os::Windows);
        w.appdata = Some(PathBuf::from(r"C:\Users\Sam\AppData\Roaming"));
        assert_eq!(
            w.data_dir_candidates(None),
            vec![PathBuf::from(r"C:\Users\Sam\AppData\Roaming").join("PrismLauncher")]
        );
        assert!(w.flatpak_data_dir().is_none());
        let m = env(Os::MacOs);
        assert_eq!(
            m.data_dir_candidates(None),
            vec![PathBuf::from("/home/sam/Library/Application Support/PrismLauncher")]
        );
        assert!(PrismEnv::empty(Os::Linux).data_dir_candidates(None).is_empty());
    }

    #[test]
    fn portable_mode_follows_prisms_root_path_rules() {
        let d = tempfile::tempdir().unwrap();
        // Linux layout: <root>/bin/prismlauncher, data in <root>/UserData
        let root = d.path().join("prism portable ünï");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let exe = root.join("bin").join("prismlauncher");
        assert_eq!(portable_data_dir(Os::Linux, &exe), None);
        std::fs::write(root.join("portable.txt"), b"").unwrap();
        assert_eq!(portable_data_dir(Os::Linux, &exe), Some(root.clone()));
        std::fs::create_dir_all(root.join("UserData")).unwrap();
        assert_eq!(portable_data_dir(Os::Linux, &exe), Some(root.join("UserData")), "UserData wins over portable.txt");
        // Windows layout: <root>/prismlauncher.exe
        let wexe = root.join("prismlauncher.exe");
        assert_eq!(portable_data_dir(Os::Windows, &wexe), Some(root.join("UserData")));
        assert_eq!(portable_data_dir(Os::MacOs, &wexe), None);
        let mut e = env(Os::Linux);
        e.home = Some(d.path().join("home"));
        let c = e.data_dir_candidates(Some(&exe));
        assert_eq!(c[0], root.join("UserData"));
    }

    #[test]
    fn same_dir_handles_symlinks_and_trailing_slashes() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        assert!(same_dir(&a, &d.path().join("a/")));
        assert!(same_dir(&a, &d.path().join("a/../a")));
        assert!(!same_dir(&a, d.path()));
        assert!(same_dir(Path::new("/nonexistent/x/"), Path::new("/nonexistent/x")));
        #[cfg(unix)]
        {
            let l = d.path().join("link");
            std::os::unix::fs::symlink(&a, &l).unwrap();
            assert!(same_dir(&a, &l));
        }
    }
}
