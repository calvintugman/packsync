//! Path safety. Remote manifests are untrusted input: every path is validated before it is ever
//! joined to a local directory. This module also detects names that cannot be materialised on
//! another platform (Windows reserved names, illegal characters, case/Unicode-normalisation
//! collisions) so incompatibilities are found *before* a working instance is touched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result};

pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_COMPONENT_BYTES: usize = 255;
pub const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathIssue {
    Empty,
    Absolute,
    EmptyComponent,
    DotComponent,
    ParentComponent,
    Backslash,
    Control,
    Nul,
    ComponentTooLong,
    TooLong,
    TooDeep,
    /// Windows drive prefix like `C:` in the first component.
    DrivePrefix,
}

impl std::fmt::Display for PathIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PathIssue::Empty => "empty path",
            PathIssue::Absolute => "absolute path",
            PathIssue::EmptyComponent => "empty path component (// or trailing /)",
            PathIssue::DotComponent => "'.' component",
            PathIssue::ParentComponent => "'..' component",
            PathIssue::Backslash => "backslash in name",
            PathIssue::Control => "control character in name",
            PathIssue::Nul => "NUL byte in name",
            PathIssue::ComponentTooLong => "path component longer than 255 bytes",
            PathIssue::TooLong => "path longer than 1024 bytes",
            PathIssue::TooDeep => "path nested deeper than 64 levels",
            PathIssue::DrivePrefix => "Windows drive prefix",
        };
        f.write_str(s)
    }
}

/// Validate a snapshot-relative path: `/`-separated, relative, normalised, bounded. This is the
/// single gate for all paths coming from manifests.
pub fn validate_relpath(p: &str) -> std::result::Result<(), PathIssue> {
    if p.is_empty() {
        return Err(PathIssue::Empty);
    }
    if p.len() > MAX_PATH_BYTES {
        return Err(PathIssue::TooLong);
    }
    if p.starts_with('/') {
        return Err(PathIssue::Absolute);
    }
    if p.contains('\\') {
        return Err(PathIssue::Backslash);
    }
    if p.contains('\0') {
        return Err(PathIssue::Nul);
    }
    let mut depth = 0;
    for (i, c) in p.split('/').enumerate() {
        depth += 1;
        if c.is_empty() {
            return Err(PathIssue::EmptyComponent);
        }
        if c == "." {
            return Err(PathIssue::DotComponent);
        }
        if c == ".." {
            return Err(PathIssue::ParentComponent);
        }
        if c.len() > MAX_COMPONENT_BYTES {
            return Err(PathIssue::ComponentTooLong);
        }
        if c.chars().any(|ch| ch.is_control()) {
            return Err(PathIssue::Control);
        }
        if i == 0 && c.len() >= 2 && c.as_bytes()[1] == b':' && c.as_bytes()[0].is_ascii_alphabetic() {
            return Err(PathIssue::DrivePrefix);
        }
    }
    if depth > MAX_DEPTH {
        return Err(PathIssue::TooDeep);
    }
    Ok(())
}

/// Convert a validated snapshot path into a local path under `root`.
pub fn join_safe(root: &Path, rel: &str) -> Result<PathBuf> {
    validate_relpath(rel).map_err(|e| Error::invalid("path", format!("{rel:?}: {e}")))?;
    let mut p = root.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    Ok(p)
}

/// Convert a local path under `root` into a snapshot path; `None` if it is outside `root` or not
/// representable (non-UTF-8, invalid component).
pub fn to_relpath(root: &Path, p: &Path) -> Option<String> {
    let rel = p.strip_prefix(root).ok()?;
    let mut out = String::new();
    for (i, c) in rel.components().enumerate() {
        let s = match c {
            std::path::Component::Normal(s) => s.to_str()?,
            _ => return None,
        };
        if i > 0 {
            out.push('/');
        }
        out.push_str(s);
    }
    validate_relpath(&out).ok()?;
    Some(out)
}

/// Refuse to write through a symlinked ancestor: every existing directory between `root` and the
/// target of `rel` must be a real directory. (Manifests cannot contain symlinks; this protects
/// against symlinks that already exist locally, e.g. a user's linked `saves` folder.)
pub fn ensure_no_symlink_ancestors(root: &Path, rel: &str) -> Result<()> {
    let mut cur = root.to_path_buf();
    // root itself may legitimately be reached through symlinks (e.g. /home -> /mnt/home); only
    // components *below* root are checked.
    let comps: Vec<&str> = rel.split('/').collect();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        match std::fs::symlink_metadata(crate::fsutil::long_path(&cur)) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(Error::Unsupported(format!(
                    "{} is a symbolic link; PackSync never follows or replaces links inside an instance",
                    cur.display()
                )));
            }
            Ok(md) if i + 1 < comps.len() && !md.is_dir() => {
                return Err(Error::invalid("path", format!("{} exists and is not a directory", cur.display())));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // nothing deeper exists
            Err(e) => return Err(Error::io(format!("checking {}", cur.display()), e)),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortabilityIssue {
    /// `CON`, `NUL`, `COM1`, ... (with or without extension) cannot be created on Windows.
    WindowsReservedName(String),
    WindowsIllegalChar(char),
    WindowsTrailingDotOrSpace,
    /// Over 255 UTF-16 code units.
    WindowsComponentTooLong,
}

impl std::fmt::Display for PortabilityIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortabilityIssue::WindowsReservedName(n) => write!(f, "'{n}' is a reserved device name on Windows"),
            PortabilityIssue::WindowsIllegalChar(c) => write!(f, "'{c}' is not allowed in Windows file names"),
            PortabilityIssue::WindowsTrailingDotOrSpace => write!(f, "names cannot end with a dot or space on Windows"),
            PortabilityIssue::WindowsComponentTooLong => write!(f, "name longer than 255 UTF-16 units"),
        }
    }
}

fn is_windows_reserved(stem: &str) -> bool {
    let s = stem.to_ascii_uppercase();
    matches!(s.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (s.len() == 4 && (s.starts_with("COM") || s.starts_with("LPT")) && s.as_bytes()[3].is_ascii_digit())
}

/// Problems that would prevent materialising `path` on Windows (checked on every platform so a
/// Linux snapshot can warn about it and a Windows restore can refuse before touching anything).
pub fn windows_issues(path: &str) -> Vec<PortabilityIssue> {
    let mut out = vec![];
    for comp in path.split('/') {
        let stem = comp.split('.').next().unwrap_or(comp).trim_end();
        if is_windows_reserved(stem) {
            out.push(PortabilityIssue::WindowsReservedName(comp.to_string()));
        }
        for ch in comp.chars() {
            if matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (ch as u32) < 0x20 {
                out.push(PortabilityIssue::WindowsIllegalChar(ch));
                break;
            }
        }
        if comp.ends_with('.') || comp.ends_with(' ') {
            out.push(PortabilityIssue::WindowsTrailingDotOrSpace);
        }
        if comp.encode_utf16().count() > 255 {
            out.push(PortabilityIssue::WindowsComponentTooLong);
        }
    }
    out
}

/// How a target filesystem compares names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldMode {
    /// Linux ext4/btrfs/xfs: names are exact byte strings.
    Sensitive,
    /// Windows NTFS, default macOS: case-insensitive.
    CaseInsensitive,
    /// macOS APFS/HFS+: case-insensitive and Unicode-normalisation-insensitive.
    CaseAndNormalizationInsensitive,
}

impl FoldMode {
    /// The strictest mode: names that survive this are safe on every supported platform.
    pub const PORTABLE: FoldMode = FoldMode::CaseAndNormalizationInsensitive;

    pub fn fold(&self, s: &str) -> String {
        match self {
            FoldMode::Sensitive => s.to_string(),
            FoldMode::CaseInsensitive => s.to_lowercase(),
            FoldMode::CaseAndNormalizationInsensitive => s.nfc().collect::<String>().to_lowercase().nfc().collect(),
        }
    }
}

/// Entries that would map to the same name on a filesystem using `mode`. Only *files* and
/// file-vs-directory clashes are collisions; two directories that merge are harmless.
/// `entries` are `(path, is_dir)`.
pub fn find_collisions<'a>(entries: impl IntoIterator<Item = (&'a str, bool)>, mode: FoldMode) -> Vec<Vec<String>> {
    if mode == FoldMode::Sensitive {
        return vec![];
    }
    let mut groups: BTreeMap<String, Vec<(&str, bool)>> = BTreeMap::new();
    for (p, is_dir) in entries {
        groups.entry(mode.fold(p)).or_default().push((p, is_dir));
    }
    let mut out = vec![];
    for (_, g) in groups {
        let distinct: std::collections::BTreeSet<&str> = g.iter().map(|(p, _)| *p).collect();
        if distinct.len() < 2 {
            continue;
        }
        let files = g.iter().filter(|(_, d)| !*d).count();
        let dirs = g.len() - files;
        if files >= 1 && (files + dirs) >= 2 {
            let mut names: Vec<String> = distinct.iter().map(|s| s.to_string()).collect();
            names.sort();
            out.push(names);
        }
    }
    out
}

/// Probe whether the filesystem at `dir` is case-insensitive by creating a mixed-case probe file.
pub fn is_case_insensitive_fs(dir: &Path) -> bool {
    let probe = dir.join(format!(".PackSyncCaseProbe-{}", uuid::Uuid::new_v4().simple()));
    let lower = dir.join(probe.file_name().unwrap().to_string_lossy().to_lowercase());
    if std::fs::write(&probe, b"").is_err() {
        return false;
    }
    let insensitive = lower.exists();
    let _ = std::fs::remove_file(&probe);
    insensitive
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_malformed_paths() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            "a\\b",
            "C:/Windows",
            "c:evil",
            "a/b\0c",
            "tab\there",
        ] {
            assert!(validate_relpath(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(validate_relpath("a/".repeat(65).trim_end_matches('/')).is_err(), "too deep");
        assert!(validate_relpath(&"x".repeat(256)).is_err(), "component too long");
        assert!(validate_relpath(&format!("{}/{}", "d".repeat(250), "e".repeat(250))).is_ok());
        assert!(validate_relpath(&"a/".repeat(600)).is_err());
    }

    #[test]
    fn accepts_realistic_minecraft_paths() {
        for ok in [
            ".minecraft/saves/Новый мир/region/r.-1.0.mca",
            "minecraft/mods/jei-1.20.1-forge-15.20.0.106.jar",
            "minecraft/saves/世界/level.dat",
            "minecraft/config/ae2/common.json",
            "saves/ünï cödé 🎮/playerdata/0000.dat",
            "a b/c d.txt",
        ] {
            assert!(validate_relpath(ok).is_ok(), "{ok:?} must be accepted");
        }
    }

    #[test]
    fn join_safe_never_escapes() {
        let root = Path::new("/inst");
        assert_eq!(join_safe(root, "a/b.txt").unwrap(), Path::new("/inst/a/b.txt"));
        assert!(join_safe(root, "../etc").is_err());
        assert!(join_safe(root, "/etc").is_err());
    }

    #[test]
    fn relpath_roundtrip() {
        let root = Path::new("/inst");
        assert_eq!(to_relpath(root, Path::new("/inst/a/b")).unwrap(), "a/b");
        assert!(to_relpath(root, Path::new("/other/a")).is_none());
        assert!(to_relpath(root, Path::new("/inst")).is_none());
    }

    #[test]
    fn windows_portability() {
        assert!(windows_issues("saves/World/region/r.0.0.mca").is_empty());
        assert!(!windows_issues("config/CON.txt").is_empty());
        assert!(!windows_issues("a/aux").is_empty());
        assert!(!windows_issues("a/com1.cfg").is_empty());
        assert!(windows_issues("a/com10.cfg").is_empty());
        assert!(!windows_issues("a/what?.txt").is_empty());
        assert!(!windows_issues("a/name:stream").is_empty());
        assert!(!windows_issues("a/trailing.").is_empty());
        assert!(!windows_issues("a/trailing ").is_empty());
        assert!(windows_issues("a/.hidden").is_empty());
    }

    #[test]
    fn collisions_by_case_and_normalisation() {
        let e = [("Config/A.cfg", false), ("config/a.cfg", false), ("config/b.cfg", false)];
        assert!(find_collisions(e.iter().copied(), FoldMode::Sensitive).is_empty());
        let g = find_collisions(e.iter().copied(), FoldMode::CaseInsensitive);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0], vec!["Config/A.cfg".to_string(), "config/a.cfg".to_string()]);

        // NFC vs NFD "é"
        let nfc = "caf\u{e9}.txt";
        let nfd = "cafe\u{301}.txt";
        let e = [(nfc, false), (nfd, false)];
        assert!(find_collisions(e.iter().copied(), FoldMode::CaseInsensitive).is_empty());
        assert_eq!(find_collisions(e.iter().copied(), FoldMode::CaseAndNormalizationInsensitive).len(), 1);

        // two directories differing in case merge harmlessly; file vs dir does not
        let e = [("A", true), ("a", true)];
        assert!(find_collisions(e.iter().copied(), FoldMode::CaseInsensitive).is_empty());
        let e = [("A", true), ("a", false)];
        assert_eq!(find_collisions(e.iter().copied(), FoldMode::CaseInsensitive).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestors_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("saves")).unwrap();
        let err = ensure_no_symlink_ancestors(d.path(), "saves/World/level.dat").unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        std::fs::create_dir_all(d.path().join("real/dir")).unwrap();
        assert!(ensure_no_symlink_ancestors(d.path(), "real/dir/file.txt").is_ok());
        assert!(ensure_no_symlink_ancestors(d.path(), "brand/new/path.txt").is_ok());
    }

    #[test]
    fn case_probe_matches_this_filesystem_semantics() {
        let d = tempfile::tempdir().unwrap();
        // On Linux CI this is false; on Windows/macOS defaults true. Just make sure it does not
        // leave probe files behind.
        let _ = is_case_insensitive_fs(d.path());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }
}
