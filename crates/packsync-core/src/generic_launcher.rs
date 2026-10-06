//! `GenericLauncher`: a launcher-agnostic adapter for a plain Minecraft game folder chosen by hand
//! (no launcher metadata to read). It also backs the test harness. It can restore into a chosen
//! directory but cannot launch the game or install hooks.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::launcher::*;
use crate::mixed::MixedPolicy;
use crate::paths::validate_relpath;

#[derive(Default)]
pub struct GenericLauncher;

impl GenericLauncher {
    pub const KIND: &'static str = "generic";
}

fn safe_folder_name(name: &str) -> String {
    let cleaned: String =
        name.chars()
            .map(|c| {
                if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
    let t = cleaned.trim().trim_matches('.').to_string();
    if t.is_empty() { "instance".into() } else { t }
}

impl Launcher for GenericLauncher {
    fn kind(&self) -> &'static str {
        Self::KIND
    }

    fn discover(&self) -> Result<Vec<DiscoveredInstance>> {
        Ok(vec![])
    }

    fn describe(&self, root: &Path) -> Result<DiscoveredInstance> {
        if !root.is_dir() {
            return Err(Error::invalid("instance folder", format!("{} is not a folder", root.display())));
        }
        // The folder itself is the game directory (`saves/`, `mods/`, ... directly inside).
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "instance".into());
        Ok(DiscoveredInstance {
            launcher_ref: LauncherRef {
                launcher: Self::KIND.into(),
                data_dir: None,
                instance_id: None,
                executable: None,
            },
            root: root.to_path_buf(),
            name,
            game_root: String::new(),
            identity: PackIdentity::default(),
            worlds: crate::worlds::list_worlds(root),
        })
    }

    fn rules_fragment(&self) -> &'static str {
        ""
    }

    fn mixed_policies(&self) -> Vec<MixedPolicy> {
        vec![]
    }

    fn local_settings(&self, _root: &Path) -> Result<LocalSettings> {
        Ok(LocalSettings::default())
    }

    fn register_restored(&self, req: &RegisterRequest) -> Result<RegisteredInstance> {
        let parent =
            req.staged_root.parent().ok_or_else(|| Error::invalid("restore location", "no parent directory"))?;
        let mut name = safe_folder_name(&req.preferred_folder);
        validate_relpath(&name).map_err(|e| Error::invalid("folder name", e.to_string()))?;
        let mut n = 2;
        while parent.join(&name).exists() {
            name = format!("{} ({n})", safe_folder_name(&req.preferred_folder));
            n += 1;
        }
        let dest: PathBuf = parent.join(&name);
        std::fs::rename(&req.staged_root, &dest)
            .map_err(|e| Error::io(format!("moving the restored instance to {}", dest.display()), e))?;
        Ok(RegisteredInstance {
            launcher_ref: LauncherRef {
                launcher: Self::KIND.into(),
                data_dir: None,
                instance_id: Some(name),
                executable: None,
            },
            root: dest,
            game_root: String::new(),
            notes: vec!["Restored as a plain game folder; point your launcher at it.".into()],
        })
    }

    fn launch(&self, _req: &LaunchRequest) -> Result<LaunchHandle> {
        Err(Error::Unsupported("the generic launcher cannot start the game; use a launcher integration".into()))
    }

    fn is_launcher_running(&self, _r: &LauncherRef) -> bool {
        false
    }

    fn game_process_patterns(&self, _root: &Path) -> Vec<String> {
        vec![]
    }

    fn hook_state(&self, _root: &Path) -> Result<HookState> {
        Ok(HookState::default())
    }
    fn plan_install_hooks(&self, root: &Path, _spec: &HookSpec) -> Result<HookPlan> {
        Ok(HookPlan {
            root: root.to_path_buf(),
            changes: vec![],
            preserved_commands: vec![],
            blockers: vec!["the generic launcher has no hook support".into()],
        })
    }
    fn apply_hook_plan(&self, _plan: &HookPlan) -> Result<()> {
        Err(Error::Unsupported("the generic launcher has no hook support".into()))
    }
    fn plan_remove_hooks(&self, root: &Path) -> Result<HookPlan> {
        Ok(HookPlan { root: root.to_path_buf(), changes: vec![], preserved_commands: vec![], blockers: vec![] })
    }
}
