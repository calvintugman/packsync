//! Finding Prism data directories and the instances inside them.
//!
//! Mirrors `Application.cpp` / `InstanceList::discoverInstances` of Prism 11.0.3: an instance is a
//! sub-folder of the `InstanceDir` setting that contains `instance.cfg`; its ID is the folder name.

use std::path::{Path, PathBuf};

use packsync_core::error::Result;

use crate::files::read_bounded;
use crate::qtini::{IniDoc, QtValue};

/// Prism's launcher-wide settings file. **It may hold API keys** (`FlameKeyOverride`,
/// `ModrinthToken`, …) and global launch commands: only `InstanceDir` is ever read from it, nothing
/// else is stored, returned or logged.
pub const LAUNCHER_CFG: &str = "prismlauncher.cfg";
pub const DEFAULT_INSTANCE_DIR: &str = "instances";
pub const INSTANCE_CFG: &str = "instance.cfg";
pub const PACK_JSON: &str = "mmc-pack.json";

/// Folders inside the instances directory that are never instances: Prism's own staging folder
/// (`.tmp`) and the MultiMC-era temp folders. Any name starting with `.` is skipped as well (that
/// covers PackSync's own `.packsync-restore-*` staging folders).
const TEMP_NAMES: &[&str] = &["_LAUNCHER_TEMP", "_MMC_TEMP", "_PRISM_TEMP"];

/// Is `data_dir` a Prism data directory we can use? (It exists; an empty one is fine: a fresh Prism
/// creates its files lazily.)
pub fn is_data_dir(data_dir: &Path) -> bool {
    data_dir.is_dir()
}

/// The instances directory configured in `<data_dir>/prismlauncher.cfg` (`InstanceDir`), resolved
/// like Prism does: absolute as is, relative to the data directory (Prism `chdir`s into it).
///
/// Only the `InstanceDir` value is extracted; the rest of the file is dropped unread.
pub fn instances_dir(data_dir: &Path) -> PathBuf {
    let cfg = data_dir.join(LAUNCHER_CFG);
    let configured = match crate::files::read_bounded_opt(&cfg, 1024 * 1024) {
        Ok(Some(bytes)) => instance_dir_value(&bytes),
        // A missing, unreadable or oversized launcher config simply means "default".
        Ok(None) | Err(_) => None,
    };
    let rel = configured.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| DEFAULT_INSTANCE_DIR.to_string());
    let p = PathBuf::from(&rel);
    if p.is_absolute() { p } else { data_dir.join(p) }
}

/// Extract the `InstanceDir` value from the bytes of `prismlauncher.cfg` and nothing else.
pub fn instance_dir_value(bytes: &[u8]) -> Option<String> {
    let doc = IniDoc::parse(bytes).ok()?;
    match doc.get("InstanceDir")? {
        QtValue::Str(s) => Some(s),
        _ => None,
    }
}

/// One instance folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceDirEntry {
    /// Prism's instance ID (the folder name).
    pub id: String,
    pub path: PathBuf,
}

fn is_hidden_or_temp(name: &str) -> bool {
    name.starts_with('.') || TEMP_NAMES.contains(&name)
}

/// Instance folders below `instances_dir`, sorted by name (case-insensitive, then exact).
/// Unreadable directories and non-UTF-8 names are skipped.
pub fn list_instance_dirs(instances_dir: &Path) -> Vec<InstanceDirEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(instances_dir) else { return out };
    for e in rd.flatten() {
        let Some(name) = e.file_name().to_str().map(|s| s.to_string()) else { continue };
        if is_hidden_or_temp(&name) {
            continue;
        }
        let path = e.path();
        if !path.is_dir() || !path.join(INSTANCE_CFG).exists() {
            continue;
        }
        // Prism ignores a symlink whose target lives directly in the instances folder (it would
        // show the same instance twice).
        if e.file_type().map(|t| t.is_symlink()).unwrap_or(false)
            && let (Ok(target), Ok(inst)) = (dunce::canonicalize(&path), dunce::canonicalize(instances_dir))
            && target.parent() == Some(inst.as_path())
        {
            continue;
        }
        out.push(InstanceDirEntry { id: name, path });
    }
    out.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Read `instance.cfg` of an instance folder (bounded).
pub fn read_instance_cfg(root: &Path) -> Result<Vec<u8>> {
    read_bounded(&root.join(INSTANCE_CFG), crate::qtini::MAX_INI_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_instance(instances: &Path, name: &str) -> PathBuf {
        let p = instances.join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("instance.cfg"), b"[General]\nInstanceType=OneSix\n").unwrap();
        p
    }

    #[test]
    fn instance_dir_default_relative_and_absolute() {
        let d = tempfile::tempdir().unwrap();
        // no config at all
        assert_eq!(instances_dir(d.path()), d.path().join("instances"));
        // relative
        std::fs::write(
            d.path().join("prismlauncher.cfg"),
            "[General]\nConfigVersion=1.3\nInstanceDir=my instances/ünï\n",
        )
        .unwrap();
        assert_eq!(instances_dir(d.path()), d.path().join("my instances/ünï"));
        // absolute
        let abs = d.path().join("elsewhere");
        // as Prism (Qt) writes it: forward slashes on every platform (a raw `\` would be an INI escape)
        let stored = abs.display().to_string().replace('\\', "/");
        std::fs::write(d.path().join("prismlauncher.cfg"), format!("[General]\nInstanceDir={stored}\n")).unwrap();
        assert_eq!(instances_dir(d.path()), abs);
        // empty / list-valued / garbage values fall back to the default
        std::fs::write(d.path().join("prismlauncher.cfg"), "[General]\nInstanceDir=\n").unwrap();
        assert_eq!(instances_dir(d.path()), d.path().join("instances"));
        std::fs::write(d.path().join("prismlauncher.cfg"), "[General]\nInstanceDir=a,b\n").unwrap();
        assert_eq!(instances_dir(d.path()), d.path().join("instances"));
        std::fs::write(d.path().join("prismlauncher.cfg"), [0xff, 0xfe, b'=', b'\n']).unwrap();
        assert_eq!(instances_dir(d.path()), d.path().join("instances"));
    }

    #[test]
    fn only_instance_dir_is_extracted_from_the_launcher_config() {
        let cfg = "[General]\nFlameKeyOverride=SECRET-KEY\nModrinthToken=SECRET-TOKEN\nPreLaunchCommand=secret-hook\nInstanceDir=insts\n";
        assert_eq!(instance_dir_value(cfg.as_bytes()).as_deref(), Some("insts"));
        let none = "[General]\nFlameKeyOverride=SECRET-KEY\n";
        assert_eq!(instance_dir_value(none.as_bytes()), None);
    }

    #[test]
    fn lists_real_looking_folder_names_and_skips_hidden_temp_and_non_instances() {
        let d = tempfile::tempdir().unwrap();
        let inst = d.path().join("instances");
        std::fs::create_dir_all(&inst).unwrap();
        for n in [
            "GregTech Easy(1)",
            "All the Mods 10- To the Sky   ATM10SKY",
            "GregTech.Odyssey-0.5.5-beta",
            "ünï cödé 🎮",
            "Zeta",
            "alpha",
        ] {
            mk_instance(&inst, n);
        }
        // not instances / must be skipped
        std::fs::create_dir_all(inst.join("no cfg here")).unwrap();
        std::fs::write(inst.join("instgroups.json"), b"{}").unwrap();
        mk_instance(&inst, ".tmp");
        mk_instance(&inst, ".packsync-restore-abc");
        mk_instance(&inst, "_LAUNCHER_TEMP");
        // a nested instance two levels down is not an instance
        let nested = inst.join("group folder");
        std::fs::create_dir_all(&nested).unwrap();
        mk_instance(&nested, "inner");

        let names: Vec<String> = list_instance_dirs(&inst).into_iter().map(|e| e.id).collect();
        assert_eq!(
            names,
            vec![
                "All the Mods 10- To the Sky   ATM10SKY",
                "alpha",
                "GregTech Easy(1)",
                "GregTech.Odyssey-0.5.5-beta",
                "Zeta",
                "ünï cödé 🎮"
            ]
        );
        assert!(list_instance_dirs(&d.path().join("missing")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_sibling_instance_is_ignored_like_prism_does() {
        let d = tempfile::tempdir().unwrap();
        let inst = d.path().join("instances");
        std::fs::create_dir_all(&inst).unwrap();
        let real = mk_instance(&inst, "Real");
        std::os::unix::fs::symlink(&real, inst.join("Alias")).unwrap();
        let outside = mk_instance(d.path(), "Outside");
        std::os::unix::fs::symlink(&outside, inst.join("Linked")).unwrap();
        let names: Vec<String> = list_instance_dirs(&inst).into_iter().map(|e| e.id).collect();
        assert_eq!(names, vec!["Linked", "Real"]);
    }
}
